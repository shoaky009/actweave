//! Application-independent contracts and bounded execution loop.
use crate::batch::MAX_REPEAT;
pub use crate::batch::{BatchProgress, BatchRequest, BatchStatus, RepeatRequest};
use crate::execution::{ActiveExecution, ExecutionContext, MAX_ACTIONS};
pub use crate::execution::{ExecutionProgress, ExecutionStatus};
use crate::skills::{self, LoadResult, LoadSkills, SkillMode, SkillView};
pub use adapter_api::{
    ActionOutcome, Adapter, AdapterError, AppState, Availability, CancellationToken,
    DecisionContext, ExecutionControl, ExecutionFailure, FailureStage, Skill, SkillContext,
    ToolCall, ToolResult,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, future::Future, time::Duration};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("decision log output: {0}")]
    Logging(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("model service: {0}")]
    Model(String),
    #[error(transparent)]
    Adapter(#[from] AdapterError),
}

/// A user goal; contains no application-specific execution instructions.
#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub goal: String,
}
impl Task {
    pub fn new(goal: impl Into<String>) -> Result<Self, Error> {
        let goal = goal.into();
        if goal.trim().is_empty() {
            return Err(Error::Invalid("empty task".into()));
        }
        Ok(Self { goal })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Action {
    Call(ToolCall),
    Repeat(RepeatRequest),
    /// Execute locally until the adapter explicitly reports Completed.
    UntilDone(ToolCall),
}

/// Applied only after the action completes normally, never after an interruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Continuation {
    Decide,
    Finish,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Decision {
    Execute {
        actions: Vec<Action>,
        then: Continuation,
    },
    /// Replace an interrupted suffix. An active batch must be carried forward first.
    ReplaceRemaining {
        actions: Vec<Action>,
        then: Continuation,
    },
    LoadSkills(LoadSkills),
    /// Resume the pending action with its original continuation.
    Resume,
    Completed(String),
    Failed(String),
}

/// Provider-independent decision interface: models, rules or human input can implement it.
/// Tools are described by their parameter schema; candidates are optional suggestions.
/// The returned call is validated/executed by the adapter, not by the decision provider.
pub trait Agent {
    /// Bind task diagnostics and actual model-request accounting. Rule/manual providers
    /// need not use metrics; model providers count each request via model_request().
    fn bind_task(&mut self, _task_id: &str, _metrics: crate::metrics::TaskMetrics) {}

    fn decide(
        &mut self,
        task: &Task,
        state: &AppState,
        skills: &[Skill],
        previous: Option<&ToolResult>,
    ) -> impl Future<Output = Result<Decision, Error>> + Send;

    /// Override to support discovery. Existing agents receive only injected tools.
    fn decide_with_skills(
        &mut self,
        task: &Task,
        state: &AppState,
        view: &SkillView,
        previous: Option<&ToolResult>,
    ) -> impl Future<Output = Result<Decision, Error>> + Send {
        self.decide(task, state, &view.loaded, previous)
    }
}
#[derive(Debug, Serialize)]
pub enum Event {
    Observed(AppState),
    SkillsResolved {
        step: usize,
        skills: Vec<Skill>,
        guidance: String,
    },
    SkillsInjected {
        names: Vec<String>,
        directory_count: usize,
    },
    SkillsLoaded(LoadResult),
    Decided(Decision),
    Executed(ToolResult),
    ExecutionFailed(ExecutionFailure),
    ExecutionRejected(String),
    BatchProgress(BatchProgress),
    ExecutionProgress(ExecutionProgress),
    ActionStarted {
        index: usize,
        total: usize,
        call: ToolCall,
    },
}
#[derive(Debug, PartialEq, Eq, Serialize)]
pub enum Status {
    Cancelled,
    Completed,
    Failed,
}
#[derive(Debug, Serialize)]
pub struct Outcome {
    pub summary: crate::metrics::TaskSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionProgress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch: Option<BatchProgress>,
    pub status: Status,
    pub reason: String,
    pub decisions: usize,
    pub state: AppState,
}

/// Run with all available skills injected, preserving the simple default.
pub async fn run<A: Agent, G: Adapter>(
    task: &Task,
    agent: &mut A,
    environment: &mut G,
    max_decisions: usize,
    emit: impl FnMut(Event),
) -> Result<Outcome, Error> {
    run_with_options(
        task,
        agent,
        environment,
        RunOptions {
            max_decisions,
            skill_mode: SkillMode::All,
            ..RunOptions::default()
        },
        emit,
    )
    .await
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Stop after this many failures of the same call or rejections of the same plan.
    pub max_repeated_failures: u32,
    pub max_actions: usize,
    pub max_decisions: usize,
    pub skill_mode: SkillMode,
    pub max_repeat: u32,
    pub max_batch_attempts: u32,
    pub batch_timeout: Duration,
    pub cancellation: CancellationToken,
}
impl Default for RunOptions {
    fn default() -> Self {
        Self {
            max_repeated_failures: 3,
            max_actions: 4,
            max_decisions: 12,
            skill_mode: SkillMode::All,
            max_repeat: MAX_REPEAT,
            max_batch_attempts: 200,
            batch_timeout: Duration::from_secs(120),
            cancellation: CancellationToken::default(),
        }
    }
}
fn finish(
    status: Status,
    reason: String,
    decisions: usize,
    state: AppState,
    execution: Option<&ActiveExecution>,
) -> Outcome {
    Outcome {
        summary: crate::metrics::TaskSummary::default(),
        status,
        reason,
        decisions,
        state,
        batch: execution
            .and_then(|e| e.batch.as_ref())
            .map(|b| b.progress.clone()),
        execution: execution.map(|e| e.progress.clone()),
    }
}

fn validation_failure(call: Option<&ToolCall>, message: impl Into<String>) -> ExecutionFailure {
    ExecutionFailure {
        stage: FailureStage::Validation,
        call: call.cloned(),
        message: message.into(),
    }
}

fn validate_actions(
    actions: &[Action],
    skills: &[Skill],
    selected: &BTreeSet<String>,
    options: &RunOptions,
) -> Result<(), ExecutionFailure> {
    if actions.is_empty() || actions.len() > options.max_actions {
        return Err(validation_failure(
            None,
            "action list must be nonempty and within max_actions",
        ));
    }
    for action in actions {
        let name = &action.call().name;
        let skill = skills.iter().find(|s| &s.name == name).ok_or_else(|| {
            validation_failure(Some(action.call()), format!("unknown skill: {name}"))
        })?;
        if options.skill_mode == SkillMode::OnDemand && !selected.contains(name) {
            return Err(validation_failure(
                Some(action.call()),
                format!("skill '{name}' is not loaded; request LoadSkills first"),
            ));
        }
        if let Action::Repeat(request) = action
            && (!skill.repeatable
                || request.times == 0
                || request.times > options.max_repeat.min(options.max_batch_attempts))
        {
            return Err(validation_failure(
                Some(action.call()),
                "repeat skill must be explicitly repeatable and count within configured limits",
            ));
        }
        if matches!(action, Action::UntilDone(_)) && !skill.loopable {
            return Err(validation_failure(
                Some(action.call()),
                "until-done skill must explicitly support local continuation",
            ));
        }
    }
    Ok(())
}

fn reject(
    message: String,
    call: Option<ToolCall>,
    previous: &mut Option<ToolResult>,
    emit: &mut impl FnMut(Event),
) {
    if let Some(call) = call {
        let result = ToolResult {
            call,
            success: false,
            message,
            outcome: None,
        };
        emit(Event::Executed(result.clone()));
        *previous = Some(result);
    } else {
        emit(Event::ExecutionRejected(message));
    }
}

/// Run a task; each model decision may submit several ordered actions.
pub async fn run_with_options<A: Agent, G: Adapter>(
    task: &Task,
    agent: &mut A,
    environment: &mut G,
    options: RunOptions,
    emit: impl FnMut(Event),
) -> Result<Outcome, Error> {
    crate::task_runtime::TaskRuntime::new(task.clone(), options)
        .run(agent, environment, emit)
        .await
}

pub(crate) async fn run_managed<A: Agent, G: Adapter>(
    task: &Task,
    agent: &mut A,
    environment: &mut G,
    options: RunOptions,
    runtime: &crate::task_runtime::TaskHandle,
    emit: impl FnMut(Event),
) -> Result<Outcome, Error> {
    if task.goal.trim().is_empty()
        || options.max_decisions == 0
        || options.max_repeated_failures == 0
        || options.max_actions == 0
        || options.max_actions > MAX_ACTIONS
        || options.max_repeat == 0
        || options.max_repeat > MAX_REPEAT
        || options.max_batch_attempts == 0
        || options.batch_timeout.is_zero()
        || std::time::Instant::now()
            .checked_add(options.batch_timeout)
            .is_none()
    {
        return Err(Error::Invalid(
            "nonempty task and positive bounded execution budgets required".into(),
        ));
    }
    let mut ctx = ExecutionContext {
        failures: Default::default(),
        task,
        environment,
        options: &options,
        runtime,
        state: AppState {
            scene: "unobserved".into(),
            facts: serde_json::json!({}),
        },
        previous: None,
        failure: None,
        interruption: None,
        emit,
    };
    let mut selected = BTreeSet::new();
    let mut last_load = None;
    let mut pending: Option<ActiveExecution> = None;
    runtime.checkpoint(None).await?;
    if options.cancellation.is_cancelled() {
        return Ok(finish(
            Status::Cancelled,
            "cancelled before start; state not observed".into(),
            0,
            ctx.state,
            None,
        ));
    }
    ctx.observe()?;
    for step in 1..=options.max_decisions {
        runtime
            .checkpoint(pending.as_ref().and_then(ActiveExecution::deadline))
            .await?;
        if let Some(active) = &mut pending
            && active.check_limit(&mut ctx.emit)
        {
            let status = if active.progress.status == ExecutionStatus::Cancelled {
                Status::Cancelled
            } else {
                Status::Failed
            };
            return Ok(finish(
                status,
                active.progress.message.clone(),
                step - 1,
                ctx.state,
                pending.as_ref(),
            ));
        }
        if options.cancellation.is_cancelled() {
            if let Some(active) = &mut pending {
                active.stop(
                    ExecutionStatus::Cancelled,
                    "cancelled by user",
                    &mut ctx.emit,
                );
            }
            return Ok(finish(
                Status::Cancelled,
                "cancelled by user".into(),
                step - 1,
                ctx.state,
                pending.as_ref(),
            ));
        }
        ctx.interruption = pending.as_ref().and_then(|e| e.progress.failure.clone());
        let context = ctx.decision_context(step)?;
        let skills = context.skills;
        (ctx.emit)(Event::SkillsResolved {
            step,
            skills: skills.clone(),
            guidance: context.guidance.clone(),
        });
        let mut view = skills::view(&skills, options.skill_mode, &selected, last_load.clone())?;
        view.guidance = context.guidance;
        view.failure = ctx.failure.clone();
        view.batch = pending
            .as_ref()
            .and_then(|e| e.batch.as_ref())
            .map(|b| b.progress.clone());
        view.execution = pending.as_ref().map(|e| e.progress.clone());
        view.max_repeat = options.max_repeat.min(options.max_batch_attempts);
        view.max_actions = options.max_actions;
        (ctx.emit)(Event::SkillsInjected {
            names: view.loaded.iter().map(|s| s.name.clone()).collect(),
            directory_count: view.directory.len(),
        });
        runtime
            .checkpoint(pending.as_ref().and_then(ActiveExecution::deadline))
            .await?;
        let decision = tokio::select! {
            biased;
            _ = runtime.cancelled() => {
                if let Some(active) = &mut pending {
                    active.check_limit(&mut ctx.emit);
                    active.stop(ExecutionStatus::Cancelled, "cancelled while waiting for decision", &mut ctx.emit);
                }
                return Ok(finish(Status::Cancelled, "cancelled while waiting for decision".into(), step - 1, ctx.state, pending.as_ref()));
            }
            _ = async {
                match pending.as_ref().and_then(ActiveExecution::deadline) {
                    Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                if let Some(active) = &mut pending { active.check_limit(&mut ctx.emit); }
                return Ok(finish(Status::Failed, "batch deadline exceeded".into(), step - 1, ctx.state, pending.as_ref()));
            }
            result = agent.decide_with_skills(task, &ctx.state, &view, ctx.previous.as_ref()) => result?,
        };
        (ctx.emit)(Event::Decided(decision.clone()));
        runtime
            .checkpoint(pending.as_ref().and_then(ActiveExecution::deadline))
            .await?;
        if let Some(active) = &mut pending
            && active.check_limit(&mut ctx.emit)
        {
            let status = if active.progress.status == ExecutionStatus::Cancelled {
                Status::Cancelled
            } else {
                Status::Failed
            };
            return Ok(finish(
                status,
                active.progress.message.clone(),
                step,
                ctx.state,
                pending.as_ref(),
            ));
        }
        if options.cancellation.is_cancelled() {
            if let Some(active) = &mut pending {
                active.stop(
                    ExecutionStatus::Cancelled,
                    "cancelled by user",
                    &mut ctx.emit,
                );
            }
            return Ok(finish(
                Status::Cancelled,
                "cancelled by user".into(),
                step,
                ctx.state,
                pending.as_ref(),
            ));
        }
        let plan_key =
            serde_json::to_value(&decision).map_err(|error| Error::Invalid(error.to_string()))?;
        let mut execution = match decision {
            Decision::Completed(reason) => {
                let (status, reason) = if pending.is_some() {
                    (Status::Failed, "cannot declare completion while an execution is interrupted; resume or fail it".into())
                } else {
                    (Status::Completed, reason)
                };
                return Ok(finish(status, reason, step, ctx.state, pending.as_ref()));
            }
            Decision::Failed(reason) => {
                return Ok(finish(
                    Status::Failed,
                    reason,
                    step,
                    ctx.state,
                    pending.as_ref(),
                ));
            }
            Decision::LoadSkills(request) => {
                let selection = if options.skill_mode == SkillMode::OnDemand {
                    skills::select(&request, &skills)
                } else {
                    Err("all mode already injects available skills".into())
                };
                let (success, message) = match selection {
                    Ok(names) => {
                        selected = names;
                        (true, "selection replaced".into())
                    }
                    Err(message) => (false, message),
                };
                let result = LoadResult {
                    request,
                    success,
                    selected: selected.iter().cloned().collect(),
                    message,
                };
                (ctx.emit)(Event::SkillsLoaded(result.clone()));
                last_load = Some(result);
                ctx.observe()?;
                continue;
            }
            Decision::Execute { actions, then } => {
                last_load = None;
                let validation = validate_actions(&actions, &skills, &selected, &options).and_then(|()| {
                    if let Some(active) = &pending {
                        if then != Continuation::Decide || !matches!(actions.as_slice(), [Action::Call(_)]) {
                            return Err(validation_failure(None, "execution pending: use Resume or one repair Call with Decide"));
                        }
                        if active.pending_call().is_some_and(|c| c.name == actions[0].call().name) {
                            return Err(validation_failure(Some(actions[0].call()), "use Resume for the pending action; direct calls would lose progress"));
                        }
                    }
                    Ok(())
                });
                if let Err(failure) = validation {
                    ctx.fail(failure.stage, failure.call.clone(), failure.message.clone());
                    reject(
                        failure.message,
                        failure.call,
                        &mut ctx.previous,
                        &mut ctx.emit,
                    );
                    if let Some(reason) =
                        ctx.failures
                            .plan(plan_key, true, options.max_repeated_failures)
                    {
                        if let Some(active) = &mut pending {
                            active.stop(ExecutionStatus::Failed, reason.clone(), &mut ctx.emit);
                        }
                        return Ok(finish(
                            Status::Failed,
                            reason,
                            step,
                            ctx.state,
                            pending.as_ref(),
                        ));
                    }
                    ctx.observe()?;
                    continue;
                }
                ctx.failures
                    .plan(plan_key, false, options.max_repeated_failures);
                ActiveExecution::new(actions, then)
            }
            Decision::ReplaceRemaining { actions, then } => {
                let validation =
                    validate_actions(&actions, &skills, &selected, &options).and_then(|()| {
                        match pending.as_mut() {
                            Some(active) => active.replace_remaining(actions.clone(), then),
                            None => Err("no interrupted execution to replace".into()),
                        }
                        .map_err(|message| validation_failure(None, message))
                    });
                if let Err(failure) = validation {
                    ctx.fail(failure.stage, failure.call.clone(), failure.message.clone());
                    reject(
                        failure.message,
                        failure.call,
                        &mut ctx.previous,
                        &mut ctx.emit,
                    );
                    if let Some(reason) =
                        ctx.failures
                            .plan(plan_key, true, options.max_repeated_failures)
                    {
                        if let Some(active) = &mut pending {
                            active.stop(ExecutionStatus::Failed, reason.clone(), &mut ctx.emit);
                        }
                        return Ok(finish(
                            Status::Failed,
                            reason,
                            step,
                            ctx.state,
                            pending.as_ref(),
                        ));
                    }
                    ctx.observe()?;
                    continue;
                }
                ctx.failures
                    .plan(plan_key, false, options.max_repeated_failures);
                last_load = None;
                pending
                    .take()
                    .ok_or_else(|| Error::Invalid("no execution to replace".into()))?
            }
            Decision::Resume => {
                last_load = None;
                pending
                    .take()
                    .ok_or_else(|| Error::Invalid("no execution to resume".into()))?
            }
        };
        execution.drive(&mut ctx, step, &selected).await?;
        match execution.progress.status {
            ExecutionStatus::Completed if execution.progress.then == Continuation::Finish => {
                return Ok(finish(
                    Status::Completed,
                    execution.progress.message.clone(),
                    step,
                    ctx.state,
                    Some(&execution),
                ));
            }
            ExecutionStatus::Cancelled | ExecutionStatus::Failed => {
                let status = if execution.progress.status == ExecutionStatus::Cancelled {
                    Status::Cancelled
                } else {
                    Status::Failed
                };
                if let Some(active) = &mut pending {
                    active.stop(
                        execution.progress.status,
                        execution.progress.message.clone(),
                        &mut ctx.emit,
                    );
                }
                return Ok(finish(
                    status,
                    execution.progress.message.clone(),
                    step,
                    ctx.state,
                    pending.as_ref().or(Some(&execution)),
                ));
            }
            ExecutionStatus::Interrupted if execution.retained_on_error() && pending.is_none() => {
                pending = Some(execution);
            }
            _ => {}
        }
        if options.cancellation.is_cancelled() {
            if let Some(active) = &mut pending {
                active.stop(
                    ExecutionStatus::Cancelled,
                    "cancelled by user",
                    &mut ctx.emit,
                );
            }
            return Ok(finish(
                Status::Cancelled,
                "cancelled by user".into(),
                step,
                ctx.state,
                pending.as_ref(),
            ));
        }
    }
    if let Some(active) = &mut pending {
        active.stop(
            ExecutionStatus::Failed,
            "decision limit reached",
            &mut ctx.emit,
        );
    }
    Ok(finish(
        Status::Failed,
        "decision limit reached".into(),
        options.max_decisions,
        ctx.state,
        pending.as_ref(),
    ))
}
