use actweave::{
    adapter::{DemoAdapter, Scenario},
    core::*,
    manual::ManualAgent,
};
use serde_json::json;
use std::{cell::RefCell, io::Cursor};

fn resolve(adapter: &DemoAdapter, state: &AppState) -> Vec<Skill> {
    adapter
        .decision_context(&SkillContext {
            state,
            task_goal: "完成训练",
            decision_step: 1,
            previous_result: None,
            failure: None,
            interruption: None,
        })
        .unwrap()
        .skills
}
fn skill<'a>(skills: &'a [Skill], name: &str) -> &'a Skill {
    skills.iter().find(|s| s.name == name).unwrap()
}
fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments,
    }
}

#[tokio::test]
async fn adapter_uses_supplied_context_and_execution_rechecks_runtime() {
    let mut adapter = DemoAdapter::default();
    let initial = adapter.observe().unwrap();
    let skills = resolve(&adapter, &initial);
    assert!(
        matches!(&skill(&skills,"start_training").availability,Availability::Unavailable{reason} if reason.contains("set_mode"))
    );
    assert_eq!(
        skill(&skills, "start_training").tags,
        vec!["training", "write"]
    );
    adapter
        .execute(
            &call("set_mode", json!({"mode":"training"})),
            &ExecutionControl::default(),
        )
        .await
        .unwrap();
    // Resolving an old context must not silently mix observations from different rounds.
    assert!(
        !skill(&resolve(&adapter, &initial), "start_training")
            .availability
            .is_available()
    );
    let ready = adapter.observe().unwrap();
    assert!(
        skill(&resolve(&adapter, &ready), "start_training")
            .availability
            .is_available()
    );
    adapter
        .execute(
            &call("set_mode", json!({"mode":"idle"})),
            &ExecutionControl::default(),
        )
        .await
        .unwrap();
    // A previously available skill is not authorization to bypass current preconditions.
    assert!(
        adapter
            .execute(
                &call("start_training", json!({})),
                &ExecutionControl::default()
            )
            .await
            .is_err()
    );
    assert_eq!(
        adapter.observe().unwrap().facts["training"]["status"],
        "not_started"
    );
}

#[test]
fn unavailable_reasons_and_candidates_follow_training_state() {
    let adapter = DemoAdapter::default();
    let mut context_state = adapter.observe().unwrap();
    context_state.facts["mode"] = json!("training");
    context_state.facts["training"]["status"] = json!("running");
    let running = resolve(&adapter, &context_state);
    for name in ["set_mode", "start_training"] {
        assert!(!skill(&running, name).availability.is_available());
        // Candidate arguments describe the capability; they do not grant availability.
        assert!(!skill(&running, name).calls.is_empty());
    }
    assert!(
        skill(&running, "get_training_status")
            .availability
            .is_available()
    );
    context_state.facts["training"]["status"] = json!("completed");
    let completed = resolve(&adapter, &context_state);
    assert!(skill(&completed, "set_mode").availability.is_available());
    assert!(
        !skill(&completed, "start_training")
            .availability
            .is_available()
    );
    let blocked = DemoAdapter::new(Scenario::Blocked);
    let skills = resolve(&blocked, &blocked.observe().unwrap());
    assert!(
        matches!(&skill(&skills,"start_training").availability,Availability::Unavailable{reason} if reason.contains("永久不可用"))
    );
}

#[test]
fn malformed_context_is_an_error_instead_of_assumed_availability() {
    let adapter = DemoAdapter::default();
    let state = AppState {
        scene: "demo".into(),
        facts: json!({}),
    };
    assert!(
        adapter
            .decision_context(&SkillContext {
                state: &state,
                task_goal: "grant permission",
                decision_step: 1,
                previous_result: None,
                failure: None,
                interruption: None,
            })
            .is_err()
    );
}

#[derive(Debug, PartialEq)]
struct Seen {
    step: usize,
    goal: String,
    mode: String,
    previous_success: Option<bool>,
}
#[derive(Default)]
struct Recording {
    demo: DemoAdapter,
    seen: RefCell<Vec<Seen>>,
    executed: Vec<String>,
}
impl Adapter for Recording {
    fn observe(&self) -> Result<AppState, AdapterError> {
        self.demo.observe()
    }
    fn decision_context(
        &self,
        context: &SkillContext<'_>,
    ) -> Result<DecisionContext, AdapterError> {
        self.seen.borrow_mut().push(Seen {
            step: context.decision_step,
            goal: context.task_goal.into(),
            mode: context.state.facts["mode"].as_str().unwrap().into(),
            previous_success: context.previous_result.map(|r| r.success),
        });
        self.demo.decision_context(context)
    }
    async fn execute(
        &mut self,
        call: &ToolCall,
        control: &ExecutionControl,
    ) -> Result<adapter_api::ActionReport, AdapterError> {
        self.executed.push(call.name.clone());
        self.demo.execute(call, control).await
    }
}

#[tokio::test]
async fn core_passes_current_context_and_honors_adapter_unavailability() {
    let mut adapter = Recording::default();
    let input = concat!(
        "{\"Execute\":{\"then\":\"Decide\",\"actions\":[{\"Call\":{\"name\":\"start_training\",\"arguments\":{}}}]}}\n",
        "{\"Execute\":{\"then\":\"Decide\",\"actions\":[{\"Call\":{\"name\":\"set_mode\",\"arguments\":{\"mode\":\"training\"}}}]}}\n",
        "{\"Completed\":\"mode ready\"}\n"
    );
    let mut agent = ManualAgent::new(Cursor::new(input), Vec::new());
    let mut published = vec![];
    let mut failures = vec![];
    let result = run(
        &Task::new("training mode").unwrap(),
        &mut agent,
        &mut adapter,
        4,
        |event| match event {
            Event::SkillsResolved { step, skills, .. } => published.push((
                step,
                skill(&skills, "start_training").availability.is_available(),
            )),
            Event::Executed(result) if !result.success => failures.push(result.message),
            _ => {}
        },
    )
    .await
    .unwrap();
    assert_eq!(result.status, Status::Completed);
    assert_eq!(adapter.executed, vec!["set_mode"]);
    assert_eq!(published, vec![(1, false), (2, false), (3, true)]);
    assert!(failures[0].contains("set_mode"));
    assert_eq!(
        *adapter.seen.borrow(),
        vec![
            Seen {
                step: 1,
                goal: "training mode".into(),
                mode: "idle".into(),
                previous_success: None
            },
            Seen {
                step: 1,
                goal: "training mode".into(),
                mode: "idle".into(),
                previous_success: None
            },
            Seen {
                step: 2,
                goal: "training mode".into(),
                mode: "idle".into(),
                previous_success: Some(false)
            },
            Seen {
                step: 2,
                goal: "training mode".into(),
                mode: "idle".into(),
                previous_success: Some(false)
            },
            Seen {
                step: 3,
                goal: "training mode".into(),
                mode: "training".into(),
                previous_success: Some(true)
            },
        ]
    );
}
