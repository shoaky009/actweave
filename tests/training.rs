//! Deterministic feedback tests; actual JEV behavior is checked separately with live runs.
use actweave::{
    adapter::{DemoAdapter, Scenario},
    core::*,
};
use serde_json::{Value, json};
use std::time::Duration;

struct TrainingAgent;
impl Agent for TrainingAgent {
    async fn decide(
        &mut self,
        _: &Task,
        state: &AppState,
        _: &[Skill],
        previous: Option<&ToolResult>,
    ) -> Result<Decision, Error> {
        if state.facts["training"]["available"] == false {
            return Ok(Decision::Failed("training unavailable".into()));
        }
        let (name, arguments) = if state.facts["mode"] == "idle" {
            ("set_mode", json!({"mode":"training"}))
        } else if state.facts["training"]["status"] == "not_started" {
            if let Some(result) = previous.filter(|result| !result.success) {
                assert!(result.message.contains("现在可以重试"));
            }
            ("start_training", json!({}))
        } else if state.facts["training"]["status"] == "running" {
            // Let real runtime time elapse. Observation itself must not complete training.
            std::thread::sleep(Duration::from_millis(550));
            ("get_training_status", json!({}))
        } else {
            assert_eq!(state.facts["training"]["status"], "completed");
            return Ok(Decision::Completed("training finished".into()));
        };
        Ok(Decision::Execute {
            actions: vec![Action::Call(ToolCall {
                name: name.into(),
                arguments,
            })],
            then: Continuation::Decide,
        })
    }
}

#[tokio::test]
async fn normal_and_transient_runs_finish_only_after_observed_completion() {
    for (scenario, expected_failures) in [(Scenario::Normal, 0), (Scenario::TransientFailure, 1)] {
        let mut results = vec![];
        let outcome = run(
            &Task::new("完成一次训练").unwrap(),
            &mut TrainingAgent,
            &mut DemoAdapter::new(scenario),
            8,
            |event| {
                if let Event::Executed(result) = event {
                    results.push(result);
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome.status, Status::Completed);
        assert_eq!(outcome.state.facts["training"]["status"], "completed");
        assert_eq!(
            results.iter().filter(|r| !r.success).count(),
            expected_failures
        );
        assert_eq!(
            results
                .iter()
                .filter(|r| r.call.name == "start_training" && r.success)
                .count(),
            1
        );
        assert!(
            results
                .iter()
                .any(|r| r.call.name == "get_training_status" && r.success)
        );
    }
}
#[tokio::test]
async fn blocked_environment_is_reported_to_agent_and_terminates() {
    let outcome = run(
        &Task::new("完成一次训练").unwrap(),
        &mut TrainingAgent,
        &mut DemoAdapter::new(Scenario::Blocked),
        8,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Failed);
    assert_eq!(outcome.decisions, 1);
    assert_eq!(outcome.state.facts["training"]["status"], "not_started");
}
#[tokio::test]
async fn adapter_rejects_training_preconditions_and_extra_arguments_without_starting() {
    let mut adapter = DemoAdapter::default();
    let start = ToolCall {
        name: "start_training".into(),
        arguments: json!({}),
    };
    assert!(
        adapter
            .execute(&start, &ExecutionControl::default())
            .await
            .unwrap_err()
            .to_string()
            .contains("需要先")
    );
    adapter
        .execute(
            &ToolCall {
                name: "set_mode".into(),
                arguments: json!({"mode":"training"}),
            },
            &ExecutionControl::default(),
        )
        .await
        .unwrap();
    for name in ["start_training", "get_training_status"] {
        assert!(
            adapter
                .execute(
                    &ToolCall {
                        name: name.into(),
                        arguments: json!({"unexpected":true})
                    },
                    &ExecutionControl::default()
                )
                .await
                .is_err()
        );
    }
    for _ in 0..3 {
        let response = adapter
            .execute(
                &ToolCall {
                    name: "get_training_status".into(),
                    arguments: json!({}),
                },
                &ExecutionControl::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&response.message).unwrap()["training_status"],
            "not_started"
        );
    }
    assert_eq!(
        adapter.observe().unwrap().facts["training"]["status"],
        "not_started"
    );
}
