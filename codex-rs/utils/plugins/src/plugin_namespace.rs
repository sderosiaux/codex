//! Resolve plugin namespace from skill file paths by walking ancestors for `plugin.json`.

use codex_exec_server::EnvironmentAccess;
use codex_exec_server::EnvironmentAccessExt;
use codex_exec_server::GetMetadataOptions;
use codex_exec_server::ReadFileOptions;
use codex_exec_server_protocol::DISCOVERABLE_PLUGIN_MANIFEST_PATHS;
use codex_utils_path_uri::PathUri;
use std::path::Path;
use std::path::PathBuf;

pub const AGENT_PLUGIN_MANIFEST_RELATIVE_PATH: &str = "plugin.json";
/// Published Agent Plugins v1 manifest schema:
/// https://github.com/agentplugins/agent-plugins-spec/blob/main/schemas/1.0.0/plugin.schema.json
pub const AGENT_PLUGIN_SCHEMA_URI: &str =
    "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";
pub const SUPPORTED_AGENT_PLUGIN_SCHEMA_URIS: &[&str] = &[AGENT_PLUGIN_SCHEMA_URI];
pub const AGENT_PLUGIN_SCHEMA_PREFIX: &str = "https://agent-plugins.org/schemas/";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentPluginSchemaStatus {
    Supported,
    Unsupported,
    Unrelated,
}

pub fn agent_plugin_schema_status(contents: &str) -> AgentPluginSchemaStatus {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(contents) else {
        return AgentPluginSchemaStatus::Unrelated;
    };
    let Some(schema) = value.get("$schema").and_then(serde_json::Value::as_str) else {
        return AgentPluginSchemaStatus::Unrelated;
    };
    if SUPPORTED_AGENT_PLUGIN_SCHEMA_URIS.contains(&schema) {
        AgentPluginSchemaStatus::Supported
    } else if schema.starts_with(AGENT_PLUGIN_SCHEMA_PREFIX) {
        AgentPluginSchemaStatus::Unsupported
    } else {
        AgentPluginSchemaStatus::Unrelated
    }
}

pub fn find_plugin_manifest_path(plugin_root: &Path) -> Option<PathBuf> {
    let agent_manifest_path = plugin_root.join(AGENT_PLUGIN_MANIFEST_RELATIVE_PATH);
    match std::fs::symlink_metadata(&agent_manifest_path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.file_type().is_file() => {
            return None;
        }
        Ok(_) => {
            if std::fs::read_to_string(&agent_manifest_path)
                .ok()
                .is_some_and(|contents| {
                    agent_plugin_schema_status(&contents) != AgentPluginSchemaStatus::Unrelated
                })
            {
                return Some(agent_manifest_path);
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }

    for relative_path in DISCOVERABLE_PLUGIN_MANIFEST_PATHS {
        let manifest_path = plugin_root.join(relative_path);
        let manifest_parent = manifest_path.parent()?;
        match std::fs::symlink_metadata(manifest_parent) {
            Ok(metadata) if !metadata.file_type().is_dir() => return None,
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        }
        match std::fs::symlink_metadata(&manifest_path) {
            Ok(metadata) if metadata.file_type().is_file() => return Some(manifest_path),
            Ok(_) => return None,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    None
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPluginManifestName {
    #[serde(default)]
    name: String,
}

/// Returns the plugin manifest `name` defined directly below `plugin_root`.
pub async fn plugin_namespace_for_root_uri(
    fs: &dyn EnvironmentAccess,
    plugin_root: &PathUri,
) -> Option<String> {
    let mut manifest_path = None;
    for relative_path in DISCOVERABLE_PLUGIN_MANIFEST_PATHS {
        let candidate = plugin_root.join(relative_path).ok()?;
        match fs
            .get_metadata(&candidate, GetMetadataOptions::default())
            .await
        {
            Ok(metadata) if metadata.is_file => {
                manifest_path = Some(candidate);
                break;
            }
            Ok(_) | Err(_) => {}
        }
    }
    let contents = fs
        .read_file_text(&manifest_path?, ReadFileOptions::default())
        .await
        .ok()?;
    let RawPluginManifestName { name: raw_name } = serde_json::from_str(&contents).ok()?;
    Some(
        plugin_root
            .basename()
            .filter(|_| raw_name.trim().is_empty())
            .unwrap_or(raw_name),
    )
}
