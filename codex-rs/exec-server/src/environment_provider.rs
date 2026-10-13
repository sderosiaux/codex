use std::future::Future;
use std::pin::Pin;

use codex_config::ScopedSkillsConfig;

use crate::ExecServerError;
use crate::client_api::DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT;
use crate::client_api::ExecServerTransportParams;
use crate::environment::CODEX_EXEC_SERVER_URL_ENV_VAR;
use crate::environment::LOCAL_ENVIRONMENT_ID;
use crate::environment::REMOTE_ENVIRONMENT_ID;

/// Lists the remote environment transports available to Codex.
///
/// Implementations own a startup snapshot containing both the available
/// environment transport list in configured order and the default environment
/// selection. Providers return transport descriptions before the effective HTTP
/// policy is available; `include_local` controls whether `EnvironmentManager`
/// should add the local environment when the snapshot is built.
pub trait EnvironmentProvider: Send + Sync {
    /// Returns the provider-owned environment startup snapshot.
    fn snapshot(&self) -> EnvironmentProviderFuture<'_>;
}

pub type EnvironmentProviderFuture<'a> =
    Pin<Box<dyn Future<Output = Result<EnvironmentProviderSnapshot, ExecServerError>> + Send + 'a>>;

#[derive(Clone)]
pub struct EnvironmentProviderSnapshot {
    pub(crate) environments: Vec<EnvironmentProviderEntry>,
    pub default: EnvironmentDefault,
    pub include_local: bool,
}

/// Keeps each configured environment's requirements with its transport.
#[derive(Clone, Debug)]
pub(crate) struct EnvironmentProviderEntry {
    pub(crate) id: String,
    pub(crate) transport: ExecServerTransportParams,
    pub(crate) skills: ScopedSkillsConfig,
}

impl std::fmt::Debug for EnvironmentProviderSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let environment_ids: Vec<_> = self.environments.iter().map(|entry| &entry.id).collect();
        f.debug_struct("EnvironmentProviderSnapshot")
            .field("environments", &environment_ids)
            .field("default", &self.default)
            .field("include_local", &self.include_local)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvironmentDefault {
    Disabled,
    EnvironmentId(String),
}

/// Default provider backed by `CODEX_EXEC_SERVER_URL`.
#[derive(Clone, Debug)]
pub struct DefaultEnvironmentProvider {
    exec_server_url: Option<String>,
}

impl DefaultEnvironmentProvider {
    /// Builds a provider from an already-read raw `CODEX_EXEC_SERVER_URL` value.
    pub fn new(exec_server_url: Option<String>) -> Self {
        Self { exec_server_url }
    }

    /// Builds a provider by reading `CODEX_EXEC_SERVER_URL`.
    pub fn from_env() -> Self {
        Self::new(std::env::var(CODEX_EXEC_SERVER_URL_ENV_VAR).ok())
    }

    pub(crate) fn snapshot_inner(&self) -> EnvironmentProviderSnapshot {
        let mut environments = Vec::new();
        let (exec_server_url, disabled) = normalize_exec_server_url(self.exec_server_url.clone());

        if let Some(exec_server_url) = exec_server_url {
            environments.push(EnvironmentProviderEntry {
                id: REMOTE_ENVIRONMENT_ID.to_string(),
                transport: ExecServerTransportParams::websocket_url(
                    exec_server_url,
                    DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT,
                ),
                skills: ScopedSkillsConfig::default(),
            });
        }

        let has_remote = environments
            .iter()
            .any(|entry| entry.id == REMOTE_ENVIRONMENT_ID);
        let include_local = !disabled && !has_remote;
        let default = if disabled {
            EnvironmentDefault::Disabled
        } else if has_remote {
            EnvironmentDefault::EnvironmentId(REMOTE_ENVIRONMENT_ID.to_string())
        } else {
            EnvironmentDefault::EnvironmentId(LOCAL_ENVIRONMENT_ID.to_string())
        };

        EnvironmentProviderSnapshot {
            environments,
            default,
            include_local,
        }
    }
}

impl EnvironmentProvider for DefaultEnvironmentProvider {
    fn snapshot(&self) -> EnvironmentProviderFuture<'_> {
        Box::pin(async { Ok(self.snapshot_inner()) })
    }
}

pub(crate) fn normalize_exec_server_url(exec_server_url: Option<String>) -> (Option<String>, bool) {
    match exec_server_url.as_deref().map(str::trim) {
        None | Some("") => (None, false),
        Some(url) if url.eq_ignore_ascii_case("none") => (None, true),
        Some(url) => (Some(url.to_string()), false),
    }
}
