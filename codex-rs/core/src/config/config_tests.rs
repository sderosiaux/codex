use crate::config::edit::ConfigEdit;
use crate::config::edit::ConfigEditsBuilder;
use crate::config::edit::apply_blocking;
use crate::context::ContextualUserFragment;
use crate::plugins::plugins_manager_for_config;
use crate::session::multi_agents::resolve_usage_hints;
use codex_config::CONFIG_TOML_FILE;
use codex_config::ConfigLayerEntry;
use codex_config::ConfigLayerSource;
use codex_config::ConfigLayerStack;
use codex_config::ProfileV2Name;
use codex_config::config_toml::AgentRoleToml;
use codex_config::config_toml::AgentsToml;
use codex_config::config_toml::ConfigToml;
use codex_config::config_toml::ExperimentalRequestUserInput;
use codex_config::config_toml::ToolsToml;
use codex_config::types::BundledSkillsConfig;
use codex_config::types::HistoryPersistence;
use codex_config::types::McpServerEnvVar;
use codex_config::types::McpServerOAuthConfig;
use codex_config::types::McpServerTransportConfig;
use codex_config::types::MemoriesConfig;
use codex_config::types::MemoriesToml;
use codex_config::types::OtelConfigToml;
use codex_config::types::OtelExporterKind;
use codex_config::types::SkillsConfig;
use codex_config::types::ToolSuggestDisabledTool;
use codex_config::types::ToolSuggestDiscoverableType;
use codex_exec_server::LOCAL_FS;
use codex_features::Feature;
use codex_features::FeaturesToml;
use codex_login::test_support::auth_manager_from_optional_auth;
use codex_model_provider::ProviderCapabilities;
use codex_model_provider_info::LMSTUDIO_OSS_PROVIDER_ID;
use codex_model_provider_info::OLLAMA_OSS_PROVIDER_ID;
use codex_model_provider_info::WireApi;
use codex_models_manager::bundled_models_response;
use codex_prompts::ResolvedMessage;
use codex_protocol::config_types::ModelProviderAuthInfo;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::protocol::MultiAgentVersion;
use codex_utils_path_uri::LegacyAppPathString;
use tempfile::tempdir;

use super::*;
use core_test_support::PathBufExt;
use core_test_support::TempDirExt;
use pretty_assertions::assert_eq;
use rmcp::model::ElicitationCapability;
use rmcp::model::FormElicitationCapability;
use rmcp::model::UrlElicitationCapability;

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::time::Duration;
use tempfile::TempDir;

fn stdio_mcp(command: &str) -> McpServerConfig {
    stdio_mcp_with_args(command, &[])
}

fn stdio_mcp_with_args(command: &str, args: &[&str]) -> McpServerConfig {
    McpServerConfig {
        auth: Default::default(),
        transport: McpServerTransportConfig::Stdio {
            command: command.to_string(),
            args: args.iter().map(ToString::to_string).collect(),
            env: None,
            env_vars: Vec::new(),
            cwd: None,
        },
        environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
        enabled: true,
        required: false,
        startup_readiness: Default::default(),
        supports_parallel_tool_calls: false,
        tool_input_schema_max_bytes: None,
        omit_tools_from: None,
        disabled_reason: None,
        startup_timeout_sec: None,
        tool_timeout_sec: None,
        enabled_tools: None,
        disabled_tools: None,
        scopes: None,
        oauth: None,
        oauth_resource: None,
        tools: HashMap::new(),
    }
}

fn http_mcp(url: &str) -> McpServerConfig {
    McpServerConfig {
        auth: Default::default(),
        transport: McpServerTransportConfig::StreamableHttp {
            url: url.to_string(),
            bearer_token_env_var: None,
            http_headers: None,
            env_http_headers: None,
            http_headers_helper: None,
        },
        environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
        enabled: true,
        required: false,
        startup_readiness: Default::default(),
        supports_parallel_tool_calls: false,
        tool_input_schema_max_bytes: None,
        omit_tools_from: None,
        disabled_reason: None,
        startup_timeout_sec: None,
        tool_timeout_sec: None,
        enabled_tools: None,
        disabled_tools: None,
        scopes: None,
        oauth: None,
        oauth_resource: None,
        tools: HashMap::new(),
    }
}

#[tokio::test]
async fn load_config_normalizes_relative_cwd_override() -> std::io::Result<()> {
    let expected_cwd = AbsolutePathBuf::relative_to_current_dir("nested")?;
    let codex_home = tempdir()?;
    let config = Config::load_from_base_config_with_overrides(
        ConfigToml::default(),
        ConfigOverrides {
            cwd: Some(PathBuf::from("nested")),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await?;

    assert_eq!(config.cwd, expected_cwd);
    Ok(())
}

#[tokio::test]
async fn load_config_applies_optional_mcp_startup_grace() -> std::io::Result<()> {
    let codex_home = tempdir()?;
    let config_toml: ConfigToml = toml::from_str("mcp_optional_startup_grace_ms = 2500")
        .expect("optional MCP startup grace should parse from config.toml");
    let config = Config::load_from_base_config_with_overrides(
        config_toml,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.mcp_optional_startup_grace,
        Duration::from_millis(2500)
    );
    Ok(())
}

#[tokio::test]
async fn load_config_resolves_thread_unload_delay() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    for (toml, seconds) in [
        ("", 1800),
        ("thread_unload_delay_secs = 0", 0),
        ("thread_unload_delay_secs = 60", 60),
    ] {
        let config = Config::load_from_base_config_with_overrides(
            toml::from_str(toml)?,
            ConfigOverrides::default(),
            codex_home.abs(),
        )
        .await?;
        assert_eq!(config.thread_unload_delay, Duration::from_secs(seconds));
    }
    Ok(())
}

#[tokio::test]
async fn load_config_rejects_thread_unload_delay_overflow() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let error = Config::load_from_base_config_with_overrides(
        ConfigToml {
            thread_unload_delay_secs: Some(u64::MAX),
            ..Default::default()
        },
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await
    .expect_err("idle timeout must fit in a monotonic deadline");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(error.to_string(), "thread_unload_delay_secs is too large");
    Ok(())
}

#[tokio::test]
async fn test_toml_parsing() {
    let history_with_persistence = r#"
[history]
persistence = "save-all"
"#;
    let history_with_persistence_cfg = toml::from_str::<ConfigToml>(history_with_persistence)
        .expect("TOML deserialization should succeed");
    assert_eq!(
        Some(History {
            persistence: HistoryPersistence::SaveAll,
            max_bytes: None,
        }),
        history_with_persistence_cfg.history
    );

    let history_no_persistence = r#"
[history]
persistence = "none"
"#;

    let history_no_persistence_cfg = toml::from_str::<ConfigToml>(history_no_persistence)
        .expect("TOML deserialization should succeed");
    assert_eq!(
        Some(History {
            persistence: HistoryPersistence::None,
            max_bytes: None,
        }),
        history_no_persistence_cfg.history
    );

    let memories = r#"
[memories]
disable_on_external_context = true
generate_memories = false
use_memories = false
dedicated_tools = true
max_raw_memories_for_consolidation = 512
max_unused_days = 21
max_rollout_age_days = 42
max_rollouts_per_startup = 9
min_rollout_idle_hours = 24
min_rate_limit_remaining_percent = 12
extract_model = "gpt-5-mini"
consolidation_model = "gpt-5.2"
"#;
    let memories_cfg =
        toml::from_str::<ConfigToml>(memories).expect("TOML deserialization should succeed");
    assert_eq!(
        Some(MemoriesToml {
            dual_write: None,
            version: None,
            disable_on_external_context: Some(true),
            generate_memories: Some(false),
            use_memories: Some(false),
            dedicated_tools: Some(true),
            max_raw_memories_for_consolidation: Some(512),
            max_unused_days: Some(21),
            max_rollout_age_days: Some(42),
            max_rollouts_per_startup: Some(9),
            min_rollout_idle_hours: Some(24),
            min_rate_limit_remaining_percent: Some(12),
            extract_model: Some("gpt-5-mini".to_string()),
            consolidation_model: Some("gpt-5.2".to_string()),
        }),
        memories_cfg.memories
    );

    let config = Config::load_from_base_config_with_overrides(
        memories_cfg,
        ConfigOverrides::default(),
        tempdir().expect("tempdir").abs(),
    )
    .await
    .expect("load config from memories settings");
    assert_eq!(
        config.memories,
        MemoriesConfig {
            dual_write: false,
            version: codex_protocol::MemoryVersion::V1,
            disable_on_external_context: true,
            generate_memories: false,
            use_memories: false,
            dedicated_tools: true,
            max_raw_memories_for_consolidation: 512,
            max_unused_days: 21,
            max_rollout_age_days: 42,
            max_rollouts_per_startup: 9,
            min_rollout_idle_hours: 24,
            min_rate_limit_remaining_percent: 12,
            extract_model: Some("gpt-5-mini".to_string()),
            consolidation_model: Some("gpt-5.2".to_string()),
        }
    );

    let legacy_memories_cfg =
        toml::from_str::<ConfigToml>("[memories]\nno_memories_if_mcp_or_web_search = true\n")
            .expect("legacy memories TOML should deserialize");
    assert!(
        MemoriesConfig::from(
            legacy_memories_cfg
                .memories
                .expect("legacy memories config")
        )
        .disable_on_external_context
    );
}

#[tokio::test]
async fn goal_max_token_budget_requires_positive_integer() {
    let config_toml = toml::from_str::<ConfigToml>("[goals]\nmax_goal_token_budget = 25000\n")
        .expect("positive goal token budget should deserialize");
    let config = Config::load_from_base_config_with_overrides(
        config_toml,
        ConfigOverrides::default(),
        tempdir().expect("tempdir").abs(),
    )
    .await
    .expect("positive goal token budget should load");
    assert_eq!(config.max_goal_token_budget, Some(25_000));

    for invalid in ["0", "-1", "1.5", "\"100\""] {
        let config = format!("[goals]\nmax_goal_token_budget = {invalid}\n");
        assert!(
            toml::from_str::<ConfigToml>(&config).is_err(),
            "invalid goal token budget should be rejected: {invalid}"
        );
    }
}

#[test]
fn parses_bundled_skills_config() {
    let cfg: ConfigToml = toml::from_str(
        r#"
[skills]
include_instructions = false
max_context_tokens = 1200

[skills.bundled]
enabled = false
"#,
    )
    .expect("TOML deserialization should succeed");

    assert_eq!(
        cfg.skills,
        Some(SkillsConfig {
            bundled: Some(BundledSkillsConfig { enabled: false }),
            include_instructions: Some(false),
            max_context_tokens: std::num::NonZeroUsize::new(1_200),
            config: Vec::new(),
        })
    );

    assert!(toml::from_str::<ConfigToml>("[skills]\nmax_context_tokens = 0\n").is_err());
}

#[test]
fn tools_web_search_true_deserializes_to_none() {
    let cfg: ConfigToml = toml::from_str(
        r#"
[tools]
web_search = true
"#,
    )
    .expect("TOML deserialization should succeed");

    assert_eq!(
        cfg.tools,
        Some(ToolsToml {
            web_search: None,
            experimental_request_user_input: None,
            update_plan: None,
        })
    );
}

#[test]
fn tools_web_search_false_deserializes_to_none() {
    let cfg: ConfigToml = toml::from_str(
        r#"
[tools]
web_search = false
"#,
    )
    .expect("TOML deserialization should succeed");

    assert_eq!(
        cfg.tools,
        Some(ToolsToml {
            web_search: None,
            experimental_request_user_input: None,
            update_plan: None,
        })
    );
}

#[test]
fn tools_experimental_request_user_input_defaults_to_enabled() {
    let cfg: ConfigToml = toml::from_str(
        r#"
[tools.experimental_request_user_input]
"#,
    )
    .expect("TOML deserialization should succeed");

    assert_eq!(
        cfg.tools,
        Some(ToolsToml {
            web_search: None,
            experimental_request_user_input: Some(ExperimentalRequestUserInput { enabled: true }),
            update_plan: None,
        })
    );
}

#[test]
fn tools_experimental_request_user_input_can_be_disabled() {
    let cfg: ConfigToml = toml::from_str(
        r#"
[tools.experimental_request_user_input]
enabled = false
"#,
    )
    .expect("TOML deserialization should succeed");

    assert_eq!(
        cfg.tools,
        Some(ToolsToml {
            web_search: None,
            experimental_request_user_input: Some(ExperimentalRequestUserInput { enabled: false }),
            update_plan: None,
        })
    );
}

#[tokio::test]
async fn load_config_resolves_experimental_request_user_input_enabled() -> std::io::Result<()> {
    let codex_home = tempdir()?;
    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            tools: Some(ToolsToml {
                web_search: None,
                experimental_request_user_input: Some(ExperimentalRequestUserInput {
                    enabled: false,
                }),
                update_plan: None,
            }),
            ..ConfigToml::default()
        },
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert!(!config.experimental_request_user_input_enabled);
    Ok(())
}

#[tokio::test]
async fn load_config_resolves_non_prefixed_mcp_tool_servers() -> std::io::Result<()> {
    let cases = [
        (
            "[features]\nnon_prefixed_mcp_tool_names = false\n",
            None,
            true,
        ),
        (
            "[features]\nnon_prefixed_mcp_tool_names = true\n",
            None,
            false,
        ),
        (
            "[features.non_prefixed_mcp_tool_names]\nenabled = true\n",
            None,
            false,
        ),
        (
            "[features.non_prefixed_mcp_tool_names]\nenabled = true\nserver_names = [\"history\", \"notes\"]\n",
            Some(vec!["history".to_string(), "notes".to_string()]),
            true,
        ),
        (
            "[features.non_prefixed_mcp_tool_names]\nenabled = true\nserver_names = []\n",
            Some(Vec::new()),
            true,
        ),
        (
            "[features.non_prefixed_mcp_tool_names]\nenabled = false\nserver_names = [\"history\"]\n",
            None,
            true,
        ),
    ];

    for (config_contents, expected_servers, expected_prefix) in cases {
        let codex_home = tempdir()?;
        let config_toml = toml::from_str::<ConfigToml>(config_contents)
            .expect("TOML deserialization should succeed");
        let config = Config::load_from_base_config_with_overrides(
            config_toml,
            ConfigOverrides::default(),
            codex_home.abs(),
        )
        .await?;

        assert_eq!(config.non_prefixed_mcp_tool_servers, expected_servers);
        assert_eq!(config.prefix_mcp_tool_names(), expected_prefix);
        let plugins_manager =
            plugins_manager_for_config(&config, auth_manager_from_optional_auth(/*auth*/ None));
        let mcp_config = config.to_mcp_config(&plugins_manager).await;
        assert_eq!(mcp_config.prefix_mcp_tool_names, expected_prefix);
        assert_eq!(
            mcp_config.non_prefixed_mcp_tool_servers,
            expected_servers.unwrap_or_default()
        );
    }
    Ok(())
}

#[tokio::test]
async fn load_config_resolves_update_plan_enabled() -> std::io::Result<()> {
    let codex_home = tempdir()?;
    for (config_toml, expected_enabled) in [
        ("", false),
        ("[tools.update_plan]", false),
        ("[tools.update_plan]\nenabled = false", false),
        ("[tools.update_plan]\nenabled = true", true),
    ] {
        let config = Config::load_from_base_config_with_overrides(
            toml::from_str(config_toml).expect("TOML deserialization should succeed"),
            ConfigOverrides::default(),
            codex_home.abs(),
        )
        .await?;

        assert_eq!(
            config.update_plan_enabled, expected_enabled,
            "{config_toml}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn load_config_resolves_tool_registry_config() -> std::io::Result<()> {
    let codex_home = tempdir()?;

    for (config_toml, error_on_tool_collisions, turn_metadata_includes_tool_info) in [
        ("", false, false),
        (
            "[features.tool_registry]\nerror_on_tool_collisions = true\n",
            true,
            false,
        ),
        (
            "[features.tool_registry]\nturn_metadata_includes_tool_info = true\n",
            false,
            true,
        ),
    ] {
        let config_toml: ConfigToml =
            toml::from_str(config_toml).expect("TOML deserialization should succeed");
        let config = Config::load_from_base_config_with_overrides(
            config_toml,
            ConfigOverrides::default(),
            codex_home.abs(),
        )
        .await?;

        assert_eq!(
            config.tool_registry.error_on_tool_collisions,
            error_on_tool_collisions
        );
        assert_eq!(
            config.tool_registry.turn_metadata_includes_tool_info,
            turn_metadata_includes_tool_info
        );
    }

    Ok(())
}

#[tokio::test]
async fn load_config_resolves_token_budget_config() -> std::io::Result<()> {
    for (config_toml, expected) in [
        (
            "[features]\ntoken_budget = true\n",
            Some(TokenBudgetConfig::default()),
        ),
        (
            "features.context_management.experimental_mode = true\n",
            None,
        ),
        (
            r#"
[features.token_budget]
enabled = true
use_history_notes_extension = true
reminder_threshold_tokens = 16000
reminder_message_template = "Custom reminder: {n_remaining} tokens."
guidance_message = "Preserve important state before compaction."
auto_compact_fallback_prompt = "  Write notes immediately.  "
auto_compact_fallback_buffer_tokens = 8000
"#,
            Some(TokenBudgetConfig {
                use_history_notes_extension: true,
                reminder_threshold_tokens: Some(16_000),
                reminder_message_template: "Custom reminder: {n_remaining} tokens.".to_string(),
                guidance_message: Some("Preserve important state before compaction.".to_string()),
                auto_compact_fallback_prompt: Some("Write notes immediately.".to_string()),
                auto_compact_fallback_buffer_tokens: Some(8_000),
            }),
        ),
    ] {
        let codex_home = tempdir()?;
        let config_toml = toml::from_str(config_toml).expect("TOML should deserialize");
        let config = Config::load_from_base_config_with_overrides(
            config_toml,
            ConfigOverrides::default(),
            codex_home.abs(),
        )
        .await?;

        assert_eq!(
            config.features.enabled(Feature::TokenBudget),
            expected.is_some()
        );
        if expected.is_none() {
            assert!(config.features.enabled(Feature::ContextManagement));
        }
        assert_eq!(config.token_budget, expected);
    }
    Ok(())
}

#[tokio::test]
async fn load_config_rejects_overlong_auto_compact_fallback_prompt() -> std::io::Result<()> {
    let codex_home = tempdir()?;
    let prompt = "x".repeat(AUTO_COMPACT_FALLBACK_PROMPT_MAX_BYTES + 1);
    let config_toml = toml::from_str(&format!(
        "[features.token_budget]\nenabled = true\nauto_compact_fallback_prompt = {prompt:?}\n"
    ))
    .expect("TOML should deserialize");
    let error = Config::load_from_base_config_with_overrides(
        config_toml,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await
    .expect_err("overlong fallback prompt should be rejected");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    Ok(())
}

#[tokio::test]
async fn load_config_rejects_invalid_token_budget_reminder_template() -> std::io::Result<()> {
    for reminder_message_template in [
        String::new(),
        "x".repeat(TOKEN_BUDGET_REMINDER_MESSAGE_TEMPLATE_MAX_BYTES + 1),
    ] {
        let codex_home = tempdir()?;
        let config_toml = toml::from_str(&format!(
            "[features.token_budget]\nenabled = true\nreminder_message_template = {reminder_message_template:?}\n"
        ))
        .expect("TOML should deserialize");
        let error = Config::load_from_base_config_with_overrides(
            config_toml,
            ConfigOverrides::default(),
            codex_home.abs(),
        )
        .await
        .expect_err("invalid reminder template should be rejected");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
    Ok(())
}

#[tokio::test]
async fn load_config_rejects_non_positive_token_budget_reminder_threshold() -> std::io::Result<()> {
    for reminder_threshold_tokens in [-1, 0] {
        let codex_home = tempdir()?;
        let config_toml = toml::from_str(&format!(
            "[features.token_budget]\nenabled = true\nreminder_threshold_tokens = {reminder_threshold_tokens}\n"
        ))
        .expect("TOML should deserialize");
        let error = Config::load_from_base_config_with_overrides(
            config_toml,
            ConfigOverrides::default(),
            codex_home.abs(),
        )
        .await
        .expect_err("non-positive reminder threshold should be rejected");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "features.token_budget.reminder_threshold_tokens must be positive"
        );
    }
    Ok(())
}

#[tokio::test]
async fn load_config_rejects_non_positive_auto_compact_fallback_buffer() -> std::io::Result<()> {
    for auto_compact_fallback_buffer_tokens in [-1, 0] {
        let codex_home = tempdir()?;
        let config_toml = toml::from_str(&format!(
            "[features.token_budget]\nenabled = true\nauto_compact_fallback_prompt = \"Write notes.\"\nauto_compact_fallback_buffer_tokens = {auto_compact_fallback_buffer_tokens}\n"
        ))
        .expect("TOML should deserialize");
        let error = Config::load_from_base_config_with_overrides(
            config_toml,
            ConfigOverrides::default(),
            codex_home.abs(),
        )
        .await
        .expect_err("non-positive fallback buffer should be rejected");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "features.token_budget.auto_compact_fallback_buffer_tokens must be positive"
        );
    }
    Ok(())
}

#[tokio::test]
async fn load_config_rejects_missing_auto_compact_fallback_buffer() -> std::io::Result<()> {
    let codex_home = tempdir()?;
    let config_toml = toml::from_str(
        "[features.token_budget]\nenabled = true\nauto_compact_fallback_prompt = \"Write notes.\"\n",
    )
    .expect("TOML should deserialize");

    let error = Config::load_from_base_config_with_overrides(
        config_toml,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await
    .expect_err("missing fallback buffer should be rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        error.to_string(),
        "features.token_budget.auto_compact_fallback_buffer_tokens is required when auto_compact_fallback_prompt is set"
    );

    Ok(())
}

#[tokio::test]
async fn load_config_resolves_rollout_budget() -> std::io::Result<()> {
    let codex_home = tempdir()?;
    let config_toml: ConfigToml = toml::from_str(
        r#"
[features.rollout_budget]
enabled = true
limit_tokens = 100000
reminder_at_remaining_tokens = [50000, 25000, 10000]
sampling_token_weight = 1.0
prefill_token_weight = 0.1
"#,
    )
    .expect("TOML deserialization should succeed");
    let config = Config::load_from_base_config_with_overrides(
        config_toml,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert!(config.features.enabled(Feature::RolloutBudget));
    assert!(!config.features.enabled(Feature::TokenBudget));
    assert_eq!(
        config.rollout_budget,
        Some(RolloutBudgetConfig {
            limit_tokens: 100_000,
            reminder_at_remaining_tokens: vec![50_000, 25_000, 10_000],
            sampling_token_weight: 1.0,
            prefill_token_weight: 0.1,
        })
    );
    Ok(())
}

#[tokio::test]
async fn load_config_rejects_enabled_rollout_budget_without_limit() -> std::io::Result<()> {
    for config_toml in [
        "[features]\nrollout_budget = true\n",
        "[features.rollout_budget]\nenabled = true\n",
    ] {
        let codex_home = tempdir()?;
        let config_toml: ConfigToml =
            toml::from_str(config_toml).expect("TOML deserialization should succeed");
        let err = Config::load_from_base_config_with_overrides(
            config_toml,
            ConfigOverrides::default(),
            codex_home.abs(),
        )
        .await
        .expect_err("enabled rollout budget without limit_tokens should be rejected");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(
            err.to_string(),
            "features.rollout_budget.limit_tokens is required when rollout_budget is enabled"
        );
    }
    Ok(())
}

#[tokio::test]
async fn load_config_resolves_current_time_reminder() -> std::io::Result<()> {
    for (config_toml, expected) in [
        (
            r#"
[features]
current_time_reminder = true
"#,
            CurrentTimeReminderConfig::default(),
        ),
        (
            r#"
[features.current_time_reminder]
enabled = true
reminder_interval_seconds = 0
clock_source = "external"
delivery_mode = "after_user_or_tool_output"
sleep_tool = true
"#,
            CurrentTimeReminderConfig {
                reminder_interval_seconds: 0,
                clock_source: CurrentTimeSource::External,
                delivery_mode: CurrentTimeReminderDeliveryMode::AfterUserOrToolOutput,
                sleep_tool: true,
            },
        ),
    ] {
        let config = load_current_time_reminder_config(config_toml).await?;
        assert!(config.features.enabled(Feature::CurrentTimeReminder));
        assert_eq!(config.current_time_reminder, Some(expected));
    }
    Ok(())
}

async fn load_current_time_reminder_config(config_toml: &str) -> std::io::Result<Config> {
    let codex_home = tempdir()?;
    let config_toml = toml::from_str(config_toml).expect("TOML should deserialize");
    Config::load_from_base_config_with_overrides(
        config_toml,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await
}

#[test]
fn rejects_provider_auth_with_env_key() {
    let err = toml::from_str::<ConfigToml>(
        r#"
[model_providers.corp]
name = "Corp"
env_key = "CORP_TOKEN"

[model_providers.corp.auth]
command = "print-token"
"#,
    )
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("model_providers.corp: provider auth cannot be combined with env_key")
    );
}

#[test]
fn rejects_provider_aws_for_custom_provider() {
    let err = toml::from_str::<ConfigToml>(
        r#"
[model_providers.custom]
name = "Custom Provider"

[model_providers.custom.aws]
profile = "codex-bedrock"
"#,
    )
    .unwrap_err();

    assert!(
        err.to_string().contains(
            "model_providers.custom: provider aws is only supported for `amazon-bedrock`"
        )
    );
}

#[tokio::test]
async fn load_config_applies_amazon_bedrock_transport_overrides() {
    let cfg = toml::from_str::<ConfigToml>(
        r#"
model_provider = "amazon-bedrock"

[model_providers.amazon-bedrock]
base_url = "https://bedrock.example.com/v1"
http_headers = { "X-Custom-Header" = "value" }

[model_providers.amazon-bedrock.auth]
command = "print-token"
"#,
    )
    .expect("Amazon Bedrock transport overrides should deserialize");

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        tempdir().expect("tempdir").abs(),
    )
    .await
    .expect("load config");

    let mut expected_provider = built_in_model_providers(/*openai_base_url*/ None)
        .remove("amazon-bedrock")
        .expect("Amazon Bedrock provider should be built in");
    expected_provider.base_url = Some("https://bedrock.example.com/v1".to_string());
    expected_provider.auth = Some(ModelProviderAuthInfo {
        command: "print-token".to_string(),
        args: Vec::new(),
        timeout_ms: std::num::NonZeroU64::new(5_000).expect("timeout should be non-zero"),
        refresh_interval_ms: 300_000,
        cwd: std::env::current_dir()
            .expect("current directory should be available")
            .try_into()
            .expect("current directory should be absolute"),
    });
    expected_provider
        .http_headers
        .get_or_insert_default()
        .insert("X-Custom-Header".to_string(), "value".into());

    assert_eq!(config.model_provider_id, "amazon-bedrock");
    assert_eq!(config.model_provider, expected_provider);
}

#[test]
fn config_toml_rejects_unsupported_amazon_bedrock_overrides() {
    let err = toml::from_str::<ConfigToml>(
        r#"
model_provider = "amazon-bedrock"

[model_providers.amazon-bedrock]
name = "Custom Bedrock"
requires_openai_auth = true
supports_websockets = true
"#,
    )
    .expect_err("Amazon Bedrock unsupported overrides should fail validation");
    assert!(err.to_string().contains(
        "model_providers.amazon-bedrock only supports changing `base_url`, `auth`, `http_headers`, `aws.profile`, `aws.region`, `aws.credential_export`, and `aws.auth_refresh`; other non-default provider fields are not supported"
    ));
}

#[tokio::test]
async fn forced_chatgpt_workspace_id_empty_values_disable_runtime_restriction()
-> std::io::Result<()> {
    let cases: Vec<(&str, &str, Option<Vec<&str>>)> = vec![
        ("unset", "", None),
        ("empty string", r#"forced_chatgpt_workspace_id = """#, None),
        (
            "whitespace string",
            r#"forced_chatgpt_workspace_id = "   ""#,
            None,
        ),
        ("empty list", r#"forced_chatgpt_workspace_id = []"#, None),
        (
            "blank list entries",
            r#"forced_chatgpt_workspace_id = ["", "  "]"#,
            None,
        ),
        (
            "mixed list entries",
            r#"forced_chatgpt_workspace_id = ["", " 123e4567-e89b-42d3-a456-426614174000 ", "123e4567-e89b-42d3-a456-426614174001"]"#,
            Some(vec![
                "123e4567-e89b-42d3-a456-426614174000",
                "123e4567-e89b-42d3-a456-426614174001",
            ]),
        ),
    ];

    for (name, toml, expected) in cases {
        let cfg_toml: ConfigToml = toml::from_str(toml)
            .unwrap_or_else(|err| panic!("{name} should parse forced_chatgpt_workspace_id: {err}"));
        let config = Config::load_from_base_config_with_overrides(
            cfg_toml,
            ConfigOverrides::default(),
            tempdir().expect("tempdir").abs(),
        )
        .await?;

        let expected = expected.map(|values| {
            values
                .into_iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        });
        assert_eq!(config.forced_chatgpt_workspace_id, expected, "{name}");
    }

    Ok(())
}

#[tokio::test]
async fn legacy_remote_thread_store_endpoint_is_rejected() {
    let cfg: ConfigToml =
        toml::from_str(r#"experimental_thread_store_endpoint = "https://example.com""#)
            .expect("legacy remote thread-store endpoint should still deserialize");

    let err = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        tempdir().expect("tempdir").abs(),
    )
    .await
    .expect_err("legacy remote thread-store endpoint should be rejected at load time");

    assert!(
        err.to_string()
            .contains("experimental_thread_store_endpoint")
    );
    assert!(err.to_string().contains("no longer supported"));
}

#[tokio::test]
async fn rebuild_with_session_layers_refreshes_plugin_derived_mcp_config() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;
    let plugin_root = codex_home
        .path()
        .join("plugins/cache")
        .join("test/sample/local");
    std::fs::create_dir_all(plugin_root.join(".codex-plugin"))?;
    std::fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"sample"}"#,
    )?;
    std::fs::write(
        plugin_root.join(".mcp.json"),
        r#"{
  "mcpServers": {
    "sample": {
      "type": "http",
      "url": "https://sample.example/mcp"
    }
  }
}"#,
    )?;

    let user_file = AbsolutePathBuf::resolve_path_against_base(CONFIG_TOML_FILE, codex_home.path());
    let refreshed_layer_stack = ConfigLayerStack::new(vec![ConfigLayerEntry::new(
        ConfigLayerSource::User {
            file: user_file.clone(),
            profile: None,
        },
        toml::toml! {
            [features]
            plugins = true

            [plugins."sample@test"]
            enabled = true
        }
        .into(),
    )])?;
    let thread_layer_stack = ConfigLayerStack::new(vec![ConfigLayerEntry::new(
        ConfigLayerSource::User {
            file: user_file,
            profile: None,
        },
        toml::toml! {
            [features]
            plugins = false

            [plugins."sample@test"]
            enabled = true
        }
        .into(),
    )])?;
    let thread_config = Config::load_config_with_layer_stack(
        LOCAL_FS.as_ref(),
        thread_layer_stack.effective_config().try_into()?,
        ConfigOverrides {
            cwd: Some(codex_home.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
        thread_layer_stack,
    )
    .await?;
    let config = Config::rebuild_with_session_layers(
        &thread_config.config_layer_stack,
        thread_config.cwd.to_path_buf(),
        &refreshed_layer_stack,
        codex_home.abs(),
    )
    .await?;
    let plugins_manager =
        plugins_manager_for_config(&config, auth_manager_from_optional_auth(/*auth*/ None));
    let mcp_config = config.to_mcp_config(&plugins_manager).await;
    let configured_servers = mcp_config.mcp_server_catalog.configured_servers();

    assert_eq!(
        configured_servers.get("sample"),
        Some(&http_mcp("https://sample.example/mcp"))
    );
    assert_eq!(
        mcp_config
            .mcp_server_catalog
            .plugin_attributions_by_server_name(),
        HashMap::from([(
            "sample".to_string(),
            McpPluginAttribution::new("sample@test".to_string(), "sample".to_string())
                .with_host_root(PathUri::from_host_native_path(&plugin_root)?),
        )])
    );

    Ok(())
}

#[tokio::test]
async fn to_mcp_config_omits_plugin_id_when_user_server_shadows_plugin_mcp() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;
    let plugin_root = codex_home
        .path()
        .join("plugins/cache")
        .join("test/sample/local");
    std::fs::create_dir_all(plugin_root.join(".codex-plugin"))?;
    std::fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"sample"}"#,
    )?;
    std::fs::write(
        plugin_root.join(".mcp.json"),
        r#"{
  "mcpServers": {
    "sample": {
      "type": "http",
      "url": "https://plugin.example/mcp"
    }
  }
}"#,
    )?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"
[features]
plugins = true

[mcp_servers.sample]
url = "https://user.example/mcp"

[plugins."sample@test"]
enabled = true
"#,
    )?;

    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await?;
    let plugins_manager =
        plugins_manager_for_config(&config, auth_manager_from_optional_auth(/*auth*/ None));
    let mcp_config = config.to_mcp_config(&plugins_manager).await;
    let configured_servers = mcp_config.mcp_server_catalog.configured_servers();

    assert_eq!(
        configured_servers.get("sample"),
        Some(&http_mcp("https://user.example/mcp"))
    );
    assert!(
        mcp_config
            .mcp_server_catalog
            .plugin_attributions_by_server_name()
            .is_empty()
    );

    Ok(())
}

#[tokio::test]
async fn config_defaults_to_file_cli_auth_store_mode() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg = ConfigToml::default();

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.cli_auth_credentials_store_mode,
        AuthCredentialsStoreMode::File,
    );

    Ok(())
}

#[tokio::test]
async fn config_resolves_explicit_keyring_auth_store_mode() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg = ConfigToml {
        cli_auth_credentials_store: Some(AuthCredentialsStoreMode::Keyring),
        ..Default::default()
    };

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.cli_auth_credentials_store_mode,
        resolve_cli_auth_credentials_store_mode(
            AuthCredentialsStoreMode::Keyring,
            env!("CARGO_PKG_VERSION"),
        ),
    );

    Ok(())
}

#[tokio::test]
async fn config_resolves_default_oauth_store_mode() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg = ConfigToml::default();

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.mcp_oauth_credentials_store_mode,
        resolve_mcp_oauth_credentials_store_mode(
            OAuthCredentialsStoreMode::Auto,
            env!("CARGO_PKG_VERSION"),
        ),
    );

    Ok(())
}

#[test]
fn local_dev_builds_force_file_cli_auth_store_modes() {
    assert_eq!(
        resolve_cli_auth_credentials_store_mode(
            AuthCredentialsStoreMode::Keyring,
            LOCAL_DEV_BUILD_VERSION,
        ),
        AuthCredentialsStoreMode::File,
    );
    assert_eq!(
        resolve_cli_auth_credentials_store_mode(
            AuthCredentialsStoreMode::Auto,
            LOCAL_DEV_BUILD_VERSION,
        ),
        AuthCredentialsStoreMode::File,
    );
    assert_eq!(
        resolve_cli_auth_credentials_store_mode(
            AuthCredentialsStoreMode::Ephemeral,
            LOCAL_DEV_BUILD_VERSION,
        ),
        AuthCredentialsStoreMode::Ephemeral,
    );
    assert_eq!(
        resolve_cli_auth_credentials_store_mode(AuthCredentialsStoreMode::Keyring, "1.2.3"),
        AuthCredentialsStoreMode::Keyring,
    );
}

#[test]
fn local_dev_builds_force_file_mcp_oauth_store_modes() {
    assert_eq!(
        resolve_mcp_oauth_credentials_store_mode(
            OAuthCredentialsStoreMode::Keyring,
            LOCAL_DEV_BUILD_VERSION,
        ),
        OAuthCredentialsStoreMode::File,
    );
    assert_eq!(
        resolve_mcp_oauth_credentials_store_mode(
            OAuthCredentialsStoreMode::Auto,
            LOCAL_DEV_BUILD_VERSION,
        ),
        OAuthCredentialsStoreMode::File,
    );
    assert_eq!(
        resolve_mcp_oauth_credentials_store_mode(OAuthCredentialsStoreMode::Keyring, "1.2.3"),
        OAuthCredentialsStoreMode::Keyring,
    );
}

#[test]
fn web_search_mode_defaults_to_none_if_unset() {
    let cfg = ConfigToml::default();
    let features = Features::with_defaults();

    assert_eq!(resolve_web_search_mode(&cfg, &features), None);
}

#[test]
fn web_search_mode_prefers_config_over_legacy_flags() {
    let cfg = ConfigToml {
        web_search: Some(WebSearchMode::Live),
        ..Default::default()
    };
    let mut features = Features::with_defaults();
    features.enable(Feature::WebSearchCached);

    assert_eq!(
        resolve_web_search_mode(&cfg, &features),
        Some(WebSearchMode::Live)
    );
}

#[test]
fn web_search_mode_disabled_overrides_legacy_request() {
    let cfg = ConfigToml {
        web_search: Some(WebSearchMode::Disabled),
        ..Default::default()
    };
    let mut features = Features::with_defaults();
    features.enable(Feature::WebSearchRequest);

    assert_eq!(
        resolve_web_search_mode(&cfg, &features),
        Some(WebSearchMode::Disabled)
    );
}

#[test]
fn web_search_mode_for_turn_falls_back_when_provider_disallows_external_web_access() {
    for preferred in [
        WebSearchMode::Live,
        WebSearchMode::Cached,
        WebSearchMode::Indexed,
    ] {
        let mode = resolve_web_search_mode_for_turn(
            preferred,
            ProviderCapabilities {
                external_web_access: false,
                ..ProviderCapabilities::default()
            },
        );

        assert_eq!(mode, WebSearchMode::Cached);
    }
}

#[test]
fn web_search_mode_for_turn_prefers_live_over_cached() {
    let mode =
        resolve_web_search_mode_for_turn(WebSearchMode::Cached, ProviderCapabilities::default());

    assert_eq!(mode, WebSearchMode::Live);
}

#[test]
fn web_search_mode_for_turn_keeps_explicit_disabled_and_indexed() {
    for preferred in [WebSearchMode::Disabled, WebSearchMode::Indexed] {
        let mode = resolve_web_search_mode_for_turn(preferred, ProviderCapabilities::default());

        assert_eq!(mode, preferred);
    }
}

#[tokio::test]
async fn responses_websocket_features_do_not_change_wire_api() -> std::io::Result<()> {
    for feature_key in ["responses_websockets", "responses_websockets_v2"] {
        let codex_home = TempDir::new()?;
        let mut entries = BTreeMap::new();
        entries.insert(feature_key.to_string(), true);
        let cfg = ConfigToml {
            features: Some(FeaturesToml::from(entries)),
            ..Default::default()
        };

        let config = Config::load_from_base_config_with_overrides(
            cfg,
            ConfigOverrides::default(),
            codex_home.abs(),
        )
        .await?;

        assert_eq!(config.model_provider.wire_api, WireApi::Responses);
    }

    Ok(())
}

#[tokio::test]
async fn config_honors_explicit_file_oauth_store_mode() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg = ConfigToml {
        mcp_oauth_credentials_store: Some(OAuthCredentialsStoreMode::File),
        ..Default::default()
    };

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.mcp_oauth_credentials_store_mode,
        OAuthCredentialsStoreMode::File,
    );

    Ok(())
}

#[tokio::test]
async fn load_global_mcp_servers_returns_empty_if_missing() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;

    let servers = load_global_mcp_servers(codex_home.path()).await?;
    assert!(servers.is_empty());

    Ok(())
}

#[tokio::test]
async fn replace_mcp_servers_round_trips_entries() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;
    let expected_cwd = LegacyAppPathString::from_path(codex_home.path());

    let mut servers = BTreeMap::new();
    servers.insert(
        "docs".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::Stdio {
                command: "echo".to_string(),
                args: vec!["hello".to_string()],
                env: None,
                env_vars: Vec::new(),
                cwd: Some(expected_cwd.clone()),
            },
            environment_id: "remote".to_string(),
            enabled: true,
            required: false,
            startup_readiness: Default::default(),
            supports_parallel_tool_calls: false,
            tool_input_schema_max_bytes: std::num::NonZeroUsize::new(8_000),
            omit_tools_from: None,
            disabled_reason: None,
            startup_timeout_sec: Some(Duration::from_secs(3)),
            tool_timeout_sec: Some(Duration::from_secs(5)),
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    );

    apply_blocking(
        codex_home.path(),
        &[ConfigEdit::ReplaceMcpServers(servers.clone())],
    )?;

    let loaded = load_global_mcp_servers(codex_home.path()).await?;
    assert_eq!(loaded.len(), 1);
    let docs = loaded.get("docs").expect("docs entry");
    match &docs.transport {
        McpServerTransportConfig::Stdio {
            command,
            args,
            env,
            env_vars,
            cwd,
        } => {
            assert_eq!(command, "echo");
            assert_eq!(args, &vec!["hello".to_string()]);
            assert!(env.is_none());
            assert!(env_vars.is_empty());
            assert_eq!(cwd, &Some(expected_cwd));
        }
        other => panic!("unexpected transport {other:?}"),
    }
    assert_eq!(docs.startup_timeout_sec, Some(Duration::from_secs(3)));
    assert_eq!(docs.tool_timeout_sec, Some(Duration::from_secs(5)));
    assert_eq!(docs.environment_id, "remote");
    assert_eq!(
        docs.tool_input_schema_max_bytes,
        std::num::NonZeroUsize::new(8_000)
    );
    assert!(docs.enabled);

    let empty = BTreeMap::new();
    apply_blocking(
        codex_home.path(),
        &[ConfigEdit::ReplaceMcpServers(empty.clone())],
    )?;
    let loaded = load_global_mcp_servers(codex_home.path()).await?;
    assert!(loaded.is_empty());

    Ok(())
}

#[tokio::test]
async fn load_global_mcp_servers_accepts_legacy_ms_field() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;
    let config_path = codex_home.path().join(CONFIG_TOML_FILE);

    std::fs::write(
        &config_path,
        r#"
[mcp_servers]
[mcp_servers.docs]
command = "echo"
startup_timeout_ms = 2500
"#,
    )?;

    let servers = load_global_mcp_servers(codex_home.path()).await?;
    let docs = servers.get("docs").expect("docs entry");
    assert_eq!(docs.startup_timeout_sec, Some(Duration::from_millis(2500)));

    Ok(())
}

#[test]
fn mcp_servers_toml_ignores_unknown_server_fields() {
    let config = toml::from_str::<ConfigToml>(
        r#"
[mcp_servers.docs]
command = "docs-server"
trust_level = "trusted"
"#,
    )
    .expect("unknown MCP server fields should be ignored");

    assert_eq!(
        config.mcp_servers.get("docs"),
        Some(&stdio_mcp("docs-server"))
    );
}

#[tokio::test]
async fn to_mcp_config_preserves_apps_feature_from_config() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let mut config = Config::load_from_base_config_with_overrides(
        ConfigToml::default(),
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;
    let plugins_manager =
        plugins_manager_for_config(&config, auth_manager_from_optional_auth(/*auth*/ None));

    config.apps_mcp_product_sku = Some("tpp".to_string());
    let mcp_config = config.to_mcp_config(&plugins_manager).await;
    assert!(mcp_config.apps_enabled);
    assert_eq!(mcp_config.apps_mcp_product_sku.as_deref(), Some("tpp"));

    let _ = config.features.disable(Feature::Apps);
    let mcp_config = config.to_mcp_config(&plugins_manager).await;
    assert!(!mcp_config.apps_enabled);

    let _ = config.features.enable(Feature::Apps);
    let mcp_config = config.to_mcp_config(&plugins_manager).await;
    assert!(mcp_config.apps_enabled);

    Ok(())
}

#[tokio::test]
async fn to_mcp_config_flows_mcp_tool_prefix_from_feature() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let mut config = Config::load_from_base_config_with_overrides(
        ConfigToml::default(),
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;
    let plugins_manager =
        plugins_manager_for_config(&config, auth_manager_from_optional_auth(/*auth*/ None));

    let mcp_config = config.to_mcp_config(&plugins_manager).await;
    assert!(mcp_config.prefix_mcp_tool_names);
    assert!(mcp_config.non_prefixed_mcp_tool_servers.is_empty());

    let _ = config.features.enable(Feature::NonPrefixedMcpToolNames);
    let mcp_config = config.to_mcp_config(&plugins_manager).await;
    assert!(!mcp_config.prefix_mcp_tool_names);
    assert!(mcp_config.non_prefixed_mcp_tool_servers.is_empty());

    config.non_prefixed_mcp_tool_servers = Some(vec!["history".to_string(), "notes".to_string()]);
    let mcp_config = config.to_mcp_config(&plugins_manager).await;
    assert!(mcp_config.prefix_mcp_tool_names);
    assert_eq!(
        mcp_config.non_prefixed_mcp_tool_servers,
        vec!["history".to_string(), "notes".to_string()]
    );

    let _ = config.features.disable(Feature::NonPrefixedMcpToolNames);
    let mcp_config = config.to_mcp_config(&plugins_manager).await;
    assert!(mcp_config.prefix_mcp_tool_names);
    assert!(mcp_config.non_prefixed_mcp_tool_servers.is_empty());

    Ok(())
}

#[tokio::test]
async fn to_mcp_config_flows_independent_mcp_2026_features() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let mut config = Config::load_from_base_config_with_overrides(
        ConfigToml::default(),
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;
    let plugins_manager =
        plugins_manager_for_config(&config, auth_manager_from_optional_auth(/*auth*/ None));

    let mcp_config = config.to_mcp_config(&plugins_manager).await;
    assert_eq!(
        (
            mcp_config.protocol_mode,
            mcp_config.host_owned_apps_protocol_mode,
        ),
        (
            codex_mcp::McpProtocolMode::Legacy,
            codex_mcp::McpProtocolMode::Legacy,
        )
    );

    let _ = config.features.enable(Feature::Mcp20260728);
    let mcp_config = config.to_mcp_config(&plugins_manager).await;
    assert_eq!(
        (
            mcp_config.protocol_mode,
            mcp_config.host_owned_apps_protocol_mode,
        ),
        (
            codex_mcp::McpProtocolMode::V20260728,
            codex_mcp::McpProtocolMode::Legacy,
        )
    );

    let _ = config.features.disable(Feature::Mcp20260728);
    let _ = config.features.enable(Feature::CodexAppsMcp20260728);
    let mcp_config = config.to_mcp_config(&plugins_manager).await;
    assert_eq!(
        (
            mcp_config.protocol_mode,
            mcp_config.host_owned_apps_protocol_mode,
        ),
        (
            codex_mcp::McpProtocolMode::Legacy,
            codex_mcp::McpProtocolMode::V20260728,
        )
    );

    Ok(())
}

#[tokio::test]
async fn to_mcp_config_preserves_auth_elicitation_feature_from_config() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let mut config = Config::load_from_base_config_with_overrides(
        ConfigToml::default(),
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;
    let plugins_manager =
        plugins_manager_for_config(&config, auth_manager_from_optional_auth(/*auth*/ None));

    let mcp_config = config.to_mcp_config(&plugins_manager).await;
    assert_eq!(
        mcp_config.client_elicitation_capability,
        ElicitationCapability::new()
            .with_form(FormElicitationCapability::new())
            .with_url(UrlElicitationCapability::new())
    );

    let _ = config.features.disable(Feature::AuthElicitation);
    let mcp_config = config.to_mcp_config(&plugins_manager).await;
    assert_eq!(
        mcp_config.client_elicitation_capability,
        ElicitationCapability::default()
    );

    Ok(())
}

#[tokio::test]
async fn load_global_mcp_servers_rejects_inline_bearer_token() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;
    let config_path = codex_home.path().join(CONFIG_TOML_FILE);

    std::fs::write(
        &config_path,
        r#"
[mcp_servers.docs]
url = "https://example.com/mcp"
bearer_token = "secret"
"#,
    )?;

    let err = load_global_mcp_servers(codex_home.path())
        .await
        .expect_err("bearer_token entries should be rejected");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("bearer_token"));
    assert!(err.to_string().contains("bearer_token_env_var"));

    Ok(())
}

#[tokio::test]
async fn replace_mcp_servers_serializes_env_sorted() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;

    let servers = BTreeMap::from([(
        "docs".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::Stdio {
                command: "docs-server".to_string(),
                args: vec!["--verbose".to_string()],
                env: Some(HashMap::from([
                    ("ZIG_VAR".to_string(), "3".to_string()),
                    ("ALPHA_VAR".to_string(), "1".to_string()),
                ])),
                env_vars: Vec::new(),
                cwd: None,
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: true,
            required: false,
            startup_readiness: Default::default(),
            supports_parallel_tool_calls: false,
            tool_input_schema_max_bytes: None,
            omit_tools_from: None,
            disabled_reason: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    )]);

    apply_blocking(
        codex_home.path(),
        &[ConfigEdit::ReplaceMcpServers(servers.clone())],
    )?;

    let config_path = codex_home.path().join(CONFIG_TOML_FILE);
    let serialized = std::fs::read_to_string(&config_path)?;
    assert_eq!(
        serialized,
        r#"[mcp_servers.docs]
command = "docs-server"
args = ["--verbose"]

[mcp_servers.docs.env]
ALPHA_VAR = "1"
ZIG_VAR = "3"
"#
    );

    let loaded = load_global_mcp_servers(codex_home.path()).await?;
    let docs = loaded.get("docs").expect("docs entry");
    match &docs.transport {
        McpServerTransportConfig::Stdio {
            command,
            args,
            env,
            env_vars,
            cwd,
        } => {
            assert_eq!(command, "docs-server");
            assert_eq!(args, &vec!["--verbose".to_string()]);
            let env = env
                .as_ref()
                .expect("env should be preserved for stdio transport");
            assert_eq!(env.get("ALPHA_VAR"), Some(&"1".to_string()));
            assert_eq!(env.get("ZIG_VAR"), Some(&"3".to_string()));
            assert!(env_vars.is_empty());
            assert!(cwd.is_none());
        }
        other => panic!("unexpected transport {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn replace_mcp_servers_serializes_env_vars() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;

    let servers = BTreeMap::from([(
        "docs".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::Stdio {
                command: "docs-server".to_string(),
                args: Vec::new(),
                env: None,
                env_vars: vec!["ALPHA".into(), "BETA".into()],
                cwd: None,
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: true,
            required: false,
            startup_readiness: Default::default(),
            supports_parallel_tool_calls: false,
            tool_input_schema_max_bytes: None,
            omit_tools_from: None,
            disabled_reason: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    )]);

    apply_blocking(
        codex_home.path(),
        &[ConfigEdit::ReplaceMcpServers(servers.clone())],
    )?;

    let config_path = codex_home.path().join(CONFIG_TOML_FILE);
    let serialized = std::fs::read_to_string(&config_path)?;
    assert!(
        serialized.contains(r#"env_vars = ["ALPHA", "BETA"]"#),
        "serialized config missing env_vars field:\n{serialized}"
    );

    let loaded = load_global_mcp_servers(codex_home.path()).await?;
    let docs = loaded.get("docs").expect("docs entry");
    match &docs.transport {
        McpServerTransportConfig::Stdio { env_vars, .. } => {
            assert_eq!(env_vars, &vec!["ALPHA".into(), "BETA".into()]);
        }
        other => panic!("unexpected transport {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn replace_mcp_servers_serializes_sourced_env_vars() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;

    let servers = BTreeMap::from([(
        "docs".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::Stdio {
                command: "docs-server".to_string(),
                args: Vec::new(),
                env: None,
                env_vars: vec![
                    "LEGACY".into(),
                    McpServerEnvVar::Config {
                        name: "REMOTE_TOKEN".to_string(),
                        source: Some("remote".to_string()),
                    },
                ],
                cwd: None,
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: true,
            required: false,
            startup_readiness: Default::default(),
            supports_parallel_tool_calls: false,
            tool_input_schema_max_bytes: None,
            omit_tools_from: None,
            disabled_reason: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    )]);

    apply_blocking(
        codex_home.path(),
        &[ConfigEdit::ReplaceMcpServers(servers.clone())],
    )?;

    let config_path = codex_home.path().join(CONFIG_TOML_FILE);
    let serialized = std::fs::read_to_string(&config_path)?;
    assert!(
        serialized
            .contains(r#"env_vars = ["LEGACY", { name = "REMOTE_TOKEN", source = "remote" }]"#),
        "serialized config missing sourced env_vars field:\n{serialized}"
    );

    let loaded = load_global_mcp_servers(codex_home.path()).await?;
    assert_eq!(loaded, servers);

    Ok(())
}

#[tokio::test]
async fn replace_mcp_servers_serializes_cwd() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;

    let cwd_path = PathBuf::from("/tmp/codex-mcp");
    let cwd = LegacyAppPathString::from_path(&cwd_path);
    let servers = BTreeMap::from([(
        "docs".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::Stdio {
                command: "docs-server".to_string(),
                args: Vec::new(),
                env: None,
                env_vars: Vec::new(),
                cwd: Some(cwd.clone()),
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: true,
            required: false,
            startup_readiness: Default::default(),
            supports_parallel_tool_calls: false,
            tool_input_schema_max_bytes: None,
            omit_tools_from: None,
            disabled_reason: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    )]);

    apply_blocking(
        codex_home.path(),
        &[ConfigEdit::ReplaceMcpServers(servers.clone())],
    )?;

    let config_path = codex_home.path().join(CONFIG_TOML_FILE);
    let serialized = std::fs::read_to_string(&config_path)?;
    assert!(
        serialized.contains(r#"cwd = "/tmp/codex-mcp""#),
        "serialized config missing cwd field:\n{serialized}"
    );

    let loaded = load_global_mcp_servers(codex_home.path()).await?;
    let docs = loaded.get("docs").expect("docs entry");
    match &docs.transport {
        McpServerTransportConfig::Stdio { cwd, .. } => {
            assert_eq!(cwd, &Some(LegacyAppPathString::from_path(&cwd_path)));
        }
        other => panic!("unexpected transport {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn replace_mcp_servers_streamable_http_serializes_bearer_token() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;

    let servers = BTreeMap::from([(
        "docs".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::StreamableHttp {
                url: "https://example.com/mcp".to_string(),
                bearer_token_env_var: Some("MCP_TOKEN".to_string()),
                http_headers: None,
                env_http_headers: None,
                http_headers_helper: None,
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: true,
            required: false,
            startup_readiness: Default::default(),
            supports_parallel_tool_calls: false,
            tool_input_schema_max_bytes: None,
            omit_tools_from: None,
            disabled_reason: None,
            startup_timeout_sec: Some(Duration::from_secs(2)),
            tool_timeout_sec: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    )]);

    apply_blocking(
        codex_home.path(),
        &[ConfigEdit::ReplaceMcpServers(servers.clone())],
    )?;

    let config_path = codex_home.path().join(CONFIG_TOML_FILE);
    let serialized = std::fs::read_to_string(&config_path)?;
    assert_eq!(
        serialized,
        r#"[mcp_servers.docs]
url = "https://example.com/mcp"
bearer_token_env_var = "MCP_TOKEN"
startup_timeout_sec = 2.0
"#
    );

    let loaded = load_global_mcp_servers(codex_home.path()).await?;
    let docs = loaded.get("docs").expect("docs entry");
    match &docs.transport {
        McpServerTransportConfig::StreamableHttp {
            url,
            bearer_token_env_var,
            http_headers,
            env_http_headers,
            ..
        } => {
            assert_eq!(url, "https://example.com/mcp");
            assert_eq!(bearer_token_env_var.as_deref(), Some("MCP_TOKEN"));
            assert!(http_headers.is_none());
            assert!(env_http_headers.is_none());
        }
        other => panic!("unexpected transport {other:?}"),
    }
    assert_eq!(docs.startup_timeout_sec, Some(Duration::from_secs(2)));

    Ok(())
}

#[tokio::test]
async fn replace_mcp_servers_streamable_http_serializes_custom_headers() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;

    let servers = BTreeMap::from([(
        "docs".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::StreamableHttp {
                url: "https://example.com/mcp".to_string(),
                bearer_token_env_var: Some("MCP_TOKEN".to_string()),
                http_headers: Some(HashMap::from([("X-Doc".to_string(), "42".to_string())])),
                env_http_headers: Some(HashMap::from([(
                    "X-Auth".to_string(),
                    "DOCS_AUTH".to_string(),
                )])),
                http_headers_helper: None,
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: true,
            required: false,
            startup_readiness: Default::default(),
            supports_parallel_tool_calls: false,
            tool_input_schema_max_bytes: None,
            omit_tools_from: None,
            disabled_reason: None,
            startup_timeout_sec: Some(Duration::from_secs(2)),
            tool_timeout_sec: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    )]);
    apply_blocking(
        codex_home.path(),
        &[ConfigEdit::ReplaceMcpServers(servers.clone())],
    )?;

    let config_path = codex_home.path().join(CONFIG_TOML_FILE);
    let serialized = std::fs::read_to_string(&config_path)?;
    assert_eq!(
        serialized,
        r#"[mcp_servers.docs]
url = "https://example.com/mcp"
bearer_token_env_var = "MCP_TOKEN"
startup_timeout_sec = 2.0

[mcp_servers.docs.http_headers]
X-Doc = "42"

[mcp_servers.docs.env_http_headers]
X-Auth = "DOCS_AUTH"
"#
    );

    let loaded = load_global_mcp_servers(codex_home.path()).await?;
    let docs = loaded.get("docs").expect("docs entry");
    match &docs.transport {
        McpServerTransportConfig::StreamableHttp {
            http_headers,
            env_http_headers,
            ..
        } => {
            assert_eq!(
                http_headers,
                &Some(HashMap::from([("X-Doc".to_string(), "42".to_string())]))
            );
            assert_eq!(
                env_http_headers,
                &Some(HashMap::from([(
                    "X-Auth".to_string(),
                    "DOCS_AUTH".to_string()
                )]))
            );
        }
        other => panic!("unexpected transport {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn replace_mcp_servers_streamable_http_removes_optional_sections() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;

    let config_path = codex_home.path().join(CONFIG_TOML_FILE);

    let mut servers = BTreeMap::from([(
        "docs".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::StreamableHttp {
                url: "https://example.com/mcp".to_string(),
                bearer_token_env_var: Some("MCP_TOKEN".to_string()),
                http_headers: Some(HashMap::from([("X-Doc".to_string(), "42".to_string())])),
                env_http_headers: Some(HashMap::from([(
                    "X-Auth".to_string(),
                    "DOCS_AUTH".to_string(),
                )])),
                http_headers_helper: None,
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: true,
            required: false,
            startup_readiness: Default::default(),
            supports_parallel_tool_calls: false,
            tool_input_schema_max_bytes: None,
            omit_tools_from: None,
            disabled_reason: None,
            startup_timeout_sec: Some(Duration::from_secs(2)),
            tool_timeout_sec: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    )]);

    apply_blocking(
        codex_home.path(),
        &[ConfigEdit::ReplaceMcpServers(servers.clone())],
    )?;
    let serialized_with_optional = std::fs::read_to_string(&config_path)?;
    assert!(serialized_with_optional.contains("bearer_token_env_var = \"MCP_TOKEN\""));
    assert!(serialized_with_optional.contains("[mcp_servers.docs.http_headers]"));
    assert!(serialized_with_optional.contains("[mcp_servers.docs.env_http_headers]"));

    servers.insert(
        "docs".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::StreamableHttp {
                url: "https://example.com/mcp".to_string(),
                bearer_token_env_var: None,
                http_headers: None,
                env_http_headers: None,
                http_headers_helper: None,
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: true,
            required: false,
            startup_readiness: Default::default(),
            supports_parallel_tool_calls: false,
            tool_input_schema_max_bytes: None,
            omit_tools_from: None,
            disabled_reason: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    );
    apply_blocking(
        codex_home.path(),
        &[ConfigEdit::ReplaceMcpServers(servers.clone())],
    )?;

    let serialized = std::fs::read_to_string(&config_path)?;
    assert_eq!(
        serialized,
        r#"[mcp_servers.docs]
url = "https://example.com/mcp"
"#
    );

    let loaded = load_global_mcp_servers(codex_home.path()).await?;
    let docs = loaded.get("docs").expect("docs entry");
    match &docs.transport {
        McpServerTransportConfig::StreamableHttp {
            url,
            bearer_token_env_var,
            http_headers,
            env_http_headers,
            ..
        } => {
            assert_eq!(url, "https://example.com/mcp");
            assert!(bearer_token_env_var.is_none());
            assert!(http_headers.is_none());
            assert!(env_http_headers.is_none());
        }
        other => panic!("unexpected transport {other:?}"),
    }

    assert!(docs.startup_timeout_sec.is_none());

    Ok(())
}

#[tokio::test]
async fn replace_mcp_servers_streamable_http_isolates_headers_between_servers() -> anyhow::Result<()>
{
    let codex_home = TempDir::new()?;
    let config_path = codex_home.path().join(CONFIG_TOML_FILE);

    let servers = BTreeMap::from([
        (
            "docs".to_string(),
            McpServerConfig {
                auth: Default::default(),
                transport: McpServerTransportConfig::StreamableHttp {
                    url: "https://example.com/mcp".to_string(),
                    bearer_token_env_var: Some("MCP_TOKEN".to_string()),
                    http_headers: Some(HashMap::from([("X-Doc".to_string(), "42".to_string())])),
                    env_http_headers: Some(HashMap::from([(
                        "X-Auth".to_string(),
                        "DOCS_AUTH".to_string(),
                    )])),
                    http_headers_helper: None,
                },
                environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
                enabled: true,
                required: false,
                startup_readiness: Default::default(),
                supports_parallel_tool_calls: false,
                tool_input_schema_max_bytes: None,
                omit_tools_from: None,
                disabled_reason: None,
                startup_timeout_sec: Some(Duration::from_secs(2)),
                tool_timeout_sec: None,
                enabled_tools: None,
                disabled_tools: None,
                scopes: None,
                oauth: None,
                oauth_resource: None,
                tools: HashMap::new(),
            },
        ),
        (
            "logs".to_string(),
            McpServerConfig {
                auth: Default::default(),
                transport: McpServerTransportConfig::Stdio {
                    command: "logs-server".to_string(),
                    args: vec!["--follow".to_string()],
                    env: None,
                    env_vars: Vec::new(),
                    cwd: None,
                },
                environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
                enabled: true,
                required: false,
                startup_readiness: Default::default(),
                supports_parallel_tool_calls: false,
                tool_input_schema_max_bytes: None,
                omit_tools_from: None,
                disabled_reason: None,
                startup_timeout_sec: None,
                tool_timeout_sec: None,
                enabled_tools: None,
                disabled_tools: None,
                scopes: None,
                oauth: None,
                oauth_resource: None,
                tools: HashMap::new(),
            },
        ),
    ]);

    apply_blocking(
        codex_home.path(),
        &[ConfigEdit::ReplaceMcpServers(servers.clone())],
    )?;

    let serialized = std::fs::read_to_string(&config_path)?;
    assert!(
        serialized.contains("[mcp_servers.docs.http_headers]"),
        "serialized config missing docs headers section:\n{serialized}"
    );
    assert!(
        !serialized.contains("[mcp_servers.logs.http_headers]"),
        "serialized config should not add logs headers section:\n{serialized}"
    );
    assert!(
        !serialized.contains("[mcp_servers.logs.env_http_headers]"),
        "serialized config should not add logs env headers section:\n{serialized}"
    );
    assert!(
        !serialized.contains("mcp_servers.logs.bearer_token_env_var"),
        "serialized config should not add bearer token to logs:\n{serialized}"
    );

    let loaded = load_global_mcp_servers(codex_home.path()).await?;
    let docs = loaded.get("docs").expect("docs entry");
    match &docs.transport {
        McpServerTransportConfig::StreamableHttp {
            http_headers,
            env_http_headers,
            ..
        } => {
            assert_eq!(
                http_headers,
                &Some(HashMap::from([("X-Doc".to_string(), "42".to_string())]))
            );
            assert_eq!(
                env_http_headers,
                &Some(HashMap::from([(
                    "X-Auth".to_string(),
                    "DOCS_AUTH".to_string()
                )]))
            );
        }
        other => panic!("unexpected transport {other:?}"),
    }
    let logs = loaded.get("logs").expect("logs entry");
    match &logs.transport {
        McpServerTransportConfig::Stdio { env, .. } => {
            assert!(env.is_none());
        }
        other => panic!("unexpected transport {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn replace_mcp_servers_serializes_disabled_flag() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;

    let servers = BTreeMap::from([(
        "docs".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::Stdio {
                command: "docs-server".to_string(),
                args: Vec::new(),
                env: None,
                env_vars: Vec::new(),
                cwd: None,
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: false,
            required: false,
            startup_readiness: Default::default(),
            supports_parallel_tool_calls: false,
            tool_input_schema_max_bytes: None,
            omit_tools_from: None,
            disabled_reason: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    )]);

    apply_blocking(
        codex_home.path(),
        &[ConfigEdit::ReplaceMcpServers(servers.clone())],
    )?;

    let config_path = codex_home.path().join(CONFIG_TOML_FILE);
    let serialized = std::fs::read_to_string(&config_path)?;
    assert!(
        serialized.contains("enabled = false"),
        "serialized config missing disabled flag:\n{serialized}"
    );

    let loaded = load_global_mcp_servers(codex_home.path()).await?;
    let docs = loaded.get("docs").expect("docs entry");
    assert!(!docs.enabled);

    Ok(())
}

#[tokio::test]
async fn replace_mcp_servers_serializes_required_flag() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;

    let servers = BTreeMap::from([(
        "docs".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::Stdio {
                command: "docs-server".to_string(),
                args: Vec::new(),
                env: None,
                env_vars: Vec::new(),
                cwd: None,
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: true,
            required: true,
            startup_readiness: Default::default(),
            supports_parallel_tool_calls: false,
            tool_input_schema_max_bytes: None,
            omit_tools_from: None,
            disabled_reason: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    )]);

    apply_blocking(
        codex_home.path(),
        &[ConfigEdit::ReplaceMcpServers(servers.clone())],
    )?;

    let config_path = codex_home.path().join(CONFIG_TOML_FILE);
    let serialized = std::fs::read_to_string(&config_path)?;
    assert!(
        serialized.contains("required = true"),
        "serialized config missing required flag:\n{serialized}"
    );

    let loaded = load_global_mcp_servers(codex_home.path()).await?;
    let docs = loaded.get("docs").expect("docs entry");
    assert!(docs.required);

    Ok(())
}

#[tokio::test]
async fn replace_mcp_servers_serializes_tool_filters() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;

    let servers = BTreeMap::from([(
        "docs".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::Stdio {
                command: "docs-server".to_string(),
                args: Vec::new(),
                env: None,
                env_vars: Vec::new(),
                cwd: None,
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: true,
            required: false,
            startup_readiness: Default::default(),
            supports_parallel_tool_calls: false,
            tool_input_schema_max_bytes: None,
            omit_tools_from: None,
            disabled_reason: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            enabled_tools: Some(vec!["allowed".to_string()]),
            disabled_tools: Some(vec!["blocked".to_string()]),
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    )]);

    apply_blocking(
        codex_home.path(),
        &[ConfigEdit::ReplaceMcpServers(servers.clone())],
    )?;

    let config_path = codex_home.path().join(CONFIG_TOML_FILE);
    let serialized = std::fs::read_to_string(&config_path)?;
    assert!(serialized.contains(r#"enabled_tools = ["allowed"]"#));
    assert!(serialized.contains(r#"disabled_tools = ["blocked"]"#));

    let loaded = load_global_mcp_servers(codex_home.path()).await?;
    let docs = loaded.get("docs").expect("docs entry");
    assert_eq!(
        docs.enabled_tools.as_ref(),
        Some(&vec!["allowed".to_string()])
    );
    assert_eq!(
        docs.disabled_tools.as_ref(),
        Some(&vec!["blocked".to_string()])
    );

    Ok(())
}

#[tokio::test]
async fn replace_mcp_servers_streamable_http_serializes_oauth_resource() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;

    let servers = BTreeMap::from([(
        "docs".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::StreamableHttp {
                url: "https://example.com/mcp".to_string(),
                bearer_token_env_var: None,
                http_headers: None,
                env_http_headers: None,
                http_headers_helper: None,
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: true,
            required: false,
            startup_readiness: Default::default(),
            supports_parallel_tool_calls: false,
            tool_input_schema_max_bytes: None,
            omit_tools_from: None,
            disabled_reason: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: Some(McpServerOAuthConfig {
                client_id: Some("eci-prd-pub-codex-123".to_string()),
                callback_url: None,
                callback_port: None,
                ..Default::default()
            }),
            oauth_resource: Some("https://resource.example.com".to_string()),
            tools: HashMap::new(),
        },
    )]);

    apply_blocking(
        codex_home.path(),
        &[ConfigEdit::ReplaceMcpServers(servers.clone())],
    )?;

    let config_path = codex_home.path().join(CONFIG_TOML_FILE);
    let serialized = std::fs::read_to_string(&config_path)?;
    assert!(serialized.contains("[mcp_servers.docs.oauth]"));
    assert!(serialized.contains(r#"client_id = "eci-prd-pub-codex-123""#));
    assert!(serialized.contains(r#"oauth_resource = "https://resource.example.com""#));

    let loaded = load_global_mcp_servers(codex_home.path()).await?;
    let docs = loaded.get("docs").expect("docs entry");
    assert_eq!(
        docs.oauth_resource.as_deref(),
        Some("https://resource.example.com")
    );
    assert_eq!(docs.oauth_client_id(), Some("eci-prd-pub-codex-123"));

    Ok(())
}

#[tokio::test]
async fn set_model_updates_defaults() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;

    ConfigEditsBuilder::new(codex_home.path())
        .set_model(Some("gpt-5.4"), Some(ReasoningEffort::High))
        .apply()
        .await?;

    let serialized = tokio::fs::read_to_string(codex_home.path().join(CONFIG_TOML_FILE)).await?;
    let parsed: ConfigToml = toml::from_str(&serialized)?;

    assert_eq!(parsed.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(parsed.model_reasoning_effort, Some(ReasoningEffort::High));

    Ok(())
}

#[tokio::test]
async fn for_config_writes_selected_user_config_file() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;
    let base_config = codex_home.path().join(CONFIG_TOML_FILE);
    let selected_config = codex_home.path().join("work.config.toml");
    tokio::fs::write(&base_config, r#"model_provider = "openai""#).await?;
    tokio::fs::write(&selected_config, r#"model = "gpt-old""#).await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .loader_overrides(LoaderOverrides {
            user_config_path: Some(selected_config.abs()),
            user_config_profile: Some("work".parse().expect("profile-v2 name")),
            ..LoaderOverrides::without_managed_config_for_tests()
        })
        .build()
        .await?;

    ConfigEditsBuilder::for_config(&config)
        .set_model(Some("gpt-new"), Some(ReasoningEffort::High))
        .apply()
        .await?;

    let selected_serialized = tokio::fs::read_to_string(&selected_config).await?;
    let selected: ConfigToml = toml::from_str(&selected_serialized)?;
    assert_eq!(selected.model.as_deref(), Some("gpt-new"));
    assert_eq!(selected.model_reasoning_effort, Some(ReasoningEffort::High));
    assert_eq!(
        tokio::fs::read_to_string(&base_config).await?,
        r#"model_provider = "openai""#
    );

    Ok(())
}

#[tokio::test]
async fn set_model_overwrites_existing_model() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;
    let config_path = codex_home.path().join(CONFIG_TOML_FILE);

    tokio::fs::write(
        &config_path,
        r#"
model = "gpt-5.4"
model_reasoning_effort = "medium"

[profiles.dev]
model = "gpt-4.1"
"#,
    )
    .await?;

    ConfigEditsBuilder::new(codex_home.path())
        .set_model(Some("o4-mini"), Some(ReasoningEffort::High))
        .apply()
        .await?;

    let serialized = tokio::fs::read_to_string(config_path).await?;
    let parsed: ConfigToml = toml::from_str(&serialized)?;

    assert_eq!(parsed.model.as_deref(), Some("o4-mini"));
    assert_eq!(parsed.model_reasoning_effort, Some(ReasoningEffort::High));
    assert_eq!(
        parsed
            .profiles
            .get("dev")
            .and_then(|profile| profile.model.as_deref()),
        Some("gpt-4.1"),
    );

    Ok(())
}

struct PrecedenceTestFixture {
    cwd: TempDir,
    codex_home: TempDir,
    cfg: ConfigToml,
}

impl PrecedenceTestFixture {
    fn cwd_path(&self) -> PathBuf {
        self.cwd.path().to_path_buf()
    }

    fn codex_home(&self) -> AbsolutePathBuf {
        self.codex_home.abs()
    }
}

#[tokio::test]
async fn cli_override_sets_compact_prompt() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let overrides = ConfigOverrides {
        compact_prompt: Some("Use the compact override".to_string()),
        ..Default::default()
    };

    let config = Config::load_from_base_config_with_overrides(
        ConfigToml::default(),
        overrides,
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.compact_prompt.as_deref(),
        Some("Use the compact override")
    );

    Ok(())
}

#[tokio::test]
async fn loads_compact_prompt_from_file() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let workspace = codex_home.path().join("workspace");
    std::fs::create_dir_all(&workspace)?;

    let prompt_path = workspace.join("compact_prompt.txt");
    std::fs::write(&prompt_path, "  summarize differently  ")?;

    let cfg = ConfigToml {
        experimental_compact_prompt_file: Some(prompt_path.abs()),
        ..Default::default()
    };

    let overrides = ConfigOverrides {
        cwd: Some(workspace),
        ..Default::default()
    };

    let config =
        Config::load_from_base_config_with_overrides(cfg, overrides, codex_home.abs()).await?;

    assert_eq!(
        config.compact_prompt.as_deref(),
        Some("summarize differently")
    );

    Ok(())
}

#[tokio::test]
async fn load_config_rejects_missing_agent_role_config_file() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let missing_path = codex_home.path().join("agents").join("researcher.toml");
    let cfg = ConfigToml {
        agents: Some(AgentsToml {
            enabled: None,
            max_concurrent_threads_per_session: None,
            max_depth: None,
            default_subagent_model: None,
            default_subagent_reasoning_effort: None,
            job_max_runtime_seconds: None,
            interrupt_message: None,
            roles: BTreeMap::from([(
                "researcher".to_string(),
                AgentRoleToml {
                    description: Some("Research role".to_string()),
                    config_file: Some(missing_path.abs()),
                    nickname_candidates: None,
                },
            )]),
        }),
        ..Default::default()
    };

    let result = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await;
    let err = result.expect_err("missing role config file should be rejected");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    let message = err.to_string();
    assert!(message.contains("agents.researcher.config_file"));
    assert!(message.contains("must point to an existing file"));

    Ok(())
}

#[tokio::test]
async fn agent_role_relative_config_file_resolves_against_config_toml() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let role_config_path = codex_home.path().join("agents").join("researcher.toml");
    tokio::fs::create_dir_all(
        role_config_path
            .parent()
            .expect("role config should have a parent directory"),
    )
    .await?;
    tokio::fs::write(
        &role_config_path,
        "developer_instructions = \"Research carefully\"\nmodel = \"gpt-5\"",
    )
    .await?;
    tokio::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[agents.researcher]
description = "Research role"
config_file = "./agents/researcher.toml"
nickname_candidates = ["Hypatia", "Noether"]
"#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;
    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.config_file.as_ref()),
        Some(&role_config_path)
    );
    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.nickname_candidates.as_ref())
            .map(|candidates| candidates.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["Hypatia", "Noether"])
    );

    Ok(())
}

#[tokio::test]
async fn agent_role_relative_config_file_resolves_from_config_layer() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let role_config_path = codex_home.path().join("agents").join("researcher.toml");
    tokio::fs::create_dir_all(
        role_config_path
            .parent()
            .expect("role config should have a parent directory"),
    )
    .await?;
    tokio::fs::write(
        &role_config_path,
        "developer_instructions = \"Research carefully\"\nmodel = \"gpt-5\"",
    )
    .await?;
    let layer_config = toml::from_str(
        r#"[agents.researcher]
description = "Research role"
config_file = "./agents/researcher.toml"
"#,
    )
    .expect("agent role layer config should parse");
    let config_layer_stack =
        codex_config::ConfigLayerStack::new(vec![codex_config::ConfigLayerEntry::new(
            ConfigLayerSource::User {
                file: codex_home.path().join(CONFIG_TOML_FILE).abs(),
                profile: None,
            },
            layer_config,
        )])
        .map_err(std::io::Error::other)?;

    let config = Config::load_config_with_layer_stack(
        LOCAL_FS.as_ref(),
        ConfigToml::default(),
        ConfigOverrides {
            cwd: Some(codex_home.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
        config_layer_stack,
    )
    .await?;

    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.config_file.as_ref()),
        Some(&role_config_path)
    );

    Ok(())
}

#[tokio::test]
async fn agent_role_file_metadata_overrides_config_toml_metadata() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let role_config_path = codex_home.path().join("agents").join("researcher.toml");
    tokio::fs::create_dir_all(
        role_config_path
            .parent()
            .expect("role config should have a parent directory"),
    )
    .await?;
    tokio::fs::write(
        &role_config_path,
        r#"
description = "Role metadata from file"
nickname_candidates = ["Hypatia"]
developer_instructions = "Research carefully"
model = "gpt-5.2"
"#,
    )
    .await?;
    tokio::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[agents.researcher]
description = "Research role from config"
config_file = "./agents/researcher.toml"
nickname_candidates = ["Noether"]
"#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;
    let role = config
        .agent_roles
        .get("researcher")
        .expect("researcher role should load");
    assert_eq!(role.description.as_deref(), Some("Role metadata from file"));
    assert_eq!(role.config_file.as_ref(), Some(&role_config_path));
    assert_eq!(
        role.nickname_candidates
            .as_ref()
            .map(|candidates| candidates.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["Hypatia"])
    );

    Ok(())
}

#[tokio::test]
async fn agent_role_file_without_developer_instructions_is_dropped_with_warning()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let repo_root = TempDir::new()?;
    let nested_cwd = repo_root.path().join("packages").join("app");
    std::fs::create_dir_all(repo_root.path().join(".git"))?;
    std::fs::write(repo_root.path().join(".git/HEAD"), "ref: refs/heads/main\n")?;
    std::fs::create_dir_all(&nested_cwd)?;

    tokio::fs::write(codex_home.path().join(CONFIG_TOML_FILE), "").await?;

    let standalone_agents_dir = repo_root.path().join(".codex").join("agents");
    tokio::fs::create_dir_all(&standalone_agents_dir).await?;
    tokio::fs::write(
        standalone_agents_dir.join("researcher.toml"),
        r#"
name = "researcher"
description = "Role metadata from file"
model = "gpt-5.2"
"#,
    )
    .await?;
    tokio::fs::write(
        standalone_agents_dir.join("reviewer.toml"),
        r#"
name = "reviewer"
description = "Review role"
developer_instructions = "Review carefully"
model = "gpt-5.2"
"#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(nested_cwd),
            ..Default::default()
        })
        .build()
        .await?;
    assert!(!config.agent_roles.contains_key("researcher"));
    assert_eq!(
        config
            .agent_roles
            .get("reviewer")
            .and_then(|role| role.description.as_deref()),
        Some("Review role")
    );
    assert!(
        config
            .startup_warnings
            .iter()
            .any(|warning| warning.contains("must define `developer_instructions`"))
    );

    Ok(())
}

#[tokio::test]
async fn legacy_agent_role_config_file_allows_missing_developer_instructions() -> std::io::Result<()>
{
    let codex_home = TempDir::new()?;
    let role_config_path = codex_home.path().join("agents").join("researcher.toml");
    tokio::fs::create_dir_all(
        role_config_path
            .parent()
            .expect("role config should have a parent directory"),
    )
    .await?;
    tokio::fs::write(
        &role_config_path,
        r#"
model = "gpt-5.2"
model_reasoning_effort = "high"
"#,
    )
    .await?;
    tokio::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[agents.researcher]
description = "Research role from config"
config_file = "./agents/researcher.toml"
"#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;
    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.description.as_deref()),
        Some("Research role from config")
    );
    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.config_file.as_ref()),
        Some(&role_config_path)
    );

    Ok(())
}

#[tokio::test]
async fn agent_role_without_description_after_merge_is_dropped_with_warning() -> std::io::Result<()>
{
    let codex_home = TempDir::new()?;
    let role_config_path = codex_home.path().join("agents").join("researcher.toml");
    tokio::fs::create_dir_all(
        role_config_path
            .parent()
            .expect("role config should have a parent directory"),
    )
    .await?;
    tokio::fs::write(
        &role_config_path,
        r#"
developer_instructions = "Research carefully"
model = "gpt-5.2"
"#,
    )
    .await?;
    tokio::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[agents.researcher]
config_file = "./agents/researcher.toml"

[agents.reviewer]
description = "Review role"
"#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;
    assert!(!config.agent_roles.contains_key("researcher"));
    assert_eq!(
        config
            .agent_roles
            .get("reviewer")
            .and_then(|role| role.description.as_deref()),
        Some("Review role")
    );
    assert!(
        config
            .startup_warnings
            .iter()
            .any(|warning| warning.contains("agent role `researcher` must define a description"))
    );

    Ok(())
}

#[tokio::test]
async fn discovered_agent_role_file_without_name_is_dropped_with_warning() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let repo_root = TempDir::new()?;
    let nested_cwd = repo_root.path().join("packages").join("app");
    std::fs::create_dir_all(repo_root.path().join(".git"))?;
    std::fs::write(repo_root.path().join(".git/HEAD"), "ref: refs/heads/main\n")?;
    std::fs::create_dir_all(&nested_cwd)?;

    tokio::fs::write(codex_home.path().join(CONFIG_TOML_FILE), "").await?;

    let standalone_agents_dir = repo_root.path().join(".codex").join("agents");
    tokio::fs::create_dir_all(&standalone_agents_dir).await?;
    tokio::fs::write(
        standalone_agents_dir.join("researcher.toml"),
        r#"
description = "Role metadata from file"
developer_instructions = "Research carefully"
"#,
    )
    .await?;
    tokio::fs::write(
        standalone_agents_dir.join("reviewer.toml"),
        r#"
name = "reviewer"
description = "Review role"
developer_instructions = "Review carefully"
"#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(nested_cwd),
            ..Default::default()
        })
        .build()
        .await?;
    assert!(!config.agent_roles.contains_key("researcher"));
    assert_eq!(
        config
            .agent_roles
            .get("reviewer")
            .and_then(|role| role.description.as_deref()),
        Some("Review role")
    );
    assert!(
        config
            .startup_warnings
            .iter()
            .any(|warning| warning.contains("must define a non-empty `name`"))
    );

    Ok(())
}

#[tokio::test]
async fn agent_role_file_name_takes_precedence_over_config_key() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let role_config_path = codex_home.path().join("agents").join("researcher.toml");
    tokio::fs::create_dir_all(
        role_config_path
            .parent()
            .expect("role config should have a parent directory"),
    )
    .await?;
    tokio::fs::write(
        &role_config_path,
        r#"
name = "archivist"
description = "Role metadata from file"
developer_instructions = "Research carefully"
model = "gpt-5.2"
"#,
    )
    .await?;
    tokio::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[agents.researcher]
description = "Research role from config"
config_file = "./agents/researcher.toml"
"#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;
    assert_eq!(config.agent_roles.contains_key("researcher"), false);
    let role = config
        .agent_roles
        .get("archivist")
        .expect("role should use file-provided name");
    assert_eq!(role.description.as_deref(), Some("Role metadata from file"));
    assert_eq!(role.config_file.as_ref(), Some(&role_config_path));

    Ok(())
}

#[tokio::test]
async fn loads_legacy_split_agent_roles_from_config_toml() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let researcher_path = codex_home.path().join("agents").join("researcher.toml");
    let reviewer_path = codex_home.path().join("agents").join("reviewer.toml");
    tokio::fs::create_dir_all(
        researcher_path
            .parent()
            .expect("role config should have a parent directory"),
    )
    .await?;
    tokio::fs::write(
        &researcher_path,
        "developer_instructions = \"Research carefully\"\nmodel = \"gpt-5\"",
    )
    .await?;
    tokio::fs::write(
        &reviewer_path,
        "developer_instructions = \"Review carefully\"\nmodel = \"gpt-4.1\"",
    )
    .await?;
    tokio::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[agents.researcher]
description = "Research role"
config_file = "./agents/researcher.toml"
nickname_candidates = ["Hypatia", "Noether"]

[agents.reviewer]
description = "Review role"
config_file = "./agents/reviewer.toml"
nickname_candidates = ["Atlas"]
"#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.description.as_deref()),
        Some("Research role")
    );
    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.config_file.as_ref()),
        Some(&researcher_path)
    );
    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.nickname_candidates.as_ref())
            .map(|candidates| candidates.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["Hypatia", "Noether"])
    );
    assert_eq!(
        config
            .agent_roles
            .get("reviewer")
            .and_then(|role| role.description.as_deref()),
        Some("Review role")
    );
    assert_eq!(
        config
            .agent_roles
            .get("reviewer")
            .and_then(|role| role.config_file.as_ref()),
        Some(&reviewer_path)
    );
    assert_eq!(
        config
            .agent_roles
            .get("reviewer")
            .and_then(|role| role.nickname_candidates.as_ref())
            .map(|candidates| candidates.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["Atlas"])
    );

    Ok(())
}

#[tokio::test]
async fn discovers_multiple_standalone_agent_role_files() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let repo_root = TempDir::new()?;
    let nested_cwd = repo_root.path().join("packages").join("app");
    std::fs::create_dir_all(repo_root.path().join(".git"))?;
    std::fs::write(repo_root.path().join(".git/HEAD"), "ref: refs/heads/main\n")?;
    std::fs::create_dir_all(&nested_cwd)?;

    std::fs::write(codex_home.path().join(CONFIG_TOML_FILE), "")?;

    let root_agent = repo_root
        .path()
        .join(".codex")
        .join("agents")
        .join("root.toml");
    std::fs::create_dir_all(
        root_agent
            .parent()
            .expect("root agent should have a parent directory"),
    )?;
    std::fs::write(
        &root_agent,
        r#"
name = "researcher"
description = "from root"
developer_instructions = "Research carefully"
"#,
    )?;

    let nested_agent = repo_root
        .path()
        .join("packages")
        .join(".codex")
        .join("agents")
        .join("review")
        .join("nested.toml");
    std::fs::create_dir_all(
        nested_agent
            .parent()
            .expect("nested agent should have a parent directory"),
    )?;
    std::fs::write(
        &nested_agent,
        r#"
name = "reviewer"
description = "from nested"
nickname_candidates = ["Atlas"]
developer_instructions = "Review carefully"
"#,
    )?;

    let sibling_agent = repo_root
        .path()
        .join("packages")
        .join(".codex")
        .join("agents")
        .join("writer.toml");
    std::fs::create_dir_all(
        sibling_agent
            .parent()
            .expect("sibling agent should have a parent directory"),
    )?;
    std::fs::write(
        &sibling_agent,
        r#"
name = "writer"
description = "from sibling"
nickname_candidates = ["Sagan"]
developer_instructions = "Write carefully"
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(nested_cwd),
            ..Default::default()
        })
        .build()
        .await?;

    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.description.as_deref()),
        Some("from root")
    );
    assert_eq!(
        config
            .agent_roles
            .get("reviewer")
            .and_then(|role| role.description.as_deref()),
        Some("from nested")
    );
    assert_eq!(
        config
            .agent_roles
            .get("reviewer")
            .and_then(|role| role.nickname_candidates.as_ref())
            .map(|candidates| candidates.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["Atlas"])
    );
    assert_eq!(
        config
            .agent_roles
            .get("writer")
            .and_then(|role| role.description.as_deref()),
        Some("from sibling")
    );
    assert_eq!(
        config
            .agent_roles
            .get("writer")
            .and_then(|role| role.nickname_candidates.as_ref())
            .map(|candidates| candidates.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["Sagan"])
    );

    Ok(())
}

#[tokio::test]
async fn mixed_legacy_and_standalone_agent_role_sources_merge_with_precedence()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let repo_root = TempDir::new()?;
    let nested_cwd = repo_root.path().join("packages").join("app");
    std::fs::create_dir_all(repo_root.path().join(".git"))?;
    std::fs::write(repo_root.path().join(".git/HEAD"), "ref: refs/heads/main\n")?;
    std::fs::create_dir_all(&nested_cwd)?;

    tokio::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        format!(
            r#"[agents.researcher]
description = "Research role from config"
config_file = "./agents/researcher.toml"
nickname_candidates = ["Noether"]

[agents.critic]
description = "Critic role from config"
config_file = "./agents/critic.toml"
nickname_candidates = ["Ada"]
"#
        ),
    )
    .await?;

    let home_agents_dir = codex_home.path().join("agents");
    tokio::fs::create_dir_all(&home_agents_dir).await?;
    tokio::fs::write(
        home_agents_dir.join("researcher.toml"),
        r#"
developer_instructions = "Research carefully"
model = "gpt-5.2"
"#,
    )
    .await?;
    tokio::fs::write(
        home_agents_dir.join("critic.toml"),
        r#"
developer_instructions = "Critique carefully"
model = "gpt-4.1"
"#,
    )
    .await?;

    let standalone_agents_dir = repo_root.path().join(".codex").join("agents");
    tokio::fs::create_dir_all(&standalone_agents_dir).await?;
    tokio::fs::write(
        standalone_agents_dir.join("researcher.toml"),
        r#"
name = "researcher"
description = "Research role from file"
nickname_candidates = ["Hypatia"]
developer_instructions = "Research from file"
model = "gpt-5-mini"
"#,
    )
    .await?;
    tokio::fs::write(
        standalone_agents_dir.join("writer.toml"),
        r#"
name = "writer"
description = "Writer role from file"
nickname_candidates = ["Sagan"]
developer_instructions = "Write carefully"
model = "gpt-5.2"
"#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(nested_cwd),
            ..Default::default()
        })
        .build()
        .await?;

    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.description.as_deref()),
        Some("Research role from file")
    );
    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.config_file.as_ref()),
        Some(&standalone_agents_dir.join("researcher.toml"))
    );
    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.nickname_candidates.as_ref())
            .map(|candidates| candidates.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["Hypatia"])
    );
    assert_eq!(
        config
            .agent_roles
            .get("critic")
            .and_then(|role| role.description.as_deref()),
        Some("Critic role from config")
    );
    assert_eq!(
        config
            .agent_roles
            .get("critic")
            .and_then(|role| role.config_file.as_ref()),
        Some(&home_agents_dir.join("critic.toml"))
    );
    assert_eq!(
        config
            .agent_roles
            .get("critic")
            .and_then(|role| role.nickname_candidates.as_ref())
            .map(|candidates| candidates.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["Ada"])
    );
    assert_eq!(
        config
            .agent_roles
            .get("writer")
            .and_then(|role| role.description.as_deref()),
        Some("Writer role from file")
    );
    assert_eq!(
        config
            .agent_roles
            .get("writer")
            .and_then(|role| role.nickname_candidates.as_ref())
            .map(|candidates| candidates.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["Sagan"])
    );

    Ok(())
}

#[tokio::test]
async fn higher_precedence_agent_role_can_inherit_description_from_lower_layer()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let repo_root = TempDir::new()?;
    let nested_cwd = repo_root.path().join("packages").join("app");
    std::fs::create_dir_all(repo_root.path().join(".git"))?;
    std::fs::write(repo_root.path().join(".git/HEAD"), "ref: refs/heads/main\n")?;
    std::fs::create_dir_all(&nested_cwd)?;

    tokio::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        format!(
            r#"[agents.researcher]
description = "Research role from config"
config_file = "./agents/researcher.toml"
"#
        ),
    )
    .await?;

    let home_agents_dir = codex_home.path().join("agents");
    tokio::fs::create_dir_all(&home_agents_dir).await?;
    tokio::fs::write(
        home_agents_dir.join("researcher.toml"),
        r#"
developer_instructions = "Research carefully"
model = "gpt-5.2"
"#,
    )
    .await?;

    let standalone_agents_dir = repo_root.path().join(".codex").join("agents");
    tokio::fs::create_dir_all(&standalone_agents_dir).await?;
    tokio::fs::write(
        standalone_agents_dir.join("researcher.toml"),
        r#"
name = "researcher"
nickname_candidates = ["Hypatia"]
developer_instructions = "Research from file"
model = "gpt-5-mini"
"#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(nested_cwd),
            ..Default::default()
        })
        .build()
        .await?;

    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.description.as_deref()),
        Some("Research role from config")
    );
    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.config_file.as_ref()),
        Some(&standalone_agents_dir.join("researcher.toml"))
    );
    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.nickname_candidates.as_ref())
            .map(|candidates| candidates.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["Hypatia"])
    );

    Ok(())
}

#[test]
fn legacy_agent_job_max_runtime_seconds_is_accepted_as_noop() {
    let parsed = toml::from_str::<ConfigToml>(
        r#"
[agents]
job_max_runtime_seconds = 900
"#,
    )
    .expect("legacy agent job setting should deserialize");

    assert_eq!(
        parsed.agents,
        Some(AgentsToml {
            job_max_runtime_seconds: Some(900),
            ..Default::default()
        })
    );
}

#[tokio::test]
async fn load_config_resolves_agent_controls() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg = ConfigToml {
        agents: Some(AgentsToml {
            enabled: Some(false),
            max_depth: Some(2),
            default_subagent_model: Some("gpt-5.6-terra".to_string()),
            default_subagent_reasoning_effort: Some(ReasoningEffort::High),
            interrupt_message: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        (
            config.agents_enabled,
            config.agent_max_depth,
            config.agent_default_subagent_model.as_deref(),
            config.agent_default_subagent_reasoning_effort,
            config.agent_interrupt_message_enabled,
        ),
        (
            false,
            2,
            Some("gpt-5.6-terra"),
            Some(ReasoningEffort::High),
            false,
        )
    );

    Ok(())
}

#[test]
fn agents_max_threads_alias_matches_canonical_config() {
    let canonical: ConfigToml = toml::from_str(
        r#"[agents]
max_concurrent_threads_per_session = 7
"#,
    )
    .expect("canonical agents thread limit should parse");
    let legacy: ConfigToml = toml::from_str(
        r#"[agents]
max_threads = 7
"#,
    )
    .expect("legacy agents thread limit should parse");

    assert_eq!(legacy, canonical);
    let serialized = toml::to_string(&legacy).expect("agents config should serialize");
    assert!(serialized.contains("max_concurrent_threads_per_session = 7"));
    assert!(!serialized.contains("max_threads"));
}

#[tokio::test]
async fn load_config_normalizes_agent_role_nickname_candidates() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg = ConfigToml {
        agents: Some(AgentsToml {
            enabled: None,
            max_concurrent_threads_per_session: None,
            max_depth: None,
            default_subagent_model: None,
            default_subagent_reasoning_effort: None,
            job_max_runtime_seconds: None,
            interrupt_message: None,
            roles: BTreeMap::from([(
                "researcher".to_string(),
                AgentRoleToml {
                    description: Some("Research role".to_string()),
                    config_file: None,
                    nickname_candidates: Some(vec![
                        "  Hypatia  ".to_string(),
                        "Noether".to_string(),
                    ]),
                },
            )]),
        }),
        ..Default::default()
    };

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config
            .agent_roles
            .get("researcher")
            .and_then(|role| role.nickname_candidates.as_ref())
            .map(|candidates| candidates.iter().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["Hypatia", "Noether"])
    );

    Ok(())
}

#[tokio::test]
async fn load_config_rejects_empty_agent_role_nickname_candidates() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg = ConfigToml {
        agents: Some(AgentsToml {
            enabled: None,
            max_concurrent_threads_per_session: None,
            max_depth: None,
            default_subagent_model: None,
            default_subagent_reasoning_effort: None,
            job_max_runtime_seconds: None,
            interrupt_message: None,
            roles: BTreeMap::from([(
                "researcher".to_string(),
                AgentRoleToml {
                    description: Some("Research role".to_string()),
                    config_file: None,
                    nickname_candidates: Some(Vec::new()),
                },
            )]),
        }),
        ..Default::default()
    };

    let result = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await;
    let err = result.expect_err("empty nickname candidates should be rejected");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        err.to_string()
            .contains("agents.researcher.nickname_candidates")
    );

    Ok(())
}

#[tokio::test]
async fn load_config_rejects_duplicate_agent_role_nickname_candidates() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg = ConfigToml {
        agents: Some(AgentsToml {
            enabled: None,
            max_concurrent_threads_per_session: None,
            max_depth: None,
            default_subagent_model: None,
            default_subagent_reasoning_effort: None,
            job_max_runtime_seconds: None,
            interrupt_message: None,
            roles: BTreeMap::from([(
                "researcher".to_string(),
                AgentRoleToml {
                    description: Some("Research role".to_string()),
                    config_file: None,
                    nickname_candidates: Some(vec!["Hypatia".to_string(), " Hypatia ".to_string()]),
                },
            )]),
        }),
        ..Default::default()
    };

    let result = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await;
    let err = result.expect_err("duplicate nickname candidates should be rejected");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        err.to_string()
            .contains("agents.researcher.nickname_candidates cannot contain duplicates")
    );

    Ok(())
}

#[tokio::test]
async fn load_config_rejects_unsafe_agent_role_nickname_candidates() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg = ConfigToml {
        agents: Some(AgentsToml {
            enabled: None,
            max_concurrent_threads_per_session: None,
            max_depth: None,
            default_subagent_model: None,
            default_subagent_reasoning_effort: None,
            job_max_runtime_seconds: None,
            interrupt_message: None,
            roles: BTreeMap::from([(
                "researcher".to_string(),
                AgentRoleToml {
                    description: Some("Research role".to_string()),
                    config_file: None,
                    nickname_candidates: Some(vec!["Agent <One>".to_string()]),
                },
            )]),
        }),
        ..Default::default()
    };

    let result = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await;
    let err = result.expect_err("unsafe nickname candidates should be rejected");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(err.to_string().contains(
            "agents.researcher.nickname_candidates may only contain ASCII letters, digits, spaces, hyphens, and underscores"
        ));

    Ok(())
}

#[tokio::test]
async fn model_catalog_json_loads_from_path() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let catalog_path = codex_home.path().join("catalog.json");
    let mut catalog = bundled_models_response()
        .unwrap_or_else(|err| panic!("bundled models.json should parse: {err}"));
    catalog.models = catalog.models.into_iter().take(1).collect();
    std::fs::write(
        &catalog_path,
        serde_json::to_string(&catalog).expect("serialize catalog"),
    )?;

    let cfg = ConfigToml {
        model_catalog_json: Some(catalog_path.abs()),
        ..Default::default()
    };

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(config.model_catalog, Some(catalog));
    Ok(())
}

#[tokio::test]
async fn model_catalog_json_rejects_empty_catalog() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let catalog_path = codex_home.path().join("catalog.json");
    std::fs::write(&catalog_path, r#"{"models":[]}"#)?;

    let cfg = ConfigToml {
        model_catalog_json: Some(catalog_path.abs()),
        ..Default::default()
    };

    let err = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await
    .expect_err("empty custom catalog should fail config load");

    assert_eq!(err.kind(), ErrorKind::InvalidData);
    assert!(
        err.to_string().contains("must contain at least one model"),
        "unexpected error: {err}"
    );
    Ok(())
}

fn create_test_fixture() -> std::io::Result<PrecedenceTestFixture> {
    let toml = r#"
model = "o3"
approval_policy = "on-request"

[analytics]
enabled = true

[model_providers.openai-custom]
name = "OpenAI custom"
base_url = "https://api.openai.com/v1"
env_key = "OPENAI_API_KEY"
wire_api = "responses"
request_max_retries = 4            # retry failed HTTP requests
stream_max_retries = 10            # retry dropped SSE streams
stream_idle_timeout_ms = 300000    # 5m idle timeout
websocket_connect_timeout_ms = 15000

[profiles.o3]
model = "o3"
model_provider = "openai"
approval_policy = "never"
model_reasoning_effort = "high"
model_reasoning_summary = "detailed"

[profiles.gpt3]
model = "gpt-3.5-turbo"
model_provider = "openai-custom"

[profiles.zdr]
model = "o3"
model_provider = "openai"
approval_policy = "on-request"

[profiles.zdr.analytics]
enabled = false

[profiles.gpt5]
model = "gpt-5.4"
model_provider = "openai"
approval_policy = "on-request"
model_reasoning_effort = "high"
model_reasoning_summary = "detailed"
model_verbosity = "high"
"#;

    let cfg: ConfigToml = toml::from_str(toml).expect("TOML deserialization should succeed");

    // Use a temporary directory for the cwd so it does not contain an
    // AGENTS.md file.
    let cwd_temp_dir = TempDir::new().unwrap();
    let cwd = cwd_temp_dir.path().to_path_buf();
    // Make it look like a Git repo so it does not search for AGENTS.md in
    // a parent folder, either.
    std::fs::write(cwd.join(".git"), "gitdir: nowhere")?;

    let codex_home_temp_dir = TempDir::new().unwrap();

    Ok(PrecedenceTestFixture {
        cwd: cwd_temp_dir,
        codex_home: codex_home_temp_dir,
        cfg,
    })
}

#[tokio::test]
async fn metrics_exporter_defaults_to_statsig_when_missing() -> std::io::Result<()> {
    let fixture = create_test_fixture()?;

    let config = Config::load_from_base_config_with_overrides(
        fixture.cfg.clone(),
        ConfigOverrides {
            cwd: Some(fixture.cwd_path()),
            ..Default::default()
        },
        fixture.codex_home(),
    )
    .await?;

    assert_eq!(config.otel.metrics_exporter, OtelExporterKind::Statsig);
    assert!(!config.otel.agent_response_logging_enabled());
    Ok(())
}

#[tokio::test]
async fn trace_exporter_defaults_to_none_when_log_exporter_is_set() -> std::io::Result<()> {
    let fixture = create_test_fixture()?;
    let mut cfg = fixture.cfg.clone();
    cfg.otel = Some(OtelConfigToml {
        tool_result: toml::from_str("max_bytes = 8192").expect("tool-result logging config"),
        log_agent_responses: Some(true),
        exporter: Some(OtelExporterKind::OtlpHttp {
            endpoint: "http://localhost:14318/v1/logs".to_string(),
            headers: HashMap::new(),
            protocol: codex_config::types::OtelHttpProtocol::Binary,
            tls: None,
        }),
        metrics_exporter: Some(OtelExporterKind::None),
        ..Default::default()
    });

    let mut config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides {
            cwd: Some(fixture.cwd_path()),
            ..Default::default()
        },
        fixture.codex_home(),
    )
    .await?;

    assert_eq!(config.otel.tool_result.max_bytes, 8192);
    assert!(config.otel.agent_response_logging_enabled());
    assert!(matches!(
        config.otel.exporter,
        OtelExporterKind::OtlpHttp { .. }
    ));
    assert_eq!(config.otel.trace_exporter, OtelExporterKind::None);
    config.otel.exporter = OtelExporterKind::None;
    Ok(())
}

#[tokio::test]
async fn load_config_applies_otel_trace_metadata() -> std::io::Result<()> {
    let mut fixture = create_test_fixture()?;
    fixture.cfg = toml::from_str(
        r#"
[otel.span_attributes]
"example.trace_attr" = "enabled"

[otel.tracestate.example]
alpha = "one"
beta = "two"
"#,
    )
    .expect("TOML deserialization should succeed");

    let config = Config::load_from_base_config_with_overrides(
        fixture.cfg.clone(),
        ConfigOverrides {
            cwd: Some(fixture.cwd_path()),
            ..Default::default()
        },
        fixture.codex_home(),
    )
    .await?;

    assert_eq!(
        config.otel.span_attributes,
        BTreeMap::from([("example.trace_attr".to_string(), "enabled".to_string())])
    );
    assert_eq!(
        config.otel.tracestate,
        BTreeMap::from([(
            "example".to_string(),
            BTreeMap::from([
                ("alpha".to_string(), "one".to_string()),
                ("beta".to_string(), "two".to_string()),
            ]),
        )])
    );
    Ok(())
}

#[tokio::test]
async fn load_config_drops_invalid_otel_trace_metadata_entries() -> std::io::Result<()> {
    let mut fixture = create_test_fixture()?;
    fixture.cfg = toml::from_str(
        r#"
[otel]
environment = "test"

[otel.span_attributes]
"" = "missing-key"
"example.trace_attr" = "enabled"

[otel.tracestate.example]
alpha = "one"
beta = "two\ntoo"

[otel.tracestate.bad]
alpha = "one\ntwo"
"#,
    )
    .expect("TOML deserialization should succeed");

    let config = Config::load_from_base_config_with_overrides(
        fixture.cfg.clone(),
        ConfigOverrides {
            cwd: Some(fixture.cwd_path()),
            ..Default::default()
        },
        fixture.codex_home(),
    )
    .await?;

    assert_eq!(config.otel.environment, "test");
    assert_eq!(
        config.otel.span_attributes,
        BTreeMap::from([("example.trace_attr".to_string(), "enabled".to_string())])
    );
    assert_eq!(
        config.otel.tracestate,
        BTreeMap::from([(
            "example".to_string(),
            BTreeMap::from([("alpha".to_string(), "one".to_string())]),
        )])
    );
    assert!(
        config.startup_warnings.iter().any(|warning| {
            warning.contains("Ignoring invalid `otel.span_attributes` config")
                && warning.contains("configured span attribute key must not be empty")
        }),
        "{:?}",
        config.startup_warnings
    );
    assert!(
        config.startup_warnings.iter().any(|warning| {
            warning.contains("Ignoring invalid `otel.tracestate` config")
                && warning.contains("invalid configured tracestate value for example.beta")
        }),
        "{:?}",
        config.startup_warnings
    );
    assert!(
        config.startup_warnings.iter().any(|warning| {
            warning.contains("Ignoring invalid `otel.tracestate` config")
                && warning.contains("invalid configured tracestate value for bad.alpha")
        }),
        "{:?}",
        config.startup_warnings
    );
    Ok(())
}

#[tokio::test]
async fn explicit_null_service_tier_override_maps_to_default_service_tier() -> std::io::Result<()> {
    let fixture = create_test_fixture()?;

    let config = Config::load_from_base_config_with_overrides(
        fixture.cfg.clone(),
        ConfigOverrides {
            cwd: Some(fixture.cwd_path()),
            service_tier: Some(None),
            ..Default::default()
        },
        fixture.codex_home(),
    )
    .await?;

    assert_eq!(
        config.service_tier,
        Some(SERVICE_TIER_DEFAULT_REQUEST_VALUE.to_string())
    );
    Ok(())
}

#[tokio::test]
async fn default_service_tier_override_uses_default_request_value() -> std::io::Result<()> {
    let fixture = create_test_fixture()?;

    let config = Config::load_from_base_config_with_overrides(
        fixture.cfg.clone(),
        ConfigOverrides {
            cwd: Some(fixture.cwd_path()),
            service_tier: Some(Some("default".to_string())),
            ..Default::default()
        },
        fixture.codex_home(),
    )
    .await?;

    assert_eq!(
        config.service_tier,
        Some(SERVICE_TIER_DEFAULT_REQUEST_VALUE.to_string())
    );
    Ok(())
}

#[tokio::test]
async fn legacy_fast_service_tier_override_uses_priority_request_value() -> std::io::Result<()> {
    let fixture = create_test_fixture()?;

    let config = Config::load_from_base_config_with_overrides(
        fixture.cfg.clone(),
        ConfigOverrides {
            cwd: Some(fixture.cwd_path()),
            service_tier: Some(Some("fast".to_string())),
            ..Default::default()
        },
        fixture.codex_home(),
    )
    .await?;

    assert_eq!(
        config.service_tier,
        Some(ServiceTier::Fast.request_value().to_string())
    );
    Ok(())
}

#[tokio::test]
async fn config_toml_priority_service_tier_uses_priority_request_value() -> std::io::Result<()> {
    let mut fixture = create_test_fixture()?;
    fixture.cfg.service_tier = Some(ServiceTier::Fast.request_value().to_string());
    let cwd = fixture.cwd_path();
    let codex_home = fixture.codex_home();

    let config = Config::load_from_base_config_with_overrides(
        fixture.cfg,
        ConfigOverrides {
            cwd: Some(cwd),
            ..Default::default()
        },
        codex_home,
    )
    .await?;

    assert_eq!(
        config.service_tier,
        Some(ServiceTier::Fast.request_value().to_string())
    );
    Ok(())
}

#[tokio::test]
async fn config_toml_service_tier_accepts_arbitrary_string() -> std::io::Result<()> {
    let mut fixture = create_test_fixture()?;
    fixture.cfg.service_tier = Some("experimental-tier-id".to_string());
    let cwd = fixture.cwd_path();
    let codex_home = fixture.codex_home();

    let config = Config::load_from_base_config_with_overrides(
        fixture.cfg,
        ConfigOverrides {
            cwd: Some(cwd),
            ..Default::default()
        },
        codex_home,
    )
    .await?;

    assert_eq!(
        config.service_tier,
        Some("experimental-tier-id".to_string())
    );
    Ok(())
}

#[tokio::test]
async fn config_toml_legacy_fast_service_tier_uses_priority_request_value() -> std::io::Result<()> {
    let mut fixture = create_test_fixture()?;
    fixture.cfg.service_tier = Some("fast".to_string());
    let cwd = fixture.cwd_path();
    let codex_home = fixture.codex_home();

    let config = Config::load_from_base_config_with_overrides(
        fixture.cfg,
        ConfigOverrides {
            cwd: Some(cwd),
            ..Default::default()
        },
        codex_home,
    )
    .await?;

    assert_eq!(
        config.service_tier,
        Some(ServiceTier::Fast.request_value().to_string())
    );
    Ok(())
}

#[test]
fn test_set_default_oss_provider() -> std::io::Result<()> {
    let temp_dir = TempDir::new()?;
    let codex_home = temp_dir.path();
    let config_path = codex_home.join(CONFIG_TOML_FILE);

    // Test setting valid provider on empty config
    set_default_oss_provider(codex_home, OLLAMA_OSS_PROVIDER_ID)?;
    let content = std::fs::read_to_string(&config_path)?;
    assert!(content.contains("oss_provider = \"ollama\""));

    // Test updating existing config
    std::fs::write(&config_path, "model = \"gpt-4\"\n")?;
    set_default_oss_provider(codex_home, LMSTUDIO_OSS_PROVIDER_ID)?;
    let content = std::fs::read_to_string(&config_path)?;
    assert!(content.contains("oss_provider = \"lmstudio\""));
    assert!(content.contains("model = \"gpt-4\""));

    // Test overwriting existing oss_provider
    set_default_oss_provider(codex_home, OLLAMA_OSS_PROVIDER_ID)?;
    let content = std::fs::read_to_string(&config_path)?;
    assert!(content.contains("oss_provider = \"ollama\""));
    assert!(!content.contains("oss_provider = \"lmstudio\""));

    // Test invalid provider
    let result = set_default_oss_provider(codex_home, "invalid_provider");
    assert!(result.is_err());
    let error = result.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("Invalid OSS provider"));
    assert!(error.to_string().contains("invalid_provider"));

    Ok(())
}

#[test]
fn test_set_default_oss_provider_rejects_legacy_ollama_chat_provider() -> std::io::Result<()> {
    let temp_dir = TempDir::new()?;
    let codex_home = temp_dir.path();

    let result = set_default_oss_provider(codex_home, LEGACY_OLLAMA_CHAT_PROVIDER_ID);
    assert!(result.is_err());
    let error = result.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        error
            .to_string()
            .contains(OLLAMA_CHAT_PROVIDER_REMOVED_ERROR)
    );

    Ok(())
}

#[tokio::test]
async fn test_load_config_rejects_legacy_ollama_chat_provider_with_helpful_error()
-> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg = ConfigToml {
        model_provider: Some(LEGACY_OLLAMA_CHAT_PROVIDER_ID.to_string()),
        ..Default::default()
    };

    let result = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await;
    assert!(result.is_err());
    let error = result.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert!(
        error
            .to_string()
            .contains(OLLAMA_CHAT_PROVIDER_REMOVED_ERROR)
    );

    Ok(())
}

#[test]
fn test_resolve_oss_provider_explicit_override() {
    let config_toml = ConfigToml::default();
    let result = resolve_oss_provider(Some("custom-provider"), &config_toml);
    assert_eq!(result, Some("custom-provider".to_string()));
}

#[test]
fn test_resolve_oss_provider_from_global_config() {
    let config_toml = ConfigToml {
        oss_provider: Some("global-provider".to_string()),
        ..Default::default()
    };

    let result = resolve_oss_provider(/*explicit_provider*/ None, &config_toml);
    assert_eq!(result, Some("global-provider".to_string()));
}

#[test]
fn test_resolve_oss_provider_none_when_not_configured() {
    let config_toml = ConfigToml::default();
    let result = resolve_oss_provider(/*explicit_provider*/ None, &config_toml);
    assert_eq!(result, None);
}

#[test]
fn test_resolve_oss_provider_explicit_overrides_global() {
    let config_toml = ConfigToml {
        oss_provider: Some("global-provider".to_string()),
        ..Default::default()
    };

    let result = resolve_oss_provider(Some("explicit-provider"), &config_toml);
    assert_eq!(result, Some("explicit-provider".to_string()));
}

#[test]
fn config_toml_deserializes_mcp_oauth_callback_port() {
    let toml = r#"mcp_oauth_callback_port = 4321"#;
    let cfg: ConfigToml =
        toml::from_str(toml).expect("TOML deserialization should succeed for callback port");
    assert_eq!(cfg.mcp_oauth_callback_port, Some(4321));
}

#[test]
fn config_toml_deserializes_mcp_oauth_callback_url() {
    let toml = r#"mcp_oauth_callback_url = "https://example.com/callback""#;
    let cfg: ConfigToml =
        toml::from_str(toml).expect("TOML deserialization should succeed for callback URL");
    assert_eq!(
        cfg.mcp_oauth_callback_url.as_deref(),
        Some("https://example.com/callback")
    );
}

#[tokio::test]
async fn config_loads_mcp_oauth_callback_port_from_toml() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let toml = r#"
model = "gpt-5.4"
mcp_oauth_callback_port = 5678
"#;
    let cfg: ConfigToml =
        toml::from_str(toml).expect("TOML deserialization should succeed for callback port");

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(config.mcp_oauth_callback_port, Some(5678));
    Ok(())
}

#[tokio::test]
async fn config_loads_allow_login_shell_from_toml() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg: ConfigToml = toml::from_str(
        r#"
model = "gpt-5.4"
allow_login_shell = false
"#,
    )
    .expect("TOML deserialization should succeed for allow_login_shell");

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert!(!config.allow_login_shell);
    Ok(())
}

#[tokio::test]
async fn config_loads_apps_mcp_product_sku_from_toml() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let toml = r#"
model = "gpt-5.4"
apps_mcp_product_sku = "tpp"
"#;
    let cfg: ConfigToml =
        toml::from_str(toml).expect("TOML deserialization should succeed for apps MCP SKU");

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(config.apps_mcp_product_sku.as_deref(), Some("tpp"));
    Ok(())
}

#[tokio::test]
async fn config_loads_cloud_skills_with_legacy_noop() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    for (settings, expected) in [
        ("", true),
        ("[cloud.skills]", true),
        ("[cloud.skills]\nenabled = false", false),
        ("[cloud.skills]\nenabled = true", true),
        ("[orchestrator.skills]\nenabled = false", true),
        ("[orchestrator.skills]\nenabled = true", true),
        (
            "[orchestrator.skills]\nenabled = false\n[cloud.skills]",
            true,
        ),
        (
            "[orchestrator.skills]\nenabled = true\n[cloud.skills]\nenabled = false",
            false,
        ),
        (
            "[orchestrator.skills]\nenabled = false\n[cloud.skills]\nenabled = true",
            true,
        ),
    ] {
        let cfg: ConfigToml = toml::from_str(&format!(
            "model = \"gpt-5.4\"\n{settings}\n[orchestrator.mcp]\nenabled = false"
        ))
        .expect("cloud skill settings should deserialize");
        let config = Config::load_from_base_config_with_overrides(
            cfg,
            ConfigOverrides::default(),
            codex_home.abs(),
        )
        .await?;

        assert_eq!(
            (config.cloud_skill_enabled, config.orchestrator_mcp_enabled),
            (expected, false),
            "{settings}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn config_loads_orchestrator_settings_from_toml() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let cfg: ConfigToml = toml::from_str(
        r#"
model = "gpt-5.4"

[orchestrator.skills]
enabled = false

[orchestrator.mcp]
enabled = false
"#,
    )
    .expect("TOML deserialization should succeed for orchestrator settings");

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        (config.cloud_skill_enabled, config.orchestrator_mcp_enabled),
        (true, false)
    );
    Ok(())
}

#[tokio::test]
async fn config_loads_mcp_oauth_callback_url_from_toml() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let toml = r#"
model = "gpt-5.4"
mcp_oauth_callback_url = "https://example.com/callback"
"#;
    let cfg: ConfigToml =
        toml::from_str(toml).expect("TOML deserialization should succeed for callback URL");

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.mcp_oauth_callback_url.as_deref(),
        Some("https://example.com/callback")
    );
    Ok(())
}

#[tokio::test]
async fn prompt_instruction_blocks_can_be_disabled_from_config() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"include_apps_instructions = false
include_collaboration_mode_instructions = false
include_environment_context = false
include_environment_context_time = false

[skills]
include_instructions = false
"#,
    )?;

    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    assert!(!config.include_apps_instructions);
    assert!(!config.include_collaboration_mode_instructions);
    assert!(!config.include_skill_instructions);
    assert!(!config.include_environment_context);
    assert!(!config.include_environment_context_time);
    Ok(())
}

#[tokio::test]
async fn multi_agent_v2_config_from_feature_table() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
max_concurrent_threads_per_session = 5
min_wait_timeout_ms = 2500
max_wait_timeout_ms = 120000
default_wait_timeout_ms = 30000
usage_hint_text = "Custom delegation guidance."
root_agent_usage_hint_text = "Root guidance."
subagent_usage_hint_text = "Subagent guidance."
subagent_developer_instructions = "  Delegate carefully.  "
preserve_fork_prefix = true
multi_agent_mode_hint_text = "Custom mode guidance."
tool_namespace = "agents"
hide_spawn_agent_metadata = true
expose_spawn_agent_model_overrides = false
wait_agent_enabled = false
disable_direct_message = true
message_board_in_memory = true

[agents]
max_concurrent_threads_per_session = 9
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    assert!(config.features.enabled(Feature::MultiAgentV2));
    assert_eq!(config.multi_agent_v2.max_concurrent_threads_per_session, 5);
    assert_eq!(config.multi_agent_v2.min_wait_timeout_ms, 2500);
    assert_eq!(config.multi_agent_v2.max_wait_timeout_ms, 120000);
    assert_eq!(config.multi_agent_v2.default_wait_timeout_ms, 30000);
    assert_eq!(
        (
            config.agent_max_threads,
            config.effective_agent_max_threads(MultiAgentVersion::V2)
        ),
        (Some(9), Some(4))
    );
    assert_eq!(
        config.multi_agent_v2.usage_hint_text.as_deref(),
        Some("Custom delegation guidance.")
    );
    assert_eq!(
        config.multi_agent_v2.root_agent_usage_hint_text.as_deref(),
        Some("Root guidance.")
    );
    assert_eq!(
        config.multi_agent_v2.subagent_usage_hint_text.as_deref(),
        Some("Subagent guidance.")
    );
    assert_eq!(
        config
            .multi_agent_v2
            .subagent_developer_instructions
            .as_deref(),
        Some("Delegate carefully.")
    );
    assert_eq!(
        config.multi_agent_v2.multi_agent_mode_hint_text.as_deref(),
        Some("Custom mode guidance.")
    );
    assert_eq!(
        config.multi_agent_v2.tool_namespace.as_deref(),
        Some("agents")
    );
    assert!(config.multi_agent_v2.hide_spawn_agent_metadata);
    assert!(config.multi_agent_v2.preserve_fork_prefix);
    assert!(!config.multi_agent_v2.expose_spawn_agent_model_overrides);
    assert!(!config.multi_agent_v2.wait_agent_enabled);
    assert!(config.multi_agent_v2.disable_direct_message);
    assert!(config.multi_agent_v2.message_board_in_memory);

    Ok(())
}

#[tokio::test]
async fn multi_agent_v2_default_session_thread_cap_counts_root() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    assert_eq!(
        config.multi_agent_v2,
        resolve_multi_agent_v2_config(&ConfigToml::default())
    );
    assert_eq!(
        (
            config.agent_max_threads,
            config.effective_agent_max_threads(MultiAgentVersion::V2)
        ),
        (None, Some(3))
    );

    Ok(())
}

#[test]
fn multi_agent_v2_default_usage_hints_use_configured_thread_cap() {
    let config_toml = toml::from_str(
        r#"[features.multi_agent_v2]
enabled = true
max_concurrent_threads_per_session = 17
"#,
    )
    .expect("multi-agent v2 config should parse");

    let config = resolve_multi_agent_v2_config(&config_toml);
    let concurrency_guidance = "There are 17 available concurrency slots, meaning that up to 17 agents can be active at once, including you.";
    let messages = ResolvedModelMessages::bundled().multi_agent();
    assert!(config.wait_agent_enabled);
    for wait_agent_enabled in [true, false] {
        let mut config = config.clone();
        config.wait_agent_enabled = wait_agent_enabled;
        let usage_hints = resolve_usage_hints(
            &config, messages, /*omit_update_plan_instructions*/ false,
        );
        for hint in [usage_hints.root, usage_hints.subagent] {
            let hint = hint.expect("default usage hints should be present").body();
            assert!(hint.contains(concurrency_guidance));
            assert_eq!(
                hint.contains("When calling `wait_agent`, prefer longer waits"),
                wait_agent_enabled
            );
        }
    }

    let mut empty_messages = messages;
    empty_messages.root = ResolvedMessage::Catalog("");
    empty_messages.subagent = ResolvedMessage::Catalog("");
    let usage_hints = resolve_usage_hints(
        &config,
        empty_messages,
        /*omit_update_plan_instructions*/ false,
    );
    assert!(usage_hints.root.is_none() && usage_hints.subagent.is_none());
}

#[test]
fn multi_agent_v2_model_override_exposure_preserves_configured_usage_hints() {
    let config_toml = toml::from_str(
        r###"[features.multi_agent_v2]
enabled = true
root_agent_usage_hint_text = "## Plan tool\nRoot guidance."
subagent_usage_hint_text = "## `update_plan`\nSubagent guidance."
expose_spawn_agent_model_overrides = true
"###,
    )
    .expect("multi-agent v2 config should parse");

    let config = resolve_multi_agent_v2_config(&config_toml);
    assert!(config.expose_spawn_agent_model_overrides);
    assert_eq!(
        config.root_agent_usage_hint_text.as_deref(),
        Some("## Plan tool\nRoot guidance.")
    );
    assert_eq!(
        config.subagent_usage_hint_text.as_deref(),
        Some("## `update_plan`\nSubagent guidance.")
    );
    let mut messages = ResolvedModelMessages::bundled().multi_agent();
    messages.root = ResolvedMessage::Catalog("Catalog root base.");
    messages.subagent = ResolvedMessage::Catalog("Catalog subagent base.");
    let usage_hints = resolve_usage_hints(
        &config, messages, /*omit_update_plan_instructions*/ true,
    );
    assert_eq!(
        (
            usage_hints.root.map(|hint| hint.body()),
            usage_hints.subagent.map(|hint| hint.body()),
        ),
        (
            Some("## Plan tool\nRoot guidance.".to_string()),
            Some("## `update_plan`\nSubagent guidance.".to_string()),
        )
    );
}

#[test]
fn multi_agent_v2_exposes_model_overrides_by_default() {
    let config_toml =
        toml::from_str(r#"[features.multi_agent_v2]"#).expect("multi-agent v2 config should parse");

    let mut config = resolve_multi_agent_v2_config(&config_toml);
    assert!(config.expose_spawn_agent_model_overrides);
    let messages = ResolvedModelMessages::bundled().multi_agent();
    let usage_hints = resolve_usage_hints(
        &config, messages, /*omit_update_plan_instructions*/ false,
    );
    config.expose_spawn_agent_model_overrides = false;
    let usage_hints_without_model_overrides = resolve_usage_hints(
        &config, messages, /*omit_update_plan_instructions*/ false,
    );

    for (hint, hint_without_model_overrides) in [
        (usage_hints.root, usage_hints_without_model_overrides.root),
        (
            usage_hints.subagent,
            usage_hints_without_model_overrides.subagent,
        ),
    ] {
        let hint = hint.expect("default usage hints should be present").body();
        let hint_without_model_overrides = hint_without_model_overrides
            .expect("default usage hints should be present without model overrides")
            .body();

        let model_override_guidance = hint
            .strip_prefix(hint_without_model_overrides.as_str())
            .expect("model-override guidance should extend the base usage hint");
        for required_fragment in [
            "Full-history forks",
            "`fork_turns`",
            "`model`",
            "`reasoning_effort`",
        ] {
            assert!(
                model_override_guidance.contains(required_fragment),
                "model-override guidance should contain {required_fragment}"
            );
        }
    }
}

#[tokio::test]
async fn multi_agent_v2_allows_disabled_wait_agent_without_sleep_tool() -> std::io::Result<()> {
    for config_toml in [
        r#"
[features.multi_agent_v2]
enabled = true
wait_agent_enabled = false
"#,
        r#"
[features.multi_agent_v2]
enabled = true
wait_agent_enabled = false

[features.current_time_reminder]
enabled = true
sleep_tool = false
"#,
        r#"
[features.multi_agent_v2]
enabled = true
wait_agent_enabled = false

[features.current_time_reminder]
enabled = false
sleep_tool = true
"#,
    ] {
        let codex_home = tempdir()?;
        let config_toml = toml::from_str(config_toml).expect("TOML should deserialize");
        let config = Config::load_from_base_config_with_overrides(
            config_toml,
            ConfigOverrides::default(),
            codex_home.abs(),
        )
        .await?;

        assert!(!config.multi_agent_v2.wait_agent_enabled);
    }

    Ok(())
}

#[test]
fn multi_agent_v2_preserves_empty_mode_hint_override() {
    let config_toml = toml::from_str(
        r#"[features.multi_agent_v2]
multi_agent_mode_hint_text = ""
subagent_developer_instructions = "  \t  "
"#,
    )
    .expect("multi-agent v2 config should parse");

    let expected = MultiAgentV2Config {
        subagent_developer_instructions: Some(String::new()),
        multi_agent_mode_hint_text: Some(String::new()),
        ..resolve_multi_agent_v2_config(&ConfigToml::default())
    };
    assert_eq!(resolve_multi_agent_v2_config(&config_toml), expected);
}

#[tokio::test]
async fn multi_agent_v2_empty_usage_hint_overrides_are_preserved() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
root_agent_usage_hint_text = ""
subagent_usage_hint_text = ""
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    let mut messages = ResolvedModelMessages::bundled().multi_agent();
    messages.root = ResolvedMessage::Catalog("catalog root");
    messages.subagent = ResolvedMessage::Catalog("catalog subagent");
    let usage_hints = resolve_usage_hints(
        &config.multi_agent_v2,
        messages,
        /*omit_update_plan_instructions*/ false,
    );
    assert_eq!(
        (
            config.multi_agent_v2.root_agent_usage_hint_text.as_deref(),
            config.multi_agent_v2.subagent_usage_hint_text.as_deref(),
            usage_hints.root,
            usage_hints.subagent,
        ),
        (Some(""), Some(""), None, None)
    );

    Ok(())
}

#[tokio::test]
async fn multi_agent_v2_uses_agents_max_concurrent_threads_per_session() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true

[agents]
max_concurrent_threads_per_session = 7
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;
    assert_eq!(
        (
            config.multi_agent_v2.max_concurrent_threads_per_session,
            config.effective_agent_max_threads(MultiAgentVersion::V2),
        ),
        (8, Some(7))
    );

    Ok(())
}

#[tokio::test]
async fn catalog_v2_allows_agents_thread_limit_when_feature_disabled() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = false

[agents]
max_concurrent_threads_per_session = 3
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    assert_eq!(
        (
            config.multi_agent_v2.max_concurrent_threads_per_session,
            config.effective_agent_max_threads(MultiAgentVersion::V2),
        ),
        (4, Some(3))
    );

    Ok(())
}

#[tokio::test]
async fn multi_agent_v2_rejects_invalid_wait_timeouts() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
min_wait_timeout_ms = 0
max_wait_timeout_ms = 0
default_wait_timeout_ms = 0
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    assert_eq!(config.multi_agent_v2.min_wait_timeout_ms, 0);
    assert_eq!(config.multi_agent_v2.max_wait_timeout_ms, 0);
    assert_eq!(config.multi_agent_v2.default_wait_timeout_ms, 0);

    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
min_wait_timeout_ms = -1
"#,
    )?;

    let err = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await
        .expect_err("negative min_wait_timeout_ms should be rejected");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "features.multi_agent_v2.min_wait_timeout_ms must be at least 0"
    );

    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
min_wait_timeout_ms = 3600001
"#,
    )?;

    let err = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await
        .expect_err("too large min_wait_timeout_ms should be rejected");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "features.multi_agent_v2.min_wait_timeout_ms must be at most 3600000"
    );

    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
max_wait_timeout_ms = -1
"#,
    )?;

    let err = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await
        .expect_err("negative max_wait_timeout_ms should be rejected");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "features.multi_agent_v2.max_wait_timeout_ms must be at least 0"
    );

    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
max_wait_timeout_ms = 3600001
"#,
    )?;

    let err = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await
        .expect_err("too large max_wait_timeout_ms should be rejected");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "features.multi_agent_v2.max_wait_timeout_ms must be at most 3600000"
    );

    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
default_wait_timeout_ms = -1
"#,
    )?;

    let err = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await
        .expect_err("negative default_wait_timeout_ms should be rejected");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "features.multi_agent_v2.default_wait_timeout_ms must be at least 0"
    );

    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
min_wait_timeout_ms = 1000
max_wait_timeout_ms = 500
"#,
    )?;

    let err = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await
        .expect_err("min greater than max should be rejected");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "features.multi_agent_v2.min_wait_timeout_ms must be at most features.multi_agent_v2.max_wait_timeout_ms"
    );

    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
min_wait_timeout_ms = 1000
max_wait_timeout_ms = 2000
default_wait_timeout_ms = 500
"#,
    )?;

    let err = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await
        .expect_err("default less than min should be rejected");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "features.multi_agent_v2.default_wait_timeout_ms must be at least features.multi_agent_v2.min_wait_timeout_ms"
    );

    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
min_wait_timeout_ms = 1000
max_wait_timeout_ms = 2000
default_wait_timeout_ms = 2500
"#,
    )?;

    let err = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await
        .expect_err("default greater than max should be rejected");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        "features.multi_agent_v2.default_wait_timeout_ms must be at most features.multi_agent_v2.max_wait_timeout_ms"
    );

    Ok(())
}

#[tokio::test]
async fn multi_agent_v2_rejects_invalid_tool_namespace() -> std::io::Result<()> {
    for (namespace, expected_message) in [
        (
            "bad namespace",
            "features.multi_agent_v2.tool_namespace must match ^[a-zA-Z0-9_-]+$",
        ),
        (
            "functions",
            "features.multi_agent_v2.tool_namespace uses a reserved namespace: functions",
        ),
    ] {
        let codex_home = TempDir::new()?;
        std::fs::write(
            codex_home.path().join(CONFIG_TOML_FILE),
            format!(
                r#"[features.multi_agent_v2]
enabled = true
tool_namespace = "{namespace}"
"#
            ),
        )?;

        let err = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(codex_home.path().to_path_buf())
            .fallback_cwd(Some(codex_home.path().to_path_buf()))
            .build()
            .await
            .expect_err("invalid multi_agent_v2 tool namespace should fail");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), expected_message);
    }

    Ok(())
}

#[tokio::test]
async fn multi_agent_v2_session_thread_cap_one_disallows_subagents() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"[features.multi_agent_v2]
enabled = true
max_concurrent_threads_per_session = 1
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    assert_eq!(config.multi_agent_v2.max_concurrent_threads_per_session, 1);
    assert_eq!(
        (
            config.agent_max_threads,
            config.effective_agent_max_threads(MultiAgentVersion::V2)
        ),
        (None, Some(0))
    );

    Ok(())
}

#[tokio::test]
async fn tool_suggest_discoverables_load_from_config_toml() -> std::io::Result<()> {
    let cfg: ConfigToml = toml::from_str(
        r#"
[tool_suggest]
discoverables = [
  { type = "connector", id = "connector_alpha" },
  { type = "plugin", id = "plugin_alpha@openai-curated" },
  { type = "connector", id = "   " }
]
"#,
    )
    .expect("TOML deserialization should succeed");

    assert_eq!(
        cfg.tool_suggest,
        Some(ToolSuggestConfig {
            discoverables: vec![
                ToolSuggestDiscoverable {
                    kind: ToolSuggestDiscoverableType::Connector,
                    id: "connector_alpha".to_string(),
                },
                ToolSuggestDiscoverable {
                    kind: ToolSuggestDiscoverableType::Plugin,
                    id: "plugin_alpha@openai-curated".to_string(),
                },
                ToolSuggestDiscoverable {
                    kind: ToolSuggestDiscoverableType::Connector,
                    id: "   ".to_string(),
                },
            ],
            disabled_tools: Vec::new(),
        })
    );

    let codex_home = TempDir::new()?;
    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.tool_suggest,
        ToolSuggestConfig {
            discoverables: vec![
                ToolSuggestDiscoverable {
                    kind: ToolSuggestDiscoverableType::Connector,
                    id: "connector_alpha".to_string(),
                },
                ToolSuggestDiscoverable {
                    kind: ToolSuggestDiscoverableType::Plugin,
                    id: "plugin_alpha@openai-curated".to_string(),
                },
            ],
            disabled_tools: Vec::new(),
        }
    );
    Ok(())
}

#[tokio::test]
async fn tool_suggest_disabled_tools_load_from_config_toml() -> std::io::Result<()> {
    let cfg: ConfigToml = toml::from_str(
        r#"
[tool_suggest]
disabled_tools = [
  { type = "connector", id = " connector_calendar " },
  { type = "connector", id = "connector_calendar" },
  { type = "connector", id = "   " },
  { type = "plugin", id = "slack@openai-curated" }
]
"#,
    )
    .expect("TOML deserialization should succeed");

    assert_eq!(
        cfg.tool_suggest,
        Some(ToolSuggestConfig {
            discoverables: Vec::new(),
            disabled_tools: vec![
                ToolSuggestDisabledTool::connector(" connector_calendar "),
                ToolSuggestDisabledTool::connector("connector_calendar"),
                ToolSuggestDisabledTool::connector("   "),
                ToolSuggestDisabledTool::plugin("slack@openai-curated"),
            ],
        })
    );

    let codex_home = TempDir::new()?;
    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert_eq!(
        config.tool_suggest,
        ToolSuggestConfig {
            discoverables: Vec::new(),
            disabled_tools: vec![
                ToolSuggestDisabledTool::connector("connector_calendar"),
                ToolSuggestDisabledTool::plugin("slack@openai-curated"),
            ],
        }
    );
    Ok(())
}

#[tokio::test]
async fn tool_suggest_disabled_tools_merge_across_config_layers() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        format!(
            r#"

[tool_suggest]
disabled_tools = [
  {{ type = "connector", id = " user_connector " }},
  {{ type = "plugin", id = "shared_plugin" }},
  {{ type = "connector", id = "project_connector" }},
]
"#
        ),
    )?;

    let project_config_dir = workspace.path().join(".codex");
    std::fs::create_dir_all(&project_config_dir)?;
    std::fs::write(
        project_config_dir.join(CONFIG_TOML_FILE),
        r#"
[tool_suggest]
disabled_tools = [
  { type = "connector", id = "project_connector" },
  { type = "plugin", id = "project_plugin" },
  { type = "plugin", id = "shared_plugin" },
]
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(workspace.path().to_path_buf()),
            ..Default::default()
        })
        .build()
        .await?;

    assert_eq!(
        config.tool_suggest.disabled_tools,
        vec![
            ToolSuggestDisabledTool::connector("user_connector"),
            ToolSuggestDisabledTool::plugin("shared_plugin"),
            ToolSuggestDisabledTool::connector("project_connector"),
            ToolSuggestDisabledTool::plugin("project_plugin"),
        ]
    );
    Ok(())
}

#[test]
fn accepts_amazon_bedrock_aws_profile_override() {
    let cfg = toml::from_str::<ConfigToml>(
        r#"
[model_providers.amazon-bedrock.aws]
profile = "codex-bedrock"
region = "us-west-2"
"#,
    )
    .expect("Amazon Bedrock AWS overrides should deserialize");

    assert_eq!(
        cfg.model_providers
            .get("amazon-bedrock")
            .and_then(|provider| provider.aws.as_ref())
            .and_then(|aws| aws.profile.as_deref()),
        Some("codex-bedrock")
    );
    assert_eq!(
        cfg.model_providers
            .get("amazon-bedrock")
            .and_then(|provider| provider.aws.as_ref())
            .and_then(|aws| aws.region.as_deref()),
        Some("us-west-2")
    );
}

#[tokio::test]
async fn load_config_applies_amazon_bedrock_aws_profile_override() {
    let cfg = toml::from_str::<ConfigToml>(
        r#"
model_provider = "amazon-bedrock"

[model_providers.amazon-bedrock.aws]
profile = "codex-bedrock"
region = "us-west-2"
"#,
    )
    .expect("Amazon Bedrock AWS overrides should deserialize");

    let config = Config::load_from_base_config_with_overrides(
        cfg,
        ConfigOverrides::default(),
        tempdir().expect("tempdir").abs(),
    )
    .await
    .expect("load config");

    assert_eq!(config.model_provider_id, "amazon-bedrock");
    assert_eq!(
        config
            .model_provider
            .aws
            .as_ref()
            .and_then(|aws| aws.profile.as_deref()),
        Some("codex-bedrock")
    );
    assert_eq!(
        config
            .model_provider
            .aws
            .as_ref()
            .and_then(|aws| aws.region.as_deref()),
        Some("us-west-2")
    );
}

#[test]
fn profile_v2_config_path_resolves_validated_names() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;
    let profile_name: ProfileV2Name = "work".parse()?;
    assert_eq!(
        resolve_profile_v2_config_path(codex_home.path(), &profile_name),
        codex_home.path().join("work.config.toml").abs()
    );
    Ok(())
}

#[tokio::test]
async fn legacy_profile_selection_is_rejected() -> std::io::Result<()> {
    let mut fixture = create_test_fixture()?;
    fixture.cfg.profile = Some("gpt3".to_string());

    let err = Config::load_from_base_config_with_overrides(
        fixture.cfg.clone(),
        ConfigOverrides {
            cwd: Some(fixture.cwd_path()),
            ..Default::default()
        },
        fixture.codex_home(),
    )
    .await
    .expect_err("legacy profile selection should be rejected");

    assert_eq!(err.kind(), ErrorKind::InvalidData);
    assert!(
        err.to_string()
            .contains("legacy `profile = \"gpt3\"` config is no longer supported"),
        "unexpected error: {err}"
    );
    Ok(())
}

#[tokio::test]
async fn respect_system_proxy_cli_override_enables_feature() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"
[features]
respect_system_proxy = false
"#,
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .cli_overrides(vec![(
            "features.respect_system_proxy".to_string(),
            toml::Value::Boolean(true),
        )])
        .build()
        .await?;

    assert!(config.respect_system_proxy);
    Ok(())
}

#[tokio::test]
async fn respect_system_proxy_feature_resolves_enabled() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let config = Config::load_from_base_config_with_overrides(
        ConfigToml {
            features: Some(
                toml::from_str(
                    r#"
respect_system_proxy = true
"#,
                )
                .expect("valid features"),
            ),
            ..Default::default()
        },
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;

    assert!(config.respect_system_proxy);
    assert_eq!(
        config.http_client_factory().outbound_proxy_policy(),
        codex_http_client::OutboundProxyPolicy::RespectSystemProxy
    );
    assert_eq!(
        config
            .auth_route_config()
            .http_client_factory()
            .outbound_proxy_policy(),
        codex_http_client::OutboundProxyPolicy::RespectSystemProxy
    );
    assert_eq!(
        config.plugins_config_input().remote_plugin_service_config(),
        codex_core_plugins::remote::RemotePluginServiceConfig::new(
            config.chatgpt_base_url,
            codex_http_client::HttpClientFactory::new(
                codex_http_client::OutboundProxyPolicy::RespectSystemProxy,
            ),
            /*product_sku*/ None,
        )
    );
    Ok(())
}

#[tokio::test]
async fn system_proxy_fallback_config_matches_bootstrap() -> std::io::Result<()> {
    for (features, expected_fallback, expected_policy) in [
        (
            "",
            true,
            codex_http_client::OutboundProxyPolicy::ReqwestDefault,
        ),
        (
            "respect_system_proxy = false",
            true,
            codex_http_client::OutboundProxyPolicy::ReqwestDefault,
        ),
        (
            "system_proxy_fallback = false",
            false,
            codex_http_client::OutboundProxyPolicy::ReqwestDefault,
        ),
        (
            "respect_system_proxy = true",
            false,
            codex_http_client::OutboundProxyPolicy::RespectSystemProxy,
        ),
        (
            "respect_system_proxy = true\nsystem_proxy_fallback = false",
            false,
            codex_http_client::OutboundProxyPolicy::RespectSystemProxy,
        ),
    ] {
        let codex_home = TempDir::new()?;
        std::fs::write(
            codex_home.path().join(CONFIG_TOML_FILE),
            format!("[features]\n{features}\n"),
        )?;
        let config = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await?;
        let bootstrap = ConfigToml {
            features: Some(toml::from_str(features).expect("valid feature configuration")),
            ..Default::default()
        };
        let factory = resolve_bootstrap_http_client_factory(&bootstrap);
        assert_eq!(
            factory.allows_system_proxy_fallback(),
            expected_fallback,
            "{features}"
        );
        assert_eq!(
            factory.outbound_proxy_policy(),
            expected_policy,
            "{features}"
        );
        assert_eq!(config.http_client_factory(), factory, "{features}");
        assert_eq!(
            config.auth_route_config().http_client_factory(),
            &factory,
            "{features}"
        );
    }
    Ok(())
}
