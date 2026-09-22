use actweave::{
    adapter::{DemoAdapter, Scenario},
    core::*,
    skills::SkillView,
};
use adapter_api::ActionReport;
use serde_json::json;
use std::collections::VecDeque;

fn call(name: &str, arguments: serde_json::Value, then: Continuation) -> Decision {
    Decision::Execute {
        actions: vec![Action::Call(ToolCall {
            name: name.into(),
            arguments,
        })],
        then,
    }
}
fn repeat(times: u32, then: Continuation) -> Decision {
    Decision::Execute {
        actions: vec![Action::Repeat(RepeatRequest {
            call: ToolCall {
                name: "perform_trial".into(),
                arguments: json!({}),
            },
            times,
        })],
        then,
    }
}
struct Script {
    decisions: VecDeque<Decision>,
    pending: Vec<Option<BatchProgress>>,
    feedback: Vec<Option<ToolResult>>,
}
impl Script {
    fn new(decisions: Vec<Decision>) -> Self {
        Self {
            decisions: decisions.into(),
            pending: vec![],
            feedback: vec![],
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
        self.pending.push(view.batch.clone());
        self.feedback.push(previous.cloned());
        Ok(self
            .decisions
            .pop_front()
            .expect("unexpected extra model call"))
    }
}
async fn execute(agent: &mut Script, adapter: &mut impl Adapter) -> Outcome {
    run(
        &Task::new("generic task").unwrap(),
        agent,
        adapter,
        8,
        |_| {},
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn finish_returns_fresh_state_without_confirmation_decision() {
    let mut agent = Script::new(vec![call(
        "set_mode",
        json!({"mode":"training"}),
        Continuation::Finish,
    )]);
    let outcome = execute(&mut agent, &mut DemoAdapter::default()).await;
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.decisions, 1);
    assert_eq!(outcome.state.facts["mode"], "training");
}

#[tokio::test]
async fn failed_finish_returns_feedback_and_requires_a_new_decision() {
    let mut agent = Script::new(vec![
        call("set_mode", json!({"mode":"invalid"}), Continuation::Finish),
        call("set_mode", json!({"mode":"training"}), Continuation::Finish),
    ]);
    let outcome = execute(&mut agent, &mut DemoAdapter::default()).await;
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.decisions, 2);
    assert!(!agent.feedback[1].as_ref().unwrap().success);
}

struct Interrupted(DemoAdapter);
impl Adapter for Interrupted {
    fn observe(&self) -> Result<AppState, AdapterError> {
        self.0.observe()
    }
    fn decision_context(
        &self,
        context: &SkillContext<'_>,
    ) -> Result<DecisionContext, AdapterError> {
        self.0.decision_context(context)
    }
    async fn execute(
        &mut self,
        _: &ToolCall,
        control: &ExecutionControl,
    ) -> Result<ActionReport, AdapterError> {
        control.check()?;
        Ok(ActionReport {
            outcome: ActionOutcome::Interrupted,
            message: "needs new decision".into(),
        })
    }
}
#[tokio::test]
async fn interrupted_finish_never_marks_task_complete() {
    let mut agent = Script::new(vec![
        call("get_state", json!({}), Continuation::Finish),
        Decision::Failed("cannot proceed".into()),
    ]);
    let outcome = execute(&mut agent, &mut Interrupted(DemoAdapter::default())).await;
    assert_eq!(outcome.status, Status::Failed);
    assert_eq!(outcome.decisions, 2);
    assert!(matches!(
        agent.feedback[1].as_ref().unwrap().outcome,
        Some(ActionOutcome::Interrupted)
    ));
}

#[tokio::test]
async fn normal_miss_is_completed_action_not_interruption() {
    let mut agent = Script::new(vec![
        call("perform_trial", json!({}), Continuation::Decide),
        call("perform_trial", json!({}), Continuation::Decide),
        call("perform_trial", json!({}), Continuation::Finish),
    ]);
    let outcome = execute(&mut agent, &mut DemoAdapter::default()).await;
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.state.facts["trial"]["completed"], 3);
    assert_eq!(outcome.state.facts["trial"]["successful"], 2);
}

#[tokio::test]
async fn completed_decide_batch_clears_pending_and_allows_another_batch() {
    let mut agent = Script::new(vec![
        repeat(2, Continuation::Decide),
        repeat(3, Continuation::Finish),
    ]);
    let outcome = execute(&mut agent, &mut DemoAdapter::default()).await;
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.decisions, 2);
    assert!(agent.pending[1].is_none());
    assert!(agent.feedback[1].as_ref().unwrap().success);
    assert_eq!(outcome.state.facts["trial"]["completed"], 5);
}

#[tokio::test]
async fn resumed_batch_preserves_decide_and_executes_following_action() {
    let mut agent = Script::new(vec![
        repeat(5, Continuation::Decide),
        call("reset_trial", json!({}), Continuation::Decide),
        Decision::Resume,
        call("set_mode", json!({"mode":"training"}), Continuation::Finish),
    ]);
    let outcome = execute(
        &mut agent,
        &mut DemoAdapter::new(Scenario::BatchInterrupted),
    )
    .await;
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(
        agent.pending[2].as_ref().unwrap().then,
        Continuation::Decide
    );
    assert!(agent.pending[3].is_none());
    assert_eq!(outcome.state.facts["trial"]["completed"], 5);
    assert_eq!(outcome.state.facts["mode"], "training");
}

#[tokio::test]
async fn recovery_cannot_finish_while_batch_is_pending() {
    let mut agent = Script::new(vec![
        repeat(5, Continuation::Finish),
        call("reset_trial", json!({}), Continuation::Finish),
        call("reset_trial", json!({}), Continuation::Decide),
        Decision::Resume,
    ]);
    let outcome = execute(
        &mut agent,
        &mut DemoAdapter::new(Scenario::BatchInterrupted),
    )
    .await;
    assert!(!agent.feedback[2].as_ref().unwrap().success);
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.batch.unwrap().then, Continuation::Finish);
    assert_eq!(outcome.state.facts["trial"]["completed"], 5);
}

#[test]
fn execution_requires_explicit_continuation_and_rejects_old_format() {
    for value in [
        json!({"Call":{"name":"get_state","arguments":{}}}),
        json!({"Repeat":{"call":{"name":"perform_trial","arguments":{}},"times":2}}),
        json!({"Execute":{"action":{"Call":{"name":"get_state","arguments":{}}}}}),
        json!({"Execute":{"action":{"Call":{"name":"get_state","arguments":{}}},"then":"unknown"}}),
    ] {
        assert!(serde_json::from_value::<Decision>(value).is_err());
    }
}
