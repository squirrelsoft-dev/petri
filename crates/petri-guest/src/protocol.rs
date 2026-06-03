use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const PROTOCOL_VERSION: u64 = 1;

#[derive(Debug, Deserialize)]
pub struct DispatchRequest {
    pub protocol_version: u64,
    pub id: String,
    #[serde(default)]
    pub control: Option<String>,
    #[serde(default)]
    pub target_id: Option<String>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub args: Option<Value>,
    #[serde(default)]
    pub limits: Option<RequestLimits>,
}

#[derive(Debug, Deserialize)]
pub struct RequestLimits {
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub max_output_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct BashCommandArgs {
    pub command: String,
    #[serde(default)]
    pub argv: Vec<String>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Success,
    Failure,
    Rejected,
    Timeout,
    Cancelled,
    Malformed,
}

#[derive(Debug, Serialize)]
pub struct ResultFrame {
    pub protocol_version: u64,
    pub id: Option<String>,
    pub status: Status,
    pub elapsed_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<Option<i32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_truncated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorFrame>,
}

#[derive(Debug, Serialize)]
pub struct ErrorFrame {
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Map<String, Value>>,
}

impl ResultFrame {
    pub fn placeholder_success(id: String, elapsed_ms: u128) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            id: Some(id),
            status: Status::Success,
            elapsed_ms,
            stdout: Some("petri-guest placeholder response\n".to_string()),
            stderr: Some(String::new()),
            exit_code: Some(Some(0)),
            output_truncated: Some(false),
            error: None,
        }
    }

    pub fn rejected(
        id: Option<String>,
        elapsed_ms: u128,
        code: &'static str,
        message: impl Into<String>,
        details: Option<Map<String, Value>>,
    ) -> Self {
        Self::error(Status::Rejected, id, elapsed_ms, code, message, details)
    }

    pub fn malformed(
        id: Option<String>,
        elapsed_ms: u128,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self::error(Status::Malformed, id, elapsed_ms, code, message, None)
    }

    fn error(
        status: Status,
        id: Option<String>,
        elapsed_ms: u128,
        code: &'static str,
        message: impl Into<String>,
        details: Option<Map<String, Value>>,
    ) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            id,
            status,
            elapsed_ms,
            stdout: None,
            stderr: None,
            exit_code: None,
            output_truncated: None,
            error: Some(ErrorFrame {
                code,
                message: message.into(),
                details,
            }),
        }
    }
}

pub fn request_id_from_value(value: &Value) -> Option<String> {
    value
        .as_object()
        .and_then(|object| object.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned)
}
