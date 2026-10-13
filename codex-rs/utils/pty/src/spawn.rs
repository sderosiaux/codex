//! Process launch shared by local turns and the exec-server.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;

use crate::ChildFds;
use crate::SpawnedProcess;
use crate::TerminalSize;
use crate::terminal_queries::respond_to_terminal_queries;

/// Executor-native process launch request.
pub struct SpawnRequest<'a> {
    pub command: &'a [String],
    pub cwd: &'a Path,
    pub env: &'a HashMap<String, String>,
    pub arg0: &'a Option<String>,
    pub tty: bool,
    pub stdin_open: bool,
    pub inherited_fds: ChildFds<'a>,
}

/// Spawns `command` directly, attached to a PTY or to pipes.
pub async fn spawn_process(request: SpawnRequest<'_>) -> Result<SpawnedProcess> {
    let (program, args) = request
        .command
        .split_first()
        .context("missing program for process spawn")?;
    if request.tty {
        crate::pty::spawn_process(
            program,
            args,
            request.cwd,
            request.env,
            request.arg0,
            TerminalSize::default(),
            request.inherited_fds,
        )
        .await
        .map(respond_to_terminal_queries)
    } else if request.stdin_open {
        crate::pipe::spawn_process(
            program,
            args,
            request.cwd,
            request.env,
            request.arg0,
            request.inherited_fds.as_slice(),
        )
        .await
    } else {
        crate::pipe::spawn_process_no_stdin(
            program,
            args,
            request.cwd,
            request.env,
            request.arg0,
            request.inherited_fds.as_slice(),
        )
        .await
    }
}
