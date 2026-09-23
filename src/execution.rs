//! Ordered local execution. Only adapters interpret state and action preconditions.
use crate::{batch::ActiveBatch, core::*, task_runtime::TaskHandle};
use serde::Serialize;

pub const MAX_ACTIONS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    Running,
    Interrupted,
    Completed,
    Cancelled,
    Failed,
}

/// `completed` is the number of confirmed actions; it also indexes the next action.
#[derive(Debug, Clone, Serialize)]
pub struct ExecutionProgress {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<ExecutionFailure>,
    pub actions: Vec<Action>,
    pub then: Continuation,
    pub completed: usize,
    pub status: ExecutionStatus,
    pub message: String,
}
pub(crate) struct ActiveExecution {
    pub progress: ExecutionProgress,
    pub batch: Option<ActiveBatch>,
    replaced: bool,
}
pub(crate) struct ExecutionContext<'a, G, F> {
    pub failures: crate::failure_guard::FailureGuard,
    pub task: &'a Task,
    pub environment: &'a mut G,
    pub options: &'a RunOptions,
    pub runtime: &'a TaskHandle,
    pub state: AppState,
    pub previous: Option<ToolResult>,
    pub failure: Option<ExecutionFailure>,
    pub interruption: Option<ExecutionFailure>,
    pub emit: F,
}
pub(crate) fn record_failure(
    target: &mut Option<ExecutionFailure>,
    stage: FailureStage,
    call: Option<ToolCall>,
    message: String,
    emit: &mut impl FnMut(Event),
) {
    let failure = ExecutionFailure {
        stage,
        call,
        message,
    };
    emit(Event::ExecutionFailed(failure.clone()));
    *target = Some(failure);
}
impl<G: Adapter, F: FnMut(Event)> ExecutionContext<'_, G, F> {
    pub fn observe(&mut self) -> Result<(), Error> {
        self.observe_after(None)
    }
    pub fn observe_after(&mut self, call: Option<&ToolCall>) -> Result<(), Error> {
        self.state = match self.environment.observe() {
            Ok(state) => state,
            Err(error) => {
                self.fail(
                    FailureStage::Observation,
                    call.cloned(),
                    format!(
                        "{}observation failed: {error}",
                        if call.is_some() { "post-action " } else { "" }
                    ),
                );
                return Err(error.into());
            }
        };
        if self
            .failure
            .as_ref()
            .is_some_and(|f| f.stage == FailureStage::Observation)
        {
            self.failure = None;
        }
        (self.emit)(Event::Observed(self.state.clone()));
        Ok(())
    }
    pub fn fail(&mut self, stage: FailureStage, call: Option<ToolCall>, message: String) {
        record_failure(&mut self.failure, stage, call, message, &mut self.emit);
    }
    pub fn decision_context(&mut self, step: usize) -> Result<DecisionContext, Error> {
        let context = self.environment.decision_context(&SkillContext {
            state: &self.state,
            task_goal: &self.task.goal,
            decision_step: step,
            previous_result: self.previous.as_ref(),
            failure: self.failure.as_ref(),
            interruption: self.interruption.as_ref(),
        });
        context.map_err(|error| {
            self.fail(FailureStage::Context, None, error.to_string());
            error.into()
        })
    }
}
impl ActiveExecution {
    pub fn replace_remaining(
        &mut self,
        mut actions: Vec<Action>,
        then: Continuation,
    ) -> Result<(), String> {
        if self.progress.status != ExecutionStatus::Interrupted {
            return Err("only interrupted executions can be replaced".into());
        }
        if let Some(batch) = &mut self.batch {
            if !batch.progress.finished {
                let valid = match (actions.first_mut(), &batch.progress.request) {
                    (Some(Action::Repeat(request)), BatchRequest::Repeat(original))
                        if request.call.name == original.call.name
                            && request.call.arguments == original.call.arguments
                            && Some(request.times) == batch.progress.remaining =>
                    {
                        *request = original.clone();
                        true
                    }
                    (Some(Action::UntilDone(call)), BatchRequest::UntilDone(original)) => {
                        call.name == original.name && call.arguments == original.arguments
                    }
                    _ => false,
                };
                if !valid {
                    return Err("replacement must first carry forward the same pending loop or repeat with exactly its remaining count; repair separately first".into());
                }
                // Retain the original ledger, deadline and attempt budget.
                batch.progress.then = then;
            } else if self.progress.completed < self.progress.actions.len() {
                return Err(
                    "batch completed but observation is pending; use Resume before replacing"
                        .into(),
                );
            }
        }
        self.progress.actions.truncate(self.progress.completed);
        self.progress.actions.extend(actions);
        self.progress.then = then;
        self.replaced = true;
        Ok(())
    }
    pub fn new(actions: Vec<Action>, then: Continuation) -> Self {
        Self {
            progress: ExecutionProgress {
                failure: None,
                actions,
                then,
                completed: 0,
                status: ExecutionStatus::Running,
                message: String::new(),
            },
            batch: None,
            replaced: false,
        }
    }
    pub fn pending_call(&self) -> Option<&ToolCall> {
        self.progress
            .actions
            .get(self.progress.completed)
            .map(Action::call)
    }
    pub fn deadline(&self) -> Option<std::time::Instant> {
        self.batch.as_ref().and_then(|b| b.control.deadline)
    }
    pub fn stop(
        &mut self,
        status: ExecutionStatus,
        message: impl Into<String>,
        emit: &mut impl FnMut(Event),
    ) {
        self.progress.status = status;
        self.progress.message = message.into();
        emit(Event::ExecutionProgress(self.progress.clone()));
    }
    pub fn check_limit(&mut self, emit: &mut impl FnMut(Event)) -> bool {
        if let Some(batch) = &mut self.batch
            && batch.check_limit(emit)
        {
            let status = if batch.progress.status == BatchStatus::Cancelled {
                ExecutionStatus::Cancelled
            } else {
                ExecutionStatus::Failed
            };
            let message = batch.progress.message.clone();
            self.stop(status, message, emit);
            true
        } else {
            false
        }
    }
    pub fn retained_on_error(&self) -> bool {
        self.replaced
            || self.progress.completed > 0
            || self.progress.actions.len() > 1
            || matches!(
                self.progress.actions.first(),
                Some(Action::Repeat(_) | Action::UntilDone(_))
            )
    }
    pub async fn drive<G: Adapter, F: FnMut(Event)>(
        &mut self,
        ctx: &mut ExecutionContext<'_, G, F>,
        step: usize,
        selected: &std::collections::BTreeSet<String>,
    ) -> Result<(), Error> {
        self.progress.failure = None;
        self.stop(ExecutionStatus::Running, "executing", &mut ctx.emit);
        // A confirmed final action may have been followed by an observation error.
        // Re-observe on resume without replaying that action.
        if self.progress.completed == self.progress.actions.len()
            && let Err(error) = ctx.observe()
        {
            self.progress.failure = ctx.failure.clone();
            self.stop(
                ExecutionStatus::Interrupted,
                error.to_string(),
                &mut ctx.emit,
            );
            return Ok(());
        }
        while self.progress.completed < self.progress.actions.len() {
            tokio::task::yield_now().await;
            ctx.runtime.checkpoint(self.deadline()).await?;
            if self.check_limit(&mut ctx.emit) {
                return Ok(());
            }
            if ctx.options.cancellation.is_cancelled() {
                self.stop(
                    ExecutionStatus::Cancelled,
                    "cancelled by user",
                    &mut ctx.emit,
                );
                return Ok(());
            }
            if let Err(error) = ctx.observe() {
                self.progress.failure = ctx.failure.clone();
                self.stop(
                    ExecutionStatus::Interrupted,
                    error.to_string(),
                    &mut ctx.emit,
                );
                return Ok(());
            }
            // A terminal report survives a failed post-action observation. Once a fresh
            // observation succeeds, advance without requiring or replaying that skill.
            if self
                .batch
                .as_ref()
                .is_some_and(|batch| batch.progress.finished)
            {
                if let Some(batch) = &mut self.batch {
                    batch.progress.status = BatchStatus::Completed;
                    (ctx.emit)(Event::BatchProgress(batch.progress.clone()));
                }
                self.progress.completed += 1;
                if self.progress.completed < self.progress.actions.len() {
                    self.batch = None;
                }
                (ctx.emit)(Event::ExecutionProgress(self.progress.clone()));
                ctx.failure = None;
                continue;
            }
            let skills = match ctx.decision_context(step) {
                Ok(context) => context.skills,
                Err(error) => {
                    self.progress.failure = ctx.failure.clone();
                    self.stop(
                        ExecutionStatus::Interrupted,
                        error.to_string(),
                        &mut ctx.emit,
                    );
                    return Ok(());
                }
            };
            let action = &self.progress.actions[self.progress.completed];
            let call = action.call().clone();
            let invalid = match skills.iter().find(|s| s.name == call.name) {
                None => Some("unknown skill"),
                Some(s) if !s.availability.is_available() => Some("skill unavailable"),
                Some(_)
                    if ctx.options.skill_mode == crate::skills::SkillMode::OnDemand
                        && !selected.contains(&call.name) =>
                {
                    Some("skill is not loaded; request LoadSkills first")
                }
                Some(s) if matches!(action, Action::Repeat(_)) && !s.repeatable => {
                    Some("skill is no longer repeatable")
                }
                Some(s) if matches!(action, Action::UntilDone(_)) && !s.loopable => {
                    Some("skill no longer supports local continuation")
                }
                _ => None,
            };
            if let Some(reason) = invalid {
                let message = format!(
                    "{reason}: {} ({:?})",
                    call.name,
                    skills
                        .iter()
                        .find(|s| s.name == call.name)
                        .map(|s| &s.availability)
                );
                let result = ToolResult {
                    call,
                    success: false,
                    outcome: None,
                    message: message.clone(),
                };
                ctx.fail(
                    FailureStage::Availability,
                    Some(result.call.clone()),
                    message.clone(),
                );
                self.progress.failure = ctx.failure.clone();
                (ctx.emit)(Event::Executed(result.clone()));
                ctx.previous = Some(result);
                self.stop(ExecutionStatus::Interrupted, message, &mut ctx.emit);
                return Ok(());
            }
            (ctx.emit)(Event::ActionStarted {
                index: self.progress.completed,
                total: self.progress.actions.len(),
                call: call.clone(),
            });
            ctx.runtime.check_log()?;
            match action {
                Action::Call(_) => {
                    let control = ExecutionControl {
                        cancellation: ctx.options.cancellation.clone(),
                        deadline: None,
                    };
                    let report = ctx.environment.execute(&call, &control).await;
                    let stop_reason =
                        ctx.failures
                            .action(&call, &report, ctx.options.max_repeated_failures);
                    let (result, status) = match report {
                        Ok(report) => (
                            ToolResult {
                                call,
                                success: matches!(report.outcome, ActionOutcome::Completed { .. }),
                                message: report.message,
                                outcome: Some(report.outcome),
                            },
                            ExecutionStatus::Interrupted,
                        ),
                        Err(error) => {
                            let status = match error {
                                AdapterError::Cancelled => ExecutionStatus::Cancelled,
                                AdapterError::TimedOut => ExecutionStatus::Failed,
                                _ => ExecutionStatus::Interrupted,
                            };
                            (
                                ToolResult {
                                    call,
                                    success: false,
                                    message: error.to_string(),
                                    outcome: None,
                                },
                                status,
                            )
                        }
                    };
                    if result.success {
                        self.progress.completed += 1;
                    }
                    let success = result.success;
                    let message = result.message.clone();
                    let completed_call = result.call.clone();
                    ctx.failure = None;
                    if !success {
                        ctx.fail(
                            FailureStage::Execution,
                            Some(completed_call.clone()),
                            message.clone(),
                        );
                    }
                    (ctx.emit)(Event::Executed(result.clone()));
                    ctx.previous = Some(result);
                    if let Some(reason) = stop_reason {
                        self.progress.failure = ctx.failure.clone();
                        if ctx.options.cancellation.is_cancelled() {
                            self.stop(
                                ExecutionStatus::Cancelled,
                                "cancelled by user",
                                &mut ctx.emit,
                            );
                        } else {
                            self.stop(ExecutionStatus::Failed, reason, &mut ctx.emit);
                        }
                        return Ok(());
                    }
                    if let Err(error) = ctx.observe_after(Some(&completed_call)) {
                        self.progress.failure = ctx.failure.clone();
                        self.stop(
                            ExecutionStatus::Interrupted,
                            error.to_string(),
                            &mut ctx.emit,
                        );
                        return Ok(());
                    }
                    if !success {
                        self.progress.failure = ctx.failure.clone();
                        self.stop(status, message, &mut ctx.emit);
                        return Ok(());
                    }
                }
                Action::Repeat(_) | Action::UntilDone(_) => {
                    if self.batch.is_none() {
                        let request = match action {
                            Action::Repeat(request) => BatchRequest::Repeat(request.clone()),
                            Action::UntilDone(call) => BatchRequest::UntilDone(call.clone()),
                            Action::Call(_) => unreachable!(),
                        };
                        self.batch = Some(ActiveBatch::new(
                            request,
                            self.progress.then,
                            ctx.options.batch_timeout,
                            ctx.options.max_batch_attempts,
                            ctx.options.cancellation.clone(),
                            ctx.runtime.clone(),
                        ));
                    }
                    if let Some(batch) = &mut self.batch {
                        batch.drive(ctx, step).await;
                        let status = match batch.progress.status {
                            BatchStatus::Completed => None,
                            BatchStatus::Cancelled => Some(ExecutionStatus::Cancelled),
                            BatchStatus::TimedOut
                            | BatchStatus::AttemptLimit
                            | BatchStatus::Failed => Some(ExecutionStatus::Failed),
                            _ => Some(ExecutionStatus::Interrupted),
                        };
                        if let Some(status) = status {
                            self.progress.failure = ctx.failure.clone();
                            let message = batch.progress.message.clone();
                            self.stop(status, message, &mut ctx.emit);
                            return Ok(());
                        }
                    }
                    self.progress.completed += 1;
                    // Only the active/final batch belongs to progress, not previous batches.
                    if self.progress.completed < self.progress.actions.len() {
                        self.batch = None;
                    }
                }
            }
            (ctx.emit)(Event::ExecutionProgress(self.progress.clone()));
        }
        if ctx.options.cancellation.is_cancelled() {
            self.stop(
                ExecutionStatus::Cancelled,
                "cancelled by user",
                &mut ctx.emit,
            );
        } else {
            ctx.failure = None;
            self.stop(
                ExecutionStatus::Completed,
                "all requested actions completed",
                &mut ctx.emit,
            );
        }
        Ok(())
    }
}
