use std::ffi::OsString;
use std::path::PathBuf;

use crate::backend::HostBackend;
use crate::dispatch::{DispatchRequest, RequestLimits};
use crate::error::{PetriError, Result};
use crate::instance::{InstanceConfig, InstanceId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Create(CreateCommand),
    Dispatch(DispatchCommand),
    Stop(InstanceCommand),
    Teardown(InstanceCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateCommand {
    pub config: InstanceConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchCommand {
    pub instance_id: InstanceId,
    pub request: DispatchRequest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceCommand {
    pub instance_id: InstanceId,
}

pub fn run(args: impl IntoIterator<Item = OsString>, backend: &impl HostBackend) -> Result<String> {
    match parse(args)? {
        Command::Create(command) => {
            let handle = backend.create(command.config)?;
            Ok(format!(
                "created instance {} with backend {}",
                handle.id, handle.backend
            ))
        }
        Command::Dispatch(command) => {
            let result = backend.dispatch(&command.instance_id, command.request)?;
            serde_json::to_string(&result).map_err(|err| PetriError::Cli(err.to_string()))
        }
        Command::Stop(command) => {
            backend.stop(&command.instance_id)?;
            Ok(format!("stopped instance {}", command.instance_id))
        }
        Command::Teardown(command) => {
            backend.teardown(&command.instance_id)?;
            Ok(format!("tore down instance {}", command.instance_id))
        }
    }
}

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command> {
    let mut args = args
        .into_iter()
        .map(|arg| arg.to_string_lossy().into_owned());
    let Some(command) = args.next() else {
        return Err(PetriError::Cli(usage()));
    };

    match command.as_str() {
        "create" => parse_create(args),
        "dispatch" => parse_dispatch(args),
        "stop" => parse_instance_command(args, CommandKind::Stop),
        "teardown" => parse_instance_command(args, CommandKind::Teardown),
        "--help" | "-h" | "help" => Err(PetriError::Cli(usage())),
        _ => Err(PetriError::Cli(format!(
            "unknown command '{command}'\n{}",
            usage()
        ))),
    }
}

fn parse_create(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let mut id = None;
    let mut backend = "macos".to_string();
    let mut image = None;
    let mut workspace = None;
    let mut policy = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--id" => id = Some(InstanceId::new(next_arg(&mut args, "--id")?)?),
            "--backend" => backend = next_arg(&mut args, "--backend")?,
            "--image" => image = Some(PathBuf::from(next_arg(&mut args, "--image")?)),
            "--workspace" => workspace = Some(PathBuf::from(next_arg(&mut args, "--workspace")?)),
            "--policy" => policy = Some(PathBuf::from(next_arg(&mut args, "--policy")?)),
            "--help" | "-h" => return Err(PetriError::Cli(create_usage())),
            _ => return Err(PetriError::Cli(format!("unknown create argument '{arg}'"))),
        }
    }

    let id = id.ok_or(PetriError::MissingArgument { flag: "--id" })?;
    let workspace = workspace.ok_or(PetriError::MissingArgument {
        flag: "--workspace",
    })?;
    let policy = policy.ok_or(PetriError::MissingArgument { flag: "--policy" })?;
    let mut config = InstanceConfig::new(id, backend, workspace, policy);

    if let Some(image) = image {
        config = config.with_image(image);
    }

    Ok(Command::Create(CreateCommand { config }))
}

fn parse_dispatch(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let mut instance_id = None;
    let mut request_id = "dispatch-1".to_string();
    let mut tool = "bash_command".to_string();
    let mut command = None;
    let mut argv = Vec::new();
    let mut cwd = None;
    let mut timeout_ms = None;
    let mut max_output_bytes = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--id" => instance_id = Some(InstanceId::new(next_arg(&mut args, "--id")?)?),
            "--request-id" => request_id = next_arg(&mut args, "--request-id")?,
            "--tool" => tool = next_arg(&mut args, "--tool")?,
            "--command" => command = Some(next_arg(&mut args, "--command")?),
            "--arg" => argv.push(next_arg(&mut args, "--arg")?),
            "--cwd" => cwd = Some(PathBuf::from(next_arg(&mut args, "--cwd")?)),
            "--timeout-ms" => {
                timeout_ms = Some(parse_u64(
                    next_arg(&mut args, "--timeout-ms")?,
                    "--timeout-ms",
                )?)
            }
            "--max-output-bytes" => {
                max_output_bytes = Some(parse_u64(
                    next_arg(&mut args, "--max-output-bytes")?,
                    "--max-output-bytes",
                )?)
            }
            "--help" | "-h" => return Err(PetriError::Cli(dispatch_usage())),
            _ => {
                return Err(PetriError::Cli(format!(
                    "unknown dispatch argument '{arg}'"
                )));
            }
        }
    }

    if tool != "bash_command" {
        return Err(PetriError::InvalidArgument {
            flag: "--tool",
            value: tool,
            message: "only bash_command is defined by protocol version 1".to_string(),
        });
    }

    let instance_id = instance_id.ok_or(PetriError::MissingArgument { flag: "--id" })?;
    let command = command.ok_or(PetriError::MissingArgument { flag: "--command" })?;
    let cwd = cwd.ok_or(PetriError::MissingArgument { flag: "--cwd" })?;
    let limits = (timeout_ms.is_some() || max_output_bytes.is_some()).then_some(RequestLimits {
        timeout_ms,
        max_output_bytes,
    });
    let request = DispatchRequest::bash_command(request_id, command, argv, cwd, limits);

    Ok(Command::Dispatch(DispatchCommand {
        instance_id,
        request,
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandKind {
    Stop,
    Teardown,
}

fn parse_instance_command(
    mut args: impl Iterator<Item = String>,
    kind: CommandKind,
) -> Result<Command> {
    let mut id = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--id" => id = Some(InstanceId::new(next_arg(&mut args, "--id")?)?),
            "--help" | "-h" => {
                return Err(PetriError::Cli(match kind {
                    CommandKind::Stop => stop_usage(),
                    CommandKind::Teardown => teardown_usage(),
                }));
            }
            _ => return Err(PetriError::Cli(format!("unknown argument '{arg}'"))),
        }
    }

    let instance_id = id.ok_or(PetriError::MissingArgument { flag: "--id" })?;
    let command = InstanceCommand { instance_id };

    Ok(match kind {
        CommandKind::Stop => Command::Stop(command),
        CommandKind::Teardown => Command::Teardown(command),
    })
}

fn next_arg(args: &mut impl Iterator<Item = String>, flag: &'static str) -> Result<String> {
    args.next().ok_or(PetriError::MissingArgument { flag })
}

fn parse_u64(value: String, flag: &'static str) -> Result<u64> {
    value
        .parse::<u64>()
        .map_err(|_| PetriError::InvalidArgument {
            flag,
            value,
            message: "expected a positive integer".to_string(),
        })
        .and_then(|value| {
            if value == 0 {
                Err(PetriError::InvalidArgument {
                    flag,
                    value: value.to_string(),
                    message: "expected a positive integer".to_string(),
                })
            } else {
                Ok(value)
            }
        })
}

pub fn usage() -> String {
    [
        "usage: petri <command> [options]",
        "",
        "commands:",
        "  create    create a configured Petri instance",
        "  dispatch  send a protocol v1 dispatch request",
        "  stop      stop a running instance",
        "  teardown  remove instance runtime state",
        "",
        &create_usage(),
        &dispatch_usage(),
        &stop_usage(),
        &teardown_usage(),
    ]
    .join("\n")
}

fn create_usage() -> String {
    "usage: petri create --id <id> --workspace <path> --policy <path> [--image <path>] [--backend macos|stub]".to_string()
}

fn dispatch_usage() -> String {
    "usage: petri dispatch --id <id> --command <name> --cwd <path> [--request-id <id>] [--arg <value>]... [--timeout-ms <ms>] [--max-output-bytes <bytes>]".to_string()
}

fn stop_usage() -> String {
    "usage: petri stop --id <id>".to_string()
}

fn teardown_usage() -> String {
    "usage: petri teardown --id <id>".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_create_command() {
        let command = parse(args(&[
            "create",
            "--id",
            "dev_1",
            "--workspace",
            "/workspace",
            "--policy",
            "policy.toml",
            "--image",
            "base.img",
        ]))
        .unwrap();

        let Command::Create(command) = command else {
            panic!("expected create command");
        };

        assert_eq!(command.config.id.as_str(), "dev_1");
        assert_eq!(command.config.workspace, PathBuf::from("/workspace"));
        assert_eq!(command.config.policy, PathBuf::from("policy.toml"));
        assert_eq!(command.config.image, Some(PathBuf::from("base.img")));
    }

    #[test]
    fn parses_dispatch_command_with_limits() {
        let command = parse(args(&[
            "dispatch",
            "--id",
            "dev-1",
            "--request-id",
            "req-1",
            "--command",
            "cargo",
            "--arg",
            "test",
            "--cwd",
            "/workspace",
            "--timeout-ms",
            "1000",
        ]))
        .unwrap();

        let Command::Dispatch(command) = command else {
            panic!("expected dispatch command");
        };

        assert_eq!(command.instance_id.as_str(), "dev-1");
        assert_eq!(command.request.id, "req-1");
        assert_eq!(command.request.tool, "bash_command");
        assert_eq!(command.request.limits.unwrap().timeout_ms, Some(1000));
    }

    #[test]
    fn rejects_unknown_tool() {
        let err = parse(args(&[
            "dispatch",
            "--id",
            "dev-1",
            "--tool",
            "unknown",
            "--command",
            "cargo",
            "--cwd",
            "/workspace",
        ]))
        .unwrap_err()
        .to_string();

        assert!(err.contains("only bash_command"));
    }
}
