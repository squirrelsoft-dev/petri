use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::backend::HostBackend;
use crate::dispatch::{DispatchRequest, RequestLimits};
use crate::error::{PetriError, Result};
use crate::instance::{InstanceConfig, InstanceId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Create(CreateCommand),
    Dispatch(DispatchCommand),
    ImageBuild(ImageBuildCommand),
    SandboxList(SandboxListCommand),
    SandboxConnect(InstanceCommand),
    SandboxKill(SandboxKillCommand),
    Stop(InstanceCommand),
    Teardown(InstanceCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateCommand {
    pub config: InstanceConfig,
    pub output: CreateOutput,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchCommand {
    pub instance_id: InstanceId,
    pub request: DispatchRequest,
    pub stdin_passthrough: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateOutput {
    LegacyMessage,
    SandboxId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxListCommand {
    pub state: Option<SandboxStateFilter>,
    pub metadata: BTreeMap<String, String>,
    pub limit: Option<usize>,
    pub format: OutputFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxStateFilter {
    Running,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Pretty,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxKillCommand {
    pub all: bool,
    pub instance_ids: Vec<InstanceId>,
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
    pub builder: ImageBuilder,
    pub builder_image: Option<PathBuf>,
    pub builder_source: Option<String>,
    pub builder_source_sha256: Option<String>,
    pub builder_source_checksums: Option<String>,
    pub builder_cache_dir: Option<PathBuf>,
    pub prepare_builder: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceCommand {
    pub instance_id: InstanceId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageBuilder {
    Auto,
    Linux,
    Vm,
}

pub fn run(args: impl IntoIterator<Item = OsString>, backend: &impl HostBackend) -> Result<String> {
    run_with_stdin(args, backend, None)
}

pub fn run_with_stdin(
    args: impl IntoIterator<Item = OsString>,
    backend: &impl HostBackend,
    stdin: Option<String>,
) -> Result<String> {
    match parse(args)? {
        Command::Create(command) => {
            let handle = backend.create(command.config)?;
            match command.output {
                CreateOutput::LegacyMessage => Ok(format!(
                    "created instance {} with backend {}",
                    handle.id, handle.backend
                )),
                CreateOutput::SandboxId => Ok(handle.id.to_string()),
            }
        }
        Command::Dispatch(command) => {
            let request = match (command.stdin_passthrough, stdin) {
                (true, Some(stdin)) => command.request.with_stdin(stdin),
                _ => command.request,
            };
            let result = backend.dispatch(&command.instance_id, request)?;
            serde_json::to_string(&result).map_err(|err| PetriError::Cli(err.to_string()))
        }
        Command::ImageBuild(command) => run_image_build(command, backend),
        Command::SandboxList(command) => run_sandbox_list(command, backend),
        Command::SandboxConnect(command) => Err(PetriError::Cli(format!(
            "sandbox connect is not implemented yet for instance {}",
            command.instance_id
        ))),
        Command::SandboxKill(command) => run_sandbox_kill(command, backend),
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
        "sandbox" => parse_sandbox(args),
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

fn parse_sandbox(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let Some(subcommand) = args.next() else {
        return Err(PetriError::Cli(sandbox_usage()));
    };

    match subcommand.as_str() {
        "list" => parse_sandbox_list(args),
        "create" => parse_sandbox_create(args),
        "connect" => parse_sandbox_connect(args),
        "exec" => parse_sandbox_exec(args),
        "kill" => parse_sandbox_kill(args),
        "--help" | "-h" | "help" => Err(PetriError::Cli(sandbox_usage())),
        _ => Err(PetriError::Cli(format!(
            "unknown sandbox command '{subcommand}'\n{}",
            sandbox_usage()
        ))),
    }
}

fn parse_sandbox_list(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let mut command = SandboxListCommand {
        state: None,
        metadata: BTreeMap::new(),
        limit: None,
        format: OutputFormat::Pretty,
    };

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--state" => command.state = Some(parse_state_filter(next_arg(&mut args, "--state")?)?),
            "--metadata" => {
                command.metadata =
                    parse_key_value_list(next_arg(&mut args, "--metadata")?, "--metadata")?
            }
            "--limit" => {
                let value = parse_u64(next_arg(&mut args, "--limit")?, "--limit")?;
                command.limit =
                    Some(
                        usize::try_from(value).map_err(|_| PetriError::InvalidArgument {
                            flag: "--limit",
                            value: value.to_string(),
                            message: "expected a value that fits in usize".to_string(),
                        })?,
                    );
            }
            "--format" => command.format = parse_output_format(next_arg(&mut args, "--format")?)?,
            "--help" | "-h" => return Err(PetriError::Cli(sandbox_list_usage())),
            _ => {
                return Err(PetriError::Cli(format!(
                    "unknown sandbox list argument '{arg}'"
                )));
            }
        }
    }

    Ok(Command::SandboxList(command))
}

fn parse_sandbox_create(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let mut template = None;
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
            "--help" | "-h" => return Err(PetriError::Cli(sandbox_create_usage())),
            _ if arg.starts_with('-') => {
                return Err(PetriError::Cli(format!(
                    "unknown sandbox create argument '{arg}'"
                )));
            }
            _ => {
                if template.replace(arg.clone()).is_some() {
                    return Err(PetriError::Cli(format!(
                        "unexpected sandbox create argument '{arg}'"
                    )));
                }
            }
        }
    }

    let id = match id {
        Some(id) => id,
        None => InstanceId::new(format!("petri-{}", unique_build_id()?))?,
    };
    let workspace = workspace.ok_or(PetriError::MissingArgument {
        flag: "--workspace",
    })?;
    let policy = policy.ok_or(PetriError::MissingArgument { flag: "--policy" })?;
    let mut config = InstanceConfig::new(id, backend, workspace, policy);
    let image = match (image, template.as_deref()) {
        (Some(image), _) => Some(image),
        (None, None | Some("base")) => Some(default_base_image()),
        (None, Some(template)) => {
            return Err(PetriError::InvalidArgument {
                flag: "template",
                value: template.to_string(),
                message: "only the base template is currently supported".to_string(),
            });
        }
    };

    if let Some(image) = image {
        config = config.with_image(image);
    }

    Ok(Command::Create(CreateCommand {
        config,
        output: CreateOutput::SandboxId,
    }))
}

fn parse_sandbox_connect(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let Some(id) = args.next() else {
        return Err(PetriError::Cli(sandbox_connect_usage()));
    };
    if matches!(id.as_str(), "--help" | "-h") {
        return Err(PetriError::Cli(sandbox_connect_usage()));
    }
    if let Some(extra) = args.next() {
        return Err(PetriError::Cli(format!(
            "unexpected sandbox connect argument '{extra}'"
        )));
    }

    Ok(Command::SandboxConnect(InstanceCommand {
        instance_id: InstanceId::new(id)?,
    }))
}

fn parse_sandbox_exec(args: impl Iterator<Item = String>) -> Result<Command> {
    let mut args = args.peekable();
    let mut request_id = "sandbox-exec-1".to_string();
    let mut cwd = PathBuf::from("/workspace");
    let mut env = BTreeMap::new();
    let mut timeout_ms = None;
    let mut max_output_bytes = None;
    let mut instance_id = None;
    let mut command = None;
    let mut argv = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--request-id" => request_id = next_arg(&mut args, "--request-id")?,
            "--cwd" => cwd = PathBuf::from(next_arg(&mut args, "--cwd")?),
            "--env" => {
                let value = next_arg(&mut args, "--env")?;
                env.extend(parse_key_value_list(value, "--env")?);
            }
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
            "--help" | "-h" => return Err(PetriError::Cli(sandbox_exec_usage())),
            "--background" => {
                return Err(PetriError::Cli(
                    "sandbox exec --background is not implemented yet".to_string(),
                ));
            }
            "--user" => {
                let value = next_arg(&mut args, "--user")?;
                return Err(PetriError::Cli(format!(
                    "sandbox exec --user {value} is not implemented yet"
                )));
            }
            _ if arg.starts_with('-') => {
                return Err(PetriError::Cli(format!(
                    "unknown sandbox exec argument '{arg}'"
                )));
            }
            _ if instance_id.is_none() => instance_id = Some(InstanceId::new(arg)?),
            _ => {
                command = Some(arg);
                argv.extend(args);
                break;
            }
        }
    }

    let instance_id = instance_id.ok_or(PetriError::MissingArgument {
        flag: "<sandbox-id>",
    })?;
    let command = command.ok_or(PetriError::MissingArgument { flag: "<command>" })?;
    let limits = (timeout_ms.is_some() || max_output_bytes.is_some()).then_some(RequestLimits {
        timeout_ms,
        max_output_bytes,
    });
    let request = DispatchRequest::bash_command(request_id, command, argv, cwd, env, None, limits);

    Ok(Command::Dispatch(DispatchCommand {
        instance_id,
        request,
        stdin_passthrough: true,
    }))
}

fn parse_sandbox_kill(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let mut all = false;
    let mut instance_ids = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--all" => all = true,
            "--help" | "-h" => return Err(PetriError::Cli(sandbox_kill_usage())),
            _ if arg.starts_with('-') => {
                return Err(PetriError::Cli(format!(
                    "unknown sandbox kill argument '{arg}'"
                )));
            }
            _ => instance_ids.push(InstanceId::new(arg)?),
        }
    }

    if (all && !instance_ids.is_empty()) || (!all && instance_ids.is_empty()) {
        return Err(PetriError::Cli(
            "sandbox kill requires --all or at least one sandbox id".to_string(),
        ));
    }

    Ok(Command::SandboxKill(SandboxKillCommand {
        all,
        instance_ids,
    }))
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
        builder: ImageBuilder::Auto,
        builder_image: None,
        builder_source: None,
        builder_source_sha256: None,
        builder_source_checksums: None,
        builder_cache_dir: None,
        prepare_builder: false,
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
            "--builder" => {
                command.builder = parse_image_builder(next_arg(&mut args, "--builder")?)?
            }
            "--builder-image" => {
                command.builder_image = Some(PathBuf::from(next_arg(&mut args, "--builder-image")?))
            }
            "--builder-source" => {
                command.builder_source = Some(next_arg(&mut args, "--builder-source")?)
            }
            "--builder-source-sha256" => {
                command.builder_source_sha256 =
                    Some(next_arg(&mut args, "--builder-source-sha256")?)
            }
            "--builder-source-checksums" => {
                command.builder_source_checksums =
                    Some(next_arg(&mut args, "--builder-source-checksums")?)
            }
            "--builder-cache-dir" => {
                command.builder_cache_dir =
                    Some(PathBuf::from(next_arg(&mut args, "--builder-cache-dir")?))
            }
            "--prepare-builder" => command.prepare_builder = true,
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

fn parse_image_builder(value: String) -> Result<ImageBuilder> {
    match value.as_str() {
        "auto" => Ok(ImageBuilder::Auto),
        "linux" => Ok(ImageBuilder::Linux),
        "vm" => Ok(ImageBuilder::Vm),
        _ => Err(PetriError::InvalidArgument {
            flag: "--builder",
            value,
            message: "expected auto, linux, or vm".to_string(),
        }),
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

    Ok(Command::Create(CreateCommand {
        config,
        output: CreateOutput::LegacyMessage,
    }))
}

fn parse_dispatch(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let mut instance_id = None;
    let mut request_id = "dispatch-1".to_string();
    let mut tool = "bash_command".to_string();
    let mut command = None;
    let mut argv = Vec::new();
    let mut cwd = None;
    let mut args_json = None;
    let mut timeout_ms = None;
    let mut max_output_bytes = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--id" => instance_id = Some(InstanceId::new(next_arg(&mut args, "--id")?)?),
            "--request-id" => request_id = next_arg(&mut args, "--request-id")?,
            "--tool" => tool = next_arg(&mut args, "--tool")?,
            "--command" => command = Some(next_arg(&mut args, "--command")?),
            "--arg" => argv.push(next_arg(&mut args, "--arg")?),
            "--args-json" => args_json = Some(next_arg(&mut args, "--args-json")?),
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

    let instance_id = instance_id.ok_or(PetriError::MissingArgument { flag: "--id" })?;
    let limits = (timeout_ms.is_some() || max_output_bytes.is_some()).then_some(RequestLimits {
        timeout_ms,
        max_output_bytes,
    });

    let request = if tool == "bash_command" {
        let command = command.ok_or(PetriError::MissingArgument { flag: "--command" })?;
        let cwd = cwd.ok_or(PetriError::MissingArgument { flag: "--cwd" })?;
        DispatchRequest::bash_command(
            request_id,
            command,
            argv,
            cwd,
            BTreeMap::new(),
            None,
            limits,
        )
    } else if petri_protocol::lsp_tools::is_lsp_tool(&tool) {
        // Structured tools carry a raw JSON args object via --args-json.
        let raw = args_json.ok_or(PetriError::MissingArgument {
            flag: "--args-json",
        })?;
        let parsed = serde_json::from_str::<serde_json::Value>(&raw).map_err(|err| {
            PetriError::InvalidArgument {
                flag: "--args-json",
                value: raw.clone(),
                message: format!("must be a JSON object: {err}"),
            }
        })?;
        DispatchRequest {
            protocol_version: crate::dispatch::PROTOCOL_VERSION,
            id: request_id,
            control: None,
            target_id: None,
            tool: Some(tool),
            args: Some(parsed),
            limits,
        }
    } else {
        return Err(PetriError::InvalidArgument {
            flag: "--tool",
            value: tool,
            message: "supported tools: bash_command, lsp_hover, lsp_definition, lsp_references, lsp_diagnostics, lsp_rename".to_string(),
        });
    };

    Ok(Command::Dispatch(DispatchCommand {
        instance_id,
        request,
        stdin_passthrough: false,
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

fn parse_state_filter(value: String) -> Result<SandboxStateFilter> {
    match value.as_str() {
        "running" => Ok(SandboxStateFilter::Running),
        "paused" => Ok(SandboxStateFilter::Paused),
        _ => Err(PetriError::InvalidArgument {
            flag: "--state",
            value,
            message: "expected running or paused".to_string(),
        }),
    }
}

fn parse_output_format(value: String) -> Result<OutputFormat> {
    match value.as_str() {
        "pretty" => Ok(OutputFormat::Pretty),
        "json" => Ok(OutputFormat::Json),
        _ => Err(PetriError::InvalidArgument {
            flag: "--format",
            value,
            message: "expected pretty or json".to_string(),
        }),
    }
}

fn parse_key_value_list(value: String, flag: &'static str) -> Result<BTreeMap<String, String>> {
    let mut pairs = BTreeMap::new();
    if value.is_empty() {
        return Ok(pairs);
    }

    for pair in value.split(',') {
        let Some((key, value)) = pair.split_once('=') else {
            return Err(PetriError::InvalidArgument {
                flag,
                value: pair.to_string(),
                message: "expected key=value".to_string(),
            });
        };
        if key.is_empty() {
            return Err(PetriError::InvalidArgument {
                flag,
                value: pair.to_string(),
                message: "key must be non-empty".to_string(),
            });
        }
        pairs.insert(key.to_string(), value.to_string());
    }

    Ok(pairs)
}

fn default_base_image() -> PathBuf {
    std::env::var_os("PETRI_BASE_IMAGE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            repo_root_fallback()
                .join("target")
                .join("petri-images")
                .join("base")
        })
}

fn run_sandbox_list(command: SandboxListCommand, backend: &impl HostBackend) -> Result<String> {
    let mut instances = backend.list()?;
    if !command.metadata.is_empty() {
        instances.clear();
    }
    if let Some(state) = command.state {
        instances.retain(|instance| match state {
            SandboxStateFilter::Running => instance.state.is_running(),
            SandboxStateFilter::Paused => false,
        });
    }
    if let Some(limit) = command.limit {
        instances.truncate(limit);
    }

    match command.format {
        OutputFormat::Json => {
            serde_json::to_string(&instances).map_err(|err| PetriError::Cli(err.to_string()))
        }
        OutputFormat::Pretty => {
            if instances.is_empty() {
                return Ok("no sandboxes".to_string());
            }

            let mut lines = vec!["ID\tBACKEND\tSTATE".to_string()];
            lines.extend(instances.into_iter().map(|instance| {
                format!(
                    "{}\t{}\t{:?}",
                    instance.id, instance.backend, instance.state
                )
            }));
            Ok(lines.join("\n"))
        }
    }
}

fn run_sandbox_kill(command: SandboxKillCommand, backend: &impl HostBackend) -> Result<String> {
    let instance_ids = if command.all {
        backend
            .list()?
            .into_iter()
            .map(|instance| instance.id)
            .collect()
    } else {
        command.instance_ids
    };

    for instance_id in &instance_ids {
        backend.teardown(instance_id)?;
    }

    Ok(format!("killed {} sandbox(s)", instance_ids.len()))
}

fn run_image_build(command: ImageBuildCommand, backend: &impl HostBackend) -> Result<String> {
    if command.prepare_builder {
        return run_prepare_builder(command, backend);
    }

    match selected_image_builder(command.builder)? {
        ImageBuilder::Linux => run_linux_image_build(command),
        ImageBuilder::Vm => run_vm_image_build(command, backend),
        ImageBuilder::Auto => unreachable!("selected_image_builder resolves auto"),
    }
}

const DEFAULT_BUILDER_SOURCE: &str =
    "https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-arm64.raw";
const DEFAULT_BUILDER_SOURCE_CHECKSUMS: &str =
    "https://cloud.debian.org/images/cloud/trixie/latest/SHA512SUMS";
const BUILDER_IMAGE_VERSION: u32 = 1;
const DEFAULT_BUILDER_DISK_SIZE: u64 = 16 * 1024 * 1024 * 1024;
const BUILDER_PACKAGES: &[&str] = &[
    "bash",
    "ca-certificates",
    "coreutils",
    "e2fsprogs",
    "git",
    "jq",
    "mmdebstrap",
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct VerifiedBuilderSource {
    source: String,
    path: PathBuf,
    checksum_algorithm: String,
    checksum_hex: String,
}

fn run_prepare_builder(command: ImageBuildCommand, backend: &impl HostBackend) -> Result<String> {
    let builder_image = command
        .builder_image
        .clone()
        .or_else(|| std::env::var_os("PETRI_BUILDER_IMAGE").map(PathBuf::from))
        .ok_or_else(|| {
            PetriError::Cli(
                "builder preparation requires --builder-image <bundle> or PETRI_BUILDER_IMAGE"
                    .to_string(),
            )
        })?;

    if !cfg!(target_os = "macos") {
        return Err(PetriError::Cli(
            "builder preparation currently requires macOS Virtualization.framework".to_string(),
        ));
    }

    if command.skip_guest_build || command.guest_binary.is_some() {
        return Err(PetriError::Cli(
            "builder preparation builds petri-guest on the host; do not pass --skip-guest-build or --guest-binary".to_string(),
        ));
    }

    let repo_root = repo_root()?;
    let target = command
        .target
        .clone()
        .unwrap_or_else(|| configured_guest_target(command.config.as_deref()));
    build_host_guest_binary(&repo_root, &target)?;

    let guest_binary = repo_root
        .join("target")
        .join(&target)
        .join("release")
        .join("petri-guest");
    let guest_binary_in_vm = guest_path_for_repo_file(&repo_root, &guest_binary)?;
    let bootstrap_log = repo_root.join("target").join("petri-builder-bootstrap.log");
    let bootstrap_log_in_vm = guest_path_for_repo_file(&repo_root, &bootstrap_log)?;
    let _ = fs::remove_file(&bootstrap_log);
    let cache_dir = absolute_out_dir(
        &repo_root,
        command
            .builder_cache_dir
            .clone()
            .unwrap_or_else(|| repo_root.join("target").join("petri-builder-cache")),
    );
    fs::create_dir_all(&cache_dir).map_err(|source| PetriError::Io {
        path: cache_dir.clone(),
        source,
    })?;

    let source = acquire_builder_source(&command, &cache_dir)?;
    let parent = builder_image
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&parent).map_err(|source| PetriError::Io {
        path: parent.clone(),
        source,
    })?;

    let build_id = unique_build_id()?;
    let staging = parent.join(format!(
        ".{}-staging-{build_id}",
        builder_image
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("petri-builder")
    ));
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(|source| PetriError::Io {
            path: staging.clone(),
            source,
        })?;
    }
    fs::create_dir_all(&staging).map_err(|source| PetriError::Io {
        path: staging.clone(),
        source,
    })?;

    let root_img = staging.join("root.img");
    fs::copy(&source.path, &root_img).map_err(|source| PetriError::Io {
        path: root_img.clone(),
        source,
    })?;
    let disk_size = command
        .disk_size
        .as_deref()
        .map(parse_disk_size)
        .transpose()?
        .unwrap_or(DEFAULT_BUILDER_DISK_SIZE);
    expand_file(&root_img, disk_size)?;
    configure_builder_efi_console(&root_img)?;

    let seed_iso = staging.join("seed.iso");
    write_cloud_init_seed(
        &staging,
        &seed_iso,
        &guest_binary_in_vm,
        &bootstrap_log_in_vm,
        &source,
        disk_size,
    )?;
    write_builder_manifest(&staging, true)?;
    eprintln!(
        "booting builder VM for first-boot provisioning; staging bundle: {}",
        staging.display()
    );

    let policy = write_builder_policy(&repo_root)?;
    let instance_id = InstanceId::new(format!("petri-bootstrap-{}", unique_build_id()?))?;
    let config = InstanceConfig::new(
        instance_id.clone(),
        "macos",
        repo_root.clone(),
        policy.clone(),
    )
    .with_image(staging.clone());

    let _ = backend.teardown(&instance_id);
    backend.create(config).inspect_err(|_| {
        eprintln!(
            "builder provisioning failed while booting {}; staging bundle left at {}",
            source.source,
            staging.display()
        );
    })?;

    let package_install = DispatchRequest::bash_command(
        "builder-package-install",
        "bash",
        vec!["-lc".to_string(), builder_package_install_script()],
        PathBuf::from("/workspace"),
        BTreeMap::new(),
        None,
        Some(RequestLimits {
            timeout_ms: Some(30 * 60 * 1000),
            max_output_bytes: Some(2 * 1024 * 1024),
        }),
    );
    let result = backend.dispatch(&instance_id, package_install)?;
    if result.status != crate::dispatch::Status::Success || result.exit_code != Some(Some(0)) {
        let _ = backend.teardown(&instance_id);
        return Err(PetriError::Cli(format!(
            "builder package installation failed; staging bundle left at {}\nstdout:\n{}\nstderr:\n{}",
            staging.display(),
            result.stdout.unwrap_or_default(),
            result.stderr.unwrap_or_default()
        )));
    }

    let validation = DispatchRequest::bash_command(
        "builder-validation",
        "bash",
        vec![
            "-lc".to_string(),
            "test -f /var/lib/petri-builder/provisioned.json && for tool in mmdebstrap python3 mke2fs sha256sum; do command -v \"$tool\"; done && systemctl is-enabled workspace.mount run-petri.mount petri-guest.service && systemctl is-active workspace.mount run-petri.mount petri-guest.service"
                .to_string(),
        ],
        PathBuf::from("/workspace"),
        BTreeMap::new(),
        None,
        Some(RequestLimits {
            timeout_ms: Some(5 * 60 * 1000),
            max_output_bytes: Some(256 * 1024),
        }),
    );

    let result = backend.dispatch(&instance_id, validation)?;
    if result.status != crate::dispatch::Status::Success || result.exit_code != Some(Some(0)) {
        return Err(PetriError::Cli(format!(
            "builder validation failed; staging bundle left at {}\nstdout:\n{}\nstderr:\n{}",
            staging.display(),
            result.stdout.unwrap_or_default(),
            result.stderr.unwrap_or_default()
        )));
    }

    let finalization = DispatchRequest::bash_command(
        "builder-finalization",
        "bash",
        vec![
            "-lc".to_string(),
            "set -euo pipefail; systemctl disable --now cloud-init.service cloud-init-local.service cloud-config.service cloud-final.service cloud-init-main.service cloud-init-hotplugd.socket 2>/dev/null || true; cloud-init clean --logs --seed || true; rm -rf /var/lib/cloud/instances /var/lib/cloud/seed; sync"
                .to_string(),
        ],
        PathBuf::from("/workspace"),
        BTreeMap::new(),
        None,
        Some(RequestLimits {
            timeout_ms: Some(5 * 60 * 1000),
            max_output_bytes: Some(256 * 1024),
        }),
    );
    let result = backend.dispatch(&instance_id, finalization);
    let _ = backend.teardown(&instance_id);
    let result = result?;
    if result.status != crate::dispatch::Status::Success || result.exit_code != Some(Some(0)) {
        return Err(PetriError::Cli(format!(
            "builder finalization failed; staging bundle left at {}\nstdout:\n{}\nstderr:\n{}",
            staging.display(),
            result.stdout.unwrap_or_default(),
            result.stderr.unwrap_or_default()
        )));
    }

    fs::remove_file(&seed_iso).map_err(|source| PetriError::Io {
        path: seed_iso.clone(),
        source,
    })?;
    restore_builder_efi_grub(&root_img)?;
    write_builder_manifest(&staging, false)?;
    write_builder_build_info(&staging, &source, disk_size)?;
    write_sha256sums(
        &staging,
        &["petri-image.json", "root.img", "build-info.json"],
    )?;
    atomic_replace_dir(&staging, &builder_image)?;

    Ok(format!(
        "builder image prepared: {}",
        builder_image.display()
    ))
}

fn builder_package_install_script() -> String {
    let packages = BUILDER_PACKAGES
        .iter()
        .map(|package| shell_quote_str(package))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "set -euo pipefail; cat > /etc/systemd/network/10-petri-builder.network <<'EOF'\n[Match]\nName=en*\n\n[Network]\nDHCP=yes\nIPv6AcceptRA=yes\nEOF\nsystemctl enable systemd-networkd.service systemd-resolved.service; systemctl restart systemd-networkd.service; sleep 3; rm -f /etc/resolv.conf; printf 'nameserver 1.1.1.1\\nnameserver 8.8.8.8\\noptions timeout:2 attempts:3\\n' > /etc/resolv.conf; export DEBIAN_FRONTEND=noninteractive; apt-get update -o Acquire::Retries=5; apt-get install -y --no-install-recommends {packages}; for tool in mmdebstrap python3 mke2fs sha256sum; do command -v \"$tool\"; done"
    )
}

fn acquire_builder_source(
    command: &ImageBuildCommand,
    cache_dir: &Path,
) -> Result<VerifiedBuilderSource> {
    let source = command
        .builder_source
        .clone()
        .unwrap_or_else(|| DEFAULT_BUILDER_SOURCE.to_string());
    if !source.ends_with(".raw") {
        return Err(PetriError::Cli(format!(
            "unsupported builder source format for {source}; only .raw is supported"
        )));
    }

    let path = if is_url(&source) {
        let dest = cache_dir.join(url_file_name(&source)?);
        if !dest.is_file() {
            run_status(
                ProcessCommand::new("curl")
                    .arg("-fL")
                    .arg("--retry")
                    .arg("3")
                    .arg("-o")
                    .arg(&dest)
                    .arg(&source),
                format!("failed to download builder source {source}"),
            )?;
        }
        dest
    } else {
        fs::canonicalize(&source).map_err(|source_err| PetriError::Io {
            path: PathBuf::from(&source),
            source: source_err,
        })?
    };

    let checksum = if let Some(hex) = &command.builder_source_sha256 {
        ("sha256".to_string(), hex.to_ascii_lowercase())
    } else {
        let checksum_source = command
            .builder_source_checksums
            .clone()
            .or_else(|| (source == DEFAULT_BUILDER_SOURCE).then(|| DEFAULT_BUILDER_SOURCE_CHECKSUMS.to_string()))
            .ok_or_else(|| {
                PetriError::Cli(
                    "builder source verification requires --builder-source-sha256 or --builder-source-checksums"
                        .to_string(),
                )
            })?;
        checksum_from_file_or_url(&checksum_source, cache_dir, &path)?
    };

    verify_checksum(&path, &checksum.0, &checksum.1)?;
    Ok(VerifiedBuilderSource {
        source,
        path,
        checksum_algorithm: checksum.0,
        checksum_hex: checksum.1,
    })
}

fn checksum_from_file_or_url(
    source: &str,
    cache_dir: &Path,
    image_path: &Path,
) -> Result<(String, String)> {
    let path = if is_url(source) {
        let dest = cache_dir.join(url_cache_file_name(source)?);
        if !dest.is_file() {
            run_status(
                ProcessCommand::new("curl")
                    .arg("-fL")
                    .arg("--retry")
                    .arg("3")
                    .arg("-o")
                    .arg(&dest)
                    .arg(source),
                format!("failed to download builder checksum file {source}"),
            )?;
        }
        dest
    } else {
        PathBuf::from(source)
    };
    let input = fs::read_to_string(&path).map_err(|source| PetriError::Io {
        path: path.clone(),
        source,
    })?;
    let image_name = image_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| PetriError::Cli(format!("invalid image path {}", image_path.display())))?;
    parse_checksum_file(&input, image_name).ok_or_else(|| {
        PetriError::Cli(format!(
            "checksum file {} does not contain an entry for {image_name}",
            path.display()
        ))
    })
}

fn parse_checksum_file(input: &str, image_name: &str) -> Option<(String, String)> {
    input.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let hex = fields.next()?.trim_start_matches('\\');
        let filename = fields.next()?.trim_start_matches('*');
        if Path::new(filename).file_name()?.to_str()? != image_name {
            return None;
        }
        let algorithm = match hex.len() {
            64 => "sha256",
            128 => "sha512",
            _ => return None,
        };
        Some((algorithm.to_string(), hex.to_ascii_lowercase()))
    })
}

fn url_cache_file_name(source: &str) -> Result<String> {
    if !is_url(source) {
        return Err(PetriError::Cli(format!("not a URL: {source}")));
    }
    Ok(source
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect())
}

fn verify_checksum(path: &Path, algorithm: &str, expected: &str) -> Result<()> {
    let actual = file_checksum(path, algorithm)?;
    if actual != expected.to_ascii_lowercase() {
        return Err(PetriError::Cli(format!(
            "{algorithm} mismatch for {}: expected {expected}, got {actual}",
            path.display()
        )));
    }
    Ok(())
}

fn file_checksum(path: &Path, algorithm: &str) -> Result<String> {
    let bits = match algorithm {
        "sha256" => "256",
        "sha512" => "512",
        _ => {
            return Err(PetriError::Cli(format!(
                "unsupported checksum algorithm {algorithm}"
            )));
        }
    };
    let output = ProcessCommand::new("shasum")
        .arg("-a")
        .arg(bits)
        .arg(path)
        .output()
        .map_err(|source| PetriError::Io {
            path: PathBuf::from("shasum"),
            source,
        })?;
    if !output.status.success() {
        return Err(PetriError::Cli(format!(
            "failed to compute {algorithm} for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(|value| value.to_ascii_lowercase())
        .ok_or_else(|| PetriError::Cli(format!("shasum produced no output for {}", path.display())))
}

fn write_cloud_init_seed(
    staging: &Path,
    seed_iso: &Path,
    guest_binary_in_vm: &Path,
    bootstrap_log_in_vm: &Path,
    source: &VerifiedBuilderSource,
    disk_size: u64,
) -> Result<()> {
    let seed_dir = staging.join("seed");
    fs::create_dir_all(&seed_dir).map_err(|source| PetriError::Io {
        path: seed_dir.clone(),
        source,
    })?;
    fs::write(
        seed_dir.join("meta-data"),
        "instance-id: petri-builder\nlocal-hostname: petri-builder\n",
    )
    .map_err(|source| PetriError::Io {
        path: seed_dir.join("meta-data"),
        source,
    })?;
    fs::write(
        seed_dir.join("user-data"),
        builder_cloud_init(guest_binary_in_vm, bootstrap_log_in_vm, source, disk_size),
    )
    .map_err(|source| PetriError::Io {
        path: seed_dir.join("user-data"),
        source,
    })?;
    run_status(
        ProcessCommand::new("hdiutil")
            .arg("makehybrid")
            .arg("-iso")
            .arg("-joliet")
            .arg("-default-volume-name")
            .arg("cidata")
            .arg("-o")
            .arg(seed_iso)
            .arg(&seed_dir),
        format!(
            "failed to create cloud-init seed image {}",
            seed_iso.display()
        ),
    )?;
    fs::remove_dir_all(&seed_dir).map_err(|source| PetriError::Io {
        path: seed_dir,
        source,
    })
}

fn builder_cloud_init(
    guest_binary_in_vm: &Path,
    bootstrap_log_in_vm: &Path,
    source: &VerifiedBuilderSource,
    disk_size: u64,
) -> String {
    let packages = BUILDER_PACKAGES
        .iter()
        .map(|package| format!("  - {package}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"#cloud-config
package_update: true
package_upgrade: false
packages:
{packages}
bootcmd:
  - [ mkdir, -p, /workspace, /var/log ]
  - [ bash, -lc, "mountpoint -q /workspace || mount -t virtiofs workspace /workspace || true" ]
  - [ bash, -lc, "mkdir -p \"$(dirname '{bootstrap_log}')\" || true" ]
  - [ bash, -lc, "printf '%s\n' 'petri builder bootcmd started' | tee -a /var/log/petri-builder-provision.log '{bootstrap_log}' /dev/hvc0 || true" ]
output:
  all: "| tee -a /var/log/cloud-init-output.log /var/log/petri-builder-provision.log {bootstrap_log} /dev/hvc0"
write_files:
  - path: /etc/systemd/system/workspace.mount
    permissions: "0644"
    content: |
      [Unit]
      Description=Petri workspace virtiofs mount
      [Mount]
      What=workspace
      Where=/workspace
      Type=virtiofs
      Options=defaults
      [Install]
      WantedBy=multi-user.target
  - path: /etc/systemd/system/run-petri.mount
    permissions: "0644"
    content: |
      [Unit]
      Description=Petri config virtiofs mount
      [Mount]
      What=petri-config
      Where=/run/petri
      Type=virtiofs
      Options=defaults
      [Install]
      WantedBy=multi-user.target
  - path: /etc/systemd/system/petri-guest.service
    permissions: "0644"
    content: |
      [Unit]
      Description=Petri guest dispatch service
      After=workspace.mount run-petri.mount network-online.target
      Requires=workspace.mount run-petri.mount
      [Service]
      ExecStart=/usr/local/bin/petri-guest --policy /run/petri/policy.toml --transport vsock --vsock-port 7777
      Restart=always
      RestartSec=1
      [Install]
      WantedBy=multi-user.target
runcmd:
  - [ bash, -lc, "set -euxo pipefail; mkdir -p /workspace /run/petri /var/lib/petri-builder; mountpoint -q /workspace || mount -t virtiofs workspace /workspace; mkdir -p \"$(dirname '{bootstrap_log}')\"; echo petri builder provisioning started; systemctl daemon-reload; install -m 0755 '{guest_binary}' /usr/local/bin/petri-guest; systemctl enable workspace.mount run-petri.mount petri-guest.service; systemctl start workspace.mount run-petri.mount petri-guest.service; printf '%s\n' '{{\"schema\":1,\"source\":\"{source_url}\",\"checksum\":\"{checksum_algorithm}:{checksum_hex}\",\"disk_size_bytes\":{disk_size}}}' > /var/lib/petri-builder/provisioned.json; echo petri builder provisioning complete" ]
"#,
        packages = packages,
        guest_binary = guest_binary_in_vm.display(),
        bootstrap_log = bootstrap_log_in_vm.display(),
        source_url = source.source.replace('"', "\\\""),
        checksum_algorithm = source.checksum_algorithm,
        checksum_hex = source.checksum_hex,
        disk_size = disk_size,
    )
}

fn write_builder_manifest(staging: &Path, include_seed: bool) -> Result<()> {
    let auxiliary = if include_seed {
        ",\n  \"ready_timeout_secs\": 1800,\n  \"auxiliary_disks\": [\"seed.iso\"]"
    } else {
        ""
    };
    fs::write(
        staging.join("petri-image.json"),
        format!(
            "{{\n  \"architecture\": \"aarch64\",\n  \"boot_mode\": \"efi\",\n  \"disk\": \"root.img\",\n  \"dispatch_port\": 7777{auxiliary}\n}}\n"
        ),
    )
    .map_err(|source| PetriError::Io {
        path: staging.join("petri-image.json"),
        source,
    })
}

fn configure_builder_efi_console(root_img: &Path) -> Result<()> {
    let Some(parent) = root_img.parent() else {
        return Err(PetriError::Cli(format!(
            "builder root image has no parent directory: {}",
            root_img.display()
        )));
    };
    let kernel_version = detect_builder_kernel_version(root_img)?;
    let attach_output = command_stdout(
        ProcessCommand::new("hdiutil")
            .arg("attach")
            .arg("-nomount")
            .arg("-imagekey")
            .arg("diskimage-class=CRawDiskImage")
            .arg(root_img),
        format!("failed to attach builder root image {}", root_img.display()),
    )?;

    let attach = parse_hdiutil_attach(&attach_output)?;
    let mount_dir = parent.join("efi");
    fs::create_dir_all(&mount_dir).map_err(|source| PetriError::Io {
        path: mount_dir.clone(),
        source,
    })?;

    let result = (|| {
        run_status(
            ProcessCommand::new("mount")
                .arg("-t")
                .arg("msdos")
                .arg(&attach.efi_partition)
                .arg(&mount_dir),
            format!(
                "failed to mount builder EFI partition {}",
                attach.efi_partition
            ),
        )?;

        let grub_cfg = mount_dir.join("EFI").join("debian").join("grub.cfg");
        let original = fs::read_to_string(&grub_cfg).map_err(|source| PetriError::Io {
            path: grub_cfg.clone(),
            source,
        })?;
        let root_uuid = parse_grub_root_uuid(&original)?;
        let fallback = mount_dir
            .join("EFI")
            .join("debian")
            .join("grub-petri-original.cfg");
        if !fallback.exists() {
            fs::write(&fallback, original).map_err(|source| PetriError::Io {
                path: fallback.clone(),
                source,
            })?;
        }

        let grub = format!(
            r#"search.fs_uuid {root_uuid} root
set default=0
set timeout=0

menuentry 'Petri builder bootstrap' {{
    linux /boot/vmlinuz-{kernel_version} root=UUID={root_uuid} ro console=tty0 console=hvc0 systemd.show_status=1 systemd.log_target=console systemd.journald.forward_to_console=1 cloud-init=enabled ds=nocloud
    initrd /boot/initrd.img-{kernel_version}
}}

menuentry 'Debian original GRUB config' {{
    configfile ($root)/boot/grub/grub.cfg
}}
"#,
            root_uuid = root_uuid,
            kernel_version = kernel_version,
        );
        fs::write(&grub_cfg, grub).map_err(|source| PetriError::Io {
            path: grub_cfg,
            source,
        })
    })();

    let unmount = run_status(
        ProcessCommand::new("umount").arg(&mount_dir),
        format!(
            "failed to unmount builder EFI partition at {}",
            mount_dir.display()
        ),
    );
    let detach = run_status(
        ProcessCommand::new("hdiutil")
            .arg("detach")
            .arg(&attach.disk),
        format!("failed to detach builder root image {}", attach.disk),
    );

    result?;
    unmount?;
    detach?;
    Ok(())
}

fn restore_builder_efi_grub(root_img: &Path) -> Result<()> {
    let Some(parent) = root_img.parent() else {
        return Err(PetriError::Cli(format!(
            "builder root image has no parent directory: {}",
            root_img.display()
        )));
    };
    let attach_output = command_stdout(
        ProcessCommand::new("hdiutil")
            .arg("attach")
            .arg("-nomount")
            .arg("-imagekey")
            .arg("diskimage-class=CRawDiskImage")
            .arg(root_img),
        format!("failed to attach builder root image {}", root_img.display()),
    )?;

    let attach = parse_hdiutil_attach(&attach_output)?;
    let mount_dir = parent.join("efi");
    fs::create_dir_all(&mount_dir).map_err(|source| PetriError::Io {
        path: mount_dir.clone(),
        source,
    })?;

    let result = (|| {
        run_status(
            ProcessCommand::new("mount")
                .arg("-t")
                .arg("msdos")
                .arg(&attach.efi_partition)
                .arg(&mount_dir),
            format!(
                "failed to mount builder EFI partition {}",
                attach.efi_partition
            ),
        )?;

        let grub_cfg = mount_dir.join("EFI").join("debian").join("grub.cfg");
        let fallback = mount_dir
            .join("EFI")
            .join("debian")
            .join("grub-petri-original.cfg");
        let original = fs::read(&fallback).map_err(|source| PetriError::Io {
            path: fallback.clone(),
            source,
        })?;
        fs::write(&grub_cfg, original).map_err(|source| PetriError::Io {
            path: grub_cfg,
            source,
        })
    })();

    let unmount = run_status(
        ProcessCommand::new("umount").arg(&mount_dir),
        format!(
            "failed to unmount builder EFI partition at {}",
            mount_dir.display()
        ),
    );
    let detach = run_status(
        ProcessCommand::new("hdiutil")
            .arg("detach")
            .arg(&attach.disk),
        format!("failed to detach builder root image {}", attach.disk),
    );

    result?;
    unmount?;
    detach?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HdiutilAttach {
    disk: String,
    efi_partition: String,
}

fn parse_hdiutil_attach(output: &str) -> Result<HdiutilAttach> {
    let mut disk = None;
    let mut efi_partition = None;

    for line in output.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        let Some(device) = fields.first() else {
            continue;
        };
        if !device.starts_with("/dev/disk") {
            continue;
        }
        if is_hdiutil_whole_disk(device) {
            disk = Some((*device).to_string());
        }
        if fields.iter().any(|field| *field == "EFI") {
            efi_partition = Some((*device).to_string());
        }
    }

    Ok(HdiutilAttach {
        disk: disk.ok_or_else(|| {
            PetriError::Cli(format!(
                "failed to parse hdiutil attach disk from output:\n{output}"
            ))
        })?,
        efi_partition: efi_partition.ok_or_else(|| {
            PetriError::Cli(format!(
                "failed to parse hdiutil attach EFI partition from output:\n{output}"
            ))
        })?,
    })
}

fn is_hdiutil_whole_disk(device: &str) -> bool {
    let Some(suffix) = device.strip_prefix("/dev/disk") else {
        return false;
    };
    !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit())
}

fn parse_grub_root_uuid(input: &str) -> Result<String> {
    input
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            if fields.next()? == "search.fs_uuid" {
                fields.next().map(ToOwned::to_owned)
            } else {
                None
            }
        })
        .ok_or_else(|| {
            PetriError::Cli("failed to parse root filesystem UUID from EFI grub.cfg".to_string())
        })
}

fn detect_builder_kernel_version(root_img: &Path) -> Result<String> {
    let output = command_stdout(
        ProcessCommand::new("strings").arg("-a").arg(root_img),
        format!(
            "failed to scan builder root image for kernel version: {}",
            root_img.display()
        ),
    )?;
    let mut versions = output
        .lines()
        .filter_map(|line| line.strip_prefix("vmlinuz-"))
        .filter(|version| {
            version
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '+'))
        })
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    versions.sort();
    versions.dedup();
    versions.pop().ok_or_else(|| {
        PetriError::Cli(format!(
            "failed to detect Debian kernel version in {}",
            root_img.display()
        ))
    })
}

fn write_builder_build_info(
    staging: &Path,
    source: &VerifiedBuilderSource,
    disk_size: u64,
) -> Result<()> {
    let payload = serde_json::json!({
        "schema": BUILDER_IMAGE_VERSION,
        "kind": "petri-builder",
        "architecture": "aarch64",
        "boot_mode": "efi",
        "upstream_source": source.source,
        "upstream_cache_path": source.path,
        "upstream_checksum": {
            "algorithm": source.checksum_algorithm,
            "hex": source.checksum_hex,
        },
        "provisioned_packages": BUILDER_PACKAGES,
        "disk_size_bytes": disk_size,
        "petri_git": git_revision_info().unwrap_or_else(|err| serde_json::json!({ "error": err.to_string() })),
        "build_timestamp_unix_ms": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or_default(),
    });
    let path = staging.join("build-info.json");
    fs::write(
        &path,
        serde_json::to_string_pretty(&payload)
            .map_err(|err| PetriError::Cli(format!("failed to encode build info: {err}")))?,
    )
    .map_err(|source| PetriError::Io { path, source })
}

fn git_revision_info() -> Result<serde_json::Value> {
    let repo = repo_root()?;
    let revision = command_stdout(
        ProcessCommand::new("git")
            .arg("rev-parse")
            .arg("HEAD")
            .current_dir(&repo),
        "failed to read git revision".to_string(),
    )?;
    let status = command_stdout(
        ProcessCommand::new("git")
            .arg("status")
            .arg("--porcelain")
            .current_dir(&repo),
        "failed to read git status".to_string(),
    )?;
    Ok(serde_json::json!({
        "revision": revision.trim(),
        "dirty": !status.trim().is_empty(),
    }))
}

fn write_sha256sums(dir: &Path, files: &[&str]) -> Result<()> {
    let mut output = String::new();
    for file in files {
        let checksum = file_checksum(&dir.join(file), "sha256")?;
        output.push_str(&format!("{checksum}  {file}\n"));
    }
    let path = dir.join("SHA256SUMS");
    fs::write(&path, output).map_err(|source| PetriError::Io { path, source })
}

fn run_status(command: &mut ProcessCommand, message: String) -> Result<()> {
    let status = command.status().map_err(|source| PetriError::Io {
        path: PathBuf::from(command.get_program()),
        source,
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(PetriError::Cli(format!("{message}: {status}")))
    }
}

fn command_stdout(command: &mut ProcessCommand, message: String) -> Result<String> {
    let output = command.output().map_err(|source| PetriError::Io {
        path: PathBuf::from(command.get_program()),
        source,
    })?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(PetriError::Cli(format!(
            "{message}: {}",
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

fn parse_disk_size(value: &str) -> Result<u64> {
    let value = value.trim();
    let (digits, multiplier) = match value.as_bytes().last().copied() {
        Some(b'G') | Some(b'g') => (&value[..value.len() - 1], 1024_u64.pow(3)),
        Some(b'M') | Some(b'm') => (&value[..value.len() - 1], 1024_u64.pow(2)),
        Some(b'K') | Some(b'k') => (&value[..value.len() - 1], 1024_u64),
        _ => (value, 1),
    };
    let number = digits
        .parse::<u64>()
        .map_err(|_| PetriError::InvalidArgument {
            flag: "--disk-size",
            value: value.to_string(),
            message: "expected bytes or a K, M, or G suffix".to_string(),
        })?;
    number
        .checked_mul(multiplier)
        .ok_or_else(|| PetriError::InvalidArgument {
            flag: "--disk-size",
            value: value.to_string(),
            message: "size is too large".to_string(),
        })
}

fn expand_file(path: &Path, size: u64) -> Result<()> {
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|source| PetriError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    let current = file.metadata().map_err(|source| PetriError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if current.len() < size {
        file.set_len(size).map_err(|source| PetriError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}

fn is_url(value: &str) -> bool {
    value.starts_with("https://") || value.starts_with("http://")
}

fn url_file_name(url: &str) -> Result<String> {
    url.rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| PetriError::Cli(format!("URL has no file name: {url}")))
}

fn run_linux_image_build(command: ImageBuildCommand) -> Result<String> {
    let script = image_build_script();
    let mut process = ProcessCommand::new(&script);

    append_image_build_script_args(&mut process, &command);

    let status = process.status().map_err(|source| PetriError::Io {
        path: script.clone(),
        source,
    })?;

    if !status.success() {
        return Err(PetriError::Cli(format!("image build failed with {status}")));
    }

    Ok("image build completed".to_string())
}

fn append_image_build_script_args(process: &mut ProcessCommand, command: &ImageBuildCommand) {
    if let Some(config) = &command.config {
        process.arg("--config").arg(config);
    }
    if let Some(out_dir) = &command.out_dir {
        process.arg("--out-dir").arg(out_dir);
    }
    if let Some(arch) = &command.arch {
        process.arg("--arch").arg(arch);
    }
    if let Some(debian_arch) = &command.debian_arch {
        process.arg("--debian-arch").arg(debian_arch);
    }
    if let Some(target) = &command.target {
        process.arg("--target").arg(target);
    }
    if let Some(disk_size) = &command.disk_size {
        process.arg("--disk-size").arg(disk_size);
    }
    if command.skip_guest_build {
        process.arg("--skip-guest-build");
    }
    if let Some(guest_binary) = &command.guest_binary {
        process.arg("--guest-binary").arg(guest_binary);
    }
}

fn selected_image_builder(builder: ImageBuilder) -> Result<ImageBuilder> {
    match builder {
        ImageBuilder::Auto if cfg!(target_os = "macos") => Ok(ImageBuilder::Vm),
        ImageBuilder::Auto => Ok(ImageBuilder::Linux),
        ImageBuilder::Vm if !cfg!(target_os = "macos") => Err(PetriError::Cli(
            "the VM image builder is currently supported only on macOS".to_string(),
        )),
        builder => Ok(builder),
    }
}

fn run_vm_image_build(command: ImageBuildCommand, backend: &impl HostBackend) -> Result<String> {
    if command.skip_guest_build || command.guest_binary.is_some() {
        return Err(PetriError::Cli(
            "the VM image builder builds petri-guest on the host and passes it into the builder; do not pass --skip-guest-build or --guest-binary".to_string(),
        ));
    }

    let builder_image = command
        .builder_image
        .clone()
        .or_else(|| std::env::var_os("PETRI_BUILDER_IMAGE").map(PathBuf::from))
        .ok_or_else(|| {
            PetriError::Cli(
                "macOS image builds require --builder-image <bundle> or PETRI_BUILDER_IMAGE"
                    .to_string(),
            )
        })?;

    let repo_root = repo_root()?;
    let target = command
        .target
        .clone()
        .unwrap_or_else(|| configured_guest_target(command.config.as_deref()));
    build_host_guest_binary(&repo_root, &target)?;

    let guest_binary = repo_root
        .join("target")
        .join(&target)
        .join("release")
        .join("petri-guest");
    let guest_binary_in_vm = guest_path_for_repo_file(&repo_root, &guest_binary)?;
    let out_dir = absolute_out_dir(
        &repo_root,
        command
            .out_dir
            .clone()
            .unwrap_or_else(|| repo_root.join("target").join("petri-images").join("base")),
    );
    let staged_out_dir = repo_root
        .join("target")
        .join("petri-builder-output")
        .join(unique_build_id()?);
    let staged_out_dir_in_vm = guest_path_for_repo_file(&repo_root, &staged_out_dir)?;

    if staged_out_dir.exists() {
        fs::remove_dir_all(&staged_out_dir).map_err(|source| PetriError::Io {
            path: staged_out_dir.clone(),
            source,
        })?;
    }
    fs::create_dir_all(&staged_out_dir).map_err(|source| PetriError::Io {
        path: staged_out_dir.clone(),
        source,
    })?;

    let policy = write_builder_policy(&repo_root)?;
    let instance_id = InstanceId::new(format!("petri-builder-{}", unique_build_id()?))?;
    let config = InstanceConfig::new(
        instance_id.clone(),
        "macos",
        repo_root.clone(),
        policy.clone(),
    )
    .with_image(builder_image);

    let _ = backend.teardown(&instance_id);
    backend.create(config)?;

    let dispatch = DispatchRequest::bash_command(
        "image-build",
        "bash",
        vec![
            "-lc".to_string(),
            vm_build_script(&command, &staged_out_dir_in_vm, &guest_binary_in_vm)?,
        ],
        PathBuf::from("/workspace"),
        BTreeMap::new(),
        None,
        Some(RequestLimits {
            timeout_ms: Some(3 * 60 * 60 * 1000),
            max_output_bytes: Some(4 * 1024 * 1024),
        }),
    );

    let result = backend.dispatch(&instance_id, dispatch);
    let _ = backend.teardown(&instance_id);
    result.and_then(|result| {
        if result.status == crate::dispatch::Status::Success && result.exit_code == Some(Some(0)) {
            replace_dir(&staged_out_dir, &out_dir)?;
            Ok(format!("image build completed: {}", out_dir.display()))
        } else {
            Err(PetriError::Cli(format!(
                "VM image build failed: status={:?} exit_code={:?}\nstdout:\n{}\nstderr:\n{}",
                result.status,
                result.exit_code,
                result.stdout.unwrap_or_default(),
                result.stderr.unwrap_or_default()
            )))
        }
    })
}

fn configured_guest_target(config: Option<&Path>) -> String {
    let config = config
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root_fallback().join("images/base/petri-base-image.toml"));

    fs::read_to_string(config)
        .ok()
        .and_then(|input| read_toml_scalar(&input, "target"))
        .unwrap_or_else(|| "aarch64-unknown-linux-musl".to_string())
}

fn read_toml_scalar(input: &str, key: &str) -> Option<String> {
    input.lines().find_map(|line| {
        let (candidate, value) = line.split_once('=')?;
        (candidate.trim() == key).then(|| value.trim().trim_matches('"').to_string())
    })
}

fn absolute_out_dir(repo_root: &Path, out_dir: PathBuf) -> PathBuf {
    if out_dir.is_absolute() {
        out_dir
    } else {
        repo_root.join(out_dir)
    }
}

fn build_host_guest_binary(repo_root: &Path, target: &str) -> Result<()> {
    let mut rustup = ProcessCommand::new("rustup");
    rustup.arg("target").arg("add").arg(target);
    let status = rustup.status().map_err(|source| PetriError::Io {
        path: PathBuf::from("rustup"),
        source,
    })?;
    if !status.success() {
        return Err(PetriError::Cli(format!(
            "failed to install Rust target {target}: {status}"
        )));
    }

    let mut cargo = ProcessCommand::new("cargo");
    configure_guest_target_linker(&mut cargo, target);
    cargo
        .arg("build")
        .arg("-p")
        .arg("petri-guest")
        .arg("--release")
        .arg("--target")
        .arg(target)
        .current_dir(repo_root);
    let status = cargo.status().map_err(|source| PetriError::Io {
        path: PathBuf::from("cargo"),
        source,
    })?;
    if !status.success() {
        return Err(PetriError::Cli(format!(
            "failed to build petri-guest for {target}: {status}"
        )));
    }

    Ok(())
}

fn configure_guest_target_linker(command: &mut ProcessCommand, target: &str) {
    if target == "aarch64-unknown-linux-musl"
        && std::env::var_os("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER").is_none()
    {
        command.env("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER", "rust-lld");
    }
}

fn vm_build_script(
    command: &ImageBuildCommand,
    staged_out_dir: &Path,
    guest_binary: &Path,
) -> Result<String> {
    let mut args = vec![
        "scripts/build-base-image.sh".to_string(),
        "--out-dir".to_string(),
        shell_quote(staged_out_dir),
        "--skip-guest-build".to_string(),
        "--guest-binary".to_string(),
        shell_quote(guest_binary),
    ];

    if let Some(config) = &command.config {
        args.push("--config".to_string());
        args.push(shell_quote(&host_path_to_guest_repo_path(config)?));
    }
    if let Some(arch) = &command.arch {
        args.push("--arch".to_string());
        args.push(shell_quote_str(arch));
    }
    if let Some(debian_arch) = &command.debian_arch {
        args.push("--debian-arch".to_string());
        args.push(shell_quote_str(debian_arch));
    }
    if let Some(target) = &command.target {
        args.push("--target".to_string());
        args.push(shell_quote_str(target));
    }
    if let Some(disk_size) = &command.disk_size {
        args.push("--disk-size".to_string());
        args.push(shell_quote_str(disk_size));
    }

    Ok(format!(
        "set -euo pipefail; rm -f /etc/resolv.conf; printf 'nameserver 1.1.1.1\\nnameserver 8.8.8.8\\noptions timeout:2 attempts:3\\n' > /etc/resolv.conf; export DEBIAN_FRONTEND=noninteractive; apt-get update -o Acquire::Retries=5; apt-get install -y --no-install-recommends gdisk; export TMPDIR=/var/tmp/petri-builder-tmp; mkdir -p \"$TMPDIR\"; {}",
        args.join(" ")
    ))
}

fn write_builder_policy(repo_root: &Path) -> Result<PathBuf> {
    let policy_dir = repo_root.join("target").join("petri-builder");
    fs::create_dir_all(&policy_dir).map_err(|source| PetriError::Io {
        path: policy_dir.clone(),
        source,
    })?;
    let policy = policy_dir.join("policy.toml");
    fs::write(
        &policy,
        r#"[policy]
network_enabled = true
max_runtime_secs = 10800
max_output_bytes = 4194304
workspace_path = "/workspace"
# The builder is a trusted provisioning context: its commands write /etc, install
# packages, and run mmdebstrap, so they must keep root. Disable the per-command
# privilege drop that untrusted sandboxes use by default. See ADR 0002.
drop_privileges = false

# The image builder runs arbitrary provisioning shell, so it declares the yolo
# command level explicitly rather than smuggling `*` in through an allowlisted
# shell. See docs/adr/0002-policy-modes-and-runtime-mode-switching.md.
[policy.command]
default = "yolo"
max = "yolo"
"#,
    )
    .map_err(|source| PetriError::Io {
        path: policy.clone(),
        source,
    })?;
    Ok(policy)
}

fn replace_dir(from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        fs::remove_dir_all(to).map_err(|source| PetriError::Io {
            path: to.to_path_buf(),
            source,
        })?;
    }
    copy_dir(from, to)
}

fn atomic_replace_dir(from: &Path, to: &Path) -> Result<()> {
    let parent = to
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let backup = parent.join(format!(
        ".{}-old-{}",
        to.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("petri-builder"),
        unique_build_id()?
    ));

    if to.exists() {
        fs::rename(to, &backup).map_err(|source| PetriError::Io {
            path: to.to_path_buf(),
            source,
        })?;
    }

    match fs::rename(from, to) {
        Ok(()) => {
            if backup.exists() {
                fs::remove_dir_all(&backup).map_err(|source| PetriError::Io {
                    path: backup,
                    source,
                })?;
            }
            Ok(())
        }
        Err(source) => {
            if backup.exists() {
                let _ = fs::rename(&backup, to);
            }
            Err(PetriError::Io {
                path: from.to_path_buf(),
                source,
            })
        }
    }
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).map_err(|source| PetriError::Io {
        path: to.to_path_buf(),
        source,
    })?;
    for entry in fs::read_dir(from).map_err(|source| PetriError::Io {
        path: from.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| PetriError::Io {
            path: from.to_path_buf(),
            source,
        })?;
        let source_path = entry.path();
        let dest_path = to.join(entry.file_name());
        if source_path.is_dir() {
            copy_dir(&source_path, &dest_path)?;
        } else {
            fs::copy(&source_path, &dest_path).map_err(|source| PetriError::Io {
                path: dest_path,
                source,
            })?;
        }
    }
    Ok(())
}

fn host_path_to_guest_repo_path(path: &Path) -> Result<PathBuf> {
    let repo_root = repo_root()?;
    let host_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_root.join(path)
    };
    guest_path_for_repo_file(&repo_root, &host_path)
}

fn guest_path_for_repo_file(repo_root: &Path, path: &Path) -> Result<PathBuf> {
    let relative = path.strip_prefix(repo_root).map_err(|_| {
        PetriError::Cli(format!(
            "VM image builder can only use paths under the repo workspace: {}",
            path.display()
        ))
    })?;
    Ok(PathBuf::from("/workspace").join(relative))
}

fn repo_root() -> Result<PathBuf> {
    fs::canonicalize(repo_root_fallback()).map_err(|source| PetriError::Io {
        path: repo_root_fallback(),
        source,
    })
}

fn repo_root_fallback() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn unique_build_id() -> Result<String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| PetriError::Cli(format!("system clock is before UNIX epoch: {err}")))?
        .as_millis();
    Ok(format!("{millis}"))
}

fn shell_quote(path: &Path) -> String {
    shell_quote_str(&path.to_string_lossy())
}

fn shell_quote_str(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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
        "  sandbox  create, list, exec, connect to, and kill sandboxes",
        "  image    build and inspect Petri VM images",
        "",
        "compatibility aliases:",
        "  create    alias for sandbox create with explicit --id",
        "  dispatch  alias for sandbox exec using protocol flags",
        "  stop      stop a running instance",
        "  teardown  alias for sandbox kill <id>",
        "",
        &sandbox_usage(),
        &create_usage(),
        &dispatch_usage(),
        &image_usage(),
        &stop_usage(),
        &teardown_usage(),
    ]
    .join("\n")
}

fn sandbox_usage() -> String {
    [
        "usage: petri sandbox <command> [options]",
        "",
        "commands:",
        "  list     list known local sandboxes",
        "  create   create a sandbox from a template",
        "  connect  connect to a running sandbox",
        "  exec     run a command inside a sandbox",
        "  kill     stop and remove sandbox runtime state",
        "",
        &sandbox_list_usage(),
        &sandbox_create_usage(),
        &sandbox_connect_usage(),
        &sandbox_exec_usage(),
        &sandbox_kill_usage(),
    ]
    .join("\n")
}

fn sandbox_list_usage() -> String {
    "usage: petri sandbox list [--state running|paused] [--metadata key=value,key2=value2] [--limit <n>] [--format pretty|json]".to_string()
}

fn sandbox_create_usage() -> String {
    "usage: petri sandbox create [base] --workspace <path> --policy <path> [--id <id>] [--image <path>] [--backend macos|stub]".to_string()
}

fn sandbox_connect_usage() -> String {
    "usage: petri sandbox connect <sandbox-id>".to_string()
}

fn sandbox_exec_usage() -> String {
    "usage: petri sandbox exec [--cwd <path>] [--env key=value[,key2=value2]] [--timeout-ms <ms>] [--max-output-bytes <bytes>] <sandbox-id> <command> [args...]".to_string()
}

fn sandbox_kill_usage() -> String {
    "usage: petri sandbox kill [--all | <sandbox-id>...]".to_string()
}

fn create_usage() -> String {
    "usage: petri create --id <id> --workspace <path> --policy <path> [--image <path>] [--backend macos|stub]".to_string()
}

fn dispatch_usage() -> String {
    "usage: petri dispatch --id <id> [--tool bash_command|lsp_hover|lsp_definition|lsp_references|lsp_diagnostics|lsp_rename]\n  bash_command: --command <name> --cwd <path> [--arg <value>]...\n  lsp_*: --args-json '<json args object>'\n  common: [--request-id <id>] [--timeout-ms <ms>] [--max-output-bytes <bytes>]".to_string()
}

fn image_usage() -> String {
    format!(
        "usage: petri image <command> [options]\n\ncommands:\n  build  {}\n\n{}",
        image_build_usage(),
        "Set PETRI_IMAGE_BUILD_SCRIPT to override the bundled builder path."
    )
}

fn image_build_usage() -> String {
    "usage: petri image build [--builder auto|linux|vm] [--builder-image <bundle>] [--prepare-builder] [--builder-source <url-or-path>] [--builder-source-sha256 <hex>|--builder-source-checksums <path-or-url>] [--builder-cache-dir <path>] [--config <path>] [--out-dir <path>] [--arch <arch>] [--debian-arch <arch>] [--target <triple>] [--disk-size <size>] [--skip-guest-build --guest-binary <path>]".to_string()
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
        assert_eq!(command.request.tool.as_deref(), Some("bash_command"));
        assert_eq!(command.request.limits.unwrap().timeout_ms, Some(1000));
        assert!(!command.stdin_passthrough);
    }

    #[test]
    fn parses_sandbox_create_base_template() {
        let command = parse(args(&[
            "sandbox",
            "create",
            "base",
            "--id",
            "dev-1",
            "--workspace",
            "/workspace",
            "--policy",
            "policy.toml",
        ]))
        .unwrap();

        let Command::Create(command) = command else {
            panic!("expected create command");
        };

        assert_eq!(command.output, CreateOutput::SandboxId);
        assert_eq!(command.config.id.as_str(), "dev-1");
        assert_eq!(command.config.workspace, PathBuf::from("/workspace"));
        assert_eq!(command.config.policy, PathBuf::from("policy.toml"));
        assert!(command.config.image.is_some());
    }

    #[test]
    fn parses_sandbox_exec_with_options_and_args() {
        let command = parse(args(&[
            "sandbox",
            "exec",
            "dev-1",
            "--cwd",
            "/workspace",
            "--env",
            "FOO=bar,BAZ=qux",
            "--timeout-ms",
            "1000",
            "ls",
            "-la",
        ]))
        .unwrap();

        let Command::Dispatch(command) = command else {
            panic!("expected dispatch command");
        };

        assert_eq!(command.instance_id.as_str(), "dev-1");
        assert!(command.stdin_passthrough);
        assert_eq!(command.request.tool.as_deref(), Some("bash_command"));
        assert_eq!(command.request.limits.unwrap().timeout_ms, Some(1000));
        let args = command.request.args.unwrap();
        assert_eq!(args["command"], "ls");
        assert_eq!(args["argv"], serde_json::json!(["-la"]));
        assert_eq!(args["cwd"], "/workspace");
        assert_eq!(args["env"]["FOO"], "bar");
        assert_eq!(args["env"]["BAZ"], "qux");
    }

    #[test]
    fn parses_sandbox_list_json() {
        let command = parse(args(&[
            "sandbox", "list", "--state", "running", "--limit", "10", "--format", "json",
        ]))
        .unwrap();

        let Command::SandboxList(command) = command else {
            panic!("expected sandbox list command");
        };

        assert_eq!(command.state, Some(SandboxStateFilter::Running));
        assert_eq!(command.limit, Some(10));
        assert_eq!(command.format, OutputFormat::Json);
    }

    #[test]
    fn parses_sandbox_kill_ids() {
        let command = parse(args(&["sandbox", "kill", "dev-1", "dev-2"])).unwrap();

        let Command::SandboxKill(command) = command else {
            panic!("expected sandbox kill command");
        };

        assert!(!command.all);
        assert_eq!(command.instance_ids.len(), 2);
        assert_eq!(command.instance_ids[0].as_str(), "dev-1");
        assert_eq!(command.instance_ids[1].as_str(), "dev-2");
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

        assert!(err.contains("supported tools"));
    }

    #[test]
    fn parses_lsp_dispatch_with_args_json() {
        let command = parse(args(&[
            "dispatch",
            "--id",
            "dev-1",
            "--tool",
            "lsp_hover",
            "--args-json",
            r#"{"file":"/workspace/src/main.rs","line":42,"col":15}"#,
        ]))
        .unwrap();

        let Command::Dispatch(command) = command else {
            panic!("expected dispatch command");
        };
        assert_eq!(command.request.tool.as_deref(), Some("lsp_hover"));
        let args = command.request.args.expect("lsp args");
        assert_eq!(args["file"], "/workspace/src/main.rs");
        assert_eq!(args["line"], 42);
        assert_eq!(args["col"], 15);
    }

    #[test]
    fn rejects_lsp_dispatch_without_args_json() {
        let err = parse(args(&["dispatch", "--id", "dev-1", "--tool", "lsp_hover"]))
            .unwrap_err()
            .to_string();

        assert!(err.contains("--args-json"));
    }

    #[test]
    fn rejects_lsp_dispatch_with_invalid_args_json() {
        let err = parse(args(&[
            "dispatch",
            "--id",
            "dev-1",
            "--tool",
            "lsp_hover",
            "--args-json",
            "not json",
        ]))
        .unwrap_err()
        .to_string();

        assert!(err.contains("JSON"));
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
        assert_eq!(command.builder, ImageBuilder::Auto);
        assert_eq!(command.builder_image, None);
        assert!(!command.prepare_builder);
    }

    #[test]
    fn parses_image_build_builder_options() {
        let command = parse(args(&[
            "image",
            "build",
            "--builder",
            "vm",
            "--builder-image",
            "target/petri-builder",
            "--builder-source",
            "debian.raw",
            "--builder-source-sha256",
            "abc123",
            "--builder-source-checksums",
            "SHA256SUMS",
            "--builder-cache-dir",
            "target/builder-cache",
            "--prepare-builder",
        ]))
        .unwrap();

        let Command::ImageBuild(command) = command else {
            panic!("expected image build command");
        };

        assert_eq!(command.builder, ImageBuilder::Vm);
        assert_eq!(
            command.builder_image,
            Some(PathBuf::from("target/petri-builder"))
        );
        assert_eq!(command.builder_source.as_deref(), Some("debian.raw"));
        assert_eq!(command.builder_source_sha256.as_deref(), Some("abc123"));
        assert_eq!(
            command.builder_source_checksums.as_deref(),
            Some("SHA256SUMS")
        );
        assert_eq!(
            command.builder_cache_dir,
            Some(PathBuf::from("target/builder-cache"))
        );
        assert!(command.prepare_builder);
    }

    #[test]
    fn parses_checksum_file_entry() {
        let input = "abc  ignored\n0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  debian.raw\n";

        assert_eq!(
            parse_checksum_file(input, "debian.raw"),
            Some((
                "sha256".to_string(),
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string()
            ))
        );
    }

    #[test]
    fn parses_disk_size_suffixes() {
        assert_eq!(parse_disk_size("16G").unwrap(), 16 * 1024 * 1024 * 1024);
        assert_eq!(parse_disk_size("512M").unwrap(), 512 * 1024 * 1024);
    }

    #[test]
    fn parses_hdiutil_attach_output() {
        let output = "/dev/disk8              GUID_partition_scheme\n/dev/disk8s1            B921B045-1DF0-41C3-AF44-4C6F280\n/dev/disk8s15           EFI\n";

        let attach = parse_hdiutil_attach(output).unwrap();

        assert_eq!(attach.disk, "/dev/disk8");
        assert_eq!(attach.efi_partition, "/dev/disk8s15");
    }

    #[test]
    fn url_cache_file_name_includes_full_url() {
        assert_eq!(
            url_cache_file_name("https://cloud.debian.org/images/cloud/trixie/latest/SHA512SUMS")
                .unwrap(),
            "https___cloud.debian.org_images_cloud_trixie_latest_SHA512SUMS"
        );
    }

    #[test]
    fn rejects_unknown_image_builder() {
        let err = parse(args(&["image", "build", "--builder", "container"]))
            .unwrap_err()
            .to_string();

        assert!(err.contains("expected auto, linux, or vm"));
    }
}
