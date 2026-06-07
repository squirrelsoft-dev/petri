use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub mod policy;

pub const PROTOCOL_VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchRequest {
    pub protocol_version: u64,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
            control: None,
            target_id: None,
            tool: Some("bash_command".to_string()),
            args: Some(serde_json::json!({
                "command": command.into(),
                "argv": argv,
                "cwd": cwd,
                "env": env,
                "stdin": stdin,
            })),
            limits,
        }
    }

    pub fn with_stdin(mut self, stdin: impl Into<String>) -> Self {
        if let Some(args) = self.args.as_mut().and_then(Value::as_object_mut) {
            args.insert("stdin".to_string(), Value::String(stdin.into()));
        }
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BashCommandArgs {
    pub command: String,
    #[serde(default)]
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub stdin: Option<String>,
}

/// Args for the `set_mode` control frame. The `command` axis is guest-enforced
/// and validated against the boot policy's command ceiling. The `network` axis
/// is enforced host-side at the VM boundary and is rejected in a guest-bound
/// frame; the field exists only so the guest can return a clear error. See ADR 0002.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetModeArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
}

/// Tool names for the semantic (LSP-backed) tool surface.
pub mod lsp_tools {
    pub const HOVER: &str = "lsp_hover";
    pub const DEFINITION: &str = "lsp_definition";
    pub const REFERENCES: &str = "lsp_references";
    pub const DIAGNOSTICS: &str = "lsp_diagnostics";
    pub const RENAME: &str = "lsp_rename";

    /// Whether `tool` names one of the LSP tools.
    pub fn is_lsp_tool(tool: &str) -> bool {
        matches!(tool, HOVER | DEFINITION | REFERENCES | RENAME | DIAGNOSTICS)
    }
}

/// Args shared by position-based LSP tools (`lsp_hover`, `lsp_definition`,
/// `lsp_references`). Positions are zero-based, matching the LSP spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspPositionArgs {
    pub file: PathBuf,
    pub line: u32,
    pub col: u32,
}

/// Args for `lsp_diagnostics`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspDiagnosticsArgs {
    pub file: PathBuf,
}

/// Args for `lsp_rename`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspRenameArgs {
    pub file: PathBuf,
    pub line: u32,
    pub col: u32,
    pub new_name: String,
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
    /// Structured, tool-specific result payload. Used by non-process tools such
    /// as the `lsp_*` family, which return JSON data rather than stdio streams.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorFrame>,
}

pub type DispatchResult = ResultFrame;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorFrame {
    pub code: String,
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
            data: None,
            error: None,
        }
    }

    /// Build a successful result for a structured (non-process) tool, carrying a
    /// JSON `data` payload instead of stdio streams. Used by the `lsp_*` tools.
    pub fn data(id: String, elapsed_ms: u64, data: Value) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            id: Some(id),
            status: Status::Success,
            elapsed_ms,
            stdout: None,
            stderr: None,
            exit_code: None,
            output_truncated: None,
            data: Some(data),
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
            code: "timeout_exceeded".to_string(),
            message: "request exceeded effective timeout".to_string(),
            details: None,
        });
        result
    }

    pub fn rejected(
        id: Option<String>,
        elapsed_ms: u64,
        code: impl Into<String>,
        message: impl Into<String>,
        details: Option<Map<String, Value>>,
    ) -> Self {
        Self::error(Status::Rejected, id, elapsed_ms, code, message, details)
    }

    pub fn malformed(
        id: Option<String>,
        elapsed_ms: u64,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::error(Status::Malformed, id, elapsed_ms, code, message, None)
    }

    fn error(
        status: Status,
        id: Option<String>,
        elapsed_ms: u64,
        code: impl Into<String>,
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
            data: None,
            error: Some(ErrorFrame {
                code: code.into(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_in_schema_and_fixtures_are_valid_json() {
        for input in [
            include_str!("../../../schema/petri-protocol-v1.schema.json"),
            include_str!("../../../schema/fixtures/dispatch/bash-command.request.json"),
            include_str!("../../../schema/fixtures/dispatch/cancel.request.json"),
            include_str!("../../../schema/fixtures/dispatch/set-mode.request.json"),
            include_str!("../../../schema/fixtures/dispatch/set-mode.result.json"),
            include_str!("../../../schema/fixtures/dispatch/success.result.json"),
            include_str!("../../../schema/fixtures/dispatch/policy-rejection.result.json"),
            include_str!("../../../schema/fixtures/dispatch/lsp-hover.request.json"),
            include_str!("../../../schema/fixtures/dispatch/lsp-hover.result.json"),
            include_str!("../../../schema/fixtures/dispatch/lsp-unavailable.result.json"),
        ] {
            serde_json::from_str::<Value>(input).unwrap();
        }
    }

    #[test]
    fn lsp_fixtures_deserialize_into_wire_types() {
        let request: DispatchRequest = serde_json::from_str(include_str!(
            "../../../schema/fixtures/dispatch/lsp-hover.request.json"
        ))
        .unwrap();
        assert_eq!(request.tool.as_deref(), Some("lsp_hover"));
        let args: LspPositionArgs = serde_json::from_value(request.args.unwrap()).unwrap();
        assert_eq!(args.line, 42);
        assert_eq!(args.col, 15);

        let result: ResultFrame = serde_json::from_str(include_str!(
            "../../../schema/fixtures/dispatch/lsp-hover.result.json"
        ))
        .unwrap();
        assert_eq!(result.status, Status::Success);
        assert_eq!(result.data.unwrap()["available"], Value::Bool(true));
    }
}
