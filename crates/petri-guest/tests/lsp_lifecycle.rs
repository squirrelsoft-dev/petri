//! End-to-end LSP manager lifecycle tests driving a real subprocess
//! (`examples/fake_lsp_server.rs`): lazy start, server reuse, and
//! restart-after-crash recovery.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use petri_guest::lsp::config::{LspConfig, LspServerConfig};
use petri_guest::lsp::manager::{LspManager, LspOutcome};
use serde_json::json;

fn fake_server_bin() -> PathBuf {
    let mut dir = std::env::current_exe().expect("current exe");
    dir.pop(); // test binary file
    if dir.ends_with("deps") {
        dir.pop();
    }
    let bin = dir.join("examples").join("fake_lsp_server");
    if !bin.exists() {
        // `cargo test --test <name>` does not build examples; build it on demand
        // so the test is self-contained regardless of invocation.
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let status = std::process::Command::new(cargo)
            .args(["build", "-p", "petri-guest", "--example", "fake_lsp_server"])
            .status()
            .expect("build fake_lsp_server example");
        assert!(status.success(), "failed to build fake_lsp_server example");
    }
    assert!(
        bin.exists(),
        "fake_lsp_server example not built at {}",
        bin.display()
    );
    bin
}

fn unique_dir(tag: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("petri-lsp-{tag}-{}-{unique}", std::process::id()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn manager_for(workspace: &Path, args: Vec<String>) -> LspManager {
    let config = LspConfig {
        enabled: true,
        servers: vec![LspServerConfig {
            language: "rust".to_string(),
            binary: fake_server_bin().to_string_lossy().into_owned(),
            args,
        }],
    };
    LspManager::new(config, workspace.to_path_buf())
}

#[test]
fn lazy_start_serves_request_and_reuses_server() {
    let workspace = unique_dir("happy");
    let file = workspace.join("main.rs");
    fs::write(&file, "fn main() {}\n").unwrap();
    // State file pre-created => the fake server never crashes.
    let state = workspace.join("state");
    fs::write(&state, "ready").unwrap();

    let manager = manager_for(&workspace, vec![state.to_string_lossy().into_owned()]);

    let first = manager.dispatch(
        "lsp_hover",
        Some(&json!({ "file": file, "line": 0, "col": 3 })),
    );
    match first {
        LspOutcome::Available(value) => {
            assert_eq!(value.get("method").unwrap(), "textDocument/hover");
        }
        other => panic!("expected available, got {other:?}"),
    }

    // A second request reuses the same process and still succeeds.
    let second = manager.dispatch(
        "lsp_definition",
        Some(&json!({ "file": file, "line": 0, "col": 3 })),
    );
    match second {
        LspOutcome::Available(value) => {
            assert_eq!(value.get("method").unwrap(), "textDocument/definition");
        }
        other => panic!("expected available, got {other:?}"),
    }
}

#[test]
fn recovers_after_server_crash() {
    let workspace = unique_dir("crash");
    let file = workspace.join("main.rs");
    fs::write(&file, "fn main() {}\n").unwrap();
    // State file absent => first tool request crashes the server, then the
    // manager restarts it and the retry succeeds.
    let state = workspace.join("state");

    let manager = manager_for(&workspace, vec![state.to_string_lossy().into_owned()]);

    let outcome = manager.dispatch(
        "lsp_hover",
        Some(&json!({ "file": file, "line": 0, "col": 3 })),
    );
    match outcome {
        LspOutcome::Available(value) => {
            assert_eq!(value.get("recovered").unwrap(), true);
        }
        other => panic!("expected recovered available, got {other:?}"),
    }
    assert!(state.exists(), "crash marker should have been written");
}
