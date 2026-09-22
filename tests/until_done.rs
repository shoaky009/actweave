use actweave::{
    adapter::{DemoAdapter, Scenario},
    core::*,
    skills::{LoadSkills, SkillMode, SkillView},
    task_runtime::{TaskRuntime, TaskStatus},
};
use adapter_api::ActionReport;
use serde_json::json;
use std::{cell::Cell, collections::VecDeque, time::Instant};

fn call(name: &str) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: json!({}),
    }
}
fn explore() -> Action {
    Action::UntilDone(call("explore_step"))
}
fn execute(actions: Vec<Action>, then: Continuation) -> Decision {
    Decision::Execute { actions, then }
}
fn repair() -> Decision {
    execute(
        vec![Action::Call(call("reset_exploration"))],
        Continuation::Decide,
    )
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
        unreachable!()
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
            .expect("unexpected model decision"))
    }
}

#[tokio::test]
async fn unknown_work_count_finishes_locally_with_one_decision() {
    let mut agent = Script::new(vec![execute(vec![explore()], Continuation::Finish)]);
    let result = run(
        &Task::new("explore everything").unwrap(),
        &mut agent,
        &mut DemoAdapter::new(Scenario::Exploration),
        1,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(result.status, Status::Completed);
    assert_eq!(result.decisions, 1);
    assert_eq!(result.state.facts["exploration"]["collected"], 3);
    assert_eq!(result.state.facts["exploration"]["finished"], true);
    let batch = result.batch.unwrap();
    assert_eq!(batch.remaining, None);
    // Three collection steps and one final completion check, not four collected items.
    assert_eq!(
        (batch.completed, batch.successful, batch.attempts),
        (4, 3, 4)
    );
    assert!(batch.finished);
}

#[tokio::test]
async fn loop_inside_sequence_runs_tail_then_obeys_decide() {
    let mut agent = Script::new(vec![
        execute(
            vec![
                Action::Call(call("get_state")),
                explore(),
                Action::Call(call("get_state")),
            ],
            Continuation::Decide,
        ),
        Decision::Completed("done".into()),
    ]);
    let mut started = vec![];
    let result = run(
        &Task::new("explore and inspect").unwrap(),
        &mut agent,
        &mut DemoAdapter::new(Scenario::Exploration),
        2,
        |event| {
            if let Event::ActionStarted { call, .. } = event {
                started.push(call.name);
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(result.status, Status::Completed);
    assert_eq!(started, ["get_state", "explore_step", "get_state"]);
    assert_eq!(agent.seen.len(), 2);
    assert!(agent.seen[1].is_none());
}

#[tokio::test]
async fn interruption_repairs_and_resumes_with_original_progress() {
    let mut agent = Script::new(vec![
        execute(vec![explore()], Continuation::Finish),
        repair(),
        Decision::Resume,
    ]);
    let result = run(
        &Task::new("explore everything").unwrap(),
        &mut agent,
        &mut DemoAdapter::new(Scenario::ExplorationInterrupted),
        3,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(result.status, Status::Completed);
    assert_eq!(agent.seen[1].as_ref().unwrap().completed, 1);
    assert_eq!(agent.seen[2].as_ref().unwrap().attempts, 2);
    assert_eq!(result.batch.unwrap().attempts, 5);
    assert_eq!(result.state.facts["exploration"]["collected"], 3);
}

#[tokio::test]
async fn budget_exhaustion_is_not_completion_even_after_last_item_is_collected() {
    let mut agent = Script::new(vec![execute(vec![explore()], Continuation::Finish)]);
    let result = run_with_options(
        &Task::new("explore everything").unwrap(),
        &mut agent,
        &mut DemoAdapter::new(Scenario::Exploration),
        RunOptions {
            max_batch_attempts: 3,
            ..RunOptions::default()
        },
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(result.status, Status::Failed);
    assert_eq!(result.state.facts["exploration"]["collected"], 3);
    let batch = result.batch.unwrap();
    assert_eq!(batch.status, BatchStatus::AttemptLimit);
    assert!(!batch.finished);
    assert_eq!(agent.seen.len(), 1);
}

#[tokio::test]
async fn replacement_cannot_discard_loop_or_reset_its_budget() {
    let mut agent = Script::new(vec![
        execute(vec![explore()], Continuation::Finish),
        Decision::ReplaceRemaining {
            actions: vec![Action::Call(call("get_state"))],
            then: Continuation::Finish,
        },
        repair(),
        Decision::ReplaceRemaining {
            actions: vec![explore(), Action::Call(call("get_state"))],
            then: Continuation::Finish,
        },
    ]);
    let result = run_with_options(
        &Task::new("explore everything").unwrap(),
        &mut agent,
        &mut DemoAdapter::new(Scenario::ExplorationInterrupted),
        RunOptions {
            max_batch_attempts: 4,
            ..RunOptions::default()
        },
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(result.status, Status::Failed);
    assert_eq!(agent.seen[2].as_ref().unwrap().attempts, 2);
    assert_eq!(result.batch.unwrap().attempts, 4);
    assert_eq!(result.execution.unwrap().completed, 0);
}

#[tokio::test]
async fn loop_requires_capability_and_on_demand_selection_before_effects() {
    let mut agent = Script::new(vec![
        Decision::LoadSkills(LoadSkills {
            names: vec!["get_state".into()],
            tags: vec![],
        }),
        execute(
            vec![Action::UntilDone(call("get_state"))],
            Continuation::Finish,
        ),
        execute(vec![explore()], Continuation::Finish),
        Decision::LoadSkills(LoadSkills {
            names: vec!["explore_step".into()],
            tags: vec![],
        }),
        execute(vec![explore()], Continuation::Finish),
    ]);
    let result = run_with_options(
        &Task::new("explore everything").unwrap(),
        &mut agent,
        &mut DemoAdapter::new(Scenario::Exploration),
        RunOptions {
            skill_mode: SkillMode::OnDemand,
            ..RunOptions::default()
        },
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(result.status, Status::Completed);
    assert_eq!(result.batch.unwrap().attempts, 4);
    assert!(agent.seen.iter().all(Option::is_none));
}

/// Observations can fail after an effect. Completion also revokes the capability.
struct ObservationFault {
    demo: DemoAdapter,
    fail_after: usize,
    calls: usize,
    fail_next: Cell<bool>,
    revoke_after: usize,
    deadlines: Vec<Option<Instant>>,
}
impl Adapter for ObservationFault {
    fn observe(&self) -> Result<AppState, AdapterError> {
        if self.fail_next.replace(false) {
            Err(AdapterError::Runtime("observation unavailable".into()))
        } else {
            self.demo.observe()
        }
    }
    fn decision_context(
        &self,
        context: &SkillContext<'_>,
    ) -> Result<DecisionContext, AdapterError> {
        let mut skills = self.demo.decision_context(context)?;
        if self.calls >= self.revoke_after {
            skills
                .skills
                .iter_mut()
                .find(|s| s.name == "explore_step")
                .unwrap()
                .availability = Availability::Unavailable {
                reason: "revoked".into(),
            };
        }
        Ok(skills)
    }
    async fn execute(
        &mut self,
        call: &ToolCall,
        control: &ExecutionControl,
    ) -> Result<ActionReport, AdapterError> {
        self.calls += 1;
        self.deadlines.push(control.deadline);
        let report = self.demo.execute(call, control).await?;
        if self.calls == self.fail_after {
            self.fail_next.set(true);
        }
        Ok(report)
    }
}
fn faulty(fail_after: usize, revoke_after: usize) -> ObservationFault {
    ObservationFault {
        demo: DemoAdapter::new(Scenario::Exploration),
        fail_after,
        calls: 0,
        fail_next: Cell::new(false),
        revoke_after,
        deadlines: vec![],
    }
}

#[tokio::test]
async fn observation_failures_preserve_steps_terminal_signal_and_original_deadline() {
    for fail_after in [1, 4] {
        let mut adapter = faulty(fail_after, 4);
        let mut agent = Script::new(vec![
            execute(vec![explore()], Continuation::Finish),
            Decision::Resume,
        ]);
        let result = run(
            &Task::new("explore everything").unwrap(),
            &mut agent,
            &mut adapter,
            2,
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(result.status, Status::Completed);
        assert_eq!(adapter.calls, 4);
        assert_eq!(agent.seen[1].as_ref().unwrap().completed, fail_after as u32);
        assert_eq!(agent.seen[1].as_ref().unwrap().finished, fail_after == 4);
        assert!(adapter.deadlines[0].is_some());
        assert!(adapter.deadlines.iter().all(|d| *d == adapter.deadlines[0]));
    }
}

#[tokio::test]
async fn revoked_availability_interrupts_instead_of_finishing() {
    let mut adapter = faulty(usize::MAX, 1);
    let mut agent = Script::new(vec![
        execute(vec![explore()], Continuation::Finish),
        Decision::Failed("capability revoked".into()),
    ]);
    let result = run(
        &Task::new("explore everything").unwrap(),
        &mut agent,
        &mut adapter,
        2,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(result.status, Status::Failed);
    assert_eq!(adapter.calls, 1);
    let pending = agent.seen[1].as_ref().unwrap();
    assert_eq!(pending.status, BatchStatus::Interrupted);
    assert!(!pending.finished);
}

#[tokio::test(flavor = "current_thread")]
async fn pause_resume_and_cancel_control_unknown_length_loop_without_model_calls() {
    for cancel in [false, true] {
        let runtime = TaskRuntime::new(
            Task::new("explore everything").unwrap(),
            RunOptions::default(),
        );
        let handle = runtime.handle();
        let mut updates = handle.subscribe();
        let mut agent = Script::new(vec![execute(vec![explore()], Continuation::Finish)]);
        let mut adapter = DemoAdapter::new(Scenario::Exploration);
        let work = runtime.run(&mut agent, &mut adapter, |event| {
            if let Event::BatchProgress(progress) = event
                && progress.completed == 1
            {
                handle.pause();
            }
        });
        let control = async {
            loop {
                if updates.borrow_and_update().status == TaskStatus::Paused {
                    break;
                }
                updates.changed().await.unwrap();
            }
            assert_eq!(
                handle.snapshot().current_skill.as_deref(),
                Some("explore_step")
            );
            if cancel {
                handle.cancel();
            } else {
                handle.resume();
            }
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(work, control)
        })
        .await
        .unwrap();
        let result = result.unwrap();
        assert_eq!(result.decisions, 1);
        assert_eq!(
            result.status,
            if cancel {
                Status::Cancelled
            } else {
                Status::Completed
            }
        );
        assert_eq!(result.batch.unwrap().completed, if cancel { 1 } else { 4 });
    }
}
