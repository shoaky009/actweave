//! Priority-based visual branching, with bounded polling and explicit error routes.
use crate::{
    Control, Error,
    action::{Action, ActionResult, Actions, Backend},
    recognition::{Recognition, RecognitionResult, Recognizers},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};
use tokio::time::Instant;

const INPUT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub recognition: Recognition,
    #[serde(default)]
    pub actions: Vec<Action>,
    #[serde(default)]
    pub next: Vec<String>,
    #[serde(default)]
    pub on_error: Vec<String>,
    /// Candidates to re-observe after an input may have been interrupted.
    #[serde(default)]
    pub on_interrupted: Vec<String>,
    /// Bound each phase: this node's actions, then polling its next candidates.
    pub timeout_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Flow {
    pub entry: String,
    pub nodes: BTreeMap<String, Node>,
    pub poll_interval_ms: u64,
    /// Total polling/action operations, including error paths. Never reset by a loop.
    pub max_operations: u64,
}
impl Flow {
    pub fn validate(&self) -> Result<(), Error> {
        if !self.nodes.contains_key(&self.entry)
            || self.poll_interval_ms == 0
            || self.max_operations == 0
        {
            return Err(Error::Invalid("entry and positive budgets required".into()));
        }
        for (name, node) in &self.nodes {
            if name.trim().is_empty()
                || node.timeout_ms == 0
                || Instant::now()
                    .checked_add(Duration::from_millis(node.timeout_ms))
                    .is_none()
            {
                return Err(Error::Invalid(format!("invalid node or timeout: {name}")));
            }
            node.recognition.validate()?;
            for target in node
                .next
                .iter()
                .chain(&node.on_error)
                .chain(&node.on_interrupted)
            {
                if !self.nodes.contains_key(target) {
                    return Err(Error::Invalid(format!("unknown target {target} in {name}")));
                }
            }
            for action in &node.actions {
                match action {
                    Action::KeyDown { key } | Action::KeyUp { key } if key.trim().is_empty() => {
                        return Err(Error::Invalid("empty key".into()));
                    }
                    Action::Custom { name, .. } if name.trim().is_empty() => {
                        return Err(Error::Invalid("empty custom action".into()));
                    }
                    Action::Wait { duration_ms }
                    | Action::LongPress { duration_ms, .. }
                    | Action::Swipe { duration_ms, .. }
                        if *duration_ms > node.timeout_ms =>
                    {
                        return Err(Error::Invalid(format!(
                            "action duration exceeds node timeout: {name}"
                        )));
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Running,
    Completed,
    Blocked,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    pub node: String,
    pub action_index: Option<usize>,
    pub message: String,
}

/// Snapshot can be translated into adapter state and logs; it is not a Core contract.
#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    pub status: Status,
    pub node: String,
    pub action_index: Option<usize>,
    pub operations: u64,
    pub recognition: RecognitionResult,
    pub action_result: Value,
    pub failure: Option<Failure>,
    pub message: String,
}

/// Own one runner per invocation/session. `step` performs one recognition pass or
/// one action. Callers may query between steps; no permanent polling task is spawned.
/// A completed flow is only local completion, not proof of the user's task success.
pub struct Runner {
    pipeline: Flow,
    progress: Progress,
    candidates: Vec<String>,
    deadline: Instant,
    next_poll: Instant,
    paused_at: Option<Instant>,
}
impl Runner {
    pub fn new<B: Backend>(
        pipeline: Flow,
        recognizers: &Recognizers,
        actions: &Actions<B>,
    ) -> Result<Self, Error> {
        pipeline.validate()?;
        for (name, node) in &pipeline.nodes {
            if !recognizers.supports(&node.recognition)
                || node.actions.iter().any(|a| !actions.supports(a))
            {
                return Err(Error::Unsupported(format!(
                    "unregistered capability in {name}"
                )));
            }
        }
        let deadline =
            Instant::now() + Duration::from_millis(pipeline.nodes[&pipeline.entry].timeout_ms);
        Ok(Self {
            candidates: vec![pipeline.entry.clone()],
            deadline,
            next_poll: Instant::now(),
            paused_at: None,
            progress: Progress {
                status: Status::Running,
                node: pipeline.entry.clone(),
                action_index: None,
                operations: 0,
                recognition: RecognitionResult::default(),
                action_result: Value::Null,
                failure: None,
                message: "waiting for entry".into(),
            },
            pipeline,
        })
    }
    pub fn progress(&self) -> &Progress {
        &self.progress
    }
    fn notify_interrupted<B: Backend>(&self, actions: &mut Actions<B>) {
        if let Some(index) = self.progress.action_index
            && let Some(action) = self.pipeline.nodes[&self.progress.node].actions.get(index)
        {
            actions.interrupted(action);
        }
    }
    async fn release_input<B: Backend>(&mut self, backend: &mut B) -> Result<(), Error> {
        let result = match tokio::time::timeout(INPUT_CLEANUP_TIMEOUT, backend.release_all()).await
        {
            Ok(result) => result,
            Err(_) => Err(Error::Backend("input cleanup timed out".into())),
        };
        if let Err(error) = result {
            self.progress.status = Status::Failed;
            self.progress.message = format!("input cleanup failed: {error}");
            return Err(Error::Cleanup(error.to_string()));
        }
        Ok(())
    }
    pub async fn cancel<B: Backend>(&mut self, backend: &mut B) -> Result<(), Error> {
        self.release_input(backend).await?;
        self.progress.status = Status::Cancelled;
        self.progress.message = "cancelled".into();
        Ok(())
    }
    async fn park<B: Backend>(
        &mut self,
        backend: &mut B,
        actions: &mut Actions<B>,
        control: &Control,
        interrupted_action: bool,
    ) -> Result<(), Error> {
        self.release_input(backend).await?;
        if self.progress.action_index.is_some() {
            self.notify_interrupted(actions);
            let recovery = &self.pipeline.nodes[&self.progress.node].on_interrupted;
            if recovery.is_empty() && (interrupted_action || self.progress.action_index != Some(0))
            {
                self.progress.status = Status::Blocked;
                self.progress.message = "action outcome unknown after pause".into();
            } else {
                self.candidates = if recovery.is_empty() {
                    vec![self.progress.node.clone()]
                } else {
                    recovery.clone()
                };
                self.progress.action_index = None;
                self.progress.recognition = RecognitionResult::default();
                self.progress.action_result = Value::Null;
                self.progress.message = "re-observe after pause".into();
            }
        }
        let paused_at = Instant::now();
        self.paused_at = Some(paused_at);
        control.acknowledge_pause();
        tokio::select! {
            biased;
            _ = control.cancelled() => self.cancel(backend).await?,
            _ = control.resumed() => {},
        }
        let elapsed = self.paused_at.take().unwrap_or(paused_at).elapsed();
        self.deadline += elapsed;
        self.next_poll += elapsed;
        Ok(())
    }
    pub async fn step<B: Backend>(
        &mut self,
        backend: &mut B,
        recognizers: &mut Recognizers,
        actions: &mut Actions<B>,
        control: &Control,
    ) -> Result<&Progress, Error> {
        if self.progress.status != Status::Running {
            return Ok(&self.progress);
        }
        if control.check().is_err() {
            self.cancel(backend).await?;
            return Ok(&self.progress);
        }
        if control.is_pause_requested() {
            self.park(backend, actions, control, false).await?;
            return Ok(&self.progress);
        }
        if self.progress.operations >= self.pipeline.max_operations {
            self.progress.status = Status::Failed;
            self.progress.message = "flow operation budget exhausted".into();
            self.release_input(backend).await?;
            return Ok(&self.progress);
        }
        self.progress.operations += 1;
        let deadline = self.deadline;
        let result = tokio::select! {
            biased;
            _ = control.cancelled() => Some(Err(Error::Cancelled)),
            _ = control.pause_requested() => None,
            _ = tokio::time::sleep_until(deadline) => Some(Err(Error::TimedOut)),
            result = self.advance(backend, recognizers, actions, control) => Some(result),
        };
        let Some(result) = result else {
            let interrupted_action = self.progress.action_index.is_some();
            self.park(backend, actions, control, interrupted_action)
                .await?;
            return Ok(&self.progress);
        };
        if let Err(error) = result {
            self.notify_interrupted(actions);
            let cancelled = matches!(error, Error::Cancelled);
            self.progress.failure = Some(Failure {
                node: self.progress.node.clone(),
                action_index: self.progress.action_index,
                message: error.to_string(),
            });
            self.progress.message = error.to_string();
            // Release any partial input before entering a recovery flow.
            if let Err(cleanup) = self.release_input(backend).await {
                self.progress.message = format!("{error}; {}", self.progress.message);
                return Err(cleanup);
            }
            let recovery = &self.pipeline.nodes[&self.progress.node].on_error;
            if cancelled {
                self.progress.status = Status::Cancelled;
            } else if recovery.is_empty()
                || matches!(error, Error::Invalid(_) | Error::Unsupported(_))
            {
                self.progress.status = Status::Failed;
            } else {
                self.candidates = recovery.clone();
                self.progress.action_index = None;
                self.deadline = Instant::now()
                    + Duration::from_millis(self.pipeline.nodes[&self.progress.node].timeout_ms);
                self.next_poll =
                    Instant::now() + Duration::from_millis(self.pipeline.poll_interval_ms);
            }
        }
        if matches!(self.progress.status, Status::Completed | Status::Blocked) {
            self.release_input(backend).await?;
        }
        Ok(&self.progress)
    }
    async fn advance<B: Backend>(
        &mut self,
        backend: &mut B,
        recognizers: &mut Recognizers,
        actions: &mut Actions<B>,
        control: &Control,
    ) -> Result<(), Error> {
        if let Some(index) = self.progress.action_index {
            let node = &self.pipeline.nodes[&self.progress.node];
            if let Some(action) = node.actions.get(index) {
                let result = actions
                    .execute(action, backend, &self.progress.recognition, control)
                    .await?;
                match result {
                    ActionResult::Continue(output) => {
                        self.progress.action_result = output;
                        self.progress.action_index = Some(index + 1);
                        self.progress.message = "action completed".into();
                    }
                    ActionResult::Reobserve(output) => {
                        if index != 0 || node.actions.len() != 1 {
                            return Err(Error::Invalid(
                                "reobserve requires a single-action node".into(),
                            ));
                        }
                        self.progress.action_result = output;
                        self.progress.action_index = None;
                        self.candidates = vec![self.progress.node.clone()];
                        self.deadline = Instant::now() + Duration::from_millis(node.timeout_ms);
                        self.next_poll = Instant::now();
                        self.progress.message = "re-observe".into();
                        return Ok(());
                    }
                    ActionResult::Route {
                        node: target,
                        output,
                    } => {
                        if !node.next.contains(&target) {
                            return Err(Error::Invalid(format!(
                                "action routed outside next candidates: {target}"
                            )));
                        }
                        self.progress.action_result = output;
                        self.progress.action_index = None;
                        self.candidates = vec![target];
                        self.deadline = Instant::now() + Duration::from_millis(node.timeout_ms);
                        self.next_poll = Instant::now();
                        self.progress.message = "action routed".into();
                        return Ok(());
                    }
                    ActionResult::Complete(output) => {
                        self.progress.action_result = output;
                        self.progress.action_index = None;
                        self.progress.status = Status::Completed;
                        self.progress.message = "flow completed".into();
                        return Ok(());
                    }
                    ActionResult::Blocked(reason) => {
                        self.progress.action_index = None;
                        self.progress.status = Status::Blocked;
                        self.progress.message = reason;
                        return Ok(());
                    }
                }
            }
            if self.progress.action_index == Some(node.actions.len()) {
                self.progress.action_index = None;
                self.candidates = node.next.clone();
                self.deadline = Instant::now() + Duration::from_millis(node.timeout_ms);
                self.next_poll = Instant::now();
                if self.candidates.is_empty() {
                    self.progress.status = Status::Completed;
                    self.progress.message = "flow completed".into();
                }
            }
        } else {
            tokio::time::sleep_until(self.next_poll).await;
            let frame = backend.capture(control).await?;
            for name in &self.candidates {
                let result = recognizers
                    .recognize(&self.pipeline.nodes[name].recognition, &frame, control)
                    .await?;
                if result.matched() {
                    self.progress.node = name.clone();
                    self.progress.recognition = result;
                    self.progress.action_result = Value::Null;
                    self.progress.action_index = Some(0);
                    self.progress.message = "node matched".into();
                    self.deadline = Instant::now()
                        + Duration::from_millis(self.pipeline.nodes[name].timeout_ms);
                    return Ok(());
                }
            }
            self.progress.message = "no candidate matched".into();
            self.next_poll = Instant::now() + Duration::from_millis(self.pipeline.poll_interval_ms);
        }
        Ok(())
    }
}
