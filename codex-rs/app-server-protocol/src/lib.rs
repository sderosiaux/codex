//! Thread-history projection types shared by the thread store and core.

mod experimental_api;
mod protocol;

pub use experimental_api::*;
pub use protocol::item_builders::*;
pub use protocol::thread_history::*;
pub use protocol::thread_history_projection::*;
pub use protocol::v2::*;

#[cfg(not(test))]
pub(crate) use codex_app_server_protocol_noop_macros::JsonSchema;
#[cfg(not(test))]
pub(crate) use codex_app_server_protocol_noop_macros::TS;
#[cfg(test)]
pub(crate) use schemars::JsonSchema;
#[cfg(test)]
pub(crate) use ts_rs::TS;
