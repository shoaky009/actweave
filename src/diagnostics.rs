//! Shared diagnostic sink for decision implementations; never receives credentials.
use crate::core::{Decision, Error};
use serde::Serialize;
use serde_json::Value;
use std::{
    io::Write,
    time::{SystemTime, UNIX_EPOCH},
};

/// Decision-facing diagnostic events. Authentication headers are never included.
#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum LogEvent<'a> {
    #[serde(rename = "model_request")]
    Request {
        body: &'a Value,
    },
    #[serde(rename = "model_response")]
    Response {
        body: &'a Value,
    },
    #[serde(rename = "model_decision")]
    Decision {
        decision: &'a Decision,
    },
    #[serde(rename = "model_error")]
    Error {
        message: String,
    },
    ManualInput {
        body: &'a Value,
    },
    ManualDecision {
        decision: &'a Decision,
    },
    ManualError {
        message: String,
    },
}

/// JSONL envelope shared by all events in one decision request.
#[derive(Serialize)]
struct DecisionLogRecord<'a> {
    timestamp_ms: u128,
    provider: &'a str,
    session_id: &'a str,
    task_id: Option<&'a str>,
    request_id: u64,
    #[serde(flatten)]
    event: LogEvent<'a>,
}

pub(crate) struct DecisionLog {
    writer: Box<dyn Write + Send>,
    provider: &'static str,
    session_id: String,
    task_id: Option<String>,
    request_id: u64,
}
impl DecisionLog {
    pub(crate) fn new(provider: &'static str) -> Self {
        Self {
            writer: Box::new(std::io::sink()),
            provider,
            session_id: format!(
                "{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ),
            request_id: 0,
            task_id: None,
        }
    }
    pub(crate) fn set_writer(&mut self, writer: impl Write + Send + 'static) {
        self.writer = Box::new(writer);
    }
    pub(crate) fn next_request(&mut self) {
        self.request_id += 1;
    }
    pub(crate) fn bind_task(&mut self, task_id: &str) {
        self.task_id = Some(task_id.to_owned());
        self.request_id = 0;
    }
    pub(crate) fn emit(&mut self, event: LogEvent<'_>) -> Result<(), Error> {
        let entry = DecisionLogRecord {
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            provider: self.provider,
            session_id: &self.session_id,
            task_id: self.task_id.as_deref(),
            request_id: self.request_id,
            event,
        };
        serde_json::to_writer(&mut self.writer, &entry)
            .map_err(|e| Error::Logging(e.to_string()))?;
        self.writer
            .write_all(b"\n")
            .and_then(|()| self.writer.flush())
            .map_err(|e| Error::Logging(e.to_string()))
    }
}
