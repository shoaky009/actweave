//! User-facing features and validated inputs, independent of model decisions.
use crate::{AdapterError, ToolCall};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ParameterKind {
    String,
    Integer { min: i64, max: i64 },
    Boolean,
    Enum { choices: Vec<Choice> },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Parameter {
    pub id: String,
    pub name: String,
    pub kind: ParameterKind,
    pub default: Option<Value>,
}
impl Parameter {
    fn accepts(&self, value: &Value) -> bool {
        match &self.kind {
            ParameterKind::String => value.is_string(),
            ParameterKind::Integer { min, max } => {
                value.as_i64().is_some_and(|v| v >= *min && v <= *max)
            }
            ParameterKind::Boolean => value.is_boolean(),
            ParameterKind::Enum { choices } => value
                .as_str()
                .is_some_and(|v| choices.iter().any(|c| c.value == v)),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Feature {
    pub id: String,
    pub name: String,
    pub description: String,
    pub parameters: Vec<Parameter>,
}
impl Feature {
    pub fn validate(&self) -> Result<(), AdapterError> {
        let invalid =
            |message: &str| AdapterError::Invalid(format!("feature {}: {message}", self.id));
        if self.id.trim().is_empty() || self.name.trim().is_empty() {
            return Err(invalid("empty id or name"));
        }
        let mut names = BTreeSet::new();
        for parameter in &self.parameters {
            if parameter.id.trim().is_empty() || !names.insert(&parameter.id) {
                return Err(invalid("duplicate or empty parameter"));
            }
            match &parameter.kind {
                ParameterKind::Integer { min, max } if min > max => {
                    return Err(invalid("invalid integer range"));
                }
                ParameterKind::Enum { choices } => {
                    let values: BTreeSet<_> = choices.iter().map(|c| &c.value).collect();
                    if choices.is_empty()
                        || values.len() != choices.len()
                        || choices.iter().any(|c| c.value.is_empty())
                    {
                        return Err(invalid("invalid enum choices"));
                    }
                }
                _ => {}
            }
            if parameter
                .default
                .as_ref()
                .is_some_and(|v| !parameter.accepts(v))
            {
                return Err(invalid("invalid default"));
            }
        }
        Ok(())
    }
    /// Apply defaults and reject unknown, missing or ill-typed fields before effects.
    pub fn resolve(&self, input: &Value) -> Result<Value, AdapterError> {
        self.validate()?;
        let input = input
            .as_object()
            .ok_or_else(|| AdapterError::Invalid("feature arguments must be an object".into()))?;
        for key in input.keys() {
            if !self.parameters.iter().any(|p| &p.id == key) {
                return Err(AdapterError::Invalid(format!("unknown parameter: {key}")));
            }
        }
        let mut output = Map::new();
        for parameter in &self.parameters {
            let value = input
                .get(&parameter.id)
                .or(parameter.default.as_ref())
                .ok_or_else(|| {
                    AdapterError::Invalid(format!("missing parameter: {}", parameter.id))
                })?;
            if !parameter.accepts(value) {
                return Err(AdapterError::Invalid(format!(
                    "invalid parameter: {}",
                    parameter.id
                )));
            }
            output.insert(parameter.id.clone(), value.clone());
        }
        Ok(Value::Object(output))
    }
    /// CLI helper; string/enum values stay strings, numbers and booleans are parsed.
    pub fn parse_arguments(&self, arguments: &[String]) -> Result<Value, AdapterError> {
        let mut input = Map::new();
        for argument in arguments {
            let (key, raw) = argument
                .split_once('=')
                .ok_or_else(|| AdapterError::Invalid("expected key=value".into()))?;
            let parameter = self
                .parameters
                .iter()
                .find(|p| p.id == key)
                .ok_or_else(|| AdapterError::Invalid(format!("unknown parameter: {key}")))?;
            let value = match parameter.kind {
                ParameterKind::String | ParameterKind::Enum { .. } => Value::String(raw.into()),
                _ => serde_json::from_str(raw)
                    .map_err(|_| AdapterError::Invalid(format!("invalid parameter: {key}")))?,
            };
            if input.insert(key.into(), value).is_some() {
                return Err(AdapterError::Invalid(format!("duplicate parameter: {key}")));
            }
        }
        self.resolve(&Value::Object(input))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepeatRequest {
    pub call: ToolCall,
    pub times: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Action {
    Call(ToolCall),
    Repeat(RepeatRequest),
    UntilDone(ToolCall),
}
impl Action {
    pub fn call(&self) -> &ToolCall {
        match self {
            Self::Call(call) | Self::UntilDone(call) => call,
            Self::Repeat(request) => &request.call,
        }
    }
}
/// A finite request finishes locally or reports failure; it never requests a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRequest {
    pub actions: Vec<Action>,
}
