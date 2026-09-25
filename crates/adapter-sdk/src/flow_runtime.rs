//! Drive one Flow from an adapter skill with the host's task controls.
use crate::{AdapterError, CancellationToken, ExecutionControl, PauseToken};
use automation::{
    Control, Error,
    action::{Actions, Backend},
    flow::{Progress, Runner, Status},
    recognition::Recognizers,
};
use std::{future::pending, time::Instant};

async fn deadline_reached(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
        None => pending().await,
    }
}

async fn pause_requested(pause: Option<&PauseToken>) {
    match pause {
        Some(pause) => pause.requested().await,
        None => pending().await,
    }
}

async fn bridge(
    parent_cancel: CancellationToken,
    parent_pause: Option<PauseToken>,
    deadline: Option<Instant>,
    flow: Control,
) {
    loop {
        tokio::select! {
            biased;
            _ = parent_cancel.cancelled() => { flow.cancel(); return; }
            _ = deadline_reached(deadline) => { flow.cancel(); return; }
            _ = pause_requested(parent_pause.as_ref()) => flow.pause(),
        }
        tokio::select! {
            biased;
            _ = parent_cancel.cancelled() => { flow.cancel(); return; }
            _ = deadline_reached(deadline) => { flow.cancel(); return; }
            _ = flow.paused() => {},
        }
        if let Some(pause) = &parent_pause {
            pause.acknowledge();
            tokio::select! {
                biased;
                _ = parent_cancel.cancelled() => { flow.cancel(); return; }
                _ = deadline_reached(deadline) => { flow.cancel(); return; }
                _ = pause.resumed() => flow.resume(),
            }
        }
    }
}

fn map_error(error: Error) -> AdapterError {
    match error {
        Error::Cancelled => AdapterError::Cancelled,
        Error::TimedOut => AdapterError::TimedOut,
        Error::Invalid(message) => AdapterError::Invalid(message),
        Error::Cleanup(message) => AdapterError::CleanupFailed(message),
        other => AdapterError::Runtime(other.to_string()),
    }
}

/// Run a Flow until its local outcome is known. The adapter decides what that
/// outcome means for the skill and the user's wider task.
pub async fn run<B: Backend>(
    runner: &mut Runner,
    backend: &mut B,
    recognizers: &mut Recognizers,
    actions: &mut Actions<B>,
    control: &ExecutionControl,
) -> Result<Progress, AdapterError> {
    control.check()?;
    let flow_control = Control::default();
    if control.pause.as_ref().is_some_and(PauseToken::is_requested) {
        flow_control.pause();
    }
    let bridge_task = tokio::spawn(bridge(
        control.cancellation.clone(),
        control.pause.clone(),
        control.deadline,
        flow_control.clone(),
    ));
    let result = async {
        while runner.progress().status == Status::Running {
            runner
                .step(backend, recognizers, actions, &flow_control)
                .await
                .map_err(map_error)?;
        }
        Ok::<_, AdapterError>(runner.progress().clone())
    }
    .await;
    bridge_task.abort();
    let progress = result?;
    control.check()?;
    Ok(progress)
}
