//! Client-local catalog revisions and synchronization for MCP calls.
//!
//! Only active readers, calls, and frozen model bindings pin earlier snapshots.
//! Explicit refreshes publish after calls using this client's current revision finish.

use std::future::Future;
use std::sync::Arc;

use tokio::sync::RwLock;
use tokio::sync::RwLockReadGuard;

use crate::tools::ToolInfo;

pub(crate) struct ClientToolCatalog {
    current: RwLock<CatalogState>,
}

struct CatalogState {
    revision: u64,
    /// The catalog revision installed by this client's last explicit refresh.
    last_refresh_revision: u64,
    tools: Arc<[ToolInfo]>,
}

pub(crate) struct ToolCatalogSnapshot {
    pub(crate) revision: u64,
    pub(crate) tools: Arc<[ToolInfo]>,
}

impl ClientToolCatalog {
    pub(crate) fn new(tools: impl Into<Arc<[ToolInfo]>>) -> Self {
        Self {
            current: RwLock::new(CatalogState {
                revision: 0,
                last_refresh_revision: 0,
                tools: tools.into(),
            }),
        }
    }

    pub(crate) async fn read<R>(&self, read: impl FnOnce(ToolCatalogSnapshot) -> R) -> R {
        let (_current, snapshot) = self.read_current().await;
        read(snapshot)
    }

    /// Captures tools and their client-local revision together, without retaining them in the client.
    async fn read_current(&self) -> (RwLockReadGuard<'_, CatalogState>, ToolCatalogSnapshot) {
        let current = self.current.read().await;
        let snapshot = ToolCatalogSnapshot {
            revision: current.revision,
            tools: Arc::clone(&current.tools),
        };
        (current, snapshot)
    }

    /// Reject stale calls before preparation and hold catalog authority until execution finishes.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "catalog publication must wait for call preparation and execution"
    )]
    pub(crate) async fn run_with_snapshot<R, F, Fut>(
        &self,
        expected: &ToolCatalogSnapshot,
        run: F,
    ) -> Option<R>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = R>,
    {
        let (current, snapshot) = self.read_current().await;
        if current.last_refresh_revision > expected.revision
            || (snapshot.revision != expected.revision
                && !catalogs_match(&snapshot.tools, &expected.tools))
        {
            return None;
        }
        let result = run().await;
        drop(current);
        Some(result)
    }
}

/// Compares complete definitions independently of tool-list order. Stable sorting keeps
/// conflicting duplicate identities in their original order, since deduplication can pick the
/// first definition. This slow path runs only after the client's revision has changed.
fn catalogs_match(left: &[ToolInfo], right: &[ToolInfo]) -> bool {
    if left == right {
        return true;
    }
    if left.len() != right.len() {
        return false;
    }
    let mut left = left.iter().collect::<Vec<_>>();
    let mut right = right.iter().collect::<Vec<_>>();
    for tools in [&mut left, &mut right] {
        tools.sort_by_key(|tool| {
            (
                tool.server_name.as_str(),
                tool.tool.name.as_ref(),
                tool.callable_namespace.as_str(),
                tool.callable_name.as_str(),
            )
        });
    }
    left == right
}

/// A binding cache key includes client identity, since new clients start at zero.
#[derive(Clone)]
pub(crate) struct ClientToolCatalogRevision {
    pub(crate) catalog: Arc<ClientToolCatalog>,
    pub(crate) revision: u64,
}

impl PartialEq for ClientToolCatalogRevision {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.catalog, &other.catalog) && self.revision == other.revision
    }
}

impl Eq for ClientToolCatalogRevision {}
