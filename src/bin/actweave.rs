use actweave::{
    adapter::{self, Scenario},
    core::{Action, Agent, Availability, Continuation, Decision, Event, RunOptions, Status, Task},
    jev::{DEFAULT_ENDPOINT, DEFAULT_MODEL, JevAgent},
    manual::ManualAgent,
    skills::SkillMode,
    task_runtime::TaskRuntime,
};
use clap::{Parser, ValueEnum};
use std::{fs::OpenOptions, io::Write, path::PathBuf, process::ExitCode};

struct DebugWriter(std::fs::File);
impl Write for DebugWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.write_all(bytes)?;
        std::io::stderr().write_all(bytes)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()?;
        std::io::stderr().flush()
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum LogLevel {
    Info,
    Debug,
}

#[derive(Clone, Copy, ValueEnum)]
enum DemoScenario {
    Normal,
    TransientFailure,
    Blocked,
    BatchInterrupted,
    BatchBlocked,
    Exploration,
    ExplorationInterrupted,
}
impl From<DemoScenario> for Scenario {
    fn from(value: DemoScenario) -> Self {
        match value {
            DemoScenario::Normal => Self::Normal,
            DemoScenario::TransientFailure => Self::TransientFailure,
            DemoScenario::Blocked => Self::Blocked,
            DemoScenario::BatchInterrupted => Self::BatchInterrupted,
            DemoScenario::BatchBlocked => Self::BatchBlocked,
            DemoScenario::Exploration => Self::Exploration,
            DemoScenario::ExplorationInterrupted => Self::ExplorationInterrupted,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum AgentKind {
    Jev,
    Manual,
}

#[derive(Clone, Copy, ValueEnum)]
enum Injection {
    All,
    OnDemand,
}

#[derive(Parser)]
#[command(about = "可切换决策器的语义工具调用 MVP")]
struct Args {
    /// 自然语言任务，例如：切换到训练模式
    task: String,
    /// 宿主选择适配器；只加载此实例提供的状态和 Skills
    #[arg(long, default_value = "demo")]
    adapter: String,
    /// all 直接注入；on-demand 先展示目录，由决策器按名称或标签加载
    #[arg(long, value_enum, default_value = "all")]
    skill_mode: Injection,
    /// 选择决策器：JEV 模型或手动 JSON 输入（无需密钥）
    #[arg(long, value_enum, default_value = "jev")]
    agent: AgentKind,
    /// Demo 环境：训练、固定次数试验、未知数量探索及其异常场景
    #[arg(long, value_enum, default_value = "normal")]
    scenario: DemoScenario,
    /// JEV 端点；未指定时读取 JEV_ENDPOINT，再使用默认端点
    #[arg(long)]
    endpoint: Option<String>,
    /// JEV 模型；未指定时读取 JEV_MODEL，再使用 jev-latest
    #[arg(long)]
    model: Option<String>,
    #[arg(long, default_value_t = 12)]
    max_decisions: usize,
    /// 同一调用失败或相同计划被拒绝的停止阈值
    #[arg(long, default_value_t=3, value_parser=clap::value_parser!(u32).range(1..))]
    max_repeated_failures: u32,
    /// 单次决策最多下达的有序动作数（1..=16）
    #[arg(long, default_value_t=4, value_parser=clap::value_parser!(u32).range(1..=16))]
    max_actions: u32,
    /// 单个批次最多完成的动作次数（1..=100）
    #[arg(long, default_value_t=100, value_parser=clap::value_parser!(u32).range(1..=100))]
    max_repeat: u32,
    #[arg(long, default_value_t=200, value_parser=clap::value_parser!(u32).range(1..))]
    max_batch_attempts: u32,
    /// 批次总时限，包含异常后的重新决策和恢复
    #[arg(long, default_value_t=120, value_parser=clap::value_parser!(u64).range(1..))]
    batch_timeout_seconds: u64,
    /// INFO 展示执行进度；DEBUG 同时输出完整决策诊断
    #[arg(long, value_enum, default_value = "info")]
    log_level: LogLevel,
    /// 将完整决策 DEBUG 日志追加到 JSONL 文件，不影响终端 INFO 输出
    #[arg(long)]
    log_file: Option<PathBuf>,
    /// 显式开启任务文件日志；每个任务创建独立目录，DEBUG 另存模型诊断
    #[arg(long)]
    log_dir: Option<PathBuf>,
}
#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    match execute(args).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("[ERROR] 任务执行中断：{e}");
            ExitCode::from(2)
        }
    }
}
async fn execute(args: Args) -> Result<bool, Box<dyn std::error::Error>> {
    let task = Task::new(&args.task)?;
    let mut registry = adapter_sdk::Registry::default();
    adapter::register(&mut registry, args.scenario.into())?;
    let mut environment = registry.create(&args.adapter, adapter_sdk::Host::default())?;
    if matches!(args.agent, AgentKind::Manual) && (args.endpoint.is_some() || args.model.is_some())
    {
        return Err("manual 决策器不使用 --endpoint 或 --model".into());
    }
    let runtime = TaskRuntime::new(
        task,
        RunOptions {
            max_decisions: args.max_decisions,
            max_repeated_failures: args.max_repeated_failures,
            max_actions: args.max_actions as usize,
            max_repeat: args.max_repeat,
            max_batch_attempts: args.max_batch_attempts,
            batch_timeout: std::time::Duration::from_secs(args.batch_timeout_seconds),
            skill_mode: match args.skill_mode {
                Injection::All => SkillMode::All,
                Injection::OnDemand => SkillMode::OnDemand,
            },
            ..RunOptions::default()
        },
    );
    let task_id = runtime.handle().snapshot().task_id;
    let task_dir = args.log_dir.as_ref().map(|dir| dir.join(&task_id));
    let runtime = if let Some(dir) = &task_dir {
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::create_dir(dir)?;
        let runtime = runtime.with_log_writer(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(dir.join("events.jsonl"))?,
        );
        println!("[INFO] 任务 ID：{task_id}；日志目录：{}", dir.display());
        runtime
    } else {
        runtime
    };
    let writer: Box<dyn Write + Send> = match &args.log_file {
        Some(path) => Box::new(OpenOptions::new().create(true).append(true).open(path)?),
        None if matches!(args.log_level, LogLevel::Debug) => match &task_dir {
            Some(dir) => Box::new(DebugWriter(
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(dir.join("decisions.jsonl"))?,
            )),
            None => Box::new(std::io::stderr()),
        },
        None => Box::new(std::io::sink()),
    };
    match args.agent {
        AgentKind::Jev => {
            let key = std::env::var("JEVKEY").map_err(|_| "请设置环境变量 JEVKEY")?;
            let endpoint = args
                .endpoint
                .clone()
                .or_else(|| std::env::var("JEV_ENDPOINT").ok())
                .unwrap_or_else(|| DEFAULT_ENDPOINT.into());
            let model = args
                .model
                .clone()
                .or_else(|| std::env::var("JEV_MODEL").ok())
                .unwrap_or_else(|| DEFAULT_MODEL.into());
            let mut agent = JevAgent::new(key, endpoint, model)?.with_log_writer(writer);
            execute_with(runtime, &args, &mut agent, "JEV", &mut environment).await
        }
        AgentKind::Manual => {
            let mut agent =
                ManualAgent::new(std::io::BufReader::new(std::io::stdin()), std::io::stderr())
                    .with_log_writer(writer);
            execute_with(runtime, &args, &mut agent, "Manual", &mut environment).await
        }
    }
}

// The presentation and execution path is shared by every provider.
async fn execute_with(
    runtime: TaskRuntime,
    args: &Args,
    agent: &mut impl Agent,
    agent_name: &str,
    environment: &mut adapter_sdk::RegisteredAdapter,
) -> Result<bool, Box<dyn std::error::Error>> {
    println!("[INFO] 决策器：{agent_name}");
    println!("[INFO] 适配器：{}", args.adapter);
    println!("[INFO] 开始任务：{}", runtime.handle().snapshot().goal);
    println!("[INFO] 最多执行 {} 轮决策", args.max_decisions);
    let handle = runtime.handle();
    let summary_handle = handle.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            handle.cancel();
            eprintln!("[WARN] 已请求取消，将在安全操作边界停止");
        }
    });
    let mut step = 0;
    let outcome = runtime
        .run(agent, environment, |event| match event {
            Event::Observed(_) => {
                println!(
                    "[INFO] 已{}当前状态",
                    if step == 0 { "读取" } else { "更新" }
                );
            }
            Event::SkillsResolved {
                step,
                skills,
                guidance,
            } => {
                if !guidance.is_empty() {
                    println!("[INFO] 本轮指导：{guidance}");
                }
                let available: Vec<_> = skills
                    .iter()
                    .filter(|skill| skill.availability.is_available())
                    .map(|skill| skill.name.as_str())
                    .collect();
                println!(
                    "[INFO] 第 {step} 步：可用 Skills：{}",
                    if available.is_empty() {
                        "无".into()
                    } else {
                        available.join(", ")
                    }
                );
                for skill in &skills {
                    if let Availability::Unavailable { reason } = &skill.availability {
                        println!("[INFO] Skill `{}` 暂不可用：{reason}", skill.name);
                    }
                }
            }
            Event::SkillsInjected {
                names,
                directory_count,
            } => {
                println!(
                    "[INFO] 本轮已加载 Skills：{}；轻量目录 {} 项",
                    if names.is_empty() {
                        "无".into()
                    } else {
                        names.join(", ")
                    },
                    directory_count
                );
                println!("[INFO] 第 {} 步：正在等待决策器选择…", step + 1);
            }
            Event::SkillsLoaded(result) => {
                if result.success {
                    println!("[INFO] 已更新加载范围：{}", result.selected.join(", "));
                } else {
                    println!("[WARN] 加载失败：{}", result.message);
                }
            }
            Event::Decided(decision) => {
                step += 1;
                if let Decision::Execute { then, .. } | Decision::ReplaceRemaining { then, .. } =
                    &decision
                {
                    println!(
                        "[INFO] 动作完成后：{}",
                        match then {
                            Continuation::Decide => "继续决策",
                            Continuation::Finish => "结束任务",
                        }
                    );
                }
                match decision {
                    Decision::Execute { actions, .. } => {
                        println!("[INFO] 第 {step} 步：下达 {} 个有序动作", actions.len());
                        for action in actions {
                            if let Action::UntilDone(call) = &action {
                                println!(
                                    "[INFO] 循环 `{}`：由 Adapter 确认完成，正常步骤无需模型决策",
                                    call.name
                                );
                            }
                            if let Action::Repeat(request) = action {
                                println!(
                                    "[INFO] 批次 `{}`：目标 {} 次动作",
                                    request.call.name, request.times
                                );
                            }
                        }
                    }
                    Decision::ReplaceRemaining { actions, .. } => {
                        println!(
                            "[INFO] 第 {step} 步：替换剩余计划，共 {} 个动作，保留已完成进度",
                            actions.len()
                        );
                    }
                    Decision::Resume => {
                        println!("[INFO] 第 {step} 步：恢复原执行，继续剩余动作")
                    }
                    Decision::LoadSkills(request) => println!(
                        "[INFO] 第 {step} 步：请求加载 Skills，名称：{:?}，tags：{:?}",
                        request.names, request.tags
                    ),
                    Decision::Completed(_) => {
                        println!("[INFO] 第 {step} 步：决策器判断任务已完成")
                    }
                    Decision::Failed(_) => {
                        println!("[WARN] 第 {step} 步：决策器判断当前能力与状态无法完成任务")
                    }
                }
            }
            Event::BatchProgress(progress) => {
                let completion = match &progress.request {
                    actweave::core::BatchRequest::Repeat(request) => {
                        format!("已完成 {}/{} 次", progress.completed, request.times)
                    }
                    actweave::core::BatchRequest::UntilDone(_) => format!(
                        "已执行 {} 步，{}",
                        progress.completed,
                        if progress.finished {
                            "Adapter 已确认完成"
                        } else {
                            "等待 Adapter 确认完成"
                        }
                    ),
                };
                println!(
                    "[INFO] 批次 `{}`：{}，达成效果 {} 次，尝试 {} 次，状态 {:?}",
                    progress.request.call().name,
                    completion,
                    progress.successful,
                    progress.attempts,
                    progress.status
                );
                if progress.status == actweave::core::BatchStatus::Interrupted {
                    println!(
                        "[WARN] 批次中断：{}；{}，交回决策器处理",
                        progress.message,
                        progress
                            .remaining
                            .map_or_else(|| "剩余工作量未知".into(), |n| format!("剩余 {n} 次"))
                    );
                }
            }
            Event::ActionStarted { index, total, call } => println!(
                "[INFO] 动作 {}/{}：选择 Skill `{}`，参数：{}；开始调用",
                index + 1,
                total,
                call.name,
                call.arguments
            ),
            Event::ExecutionProgress(progress) => {
                if progress.status == actweave::core::ExecutionStatus::Interrupted {
                    println!(
                        "[WARN] 执行中断：已完成 {}/{} 个动作；{}",
                        progress.completed,
                        progress.actions.len(),
                        progress.message
                    );
                }
            }
            Event::ExecutionFailed(failure) => {
                let stage = match failure.stage {
                    actweave::core::FailureStage::Observation => "观察",
                    actweave::core::FailureStage::Context => "上下文生成",
                    actweave::core::FailureStage::Availability => "前置条件检查",
                    actweave::core::FailureStage::Execution => "动作执行",
                    actweave::core::FailureStage::Validation => "计划校验",
                };
                println!("[WARN] {stage}失败：{}", failure.message);
            }
            Event::ExecutionRejected(reason) => println!("[WARN] 拒绝执行：{reason}"),
            Event::Executed(result) => {
                if result.success {
                    println!(
                        "[INFO] 第 {step} 步：Skill `{}` 调用成功：{}",
                        result.call.name, result.message
                    );
                } else {
                    println!(
                        "[WARN] 第 {step} 步：Skill `{}` 调用失败：{}",
                        result.call.name, result.message
                    );
                    if step < args.max_decisions {
                        println!("[INFO] 已记录失败原因，供后续决策使用");
                    }
                }
            }
        })
        .await;
    signal_task.abort();
    if let Ok(outcome) = &outcome {
        match outcome.status {
            Status::Cancelled => println!("[WARN] 用户取消任务，已完成动作不会回滚"),
            Status::Completed => println!("[INFO] 任务完成，共 {} 步决策", outcome.decisions),
            Status::Failed if outcome.reason == "decision limit reached" => {
                println!("[WARN] 任务未完成：已达到 {} 步决策上限", outcome.decisions)
            }
            Status::Failed => println!(
                "[WARN] 任务未完成，共 {} 步决策：{}",
                outcome.decisions, outcome.reason
            ),
        }
    }
    let snapshot = summary_handle.snapshot();
    if let Some(summary) = snapshot.summary {
        let status = match snapshot.status {
            actweave::task_runtime::TaskStatus::Completed => "已完成",
            actweave::task_runtime::TaskStatus::Cancelled => "已取消",
            _ => "失败",
        };
        println!("[INFO] 任务结束：{status}");
        println!(
            "[INFO] 大模型请求：{} 次（成功 {}，失败 {}）",
            summary.model_requests.total,
            summary.model_requests.succeeded,
            summary.model_requests.failed
        );
        println!(
            "[INFO] 动作调用：{} 次（正常 {}，中断或报错 {}）",
            summary.actions.total, summary.actions.succeeded, summary.actions.failed
        );
        println!("[INFO] 执行中断：{} 次", summary.interruptions);
        println!(
            "[INFO] 总耗时：{:.3} 秒",
            summary.elapsed_ms as f64 / 1000.0
        );
    }
    Ok(outcome?.status == Status::Completed)
}
