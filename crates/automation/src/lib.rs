//! Local visual automation. Independent of adapters, decision providers and task cores.
pub mod action;
pub mod flow;
pub mod recognition;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Notify;

/// Operational errors are distinct from a successful recognition with no matches.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid automation configuration: {0}")]
    Invalid(String),
    #[error("unsupported capability: {0}")]
    Unsupported(String),
    #[error("backend failed: {0}")]
    Backend(String),
    #[error("cancelled")]
    Cancelled,
    #[error("node timed out")]
    TimedOut,
}

/// Shared cancellation for capture, recognition and input backends.
#[derive(Clone, Default)]
pub struct Control(Arc<Signal>);
#[derive(Default)]
struct Signal {
    cancelled: AtomicBool,
    notify: Notify,
}
impl Control {
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }
    pub fn check(&self) -> Result<(), Error> {
        if self.0.cancelled.load(Ordering::SeqCst) {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.check().is_err() {
                return;
            }
            notified.await;
        }
    }
}
