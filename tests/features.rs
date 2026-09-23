use actweave::{
    adapter::{DemoAdapter, Scenario},
    core::*,
    task_runtime::{TaskRuntime, TaskStatus},
};
use adapter_api::ExecutionRequest;
use serde_json::json;
use std::{process::Command, time::Duration};

fn request(adapter: &DemoAdapter, times: u32) -> ExecutionRequest {
    adapter
        .prepare_feature("repeat_trial", &json!({"times":times}))
        .unwrap()
}
#[tokio::test]
async fn local_feature_has_zero_decisions_and_model_requests() {
    let mut adapter = DemoAdapter::default();
    let request = request(&adapter, 10);
    let runtime = TaskRuntime::new(Task::new("trial").unwrap(), RunOptions::default());
    let handle = runtime.handle();
    let outcome = runtime
        .run_request(request, &mut adapter, |event| {
            assert!(!matches!(event, Event::Decided(_)))
        })
        .await
        .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.decisions, 0);
    assert_eq!(outcome.summary.model_requests.total, 0);
    assert_eq!(outcome.summary.actions.total, 10);
    assert_eq!(handle.snapshot().status, TaskStatus::Completed);
    assert_eq!(outcome.state.facts["trial"]["completed"], 10);
}
#[tokio::test]
async fn local_failure_stops_without_retry_or_model() {
    let mut adapter = DemoAdapter::new(Scenario::BatchInterrupted);
    let request = request(&adapter, 10);
    let outcome = TaskRuntime::new(Task::new("trial").unwrap(), RunOptions::default())
        .run_request(request, &mut adapter, |_| {})
        .await
        .unwrap();
    assert_eq!(outcome.status, Status::Failed);
    assert_eq!(outcome.summary.actions.total, 4);
    assert_eq!(outcome.state.facts["trial"]["completed"], 3);
    assert_eq!(outcome.summary.model_requests.total, 0);
}
#[tokio::test]
async fn local_cancel_preserves_confirmed_progress() {
    let mut adapter = DemoAdapter::default();
    let request = request(&adapter, 10);
    let runtime = TaskRuntime::new(Task::new("trial").unwrap(), RunOptions::default());
    let handle = runtime.handle();
    let outcome = runtime
        .run_request(request, &mut adapter, |event| {
            if let Event::BatchProgress(p) = event
                && p.completed == 2
            {
                handle.cancel();
            }
        })
        .await
        .unwrap();
    assert_eq!(outcome.status, Status::Cancelled);
    assert_eq!(outcome.summary.actions.total, 2);
}
#[tokio::test]
async fn local_pause_resumes_existing_batch() {
    let mut adapter = DemoAdapter::default();
    let request = request(&adapter, 3);
    let runtime = TaskRuntime::new(Task::new("trial").unwrap(), RunOptions::default());
    let handle = runtime.handle();
    let mut updates = handle.subscribe();
    let mut paused = false;
    let work = runtime.run_request(request, &mut adapter, |event| {
        if let Event::BatchProgress(p) = event
            && p.completed == 1
            && !paused
        {
            paused = true;
            handle.pause();
        }
    });
    let controller = async {
        loop {
            if updates.borrow_and_update().status == TaskStatus::Paused {
                break;
            }
            updates.changed().await.unwrap();
        }
        handle.resume();
    };
    let (outcome, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(work, controller)
    })
    .await
    .unwrap();
    assert_eq!(outcome.unwrap().summary.actions.total, 3);
}
#[test]
fn cli_feature_needs_no_key_and_rejects_bad_parameters() {
    for (parameters, success) in [("times=3", true), ("times=0", false), ("unknown=3", false)] {
        let output = Command::new(env!("CARGO_BIN_EXE_actweave"))
            .args([
                "--adapter",
                "demo",
                "--feature",
                "repeat_trial",
                "--param",
                parameters,
            ])
            .env_remove("JEVKEY")
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            success,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        if success {
            assert!(stdout.contains("大模型请求：0"));
            assert!(stdout.contains("动作调用：3"));
        } else {
            assert!(!stdout.contains("开始任务"));
        }
    }
}
