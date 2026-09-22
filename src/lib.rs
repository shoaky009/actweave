//! Minimal Observe → Decide → Act loop shared by CLI and future GUI clients.
pub use demo_adapter as adapter;
pub mod core;
pub mod jev;
pub use demo_adapter::runtime;

mod diagnostics;
mod failure_guard;
pub mod manual;

pub mod skills;

pub mod batch;
pub mod execution;
pub mod metrics;
pub mod task_runtime;
