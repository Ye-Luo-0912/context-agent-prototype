use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Notify;

/// Cooperative cancellation token shared between the kernel and a model
/// provider. Cloning is cheap; any clone can cancel all clones.
///
/// Not serializable on purpose: it is a runtime handle, never part of a
/// journaled event or a checkpoint.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the token cancelled and wake every waiter.
    pub fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::AcqRel) {
            self.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Resolves once the token is cancelled (immediately if already so).
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let notified = self.notify.notified();
        // Re-check after registering to close the lost-wakeup race.
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}
