use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

use codex_config::CONFIG_TOML_FILE;
use codex_config::ConfigLayerEntry;
use codex_config::ConfigLayerSource;
use codex_config::ConfigLayerStack;
use codex_config::HookEventsToml;
use codex_config::HookHandlerConfig;
use codex_config::HookStateToml;
use codex_config::HooksFile;
use codex_config::MatcherGroup;
use codex_plugin::PluginHookSource;
use codex_plugin::is_allowlisted_bundled_cleanup_hook;
use codex_protocol::protocol::HookEventName;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;

use super::ConfiguredHandler;
use super::ConfiguredHandlerKind;
use super::HookListEntry;
use super::HookListEntryHandler;
use super::dispatcher::hook_event_name_label;
use crate::config_rules::hook_states_from_stack;
use crate::engine::HookMatcher;
use crate::events::common::matcher_pattern_for_event;
use crate::events::session_end::SESSION_END_DEFAULT_TIMEOUT_SEC;
use crate::events::session_end::SESSION_END_MAX_TIMEOUT_SEC;
use crate::output_spill::AdditionalContextLimit;
use crate::output_spill::DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT;
use codex_protocol::protocol::HookSource;

pub(crate) struct DiscoveryResult {
    pub handlers: Vec<ConfiguredHandler>,
    pub hook_entries: Vec<HookListEntry>,
    pub warnings: Vec<String>,
}

struct HookHandlerSource<'a> {
    path: &'a AbsolutePathBuf,
    key_source: String,
    source: HookSource,
    hook_states: &'a HashMap<String, HookStateToml>,
    env: HashMap<String, String>,
    plugin_id: Option<String>,
}

struct NormalizedHandler {
    config: HookHandlerConfig,
    kind: ConfiguredHandlerKind,
    timeout_sec: u64,
    status_message: Option<String>,
    additional_context_limit: Option<usize>,
}

pub(crate) fn discover_handlers(
    config_layer_stack: Option<&ConfigLayerStack>,
    plugin_hook_sources: Vec<PluginHookSource>,
    plugin_hook_load_warnings: Vec<String>,
) -> DiscoveryResult {
    let mut handlers = Vec::new();
    let mut hook_entries = Vec::new();
    let mut warnings = plugin_hook_load_warnings;
    let mut display_order = 0_i64;
    let mut visited_json_hook_folders = HashSet::new();
    let hook_states = hook_states_from_stack(config_layer_stack);

    if let Some(config_layer_stack) = config_layer_stack {
        for layer in config_layer_stack.layers_low_to_high() {
            let hook_source = hook_source_for_config_layer_source(&layer.name);
            let json_hooks = match layer.hooks_config_folder() {
                Some(config_folder) if visited_json_hook_folders.insert(config_folder.clone()) => {
                    load_hooks_json(Some(config_folder.as_path()), &mut warnings)
                }
                _ => None,
            };
            let toml_hooks = load_toml_hooks_from_layer(layer, &mut warnings);

            if let (Some((json_source_path, json_events)), Some((toml_source_path, toml_events))) =
                (&json_hooks, &toml_hooks)
                && !json_events.is_empty()
                && !toml_events.is_empty()
            {
                warnings.push(format!(
                    "loading hooks from both {} and {}; prefer a single representation for this layer",
                    json_source_path.display(),
                    toml_source_path.display()
                ));
            }

            for (source_path, hook_events) in [json_hooks, toml_hooks].into_iter().flatten() {
                append_hook_events(
                    &mut handlers,
                    &mut hook_entries,
                    &mut warnings,
                    &mut display_order,
                    HookHandlerSource {
                        path: &source_path,
                        key_source: source_path.display().to_string(),
                        source: hook_source,
                        hook_states: &hook_states,
                        env: HashMap::new(),
                        plugin_id: None,
                    },
                    hook_events,
                );
            }
        }
    }

    append_plugin_hook_sources(
        &mut handlers,
        &mut hook_entries,
        &mut warnings,
        &mut display_order,
        plugin_hook_sources,
        &hook_states,
    );

    DiscoveryResult {
        handlers,
        hook_entries,
        warnings,
    }
}

fn append_plugin_hook_sources(
    handlers: &mut Vec<ConfiguredHandler>,
    hook_entries: &mut Vec<HookListEntry>,
    warnings: &mut Vec<String>,
    display_order: &mut i64,
    plugin_hook_sources: Vec<PluginHookSource>,
    hook_states: &HashMap<String, HookStateToml>,
) {
    for source in plugin_hook_sources {
        let PluginHookSource {
            plugin_root,
            plugin_id,
            plugin_data_root,
            source_path,
            source_relative_path,
            hooks,
        } = source;
        let mut env = HashMap::new();
        let plugin_root_value = plugin_root.display().to_string();
        let plugin_data_root_value = plugin_data_root.display().to_string();
        env.insert("PLUGIN_ROOT".to_string(), plugin_root_value.clone());
        // For OOTB compat with existing plugins that use this env var.
        env.insert("CLAUDE_PLUGIN_ROOT".to_string(), plugin_root_value);
        env.insert("PLUGIN_DATA".to_string(), plugin_data_root_value.clone());
        // For OOTB compat with existing plugins that use this env var.
        env.insert("CLAUDE_PLUGIN_DATA".to_string(), plugin_data_root_value);
        let plugin_id = plugin_id.as_key();
        append_hook_events(
            handlers,
            hook_entries,
            warnings,
            display_order,
            HookHandlerSource {
                path: &source_path,
                key_source: crate::declarations::plugin_hook_key_source(
                    plugin_id.as_str(),
                    source_relative_path.as_str(),
                ),
                source: HookSource::Plugin,
                hook_states,
                env,
                plugin_id: Some(plugin_id),
            },
            hooks,
        );
    }
}

fn load_hooks_json(
    config_folder: Option<&Path>,
    warnings: &mut Vec<String>,
) -> Option<(AbsolutePathBuf, HookEventsToml)> {
    let source_path = config_folder?.join("hooks.json");
    if !source_path.as_path().is_file() {
        return None;
    }

    let contents = match fs::read_to_string(source_path.as_path()) {
        Ok(contents) => contents,
        Err(err) => {
            warnings.push(format!(
                "failed to read hooks config {}: {err}",
                source_path.display()
            ));
            return None;
        }
    };

    let parsed: HooksFile = match serde_json::from_str(&contents) {
        Ok(parsed) => parsed,
        Err(err) => {
            warnings.push(format!(
                "failed to parse hooks config {}: {err}",
                source_path.display()
            ));
            return None;
        }
    };

    let source_path = AbsolutePathBuf::from_absolute_path(&source_path)
        .inspect_err(|err| {
            warnings.push(format!(
                "failed to normalize hooks config path {}: {err}",
                source_path.display()
            ));
        })
        .ok()?;

    (!parsed.hooks.is_empty()).then_some((source_path, parsed.hooks))
}

fn load_toml_hooks_from_layer(
    layer: &ConfigLayerEntry,
    warnings: &mut Vec<String>,
) -> Option<(AbsolutePathBuf, HookEventsToml)> {
    let source_path = config_toml_source_path(layer);
    let hook_value = layer.config.get("hooks")?.clone();
    let parsed = match HookEventsToml::deserialize(hook_value) {
        Ok(parsed) => parsed,
        Err(err) => {
            warnings.push(format!(
                "failed to parse TOML hooks in {}: {err}",
                source_path.display()
            ));
            return None;
        }
    };

    (!parsed.is_empty()).then_some((source_path, parsed))
}

fn config_toml_source_path(layer: &ConfigLayerEntry) -> AbsolutePathBuf {
    match &layer.name {
        ConfigLayerSource::PackagedDefaults { file }
        | ConfigLayerSource::System { file }
        | ConfigLayerSource::User { file, .. } => file.clone(),
        ConfigLayerSource::Project { dot_codex_folder } => layer
            .hooks_config_folder()
            .unwrap_or_else(|| dot_codex_folder.clone())
            .join(CONFIG_TOML_FILE),
        ConfigLayerSource::SessionFlags => synthetic_layer_path("<session-flags>/config.toml"),
    }
}

fn synthetic_layer_path(path: &str) -> AbsolutePathBuf {
    #[cfg(windows)]
    {
        AbsolutePathBuf::resolve_path_against_base(path, r"C:\")
    }

    #[cfg(not(windows))]
    {
        AbsolutePathBuf::resolve_path_against_base(path, "/")
    }
}

fn append_hook_events(
    handlers: &mut Vec<ConfiguredHandler>,
    hook_entries: &mut Vec<HookListEntry>,
    warnings: &mut Vec<String>,
    display_order: &mut i64,
    source: HookHandlerSource<'_>,
    hook_events: HookEventsToml,
) {
    for (event_name, groups) in hook_events.into_matcher_groups() {
        append_matcher_groups(
            handlers,
            hook_entries,
            warnings,
            display_order,
            &source,
            event_name,
            groups,
        );
    }
}

fn append_matcher_groups(
    handlers: &mut Vec<ConfiguredHandler>,
    hook_entries: &mut Vec<HookListEntry>,
    warnings: &mut Vec<String>,
    display_order: &mut i64,
    source: &HookHandlerSource<'_>,
    event_name: codex_protocol::protocol::HookEventName,
    groups: Vec<MatcherGroup>,
) {
    for (group_index, group) in groups.into_iter().enumerate() {
        let matcher = matcher_pattern_for_event(event_name, group.matcher.as_deref());
        let compiled_matcher = match matcher.map(HookMatcher::new).transpose() {
            Ok(matcher) => matcher,
            Err(err) => {
                let matcher = matcher.unwrap_or_default();
                let warning = format!(
                    "invalid matcher {matcher:?} in {}: {err}",
                    source.path.display()
                );
                warnings.push(warning);
                continue;
            }
        };
        for (handler_index, handler) in group.hooks.iter().cloned().enumerate() {
            let normalized = match handler {
                HookHandlerConfig::Command {
                    command,
                    command_windows,
                    timeout_sec,
                    r#async,
                    status_message,
                    additional_context_limit,
                } => {
                    let command = if cfg!(windows) {
                        command_windows.unwrap_or(command)
                    } else {
                        command
                    };
                    if command.trim().is_empty() {
                        warnings.push(format!(
                            "skipping empty hook command in {}",
                            source.path.display()
                        ));
                        continue;
                    }
                    let timeout_sec = normalize_command_hook(
                        event_name,
                        timeout_sec,
                        source.path.as_path(),
                        warnings,
                    );
                    let runs_async = r#async && event_name != HookEventName::SessionEnd;
                    if r#async && !runs_async {
                        warnings.push(format!(
                            "running async {} hook synchronously in {}",
                            hook_event_name_label(event_name),
                            source.path.display()
                        ));
                    }
                    let additional_context_limit = if matches!(
                        event_name,
                        codex_protocol::protocol::HookEventName::PreToolUse
                            | codex_protocol::protocol::HookEventName::PostToolUse
                            | codex_protocol::protocol::HookEventName::SessionStart
                            | codex_protocol::protocol::HookEventName::UserPromptSubmit
                            | codex_protocol::protocol::HookEventName::SubagentStart
                    ) {
                        additional_context_limit
                    } else {
                        if additional_context_limit.is_some() {
                            warnings.push(format!(
                                "ignoring additionalContextLimit for {event_name:?} hook in {}: this event cannot emit additionalContext",
                                source.path.display()
                            ));
                        }
                        None
                    };
                    let normalized_additional_context_limit = additional_context_limit
                        .filter(|limit| *limit != DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT);
                    let config = HookHandlerConfig::Command {
                        command: command.clone(),
                        command_windows: None,
                        timeout_sec: Some(timeout_sec),
                        r#async,
                        status_message: status_message.clone(),
                        additional_context_limit: normalized_additional_context_limit,
                    };
                    let command = source.env.iter().fold(command, |command, (key, value)| {
                        command.replace(&format!("${{{key}}}"), value)
                    });
                    NormalizedHandler {
                        config,
                        kind: ConfiguredHandlerKind::Command {
                            command,
                            env: source.env.clone(),
                            r#async: runs_async,
                        },
                        timeout_sec,
                        status_message,
                        additional_context_limit,
                    }
                }
                HookHandlerConfig::McpTool {
                    server,
                    tool,
                    input,
                    timeout_sec,
                    status_message,
                } => {
                    if event_name == HookEventName::SessionEnd {
                        warnings.push(format!(
                            "skipping MCP tool hook in {}: {} MCP hooks are not supported",
                            source.path.display(),
                            hook_event_name_label(event_name),
                        ));
                        continue;
                    }
                    if server.trim().is_empty() || tool.trim().is_empty() {
                        warnings.push(format!(
                            "skipping MCP tool hook in {}: server and tool must not be empty",
                            source.path.display()
                        ));
                        continue;
                    }
                    let timeout_sec = normalize_command_hook(
                        event_name,
                        timeout_sec,
                        source.path.as_path(),
                        warnings,
                    );
                    let config = HookHandlerConfig::McpTool {
                        server: server.clone(),
                        tool: tool.clone(),
                        input: input.clone(),
                        timeout_sec: Some(timeout_sec),
                        status_message: status_message.clone(),
                    };
                    NormalizedHandler {
                        config,
                        kind: ConfiguredHandlerKind::McpTool {
                            server,
                            tool,
                            input,
                        },
                        timeout_sec,
                        status_message,
                        additional_context_limit: None,
                    }
                }
                HookHandlerConfig::Prompt {} => {
                    warnings.push(format!(
                        "skipping prompt hook in {}: prompt hooks are not supported yet",
                        source.path.display()
                    ));
                    continue;
                }
                HookHandlerConfig::Agent {} => {
                    warnings.push(format!(
                        "skipping agent hook in {}: agent hooks are not supported yet",
                        source.path.display()
                    ));
                    continue;
                }
            };

            let NormalizedHandler {
                config,
                kind,
                timeout_sec,
                status_message,
                additional_context_limit,
            } = normalized;
            let key = crate::hook_key(&source.key_source, event_name, group_index, handler_index);
            let state = source.hook_states.get(&key);
            let builtin = source.plugin_id.as_deref().is_some_and(|plugin_id| {
                is_allowlisted_bundled_cleanup_hook(
                    plugin_id,
                    event_name,
                    group.matcher.as_deref(),
                    &config,
                    /*app_connector_id*/ None,
                )
            });
            let enabled = builtin || state.and_then(|state| state.enabled) != Some(false);
            let handler = match &kind {
                ConfiguredHandlerKind::Command {
                    command, r#async, ..
                } => HookListEntryHandler::Command {
                    command: command.clone(),
                    r#async: *r#async,
                },
                ConfiguredHandlerKind::McpTool { server, tool, .. } => {
                    HookListEntryHandler::McpTool {
                        server: server.clone(),
                        tool: tool.clone(),
                    }
                }
            };

            hook_entries.push(HookListEntry {
                builtin,
                key,
                event_name,
                handler,
                matcher: matcher.map(ToOwned::to_owned),
                timeout_sec,
                status_message: status_message.clone(),
                additional_context_limit,
                source_path: source.path.clone(),
                source: source.source,
                plugin_id: source.plugin_id.clone(),
                display_order: *display_order,
                enabled,
            });
            if enabled {
                handlers.push(ConfiguredHandler {
                    builtin,
                    event_name,
                    matcher: compiled_matcher.clone(),
                    timeout_sec,
                    status_message,
                    additional_context_limit: AdditionalContextLimit::from_config(
                        additional_context_limit,
                    ),
                    source_path: source.path.clone().into(),
                    source: source.source,
                    display_order: *display_order,
                    kind,
                });
            }
            *display_order += 1;
        }
    }
}

/// Normalizes hook timeouts. SessionEnd and Interrupt default to one second and are capped at three
/// seconds; all other hooks keep the standard ten-minute default.
fn normalize_command_hook(
    event_name: HookEventName,
    timeout_sec: Option<u64>,
    source_path: &Path,
    warnings: &mut Vec<String>,
) -> u64 {
    match event_name {
        HookEventName::SessionEnd | HookEventName::Interrupt => {
            let max_timeout_sec = SESSION_END_MAX_TIMEOUT_SEC;
            if timeout_sec.is_some_and(|timeout_sec| timeout_sec > max_timeout_sec) {
                warnings.push(format!(
                    "clamping {} hook timeout to {max_timeout_sec}s in {}",
                    hook_event_name_label(event_name),
                    source_path.display()
                ));
            }
            timeout_sec
                .unwrap_or(SESSION_END_DEFAULT_TIMEOUT_SEC)
                .clamp(1, max_timeout_sec)
        }
        _ => timeout_sec.unwrap_or(600).max(1),
    }
}

fn hook_source_for_config_layer_source(source: &ConfigLayerSource) -> HookSource {
    match source {
        ConfigLayerSource::PackagedDefaults { .. } => HookSource::Unknown,
        ConfigLayerSource::System { .. } => HookSource::System,
        ConfigLayerSource::User { .. } => HookSource::User,
        ConfigLayerSource::Project { .. } => HookSource::Project,
        ConfigLayerSource::SessionFlags => HookSource::SessionFlags,
    }
}

#[cfg(test)]
mod tests {
    use super::HookMatcher;
    use codex_config::ConfigLayerEntry;
    use codex_config::ConfigLayerSource;
    use codex_config::HookEventsToml;
    use codex_protocol::protocol::HookEventName;
    use codex_protocol::protocol::HookExecutionMode;
    use codex_protocol::protocol::HookSource;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use codex_utils_absolute_path::test_support::PathBufExt;
    use codex_utils_absolute_path::test_support::test_path_buf;
    use pretty_assertions::assert_eq;

    use super::ConfiguredHandler;
    use super::ConfiguredHandlerKind;
    use super::HookListEntry;
    use super::HookListEntryHandler;
    use super::append_matcher_groups;
    use super::normalize_command_hook;
    use crate::output_spill::AdditionalContextLimit;
    use crate::output_spill::DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT;
    use codex_config::HookHandlerConfig;
    use codex_config::HookStateToml;
    use codex_config::MatcherGroup;
    use codex_config::TomlValue;

    fn source_path() -> AbsolutePathBuf {
        test_path_buf("/tmp/hooks.json").abs()
    }

    fn hook_source() -> HookSource {
        HookSource::System
    }

    fn hook_handler_source<'a>(
        path: &'a AbsolutePathBuf,
        hook_states: &'a std::collections::HashMap<String, HookStateToml>,
    ) -> super::HookHandlerSource<'a> {
        super::HookHandlerSource {
            path,
            key_source: path.display().to_string(),
            source: hook_source(),
            hook_states,
            env: std::collections::HashMap::new(),
            plugin_id: None,
        }
    }

    fn user_hook_handler_source<'a>(
        path: &'a AbsolutePathBuf,
        hook_states: &'a std::collections::HashMap<String, HookStateToml>,
    ) -> super::HookHandlerSource<'a> {
        super::HookHandlerSource {
            path,
            key_source: path.display().to_string(),
            source: HookSource::User,
            hook_states,
            env: std::collections::HashMap::new(),
            plugin_id: None,
        }
    }

    fn command_group(matcher: Option<&str>) -> MatcherGroup {
        MatcherGroup {
            matcher: matcher.map(str::to_string),
            hooks: vec![HookHandlerConfig::Command {
                command: "echo hello".to_string(),
                command_windows: None,
                timeout_sec: None,
                r#async: false,
                status_message: None,
                additional_context_limit: None,
            }],
        }
    }

    fn command_group_with_additional_context_limit(
        additional_context_limit: usize,
    ) -> MatcherGroup {
        MatcherGroup {
            matcher: None,
            hooks: vec![HookHandlerConfig::Command {
                command: "echo hello".to_string(),
                command_windows: None,
                timeout_sec: None,
                r#async: false,
                status_message: None,
                additional_context_limit: Some(additional_context_limit),
            }],
        }
    }

    #[test]
    fn mcp_tool_hooks_preserve_argument_templates_and_list_their_target() {
        let source_path = source_path();
        let hook_states = std::collections::HashMap::new();
        let input = serde_json::from_value(serde_json::json!({
            "file_path": "${tool_input.file_path}",
            "optional": "${tool_input.optional}",
        }))
        .expect("MCP hook input should be an object");
        let mut handlers = Vec::new();
        let mut entries = Vec::new();
        let mut warnings = Vec::new();
        let mut display_order = 0;

        append_matcher_groups(
            &mut handlers,
            &mut entries,
            &mut warnings,
            &mut display_order,
            &hook_handler_source(&source_path, &hook_states),
            HookEventName::PostToolUse,
            vec![MatcherGroup {
                matcher: Some("Write|Edit".to_string()),
                hooks: vec![HookHandlerConfig::McpTool {
                    server: "security".to_string(),
                    tool: "scan".to_string(),
                    input,
                    timeout_sec: Some(30),
                    status_message: Some("Scanning file".to_string()),
                }],
            }],
        );

        assert!(warnings.is_empty());
        assert_eq!(handlers.len(), 1);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].handler,
            HookListEntryHandler::McpTool {
                server: "security".to_string(),
                tool: "scan".to_string(),
            }
        );
    }

    #[test]
    fn session_end_mcp_tool_hooks_are_warned_and_skipped() {
        let source_path = source_path();
        let hook_states = std::collections::HashMap::new();
        let mut handlers = Vec::new();
        let mut entries = Vec::new();
        let mut warnings = Vec::new();
        let mut display_order = 0;

        append_matcher_groups(
            &mut handlers,
            &mut entries,
            &mut warnings,
            &mut display_order,
            &hook_handler_source(&source_path, &hook_states),
            HookEventName::SessionEnd,
            vec![MatcherGroup {
                matcher: None,
                hooks: vec![HookHandlerConfig::McpTool {
                    server: "security".to_string(),
                    tool: "scan".to_string(),
                    input: serde_json::Map::new(),
                    timeout_sec: None,
                    status_message: None,
                }],
            }],
        );

        assert!(handlers.is_empty());
        assert!(entries.is_empty());
        assert_eq!(
            warnings,
            vec![format!(
                "skipping MCP tool hook in {}: SessionEnd MCP hooks are not supported",
                source_path.display()
            )]
        );
    }

    #[test]
    fn interrupt_mcp_tool_hooks_are_supported_and_timeout_is_clamped() {
        let source_path = source_path();
        let hook_states = std::collections::HashMap::new();
        let mut handlers = Vec::new();
        let mut entries = Vec::new();
        let mut warnings = Vec::new();
        let mut display_order = 0;

        append_matcher_groups(
            &mut handlers,
            &mut entries,
            &mut warnings,
            &mut display_order,
            &hook_handler_source(&source_path, &hook_states),
            HookEventName::Interrupt,
            vec![MatcherGroup {
                matcher: None,
                hooks: vec![
                    HookHandlerConfig::McpTool {
                        server: "security".to_string(),
                        tool: "scan".to_string(),
                        input: serde_json::Map::new(),
                        timeout_sec: None,
                        status_message: None,
                    },
                    HookHandlerConfig::McpTool {
                        server: "security".to_string(),
                        tool: "report".to_string(),
                        input: serde_json::Map::new(),
                        timeout_sec: Some(600),
                        status_message: None,
                    },
                ],
            }],
        );

        assert_eq!(
            handlers
                .iter()
                .map(|handler| handler.timeout_sec)
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.timeout_sec)
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert!(
            entries
                .iter()
                .all(|entry| matches!(entry.handler, HookListEntryHandler::McpTool { .. }))
        );
        assert_eq!(
            warnings,
            vec![format!(
                "clamping Interrupt hook timeout to 3s in {}",
                source_path.display()
            )]
        );
    }

    fn discover_command(
        event_name: HookEventName,
        additional_context_limit: Option<usize>,
    ) -> (ConfiguredHandler, HookListEntry, Vec<String>) {
        let source_path = source_path();
        let hook_states = std::collections::HashMap::new();
        let mut handlers = Vec::new();
        let mut entries = Vec::new();
        let mut warnings = Vec::new();
        let mut display_order = 0;
        append_matcher_groups(
            &mut handlers,
            &mut entries,
            &mut warnings,
            &mut display_order,
            &hook_handler_source(&source_path, &hook_states),
            event_name,
            vec![match additional_context_limit {
                Some(limit) => command_group_with_additional_context_limit(limit),
                None => command_group(/*matcher*/ None),
            }],
        );
        (handlers.remove(0), entries.remove(0), warnings)
    }

    #[test]
    fn supported_events_retain_per_handler_additional_context_limit() {
        for event_name in [
            HookEventName::PreToolUse,
            HookEventName::PostToolUse,
            HookEventName::SessionStart,
            HookEventName::UserPromptSubmit,
            HookEventName::SubagentStart,
        ] {
            let (explicit_default_handler, explicit_default_entry, _) = discover_command(
                event_name,
                /*additional_context_limit*/ Some(DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT),
            );
            let (custom_handler, custom_entry, _) =
                discover_command(event_name, /*additional_context_limit*/ Some(20_000));
            let (unlimited_handler, unlimited_entry, _) =
                discover_command(event_name, /*additional_context_limit*/ Some(0));

            assert_eq!(
                custom_handler.additional_context_limit,
                AdditionalContextLimit::from_config(Some(20_000))
            );
            assert_eq!(custom_entry.additional_context_limit, Some(20_000));
            assert_eq!(
                explicit_default_handler.additional_context_limit,
                AdditionalContextLimit::from_config(Some(DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT))
            );
            assert_eq!(
                explicit_default_entry.additional_context_limit,
                Some(DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT)
            );
            assert_eq!(
                unlimited_handler.additional_context_limit,
                AdditionalContextLimit::from_config(Some(0))
            );
            assert_eq!(unlimited_entry.additional_context_limit, Some(0));
        }
    }

    #[test]
    fn unsupported_event_warns_and_ignores_additional_context_limit() {
        let source_path = source_path();
        let (handler, _, warnings) = discover_command(
            HookEventName::Stop,
            /*additional_context_limit*/ Some(4_096),
        );

        assert_eq!(handler.additional_context_limit, Default::default());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("ignoring additionalContextLimit for Stop hook"));
        assert!(warnings[0].contains(&source_path.display().to_string()));
    }

    #[test]
    fn user_prompt_submit_ignores_invalid_matcher_during_discovery() {
        let mut handlers = Vec::new();
        let mut warnings = Vec::new();
        let mut display_order = 0;
        let source_path = source_path();
        let hook_states = std::collections::HashMap::new();

        append_matcher_groups(
            &mut handlers,
            &mut Vec::new(),
            &mut warnings,
            &mut display_order,
            &hook_handler_source(&source_path, &hook_states),
            HookEventName::UserPromptSubmit,
            vec![command_group(Some("["))],
        );

        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(
            handlers,
            vec![ConfiguredHandler {
                builtin: false,
                event_name: HookEventName::UserPromptSubmit,
                matcher: None,
                timeout_sec: 600,
                status_message: None,
                additional_context_limit: Default::default(),
                source_path: source_path.clone().into(),
                source: hook_source(),
                display_order: 0,
                kind: ConfiguredHandlerKind::Command {
                    command: "echo hello".to_string(),
                    r#async: false,
                    env: std::collections::HashMap::new(),
                },
            }]
        );
    }

    #[test]
    fn pre_tool_use_keeps_valid_matcher_during_discovery() {
        let mut handlers = Vec::new();
        let mut warnings = Vec::new();
        let mut display_order = 0;
        let source_path = source_path();
        let hook_states = std::collections::HashMap::new();

        append_matcher_groups(
            &mut handlers,
            &mut Vec::new(),
            &mut warnings,
            &mut display_order,
            &hook_handler_source(&source_path, &hook_states),
            HookEventName::PreToolUse,
            vec![command_group(Some("^Bash$"))],
        );

        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(
            handlers,
            vec![ConfiguredHandler {
                builtin: false,
                event_name: HookEventName::PreToolUse,
                matcher: Some(HookMatcher::new("^Bash$").expect("valid matcher")),
                timeout_sec: 600,
                status_message: None,
                additional_context_limit: Default::default(),
                source_path: source_path.clone().into(),
                source: hook_source(),
                display_order: 0,
                kind: ConfiguredHandlerKind::Command {
                    command: "echo hello".to_string(),
                    r#async: false,
                    env: std::collections::HashMap::new(),
                },
            }]
        );
    }

    #[test]
    fn session_end_normalizes_timeout() {
        let mut handlers = Vec::new();
        let mut hook_entries = Vec::new();
        let mut warnings = Vec::new();
        let mut display_order = 0;
        let source_path = source_path();
        let hook_states = std::collections::HashMap::new();

        append_matcher_groups(
            &mut handlers,
            &mut hook_entries,
            &mut warnings,
            &mut display_order,
            &hook_handler_source(&source_path, &hook_states),
            HookEventName::SessionEnd,
            vec![MatcherGroup {
                matcher: Some("other".to_string()),
                hooks: vec![
                    HookHandlerConfig::Command {
                        command: "echo default".to_string(),
                        command_windows: None,
                        timeout_sec: None,
                        r#async: false,
                        status_message: None,
                        additional_context_limit: None,
                    },
                    HookHandlerConfig::Command {
                        command: "echo clamped".to_string(),
                        command_windows: None,
                        timeout_sec: Some(600),
                        r#async: true,
                        status_message: None,
                        additional_context_limit: None,
                    },
                ],
            }],
        );

        assert_eq!(
            handlers
                .iter()
                .map(|handler| handler.timeout_sec)
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert!(
            handlers
                .iter()
                .all(ConfiguredHandler::can_apply_control_effects)
        );
        assert_eq!(
            handlers
                .iter()
                .map(|handler| handler.matcher.as_ref().map(HookMatcher::as_str))
                .collect::<Vec<_>>(),
            vec![Some("other"), Some("other")]
        );
        assert_eq!(
            hook_entries
                .iter()
                .map(|entry| entry.timeout_sec)
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert!(hook_entries.iter().all(|entry| matches!(
            entry.handler,
            HookListEntryHandler::Command { r#async: false, .. }
        )));
        assert_eq!(
            hook_entries
                .iter()
                .map(|entry| entry.matcher.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("other"), Some("other")]
        );
        assert_eq!(
            warnings,
            vec![
                format!(
                    "clamping SessionEnd hook timeout to 3s in {}",
                    source_path.display()
                ),
                format!(
                    "running async SessionEnd hook synchronously in {}",
                    source_path.display()
                ),
            ]
        );
    }

    #[test]
    fn interrupt_normalizes_timeout_and_supports_async_execution() {
        let mut handlers = Vec::new();
        let mut hook_entries = Vec::new();
        let mut warnings = Vec::new();
        let mut display_order = 0;
        let source_path = source_path();
        let hook_states = std::collections::HashMap::new();

        append_matcher_groups(
            &mut handlers,
            &mut hook_entries,
            &mut warnings,
            &mut display_order,
            &hook_handler_source(&source_path, &hook_states),
            HookEventName::Interrupt,
            vec![MatcherGroup {
                matcher: Some("ignored".to_string()),
                hooks: vec![HookHandlerConfig::Command {
                    command: "echo interrupt".to_string(),
                    command_windows: None,
                    timeout_sec: Some(600),
                    r#async: true,
                    status_message: None,
                    additional_context_limit: None,
                }],
            }],
        );

        assert_eq!(
            normalize_command_hook(
                HookEventName::Interrupt,
                /*timeout_sec*/ None,
                source_path.as_path(),
                &mut Vec::new(),
            ),
            1
        );
        assert_eq!(
            handlers
                .iter()
                .map(|handler| (
                    handler.timeout_sec,
                    handler.matcher.as_ref().map(HookMatcher::as_str),
                    handler.execution_mode()
                ))
                .collect::<Vec<_>>(),
            vec![(3, None, HookExecutionMode::Async)]
        );
        assert_eq!(
            hook_entries
                .iter()
                .map(|entry| (entry.timeout_sec, entry.matcher.as_deref()))
                .collect::<Vec<_>>(),
            vec![(3, None)]
        );
        assert!(hook_entries.iter().all(|entry| matches!(
            entry.handler,
            HookListEntryHandler::Command { r#async: true, .. }
        )));
        assert_eq!(
            warnings,
            vec![format!(
                "clamping Interrupt hook timeout to 3s in {}",
                source_path.display()
            )]
        );
    }

    #[test]
    fn enabled_user_hooks_register_without_trust_review() {
        let mut handlers = Vec::new();
        let mut hook_entries = Vec::new();
        let mut warnings = Vec::new();
        let mut display_order = 0;
        let source_path = source_path();
        let hook_states = std::collections::HashMap::new();

        append_matcher_groups(
            &mut handlers,
            &mut hook_entries,
            &mut warnings,
            &mut display_order,
            &user_hook_handler_source(&source_path, &hook_states),
            HookEventName::PreToolUse,
            vec![command_group(Some("Bash"))],
        );

        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(handlers.len(), 1);
        assert_eq!(hook_entries.len(), 1);
        assert_eq!(hook_entries[0].enabled, true);
    }

    #[test]
    fn disabled_user_hooks_are_listed_but_not_registered() {
        let mut handlers = Vec::new();
        let mut hook_entries = Vec::new();
        let mut warnings = Vec::new();
        let mut display_order = 0;
        let source_path = source_path();
        let hook_states = std::collections::HashMap::from([(
            format!("{}:pre_tool_use:0:0", source_path.display()),
            HookStateToml {
                enabled: Some(false),
            },
        )]);

        append_matcher_groups(
            &mut handlers,
            &mut hook_entries,
            &mut warnings,
            &mut display_order,
            &user_hook_handler_source(&source_path, &hook_states),
            HookEventName::PreToolUse,
            vec![command_group(Some("Bash"))],
        );

        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(handlers, Vec::<ConfiguredHandler>::new());
        assert_eq!(hook_entries.len(), 1);
        assert_eq!(hook_entries[0].enabled, false);
    }

    #[test]
    fn pre_tool_use_treats_star_matcher_as_match_all() {
        let mut handlers = Vec::new();
        let mut warnings = Vec::new();
        let mut display_order = 0;
        let source_path = source_path();
        let hook_states = std::collections::HashMap::new();

        append_matcher_groups(
            &mut handlers,
            &mut Vec::new(),
            &mut warnings,
            &mut display_order,
            &hook_handler_source(&source_path, &hook_states),
            HookEventName::PreToolUse,
            vec![command_group(Some("*"))],
        );

        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(handlers.len(), 1);
        assert_eq!(
            handlers[0].matcher.as_ref().map(HookMatcher::as_str),
            Some("*")
        );
    }

    #[test]
    fn post_tool_use_keeps_valid_matcher_during_discovery() {
        let mut handlers = Vec::new();
        let mut warnings = Vec::new();
        let mut display_order = 0;
        let source_path = source_path();
        let hook_states = std::collections::HashMap::new();

        append_matcher_groups(
            &mut handlers,
            &mut Vec::new(),
            &mut warnings,
            &mut display_order,
            &hook_handler_source(&source_path, &hook_states),
            HookEventName::PostToolUse,
            vec![command_group(Some("Edit|Write"))],
        );

        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(handlers.len(), 1);
        assert_eq!(handlers[0].event_name, HookEventName::PostToolUse);
        assert_eq!(
            handlers[0].matcher.as_ref().map(HookMatcher::as_str),
            Some("Edit|Write")
        );
    }

    #[test]
    fn toml_hook_discovery_ignores_malformed_state_entries() {
        let layer = ConfigLayerEntry::new(
            ConfigLayerSource::User {
                file: test_path_buf("/tmp/config.toml").abs(),
                profile: None,
            },
            config_with_malformed_state_and_session_start_hook(),
        );
        let mut warnings = Vec::new();

        let (_, hooks) = super::load_toml_hooks_from_layer(&layer, &mut warnings)
            .expect("valid hook events should still load");

        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(
            hooks,
            HookEventsToml {
                session_start: vec![MatcherGroup {
                    matcher: None,
                    hooks: vec![HookHandlerConfig::Command {
                        command: "echo hello".to_string(),
                        command_windows: None,
                        timeout_sec: None,
                        r#async: false,
                        status_message: None,
                        additional_context_limit: None,
                    }],
                }],
                ..Default::default()
            }
        );
    }

    #[test]
    fn pre_tool_use_resolves_windows_command_override_during_discovery() {
        let mut handlers = Vec::new();
        let mut warnings = Vec::new();
        let mut display_order = 0;
        let source_path = source_path();
        let hook_states = std::collections::HashMap::new();

        append_matcher_groups(
            &mut handlers,
            &mut Vec::new(),
            &mut warnings,
            &mut display_order,
            &hook_handler_source(&source_path, &hook_states),
            HookEventName::PreToolUse,
            vec![MatcherGroup {
                matcher: Some("^Bash$".to_string()),
                hooks: vec![HookHandlerConfig::Command {
                    command: "echo unix".to_string(),
                    command_windows: Some("echo windows".to_string()),
                    timeout_sec: None,
                    r#async: false,
                    status_message: None,
                    additional_context_limit: None,
                }],
            }],
        );

        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(handlers.len(), 1);
        assert_eq!(
            handlers[0].kind,
            ConfiguredHandlerKind::Command {
                command: if cfg!(windows) {
                    "echo windows"
                } else {
                    "echo unix"
                }
                .to_string(),
                env: std::collections::HashMap::new(),
                r#async: false,
            }
        );
    }

    fn config_with_malformed_state_and_session_start_hook() -> TomlValue {
        serde_json::from_value(serde_json::json!({
            "hooks": {
                "state": {
                    "some_key": {
                        "enabled": "not a bool",
                    },
                },
                "SessionStart": [{
                    "hooks": [{
                        "type": "command",
                        "command": "echo hello",
                    }],
                }],
            },
        }))
        .expect("config TOML should deserialize")
    }

    #[test]
    fn hook_source_for_config_layer_source_discards_source_details() {
        let config_file = test_path_buf("/tmp/.codex/config.toml").abs();
        let dot_codex_folder = test_path_buf("/tmp/worktree/.codex").abs();

        assert_eq!(
            [
                super::hook_source_for_config_layer_source(&ConfigLayerSource::PackagedDefaults {
                    file: config_file.clone(),
                }),
                super::hook_source_for_config_layer_source(&ConfigLayerSource::System {
                    file: config_file.clone(),
                }),
                super::hook_source_for_config_layer_source(&ConfigLayerSource::User {
                    file: config_file,
                    profile: None,
                }),
                super::hook_source_for_config_layer_source(&ConfigLayerSource::Project {
                    dot_codex_folder
                }),
                super::hook_source_for_config_layer_source(&ConfigLayerSource::SessionFlags),
            ],
            [
                HookSource::Unknown,
                HookSource::System,
                HookSource::User,
                HookSource::Project,
                HookSource::SessionFlags,
            ]
        );
    }
}
