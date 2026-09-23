//! Bounded local repetition. No model or application-specific logic lives here.
use crate::core::{
    ActionOutcome, Adapter, AdapterError, CancellationToken, Event, ExecutionControl, FailureStage,
    SkillContext, ToolCall, ToolResult,
};
use crate::execution::{ExecutionContext, record_failure};
use serde::Serialize;
use std::time::{Duration, Instant};

pub const MAX_REPEAT: u32 = 100;
/// A bounded number of executions of the same tool call.
pub use adapter_api::RepeatRequest;
/// Local repetition stops at a count or on explicit adapter completion.
#[derive(Debug, Clone, Serialize)]
pub enum BatchRequest {
    Repeat(RepeatRequest),
    UntilDone(ToolCall),
}
impl BatchRequest {
    pub fn call(&self) -> &ToolCall {
        match self {
            Self::Repeat(request) => &request.call,
            Self::UntilDone(call) => call,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchStatus {
    Running,
    Interrupted,
    Completed,
    Cancelled,
    TimedOut,
    AttemptLimit,
    Failed,
}
#[derive(Debug, Clone, Serialize)]
pub struct BatchProgress {
    pub then: crate::core::Continuation,
    pub request: BatchRequest,
    pub completed: u32,
    pub successful: u32,
    pub attempts: u32,
    /// Unknown for an until-done loop; never invent a remaining work count.
    pub remaining: Option<u32>,
    /// Adapter completion or requested count reached, even if observation then fails.
    pub finished: bool,
    pub status: BatchStatus,
    pub message: String,
}
pub(crate) struct ActiveBatch {
    pub progress: BatchProgress,
    pub control: ExecutionControl,
    pub max_attempts: u32,
    runtime: crate::task_runtime::TaskHandle,
}
impl ActiveBatch {
    pub fn new(
        request: BatchRequest,
        then: crate::core::Continuation,
        timeout: Duration,
        max_attempts: u32,
        cancellation: CancellationToken,
        runtime: crate::task_runtime::TaskHandle,
    ) -> Self {
        Self {
            runtime,
            progress: BatchProgress {
                then,
                remaining: match &request {
                    BatchRequest::Repeat(request) => Some(request.times),
                    BatchRequest::UntilDone(_) => None,
                },
                finished: false,
                request,
                completed: 0,
                successful: 0,
                attempts: 0,
                status: BatchStatus::Running,
                message: String::new(),
            },
            control: ExecutionControl {
                cancellation,
                deadline: Instant::now().checked_add(timeout),
            },
            max_attempts,
        }
    }
    fn stop(&mut self, status: BatchStatus, message: String, emit: &mut impl FnMut(Event)) {
        self.progress.status = status;
        self.progress.message = message;
        emit(Event::BatchProgress(self.progress.clone()));
    }
    pub fn check_limit(&mut self, emit: &mut impl FnMut(Event)) -> bool {
        let stop = match self.control.check() {
            Err(AdapterError::Cancelled) => {
                Some((BatchStatus::Cancelled, "cancelled by user".into()))
            }
            Err(_) => Some((BatchStatus::TimedOut, "batch deadline exceeded".into())),
            Ok(()) if self.progress.attempts >= self.max_attempts && !self.progress.finished => {
                Some((
                    BatchStatus::AttemptLimit,
                    "batch attempt limit reached".into(),
                ))
            }
            _ => None,
        };
        if let Some((status, message)) = stop {
            self.stop(status, message, emit);
            true
        } else {
            false
        }
    }
    /// Re-observe and resolve skills for every iteration, including resumption after exceptions.
    pub async fn drive<G: Adapter, F: FnMut(Event)>(
        &mut self,
        ctx: &mut ExecutionContext<'_, G, F>,
        step: usize,
    ) {
        let ExecutionContext {
            task,
            environment,
            state,
            previous,
            failure,
            interruption,
            failures,
            options,
            emit,
            ..
        } = ctx;
        self.progress.status = BatchStatus::Running;
        emit(Event::BatchProgress(self.progress.clone()));
        while !self.progress.finished {
            tokio::task::yield_now().await;
            if let Err(error) = self.runtime.checkpoint(self.control.deadline).await {
                self.stop(BatchStatus::Interrupted, error.to_string(), emit);
                return;
            }
            if self.check_limit(emit) {
                return;
            }
            match environment.observe() {
                Ok(observed) => {
                    *state = observed;
                    if failure
                        .as_ref()
                        .is_some_and(|f| f.stage == FailureStage::Observation)
                    {
                        *failure = None;
                    }
                }
                Err(error) => {
                    record_failure(
                        failure,
                        FailureStage::Observation,
                        None,
                        format!("observation failed: {error}"),
                        emit,
                    );
                    self.stop(
                        BatchStatus::Interrupted,
                        format!("observation failed: {error}"),
                        emit,
                    );
                    return;
                }
            }
            let skills = match environment.decision_context(&SkillContext {
                state,
                task_goal: &task.goal,
                decision_step: step,
                previous_result: previous.as_ref(),
                failure: failure.as_ref(),
                interruption: interruption.as_ref(),
            }) {
                Ok(context) => context.skills,
                Err(error) => {
                    record_failure(
                        failure,
                        FailureStage::Context,
                        Some(self.progress.request.call().clone()),
                        error.to_string(),
                        emit,
                    );
                    self.stop(BatchStatus::Interrupted, error.to_string(), emit);
                    return;
                }
            };
            let skill = skills
                .iter()
                .find(|skill| skill.name == self.progress.request.call().name);
            match skill {
                Some(skill)
                    if skill.availability.is_available()
                        && match self.progress.request {
                            BatchRequest::Repeat(_) => skill.repeatable,
                            BatchRequest::UntilDone(_) => skill.loopable,
                        } => {}
                Some(skill) => {
                    record_failure(
                        failure,
                        FailureStage::Availability,
                        Some(self.progress.request.call().clone()),
                        format!("repeat capability unavailable: {:?}", skill.availability),
                        emit,
                    );
                    self.stop(
                        BatchStatus::Interrupted,
                        format!("repeat capability unavailable: {:?}", skill.availability),
                        emit,
                    );
                    return;
                }
                None => {
                    record_failure(
                        failure,
                        FailureStage::Availability,
                        Some(self.progress.request.call().clone()),
                        "batch skill no longer exists".into(),
                        emit,
                    );
                    self.stop(
                        BatchStatus::Interrupted,
                        "batch skill no longer exists".into(),
                        emit,
                    );
                    return;
                }
            }
            if let Err(error) = self.runtime.check_log() {
                self.stop(BatchStatus::Interrupted, error.to_string(), emit);
                return;
            }
            self.progress.attempts += 1;
            let result = environment
                .execute(self.progress.request.call(), &self.control)
                .await;
            let stop_reason = failures.action(
                self.progress.request.call(),
                &result,
                options.max_repeated_failures,
            );
            let mut stop_status = BatchStatus::Interrupted;
            let result = match result {
                Ok(report) => {
                    let step = match report.outcome {
                        ActionOutcome::Completed { successful } => Some((successful, true)),
                        ActionOutcome::Continue { successful }
                            if matches!(self.progress.request, BatchRequest::UntilDone(_)) =>
                        {
                            Some((successful, false))
                        }
                        _ => None,
                    };
                    if let Some((successful, done)) = step {
                        self.progress.completed += 1;
                        if let Some(remaining) = &mut self.progress.remaining {
                            *remaining -= 1;
                            self.progress.finished = *remaining == 0;
                        } else {
                            self.progress.finished = done;
                        }
                        self.progress.successful += u32::from(successful);
                    }
                    ToolResult {
                        call: self.progress.request.call().clone(),
                        success: step.is_some(),
                        message: if step.is_none()
                            && matches!(report.outcome, ActionOutcome::Continue { .. })
                        {
                            "Continue requires UntilDone; a fixed repeat cannot confirm this action as completed".into()
                        } else {
                            report.message
                        },
                        outcome: Some(report.outcome),
                    }
                }
                Err(error) => {
                    stop_status = match error {
                        AdapterError::Cancelled => BatchStatus::Cancelled,
                        AdapterError::TimedOut => BatchStatus::TimedOut,
                        _ => BatchStatus::Interrupted,
                    };
                    ToolResult {
                        call: self.progress.request.call().clone(),
                        success: false,
                        message: error.to_string(),
                        outcome: None,
                    }
                }
            };
            let failed = !result.success;
            let message = result.message.clone();
            *failure = None;
            if failed {
                record_failure(
                    failure,
                    FailureStage::Execution,
                    Some(result.call.clone()),
                    message.clone(),
                    emit,
                );
            }
            *previous = Some(result);
            if let Some(reason) = stop_reason {
                if self.check_limit(emit) {
                    return;
                }
                self.stop(BatchStatus::Failed, reason, emit);
                return;
            }
            // Count a confirmed action even if the following observation fails.
            match environment.observe() {
                Ok(observed) => *state = observed,
                Err(error) => {
                    record_failure(
                        failure,
                        FailureStage::Observation,
                        Some(self.progress.request.call().clone()),
                        format!("post-action observation failed: {error}"),
                        emit,
                    );
                    self.stop(
                        BatchStatus::Interrupted,
                        format!("post-action observation failed: {error}"),
                        emit,
                    );
                    return;
                }
            }
            if failed {
                self.stop(stop_status, message, emit);
                return;
            }
            if self.check_limit(emit) {
                return;
            }
            self.progress.message = message;
            emit(Event::BatchProgress(self.progress.clone()));
        }
        self.stop(
            BatchStatus::Completed,
            "local execution completed".into(),
            emit,
        );
    }
}
