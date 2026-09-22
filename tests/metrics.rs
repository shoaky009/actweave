use actweave::{
    adapter::{DemoAdapter, Scenario},
    core::*,
    manual::ManualAgent,
    metrics::{CallCounts, TaskMetrics},
    task_runtime::{TaskRuntime, TaskStatus},
};
use serde_json::json;
use std::io::Cursor;

fn counts(total: u64, succeeded: u64, failed: u64) -> CallCounts {
    CallCounts {
        total,
        succeeded,
        failed,
    }
}
fn call(name: &str) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: json!({}),
    }
}
fn execute(actions: Vec<Action>, then: Continuation) -> Decision {
    Decision::Execute { actions, then }
}
fn repeat(times: u32) -> Action {
    Action::Repeat(RepeatRequest {
        call: call("perform_trial"),
        times,
    })
}
fn agent(decisions: Vec<Decision>) -> ManualAgent<Cursor<String>, std::io::Sink> {
    ManualAgent::new(
        Cursor::new(
            decisions
                .iter()
                .map(|d| serde_json::to_string(d).unwrap())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        std::io::sink(),
    )
}
fn runtime() -> TaskRuntime {
    TaskRuntime::new(Task::new("demo task").unwrap(), RunOptions::default())
}

#[tokio::test]
async fn repeat_counts_every_call_and_normal_misses_are_not_failures() {
    let runtime = runtime();
    let handle = runtime.handle();
    let outcome = runtime
        .run(
            &mut agent(vec![execute(vec![repeat(10)], Continuation::Finish)]),
            &mut DemoAdapter::default(),
            |_| {},
        )
        .await
        .unwrap();
    assert_eq!(outcome.summary.model_requests, counts(0, 0, 0));
    assert_eq!(outcome.summary.actions, counts(10, 10, 0));
    assert_eq!(outcome.summary.interruptions, 0);
    assert_eq!(outcome.batch.unwrap().successful, 7);
    assert_eq!(handle.snapshot().summary, Some(outcome.summary));
}

#[tokio::test]
async fn until_done_and_repair_count_actual_calls_with_one_interruption() {
    let outcome = runtime()
        .run(
            &mut agent(vec![
                execute(
                    vec![Action::UntilDone(call("explore_step"))],
                    Continuation::Finish,
                ),
                execute(
                    vec![Action::Call(call("reset_exploration"))],
                    Continuation::Decide,
                ),
                Decision::Resume,
            ]),
            &mut DemoAdapter::new(Scenario::ExplorationInterrupted),
            |_| {},
        )
        .await
        .unwrap();
    assert_eq!(outcome.summary.actions, counts(6, 5, 1));
    assert_eq!(outcome.summary.interruptions, 1);
    assert_eq!(outcome.decisions, 3);
    assert_eq!(outcome.summary.model_requests.total, 0);
}

#[tokio::test]
async fn rejected_plans_and_unavailable_skills_are_not_actual_calls() {
    let outcome = runtime()
        .run(
            &mut agent(vec![
                execute(vec![Action::Call(call("missing"))], Continuation::Finish),
                execute(
                    vec![Action::Call(call("start_training"))],
                    Continuation::Finish,
                ),
                Decision::Failed("unavailable".into()),
            ]),
            &mut DemoAdapter::default(),
            |_| {},
        )
        .await
        .unwrap();
    assert_eq!(outcome.summary.actions, counts(0, 0, 0));
    // Plan validation is not an execution interruption; unavailable execution is.
    assert_eq!(outcome.summary.interruptions, 1);
}

#[tokio::test]
async fn adapter_argument_errors_count_as_calls_and_failures() {
    let invalid = ToolCall {
        name: "set_mode".into(),
        arguments: json!({"mode":"unknown"}),
    };
    let outcome = runtime()
        .run(
            &mut agent(vec![
                execute(vec![Action::Call(invalid)], Continuation::Finish),
                Decision::Failed("invalid arguments".into()),
            ]),
            &mut DemoAdapter::default(),
            |_| {},
        )
        .await
        .unwrap();
    assert_eq!(outcome.summary.actions, counts(1, 0, 1));
    assert_eq!(outcome.summary.interruptions, 1);
}

#[tokio::test]
async fn cancellation_before_start_and_input_errors_still_have_summaries() {
    let runtime = runtime();
    let handle = runtime.handle();
    handle.cancel();
    let outcome = runtime
        .run(&mut agent(vec![]), &mut DemoAdapter::default(), |_| {})
        .await
        .unwrap();
    assert_eq!(outcome.status, Status::Cancelled);
    assert_eq!(outcome.summary.actions.total, 0);
    let runtime = TaskRuntime::new(
        Task::new("invalid manual input").unwrap(),
        RunOptions::default(),
    );
    let handle = runtime.handle();
    assert!(
        runtime
            .run(&mut agent(vec![]), &mut DemoAdapter::default(), |_| {})
            .await
            .is_err()
    );
    assert_eq!(handle.snapshot().status, TaskStatus::Failed);
    assert_eq!(handle.snapshot().summary.unwrap().model_requests.total, 0);
}

/// Simulates a provider retry within one decision, exercising the provider-neutral hook.
#[derive(Default)]
struct RetryingProvider {
    metrics: TaskMetrics,
}
impl Agent for RetryingProvider {
    fn bind_task(&mut self, _: &str, metrics: TaskMetrics) {
        self.metrics = metrics;
    }
    async fn decide(
        &mut self,
        _: &Task,
        _: &AppState,
        _: &[Skill],
        _: Option<&ToolResult>,
    ) -> Result<Decision, Error> {
        drop(self.metrics.model_request());
        self.metrics.model_request().success();
        Ok(Decision::Completed("done".into()))
    }
}
#[tokio::test]
async fn model_requests_are_not_decision_rounds_and_reused_provider_is_task_scoped() {
    let mut provider = RetryingProvider::default();
    for _ in 0..2 {
        let outcome = runtime()
            .run(&mut provider, &mut DemoAdapter::default(), |_| {})
            .await
            .unwrap();
        assert_eq!(outcome.decisions, 1);
        assert_eq!(outcome.summary.model_requests, counts(2, 1, 1));
    }
}

struct WaitingProvider {
    metrics: TaskMetrics,
    entered: std::sync::Arc<tokio::sync::Notify>,
}
impl Agent for WaitingProvider {
    fn bind_task(&mut self, _: &str, metrics: TaskMetrics) {
        self.metrics = metrics;
    }
    async fn decide(
        &mut self,
        _: &Task,
        _: &AppState,
        _: &[Skill],
        _: Option<&ToolResult>,
    ) -> Result<Decision, Error> {
        let _request = self.metrics.model_request();
        self.entered.notify_one();
        std::future::pending().await
    }
}
#[tokio::test]
async fn cancelling_in_flight_model_request_counts_it_without_a_decision() {
    let runtime = runtime();
    let handle = runtime.handle();
    let entered = std::sync::Arc::new(tokio::sync::Notify::new());
    let mut provider = WaitingProvider {
        metrics: TaskMetrics::default(),
        entered: entered.clone(),
    };
    let mut adapter = DemoAdapter::default();
    let work = runtime.run(&mut provider, &mut adapter, |_| {});
    let cancel = async {
        entered.notified().await;
        handle.cancel();
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::join!(work, cancel)
    })
    .await
    .unwrap();
    let result = result.unwrap();
    assert_eq!(result.status, Status::Cancelled);
    assert_eq!(result.decisions, 0);
    assert_eq!(result.summary.model_requests, counts(1, 0, 1));
    assert_eq!(result.summary.actions.total, 0);
}
