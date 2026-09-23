//! Minimal adapter demonstrating semantic tools over simulated application state.
use crate::runtime::{DemoRuntime, ExplorationSnapshot, Mode, RuntimeError, TrainingStatus};
use adapter_sdk::{
    ActionOutcome, ActionReport, Adapter, AdapterError as Error, AppState, Availability,
    DecisionContext, ExecutionControl, FailureStage, Skill, SkillContext, ToolCall,
};
pub use runtime::Scenario;
pub mod runtime;
mod trial_flow;
use serde::Deserialize;
use serde_json::json;

#[derive(Default)]
pub struct DemoAdapter {
    runtime: DemoRuntime,
}

/// Shared registration entry used by both the application and standalone dev host.
pub fn register(registry: &mut adapter_sdk::Registry, scenario: Scenario) -> Result<(), Error> {
    registry.register("demo", move |host| {
        host.info("Demo Adapter 已加载");
        Ok(DemoAdapter::new(scenario))
    })
}
impl DemoAdapter {
    pub fn new(scenario: Scenario) -> Self {
        Self {
            runtime: DemoRuntime::new(scenario),
        }
    }
}

fn runtime_error(error: RuntimeError) -> Error {
    Error::Runtime(
        match error {
            RuntimeError::WrongMode => "需要先通过 set_mode 切换到 training 模式，再启动训练。",
            RuntimeError::Busy => "训练服务暂时繁忙，本次未启动；现在可以重试 start_training。",
            RuntimeError::Unavailable => "训练服务永久不可用，当前工具无法恢复，请判断任务失败。",
            RuntimeError::AlreadyStarted => {
                "本次训练已启动，不能重复启动；请查询 get_training_status。"
            }
            RuntimeError::TrialNeedsReset => {
                "试验装置需要复位，本次动作未完成；请调用 reset_trial 后恢复批次。"
            }
            RuntimeError::TrialUnavailable => "试验装置永久不可用。",
            RuntimeError::TrainingInProgress => "训练正在进行，无法切换模式，请等待训练完成。",
            RuntimeError::ExplorationNeedsReset => {
                "探索设备中断，先调用 reset_exploration，再恢复原循环；已收集物品保留。"
            }
        }
        .into(),
    )
}

fn no_args(call: &ToolCall) -> Result<(), Error> {
    if call.arguments != json!({}) {
        return Err(Error::Invalid(format!("{} expects {{}}", call.name)));
    }
    Ok(())
}

fn empty_skill(name: &str, description: &str, tags: &[&str]) -> Skill {
    Skill {
        name: name.into(),
        repeatable: false,
        loopable: false,
        description: description.into(),
        tags: tags.iter().map(|tag| (*tag).into()).collect(),
        availability: Availability::Available,
        parameters: json!({"type":"object","properties":{},"additionalProperties":false}),
        calls: vec![ToolCall {
            name: name.into(),
            arguments: json!({}),
        }],
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetMode {
    mode: Mode,
}
#[derive(Deserialize)]
struct Snapshot {
    mode: Mode,
    training: TrainingSnapshot,
    #[serde(default)]
    trial: TrialSnapshot,
    #[serde(default)]
    exploration: Option<ExplorationSnapshot>,
}
#[derive(Deserialize)]
struct TrainingSnapshot {
    status: TrainingStatus,
    available: bool,
}

#[derive(Deserialize, Default)]
struct TrialSnapshot {
    ready: bool,
    needs_reset: bool,
}

fn guidance(snapshot: &Snapshot, context: &SkillContext<'_>) -> String {
    if context
        .failure
        .is_some_and(|f| f.stage == FailureStage::Observation)
    {
        return "最近一次观察失败，当前状态可能过期。先恢复观察；工具已经确认完成的动作不能重做，不能把观察失败当作动作失败。".into();
    }
    if let Some(exploration) = snapshot.exploration {
        if exploration.needs_reset {
            return "探索设备需要恢复：先使用 reset_exploration，保留已收集物品和未完成探索，不要重新开始。".into();
        }
        if context
            .interruption
            .and_then(|f| f.call.as_ref())
            .is_some_and(|c| c.name == "explore_step")
        {
            return "探索设备已可用，继续原先中断的探索，保留已有进度；修复成功不代表探索目标完成。".into();
        }
        return if exploration.finished {
            "Adapter 已确认探索完成；若用户目标就是探索该区域，可以结束任务，不要再次探索。"
        } else {
            "若任务要求探索全部物品，使用 explore_step 的本地循环。数量未知，由 Adapter 确认完成，不能凭一次收集或暂未发现物品就结束。"
        }.into();
    }
    if snapshot.trial.needs_reset {
        return "试验设备需要 reset_trial；复位保留已完成次数，随后继续未完成的批次。".into();
    }
    if context
        .interruption
        .and_then(|f| f.call.as_ref())
        .is_some_and(|c| c.name == "perform_trial")
    {
        return "试验设备已可用，继续原批次剩余次数，不要重新下达完整次数。".into();
    }
    if !snapshot.training.available {
        return "训练服务永久不可用；若任务依赖训练，当前工具无法完成它，重复启动不会恢复服务。"
            .into();
    }
    match snapshot.training.status {
        TrainingStatus::Running => "训练已经启动但尚未完成；需要确认完成时查询 get_training_status，不要再次启动。",
        TrainingStatus::Completed => "训练已经确认完成；若用户目标已满足，可以结束任务，不要重新启动训练。",
        TrainingStatus::NotStarted if snapshot.mode != Mode::Training => "若任务需要训练，先用 set_mode 切换到 training，再启动；切换模式不等于完成训练。",
        TrainingStatus::NotStarted if context.previous_result.is_some_and(|r| r.call.name == "start_training" && !r.success) => "上次启动未成功，当前仍未启动训练且前置条件已满足；可以继续原中断步骤或重试启动，不要误判训练正在进行。",
        TrainingStatus::NotStarted => "训练前置条件已满足；若任务需要训练，可以启动，启动后仍需确认训练完成。",
    }.into()
}

impl Adapter for DemoAdapter {
    fn features(&self) -> Result<Vec<adapter_sdk::Feature>, Error> {
        use adapter_sdk::{Feature, Parameter, ParameterKind};
        Ok(vec![Feature {
            id: "repeat_trial".into(),
            name: "重复试验".into(),
            description: "执行指定次数的试验；没有收益也算完成一次。".into(),
            parameters: vec![Parameter {
                id: "times".into(),
                name: "次数".into(),
                kind: ParameterKind::Integer { min: 1, max: 100 },
                default: Some(json!(10)),
            }],
        }])
    }
    fn prepare_feature(
        &self,
        id: &str,
        arguments: &serde_json::Value,
    ) -> Result<adapter_sdk::ExecutionRequest, Error> {
        let feature = self
            .features()?
            .into_iter()
            .find(|f| f.id == id)
            .ok_or_else(|| Error::Invalid(format!("unknown feature: {id}")))?;
        let arguments = feature.resolve(arguments)?;
        let times = arguments["times"]
            .as_u64()
            .ok_or_else(|| Error::Invalid("missing times".into()))? as u32;
        Ok(adapter_sdk::ExecutionRequest {
            actions: vec![adapter_sdk::Action::Repeat(adapter_sdk::RepeatRequest {
                call: ToolCall {
                    name: "perform_trial".into(),
                    arguments: json!({}),
                },
                times,
            })],
        })
    }
    fn observe(&self) -> Result<AppState, Error> {
        let (completed, successful, ready, needs_reset) = self.runtime.trial_snapshot();
        let mut state = AppState {
            scene: "demo program".into(),
            facts: json!({"mode": self.runtime.mode(),"trial":{"completed":completed,"successful":successful,"ready":ready,"needs_reset":needs_reset}, "training": {
                "status": self.runtime.training_status(),
                "available": self.runtime.available(),
                "unavailable_reason": if self.runtime.available() { None } else { Some("Training service is permanently unavailable; no available tool can restore it.") }
            }}),
        };
        if let Some(exploration) = self.runtime.exploration_snapshot() {
            state.facts["exploration"] = json!(exploration);
        }
        Ok(state)
    }
    fn decision_context(&self, context: &SkillContext<'_>) -> Result<DecisionContext, Error> {
        let snapshot: Snapshot = serde_json::from_value(context.state.facts.clone())
            .map_err(|e| Error::Invalid(format!("invalid demo state in skill context: {e}")))?;
        let mut skills = vec![
            Skill {
                name: "set_mode".into(),
                repeatable: false,
                loopable: false,
                tags: vec!["session".into(), "write".into()],
                availability: Availability::Available,
                description: "Set the program mode to idle or training.".into(),
                parameters: json!({"type":"object","properties":{"mode":{"type":"string","enum":["idle","training"]}},"required":["mode"],"additionalProperties":false}),
                calls: vec![
                    ToolCall {
                        name: "set_mode".into(),
                        arguments: json!({"mode":"idle"}),
                    },
                    ToolCall {
                        name: "set_mode".into(),
                        arguments: json!({"mode":"training"}),
                    },
                ],
            },
            Skill {
                name: "get_state".into(),
                repeatable: false,
                loopable: false,
                tags: vec!["observation".into(), "read".into()],
                availability: Availability::Available,
                description: "Refresh the current program state.".into(),
                parameters: json!({"type":"object","properties":{},"additionalProperties":false}),
                calls: vec![ToolCall {
                    name: "get_state".into(),
                    arguments: json!({}),
                }],
            },
            empty_skill(
                "start_training",
                "Start one training run. Requires mode=training, training.available=true and status=not_started. A transient busy error can be retried. Starting is not completion: inspect training.status until completed. Only one run is supported per program session.",
                &["training", "write"],
            ),
            empty_skill(
                "get_training_status",
                "Read the training status without modifying it. A running training finishes after 500 ms of real elapsed time. After starting, use this tool to check completion. Repeatedly starting does not advance training.",
                &["training", "observation", "read"],
            ),
        ];
        let mut trial = empty_skill(
            "perform_trial",
            "Complete one trial action. A normal miss still counts as one completed action. No preparation is required. An interrupted device needs reset_trial before further actions; reset preserves completed actions.",
            &["trial", "repeat"],
        );
        trial.repeatable = true;
        skills.push(trial);
        skills.push(empty_skill("reset_trial","Reset an interrupted trial device. Preserves all completed actions; resume the pending batch afterward.",&["trial","recovery"]));
        if let Some(exploration) = snapshot.exploration {
            let mut explore = empty_skill(
                "explore_step",
                "Discover and collect locally. Returns Continue after each step, Completed only when the adapter verifies exploration is finished. Item count is unknown to the caller. Use UntilDone; recover interruptions with reset_exploration then Resume.",
                &["exploration", "collection"],
            );
            explore.loopable = true;
            if exploration.needs_reset {
                explore.availability = Availability::Unavailable {
                    reason: "先调用 reset_exploration，然后恢复探索。".into(),
                };
            }
            skills.push(explore);
            let mut reset = empty_skill(
                "reset_exploration",
                "Restore exploration without discarding collected items; resume the interrupted loop afterward.",
                &["exploration", "recovery"],
            );
            if !exploration.needs_reset {
                reset.availability = Availability::Unavailable {
                    reason: "探索设备不需要恢复。".into(),
                };
            }
            skills.push(reset);
        }
        for skill in &mut skills {
            let reason = match skill.name.as_str() {
                "perform_trial" if !snapshot.trial.ready && snapshot.trial.needs_reset => {
                    Some("需要先调用 reset_trial 复位，再恢复未完成批次。")
                }
                "perform_trial" if !snapshot.trial.ready => Some("试验装置永久不可用。"),
                "reset_trial" if !snapshot.trial.needs_reset => Some("试验装置不需要复位。"),
                "set_mode" if snapshot.training.status == TrainingStatus::Running => {
                    Some("训练进行中，完成后才能切换模式。")
                }
                "start_training" if !snapshot.training.available => {
                    Some("训练服务永久不可用，当前工具无法恢复。")
                }
                "start_training" if snapshot.training.status != TrainingStatus::NotStarted => {
                    Some("本次训练已启动或完成；请查询 get_training_status，不能重复启动。")
                }
                "start_training" if snapshot.mode != Mode::Training => {
                    Some("需要先使用 set_mode 切换到 training 模式。")
                }
                _ => None,
            };
            if let Some(reason) = reason {
                skill.availability = Availability::Unavailable {
                    reason: reason.into(),
                };
            }
        }
        Ok(DecisionContext {
            skills,
            guidance: guidance(&snapshot, context),
        })
    }
    async fn execute(
        &mut self,
        call: &ToolCall,
        control: &ExecutionControl,
    ) -> Result<ActionReport, Error> {
        control.check()?;
        let message = match call.name.as_str() {
            "explore_step" => {
                no_args(call)?;
                let done = match self.runtime.explore_step() {
                    Ok(done) => done,
                    Err(RuntimeError::ExplorationNeedsReset) => {
                        return Ok(ActionReport {
                            outcome: ActionOutcome::Interrupted,
                            message:
                                "探索设备中断，调用 reset_exploration 后恢复，已收集物品保留。"
                                    .into(),
                        });
                    }
                    Err(error) => return Err(runtime_error(error)),
                };
                return Ok(ActionReport {
                    outcome: if done {
                        ActionOutcome::Completed { successful: false }
                    } else {
                        ActionOutcome::Continue { successful: true }
                    },
                    message: if done {
                        "已确认探索完成。"
                    } else {
                        "已收集一个物品，继续探索。"
                    }
                    .into(),
                });
            }
            "reset_exploration" => {
                no_args(call)?;
                self.runtime.reset_exploration().map_err(runtime_error)?;
                Ok("探索设备已恢复，保留已收集物品。".into())
            }
            "get_state" => {
                no_args(call)?;
                Ok("observation requested".into())
            }
            "set_mode" => {
                let args: SetMode = serde_json::from_value(call.arguments.clone())
                    .map_err(|e| Error::Invalid(e.to_string()))?;
                self.runtime.set_mode(args.mode).map_err(runtime_error)?;
                Ok("mode updated".into())
            }
            "perform_trial" => {
                no_args(call)?;
                let successful = trial_flow::execute(&mut self.runtime, control).await?;
                return Ok(ActionReport::completed(
                    if successful {
                        "试验动作完成，获得结果。"
                    } else {
                        "试验动作完成，本次未获得结果。"
                    }
                    .into(),
                    successful,
                ));
            }
            "reset_trial" => {
                no_args(call)?;
                self.runtime.reset_trial();
                Ok("试验装置已复位，保留原有进度。".into())
            }
            "start_training" => {
                no_args(call)?;
                self.runtime.start_training().map_err(runtime_error)?;
                Ok("训练已启动，尚未确认完成；请查询训练状态。".into())
            }
            "get_training_status" => {
                no_args(call)?;
                Ok(json!({"training_status": self.runtime.training_status()}).to_string())
            }
            _ => Err(Error::Invalid("unknown skill".into())),
        }?;
        Ok(ActionReport::completed(message, true))
    }
}
