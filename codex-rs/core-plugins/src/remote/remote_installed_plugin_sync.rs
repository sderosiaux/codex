use super::REMOTE_CREATED_BY_ME_MARKETPLACE_NAME;
use super::REMOTE_GLOBAL_MARKETPLACE_NAME;
use super::REMOTE_WORKSPACE_MARKETPLACE_NAME;
use super::REMOTE_WORKSPACE_SHARED_WITH_ME_MARKETPLACE_NAME;
use super::REMOTE_WORKSPACE_SHARED_WITH_ME_PRIVATE_MARKETPLACE_NAME;
use super::REMOTE_WORKSPACE_SHARED_WITH_ME_UNLISTED_MARKETPLACE_NAME;
use super::RemoteInstalledPlugin;
use super::RemoteInstalledPluginScope;
use super::RemotePluginCapabilities;
use super::RemotePluginCatalogError;
use super::RemotePluginScope;
use super::RemotePluginServiceConfig;
use super::RemotePluginShareDiscoverability;
use super::ensure_chatgpt_auth;
use super::fetch_installed_plugins;
use crate::store::PLUGINS_CACHE_DIR;
use crate::store::PluginStore;
use crate::store::PluginStoreError;
use codex_login::CodexAuth;
use codex_plugin::PluginId;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::Weak;
use tokio::fs;
use tokio::sync::Semaphore;
use tracing::warn;

static REMOTE_INSTALLED_PLUGIN_BUNDLE_SYNC_GATES: OnceLock<
    Mutex<HashMap<PathBuf, Weak<Semaphore>>>,
> = OnceLock::new();
static REMOTE_PLUGIN_CACHE_MUTATIONS_IN_FLIGHT: OnceLock<
    Mutex<HashMap<RemotePluginCacheMutationKey, usize>>,
> = OnceLock::new();

/// A remote plugin bundle newly installed or updated from an authenticated snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePluginMaterialization {
    pub plugin_id: PluginId,
    pub scope: RemotePluginScope,
    pub discoverability: Option<RemotePluginShareDiscoverability>,
    pub authenticated_account_id: Option<String>,
}

/// A local plugin and the runtime categories affected by its bundle or installed-state change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePluginChange {
    pub plugin_id: String,
    pub capabilities: RemotePluginCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoteInstalledPluginBundleSyncOutcome {
    /// Internal provenance for materialization-owned hook trust, not runtime change reporting.
    pub materialized_remote_plugins: Vec<RemotePluginMaterialization>,
    /// Affected plugins with capabilities from either side of the change, including removals.
    /// Installed-state removals do not depend on cache cleanup succeeding.
    pub changed_plugins: Vec<RemotePluginChange>,
    pub failed_remote_plugin_ids: Vec<String>,
    /// Failures that leave an otherwise valid installed plugin unavailable locally.
    pub failed_materialization_remote_plugin_ids: Vec<String>,
}

pub(crate) struct RemoteInstalledPluginBundleSyncResult {
    pub(crate) outcome: RemoteInstalledPluginBundleSyncOutcome,
    pub(crate) installed_plugins: Vec<RemoteInstalledPlugin>,
}

#[derive(Debug, thiserror::Error)]
pub enum RemoteInstalledPluginBundleSyncError {
    #[error("{0}")]
    Catalog(#[from] RemotePluginCatalogError),

    #[error("{0}")]
    Store(#[from] PluginStoreError),

    #[error("timed out waiting for another remote plugin cache mutation; retry")]
    LockTimeout,

    #[error("remote plugin state changed during reconciliation; retry reconciliation")]
    Superseded,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RemotePluginCacheMutationKey {
    plugin_cache_root: PathBuf,
    marketplace_name: String,
    plugin_name: String,
}

pub struct RemotePluginCacheMutationGuard {
    key: RemotePluginCacheMutationKey,
}

pub(crate) fn remote_installed_plugin_bundle_sync_gate(codex_home: &Path) -> Arc<Semaphore> {
    let plugin_cache_root = remote_plugin_cache_root(codex_home);
    let gates =
        REMOTE_INSTALLED_PLUGIN_BUNDLE_SYNC_GATES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut gates = match gates.lock() {
        Ok(gates) => gates,
        Err(err) => err.into_inner(),
    };
    if let Some(gate) = gates.get(&plugin_cache_root).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(Semaphore::new(/*permits*/ 1));
    gates.insert(plugin_cache_root, Arc::downgrade(&gate));
    gate
}

pub async fn sync_remote_installed_plugin_bundles_once(
    codex_home: PathBuf,
    config: &RemotePluginServiceConfig,
    auth: Option<&CodexAuth>,
) -> Result<RemoteInstalledPluginBundleSyncOutcome, RemoteInstalledPluginBundleSyncError> {
    let result = sync_remote_installed_plugin_bundles_once_with_snapshot(
        codex_home,
        config,
        auth,
        /*previous_plugin_ids*/ &[],
    )
    .await?;
    Ok(result.outcome)
}

pub(crate) async fn sync_remote_installed_plugin_bundles_once_with_snapshot(
    codex_home: PathBuf,
    config: &RemotePluginServiceConfig,
    auth: Option<&CodexAuth>,
    previous_plugin_ids: &[PluginId],
) -> Result<RemoteInstalledPluginBundleSyncResult, RemoteInstalledPluginBundleSyncError> {
    let auth = ensure_chatgpt_auth(auth)?;
    let authenticated_account_id = auth.get_account_id();
    let fetched_installed_plugins = fetch_installed_plugins(
        config,
        auth,
        RemoteInstalledPluginScope::All,
        /*include_download_urls*/ true,
    )
    .await?;
    // `/installed` is an authoritative full snapshot. If any row cannot be
    // canonicalized to a local cache key, omitting it makes an installed plugin
    // indistinguishable from an uninstall, so reject the pass before downloads,
    // publication, or stale cleanup.
    let mut validated_installed_plugins = Vec::with_capacity(fetched_installed_plugins.len());
    for installed_plugin in fetched_installed_plugins {
        let cached_plugin = super::remote_installed_plugin_to_cache_entry(&installed_plugin)?;
        let plugin_id = PluginId::new(
            installed_plugin.plugin.name.clone(),
            cached_plugin.marketplace_name.clone(),
        )
        .map_err(|err| {
            RemotePluginCatalogError::UnexpectedResponse(format!(
                "remote installed plugin `{}` has an invalid local cache id: {err}",
                installed_plugin.plugin.id
            ))
        })?;
        validated_installed_plugins.push((installed_plugin, cached_plugin, plugin_id));
    }
    let store = PluginStore::try_new(codex_home.clone())?;
    let installed_plugin_ids = validated_installed_plugins
        .iter()
        .map(|(_, _, plugin_id)| plugin_id.as_key())
        .collect::<BTreeSet<_>>();
    let mut changed_plugins = BTreeMap::new();
    // Metadata publication removes these plugins even when cleanup fails or partially deletes
    // a bundle. Capture cached capabilities first so consumers can still invalidate runtimes;
    // plugins that were never available locally have no previous bundle to invalidate.
    for plugin_id in previous_plugin_ids {
        let key = plugin_id.as_key();
        if installed_plugin_ids.contains(&key) || store.active_plugin_root(plugin_id).is_none() {
            continue;
        }
        let mut capabilities = RemotePluginCapabilities::default();
        capabilities.include_active_bundle(&store, plugin_id).await;
        changed_plugins.insert(
            key.clone(),
            RemotePluginChange {
                plugin_id: key,
                capabilities,
            },
        );
    }
    let mut installed_plugin_names_by_marketplace =
        BTreeMap::<String, BTreeSet<String>>::from_iter([
            (REMOTE_GLOBAL_MARKETPLACE_NAME.to_string(), BTreeSet::new()),
            (
                REMOTE_CREATED_BY_ME_MARKETPLACE_NAME.to_string(),
                BTreeSet::new(),
            ),
            (
                REMOTE_WORKSPACE_MARKETPLACE_NAME.to_string(),
                BTreeSet::new(),
            ),
            (
                REMOTE_WORKSPACE_SHARED_WITH_ME_MARKETPLACE_NAME.to_string(),
                BTreeSet::new(),
            ),
            (
                REMOTE_WORKSPACE_SHARED_WITH_ME_PRIVATE_MARKETPLACE_NAME.to_string(),
                BTreeSet::new(),
            ),
            (
                REMOTE_WORKSPACE_SHARED_WITH_ME_UNLISTED_MARKETPLACE_NAME.to_string(),
                BTreeSet::new(),
            ),
        ]);
    let mut materialized_remote_plugins = BTreeMap::new();
    let mut failed_remote_plugin_ids = BTreeSet::new();
    let mut failed_materialization_remote_plugin_ids = BTreeSet::new();
    let mut installed_plugins = Vec::new();

    for (installed_plugin, cached_plugin, plugin_id) in validated_installed_plugins {
        let marketplace_name = cached_plugin.marketplace_name.clone();
        let plugin = installed_plugin.plugin;
        let scope = plugin.scope;
        let discoverability = plugin.discoverability;
        installed_plugin_names_by_marketplace
            .entry(marketplace_name.clone())
            .or_default()
            .insert(plugin.name.clone());
        let release_version = plugin
            .release
            .version
            .as_deref()
            .map(str::trim)
            .filter(|version| !version.is_empty());
        if let Some(release_version) = release_version
            && store.active_plugin_version(&plugin_id).as_deref() == Some(release_version)
        {
            if let Err(err) = store.write_remote_plugin_id(&plugin_id, &plugin.id) {
                warn!(
                    remote_plugin_id = %plugin.id,
                    plugin = %plugin.name,
                    marketplace = %marketplace_name,
                    error = %err,
                    "failed to persist identity for cached remote installed plugin"
                );
                failed_remote_plugin_ids.insert(plugin.id);
            }
            installed_plugins.push(cached_plugin);
            continue;
        }

        let bundle = match crate::remote_bundle::validate_remote_plugin_bundle(
            &plugin.id,
            &marketplace_name,
            &plugin.name,
            release_version,
            plugin.release.bundle_download_url.as_deref(),
            plugin.release.app_manifest.clone(),
        ) {
            Ok(bundle) => bundle,
            Err(err) => {
                warn!(
                    remote_plugin_id = %plugin.id,
                    plugin = %plugin.name,
                    marketplace = %marketplace_name,
                    error = %err,
                    "skipping remote installed plugin bundle download"
                );
                failed_remote_plugin_ids.insert(plugin.id.clone());
                failed_materialization_remote_plugin_ids.insert(plugin.id);
                installed_plugins.push(cached_plugin);
                continue;
            }
        };

        // Read the old bundle before installation replaces it, including capabilities removed
        // by the new version. Unchanged bundles never reach this metadata-loading path.
        let mut capabilities = RemotePluginCapabilities::default();
        capabilities.include_active_bundle(&store, &plugin_id).await;
        match crate::remote_bundle::download_and_install_remote_plugin_bundle(
            config,
            codex_home.clone(),
            bundle,
        )
        .await
        {
            Ok(result) => {
                let plugin_id = result.plugin_id;
                capabilities.include_active_bundle(&store, &plugin_id).await;
                changed_plugins.insert(
                    plugin_id.as_key(),
                    RemotePluginChange {
                        plugin_id: plugin_id.as_key(),
                        capabilities,
                    },
                );
                materialized_remote_plugins.insert(
                    plugin_id.as_key(),
                    RemotePluginMaterialization {
                        plugin_id,
                        scope,
                        discoverability,
                        authenticated_account_id: authenticated_account_id.clone(),
                    },
                );
            }
            Err(err) => {
                warn!(
                    remote_plugin_id = %plugin.id,
                    plugin = %plugin.name,
                    marketplace = %marketplace_name,
                    error = %err,
                    "failed to download remote installed plugin bundle"
                );
                failed_remote_plugin_ids.insert(plugin.id.clone());
                failed_materialization_remote_plugin_ids.insert(plugin.id);
            }
        }
        installed_plugins.push(cached_plugin);
    }

    installed_plugins.sort_by(|left, right| {
        left.marketplace_name
            .cmp(&right.marketplace_name)
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut removed_cache_plugins = Vec::new();
    if let Err(err) = remove_stale_remote_plugin_caches(
        &store,
        &installed_plugin_names_by_marketplace,
        &mut removed_cache_plugins,
    )
    .await
    {
        warn!(error = %err, "failed to remove stale remote plugin cache entries");
    }
    for plugin in removed_cache_plugins {
        changed_plugins
            .entry(plugin.plugin_id.clone())
            .or_insert(plugin);
    }

    Ok(RemoteInstalledPluginBundleSyncResult {
        outcome: RemoteInstalledPluginBundleSyncOutcome {
            materialized_remote_plugins: materialized_remote_plugins.into_values().collect(),
            changed_plugins: changed_plugins.into_values().collect(),
            failed_remote_plugin_ids: failed_remote_plugin_ids.into_iter().collect(),
            failed_materialization_remote_plugin_ids: failed_materialization_remote_plugin_ids
                .into_iter()
                .collect(),
        },
        installed_plugins,
    })
}

pub fn mark_remote_plugin_cache_mutation_in_flight(
    codex_home: &Path,
    marketplace_name: &str,
    plugin_name: &str,
) -> RemotePluginCacheMutationGuard {
    let key = RemotePluginCacheMutationKey {
        plugin_cache_root: remote_plugin_cache_root(codex_home),
        marketplace_name: marketplace_name.to_string(),
        plugin_name: plugin_name.to_string(),
    };
    let mutations =
        REMOTE_PLUGIN_CACHE_MUTATIONS_IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new()));
    let mut mutations = match mutations.lock() {
        Ok(mutations) => mutations,
        Err(err) => err.into_inner(),
    };
    *mutations.entry(key.clone()).or_default() += 1;
    RemotePluginCacheMutationGuard { key }
}

impl Drop for RemotePluginCacheMutationGuard {
    fn drop(&mut self) {
        let Some(mutations) = REMOTE_PLUGIN_CACHE_MUTATIONS_IN_FLIGHT.get() else {
            return;
        };
        let mut mutations = match mutations.lock() {
            Ok(mutations) => mutations,
            Err(err) => err.into_inner(),
        };
        if let Some(count) = mutations.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                mutations.remove(&self.key);
            }
        }
    }
}

async fn remove_stale_remote_plugin_caches(
    store: &PluginStore,
    installed_plugin_names_by_marketplace: &BTreeMap<String, BTreeSet<String>>,
    removed_plugins: &mut Vec<RemotePluginChange>,
) -> Result<(), String> {
    let codex_home = store.codex_home().as_path();
    for marketplace_name in [
        REMOTE_GLOBAL_MARKETPLACE_NAME,
        REMOTE_CREATED_BY_ME_MARKETPLACE_NAME,
        REMOTE_WORKSPACE_MARKETPLACE_NAME,
        REMOTE_WORKSPACE_SHARED_WITH_ME_MARKETPLACE_NAME,
        REMOTE_WORKSPACE_SHARED_WITH_ME_PRIVATE_MARKETPLACE_NAME,
        REMOTE_WORKSPACE_SHARED_WITH_ME_UNLISTED_MARKETPLACE_NAME,
    ] {
        let marketplace_root = codex_home.join(PLUGINS_CACHE_DIR).join(marketplace_name);
        if !marketplace_root.exists() {
            continue;
        }
        let installed_plugin_names = installed_plugin_names_by_marketplace
            .get(marketplace_name)
            .cloned()
            .unwrap_or_default();
        let mut entries = fs::read_dir(&marketplace_root).await.map_err(|err| {
            format!(
                "failed to read remote plugin cache directory {}: {err}",
                marketplace_root.display()
            )
        })?;
        while let Some(entry) = entries.next_entry().await.map_err(|err| {
            format!(
                "failed to enumerate remote plugin cache directory {}: {err}",
                marketplace_root.display()
            )
        })? {
            let plugin_name = entry.file_name().into_string().map_err(|file_name| {
                format!(
                    "remote plugin cache entry under {} is not valid UTF-8: {:?}",
                    marketplace_root.display(),
                    file_name
                )
            })?;
            if installed_plugin_names.contains(&plugin_name) {
                continue;
            }
            if is_remote_plugin_cache_mutation_in_flight(codex_home, marketplace_name, &plugin_name)
            {
                continue;
            }

            let cache_path = entry.path();
            let plugin_id = PluginId::new(plugin_name.clone(), marketplace_name.to_string());
            let mut capabilities = RemotePluginCapabilities::default();
            if let Ok(plugin_id) = &plugin_id {
                capabilities.include_active_bundle(store, plugin_id).await;
            }
            if cache_path.is_dir() {
                fs::remove_dir_all(&cache_path).await.map_err(|err| {
                    format!(
                        "failed to remove stale remote plugin cache entry {}: {err}",
                        cache_path.display()
                    )
                })?;
            } else {
                fs::remove_file(&cache_path).await.map_err(|err| {
                    format!(
                        "failed to remove stale remote plugin cache entry {}: {err}",
                        cache_path.display()
                    )
                })?;
            }
            let plugin_key = plugin_id
                .map(|plugin_id| plugin_id.as_key())
                .unwrap_or_else(|_| format!("{plugin_name}@{marketplace_name}"));
            removed_plugins.push(RemotePluginChange {
                plugin_id: plugin_key,
                capabilities,
            });
        }
    }

    Ok(())
}

fn remote_plugin_cache_root(codex_home: &Path) -> PathBuf {
    codex_home.join(PLUGINS_CACHE_DIR)
}

fn is_remote_plugin_cache_mutation_in_flight(
    codex_home: &Path,
    marketplace_name: &str,
    plugin_name: &str,
) -> bool {
    let Some(mutations) = REMOTE_PLUGIN_CACHE_MUTATIONS_IN_FLIGHT.get() else {
        return false;
    };
    let mutations = match mutations.lock() {
        Ok(mutations) => mutations,
        Err(err) => err.into_inner(),
    };
    mutations.contains_key(&RemotePluginCacheMutationKey {
        plugin_cache_root: remote_plugin_cache_root(codex_home),
        marketplace_name: marketplace_name.to_string(),
        plugin_name: plugin_name.to_string(),
    })
}
