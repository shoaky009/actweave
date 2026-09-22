use actweave::{
    adapter::DemoAdapter,
    core::*,
    manual::ManualAgent,
    skills::{LoadSkills, SkillMode, SkillView},
};
use serde_json::json;
use std::{cell::Cell, collections::VecDeque, io::Cursor};

fn load(names: &[&str], tags: &[&str]) -> Decision {
    Decision::LoadSkills(LoadSkills {
        names: names.iter().map(|s| s.to_string()).collect(),
        tags: tags.iter().map(|s| s.to_string()).collect(),
    })
}
fn call(name: &str, args: serde_json::Value) -> Decision {
    Decision::Execute {
        actions: vec![Action::Call(ToolCall {
            name: name.into(),
            arguments: args,
        })],
        then: Continuation::Decide,
    }
}
fn options(limit: usize) -> RunOptions {
    RunOptions {
        max_decisions: limit,
        skill_mode: SkillMode::OnDemand,
        ..RunOptions::default()
    }
}
struct Inspector {
    steps: VecDeque<(Vec<&'static str>, Decision)>,
}
impl Agent for Inspector {
    async fn decide(
        &mut self,
        _: &Task,
        _: &AppState,
        _: &[Skill],
        _: Option<&ToolResult>,
    ) -> Result<Decision, Error> {
        panic!("discovery view required")
    }
    async fn decide_with_skills(
        &mut self,
        _: &Task,
        _: &AppState,
        view: &SkillView,
        _: Option<&ToolResult>,
    ) -> Result<Decision, Error> {
        let (expected, decision) = self.steps.pop_front().unwrap();
        assert_eq!(
            view.loaded
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert!(
            view.directory
                .iter()
                .all(|summary| !view.loaded.iter().any(|s| s.name == summary.name))
        );
        Ok(decision)
    }
}

#[tokio::test]
async fn names_and_tags_replace_selection_and_invalid_load_preserves_it() {
    let mut agent = Inspector {
        steps: VecDeque::from([
            (vec![], load(&["set_mode"], &[])),
            (vec!["set_mode"], load(&[], &["missing"])),
            (
                vec!["set_mode"],
                call("set_mode", json!({"mode":"training"})),
            ),
            (vec!["set_mode"], load(&[], &["training"])),
            (
                vec!["start_training", "get_training_status"],
                call("set_mode", json!({"mode":"idle"})),
            ),
            (
                vec!["start_training", "get_training_status"],
                Decision::Completed("mode ready".into()),
            ),
        ]),
    };
    let mut loads = vec![];
    let mut results = vec![];
    let result = run_with_options(
        &Task::new("training mode").unwrap(),
        &mut agent,
        &mut DemoAdapter::default(),
        options(8),
        |event| match event {
            Event::SkillsLoaded(result) => loads.push(result),
            Event::Executed(result) => results.push(result),
            _ => {}
        },
    )
    .await
    .unwrap();
    assert_eq!(result.status, Status::Completed);
    assert_eq!(result.state.facts["mode"], "training");
    assert_eq!(
        loads.iter().map(|r| r.success).collect::<Vec<_>>(),
        vec![true, false, true]
    );
    assert_eq!(loads[1].selected, vec!["set_mode"]);
    assert!(results[1].message.contains("not loaded"));
}

struct Revoke {
    demo: DemoAdapter,
    observations: Cell<usize>,
    executions: usize,
}
impl Adapter for Revoke {
    fn observe(&self) -> Result<AppState, AdapterError> {
        let count = self.observations.get();
        self.observations.set(count + 1);
        let mut state = self.demo.observe()?;
        if count > 0 {
            state.facts["training"]["available"] = json!(false);
        }
        Ok(state)
    }
    fn decision_context(
        &self,
        context: &SkillContext<'_>,
    ) -> Result<DecisionContext, AdapterError> {
        self.demo.decision_context(context)
    }
    async fn execute(
        &mut self,
        call: &ToolCall,
        control: &ExecutionControl,
    ) -> Result<adapter_api::ActionReport, AdapterError> {
        self.executions += 1;
        self.demo.execute(call, control).await
    }
}
#[tokio::test]
async fn loaded_skill_is_rechecked_and_cannot_bypass_revoked_availability() {
    let mut demo = DemoAdapter::default();
    demo.execute(
        &ToolCall {
            name: "set_mode".into(),
            arguments: json!({"mode":"training"}),
        },
        &ExecutionControl::default(),
    )
    .await
    .unwrap();
    let mut adapter = Revoke {
        demo,
        observations: Cell::new(0),
        executions: 0,
    };
    let mut agent = Inspector {
        steps: VecDeque::from([
            (vec![], load(&["start_training"], &[])),
            (vec![], call("start_training", json!({}))),
            (vec![], Decision::Failed("unavailable".into())),
        ]),
    };
    let mut failure = None;
    let result = run_with_options(
        &Task::new("train").unwrap(),
        &mut agent,
        &mut adapter,
        options(4),
        |e| {
            if let Event::Executed(r) = e {
                failure = Some(r)
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(result.status, Status::Failed);
    assert_eq!(adapter.executions, 0);
    assert!(failure.unwrap().message.contains("unavailable"));
}

#[tokio::test]
async fn selection_does_not_leak_between_tasks_even_with_same_agent() {
    let mut agent = ManualAgent::new(
        Cursor::new(concat!(
            "{\"LoadSkills\":{\"names\":[\"set_mode\"]}}\n{\"Completed\":\"first task\"}\n",
            "{\"Execute\":{\"then\":\"Decide\",\"actions\":[{\"Call\":{\"name\":\"set_mode\",\"arguments\":{\"mode\":\"training\"}}}]}}\n{\"Failed\":\"not loaded\"}\n"
        )),
        Vec::new(),
    );
    let task = Task::new("task").unwrap();
    run_with_options(
        &task,
        &mut agent,
        &mut DemoAdapter::default(),
        options(3),
        |_| {},
    )
    .await
    .unwrap();
    let result = run_with_options(
        &task,
        &mut agent,
        &mut DemoAdapter::default(),
        options(3),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(result.state.facts["mode"], "idle");
}
#[tokio::test]
async fn repeated_loading_is_bounded_by_shared_decision_limit() {
    let mut agent = Inspector {
        steps: VecDeque::from([
            (vec![], load(&["set_mode"], &[])),
            (vec!["set_mode"], load(&["set_mode"], &[])),
        ]),
    };
    let result = run_with_options(
        &Task::new("train").unwrap(),
        &mut agent,
        &mut DemoAdapter::default(),
        options(2),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(result.reason, "decision limit reached");
    assert_eq!(result.decisions, 2);
    assert_eq!(result.state.facts["mode"], "idle");
}
