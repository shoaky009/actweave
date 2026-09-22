use actweave::{adapter::DemoAdapter, core::*};
use adapter_api::ActionReport;
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
        Ok(self.0.pop_front().expect("unexpected extra decision"))
    }
}

struct Mock {
    demo: DemoAdapter,
    outcomes: VecDeque<Option<ActionOutcome>>,
    calls: usize,
}
impl Mock {
    fn new(outcomes: Vec<Option<ActionOutcome>>) -> Self {
        Self {
            demo: DemoAdapter::default(),
            outcomes: outcomes.into(),
            calls: 0,
        }
    }
}
impl Adapter for Mock {
    fn observe(&self) -> Result<AppState, AdapterError> {
        self.demo.observe()
    }
    fn decision_context(&self, ctx: &SkillContext<'_>) -> Result<DecisionContext, AdapterError> {
        let mut context = self.demo.decision_context(ctx)?;
        for skill in &mut context.skills {
            if skill.name == "get_state" {
                skill.repeatable = true;
                skill.loopable = true;
            }
        }
        Ok(context)
    }
    async fn execute(
        &mut self,
        call: &ToolCall,
        control: &ExecutionControl,
    ) -> Result<ActionReport, AdapterError> {
        if call.name != "get_state" {
            return self.demo.execute(call, control).await;
        }
        self.calls += 1;
        match self.outcomes.pop_front().expect("unexpected extra action") {
            Some(outcome) => Ok(ActionReport {
                outcome,
                message: "reported".into(),
            }),
            None => Err(AdapterError::Runtime(format!(
                "changing error {}",
                self.calls
            ))),
        }
    }
}
fn call() -> ToolCall {
    ToolCall {
        name: "get_state".into(),
        arguments: json!({"a":1,"b":2}),
    }
}
fn execute(action: Action) -> Decision {
    Decision::Execute {
        actions: vec![action],
        then: Continuation::Decide,
    }
}
async fn run_script(decisions: Vec<Decision>, mock: &mut Mock, limit: u32) -> Outcome {
    run_with_options(
        &Task::new("test protection").unwrap(),
        &mut Script(decisions.into()),
        mock,
        RunOptions {
            max_repeated_failures: limit,
            ..Default::default()
        },
        |_| {},
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn repeated_errors_stop_without_another_decision_and_ignore_json_key_order() {
    let mut mock = Mock::new(vec![None, None, None]);
    let mut reordered = call();
    reordered.arguments = serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap();
    let outcome = run_script(
        vec![
            execute(Action::Call(call())),
            execute(Action::Call(reordered)),
            execute(Action::Call(call())),
        ],
        &mut mock,
        3,
    )
    .await;
    assert_eq!(outcome.status, Status::Failed);
    assert!(outcome.reason.contains("重复失败保护"));
    assert_eq!(outcome.decisions, 3);
    assert_eq!(outcome.summary.actions.total, 3);
    assert_eq!(outcome.summary.actions.failed, 3);
    assert_eq!(outcome.execution.unwrap().status, ExecutionStatus::Failed);
}

#[tokio::test]
async fn resume_replacement_and_unrelated_repair_preserve_failure_counts_and_progress() {
    let repeat = Action::Repeat(RepeatRequest {
        call: call(),
        times: 10,
    });
    let remaining = Action::Repeat(RepeatRequest {
        call: call(),
        times: 9,
    });
    let mut mock = Mock::new(vec![
        Some(ActionOutcome::Completed { successful: true }),
        None,
        None,
        None,
    ]);
    let outcome = run_script(
        vec![
            execute(repeat),
            execute(Action::Call(ToolCall {
                name: "set_mode".into(),
                arguments: json!({"mode":"training"}),
            })),
            Decision::Resume,
            Decision::ReplaceRemaining {
                actions: vec![remaining],
                then: Continuation::Finish,
            },
        ],
        &mut mock,
        3,
    )
    .await;
    assert!(outcome.reason.contains("重复失败保护"));
    let batch = outcome.batch.unwrap();
    assert_eq!(batch.completed, 1);
    assert_eq!(batch.remaining, Some(9));
    assert_eq!(batch.attempts, 4);
    assert_eq!(batch.status, BatchStatus::Failed);
    assert_eq!(outcome.decisions, 4);
}

#[tokio::test]
async fn normal_returns_reset_only_the_same_call_even_without_reward() {
    let normal = Some(ActionOutcome::Completed { successful: false });
    let mut mock = Mock::new(vec![None, normal.clone(), None, normal]);
    let mut decisions = vec![execute(Action::Call(call())); 4];
    decisions.push(Decision::Completed("done".into()));
    let outcome = run_script(decisions, &mut mock, 2).await;
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.summary.actions.succeeded, 2);
}

#[tokio::test]
async fn continue_is_normal_and_until_done_interruption_is_counted() {
    let mut mock = Mock::new(vec![
        Some(ActionOutcome::Continue { successful: false }),
        Some(ActionOutcome::Interrupted),
        Some(ActionOutcome::Interrupted),
    ]);
    let outcome = run_script(
        vec![execute(Action::UntilDone(call())), Decision::Resume],
        &mut mock,
        2,
    )
    .await;
    assert!(outcome.reason.contains("重复失败保护"));
    assert_eq!(outcome.batch.unwrap().completed, 1);
    assert_eq!(outcome.summary.actions.total, 3);
}

#[tokio::test]
async fn rejected_plans_stop_without_calling_adapter() {
    for replacement in [false, true] {
        let actions = vec![Action::Call(ToolCall {
            name: "missing".into(),
            arguments: json!({}),
        })];
        let plan = if replacement {
            Decision::ReplaceRemaining {
                actions,
                then: Continuation::Finish,
            }
        } else {
            Decision::Execute {
                actions,
                then: Continuation::Finish,
            }
        };
        let outcome = run_script(vec![plan; 3], &mut Mock::new(vec![]), 3).await;
        assert!(outcome.reason.contains("相同计划"));
        assert_eq!(outcome.summary.actions.total, 0);
        assert_eq!(outcome.decisions, 3);
    }
}

#[tokio::test]
async fn counts_are_task_local_and_threshold_is_configurable() {
    let mut mock = Mock::new(vec![None, None, None, None]);
    for _ in 0..2 {
        let outcome = run_script(vec![execute(Action::Call(call())); 2], &mut mock, 2).await;
        assert_eq!(outcome.decisions, 2);
        assert!(outcome.reason.contains("重复失败保护"));
    }
}

#[tokio::test]
async fn zero_threshold_is_invalid() {
    let result = run_with_options(
        &Task::new("invalid").unwrap(),
        &mut Script(VecDeque::new()),
        &mut Mock::new(vec![]),
        RunOptions {
            max_repeated_failures: 0,
            ..Default::default()
        },
        |_| {},
    )
    .await;
    assert!(matches!(result, Err(Error::Invalid(_))));
}

#[tokio::test]
async fn skill_loading_does_not_reset_action_failures() {
    use actweave::skills::{LoadSkills, SkillMode};
    let load = Decision::LoadSkills(LoadSkills {
        names: vec!["get_state".into()],
        tags: vec![],
    });
    let decisions = vec![
        load.clone(),
        execute(Action::Call(call())),
        load,
        execute(Action::Call(call())),
    ];
    let result = run_with_options(
        &Task::new("load and retry").unwrap(),
        &mut Script(decisions.into()),
        &mut Mock::new(vec![None, None]),
        RunOptions {
            skill_mode: SkillMode::OnDemand,
            max_repeated_failures: 2,
            ..Default::default()
        },
        |_| {},
    )
    .await
    .unwrap();
    assert!(result.reason.contains("重复失败保护"));
    assert_eq!(result.decisions, 4);
}

#[tokio::test]
async fn cancellation_takes_precedence_at_failure_threshold() {
    let token = CancellationToken::default();
    let result = run_with_options(
        &Task::new("cancel").unwrap(),
        &mut Script(vec![execute(Action::Call(call()))].into()),
        &mut Mock::new(vec![None]),
        RunOptions {
            cancellation: token.clone(),
            max_repeated_failures: 1,
            ..Default::default()
        },
        |event| {
            if matches!(event, Event::Executed(_)) {
                token.cancel();
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(result.status, Status::Cancelled);
}

#[tokio::test]
async fn different_parameters_have_separate_failure_counts() {
    let mut other = call();
    other.arguments = json!({"a":2,"b":2});
    let result = run_script(
        vec![
            execute(Action::Call(call())),
            execute(Action::Call(other)),
            Decision::Completed("done".into()),
        ],
        &mut Mock::new(vec![None, None]),
        2,
    )
    .await;
    assert_eq!(result.status, Status::Completed);
}
