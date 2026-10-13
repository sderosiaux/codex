mod connect_options;

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::time::Duration;

use arc_swap::ArcSwapOption;
use codex_config::ScopedSkillsConfig;
use codex_http_client::HttpClientFactory;
use codex_protocol::capabilities::CapabilityRootLocation;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::shell_environment::CODEX_EXEC_SERVER_NOISE_AUTH_TOKEN_ENV_VAR;

use crate::CapabilityRootsDiscoverParams;
use crate::CapabilityRootsDiscoverResponse;
use crate::EnvironmentConfigReadParams;
use crate::EnvironmentConfigReadResponse;
use crate::ExecServerError;
use crate::ExecutorFileSystem;
use crate::HttpClient;
use crate::NoiseChannelIdentity;
use crate::NoiseRendezvousConnectProvider;
use crate::client::LazyRemoteExecServerClient;
use crate::client::http_client::RouteAwareHttpClient;
use crate::client_api::DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT;
use crate::client_api::ExecServerTransportParams;
use crate::environment_bootstrap::PreparedEnvironmentManager;
use crate::environment_bootstrap::PreparedEnvironmentSource;
use crate::environment_config::read_environment_config;
use crate::environment_provider::DefaultEnvironmentProvider;
use crate::environment_provider::EnvironmentDefault;
use crate::environment_provider::EnvironmentProvider;
use crate::environment_provider::EnvironmentProviderEntry;
use crate::environment_provider::EnvironmentProviderSnapshot;
use crate::environment_provider::normalize_exec_server_url;
use crate::environment_toml::environment_provider_from_codex_home;
use crate::local_file_system::LocalFileSystem;
use crate::local_process::LocalProcess;
use crate::process::ExecBackend;
use crate::protocol::EnvironmentInfo;
use crate::remote::NoiseRendezvousEnvironmentConfig;
use crate::remote_file_system::RemoteFileSystem;
use crate::remote_process::RemoteProcess;
use tokio::sync::watch;
use tokio_util::task::AbortOnDropHandle;
use tracing::Instrument;
use tracing::instrument::WithSubscriber;

#[path = "environment/accepted.rs"]
mod accepted;

pub use connect_options::RemoteEnvironmentOptions;

pub const CODEX_EXEC_SERVER_URL_ENV_VAR: &str = "CODEX_EXEC_SERVER_URL";
pub const CODEX_EXEC_SERVER_NOISE_REGISTRY_URL_ENV_VAR: &str =
    "CODEX_EXEC_SERVER_NOISE_REGISTRY_URL";
pub const CODEX_EXEC_SERVER_NOISE_ENVIRONMENT_ID_ENV_VAR: &str =
    "CODEX_EXEC_SERVER_NOISE_ENVIRONMENT_ID";
pub const CODEX_EXEC_SERVER_NOISE_CHATGPT_ACCOUNT_ID_ENV_VAR: &str =
    "CODEX_EXEC_SERVER_NOISE_CHATGPT_ACCOUNT_ID";

/// The current connection state for one concrete environment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvironmentConnectionState {
    /// An initialized exec-server connection is available.
    Connected,
    /// No initialized exec-server connection is currently available.
    Disconnected,
}

/// Outcome of an initial connection operation, including its internal retries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionAttemptOutcome {
    Success,
    Failure,
    Cancelled,
}

/// Owns the execution/filesystem environments available to the Codex runtime.
///
/// `EnvironmentManager` is a shared registry for concrete environments. Its
/// default constructor preserves the legacy `CODEX_EXEC_SERVER_URL` behavior
/// while configured construction accepts a provider-supplied snapshot.
///
/// Setting `CODEX_EXEC_SERVER_URL=none` disables environment access by leaving
/// the default environment unset and omitting the local environment. Callers
/// use `default_environment().is_some()` as the signal for model-facing
/// shell/filesystem tool availability.
///
/// Ordinary remote environments begin connecting when added to the manager.
/// Provisioned remote environments connect only after they are selected for use;
/// their deferred transport waits for provisioning to complete first. Filesystem
/// and execution backends share the resulting startup and reconnect as needed.
#[derive(Debug)]
pub struct EnvironmentManager {
    default_environment: Option<String>,
    pub(super) environments: RwLock<HashMap<String, Arc<Environment>>>,
    local_environment: Option<Arc<Environment>>,
    http_client_factory: HttpClientFactory,
}

/// Information supplied by the environment owner when an environment is ready.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EnvironmentReadyInfo {
    /// Ordered capability roots selected for this environment.
    pub selected_capability_roots: Vec<SelectedCapabilityRoot>,
}

/// Maximum capability roots accepted from environment ready information.
pub const MAX_SELECTED_CAPABILITY_ROOTS: usize = 256;

pub const LOCAL_ENVIRONMENT_ID: &str = "local";
pub const REMOTE_ENVIRONMENT_ID: &str = "remote";

/// Non-mutating connection status observed by an environment owner.
///
/// Computing this status never starts, waits for, or reconnects an exec-server
/// transport. Already-ready remote environments may receive a fail-fast probe
/// over their existing connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentObservedStatus {
    /// A local environment, or a remote environment whose existing connection answered a probe.
    Ready,
    /// The configured environment has no ready connection and no observed connection failure.
    ///
    /// This includes lazy transports that have never been started and initial startup that has
    /// not finished. Computing status does not start the environment or wait for startup.
    Pending,
    /// A connection attempt, prior connection, or fail-fast status probe observed a failure.
    ///
    /// This does not promise that the failure is terminal: later normal environment use may
    /// recover the connection. Computing status itself does not trigger recovery.
    Disconnected {
        /// Human-readable reason recorded by the failed connection attempt or probe.
        error: String,
    },
}

impl EnvironmentManager {
    /// Builds a manager with no configured execution environments.
    pub fn without_environments(http_client_factory: HttpClientFactory) -> Self {
        Self {
            default_environment: None,
            environments: RwLock::new(HashMap::new()),
            local_environment: None,
            http_client_factory,
        }
    }

    /// Discovers configured environments without starting remote connections.
    ///
    /// If `CODEX_HOME/environments.toml` is present, it defines the configured
    /// environments. Otherwise this preserves the legacy
    /// `CODEX_EXEC_SERVER_URL` behavior.
    pub async fn prepare_from_codex_home(
        codex_home: impl AsRef<std::path::Path>,
    ) -> Result<PreparedEnvironmentManager, ExecServerError> {
        let source = if let Some(config) = noise_environment_config_from_env()? {
            PreparedEnvironmentSource::Noise(config)
        } else {
            let provider = environment_provider_from_codex_home(codex_home.as_ref())?;
            PreparedEnvironmentSource::Snapshot(provider.snapshot().await?)
        };
        Ok(PreparedEnvironmentManager { source })
    }

    /// Builds a manager from `CODEX_HOME` with an explicit outbound HTTP policy.
    pub async fn from_codex_home(
        codex_home: impl AsRef<std::path::Path>,
        http_client_factory: HttpClientFactory,
    ) -> Result<Self, ExecServerError> {
        Self::prepare_from_codex_home(codex_home)
            .await?
            .build(http_client_factory)
    }

    /// Discovers environment-variable environments without starting connections.
    pub async fn prepare_from_env() -> Result<PreparedEnvironmentManager, ExecServerError> {
        let source = if let Some(config) = noise_environment_config_from_env()? {
            PreparedEnvironmentSource::Noise(config)
        } else {
            let provider = DefaultEnvironmentProvider::from_env();
            PreparedEnvironmentSource::Snapshot(provider.snapshot().await?)
        };
        Ok(PreparedEnvironmentManager { source })
    }

    /// Builds a manager from environment variables with an explicit outbound HTTP policy.
    pub async fn from_env(http_client_factory: HttpClientFactory) -> Result<Self, ExecServerError> {
        Self::prepare_from_env().await?.build(http_client_factory)
    }

    pub(crate) fn from_noise_environment_config(
        config: NoiseRendezvousEnvironmentConfig,
        http_client_factory: HttpClientFactory,
    ) -> Result<Self, ExecServerError> {
        let connect_provider = config.into_connect_provider(http_client_factory.clone())?;
        let manager = Self {
            default_environment: Some(REMOTE_ENVIRONMENT_ID.to_string()),
            environments: RwLock::new(HashMap::new()),
            local_environment: None,
            http_client_factory,
        };
        let identity = noise_channel_identity()?;
        let environment = Arc::new(Environment::remote_with_transport(
            ExecServerTransportParams::NoiseRendezvous {
                provider: connect_provider,
                identity,
            },
            manager.http_client_factory.clone(),
            ScopedSkillsConfig::default(),
        ));
        manager.insert_environment(REMOTE_ENVIRONMENT_ID.to_string(), environment)?;
        Ok(manager)
    }

    pub(crate) fn from_snapshot(
        snapshot: EnvironmentProviderSnapshot,
        http_client_factory: HttpClientFactory,
    ) -> Result<Self, ExecServerError> {
        let EnvironmentProviderSnapshot {
            environments,
            default,
            include_local,
        } = snapshot;
        let mut environment_map =
            HashMap::with_capacity(environments.len() + usize::from(include_local));
        let local_environment = if include_local {
            let local_environment = Arc::new(Environment::local(http_client_factory.clone()));
            environment_map.insert(
                LOCAL_ENVIRONMENT_ID.to_string(),
                Arc::clone(&local_environment),
            );
            Some(local_environment)
        } else {
            None
        };
        for EnvironmentProviderEntry {
            id,
            transport,
            skills,
        } in environments
        {
            if id.is_empty() {
                return Err(ExecServerError::Protocol(
                    "environment id cannot be empty".to_string(),
                ));
            }
            if id == LOCAL_ENVIRONMENT_ID {
                return Err(ExecServerError::Protocol(format!(
                    "environment id `{LOCAL_ENVIRONMENT_ID}` is reserved for EnvironmentManager"
                )));
            }
            let environment =
                Environment::remote_with_transport(transport, http_client_factory.clone(), skills);
            if environment_map
                .insert(id.clone(), Arc::new(environment))
                .is_some()
            {
                return Err(ExecServerError::Protocol(format!(
                    "environment id `{id}` is duplicated"
                )));
            }
        }
        let default_environment = match default {
            EnvironmentDefault::Disabled => None,
            EnvironmentDefault::EnvironmentId(environment_id) => {
                if !environment_map.contains_key(&environment_id) {
                    return Err(ExecServerError::Protocol(format!(
                        "default environment `{environment_id}` is not configured"
                    )));
                }
                Some(environment_id)
            }
        };
        // The snapshot is valid; start connecting its remote environments in the background.
        for environment in environment_map.values() {
            environment.start_connecting();
        }
        Ok(Self {
            default_environment,
            environments: RwLock::new(environment_map),
            local_environment,
            http_client_factory,
        })
    }

    /// Returns the default environment instance.
    pub fn default_environment(&self) -> Option<Arc<Environment>> {
        self.default_environment
            .as_deref()
            .and_then(|environment_id| self.get_environment(environment_id))
    }

    /// Returns the id of the default environment.
    pub fn default_environment_id(&self) -> Option<&str> {
        self.default_environment.as_deref()
    }

    /// Returns the ordered environment ids used for new thread startup.
    pub fn default_environment_ids(&self) -> Vec<String> {
        let Some(default_environment_id) = self.default_environment.as_ref() else {
            return Vec::new();
        };
        let environments = self
            .environments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut environment_ids = Vec::with_capacity(environments.len());
        environment_ids.push(default_environment_id.clone());
        environment_ids.extend(
            environments
                .keys()
                .filter(|environment_id| *environment_id != default_environment_id)
                .cloned(),
        );
        environment_ids
    }

    /// Returns the local environment instance when one is configured.
    pub fn try_local_environment(&self) -> Option<Arc<Environment>> {
        self.local_environment.as_ref().map(Arc::clone)
    }

    /// Returns a named environment instance.
    pub fn get_environment(&self, environment_id: &str) -> Option<Arc<Environment>> {
        self.environments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(environment_id)
            .cloned()
    }

    /// Records a Ready or Failed provisioning result for an environment.
    ///
    /// Ordinary environments are ignored. A provisioned environment keeps the same `Arc` from
    /// Pending through Ready or Failed, and is created if the report arrives first.
    ///
    /// Ready updates capability roots and can recover a failed provisioning attempt. Failed keeps
    /// the first error until a Ready report arrives; a late failure cannot replace Ready. Invalid
    /// Ready information fails an existing Pending environment but does not create a missing one.
    ///
    /// This only updates provisioning. The connection starts when the environment is selected.
    pub fn report_environment_provisioning_status(
        &self,
        environment_id: String,
        readiness: Result<EnvironmentReadyInfo, String>,
        provider_if_missing: Arc<dyn NoiseRendezvousConnectProvider>,
    ) -> Result<Option<Arc<Environment>>, ExecServerError> {
        validate_environment_id(&environment_id)?;
        let mut environments = self
            .environments
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(environment) = environments.get(&environment_id).cloned() {
            if environment.provisioning_status_tx.is_none() {
                return Ok(None);
            }
            match readiness {
                Ok(ready_info) => {
                    environment.apply_ready_report(&environment_id, ready_info)?;
                }
                Err(error) => {
                    environment.apply_error_report(&environment_id, error)?;
                }
            }
            return Ok(Some(environment));
        }

        let environment = match readiness {
            Ok(ready_info) => {
                validate_environment_ready_info(&environment_id, &ready_info)?;
                let environment = Arc::new(
                    self.provisioning_noise_environment(provider_if_missing, Some(Ok(())))?,
                );
                environment.ready_info.store(Some(Arc::new(ready_info)));
                environment
            }
            Err(error) => Arc::new(
                self.provisioning_noise_environment(provider_if_missing, Some(Err(error)))?,
            ),
        };
        environments.insert(environment_id, Arc::clone(&environment));
        Ok(Some(environment))
    }

    /// Returns the outbound HTTP policy carried by this manager.
    pub fn http_client_factory(&self) -> &HttpClientFactory {
        &self.http_client_factory
    }

    /// Returns the current status of one named environment when it is configured.
    pub async fn get_environment_status(
        &self,
        environment_id: &str,
    ) -> Option<EnvironmentObservedStatus> {
        let environment = self.get_environment(environment_id)?;
        Some(environment.status().await)
    }

    /// Adds or replaces a named remote environment without changing the
    /// manager's default environment selection. Uses the default WebSocket
    /// connection timeout when none is provided.
    pub fn upsert_environment(
        &self,
        environment_id: String,
        exec_server_url: String,
        connect_timeout: Option<std::time::Duration>,
    ) -> Result<(), ExecServerError> {
        self.upsert_environment_with_options(
            environment_id,
            RemoteEnvironmentOptions {
                exec_server_url,
                connect_timeout,
                http_headers: HashMap::new(),
            },
            ScopedSkillsConfig::default(),
        )
    }

    /// Adds or replaces a direct environment with trusted host-owned connection options.
    ///
    /// Invalid headers and WebSocket-controlled handshake headers are rejected
    /// before the environment is registered. Valid headers are retained for
    /// automatic reconnects without changing existing URL-only environment APIs.
    pub fn upsert_environment_with_options(
        &self,
        environment_id: String,
        options: RemoteEnvironmentOptions,
        skills: ScopedSkillsConfig,
    ) -> Result<(), ExecServerError> {
        validate_environment_id(&environment_id)?;
        let transport = options.into_transport_params()?;
        let environment =
            Environment::remote_with_transport(transport, self.http_client_factory.clone(), skills);
        self.insert_environment(environment_id, Arc::new(environment))
    }

    /// Returns the stable environment for an ID, creating it as pending when absent.
    pub fn materialize_pending_noise_environment(
        &self,
        environment_id: String,
        provider: Arc<dyn NoiseRendezvousConnectProvider>,
    ) -> Result<Arc<Environment>, ExecServerError> {
        validate_environment_id(&environment_id)?;
        let mut environments = self
            .environments
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(environment) = environments.get(&environment_id) {
            if environment.provisioning_status_tx.is_none() {
                return Err(ExecServerError::ProvisioningModeConflict { environment_id });
            }
            return Ok(Arc::clone(environment));
        }

        let environment =
            Arc::new(self.provisioning_noise_environment(provider, /*initial_result*/ None)?);
        environments.insert(environment_id, Arc::clone(&environment));
        Ok(environment)
    }

    fn provisioning_noise_environment(
        &self,
        provider: Arc<dyn NoiseRendezvousConnectProvider>,
        initial_result: Option<Result<(), String>>,
    ) -> Result<Environment, ExecServerError> {
        let identity = noise_channel_identity()?;
        let (provisioning_status_tx, provisioning_status_rx) = watch::channel(initial_result);
        let mut environment = Environment::remote_with_transport(
            ExecServerTransportParams::Deferred(Box::new(crate::client_api::Deferred {
                readiness: provisioning_status_rx,
                transport: ExecServerTransportParams::NoiseRendezvous { provider, identity },
            })),
            self.http_client_factory.clone(),
            ScopedSkillsConfig::default(),
        );
        environment.provisioning_status_tx = Some(provisioning_status_tx);
        Ok(environment)
    }

    fn insert_environment(
        &self,
        environment_id: String,
        environment: Arc<Environment>,
    ) -> Result<(), ExecServerError> {
        let replaced = {
            let mut environments = self
                .environments
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            environments.insert(environment_id, Arc::clone(&environment))
        };
        drop(replaced);
        environment.start_connecting();
        Ok(())
    }
}

fn validate_environment_ready_info(
    environment_id: &str,
    ready_info: &EnvironmentReadyInfo,
) -> Result<(), ExecServerError> {
    if ready_info.selected_capability_roots.len() > MAX_SELECTED_CAPABILITY_ROOTS {
        return Err(ExecServerError::Protocol(format!(
            "environment ready info contains more than {MAX_SELECTED_CAPABILITY_ROOTS} selected capability roots"
        )));
    }

    let mut root_ids = HashSet::with_capacity(ready_info.selected_capability_roots.len());
    for root in &ready_info.selected_capability_roots {
        let CapabilityRootLocation::Environment {
            environment_id: root_environment_id,
            ..
        } = &root.location;
        if root.id.trim().is_empty()
            || root_environment_id != environment_id
            || !root_ids.insert(root.id.as_str())
        {
            return Err(ExecServerError::Protocol(format!(
                "selected capability roots must have unique non-empty IDs and belong to environment `{environment_id}`"
            )));
        }
    }

    Ok(())
}

fn noise_channel_identity() -> Result<NoiseChannelIdentity, ExecServerError> {
    NoiseChannelIdentity::generate().map_err(|error| {
        ExecServerError::Protocol(format!(
            "failed to generate Noise harness identity: {error}"
        ))
    })
}

fn validate_environment_id(environment_id: &str) -> Result<(), ExecServerError> {
    if environment_id.is_empty() {
        return Err(ExecServerError::Protocol(
            "environment id cannot be empty".to_string(),
        ));
    }
    if environment_id == LOCAL_ENVIRONMENT_ID {
        return Err(ExecServerError::Protocol(format!(
            "environment id `{LOCAL_ENVIRONMENT_ID}` is reserved for EnvironmentManager"
        )));
    }
    Ok(())
}

fn noise_environment_config_from_env()
-> Result<Option<NoiseRendezvousEnvironmentConfig>, ExecServerError> {
    noise_environment_config_from_values(
        optional_environment_value(CODEX_EXEC_SERVER_NOISE_REGISTRY_URL_ENV_VAR),
        optional_environment_value(CODEX_EXEC_SERVER_NOISE_ENVIRONMENT_ID_ENV_VAR),
        optional_environment_value(CODEX_EXEC_SERVER_NOISE_AUTH_TOKEN_ENV_VAR),
        optional_environment_value(CODEX_EXEC_SERVER_NOISE_CHATGPT_ACCOUNT_ID_ENV_VAR),
    )
}

fn noise_environment_config_from_values(
    registry_url: Option<String>,
    environment_id: Option<String>,
    auth_token: Option<String>,
    chatgpt_account_id: Option<String>,
) -> Result<Option<NoiseRendezvousEnvironmentConfig>, ExecServerError> {
    let (registry_url, environment_id, auth_token) =
        match (registry_url, environment_id, auth_token) {
            (None, None, None) => return Ok(None),
            (Some(registry_url), Some(environment_id), Some(auth_token)) => {
                (registry_url, environment_id, auth_token)
            }
            _ => {
                return Err(ExecServerError::EnvironmentRegistryConfig(format!(
                    "Noise environment requires {CODEX_EXEC_SERVER_NOISE_REGISTRY_URL_ENV_VAR}, \
{CODEX_EXEC_SERVER_NOISE_ENVIRONMENT_ID_ENV_VAR}, and \
{CODEX_EXEC_SERVER_NOISE_AUTH_TOKEN_ENV_VAR}"
                )));
            }
        };

    NoiseRendezvousEnvironmentConfig::new(
        registry_url,
        environment_id,
        auth_token,
        chatgpt_account_id,
    )
    .map(Some)
}

fn optional_environment_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Concrete execution/filesystem environment selected for a session.
///
/// This bundles the selected backend metadata together with the local runtime
/// paths used by filesystem helpers.
#[derive(Clone)]
pub struct Environment {
    skills: ScopedSkillsConfig,
    remote_client: Option<LazyRemoteExecServerClient>,
    ready_info: Arc<ArcSwapOption<EnvironmentReadyInfo>>,
    // No sender means an ordinary environment. A provisioned environment retains a sender whose
    // value is None while Pending, Some(Ok(())) when Ready, or Some(Err(error)) when Failed.
    provisioning_status_tx: Option<watch::Sender<Option<Result<(), String>>>>,
    // Dropping the environment stops unfinished background startup work.
    startup_task: Arc<Mutex<Option<AbortOnDropHandle<()>>>>,
    exec_backend: Arc<dyn ExecBackend>,
    filesystem: Arc<dyn ExecutorFileSystem>,
    http_client: Arc<dyn HttpClient>,
}

impl Environment {}

impl std::fmt::Debug for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Environment").finish_non_exhaustive()
    }
}

impl Environment {
    /// Builds an environment using the caller's effective outbound HTTP policy.
    pub fn create(
        exec_server_url: Option<String>,
        http_client_factory: HttpClientFactory,
    ) -> Result<Self, ExecServerError> {
        Self::create_inner(exec_server_url, http_client_factory)
    }

    /// Builds an environment from the raw `CODEX_EXEC_SERVER_URL` value and
    /// local runtime paths used when creating local filesystem helpers.
    fn create_inner(
        exec_server_url: Option<String>,
        http_client_factory: HttpClientFactory,
    ) -> Result<Self, ExecServerError> {
        let (exec_server_url, disabled) = normalize_exec_server_url(exec_server_url);
        if disabled {
            return Err(ExecServerError::Protocol(
                "disabled mode does not create an Environment".to_string(),
            ));
        }

        Ok(match exec_server_url {
            Some(exec_server_url) => Self::remote_with_transport(
                ExecServerTransportParams::websocket_url(
                    exec_server_url,
                    DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT,
                ),
                http_client_factory,
                ScopedSkillsConfig::default(),
            ),
            None => Self::local(http_client_factory),
        })
    }

    pub(crate) fn local(http_client_factory: HttpClientFactory) -> Self {
        Self {
            remote_client: None,
            skills: ScopedSkillsConfig::default(),
            ready_info: Arc::new(ArcSwapOption::empty()),
            provisioning_status_tx: None,
            startup_task: Arc::new(Mutex::new(None)),
            exec_backend: Arc::new(LocalProcess::default()),
            filesystem: Arc::new(LocalFileSystem::new()),
            http_client: Arc::new(RouteAwareHttpClient::new(http_client_factory)),
        }
    }

    pub(crate) fn remote_with_transport(
        remote_transport: ExecServerTransportParams,
        http_client_factory: HttpClientFactory,
        skills: ScopedSkillsConfig,
    ) -> Self {
        let client = LazyRemoteExecServerClient::new(remote_transport, http_client_factory);
        Self::remote_with_client(client, skills)
    }

    pub(crate) fn remote_with_client(
        client: LazyRemoteExecServerClient,
        skills: ScopedSkillsConfig,
    ) -> Self {
        let exec_backend: Arc<dyn ExecBackend> = Arc::new(RemoteProcess::new(client.clone()));
        let filesystem: Arc<dyn ExecutorFileSystem> =
            Arc::new(RemoteFileSystem::new(client.clone()));

        Self {
            remote_client: Some(client.clone()),
            skills,
            ready_info: Arc::new(ArcSwapOption::empty()),
            provisioning_status_tx: None,
            startup_task: Arc::new(Mutex::new(None)),
            exec_backend,
            filesystem,
            http_client: Arc::new(client),
        }
    }

    /// Skill names required whenever this registration is selected by a turn.
    pub fn required_skills(&self) -> &[String] {
        &self.skills.required
    }

    pub fn is_remote(&self) -> bool {
        self.remote_client.is_some()
    }

    fn apply_error_report(
        &self,
        environment_id: &str,
        error: String,
    ) -> Result<(), ExecServerError> {
        let Some(provisioning_status_tx) = &self.provisioning_status_tx else {
            return Ok(());
        };
        let mut transition_error = None;
        provisioning_status_tx.send_if_modified(|current| match current.as_ref() {
            None => {
                *current = Some(Err(error.clone()));
                true
            }
            Some(Ok(())) => {
                transition_error = Some(ExecServerError::Protocol(format!(
                    "environment `{environment_id}` is already ready, but a later provisioning report failed: {error}"
                )));
                false
            }
            Some(Err(_)) => false,
        });

        transition_error.map_or(Ok(()), Err)
    }

    fn apply_ready_report(
        &self,
        environment_id: &str,
        ready_info: EnvironmentReadyInfo,
    ) -> Result<(), ExecServerError> {
        let Some(provisioning_status_tx) = &self.provisioning_status_tx else {
            return Ok(());
        };
        let mut transition_error = None;
        provisioning_status_tx.send_if_modified(|current| {
            if let Err(error) = validate_environment_ready_info(environment_id, &ready_info) {
                let pending = current.is_none();
                if pending {
                    *current = Some(Err(error.to_string()));
                }
                transition_error = Some(error);
                return pending;
            }
            self.ready_info.store(Some(Arc::new(ready_info.clone())));
            let was_ready = matches!(current, Some(Ok(())));
            *current = Some(Ok(()));
            !was_ready
        });

        transition_error.map_or(Ok(()), Err)
    }

    /// Returns a snapshot of the last accepted Ready report.
    ///
    /// `None` means no Ready report has been accepted, including for ordinary environments.
    /// A report with no capability roots is distinct from `None`. The snapshot does not change
    /// when later reports arrive and does not indicate whether the connection is healthy.
    pub fn last_ready_info(&self) -> Option<Arc<EnvironmentReadyInfo>> {
        self.ready_info.load_full()
    }

    /// Returns the capability roots most recently reported for this environment.
    pub fn selected_capability_roots(&self) -> Vec<SelectedCapabilityRoot> {
        self.ready_info
            .load()
            .as_ref()
            .map_or_else(Vec::new, |ready_info| {
                ready_info.selected_capability_roots.clone()
            })
    }

    /// Observes initial connection attempts from the end of provisioning through client installation.
    /// Register before connecting. The first observer wins; past attempts and reconnections are omitted.
    pub fn observe_connection_attempts(
        &self,
        observer: impl Fn(std::time::Duration, ConnectionAttemptOutcome) + Send + Sync + 'static,
    ) {
        if let Some(client) = &self.remote_client {
            let _ = client.connection_observer.set(Arc::new(observer));
        }
    }

    /// Subscribes to the current connection state for this remote environment.
    pub fn subscribe_connection_state(
        &self,
    ) -> Option<watch::Receiver<EnvironmentConnectionState>> {
        self.remote_client
            .as_ref()
            .map(LazyRemoteExecServerClient::subscribe_connection_state)
    }

    /// Returns environment information from the selected execution/filesystem environment.
    /// Remote metadata is cached for the current client's lifetime.
    #[tracing::instrument(
        name = "exec_server.environment.info",
        skip_all,
        fields(remote = self.is_remote())
    )]
    pub async fn info(&self) -> Result<EnvironmentInfo, ExecServerError> {
        match &self.remote_client {
            Some(client) => client.environment_info().await,
            None => Ok(EnvironmentInfo::local()),
        }
    }

    /// Registration on the installed Noise session, without connecting or refreshing it.
    ///
    /// Returns `None` before a session is installed and for non-Noise environments.
    /// A registry lookup does not change this value until the new connection is
    /// installed. Recovery may renew the registration while preserving the session.
    /// Callers that authorize asynchronously must check the snapshot again before
    /// admitting work.
    pub fn cached_executor_registration_id(&self) -> Option<String> {
        self.remote_client
            .as_ref()
            .and_then(LazyRemoteExecServerClient::cached_executor_registration_id)
    }

    /// Refresh the connection to the executor currently registered for this environment.
    ///
    /// # Caller contract
    ///
    /// Call after a planned replacement has registered and become available under the
    /// same environment ID. This method does not provision or destroy executors, or
    /// wait for the registry to identify a particular replacement. It requires a remote
    /// Noise registry-backed environment; other environment types return an error.
    ///
    /// # Session behavior
    ///
    /// A fresh registry lookup determines whether the current session can be reused.
    /// A changed registration or executor key, or a failed or missing session, causes a fresh connection
    /// without resuming the old session. Retirement cancels old recovery, fails its
    /// outstanding work and process handles, and never replays commands. The environment
    /// object and filesystem handle remain usable through the new connection.
    /// A matching registration and executor key preserve a session that has not failed, including one
    /// that is recovering; the live readiness check rejects a recovering connection.
    ///
    /// # Completion and errors
    ///
    /// Success means the selected connection answered a live status RPC, not merely that
    /// metadata was cached. Refresh bypasses the old session's recovery deadline, but
    /// registry lookup, connection, and status RPC timeouts still apply. If the initial
    /// registry lookup fails, refresh leaves the old session untouched; errors after
    /// retirement do not restore it. Ordinary disconnect recovery is unchanged unless
    /// refresh retires the session.
    #[tracing::instrument(
        name = "exec_server.environment.refresh_connection",
        skip_all,
        fields(remote = self.is_remote())
    )]
    pub async fn refresh_connection(&self) -> Result<(), ExecServerError> {
        let client = self.remote_client.as_ref().ok_or_else(|| {
            ExecServerError::Protocol(
                "connection refresh requires a remote environment".to_string(),
            )
        })?;
        client.refresh_connection().await
    }

    /// Fetches uncached metadata, connecting or waiting for recovery as needed.
    // TODO: Remove after app-server migrates off of force_environment_info.
    #[tracing::instrument(
        name = "exec_server.environment.force_info",
        skip_all,
        fields(remote = self.is_remote())
    )]
    pub async fn force_info(&self) -> Result<EnvironmentInfo, ExecServerError> {
        match &self.remote_client {
            Some(client) => client.get().await?.force_environment_info().await,
            None => Ok(EnvironmentInfo::local()),
        }
    }

    /// Reads selected executor-local configuration fields for this environment.
    pub async fn read_environment_config(
        &self,
        params: EnvironmentConfigReadParams,
    ) -> Result<EnvironmentConfigReadResponse, ExecServerError> {
        match &self.remote_client {
            Some(client) => client.get().await?.read_environment_config(params).await,
            None => read_environment_config(self.filesystem.as_ref(), params)
                .await
                .map_err(|error| ExecServerError::Protocol(error.to_string())),
        }
    }

    /// Reads configuration with a timeout for the remote RPC once connected.
    /// Remote transports without a reconnect strategy remain open after a timeout.
    /// Local reads retain their existing behavior without a timeout.
    pub async fn read_environment_config_with_timeout(
        &self,
        params: EnvironmentConfigReadParams,
        rpc_timeout: Duration,
    ) -> Result<EnvironmentConfigReadResponse, ExecServerError> {
        match &self.remote_client {
            Some(client) => {
                client
                    .get()
                    .await?
                    .read_environment_config_with_timeout(params, rpc_timeout)
                    .await
            }
            None => self.read_environment_config(params).await,
        }
    }

    /// Discovers plugin and skill manifests through the environment's high-level discovery API.
    pub async fn discover_capability_roots(
        &self,
        params: CapabilityRootsDiscoverParams,
    ) -> Result<CapabilityRootsDiscoverResponse, ExecServerError> {
        match &self.remote_client {
            Some(client) => {
                let mut connection_state = client.subscribe_connection_state();
                let client = client.get().await?;
                let discover = || async { client.discover_capability_roots(params.clone()).await };
                match discover().await {
                    Err(error) if crate::client::is_retryable_recovery_error(&error) => {
                        tracing::warn!(%error, "replaying capability discovery after executor recovery");
                        let recovered =
                            tokio::time::timeout(std::time::Duration::from_secs(8), async {
                                loop {
                                    match self.readiness_result() {
                                        Some(Ok(())) => return Some(Ok(())),
                                        Some(Err(error))
                                            if !crate::client::is_retryable_recovery_error(
                                                &error,
                                            ) =>
                                        {
                                            return Some(Err(error));
                                        }
                                        Some(Err(_)) | None => {}
                                    }
                                    if connection_state.changed().await.is_err() {
                                        return None;
                                    }
                                }
                            })
                            .await;
                        match recovered {
                            Ok(Some(Ok(()))) => discover().await,
                            Ok(Some(Err(error))) => Err(error),
                            Ok(None) | Err(_) => Err(error),
                        }
                    }
                    response => response,
                }
            }
            None => crate::discover_capability_roots(self.filesystem.as_ref(), params)
                .await
                .map_err(|error| ExecServerError::Protocol(error.to_string())),
        }
    }

    /// Starts connecting a remote environment without waiting for it.
    /// Requires an active Tokio runtime when background startup is supported.
    pub fn start_connecting(&self) {
        let Some(client) = &self.remote_client else {
            return;
        };
        let mut startup_task = self
            .startup_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if startup_task.is_none() {
            *startup_task = client.start_connecting();
        }
    }

    /// Starts the initial connection after an environment is actually selected for use.
    pub(crate) fn start_connecting_for_use(environment: &Arc<Self>) {
        let Some(client) = &environment.remote_client else {
            return;
        };
        let mut startup_task = environment
            .startup_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if startup_task.is_none() {
            let client = client.clone();
            *startup_task = Some(AbortOnDropHandle::new(tokio::spawn(
                async move {
                    if let Err(error) = client.wait_until_ready().await {
                        tracing::debug!(%error, "exec-server environment startup failed");
                    }
                }
                .in_current_span()
                .with_current_subscriber(),
            )));
        }
    }

    /// Returns whether startup has completed, including a first connection made by refresh.
    pub fn startup_finished(&self) -> bool {
        self.remote_client
            .as_ref()
            .is_none_or(LazyRemoteExecServerClient::startup_finished)
    }

    /// Waits for initial startup, retrying a previous transient failure when possible.
    #[tracing::instrument(
        name = "exec_server.environment.wait_until_ready",
        skip_all,
        fields(remote = self.is_remote())
    )]
    pub async fn wait_until_ready(&self) -> Result<(), ExecServerError> {
        match &self.remote_client {
            Some(client) => client.wait_until_ready().await,
            None => Ok(()),
        }
    }

    /// Returns whether the environment can serve a request without waiting or reconnecting.
    pub(crate) fn readiness_result(&self) -> Option<Result<(), ExecServerError>> {
        match &self.remote_client {
            Some(client) => client.readiness_result(),
            None => Some(Ok(())),
        }
    }

    /// Returns the environment's status without starting or recovering it.
    ///
    /// Local environments are always ready. Remote environments with an
    /// already-ready cached connection receive a fail-fast `environment/status`
    /// probe; other remote states are returned from cached connection state
    /// without waiting for startup or recovery.
    pub async fn status(&self) -> EnvironmentObservedStatus {
        match &self.remote_client {
            Some(client) => client.status().await,
            None => EnvironmentObservedStatus::Ready,
        }
    }

    pub fn get_exec_backend(&self) -> Arc<dyn ExecBackend> {
        Arc::clone(&self.exec_backend)
    }

    pub fn get_http_client(&self) -> Arc<dyn HttpClient> {
        Arc::clone(&self.http_client)
    }

    pub fn get_filesystem(&self) -> Arc<dyn ExecutorFileSystem> {
        Arc::clone(&self.filesystem)
    }

    /// Borrows the shared filesystem identity without extending the environment's lifetime.
    pub fn filesystem_ref(&self) -> &Arc<dyn ExecutorFileSystem> {
        &self.filesystem
    }

    /// Returns a filesystem view that fails instead of starting or waiting for a connection.
    pub fn get_filesystem_without_reconnect(&self) -> Arc<dyn ExecutorFileSystem> {
        match &self.remote_client {
            Some(client) => Arc::new(RemoteFileSystem::new(client.fail_fast())),
            None => Arc::clone(&self.filesystem),
        }
    }
}
