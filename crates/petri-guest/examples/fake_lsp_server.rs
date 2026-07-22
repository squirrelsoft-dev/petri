//! A minimal fake language server used by integration tests.
//!
//! Speaks the LSP base protocol (`Content-Length`-framed JSON-RPC) well enough
//! to exercise the guest's [`petri_guest::lsp`] client and manager without a
//! real toolchain. Behavior is driven by a single argument: a "state file"
//! path used to simulate a one-time crash across restarts.
//!
//! - `initialize` is always answered with empty capabilities.
//! - `shutdown` is answered with `null`; `exit` terminates the process.
//! - The first tool request (e.g. `textDocument/hover`): if the state file does
//!   not yet exist, the server creates it and exits without responding
//!   (simulating a crash mid-request). On a later process (state file present)
//!   it answers normally, proving the manager restarted and retried.
//! - If the state file already exists at startup, the server never crashes.

use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::process::exit;

use serde_json::{Value, json};

fn main() {
    let state_file = env::args().nth(1);
    let stdin = io::stdin();
    let mut reader = io::BufReader::new(stdin.lock());
    let stdout = io::stdout();
    let mut writer = stdout.lock();

    loop {
        let message = match read_message(&mut reader) {
            Ok(Some(message)) => message,
            _ => return,
        };

        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").cloned();

        match (method, id) {
            (Some("initialize"), Some(id)) => {
                respond(&mut writer, id, json!({ "capabilities": {} }));
            }
            (Some("shutdown"), Some(id)) => {
                respond(&mut writer, id, Value::Null);
            }
            (Some("exit"), _) => return,
            // Notifications and anything unrecognized: ignore.
            (Some(_), None) | (None, _) => {}
            // Any other tool request.
            (Some(other), Some(id)) => {
                if let Some(path) = &state_file
                    && fs::metadata(path).is_err()
                {
                    // First life: mark the crash and die without responding.
                    let _ = fs::write(path, "crashed");
                    exit(1);
                }
                respond(
                    &mut writer,
                    id,
                    json!({ "method": other, "recovered": true }),
                );
            }
        }
    }
}

fn respond(writer: &mut impl Write, id: Value, result: Value) {
    let message = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    let body = serde_json::to_vec(&message).unwrap();
    write!(writer, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
    writer.write_all(&body).unwrap();
    writer.flush().unwrap();
}

fn read_message(reader: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().ok();
        }
    }
    let length = match content_length {
        Some(length) => length,
        None => return Ok(None),
    };
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body).unwrap()))
}
