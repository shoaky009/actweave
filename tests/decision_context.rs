use actweave::{
    adapter::{DemoAdapter, Scenario},
    core::*,
    skills::SkillView,
};
use adapter_api::ActionReport;
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
};

fn call(name: &str) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: json!({}),
    }
}
fn set_mode() -> Action {
    Action::Call(ToolCall {
        name: "set_mode".into(),
        arguments: json!({"mode":"training"}),
    })
}
fn execute(actions: Vec<Action>, then: Continuation) -> Decision {
    Decision::Execute { actions, then }
}
fn explore(then: Continuation) -> Decision {
    execute(vec![Action::UntilDone(call("explore_step"))], then)
}
fn repair() -> Decision {
    execute(
        vec![Action::Call(call("reset_exploration"))],
        Continuation::Decide,
    )
}
#[derive(Clone)]
struct Feedback {
    previous: Option<ToolResult>,
    failure: Option<ExecutionFailure>,
    interruption: Option<ExecutionFailure>,
}
struct Frame {
    guidance: String,
    feedback: Feedback,
    execution: Option<ExecutionProgress>,
}
struct Script {
    decisions: VecDeque<Decision>,
    frames: Vec<Frame>,
}
impl Script {
    fn new(decisions: Vec<Decision>) -> Self {
        Self {
            decisions: decisions.into(),
            frames: vec![],
        }
    }
}
impl Agent for Script {
    async fn decide(
        &mut self,
        _: &Task,
        _: &AppState,
        _: &[Skill],
        _: Option<&ToolResult>,
    ) -> Result<Decision, Error> {
        unreachable!()
    }
    async fn decide_with_skills(
        &mut self,
        _: &Task,
        _: &AppState,
        view: &SkillView,
        previous: Option<&ToolResult>,
    ) -> Result<Decision, Error> {
        self.frames.push(Frame {
            guidance: view.guidance.clone(),
            feedback: Feedback {
                previous: previous.cloned(),
                failure: view.failure.clone(),
                interruption: view.execution.as_ref().and_then(|e| e.failure.clone()),
            },
            execution: view.execution.clone(),
        });
        Ok(self.decisions.pop_front().expect("unexpected model call"))
    }
}
struct RecordingAdapter {
    demo: DemoAdapter,
    fail_after: Option<&'static str>,
    fail_next: Cell<bool>,
    calls: Vec<String>,
    feedback: RefCell<Vec<Feedback>>,
}
impl RecordingAdapter {
    fn new(scenario: Scenario, fail_after: Option<&'static str>) -> Self {
        Self {
            demo: DemoAdapter::new(scenario),
            fail_after,
            fail_next: Cell::new(false),
            calls: vec![],
            feedback: RefCell::new(vec![]),
        }
    }
}
impl Adapter for RecordingAdapter {
    fn observe(&self) -> Result<AppState, AdapterError> {
        if self.fail_next.replace(false) {
            Err(AdapterError::Runtime("observation lost".into()))
        } else {
            self.demo.observe()
        }
    }
    fn decision_context(
        &self,
        context: &SkillContext<'_>,
    ) -> Result<DecisionContext, AdapterError> {
        self.feedback.borrow_mut().push(Feedback {
            previous: context.previous_result.cloned(),
            failure: context.failure.cloned(),
            interruption: context.interruption.cloned(),
        });
        self.demo.decision_context(context)
    }
    async fn execute(
        &mut self,
        call: &ToolCall,
        control: &ExecutionControl,
    ) -> Result<ActionReport, AdapterError> {
        self.calls.push(call.name.clone());
        let report = self.demo.execute(call, control).await?;
        if self.fail_after == Some(call.name.as_str()) {
            self.fail_after = None;
            self.fail_next.set(true);
        }
        Ok(report)
    }
}

#[tokio::test]
async fn guidance_uses_supplied_state_and_replaces_previous_stage() {
    let mut adapter = DemoAdapter::default();
    let initial = adapter.observe().unwrap();
    let resolve = |adapter: &DemoAdapter, state: &AppState| {
        adapter
            .decision_context(&SkillContext {
                state,
                task_goal: "start training",
                decision_step: 1,
                previous_result: None,
                failure: None,
                interruption: None,
            })
            .unwrap()
    };
    let idle = resolve(&adapter, &initial).guidance;
    adapter
        .execute(set_mode().call(), &ExecutionControl::default())
        .await
        .unwrap();
    let ready = resolve(&adapter, &adapter.observe().unwrap()).guidance;
    assert_ne!(idle, ready);
    assert!(ready.contains("前置条件已满足"));
    assert!(!ready.contains(&idle));
    assert_eq!(resolve(&adapter, &initial).guidance, idle);
}

#[tokio::test]
async fn successful_repair_preserves_original_interruption_for_adapter_and_model() {
    let mut adapter = RecordingAdapter::new(Scenario::ExplorationInterrupted, None);
    let mut agent = Script::new(vec![
        explore(Continuation::Decide),
        repair(),
        Decision::Resume,
        Decision::Completed("exploration finished".into()),
    ]);
    let outcome = run(
        &Task::new("explore").unwrap(),
        &mut agent,
        &mut adapter,
        4,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert!(agent.frames[1].guidance.contains("reset_exploration"));
    assert!(agent.frames[2].guidance.contains("继续原先中断"));
    assert!(!agent.frames[2].guidance.contains("reset_exploration"));
    assert!(agent.frames[2].feedback.failure.is_none());
    assert_eq!(
        agent.frames[2]
            .feedback
            .interruption
            .as_ref()
            .unwrap()
            .stage,
        FailureStage::Execution
    );
    assert!(adapter.feedback.borrow().iter().any(|f| {
        f.failure.is_none()
            && f.previous
                .as_ref()
                .is_some_and(|r| r.success && r.call.name == "reset_exploration")
            && f.interruption
                .as_ref()
                .is_some_and(|i| i.call.as_ref().unwrap().name == "explore_step")
    }));
    assert!(agent.frames[3].feedback.failure.is_none());
    assert!(agent.frames[3].feedback.interruption.is_none());
    assert!(agent.frames[3].guidance.contains("已确认探索完成"));
    assert!(!agent.frames[3].guidance.contains("继续原先中断"));
}

#[tokio::test]
async fn successful_single_action_with_observation_failure_is_not_replayed() {
    let mut adapter = RecordingAdapter::new(Scenario::Normal, Some("set_mode"));
    let mut agent = Script::new(vec![
        execute(vec![set_mode()], Continuation::Finish),
        Decision::Resume,
    ]);
    let mut failures = vec![];
    let outcome = run(
        &Task::new("switch mode").unwrap(),
        &mut agent,
        &mut adapter,
        2,
        |event| {
            if let Event::ExecutionFailed(failure) = event {
                failures.push(failure);
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(adapter.calls, ["set_mode"]);
    let frame = &agent.frames[1];
    assert!(frame.feedback.previous.as_ref().unwrap().success);
    assert_eq!(
        frame.feedback.failure.as_ref().unwrap().stage,
        FailureStage::Observation
    );
    assert_eq!(
        frame
            .feedback
            .failure
            .as_ref()
            .unwrap()
            .call
            .as_ref()
            .unwrap()
            .arguments,
        json!({"mode":"training"})
    );
    assert_eq!(frame.execution.as_ref().unwrap().completed, 1);
    assert!(frame.guidance.contains("不能重做"));
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].stage, FailureStage::Observation);
}

#[tokio::test]
async fn repair_observation_failure_does_not_replace_original_pending_work() {
    let mut adapter =
        RecordingAdapter::new(Scenario::ExplorationInterrupted, Some("reset_exploration"));
    let mut agent = Script::new(vec![
        explore(Continuation::Finish),
        repair(),
        Decision::Resume,
    ]);
    let outcome = run(
        &Task::new("explore").unwrap(),
        &mut agent,
        &mut adapter,
        3,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    let frame = &agent.frames[2];
    assert_eq!(
        frame.feedback.failure.as_ref().unwrap().stage,
        FailureStage::Observation
    );
    assert_eq!(
        frame.feedback.interruption.as_ref().unwrap().stage,
        FailureStage::Execution
    );
    assert_eq!(
        frame.execution.as_ref().unwrap().actions[0].call().name,
        "explore_step"
    );
    assert!(frame.feedback.previous.as_ref().unwrap().success);
    assert_eq!(
        adapter
            .calls
            .iter()
            .filter(|name| *name == "reset_exploration")
            .count(),
        1
    );
    assert_eq!(outcome.batch.unwrap().attempts, 5);
}

#[tokio::test]
async fn actual_execution_failure_drives_retry_guidance_without_inventing_completion() {
    let mut adapter = RecordingAdapter::new(Scenario::TransientFailure, None);
    let mut agent = Script::new(vec![
        execute(vec![set_mode()], Continuation::Decide),
        execute(
            vec![Action::Call(call("start_training"))],
            Continuation::Decide,
        ),
        execute(
            vec![Action::Call(call("start_training"))],
            Continuation::Finish,
        ),
    ]);
    let outcome = run(
        &Task::new("start training").unwrap(),
        &mut agent,
        &mut adapter,
        3,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    let frame = &agent.frames[2];
    assert!(!frame.feedback.previous.as_ref().unwrap().success);
    assert_eq!(
        frame.feedback.failure.as_ref().unwrap().stage,
        FailureStage::Execution
    );
    assert!(frame.guidance.contains("上次启动未成功"));
    assert!(adapter.feedback.borrow().iter().any(|f| {
        f.failure
            .as_ref()
            .is_some_and(|e| e.stage == FailureStage::Execution)
    }));
}

#[tokio::test]
async fn validation_identifies_the_bad_action_and_availability_is_distinct() {
    let mut adapter = RecordingAdapter::new(Scenario::Normal, None);
    let mut agent = Script::new(vec![
        execute(
            vec![set_mode(), Action::Call(call("missing"))],
            Continuation::Finish,
        ),
        execute(
            vec![Action::Call(call("start_training"))],
            Continuation::Finish,
        ),
        Decision::Failed("unavailable".into()),
    ]);
    run(
        &Task::new("start training").unwrap(),
        &mut agent,
        &mut adapter,
        3,
        |_| {},
    )
    .await
    .unwrap();
    assert!(adapter.calls.is_empty());
    let invalid = agent.frames[1].feedback.failure.as_ref().unwrap();
    assert_eq!(invalid.stage, FailureStage::Validation);
    assert_eq!(invalid.call.as_ref().unwrap().name, "missing");
    let unavailable = agent.frames[2].feedback.failure.as_ref().unwrap();
    assert_eq!(unavailable.stage, FailureStage::Availability);
    assert_eq!(unavailable.call.as_ref().unwrap().name, "start_training");
}
