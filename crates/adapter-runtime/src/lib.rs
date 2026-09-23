//! Standalone skill debugging host. No models, task planner or Core dependency.
use adapter_sdk::*;

pub struct Runtime {
    adapter: RegisteredAdapter,
    host: Host,
    goal: String,
    step: usize,
    previous: Option<ToolResult>,
    failure: Option<ExecutionFailure>,
}
impl Runtime {
    pub fn new(
        registry: &Registry,
        name: &str,
        goal: impl Into<String>,
        host: Host,
    ) -> Result<Self, AdapterError> {
        Ok(Self {
            adapter: registry.create(name, host.clone())?,
            host,
            goal: goal.into(),
            step: 0,
            previous: None,
            failure: None,
        })
    }
    pub fn observe(&self) -> Result<AppState, AdapterError> {
        self.adapter.observe()
    }
    pub fn features(&self) -> Result<Vec<Feature>, AdapterError> {
        self.adapter.features()
    }
    pub fn skills(&self) -> Result<DecisionContext, AdapterError> {
        let state = self.observe()?;
        self.adapter.decision_context(&SkillContext {
            state: &state,
            task_goal: &self.goal,
            decision_step: self.step + 1,
            previous_result: self.previous.as_ref(),
            failure: self.failure.as_ref(),
            interruption: None,
        })
    }
    /// Executes one explicitly selected skill; cancellation/deadline are cooperative.
    /// Arguments remain adapter-owned. No automatic retries or model calls.
    pub async fn invoke(
        &mut self,
        call: &ToolCall,
        control: &ExecutionControl,
    ) -> Result<ActionReport, AdapterError> {
        control.check()?;
        let context = self.skills()?;
        let skill = context
            .skills
            .iter()
            .find(|s| s.name == call.name)
            .ok_or_else(|| AdapterError::Invalid(format!("unknown skill: {}", call.name)))?;
        if !skill.availability.is_available() {
            return Err(AdapterError::Invalid(format!(
                "skill unavailable: {}",
                call.name
            )));
        }
        self.step += 1;
        self.host.info(&format!(
            "步骤 {}：调用 {}，参数 {}",
            self.step, call.name, call.arguments
        ));
        let result = self.adapter.execute(call, control).await;
        let (success, outcome, message) = match &result {
            Ok(report) => (
                !matches!(report.outcome, ActionOutcome::Interrupted),
                Some(report.outcome.clone()),
                report.message.clone(),
            ),
            Err(error) => (false, None, error.to_string()),
        };
        self.failure = if success {
            None
        } else {
            Some(ExecutionFailure {
                stage: FailureStage::Execution,
                call: Some(call.clone()),
                message: message.clone(),
            })
        };
        self.previous = Some(ToolResult {
            call: call.clone(),
            success,
            outcome,
            message: message.clone(),
        });
        self.host.info(&format!(
            "{}：{}",
            if success {
                "正常返回"
            } else {
                "执行中断或失败"
            },
            message
        ));
        result
    }
}
