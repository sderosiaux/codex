mod backoff;

pub use backoff::backoff;

use std::future::Future;
use tokio_util::sync::CancellationToken;

/// Stack budget for threads that poll Codex async work.
pub const THREAD_STACK_SIZE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum CancelErr {
    Cancelled,
}

pub trait OrCancelExt: Sized {
    type Output;

    fn or_cancel(
        self,
        token: &CancellationToken,
    ) -> impl Future<Output = Result<Self::Output, CancelErr>> + Send;
}

impl<F> OrCancelExt for F
where
    F: Future + Send,
    F::Output: Send,
{
    type Output = F::Output;

    async fn or_cancel(self, token: &CancellationToken) -> Result<Self::Output, CancelErr> {
        tokio::select! {
            _ = token.cancelled() => Err(CancelErr::Cancelled),
            res = self => Ok(res),
        }
    }
}
