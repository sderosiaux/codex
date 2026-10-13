use super::discover_project_layers;
use super::discover_project_root;
use super::load_config_toml_for_required_layer_raw;
use super::load_root_checkout_project_config;
use super::project_root_markers_from_config;
use super::system_config_toml_file_with_overrides;
use crate::CONFIG_TOML_FILE;
use crate::ConfigLayerSource;
use crate::LoaderOverrides;
use crate::default_project_root_markers;
use crate::merge_toml_values;
use codex_file_system::ExecutorFileSystem;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use toml::Value as TomlValue;

/// Executor-local configuration layers before schema-specific path resolution.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalConfigLayers {
    pub config: LocalTomlLayerStack<ConfigLayerSource>,
}

impl LocalConfigLayers {
    /// Retains only the requested TOML paths and drops empty layers.
    ///
    /// An empty path selects the entire document. RPC boundaries should reject
    /// that form if whole-document reads are not part of their contract.
    pub fn project(self, config_paths: &[Vec<String>]) -> Self {
        Self {
            config: self.config.project(config_paths),
        }
    }
}

/// One ordered set of executor-local TOML layers.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalTomlLayerStack<S> {
    /// Layers ordered from lowest to highest precedence.
    pub layers: Vec<LocalTomlLayer<S>>,
}

impl<S> LocalTomlLayerStack<S> {
    fn project(self, paths: &[Vec<String>]) -> Self {
        let selectors = SelectorNode::from_paths(paths);
        let layers = self
            .layers
            .into_iter()
            .filter_map(|layer| {
                project_toml(&layer.toml, &selectors).map(|toml| LocalTomlLayer { toml, ..layer })
            })
            .collect();
        Self { layers }
    }
}

/// One executor-local TOML source with the directory used to interpret its
/// relative paths.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalTomlLayer<S> {
    pub source: S,
    pub base_dir: AbsolutePathBuf,
    pub toml: TomlValue,
}

/// Loads the fixed executor-local configuration sources used by environment
/// config reads.
///
/// Selected profiles, session flags, and thread-provided layers are not
/// included. Project discovery uses the executor's system and base-user config.
pub async fn load_local_config_layers(
    fs: &dyn ExecutorFileSystem,
    codex_home: &Path,
    cwd: &AbsolutePathBuf,
) -> io::Result<LocalConfigLayers> {
    load_local_config_layers_with_overrides(fs, codex_home, cwd, &LoaderOverrides::default()).await
}

pub(super) async fn load_local_config_layers_with_overrides(
    fs: &dyn ExecutorFileSystem,
    codex_home: &Path,
    cwd: &AbsolutePathBuf,
    overrides: &LoaderOverrides,
) -> io::Result<LocalConfigLayers> {
    let codex_home = AbsolutePathBuf::from_absolute_path(codex_home)?;
    let system_file = system_config_toml_file_with_overrides(overrides)?;
    let system =
        load_config_toml_for_required_layer_raw(fs, &system_file, /*strict_config*/ false).await?;
    let user_file = codex_home.join(CONFIG_TOML_FILE);
    let user =
        load_config_toml_for_required_layer_raw(fs, &user_file, /*strict_config*/ false).await?;

    let mut discovery_config = TomlValue::Table(toml::map::Map::new());
    merge_toml_values(&mut discovery_config, &system.toml);
    merge_toml_values(&mut discovery_config, &user.toml);
    let project_root_markers = project_root_markers_from_config(&discovery_config)?
        .unwrap_or_else(default_project_root_markers);
    let project_root = discover_project_root(fs, cwd, &project_root_markers)
        .await?
        .unwrap_or_else(|| cwd.clone());
    let project_layers = discover_project_layers(
        fs,
        cwd,
        &project_root,
        codex_home.as_path(),
        /*strict_config*/ false,
    )
    .await?;

    let mut config_layers = vec![
        LocalTomlLayer {
            source: ConfigLayerSource::System { file: system_file },
            base_dir: system.base_dir,
            toml: system.toml,
        },
        LocalTomlLayer {
            source: ConfigLayerSource::User {
                file: user_file,
                profile: None,
            },
            base_dir: user.base_dir,
            toml: user.toml,
        },
    ];
    let checkout_root = super::find_git_checkout_root(fs, cwd).await;
    let repo_root = super::resolve_main_checkout_root(fs, checkout_root.as_ref()).await;
    for layer in project_layers {
        let hooks_config_folder = super::root_checkout_hooks_folder_for_dir(
            checkout_root.as_ref(),
            repo_root.as_ref(),
            &layer.dot_codex_folder,
        );
        let mut config = layer.config;
        if hooks_config_folder.is_some()
            && let Some(table) = config.as_table_mut()
        {
            table.remove("hooks");
        }
        config_layers.push(LocalTomlLayer {
            source: ConfigLayerSource::Project {
                dot_codex_folder: layer.dot_codex_folder.clone(),
            },
            base_dir: layer.dot_codex_folder,
            toml: config,
        });

        let Some(hooks_config_folder) = hooks_config_folder else {
            continue;
        };
        let root_config = load_root_checkout_project_config(fs, &hooks_config_folder).await?;
        let Some(hooks) = root_config.get("hooks") else {
            continue;
        };
        config_layers.push(LocalTomlLayer {
            source: ConfigLayerSource::Project {
                dot_codex_folder: hooks_config_folder.clone(),
            },
            base_dir: hooks_config_folder,
            toml: TomlValue::Table(toml::map::Map::from_iter([(
                "hooks".to_string(),
                hooks.clone(),
            )])),
        });
    }

    Ok(LocalConfigLayers {
        config: LocalTomlLayerStack {
            layers: config_layers,
        },
    })
}

#[derive(Default)]
struct SelectorNode {
    terminal: bool,
    children: BTreeMap<String, SelectorNode>,
}

impl SelectorNode {
    fn from_paths(paths: &[Vec<String>]) -> Self {
        let mut root = Self::default();
        for path in paths {
            root.insert(path);
        }
        root
    }

    fn insert(&mut self, path: &[String]) {
        if self.terminal {
            return;
        }
        let Some((segment, remaining)) = path.split_first() else {
            self.terminal = true;
            self.children.clear();
            return;
        };
        self.children
            .entry(segment.clone())
            .or_default()
            .insert(remaining);
    }
}

fn project_toml(value: &TomlValue, selector: &SelectorNode) -> Option<TomlValue> {
    let projected = project_toml_value(value, selector);
    if !selector.terminal && projected.as_table().is_some_and(toml::map::Map::is_empty) {
        return None;
    }
    Some(projected)
}

fn project_toml_value(value: &TomlValue, selector: &SelectorNode) -> TomlValue {
    if selector.terminal {
        return value.clone();
    }
    let Some(table) = value.as_table() else {
        // Preserve a non-table ancestor so it can still override lower layers.
        return value.clone();
    };
    let mut projected = toml::map::Map::new();
    for (key, value) in table {
        let Some(child_selector) = selector.children.get(key) else {
            continue;
        };
        projected.insert(key.clone(), project_toml_value(value, child_selector));
    }
    TomlValue::Table(projected)
}
