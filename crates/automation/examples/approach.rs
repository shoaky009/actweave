//! Run with `cargo run -p automation --example approach`.
//! The mock observer stands in for adapter-owned tracking and OCR association.
use automation::{
    Control, Error,
    action::{Action, ActionFuture, ActionResult, Actions, Backend, CustomAction, Input},
    control::{
        ApproachConfig, ApproachController, ApproachGoal, Decision, Observation, ObservedTarget,
        TargetRef,
    },
    flow::{Flow, Node, Runner, Status},
    recognition::{Frame, Recognition, RecognitionResult, Recognizers},
};
use serde_json::{Value, json};

const MINT: TargetRef = TargetRef(7);

struct MockWorld {
    offset: f32,
    range: f32,
    forward_held: bool,
}

impl MockWorld {
    fn observe(&self) -> Observation {
        Observation {
            targets: vec![ObservedTarget {
                id: MINT,
                horizontal_offset: self.offset,
                range: Some(self.range),
            }],
        }
    }
    // An adapter may use OCR, inventory changes, or other evidence here.
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

struct ApproachMint {
    controller: ApproachController,
}
impl CustomAction<MockWorld> for ApproachMint {
    fn execute<'a>(
        &'a mut self,
        backend: &'a mut MockWorld,
        _: &'a Value,
        _: &'a RecognitionResult,
        control: &'a Control,
    ) -> ActionFuture<'a> {
        Box::pin(async move {
            control.check()?;
            if backend.goal_verified() {
                println!("goal verified");
                return Ok(ActionResult::Complete(
                    json!({"target":"mint","reached":true}),
                ));
            }
            let observation = backend.observe();
            let decision = self.controller.step(&observation)?;
            println!("{decision:?}");
            match decision {
                Decision::Turn { dx } => {
                    backend
                        .input(&Input::RelativeMove { dx, dy: 0 }, control)
                        .await?;
                }
                Decision::Move { duration_ms } => {
                    backend.input(&Input::KeyDown("W".into()), control).await?;
                    tokio::select! {
                        _ = control.cancelled() => return Err(Error::Cancelled),
                        _ = tokio::time::sleep(std::time::Duration::from_millis(duration_ms)) => {}
                    }
                    backend.input(&Input::KeyUp("W".into()), control).await?;
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

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let flow = Flow {
        entry: "approach".into(),
        nodes: [(
            "approach".into(),
            Node {
                recognition: Recognition::DirectHit { roi: None },
                actions: vec![Action::Custom {
                    name: "approach_mint".into(),
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
    };
    let mut recognizers = Recognizers::default();
    let mut actions = Actions::default();
    let controller = ApproachController::new(
        ApproachGoal { target: MINT },
        ApproachConfig {
            turn_pixels_per_offset: 100.0,
            center_tolerance: 0.1,
            max_turn_pixels: 30,
            move_ms: 20,
            max_missing_observations: 2,
            min_range_progress: 0.05,
            max_stalled_moves: 2,
        },
    )?;
    actions.register("approach_mint".into(), ApproachMint { controller })?;
    let mut runner = Runner::new(flow, &recognizers, &actions)?;
    let mut world = MockWorld {
        offset: 0.6,
        range: 1.0,
        forward_held: false,
    };
    let control = Control::default();
    while runner.progress().status == Status::Running {
        runner
            .step(&mut world, &mut recognizers, &mut actions, &control)
            .await?;
    }
    println!("{}", serde_json::to_string(runner.progress())?);
    if runner.progress().status != Status::Completed {
        return Err(Error::Backend(runner.progress().message.clone()).into());
    }
    Ok(())
}
