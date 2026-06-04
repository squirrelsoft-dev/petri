use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;

use crate::backend::HostBackend;
use crate::dispatch::{DispatchRequest, RequestLimits};
use crate::error::{PetriError, Result};
use crate::instance::{InstanceConfig, InstanceId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Create(CreateCommand),
    Dispatch(DispatchCommand),
    ImageBuild(ImageBuildCommand),
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
pub struct ImageBuildCommand {
    pub config: Option<PathBuf>,
    pub out_dir: Option<PathBuf>,
    pub arch: Option<String>,
    pub debian_arch: Option<String>,
    pub target: Option<String>,
    pub disk_size: Option<String>,
    pub skip_guest_build: bool,
    pub guest_binary: Option<PathBuf>,
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
        Command::ImageBuild(command) => run_image_build(command),
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
        "image" => parse_image(args),
        "stop" => parse_instance_command(args, CommandKind::Stop),
        "teardown" => parse_instance_command(args, CommandKind::Teardown),
        "--help" | "-h" | "help" => Err(PetriError::Cli(usage())),
        _ => Err(PetriError::Cli(format!(
            "unknown command '{command}'\n{}",
            usage()
        ))),
    }
}

fn parse_image(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let Some(subcommand) = args.next() else {
        return Err(PetriError::Cli(image_usage()));
    };

    match subcommand.as_str() {
        "build" => parse_image_build(args),
        "--help" | "-h" | "help" => Err(PetriError::Cli(image_usage())),
        _ => Err(PetriError::Cli(format!(
            "unknown image command '{subcommand}'\n{}",
            image_usage()
        ))),
    }
}

fn parse_image_build(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let mut command = ImageBuildCommand {
        config: None,
        out_dir: None,
        arch: None,
        debian_arch: None,
        target: None,
        disk_size: None,
        skip_guest_build: false,
        guest_binary: None,
    };

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => command.config = Some(PathBuf::from(next_arg(&mut args, "--config")?)),
            "--out-dir" => command.out_dir = Some(PathBuf::from(next_arg(&mut args, "--out-dir")?)),
            "--arch" => command.arch = Some(next_arg(&mut args, "--arch")?),
            "--debian-arch" => command.debian_arch = Some(next_arg(&mut args, "--debian-arch")?),
            "--target" => command.target = Some(next_arg(&mut args, "--target")?),
            "--disk-size" => command.disk_size = Some(next_arg(&mut args, "--disk-size")?),
            "--skip-guest-build" => command.skip_guest_build = true,
            "--guest-binary" => {
                command.guest_binary = Some(PathBuf::from(next_arg(&mut args, "--guest-binary")?))
            }
            "--help" | "-h" => return Err(PetriError::Cli(image_build_usage())),
            _ => {
                return Err(PetriError::Cli(format!(
                    "unknown image build argument '{arg}'"
                )));
            }
        }
    }

    Ok(Command::ImageBuild(command))
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

fn run_image_build(command: ImageBuildCommand) -> Result<String> {
    let script = image_build_script();
    let mut process = ProcessCommand::new(&script);

    if let Some(config) = command.config {
        process.arg("--config").arg(config);
    }
    if let Some(out_dir) = command.out_dir {
        process.arg("--out-dir").arg(out_dir);
    }
    if let Some(arch) = command.arch {
        process.arg("--arch").arg(arch);
    }
    if let Some(debian_arch) = command.debian_arch {
        process.arg("--debian-arch").arg(debian_arch);
    }
    if let Some(target) = command.target {
        process.arg("--target").arg(target);
    }
    if let Some(disk_size) = command.disk_size {
        process.arg("--disk-size").arg(disk_size);
    }
    if command.skip_guest_build {
        process.arg("--skip-guest-build");
    }
    if let Some(guest_binary) = command.guest_binary {
        process.arg("--guest-binary").arg(guest_binary);
    }

    let status = process.status().map_err(|source| PetriError::Io {
        path: script.clone(),
        source,
    })?;

    if !status.success() {
        return Err(PetriError::Cli(format!("image build failed with {status}")));
    }

    Ok("image build completed".to_string())
}

fn image_build_script() -> PathBuf {
    std::env::var_os("PETRI_IMAGE_BUILD_SCRIPT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/build-base-image.sh")
        })
}

pub fn usage() -> String {
    [
        "usage: petri <command> [options]",
        "",
        "commands:",
        "  create    create a configured Petri instance",
        "  dispatch  send a protocol v1 dispatch request",
        "  image     build and inspect Petri VM images",
        "  stop      stop a running instance",
        "  teardown  remove instance runtime state",
        "",
        &create_usage(),
        &dispatch_usage(),
        &image_usage(),
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

fn image_usage() -> String {
    format!(
        "usage: petri image <command> [options]\n\ncommands:\n  build  {}\n\n{}",
        image_build_usage(),
        "Set PETRI_IMAGE_BUILD_SCRIPT to override the bundled builder path."
    )
}

fn image_build_usage() -> String {
    "usage: petri image build [--config <path>] [--out-dir <path>] [--arch <arch>] [--debian-arch <arch>] [--target <triple>] [--disk-size <size>] [--skip-guest-build --guest-binary <path>]".to_string()
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

    #[test]
    fn parses_image_build_command() {
        let command = parse(args(&[
            "image",
            "build",
            "--config",
            "images/base/petri-base-image.toml",
            "--out-dir",
            "target/petri-images/custom",
            "--disk-size",
            "4G",
            "--skip-guest-build",
            "--guest-binary",
            "target/petri-guest",
        ]))
        .unwrap();

        let Command::ImageBuild(command) = command else {
            panic!("expected image build command");
        };

        assert_eq!(
            command.config,
            Some(PathBuf::from("images/base/petri-base-image.toml"))
        );
        assert_eq!(
            command.out_dir,
            Some(PathBuf::from("target/petri-images/custom"))
        );
        assert_eq!(command.disk_size.as_deref(), Some("4G"));
        assert!(command.skip_guest_build);
        assert_eq!(
            command.guest_binary,
            Some(PathBuf::from("target/petri-guest"))
        );
    }
}
