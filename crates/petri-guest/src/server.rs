use std::io::{BufRead, BufReader, Write};
use std::time::Instant;

use serde_json::{Map, Value};

use crate::policy::Policy;
use crate::protocol::{
    BashCommandArgs, DispatchRequest, PROTOCOL_VERSION, ResultFrame, request_id_from_value,
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
            started.elapsed().as_millis(),
            "malformed_frame",
            "empty dispatch frame",
        );
    }

    let value = match serde_json::from_str::<Value>(line) {
        Ok(value) if value.is_object() => value,
        Ok(_) => {
            return ResultFrame::malformed(
                None,
                started.elapsed().as_millis(),
                "malformed_frame",
                "dispatch frame must be a JSON object",
            );
        }
        Err(_) => {
            return ResultFrame::malformed(
                None,
                started.elapsed().as_millis(),
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
                started.elapsed().as_millis(),
                "invalid_request",
                format!("invalid dispatch request: {err}"),
                None,
            );
        }
    };

    if request.id.is_empty() {
        return ResultFrame::rejected(
            None,
            started.elapsed().as_millis(),
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
            started.elapsed().as_millis(),
            "unsupported_protocol_version",
            "unsupported protocol version",
            Some(details),
        );
    }

    if request.control.as_deref() == Some("cancel") {
        return ResultFrame::rejected(
            Some(request.id),
            started.elapsed().as_millis(),
            "invalid_request",
            "cancellation is not implemented in the skeleton guest",
            None,
        );
    }

    if request.control.is_some() || request.target_id.is_some() {
        return ResultFrame::rejected(
            Some(request.id),
            started.elapsed().as_millis(),
            "invalid_request",
            "unsupported control request",
            None,
        );
    }

    if let Some(limits) = &request.limits {
        if matches!(limits.timeout_ms, Some(0)) || matches!(limits.max_output_bytes, Some(0)) {
            return ResultFrame::rejected(
                Some(request.id),
                started.elapsed().as_millis(),
                "invalid_request",
                "request limits must be positive",
                None,
            );
        }

        if matches!(limits.timeout_ms, Some(timeout_ms) if timeout_ms > policy.max_runtime_secs * 1000)
        {
            return policy_denied(
                request.id,
                started.elapsed().as_millis(),
                "limits.timeout_ms",
                "request timeout exceeds policy runtime cap",
            );
        }

        if matches!(limits.max_output_bytes, Some(max_output_bytes) if max_output_bytes > policy.max_output_bytes)
        {
            return policy_denied(
                request.id,
                started.elapsed().as_millis(),
                "limits.max_output_bytes",
                "request output cap exceeds policy output cap",
            );
        }
    }

    match request.tool.as_deref() {
        Some("bash_command") => handle_bash_command(request, policy, started),
        Some(_) => ResultFrame::rejected(
            Some(request.id),
            started.elapsed().as_millis(),
            "unknown_tool",
            "unknown tool",
            None,
        ),
        None => ResultFrame::rejected(
            Some(request.id),
            started.elapsed().as_millis(),
            "invalid_request",
            "tool is required",
            None,
        ),
    }
}

fn handle_bash_command(request: DispatchRequest, policy: &Policy, started: Instant) -> ResultFrame {
    let Some(args) = request.args else {
        return ResultFrame::rejected(
            Some(request.id),
            started.elapsed().as_millis(),
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
                started.elapsed().as_millis(),
                "invalid_request",
                format!("invalid bash_command args: {err}"),
                None,
            );
        }
    };

    if args.command.is_empty() || args.command.chars().any(char::is_whitespace) {
        return ResultFrame::rejected(
            Some(request.id),
            started.elapsed().as_millis(),
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
            started.elapsed().as_millis(),
            "policy_denied",
            "command is not allowed by policy",
            Some(details),
        );
    }

    if !policy.cwd_is_in_workspace(&args.cwd) {
        return policy_denied(
            request.id,
            started.elapsed().as_millis(),
            "args.cwd",
            "working directory must be inside policy workspace",
        );
    }

    let _argv = args.argv;
    ResultFrame::placeholder_success(request.id, started.elapsed().as_millis())
}

fn policy_denied(
    id: String,
    elapsed_ms: u128,
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::policy::Policy;
    use crate::protocol::Status;

    use super::*;

    fn policy() -> Policy {
        Policy {
            network_enabled: false,
            allowed_commands: ["cargo".to_string()].into_iter().collect(),
            max_runtime_secs: 60,
            max_output_bytes: 1024,
            workspace_path: PathBuf::from("/workspace"),
        }
    }

    #[test]
    fn accepts_placeholder_bash_command() {
        let line = r#"{"protocol_version":1,"id":"req-1","tool":"bash_command","args":{"command":"cargo","argv":["test"],"cwd":"/workspace"}}"#;

        let result = handle_frame(line, &policy());

        assert_eq!(result.id.as_deref(), Some("req-1"));
        assert_eq!(result.status, Status::Success);
        assert_eq!(result.exit_code, Some(Some(0)));
    }

    #[test]
    fn ignores_unknown_request_fields() {
        let line = r#"{"protocol_version":1,"id":"req-1","tool":"bash_command","args":{"command":"cargo","cwd":"/workspace","future_arg":true},"future_field":true,"limits":{"future_limit":1}}"#;

        let result = handle_frame(line, &policy());

        assert_eq!(result.status, Status::Success);
    }

    #[test]
    fn rejects_unknown_tool() {
        let line = r#"{"protocol_version":1,"id":"req-1","tool":"unknown","args":{}}"#;

        let result = handle_frame(line, &policy());

        assert_eq!(result.status, Status::Rejected);
        assert_eq!(result.error.unwrap().code, "unknown_tool");
    }

    #[test]
    fn rejects_policy_command_violation() {
        let line = r#"{"protocol_version":1,"id":"req-1","tool":"bash_command","args":{"command":"bash","cwd":"/workspace"}}"#;

        let result = handle_frame(line, &policy());

        assert_eq!(result.status, Status::Rejected);
        assert_eq!(result.error.unwrap().code, "policy_denied");
    }

    #[test]
    fn rejects_unsupported_protocol_version() {
        let line = r#"{"protocol_version":2,"id":"req-1","tool":"bash_command","args":{"command":"cargo","cwd":"/workspace"}}"#;

        let result = handle_frame(line, &policy());

        assert_eq!(result.status, Status::Rejected);
        assert_eq!(result.error.unwrap().code, "unsupported_protocol_version");
    }

    #[test]
    fn reports_malformed_json() {
        let result = handle_frame(r#"{"protocol_version":1,"id":"req-1","tool":"#, &policy());

        assert_eq!(result.status, Status::Malformed);
        assert!(result.id.is_none());
    }

    #[test]
    fn writes_ndjson_response() {
        let input = br#"{"protocol_version":1,"id":"req-1","tool":"bash_command","args":{"command":"cargo","cwd":"/workspace"}}
"#;
        let mut output = Vec::new();

        serve_lines(&input[..], &mut output, &policy()).unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.ends_with('\n'));
        assert!(output.contains(r#""id":"req-1""#));
        assert!(output.contains(r#""status":"success""#));
    }
}
