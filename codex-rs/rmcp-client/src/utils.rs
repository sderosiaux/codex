use anyhow::Result;
use codex_config::types::McpServerEnvVar;
use codex_protocol::shell_environment::is_non_inheritable_env_var;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use http::header::USER_AGENT;
use std::collections::HashMap;
use std::env;
use std::ffi::OsString;

/// CA bundle variables forwarded to stdio MCP servers as absolute paths.
///
/// Best-effort compatibility set for common child toolchains that accept a CA bundle path.
pub const CUSTOM_CA_ENV_KEYS: [&str; 11] = [
    "CODEX_CA_CERTIFICATE",
    "SSL_CERT_FILE",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
    "GIT_SSL_CAINFO",
    "CARGO_HTTP_CAINFO",
    "PIP_CERT",
    "BUNDLE_SSL_CA_CERT",
    "npm_config_cafile",
    "NPM_CONFIG_CAFILE",
];

pub(crate) const MCP_USER_AGENT: &str = concat!("codex-mcp-client/", env!("CARGO_PKG_VERSION"));

pub(crate) fn create_env_for_mcp_server(
    extra_env: Option<HashMap<OsString, OsString>>,
    env_vars: &[McpServerEnvVar],
) -> Result<HashMap<OsString, OsString>> {
    let additional_env_vars = local_stdio_env_var_names(env_vars)?;
    let mut env: HashMap<OsString, OsString> = DEFAULT_ENV_VARS
        .iter()
        .copied()
        .chain(additional_env_vars)
        .filter_map(|var| env::var_os(var).map(|value| (OsString::from(var), value)))
        .collect();
    for name in CUSTOM_CA_ENV_KEYS {
        let Some(value) = env::var_os(name) else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        let value = std::path::absolute(value)?.into_os_string();
        #[cfg(windows)]
        env.retain(|key, _| !key.to_string_lossy().eq_ignore_ascii_case(name));
        env.insert(OsString::from(name), value);
    }
    for (name, value) in extra_env.unwrap_or_default() {
        if cfg!(windows)
            || name.to_str().is_some_and(|name| {
                CUSTOM_CA_ENV_KEYS
                    .iter()
                    .any(|ca_name| ca_name.eq_ignore_ascii_case(name))
            })
        {
            env.retain(|key, _| {
                !key.to_string_lossy()
                    .eq_ignore_ascii_case(&name.to_string_lossy())
            });
        }
        env.insert(name, value);
    }
    env.retain(|name, _| {
        name.to_str()
            .is_none_or(|name| !is_non_inheritable_env_var(name))
    });
    Ok(env)
}

fn local_stdio_env_var_names(env_vars: &[McpServerEnvVar]) -> Result<impl Iterator<Item = &str>> {
    Ok(env_vars
        .iter()
        .map(McpServerEnvVar::name)
        .filter(|name| !is_non_inheritable_env_var(name)))
}

pub(crate) fn build_default_headers(
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(MCP_USER_AGENT));

    if let Some(static_headers) = http_headers {
        for (name, value) in static_headers {
            let header_name = match HeaderName::from_bytes(name.as_bytes()) {
                Ok(name) => name,
                Err(err) => {
                    tracing::warn!("invalid HTTP header name `{name}`: {err}");
                    continue;
                }
            };
            let header_value = match HeaderValue::from_str(value.as_str()) {
                Ok(value) => value,
                Err(err) => {
                    tracing::warn!("invalid HTTP header value for `{name}`: {err}");
                    continue;
                }
            };
            headers.insert(header_name, header_value);
        }
    }

    if let Some(env_headers) = env_http_headers {
        for (name, env_var) in env_headers {
            if let Ok(value) = env::var(&env_var) {
                if value.trim().is_empty() {
                    continue;
                }

                let header_name = match HeaderName::from_bytes(name.as_bytes()) {
                    Ok(name) => name,
                    Err(err) => {
                        tracing::warn!("invalid HTTP header name `{name}`: {err}");
                        continue;
                    }
                };

                let header_value = match HeaderValue::from_str(value.as_str()) {
                    Ok(value) => value,
                    Err(err) => {
                        tracing::warn!(
                            "invalid HTTP header value read from {env_var} for `{name}`: {err}"
                        );
                        continue;
                    }
                };
                headers.insert(header_name, header_value);
            }
        }
    }

    Ok(headers)
}

#[cfg(unix)]
pub(crate) const DEFAULT_ENV_VARS: &[&str] = &[
    "HOME",
    "LOGNAME",
    "PATH",
    "SHELL",
    "USER",
    "__CF_USER_TEXT_ENCODING",
    "LANG",
    "LC_ALL",
    "TERM",
    "TMPDIR",
    "TZ",
];

#[cfg(windows)]
pub(crate) const DEFAULT_ENV_VARS: &[&str] =
    codex_protocol::shell_environment::WINDOWS_CORE_ENV_VARS;
