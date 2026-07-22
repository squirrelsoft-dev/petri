//! End-to-end tests against real language servers, when present on the host.
//!
//! These are `#[ignore]`d by default because they depend on language server
//! binaries being installed and can be slow (real indexing). Run explicitly:
//!
//! ```sh
//! cargo test -p petri-guest --test lsp_real_server -- --ignored --nocapture
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use petri_guest::lsp::config::{LspConfig, LspServerConfig};
use petri_guest::lsp::manager::{LspManager, LspOutcome};
use serde_json::json;

fn have(binary: &str) -> bool {
    // Probe the binary is actually runnable. rust-analyzer on PATH may be a
    // rustup proxy whose component is not installed, so `--version` failing
    // correctly marks it unavailable. gopls uses a `version` subcommand.
    let probes: &[&[&str]] = &[&["--version"], &["version"]];
    probes.iter().any(|args| {
        std::process::Command::new(binary)
            .args(*args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

fn unique_dir(tag: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("petri-lsp-real-{tag}-{unique}"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn manager(language: &str, binary: &str, args: &[&str], workspace: &Path) -> LspManager {
    let config = LspConfig {
        enabled: true,
        servers: vec![LspServerConfig {
            language: language.to_string(),
            binary: binary.to_string(),
            args: args.iter().map(std::string::ToString::to_string).collect(),
        }],
    };
    LspManager::new(config, workspace.to_path_buf())
}

/// Retry a position request until the server returns a non-null result or the
/// deadline passes (real servers answer `null` until indexing completes).
fn hover_until_ready(manager: &LspManager, file: &Path, line: u32, col: u32) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last = json!(null);
    while Instant::now() < deadline {
        match manager.dispatch(
            "lsp_hover",
            Some(&json!({ "file": file, "line": line, "col": col })),
        ) {
            LspOutcome::Available(value) => {
                last = value.clone();
                if !value.is_null() {
                    return value;
                }
            }
            other => panic!("expected available, got {other:?}"),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    last
}

#[test]
#[ignore = "requires rust-analyzer"]
fn rust_analyzer_hover_and_definition() {
    if !have("rust-analyzer") {
        eprintln!("skipping: rust-analyzer not installed");
        return;
    }

    let workspace = unique_dir("rust");
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::create_dir_all(workspace.join("src")).unwrap();
    let file = workspace.join("src/main.rs");
    fs::write(
        &file,
        "fn greeting() -> String {\n    String::from(\"hi\")\n}\n\nfn main() {\n    let message = greeting();\n    println!(\"{message}\");\n}\n",
    )
    .unwrap();

    let manager = manager("rust", "rust-analyzer", &[], &workspace);

    // Hover over the `greeting` call on line 5 (0-based), col 18.
    let hover = hover_until_ready(&manager, &file, 5, 18);
    eprintln!("rust-analyzer hover => {hover}");
    let rendered = hover.to_string();
    assert!(
        rendered.contains("greeting") || rendered.contains("String"),
        "hover should describe the symbol, got: {rendered}"
    );

    // Definition of `greeting` at the same position should point back into the file.
    let definition = manager.dispatch(
        "lsp_definition",
        Some(&json!({ "file": file, "line": 5, "col": 18 })),
    );
    match definition {
        LspOutcome::Available(value) => {
            eprintln!("rust-analyzer definition => {value}");
            assert!(
                value.to_string().contains("main.rs"),
                "definition should reference the source file, got: {value}"
            );
        }
        other => panic!("expected available definition, got {other:?}"),
    }
}

#[test]
#[ignore = "requires gopls"]
fn gopls_hover() {
    if !have("gopls") {
        eprintln!("skipping: gopls not installed");
        return;
    }

    let workspace = unique_dir("go");
    fs::write(workspace.join("go.mod"), "module probe\n\ngo 1.21\n").unwrap();
    let file = workspace.join("main.go");
    fs::write(
        &file,
        "package main\n\nimport \"fmt\"\n\nfunc greeting() string {\n\treturn \"hi\"\n}\n\nfunc main() {\n\tfmt.Println(greeting())\n}\n",
    )
    .unwrap();

    let manager = manager("go", "gopls", &[], &workspace);

    // Hover over the `greeting` call on line 9 (0-based), col 13.
    let hover = hover_until_ready(&manager, &file, 9, 13);
    eprintln!("gopls hover => {hover}");
    assert!(
        hover.to_string().contains("greeting") || hover.to_string().contains("string"),
        "hover should describe greeting(), got: {hover}"
    );
}

#[test]
#[ignore = "requires typescript-language-server"]
fn typescript_language_server_hover() {
    if !have("typescript-language-server") {
        eprintln!("skipping: typescript-language-server not installed");
        return;
    }

    let workspace = unique_dir("ts");
    let file = workspace.join("index.ts");
    fs::write(
        &file,
        "function greeting(): string {\n  return \"hi\";\n}\n\nconst message = greeting();\nconsole.log(message);\n",
    )
    .unwrap();

    let manager = manager(
        "typescript",
        "typescript-language-server",
        &["--stdio"],
        &workspace,
    );

    // Hover over the `greeting` call on line 4 (0-based), col 16.
    let hover = hover_until_ready(&manager, &file, 4, 16);
    eprintln!("typescript hover => {hover}");
    assert!(
        hover.to_string().contains("greeting") || hover.to_string().contains("string"),
        "hover should describe greeting(), got: {hover}"
    );
}

#[test]
#[ignore = "requires clangd"]
fn clangd_hover() {
    if !have("clangd") {
        eprintln!("skipping: clangd not installed");
        return;
    }

    let workspace = unique_dir("c");
    let file = workspace.join("main.c");
    fs::write(
        &file,
        "int add(int a, int b) {\n    return a + b;\n}\n\nint main(void) {\n    return add(1, 2);\n}\n",
    )
    .unwrap();

    let manager = manager("c", "clangd", &[], &workspace);

    // Hover over the `add` call on line 5, col 11.
    let hover = hover_until_ready(&manager, &file, 5, 11);
    eprintln!("clangd hover => {hover}");
    assert!(
        hover.to_string().contains("add") || hover.to_string().contains("int"),
        "hover should describe add(), got: {hover}"
    );
}
