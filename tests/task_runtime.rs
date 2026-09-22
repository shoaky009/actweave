use actweave::{
    adapter::DemoAdapter,
    core::*,
    manual::ManualAgent,
    task_runtime::{TaskRuntime, TaskStatus},
};
use serde_json::Value;
use std::{
    io::{Cursor, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Capture {
    fn records(&self) -> Vec<Value> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}
fn runtime() -> TaskRuntime {
    TaskRuntime::new(Task::new("ten trials").unwrap(), RunOptions::default())
}
fn agent() -> ManualAgent<Cursor<&'static str>, std::io::Sink> {
    ManualAgent::new(
        Cursor::new(
            "{\"Execute\":{\"then\":\"Finish\",\"actions\":[{\"Repeat\":{\"call\":{\"name\":\"perform_trial\",\"arguments\":{}},\"times\":10}}]}}\n",
        ),
        std::io::sink(),
    )
}
async fn wait_paused(handle: &actweave::task_runtime::TaskHandle) {
    let mut rx = handle.subscribe();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if rx.borrow_and_update().status == TaskStatus::Paused {
                break;
            }
            rx.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn pause_keeps_batch_and_resume_does_not_ask_model_again() {
    let runtime = runtime();
    let handle = runtime.handle();
    let progress = AtomicU32::new(0);
    let mut agent = agent();
    let mut adapter = DemoAdapter::default();
    let work = runtime.run(&mut agent, &mut adapter, |event| {
        if let Event::BatchProgress(p) = event {
            progress.store(p.completed, Ordering::SeqCst);
            if p.completed == 3 {
                handle.pause();
            }
        }
    });
    let control = async {
        wait_paused(&handle).await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(progress.load(Ordering::SeqCst), 3);
        assert_eq!(
            handle.snapshot().current_skill.as_deref(),
            Some("perform_trial")
        );
        assert!(handle.resume());
    };
    let (result, ()) = tokio::join!(work, control);
    let result = result.unwrap();
    assert_eq!(result.batch.unwrap().completed, 10);
    assert_eq!(result.decisions, 1);
    assert_eq!(handle.snapshot().status, TaskStatus::Completed);
    assert!(!handle.pause());
    assert!(!handle.cancel());
    assert!(!handle.resume());
}

#[tokio::test]
async fn cancel_wakes_paused_task_without_executing_remaining_actions() {
    let runtime = runtime();
    let handle = runtime.handle();
    let mut agent = agent();
    let mut adapter = DemoAdapter::default();
    let work = runtime.run(&mut agent, &mut adapter, |event| {
        if let Event::BatchProgress(p) = event
            && p.completed == 3
        {
            handle.pause();
        }
    });
    let control = async {
        wait_paused(&handle).await;
        assert!(handle.cancel());
    };
    let (result, ()) = tokio::join!(work, control);
    let result = result.unwrap();
    assert_eq!(result.status, Status::Cancelled);
    assert_eq!(result.batch.unwrap().completed, 3);
    assert_eq!(handle.snapshot().status, TaskStatus::Cancelled);
}

#[tokio::test]
async fn pause_before_start_and_model_error_have_managed_states() {
    let runtime = runtime();
    let handle = runtime.handle();
    assert!(handle.pause());
    let mut agent = ManualAgent::new(Cursor::new("invalid\n"), std::io::sink());
    let mut adapter = DemoAdapter::default();
    let work = runtime.run(&mut agent, &mut adapter, |_| {});
    let control = async {
        wait_paused(&handle).await;
        handle.resume();
    };
    let (result, ()) = tokio::join!(work, control);
    assert!(result.is_err());
    assert_eq!(handle.snapshot().status, TaskStatus::Failed);
    assert!(handle.snapshot().reason.unwrap().contains("invalid manual"));
}

#[tokio::test]
async fn batch_deadline_still_applies_while_paused() {
    let runtime = TaskRuntime::new(
        Task::new("trial").unwrap(),
        RunOptions {
            batch_timeout: Duration::from_millis(50),
            ..RunOptions::default()
        },
    );
    let handle = runtime.handle();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        runtime.run(&mut agent(), &mut DemoAdapter::default(), |event| {
            if let Event::BatchProgress(p) = event
                && p.completed == 1
            {
                handle.pause();
            }
        }),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.status, Status::Failed);
    assert_eq!(result.batch.unwrap().status, BatchStatus::TimedOut);
    assert_eq!(handle.snapshot().status, TaskStatus::Failed);
}

#[tokio::test]
async fn reused_provider_logs_distinct_tasks_and_independent_event_streams() {
    let decisions = Capture::default();
    let mut agent = ManualAgent::new(
        Cursor::new("{\"Completed\":\"first\"}\n{\"Completed\":\"second\"}\n"),
        std::io::sink(),
    )
    .with_log_writer(decisions.clone());
    let mut ids = vec![];
    for _ in 0..2 {
        let events = Capture::default();
        let runtime = runtime().with_log_writer(events.clone());
        let id = runtime.handle().snapshot().task_id;
        runtime
            .run(&mut agent, &mut DemoAdapter::default(), |_| {})
            .await
            .unwrap();
        let records = events.records();
        assert!(records.iter().all(|r| r["task_id"] == id));
        assert_eq!(records.last().unwrap()["data"]["status"], "completed");
        assert_eq!(
            records.last().unwrap()["data"]["summary"]["model_requests"]["total"],
            0
        );
        ids.push(id);
    }
    assert_ne!(ids[0], ids[1]);
    let records = decisions.records();
    assert_eq!(records.len(), 4);
    assert_eq!(records[0]["task_id"], ids[0]);
    assert_eq!(records[2]["task_id"], ids[1]);
    assert_eq!(records[2]["request_id"], 1);
}

#[tokio::test]
async fn dropped_execution_does_not_leave_a_running_handle() {
    let runtime = runtime();
    let handle = runtime.handle();
    handle.pause();
    let mut agent = agent();
    let mut adapter = DemoAdapter::default();
    let work = runtime.run(&mut agent, &mut adapter, |_| {});
    assert!(
        tokio::time::timeout(Duration::from_millis(30), work)
            .await
            .is_err()
    );
    assert_eq!(handle.snapshot().status, TaskStatus::Failed);
}

struct BrokenWriter;

#[tokio::test]
async fn pause_after_decision_preserves_it_without_executing_the_action() {
    let runtime = runtime();
    let handle = runtime.handle();
    let mut agent = agent();
    let mut adapter = DemoAdapter::default();
    let work = runtime.run(&mut agent, &mut adapter, |event| {
        if matches!(event, Event::Decided(_)) {
            handle.pause();
        }
    });
    let control = async {
        wait_paused(&handle).await;
        handle.cancel();
    };
    let (result, ()) = tokio::join!(work, control);
    assert_eq!(result.unwrap().status, Status::Cancelled);
    assert_eq!(adapter.observe().unwrap().facts["trial"]["completed"], 0);
}

#[tokio::test]
async fn cancellation_after_last_ordinary_action_reports_fresh_state() {
    let runtime = TaskRuntime::new(
        Task::new("training").unwrap(),
        RunOptions {
            max_decisions: 1,
            ..RunOptions::default()
        },
    );
    let handle = runtime.handle();
    let mut agent = ManualAgent::new(
        Cursor::new(
            "{\"Execute\":{\"then\":\"Decide\",\"actions\":[{\"Call\":{\"name\":\"set_mode\",\"arguments\":{\"mode\":\"training\"}}}]}}\n",
        ),
        std::io::sink(),
    );
    let result = runtime
        .run(&mut agent, &mut DemoAdapter::default(), |event| {
            if matches!(event, Event::Executed(_)) {
                handle.cancel();
            }
        })
        .await
        .unwrap();
    assert_eq!(result.status, Status::Cancelled);
    assert_eq!(result.state.facts["mode"], "training");
}

struct WaitingAgent(Arc<tokio::sync::Notify>);
impl Agent for WaitingAgent {
    async fn decide(
        &mut self,
        _: &Task,
        _: &AppState,
        _: &[Skill],
        _: Option<&ToolResult>,
    ) -> Result<Decision, Error> {
        self.0.notify_one();
        std::future::pending().await
    }
}
#[tokio::test]
async fn cancel_interrupts_an_async_model_wait() {
    let runtime = runtime();
    let handle = runtime.handle();
    let entered = Arc::new(tokio::sync::Notify::new());
    let mut agent = WaitingAgent(entered.clone());
    let mut adapter = DemoAdapter::default();
    let work = runtime.run(&mut agent, &mut adapter, |_| {});
    let control = async {
        entered.notified().await;
        handle.cancel();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(work, control)
    })
    .await
    .unwrap();
    assert_eq!(result.unwrap().status, Status::Cancelled);
    assert_eq!(handle.snapshot().status, TaskStatus::Cancelled);
}
impl Write for BrokenWriter {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("disk full"))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
#[tokio::test]
async fn journal_failure_is_visible_and_prevents_execution() {
    let runtime = runtime().with_log_writer(BrokenWriter);
    let handle = runtime.handle();
    let mut adapter = DemoAdapter::default();
    assert!(matches!(
        runtime.run(&mut agent(), &mut adapter, |_| {}).await,
        Err(Error::Logging(_))
    ));
    assert_eq!(handle.snapshot().status, TaskStatus::Failed);
    assert_eq!(adapter.observe().unwrap().facts["trial"]["completed"], 0);
}
