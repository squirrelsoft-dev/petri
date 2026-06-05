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
    pub elapsed_ms: u64,
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
    pub fn process(
        id: String,
        status: Status,
        elapsed_ms: u64,
        stdout: String,
        stderr: String,
        exit_code: Option<i32>,
        output_truncated: bool,
    ) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            id: Some(id),
            status,
            elapsed_ms,
            stdout: Some(stdout),
            stderr: Some(stderr),
            exit_code: Some(exit_code),
            output_truncated: Some(output_truncated),
            error: None,
        }
    }

    pub fn timeout(
        id: String,
        elapsed_ms: u64,
        stdout: String,
        stderr: String,
        output_truncated: bool,
    ) -> Self {
        let mut result = Self::process(
            id,
            Status::Timeout,
            elapsed_ms,
            stdout,
            stderr,
            None,
            output_truncated,
        );
        result.error = Some(ErrorFrame {
            code: "timeout_exceeded",
            message: "request exceeded effective timeout".to_string(),
            details: None,
        });
        result
    }

    pub fn rejected(
        id: Option<String>,
        elapsed_ms: u64,
        code: &'static str,
        message: impl Into<String>,
        details: Option<Map<String, Value>>,
    ) -> Self {
        Self::error(Status::Rejected, id, elapsed_ms, code, message, details)
    }

    pub fn malformed(
        id: Option<String>,
        elapsed_ms: u64,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self::error(Status::Malformed, id, elapsed_ms, code, message, None)
    }

    fn error(
        status: Status,
        id: Option<String>,
        elapsed_ms: u64,
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
