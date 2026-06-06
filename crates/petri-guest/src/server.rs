use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::policy::Policy;
use crate::protocol::{
    BashCommandArgs, DispatchRequest, PROTOCOL_VERSION, ResultFrame, Status, request_id_from_value,
};

pub fn serve_lines(
    reader: impl std::io::Read,
    mut writer: impl Write,
    policy: &Policy,
) -> Result<(), std::io::Error> {
    let reader = BufReader::new(reader);

    for line in reader.lines() {
        let line = line?;
        let result = handle_frame(&line, policy);
        serde_json::to_writer(&mut writer, &result)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }

    Ok(())
}

pub fn handle_frame(line: &str, policy: &Policy) -> ResultFrame {
    let started = Instant::now();
    let line = line.trim_end_matches('\r');

    if line.is_empty() {
        return ResultFrame::malformed(
            None,
            elapsed_ms(started.elapsed()),
            "malformed_frame",
            "empty dispatch frame",
        );
    }

    let value = match serde_json::from_str::<Value>(line) {
        Ok(value) if value.is_object() => value,
        Ok(_) => {
            return ResultFrame::malformed(
                None,
                elapsed_ms(started.elapsed()),
                "malformed_frame",
                "dispatch frame must be a JSON object",
            );
        }
        Err(_) => {
            return ResultFrame::malformed(
                None,
                elapsed_ms(started.elapsed()),
                "malformed_frame",
                "dispatch frame is not valid JSON",
            );
        }
    };

    let id = request_id_from_value(&value);
    let request = match serde_json::from_value::<DispatchRequest>(value.clone()) {
        Ok(request) => request,
        Err(err) => {
            return ResultFrame::rejected(
                id,
                elapsed_ms(started.elapsed()),
                "invalid_request",
                format!("invalid dispatch request: {err}"),
                None,
            );
        }
    };

    if request.id.is_empty() {
        return ResultFrame::rejected(
            None,
            elapsed_ms(started.elapsed()),
            "invalid_request",
            "request id must be non-empty",
            None,
        );
    }

    if request.protocol_version != PROTOCOL_VERSION {
        let mut details = Map::new();
        details.insert(
            "protocol_version".to_string(),
            Value::from(request.protocol_version),
        );
        return ResultFrame::rejected(
            Some(request.id),
            elapsed_ms(started.elapsed()),
            "unsupported_protocol_version",
            "unsupported protocol version",
            Some(details),
        );
    }

    if request.control.as_deref() == Some("cancel") {
        return ResultFrame::rejected(
            Some(request.id),
            elapsed_ms(started.elapsed()),
            "invalid_request",
            "cancellation is not implemented in the skeleton guest",
            None,
        );
    }

    if request.control.is_some() || request.target_id.is_some() {
        return ResultFrame::rejected(
            Some(request.id),
            elapsed_ms(started.elapsed()),
            "invalid_request",
            "unsupported control request",
            None,
        );
    }

    if let Some(limits) = &request.limits {
        if matches!(limits.timeout_ms, Some(0)) || matches!(limits.max_output_bytes, Some(0)) {
            return ResultFrame::rejected(
                Some(request.id),
                elapsed_ms(started.elapsed()),
                "invalid_request",
                "request limits must be positive",
                None,
            );
        }

        if matches!(limits.timeout_ms, Some(timeout_ms) if timeout_ms > policy.max_runtime_secs * 1000)
        {
            return policy_denied(
                request.id,
                elapsed_ms(started.elapsed()),
                "limits.timeout_ms",
                "request timeout exceeds policy runtime cap",
            );
        }

        if matches!(limits.max_output_bytes, Some(max_output_bytes) if max_output_bytes > policy.max_output_bytes)
        {
            return policy_denied(
                request.id,
                elapsed_ms(started.elapsed()),
                "limits.max_output_bytes",
                "request output cap exceeds policy output cap",
            );
        }
    }

    match request.tool.as_deref() {
        Some("bash_command") => handle_bash_command(request, policy, started),
        Some(_) => ResultFrame::rejected(
            Some(request.id),
            elapsed_ms(started.elapsed()),
            "unknown_tool",
            "unknown tool",
            None,
        ),
        None => ResultFrame::rejected(
            Some(request.id),
            elapsed_ms(started.elapsed()),
            "invalid_request",
            "tool is required",
            None,
        ),
    }
}

fn handle_bash_command(request: DispatchRequest, policy: &Policy, started: Instant) -> ResultFrame {
    let timeout = effective_timeout(&request, policy);
    let max_output_bytes = effective_output_cap(&request, policy);

    let Some(args) = request.args else {
        return ResultFrame::rejected(
            Some(request.id),
            elapsed_ms(started.elapsed()),
            "invalid_request",
            "args is required",
            None,
        );
    };

    let args = match serde_json::from_value::<BashCommandArgs>(args) {
        Ok(args) => args,
        Err(err) => {
            return ResultFrame::rejected(
                Some(request.id),
                elapsed_ms(started.elapsed()),
                "invalid_request",
                format!("invalid bash_command args: {err}"),
                None,
            );
        }
    };

    if !is_executable_name(&args.command) {
        return ResultFrame::rejected(
            Some(request.id),
            elapsed_ms(started.elapsed()),
            "invalid_request",
            "args.command must be an executable name",
            None,
        );
    }

    if !policy.allows_command(&args.command) {
        let mut details = Map::new();
        details.insert("field".to_string(), Value::from("args.command"));
        details.insert("command".to_string(), Value::from(args.command));
        return ResultFrame::rejected(
            Some(request.id),
            elapsed_ms(started.elapsed()),
            "policy_denied",
            "command is not allowed by policy",
            Some(details),
        );
    }

    let cwd = match canonical_workspace_cwd(policy, &args.cwd) {
        Ok(cwd) => cwd,
        Err(message) => {
            return policy_denied(
                request.id,
                elapsed_ms(started.elapsed()),
                "args.cwd",
                message,
            );
        }
    };

    execute_command(
        request.id,
        args.command,
        args.argv,
        cwd,
        args.env,
        args.stdin,
        timeout,
        max_output_bytes,
        started,
    )
}

fn is_executable_name(command: &str) -> bool {
    !command.is_empty()
        && !command
            .chars()
            .any(|ch| ch.is_whitespace() || matches!(ch, '/' | '\\' | '|' | '&' | ';' | '<' | '>'))
}

fn canonical_workspace_cwd(
    policy: &Policy,
    cwd: &std::path::Path,
) -> Result<std::path::PathBuf, &'static str> {
    if !cwd.is_absolute() {
        return Err("working directory must be absolute");
    }

    let workspace = fs::canonicalize(&policy.workspace_path)
        .map_err(|_| "policy workspace must exist and be accessible")?;
    let cwd =
        fs::canonicalize(cwd).map_err(|_| "working directory must exist and be accessible")?;

    if !cwd.starts_with(&workspace) {
        return Err("working directory must be inside policy workspace");
    }

    Ok(cwd)
}

fn effective_timeout(request: &DispatchRequest, policy: &Policy) -> Duration {
    let policy_timeout = Duration::from_secs(policy.max_runtime_secs);
    request
        .limits
        .as_ref()
        .and_then(|limits| limits.timeout_ms)
        .map(Duration::from_millis)
        .map(|request_timeout| request_timeout.min(policy_timeout))
        .unwrap_or(policy_timeout)
}

fn effective_output_cap(request: &DispatchRequest, policy: &Policy) -> usize {
    let policy_cap = policy.max_output_bytes as usize;
    request
        .limits
        .as_ref()
        .and_then(|limits| limits.max_output_bytes)
        .map(|request_cap| (request_cap as usize).min(policy_cap))
        .unwrap_or(policy_cap)
}

fn execute_command(
    id: String,
    command: String,
    argv: Vec<String>,
    cwd: std::path::PathBuf,
    env: std::collections::BTreeMap<String, String>,
    stdin: Option<String>,
    timeout: Duration,
    max_output_bytes: usize,
    started: Instant,
) -> ResultFrame {
    let stdin_stdio = if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    let mut child = match Command::new(&command)
        .args(argv)
        .envs(env)
        .current_dir(cwd)
        .stdin(stdin_stdio)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            return guest_error(
                id,
                elapsed_ms(started.elapsed()),
                format!("failed to start command: {err}"),
            );
        }
    };

    if let Some(input) = stdin {
        if let Some(mut child_stdin) = child.stdin.take() {
            if let Err(err) = child_stdin.write_all(input.as_bytes()) {
                let _ = child.kill();
                return guest_error(
                    id,
                    elapsed_ms(started.elapsed()),
                    format!("failed to write command stdin: {err}"),
                );
            }
        }
    }

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let output = Arc::new(Mutex::new(CapturedOutput::new(max_output_bytes)));
    let stdout_handle =
        stdout.map(|stream| spawn_output_reader(stream, StreamKind::Stdout, output.clone()));
    let stderr_handle =
        stderr.map(|stream| spawn_output_reader(stream, StreamKind::Stderr, output.clone()));

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                join_reader(stdout_handle);
                join_reader(stderr_handle);
                let output = finish_output(output);
                let result_status = if status.success() {
                    Status::Success
                } else {
                    Status::Failure
                };
                return ResultFrame::process(
                    id,
                    result_status,
                    elapsed_ms(started.elapsed()),
                    output.stdout,
                    output.stderr,
                    status.code(),
                    output.truncated,
                );
            }
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                join_reader(stdout_handle);
                join_reader(stderr_handle);
                let output = finish_output(output);
                return ResultFrame::timeout(
                    id,
                    elapsed_ms(started.elapsed()),
                    output.stdout,
                    output.stderr,
                    output.truncated,
                );
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                join_reader(stdout_handle);
                join_reader(stderr_handle);
                return guest_error(
                    id,
                    elapsed_ms(started.elapsed()),
                    format!("failed to wait for command: {err}"),
                );
            }
        }
    }
}

fn guest_error(id: String, elapsed_ms: u64, message: String) -> ResultFrame {
    ResultFrame {
        protocol_version: PROTOCOL_VERSION,
        id: Some(id),
        status: Status::Failure,
        elapsed_ms,
        stdout: Some(String::new()),
        stderr: Some(String::new()),
        exit_code: Some(None),
        output_truncated: Some(false),
        error: Some(crate::protocol::ErrorFrame {
            code: "guest_error".to_string(),
            message,
            details: None,
        }),
    }
}

#[derive(Debug, Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

#[derive(Debug)]
struct CapturedOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    retained_bytes: usize,
    max_bytes: usize,
    truncated: bool,
}

impl CapturedOutput {
    fn new(max_bytes: usize) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: Vec::new(),
            retained_bytes: 0,
            max_bytes,
            truncated: false,
        }
    }

    fn append(&mut self, kind: StreamKind, bytes: &[u8]) {
        let available = self.max_bytes.saturating_sub(self.retained_bytes);
        let keep = available.min(bytes.len());

        if keep < bytes.len() {
            self.truncated = true;
        }

        if keep == 0 {
            return;
        }

        match kind {
            StreamKind::Stdout => self.stdout.extend_from_slice(&bytes[..keep]),
            StreamKind::Stderr => self.stderr.extend_from_slice(&bytes[..keep]),
        }
        self.retained_bytes += keep;
    }
}

#[derive(Debug)]
struct FinishedOutput {
    stdout: String,
    stderr: String,
    truncated: bool,
}

fn spawn_output_reader(
    mut stream: impl Read + Send + 'static,
    kind: StreamKind,
    output: Arc<Mutex<CapturedOutput>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(bytes_read) => {
                    if let Ok(mut output) = output.lock() {
                        output.append(kind, &buffer[..bytes_read]);
                    } else {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    })
}

fn join_reader(handle: Option<thread::JoinHandle<()>>) {
    if let Some(handle) = handle {
        let _ = handle.join();
    }
}

fn finish_output(output: Arc<Mutex<CapturedOutput>>) -> FinishedOutput {
    let output = match Arc::try_unwrap(output) {
        Ok(output) => output.into_inner().unwrap_or_else(|err| err.into_inner()),
        Err(output) => {
            let mut output = output.lock().unwrap_or_else(|err| err.into_inner());
            std::mem::replace(&mut *output, CapturedOutput::new(0))
        }
    };

    FinishedOutput {
        stdout: bytes_to_string_preserving_utf8(output.stdout),
        stderr: bytes_to_string_preserving_utf8(output.stderr),
        truncated: output.truncated,
    }
}

fn bytes_to_string_preserving_utf8(mut bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(output) => output,
        Err(err) => {
            let valid_up_to = err.utf8_error().valid_up_to();
            bytes = err.into_bytes();
            bytes.truncate(valid_up_to);
            String::from_utf8(bytes).unwrap_or_default()
        }
    }
}

fn policy_denied(
    id: String,
    elapsed_ms: u64,
    field: &'static str,
    message: &'static str,
) -> ResultFrame {
    let mut details = Map::new();
    details.insert("field".to_string(), Value::from(field));
    ResultFrame::rejected(
        Some(id),
        elapsed_ms,
        "policy_denied",
        message,
        Some(details),
    )
}

fn elapsed_ms(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::policy::Policy;
    use crate::protocol::Status;

    use super::*;

    fn workspace() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("petri-guest-test-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
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
    fn executes_allowed_command_and_captures_output() {
        let workspace = workspace();
        let line = serde_json::json!({
            "protocol_version": 1,
            "id": "req-1",
            "tool": "bash_command",
            "args": {
                "command": "printf",
                "argv": ["hello"],
                "cwd": workspace.clone(),
            }
        })
        .to_string();

        let result = handle_frame(&line, &policy(&["printf"], workspace));

        assert_eq!(result.id.as_deref(), Some("req-1"));
        assert_eq!(result.status, Status::Success);
        assert_eq!(result.exit_code, Some(Some(0)));
        assert_eq!(result.stdout.as_deref(), Some("hello"));
        assert_eq!(result.stderr.as_deref(), Some(""));
        assert_eq!(result.output_truncated, Some(false));
    }

    #[test]
    fn passes_stdin_and_env_to_command() {
        let workspace = workspace();
        let line = serde_json::json!({
            "protocol_version": 1,
            "id": "req-1",
            "tool": "bash_command",
            "args": {
                "command": "sh",
                "argv": ["-c", "printf '%s:' \"$PETRI_TEST_VALUE\"; cat"],
                "cwd": workspace.clone(),
                "env": {
                    "PETRI_TEST_VALUE": "env-ok"
                },
                "stdin": "stdin-ok",
            }
        })
        .to_string();

        let result = handle_frame(&line, &policy(&["sh"], workspace));

        assert_eq!(result.status, Status::Success);
        assert_eq!(result.stdout.as_deref(), Some("env-ok:stdin-ok"));
    }

    #[test]
    fn reports_non_zero_exit_as_failure() {
        let workspace = workspace();
        let line = serde_json::json!({
            "protocol_version": 1,
            "id": "req-1",
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
    }

    #[test]
    fn ignores_unknown_request_fields() {
        let workspace = workspace();
        let line = serde_json::json!({
            "protocol_version": 1,
            "id": "req-1",
            "tool": "bash_command",
            "args": {
                "command": "printf",
                "argv": ["ok"],
                "cwd": workspace.clone(),
                "future_arg": true,
            },
            "future_field": true,
            "limits": {
                "future_limit": 1,
            }
        })
        .to_string();

        let result = handle_frame(&line, &policy(&["printf"], workspace));

        assert_eq!(result.status, Status::Success);
    }

    #[test]
    fn rejects_unknown_tool() {
        let line = r#"{"protocol_version":1,"id":"req-1","tool":"unknown","args":{}}"#;

        let result = handle_frame(line, &policy(&["printf"], workspace()));

        assert_eq!(result.status, Status::Rejected);
        assert_eq!(result.error.unwrap().code, "unknown_tool");
    }

    #[test]
    fn rejects_policy_command_violation() {
        let line = r#"{"protocol_version":1,"id":"req-1","tool":"bash_command","args":{"command":"bash","cwd":"/workspace"}}"#;

        let result = handle_frame(line, &policy(&["printf"], workspace()));

        assert_eq!(result.status, Status::Rejected);
        assert_eq!(result.error.unwrap().code, "policy_denied");
    }

    #[test]
    fn rejects_cwd_outside_workspace() {
        let workspace = workspace();
        let outside = workspace.parent().unwrap().to_path_buf();
        let line = serde_json::json!({
            "protocol_version": 1,
            "id": "req-1",
            "tool": "bash_command",
            "args": {
                "command": "printf",
                "cwd": outside,
            }
        })
        .to_string();

        let result = handle_frame(&line, &policy(&["printf"], workspace));

        assert_eq!(result.status, Status::Rejected);
        assert_eq!(result.error.unwrap().code, "policy_denied");
    }

    #[test]
    fn times_out_long_running_command() {
        let workspace = workspace();
        let line = serde_json::json!({
            "protocol_version": 1,
            "id": "req-1",
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
        assert_eq!(result.error.unwrap().code, "timeout_exceeded");
    }

    #[test]
    fn truncates_combined_output_to_effective_cap() {
        let workspace = workspace();
        let line = serde_json::json!({
            "protocol_version": 1,
            "id": "req-1",
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

    #[test]
    fn rejects_unsupported_protocol_version() {
        let line = r#"{"protocol_version":2,"id":"req-1","tool":"bash_command","args":{"command":"cargo","cwd":"/workspace"}}"#;

        let result = handle_frame(line, &policy(&["printf"], workspace()));

        assert_eq!(result.status, Status::Rejected);
        assert_eq!(result.error.unwrap().code, "unsupported_protocol_version");
    }

    #[test]
    fn reports_malformed_json() {
        let result = handle_frame(
            r#"{"protocol_version":1,"id":"req-1","tool":"#,
            &policy(&["printf"], workspace()),
        );

        assert_eq!(result.status, Status::Malformed);
        assert!(result.id.is_none());
    }

    #[test]
    fn writes_ndjson_response() {
        let workspace = workspace();
        let input = serde_json::json!({
            "protocol_version": 1,
            "id": "req-1",
            "tool": "bash_command",
            "args": {
                "command": "printf",
                "argv": ["ok"],
                "cwd": workspace.clone(),
            }
        })
        .to_string()
            + "\n";
        let mut output = Vec::new();

        serve_lines(
            input.as_bytes(),
            &mut output,
            &policy(&["printf"], workspace),
        )
        .unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.ends_with('\n'));
        assert!(output.contains(r#""id":"req-1""#));
        assert!(output.contains(r#""status":"success""#));
    }
}
