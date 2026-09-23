//! Terminal presentation for the existing task and adapter APIs.
use actweave::{
    adapter::{self, Scenario},
    core::{Event as TaskEvent, RunOptions, Status, Task},
    jev::{DEFAULT_ENDPOINT, DEFAULT_MODEL, JevAgent},
    task_runtime::{TaskHandle, TaskRuntime},
};
use adapter_sdk::{Adapter, Feature, ParameterKind, RegisteredAdapter};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};
use std::{collections::VecDeque, io, time::Duration};
use tokio::sync::mpsc;

type UiResult<T> = Result<T, Box<dyn std::error::Error>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Adapters,
    Tasks,
    Configure,
    Running,
}

struct Input {
    name: String,
    value: String,
    kind: ParameterKind,
}
impl Input {
    fn cycle(&mut self, forward: bool) {
        match &self.kind {
            ParameterKind::Boolean => {
                self.value = if self.value == "true" {
                    "false"
                } else {
                    "true"
                }
                .into()
            }
            ParameterKind::Enum { choices } if !choices.is_empty() => {
                let current = choices
                    .iter()
                    .position(|c| c.value == self.value)
                    .unwrap_or(0);
                let index = if forward {
                    (current + 1) % choices.len()
                } else {
                    (current + choices.len() - 1) % choices.len()
                };
                self.value = choices[index].value.clone();
            }
            _ => {}
        }
    }
}

enum Message {
    Event(TaskEvent),
    Finished(Box<Result<actweave::core::Outcome, String>>),
}

struct App {
    registry: adapter_sdk::Registry,
    adapters: Vec<String>,
    selected_adapter: usize,
    adapter_name: String,
    instance: Option<RegisteredAdapter>,
    features: Vec<Feature>,
    selected_task: usize,
    feature_index: Option<usize>,
    inputs: Vec<Input>,
    focused: usize,
    screen: Screen,
    state: Option<adapter_sdk::AppState>,
    handle: Option<TaskHandle>,
    messages: mpsc::UnboundedReceiver<Message>,
    sender: mpsc::UnboundedSender<Message>,
    logs: VecDeque<String>,
    log_scroll: usize,
    done: bool,
    notice: String,
}

impl App {
    fn new() -> UiResult<Self> {
        let mut registry = adapter_sdk::Registry::default();
        adapter::register(&mut registry, Scenario::Normal)?;
        let adapters = registry.names().map(str::to_owned).collect();
        let (sender, messages) = mpsc::unbounded_channel();
        Ok(Self {
            registry,
            adapters,
            selected_adapter: 0,
            adapter_name: String::new(),
            instance: None,
            features: Vec::new(),
            selected_task: 0,
            feature_index: None,
            inputs: Vec::new(),
            focused: 0,
            screen: Screen::Adapters,
            state: None,
            handle: None,
            messages,
            sender,
            logs: VecDeque::new(),
            log_scroll: 0,
            done: false,
            notice: String::new(),
        })
    }

    fn log(&mut self, line: impl Into<String>) {
        if self.logs.len() >= 400 {
            self.logs.pop_front();
        }
        self.logs.push_back(line.into());
    }

    fn choose_adapter(&mut self) -> UiResult<()> {
        let name = self
            .adapters
            .get(self.selected_adapter)
            .ok_or("没有可用适配器")?
            .clone();
        let instance = self.registry.create(&name, adapter_sdk::Host::default())?;
        let features = instance.features()?;
        for feature in &features {
            feature.validate()?;
        }
        self.adapter_name = name;
        self.instance = Some(instance);
        self.features = features;
        self.selected_task = 0;
        self.screen = Screen::Tasks;
        self.notice.clear();
        Ok(())
    }

    fn choose_task(&mut self) {
        self.feature_index = self.selected_task.checked_sub(1);
        self.inputs.clear();
        if let Some(index) = self.feature_index {
            if let Some(feature) = self.features.get(index) {
                for p in &feature.parameters {
                    self.inputs.push(Input {
                        name: p.name.clone(),
                        value: p
                            .default
                            .as_ref()
                            .map(|v| match v {
                                serde_json::Value::String(s) => s.clone(),
                                _ => v.to_string(),
                            })
                            .unwrap_or_default(),
                        kind: p.kind.clone(),
                    });
                }
            }
        } else {
            self.inputs.push(Input {
                name: "任务目标".into(),
                value: String::new(),
                kind: ParameterKind::String,
            });
        }
        self.focused = 0;
        self.notice.clear();
        self.screen = Screen::Configure;
    }

    fn start(&mut self) -> UiResult<()> {
        let instance = self.instance.as_ref().ok_or("适配器实例不可用")?;
        let (goal, request) = if let Some(index) = self.feature_index {
            let feature = self.features.get(index).ok_or("未知功能")?;
            let pairs: Vec<_> = feature
                .parameters
                .iter()
                .zip(&self.inputs)
                .map(|(p, input)| format!("{}={}", p.id, input.value))
                .collect();
            let arguments = feature.parse_arguments(&pairs)?;
            let request = instance.prepare_feature(&feature.id, &arguments)?;
            (feature.name.clone(), Some(request))
        } else {
            let goal = self.inputs.first().map(|i| i.value.trim()).unwrap_or("");
            if goal.is_empty() {
                return Err("请输入任务目标".into());
            }
            if std::env::var("JEVKEY").is_err() {
                return Err("请先设置 JEVKEY".into());
            }
            (goal.to_owned(), None)
        };
        let runtime = TaskRuntime::new(Task::new(goal)?, RunOptions::default());
        let instance = self.instance.take().ok_or("适配器实例不可用")?;
        self.handle = Some(runtime.handle());
        self.logs.clear();
        self.log_scroll = 0;
        self.log(format!("适配器：{}", self.adapter_name));
        self.state = instance.observe().ok();
        let sender = self.sender.clone();
        self.done = false;
        self.notice.clear();
        self.screen = Screen::Running;
        tokio::task::spawn_local(async move {
            let mut instance = instance;
            let result = match request {
                Some(request) => {
                    runtime
                        .run_request(request, &mut instance, |event| {
                            let _ = sender.send(Message::Event(event));
                        })
                        .await
                }
                None => {
                    let key = match std::env::var("JEVKEY") {
                        Ok(key) => key,
                        Err(e) => {
                            let _ = sender.send(Message::Finished(Box::new(Err(e.to_string()))));
                            return;
                        }
                    };
                    let endpoint =
                        std::env::var("JEV_ENDPOINT").unwrap_or_else(|_| DEFAULT_ENDPOINT.into());
                    let model = std::env::var("JEV_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
                    match JevAgent::new(key, endpoint, model) {
                        Ok(mut agent) => {
                            runtime
                                .run(&mut agent, &mut instance, |event| {
                                    let _ = sender.send(Message::Event(event));
                                })
                                .await
                        }
                        Err(error) => {
                            let _ =
                                sender.send(Message::Finished(Box::new(Err(error.to_string()))));
                            return;
                        }
                    }
                }
            };
            let _ = sender.send(Message::Finished(Box::new(
                result.map_err(|e| e.to_string()),
            )));
        });
        Ok(())
    }

    fn message(&mut self, message: Message) {
        match message {
            Message::Event(event) => match event {
                TaskEvent::Observed(state) => {
                    self.log(format!("已观察：{}", state.scene));
                    self.state = Some(state);
                }
                TaskEvent::SkillsResolved { skills, .. } => self.log(format!(
                    "可用 Skills：{}",
                    skills
                        .iter()
                        .filter(|s| s.availability.is_available())
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                TaskEvent::ActionStarted { index, total, call } => self.log(format!(
                    "动作 {}/{}：{} {}",
                    index + 1,
                    total,
                    call.name,
                    call.arguments
                )),
                TaskEvent::Executed(result) => self.log(format!(
                    "{}：{}",
                    if result.success { "完成" } else { "失败" },
                    result.message
                )),
                TaskEvent::BatchProgress(progress) => self.log(format!(
                    "批次：完成 {}，尝试 {}，状态 {:?}",
                    progress.completed, progress.attempts, progress.status
                )),
                TaskEvent::ExecutionFailed(failure) => {
                    self.log(format!("执行失败：{}", failure.message))
                }
                TaskEvent::ExecutionRejected(reason) => self.log(format!("拒绝执行：{reason}")),
                TaskEvent::Decided(decision) => self.log(format!(
                    "决策：{}",
                    match decision {
                        actweave::core::Decision::Execute { .. } => "执行",
                        actweave::core::Decision::ReplaceRemaining { .. } => "调整计划",
                        actweave::core::Decision::LoadSkills(_) => "加载 Skill",
                        actweave::core::Decision::Resume => "继续执行",
                        actweave::core::Decision::Completed(_) => "完成",
                        actweave::core::Decision::Failed(_) => "失败",
                    }
                )),
                _ => {}
            },
            Message::Finished(result) => {
                self.done = true;
                match *result {
                    Ok(outcome) => {
                        self.state = Some(outcome.state);
                        self.log(format!(
                            "任务{}：{}",
                            match outcome.status {
                                Status::Completed => "完成",
                                Status::Cancelled => "取消",
                                Status::Failed => "失败",
                            },
                            outcome.reason
                        ));
                        self.log(format!(
                            "模型请求 {} 次，动作调用 {} 次，耗时 {:.1} 秒",
                            outcome.summary.model_requests.total,
                            outcome.summary.actions.total,
                            outcome.summary.elapsed_ms as f64 / 1000.0
                        ));
                    }
                    Err(error) => self.log(format!("任务失败：{error}")),
                }
            }
        }
    }

    fn key(&mut self, key: KeyCode) -> UiResult<bool> {
        match self.screen {
            Screen::Adapters => match key {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(false),
                KeyCode::Up => self.selected_adapter = self.selected_adapter.saturating_sub(1),
                KeyCode::Down => {
                    self.selected_adapter =
                        (self.selected_adapter + 1).min(self.adapters.len().saturating_sub(1))
                }
                KeyCode::Enter => {
                    if let Err(error) = self.choose_adapter() {
                        self.notice = error.to_string();
                    }
                }
                _ => {}
            },
            Screen::Tasks => match key {
                KeyCode::Esc => self.screen = Screen::Adapters,
                KeyCode::Up => self.selected_task = self.selected_task.saturating_sub(1),
                KeyCode::Down => {
                    self.selected_task = (self.selected_task + 1).min(self.features.len())
                }
                KeyCode::Enter => self.choose_task(),
                _ => {}
            },
            Screen::Configure => match key {
                KeyCode::Esc => self.screen = Screen::Tasks,
                KeyCode::Tab | KeyCode::Down => {
                    self.focused = (self.focused + 1) % (self.inputs.len() + 1)
                }
                KeyCode::BackTab | KeyCode::Up => {
                    self.focused = (self.focused + self.inputs.len()) % (self.inputs.len() + 1)
                }
                KeyCode::Enter if self.focused == self.inputs.len() => {
                    if let Err(e) = self.start() {
                        self.notice = e.to_string();
                    }
                }
                KeyCode::Enter => self.focused = (self.focused + 1) % (self.inputs.len() + 1),
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                    if self.inputs.get(self.focused).is_some_and(|i| {
                        matches!(i.kind, ParameterKind::Boolean | ParameterKind::Enum { .. })
                    }) =>
                {
                    if let Some(input) = self.inputs.get_mut(self.focused) {
                        input.cycle(!matches!(key, KeyCode::Left));
                    }
                }
                KeyCode::Backspace => {
                    if let Some(input) = self.inputs.get_mut(self.focused)
                        && !matches!(
                            input.kind,
                            ParameterKind::Boolean | ParameterKind::Enum { .. }
                        )
                    {
                        input.value.pop();
                    }
                }
                KeyCode::Char(c) => {
                    if let Some(input) = self.inputs.get_mut(self.focused)
                        && (matches!(input.kind, ParameterKind::String)
                            || matches!(input.kind, ParameterKind::Integer { .. })
                                && (c.is_ascii_digit() || c == '-'))
                    {
                        input.value.push(c);
                    }
                }
                _ => {}
            },
            Screen::Running => match key {
                KeyCode::Up => {
                    self.log_scroll = (self.log_scroll + 1).min(self.logs.len().saturating_sub(1))
                }
                KeyCode::Down => self.log_scroll = self.log_scroll.saturating_sub(1),
                KeyCode::Char('p') if !self.done => {
                    if let Some(h) = &self.handle {
                        h.pause();
                    }
                }
                KeyCode::Char('r') if !self.done => {
                    if let Some(h) = &self.handle {
                        h.resume();
                    }
                }
                KeyCode::Char('c') if !self.done => {
                    if let Some(h) = &self.handle {
                        h.cancel();
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') if self.done => return Ok(false),
                _ => {}
            },
        }
        Ok(true)
    }

    fn draw(&self, frame: &mut ratatui::Frame) {
        let area = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(2),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " ActWeave ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(match self.screen {
                    Screen::Adapters => "选择适配器",
                    Screen::Tasks => "选择任务",
                    Screen::Configure => "设置任务",
                    Screen::Running => "任务运行",
                }),
            ]))
            .block(Block::default().borders(Borders::BOTTOM)),
            chunks[0],
        );
        match self.screen {
            Screen::Adapters => {
                let items = self
                    .adapters
                    .iter()
                    .enumerate()
                    .map(|(i, name)| item(name, i == self.selected_adapter))
                    .collect::<Vec<_>>();
                frame.render_widget(
                    List::new(items).block(Block::default().title("适配器").borders(Borders::ALL)),
                    chunks[1],
                );
            }
            Screen::Tasks => {
                let mut items = vec![item("自然语言任务 · JEV", self.selected_task == 0)];
                items.extend(self.features.iter().enumerate().map(|(i, f)| {
                    item(
                        &format!("{} · {}", f.name, f.description),
                        self.selected_task == i + 1,
                    )
                }));
                frame.render_widget(
                    List::new(items).block(
                        Block::default()
                            .title(format!("{} / 任务", self.adapter_name))
                            .borders(Borders::ALL),
                    ),
                    chunks[1],
                );
            }
            Screen::Configure => {
                let rows = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(1), Constraint::Length(3)])
                    .split(chunks[1]);
                let mut lines = Vec::new();
                for (i, input) in self.inputs.iter().enumerate() {
                    let kind = match input.kind {
                        ParameterKind::String => "文本",
                        ParameterKind::Integer { .. } => "整数",
                        ParameterKind::Boolean => "布尔",
                        ParameterKind::Enum { .. } => "选项",
                    };
                    lines.push(Line::from(Span::styled(
                        format!(
                            "{} {} ({kind}): {}",
                            if i == self.focused { "›" } else { " " },
                            input.name,
                            input.value
                        ),
                        if i == self.focused {
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        },
                    )));
                }
                frame.render_widget(
                    Paragraph::new(lines)
                        .block(Block::default().title("参数").borders(Borders::ALL))
                        .wrap(Wrap { trim: false }),
                    rows[0],
                );
                frame.render_widget(
                    Paragraph::new(if self.focused == self.inputs.len() {
                        "› 开始任务"
                    } else {
                        "  开始任务"
                    })
                    .style(if self.focused == self.inputs.len() {
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    })
                    .block(Block::default().borders(Borders::ALL)),
                    rows[1],
                );
            }
            Screen::Running => {
                let columns = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
                    .split(chunks[1]);
                let status = self.handle.as_ref().map(|h| h.snapshot());
                let title = status
                    .as_ref()
                    .map(|s| format!("日志 · {} · {:?}", s.goal, s.status))
                    .unwrap_or_else(|| "日志".into());
                let height = columns[0].height.saturating_sub(2) as usize;
                let lines = self
                    .logs
                    .iter()
                    .rev()
                    .skip(self.log_scroll)
                    .take(height)
                    .rev()
                    .map(|s| Line::raw(s.as_str()))
                    .collect::<Vec<_>>();
                frame.render_widget(
                    Paragraph::new(lines)
                        .block(Block::default().title(title).borders(Borders::ALL))
                        .wrap(Wrap { trim: false }),
                    columns[0],
                );
                let state = self
                    .state
                    .as_ref()
                    .map(|s| {
                        format!(
                            "场景：{}\n\n{}",
                            s.scene,
                            serde_json::to_string_pretty(&s.facts).unwrap_or_default()
                        )
                    })
                    .unwrap_or_else(|| "等待状态…".into());
                frame.render_widget(
                    Paragraph::new(state)
                        .block(Block::default().title("State").borders(Borders::ALL))
                        .wrap(Wrap { trim: false }),
                    columns[1],
                );
            }
        }
        let help = match self.screen {
            Screen::Adapters | Screen::Tasks => "↑↓ 选择   Enter 确认   Esc 返回/退出",
            Screen::Configure => "Tab 切换   Enter 下一项/开始   Esc 返回",
            Screen::Running if self.done => "任务已结束   Esc 退出",
            Screen::Running => "P 暂停   R 继续   C 取消（在安全边界生效）",
        };
        frame.render_widget(
            Paragraph::new(if self.notice.is_empty() {
                help.to_owned()
            } else {
                format!("{help}  ·  {}", self.notice)
            })
            .style(Style::default().fg(if self.notice.is_empty() {
                Color::Gray
            } else {
                Color::Yellow
            })),
            chunks[2],
        );
    }
}

fn item(text: &str, selected: bool) -> ListItem<'static> {
    ListItem::new(format!("{} {text}", if selected { "›" } else { " " })).style(if selected {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    })
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

pub async fn run() -> UiResult<()> {
    terminal::enable_raw_mode()?;
    let _guard = TerminalGuard;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut app = App::new()?;
    let mut dirty = true;
    loop {
        while let Ok(message) = app.messages.try_recv() {
            app.message(message);
            dirty = true;
        }
        if dirty {
            terminal.draw(|frame| app.draw(frame))?;
            dirty = false;
        }
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            if !app.key(key.code)? {
                break;
            }
            dirty = true;
        }
        tokio::task::yield_now().await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn local_feature_runs_from_guided_screens_without_a_model() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut app = App::new().unwrap();
                app.key(KeyCode::Enter).unwrap();
                assert_eq!(app.screen, Screen::Tasks);
                app.key(KeyCode::Down).unwrap();
                app.key(KeyCode::Enter).unwrap();
                assert_eq!(app.screen, Screen::Configure);
                app.inputs[0].value = "2".into();
                app.start().unwrap();
                while !app.done {
                    let message = tokio::time::timeout(Duration::from_secs(3), app.messages.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    app.message(message);
                }
                let snapshot = app.handle.as_ref().unwrap().snapshot();
                assert_eq!(
                    snapshot.status,
                    actweave::task_runtime::TaskStatus::Completed
                );
                assert_eq!(snapshot.summary.unwrap().model_requests.total, 0);
                assert!(app.state.is_some());
            })
            .await;
    }

    #[test]
    fn invalid_feature_input_keeps_the_adapter_for_correction() {
        let mut app = App::new().unwrap();
        app.key(KeyCode::Enter).unwrap();
        app.key(KeyCode::Down).unwrap();
        app.key(KeyCode::Enter).unwrap();
        app.inputs[0].value = "not a number".into();
        assert!(app.start().is_err());
        assert!(app.instance.is_some());
        assert_eq!(app.screen, Screen::Configure);
    }
}
