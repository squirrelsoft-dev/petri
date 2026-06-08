use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::lsp::{LspManager, LspOutcome};
use crate::netstate::ActiveNetwork;
use crate::policy::{CommandLevel, NetworkLevel, Policy};
use crate::protocol::{
    BashCommandArgs, DispatchRequest, PROTOCOL_VERSION, ResultFrame, SetModeArgs, Status,
    lsp_tools, request_id_from_value,
};

pub fn serve_lines(
    reader: impl std::io::Read,
    mut writer: impl Write,
    policy: &Policy,
    lsp: &LspManager,
    active_network: &ActiveNetwork,
) -> Result<(), std::io::Error> {
    let reader = BufReader::new(reader);
    // The active command level is per-connection mutable state: it starts at the
    // boot policy default and may move up to (never past) the policy ceiling via
    // `set_mode`. The active *network* level is VM-global instead (the nftables
    // ruleset and DNS proxy it drives are per-VM, not per-connection), so it is
    // shared in. Both change only via host control frames, never inferred from
    // workload processes. See ADR 0002.
    let mut active_command = policy.command.default;

    for line in reader.lines() {
        let line = line?;
        let result = handle_frame(&line, policy, lsp, &mut active_command, active_network);
        serde_json::to_writer(&mut writer, &result)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }

    Ok(())
}

pub fn handle_frame(
    line: &str,
    policy: &Policy,
    lsp: &LspManager,
    active_command: &mut CommandLevel,
    active_network: &ActiveNetwork,
) -> ResultFrame {
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

    if let Some(control) = request.control.as_deref() {
        return match control {
            "cancel" => ResultFrame::rejected(
                Some(request.id),
                elapsed_ms(started.elapsed()),
                "invalid_request",
                "cancellation is not implemented in the skeleton guest",
                None,
            ),
            "set_mode" => {
                handle_set_mode(request, policy, active_command, active_network, started)
            }
            _ => ResultFrame::rejected(
                Some(request.id),
                elapsed_ms(started.elapsed()),
                "invalid_request",
                "unsupported control request",
                None,
            ),
        };
    }

    if request.target_id.is_some() {
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
        Some("bash_command") => handle_bash_command(request, policy, *active_command, started),
        Some(tool) if lsp_tools::is_lsp_tool(tool) => handle_lsp(request, lsp, started),
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

fn handle_bash_command(
    request: DispatchRequest,
    policy: &Policy,
    active_command: CommandLevel,
    started: Instant,
) -> ResultFrame {
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

    if !policy.command.allows(active_command, &args.command) {
        let mut details = Map::new();
        details.insert("field".to_string(), Value::from("args.command"));
        details.insert("command".to_string(), Value::from(args.command));
        details.insert("mode".to_string(), Value::from(active_command.as_str()));
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
        policy.drop_privileges,
        started,
    )
}

/// Handle a `set_mode` control frame. Moves the active command (per-connection)
/// and/or network (VM-global) levels, rejecting any request above the relevant
/// boot policy ceiling. At least one axis must be present; an omitted axis is
/// left unchanged. Both axes are guest-enforced by design (ADR 0002): command
/// via process-launch checks, network by re-applying the in-guest nftables
/// ruleset (which the DNS proxy reads through the shared active level). Only
/// host control frames reach this path; workload processes cannot emit dispatch
/// frames.
fn handle_set_mode(
    request: DispatchRequest,
    policy: &Policy,
    active_command: &mut CommandLevel,
    active_network: &ActiveNetwork,
    started: Instant,
) -> ResultFrame {
    if request.target_id.is_some() || request.tool.is_some() {
        return ResultFrame::rejected(
            Some(request.id),
            elapsed_ms(started.elapsed()),
            "invalid_request",
            "set_mode does not take target_id or tool",
            None,
        );
    }

    let Some(args) = request.args else {
        return ResultFrame::rejected(
            Some(request.id),
            elapsed_ms(started.elapsed()),
            "invalid_request",
            "set_mode requires args",
            None,
        );
    };

    let args = match serde_json::from_value::<SetModeArgs>(args) {
        Ok(args) => args,
        Err(err) => {
            return ResultFrame::rejected(
                Some(request.id),
                elapsed_ms(started.elapsed()),
                "invalid_request",
                format!("invalid set_mode args: {err}"),
                None,
            );
        }
    };

    if args.command.is_none() && args.network.is_none() {
        return ResultFrame::rejected(
            Some(request.id),
            elapsed_ms(started.elapsed()),
            "invalid_request",
            "set_mode requires a command or network level",
            None,
        );
    }

    // Validate both axes before mutating any state, so a rejected frame leaves
    // the active levels untouched.
    let new_command = match args.command {
        Some(name) => {
            let Some(level) = CommandLevel::parse(&name) else {
                return ResultFrame::rejected(
                    Some(request.id),
                    elapsed_ms(started.elapsed()),
                    "invalid_request",
                    format!("unknown command level '{name}'"),
                    None,
                );
            };
            if level > policy.command.max {
                let mut details = Map::new();
                details.insert("field".to_string(), Value::from("mode.command"));
                details.insert("requested".to_string(), Value::from(level.as_str()));
                details.insert("max".to_string(), Value::from(policy.command.max.as_str()));
                return ResultFrame::rejected(
                    Some(request.id),
                    elapsed_ms(started.elapsed()),
                    "policy_denied",
                    "requested command level exceeds policy ceiling",
                    Some(details),
                );
            }
            Some(level)
        }
        None => None,
    };

    let new_network = match args.network {
        Some(name) => {
            let Some(level) = NetworkLevel::parse(&name) else {
                return ResultFrame::rejected(
                    Some(request.id),
                    elapsed_ms(started.elapsed()),
                    "invalid_request",
                    format!("unknown network level '{name}'"),
                    None,
                );
            };
            if level > policy.network.max {
                let mut details = Map::new();
                details.insert("field".to_string(), Value::from("mode.network"));
                details.insert("requested".to_string(), Value::from(level.as_str()));
                details.insert("max".to_string(), Value::from(policy.network.max.as_str()));
                return ResultFrame::rejected(
                    Some(request.id),
                    elapsed_ms(started.elapsed()),
                    "policy_denied",
                    "requested network level exceeds policy ceiling",
                    Some(details),
                );
            }
            Some(level)
        }
        None => None,
    };

    // Re-apply the nftables ruleset for the new network level first: it is the
    // only effectful step and can fail. Doing it before committing the active
    // levels keeps a failed transition fail-closed — the prior ruleset and the
    // reported active levels stay in agreement.
    if let Some(level) = new_network {
        #[cfg(target_os = "linux")]
        if let Err(err) = crate::netfilter::apply(&policy.network, level) {
            return ResultFrame::rejected(
                Some(request.id),
                elapsed_ms(started.elapsed()),
                "internal_error",
                format!("failed to apply network level: {err}"),
                None,
            );
        }
        // On the host build there is no kernel netfilter to program; the active
        // level is still tracked so the host build can exercise this path.
        #[cfg(not(target_os = "linux"))]
        let _ = level;
    }

    let mut mode = Map::new();
    if let Some(level) = new_command {
        *active_command = level;
        mode.insert("command".to_string(), Value::from(level.as_str()));
    }
    if let Some(level) = new_network {
        active_network.set(level);
        mode.insert("network".to_string(), Value::from(level.as_str()));
    }

    ResultFrame::data(
        request.id,
        elapsed_ms(started.elapsed()),
        json!({ "mode": mode }),
    )
}

fn handle_lsp(request: DispatchRequest, lsp: &LspManager, started: Instant) -> ResultFrame {
    let tool = request.tool.as_deref().unwrap_or_default();
    let outcome = lsp.dispatch(tool, request.args.as_ref());
    let elapsed = elapsed_ms(started.elapsed());

    match outcome {
        LspOutcome::Available(result) => ResultFrame::data(
            request.id,
            elapsed,
            json!({ "available": true, "result": result }),
        ),
        LspOutcome::Unavailable { language, reason } => ResultFrame::data(
            request.id,
            elapsed,
            json!({ "available": false, "language": language, "reason": reason }),
        ),
        LspOutcome::Rejected {
            code,
            field,
            message,
        } => {
            let mut details = Map::new();
            details.insert("field".to_string(), Value::from(field));
            ResultFrame::rejected(Some(request.id), elapsed, code, message, Some(details))
        }
        LspOutcome::Failed(message) => guest_error(request.id, elapsed, message),
    }
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
    drop_privileges: bool,
    started: Instant,
) -> ResultFrame {
    let stdin_stdio = if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    let mut cmd = Command::new(&command);
    cmd.args(argv)
        .envs(env)
        .current_dir(cwd)
        .stdin(stdin_stdio)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Workload processes run as an unprivileged user so they hold no
    // capabilities (no CAP_NET_ADMIN to touch nftables, etc.) and are confined
    // to the workspace they own. The guest agent itself stays root. Trusted
    // provisioning contexts (the image builder) disable this via policy so their
    // commands keep root. See ADR 0002.
    configure_privilege_drop(&mut cmd, drop_privileges);
    let mut child = match cmd.spawn() {
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

/// Unprivileged user the guest drops workload processes to. Must match the
/// `agent` user created in the VM image (`scripts/build-base-image.sh`).
#[cfg(target_os = "linux")]
const AGENT_UID: libc::uid_t = 1000;
#[cfg(target_os = "linux")]
const AGENT_GID: libc::gid_t = 1000;

/// Arrange for a spawned workload process to drop to the unprivileged `agent`
/// user before it execs. Only applies when the guest agent runs as root (the
/// production case — the systemd unit runs `petri-guest` as root); when not root
/// there is nothing to drop and we leave the child as-is so dev/test runs work.
#[cfg(target_os = "linux")]
fn configure_privilege_drop(cmd: &mut Command, drop_privileges: bool) {
    use std::os::unix::process::CommandExt;

    // Policy may disable the drop for trusted provisioning (the image builder),
    // whose commands must keep the guest agent's privileges. SAFETY: geteuid is
    // async-signal-safe and has no preconditions. Nothing to drop unless root.
    if !drop_privileges || unsafe { libc::geteuid() } != 0 {
        return;
    }

    // SAFETY: the closure runs in the forked child between fork and exec and
    // performs only async-signal-safe syscalls (no allocation, no locks). Order
    // is load-bearing: clear supplementary groups, then gid, then uid — once the
    // uid is dropped the process can no longer call setgroups/setgid. std's
    // CommandExt::uid/gid do NOT clear supplementary groups, so we must.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setgroups(0, std::ptr::null()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::setgid(AGENT_GID) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::setuid(AGENT_UID) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Non-Linux builds (dev/test on macOS) run workloads in-process without a
/// privilege drop; the production guest is always Linux.
#[cfg(not(target_os = "linux"))]
fn configure_privilege_drop(_cmd: &mut Command, _drop_privileges: bool) {}

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
        data: None,
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

    /// Build a policy whose default level (`edit`) allows exactly the given
    /// commands, with the ceiling at `yolo` so escalation tests can climb.
    fn policy(allowed_commands: &[&str], workspace_path: PathBuf) -> Policy {
        Policy {
            network_enabled: false,
            network: crate::policy::NetworkPolicy::disabled(),
            command: crate::policy::CommandPolicy {
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

    fn test_lsp() -> LspManager {
        LspManager::new(crate::lsp::LspConfig::disabled(), std::env::temp_dir())
    }

    /// Test wrapper that supplies a disabled LSP manager and starts at the
    /// policy's default command level.
    fn handle(line: &str, policy: &Policy) -> ResultFrame {
        let mut active = policy.command.default;
        let net = ActiveNetwork::new(policy.network.default);
        handle_frame(line, policy, &test_lsp(), &mut active, &net)
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

        let result = handle(&line, &policy(&["printf"], workspace));

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

        let result = handle(&line, &policy(&["sh"], workspace));

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

        let result = handle(&line, &policy(&["false"], workspace));

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

        let result = handle(&line, &policy(&["printf"], workspace));

        assert_eq!(result.status, Status::Success);
    }

    #[test]
    fn rejects_unknown_tool() {
        let line = r#"{"protocol_version":1,"id":"req-1","tool":"unknown","args":{}}"#;

        let result = handle(line, &policy(&["printf"], workspace()));

        assert_eq!(result.status, Status::Rejected);
        assert_eq!(result.error.unwrap().code, "unknown_tool");
    }

    #[test]
    fn rejects_policy_command_violation() {
        let line = r#"{"protocol_version":1,"id":"req-1","tool":"bash_command","args":{"command":"bash","cwd":"/workspace"}}"#;

        let result = handle(line, &policy(&["printf"], workspace()));

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

        let result = handle(&line, &policy(&["printf"], workspace));

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

        let result = handle(&line, &policy(&["sleep"], workspace));

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

        let result = handle(&line, &policy(&["printf"], workspace));

        assert_eq!(result.status, Status::Success);
        assert_eq!(result.stdout.as_deref(), Some("abcd"));
        assert_eq!(result.output_truncated, Some(true));
    }

    #[test]
    fn rejects_unsupported_protocol_version() {
        let line = r#"{"protocol_version":2,"id":"req-1","tool":"bash_command","args":{"command":"cargo","cwd":"/workspace"}}"#;

        let result = handle(line, &policy(&["printf"], workspace()));

        assert_eq!(result.status, Status::Rejected);
        assert_eq!(result.error.unwrap().code, "unsupported_protocol_version");
    }

    #[test]
    fn reports_malformed_json() {
        let result = handle(
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
        let policy = policy(&["printf"], workspace);
        let active_network = ActiveNetwork::new(policy.network.default);

        serve_lines(
            input.as_bytes(),
            &mut output,
            &policy,
            &test_lsp(),
            &active_network,
        )
        .unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.ends_with('\n'));
        assert!(output.contains(r#""id":"req-1""#));
        assert!(output.contains(r#""status":"success""#));
    }

    #[test]
    fn lsp_request_degrades_gracefully_when_disabled() {
        let line = r#"{"protocol_version":1,"id":"req-1","tool":"lsp_hover","args":{"file":"/workspace/src/main.rs","line":1,"col":1}}"#;

        let result = handle(line, &policy(&["printf"], workspace()));

        assert_eq!(result.status, Status::Success);
        let data = result.data.expect("lsp result carries data");
        assert_eq!(data["available"], serde_json::Value::Bool(false));
        assert!(result.stdout.is_none());
    }

    #[test]
    fn unknown_lsp_tool_is_rejected_as_unknown_tool() {
        // A tool that looks lsp-ish but is not in the surface is unknown.
        let line = r#"{"protocol_version":1,"id":"req-1","tool":"lsp_format","args":{}}"#;

        let result = handle(line, &policy(&["printf"], workspace()));

        assert_eq!(result.status, Status::Rejected);
        assert_eq!(result.error.unwrap().code, "unknown_tool");
    }

    #[test]
    fn set_mode_escalates_within_ceiling_and_changes_command_authority() {
        let workspace = workspace();
        let policy = Policy {
            network_enabled: false,
            network: crate::policy::NetworkPolicy::disabled(),
            command: crate::policy::CommandPolicy {
                default: CommandLevel::ReadOnly,
                max: CommandLevel::Yolo,
                read_only: ["true"].into_iter().map(str::to_string).collect(),
                edit: ["printf"].into_iter().map(str::to_string).collect(),
            },
            max_runtime_secs: 60,
            max_output_bytes: 1024,
            workspace_path: workspace.clone(),
            drop_privileges: false,
        };
        let lsp = test_lsp();
        let mut active = policy.command.default;
        let net = ActiveNetwork::new(policy.network.default);

        // printf is an `edit`-tier command; at the default `read_only` it is denied.
        let printf = serde_json::json!({
            "protocol_version": 1,
            "id": "r1",
            "tool": "bash_command",
            "args": { "command": "printf", "argv": ["hi"], "cwd": workspace.clone() }
        })
        .to_string();
        let denied = handle_frame(&printf, &policy, &lsp, &mut active, &net);
        assert_eq!(denied.status, Status::Rejected);
        assert_eq!(denied.error.unwrap().code, "policy_denied");

        // Escalate to `edit`, which is within the `yolo` ceiling.
        let set =
            r#"{"protocol_version":1,"id":"m1","control":"set_mode","args":{"command":"edit"}}"#;
        let switched = handle_frame(set, &policy, &lsp, &mut active, &net);
        assert_eq!(switched.status, Status::Success);
        assert_eq!(
            switched.data.unwrap()["mode"]["command"],
            Value::from("edit")
        );
        assert_eq!(active, CommandLevel::Edit);

        // The same command now runs.
        let ok = handle_frame(&printf, &policy, &lsp, &mut active, &net);
        assert_eq!(ok.status, Status::Success);
        assert_eq!(ok.stdout.as_deref(), Some("hi"));
    }

    #[test]
    fn set_mode_rejects_level_above_ceiling() {
        let policy = Policy {
            network_enabled: false,
            network: crate::policy::NetworkPolicy::disabled(),
            command: crate::policy::CommandPolicy {
                default: CommandLevel::ReadOnly,
                max: CommandLevel::Edit,
                read_only: std::collections::HashSet::new(),
                edit: std::collections::HashSet::new(),
            },
            max_runtime_secs: 60,
            max_output_bytes: 1024,
            workspace_path: workspace(),
            drop_privileges: false,
        };
        let mut active = policy.command.default;
        let net = ActiveNetwork::new(policy.network.default);

        let set =
            r#"{"protocol_version":1,"id":"m1","control":"set_mode","args":{"command":"yolo"}}"#;
        let result = handle_frame(set, &policy, &test_lsp(), &mut active, &net);

        assert_eq!(result.status, Status::Rejected);
        assert_eq!(result.error.unwrap().code, "policy_denied");
        // The active level is unchanged after a rejected escalation.
        assert_eq!(active, CommandLevel::ReadOnly);
    }

    /// A policy whose network axis defaults to `none` but permits escalation up
    /// to `max`. The host build does not touch nftables (apply is Linux-only),
    /// so these tests exercise validation and the active-level bookkeeping only.
    fn network_policy(max: NetworkLevel) -> Policy {
        let mut policy = policy(&["printf"], workspace());
        policy.network = crate::policy::NetworkPolicy {
            default: NetworkLevel::None,
            max,
            allowlist: Vec::new(),
        };
        policy
    }

    #[test]
    fn set_mode_escalates_network_within_ceiling() {
        let policy = network_policy(NetworkLevel::Full);
        let mut active = policy.command.default;
        let net = ActiveNetwork::new(policy.network.default);
        assert_eq!(net.get(), NetworkLevel::None);

        let set =
            r#"{"protocol_version":1,"id":"m1","control":"set_mode","args":{"network":"full"}}"#;
        let result = handle_frame(set, &policy, &test_lsp(), &mut active, &net);

        assert_eq!(result.status, Status::Success);
        assert_eq!(result.data.unwrap()["mode"]["network"], Value::from("full"));
        assert_eq!(net.get(), NetworkLevel::Full);
    }

    #[test]
    fn set_mode_rejects_network_above_ceiling() {
        let policy = network_policy(NetworkLevel::Allowlist);
        let mut active = policy.command.default;
        let net = ActiveNetwork::new(policy.network.default);

        let set =
            r#"{"protocol_version":1,"id":"m1","control":"set_mode","args":{"network":"full"}}"#;
        let result = handle_frame(set, &policy, &test_lsp(), &mut active, &net);

        assert_eq!(result.status, Status::Rejected);
        let err = result.error.unwrap();
        assert_eq!(err.code, "policy_denied");
        // The active level is unchanged after a rejected escalation.
        assert_eq!(net.get(), NetworkLevel::None);
    }

    #[test]
    fn set_mode_sets_both_axes_in_one_frame() {
        let mut policy = network_policy(NetworkLevel::Full);
        policy.command.max = CommandLevel::Yolo;
        let mut active = policy.command.default;
        let net = ActiveNetwork::new(policy.network.default);

        let set = r#"{"protocol_version":1,"id":"m1","control":"set_mode","args":{"command":"edit","network":"full"}}"#;
        let result = handle_frame(set, &policy, &test_lsp(), &mut active, &net);

        assert_eq!(result.status, Status::Success);
        let mode = result.data.unwrap();
        assert_eq!(mode["mode"]["command"], Value::from("edit"));
        assert_eq!(mode["mode"]["network"], Value::from("full"));
        assert_eq!(active, CommandLevel::Edit);
        assert_eq!(net.get(), NetworkLevel::Full);
    }

    #[test]
    fn set_mode_requires_at_least_one_axis() {
        let policy = policy(&["printf"], workspace());
        let mut active = policy.command.default;
        let net = ActiveNetwork::new(policy.network.default);

        let set = r#"{"protocol_version":1,"id":"m1","control":"set_mode","args":{}}"#;
        let result = handle_frame(set, &policy, &test_lsp(), &mut active, &net);

        assert_eq!(result.status, Status::Rejected);
        assert_eq!(result.error.unwrap().code, "invalid_request");
    }

    #[test]
    fn set_mode_rejects_unknown_network_level() {
        let policy = network_policy(NetworkLevel::Full);
        let mut active = policy.command.default;
        let net = ActiveNetwork::new(policy.network.default);

        let set = r#"{"protocol_version":1,"id":"m1","control":"set_mode","args":{"network":"wide_open"}}"#;
        let result = handle_frame(set, &policy, &test_lsp(), &mut active, &net);

        assert_eq!(result.status, Status::Rejected);
        assert_eq!(result.error.unwrap().code, "invalid_request");
    }

    #[test]
    fn set_mode_rejects_unknown_level() {
        let policy = policy(&["printf"], workspace());
        let mut active = policy.command.default;
        let net = ActiveNetwork::new(policy.network.default);

        let set = r#"{"protocol_version":1,"id":"m1","control":"set_mode","args":{"command":"superuser"}}"#;
        let result = handle_frame(set, &policy, &test_lsp(), &mut active, &net);

        assert_eq!(result.status, Status::Rejected);
        assert_eq!(result.error.unwrap().code, "invalid_request");
    }
}
