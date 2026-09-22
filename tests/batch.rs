use actweave::{
    adapter::{DemoAdapter, Scenario},
    core::*,
    manual::ManualAgent,
    skills::SkillView,
};
use adapter_api::ActionReport;
use serde_json::json;
use std::{collections::VecDeque, io::Cursor, time::Duration};

fn repeat(times: u32) -> Decision {
    Decision::Execute {
        actions: vec![Action::Repeat(RepeatRequest {
            call: ToolCall {
                name: "perform_trial".into(),
                arguments: json!({}),
            },
            times,
        })],
        then: Continuation::Finish,
    }
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
struct Script {
    decisions: VecDeque<Decision>,
    seen: Vec<Option<BatchProgress>>,
}
impl Script {
    fn new(decisions: Vec<Decision>) -> Self {
        Self {
            decisions: decisions.into(),
            seen: vec![],
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
        panic!("expected decision view")
    }
    async fn decide_with_skills(
        &mut self,
        _: &Task,
        _: &AppState,
        view: &SkillView,
        _: Option<&ToolResult>,
    ) -> Result<Decision, Error> {
        self.seen.push(view.batch.clone());
        Ok(self
            .decisions
            .pop_front()
            .expect("unexpected extra model decision"))
    }
}
fn task() -> Task {
    Task::new("完成10次试验动作").unwrap()
}

#[tokio::test]
async fn one_decision_executes_ten_actions_and_counts_normal_misses() {
    let mut agent = Script::new(vec![repeat(10)]);
    let mut progresses = vec![];
    let outcome = run(&task(), &mut agent, &mut DemoAdapter::default(), 1, |e| {
        if let Event::BatchProgress(p) = e {
            progresses.push(p)
        }
    })
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.decisions, 1);
    assert_eq!(agent.seen.len(), 1);
    let batch = outcome.batch.unwrap();
    assert_eq!(
        (
            batch.completed,
            batch.successful,
            batch.attempts,
            batch.remaining
        ),
        (10, 7, 10, Some(0))
    );
    assert_eq!(outcome.state.facts["trial"]["completed"], 10);
    assert!(
        progresses
            .iter()
            .any(|p| p.completed == 3 && p.successful == 2)
    );
}
#[tokio::test]
async fn preparation_is_outside_batch_and_requires_its_own_decision() {
    let mut agent = Script::new(vec![
        call("set_mode", json!({"mode":"training"})),
        repeat(10),
    ]);
    let outcome = run(&task(), &mut agent, &mut DemoAdapter::default(), 2, |_| {})
        .await
        .unwrap();
    assert_eq!(outcome.decisions, 2);
    assert_eq!(outcome.batch.unwrap().completed, 10);
    assert_eq!(outcome.state.facts["mode"], "training");
}
#[tokio::test]
async fn interruption_preserves_progress_and_resume_only_executes_remaining_actions() {
    let mut agent = Script::new(vec![
        repeat(10),
        call("reset_trial", json!({})),
        Decision::Resume,
    ]);
    let outcome = run(
        &task(),
        &mut agent,
        &mut DemoAdapter::new(Scenario::BatchInterrupted),
        3,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(agent.seen[1].as_ref().unwrap().completed, 3);
    assert_eq!(agent.seen[2].as_ref().unwrap().remaining, Some(7));
    let batch = outcome.batch.unwrap();
    assert_eq!(
        (batch.completed, batch.successful, batch.attempts),
        (10, 7, 11)
    );
    assert_eq!(outcome.state.facts["trial"]["completed"], 10);
}
#[tokio::test]
async fn cannot_restart_or_bypass_an_interrupted_batch() {
    let mut agent = Script::new(vec![
        repeat(10),
        repeat(10),
        call("perform_trial", json!({})),
        call("reset_trial", json!({})),
        Decision::Resume,
    ]);
    let mut errors = vec![];
    let outcome = run(
        &task(),
        &mut agent,
        &mut DemoAdapter::new(Scenario::BatchInterrupted),
        5,
        |e| {
            if let Event::ExecutionFailed(r) = e
                && r.stage == FailureStage::Validation
            {
                errors.push(r.message)
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome.batch.unwrap().completed, 10);
    assert!(errors[0].contains("execution pending"));
    assert!(errors[1].contains("Resume"));
}
#[tokio::test]
async fn invalid_repeat_counts_and_nonrepeatable_skills_never_execute() {
    for decision in [
        repeat(0),
        repeat(101),
        Decision::Execute {
            actions: vec![Action::Repeat(RepeatRequest {
                call: ToolCall {
                    name: "get_state".into(),
                    arguments: json!({}),
                },
                times: 10,
            })],
            then: Continuation::Finish,
        },
    ] {
        let mut agent = Script::new(vec![decision, Decision::Failed("invalid".into())]);
        let outcome = run(&task(), &mut agent, &mut DemoAdapter::default(), 2, |_| {})
            .await
            .unwrap();
        assert!(outcome.batch.is_none());
        assert_eq!(outcome.state.facts["trial"]["completed"], 0);
    }
}
#[tokio::test]
async fn cancellation_stops_between_actions_without_another_decision() {
    let token = CancellationToken::default();
    let cancel = token.clone();
    let mut agent = Script::new(vec![repeat(10)]);
    let outcome = run_with_options(
        &task(),
        &mut agent,
        &mut DemoAdapter::default(),
        RunOptions {
            cancellation: token,
            ..RunOptions::default()
        },
        |e| {
            if let Event::BatchProgress(p) = e
                && p.completed == 3
            {
                cancel.cancel();
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Cancelled);
    assert_eq!(outcome.batch.unwrap().completed, 3);
    assert_eq!(agent.seen.len(), 1);
}
#[tokio::test]
async fn attempt_budget_includes_failed_attempts_across_resumption() {
    let mut agent = Script::new(vec![
        repeat(10),
        call("reset_trial", json!({})),
        Decision::Resume,
    ]);
    let outcome = run_with_options(
        &task(),
        &mut agent,
        &mut DemoAdapter::new(Scenario::BatchInterrupted),
        RunOptions {
            max_batch_attempts: 10,
            ..RunOptions::default()
        },
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Failed);
    let batch = outcome.batch.unwrap();
    assert_eq!(batch.status, BatchStatus::AttemptLimit);
    assert_eq!((batch.completed, batch.attempts), (9, 10));
}

struct Cooperative {
    demo: DemoAdapter,
}
impl Adapter for Cooperative {
    fn observe(&self) -> Result<AppState, AdapterError> {
        self.demo.observe()
    }
    fn decision_context(&self, c: &SkillContext<'_>) -> Result<DecisionContext, AdapterError> {
        self.demo.decision_context(c)
    }
    async fn execute(
        &mut self,
        _: &ToolCall,
        control: &ExecutionControl,
    ) -> Result<ActionReport, AdapterError> {
        loop {
            control.check()?;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
}
#[tokio::test]
async fn cooperative_action_times_out_without_counting_an_interrupted_action() {
    let mut agent = Script::new(vec![repeat(10)]);
    let outcome = run_with_options(
        &task(),
        &mut agent,
        &mut Cooperative {
            demo: DemoAdapter::default(),
        },
        RunOptions {
            batch_timeout: Duration::from_millis(10),
            ..RunOptions::default()
        },
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Failed);
    let batch = outcome.batch.unwrap();
    assert_eq!(batch.status, BatchStatus::TimedOut);
    assert_eq!(batch.completed, 0);
}
#[tokio::test]
async fn false_completion_cannot_hide_an_interrupted_batch() {
    let mut agent = Script::new(vec![repeat(10), Decision::Completed("done".into())]);
    let outcome = run(
        &task(),
        &mut agent,
        &mut DemoAdapter::new(Scenario::BatchInterrupted),
        2,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Failed);
    assert_eq!(outcome.batch.unwrap().remaining, Some(7));
}
#[tokio::test]
async fn manual_repeat_is_one_input_line_with_no_completion_confirmation() {
    let mut agent = ManualAgent::new(
        Cursor::new(
            "{\"Execute\":{\"then\":\"Finish\",\"actions\":[{\"Repeat\":{\"call\":{\"name\":\"perform_trial\",\"arguments\":{}},\"times\":10}}]}}\n",
        ),
        Vec::new(),
    );
    let outcome = run(&task(), &mut agent, &mut DemoAdapter::default(), 1, |_| {})
        .await
        .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.batch.unwrap().completed, 10);
}
