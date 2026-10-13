use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

mod host;

use crate::HostSkillsSnapshot;

use crate::catalog::SkillAuthority;
use crate::catalog::SkillCatalog;
use crate::catalog::SkillProviderResult;
use crate::catalog::SkillReadResult;
use crate::catalog::SkillResourceId;

pub use host::HostSkillProvider;

#[derive(Clone, Debug)]
pub struct SkillListQuery {
    pub turn_id: String,
    pub host_snapshot: Option<Arc<HostSkillsSnapshot>>,
    pub include_host_skills: bool,
}

/// A skill resource read against the turn's host skills snapshot.
#[derive(Clone)]
pub struct SkillReadRequest {
    pub authority: SkillAuthority,
    pub resource: SkillResourceId,
    pub host_snapshot: Option<Arc<HostSkillsSnapshot>>,
}

pub type SkillProviderFuture<'a, T> =
    Pin<Box<dyn Future<Output = SkillProviderResult<T>> + Send + 'a>>;

/// Source-specific skill catalog and resource access.
///
/// Implementations must preserve authority boundaries: a resource listed by a
/// provider must be read or searched through the same provider/authority rather
/// than converted into an ambient local path.
pub trait SkillProvider: Send + Sync {
    fn list(&self, query: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog>;

    fn read(&self, request: SkillReadRequest) -> SkillProviderFuture<'_, SkillReadResult>;
}
