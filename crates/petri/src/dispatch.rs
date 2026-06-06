use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const PROTOCOL_VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchRequest {
    pub protocol_version: u64,
    pub id: String,
    pub tool: String,
    pub args: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limits: Option<RequestLimits>,
}

impl DispatchRequest {
    pub fn bash_command(
        id: impl Into<String>,
        command: impl Into<String>,
        argv: Vec<String>,
        cwd: PathBuf,
        env: BTreeMap<String, String>,
        stdin: Option<String>,
        limits: Option<RequestLimits>,
    ) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            id: id.into(),
            tool: "bash_command".to_string(),
            args: serde_json::json!({
                "command": command.into(),
                "argv": argv,
                "cwd": cwd,
                "env": env,
                "stdin": stdin,
            }),
            limits,
        }
    }

    pub fn with_stdin(mut self, stdin: impl Into<String>) -> Self {
        if let Some(args) = self.args.as_object_mut() {
            args.insert("stdin".to_string(), Value::String(stdin.into()));
        }
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Success,
    Failure,
    Rejected,
    Timeout,
    Cancelled,
    Malformed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchResult {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorFrame {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Map<String, Value>>,
}
