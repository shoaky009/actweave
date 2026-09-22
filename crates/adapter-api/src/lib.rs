//! Shared adapter contract, independent of the execution core and model provider.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

/// Errors from argument validation or concrete runtime operations.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("execution cancelled")]
    Cancelled,
    #[error("execution deadline exceeded")]
    TimedOut,
    #[error("runtime: {0}")]
    Runtime(String),
}

/// Shared cooperative cancellation, usable from CLI, GUI or a supervising thread.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);
impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}
/// Execution cancellation and deadline; adapters check this between operations.
#[derive(Default)]
pub struct ExecutionControl {
    pub cancellation: CancellationToken,
    pub deadline: Option<Instant>,
}
impl ExecutionControl {
    pub fn check(&self) -> Result<(), AdapterError> {
        if self.cancellation.is_cancelled() {
            return Err(AdapterError::Cancelled);
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(AdapterError::TimedOut);
        }
        Ok(())
    }
}
/// Completing an action and obtaining the desired application effect are different facts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ActionOutcome {
    /// One local step finished; the adapter has not yet confirmed the overall goal.
    /// Hosts may repeat the same call only under an explicit until-done policy.
    Continue {
        successful: bool,
    },
    /// For loopable skills, the adapter has confirmed the requested goal is complete.
    Completed {
        successful: bool,
    },
    Interrupted,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionReport {
    pub outcome: ActionOutcome,
    pub message: String,
}
impl ActionReport {
    pub fn completed(message: String, successful: bool) -> Self {
        Self {
            outcome: ActionOutcome::Completed { successful },
            message,
        }
    }
}

/// Adapter-owned, JSON-serializable facts required for the next decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppState {
    pub scene: String,
    pub facts: Value,
}
/// Adapter-owned availability of a semantic capability in the supplied observation.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Availability {
    #[default]
    Available,
    Unavailable {
        reason: String,
    },
}
impl Availability {
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }
}

/// Last execution feedback, shared without referencing an execution engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ActionOutcome>,
    pub call: ToolCall,
    pub success: bool,
    pub message: String,
}

/// Where the host stopped. Observation failures do not undo confirmed tool effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureStage {
    Observation,
    Context,
    Availability,
    Execution,
    Validation,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionFailure {
    pub stage: FailureStage,
    /// Related call, if known. Only ToolResult.outcome confirms execution effects.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call: Option<ToolCall>,
    pub message: String,
}

/// Current adapter-authored decision material. Regenerated, never appended to history.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct DecisionContext {
    pub skills: Vec<Skill>,
    /// Brief domain guidance for the current state/phase; empty when unnecessary.
    /// Does not grant availability or override host execution rules.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub guidance: String,
}

/// Per-decision facts supplied by the host. Adapters assess capability, not task relevance.
/// Permissions, when applicable, must come from adapter-verified state, not task text.
pub struct SkillContext<'a> {
    pub state: &'a AppState,
    /// Opaque user goal for context; never grants permissions or changes preconditions.
    pub task_goal: &'a str,
    /// One-based decision round.
    pub decision_step: usize,
    pub previous_result: Option<&'a ToolResult>,
    /// Most recent operation failure; separate from the last tool's confirmed outcome.
    pub failure: Option<&'a ExecutionFailure>,
    /// Original unresolved interruption, retained even after a repair succeeds.
    pub interruption: Option<&'a ExecutionFailure>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    /// Explain when to use this capability, prerequisites, effects, completion semantics,
    /// and non-obvious argument meanings. Shared by all candidates for this skill.
    pub description: String,
    /// Explicit opt-in: one execution must finish one countable action, not merely start it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub repeatable: bool,
    /// Supports local steps returning Continue until Completed confirms the goal.
    /// The adapter owns progress and termination; lack of availability is not completion.
    #[serde(default, skip_serializing_if = "is_false")]
    pub loopable: bool,
    /// Descriptive labels for future discovery/filtering, not authorization rules.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default)]
    pub availability: Availability,
    /// Authoritative JSON Schema for arguments. Implementations validate it in execute.
    pub parameters: Value,
    /// Optional concrete suggestions for decision makers that choose from finite options.
    /// An empty list does not disable the skill or prohibit other valid arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<ToolCall>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
}
fn is_false(value: &bool) -> bool {
    !value
}

/// Adapters validate arguments and hide all concrete runtime operations.
pub trait Adapter {
    fn observe(&self) -> Result<AppState, AdapterError>;
    /// Resolve skills and current guidance against host context. Keep unavailable entries
    /// with reasons. Use supplied state/feedback rather than accumulating prompt history.
    fn decision_context(&self, context: &SkillContext<'_>)
    -> Result<DecisionContext, AdapterError>;
    /// Execute one action and report its outcome, regardless of its duration.
    /// Yield during waits and check `control` before effects and between operations.
    /// Blocking platform I/O must be isolated by the adapter (e.g. a dedicated worker).
    /// Core awaits the result rather than dropping an in-flight action on cancellation.
    fn execute(
        &mut self,
        call: &ToolCall,
        control: &ExecutionControl,
    ) -> impl Future<Output = Result<ActionReport, AdapterError>>;
}
