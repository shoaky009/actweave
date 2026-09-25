use adapter_sdk::{
    CancellationToken, ExecutionControl, PauseToken,
    automation::{
        Control, Error,
        action::{Action, Actions, Backend, Input},
        flow::{Flow, Node, Runner, Status},
        recognition::{Frame, Recognition, Recognizers},
    },
    flow_runtime,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

struct Device {
    held: Arc<AtomicBool>,
    presses: Arc<AtomicUsize>,
    started: Arc<Notify>,
    fail_release: bool,
    hang_release: bool,
}
impl Backend for Device {
    async fn capture(&mut self, _: &Control) -> Result<Frame, Error> {
        let color = if self.presses.load(Ordering::SeqCst) == 0 {
            [255, 0, 0]
        } else {
            [0, 0, 0]
        };
        Frame::new(1, 1, color.to_vec())
    }
    async fn input(&mut self, input: &Input, _: &Control) -> Result<(), Error> {
        match input {
            Input::KeyDown(key) if key == "W" => {
                self.held.store(true, Ordering::SeqCst);
                self.presses.fetch_add(1, Ordering::SeqCst);
                self.started.notify_one();
            }
            Input::KeyUp(key) if key == "W" => self.held.store(false, Ordering::SeqCst),
            _ => return Err(Error::Unsupported("unexpected mock input".into())),
        }
        Ok(())
    }
    async fn release_all(&mut self) -> Result<(), Error> {
        if self.hang_release {
            return std::future::pending().await;
        }
        if self.fail_release {
            return Err(Error::Backend("release failed".into()));
        }
        self.held.store(false, Ordering::SeqCst);
        Ok(())
    }
}

fn device(fail_release: bool) -> Device {
    Device {
        held: Arc::new(AtomicBool::new(false)),
        presses: Arc::new(AtomicUsize::new(0)),
        started: Arc::new(Notify::new()),
        fail_release,
        hang_release: false,
    }
}

fn runner() -> Runner {
    let entry = Node {
        recognition: Recognition::DirectHit { roi: None },
        actions: vec![
            Action::KeyDown { key: "W".into() },
            Action::Wait { duration_ms: 200 },
            Action::KeyUp { key: "W".into() },
        ],
        next: vec!["confirmed".into()],
        on_error: vec![],
        on_interrupted: vec!["confirmed".into(), "entry".into()],
        timeout_ms: 1000,
    };
    let confirmed = Node {
        recognition: Recognition::ColorMatch {
            roi: None,
            lower: [0, 0, 0],
            upper: [0, 0, 0],
            min_ratio: 1.0,
        },
        actions: vec![],
        next: vec![],
        on_error: vec![],
        on_interrupted: vec![],
        timeout_ms: 1000,
    };
    Runner::new(
        Flow {
            entry: "entry".into(),
            nodes: [("entry".into(), entry), ("confirmed".into(), confirmed)].into(),
            poll_interval_ms: 1,
            max_operations: 20,
        },
        &Recognizers::default(),
        &Actions::<Device>::default(),
    )
    .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn pause_releases_held_input_before_ack_and_resumes_from_new_observation() {
    let mut device = device(false);
    let held = device.held.clone();
    let presses = device.presses.clone();
    let started = device.started.clone();
    let acknowledged = Arc::new(AtomicBool::new(false));
    let held_at_ack = Arc::new(AtomicBool::new(true));
    let pause = PauseToken::new({
        let acknowledged = acknowledged.clone();
        let held_at_ack = held_at_ack.clone();
        let held = held.clone();
        move || {
            held_at_ack.store(held.load(Ordering::SeqCst), Ordering::SeqCst);
            acknowledged.store(true, Ordering::SeqCst);
        }
    });
    let control = ExecutionControl {
        pause: Some(pause.clone()),
        ..Default::default()
    };
    let mut runner = runner();
    let mut recognizers = Recognizers::default();
    let mut actions = Actions::default();
    let run = flow_runtime::run(
        &mut runner,
        &mut device,
        &mut recognizers,
        &mut actions,
        &control,
    );
    let steer = async {
        started.notified().await;
        pause.request();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !acknowledged.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!held_at_ack.load(Ordering::SeqCst));
        pause.resume();
    };
    let (result, ()) = tokio::join!(run, steer);
    assert_eq!(result.unwrap().status, Status::Completed);
    assert_eq!(presses.load(Ordering::SeqCst), 1);
    assert!(!held.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_releases_input_without_replaying_the_command() {
    let mut device = device(false);
    let held = device.held.clone();
    let presses = device.presses.clone();
    let started = device.started.clone();
    let cancellation = CancellationToken::default();
    let control = ExecutionControl {
        cancellation: cancellation.clone(),
        ..Default::default()
    };
    let mut runner = runner();
    let mut recognizers = Recognizers::default();
    let mut actions = Actions::default();
    let run = flow_runtime::run(
        &mut runner,
        &mut device,
        &mut recognizers,
        &mut actions,
        &control,
    );
    let steer = async {
        started.notified().await;
        cancellation.cancel();
    };
    let (result, ()) = tokio::join!(run, steer);
    assert!(matches!(result, Err(adapter_sdk::AdapterError::Cancelled)));
    assert_eq!(presses.load(Ordering::SeqCst), 1);
    assert!(!held.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "current_thread")]
async fn failed_release_never_acknowledges_pause() {
    let mut device = device(true);
    let started = device.started.clone();
    let acknowledged = Arc::new(AtomicBool::new(false));
    let pause = PauseToken::new({
        let acknowledged = acknowledged.clone();
        move || acknowledged.store(true, Ordering::SeqCst)
    });
    let control = ExecutionControl {
        pause: Some(pause.clone()),
        ..Default::default()
    };
    let mut runner = runner();
    let mut recognizers = Recognizers::default();
    let mut actions = Actions::default();
    let run = flow_runtime::run(
        &mut runner,
        &mut device,
        &mut recognizers,
        &mut actions,
        &control,
    );
    let steer = async {
        started.notified().await;
        pause.request();
    };
    let (result, ()) = tokio::join!(run, steer);
    assert!(result.unwrap_err().to_string().contains("release failed"));
    assert!(!acknowledged.load(Ordering::SeqCst));
    assert_eq!(runner.progress().status, Status::Failed);
}

#[tokio::test(start_paused = true)]
async fn stalled_release_fails_instead_of_leaving_pause_pending_forever() {
    let mut device = device(false);
    device.hang_release = true;
    let started = device.started.clone();
    let acknowledged = Arc::new(AtomicBool::new(false));
    let pause = PauseToken::new({
        let acknowledged = acknowledged.clone();
        move || acknowledged.store(true, Ordering::SeqCst)
    });
    let control = ExecutionControl {
        pause: Some(pause.clone()),
        ..Default::default()
    };
    let mut runner = runner();
    let mut recognizers = Recognizers::default();
    let mut actions = Actions::default();
    let run = flow_runtime::run(
        &mut runner,
        &mut device,
        &mut recognizers,
        &mut actions,
        &control,
    );
    let steer = async {
        started.notified().await;
        pause.request();
    };
    let (result, ()) = tokio::join!(run, steer);
    assert!(matches!(
        result,
        Err(adapter_sdk::AdapterError::CleanupFailed(_))
    ));
    assert!(!acknowledged.load(Ordering::SeqCst));
    assert_eq!(runner.progress().status, Status::Failed);
}
