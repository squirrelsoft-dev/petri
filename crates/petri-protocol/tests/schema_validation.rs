//! Validates the checked-in JSON Schema against every shared fixture, and
//! guards against drift between the Rust wire types and that schema.
//!
//! The schema at `schema/petri-protocol-v1.schema.json` is the language-client
//! contract (TypeScript, Python, Go, ...). These tests make it enforceable: a
//! fixture that stops matching the schema, or a Rust type whose serialization
//! diverges from it, fails `cargo test`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use jsonschema::Validator;
use petri_protocol::{
    DispatchRequest, PROTOCOL_VERSION, RequestLimits, ResultFrame, SetModeArgs, Status,
};
use serde_json::{Value, json};

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/petri-protocol; the repo root is two up.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("resolve repo root")
}

fn schema_value() -> Value {
    let path = repo_root().join("schema/petri-protocol-v1.schema.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read schema {}: {err}", path.display()));
    serde_json::from_str(&raw)
        .unwrap_or_else(|err| panic!("parse schema {}: {err}", path.display()))
}

fn compiled_schema() -> Validator {
    jsonschema::validator_for(&schema_value()).expect("protocol schema compiles")
}

fn assert_valid(validator: &Validator, label: &str, instance: &Value) {
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|err| format!("  at {}: {err}", err.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "{label} does not satisfy the protocol schema:\n{}",
        errors.join("\n")
    );
}

fn fixture_files() -> Vec<PathBuf> {
    let dir = repo_root().join("schema/fixtures/dispatch");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("read fixtures dir {}: {err}", dir.display()))
        .map(|entry| entry.expect("read dir entry").path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no dispatch fixtures found");
    files
}

fn read_json(path: &Path) -> Value {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("read fixture {}: {err}", path.display()));
    serde_json::from_str(&raw)
        .unwrap_or_else(|err| panic!("parse fixture {}: {err}", path.display()))
}

/// Every checked-in dispatch fixture must satisfy the published schema. This is
/// the contract every language client consumes, so drift here is a wire break.
#[test]
fn all_dispatch_fixtures_satisfy_schema() {
    let validator = compiled_schema();
    for path in fixture_files() {
        let label = path.file_name().unwrap().to_string_lossy().into_owned();
        assert_valid(&validator, &label, &read_json(&path));
    }
}

/// Frames built from the Rust constructors must validate against the schema, so
/// the hand-maintained schema cannot silently drift away from the wire types.
#[test]
fn rust_frames_match_schema() {
    let validator = compiled_schema();

    let bash = DispatchRequest::bash_command(
        "drift-bash",
        "printf",
        vec!["hello".to_string()],
        PathBuf::from("/workspace"),
        BTreeMap::new(),
        Some("input".to_string()),
        Some(RequestLimits {
            timeout_ms: Some(30_000),
            max_output_bytes: Some(1_048_576),
        }),
    );
    assert_valid(
        &validator,
        "DispatchRequest::bash_command",
        &serde_json::to_value(&bash).unwrap(),
    );

    let set_mode = DispatchRequest {
        protocol_version: PROTOCOL_VERSION,
        id: "drift-set-mode".to_string(),
        control: Some("set_mode".to_string()),
        target_id: None,
        tool: None,
        args: Some(
            serde_json::to_value(SetModeArgs {
                command: Some("edit".to_string()),
                network: None,
            })
            .unwrap(),
        ),
        limits: None,
    };
    assert_valid(
        &validator,
        "DispatchRequest set_mode",
        &serde_json::to_value(&set_mode).unwrap(),
    );

    let process = ResultFrame::process(
        "drift-process".to_string(),
        Status::Success,
        7,
        "hello".to_string(),
        String::new(),
        Some(0),
        false,
    );
    assert_valid(
        &validator,
        "ResultFrame::process",
        &serde_json::to_value(&process).unwrap(),
    );

    let timeout = ResultFrame::timeout(
        "drift-timeout".to_string(),
        20,
        String::new(),
        String::new(),
        false,
    );
    assert_valid(
        &validator,
        "ResultFrame::timeout",
        &serde_json::to_value(&timeout).unwrap(),
    );

    let rejected = ResultFrame::rejected(
        Some("drift-rejected".to_string()),
        1,
        "policy_denied",
        "command is not allowed by policy",
        None,
    );
    assert_valid(
        &validator,
        "ResultFrame::rejected",
        &serde_json::to_value(&rejected).unwrap(),
    );

    let data = ResultFrame::data("drift-data".to_string(), 37, json!({ "available": true }));
    assert_valid(
        &validator,
        "ResultFrame::data",
        &serde_json::to_value(&data).unwrap(),
    );
}

/// The validator must actually reject malformed frames; otherwise the positive
/// assertions above prove nothing.
#[test]
fn schema_rejects_invalid_frames() {
    let validator = compiled_schema();

    // Wrong protocol version.
    assert!(
        !validator.is_valid(&json!({
            "protocol_version": 2,
            "id": "bad-version",
            "tool": "bash_command",
            "args": { "command": "printf", "cwd": "/workspace" }
        })),
        "schema accepted an unsupported protocol_version"
    );

    // Unknown result status.
    assert!(
        !validator.is_valid(&json!({
            "protocol_version": 1,
            "id": "bad-status",
            "status": "exploded",
            "elapsed_ms": 1
        })),
        "schema accepted an unknown result status"
    );

    // bash_command missing required cwd.
    assert!(
        !validator.is_valid(&json!({
            "protocol_version": 1,
            "id": "bad-bash",
            "tool": "bash_command",
            "args": { "command": "printf" }
        })),
        "schema accepted a bash_command request without cwd"
    );
}
