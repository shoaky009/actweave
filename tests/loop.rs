use actweave::{adapter::DemoAdapter, core::*, runtime::Mode};
use serde_json::json;
use std::collections::VecDeque;

struct Script(VecDeque<Decision>);
impl Agent for Script {
    async fn decide(
        &mut self,
        _: &Task,
        _: &AppState,
        _: &[Skill],
        _: Option<&ToolResult>,
    ) -> Result<Decision, Error> {
        self.0
            .pop_front()
            .ok_or_else(|| Error::Model("script exhausted".into()))
    }
}
fn call(name: &str, arguments: serde_json::Value) -> Decision {
    Decision::Execute {
        actions: vec![Action::Call(ToolCall {
            name: name.into(),
            arguments,
        })],
        then: Continuation::Decide,
    }
}
#[tokio::test]
async fn observes_actual_runtime_change_before_completion() {
    let mut environment = DemoAdapter::default();
    let mut agent = Script(VecDeque::from([
        call("set_mode", json!({"mode":"training"})),
        Decision::Completed("done".into()),
    ]));
    let mut observations = vec![];
    let outcome = run(
        &Task::new("训练模式").unwrap(),
        &mut agent,
        &mut environment,
        3,
        |e| {
            if let Event::Observed(s) = e {
                observations.push(s.facts["mode"].clone())
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(
        observations,
        vec![json!(Mode::Idle), json!(Mode::Idle), json!(Mode::Training)]
    );
}
#[tokio::test]
async fn failed_tool_is_returned_to_agent_for_replanning() {
    struct Recovery {
        count: usize,
    }
    impl Agent for Recovery {
        async fn decide(
            &mut self,
            _: &Task,
            state: &AppState,
            _: &[Skill],
            previous: Option<&ToolResult>,
        ) -> Result<Decision, Error> {
            self.count += 1;
            Ok(match self.count {
                1 => call("set_mode", json!({"mode":"invalid"})),
                2 => {
                    assert!(!previous.unwrap().success);
                    assert_eq!(state.facts["mode"], "idle");
                    call("set_mode", json!({"mode":"training"}))
                }
                _ => {
                    assert!(previous.unwrap().success);
                    assert_eq!(state.facts["mode"], "training");
                    Decision::Completed("done".into())
                }
            })
        }
    }
    let outcome = run(
        &Task::new("training").unwrap(),
        &mut Recovery { count: 0 },
        &mut DemoAdapter::default(),
        3,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Completed);
}
#[tokio::test]
async fn repeated_actions_stop_at_budget() {
    let mut agent = Script(VecDeque::from([call("get_state", json!({}))]));
    let outcome = run(
        &Task::new("training").unwrap(),
        &mut agent,
        &mut DemoAdapter::default(),
        1,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Failed);
    assert_eq!(outcome.reason, "decision limit reached");
}
#[tokio::test]
async fn explicit_model_failure_is_terminal() {
    let mut agent = Script(VecDeque::from([Decision::Failed(
        "unsupported task".into(),
    )]));
    let outcome = run(
        &Task::new("unsupported").unwrap(),
        &mut agent,
        &mut DemoAdapter::default(),
        2,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Failed);
}
#[tokio::test]
async fn invalid_tools_do_not_mutate_runtime() {
    let mut environment = DemoAdapter::default();
    for (name, args) in [
        ("press_key", json!({})),
        ("set_mode", json!({"mode":"training","extra":true})),
        ("set_mode", json!({"mode":"unknown"})),
        ("get_state", json!({"extra":1})),
    ] {
        assert!(
            environment
                .execute(
                    &ToolCall {
                        name: name.into(),
                        arguments: args
                    },
                    &ExecutionControl::default()
                )
                .await
                .is_err()
        );
        assert_eq!(environment.observe().unwrap().facts["mode"], "idle");
    }
}
