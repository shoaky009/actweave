//! Interactive decision implementation for CLI debugging, independent of model providers.
use crate::skills::{self, SkillMode, SkillView};
use crate::{
    core::{Agent, AppState, Decision, Error, Skill, Task, ToolResult},
    diagnostics::{DecisionLog, LogEvent},
};
use serde_json::json;
use std::collections::BTreeSet;
use std::io::{BufRead, Write};

/// Reads one JSON Decision per turn. Call arguments need not appear in Skill.calls.
/// This synchronous input implementation is intended for CLI use, not a GUI event thread.
pub struct ManualAgent<R, W> {
    reader: R,
    output: W,
    log: DecisionLog,
}
impl<R: BufRead + Send, W: Write + Send> ManualAgent<R, W> {
    pub fn new(reader: R, output: W) -> Self {
        Self {
            reader,
            output,
            log: DecisionLog::new("manual"),
        }
    }
    pub fn with_log_writer(mut self, writer: impl Write + Send + 'static) -> Self {
        self.log.set_writer(writer);
        self
    }
    fn read_decision(&mut self, input: &serde_json::Value) -> Result<Decision, Error> {
        writeln!(self.output, "[MANUAL] 当前决策上下文：{input}\n[MANUAL] 输入 Execute，actions 为有序 Call/Repeat/UntilDone 列表，then 为 Decide 或 Finish。UntilDone 接收工具调用，按 Adapter 的 Continue 本地循环，Completed 才结束。异常恢复输入 \"Resume\"；或 ReplaceRemaining（同样提供 actions、then）替换未完成部分。挂起循环须首项承接同一 UntilDone，固定批次须首项承接同一 Repeat 且次数填 remaining；原进度和预算保留。finished=true 但观察失败时用 Resume。可执行单个 Call + Decide 修复，或 LoadSkills 加载工具。Completed/Failed 直接结束。")
            .and_then(|()| self.output.flush()).map_err(|e| Error::Invalid(format!("manual prompt: {e}")))?;
        let mut line = String::new();
        if self
            .reader
            .read_line(&mut line)
            .map_err(|e| Error::Invalid(format!("manual input: {e}")))?
            == 0
        {
            return Err(Error::Invalid(
                "manual input ended before a decision was provided".into(),
            ));
        }
        serde_json::from_str(&line)
            .map_err(|e| Error::Invalid(format!("invalid manual Decision JSON: {e}")))
    }
}
impl<R: BufRead + Send, W: Write + Send> Agent for ManualAgent<R, W> {
    fn bind_task(&mut self, task_id: &str, _metrics: crate::metrics::TaskMetrics) {
        self.log.bind_task(task_id);
    }
    async fn decide(
        &mut self,
        task: &Task,
        state: &AppState,
        skills: &[Skill],
        previous: Option<&ToolResult>,
    ) -> Result<Decision, Error> {
        let view = skills::view(skills, SkillMode::All, &BTreeSet::new(), None)?;
        self.decide_with_skills(task, state, &view, previous).await
    }
    async fn decide_with_skills(
        &mut self,
        task: &Task,
        state: &AppState,
        view: &SkillView,
        previous: Option<&ToolResult>,
    ) -> Result<Decision, Error> {
        self.log.next_request();
        let input =
            json!({"task":task,"app_state":state,"skills":view,"previous_tool_result":previous});
        self.log.emit(LogEvent::ManualInput { body: &input })?;
        match self.read_decision(&input) {
            Ok(decision) => {
                self.log.emit(LogEvent::ManualDecision {
                    decision: &decision,
                })?;
                Ok(decision)
            }
            Err(error) => {
                self.log.emit(LogEvent::ManualError {
                    message: error.to_string(),
                })?;
                Err(error)
            }
        }
    }
}
