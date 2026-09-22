//! Task-scoped counts at actual request/call boundaries, independent of diagnostics.
use adapter_api::{
    ActionOutcome, ActionReport, Adapter, AdapterError, AppState, DecisionContext,
    ExecutionControl, SkillContext, ToolCall,
};
use serde::Serialize;
use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CallCounts {
    pub total: u64,
    pub succeeded: u64,
    /// Includes errors, interruptions, and calls dropped before a normal result.
    pub failed: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TaskSummary {
    pub model_requests: CallCounts,
    pub actions: CallCounts,
    pub interruptions: u64,
    /// Wall-clock runtime, including waits and pauses, excluding time before run.
    pub elapsed_ms: u64,
}

#[derive(Default)]
struct MetricsState {
    summary: TaskSummary,
    started: Option<Instant>,
    finished: bool,
}

/// Each task owns a fresh instance. Model providers instrument each actual request,
/// including retries, rather than counting decision rounds or diagnostic records.
#[derive(Clone, Default)]
pub struct TaskMetrics(Arc<Mutex<MetricsState>>);
impl TaskMetrics {
    fn state(&self) -> MutexGuard<'_, MetricsState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub(crate) fn start(&self) {
        self.state().started = Some(Instant::now());
    }
    pub(crate) fn finish(&self) -> TaskSummary {
        let mut state = self.state();
        if !state.finished {
            state.summary.elapsed_ms = state.started.map_or(0, |start| {
                start.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
            });
            state.finished = true;
        }
        state.summary
    }
    pub(crate) fn interrupted(&self) {
        self.state().summary.interruptions += 1;
    }
    /// Start immediately before sending a model request. Mark success only after a
    /// usable response has been validated. Dropping the guard records failure.
    pub fn model_request(&self) -> CallGuard {
        self.begin(CallKind::Model)
    }
    fn begin(&self, kind: CallKind) -> CallGuard {
        kind.counts(&mut self.state().summary).total += 1;
        CallGuard {
            metrics: self.clone(),
            kind,
            succeeded: false,
        }
    }
}

#[derive(Clone, Copy)]
enum CallKind {
    Model,
    Action,
}
impl CallKind {
    fn counts(self, summary: &mut TaskSummary) -> &mut CallCounts {
        match self {
            Self::Model => &mut summary.model_requests,
            Self::Action => &mut summary.actions,
        }
    }
}
#[must_use = "retain until the request completes; dropping records an interrupted or failed call"]
pub struct CallGuard {
    metrics: TaskMetrics,
    kind: CallKind,
    succeeded: bool,
}
impl CallGuard {
    pub fn success(mut self) {
        self.succeeded = true;
    }
}
impl Drop for CallGuard {
    fn drop(&mut self) {
        let mut state = self.metrics.state();
        let counts = self.kind.counts(&mut state.summary);
        if self.succeeded {
            counts.succeeded += 1;
        } else {
            counts.failed += 1;
        }
    }
}

/// Instrument outside the adapter so platform integrations remain unaware of Core.
pub(crate) struct MeasuredAdapter<'a, G> {
    pub inner: &'a mut G,
    pub metrics: TaskMetrics,
}
impl<G: Adapter> Adapter for MeasuredAdapter<'_, G> {
    fn observe(&self) -> Result<AppState, AdapterError> {
        self.inner.observe()
    }
    fn decision_context(
        &self,
        context: &SkillContext<'_>,
    ) -> Result<DecisionContext, AdapterError> {
        self.inner.decision_context(context)
    }
    async fn execute(
        &mut self,
        call: &ToolCall,
        control: &ExecutionControl,
    ) -> Result<ActionReport, AdapterError> {
        let guard = self.metrics.begin(CallKind::Action);
        let result = self.inner.execute(call, control).await;
        if matches!(
            &result,
            Ok(ActionReport {
                outcome: ActionOutcome::Completed { .. } | ActionOutcome::Continue { .. },
                ..
            })
        ) {
            guard.success();
        }
        result
    }
}
