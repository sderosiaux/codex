use std::sync::Arc;

use codex_install_context::InstallContext;
use codex_utils_path_uri::PathUri;

use codex_http_client::HttpClientFactory;

use crate::ExecutorFileSystem;
use crate::HttpClient;
use crate::http_client::RouteAwareHttpClient;
use crate::local_file_system::LocalFileSystem;
use crate::local_process::LocalProcess;
use crate::process::ExecBackend;

pub const LOCAL_ENVIRONMENT_ID: &str = "local";

/// Owns the single local environment every thread runs commands and file operations in.
#[derive(Debug)]
pub struct EnvironmentManager {
    local: Arc<Environment>,
    http_client_factory: HttpClientFactory,
}

impl EnvironmentManager {
    pub fn new(http_client_factory: HttpClientFactory) -> Self {
        Self {
            local: Arc::new(Environment::local(http_client_factory.clone())),
            http_client_factory,
        }
    }

    pub fn local_environment(&self) -> Arc<Environment> {
        Arc::clone(&self.local)
    }

    /// Resolves an environment id. The local environment is the only one.
    pub fn get_environment(&self, environment_id: &str) -> Option<Arc<Environment>> {
        (environment_id == LOCAL_ENVIRONMENT_ID).then(|| self.local_environment())
    }

    pub fn http_client_factory(&self) -> &HttpClientFactory {
        &self.http_client_factory
    }
}

/// The machine this process runs on: local processes, filesystem and outbound HTTP.
#[derive(Clone)]
pub struct Environment {
    exec_backend: Arc<dyn ExecBackend>,
    filesystem: Arc<dyn ExecutorFileSystem>,
    http_client: Arc<dyn HttpClient>,
}

impl std::fmt::Debug for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Environment").finish_non_exhaustive()
    }
}

impl Environment {
    pub fn local(http_client_factory: HttpClientFactory) -> Self {
        Self {
            exec_backend: Arc::new(LocalProcess::default()),
            filesystem: Arc::new(LocalFileSystem::new()),
            http_client: Arc::new(RouteAwareHttpClient::new(http_client_factory)),
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
}

/// Local default directories used to resolve `:tmpdir`.
pub fn local_temporary_directories() -> Vec<PathUri> {
    let cwd = std::env::current_dir().ok();
    let temporary_directory_env_vars: &[&str] = if cfg!(windows) {
        &["TEMP", "TMP"]
    } else {
        &["TMPDIR"]
    };
    let normalize_temp_path = |path: std::ffi::OsString| {
        PathUri::from_host_native_path(&path).ok().or_else(|| {
            if cfg!(unix) {
                PathUri::from_host_native_path(cwd.as_ref()?.join(path)).ok()
            } else {
                None
            }
        })
    };
    let mut temporary_directories = Vec::new();
    for name in temporary_directory_env_vars {
        if let Some(path) = std::env::var_os(name)
            .filter(|path| !path.is_empty())
            .filter(|path| cfg!(unix) || std::path::Path::new(path).is_absolute())
            .and_then(&normalize_temp_path)
            && !temporary_directories.contains(&path)
        {
            temporary_directories.push(path);
        }
    }
    temporary_directories
}

/// Package directories to prepend to `PATH` when missing, in priority order.
pub fn local_prepend_path_dirs() -> Vec<PathUri> {
    InstallContext::current()
        .package_layout
        .as_ref()
        .and_then(|layout| layout.path_dir.as_ref())
        .into_iter()
        .map(PathUri::from_abs_path)
        .collect()
}
