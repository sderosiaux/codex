use std::path::PathBuf;
use std::sync::Arc;

use codex_history::RolloutItem;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSource;
use codex_utils_absolute_path::AbsolutePathBuf;

/// Parameters required to register a thread.
#[derive(Clone, Debug)]
pub struct CreateThreadParams {
    pub session_id: SessionId,
    pub thread_id: ThreadId,
    /// Source thread id when this thread is a fork.
    pub forked_from_id: Option<ThreadId>,
    /// Parent thread id when this thread is a sub-agent.
    pub parent_thread_id: Option<ThreadId>,
    pub source: SessionSource,
    pub thread_source: Option<ThreadSource>,
    pub originator: String,
    pub base_instructions: BaseInstructions,
    pub multi_agent_version: Option<MultiAgentVersion>,
    pub history_mode: ThreadHistoryMode,
    /// First item ordinal owned by a sub-agent whose inherited prefix was copied in.
    pub subagent_history_start_ordinal: Option<u64>,
    pub initial_window_id: String,
    pub runtime_workspace_roots: Option<Vec<AbsolutePathBuf>>,
    pub cwd: PathBuf,
    pub model_provider: String,
}

/// Parameters for reopening a thread from history already held by the caller or the store.
#[derive(Clone, Debug)]
pub struct ResumeThreadParams {
    pub thread_id: ThreadId,
    /// Replay history to install when the store does not hold the thread yet.
    pub history: Option<Arc<Vec<RolloutItem>>>,
}

/// Replayable rollout items for a thread.
#[derive(Clone, Debug)]
pub struct StoredThreadHistory {
    pub thread_id: ThreadId,
    pub items: Vec<RolloutItem>,
}

/// Thread summary, with history only when requested.
#[derive(Clone, Debug)]
pub struct StoredThread {
    pub thread_id: ThreadId,
    pub forked_from_id: Option<ThreadId>,
    pub parent_thread_id: Option<ThreadId>,
    pub source: SessionSource,
    pub thread_source: Option<ThreadSource>,
    pub history_mode: ThreadHistoryMode,
    pub cwd: PathBuf,
    pub model_provider: String,
    /// Model of the latest recorded turn, if any.
    pub model: Option<String>,
    /// Reasoning effort of the latest recorded turn, if any.
    pub reasoning_effort: Option<ReasoningEffort>,
    pub agent_nickname: Option<String>,
    pub agent_role: Option<String>,
    pub agent_path: Option<String>,
    pub history: Option<StoredThreadHistory>,
}
