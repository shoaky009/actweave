use actweave::{
    adapter::DemoAdapter,
    core::{Adapter, Agent, Error, SkillContext, Status, Task, run},
    jev::JevAgent,
};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

struct ObservationFailure {
    demo: DemoAdapter,
    fail_next: std::cell::Cell<bool>,
    calls: usize,
}
impl Adapter for ObservationFailure {
    fn observe(&self) -> Result<actweave::core::AppState, actweave::core::AdapterError> {
        if self.fail_next.replace(false) {
            Err(actweave::core::AdapterError::Runtime(
                "observation lost".into(),
            ))
        } else {
            self.demo.observe()
        }
    }
    fn decision_context(
        &self,
        context: &SkillContext<'_>,
    ) -> Result<actweave::core::DecisionContext, actweave::core::AdapterError> {
        self.demo.decision_context(context)
    }
    async fn execute(
        &mut self,
        call: &actweave::core::ToolCall,
        control: &actweave::core::ExecutionControl,
    ) -> Result<adapter_api::ActionReport, actweave::core::AdapterError> {
        self.calls += 1;
        let report = self.demo.execute(call, control).await?;
        self.fail_next.set(true);
        Ok(report)
    }
}

#[tokio::test]
async fn jev_receives_observation_failure_separately_from_confirmed_success() {
    let mut first: Value = serde_json::from_str(&response("call_1")).unwrap();
    first["answers"]["continuation"]["choice"] = json!("finish");
    let (endpoint, server) = serve(vec![(200, first.to_string()), (200, response("resume_4"))]);
    let capture = Capture::default();
    let mut agent = JevAgent::new("test-auth-secret".into(), endpoint, "test-model".into())
        .unwrap()
        .with_log_writer(capture.clone());
    let mut adapter = ObservationFailure {
        demo: DemoAdapter::default(),
        fail_next: std::cell::Cell::new(false),
        calls: 0,
    };
    let outcome = run(
        &Task::new("switch mode").unwrap(),
        &mut agent,
        &mut adapter,
        2,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.summary.model_requests.total, 2);
    assert_eq!(outcome.summary.model_requests.succeeded, 2);
    assert_eq!(outcome.summary.actions.total, 1);
    assert_eq!(outcome.summary.actions.failed, 0);
    assert_eq!(outcome.summary.interruptions, 1);
    assert_eq!(adapter.calls, 1);
    let requests = server.join().unwrap();
    let context = &requests[1]["state"];
    assert_eq!(context["latest_failure"]["stage"], "observation");
    assert_eq!(
        context["latest_failure"]["call"]["arguments"],
        json!({"mode":"training"})
    );
    assert_eq!(context["previous_tool_result"]["success"], true);
    assert_eq!(
        context["previous_tool_result"]["outcome"]["status"],
        "completed"
    );
    assert_eq!(context["pending_execution"]["completed"], 1);
    assert!(
        context["adapter_guidance"]
            .as_str()
            .unwrap()
            .contains("不能重做")
    );
    let records = capture.records();
    let logged: Vec<_> = records
        .iter()
        .filter(|r| r["event"] == "model_request")
        .collect();
    assert_eq!(logged[1]["body"], requests[1]);
}

#[tokio::test]
async fn manual_diagnostics_receive_current_guidance_and_unresolved_interruption() {
    let input = concat!(
        "{\"Execute\":{\"actions\":[{\"UntilDone\":{\"name\":\"explore_step\",\"arguments\":{}}}],\"then\":\"Finish\"}}\n",
        "{\"Execute\":{\"actions\":[{\"Call\":{\"name\":\"reset_exploration\",\"arguments\":{}}}],\"then\":\"Decide\"}}\n",
        "\"Resume\"\n"
    );
    let capture = Capture::default();
    let mut agent =
        actweave::manual::ManualAgent::new(std::io::Cursor::new(input), std::io::sink())
            .with_log_writer(capture.clone());
    let outcome = run(
        &Task::new("explore").unwrap(),
        &mut agent,
        &mut DemoAdapter::new(actweave::adapter::Scenario::ExplorationInterrupted),
        3,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    let records = capture.records();
    let inputs: Vec<_> = records
        .iter()
        .filter(|r| r["event"] == "manual_input")
        .collect();
    assert_eq!(inputs.len(), 3);
    assert!(
        inputs[1]["body"]["skills"]["guidance"]
            .as_str()
            .unwrap()
            .contains("reset_exploration")
    );
    assert_eq!(inputs[1]["body"]["skills"]["failure"]["stage"], "execution");
    let repaired = &inputs[2]["body"];
    assert!(repaired["skills"].get("failure").is_none());
    assert!(
        repaired["skills"]["guidance"]
            .as_str()
            .unwrap()
            .contains("继续原先中断")
    );
    assert_eq!(
        repaired["skills"]["execution"]["failure"]["call"]["name"],
        "explore_step"
    );
}
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
        let bytes = self.0.lock().unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(!text.contains("test-auth-secret"));
        assert!(!text.to_lowercase().contains("authorization"));
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

/// Capture actual HTTP bodies, so tests compare logs against what was sent on the wire.
fn serve(replies: Vec<(u16, String)>) -> (String, std::thread::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
    let thread = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let mut requests = vec![];
        for (status, response) in replies {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(e) => panic!("mock server did not receive request: {e}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = vec![];
            let mut byte = [0];
            while !bytes.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                bytes.push(byte[0]);
            }
            let headers = String::from_utf8(bytes).unwrap().to_lowercase();
            assert!(headers.starts_with("post /v1/systemone "));
            assert!(headers.contains("authorization: bearer test-auth-secret"));
            let length: usize = headers
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            requests.push(serde_json::from_slice(&body).unwrap());
            write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
        }
        requests
    });
    (url, thread)
}
fn response(choice: &str) -> String {
    json!({"model":"test-model","answers":{"next_action":{"type":"choice","choice":choice,"confidence":0.99},"continuation":{"type":"choice","choice":"decide"}}}).to_string()
}

fn sequence_response(actions: &[&str]) -> String {
    let mut body = json!({"answers":{"next_action":{"type":"choice","choice":"sequence"},"continuation":{"type":"choice","choice":"finish"}}});
    for slot in 1..=4 {
        body["answers"][format!("sequence_action_{slot}")] =
            json!({"type":"choice","choice":actions.get(slot - 1).copied().unwrap_or("end")});
    }
    body.to_string()
}

fn until_done_response() -> String {
    let mut body: Value = serde_json::from_str(&response("until_done_5")).unwrap();
    body["answers"]["continuation"]["choice"] = json!("finish");
    body.to_string()
}

#[tokio::test]
async fn jev_until_done_completes_unknown_work_with_one_http_request() {
    let (endpoint, server) = serve(vec![(200, until_done_response())]);
    let mut agent =
        JevAgent::new("test-auth-secret".into(), endpoint, "test-model".into()).unwrap();
    let outcome = run(
        &Task::new("探索全部物品").unwrap(),
        &mut agent,
        &mut DemoAdapter::new(actweave::adapter::Scenario::Exploration),
        1,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.batch.unwrap().attempts, 4);
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]["questions"]["next_action"]["criteria"]["until_done_5"]["until_done"]["name"],
        "explore_step"
    );
    assert_eq!(
        requests[0]["state"]["app_state"]["facts"]["exploration"]["collected"],
        0
    );
}

#[tokio::test]
async fn jev_until_done_sequence_runs_tail_without_an_extra_request() {
    let (endpoint, server) = serve(vec![(
        200,
        sequence_response(&["call_1", "until_done_5", "call_2"]),
    )]);
    let mut agent =
        JevAgent::new("test-auth-secret".into(), endpoint, "test-model".into()).unwrap();
    let outcome = run(
        &Task::new("准备、探索并观察").unwrap(),
        &mut agent,
        &mut DemoAdapter::new(actweave::adapter::Scenario::Exploration),
        1,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.execution.unwrap().completed, 3);
    assert_eq!(outcome.state.facts["exploration"]["finished"], true);
    assert_eq!(server.join().unwrap().len(), 1);
}

#[tokio::test]
async fn jev_loop_exception_exposes_progress_and_supports_replacement_after_repair() {
    let mut replacement = json!({"answers": {
        "next_action":{"type":"choice","choice":"replace_remaining"},
        "replacement_continuation":{"type":"choice","choice":"finish"}
    }});
    for (slot, key) in ["until_done_12", "call_7", "end", "end"]
        .into_iter()
        .enumerate()
    {
        replacement["answers"][format!("replacement_action_{}", slot + 1)] =
            json!({"type":"choice","choice":key});
    }
    let (endpoint, server) = serve(vec![
        (200, until_done_response()),
        (200, response("call_4")),
        (200, replacement.to_string()),
    ]);
    let mut agent =
        JevAgent::new("test-auth-secret".into(), endpoint, "test-model".into()).unwrap();
    let outcome = run(
        &Task::new("探索全部物品并观察").unwrap(),
        &mut agent,
        &mut DemoAdapter::new(actweave::adapter::Scenario::ExplorationInterrupted),
        3,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.state.facts["exploration"]["collected"], 3);
    assert_eq!(outcome.execution.unwrap().completed, 2);
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1]["state"]["pending_batch"]["completed"], 1);
    assert!(
        requests[0]["state"]["adapter_guidance"]
            .as_str()
            .unwrap()
            .contains("数量未知")
    );
    assert!(
        requests[1]["state"]["adapter_guidance"]
            .as_str()
            .unwrap()
            .contains("reset_exploration")
    );
    assert!(
        requests[2]["state"]["adapter_guidance"]
            .as_str()
            .unwrap()
            .contains("继续原先中断")
    );
    assert!(
        !requests[2]["state"]["adapter_guidance"]
            .as_str()
            .unwrap()
            .contains("reset_exploration")
    );
    assert_eq!(requests[1]["state"]["latest_failure"]["stage"], "execution");
    assert!(requests[2]["state"].get("latest_failure").is_none());
    assert_eq!(
        requests[2]["state"]["previous_tool_result"]["success"],
        true
    );
    assert_eq!(
        requests[2]["state"]["pending_execution"]["failure"]["call"]["name"],
        "explore_step"
    );
    assert_eq!(
        requests[1]["state"]["previous_tool_result"]["outcome"]["status"],
        "interrupted"
    );
    assert_eq!(
        requests[1]["state"]["pending_batch"]["remaining"],
        Value::Null
    );
    assert_eq!(requests[2]["state"]["pending_batch"]["attempts"], 2);
    assert_eq!(
        requests[2]["state"]["replacement_actions"]["until_done_12"]["until_done"]["name"],
        "explore_step"
    );
}

#[tokio::test]
async fn sequence_selects_repeat_count_in_same_http_request() {
    let mut reply: Value =
        serde_json::from_str(&sequence_response(&["call_1", "repeat_4", "call_2"])).unwrap();
    reply["answers"]["sequence_count_2"] = json!({"type":"choice","choice":"10"});
    let (endpoint, server) = serve(vec![(200, reply.to_string())]);
    let mut agent =
        JevAgent::new("test-auth-secret".into(), endpoint, "test-model".into()).unwrap();
    let outcome = run(
        &Task::new("prepare, ten trials, inspect").unwrap(),
        &mut agent,
        &mut DemoAdapter::default(),
        1,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.state.facts["trial"]["completed"], 10);
    assert_eq!(outcome.execution.unwrap().completed, 3);
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0]["questions"].get("sequence_count_2").is_some());
}

#[tokio::test]
async fn max_actions_one_omits_sequence_questions_and_future_candidates() {
    let mut reply: Value = serde_json::from_str(&response("call_1")).unwrap();
    reply["answers"]["continuation"]["choice"] = json!("finish");
    let (endpoint, server) = serve(vec![(200, reply.to_string())]);
    let mut agent =
        JevAgent::new("test-auth-secret".into(), endpoint, "test-model".into()).unwrap();
    let result = actweave::core::run_with_options(
        &Task::new("training mode").unwrap(),
        &mut agent,
        &mut DemoAdapter::default(),
        actweave::core::RunOptions {
            max_actions: 1,
            ..Default::default()
        },
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(result.status, Status::Completed);
    let requests = server.join().unwrap();
    assert!(requests[0]["questions"].get("sequence_action_1").is_none());
    assert!(
        requests[0]["questions"]["next_action"]["criteria"]
            .get("sequence")
            .is_none()
    );
    assert!(requests[0]["state"].get("future_actions").is_none());
}

#[tokio::test]
async fn one_http_request_selects_three_ordered_skills_with_future_prerequisites() {
    let (endpoint, server) = serve(vec![(
        200,
        sequence_response(&["call_1", "call_5", "call_3"]),
    )]);
    let mut agent =
        JevAgent::new("test-auth-secret".into(), endpoint, "test-model".into()).unwrap();
    let outcome = run(
        &Task::new("切换模式、启动训练并读取状态").unwrap(),
        &mut agent,
        &mut DemoAdapter::default(),
        1,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.execution.unwrap().completed, 3);
    assert_eq!(outcome.state.facts["mode"], "training");
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]["questions"]["sequence_action_1"]["criteria"]
            .get("call_5")
            .is_none()
    );
    assert!(
        requests[0]["questions"]["sequence_action_2"]["criteria"]
            .get("call_5")
            .is_some()
    );
    assert_eq!(
        requests[0]["state"]["future_actions"]["call_5"]["action"]["tool_call"]["name"],
        "start_training"
    );
    // Slot choices reuse candidate IDs, rather than duplicating tool arguments.
    assert!(requests[0]["questions"]["sequence_action_2"]["criteria"]["call_5"].is_null());
}

#[tokio::test]
async fn exception_replacement_is_selected_in_one_request_and_keeps_completed_prefix() {
    let mut reply = json!({"answers": {
        "next_action":{"type":"choice","choice":"replace_remaining"},
        "replacement_continuation":{"type":"choice","choice":"finish"}
    }});
    for slot in 1..=4 {
        reply["answers"][format!("replacement_action_{slot}")] =
            json!({"type":"choice","choice":if slot == 1 { "call_7" } else { "end" }});
    }
    let (endpoint, server) = serve(vec![
        (200, sequence_response(&["call_1", "call_5", "call_3"])),
        (200, reply.to_string()),
    ]);
    let mut agent =
        JevAgent::new("test-auth-secret".into(), endpoint, "test-model".into()).unwrap();
    let outcome = run(
        &Task::new("inspect after interruption").unwrap(),
        &mut agent,
        &mut DemoAdapter::new(actweave::adapter::Scenario::TransientFailure),
        2,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.execution.unwrap().completed, 2);
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]["questions"]
            .get("replacement_action_1")
            .is_none()
    );
    assert_eq!(
        requests[1]["state"]["replacement_actions"]["call_7"]["tool_call"]["name"],
        "get_state"
    );
}

#[tokio::test]
async fn sequence_exception_returns_cursor_and_resumes_without_replaying_prefix() {
    let (endpoint, server) = serve(vec![
        (200, sequence_response(&["call_1", "call_5", "call_3"])),
        (200, response("resume_4")),
    ]);
    let mut agent =
        JevAgent::new("test-auth-secret".into(), endpoint, "test-model".into()).unwrap();
    let mut started = vec![];
    let outcome = run(
        &Task::new("启动训练并读取状态").unwrap(),
        &mut agent,
        &mut DemoAdapter::new(actweave::adapter::Scenario::TransientFailure),
        2,
        |event| {
            if let actweave::core::Event::ActionStarted { call, .. } = event {
                started.push(call.name);
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(
        started,
        [
            "set_mode",
            "start_training",
            "start_training",
            "get_training_status"
        ]
    );
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1]["state"]["pending_execution"]["completed"], 1);
    assert!(requests[1]["questions"].get("sequence_action_1").is_none());
}

#[tokio::test]
async fn sequence_rejects_future_only_action_in_first_slot_without_effects() {
    let (endpoint, server) = serve(vec![(200, sequence_response(&["call_5", "call_1"]))]);
    let mut agent =
        JevAgent::new("test-auth-secret".into(), endpoint, "test-model".into()).unwrap();
    let mut environment = DemoAdapter::default();
    let result = run(
        &Task::new("invalid sequence").unwrap(),
        &mut agent,
        &mut environment,
        1,
        |_| {},
    )
    .await;
    server.join().unwrap();
    assert!(matches!(result, Err(Error::Model(_))));
    assert_eq!(environment.observe().unwrap().facts["mode"], "idle");
}

#[tokio::test]
async fn single_action_finish_uses_one_http_request_and_no_confirmation() {
    let mut reply: Value = serde_json::from_str(&response("call_1")).unwrap();
    reply["answers"]["continuation"]["choice"] = json!("finish");
    let (endpoint, server) = serve(vec![(200, reply.to_string())]);
    let mut agent =
        JevAgent::new("test-auth-secret".into(), endpoint, "test-model".into()).unwrap();
    let outcome = run(
        &Task::new("training mode").unwrap(),
        &mut agent,
        &mut DemoAdapter::default(),
        1,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.state.facts["mode"], "training");
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]["questions"]["continuation"]["criteria"]
            .get("finish")
            .is_some()
    );
}

#[tokio::test]
async fn repeat_decide_returns_for_next_action_instead_of_finishing_task() {
    let mut first: Value = serde_json::from_str(&repeat_response("2")).unwrap();
    first["answers"]["continuation"]["choice"] = json!("decide");
    let mut second: Value = serde_json::from_str(&response("call_1")).unwrap();
    second["answers"]["continuation"]["choice"] = json!("finish");
    let (endpoint, server) = serve(vec![(200, first.to_string()), (200, second.to_string())]);
    let mut agent =
        JevAgent::new("test-auth-secret".into(), endpoint, "test-model".into()).unwrap();
    let outcome = run(
        &Task::new("two trials then training mode").unwrap(),
        &mut agent,
        &mut DemoAdapter::default(),
        2,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.state.facts["mode"], "training");
    assert_eq!(outcome.state.facts["trial"]["completed"], 2);
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1]["state"].get("pending_batch").is_none());
}

#[tokio::test]
async fn logs_exact_model_input_response_and_tool_selection_across_rounds() {
    let (endpoint, server) = serve(vec![
        (200, response("call_1")),
        (200, response("completed")),
    ]);
    let capture = Capture::default();
    let mut agent = JevAgent::new("test-auth-secret".into(), endpoint, "test-model".into())
        .unwrap()
        .with_log_writer(capture.clone());
    let outcome = run(
        &Task::new("切换到训练模式").unwrap(),
        &mut agent,
        &mut DemoAdapter::default(),
        3,
        |_| {},
    )
    .await
    .unwrap();
    let requests = server.join().unwrap();
    let logs = capture.records();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.state.facts["mode"], "training");
    assert_eq!(logs.len(), 6);
    for (round, request) in requests.iter().enumerate() {
        let offset = round * 3;
        assert_eq!(logs[offset]["event"], "model_request");
        assert_eq!(&logs[offset]["body"], request);
        assert_eq!(logs[offset + 1]["event"], "model_response");
        assert_eq!(logs[offset + 2]["event"], "model_decision");
        for record in &logs[offset..offset + 3] {
            assert_eq!(record["request_id"], round + 1);
            assert_eq!(record["session_id"], logs[0]["session_id"]);
            assert!(record["timestamp_ms"].as_u64().unwrap() > 0);
        }
    }
    assert_eq!(requests[0]["state"]["task"]["goal"], "切换到训练模式");
    assert_eq!(
        requests[1]["state"]["app_state"]["facts"]["mode"],
        "training"
    );
    assert_eq!(
        requests[1]["state"]["previous_tool_result"]["success"],
        true
    );
    assert_eq!(
        logs[1]["body"]["answers"]["next_action"]["choice"],
        "call_1"
    );
    assert_eq!(
        logs[2]["decision"]["Execute"]["actions"][0]["Call"],
        json!({"name":"set_mode","arguments":{"mode":"training"}})
    );
    assert!(logs[5]["decision"].get("Completed").is_some());
    let initial_skills = requests[0]["state"]["available_skills"].as_array().unwrap();
    assert!(
        !initial_skills
            .iter()
            .any(|skill| skill["name"] == "start_training")
    );
    let directory = requests[0]["state"]["skill_directory"].as_array().unwrap();
    let unavailable = directory
        .iter()
        .find(|skill| skill["name"] == "start_training")
        .unwrap();
    assert_eq!(unavailable["tags"], json!(["training", "write"]));
    assert_eq!(unavailable["availability"]["status"], "unavailable");
    assert!(unavailable.get("parameters").is_none());
    assert!(unavailable.get("calls").is_none());
    assert!(
        requests[0]["questions"]["next_action"]["criteria"]
            .as_object()
            .unwrap()
            .values()
            .all(|option| option["tool_call"]["name"] != "start_training")
    );
    assert!(
        requests[1]["state"]["available_skills"]
            .as_array()
            .unwrap()
            .iter()
            .any(|skill| skill["name"] == "start_training")
    );
}

#[tokio::test]
async fn logs_service_and_protocol_errors_without_authentication_data() {
    for (status, body) in [
        (401, "private service message".into()),
        (200, "not json".into()),
        (200, response("unknown")),
    ] {
        let (endpoint, server) = serve(vec![(status, body)]);
        let capture = Capture::default();
        let mut agent = JevAgent::new("test-auth-secret".into(), endpoint, "test-model".into())
            .unwrap()
            .with_log_writer(capture.clone());
        let environment = DemoAdapter::default();
        let result = agent
            .decide(
                &Task::new("training").unwrap(),
                &environment.observe().unwrap(),
                &environment
                    .decision_context(&SkillContext {
                        state: &environment.observe().unwrap(),
                        task_goal: "training",
                        decision_step: 1,
                        previous_result: None,
                        failure: None,
                        interruption: None,
                    })
                    .unwrap()
                    .skills,
                None,
            )
            .await;
        server.join().unwrap();
        assert!(matches!(result, Err(Error::Model(_))));
        let logs = capture.records();
        assert_eq!(logs[0]["event"], "model_request");
        assert_eq!(logs.last().unwrap()["event"], "model_error");
        assert!(logs.iter().all(|log| log["event"] != "model_decision"));
    }
}

#[tokio::test]
async fn log_write_failure_is_reported_before_sending_request() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut agent = JevAgent::new(
        "test-auth-secret".into(),
        "http://127.0.0.1:1".into(),
        "test-model".into(),
    )
    .unwrap()
    .with_log_writer(Broken);
    let runtime = actweave::task_runtime::TaskRuntime::new(
        Task::new("training").unwrap(),
        actweave::core::RunOptions::default(),
    );
    let handle = runtime.handle();
    let result = runtime
        .run(&mut agent, &mut DemoAdapter::default(), |_| {})
        .await;
    assert!(matches!(result, Err(Error::Logging(_))));
    assert_eq!(handle.snapshot().summary.unwrap().model_requests.total, 0);
}

#[tokio::test]
async fn http_and_protocol_errors_are_counted_even_without_an_outcome() {
    for reply in [
        (500, "{}".into()),
        (200, "invalid JSON".into()),
        (200, response("not_offered")),
    ] {
        let (endpoint, server) = serve(vec![reply]);
        let mut agent =
            JevAgent::new("test-auth-secret".into(), endpoint, "test-model".into()).unwrap();
        let runtime = actweave::task_runtime::TaskRuntime::new(
            Task::new("training").unwrap(),
            actweave::core::RunOptions::default(),
        );
        let handle = runtime.handle();
        assert!(
            runtime
                .run(&mut agent, &mut DemoAdapter::default(), |_| {})
                .await
                .is_err()
        );
        assert_eq!(server.join().unwrap().len(), 1);
        let summary = handle.snapshot().summary.unwrap();
        assert_eq!(
            summary.model_requests,
            actweave::metrics::CallCounts {
                total: 1,
                succeeded: 0,
                failed: 1
            }
        );
        assert_eq!(summary.actions.total, 0);
    }
}

#[tokio::test]
async fn discovery_requests_omit_unloaded_definitions_and_duplicate_candidates() {
    use actweave::{
        core::{RunOptions, run_with_options},
        skills::SkillMode,
    };
    let (endpoint, server) = serve(vec![
        (200, response("load_0")),
        (200, response("call_1")),
        (200, response("completed")),
    ]);
    let mut agent =
        JevAgent::new("test-auth-secret".into(), endpoint, "test-model".into()).unwrap();
    let outcome = run_with_options(
        &Task::new("training mode").unwrap(),
        &mut agent,
        &mut DemoAdapter::default(),
        RunOptions {
            max_decisions: 4,
            skill_mode: SkillMode::OnDemand,
            ..RunOptions::default()
        },
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    let requests = server.join().unwrap();
    assert_eq!(requests[0]["state"]["available_skills"], json!([]));
    assert_eq!(
        requests[0]["state"]["skill_directory"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
    let criteria = &requests[0]["questions"]["next_action"]["criteria"];
    assert!(
        criteria
            .as_object()
            .unwrap()
            .values()
            .any(|v| v.get("load_tag").is_some())
    );
    assert!(
        criteria
            .as_object()
            .unwrap()
            .values()
            .all(|v| v.get("tool_call").is_none())
    );
    assert_eq!(
        requests[1]["state"]["available_skills"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        requests[1]["state"]["available_skills"][0]["name"],
        "set_mode"
    );
    assert_eq!(
        requests[1]["state"]["last_load"]["selected"],
        json!(["set_mode"])
    );
    assert!(requests[1]["state"].get("previous_tool_result").is_none());
    for request in &requests {
        for field in ["available_skills", "skill_directory"] {
            if let Some(skills) = request["state"][field].as_array() {
                assert!(
                    skills
                        .iter()
                        .all(|s| s.get("calls").is_none() && s.get("parameters").is_none())
                );
            }
        }
        for option in request["questions"]["next_action"]["criteria"]
            .as_object()
            .unwrap()
            .values()
        {
            assert!(option.get("description").is_none());
        }
    }
    assert_eq!(
        requests[2]["state"]["previous_tool_result"]["call"]["name"],
        "set_mode"
    );
}

fn repeat_response(times: &str) -> String {
    json!({"answers":{"next_action":{"type":"choice","choice":"repeat_4"},"repeat_count":{"type":"choice","choice":times},"continuation":{"type":"choice","choice":"finish"}}}).to_string()
}
#[tokio::test]
async fn one_http_request_drives_ten_completed_actions_without_final_model_confirmation() {
    let (endpoint, server) = serve(vec![(200, repeat_response("10"))]);
    let mut agent =
        JevAgent::new("test-auth-secret".into(), endpoint, "test-model".into()).unwrap();
    let outcome = run(
        &Task::new("完成10次试验动作").unwrap(),
        &mut agent,
        &mut DemoAdapter::default(),
        1,
        |_| {},
    )
    .await
    .unwrap();
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0]["questions"].get("next_action").is_some());
    assert!(requests[0]["questions"].get("repeat_count").is_some());
    assert!(requests[0]["questions"].get("continuation").is_some());
    assert!(
        requests[0]["questions"]["next_action"]["criteria"]
            .as_object()
            .unwrap()
            .values()
            .all(|v| v["tool_call"]["name"] != "perform_trial")
    );
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.batch.unwrap().completed, 10);
    assert_eq!(outcome.state.facts["trial"]["completed"], 10);
    assert_eq!(outcome.summary.model_requests.total, 1);
    assert_eq!(outcome.summary.model_requests.succeeded, 1);
    assert_eq!(outcome.summary.actions.total, 10);
    assert_eq!(outcome.summary.actions.succeeded, 10);
}
#[tokio::test]
async fn model_only_returns_for_exception_and_resumption_preserves_count() {
    use actweave::adapter::Scenario;
    let (endpoint, server) = serve(vec![
        (200, repeat_response("10")),
        (200, response("call_4")),
        (200, response("resume_4")),
    ]);
    let mut agent =
        JevAgent::new("test-auth-secret".into(), endpoint, "test-model".into()).unwrap();
    let outcome = run(
        &Task::new("完成10次试验动作").unwrap(),
        &mut agent,
        &mut DemoAdapter::new(Scenario::BatchInterrupted),
        3,
        |_| {},
    )
    .await
    .unwrap();
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1]["state"]["pending_batch"]["completed"], 3);
    assert_eq!(requests[1]["state"]["pending_batch"]["remaining"], 7);
    assert_eq!(requests[1]["state"]["pending_batch"]["then"], "Finish");
    assert!(
        requests[1]["questions"]["continuation"]["criteria"]
            .get("finish")
            .is_none()
    );
    assert_eq!(requests[2]["state"]["pending_batch"]["completed"], 3);
    assert!(requests[1]["questions"].get("repeat_count").is_none());
    assert_eq!(outcome.batch.unwrap().completed, 10);
    assert_eq!(outcome.state.facts["trial"]["completed"], 10);
}
