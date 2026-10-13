use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use codex_config::McpServerConfig;
use codex_config::McpServerOAuthConfig;
use codex_config::McpServerTransportConfig;
use codex_config::load_global_mcp_servers;
use codex_login::default_client::is_first_party_originator;
use codex_login::default_client::originator;
use codex_rmcp_client::McpOAuthClientRegistration;
use codex_rmcp_client::OAuthDiscoveryTimeout;
use codex_rmcp_client::StreamableHttpRedirectMode;
use codex_rmcp_client::perform_oauth_login;
use tracing::warn;

use crate::config::edit::ConfigEditsBuilder;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_mcp::McpOAuthLoginSupport;
use codex_mcp::oauth_login_support;
use codex_mcp::resolve_oauth_scopes;
use codex_mcp::should_retry_without_scopes;
use codex_skills::SkillMetadata;
use codex_skills::SkillToolDependency;

/// Installs MCP servers required by mentioned skills. Codex never asks for
/// permission, so missing dependencies are installed without a prompt.
pub(crate) async fn install_missing_mcp_dependencies(
    sess: &Session,
    turn_context: &TurnContext,
    mentioned_skills: &[SkillMetadata],
) {
    let originator_value = originator().value;
    if !is_first_party_originator(originator_value.as_str()) {
        // Only support first-party clients for now.
        return;
    }

    let config = turn_context.config.clone();
    if mentioned_skills.is_empty()
        || !config
            .features
            .enabled(codex_features::Feature::SkillMcpDependencyInstall)
    {
        return;
    }

    let installed = sess.runtime_mcp_servers(config.as_ref()).await;
    let missing = collect_missing_mcp_dependencies(mentioned_skills, &installed);
    if missing.is_empty() {
        return;
    }

    // Skip servers that attachment policy would leave disabled.
    let missing = admit_mcp_dependencies(sess, config.as_ref(), missing).await;
    install_mcp_dependencies(sess, turn_context, missing).await;
}

// Resolve proposed servers through the existing policy-aware catalog before installing them.
async fn admit_mcp_dependencies(
    sess: &Session,
    config: &crate::config::Config,
    mut candidates: HashMap<String, McpServerConfig>,
) -> HashMap<String, McpServerConfig> {
    if candidates.is_empty() {
        return candidates;
    }

    // Stage candidates in memory before catalog resolution.
    let mut candidate_config = config.clone();
    candidate_config.mcp_servers.extend(candidates.clone());

    // Keep only the same candidate when attachment policy leaves it enabled.
    let catalog = sess.runtime_mcp_config(&candidate_config).await;
    candidates.retain(|name, config| {
        catalog
            .mcp_server_catalog
            .server(name)
            .is_some_and(|server| server.config().enabled && server.config() == config)
    });
    candidates
}

async fn install_mcp_dependencies(
    sess: &Session,
    turn_context: &TurnContext,
    missing: HashMap<String, McpServerConfig>,
) {
    if missing.is_empty() {
        return;
    }

    let config = turn_context.config.as_ref();
    let codex_home = config.codex_home.clone();
    let mut servers = match load_global_mcp_servers(&codex_home).await {
        Ok(servers) => servers,
        Err(err) => {
            warn!("failed to load MCP servers while installing skill dependencies: {err}");
            return;
        }
    };

    let mut added = Vec::new();
    for (name, config) in missing {
        if servers.contains_key(&name) {
            continue;
        }
        servers.insert(name.clone(), config.clone());
        added.push((name, config));
    }

    if added.is_empty() {
        return;
    }

    if let Err(err) = ConfigEditsBuilder::new(&codex_home)
        .replace_mcp_servers(&servers)
        .apply()
        .await
    {
        warn!("failed to persist MCP dependencies for mentioned skills: {err}");
        return;
    }

    let (_, runtime_context) = sess.runtime_mcp_config_and_context(config).await;
    for (name, server_config) in &added {
        let http_client = match runtime_context.resolve_http_client(name, server_config) {
            Ok(http_client) => http_client,
            Err(err) => {
                warn!("failed to resolve MCP dependency runtime for {name}: {err}");
                continue;
            }
        };
        let discovery_timeout = if server_config.is_local_environment() {
            OAuthDiscoveryTimeout::LOCAL
        } else {
            OAuthDiscoveryTimeout::Requested
        };
        let login_support = oauth_login_support(
            &server_config.transport,
            Arc::clone(&http_client),
            discovery_timeout,
            StreamableHttpRedirectMode::Legacy,
        )
        .await;
        let oauth_config = match login_support {
            McpOAuthLoginSupport::Supported(config) => config,
            McpOAuthLoginSupport::Unsupported => continue,
            McpOAuthLoginSupport::Unknown(err) => {
                warn!("MCP server may or may not require login for dependency {name}: {err}");
                continue;
            }
        };

        let resolved_scopes = resolve_oauth_scopes(
            /*explicit_scopes*/ None,
            server_config.scopes.clone(),
            oauth_config.discovered_scopes.clone(),
        );
        let oauth_client_config = server_config.oauth.as_ref();
        let oauth_credential_name = server_config.oauth_credential_name(name);
        let callback_port = server_config.oauth_callback_port(config.mcp_oauth_callback_port);
        let first_attempt = perform_oauth_login(
            oauth_credential_name.as_ref(),
            &oauth_config.url,
            config.mcp_oauth_credentials_store_mode,
            config.auth_keyring_backend_kind(),
            oauth_config.http_headers.clone(),
            oauth_config.env_http_headers.clone(),
            &resolved_scopes.scopes,
            oauth_client_config,
            McpOAuthClientRegistration::Auto,
            server_config.oauth_resource.as_deref(),
            callback_port,
            config.mcp_oauth_callback_url.as_deref(),
            config.mcp_oauth_callback_url.as_deref(),
            Arc::clone(&http_client),
        )
        .await;

        if let Err(err) = first_attempt {
            if should_retry_without_scopes(&resolved_scopes, &err) {
                if let Err(err) = perform_oauth_login(
                    oauth_credential_name.as_ref(),
                    &oauth_config.url,
                    config.mcp_oauth_credentials_store_mode,
                    config.auth_keyring_backend_kind(),
                    oauth_config.http_headers,
                    oauth_config.env_http_headers,
                    &[],
                    oauth_client_config,
                    McpOAuthClientRegistration::Auto,
                    server_config.oauth_resource.as_deref(),
                    callback_port,
                    config.mcp_oauth_callback_url.as_deref(),
                    config.mcp_oauth_callback_url.as_deref(),
                    Arc::clone(&http_client),
                )
                .await
                {
                    warn!("failed to login to MCP dependency {name}: {err}");
                }
            } else {
                warn!("failed to login to MCP dependency {name}: {err}");
            }
        }
    }

    sess.refresh_mcp_servers_now(added.into_iter().collect())
        .await;
}

fn canonical_mcp_key(transport: &str, identifier: &str, fallback: &str) -> String {
    let identifier = identifier.trim();
    if identifier.is_empty() {
        fallback.to_string()
    } else {
        format!("mcp__{transport}__{identifier}")
    }
}

fn canonical_mcp_server_key(name: &str, config: &McpServerConfig) -> String {
    match &config.transport {
        McpServerTransportConfig::Stdio { command, .. } => {
            canonical_mcp_key("stdio", command, name)
        }
        McpServerTransportConfig::StreamableHttp { url, .. } => {
            canonical_mcp_key("streamable_http", url, name)
        }
    }
}

fn canonical_mcp_dependency_key(dependency: &SkillToolDependency) -> Result<String, String> {
    let transport = dependency.transport.as_deref().unwrap_or("streamable_http");
    if transport.eq_ignore_ascii_case("streamable_http") {
        let url = dependency
            .url
            .as_ref()
            .ok_or_else(|| "missing url for streamable_http dependency".to_string())?;
        return Ok(canonical_mcp_key("streamable_http", url, &dependency.value));
    }
    if transport.eq_ignore_ascii_case("stdio") {
        let command = dependency
            .command
            .as_ref()
            .ok_or_else(|| "missing command for stdio dependency".to_string())?;
        return Ok(canonical_mcp_key("stdio", command, &dependency.value));
    }
    Err(format!("unsupported transport {transport}"))
}

fn mcp_dependency_to_server_config(
    dependency: &SkillToolDependency,
) -> Result<McpServerConfig, String> {
    let transport = dependency.transport.as_deref().unwrap_or("streamable_http");
    if transport.eq_ignore_ascii_case("streamable_http") {
        let url = dependency
            .url
            .as_ref()
            .ok_or_else(|| "missing url for streamable_http dependency".to_string())?;
        return Ok(McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::StreamableHttp {
                url: url.clone(),
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
            oauth: dependency
                .oauth_callback_port
                .map(|callback_port| McpServerOAuthConfig {
                    client_id: None,
                    callback_url: None,
                    callback_port: Some(callback_port),
                    ..Default::default()
                }),
            oauth_resource: None,
            tools: HashMap::new(),
        });
    }

    if transport.eq_ignore_ascii_case("stdio") {
        let command = dependency
            .command
            .as_ref()
            .ok_or_else(|| "missing command for stdio dependency".to_string())?;
        return Ok(McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::Stdio {
                command: command.clone(),
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
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        });
    }

    Err(format!("unsupported transport {transport}"))
}

fn collect_missing_mcp_dependencies(
    mentioned_skills: &[SkillMetadata],
    installed: &HashMap<String, McpServerConfig>,
) -> HashMap<String, McpServerConfig> {
    let mut missing = HashMap::new();
    let installed_keys: HashSet<String> = installed
        .iter()
        .map(|(name, config)| canonical_mcp_server_key(name, config))
        .collect();
    let mut seen_canonical_keys = HashSet::new();

    for skill in mentioned_skills {
        let Some(dependencies) = skill.dependencies.as_ref() else {
            continue;
        };

        for tool in &dependencies.tools {
            if !tool.r#type.eq_ignore_ascii_case("mcp") {
                continue;
            }
            let dependency_key = match canonical_mcp_dependency_key(tool) {
                Ok(key) => key,
                Err(err) => {
                    let dependency = tool.value.as_str();
                    let skill_name = skill.name.as_str();
                    warn!(
                        "unable to auto-install MCP dependency {dependency} for skill {skill_name}: {err}",
                    );
                    continue;
                }
            };
            if installed_keys.contains(&dependency_key)
                || seen_canonical_keys.contains(&dependency_key)
            {
                continue;
            }

            let config = match mcp_dependency_to_server_config(tool) {
                Ok(config) => config,
                Err(err) => {
                    let dependency = dependency_key.as_str();
                    let skill_name = skill.name.as_str();
                    warn!(
                        "unable to auto-install MCP dependency {dependency} for skill {skill_name}: {err}",
                    );
                    continue;
                }
            };

            missing.insert(tool.value.clone(), config);
            seen_canonical_keys.insert(dependency_key);
        }
    }

    missing
}
