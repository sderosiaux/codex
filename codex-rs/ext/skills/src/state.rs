use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use codex_exec_server::EnvironmentAccessKey;
use codex_extension_api::ExtensionMetrics;
use codex_protocol::capabilities::SelectedCapabilityRoot;

use crate::SkillsExtensionConfig;
use crate::SkillsExtensionState;
use crate::catalog::SkillAuthority;
use crate::catalog::SkillCatalog;
use crate::catalog::SkillCatalogEntry;
use crate::catalog::SkillPackageId;
use crate::catalog::SkillReadResult;
use crate::provider::SkillListQuery;
use crate::shadow_selection_experiment::RecentSkillInvocations;
use crate::shadow_selection_experiment::ShadowSelectionTurnState;
use crate::shadow_selection_experiment::ShadowTaskContext;
use crate::skills_extension_state::CachedExecutorCatalog;
use crate::skills_extension_state::CachedExecutorDiscoveryCatalog;
use crate::sources::SkillProviders;

pub(crate) struct SkillsSessionState {
    pub(crate) extension_metrics: Option<Arc<dyn ExtensionMetrics>>,
}

/// Thread-owned skill configuration and caches; consumers can only read catalog snapshots.
pub struct SkillsThreadState {
    config: Mutex<SkillsExtensionConfig>,
    skills_extension_state: Mutex<SkillsExtensionState>,
    shadow_selection_turn: Mutex<Option<ShadowSelectionTurn>>,
    pub(crate) executor_read_snapshot: Mutex<Option<ExecutorReadSnapshot>>,
    pub(crate) recent_skill_invocations: Arc<RecentSkillInvocations>,
    pub(crate) shadow_task_context: Arc<ShadowTaskContext>,
}

impl SkillsThreadState {
    pub(crate) fn new(config: SkillsExtensionConfig) -> Self {
        Self {
            config: Mutex::new(config),
            skills_extension_state: Mutex::new(SkillsExtensionState::default()),
            shadow_selection_turn: Mutex::new(None),
            executor_read_snapshot: Mutex::new(None),
            recent_skill_invocations: Arc::new(RecentSkillInvocations::default()),
            shadow_task_context: Arc::new(ShadowTaskContext::default()),
        }
    }

    pub(crate) fn config(&self) -> SkillsExtensionConfig {
        self.config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn set_config(&self, config: SkillsExtensionConfig) {
        *self
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = config;
    }

    pub(crate) fn replace_shadow_selection_turn(
        &self,
        turn_id: String,
        state: Option<ShadowSelectionTurnState>,
    ) {
        *self
            .shadow_selection_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            state.map(|state| ShadowSelectionTurn {
                turn_id,
                state: Arc::new(state),
            });
    }

    pub(crate) fn shadow_selection_turn(
        &self,
        turn_id: &str,
    ) -> Option<Arc<ShadowSelectionTurnState>> {
        self.shadow_selection_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|turn| turn.turn_id == turn_id)
            .map(|turn| Arc::clone(&turn.state))
    }

    /// Returns the catalog for these exact roots, using the existing thread caches.
    #[tracing::instrument(
        name = "skills.executor.refresh_executor_catalog",
        level = "info",
        skip_all,
        fields(root_count = query.executor_roots.len())
    )]
    pub(crate) async fn refresh_executor_catalog(
        &self,
        providers: &SkillProviders,
        mut query: SkillListQuery,
    ) -> SkillCatalog {
        if query.executor_capability_discovery.is_some() {
            // High-level discovery is enabled: reuse or project its discovery snapshot.
            self.executor_discovery_catalog_snapshot(providers, query)
                .await
        } else {
            // High-level discovery is not enabled: retain the legacy per-root cache path.
            let roots = std::mem::take(&mut query.executor_roots);
            let mut catalog = SkillCatalog::default();
            for root in roots {
                query.executor_roots = vec![root.clone()];
                catalog.extend(
                    self.executor_root_catalog(providers, root, query.clone())
                        .await,
                );
            }
            catalog
        }
    }

    /// Reuses matching successful discovery; otherwise returns the result of this request.
    async fn executor_discovery_catalog_snapshot(
        &self,
        providers: &SkillProviders,
        query: SkillListQuery,
    ) -> SkillCatalog {
        let discovery = query.executor_capability_discovery.clone();
        let discovery_failed = discovery.as_ref().is_some_and(|discovery| {
            discovery.roots().iter().any(|root| {
                root.result
                    .as_ref()
                    .ok()
                    .is_none_or(|discovered| discovered.error.is_some())
            })
        });
        if !discovery_failed
            && let Some(cached) = self
                .skills_extension_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .executor_discovery_cache
                .as_ref()
                .filter(|cached| {
                    cached.matches(
                        &query.executor_roots,
                        discovery.as_ref(),
                        query.include_bundled_skills,
                    )
                })
        {
            return cached.catalog.clone();
        }

        let roots = query.executor_roots.clone();
        let include_bundled_skills = query.include_bundled_skills;
        let discovered = providers.list_executor_for_turn(query).await;
        let mut state = self
            .skills_extension_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Failed discovery is returned to this caller but is never reused by another request.
        state.executor_discovery_cache =
            discovery.map(|discovery| CachedExecutorDiscoveryCatalog {
                roots,
                discovery,
                include_bundled_skills,
                catalog: discovered.clone(),
            });
        discovered
    }

    #[tracing::instrument(name = "skills.executor.catalog_root", level = "info", skip_all)]
    async fn executor_root_catalog(
        &self,
        providers: &SkillProviders,
        root: SelectedCapabilityRoot,
        query: SkillListQuery,
    ) -> SkillCatalog {
        if let Some(cached) = self
            .skills_extension_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .executor_cache
            .iter()
            .find(|cached| cached.root == root)
        {
            return cached.catalog.clone();
        }

        let discovered = providers.list_executor_for_turn(query).await;
        let mut state = self
            .skills_extension_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cache = &mut state.executor_cache;
        if let Some(cached) = cache.iter().find(|cached| cached.root == root) {
            return cached.catalog.clone();
        }
        cache.push(CachedExecutorCatalog {
            root,
            catalog: discovered.clone(),
        });
        discovered
    }
}

/// One bounded executor resource, retained for continuations until replacement or thread drop.
/// Interleaved resources may evict it; misses reread and validate the content-bound cursor.
pub(crate) struct ExecutorReadSnapshot {
    pub(crate) authority: SkillAuthority,
    pub(crate) package: SkillPackageId,
    // The key binds cached contents to their source filesystem and callback permissions.
    pub(crate) access: EnvironmentAccessKey,
    pub(crate) result: Arc<SkillReadResult>,
}

struct ShadowSelectionTurn {
    turn_id: String,
    state: Arc<ShadowSelectionTurnState>,
}

#[derive(Default)]
pub(crate) struct EmittedCatalogBudgetWarnings(Mutex<HashSet<String>>);

impl EmittedCatalogBudgetWarnings {
    pub(crate) fn insert(&self, warning: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(warning.to_string())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SkillsTurnState {
    pub(crate) catalog: SkillCatalog,
    pub(crate) selected_entries: Vec<SkillCatalogEntry>,
    pub(crate) warnings: Vec<String>,
    pub(crate) main_prompts_injected: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HostSkillsCatalogInWorldState;

#[derive(Clone, Debug, Default)]
pub(crate) struct ExecutorSkillsStepState(pub(crate) SkillCatalog);

#[derive(Clone, Debug, Default)]
pub(crate) struct HostSkillsStepState(pub(crate) SkillCatalog);
