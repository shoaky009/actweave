//! Actions describe intent. Platform backends own input injection and held-input cleanup.
use crate::{
    Control, Error,
    recognition::{Frame, RecognitionResult},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, future::Future, pin::Pin};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

/// Client-area pixels, or the center of a detection from this node's recognition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Target {
    Point { point: Point },
    Match { index: usize },
}
impl Target {
    pub fn resolve(&self, result: &RecognitionResult) -> Result<Point, Error> {
        match self {
            Self::Point { point } => Ok(*point),
            Self::Match { index } => {
                let rect = result
                    .matches
                    .get(*index)
                    .and_then(|m| m.bounds)
                    .ok_or_else(|| {
                        Error::Invalid(format!("missing match bounds at index {index}"))
                    })?;
                Ok(Point {
                    x: rect.x + (rect.width / 2) as i32,
                    y: rect.y + (rect.height / 2) as i32,
                })
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Click {
        target: Target,
    },
    LongPress {
        target: Target,
        duration_ms: u64,
    },
    Swipe {
        from: Target,
        to: Target,
        duration_ms: u64,
    },
    KeyDown {
        key: String,
    },
    KeyUp {
        key: String,
    },
    Wait {
        duration_ms: u64,
    },
    Custom {
        name: String,
        parameters: Value,
    },
}

/// Resolved input commands; no Core or flow knowledge is required by the platform.
#[derive(Debug, Clone)]
pub enum Input {
    Click(Point),
    LongPress {
        point: Point,
        duration_ms: u64,
    },
    Swipe {
        from: Point,
        to: Point,
        duration_ms: u64,
    },
    KeyDown(String),
    KeyUp(String),
}

/// Methods must yield rather than block the executor. Dropping an operation must
/// stop further effects. `release_all` must release inputs owned by this session,
/// including partially completed gestures, and must be safe to repeat.
pub trait Backend {
    fn capture(&mut self, control: &Control) -> impl Future<Output = Result<Frame, Error>>;
    fn input(
        &mut self,
        input: &Input,
        control: &Control,
    ) -> impl Future<Output = Result<(), Error>>;
    fn release_all(&mut self) -> impl Future<Output = Result<(), Error>>;
}

pub type ActionFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, Error>> + 'a>>;
pub trait CustomAction<B> {
    fn execute<'a>(
        &'a mut self,
        backend: &'a mut B,
        parameters: &'a Value,
        recognition: &'a RecognitionResult,
        control: &'a Control,
    ) -> ActionFuture<'a>;
}

pub struct Actions<B> {
    custom: BTreeMap<String, Box<dyn CustomAction<B>>>,
}
impl<B> Default for Actions<B> {
    fn default() -> Self {
        Self {
            custom: BTreeMap::new(),
        }
    }
}
impl<B: Backend> Actions<B> {
    pub fn register(
        &mut self,
        name: String,
        handler: impl CustomAction<B> + 'static,
    ) -> Result<(), Error> {
        if name.trim().is_empty() || self.custom.contains_key(&name) {
            return Err(Error::Invalid(format!("duplicate or empty action: {name}")));
        }
        self.custom.insert(name, Box::new(handler));
        Ok(())
    }
    pub fn supports(&self, action: &Action) -> bool {
        match action {
            Action::Custom { name, .. } => self.custom.contains_key(name),
            _ => true,
        }
    }
    pub async fn execute(
        &mut self,
        action: &Action,
        backend: &mut B,
        recognition: &RecognitionResult,
        control: &Control,
    ) -> Result<Value, Error> {
        control.check()?;
        let input = match action {
            Action::Click { target } => Input::Click(target.resolve(recognition)?),
            Action::LongPress {
                target,
                duration_ms,
            } => Input::LongPress {
                point: target.resolve(recognition)?,
                duration_ms: *duration_ms,
            },
            Action::Swipe {
                from,
                to,
                duration_ms,
            } => Input::Swipe {
                from: from.resolve(recognition)?,
                to: to.resolve(recognition)?,
                duration_ms: *duration_ms,
            },
            Action::KeyDown { key } => Input::KeyDown(key.clone()),
            Action::KeyUp { key } => Input::KeyUp(key.clone()),
            Action::Wait { duration_ms } => {
                tokio::time::sleep(std::time::Duration::from_millis(*duration_ms)).await;
                return Ok(Value::Null);
            }
            Action::Custom { name, parameters } => {
                let handler = self
                    .custom
                    .get_mut(name)
                    .ok_or_else(|| Error::Unsupported(name.clone()))?;
                return handler
                    .execute(backend, parameters, recognition, control)
                    .await;
            }
        };
        backend.input(&input, control).await?;
        Ok(Value::Null)
    }
}
