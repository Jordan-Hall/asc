//! Counting the bytes a response sends, as they are sent.
//!
//! [`Metered`] wraps a response stream. Every item polled out of it is added to a pending count;
//! the count is flushed to the usage hub every [`FLUSH_BYTES`], and the rest is flushed when the
//! stream is dropped, whether it ended, failed or was cut off by the client. A download that is
//! abandoned after `k` bytes records about `k` bytes, a finished one records everything once.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::Stream;

use super::UsageHub;

/// Pending bytes at which a long stream reports without waiting for its end.
pub const FLUSH_BYTES: u64 = 8 * 1024 * 1024;

/// A stream whose items are counted against a repository's bandwidth.
pub struct Metered<S, F> {
    inner: S,
    size: F,
    hub: Arc<UsageHub>,
    repo: String,
    pending: u64,
    flush_at: u64,
}

impl<S, F> Metered<S, F> {
    /// `size` says how many bytes an item is on the wire.
    pub fn new(inner: S, hub: Arc<UsageHub>, repo: String, size: F) -> Self {
        Self::with_flush(inner, hub, repo, size, FLUSH_BYTES)
    }

    pub fn with_flush(inner: S, hub: Arc<UsageHub>, repo: String, size: F, flush_at: u64) -> Self {
        Self {
            inner,
            size,
            hub,
            repo,
            pending: 0,
            flush_at: flush_at.max(1),
        }
    }

    fn flush(&mut self) {
        let n = std::mem::take(&mut self.pending);
        if n == 0 {
            return;
        }
        // Outside a runtime (a stream dropped at shutdown) there is nowhere to write; the loss is
        // at most one flush interval.
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let (hub, repo) = (self.hub.clone(), self.repo.clone());
            rt.spawn(async move { hub.add_bandwidth(&repo, n).await });
        }
    }
}

impl<S, F> Stream for Metered<S, F>
where
    S: Stream + Unpin,
    F: Fn(&S::Item) -> u64 + Unpin,
{
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<S::Item>> {
        let this = &mut *self;
        let polled = Pin::new(&mut this.inner).poll_next(cx);
        if let Poll::Ready(Some(item)) = &polled {
            this.pending = this.pending.saturating_add((this.size)(item));
            if this.pending >= this.flush_at {
                this.flush();
            }
        }
        polled
    }
}

impl<S, F> Drop for Metered<S, F> {
    fn drop(&mut self) {
        self.flush();
    }
}
