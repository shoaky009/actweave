//! Local visual automation. Independent of adapters, decision providers and task cores.
pub mod action;
pub mod control;
pub mod flow;
pub mod recognition;

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, Ordering},
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
    #[error("input cleanup failed: {0}")]
    Cleanup(String),
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
    pause_state: AtomicU8,
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
    pub fn pause(&self) {
        if self
            .0
            .pause_state
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.0.notify.notify_waiters();
        }
    }
    pub fn resume(&self) {
        self.0.pause_state.store(0, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }
    pub fn is_pause_requested(&self) -> bool {
        self.0.pause_state.load(Ordering::SeqCst) != 0
    }
    pub fn acknowledge_pause(&self) {
        if self
            .0
            .pause_state
            .compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.0.notify.notify_waiters();
        }
    }
    pub async fn pause_requested(&self) {
        self.wait_until(|| self.is_pause_requested()).await;
    }
    pub async fn paused(&self) {
        self.wait_until(|| self.0.pause_state.load(Ordering::SeqCst) == 2)
            .await;
    }
    pub async fn resumed(&self) {
        self.wait_until(|| !self.is_pause_requested()).await;
    }
    pub async fn cancelled(&self) {
        self.wait_until(|| self.check().is_err()).await;
    }
    async fn wait_until(&self, ready: impl Fn() -> bool) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if ready() {
                return;
            }
            notified.await;
        }
    }
}
