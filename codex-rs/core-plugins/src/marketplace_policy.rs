use crate::OPENAI_API_CURATED_MARKETPLACE_NAME;
use crate::OPENAI_BUNDLED_ALPHA_MARKETPLACE_NAME;
use crate::OPENAI_BUNDLED_MARKETPLACE_NAME;
use crate::OPENAI_CURATED_MARKETPLACE_NAME;
use crate::OPENAI_PRIMARY_RUNTIME_MARKETPLACE_NAME;
use crate::marketplace::marketplace_root_dir;
use crate::marketplace_add::MarketplaceSource;
use crate::remote::RemotePluginScope;
use crate::startup_sync::curated_plugins_api_marketplace_path;
use crate::startup_sync::curated_plugins_repo_path;
use codex_config::ConfigLayerStack;
use codex_config::types::PluginConfig;
use codex_config::types::PluginsConfigToml;
use codex_plugin::PluginId;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

/// Validates that a marketplace may be installed from its path: OpenAI-managed
/// locations must carry their managed name, and reserved names cannot be loaded
/// from any other location.
pub(crate) fn validate_marketplace_install(
    codex_home: &Path,
    marketplace_path: &AbsolutePathBuf,
    marketplace_name: &str,
) -> Result<(), String> {
    let root = marketplace_root_dir(marketplace_path).map_err(|err| err.to_string())?;
    if let Some(expected_name) = managed_marketplace_name(codex_home, marketplace_path, &root) {
        return validate_expected_marketplace_name(expected_name, marketplace_name);
    }
    if is_reserved_marketplace_name(marketplace_name) {
        return Err(format!(
            "marketplace `{marketplace_name}` is reserved and cannot be loaded from this source"
        ));
    }
    Ok(())
}

/// Drops configured marketplaces that impersonate OpenAI-managed names, along
/// with the plugins that reference them.
pub(crate) fn filtered_plugin_config(
    config_layer_stack: &ConfigLayerStack,
    codex_home: &Path,
) -> Option<toml::Value> {
    let mut effective_config = config_layer_stack.effective_config();
    if effective_config.get("plugins").is_none() && effective_config.get("marketplaces").is_none() {
        return None;
    }
    let allowed_marketplace_names =
        allowed_configured_marketplace_names_in(&effective_config, codex_home);
    let configured_marketplace_names = effective_config
        .get("marketplaces")
        .and_then(toml::Value::as_table)
        .map(|marketplaces| marketplaces.keys().cloned().collect::<HashSet<_>>())
        .unwrap_or_default();

    if let Some(marketplaces) = effective_config
        .get_mut("marketplaces")
        .and_then(toml::Value::as_table_mut)
    {
        marketplaces
            .retain(|marketplace_name, _| allowed_marketplace_names.contains(marketplace_name));
    }
    if let Some(plugins) = effective_config
        .get_mut("plugins")
        .and_then(toml::Value::as_table_mut)
    {
        plugins.retain(|plugin_key, _| {
            let Ok(plugin_id) = PluginId::parse(plugin_key) else {
                return true;
            };
            !configured_marketplace_names.contains(&plugin_id.marketplace_name)
                || allowed_marketplace_names.contains(&plugin_id.marketplace_name)
        });
    }
    Some(effective_config)
}

pub fn allowed_configured_marketplace_names(
    config_layer_stack: &ConfigLayerStack,
    codex_home: &Path,
) -> HashSet<String> {
    allowed_configured_marketplace_names_in(&config_layer_stack.effective_config(), codex_home)
}

fn allowed_configured_marketplace_names_in(
    effective_config: &toml::Value,
    codex_home: &Path,
) -> HashSet<String> {
    let Some(marketplaces) = effective_config
        .get("marketplaces")
        .and_then(toml::Value::as_table)
    else {
        return HashSet::new();
    };
    marketplaces
        .iter()
        .filter_map(|(marketplace_name, marketplace)| {
            let allowed = match managed_marketplace_config_name(codex_home, marketplace) {
                Some(expected_name) => expected_name == marketplace_name,
                None => !is_reserved_marketplace_name(marketplace_name),
            };
            allowed.then(|| marketplace_name.clone())
        })
        .collect()
}

pub(crate) fn configured_plugins_from_stack(
    config_layer_stack: &ConfigLayerStack,
    codex_home: &Path,
) -> HashMap<String, PluginConfig> {
    let Some(effective_config) = filtered_plugin_config(config_layer_stack, codex_home) else {
        return HashMap::new();
    };
    let Some(plugins_value) = effective_config.get("plugins") else {
        return HashMap::new();
    };
    match PluginsConfigToml::deserialize(plugins_value.clone()) {
        Ok(plugins) => plugins.plugins,
        Err(err) => {
            tracing::warn!("invalid plugins config: {err}");
            HashMap::new()
        }
    }
}

/// Returns the managed name a local OpenAI-managed marketplace source must use.
pub(crate) fn managed_marketplace_name_for_add(
    codex_home: &Path,
    source: &MarketplaceSource,
) -> Option<&'static str> {
    match source {
        MarketplaceSource::Local { path } => managed_local_marketplace_name(codex_home, path),
        MarketplaceSource::Git { .. } => None,
    }
}

pub(crate) fn validate_marketplace_name_for_add(
    expected_name: Option<&'static str>,
    marketplace_name: &str,
) -> Result<(), String> {
    if let Some(expected_name) = expected_name {
        return validate_expected_marketplace_name(expected_name, marketplace_name);
    }
    if is_reserved_marketplace_name(marketplace_name) {
        return Err(format!(
            "marketplace `{marketplace_name}` is reserved and cannot be added from this source"
        ));
    }
    Ok(())
}

fn validate_expected_marketplace_name(
    expected_name: &str,
    marketplace_name: &str,
) -> Result<(), String> {
    (marketplace_name == expected_name)
        .then_some(())
        .ok_or_else(|| {
            format!(
                "marketplace manifest name `{marketplace_name}` does not match managed marketplace `{expected_name}`"
            )
        })
}

fn is_reserved_marketplace_name(marketplace_name: &str) -> bool {
    matches!(
        marketplace_name,
        OPENAI_CURATED_MARKETPLACE_NAME
            | OPENAI_API_CURATED_MARKETPLACE_NAME
            | OPENAI_BUNDLED_MARKETPLACE_NAME
            | OPENAI_BUNDLED_ALPHA_MARKETPLACE_NAME
            | OPENAI_PRIMARY_RUNTIME_MARKETPLACE_NAME
    ) || RemotePluginScope::from_marketplace_name(marketplace_name).is_some()
}

fn managed_marketplace_name(
    codex_home: &Path,
    marketplace_path: &AbsolutePathBuf,
    root: &AbsolutePathBuf,
) -> Option<&'static str> {
    if is_expected_managed_path(
        marketplace_path.as_path(),
        &curated_plugins_api_marketplace_path(codex_home),
    ) && is_expected_managed_path(root.as_path(), &curated_plugins_repo_path(codex_home))
    {
        return Some(OPENAI_API_CURATED_MARKETPLACE_NAME);
    }
    if is_expected_managed_path(root.as_path(), &curated_plugins_repo_path(codex_home)) {
        return Some(OPENAI_CURATED_MARKETPLACE_NAME);
    }
    managed_local_marketplace_name(codex_home, root.as_path())
}

fn managed_marketplace_config_name(
    codex_home: &Path,
    marketplace: &toml::Value,
) -> Option<&'static str> {
    if marketplace.get("source_type").and_then(toml::Value::as_str) != Some("local") {
        return None;
    }
    let path = marketplace
        .get("source")
        .and_then(toml::Value::as_str)
        .map(Path::new)
        .filter(|path| path.is_absolute())?;
    managed_local_marketplace_name(codex_home, path)
}

fn managed_local_marketplace_name(codex_home: &Path, root: &Path) -> Option<&'static str> {
    for marketplace_name in [
        OPENAI_BUNDLED_MARKETPLACE_NAME,
        OPENAI_BUNDLED_ALPHA_MARKETPLACE_NAME,
    ] {
        let expected_root = codex_home
            .join(".tmp/bundled-marketplaces")
            .join(marketplace_name);
        if is_expected_managed_path(root, &expected_root) {
            return Some(marketplace_name);
        }
    }

    let runtime_root = primary_runtime_marketplace_root()?;
    is_expected_managed_path(root, &runtime_root).then_some(OPENAI_PRIMARY_RUNTIME_MARKETPLACE_NAME)
}

fn is_expected_managed_path(path: &Path, expected: &Path) -> bool {
    if path == expected {
        return true;
    }
    #[cfg(target_os = "macos")]
    {
        path.strip_prefix("/private/var")
            .is_ok_and(|suffix| Path::new("/var").join(suffix) == expected)
            || expected
                .strip_prefix("/private/var")
                .is_ok_and(|suffix| Path::new("/var").join(suffix) == path)
    }
    #[cfg(windows)]
    {
        let path = path.to_string_lossy();
        let path = codex_utils_absolute_path::normalize_windows_device_path(&path)
            .unwrap_or_else(|| path.into_owned())
            .replace('/', "\\");
        // Resolve only the owned root; arbitrary aliases must not borrow managed provenance.
        let canonical_expected = expected.canonicalize().ok();
        std::iter::once(expected)
            .chain(canonical_expected.as_deref())
            .any(|expected| {
                let expected = expected.to_string_lossy();
                let expected = codex_utils_absolute_path::normalize_windows_device_path(&expected)
                    .unwrap_or_else(|| expected.into_owned());
                path.eq_ignore_ascii_case(&expected.replace('/', "\\"))
            })
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        false
    }
}

pub(crate) fn primary_runtime_marketplace_root() -> Option<PathBuf> {
    Some(
        primary_runtime_cache_dir()?
            .join("codex-runtimes/codex-primary-runtime/plugins")
            .join(OPENAI_PRIMARY_RUNTIME_MARKETPLACE_NAME),
    )
}

#[cfg(target_os = "windows")]
fn primary_runtime_cache_dir() -> Option<PathBuf> {
    primary_runtime_cache_dir_from_user_profile(std::env::var_os("USERPROFILE").map(PathBuf::from))
}

#[cfg(target_os = "windows")]
fn primary_runtime_cache_dir_from_user_profile(user_profile: Option<PathBuf>) -> Option<PathBuf> {
    user_profile.map(|profile| profile.join(".cache"))
}

#[cfg(target_os = "macos")]
fn primary_runtime_cache_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".cache"))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn primary_runtime_cache_dir() -> Option<PathBuf> {
    dirs::cache_dir()
}

#[cfg(test)]
#[path = "marketplace_policy_tests.rs"]
mod tests;
