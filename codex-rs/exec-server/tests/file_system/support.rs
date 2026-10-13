use std::fmt;
use std::sync::Arc;

use anyhow::Result;
use codex_exec_server::Environment;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::LocalFileSystem;

use crate::common::exec_server::ExecServerHarness;
use crate::common::exec_server::exec_server;

pub(crate) struct FileSystemContext {
    pub(crate) file_system: Arc<dyn ExecutorFileSystem>,
    _server: Option<ExecServerHarness>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum FileSystemImplementation {
    Local,
    Remote,
}

impl fmt::Display for FileSystemImplementation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local => formatter.write_str("local"),
            Self::Remote => formatter.write_str("remote"),
        }
    }
}

pub(crate) async fn create_file_system_context(
    implementation: FileSystemImplementation,
) -> Result<FileSystemContext> {
    match implementation {
        FileSystemImplementation::Local => Ok(FileSystemContext {
            file_system: Arc::new(LocalFileSystem::new()),
            _server: None,
        }),
        FileSystemImplementation::Remote => {
            let server = exec_server().await?;
            let environment =
                Environment::create_for_tests(Some(server.websocket_url().to_string()))?;
            Ok(FileSystemContext {
                file_system: environment.get_filesystem(),
                _server: Some(server),
            })
        }
    }
}
