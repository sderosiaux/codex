use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use codex_config::AbsolutePathBuf;
use codex_config::ConfigLayerEntry;
use codex_config::ConfigLayerSource;
use codex_config::ConfigLayerStack;
use codex_config::HookEventsToml;
use codex_config::HookHandlerConfig;
use codex_config::MatcherGroup;
use codex_config::TomlValue;
use codex_plugin::ExecutorPluginHookSource;
use codex_plugin::PluginHookSource;
use codex_plugin::PluginId;
use codex_protocol::ThreadId;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::HookExecutionMode;
use codex_protocol::protocol::HookHandlerType;
use codex_protocol::protocol::HookOutputEntry;
use codex_protocol::protocol::HookOutputEntryKind;
use codex_protocol::protocol::HookRunStatus;
use codex_protocol::protocol::HookSource;
use futures::FutureExt;
use futures::future::BoxFuture;
use pretty_assertions::assert_eq;
use tempfile::tempdir;
use tokio::sync::Notify;

use super::ClaudeHooksEngine;
use super::CommandHookRuntime;
use super::CommandShell;
use super::ConfiguredHandler;
use super::ConfiguredHandlerKind;
use super::HandlerSourcePath;
use crate::events::interrupt::InterruptRequest;
use crate::events::pre_tool_use::PreToolUseRequest;
use crate::events::stop::StopHookTarget;
use crate::events::stop::StopRequest;
use crate::mcp::HookMcpCall;
use crate::mcp::HookMcpExecutor;

fn cwd() -> AbsolutePathBuf {
    AbsolutePathBuf::current_dir().expect("current dir")
}

fn command_runtime(shell: CommandShell) -> CommandHookRuntime {
    let (result_sender, _result_receiver) = async_channel::unbounded();
    CommandHookRuntime::new(
        shell,
        Arc::new(std::env::vars_os().collect()),
        ThreadId::new(),
        result_sender,
    )
}

pub(crate) fn mcp_executor() -> Arc<dyn HookMcpExecutor> {
    Arc::new(StaticMcpExecutor {
        calls: Arc::new(Mutex::new(Vec::new())),
        output: String::new(),
        outputs_by_tool: HashMap::new(),
    })
}

fn config_toml_with_pre_tool_use(command: &str) -> TomlValue {
    let mut config_toml = TomlValue::Table(Default::default());
    let TomlValue::Table(config_table) = &mut config_toml else {
        unreachable!("config TOML root should be a table");
    };
    let mut hooks_table = TomlValue::Table(Default::default());
    let TomlValue::Table(hooks_entries) = &mut hooks_table else {
        unreachable!("hooks entry should be a table");
    };
    let mut pre_tool_use_group = TomlValue::Table(Default::default());
    let TomlValue::Table(pre_tool_use_group_entries) = &mut pre_tool_use_group else {
        unreachable!("PreToolUse group should be a table");
    };
    pre_tool_use_group_entries.insert(
        "matcher".to_string(),
        TomlValue::String("^Bash$".to_string()),
    );
    let mut handler = TomlValue::Table(Default::default());
    let TomlValue::Table(handler_entries) = &mut handler else {
        unreachable!("PreToolUse handler should be a table");
    };
    handler_entries.insert("type".to_string(), TomlValue::String("command".to_string()));
    handler_entries.insert(
        "command".to_string(),
        TomlValue::String(command.to_string()),
    );
    handler_entries.insert("timeout".to_string(), TomlValue::Integer(10));
    handler_entries.insert(
        "statusMessage".to_string(),
        TomlValue::String("checking".to_string()),
    );
    pre_tool_use_group_entries.insert("hooks".to_string(), TomlValue::Array(vec![handler]));
    hooks_entries.insert(
        "PreToolUse".to_string(),
        TomlValue::Array(vec![pre_tool_use_group]),
    );
    config_table.insert("hooks".to_string(), hooks_table);
    config_toml
}

#[test]
fn system_config_layer_hook_failures_are_startup_warnings() {
    let temp = tempdir().expect("create temp dir");
    let config_path =
        AbsolutePathBuf::try_from(temp.path().join("config.toml")).expect("absolute config path");
    let config_layer_stack = ConfigLayerStack::new(vec![ConfigLayerEntry::new(
        ConfigLayerSource::System { file: config_path },
        config_toml_with_pre_tool_use("  "),
    )])
    .expect("config layer stack");

    let (hooks, _result_receiver) = crate::Hooks::new(
        crate::HooksConfig {
            feature_enabled: true,
            config_layer_stack: Some(config_layer_stack),
            ..Default::default()
        },
        ThreadId::new(),
        mcp_executor(),
    );

    assert_eq!(hooks.startup_warnings().len(), 1);
    assert!(hooks.startup_warnings()[0].contains("skipping empty hook command"));
}

#[test]
fn user_disablement_applies_to_system_layer_hooks() {
    let temp = tempdir().expect("create temp dir");
    let system_config_path =
        AbsolutePathBuf::try_from(temp.path().join("system-config.toml")).expect("absolute path");
    let user_config_path =
        AbsolutePathBuf::try_from(temp.path().join("config.toml")).expect("absolute path");
    let system_key = format!("{}:pre_tool_use:0:0", system_config_path.display());
    let user_key = format!("{}:pre_tool_use:0:0", user_config_path.display());
    let config_layer_stack = ConfigLayerStack::new(vec![
        ConfigLayerEntry::new(
            ConfigLayerSource::System {
                file: system_config_path,
            },
            config_with_pre_tool_use_hook("python3 /tmp/system.py"),
        ),
        ConfigLayerEntry::new(
            ConfigLayerSource::User {
                file: user_config_path,
                profile: None,
            },
            config_with_pre_tool_use_hook_and_states("python3 /tmp/user.py", [&system_key]),
        ),
    ])
    .expect("config layer stack");

    let engine = ClaudeHooksEngine::new(
        /*enabled*/ true,
        Some(&config_layer_stack),
        Vec::new(),
        Vec::new(),
        command_runtime(CommandShell {
            program: String::new(),
            args: Vec::new(),
        }),
        mcp_executor(),
    );

    assert_eq!(engine.handlers.len(), 1);
    assert_eq!(engine.handlers[0].source, HookSource::User);
    let discovered =
        super::discovery::discover_handlers(Some(&config_layer_stack), Vec::new(), Vec::new());
    assert_eq!(
        discovered
            .hook_entries
            .iter()
            .map(|entry| (entry.key.as_str(), entry.source, entry.enabled))
            .collect::<Vec<_>>(),
        vec![
            (system_key.as_str(), HookSource::System, false),
            (user_key.as_str(), HookSource::User, true),
        ]
    );
}

fn config_with_hook_state(key: &str, enabled: bool) -> TomlValue {
    serde_json::from_value(serde_json::json!({
        "hooks": {
            "state": {
                (key): {
                    "enabled": enabled,
                },
            },
        },
    }))
    .expect("config TOML should deserialize")
}

fn config_with_pre_tool_use_hook_and_states<const N: usize>(
    command: &str,
    disabled_keys: [&str; N],
) -> TomlValue {
    let state = disabled_keys
        .into_iter()
        .map(|key| (key.to_string(), serde_json::json!({ "enabled": false })))
        .collect::<serde_json::Map<_, _>>();
    serde_json::from_value(serde_json::json!({
        "hooks": {
            "state": state,
            "PreToolUse": [{
                "hooks": [{
                    "type": "command",
                    "command": command,
                }],
            }],
        },
    }))
    .expect("config TOML should deserialize")
}

fn config_with_pre_tool_use_hook(command: &str) -> TomlValue {
    serde_json::from_value(serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                "hooks": [{
                    "type": "command",
                    "command": command,
                }],
            }],
        },
    }))
    .expect("config TOML should deserialize")
}

#[test]
fn discovers_hooks_from_json_and_toml_in_the_same_layer() {
    let temp = tempdir().expect("create temp dir");
    let config_path =
        AbsolutePathBuf::try_from(temp.path().join("config.toml")).expect("absolute config path");
    let hooks_json_path =
        AbsolutePathBuf::try_from(temp.path().join("hooks.json")).expect("absolute hooks path");
    fs::write(
        hooks_json_path.as_path(),
        r#"{
              "hooks": {
                "PreToolUse": [
                  {
                    "matcher": "^Bash$",
                    "hooks": [
                      {
                        "type": "command",
                        "command": "python3 /tmp/json-hook.py"
                      }
                    ]
                  }
                ]
              }
            }"#,
    )
    .expect("write hooks.json");
    let mut config_toml = TomlValue::Table(Default::default());
    let TomlValue::Table(config_table) = &mut config_toml else {
        unreachable!("config TOML root should be a table");
    };
    let mut hooks_table = TomlValue::Table(Default::default());
    let TomlValue::Table(hooks_entries) = &mut hooks_table else {
        unreachable!("hooks entry should be a table");
    };
    let mut pre_tool_use_group = TomlValue::Table(Default::default());
    let TomlValue::Table(pre_tool_use_group_entries) = &mut pre_tool_use_group else {
        unreachable!("PreToolUse group should be a table");
    };
    pre_tool_use_group_entries.insert(
        "matcher".to_string(),
        TomlValue::String("^Bash$".to_string()),
    );
    pre_tool_use_group_entries.insert(
        "hooks".to_string(),
        TomlValue::Array(vec![TomlValue::Table(Default::default())]),
    );
    let Some(TomlValue::Array(hooks_array)) = pre_tool_use_group_entries.get_mut("hooks") else {
        unreachable!("PreToolUse hooks should be an array");
    };
    let Some(TomlValue::Table(handler_entries)) = hooks_array.first_mut() else {
        unreachable!("PreToolUse handler should be a table");
    };
    handler_entries.insert("type".to_string(), TomlValue::String("command".to_string()));
    handler_entries.insert(
        "command".to_string(),
        TomlValue::String("python3 /tmp/toml-hook.py".to_string()),
    );
    hooks_entries.insert(
        "PreToolUse".to_string(),
        TomlValue::Array(vec![pre_tool_use_group]),
    );
    config_table.insert("hooks".to_string(), hooks_table);
    let config_layer_stack = ConfigLayerStack::new(vec![ConfigLayerEntry::new(
        ConfigLayerSource::System {
            file: config_path.clone(),
        },
        config_toml,
    )])
    .expect("config layer stack");

    let engine = ClaudeHooksEngine::new(
        /*enabled*/ true,
        Some(&config_layer_stack),
        Vec::new(),
        Vec::new(),
        command_runtime(CommandShell {
            program: String::new(),
            args: Vec::new(),
        }),
        mcp_executor(),
    );

    assert!(engine.warnings().iter().any(|warning| {
        warning.contains("loading hooks from both")
            && warning.contains(&hooks_json_path.display().to_string())
            && warning.contains(&config_path.display().to_string())
    }));

    let cwd = cwd();
    let preview = engine.preview_pre_tool_use(&PreToolUseRequest {
        session_id: ThreadId::new(),
        turn_id: "turn-1".to_string(),
        subagent: None,
        cwd,
        transcript_path: None,
        model: "gpt-test".to_string(),
        permission_mode: "default".to_string(),
        tool_name: "Bash".to_string(),
        matcher_aliases: Vec::new(),
        tool_use_id: "tool-1".to_string(),
        tool_input: serde_json::json!({ "command": "echo hello" }),
    });
    assert_eq!(preview.len(), 2);
    assert_eq!(
        engine
            .handlers
            .iter()
            .map(|handler| handler.source)
            .collect::<Vec<_>>(),
        vec![HookSource::System, HookSource::System]
    );
    assert_eq!(preview[0].source_path, hooks_json_path);
    assert_eq!(preview[1].source_path, config_path);
}

#[test]
fn profile_user_layers_load_shared_hooks_json_once() {
    let temp = tempdir().expect("create temp dir");
    let config_path =
        AbsolutePathBuf::try_from(temp.path().join("config.toml")).expect("absolute config path");
    let profile_path = AbsolutePathBuf::try_from(temp.path().join("work.config.toml"))
        .expect("absolute profile path");
    let hooks_json_path =
        AbsolutePathBuf::try_from(temp.path().join("hooks.json")).expect("absolute hooks path");
    fs::write(
        hooks_json_path.as_path(),
        r#"{
              "hooks": {
                "PreToolUse": [
                  {
                    "matcher": "^Bash$",
                    "hooks": [
                      {
                        "type": "command",
                        "command": "python3 /tmp/json-hook.py"
                      }
                    ]
                  }
                ]
              }
            }"#,
    )
    .expect("write hooks.json");
    let config_layer_stack = ConfigLayerStack::new(vec![
        ConfigLayerEntry::new(
            ConfigLayerSource::User {
                file: config_path,
                profile: None,
            },
            TomlValue::Table(Default::default()),
        ),
        ConfigLayerEntry::new(
            ConfigLayerSource::User {
                file: profile_path,
                profile: Some("work".to_string()),
            },
            TomlValue::Table(Default::default()),
        ),
    ])
    .expect("config layer stack");

    let engine = ClaudeHooksEngine::new(
        /*enabled*/ true,
        Some(&config_layer_stack),
        Vec::new(),
        Vec::new(),
        command_runtime(CommandShell {
            program: String::new(),
            args: Vec::new(),
        }),
        mcp_executor(),
    );

    assert!(engine.warnings().is_empty());
    assert_eq!(engine.handlers.len(), 1);
    let preview = engine.preview_pre_tool_use(&PreToolUseRequest {
        session_id: ThreadId::new(),
        turn_id: "turn-1".to_string(),
        subagent: None,
        cwd: cwd(),
        transcript_path: None,
        model: "gpt-test".to_string(),
        permission_mode: "default".to_string(),
        tool_name: "Bash".to_string(),
        matcher_aliases: Vec::new(),
        tool_use_id: "tool-1".to_string(),
        tool_input: serde_json::json!({ "command": "echo hello" }),
    });
    assert_eq!(preview.len(), 1);
    assert_eq!(preview[0].source_path, hooks_json_path);

    let listed = crate::list_hooks(crate::HooksConfig {
        feature_enabled: true,
        config_layer_stack: Some(config_layer_stack),
        ..Default::default()
    });
    assert!(listed.warnings.is_empty());
    assert_eq!(listed.hooks.len(), 1);
    assert_eq!(listed.hooks[0].source_path, hooks_json_path);
}

#[test]
fn malformed_hooks_json_is_reported_as_startup_warning() {
    let temp = tempdir().expect("create temp dir");
    let config_path =
        AbsolutePathBuf::try_from(temp.path().join("config.toml")).expect("absolute config path");
    let hooks_json_path =
        AbsolutePathBuf::try_from(temp.path().join("hooks.json")).expect("absolute hooks path");
    fs::write(
        hooks_json_path.as_path(),
        r#"{
          "SessionStart": [
            {
              "hooks": [
                {
                  "type": "command",
                  "command": "python3 /tmp/session-start.py"
                }
              ]
            }
          ]
        }"#,
    )
    .expect("write hooks.json");
    let config_layer_stack = ConfigLayerStack::new(vec![ConfigLayerEntry::new(
        ConfigLayerSource::System { file: config_path },
        TomlValue::Table(Default::default()),
    )])
    .expect("config layer stack");

    let engine = ClaudeHooksEngine::new(
        /*enabled*/ true,
        Some(&config_layer_stack),
        Vec::new(),
        Vec::new(),
        command_runtime(CommandShell {
            program: String::new(),
            args: Vec::new(),
        }),
        mcp_executor(),
    );

    assert!(engine.handlers.is_empty());
    assert_eq!(engine.warnings().len(), 1);
    assert!(engine.warnings()[0].contains("failed to parse hooks config"));
    assert!(
        engine.warnings()[0].contains(&hooks_json_path.display().to_string()),
        "warning should identify the malformed file: {}",
        engine.warnings()[0]
    );
    assert!(engine.warnings()[0].contains("unknown field `SessionStart`"));
}

fn bundled_cleanup_source(plugin_id: &str, server: &str, event: &str) -> PluginHookSource {
    let plugin_root = cwd().join("bundled-plugin");
    PluginHookSource {
        plugin_id: PluginId::parse(plugin_id).expect("plugin ID"),
        plugin_data_root: plugin_root.join("data"),
        source_path: plugin_root.join(".codex-plugin/plugin.json"),
        source_relative_path: "plugin.json#hooks[0]".to_string(),
        plugin_root,
        hooks: serde_json::from_value(serde_json::json!({
            (event): [{"hooks": [{
                "type": "mcp_tool",
                "server": server,
                "tool": "turn_ended",
                "input": {"turn_id": "${turn_id}"},
            }]}],
        }))
        .expect("cleanup hooks"),
    }
}

#[test]
fn bundled_cleanup_hooks_are_builtin() {
    for (plugin_id, server) in [
        ("browser@openai-bundled", "node_repl"),
        ("chrome@openai-bundled", "node_repl"),
        ("chrome-dev@openai-bundled", "node_repl"),
        ("chrome-internal@openai-bundled", "node_repl"),
        ("computer-use@openai-bundled", "node_repl"),
        ("unified-computer-use@openai-bundled", "cua_repl"),
    ] {
        for event in ["Stop", "Interrupt", "SubagentStop"] {
            let discovered = super::discovery::discover_handlers(
                /*config_layer_stack*/ None,
                vec![bundled_cleanup_source(plugin_id, server, event)],
                Vec::new(),
            );
            assert_eq!(discovered.handlers.len(), 1, "{plugin_id} {event}");
            assert_eq!(
                discovered
                    .hook_entries
                    .iter()
                    .map(|entry| (entry.plugin_id.as_deref(), entry.enabled, entry.builtin))
                    .collect::<Vec<_>>(),
                vec![(Some(plugin_id), true, true)],
                "{plugin_id} {event}"
            );
            assert!(discovered.handlers[0].builtin, "{plugin_id} {event}");
        }
    }
}

#[test]
fn bundled_cleanup_builtin_flag_does_not_extend_to_other_handlers() {
    let mut source = bundled_cleanup_source("browser@openai-bundled", "node_repl", "Stop");
    let cleanup = source.hooks.stop[0].hooks[0].clone();
    source.hooks.stop[0].hooks.extend([
        HookHandlerConfig::McpTool {
            server: "node_repl".to_string(),
            tool: "evaluate".to_string(),
            input: Default::default(),
            timeout_sec: None,
            status_message: None,
        },
        HookHandlerConfig::McpTool {
            server: "other_server".to_string(),
            tool: "turn_ended".to_string(),
            input: Default::default(),
            timeout_sec: None,
            status_message: None,
        },
        HookHandlerConfig::Command {
            command: "echo cleanup".to_string(),
            command_windows: None,
            timeout_sec: None,
            r#async: false,
            status_message: None,
            additional_context_limit: None,
        },
    ]);
    source.hooks.stop.push(MatcherGroup {
        matcher: Some("Bash".to_string()),
        hooks: vec![cleanup],
    });
    let discovered = super::discovery::discover_handlers(
        /*config_layer_stack*/ None,
        vec![source],
        Vec::new(),
    );
    // Every enabled handler runs; only the allowlisted cleanup handler is builtin.
    assert_eq!(
        discovered
            .handlers
            .iter()
            .map(|handler| handler.builtin)
            .collect::<Vec<_>>(),
        vec![true, false, false, false, false]
    );
    assert_eq!(
        discovered
            .hook_entries
            .iter()
            .map(|entry| entry.builtin)
            .collect::<Vec<_>>(),
        vec![true, false, false, false, false]
    );
}

#[test]
fn bundled_cleanup_builtin_requires_matching_plugin_server_and_event() {
    for (plugin_id, server, event) in [
        ("other@openai-bundled", "node_repl", "Stop"),
        ("browser@other", "node_repl", "Stop"),
        ("browser@openai-bundled-alpha", "node_repl", "Stop"),
        ("browser@openai-bundled", "node_repl", "PreToolUse"),
        ("browser@openai-bundled", "cua_repl", "Stop"),
        ("unified-computer-use@openai-bundled", "node_repl", "Stop"),
        ("unified-computer-use@other", "cua_repl", "Stop"),
        (
            "unified-computer-use@openai-bundled",
            "cua_repl",
            "PreToolUse",
        ),
    ] {
        let discovered = super::discovery::discover_handlers(
            /*config_layer_stack*/ None,
            vec![bundled_cleanup_source(plugin_id, server, event)],
            Vec::new(),
        );
        assert_eq!(discovered.handlers.len(), 1, "{plugin_id} {event}");
        assert!(!discovered.handlers[0].builtin, "{plugin_id} {event}");
        assert_eq!(discovered.hook_entries.len(), 1);
        assert!(!discovered.hook_entries[0].builtin, "{plugin_id} {event}");
    }
}

#[test]
fn local_hosted_app_cleanup_hooks_are_not_builtin() {
    let mut source = bundled_cleanup_source("browser@openai-curated-remote", "codex_apps", "Stop");
    source.hooks.stop[0].hooks[0] = HookHandlerConfig::McpTool {
        server: "codex_apps".to_string(),
        tool: "browser.turn_ended".to_string(),
        input: Default::default(),
        timeout_sec: None,
        status_message: None,
    };
    let discovered = super::discovery::discover_handlers(
        /*config_layer_stack*/ None,
        vec![source],
        Vec::new(),
    );
    assert_eq!(
        discovered
            .handlers
            .iter()
            .map(|handler| handler.builtin)
            .collect::<Vec<_>>(),
        vec![false]
    );
    assert_eq!(
        discovered
            .hook_entries
            .iter()
            .map(|entry| entry.builtin)
            .collect::<Vec<_>>(),
        vec![false]
    );
}

#[test]
fn builtin_cleanup_ignores_disablement() {
    let source = bundled_cleanup_source("browser@openai-bundled", "node_repl", "Stop");
    let discovered = super::discovery::discover_handlers(
        /*config_layer_stack*/ None,
        vec![source.clone()],
        Vec::new(),
    );
    let expected_entry = discovered.hook_entries[0].clone();
    let disabled_stack = ConfigLayerStack::new(vec![ConfigLayerEntry::new(
        ConfigLayerSource::User {
            file: cwd().join("config.toml"),
            profile: None,
        },
        config_with_hook_state(&expected_entry.key, /*enabled*/ false),
    )])
    .expect("disabled hook config");
    let disabled = super::discovery::discover_handlers(
        Some(&disabled_stack),
        vec![source.clone()],
        Vec::new(),
    );
    assert_eq!(disabled.handlers, discovered.handlers);
    assert_eq!(disabled.hook_entries, vec![expected_entry]);

    let feature_disabled = ClaudeHooksEngine::new(
        /*enabled*/ false,
        Some(&disabled_stack),
        vec![source],
        Vec::new(),
        command_runtime(CommandShell {
            program: String::new(),
            args: Vec::new(),
        }),
        mcp_executor(),
    );
    assert_eq!(feature_disabled.handlers, discovered.handlers);
}

#[test]
fn disabled_hooks_feature_keeps_builtin_cleanup_but_not_plugin_hooks() {
    let mut source = bundled_cleanup_source("browser@openai-bundled", "node_repl", "Stop");
    source.hooks.stop[0].hooks.push(HookHandlerConfig::Command {
        command: "echo ordinary hook".to_string(),
        command_windows: None,
        timeout_sec: None,
        r#async: false,
        status_message: None,
        additional_context_limit: None,
    });
    let discovered = super::discovery::discover_handlers(
        /*config_layer_stack*/ None,
        vec![source.clone()],
        Vec::new(),
    );
    assert_eq!(discovered.handlers.len(), 2);

    let engine = ClaudeHooksEngine::new(
        /*enabled*/ false,
        /*config_layer_stack*/ None,
        vec![source],
        Vec::new(),
        command_runtime(CommandShell {
            program: String::new(),
            args: Vec::new(),
        }),
        mcp_executor(),
    );
    assert_eq!(engine.handlers, vec![discovered.handlers[0].clone()]);
}

#[tokio::test]
async fn plugin_hook_sources_run_with_plugin_env_and_plugin_source() {
    let temp = tempdir().expect("create temp dir");
    let plugin_root =
        AbsolutePathBuf::try_from(temp.path().join("demo-plugin")).expect("plugin root");
    let plugin_data_root =
        AbsolutePathBuf::try_from(temp.path().join("plugin-data")).expect("plugin data root");
    fs::create_dir_all(plugin_root.join("hooks")).expect("create hooks dir");
    let source_path = plugin_root.join("hooks/hooks.json");
    let script_path = plugin_root.join("hooks/write_env.py");
    fs::write(
        script_path.as_path(),
        r#"import json
import os
print(json.dumps({
    "systemMessage": json.dumps({
        "plugin": os.environ.get("PLUGIN_ROOT"),
        "claude": os.environ.get("CLAUDE_PLUGIN_ROOT"),
    })
}))
"#,
    )
    .expect("write hook script");
    let plugin_id = PluginId::parse("demo-plugin@test-marketplace").expect("plugin id");
    let plugin_hook_sources = vec![PluginHookSource {
        plugin_id,
        plugin_root: plugin_root.clone(),
        plugin_data_root: plugin_data_root.clone(),
        source_path: source_path.clone(),
        source_relative_path: "hooks/hooks.json".to_string(),
        hooks: HookEventsToml {
            pre_tool_use: vec![MatcherGroup {
                matcher: Some("Bash".to_string()),
                hooks: vec![HookHandlerConfig::Command {
                    command: format!("python3 {}", script_path.display()),
                    command_windows: None,
                    timeout_sec: Some(10),
                    r#async: false,
                    status_message: None,
                    additional_context_limit: None,
                }],
            }],
            ..Default::default()
        },
    }];
    let engine = ClaudeHooksEngine::new(
        /*enabled*/ true,
        /*config_layer_stack*/ None,
        plugin_hook_sources.clone(),
        Vec::new(),
        command_runtime(CommandShell {
            program: String::new(),
            args: Vec::new(),
        }),
        mcp_executor(),
    );

    let preview = engine.preview_pre_tool_use(&PreToolUseRequest {
        session_id: ThreadId::new(),
        turn_id: "turn-1".to_string(),
        subagent: None,
        cwd: cwd(),
        transcript_path: None,
        model: "gpt-test".to_string(),
        permission_mode: "default".to_string(),
        tool_name: "Bash".to_string(),
        matcher_aliases: Vec::new(),
        tool_use_id: "tool-1".to_string(),
        tool_input: serde_json::json!({ "command": "echo hello" }),
    });
    assert_eq!(preview.len(), 1);
    assert_eq!(preview[0].source, HookSource::Plugin);
    assert_eq!(preview[0].source_path, source_path);
    let listed = crate::list_hooks(crate::HooksConfig {
        legacy_notify_argv: None,
        feature_enabled: true,
        config_layer_stack: None,
        plugin_hook_sources,
        plugin_hook_load_warnings: Vec::new(),
        shell_program: None,
        shell_args: Vec::new(),
    });
    assert_eq!(
        listed.hooks[0].plugin_id.as_deref(),
        Some("demo-plugin@test-marketplace")
    );

    let outcome = engine
        .run_pre_tool_use(PreToolUseRequest {
            session_id: ThreadId::new(),
            turn_id: "turn-1".to_string(),
            subagent: None,
            cwd: cwd(),
            transcript_path: None,
            model: "gpt-test".to_string(),
            permission_mode: "default".to_string(),
            tool_name: "Bash".to_string(),
            matcher_aliases: Vec::new(),
            tool_use_id: "tool-1".to_string(),
            tool_input: serde_json::json!({ "command": "echo hello" }),
        })
        .await;

    assert_eq!(outcome.hook_events.len(), 1);
    assert_eq!(outcome.hook_events[0].run.source, HookSource::Plugin);
    assert_eq!(
        outcome.hook_events[0].run.status,
        HookRunStatus::Completed,
        "hook entries: {:#?}",
        outcome.hook_events[0].run.entries
    );
    assert_eq!(outcome.hook_events[0].run.entries.len(), 1);
    assert_eq!(
        outcome.hook_events[0].run.entries[0].kind,
        HookOutputEntryKind::Warning
    );
    let logged: serde_json::Value =
        serde_json::from_str(&outcome.hook_events[0].run.entries[0].text)
            .expect("parse env payload");
    assert_eq!(
        logged,
        serde_json::json!({
            "plugin": plugin_root.display().to_string(),
            "claude": plugin_root.display().to_string(),
        })
    );
}

#[test]
fn plugin_hook_sources_expand_plugin_placeholders() {
    let temp = tempdir().expect("create temp dir");
    let plugin_root =
        AbsolutePathBuf::try_from(temp.path().join("demo-plugin")).expect("plugin root");
    let plugin_data_root =
        AbsolutePathBuf::try_from(temp.path().join("plugin-data")).expect("plugin data root");
    let source_path = plugin_root.join("hooks/hooks.json");
    let plugin_id = PluginId::parse("demo-plugin@test-marketplace").expect("plugin id");
    let plugin_hook_sources = vec![PluginHookSource {
        plugin_id,
        plugin_root: plugin_root.clone(),
        plugin_data_root: plugin_data_root.clone(),
        source_path,
        source_relative_path: "hooks/hooks.json".to_string(),
        hooks: HookEventsToml {
            pre_tool_use: vec![MatcherGroup {
                matcher: Some("Bash".to_string()),
                hooks: vec![HookHandlerConfig::Command {
                    command:
                        "run ${PLUGIN_ROOT} ${CLAUDE_PLUGIN_ROOT} ${PLUGIN_DATA} ${CLAUDE_PLUGIN_DATA}"
                            .to_string(),
                    command_windows: None,
                    timeout_sec: Some(5),
                    r#async: false,
                    status_message: None,
                    additional_context_limit: None,
                }],
            }],
            ..Default::default()
        },
    }];
    let engine = ClaudeHooksEngine::new(
        /*enabled*/ true,
        /*config_layer_stack*/ None,
        plugin_hook_sources,
        Vec::new(),
        command_runtime(CommandShell {
            program: String::new(),
            args: Vec::new(),
        }),
        mcp_executor(),
    );

    assert_eq!(
        engine.handlers[0].kind,
        ConfiguredHandlerKind::Command {
            command: format!(
                "run {} {} {} {}",
                plugin_root.display(),
                plugin_root.display(),
                plugin_data_root.display(),
                plugin_data_root.display()
            ),
            r#async: false,
            env: HashMap::from([
                ("PLUGIN_ROOT".to_string(), plugin_root.display().to_string()),
                (
                    "CLAUDE_PLUGIN_ROOT".to_string(),
                    plugin_root.display().to_string(),
                ),
                (
                    "PLUGIN_DATA".to_string(),
                    plugin_data_root.display().to_string(),
                ),
                (
                    "CLAUDE_PLUGIN_DATA".to_string(),
                    plugin_data_root.display().to_string(),
                ),
            ]),
        }
    );
}

#[test]
fn plugin_hook_load_warnings_are_startup_warnings() {
    let engine = ClaudeHooksEngine::new(
        /*enabled*/ true,
        /*config_layer_stack*/ None,
        Vec::new(),
        vec!["failed plugin hook".to_string()],
        command_runtime(CommandShell {
            program: String::new(),
            args: Vec::new(),
        }),
        mcp_executor(),
    );

    assert_eq!(engine.warnings(), &["failed plugin hook".to_string()]);
}

struct StaticMcpExecutor {
    calls: Arc<Mutex<Vec<HookMcpCall>>>,
    output: String,
    outputs_by_tool: HashMap<String, String>,
}

impl HookMcpExecutor for StaticMcpExecutor {
    fn execute(&self, call: HookMcpCall) -> BoxFuture<'_, anyhow::Result<String>> {
        async move {
            let output = self
                .outputs_by_tool
                .get(&call.tool)
                .unwrap_or(&self.output)
                .clone();
            self.calls.lock().expect("lock MCP calls").push(call);
            Ok(output)
        }
        .boxed()
    }
}

fn executor_stop_hook_fixture() -> (
    ClaudeHooksEngine,
    Arc<Mutex<Vec<HookMcpCall>>>,
    StopRequest,
    HookMcpCall,
    ExecutorPluginHookSource,
) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let executor = Arc::new(StaticMcpExecutor {
        calls: Arc::clone(&calls),
        output: r#"{"decision":"block","reason":"keep going"}"#.to_string(),
        outputs_by_tool: HashMap::from([(
            "terminate".to_string(),
            r#"{"continue":false,"stopReason":"done"}"#.to_string(),
        )]),
    });
    let mut engine = ClaudeHooksEngine::new(
        /*enabled*/ true,
        /*config_layer_stack*/ None,
        Vec::new(),
        Vec::new(),
        command_runtime(CommandShell {
            program: String::new(),
            args: Vec::new(),
        }),
        executor,
    );
    let source = ExecutorPluginHookSource {
        plugin_id: PluginId::parse("computer-use@openai-bundled").expect("valid plugin ID"),
        environment_id: "executor-a".to_string(),
        mcp_environment_id: None,
        mcp_metadata: None,
        plugin_root: "file:///plugins/computer-use"
            .parse()
            .expect("valid plugin root URI"),
        manifest_path: "file:///plugins/computer-use/.codex-plugin/plugin.json"
            .parse()
            .expect("valid plugin manifest URI"),
        source_relative_path: ".codex-plugin/plugin.json#hooks[0]".to_string(),
        hooks: HookEventsToml {
            stop: vec![MatcherGroup {
                matcher: None,
                hooks: vec![HookHandlerConfig::McpTool {
                    server: "node_repl".to_string(),
                    tool: "turn_ended".to_string(),
                    input: serde_json::from_value(serde_json::json!({
                        "turn_id": "${turn_id}",
                    }))
                    .expect("executor hook input"),
                    timeout_sec: None,
                    status_message: None,
                }],
            }],
            ..Default::default()
        },
    };
    engine.set_executor_hooks(vec![source.clone()]);
    assert_eq!(
        engine.handlers,
        vec![ConfiguredHandler {
            builtin: true,
            event_name: HookEventName::Stop,
            matcher: None,
            timeout_sec: 5,
            status_message: None,
            additional_context_limit: Default::default(),
            source_path: HandlerSourcePath::ExecutorScoped {
                plugin_id: PluginId::parse("computer-use@openai-bundled").expect("valid plugin ID"),
                environment_id: "executor-a".to_string(),
                mcp_environment_id: None,
                mcp_metadata: None,
                manifest_path: "file:///plugins/computer-use/.codex-plugin/plugin.json"
                    .parse()
                    .expect("valid plugin manifest URI"),
                source_relative_path: ".codex-plugin/plugin.json#hooks[0]".to_string(),
            },
            source: HookSource::Plugin,
            display_order: 0,
            kind: ConfiguredHandlerKind::McpTool {
                server: "node_repl".to_string(),
                tool: "turn_ended".to_string(),
                input: serde_json::from_value(serde_json::json!({
                    "turn_id": "${turn_id}",
                }))
                .expect("executor hook input"),
            },
        }]
    );
    assert_eq!(
        engine.handlers[0].execution_mode(),
        HookExecutionMode::Async
    );
    assert!(!engine.handlers[0].can_apply_control_effects());
    let request_metadata = Some(serde_json::Map::from_iter([(
        "x-codex-turn-metadata".to_string(),
        serde_json::json!({ "turn_id": "turn-1" }),
    )]));
    let request = StopRequest {
        session_id: ThreadId::new(),
        turn_id: "turn-1".to_string(),
        cwd: cwd(),
        transcript_path: None,
        model: "test-model".to_string(),
        permission_mode: "default".to_string(),
        request_metadata: request_metadata.clone(),
        stop_hook_active: false,
        last_assistant_message: None,
        target: StopHookTarget::Stop,
    };

    let expected_executor_call = HookMcpCall {
        server: "node_repl".to_string(),
        tool: "turn_ended".to_string(),
        environment_id: Some("executor-a".to_string()),
        metadata: request_metadata,
        input: serde_json::from_value(serde_json::json!({ "turn_id": "turn-1" }))
            .expect("expanded executor hook input"),
        timeout: Duration::from_secs(5),
    };

    (engine, calls, request, expected_executor_call, source)
}

async fn wait_for_mcp_calls(calls: &Arc<Mutex<Vec<HookMcpCall>>>, count: usize) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while calls.lock().expect("lock MCP calls").len() < count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("executor hook should complete in the background");
}

#[tokio::test]
async fn executor_stop_hooks_run_unless_regular_hooks_block_without_stopping() {
    let (mut engine, calls, request, expected_executor_call, _source) =
        executor_stop_hook_fixture();

    assert_eq!(engine.preview_stop(&request), Vec::new());
    let outcome = engine.run_stop(request.clone()).await;
    wait_for_mcp_calls(&calls, /*count*/ 1).await;
    assert!(!outcome.should_block);
    assert!(outcome.hook_events.is_empty());
    assert_eq!(
        *calls.lock().expect("lock MCP calls"),
        vec![expected_executor_call.clone()]
    );

    engine.handlers.push(ConfiguredHandler {
        builtin: false,
        event_name: HookEventName::Stop,
        matcher: None,
        timeout_sec: 30,
        status_message: None,
        additional_context_limit: Default::default(),
        source_path: cwd().join("hooks.json").into(),
        source: HookSource::User,
        display_order: 0,
        kind: ConfiguredHandlerKind::McpTool {
            server: "security".to_string(),
            tool: "check".to_string(),
            input: Default::default(),
        },
    });

    let outcome = engine.run_stop(request.clone()).await;
    assert!(outcome.should_block);
    assert_eq!(outcome.hook_events.len(), 1);
    assert_eq!(
        *calls.lock().expect("lock MCP calls"),
        vec![
            expected_executor_call.clone(),
            HookMcpCall {
                server: "security".to_string(),
                tool: "check".to_string(),
                environment_id: None,
                metadata: None,
                input: Default::default(),
                timeout: Duration::from_secs(30),
            },
        ]
    );

    engine.handlers.push(ConfiguredHandler {
        builtin: false,
        event_name: HookEventName::Stop,
        matcher: None,
        timeout_sec: 30,
        status_message: None,
        additional_context_limit: Default::default(),
        source_path: cwd().join("hooks.json").into(),
        source: HookSource::User,
        display_order: 1,
        kind: ConfiguredHandlerKind::McpTool {
            server: "security".to_string(),
            tool: "terminate".to_string(),
            input: Default::default(),
        },
    });

    let outcome = engine.run_stop(request).await;
    wait_for_mcp_calls(&calls, /*count*/ 5).await;
    assert!(outcome.should_stop);
    assert!(!outcome.should_block);
    assert_eq!(outcome.hook_events.len(), 2);
    assert_eq!(
        *calls.lock().expect("lock MCP calls"),
        vec![
            expected_executor_call.clone(),
            HookMcpCall {
                server: "security".to_string(),
                tool: "check".to_string(),
                environment_id: None,
                metadata: None,
                input: Default::default(),
                timeout: Duration::from_secs(30),
            },
            HookMcpCall {
                server: "security".to_string(),
                tool: "check".to_string(),
                environment_id: None,
                metadata: None,
                input: Default::default(),
                timeout: Duration::from_secs(30),
            },
            HookMcpCall {
                server: "security".to_string(),
                tool: "terminate".to_string(),
                environment_id: None,
                metadata: None,
                input: Default::default(),
                timeout: Duration::from_secs(30),
            },
            expected_executor_call,
        ]
    );
}

#[test]
fn executor_stop_hooks_register_each_target_environment_once() {
    let (mut engine, _, _, _, first_source) = executor_stop_hook_fixture();
    let mut expected_handlers = engine.handlers.clone();
    let mut duplicate_target = first_source.clone();
    duplicate_target.plugin_id = PluginId::parse("chrome@openai-bundled").expect("valid plugin ID");
    let mut second_source = first_source.clone();
    second_source.environment_id = "executor-b".to_string();
    let mut second_handler = expected_handlers[0].clone();
    second_handler.display_order = 1;
    let HandlerSourcePath::ExecutorScoped { environment_id, .. } = &mut second_handler.source_path
    else {
        panic!("executor Stop handler should have an executor source");
    };
    *environment_id = second_source.environment_id.clone();
    expected_handlers.push(second_handler);
    engine.set_executor_hooks(vec![first_source, duplicate_target, second_source]);

    assert_eq!(engine.handlers, expected_handlers);
}

#[tokio::test]
async fn executor_interrupt_hooks_register_and_run() {
    let (mut engine, calls, stop_request, expected_stop_call, mut source) =
        executor_stop_hook_fixture();
    source.hooks.subagent_stop = source.hooks.stop.clone();
    source.hooks.interrupt = source.hooks.stop.clone();
    engine.set_executor_hooks(vec![source]);

    assert_eq!(
        engine
            .handlers
            .iter()
            .map(|handler| handler.event_name)
            .collect::<Vec<_>>(),
        vec![
            HookEventName::SubagentStop,
            HookEventName::Stop,
            HookEventName::Interrupt,
        ]
    );
    assert_eq!(engine.preview_interrupt(), Vec::new());

    let outcome = engine
        .run_interrupt(InterruptRequest {
            session_id: stop_request.session_id,
            turn_id: stop_request.turn_id,
            cwd: stop_request.cwd,
            transcript_path: stop_request.transcript_path,
            model: stop_request.model,
            permission_mode: stop_request.permission_mode,
            request_metadata: stop_request.request_metadata,
        })
        .await;
    wait_for_mcp_calls(&calls, /*count*/ 1).await;

    assert!(outcome.hook_events.is_empty());
    assert_eq!(
        *calls.lock().expect("lock MCP calls"),
        vec![expected_stop_call]
    );
}

#[test]
fn executor_hooks_register_events_from_each_environment() {
    let (mut engine, _, _, _, mut first_source) = executor_stop_hook_fixture();
    first_source.hooks.interrupt = std::mem::take(&mut first_source.hooks.stop);
    engine.set_executor_hooks(vec![first_source.clone()]);
    let mut expected_handlers = engine.handlers.clone();
    let mut second_source = first_source.clone();
    second_source.environment_id = "executor-b".to_string();
    second_source.hooks.stop = std::mem::take(&mut second_source.hooks.interrupt);
    let mut second_handler = expected_handlers[0].clone();
    second_handler.event_name = HookEventName::Stop;
    second_handler.display_order = 1;
    let HandlerSourcePath::ExecutorScoped { environment_id, .. } = &mut second_handler.source_path
    else {
        panic!("executor Stop handler should have an executor source");
    };
    *environment_id = second_source.environment_id.clone();
    expected_handlers.push(second_handler);

    engine.set_executor_hooks(vec![first_source, second_source]);

    assert_eq!(engine.handlers, expected_handlers);
}

#[tokio::test]
async fn executor_stop_hooks_use_an_admitted_mcp_environment() {
    let (mut engine, calls, request, mut expected_call, mut source) = executor_stop_hook_fixture();
    source.mcp_environment_id = Some("local".to_string());
    engine.set_executor_hooks(vec![source]);

    engine.run_stop(request).await;
    wait_for_mcp_calls(&calls, /*count*/ 1).await;

    expected_call.environment_id = Some("local".to_string());
    assert_eq!(*calls.lock().expect("lock MCP calls"), vec![expected_call]);
}

#[tokio::test]
async fn memory_consolidation_stop_preserves_policy_and_executor_cleanup() {
    for (source, runs_policy) in [
        (HookSource::User, false),
        (HookSource::Project, false),
        (HookSource::SessionFlags, false),
        (HookSource::Plugin, false),
        (HookSource::System, true),
        (HookSource::Unknown, true),
    ] {
        let (mut engine, calls, mut request, expected_executor_call, _source) =
            executor_stop_hook_fixture();
        request.target = StopHookTarget::MemoryConsolidation;
        let policy_call = HookMcpCall {
            server: "security".to_string(),
            tool: "check".to_string(),
            environment_id: None,
            metadata: None,
            input: Default::default(),
            timeout: Duration::from_secs(5),
        };
        let mut handler = engine.handlers[0].clone();
        handler.source_path = cwd().join("hooks.json").into();
        handler.source = source;
        handler.kind = ConfiguredHandlerKind::McpTool {
            server: policy_call.server.clone(),
            tool: policy_call.tool.clone(),
            input: policy_call.input.clone(),
        };
        engine.handlers.push(handler);
        assert_eq!(
            engine.preview_stop(&request).len(),
            usize::from(runs_policy)
        );

        let outcome = engine.run_stop(request).await;
        let expected_calls = if runs_policy {
            vec![policy_call, expected_executor_call]
        } else {
            vec![expected_executor_call]
        };
        wait_for_mcp_calls(&calls, expected_calls.len()).await;
        assert_eq!(*calls.lock().expect("lock MCP calls"), expected_calls);
        assert_eq!(outcome.should_block, runs_policy);
    }
}

#[tokio::test]
async fn executor_stop_hooks_do_not_delay_stop_completion() {
    struct BlockingMcpExecutor {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    impl HookMcpExecutor for BlockingMcpExecutor {
        fn execute(&self, _call: HookMcpCall) -> BoxFuture<'_, anyhow::Result<String>> {
            async move {
                self.started.notify_one();
                self.release.notified().await;
                Ok(String::new())
            }
            .boxed()
        }
    }

    let (mut engine, _, request, _, _) = executor_stop_hook_fixture();
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    engine.mcp_executor = Arc::new(BlockingMcpExecutor {
        started: Arc::clone(&started),
        release: Arc::clone(&release),
    });

    let outcome = tokio::time::timeout(Duration::from_millis(100), engine.run_stop(request))
        .await
        .expect("executor hook must not delay Stop completion");
    assert!(!outcome.should_block);
    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .expect("executor hook should start in the background");
    release.notify_one();
}

#[tokio::test]
async fn mcp_tool_hooks_expand_event_input_and_apply_pre_tool_decisions() {
    let temp = tempdir().expect("create temp dir");
    let config_path =
        AbsolutePathBuf::try_from(temp.path().join("config.toml")).expect("absolute config path");
    fs::write(
        temp.path().join("hooks.json"),
        serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "mcp_tool",
                        "server": "security",
                        "tool": "scan",
                        "input": { "command": "${tool_input.command}" },
                        "timeout": 20,
                    }],
                }],
            },
        })
        .to_string(),
    )
    .expect("write MCP hooks.json");
    let config_layer_stack = ConfigLayerStack::new(vec![ConfigLayerEntry::new(
        ConfigLayerSource::User {
            file: config_path,
            profile: None,
        },
        TomlValue::Table(Default::default()),
    )])
    .expect("config layer stack");

    let request = PreToolUseRequest {
        session_id: ThreadId::new(),
        turn_id: "turn-1".to_string(),
        subagent: None,
        cwd: cwd(),
        transcript_path: None,
        model: "gpt-test".to_string(),
        permission_mode: "default".to_string(),
        tool_name: "Bash".to_string(),
        matcher_aliases: Vec::new(),
        tool_use_id: "tool-1".to_string(),
        tool_input: serde_json::json!({ "command": "rm important.txt" }),
    };
    let calls = Arc::new(Mutex::new(Vec::new()));
    let executor = StaticMcpExecutor {
        calls: Arc::clone(&calls),
        output: serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": "blocked by MCP scanner",
            },
        })
        .to_string(),
        outputs_by_tool: HashMap::new(),
    };
    let engine = ClaudeHooksEngine::new(
        /*enabled*/ true,
        Some(&config_layer_stack),
        Vec::new(),
        Vec::new(),
        command_runtime(CommandShell {
            program: String::new(),
            args: Vec::new(),
        }),
        Arc::new(executor),
    );
    let outcome = engine.run_pre_tool_use(request).await;

    assert!(outcome.should_block);
    assert_eq!(
        outcome.block_reason.as_deref(),
        Some("blocked by MCP scanner")
    );
    assert_eq!(
        outcome.hook_events[0].run.handler_type,
        HookHandlerType::McpTool
    );
    assert_eq!(
        outcome.hook_events[0].run.execution_mode,
        codex_protocol::protocol::HookExecutionMode::Sync
    );
    assert_eq!(
        *calls.lock().expect("lock MCP calls"),
        vec![HookMcpCall {
            server: "security".to_string(),
            tool: "scan".to_string(),
            environment_id: None,
            metadata: None,
            input: serde_json::from_value(serde_json::json!({
                "command": "rm important.txt",
            }))
            .expect("object input"),
            timeout: Duration::from_secs(20),
        }]
    );
}

#[tokio::test]
async fn mcp_interrupt_hooks_expand_event_input_and_bound_timeout() {
    let temp = tempdir().expect("create temp dir");
    let config_path =
        AbsolutePathBuf::try_from(temp.path().join("config.toml")).expect("absolute config path");
    fs::write(
        temp.path().join("hooks.json"),
        serde_json::json!({
            "hooks": {
                "Interrupt": [{
                    "hooks": [{
                        "type": "mcp_tool",
                        "server": "security",
                        "tool": "notify",
                        "input": {
                            "event": "${hook_event_name}",
                            "turn_id": "${turn_id}",
                            "permission_mode": "${permission_mode}",
                        },
                        "timeout": 20,
                    }],
                }],
            },
        })
        .to_string(),
    )
    .expect("write MCP Interrupt hooks.json");
    let config_layer_stack = ConfigLayerStack::new(vec![ConfigLayerEntry::new(
        ConfigLayerSource::User {
            file: config_path,
            profile: None,
        },
        TomlValue::Table(Default::default()),
    )])
    .expect("config layer stack");

    let calls = Arc::new(Mutex::new(Vec::new()));
    let executor = StaticMcpExecutor {
        calls: Arc::clone(&calls),
        output: serde_json::json!({
            "systemMessage": "interrupt observed",
        })
        .to_string(),
        outputs_by_tool: HashMap::new(),
    };
    let engine = ClaudeHooksEngine::new(
        /*enabled*/ true,
        Some(&config_layer_stack),
        Vec::new(),
        Vec::new(),
        command_runtime(CommandShell {
            program: String::new(),
            args: Vec::new(),
        }),
        Arc::new(executor),
    );
    let outcome = engine
        .run_interrupt(InterruptRequest {
            session_id: ThreadId::new(),
            turn_id: "turn-1".to_string(),
            cwd: cwd(),
            transcript_path: None,
            model: "gpt-test".to_string(),
            permission_mode: "default".to_string(),
            request_metadata: None,
        })
        .await;

    assert_eq!(outcome.hook_events.len(), 1);
    assert_eq!(
        outcome.hook_events[0].run.handler_type,
        HookHandlerType::McpTool
    );
    assert_eq!(outcome.hook_events[0].run.status, HookRunStatus::Completed);
    assert_eq!(
        outcome.hook_events[0].run.entries,
        vec![HookOutputEntry {
            kind: HookOutputEntryKind::Warning,
            text: "interrupt observed".to_string(),
        }]
    );
    assert_eq!(
        *calls.lock().expect("lock MCP calls"),
        vec![HookMcpCall {
            server: "security".to_string(),
            tool: "notify".to_string(),
            environment_id: None,
            metadata: None,
            input: serde_json::from_value(serde_json::json!({
                "event": "Interrupt",
                "turn_id": "turn-1",
                "permission_mode": "default",
            }))
            .expect("object input"),
            timeout: Duration::from_secs(3),
        }]
    );
}
