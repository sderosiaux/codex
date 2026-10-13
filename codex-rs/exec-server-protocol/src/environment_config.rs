use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use serde::Serialize;

pub const ENVIRONMENT_CONFIG_READ_METHOD: &str = "environmentConfig/read";

/// Selects executor-local config fields by literal TOML key path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentConfigReadParams {
    pub cwd: PathUri,
    pub config_paths: Vec<Vec<String>>,
}

/// Executor-local config layers selected for one environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentConfigReadResponse {
    /// Executor user home used to expand `~` in path-bearing values.
    pub user_home_dir: Option<PathUri>,
    pub config: EnvironmentConfigLayerStack,
}

/// One ordered set of selected TOML layers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentConfigLayerStack {
    /// Layers ordered from lowest to highest precedence.
    pub layers: Vec<EnvironmentConfigLayer>,
}

/// One selected executor-local TOML layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentConfigLayer {
    /// Opaque provenance for diagnostics. Callers must not derive behavior from it.
    pub source: String,
    /// Directory used to interpret relative paths in this layer.
    pub base_dir: PathUri,
    /// Selected raw TOML. Path-bearing values have not been normalized.
    pub toml: String,
}
