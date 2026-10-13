//! Stores published skill catalogs and executor discovery projections.

use std::sync::Arc;

use codex_exec_server::ExecutorCapabilityDiscoverySnapshot;
use codex_protocol::capabilities::SelectedCapabilityRoot;

use crate::catalog::SkillCatalog;

/// Stores published executor catalogs for a thread.
#[derive(Clone, Debug, Default)]
pub(crate) struct SkillsExtensionState {
    pub(crate) executor_cache: Vec<CachedExecutorCatalog>,
    pub(crate) executor_discovery_cache: Option<CachedExecutorDiscoveryCatalog>,
}

/// Retains the legacy per-root catalog until the thread is dropped.
#[derive(Clone, Debug)]
pub(crate) struct CachedExecutorCatalog {
    pub(crate) root: SelectedCapabilityRoot,
    pub(crate) catalog: SkillCatalog,
}

/// Keeps the latest result; only successful results for unchanged inputs can be reused.
#[derive(Clone, Debug)]
pub(crate) struct CachedExecutorDiscoveryCatalog {
    pub(crate) roots: Vec<SelectedCapabilityRoot>,
    pub(crate) discovery: ExecutorCapabilityDiscoverySnapshot,
    pub(crate) include_bundled_skills: bool,
    pub(crate) catalog: SkillCatalog,
}

impl CachedExecutorDiscoveryCatalog {
    pub(crate) fn matches(
        &self,
        roots: &[SelectedCapabilityRoot],
        discovery: Option<&ExecutorCapabilityDiscoverySnapshot>,
        include_bundled_skills: bool,
    ) -> bool {
        discovery.is_some_and(|discovery| {
            self.roots == roots
                && self.include_bundled_skills == include_bundled_skills
                && self.discovery.roots().len() == discovery.roots().len()
                && self.discovery.roots().iter().zip(discovery.roots()).all(
                    |(previous, current)| {
                        previous.selected_root == current.selected_root
                            && match (&previous.result, &current.result) {
                                (Ok(previous), Ok(current)) => Arc::ptr_eq(previous, current),
                                _ => false,
                            }
                    },
                )
        })
    }
}
