use actweave::{
    adapter::{DemoAdapter, Scenario},
    core::*,
    skills::{LoadSkills, SkillMode, SkillView},
    task_runtime::{TaskRuntime, TaskStatus},
};
use adapter_api::ActionReport;
use serde_json::json;
use std::{cell::Cell, collections::VecDeque, time::Duration};

fn call(name: &str, arguments: serde_json::Value) -> Action {
    Action::Call(ToolCall {
        name: name.into(),
        arguments,
    })
}
fn trial(times: u32) -> Action {
    Action::Repeat(RepeatRequest {
        call: ToolCall {
            name: "perform_trial".into(),
            arguments: json!({}),
        },
        times,
    })
}
fn execute(actions: Vec<Action>) -> Decision {
    Decision::Execute {
        actions,
        then: Continuation::Finish,
    }
}
fn training() -> Vec<Action> {
    vec![
        call("set_mode", json!({"mode":"training"})),
        call("start_training", json!({})),
        call("get_training_status", json!({})),
    ]
}

#[tokio::test]
async fn replacement_preserves_prefix_and_rejects_invalid_suffix_atomically() {
    let mut agent = Script::new(vec![
        execute(training()),
        Decision::ReplaceRemaining {
            actions: vec![call("missing", json!({}))],
            then: Continuation::Finish,
        },
        Decision::ReplaceRemaining {
            actions: vec![call("get_state", json!({}))],
            then: Continuation::Finish,
        },
    ]);
    let mut started = vec![];
    let result = run(
        &Task::new("replace interrupted plan").unwrap(),
        &mut agent,
        &mut DemoAdapter::new(Scenario::TransientFailure),
        3,
        |event| {
            if let Event::ActionStarted { call, .. } = event {
                started.push(call.name);
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(result.status, Status::Completed);
    assert_eq!(started, ["set_mode", "start_training", "get_state"]);
    assert_eq!(agent.progress[2].as_ref().unwrap().completed, 1);
    assert_eq!(agent.progress[2].as_ref().unwrap().actions.len(), 3);
    assert_eq!(result.execution.unwrap().completed, 2);
}

#[tokio::test]
async fn replacement_carries_partial_batch_without_resetting_counts_or_attempts() {
    let mut agent = Script::new(vec![
        execute(vec![trial(10)]),
        Decision::ReplaceRemaining {
            actions: vec![trial(10)],
            then: Continuation::Finish,
        },
        Decision::Execute {
            actions: vec![call("reset_trial", json!({}))],
            then: Continuation::Decide,
        },
        Decision::ReplaceRemaining {
            actions: vec![trial(7)],
            then: Continuation::Finish,
        },
    ]);
    let result = run(
        &Task::new("ten trials").unwrap(),
        &mut agent,
        &mut DemoAdapter::new(Scenario::BatchInterrupted),
        4,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(result.status, Status::Completed);
    assert_eq!(result.state.facts["trial"]["completed"], 10);
    assert_eq!(agent.batches[2].as_ref().unwrap().remaining, Some(7));
    let batch = result.batch.unwrap();
    assert_eq!(batch.completed, 10);
    assert_eq!(batch.attempts, 11);
}

#[tokio::test]
async fn replacement_without_pending_execution_has_no_effect() {
    let mut agent = Script::new(vec![
        Decision::ReplaceRemaining {
            actions: training(),
            then: Continuation::Finish,
        },
        Decision::Failed("no pending plan".into()),
    ]);
    let result = run(
        &Task::new("invalid replacement").unwrap(),
        &mut agent,
        &mut DemoAdapter::default(),
        2,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(result.state.facts["mode"], "idle");
}
struct Script {
    decisions: VecDeque<Decision>,
    progress: Vec<Option<ExecutionProgress>>,
    batches: Vec<Option<BatchProgress>>,
}
impl Script {
    fn new(decisions: Vec<Decision>) -> Self {
        Self {
            decisions: decisions.into(),
            progress: vec![],
            batches: vec![],
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
        self.progress.push(view.execution.clone());
        self.batches.push(view.batch.clone());
        Ok(self.decisions.pop_front().expect("unexpected model call"))
    }
}

#[tokio::test]
async fn three_skills_execute_in_order_with_one_decision_and_fresh_availability() {
    let mut agent = Script::new(vec![execute(training())]);
    let mut started = vec![];
    let result = run(
        &Task::new("start training and inspect status").unwrap(),
        &mut agent,
        &mut DemoAdapter::default(),
        1,
        |event| {
            if let Event::ActionStarted { call, .. } = event {
                started.push(call.name);
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(result.status, Status::Completed);
    assert_eq!(result.decisions, 1);
    assert_eq!(
        started,
        ["set_mode", "start_training", "get_training_status"]
    );
    assert_eq!(result.execution.unwrap().completed, 3);
}

#[tokio::test]
async fn failure_stops_tail_and_resume_does_not_replay_completed_prefix() {
    let mut agent = Script::new(vec![execute(training()), Decision::Resume]);
    let mut started = vec![];
    let result = run(
        &Task::new("start and inspect").unwrap(),
        &mut agent,
        &mut DemoAdapter::new(Scenario::TransientFailure),
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
    assert_eq!(agent.progress[1].as_ref().unwrap().completed, 1);
    assert_eq!(
        started,
        [
            "set_mode",
            "start_training",
            "start_training",
            "get_training_status"
        ]
    );
}

#[tokio::test]
async fn repeat_inside_sequence_preserves_count_then_executes_tail_after_repair() {
    let mut agent = Script::new(vec![
        execute(vec![
            call("set_mode", json!({"mode":"training"})),
            trial(10),
            call("get_state", json!({})),
        ]),
        Decision::Execute {
            actions: vec![call("reset_trial", json!({}))],
            then: Continuation::Decide,
        },
        Decision::Resume,
    ]);
    let result = run(
        &Task::new("prepare repeat and inspect").unwrap(),
        &mut agent,
        &mut DemoAdapter::new(Scenario::BatchInterrupted),
        3,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(result.status, Status::Completed);
    assert_eq!(agent.progress[1].as_ref().unwrap().completed, 1);
    assert_eq!(agent.batches[1].as_ref().unwrap().completed, 3);
    assert_eq!(result.state.facts["trial"]["completed"], 10);
    assert_eq!(result.execution.unwrap().completed, 3);
}

#[tokio::test]
async fn availability_revoked_by_earlier_step_blocks_later_call() {
    let mut actions = training();
    actions[2] = call("set_mode", json!({"mode":"idle"}));
    let mut agent = Script::new(vec![
        execute(actions),
        Decision::Failed("cannot change mode while running".into()),
    ]);
    let result = run(
        &Task::new("test revoked precondition").unwrap(),
        &mut agent,
        &mut DemoAdapter::default(),
        2,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(result.status, Status::Failed);
    assert_eq!(agent.progress[1].as_ref().unwrap().completed, 2);
    assert_eq!(result.state.facts["mode"], "training");
}

#[tokio::test]
async fn on_demand_requires_selection_for_every_action_before_any_effect() {
    let load = Decision::LoadSkills(LoadSkills {
        names: vec!["set_mode".into()],
        tags: vec![],
    });
    let mut agent = Script::new(vec![
        load,
        execute(training()),
        Decision::Failed("not loaded".into()),
    ]);
    let result = run_with_options(
        &Task::new("start training").unwrap(),
        &mut agent,
        &mut DemoAdapter::default(),
        RunOptions {
            skill_mode: SkillMode::OnDemand,
            ..RunOptions::default()
        },
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(result.state.facts["mode"], "idle");
}

#[tokio::test]
async fn on_demand_can_select_initially_unavailable_steps_and_execute_after_prerequisite() {
    let mut agent = Script::new(vec![
        Decision::LoadSkills(LoadSkills {
            names: vec![
                "set_mode".into(),
                "start_training".into(),
                "get_training_status".into(),
            ],
            tags: vec![],
        }),
        execute(training()),
    ]);
    let result = run_with_options(
        &Task::new("start and inspect").unwrap(),
        &mut agent,
        &mut DemoAdapter::default(),
        RunOptions {
            skill_mode: SkillMode::OnDemand,
            ..RunOptions::default()
        },
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(result.status, Status::Completed);
    assert_eq!(result.decisions, 2);
}

#[tokio::test]
async fn invalid_lists_are_rejected_before_executing_prefix() {
    for actions in [
        vec![],
        vec![call("get_state", json!({})); 5],
        vec![
            call("set_mode", json!({"mode":"training"})),
            call("missing", json!({})),
        ],
    ] {
        let mut agent = Script::new(vec![
            execute(actions),
            Decision::Failed("invalid plan".into()),
        ]);
        let result = run(
            &Task::new("test list validation").unwrap(),
            &mut agent,
            &mut DemoAdapter::default(),
            2,
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(result.state.facts["mode"], "idle");
    }
}

#[tokio::test]
async fn cancellation_after_first_action_prevents_tail() {
    let runtime = TaskRuntime::new(Task::new("sequence").unwrap(), RunOptions::default());
    let handle = runtime.handle();
    let mut agent = Script::new(vec![execute(training())]);
    let result = runtime
        .run(&mut agent, &mut DemoAdapter::default(), |event| {
            if matches!(event, Event::Executed(_)) {
                handle.cancel();
            }
        })
        .await
        .unwrap();
    assert_eq!(result.status, Status::Cancelled);
    assert_eq!(result.execution.unwrap().completed, 1);
    assert_eq!(result.state.facts["training"]["status"], "not_started");
}

#[tokio::test]
async fn pause_and_resume_preserve_sequence_position_without_model_calls() {
    let runtime = TaskRuntime::new(Task::new("sequence").unwrap(), RunOptions::default());
    let handle = runtime.handle();
    let mut updates = handle.subscribe();
    let mut agent = Script::new(vec![execute(training())]);
    let mut adapter = DemoAdapter::default();
    let work = runtime.run(&mut agent, &mut adapter, |event| {
        if let Event::Executed(result) = event
            && result.call.name == "set_mode"
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
        handle.resume();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(work, control)
    })
    .await
    .unwrap();
    assert_eq!(result.unwrap().decisions, 1);
}

struct ObservationFailure {
    demo: DemoAdapter,
    fail_next: Cell<bool>,
    changes: usize,
}
impl Adapter for ObservationFailure {
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
        self.demo.decision_context(context)
    }
    async fn execute(
        &mut self,
        call: &ToolCall,
        control: &ExecutionControl,
    ) -> Result<ActionReport, AdapterError> {
        let result = self.demo.execute(call, control).await?;
        if call.name == "set_mode" {
            self.changes += 1;
            self.fail_next.set(true);
        }
        Ok(result)
    }
}
#[tokio::test]
async fn post_action_observation_failure_does_not_replay_confirmed_action() {
    let mut adapter = ObservationFailure {
        demo: DemoAdapter::default(),
        fail_next: Cell::new(false),
        changes: 0,
    };
    let mut agent = Script::new(vec![
        execute(vec![
            call("set_mode", json!({"mode":"training"})),
            call("get_state", json!({})),
        ]),
        Decision::Resume,
    ]);
    let result = run(
        &Task::new("sequence").unwrap(),
        &mut agent,
        &mut adapter,
        2,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(result.status, Status::Completed);
    assert_eq!(adapter.changes, 1);
    assert_eq!(agent.progress[1].as_ref().unwrap().completed, 1);
}
