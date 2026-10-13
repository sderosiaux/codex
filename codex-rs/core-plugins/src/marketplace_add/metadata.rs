use super::MarketplaceAddError;
use super::source::MarketplaceSource;
use crate::installed_marketplaces::resolve_configured_marketplace_root;
use crate::marketplace::validate_marketplace_root;
use codex_config::CONFIG_TOML_FILE;
use codex_config::MarketplaceConfigUpdate;
use codex_config::record_user_marketplace;
use codex_utils_path::paths_match_after_normalization;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MarketplaceInstallMetadata {
    source: InstalledMarketplaceSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InstalledMarketplaceSource {
    Git {
        url: String,
        ref_name: Option<String>,
        sparse_paths: Vec<String>,
    },
    Local {
        path: String,
    },
}

pub(super) fn record_added_marketplace_entry(
    codex_home: &Path,
    marketplace_name: &str,
    install_metadata: &MarketplaceInstallMetadata,
) -> Result<(), MarketplaceAddError> {
    let source = install_metadata.config_source();
    let update = MarketplaceConfigUpdate {
        source_type: install_metadata.config_source_type(),
        source: &source,
        ref_name: install_metadata.ref_name(),
        sparse_paths: install_metadata.sparse_paths(),
    };

    record_user_marketplace(codex_home, marketplace_name, &update).map_err(|err| {
        MarketplaceAddError::Internal(format!(
            "failed to add marketplace '{marketplace_name}' to user config.toml: {err}"
        ))
    })
}

pub(super) fn installed_marketplace_root_for_source(
    codex_home: &Path,
    install_root: &Path,
    install_metadata: &MarketplaceInstallMetadata,
) -> Result<Option<PathBuf>, MarketplaceAddError> {
    let config_path = codex_home.join(CONFIG_TOML_FILE);
    let config = match fs::read_to_string(&config_path) {
        Ok(config) => config,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(MarketplaceAddError::Internal(format!(
                "failed to read user config {}: {err}",
                config_path.display()
            )));
        }
    };
    let config: toml::Value = toml::from_str(&config).map_err(|err| {
        MarketplaceAddError::Internal(format!(
            "failed to parse user config {}: {err}",
            config_path.display()
        ))
    })?;
    let Some(marketplaces) = config.get("marketplaces").and_then(toml::Value::as_table) else {
        return Ok(None);
    };

    for (marketplace_name, marketplace) in marketplaces {
        if !install_metadata.matches_config(marketplace) {
            continue;
        }
        let root = match &install_metadata.source {
            // Keep using the resolved request path if the configured alias is retargeted.
            InstalledMarketplaceSource::Local { path } => PathBuf::from(path),
            InstalledMarketplaceSource::Git { .. } => install_root.join(marketplace_name),
        };
        if validate_marketplace_root(&root).is_ok() {
            return Ok(Some(root));
        }
    }

    Ok(None)
}

pub(super) fn find_marketplace_root_by_name(
    codex_home: &Path,
    install_root: &Path,
    marketplace_name: &str,
) -> Result<Option<PathBuf>, MarketplaceAddError> {
    let config_path = codex_home.join(CONFIG_TOML_FILE);
    let config = match fs::read_to_string(&config_path) {
        Ok(config) => config,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(MarketplaceAddError::Internal(format!(
                "failed to read user config {}: {err}",
                config_path.display()
            )));
        }
    };
    let config: toml::Value = toml::from_str(&config).map_err(|err| {
        MarketplaceAddError::Internal(format!(
            "failed to parse user config {}: {err}",
            config_path.display()
        ))
    })?;
    let Some(marketplace) = config
        .get("marketplaces")
        .and_then(toml::Value::as_table)
        .and_then(|marketplaces| marketplaces.get(marketplace_name))
    else {
        return Ok(None);
    };

    let Some(root) =
        resolve_configured_marketplace_root(marketplace_name, marketplace, install_root)
    else {
        return Ok(None);
    };
    if validate_marketplace_root(&root).is_ok() {
        Ok(Some(root))
    } else {
        Ok(None)
    }
}

impl MarketplaceInstallMetadata {
    pub(super) fn from_source(source: &MarketplaceSource, sparse_paths: &[String]) -> Self {
        let source = match source {
            MarketplaceSource::Git { url, ref_name } => InstalledMarketplaceSource::Git {
                url: url.clone(),
                ref_name: ref_name.clone(),
                sparse_paths: sparse_paths.to_vec(),
            },
            MarketplaceSource::Local { path } => InstalledMarketplaceSource::Local {
                path: path.display().to_string(),
            },
        };
        Self { source }
    }

    fn config_source_type(&self) -> &'static str {
        match &self.source {
            InstalledMarketplaceSource::Git { .. } => "git",
            InstalledMarketplaceSource::Local { .. } => "local",
        }
    }

    fn config_source(&self) -> String {
        match &self.source {
            InstalledMarketplaceSource::Git { url, .. } => url.clone(),
            InstalledMarketplaceSource::Local { path } => path.clone(),
        }
    }

    fn ref_name(&self) -> Option<&str> {
        match &self.source {
            InstalledMarketplaceSource::Git { ref_name, .. } => ref_name.as_deref(),
            InstalledMarketplaceSource::Local { .. } => None,
        }
    }

    fn sparse_paths(&self) -> &[String] {
        match &self.source {
            InstalledMarketplaceSource::Git { sparse_paths, .. } => sparse_paths,
            InstalledMarketplaceSource::Local { .. } => &[],
        }
    }

    fn matches_config(&self, marketplace: &toml::Value) -> bool {
        marketplace.get("source_type").and_then(toml::Value::as_str)
            == Some(self.config_source_type())
            && marketplace
                .get("source")
                .and_then(toml::Value::as_str)
                .is_some_and(|source| match &self.source {
                    InstalledMarketplaceSource::Local { path } => {
                        paths_match_after_normalization(source, path)
                    }
                    InstalledMarketplaceSource::Git { url, .. } => source == url,
                })
            && marketplace.get("ref").and_then(toml::Value::as_str) == self.ref_name()
            && config_sparse_paths(marketplace) == self.sparse_paths()
    }
}

fn config_sparse_paths(marketplace: &toml::Value) -> Vec<String> {
    marketplace
        .get("sparse_paths")
        .and_then(toml::Value::as_array)
        .map(|paths| {
            paths
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}
