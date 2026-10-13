//! Context and error types shared by tool runtimes.

use crate::session::session::Session;
use crate::session::step_context::StepContext;
use std::sync::Arc;

pub(crate) struct ToolCtx {
    pub session: Arc<Session>,
    pub step_context: Arc<StepContext>,
    pub call_id: String,
}

#[derive(Debug)]
pub(crate) enum ToolError {
    Rejected(String),
}
