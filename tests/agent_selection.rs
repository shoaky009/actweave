use actweave::{adapter::DemoAdapter, core::*, manual::ManualAgent};
use serde_json::json;
use std::{
    io::{Cursor, Write},
    process::{Command, Stdio},
};

/// Same adapter behavior, but without finite action suggestions.
struct SchemaOnly(DemoAdapter);
impl Adapter for SchemaOnly {
    fn observe(&self) -> Result<AppState, AdapterError> {
        self.0.observe()
    }
    fn decision_context(
        &self,
        context: &SkillContext<'_>,
    ) -> Result<DecisionContext, AdapterError> {
        let mut decision = self.0.decision_context(context)?;
        for skill in &mut decision.skills {
            skill.calls.clear();
        }
        Ok(decision)
    }
    async fn execute(
        &mut self,
        call: &ToolCall,
        control: &ExecutionControl,
    ) -> Result<adapter_api::ActionReport, AdapterError> {
        self.0.execute(call, control).await
    }
}

#[test]
fn skills_without_candidates_deserialize_and_preserve_parameter_contract() {
    let schema =
        json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]});
    let skill: Skill = serde_json::from_value(
        json!({"name":"say","description":"say arbitrary text","parameters":schema}),
    )
    .unwrap();
    assert!(skill.calls.is_empty());
    let output = serde_json::to_value(skill).unwrap();
    assert_eq!(output["parameters"], schema);
    assert!(output.get("calls").is_none());
}

#[tokio::test]
async fn manual_agent_uses_schema_only_tools_and_adapter_still_rejects_invalid_arguments() {
    let input = concat!(
        "{\"Execute\":{\"then\":\"Decide\",\"actions\":[{\"Call\":{\"name\":\"set_mode\",\"arguments\":{\"mode\":\"invalid\"}}}]}}\n",
        "{\"Execute\":{\"then\":\"Decide\",\"actions\":[{\"Call\":{\"name\":\"set_mode\",\"arguments\":{\"mode\":\"training\"}}}]}}\n",
        "{\"Completed\":\"observed training mode\"}\n"
    );
    let mut agent = ManualAgent::new(Cursor::new(input), Vec::new());
    let mut results = vec![];
    let outcome = run(
        &Task::new("training mode").unwrap(),
        &mut agent,
        &mut SchemaOnly(DemoAdapter::default()),
        4,
        |event| {
            if let Event::Executed(result) = event {
                results.push(result);
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome.status, Status::Completed);
    assert_eq!(outcome.state.facts["mode"], "training");
    assert!(!results[0].success);
    assert!(results[1].success);
}

#[tokio::test]
async fn manual_eof_and_malformed_decisions_are_errors_not_completion() {
    for input in ["", "not-json\n", "{\"Unknown\":\"done\"}\n"] {
        let mut agent = ManualAgent::new(Cursor::new(input), Vec::new());
        let result = run(
            &Task::new("training").unwrap(),
            &mut agent,
            &mut DemoAdapter::default(),
            2,
            |_| {},
        )
        .await;
        assert!(matches!(result, Err(Error::Invalid(_))));
    }
}

#[test]
fn cli_can_select_manual_without_jev_credentials_and_emit_provider_diagnostics() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_actweave"))
        .args([
            "--agent",
            "manual",
            "--log-level",
            "debug",
            "切换到训练模式",
        ])
        .env_remove("JEVKEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"{\"Execute\":{\"actions\":[{\"Call\":{\"name\":\"set_mode\",\"arguments\":{\"mode\":\"training\"}}}],\"then\":\"Decide\"}}\n{\"Completed\":\"done\"}\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stdout.contains("决策器：Manual"));
    assert!(stdout.contains("选择 Skill `set_mode`"));
    assert!(stdout.contains("任务完成，共 2 步决策"));
    assert!(stdout.contains("大模型请求：0 次（成功 0，失败 0）"));
    assert!(stdout.contains("动作调用：1 次（正常 1，中断或报错 0）"));
    assert!(
        stdout
            .lines()
            .last()
            .unwrap()
            .starts_with("[INFO] 总耗时：")
    );
    assert!(!stdout.contains("JEV"));
    let logs: Vec<serde_json::Value> = stderr
        .lines()
        .filter(|line| line.starts_with('{'))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(logs.len(), 4);
    assert!(logs.iter().all(|entry| entry["provider"] == "manual"));
    assert_eq!(logs[0]["event"], "manual_input");
    assert_eq!(logs[1]["event"], "manual_decision");
    assert_eq!(
        logs[1]["decision"]["Execute"]["actions"][0]["Call"]["arguments"],
        json!({"mode":"training"})
    );
}

#[test]
fn cli_prints_summary_when_the_decision_provider_errors() {
    let output = Command::new(env!("CARGO_BIN_EXE_actweave"))
        .args(["--agent", "manual", "test error summary"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("任务结束：失败"));
    assert!(stdout.contains("大模型请求：0 次（成功 0，失败 0）"));
    assert!(stdout.contains("动作调用：0 次（正常 0，中断或报错 0）"));
    assert!(
        stdout
            .lines()
            .last()
            .unwrap()
            .starts_with("[INFO] 总耗时：")
    );
}
