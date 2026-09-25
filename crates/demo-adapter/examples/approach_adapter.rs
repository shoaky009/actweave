//! End-to-end adapter example: cargo run -p demo-adapter --example approach_adapter
use adapter_runtime::Runtime;
use adapter_sdk::{
    ActionReport, Adapter, AdapterError, AppState, Availability, DecisionContext, ExecutionControl,
    Host, Registry, Skill, SkillContext, ToolCall,
    automation::{
        Control, Error,
        action::{Action, ActionFuture, ActionResult, Actions, Backend, CustomAction, Input},
        control::{
            ApproachConfig, ApproachController, ApproachGoal, Decision, Observation,
            ObservedTarget, TargetRef,
        },
        flow::{Flow, Node, Runner, Status},
        recognition::{Frame, Recognition, RecognitionResult, Recognizers},
    },
    flow_runtime,
};
use serde_json::{Value, json};

const TARGET: TargetRef = TargetRef(7);
const SKILL: &str = "approach_target";

#[derive(Default)]
struct MockWorld {
    offset: f32,
    range: f32,
    forward_held: bool,
}

impl MockWorld {
    // A real adapter tracks the target across frames and supplies these measurements.
    fn observe_target(&self) -> Observation {
        Observation {
            targets: vec![ObservedTarget {
                id: TARGET,
                horizontal_offset: self.offset,
                range: Some(self.range),
            }],
        }
    }

    // Goal verification belongs to the adapter, not the movement controller.
    fn goal_verified(&self) -> bool {
        self.range <= 0.2
    }
}

impl Backend for MockWorld {
    async fn capture(&mut self, _: &Control) -> Result<Frame, Error> {
        Frame::new(1, 1, vec![0, 0, 0])
    }

    async fn input(&mut self, input: &Input, control: &Control) -> Result<(), Error> {
        control.check()?;
        match input {
            Input::RelativeMove { dx, .. } => self.offset -= *dx as f32 / 100.0,
            Input::KeyDown(key) if key == "W" => self.forward_held = true,
            Input::KeyUp(key) if key == "W" => {
                self.forward_held = false;
                self.range = (self.range - 0.4).max(0.0);
            }
            other => return Err(Error::Unsupported(format!("mock input: {other:?}"))),
        }
        Ok(())
    }

    async fn release_all(&mut self) -> Result<(), Error> {
        self.forward_held = false;
        Ok(())
    }
}

struct ApproachAction {
    controller: ApproachController,
}

impl CustomAction<MockWorld> for ApproachAction {
    fn execute<'a>(
        &'a mut self,
        world: &'a mut MockWorld,
        _: &'a Value,
        _: &'a RecognitionResult,
        control: &'a Control,
    ) -> ActionFuture<'a> {
        Box::pin(async move {
            control.check()?;
            if world.goal_verified() {
                return Ok(ActionResult::Complete(json!({ "target": TARGET.0 })));
            }

            let observation = world.observe_target();
            match self.controller.step(&observation)? {
                Decision::Turn { dx } => {
                    world
                        .input(&Input::RelativeMove { dx, dy: 0 }, control)
                        .await?;
                }
                Decision::Move { duration_ms } => {
                    world.input(&Input::KeyDown("W".into()), control).await?;
                    tokio::select! {
                        _ = control.cancelled() => return Err(Error::Cancelled),
                        _ = tokio::time::sleep(std::time::Duration::from_millis(duration_ms)) => {}
                    }
                    world.input(&Input::KeyUp("W".into()), control).await?;
                    self.controller.move_completed(
                        observation.targets.first().and_then(|target| target.range),
                    );
                }
                Decision::ObserveAgain => {}
                Decision::Blocked(reason) => {
                    return Ok(ActionResult::Blocked(format!(
                        "approach blocked: {reason:?}"
                    )));
                }
            }
            Ok(ActionResult::Reobserve(Value::Null))
        })
    }

    fn interrupted(&mut self) {
        self.controller.input_interrupted();
    }
}

fn approach_flow() -> Flow {
    Flow {
        entry: "approach".into(),
        nodes: [(
            "approach".into(),
            Node {
                recognition: Recognition::DirectHit { roi: None },
                actions: vec![Action::Custom {
                    name: SKILL.into(),
                    parameters: json!({}),
                }],
                next: vec![],
                on_error: vec![],
                on_interrupted: vec!["approach".into()],
                timeout_ms: 2000,
            },
        )]
        .into(),
        poll_interval_ms: 10,
        max_operations: 20,
    }
}

struct ApproachAdapter {
    world: MockWorld,
}

impl Adapter for ApproachAdapter {
    fn observe(&self) -> Result<AppState, AdapterError> {
        Ok(AppState {
            scene: "mock target area".into(),
            facts: json!({ "target_visible": true, "target_reached": self.world.goal_verified() }),
        })
    }

    fn decision_context(
        &self,
        context: &SkillContext<'_>,
    ) -> Result<DecisionContext, AdapterError> {
        let reached = context.state.facts["target_reached"]
            .as_bool()
            .ok_or_else(|| AdapterError::Invalid("missing target_reached fact".into()))?;
        Ok(DecisionContext {
            skills: vec![Skill {
                name: SKILL.into(),
                description: "Approach the visible target until the adapter verifies arrival. One call performs locally bounded turns and short moves; do not repeat after target_reached becomes true.".into(),
                repeatable: false,
                loopable: false,
                tags: vec!["movement".into()],
                availability: if reached {
                    Availability::Unavailable { reason: "target already reached".into() }
                } else {
                    Availability::Available
                },
                parameters: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
                calls: vec![ToolCall { name: SKILL.into(), arguments: json!({}) }],
            }],
            guidance: String::new(),
        })
    }

    async fn execute(
        &mut self,
        call: &ToolCall,
        control: &ExecutionControl,
    ) -> Result<ActionReport, AdapterError> {
        if call.name != SKILL || call.arguments != json!({}) {
            return Err(AdapterError::Invalid(
                "expected approach_target with {}".into(),
            ));
        }
        control.check()?;
        let controller = ApproachController::new(
            ApproachGoal { target: TARGET },
            ApproachConfig {
                turn_pixels_per_offset: 100.0,
                center_tolerance: 0.1,
                max_turn_pixels: 30,
                move_ms: 20,
                max_missing_observations: 2,
                min_range_progress: 0.05,
                max_stalled_moves: 2,
            },
        )
        .map_err(|error| AdapterError::Invalid(error.to_string()))?;
        let mut recognizers = Recognizers::default();
        let mut actions = Actions::default();
        actions
            .register(SKILL.into(), ApproachAction { controller })
            .map_err(|error| AdapterError::Invalid(error.to_string()))?;
        let mut runner = Runner::new(approach_flow(), &recognizers, &actions)
            .map_err(|error| AdapterError::Invalid(error.to_string()))?;
        let progress = flow_runtime::run(
            &mut runner,
            &mut self.world,
            &mut recognizers,
            &mut actions,
            control,
        )
        .await?;
        match progress.status {
            Status::Completed if self.world.goal_verified() => {
                Ok(ActionReport::completed("target reached".into(), true))
            }
            Status::Cancelled => Err(AdapterError::Cancelled),
            Status::Completed | Status::Blocked | Status::Failed | Status::Running => {
                Err(AdapterError::Runtime(progress.message))
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut registry = Registry::default();
    registry.register("approach_mock", |_| {
        Ok(ApproachAdapter {
            world: MockWorld {
                offset: 0.6,
                range: 1.0,
                ..Default::default()
            },
        })
    })?;
    let mut runtime = Runtime::new(
        &registry,
        "approach_mock",
        "approach the target",
        Host::new(|message| println!("[INFO] {message}")),
    )?;
    let call = ToolCall {
        name: SKILL.into(),
        arguments: json!({}),
    };
    runtime.invoke(&call, &ExecutionControl::default()).await?;
    println!("State: {}", runtime.observe()?.facts);
    Ok(())
}
