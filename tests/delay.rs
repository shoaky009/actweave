//! Single-thread tests ensure spare Tokio workers cannot hide a blocking skill.
use actweave::{
    core::*,
    task_runtime::{TaskHandle, TaskRuntime, TaskStatus},
};
use adapter_api::ActionReport;
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

struct DelayAdapter {
    started: Arc<Notify>,
    completed: Arc<AtomicU32>,
}
impl Adapter for DelayAdapter {
    fn observe(&self) -> Result<AppState, AdapterError> {
        Ok(AppState {
            scene: "delay mock".into(),
            facts: json!({"completed":self.completed.load(Ordering::SeqCst)}),
        })
    }
    fn decision_context(&self, _: &SkillContext<'_>) -> Result<DecisionContext, AdapterError> {
        Ok(DecisionContext {
            skills: vec![Skill {
                name: "delay".into(),
                description: "Wait asynchronously for 100 ms.".into(),
                repeatable: true,
                loopable: true,
                tags: vec!["mock".into()],
                availability: Availability::Available,
                parameters: json!({"type":"object","properties":{},"additionalProperties":false}),
                calls: vec![call()],
            }],
            guidance: String::new(),
        })
    }
    async fn execute(
        &mut self,
        call: &ToolCall,
        control: &ExecutionControl,
    ) -> Result<ActionReport, AdapterError> {
        if call.name != "delay" || call.arguments != json!({}) {
            return Err(AdapterError::Invalid("expected delay({})".into()));
        }
        control.check()?;
        self.started.notify_one();
        let end = tokio::time::Instant::now() + Duration::from_millis(100);
        loop {
            control.check()?;
            let now = tokio::time::Instant::now();
            if now >= end {
                break;
            }
            tokio::time::sleep((end - now).min(Duration::from_millis(5))).await;
        }
        self.completed.fetch_add(1, Ordering::SeqCst);
        Ok(ActionReport::completed("delay finished".into(), true))
    }
}
fn call() -> ToolCall {
    ToolCall {
        name: "delay".into(),
        arguments: json!({}),
    }
}
struct DecideOnce(Option<Decision>);
impl Agent for DecideOnce {
    async fn decide(
        &mut self,
        _: &Task,
        _: &AppState,
        _: &[Skill],
        _: Option<&ToolResult>,
    ) -> Result<Decision, Error> {
        Ok(self
            .0
            .take()
            .expect("control operations must not request another decision"))
    }
}
fn repeat() -> Decision {
    Decision::Execute {
        actions: vec![Action::Repeat(RepeatRequest {
            call: call(),
            times: 2,
        })],
        then: Continuation::Finish,
    }
}
fn runtime(options: RunOptions) -> TaskRuntime {
    TaskRuntime::new(Task::new("mock delay").unwrap(), options)
}
fn adapter() -> DelayAdapter {
    DelayAdapter {
        started: Arc::new(Notify::new()),
        completed: Arc::new(AtomicU32::new(0)),
    }
}
async fn wait_paused(handle: &TaskHandle) {
    let mut updates = handle.subscribe();
    loop {
        if updates.borrow_and_update().status == TaskStatus::Paused {
            return;
        }
        updates.changed().await.unwrap();
    }
}

async fn cancel_during(decision: Decision) {
    let runtime = runtime(RunOptions::default());
    let handle = runtime.handle();
    let mut adapter = adapter();
    let started = adapter.started.clone();
    let completed = adapter.completed.clone();
    let mut agent = DecideOnce(Some(decision));
    let work = runtime.run(&mut agent, &mut adapter, |_| {});
    let control = async {
        started.notified().await;
        // The action has actually started, rather than merely been selected.
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        assert_eq!(handle.snapshot().status, TaskStatus::Running);
        assert_eq!(handle.snapshot().current_skill.as_deref(), Some("delay"));
        assert!(handle.cancel());
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(work, control)
    })
    .await
    .unwrap();
    let result = result.unwrap();
    assert_eq!(result.status, Status::Cancelled);
    assert_eq!(result.summary.actions.total, 1);
    assert_eq!(result.summary.actions.succeeded, 0);
    assert_eq!(result.summary.actions.failed, 1);
    assert_eq!(handle.snapshot().summary, Some(result.summary));
    assert_eq!(completed.load(Ordering::SeqCst), 0);
    if let Some(batch) = result.batch {
        assert_eq!(batch.completed, 0);
        assert_eq!(batch.status, BatchStatus::Cancelled);
    }
    assert_eq!(handle.snapshot().status, TaskStatus::Cancelled);
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_delay_keeps_query_and_cancel_responsive() {
    cancel_during(Decision::Execute {
        actions: vec![Action::Call(call())],
        then: Continuation::Decide,
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn repeated_delay_keeps_query_and_cancel_responsive() {
    cancel_during(repeat()).await;
}

#[tokio::test(flavor = "current_thread")]
async fn until_done_delay_keeps_query_and_cancel_responsive() {
    cancel_during(Decision::Execute {
        actions: vec![Action::UntilDone(call())],
        then: Continuation::Finish,
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn until_done_deadline_expires_without_becoming_completion() {
    let runtime = runtime(RunOptions {
        batch_timeout: Duration::from_millis(20),
        ..RunOptions::default()
    });
    let mut agent = DecideOnce(Some(Decision::Execute {
        actions: vec![Action::UntilDone(call())],
        then: Continuation::Finish,
    }));
    let result = runtime
        .run(&mut agent, &mut adapter(), |_| {})
        .await
        .unwrap();
    assert_eq!(result.status, Status::Failed);
    let batch = result.batch.unwrap();
    assert_eq!(batch.status, BatchStatus::TimedOut);
    assert!(!batch.finished);
}

#[tokio::test(flavor = "current_thread")]
async fn pause_is_accepted_during_delay_and_resumes_at_action_boundary() {
    let runtime = runtime(RunOptions::default());
    let handle = runtime.handle();
    let mut adapter = adapter();
    let started = adapter.started.clone();
    let completed = adapter.completed.clone();
    let mut agent = DecideOnce(Some(repeat()));
    let work = runtime.run(&mut agent, &mut adapter, |_| {});
    let control = async {
        started.notified().await;
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        assert!(handle.pause());
        assert_eq!(handle.snapshot().status, TaskStatus::PauseRequested);
        wait_paused(&handle).await;
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        assert!(handle.resume());
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(work, control)
    })
    .await
    .unwrap();
    let result = result.unwrap();
    assert_eq!(result.status, Status::Completed);
    assert_eq!(result.decisions, 1);
    assert!(result.summary.elapsed_ms >= 220);
    assert_eq!(result.summary.actions.total, 2);
    assert_eq!(result.summary.actions.succeeded, 2);
    assert_eq!(completed.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn dropped_in_flight_action_is_counted_as_interrupted_not_normal() {
    let runtime = runtime(RunOptions::default());
    let handle = runtime.handle();
    let mut agent = DecideOnce(Some(repeat()));
    let mut adapter = adapter();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            runtime.run(&mut agent, &mut adapter, |_| {})
        )
        .await
        .is_err()
    );
    let summary = handle.snapshot().summary.unwrap();
    assert_eq!(summary.actions.total, 1);
    assert_eq!(summary.actions.succeeded, 0);
    assert_eq!(summary.actions.failed, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn delay_honors_batch_deadline_without_counting_completion() {
    let runtime = runtime(RunOptions {
        batch_timeout: Duration::from_millis(20),
        ..RunOptions::default()
    });
    let result = runtime
        .run(&mut DecideOnce(Some(repeat())), &mut adapter(), |_| {})
        .await
        .unwrap();
    assert_eq!(result.status, Status::Failed);
    let batch = result.batch.unwrap();
    assert_eq!(batch.status, BatchStatus::TimedOut);
    assert_eq!(batch.completed, 0);
}
