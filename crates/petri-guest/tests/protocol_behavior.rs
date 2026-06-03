use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use petri_guest::policy::Policy;
use petri_guest::protocol::Status;
use petri_guest::server::handle_frame;

fn workspace() -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "petri-guest-protocol-test-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn policy(allowed_commands: &[&str], workspace_path: PathBuf) -> Policy {
    Policy {
        network_enabled: false,
        allowed_commands: allowed_commands
            .iter()
            .map(|command| (*command).to_string())
            .collect(),
        max_runtime_secs: 60,
        max_output_bytes: 1024,
        workspace_path,
    }
}

#[test]
fn accepts_valid_dispatch_and_captures_process_result() {
    let workspace = workspace();
    let line = serde_json::json!({
        "protocol_version": 1,
        "id": "valid-dispatch",
        "tool": "bash_command",
        "args": {
            "command": "printf",
            "argv": ["hello"],
            "cwd": workspace.clone(),
        }
    })
    .to_string();

    let result = handle_frame(&line, &policy(&["printf"], workspace));

    assert_eq!(result.protocol_version, 1);
    assert_eq!(result.id.as_deref(), Some("valid-dispatch"));
    assert_eq!(result.status, Status::Success);
    assert_eq!(result.stdout.as_deref(), Some("hello"));
    assert_eq!(result.stderr.as_deref(), Some(""));
    assert_eq!(result.exit_code, Some(Some(0)));
    assert_eq!(result.output_truncated, Some(false));
    assert!(result.error.is_none());
}

#[test]
fn reports_malformed_json_without_request_id() {
    let result = handle_frame(
        r#"{"protocol_version":1,"id":"bad-json","tool":"#,
        &policy(&["printf"], workspace()),
    );

    assert_eq!(result.status, Status::Malformed);
    assert!(result.id.is_none());
    let error = result.error.unwrap();
    assert_eq!(error.code, "malformed_frame");
    assert_eq!(error.message, "dispatch frame is not valid JSON");
}

#[test]
fn rejects_command_disallowed_by_policy() {
    let workspace = workspace();
    let line = serde_json::json!({
        "protocol_version": 1,
        "id": "policy-rejection",
        "tool": "bash_command",
        "args": {
            "command": "false",
            "cwd": workspace.clone(),
        }
    })
    .to_string();

    let result = handle_frame(&line, &policy(&["printf"], workspace));

    assert_eq!(result.id.as_deref(), Some("policy-rejection"));
    assert_eq!(result.status, Status::Rejected);
    let error = result.error.unwrap();
    assert_eq!(error.code, "policy_denied");
    assert_eq!(error.message, "command is not allowed by policy");
}

#[test]
fn reports_non_zero_exit_as_command_failure() {
    let workspace = workspace();
    let line = serde_json::json!({
        "protocol_version": 1,
        "id": "command-failure",
        "tool": "bash_command",
        "args": {
            "command": "false",
            "cwd": workspace.clone(),
        }
    })
    .to_string();

    let result = handle_frame(&line, &policy(&["false"], workspace));

    assert_eq!(result.status, Status::Failure);
    assert_ne!(result.exit_code, Some(Some(0)));
    assert_eq!(result.stdout.as_deref(), Some(""));
    assert_eq!(result.stderr.as_deref(), Some(""));
    assert!(result.error.is_none());
}

#[test]
fn times_out_when_request_limit_is_exceeded() {
    let workspace = workspace();
    let line = serde_json::json!({
        "protocol_version": 1,
        "id": "timeout",
        "tool": "bash_command",
        "args": {
            "command": "sleep",
            "argv": ["1"],
            "cwd": workspace.clone(),
        },
        "limits": {
            "timeout_ms": 20,
        }
    })
    .to_string();

    let result = handle_frame(&line, &policy(&["sleep"], workspace));

    assert_eq!(result.status, Status::Timeout);
    assert_eq!(result.exit_code, Some(None));
    let error = result.error.unwrap();
    assert_eq!(error.code, "timeout_exceeded");
}

#[test]
fn truncates_output_to_request_limit() {
    let workspace = workspace();
    let line = serde_json::json!({
        "protocol_version": 1,
        "id": "truncation",
        "tool": "bash_command",
        "args": {
            "command": "printf",
            "argv": ["abcdef"],
            "cwd": workspace.clone(),
        },
        "limits": {
            "max_output_bytes": 4,
        }
    })
    .to_string();

    let result = handle_frame(&line, &policy(&["printf"], workspace));

    assert_eq!(result.status, Status::Success);
    assert_eq!(result.stdout.as_deref(), Some("abcd"));
    assert_eq!(result.output_truncated, Some(true));
}
