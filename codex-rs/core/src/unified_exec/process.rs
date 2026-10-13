#![allow(clippy::module_inception)]

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::broadcast;
use tokio::sync::oneshot::error::TryRecvError;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

use codex_exec_server::ExecProcess;
use codex_exec_server::ExecProcessEvent;
use codex_exec_server::ProcessSignal as ExecServerProcessSignal;
use codex_exec_server::ReadResponse as ExecReadResponse;
use codex_exec_server::StartedExecProcess;
use codex_exec_server::WriteStatus;
use codex_utils_pty::ExecCommandSession;
use codex_utils_pty::ProcessSignal as PtyProcessSignal;
use codex_utils_pty::SpawnedPty;

use super::UNIFIED_EXEC_OUTPUT_MAX_BYTES;
use super::UnifiedExecError;
use super::head_tail_buffer::HeadTailBuffer;
use super::process_state::ProcessState;
use crate::shell_snapshot::ShellSnapshotFile;

const EARLY_EXIT_GRACE_PERIOD: Duration = Duration::from_millis(150);
/// Output pending model polling and the retained completion transcript.
/// Append both under the same lock so cancellation cannot split an update.
#[derive(Default)]
pub(crate) struct OutputBuffers<const MAX_BYTES: usize = UNIFIED_EXEC_OUTPUT_MAX_BYTES> {
    pub(super) pending: HeadTailBuffer<MAX_BYTES>,
    pub(super) transcript: HeadTailBuffer<MAX_BYTES>,
}

impl<const MAX_BYTES: usize> OutputBuffers<MAX_BYTES> {
    pub(super) fn push_chunk(&mut self, chunk: &[u8]) {
        self.pending.push_chunk(chunk);
        self.transcript.push_chunk(chunk);
    }
}

/// Shared output state exposed to polling and streaming consumers.
#[derive(Clone)]
pub(crate) struct OutputHandles<const MAX_BYTES: usize = UNIFIED_EXEC_OUTPUT_MAX_BYTES> {
    pub(crate) output_buffer: Arc<Mutex<OutputBuffers<MAX_BYTES>>>,
    pub(crate) output_notify: Arc<Notify>,
    pub(crate) output_closed: Arc<AtomicBool>,
    pub(crate) output_closed_notify: Arc<Notify>,
    pub(crate) cancellation_token: CancellationToken,
}

struct OutputTaskGuard {
    output_closed: Arc<AtomicBool>,
    output_closed_notify: Arc<Notify>,
}

impl Drop for OutputTaskGuard {
    fn drop(&mut self) {
        self.output_closed.store(true, Ordering::Release);
        self.output_closed_notify.notify_waiters();
    }
}

/// Transport-specific process handle used by unified exec.
enum ProcessHandle {
    Local(Box<ExecCommandSession>),
    ExecServer(Arc<dyn ExecProcess>),
}

/// Unified wrapper over directly spawned PTY sessions and exec-server-backed
/// processes.
pub(crate) struct UnifiedExecProcess {
    process_handle: ProcessHandle,
    output_tx: broadcast::Sender<Vec<u8>>,
    output: OutputHandles,
    output_drained: Arc<Notify>,
    interaction_lock: Arc<Mutex<()>>,
    state_tx: watch::Sender<ProcessState>,
    state_rx: watch::Receiver<ProcessState>,
    output_task: Option<JoinHandle<()>>,
    // The shell may still need to replay this file after process startup returns.
    pub(crate) _shell_snapshot: Option<Arc<ShellSnapshotFile>>,
}

impl std::fmt::Debug for UnifiedExecProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnifiedExecProcess")
            .field("has_exited", &self.has_exited())
            .field("exit_code", &self.exit_code())
            .finish_non_exhaustive()
    }
}

impl UnifiedExecProcess {
    fn new(process_handle: ProcessHandle) -> Self {
        let output = OutputHandles {
            output_buffer: Arc::new(Mutex::new(OutputBuffers::default())),
            output_notify: Arc::new(Notify::new()),
            output_closed: Arc::new(AtomicBool::new(false)),
            output_closed_notify: Arc::new(Notify::new()),
            cancellation_token: CancellationToken::new(),
        };
        let output_drained = Arc::new(Notify::new());
        let (output_tx, _) = broadcast::channel(64);
        let (state_tx, state_rx) = watch::channel(ProcessState::default());

        Self {
            process_handle,
            output_tx,
            output,
            output_drained,
            interaction_lock: Arc::new(Mutex::new(())),
            state_tx,
            state_rx,
            output_task: None,
            _shell_snapshot: None,
        }
    }

    pub(super) async fn write(&self, data: &[u8]) -> Result<(), UnifiedExecError> {
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => process_handle
                .writer_sender()
                .send(data.to_vec())
                .await
                .map_err(|_| UnifiedExecError::WriteToStdin),
            ProcessHandle::ExecServer(process_handle) => {
                match process_handle.write(data.to_vec()).await {
                    Ok(response) => match response.status {
                        WriteStatus::Accepted => Ok(()),
                        WriteStatus::UnknownProcess | WriteStatus::StdinClosed => {
                            let state = self.state_rx.borrow().clone();
                            let _ = self.state_tx.send_replace(state.exited(state.exit_code));
                            self.output.cancellation_token.cancel();
                            Err(UnifiedExecError::WriteToStdin)
                        }
                        WriteStatus::Starting => Err(UnifiedExecError::WriteToStdin),
                    },
                    Err(err) => Err(UnifiedExecError::process_failed(err.to_string())),
                }
            }
        }
    }

    pub(super) fn output_handles(&self) -> &OutputHandles {
        &self.output
    }

    pub(super) fn output_receiver(&self) -> tokio::sync::broadcast::Receiver<Vec<u8>> {
        self.output_tx.subscribe()
    }

    pub(super) fn cancellation_token(&self) -> CancellationToken {
        self.output.cancellation_token.clone()
    }

    pub(super) fn output_drained_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.output_drained)
    }

    pub(super) fn interaction_lock(&self) -> Arc<Mutex<()>> {
        Arc::clone(&self.interaction_lock)
    }

    pub(super) fn has_exited(&self) -> bool {
        let state = self.state_rx.borrow().clone();
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => state.has_exited || process_handle.has_exited(),
            ProcessHandle::ExecServer(_) => state.has_exited,
        }
    }

    pub(super) fn exit_code(&self) -> Option<i32> {
        let state = self.state_rx.borrow().clone();
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => {
                state.exit_code.or_else(|| process_handle.exit_code())
            }
            ProcessHandle::ExecServer(_) => state.exit_code,
        }
    }

    fn finish_termination(&self) {
        self.output.cancellation_token.cancel();
        if let Some(output_task) = &self.output_task {
            output_task.abort();
        }
    }

    pub(super) fn terminate(&self) {
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => process_handle.terminate(),
            ProcessHandle::ExecServer(process_handle) => {
                let process_handle = Arc::clone(process_handle);
                tokio::spawn(async move {
                    let _ = process_handle.terminate().await;
                });
            }
        }
        self.finish_termination();
    }

    pub(super) async fn terminate_confirmed(&self) -> Result<(), UnifiedExecError> {
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => process_handle.terminate(),
            ProcessHandle::ExecServer(process_handle) => {
                process_handle
                    .terminate()
                    .await
                    .map_err(|err| UnifiedExecError::process_failed(err.to_string()))?;
            }
        }
        self.signal_exit(self.exit_code());
        self.finish_termination();
        Ok(())
    }

    pub(super) async fn interrupt(&self) -> Result<(), UnifiedExecError> {
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => process_handle
                .signal(PtyProcessSignal::Interrupt)
                .map_err(|err| UnifiedExecError::process_failed(err.to_string())),
            ProcessHandle::ExecServer(process_handle) => process_handle
                .signal(ExecServerProcessSignal::Interrupt)
                .await
                .map_err(|err| UnifiedExecError::process_failed(err.to_string())),
        }
    }

    pub(super) fn failure_message(&self) -> Option<String> {
        self.state_rx.borrow().failure_message.clone()
    }

    pub(super) async fn from_spawned(spawned: SpawnedPty) -> Result<Self, UnifiedExecError> {
        let SpawnedPty {
            session: process_handle,
            stdout_rx,
            stderr_rx,
            mut exit_rx,
        } = spawned;
        let output_rx = codex_utils_pty::combine_output_receivers(stdout_rx, stderr_rx);
        let mut managed = Self::new(ProcessHandle::Local(Box::new(process_handle)));
        managed.output_task = Some(Self::spawn_local_output_task(
            output_rx,
            managed.output_handles().clone(),
            managed.output_tx.clone(),
        ));

        match exit_rx.try_recv() {
            Ok(exit_code) => {
                managed.signal_exit(Some(exit_code));
                return Ok(managed);
            }
            Err(TryRecvError::Closed) => {
                managed.signal_exit(/*exit_code*/ None);
                return Ok(managed);
            }
            Err(TryRecvError::Empty) => {}
        }

        if let Ok(exit_result) = tokio::time::timeout(EARLY_EXIT_GRACE_PERIOD, &mut exit_rx).await {
            managed.signal_exit(exit_result.ok());
            return Ok(managed);
        }

        tokio::spawn({
            let state_tx = managed.state_tx.clone();
            let cancellation_token = managed.output.cancellation_token.clone();
            async move {
                let exit_code = exit_rx.await.ok();
                let state = state_tx.borrow().clone();
                let _ = state_tx.send_replace(state.exited(exit_code));
                cancellation_token.cancel();
            }
        });

        Ok(managed)
    }

    pub(super) fn from_exec_server_started(started: StartedExecProcess) -> Self {
        let process_handle = ProcessHandle::ExecServer(Arc::clone(&started.process));
        let mut managed = Self::new(process_handle);
        let output_handles = managed.output_handles().clone();
        managed.output_task = Some(Self::spawn_exec_server_output_task(
            started,
            output_handles,
            managed.output_tx.clone(),
            managed.state_tx.clone(),
        ));
        managed
    }

    fn spawn_exec_server_output_task(
        started: StartedExecProcess,
        output_handles: OutputHandles,
        output_tx: broadcast::Sender<Vec<u8>>,
        state_tx: watch::Sender<ProcessState>,
    ) -> JoinHandle<()> {
        let OutputHandles {
            output_buffer,
            output_notify,
            output_closed,
            output_closed_notify,
            cancellation_token,
        } = output_handles;
        let process = started.process;
        let mut events = process.subscribe_events();
        tokio::spawn(async move {
            let _output_task_guard = OutputTaskGuard {
                output_closed: Arc::clone(&output_closed),
                output_closed_notify: Arc::clone(&output_closed_notify),
            };
            let mut last_seq: u64 = 0;
            loop {
                let event = match events.recv().await {
                    Ok(event) => Some(event),
                    Err(broadcast::error::RecvError::Lagged(_)) => None,
                    Err(broadcast::error::RecvError::Closed) => {
                        let state = state_tx.borrow().clone();
                        let _ = state_tx.send_replace(
                            state.failed("exec-server process event stream closed".to_string()),
                        );
                        output_closed.store(true, Ordering::Release);
                        output_closed_notify.notify_waiters();
                        cancellation_token.cancel();
                        break;
                    }
                };
                let event_seq = event.as_ref().map(|event| match event {
                    ExecProcessEvent::Output(chunk) => chunk.seq,
                    ExecProcessEvent::Exited { seq, .. } | ExecProcessEvent::Closed { seq } => *seq,
                });
                if event.is_none() || event_seq.is_some_and(|seq| seq > last_seq.saturating_add(1))
                {
                    let response = match process
                        .read(
                            Some(last_seq),
                            /*max_bytes*/ None,
                            /*wait_ms*/ Some(0),
                        )
                        .await
                    {
                        Ok(response) => response,
                        Err(err) => {
                            let state = state_tx.borrow().clone();
                            let _ = state_tx.send_replace(state.failed(err.to_string()));
                            output_closed.store(true, Ordering::Release);
                            output_closed_notify.notify_waiters();
                            cancellation_token.cancel();
                            break;
                        }
                    };
                    let ExecReadResponse {
                        chunks,
                        next_seq,
                        exited,
                        exit_code,
                        closed,
                        failure,
                    } = response;
                    for chunk in chunks.into_iter().filter(|chunk| chunk.seq > last_seq) {
                        let bytes = chunk.chunk.into_inner();
                        let mut guard = output_buffer.lock().await;
                        guard.push_chunk(&bytes);
                        drop(guard);
                        let _ = output_tx.send(bytes);
                        output_notify.notify_waiters();
                    }
                    last_seq = last_seq.max(next_seq.saturating_sub(1));
                    if let Some(message) = failure {
                        let state = state_tx.borrow().clone();
                        let _ = state_tx.send_replace(state.failed(message));
                        output_closed.store(true, Ordering::Release);
                        output_closed_notify.notify_waiters();
                        cancellation_token.cancel();
                        break;
                    }
                    if exited {
                        let state = state_tx.borrow().clone();
                        let _ = state_tx.send_replace(state.exited(exit_code));
                    }
                    // Descendants may retain inherited output pipes; drain a bounded tail after exit.
                    if exited {
                        cancellation_token.cancel();
                    }
                    if closed {
                        output_closed.store(true, Ordering::Release);
                        output_closed_notify.notify_waiters();
                        cancellation_token.cancel();
                        break;
                    }
                    continue;
                }

                let Some(event) = event else {
                    continue;
                };
                match event {
                    ExecProcessEvent::Output(chunk) => {
                        if chunk.seq <= last_seq {
                            continue;
                        }
                        last_seq = chunk.seq;
                        let bytes = chunk.chunk.into_inner();
                        let mut guard = output_buffer.lock().await;
                        guard.push_chunk(&bytes);
                        drop(guard);
                        let _ = output_tx.send(bytes);
                        output_notify.notify_waiters();
                    }
                    ExecProcessEvent::Exited { seq, exit_code } => {
                        if seq <= last_seq {
                            continue;
                        }
                        last_seq = seq;
                        let state = state_tx.borrow().clone();
                        let _ = state_tx.send_replace(state.exited(Some(exit_code)));
                        cancellation_token.cancel();
                    }
                    ExecProcessEvent::Closed { seq } => {
                        if seq <= last_seq {
                            continue;
                        }
                        output_closed.store(true, Ordering::Release);
                        output_closed_notify.notify_waiters();
                        cancellation_token.cancel();
                        break;
                    }
                }
            }
        })
    }

    fn spawn_local_output_task(
        mut receiver: tokio::sync::broadcast::Receiver<Vec<u8>>,
        output_handles: OutputHandles,
        output_tx: broadcast::Sender<Vec<u8>>,
    ) -> JoinHandle<()> {
        let OutputHandles {
            output_buffer,
            output_notify,
            output_closed,
            output_closed_notify,
            ..
        } = output_handles;
        tokio::spawn(async move {
            let _output_task_guard = OutputTaskGuard {
                output_closed: Arc::clone(&output_closed),
                output_closed_notify: Arc::clone(&output_closed_notify),
            };
            loop {
                match receiver.recv().await {
                    Ok(chunk) => {
                        let mut guard = output_buffer.lock().await;
                        guard.push_chunk(&chunk);
                        drop(guard);
                        let _ = output_tx.send(chunk);
                        output_notify.notify_waiters();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        output_closed.store(true, Ordering::Release);
                        output_closed_notify.notify_waiters();
                        break;
                    }
                };
            }
        })
    }

    fn signal_exit(&self, exit_code: Option<i32>) {
        let state = self.state_rx.borrow().clone();
        let _ = self.state_tx.send_replace(state.exited(exit_code));
        self.output.cancellation_token.cancel();
    }
}

impl Drop for UnifiedExecProcess {
    fn drop(&mut self) {
        self.terminate();
    }
}
