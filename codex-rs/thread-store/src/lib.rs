//! Process-lifetime thread history.
//!
//! Threads are never written to disk. The store keeps each thread's replayable rollout items in
//! memory so sub-agents can fork from a parent and closed agents can be resumed within the same
//! process.

mod error;
mod live_thread;
mod policy;
mod store;
mod types;

pub use error::ThreadStoreError;
pub use error::ThreadStoreResult;
pub use live_thread::LiveThread;
pub use live_thread::LiveThreadInitGuard;
pub use policy::into_persisted_rollout_items;
pub use policy::persisted_rollout_items;
pub use policy::should_persist_response_item;
pub use store::ThreadStore;
pub use types::CreateThreadParams;
pub use types::ResumeThreadParams;
pub use types::StoredThread;
pub use types::StoredThreadHistory;
