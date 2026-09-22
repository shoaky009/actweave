//! Priority-based visual branching, with bounded polling and explicit error routes.
use crate::{
    Control, Error,
    action::{Action, Actions, Backend},
    recognition::{Recognition, RecognitionResult, Recognizers},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};
use tokio::time::Instant;

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
            for target in node.next.iter().chain(&node.on_error) {
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
    /// Pause at a step boundary; call after an in-flight step returns. For held keys,
    /// release via the backend before allowing an extended pause.
    pub fn pause(&mut self) {
        if self.paused_at.is_none() {
            self.paused_at = Some(Instant::now());
        }
    }
    pub fn resume(&mut self) {
        if let Some(at) = self.paused_at.take() {
            let elapsed = at.elapsed();
            self.deadline += elapsed;
            self.next_poll += elapsed;
        }
    }
    pub async fn cancel<B: Backend>(&mut self, backend: &mut B) -> Result<(), Error> {
        self.progress.status = Status::Cancelled;
        self.progress.message = "cancelled".into();
        backend.release_all().await
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
        if self.paused_at.is_some() {
            return Ok(&self.progress);
        }
        if self.progress.operations >= self.pipeline.max_operations {
            self.progress.status = Status::Failed;
            self.progress.message = "flow operation budget exhausted".into();
            backend.release_all().await?;
            return Ok(&self.progress);
        }
        self.progress.operations += 1;
        let deadline = self.deadline;
        let result = tokio::select! {
            biased;
            _ = control.cancelled() => Err(Error::Cancelled),
            _ = tokio::time::sleep_until(deadline) => Err(Error::TimedOut),
            result = self.advance(backend, recognizers, actions, control) => result,
        };
        if let Err(error) = result {
            let cancelled = matches!(error, Error::Cancelled);
            self.progress.failure = Some(Failure {
                node: self.progress.node.clone(),
                action_index: self.progress.action_index,
                message: error.to_string(),
            });
            self.progress.message = error.to_string();
            // Release any partial input before entering a recovery flow.
            if let Err(cleanup) = backend.release_all().await {
                self.progress.status = Status::Failed;
                self.progress.message = format!("{error}; input cleanup failed: {cleanup}");
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
        if self.progress.status == Status::Completed
            && let Err(error) = backend.release_all().await
        {
            self.progress.status = Status::Failed;
            self.progress.message = format!("input cleanup failed: {error}");
            return Err(error);
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
                self.progress.action_result = actions
                    .execute(action, backend, &self.progress.recognition, control)
                    .await?;
                self.progress.action_index = Some(index + 1);
                self.progress.message = "action completed".into();
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
