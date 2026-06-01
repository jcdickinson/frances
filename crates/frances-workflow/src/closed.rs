//! The shared "workflow is shutting down" signal.
//!
//! [`WorkflowClosed`] bundles the flag and the wakeup so they can't
//! drift and the await-until-closed logic lives in exactly one place.

use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

/// Set-once shutdown flag paired with its wakeup. Held as one
/// `Arc<WorkflowClosed>` and cloned to every host primitive that needs
/// to observe shutdown.
#[derive(Debug, Default)]
pub struct WorkflowClosed {
    flag: AtomicBool,
    notify: Notify,
}

impl WorkflowClosed {
    /// Mark the workflow closed and wake every waiter. Returns `true` if
    /// this call performed the transition, `false` if it was already
    /// closed — so the runtime can avoid running teardown twice.
    pub fn close(&self) -> bool {
        let first = !self.flag.swap(true, Ordering::AcqRel);
        if first {
            self.notify.notify_waiters();
        }
        first
    }

    /// Whether shutdown has begun.
    pub fn is_closed(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// Resolve once the workflow is closed, returning immediately if it
    /// already is. Registers the wakeup *before* re-reading the flag so
    /// a [`close`](Self::close) racing this call is observed, not lost.
    pub async fn closed(&self) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_closed() {
            return;
        }
        notified.await;
    }
}
