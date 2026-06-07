use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use petri_guest::lsp::{LspConfig, LspManager};
use petri_guest::policy::{CommandLevel, CommandPolicy, NetworkPolicy, Policy};
use petri_guest::protocol::{DispatchRequest, ResultFrame, Status};
use petri_guest::server::handle_frame;

/// Dispatch a frame with a disabled LSP manager (these tests cover the
/// bash/protocol surface only), starting at the policy's default command level.
fn handle(line: &str, policy: &Policy) -> ResultFrame {
    let lsp = LspManager::new(LspConfig::disabled(), std::env::temp_dir());
    let mut active = policy.command.default;
    handle_frame(line, policy, &lsp, &mut active)
}

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
        network: NetworkPolicy::disabled(),
        command: CommandPolicy {
            default: CommandLevel::Edit,
            max: CommandLevel::Yolo,
            read_only: std::collections::HashSet::new(),
            edit: allowed_commands
                .iter()
                .map(|command| (*command).to_string())
                .collect(),
        },
        max_runtime_secs: 60,
        max_output_bytes: 1024,
        workspace_path,
        drop_privileges: false,
    }
}

fn fixture(path: &str) -> serde_json::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path);
    let input = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!("failed to read fixture {}: {err}", path.display());
    });
    serde_json::from_str(&input).unwrap_or_else(|err| {
        panic!("failed to parse fixture {}: {err}", path.display());
    })
}

#[test]
fn shared_dispatch_fixtures_match_protocol_types() {
    let request = fixture("schema/fixtures/dispatch/bash-command.request.json");
    let request: DispatchRequest = serde_json::from_value(request).unwrap();
    assert_eq!(request.protocol_version, 1);
    assert_eq!(request.id, "fixture-bash-command");
    assert_eq!(request.tool.as_deref(), Some("bash_command"));

    let cancel = fixture("schema/fixtures/dispatch/cancel.request.json");
    let cancel: DispatchRequest = serde_json::from_value(cancel).unwrap();
    assert_eq!(cancel.control.as_deref(), Some("cancel"));
    assert_eq!(cancel.target_id.as_deref(), Some("fixture-bash-command"));

    for path in [
        "schema/fixtures/dispatch/success.result.json",
        "schema/fixtures/dispatch/policy-rejection.result.json",
    ] {
        let result = fixture(path);
        let result: ResultFrame = serde_json::from_value(result).unwrap();
        assert_eq!(result.protocol_version, 1);
    }
}

#[test]
fn guest_accepts_shared_bash_command_fixture() {
    let workspace = workspace();
    let mut request = fixture("schema/fixtures/dispatch/bash-command.request.json");
    request["args"]["cwd"] = serde_json::Value::from(workspace.display().to_string());
    let policy = Policy {
        network_enabled: false,
        network: NetworkPolicy::disabled(),
        command: CommandPolicy {
            default: CommandLevel::Edit,
            max: CommandLevel::Yolo,
            read_only: std::collections::HashSet::new(),
            edit: ["printf"].into_iter().map(str::to_string).collect(),
        },
        max_runtime_secs: 60,
        max_output_bytes: 1_048_576,
        workspace_path: workspace,
        drop_privileges: false,
    };

    let result = handle(&request.to_string(), &policy);

    assert_eq!(result.protocol_version, 1);
    assert_eq!(result.id.as_deref(), Some("fixture-bash-command"));
    assert_eq!(result.status, Status::Success);
    assert_eq!(result.stdout.as_deref(), Some("hello"));
}

#[test]
fn guest_accepts_shared_set_mode_fixture() {
    let request = fixture("schema/fixtures/dispatch/set-mode.request.json");
    let parsed: DispatchRequest = serde_json::from_value(request.clone()).unwrap();
    assert_eq!(parsed.control.as_deref(), Some("set_mode"));

    let result = handle(&request.to_string(), &policy(&["printf"], workspace()));

    assert_eq!(result.status, Status::Success);
    assert_eq!(
        result.data.unwrap()["mode"]["command"],
        serde_json::Value::from("edit")
    );
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

    let result = handle(&line, &policy(&["printf"], workspace));

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
    let result = handle(
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

    let result = handle(&line, &policy(&["printf"], workspace));

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

    let result = handle(&line, &policy(&["false"], workspace));

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

    let result = handle(&line, &policy(&["sleep"], workspace));

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

    let result = handle(&line, &policy(&["printf"], workspace));

    assert_eq!(result.status, Status::Success);
    assert_eq!(result.stdout.as_deref(), Some("abcd"));
    assert_eq!(result.output_truncated, Some(true));
}
