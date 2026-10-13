use crate::path_utils::resolve_symlink_write_paths;
use crate::path_utils::write_atomically;
use anyhow::Context;
use codex_config::CONFIG_TOML_FILE;
use codex_config::is_structured_feature_path;
use codex_config::types::McpServerConfig;
use codex_features::FEATURES;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::openai_models::ReasoningEffort;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use tokio::task;
use toml_edit::ArrayOfTables;
use toml_edit::DocumentMut;
use toml_edit::Item as TomlItem;
use toml_edit::Table as TomlTable;
use toml_edit::value;

mod document_helpers;

/// Discrete config mutations supported by the persistence engine.
#[derive(Clone, Debug)]
pub enum ConfigEdit {
    /// Update the active (or default) model selection and optional reasoning effort.
    SetModel {
        model: Option<String>,
        effort: Option<ReasoningEffort>,
    },
    /// Update the service tier preference for future turns.
    SetServiceTier { service_tier: Option<String> },
    /// Replace the entire `[mcp_servers]` table.
    ReplaceMcpServers(BTreeMap<String, McpServerConfig>),
    /// Set or clear a skill config entry under `[[skills.config]]` by path.
    SetSkillConfig { path: PathBuf, enabled: bool },
    /// Set or clear a skill config entry under `[[skills.config]]` by name.
    SetSkillConfigByName { name: String, enabled: bool },
    /// Set the value stored at the exact dotted path.
    SetPath {
        segments: Vec<String>,
        value: TomlItem,
    },
    /// Remove the value stored at the exact dotted path.
    ClearPath { segments: Vec<String> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SkillConfigSelector {
    Name(String),
    Path(PathBuf),
}

struct ConfigDocument {
    doc: DocumentMut,
}

#[derive(Copy, Clone)]
enum TraversalMode {
    Create,
    Existing,
}

impl ConfigDocument {
    fn new(doc: DocumentMut) -> Self {
        Self { doc }
    }

    fn apply(&mut self, edit: &ConfigEdit) -> anyhow::Result<bool> {
        match edit {
            ConfigEdit::SetModel { model, effort } => Ok({
                let mut mutated = false;
                mutated |= self.write_optional_value(
                    &["model"],
                    model.as_ref().map(|model_value| value(model_value.clone())),
                );
                mutated |= self.write_optional_value(
                    &["model_reasoning_effort"],
                    effort.as_ref().map(|effort| value(effort.to_string())),
                );
                mutated
            }),
            ConfigEdit::SetServiceTier { service_tier } => Ok(self.write_optional_value(
                &["service_tier"],
                service_tier.as_ref().map(|service_tier| {
                    // Keep the legacy config spelling stable. Runtime values use
                    // `priority`, but config.toml continues to store it as `fast`.
                    let config_value = match ServiceTier::from_request_value(service_tier) {
                        Some(ServiceTier::Fast) => "fast",
                        Some(ServiceTier::Flex) => "flex",
                        None => service_tier.as_str(),
                    };
                    value(config_value)
                }),
            )),
            ConfigEdit::ReplaceMcpServers(servers) => self.replace_mcp_servers(servers),
            ConfigEdit::SetSkillConfig { path, enabled } => {
                Ok(self.set_skill_config(SkillConfigSelector::Path(path.clone()), *enabled))
            }
            ConfigEdit::SetSkillConfigByName { name, enabled } => {
                Ok(self.set_skill_config(SkillConfigSelector::Name(name.clone()), *enabled))
            }
            ConfigEdit::SetPath { segments, value } => {
                if is_structured_feature_path(segments) && value.as_bool().is_some() {
                    let mut existing = Some(self.doc.as_item());
                    for segment in segments {
                        existing = existing.and_then(|item| item.as_table_like()?.get(segment));
                    }
                    if existing.and_then(TomlItem::as_table_like).is_some() {
                        let mut enabled_segments = segments.clone();
                        enabled_segments.push("enabled".to_string());
                        return Ok(self.insert(&enabled_segments, value.clone()));
                    }
                }
                Ok(self.insert(segments, value.clone()))
            }
            ConfigEdit::ClearPath { segments } => Ok(self.clear_owned(segments)),
        }
    }

    fn write_optional_value(&mut self, segments: &[&str], value: Option<TomlItem>) -> bool {
        match value {
            Some(item) => self.write_value(segments, item),
            None => self.clear(segments),
        }
    }

    fn write_value(&mut self, segments: &[&str], value: TomlItem) -> bool {
        let resolved = segments
            .iter()
            .map(|segment| (*segment).to_string())
            .collect::<Vec<_>>();
        self.insert(&resolved, value)
    }

    fn clear(&mut self, segments: &[&str]) -> bool {
        let resolved = segments
            .iter()
            .map(|segment| (*segment).to_string())
            .collect::<Vec<_>>();
        self.remove(&resolved)
    }

    fn clear_owned(&mut self, segments: &[String]) -> bool {
        self.remove(segments)
    }

    fn replace_mcp_servers(
        &mut self,
        servers: &BTreeMap<String, McpServerConfig>,
    ) -> anyhow::Result<bool> {
        if servers.is_empty() {
            return Ok(self.clear(&["mcp_servers"]));
        }

        let root = self.doc.as_table_mut();
        if !root.contains_key("mcp_servers") {
            root.insert(
                "mcp_servers",
                TomlItem::Table(document_helpers::new_implicit_table()),
            );
        }

        let Some(item) = root.get_mut("mcp_servers") else {
            return Ok(false);
        };

        if document_helpers::ensure_table_for_write(item).is_none() {
            *item = TomlItem::Table(document_helpers::new_implicit_table());
        }

        let Some(table) = item.as_table_mut() else {
            return Ok(false);
        };

        let keys_to_remove: Vec<String> = table
            .iter()
            .map(|(key, _)| key.to_string())
            .filter(|key| !servers.contains_key(key.as_str()))
            .collect();

        for key in keys_to_remove {
            table.remove(&key);
        }

        for (name, config) in servers {
            if let Some(existing) = table.get_mut(name.as_str()) {
                if let TomlItem::Value(value) = existing
                    && let Some(inline) = value.as_inline_table_mut()
                {
                    let replacement = document_helpers::serialize_mcp_server_inline(config)?;
                    document_helpers::merge_inline_table(inline, replacement);
                } else {
                    *existing = document_helpers::serialize_mcp_server(config)?;
                }
            } else {
                table.insert(name, document_helpers::serialize_mcp_server(config)?);
            }
        }

        Ok(true)
    }

    fn set_skill_config(&mut self, selector: SkillConfigSelector, enabled: bool) -> bool {
        let selector = match selector {
            SkillConfigSelector::Name(name) => SkillConfigSelector::Name(name.trim().to_string()),
            SkillConfigSelector::Path(path) => {
                SkillConfigSelector::Path(PathBuf::from(normalize_skill_config_path(&path)))
            }
        };
        if matches!(&selector, SkillConfigSelector::Name(name) if name.is_empty()) {
            return false;
        }
        let mut remove_skills_table = false;
        let mut mutated = false;

        {
            let root = self.doc.as_table_mut();
            let skills_item = match root.get_mut("skills") {
                Some(item) => item,
                None => {
                    if enabled {
                        return false;
                    }
                    root.insert(
                        "skills",
                        TomlItem::Table(document_helpers::new_implicit_table()),
                    );
                    let Some(item) = root.get_mut("skills") else {
                        return false;
                    };
                    item
                }
            };

            if document_helpers::ensure_table_for_write(skills_item).is_none() {
                if enabled {
                    return false;
                }
                *skills_item = TomlItem::Table(document_helpers::new_implicit_table());
            }
            let Some(skills_table) = skills_item.as_table_mut() else {
                return false;
            };

            let config_item = match skills_table.get_mut("config") {
                Some(item) => item,
                None => {
                    if enabled {
                        return false;
                    }
                    skills_table.insert("config", TomlItem::ArrayOfTables(ArrayOfTables::new()));
                    let Some(item) = skills_table.get_mut("config") else {
                        return false;
                    };
                    item
                }
            };

            if !matches!(config_item, TomlItem::ArrayOfTables(_)) {
                if enabled {
                    return false;
                }
                *config_item = TomlItem::ArrayOfTables(ArrayOfTables::new());
            }

            let TomlItem::ArrayOfTables(overrides) = config_item else {
                return false;
            };

            let existing_index = overrides.iter().enumerate().find_map(|(idx, table)| {
                skill_config_selector_from_table(table)
                    .filter(|value| value == &selector)
                    .map(|_| idx)
            });

            if enabled {
                if let Some(index) = existing_index {
                    overrides.remove(index);
                    mutated = true;
                    if overrides.is_empty() {
                        skills_table.remove("config");
                        if skills_table.is_empty() {
                            remove_skills_table = true;
                        }
                    }
                }
            } else if let Some(index) = existing_index {
                for (idx, table) in overrides.iter_mut().enumerate() {
                    if idx == index {
                        write_skill_config_selector(table, &selector);
                        table["enabled"] = value(false);
                        mutated = true;
                        break;
                    }
                }
            } else {
                let mut entry = TomlTable::new();
                entry.set_implicit(false);
                write_skill_config_selector(&mut entry, &selector);
                entry["enabled"] = value(false);
                overrides.push(entry);
                mutated = true;
            }
        }

        if remove_skills_table {
            let root = self.doc.as_table_mut();
            root.remove("skills");
        }

        mutated
    }

    fn insert(&mut self, segments: &[String], value: TomlItem) -> bool {
        let Some((last, parents)) = segments.split_last() else {
            return false;
        };

        let Some(parent) = self.descend(parents, TraversalMode::Create) else {
            return false;
        };

        let mut value = value;
        if let Some(existing) = parent.get(last) {
            Self::preserve_decor(existing, &mut value);
        }
        parent[last] = value;
        true
    }

    fn remove(&mut self, segments: &[String]) -> bool {
        let Some((last, parents)) = segments.split_last() else {
            return false;
        };

        let Some(parent) = self.descend(parents, TraversalMode::Existing) else {
            return false;
        };

        parent.remove(last).is_some()
    }

    fn descend(&mut self, segments: &[String], mode: TraversalMode) -> Option<&mut TomlTable> {
        let mut current = self.doc.as_table_mut();

        for (index, segment) in segments.iter().enumerate() {
            match mode {
                TraversalMode::Create => {
                    if !current.contains_key(segment.as_str()) {
                        current.insert(
                            segment.as_str(),
                            TomlItem::Table(document_helpers::new_implicit_table()),
                        );
                    }

                    let item = current.get_mut(segment.as_str())?;
                    if is_structured_feature_path(&segments[..=index])
                        && let Some(enabled) = item.as_bool()
                    {
                        let mut feature = document_helpers::new_implicit_table();
                        feature.insert("enabled", value(enabled));
                        *item = TomlItem::Table(feature);
                    }
                    current = document_helpers::ensure_table_for_write(item)?;
                }
                TraversalMode::Existing => {
                    let item = current.get_mut(segment.as_str())?;
                    current = document_helpers::ensure_table_for_read(item)?;
                }
            }
        }

        Some(current)
    }

    fn preserve_decor(existing: &TomlItem, replacement: &mut TomlItem) {
        match (existing, replacement) {
            (TomlItem::Table(existing_table), TomlItem::Table(replacement_table)) => {
                replacement_table
                    .decor_mut()
                    .clone_from(existing_table.decor());
                for (key, existing_item) in existing_table.iter() {
                    if let (Some(existing_key), Some(mut replacement_key)) =
                        (existing_table.key(key), replacement_table.key_mut(key))
                    {
                        replacement_key
                            .leaf_decor_mut()
                            .clone_from(existing_key.leaf_decor());
                        replacement_key
                            .dotted_decor_mut()
                            .clone_from(existing_key.dotted_decor());
                    }
                    if let Some(replacement_item) = replacement_table.get_mut(key) {
                        Self::preserve_decor(existing_item, replacement_item);
                    }
                }
            }
            (TomlItem::Value(existing_value), TomlItem::Value(replacement_value)) => {
                replacement_value
                    .decor_mut()
                    .clone_from(existing_value.decor());
            }
            _ => {}
        }
    }
}

fn normalize_skill_config_path(path: &Path) -> String {
    dunce::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

fn skill_config_selector_from_table(table: &TomlTable) -> Option<SkillConfigSelector> {
    let path = table
        .get("path")
        .and_then(|item| item.as_str())
        .map(Path::new)
        .map(|path| SkillConfigSelector::Path(PathBuf::from(normalize_skill_config_path(path))));
    let name = table
        .get("name")
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| SkillConfigSelector::Name(name.to_string()));

    match (path, name) {
        (Some(selector), None) | (None, Some(selector)) => Some(selector),
        _ => None,
    }
}

fn write_skill_config_selector(table: &mut TomlTable, selector: &SkillConfigSelector) {
    match selector {
        SkillConfigSelector::Name(name) => {
            table.remove("path");
            table["name"] = value(name.clone());
        }
        SkillConfigSelector::Path(path) => {
            table.remove("name");
            table["path"] = value(path.to_string_lossy().to_string());
        }
    }
}

/// Persist edits using a blocking strategy.
pub fn apply_blocking(codex_home: &Path, edits: &[ConfigEdit]) -> anyhow::Result<()> {
    let config_path = codex_home.join(CONFIG_TOML_FILE);
    apply_blocking_to_resolved_file(&config_path, edits)
}

fn apply_blocking_to_resolved_file(
    resolved_config_file: &Path,
    edits: &[ConfigEdit],
) -> anyhow::Result<()> {
    if edits.is_empty() {
        return Ok(());
    }

    let write_paths = resolve_symlink_write_paths(resolved_config_file)?;
    let serialized = match write_paths.read_path {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err.into()),
        },
        None => String::new(),
    };

    let doc = if serialized.is_empty() {
        DocumentMut::new()
    } else {
        serialized.parse::<DocumentMut>()?
    };

    let mut document = ConfigDocument::new(doc);
    let mut mutated = false;

    for edit in edits {
        mutated |= document.apply(edit)?;
    }

    if !mutated {
        return Ok(());
    }

    write_atomically(&write_paths.write_path, &document.doc.to_string()).with_context(|| {
        format!(
            "failed to persist config at {}",
            write_paths.write_path.display()
        )
    })?;

    Ok(())
}

/// Fluent builder to batch config edits and apply them atomically.
#[derive(Default)]
pub struct ConfigEditsBuilder {
    config_path: PathBuf,
    edits: Vec<ConfigEdit>,
}

impl ConfigEditsBuilder {
    pub fn new(codex_home: &Path) -> Self {
        Self::for_config_path(&codex_home.join(CONFIG_TOML_FILE))
    }

    pub fn for_config(config: &crate::config::Config) -> Self {
        let config_path = config
            .config_layer_stack
            .get_user_config_file()
            .map(codex_utils_absolute_path::AbsolutePathBuf::to_path_buf)
            .unwrap_or_else(|| config.codex_home.join(CONFIG_TOML_FILE).to_path_buf());
        Self::for_config_path(&config_path)
    }

    pub fn for_config_path(config_path: &Path) -> Self {
        Self {
            config_path: config_path.to_path_buf(),
            edits: Vec::new(),
        }
    }

    pub fn set_model(mut self, model: Option<&str>, effort: Option<ReasoningEffort>) -> Self {
        self.edits.push(ConfigEdit::SetModel {
            model: model.map(ToOwned::to_owned),
            effort,
        });
        self
    }

    pub fn set_service_tier(mut self, service_tier: Option<String>) -> Self {
        self.edits.push(ConfigEdit::SetServiceTier { service_tier });
        self
    }

    pub fn replace_mcp_servers(mut self, servers: &BTreeMap<String, McpServerConfig>) -> Self {
        self.edits
            .push(ConfigEdit::ReplaceMcpServers(servers.clone()));
        self
    }

    /// Enable or disable a feature flag by key under the `[features]` table.
    ///
    /// Disabling a default-false feature clears the key instead of
    /// persisting `false`, so the config does not pin the feature once it
    /// graduates to globally enabled. Structured multi-agent v2 settings are
    /// an exception: its explicit `enabled = false` preserves nested options.
    pub fn set_feature_enabled(mut self, key: &str, enabled: bool) -> Self {
        let mut segments = vec!["features".to_string(), key.to_string()];
        if key == "multi_agent_v2" && !enabled {
            segments.push("enabled".to_string());
            self.edits.push(ConfigEdit::SetPath {
                segments,
                value: value(false),
            });
            return self;
        }
        let is_default_false_feature = FEATURES
            .iter()
            .find(|spec| spec.key == key)
            .is_some_and(|spec| !spec.default_enabled);
        if enabled || !is_default_false_feature {
            self.edits.push(ConfigEdit::SetPath {
                segments,
                value: value(enabled),
            });
        } else {
            self.edits.push(ConfigEdit::ClearPath { segments });
        }
        self
    }

    pub fn with_edits<I>(mut self, edits: I) -> Self
    where
        I: IntoIterator<Item = ConfigEdit>,
    {
        self.edits.extend(edits);
        self
    }

    /// Apply edits on a blocking thread.
    pub fn apply_blocking(self) -> anyhow::Result<()> {
        apply_blocking_to_resolved_file(&self.config_path, &self.edits)
    }

    /// Apply edits asynchronously via a blocking offload.
    pub async fn apply(self) -> anyhow::Result<()> {
        task::spawn_blocking(move || {
            apply_blocking_to_resolved_file(&self.config_path, &self.edits)
        })
        .await
        .context("config persistence task panicked")?
    }
}
