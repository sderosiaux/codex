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
}

struct NormalizedHandler {
    kind: ConfiguredHandlerKind,
    timeout_sec: u64,
    status_message: Option<String>,
    additional_context_limit: Option<usize>,
}

pub(crate) fn discover_handlers(config_layer_stack: Option<&ConfigLayerStack>) -> DiscoveryResult {
    let mut handlers = Vec::new();
    let mut hook_entries = Vec::new();
    let mut warnings = Vec::new();
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
                    },
                    hook_events,
                );
            }
        }
    }

    DiscoveryResult {
        handlers,
        hook_entries,
        warnings,
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
                    NormalizedHandler {
                        kind: ConfiguredHandlerKind::Command {
                            command,
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
                    NormalizedHandler {
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
                kind,
                timeout_sec,
                status_message,
                additional_context_limit,
            } = normalized;
            let key = crate::hook_key(&source.key_source, event_name, group_index, handler_index);
            let state = source.hook_states.get(&key);
            let enabled = state.and_then(|state| state.enabled) != Some(false);
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
                key,
                event_name,
                handler,
                matcher: matcher.map(ToOwned::to_owned),
                timeout_sec,
                status_message: status_message.clone(),
                additional_context_limit,
                source_path: source.path.clone(),
                source: source.source,
                display_order: *display_order,
                enabled,
            });
            if enabled {
                handlers.push(ConfiguredHandler {
                    event_name,
                    matcher: compiled_matcher.clone(),
                    timeout_sec,
                    status_message,
                    additional_context_limit: AdditionalContextLimit::from_config(
                        additional_context_limit,
                    ),
                    source_path: source.path.clone(),
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
