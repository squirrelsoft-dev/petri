//! LSP server lifecycle and request dispatch.
//!
//! The manager owns the language server subprocesses for a guest connection.
//! Servers start lazily on first use, are reused across requests, recover from
//! crashes (one restart-and-retry), and are shut down cleanly when the manager
//! is dropped.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::config::{LspConfig, LspServerConfig};
use super::jsonrpc::{LspError, Rpc};
use super::language;

const INIT_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DIAGNOSTICS_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest file the guest will open in a language server.
const MAX_OPEN_BYTES: u64 = 16 * 1024 * 1024;

/// Outcome of an LSP dispatch, mapped to a result frame by the server layer.
#[derive(Debug)]
pub enum LspOutcome {
    /// The server answered. Payload is the raw LSP result.
    Available(Value),
    /// No server could serve this request; the caller degrades to grep/read.
    Unavailable {
        language: Option<String>,
        reason: String,
    },
    /// The request was refused (bad shape or policy). Mapped to `rejected`.
    Rejected {
        code: &'static str,
        field: &'static str,
        message: String,
    },
    /// The server failed in a non-recoverable way. Mapped to `failure`.
    Failed(String),
}

/// Manages language server processes for one guest connection.
pub struct LspManager {
    config: LspConfig,
    workspace: PathBuf,
    servers: Mutex<std::collections::HashMap<String, ServerProcess>>,
}

impl LspManager {
    pub fn new(config: LspConfig, workspace: PathBuf) -> Self {
        Self {
            config,
            workspace,
            servers: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Whether any LSP support is configured.
    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// Dispatch an `lsp_*` tool request. `args` is the raw request args object.
    pub fn dispatch(&self, tool: &str, args: Option<&Value>) -> LspOutcome {
        use petri_protocol::lsp_tools::*;
        match tool {
            HOVER => self.position_request(args, "textDocument/hover", None),
            DEFINITION => self.position_request(args, "textDocument/definition", None),
            REFERENCES => self.position_request(
                args,
                "textDocument/references",
                Some(json!({ "includeDeclaration": true })),
            ),
            RENAME => self.rename_request(args),
            DIAGNOSTICS => self.diagnostics_request(args),
            _ => LspOutcome::Rejected {
                code: "unknown_tool",
                field: "tool",
                message: format!("unknown lsp tool '{tool}'"),
            },
        }
    }

    fn position_request(
        &self,
        args: Option<&Value>,
        method: &str,
        context: Option<Value>,
    ) -> LspOutcome {
        let args = match parse_args::<petri_protocol::LspPositionArgs>(args) {
            Ok(args) => args,
            Err(outcome) => return outcome,
        };
        let prepared = match self.prepare(&args.file) {
            Ok(prepared) => prepared,
            Err(outcome) => return outcome,
        };
        let line = args.line;
        let col = args.col;
        self.run(&prepared, move |server, uri| {
            let mut params = json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": col },
            });
            if let Some(context) = &context {
                params["context"] = context.clone();
            }
            server
                .rpc
                .request(method, params, deadline(REQUEST_TIMEOUT))
        })
    }

    fn rename_request(&self, args: Option<&Value>) -> LspOutcome {
        let args = match parse_args::<petri_protocol::LspRenameArgs>(args) {
            Ok(args) => args,
            Err(outcome) => return outcome,
        };
        let prepared = match self.prepare(&args.file) {
            Ok(prepared) => prepared,
            Err(outcome) => return outcome,
        };
        let line = args.line;
        let col = args.col;
        let new_name = args.new_name;
        self.run(&prepared, move |server, uri| {
            let params = json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": col },
                "newName": new_name,
            });
            server
                .rpc
                .request("textDocument/rename", params, deadline(REQUEST_TIMEOUT))
        })
    }

    fn diagnostics_request(&self, args: Option<&Value>) -> LspOutcome {
        let args = match parse_args::<petri_protocol::LspDiagnosticsArgs>(args) {
            Ok(args) => args,
            Err(outcome) => return outcome,
        };
        let prepared = match self.prepare(&args.file) {
            Ok(prepared) => prepared,
            Err(outcome) => return outcome,
        };
        self.run(&prepared, |server, uri| {
            match server
                .rpc
                .wait_for_diagnostics(uri, deadline(DIAGNOSTICS_TIMEOUT))?
            {
                Some(params) => Ok(params),
                None => Ok(json!({ "uri": uri, "diagnostics": [] })),
            }
        })
    }

    /// Resolve language + server config + canonical path for a file, or return
    /// the degradation/rejection outcome.
    fn prepare(&self, file: &Path) -> Result<Prepared, LspOutcome> {
        if !self.config.enabled {
            return Err(LspOutcome::Unavailable {
                language: None,
                reason: "lsp support is disabled in this image".to_string(),
            });
        }

        let detected = match language::detect(file) {
            Some(detected) => detected,
            None => {
                return Err(LspOutcome::Unavailable {
                    language: None,
                    reason: "no language detected for file extension".to_string(),
                });
            }
        };

        let server = match self.config.server_for_language(detected.config_language) {
            Some(server) => server.clone(),
            None => {
                return Err(LspOutcome::Unavailable {
                    language: Some(detected.config_language.to_string()),
                    reason: format!(
                        "no language server configured for '{}'",
                        detected.config_language
                    ),
                });
            }
        };

        let path = self.canonical_in_workspace(file)?;
        Ok(Prepared {
            language: detected.config_language.to_string(),
            language_id: detected.language_id,
            server,
            path,
        })
    }

    fn canonical_in_workspace(&self, file: &Path) -> Result<PathBuf, LspOutcome> {
        if !file.is_absolute() {
            return Err(LspOutcome::Rejected {
                code: "invalid_request",
                field: "args.file",
                message: "file path must be absolute".to_string(),
            });
        }
        let workspace = fs::canonicalize(&self.workspace).map_err(|_| {
            LspOutcome::Failed("policy workspace must exist and be accessible".to_string())
        })?;
        let path = fs::canonicalize(file).map_err(|_| LspOutcome::Rejected {
            code: "invalid_request",
            field: "args.file",
            message: "file must exist and be accessible".to_string(),
        })?;
        if !path.starts_with(&workspace) {
            return Err(LspOutcome::Rejected {
                code: "policy_denied",
                field: "args.file",
                message: "file must be inside policy workspace".to_string(),
            });
        }
        Ok(path)
    }

    /// Run an operation against the server for `prepared`, starting it lazily and
    /// restarting once if the connection is lost mid-request.
    fn run<F>(&self, prepared: &Prepared, op: F) -> LspOutcome
    where
        F: Fn(&mut ServerProcess, &str) -> Result<Value, LspError>,
    {
        let mut servers = self
            .servers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        for attempt in 0..2 {
            // (Re)start if absent or the previous process died.
            let needs_start = match servers.get_mut(&prepared.language) {
                Some(server) => {
                    if server.is_alive() {
                        false
                    } else {
                        servers.remove(&prepared.language);
                        true
                    }
                }
                None => true,
            };
            if needs_start {
                match ServerProcess::start(&prepared.server, &self.workspace) {
                    Ok(server) => {
                        servers.insert(prepared.language.clone(), server);
                    }
                    Err(err) => {
                        return LspOutcome::Failed(format!(
                            "failed to start {} language server: {err}",
                            prepared.language
                        ));
                    }
                }
            }

            let server = servers
                .get_mut(&prepared.language)
                .expect("server inserted above");

            let uri = match server.ensure_open(&prepared.path, prepared.language_id) {
                Ok(uri) => uri,
                Err(err) if err.is_connection_lost() && attempt == 0 => {
                    servers.remove(&prepared.language);
                    continue;
                }
                Err(err) => return LspOutcome::Failed(err.to_string()),
            };

            match op(server, &uri) {
                Ok(value) => return LspOutcome::Available(value),
                Err(err) if err.is_connection_lost() && attempt == 0 => {
                    servers.remove(&prepared.language);
                    continue;
                }
                Err(err) => return LspOutcome::Failed(err.to_string()),
            }
        }

        LspOutcome::Failed(format!(
            "{} language server repeatedly lost its connection",
            prepared.language
        ))
    }

    /// Cleanly shut down all running servers.
    pub fn shutdown(&self) {
        let mut servers = self
            .servers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, mut server) in servers.drain() {
            server.shutdown();
        }
    }
}

impl Drop for LspManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Prepared {
    language: String,
    language_id: &'static str,
    server: LspServerConfig,
    path: PathBuf,
}

struct ServerProcess {
    child: Child,
    rpc: Rpc,
    open: HashSet<String>,
}

impl ServerProcess {
    fn start(config: &LspServerConfig, root: &Path) -> Result<Self, LspError> {
        let mut child = Command::new(&config.binary)
            .args(&config.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| LspError::Connection(format!("spawn failed: {err}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| LspError::Connection("server stdin unavailable".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LspError::Connection("server stdout unavailable".to_string()))?;

        let mut rpc = Rpc::new(stdout, stdin);
        let root_uri = path_to_uri(root);
        rpc.initialize(&root_uri, Instant::now() + INIT_TIMEOUT)?;

        Ok(Self {
            child,
            rpc,
            open: HashSet::new(),
        })
    }

    fn is_alive(&mut self) -> bool {
        !self.rpc.is_closed() && matches!(self.child.try_wait(), Ok(None))
    }

    fn ensure_open(&mut self, file: &Path, language_id: &str) -> Result<String, LspError> {
        let uri = path_to_uri(file);
        if self.open.contains(&uri) {
            return Ok(uri);
        }

        let metadata = fs::metadata(file)
            .map_err(|err| LspError::Protocol(format!("failed to stat file: {err}")))?;
        if metadata.len() > MAX_OPEN_BYTES {
            return Err(LspError::Protocol(format!(
                "file is too large to open in a language server ({} bytes)",
                metadata.len()
            )));
        }
        let text = fs::read_to_string(file)
            .map_err(|err| LspError::Protocol(format!("failed to read file: {err}")))?;

        self.rpc.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": text,
                }
            }),
        )?;
        self.open.insert(uri.clone());
        Ok(uri)
    }

    fn shutdown(&mut self) {
        self.rpc.shutdown();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn deadline(timeout: Duration) -> Instant {
    Instant::now() + timeout
}

/// Parse typed args from the request args object, mapping errors to a rejection.
fn parse_args<T: serde::de::DeserializeOwned>(args: Option<&Value>) -> Result<T, LspOutcome> {
    let args = args.ok_or_else(|| LspOutcome::Rejected {
        code: "invalid_request",
        field: "args",
        message: "args is required".to_string(),
    })?;
    serde_json::from_value::<T>(args.clone()).map_err(|err| LspOutcome::Rejected {
        code: "invalid_request",
        field: "args",
        message: format!("invalid lsp args: {err}"),
    })
}

/// Convert an absolute filesystem path to a `file://` URI with minimal
/// percent-encoding of reserved bytes.
fn path_to_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    let raw = path.to_string_lossy();
    for byte in raw.as_bytes() {
        match byte {
            b'/' => uri.push('/'),
            // RFC 3986 unreserved set, kept verbatim.
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                uri.push(*byte as char);
            }
            other => {
                let _ = write!(uri, "%{other:02X}");
            }
        }
    }
    uri
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_workspace() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("petri-lsp-test-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn config_with_rust(binary: &str) -> LspConfig {
        LspConfig {
            enabled: true,
            servers: vec![LspServerConfig {
                language: "rust".to_string(),
                binary: binary.to_string(),
                args: vec![],
            }],
        }
    }

    #[test]
    fn path_to_uri_encodes_spaces() {
        let uri = path_to_uri(&PathBuf::from("/work space/main.rs"));
        assert_eq!(uri, "file:///work%20space/main.rs");
    }

    #[test]
    fn disabled_manager_degrades() {
        let manager = LspManager::new(LspConfig::disabled(), temp_workspace());
        let outcome = manager.dispatch(
            "lsp_hover",
            Some(&json!({"file": "/workspace/x.rs", "line": 1, "col": 1})),
        );
        assert!(matches!(outcome, LspOutcome::Unavailable { .. }));
    }

    #[test]
    fn unknown_language_degrades() {
        let workspace = temp_workspace();
        let file = workspace.join("notes.md");
        fs::write(&file, "hi").unwrap();
        let manager = LspManager::new(config_with_rust("rust-analyzer"), workspace);
        let outcome = manager.dispatch(
            "lsp_hover",
            Some(&json!({"file": file, "line": 0, "col": 0})),
        );
        match outcome {
            LspOutcome::Unavailable { language, .. } => assert_eq!(language, None),
            other => panic!("expected unavailable, got {other:?}"),
        }
    }

    #[test]
    fn unconfigured_language_degrades_with_language() {
        let workspace = temp_workspace();
        let file = workspace.join("main.go");
        fs::write(&file, "package main").unwrap();
        let manager = LspManager::new(config_with_rust("rust-analyzer"), workspace);
        let outcome = manager.dispatch(
            "lsp_definition",
            Some(&json!({"file": file, "line": 0, "col": 0})),
        );
        match outcome {
            LspOutcome::Unavailable { language, .. } => {
                assert_eq!(language.as_deref(), Some("go"));
            }
            other => panic!("expected unavailable, got {other:?}"),
        }
    }

    #[test]
    fn file_outside_workspace_is_rejected() {
        let workspace = temp_workspace();
        let outside = temp_workspace().join("evil.rs");
        fs::write(&outside, "fn main() {}").unwrap();
        let manager = LspManager::new(config_with_rust("rust-analyzer"), workspace);
        let outcome = manager.dispatch(
            "lsp_hover",
            Some(&json!({"file": outside, "line": 0, "col": 0})),
        );
        match outcome {
            LspOutcome::Rejected { code, .. } => assert_eq!(code, "policy_denied"),
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[test]
    fn missing_file_is_rejected() {
        let workspace = temp_workspace();
        let manager = LspManager::new(config_with_rust("rust-analyzer"), workspace.clone());
        let outcome = manager.dispatch(
            "lsp_hover",
            Some(&json!({"file": workspace.join("nope.rs"), "line": 0, "col": 0})),
        );
        match outcome {
            LspOutcome::Rejected { code, field, .. } => {
                assert_eq!(code, "invalid_request");
                assert_eq!(field, "args.file");
            }
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[test]
    fn missing_server_binary_fails() {
        let workspace = temp_workspace();
        let file = workspace.join("main.rs");
        fs::write(&file, "fn main() {}").unwrap();
        let manager = LspManager::new(config_with_rust("petri-nonexistent-lsp-binary"), workspace);
        let outcome = manager.dispatch(
            "lsp_hover",
            Some(&json!({"file": file, "line": 0, "col": 0})),
        );
        assert!(matches!(outcome, LspOutcome::Failed(_)), "got {outcome:?}");
    }

    #[test]
    fn bad_args_are_rejected() {
        let manager = LspManager::new(config_with_rust("rust-analyzer"), temp_workspace());
        let outcome = manager.dispatch("lsp_hover", Some(&json!({"file": "/x.rs"})));
        assert!(matches!(
            outcome,
            LspOutcome::Rejected {
                code: "invalid_request",
                ..
            }
        ));
    }
}
