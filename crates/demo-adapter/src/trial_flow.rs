//! A finite semantic skill backed by a visual Flow over a simulated device.
use crate::{
    runtime::{DemoRuntime, RuntimeError},
    runtime_error,
};
use adapter_sdk::automation::{
    Control, Error,
    action::{Action, Actions, Backend, Input, Target},
    flow::{Flow, Node, Runner, Status},
    recognition::{Frame, Recognition, Recognizers},
};
use adapter_sdk::flow_runtime;
use adapter_sdk::{AdapterError, ExecutionControl};

fn definition() -> Flow {
    let color = |rgb| Recognition::ColorMatch {
        roi: None,
        lower: rgb,
        upper: rgb,
        min_ratio: 1.0,
    };
    Flow {
        entry: "ready".into(),
        poll_interval_ms: 10,
        max_operations: 8,
        nodes: [
            (
                "ready".into(),
                Node {
                    recognition: color([0, 255, 0]),
                    actions: vec![Action::Click {
                        target: Target::Match { index: 0 },
                    }],
                    next: vec!["confirmed".into()],
                    on_error: vec![],
                    on_interrupted: vec!["confirmed".into(), "ready".into()],
                    timeout_ms: 1000,
                },
            ),
            (
                "confirmed".into(),
                Node {
                    recognition: color([0, 0, 0]),
                    actions: vec![],
                    next: vec![],
                    on_error: vec![],
                    on_interrupted: vec![],
                    timeout_ms: 1000,
                },
            ),
        ]
        .into(),
    }
}

struct TrialDevice<'a> {
    runtime: &'a mut DemoRuntime,
    control: &'a ExecutionControl,
    result: Option<bool>,
    error: Option<RuntimeError>,
}
impl Backend for TrialDevice<'_> {
    async fn capture(&mut self, _: &Control) -> Result<Frame, Error> {
        self.check()?;
        // The demo has no real window: green means ready, black confirms an effect.
        Frame::new(
            1,
            1,
            if self.result.is_some() {
                vec![0, 0, 0]
            } else {
                vec![0, 255, 0]
            },
        )
    }
    async fn input(&mut self, input: &Input, _: &Control) -> Result<(), Error> {
        self.check()?;
        if !matches!(input, Input::Click(_)) || self.result.is_some() {
            return Err(Error::Invalid("trial expects exactly one click".into()));
        }
        match self.runtime.perform_trial() {
            Ok(successful) => {
                self.result = Some(successful);
                Ok(())
            }
            Err(error) => {
                self.error = Some(error);
                Err(Error::Backend("trial device interrupted".into()))
            }
        }
    }
    async fn release_all(&mut self) -> Result<(), Error> {
        Ok(())
    }
}
impl TrialDevice<'_> {
    fn check(&self) -> Result<(), Error> {
        self.control.check().map_err(|error| match error {
            AdapterError::Cancelled => Error::Cancelled,
            AdapterError::TimedOut => Error::TimedOut,
            _ => Error::Backend(error.to_string()),
        })
    }
}

pub(super) async fn execute(
    runtime: &mut DemoRuntime,
    control: &ExecutionControl,
) -> Result<bool, AdapterError> {
    control.check()?;
    // Validate device availability before starting a flow; no fake visual timeout.
    let (_, _, ready, needs_reset) = runtime.trial_snapshot();
    if !ready {
        return Err(runtime_error(if needs_reset {
            RuntimeError::TrialNeedsReset
        } else {
            RuntimeError::TrialUnavailable
        }));
    }
    let flow = definition();
    let mut recognizers = Recognizers::default();
    let mut actions = Actions::default();
    let mut runner = Runner::new(flow, &recognizers, &actions).map_err(flow_error)?;
    let mut device = TrialDevice {
        runtime,
        control,
        result: None,
        error: None,
    };
    let progress = flow_runtime::run(
        &mut runner,
        &mut device,
        &mut recognizers,
        &mut actions,
        control,
    )
    .await?;
    match progress.status {
        Status::Completed => device.result.ok_or_else(|| {
            AdapterError::Runtime("flow completed without a confirmed trial".into())
        }),
        Status::Cancelled => Err(AdapterError::Cancelled),
        Status::Blocked | Status::Failed => {
            if let Some(error) = device.error.take() {
                return Err(runtime_error(error));
            }
            Err(AdapterError::Runtime(progress.message))
        }
        Status::Running => Err(AdapterError::Runtime("flow returned while running".into())),
    }
}

fn flow_error(error: Error) -> AdapterError {
    match error {
        Error::Cancelled => AdapterError::Cancelled,
        Error::TimedOut => AdapterError::TimedOut,
        Error::Invalid(message) => AdapterError::Invalid(message),
        other => AdapterError::Runtime(other.to_string()),
    }
}
