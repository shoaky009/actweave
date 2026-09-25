//! Task ownership, safe-boundary controls and task-scoped diagnostics.
use crate::core::{
    Adapter, Agent, CancellationToken, Error, Event, Outcome, RunOptions, Status, Task,
};
use crate::metrics::{MeasuredAdapter, TaskMetrics, TaskSummary};
use adapter_api::PauseToken;
use serde::Serialize;
use std::{
    io::Write,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Created,
    Running,
    PauseRequested,
    Paused,
    CancelRequested,
    Completed,
    Failed,
    Cancelled,
}
impl TaskStatus {
    fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// Latest task metadata, independent of the adapter's runtime.
#[derive(Debug, Clone, Serialize)]
pub struct TaskSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<TaskSummary>,
    pub task_id: String,
    pub goal: String,
    pub status: TaskStatus,
    pub current_skill: Option<String>,
    pub reason: Option<String>,
}

struct Journal {
    writer: Box<dyn Write + Send>,
    error: Option<String>,
}

/// Cloneable control/query handle. Commands take effect at safe execution boundaries.
#[derive(Clone)]
pub struct TaskHandle {
    state: Arc<watch::Sender<TaskSnapshot>>,
    cancellation: CancellationToken,
    pause: PauseToken,
    journal: Arc<Mutex<Journal>>,
    metrics: TaskMetrics,
}
impl TaskHandle {
    pub fn snapshot(&self) -> TaskSnapshot {
        self.state.borrow().clone()
    }
    /// Subscribe to the latest state; intermediate updates may be coalesced.
    pub fn subscribe(&self) -> watch::Receiver<TaskSnapshot> {
        self.state.subscribe()
    }
    pub fn pause(&self) -> bool {
        let mut accepted = false;
        self.state.send_modify(|snapshot| {
            if matches!(snapshot.status, TaskStatus::Created | TaskStatus::Running) {
                snapshot.status = TaskStatus::PauseRequested;
                self.pause.request();
                self.record(&snapshot.task_id, "lifecycle", snapshot);
                accepted = true;
            }
        });
        accepted
    }
    pub fn resume(&self) -> bool {
        let mut accepted = false;
        self.state.send_modify(|snapshot| {
            if matches!(
                snapshot.status,
                TaskStatus::PauseRequested | TaskStatus::Paused
            ) {
                snapshot.status = TaskStatus::Running;
                self.pause.resume();
                self.record(&snapshot.task_id, "lifecycle", snapshot);
                accepted = true;
            }
        });
        accepted
    }
    pub fn cancel(&self) -> bool {
        let mut accepted = false;
        self.state.send_modify(|s| {
            if !s.status.terminal() && s.status != TaskStatus::CancelRequested {
                self.cancellation.cancel();
                s.status = TaskStatus::CancelRequested;
                self.pause.resume();
                self.record(&s.task_id, "lifecycle", &s);
                accepted = true;
            }
        });
        accepted
    }
    pub(crate) fn pause_token(&self) -> PauseToken {
        self.pause.clone()
    }
    fn transition(&self, from: &[TaskStatus], to: TaskStatus) -> bool {
        let mut accepted = false;
        self.state.send_modify(|s| {
            if from.contains(&s.status) {
                s.status = to;
                self.record(&s.task_id, "lifecycle", &s);
                accepted = true;
            }
        });
        accepted
    }
    fn record(&self, task_id: &str, kind: &str, data: &impl Serialize) {
        record(&self.journal, task_id, kind, data);
    }
    pub(crate) fn check_log(&self) -> Result<(), Error> {
        let journal = self.journal.lock().unwrap_or_else(|e| e.into_inner());
        match &journal.error {
            Some(e) => Err(Error::Logging(e.clone())),
            None => Ok(()),
        }
    }
    fn event(&self, event: &Event) {
        self.state.send_modify(|s| {
            match event {
                Event::ExecutionProgress(p)
                    if p.status == crate::core::ExecutionStatus::Interrupted =>
                {
                    self.metrics.interrupted()
                }
                Event::ActionStarted { call, .. } => s.current_skill = Some(call.name.clone()),
                Event::BatchProgress(p) => {
                    s.current_skill = if p.status == crate::core::BatchStatus::Completed {
                        None
                    } else {
                        Some(p.request.call().name.clone())
                    };
                }
                Event::Executed(_) => s.current_skill = None,
                _ => {}
            }
            self.record(&s.task_id, "execution", event);
        });
    }
    /// Pause holds the original execution future, including pending decisions and batches.
    /// Wall-clock batch deadlines continue to apply while paused.
    pub(crate) async fn checkpoint(&self, deadline: Option<Instant>) -> Result<(), Error> {
        let mut updates = self.subscribe();
        loop {
            self.check_log()?;
            if self.cancellation.is_cancelled() || deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(());
            }
            self.pause.acknowledge();
            if self.snapshot().status != TaskStatus::Paused {
                return Ok(());
            }
            tokio::select! {
                _ = updates.changed() => {},
                _ = tokio::time::sleep(Duration::from_millis(25)) => {},
            }
        }
    }
    pub(crate) async fn cancelled(&self) {
        let mut updates = self.subscribe();
        while !self.cancellation.is_cancelled() {
            tokio::select! {
                _ = updates.changed() => {},
                _ = tokio::time::sleep(Duration::from_millis(25)) => {},
            }
        }
    }
    fn finish(&self, result: &Result<Outcome, Error>) {
        let summary = self.metrics.finish();
        self.state.send_modify(|s| {
            let (status, reason) = match result {
                Ok(o) => (
                    match o.status {
                        Status::Completed => TaskStatus::Completed,
                        Status::Failed => TaskStatus::Failed,
                        Status::Cancelled => TaskStatus::Cancelled,
                    },
                    o.reason.clone(),
                ),
                Err(e) => (TaskStatus::Failed, e.to_string()),
            };
            s.status = status;
            s.reason = Some(reason);
            s.current_skill = None;
            s.summary = Some(summary);
            self.record(&s.task_id, "lifecycle", &s);
        });
    }
}

fn record(journal: &Arc<Mutex<Journal>>, task_id: &str, kind: &str, data: &impl Serialize) {
    let mut journal = journal.lock().unwrap_or_else(|e| e.into_inner());
    if journal.error.is_some() {
        return;
    }
    let entry = serde_json::json!({"task_id": task_id, "timestamp_ms": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis(), "kind":kind,"data":data});
    let result = serde_json::to_writer(&mut journal.writer, &entry)
        .map_err(std::io::Error::other)
        .and_then(|()| journal.writer.write_all(b"\n"))
        .and_then(|()| journal.writer.flush());
    if let Err(error) = result {
        journal.error = Some(error.to_string());
    }
}

/// One task execution. Consuming `run` prevents accidental duplicate starts.
/// The caller owns scheduling; a mutable adapter borrow prevents concurrent use of one adapter.
pub struct TaskRuntime {
    task: Task,
    options: RunOptions,
    handle: TaskHandle,
}
impl TaskRuntime {
    pub fn new(task: Task, options: RunOptions) -> Self {
        let task_id = format!(
            "{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        );
        let (state, _) = watch::channel(TaskSnapshot {
            summary: None,
            task_id,
            goal: task.goal.clone(),
            status: TaskStatus::Created,
            current_skill: None,
            reason: None,
        });
        let state = Arc::new(state);
        let journal = Arc::new(Mutex::new(Journal {
            writer: Box::new(std::io::sink()),
            error: None,
        }));
        let weak_state = Arc::downgrade(&state);
        let weak_journal = Arc::downgrade(&journal);
        let pause = PauseToken::new(move || {
            if let (Some(state), Some(journal)) = (weak_state.upgrade(), weak_journal.upgrade()) {
                state.send_modify(|snapshot| {
                    if snapshot.status == TaskStatus::PauseRequested {
                        snapshot.status = TaskStatus::Paused;
                        record(&journal, &snapshot.task_id, "lifecycle", snapshot);
                    }
                });
            }
        });
        let handle = TaskHandle {
            state,
            cancellation: options.cancellation.clone(),
            pause,
            metrics: TaskMetrics::default(),
            journal,
        };
        Self {
            task,
            options,
            handle,
        }
    }
    pub fn handle(&self) -> TaskHandle {
        self.handle.clone()
    }
    /// Configure a dedicated JSONL event sink before starting this task.
    pub fn with_log_writer(self, writer: impl Write + Send + 'static) -> Self {
        self.handle
            .journal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .writer = Box::new(writer);
        let snapshot = self.handle.snapshot();
        self.handle
            .record(&snapshot.task_id, "lifecycle", &snapshot);
        self
    }
    pub async fn run(
        self,
        agent: &mut impl Agent,
        environment: &mut impl Adapter,
        mut emit: impl FnMut(Event),
    ) -> Result<Outcome, Error> {
        self.handle.metrics.start();
        let guard = RunGuard(self.handle.clone());
        agent.bind_task(&self.handle.snapshot().task_id, self.handle.metrics.clone());
        self.handle
            .transition(&[TaskStatus::Created], TaskStatus::Running);
        let mut measured_adapter = MeasuredAdapter {
            inner: environment,
            metrics: self.handle.metrics.clone(),
        };
        let mut result = crate::core::run_managed(
            &self.task,
            agent,
            &mut measured_adapter,
            self.options.clone(),
            &self.handle,
            |event| {
                self.handle.event(&event);
                emit(event);
            },
        )
        .await;
        self.finish_run(&mut result);
        drop(guard);
        result
    }
    /// Execute a prepared feature through the same lifecycle, metrics and controls.
    pub async fn run_request(
        self,
        request: adapter_api::ExecutionRequest,
        environment: &mut impl Adapter,
        mut emit: impl FnMut(Event),
    ) -> Result<Outcome, Error> {
        self.handle.metrics.start();
        let guard = RunGuard(self.handle.clone());
        self.handle
            .transition(&[TaskStatus::Created], TaskStatus::Running);
        let mut measured_adapter = MeasuredAdapter {
            inner: environment,
            metrics: self.handle.metrics.clone(),
        };
        let mut result = crate::core::run_request_managed(
            &self.task,
            request,
            &mut measured_adapter,
            self.options.clone(),
            &self.handle,
            |event| {
                self.handle.event(&event);
                emit(event);
            },
        )
        .await;
        self.finish_run(&mut result);
        drop(guard);
        result
    }
    fn finish_run(&self, result: &mut Result<Outcome, Error>) {
        if let Err(e) = self.handle.check_log() {
            *result = Err(e);
        }
        self.handle.finish(result);
        if let Err(e) = self.handle.check_log() {
            *result = Err(e);
            self.handle.finish(result);
        }
        if let Ok(outcome) = result {
            outcome.summary = self.handle.metrics.finish();
        }
    }
}
struct RunGuard(TaskHandle);
impl Drop for RunGuard {
    fn drop(&mut self) {
        if !self.0.snapshot().status.terminal() {
            self.0.cancellation.cancel();
            self.0.finish(&Err(Error::Invalid(
                "task execution was dropped; action outcome may be unknown".into(),
            )));
        }
    }
}
