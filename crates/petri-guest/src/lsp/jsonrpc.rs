//! Minimal LSP JSON-RPC client over stdio.
//!
//! Implements just enough of the Language Server Protocol base protocol to
//! drive the guest tool surface: `Content-Length`-framed JSON-RPC messages, the
//! `initialize`/`initialized` handshake, request/response correlation, and
//! collection of `textDocument/publishDiagnostics` notifications.
//!
//! Blocking reads are bounded by running the read side on a dedicated thread
//! that forwards parsed messages over a channel, so request and diagnostics
//! waits honor a deadline even though the underlying transport (a child pipe)
//! has no native read timeout.

use std::io::{self, BufRead, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Errors surfaced while talking to a language server.
#[derive(Debug)]
pub enum LspError {
    /// The transport failed — broken pipe, closed connection, or read error.
    /// The owning manager treats this as "server crashed" and may restart.
    Connection(String),
    /// The server returned a JSON-RPC error object for a request.
    Rpc { code: i64, message: String },
    /// The operation exceeded its deadline.
    Timeout,
    /// The server sent something that did not parse as expected.
    Protocol(String),
}

impl std::fmt::Display for LspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connection(message) => write!(f, "lsp connection error: {message}"),
            Self::Rpc { code, message } => write!(f, "lsp rpc error {code}: {message}"),
            Self::Timeout => write!(f, "lsp request timed out"),
            Self::Protocol(message) => write!(f, "lsp protocol error: {message}"),
        }
    }
}

impl std::error::Error for LspError {}

impl LspError {
    /// Whether this error indicates the server connection is no longer usable
    /// and the server should be discarded (and possibly restarted).
    pub fn is_connection_lost(&self) -> bool {
        matches!(self, Self::Connection(_))
    }
}

enum ReaderEvent {
    Message(Value),
    Closed,
}

/// A live JSON-RPC connection to a language server.
pub struct Rpc {
    writer: Box<dyn Write + Send>,
    events: Receiver<ReaderEvent>,
    reader_thread: Option<JoinHandle<()>>,
    next_id: i64,
    /// Latest `publishDiagnostics` params, keyed by document URI.
    diagnostics: std::collections::HashMap<String, Value>,
    closed: bool,
}

impl Rpc {
    /// Build a connection from a reader and writer. Spawns the reader thread.
    pub fn new(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let reader_thread = thread::spawn(move || {
            let mut reader = io::BufReader::new(reader);
            loop {
                match read_message(&mut reader) {
                    Ok(Some(message)) => {
                        if tx.send(ReaderEvent::Message(message)).is_err() {
                            break;
                        }
                    }
                    Ok(None) | Err(_) => {
                        let _ = tx.send(ReaderEvent::Closed);
                        break;
                    }
                }
            }
        });

        Self {
            writer: Box::new(writer),
            events: rx,
            reader_thread: Some(reader_thread),
            next_id: 1,
            diagnostics: std::collections::HashMap::new(),
            closed: false,
        }
    }

    /// Perform the `initialize` request and send `initialized`, establishing the
    /// session rooted at `root_uri`.
    pub fn initialize(&mut self, root_uri: &str, deadline: Instant) -> Result<Value, LspError> {
        let params = json!({
            "processId": null,
            "rootUri": root_uri,
            "capabilities": {
                "textDocument": {
                    "hover": { "contentFormat": ["plaintext", "markdown"] },
                    "definition": {},
                    "references": {},
                    "rename": {},
                    "publishDiagnostics": {}
                },
                "workspace": { "workspaceFolders": true }
            },
            "workspaceFolders": [{ "uri": root_uri, "name": "workspace" }]
        });
        let result = self.request("initialize", params, deadline)?;
        self.notify("initialized", json!({}))?;
        Ok(result)
    }

    /// Send a request and pump messages until the matching response arrives or
    /// the deadline expires.
    pub fn request(
        &mut self,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> Result<Value, LspError> {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write(&message)?;

        loop {
            let message = self.pump_one(deadline)?;
            if message.get("id").and_then(value_as_i64) == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(rpc_error(error));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            // Otherwise it was a notification or a server-initiated request,
            // already handled by pump_one; keep waiting for our response.
        }
    }

    /// Send a notification (no response expected).
    pub fn notify(&mut self, method: &str, params: Value) -> Result<(), LspError> {
        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write(&message)
    }

    /// Wait until diagnostics for `uri` have been published, or the deadline
    /// expires. Returns the diagnostics array (possibly empty) on success, or
    /// `None` if none were published before the deadline.
    pub fn wait_for_diagnostics(
        &mut self,
        uri: &str,
        deadline: Instant,
    ) -> Result<Option<Value>, LspError> {
        if let Some(found) = self.diagnostics.get(uri) {
            return Ok(Some(found.clone()));
        }
        loop {
            match self.pump_one(deadline) {
                Ok(_) => {
                    if let Some(found) = self.diagnostics.get(uri) {
                        return Ok(Some(found.clone()));
                    }
                }
                Err(LspError::Timeout) => return Ok(None),
                Err(err) => return Err(err),
            }
        }
    }

    /// Read and dispatch a single inbound message, handling notifications and
    /// server-initiated requests internally. Returns the raw message so callers
    /// can inspect responses.
    fn pump_one(&mut self, deadline: Instant) -> Result<Value, LspError> {
        let now = Instant::now();
        if now >= deadline {
            return Err(LspError::Timeout);
        }
        let event = match self.events.recv_timeout(deadline - now) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => return Err(LspError::Timeout),
            Err(RecvTimeoutError::Disconnected) => {
                self.closed = true;
                return Err(LspError::Connection("reader thread stopped".to_string()));
            }
        };

        let message = match event {
            ReaderEvent::Message(message) => message,
            ReaderEvent::Closed => {
                self.closed = true;
                return Err(LspError::Connection(
                    "server closed the connection".to_string(),
                ));
            }
        };

        self.handle_inbound(&message)?;
        Ok(message)
    }

    /// React to a parsed inbound message: stash diagnostics and answer
    /// server-initiated requests so the server does not block.
    fn handle_inbound(&mut self, message: &Value) -> Result<(), LspError> {
        let method = message.get("method").and_then(Value::as_str);
        let has_id = message.get("id").is_some();

        match (method, has_id) {
            // Notification.
            (Some("textDocument/publishDiagnostics"), false) => {
                if let Some(params) = message.get("params")
                    && let Some(uri) = params.get("uri").and_then(Value::as_str)
                {
                    self.diagnostics.insert(uri.to_string(), params.clone());
                }
            }
            (Some(_), false) => { /* other notification: ignore */ }
            // Server-initiated request: must reply.
            (Some(server_method), true) => {
                let id = message.get("id").cloned().unwrap_or(Value::Null);
                let result = self.server_request_reply(server_method, message);
                let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
                self.write(&reply)?;
            }
            // Response to one of our requests: handled by request().
            (None, _) => {}
        }
        Ok(())
    }

    /// Produce a benign reply for a server-initiated request. `workspace/configuration`
    /// must return one entry per requested item; everything else gets `null`.
    fn server_request_reply(&self, method: &str, message: &Value) -> Value {
        if method == "workspace/configuration" {
            let count = message
                .get("params")
                .and_then(|params| params.get("items"))
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or(0);
            return Value::Array(vec![Value::Null; count]);
        }
        Value::Null
    }

    fn write(&mut self, message: &Value) -> Result<(), LspError> {
        write_message(&mut self.writer, message)
            .map_err(|err| LspError::Connection(err.to_string()))
    }

    /// Best-effort graceful shutdown: `shutdown` request then `exit` notification.
    pub fn shutdown(&mut self) {
        if self.closed {
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        let _ = self.request("shutdown", Value::Null, deadline);
        let _ = self.notify("exit", Value::Null);
    }

    /// Whether the connection has been observed closed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }
}

impl Drop for Rpc {
    fn drop(&mut self) {
        // Dropping the writer/receiver lets the reader thread observe EOF.
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
    }
}

fn value_as_i64(value: &Value) -> Option<i64> {
    value.as_i64()
}

fn rpc_error(error: &Value) -> LspError {
    let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown error")
        .to_string();
    LspError::Rpc { code, message }
}

/// Upper bound on a single framed message body. A well-behaved LSP server stays
/// well under this; it exists only to cap the buffer allocated from an untrusted
/// `Content-Length` header so a bad value cannot OOM the guest.
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// Write a `Content-Length`-framed JSON-RPC message.
pub fn write_message(writer: &mut dyn Write, message: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(message)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}

/// Read a single `Content-Length`-framed JSON-RPC message. Returns `Ok(None)`
/// on a clean end of stream.
pub fn read_message(reader: &mut dyn BufRead) -> io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            // EOF. Clean only if it happens before any header bytes.
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some((name, value)) = trimmed.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse::<usize>().ok();
        }
    }

    let length = content_length.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length header")
    })?;

    // Guard against a compromised or buggy LSP server advertising an enormous
    // Content-Length: don't pre-allocate an unbounded buffer from an untrusted
    // header. Real LSP messages are far below this cap.
    if length > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Content-Length {length} exceeds maximum {MAX_MESSAGE_BYTES}"),
        ));
    }

    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body)?;
    let message = serde_json::from_slice::<Value>(&body)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    Ok(Some(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn frames_round_trip() {
        let mut buffer = Vec::new();
        let message = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
        write_message(&mut buffer, &message).unwrap();

        let text = String::from_utf8(buffer.clone()).unwrap();
        assert!(text.starts_with("Content-Length: "));
        assert!(text.contains("\r\n\r\n"));

        let mut cursor = Cursor::new(buffer);
        let parsed = read_message(&mut cursor).unwrap().unwrap();
        assert_eq!(parsed, message);
    }

    #[test]
    fn read_message_returns_none_on_clean_eof() {
        let mut cursor = Cursor::new(Vec::new());
        assert!(read_message(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn read_message_rejects_oversized_content_length() {
        // A header claiming a huge body must be refused before allocating it.
        let header = format!("Content-Length: {}\r\n\r\n", MAX_MESSAGE_BYTES + 1);
        let mut cursor = Cursor::new(header.into_bytes());
        let err = read_message(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// Spawn a fake server on a loopback socket. The closure receives the
    /// server-side stream and speaks the protocol. Returns the client `Rpc`.
    fn fake_server<F>(serve: F) -> Rpc
    where
        F: FnOnce(TcpStream) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve(stream);
        });
        let client = TcpStream::connect(addr).unwrap();
        let reader = client.try_clone().unwrap();
        Rpc::new(reader, client)
    }

    #[test]
    fn request_correlates_response_by_id() {
        let mut rpc = fake_server(|stream| {
            let mut reader = io::BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let request = read_message(&mut reader).unwrap().unwrap();
            let id = request.get("id").cloned().unwrap();
            let response = json!({"jsonrpc": "2.0", "id": id, "result": {"ok": true}});
            write_message(&mut writer, &response).unwrap();
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        let result = rpc.request("hover", json!({}), deadline).unwrap();
        assert_eq!(result, json!({"ok": true}));
    }

    #[test]
    fn request_surfaces_rpc_error() {
        let mut rpc = fake_server(|stream| {
            let mut reader = io::BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let request = read_message(&mut reader).unwrap().unwrap();
            let id = request.get("id").cloned().unwrap();
            let response = json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": "method not found"}
            });
            write_message(&mut writer, &response).unwrap();
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        let err = rpc.request("bogus", json!({}), deadline).unwrap_err();
        match err {
            LspError::Rpc { code, message } => {
                assert_eq!(code, -32601);
                assert_eq!(message, "method not found");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn answers_server_request_then_returns_response() {
        let mut rpc = fake_server(|stream| {
            let mut reader = io::BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            // Read the client request.
            let request = read_message(&mut reader).unwrap().unwrap();
            let id = request.get("id").cloned().unwrap();
            // Interleave a server-initiated configuration request.
            let server_request = json!({
                "jsonrpc": "2.0",
                "id": 9001,
                "method": "workspace/configuration",
                "params": {"items": [{}, {}]}
            });
            write_message(&mut writer, &server_request).unwrap();
            // Expect the client to answer with a 2-element array.
            let reply = read_message(&mut reader).unwrap().unwrap();
            assert_eq!(reply.get("id").and_then(Value::as_i64), Some(9001));
            assert_eq!(reply.get("result").unwrap().as_array().unwrap().len(), 2);
            // Now answer the original request.
            let response = json!({"jsonrpc": "2.0", "id": id, "result": "done"});
            write_message(&mut writer, &response).unwrap();
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        let result = rpc.request("definition", json!({}), deadline).unwrap();
        assert_eq!(result, json!("done"));
    }

    #[test]
    fn collects_diagnostics_notification() {
        let mut rpc = fake_server(|stream| {
            let mut reader = io::BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            // Consume the didOpen-style notification the client sends.
            let _ = read_message(&mut reader).unwrap().unwrap();
            let note = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {
                    "uri": "file:///workspace/src/main.rs",
                    "diagnostics": [{"message": "unused"}]
                }
            });
            write_message(&mut writer, &note).unwrap();
        });

        rpc.notify("textDocument/didOpen", json!({})).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let params = rpc
            .wait_for_diagnostics("file:///workspace/src/main.rs", deadline)
            .unwrap()
            .unwrap();
        assert_eq!(
            params.get("diagnostics").unwrap().as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn request_times_out_when_no_response() {
        let mut rpc = fake_server(|stream| {
            // Hold the connection open without responding.
            let mut reader = io::BufReader::new(stream.try_clone().unwrap());
            let _ = read_message(&mut reader);
            thread::sleep(Duration::from_millis(500));
        });

        let deadline = Instant::now() + Duration::from_millis(100);
        let err = rpc.request("hover", json!({}), deadline).unwrap_err();
        assert!(matches!(err, LspError::Timeout));
    }

    #[test]
    fn detects_closed_connection() {
        let mut rpc = fake_server(|stream| {
            // Read the request then drop the connection without replying.
            let mut reader = io::BufReader::new(stream);
            let _ = read_message(&mut reader);
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        let err = rpc.request("hover", json!({}), deadline).unwrap_err();
        assert!(
            err.is_connection_lost(),
            "expected connection-lost, got {err}"
        );
    }
}
