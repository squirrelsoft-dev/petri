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
    Image(ImageCommand),
    SandboxList(SandboxListCommand),
    SandboxConnect(InstanceCommand),
    SandboxKill(SandboxKillCommand),
    SandboxBootstrap(SandboxBootstrapCommand),
    SandboxCreateFromBase(SandboxCreateFromBaseCommand),
    Policy(PolicyCommand),
    Internal(InternalCommand),
    Stop(InstanceCommand),
    Teardown(InstanceCommand),
}

/// A `petri policy <subcommand>` operating on the named policy-template
/// registry (built-in defaults plus user templates under `~/.petri/policies`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyCommand {
    List,
    Show {
        name: String,
    },
    Path {
        name: String,
    },
    Create {
        name: String,
        from: Option<String>,
        force: bool,
    },
    Edit {
        name: String,
    },
    Remove {
        name: String,
    },
}

/// `petri sandbox create <id> --base <name>:<tag> …`: boot a sandbox from a
/// frozen layer. Orchestrated at run time (ensure per-sandbox scratch, spawn
/// the detached NBD daemon, then boot via Linux direct boot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxCreateFromBaseCommand {
    pub id: InstanceId,
    pub base: String,
    pub workspace: PathBuf,
    pub policy: PathBuf,
    pub metadata: BTreeMap<String, String>,
}

/// Hidden `petri internal …` subcommands used by petri to drive its own
/// out-of-process helpers (not part of the public CLI surface).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InternalCommand {
    /// Persistent NBD service for a sandbox's layered disk. Spawned detached by
    /// `sandbox create` so the export outlives the create command; holds an
    /// advisory `flock` (single-writer) and serves until terminated.
    ServeNbd {
        image: String,
        port_file: PathBuf,
        lock_file: PathBuf,
    },
}

/// `petri sandbox create --bootstrap <name>:scratch --disk <nocloud> …`: boot a
/// disposable builder VM with the image scratch attached as an NBD data disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxBootstrapCommand {
    pub id: InstanceId,
    pub image: String,
    pub disk: PathBuf,
    pub provision: Option<PathBuf>,
    pub auto_freeze: bool,
    pub tag: Option<String>,
}

/// A `petri image <subcommand>` operating on the named-image registry (distinct
/// from the legacy `petri image build` bundle pipeline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageCommand {
    Create {
        name: String,
        base: Option<String>,
        size_gib: Option<u64>,
        /// When set, run the bootstrap loop against this nocloud EFI image and
        /// seal the result as a frozen base layer rather than creating a bare
        /// scratch. Mutually exclusive with `--base`.
        from_nocloud: Option<PathBuf>,
        /// Frozen-layer tag produced by `--from-nocloud` (default: "base").
        tag: Option<String>,
        /// Provision script to stage into the artifacts share for
        /// `--from-nocloud`; defaults to the built-in trixie provisioner.
        provision: Option<PathBuf>,
    },
    List,
    Inspect {
        reference: String,
    },
    Freeze {
        reference: String,
        tag: String,
        provision: Option<PathBuf>,
        force: bool,
    },
    Stop {
        reference: String,
    },
    Delete {
        reference: String,
        force: bool,
    },
    ShowProvision {
        reference: String,
    },
    Rebuild {
        reference: String,
        base: String,
        tag: String,
        disk: PathBuf,
    },
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
    /// Also delete each sandbox's per-sandbox scratch image (for `--base`
    /// sandboxes), discarding its structure entirely.
    pub purge: bool,
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
        Command::Create(mut command) => {
            command.config.policy = crate::policy::resolve_reference(
                &crate::policy::policies_root(),
                &command.config.policy,
            )?;
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
        Command::Image(command) => run_image_command(command, backend),
        Command::SandboxList(command) => run_sandbox_list(command, backend),
        Command::SandboxConnect(command) => run_sandbox_connect(command, backend),
        Command::SandboxKill(command) => run_sandbox_kill(command, backend),
        Command::SandboxBootstrap(command) => run_sandbox_bootstrap(command, backend),
        Command::SandboxCreateFromBase(mut command) => {
            command.policy =
                crate::policy::resolve_reference(&crate::policy::policies_root(), &command.policy)?;
            run_sandbox_create_from_base(command, backend)
        }
        Command::Policy(command) => run_policy_command(command),
        Command::Internal(InternalCommand::ServeNbd {
            image,
            port_file,
            lock_file,
        }) => run_internal_serve_nbd(&image, &port_file, &lock_file),
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
        "policy" => parse_policy(args),
        "internal" => parse_internal(args),
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
        "create" => parse_image_create(args),
        "list" => parse_image_list(args),
        "inspect" => parse_image_inspect(args),
        "freeze" => parse_image_freeze(args),
        "stop" => parse_image_stop(args),
        "delete" => parse_image_delete(args),
        "show-provision" => parse_image_show_provision(args),
        "rebuild" => parse_image_rebuild(args),
        "--help" | "-h" | "help" => Err(PetriError::Cli(image_usage())),
        _ => Err(PetriError::Cli(format!(
            "unknown image command '{subcommand}'\n{}",
            image_usage()
        ))),
    }
}

fn parse_policy(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let Some(subcommand) = args.next() else {
        return Err(PetriError::Cli(policy_usage()));
    };

    match subcommand.as_str() {
        "list" => {
            if let Some(arg) = args.next() {
                if arg == "--help" || arg == "-h" {
                    return Err(PetriError::Cli(policy_usage()));
                }
                return Err(PetriError::Cli(format!(
                    "unexpected policy list argument '{arg}'"
                )));
            }
            Ok(Command::Policy(PolicyCommand::List))
        }
        "show" => Ok(Command::Policy(PolicyCommand::Show {
            name: policy_name_arg(args, "show")?,
        })),
        "path" => Ok(Command::Policy(PolicyCommand::Path {
            name: policy_name_arg(args, "path")?,
        })),
        "edit" => Ok(Command::Policy(PolicyCommand::Edit {
            name: policy_name_arg(args, "edit")?,
        })),
        "remove" | "rm" => Ok(Command::Policy(PolicyCommand::Remove {
            name: policy_name_arg(args, "remove")?,
        })),
        "create" => parse_policy_create(args),
        "--help" | "-h" | "help" => Err(PetriError::Cli(policy_usage())),
        _ => Err(PetriError::Cli(format!(
            "unknown policy command '{subcommand}'\n{}",
            policy_usage()
        ))),
    }
}

/// Parse a policy subcommand that takes exactly one positional `<name>`.
fn policy_name_arg(args: impl Iterator<Item = String>, sub: &str) -> Result<String> {
    let mut name = None;
    for arg in args {
        match arg.as_str() {
            "--help" | "-h" => return Err(PetriError::Cli(policy_usage())),
            _ if arg.starts_with('-') => {
                return Err(PetriError::Cli(format!(
                    "unknown policy {sub} argument '{arg}'"
                )));
            }
            _ => {
                if name.replace(arg.clone()).is_some() {
                    return Err(PetriError::Cli(format!(
                        "unexpected policy {sub} argument '{arg}'"
                    )));
                }
            }
        }
    }
    name.ok_or_else(|| {
        PetriError::Cli(format!(
            "policy {sub} requires a <name>\n{}",
            policy_usage()
        ))
    })
}

fn parse_policy_create(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let mut name = None;
    let mut from = None;
    let mut force = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--from" => from = Some(next_arg(&mut args, "--from")?),
            "--force" => force = true,
            "--help" | "-h" => return Err(PetriError::Cli(policy_usage())),
            _ if arg.starts_with('-') => {
                return Err(PetriError::Cli(format!(
                    "unknown policy create argument '{arg}'"
                )));
            }
            _ => {
                if name.replace(arg.clone()).is_some() {
                    return Err(PetriError::Cli(format!(
                        "unexpected policy create argument '{arg}'"
                    )));
                }
            }
        }
    }
    let name = name.ok_or_else(|| {
        PetriError::Cli(format!(
            "policy create requires a <name>\n{}",
            policy_usage()
        ))
    })?;
    Ok(Command::Policy(PolicyCommand::Create { name, from, force }))
}

fn parse_image_create(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let mut name = None;
    let mut base = None;
    let mut size_gib = None;
    let mut from_nocloud = None;
    let mut tag = None;
    let mut provision = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--base" => base = Some(next_arg(&mut args, "--base")?),
            "--size" => size_gib = Some(parse_u64(next_arg(&mut args, "--size")?, "--size")?),
            "--from-nocloud" => {
                from_nocloud = Some(PathBuf::from(next_arg(&mut args, "--from-nocloud")?))
            }
            "--tag" => tag = Some(next_arg(&mut args, "--tag")?),
            "--provision" => provision = Some(PathBuf::from(next_arg(&mut args, "--provision")?)),
            "--help" | "-h" => return Err(PetriError::Cli(image_create_usage())),
            _ if arg.starts_with('-') => {
                return Err(PetriError::Cli(format!(
                    "unknown image create argument '{arg}'"
                )));
            }
            _ => {
                if name.replace(arg.clone()).is_some() {
                    return Err(PetriError::Cli(format!(
                        "unexpected image create argument '{arg}'"
                    )));
                }
            }
        }
    }

    let name = name.ok_or(PetriError::MissingArgument { flag: "<name>" })?;
    Ok(Command::Image(ImageCommand::Create {
        name,
        base,
        size_gib,
        from_nocloud,
        tag,
        provision,
    }))
}

fn parse_image_list(mut args: impl Iterator<Item = String>) -> Result<Command> {
    if let Some(arg) = args.next() {
        if matches!(arg.as_str(), "--help" | "-h") {
            return Err(PetriError::Cli("usage: petri image list".to_string()));
        }
        return Err(PetriError::Cli(format!(
            "unexpected image list argument '{arg}'"
        )));
    }
    Ok(Command::Image(ImageCommand::List))
}

fn parse_image_inspect(args: impl Iterator<Item = String>) -> Result<Command> {
    let reference = single_image_ref(args, "inspect")?;
    Ok(Command::Image(ImageCommand::Inspect { reference }))
}

fn parse_image_freeze(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let mut reference = None;
    let mut tag = None;
    let mut provision = None;
    let mut force = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--tag" => tag = Some(next_arg(&mut args, "--tag")?),
            "--provision" => provision = Some(PathBuf::from(next_arg(&mut args, "--provision")?)),
            "--force" => force = true,
            "--help" | "-h" => {
                return Err(PetriError::Cli(image_freeze_usage()));
            }
            _ if arg.starts_with('-') => {
                return Err(PetriError::Cli(format!(
                    "unknown image freeze argument '{arg}'"
                )));
            }
            _ => {
                if reference.replace(arg.clone()).is_some() {
                    return Err(PetriError::Cli(format!(
                        "unexpected image freeze argument '{arg}'"
                    )));
                }
            }
        }
    }

    let reference = reference.ok_or(PetriError::MissingArgument {
        flag: "<name>:scratch",
    })?;
    let tag = tag.ok_or(PetriError::MissingArgument { flag: "--tag" })?;
    Ok(Command::Image(ImageCommand::Freeze {
        reference,
        tag,
        provision,
        force,
    }))
}

fn parse_image_stop(args: impl Iterator<Item = String>) -> Result<Command> {
    let reference = single_image_ref(args, "stop")?;
    Ok(Command::Image(ImageCommand::Stop { reference }))
}

fn parse_image_show_provision(args: impl Iterator<Item = String>) -> Result<Command> {
    let reference = single_image_ref(args, "show-provision")?;
    Ok(Command::Image(ImageCommand::ShowProvision { reference }))
}

fn parse_image_rebuild(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let mut reference = None;
    let mut base = None;
    let mut tag = None;
    let mut disk = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--base" => base = Some(next_arg(&mut args, "--base")?),
            "--tag" => tag = Some(next_arg(&mut args, "--tag")?),
            "--disk" => disk = Some(PathBuf::from(next_arg(&mut args, "--disk")?)),
            "--help" | "-h" => return Err(PetriError::Cli(image_rebuild_usage())),
            _ if arg.starts_with('-') => {
                return Err(PetriError::Cli(format!(
                    "unknown image rebuild argument '{arg}'"
                )));
            }
            _ => {
                if reference.replace(arg.clone()).is_some() {
                    return Err(PetriError::Cli(format!(
                        "unexpected image rebuild argument '{arg}'"
                    )));
                }
            }
        }
    }
    let reference = reference.ok_or(PetriError::MissingArgument {
        flag: "<name>:<tag>",
    })?;
    let base = base.ok_or(PetriError::MissingArgument { flag: "--base" })?;
    let tag = tag.ok_or(PetriError::MissingArgument { flag: "--tag" })?;
    let disk = disk.ok_or(PetriError::MissingArgument { flag: "--disk" })?;
    Ok(Command::Image(ImageCommand::Rebuild {
        reference,
        base,
        tag,
        disk,
    }))
}

fn parse_image_delete(args: impl Iterator<Item = String>) -> Result<Command> {
    let mut reference = None;
    let mut force = false;
    for arg in args {
        match arg.as_str() {
            "--force" => force = true,
            "--help" | "-h" => {
                return Err(PetriError::Cli(
                    "usage: petri image delete <name>:<tag> [--force]".to_string(),
                ));
            }
            _ if arg.starts_with('-') => {
                return Err(PetriError::Cli(format!(
                    "unknown image delete argument '{arg}'"
                )));
            }
            _ => {
                if reference.replace(arg.clone()).is_some() {
                    return Err(PetriError::Cli(format!(
                        "unexpected image delete argument '{arg}'"
                    )));
                }
            }
        }
    }
    let reference = reference.ok_or(PetriError::MissingArgument {
        flag: "<name>:<tag>",
    })?;
    Ok(Command::Image(ImageCommand::Delete { reference, force }))
}

/// Parse a subcommand that takes exactly one positional `<name>:<tag>` argument.
fn single_image_ref(args: impl Iterator<Item = String>, subcommand: &str) -> Result<String> {
    let mut reference = None;
    for arg in args {
        match arg.as_str() {
            "--help" | "-h" => {
                return Err(PetriError::Cli(format!(
                    "usage: petri image {subcommand} <name>:<tag>"
                )));
            }
            _ if arg.starts_with('-') => {
                return Err(PetriError::Cli(format!(
                    "unknown image {subcommand} argument '{arg}'"
                )));
            }
            _ => {
                if reference.replace(arg.clone()).is_some() {
                    return Err(PetriError::Cli(format!(
                        "unexpected image {subcommand} argument '{arg}'"
                    )));
                }
            }
        }
    }
    reference.ok_or(PetriError::MissingArgument {
        flag: "<name>:<tag>",
    })
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

fn parse_internal(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let Some(subcommand) = args.next() else {
        return Err(PetriError::Cli(
            "usage: petri internal serve-nbd …".to_string(),
        ));
    };
    match subcommand.as_str() {
        "serve-nbd" => parse_internal_serve_nbd(args),
        _ => Err(PetriError::Cli(format!(
            "unknown internal command '{subcommand}'"
        ))),
    }
}

fn parse_internal_serve_nbd(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let mut image = None;
    let mut port_file = None;
    let mut lock_file = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--image" => image = Some(next_arg(&mut args, "--image")?),
            "--port-file" => port_file = Some(PathBuf::from(next_arg(&mut args, "--port-file")?)),
            "--lock-file" => lock_file = Some(PathBuf::from(next_arg(&mut args, "--lock-file")?)),
            other => {
                return Err(PetriError::Cli(format!(
                    "unexpected serve-nbd argument '{other}'"
                )));
            }
        }
    }
    Ok(Command::Internal(InternalCommand::ServeNbd {
        image: image.ok_or(PetriError::MissingArgument { flag: "--image" })?,
        port_file: port_file.ok_or(PetriError::MissingArgument {
            flag: "--port-file",
        })?,
        lock_file: lock_file.ok_or(PetriError::MissingArgument {
            flag: "--lock-file",
        })?,
    }))
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
    let mut metadata = BTreeMap::new();
    let mut bootstrap = None;
    let mut base = None;
    let mut disk = None;
    let mut data_disk = None;
    let mut provision = None;
    let mut auto_freeze = false;
    let mut tag = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--id" => id = Some(InstanceId::new(next_arg(&mut args, "--id")?)?),
            "--backend" => backend = next_arg(&mut args, "--backend")?,
            "--image" => image = Some(PathBuf::from(next_arg(&mut args, "--image")?)),
            "--workspace" => workspace = Some(PathBuf::from(next_arg(&mut args, "--workspace")?)),
            "--policy" => policy = Some(PathBuf::from(next_arg(&mut args, "--policy")?)),
            "--metadata" => metadata.extend(parse_key_value_list(
                next_arg(&mut args, "--metadata")?,
                "--metadata",
            )?),
            "--bootstrap" => bootstrap = Some(next_arg(&mut args, "--bootstrap")?),
            "--base" => base = Some(next_arg(&mut args, "--base")?),
            "--disk" => disk = Some(PathBuf::from(next_arg(&mut args, "--disk")?)),
            "--data-disk" => data_disk = Some(next_arg(&mut args, "--data-disk")?),
            "--provision" => provision = Some(PathBuf::from(next_arg(&mut args, "--provision")?)),
            "--auto-freeze" => auto_freeze = true,
            "--tag" => tag = Some(next_arg(&mut args, "--tag")?),
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

    // Boot a sandbox from a frozen layer: per-sandbox scratch over the base,
    // served by a detached NBD daemon, booted via Linux direct boot. The
    // positional name is the sandbox id (it names the persistent scratch);
    // `--id` overrides it.
    if let Some(base) = base {
        if image.is_some() {
            return Err(PetriError::Cli(
                "--base and --image are mutually exclusive".to_string(),
            ));
        }
        let id = match (id, template) {
            (Some(id), _) => id,
            (None, Some(name)) => InstanceId::new(name)?,
            (None, None) => {
                return Err(PetriError::Cli(
                    "sandbox create --base requires a sandbox name (e.g. 'petri sandbox create my-sbx --base debian:trixie')".to_string(),
                ));
            }
        };
        let workspace = workspace.ok_or(PetriError::MissingArgument {
            flag: "--workspace",
        })?;
        let policy = policy.ok_or(PetriError::MissingArgument { flag: "--policy" })?;
        return Ok(Command::SandboxCreateFromBase(
            SandboxCreateFromBaseCommand {
                id,
                base,
                workspace,
                policy,
                metadata,
            },
        ));
    }

    let id = match id {
        Some(id) => id,
        None => InstanceId::new(format!("petri-{}", unique_build_id()?))?,
    };

    // The bootstrap builder is a self-contained path (no ImageBundle/workspace):
    // boot a disposable EFI VM, attach the scratch over NBD, optionally provision
    // and seal.
    if let Some(image) = bootstrap {
        if auto_freeze && provision.is_none() {
            return Err(PetriError::Cli(
                "--auto-freeze requires --provision".to_string(),
            ));
        }
        if auto_freeze && tag.is_none() {
            return Err(PetriError::Cli("--auto-freeze requires --tag".to_string()));
        }
        let disk = disk.ok_or(PetriError::MissingArgument { flag: "--disk" })?;
        return Ok(Command::SandboxBootstrap(SandboxBootstrapCommand {
            id,
            image,
            disk,
            provision,
            auto_freeze,
            tag,
        }));
    }

    // Long-lived NBD data-disk attach needs a persistent daemon to host the
    // server beyond this one-shot CLI invocation — out of scope for now.
    if data_disk.is_some() {
        return Err(PetriError::Cli(
            "long-lived NBD data disk attach requires a persistent daemon (not yet implemented); use --bootstrap --auto-freeze for image builds".to_string(),
        ));
    }

    let workspace = workspace.ok_or(PetriError::MissingArgument {
        flag: "--workspace",
    })?;
    let policy = policy.ok_or(PetriError::MissingArgument { flag: "--policy" })?;
    let mut config = InstanceConfig::new(id, backend, workspace, policy).with_metadata(metadata);
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

fn parse_sandbox_kill(args: impl Iterator<Item = String>) -> Result<Command> {
    let mut all = false;
    let mut purge = false;
    let mut instance_ids = Vec::new();

    for arg in args {
        match arg.as_str() {
            "--all" => all = true,
            "--purge" => purge = true,
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
        purge,
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
        instances.retain(|instance| instance.matches_metadata(&command.metadata));
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

fn run_sandbox_connect(command: InstanceCommand, backend: &impl HostBackend) -> Result<String> {
    // Connecting confirms the sandbox exists and is running without tearing it
    // down on exit. Interactive PTY attach is deferred (#27/#29 v1), so this is
    // a non-interactive readiness check that reports the live handle.
    let handle = backend
        .list()?
        .into_iter()
        .find(|instance| instance.id == command.instance_id)
        .ok_or_else(|| PetriError::Cli(format!("no sandbox with id '{}'", command.instance_id)))?;

    if !handle.state.is_running() {
        return Err(PetriError::Cli(format!(
            "sandbox '{}' is not running (state: {})",
            handle.id,
            handle.state.as_str()
        )));
    }

    Ok(format!(
        "connected to sandbox {} (backend {}, state {})",
        handle.id,
        handle.backend,
        handle.state.as_str()
    ))
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

    let images_root = crate::image::images_root();
    for instance_id in &instance_ids {
        // Stop the VM first so it quiesces before its disk server goes away,
        // then tear down the per-sandbox NBD daemon (for `--base` sandboxes).
        backend.teardown(instance_id)?;
        teardown_sandbox_nbd(&images_root, instance_id.as_str(), command.purge);
    }

    Ok(format!("killed {} sandbox(s)", instance_ids.len()))
}

/// Tear down the detached NBD daemon backing a `--base` sandbox: SIGTERM the
/// recorded PID (releasing its flock), clear serving bookkeeping, and — under
/// `purge` — delete the per-sandbox scratch image entirely. Best-effort and a
/// no-op for bundle-based sandboxes (no daemon, no per-sandbox image).
fn teardown_sandbox_nbd(images_root: &Path, sandbox: &str, purge: bool) {
    let paths = crate::image::ImagePaths::new(images_root, sandbox);
    if !paths.exists() {
        return;
    }
    let pid_file = paths.dir.join("nbd.pid");
    if let Ok(pid_str) = fs::read_to_string(&pid_file)
        && let Ok(pid) = pid_str.trim().parse::<u32>()
    {
        let _ = kill_pid(pid);
    }
    let _ = fs::remove_file(&pid_file);
    let _ = fs::remove_file(paths.dir.join("nbd.port"));
    let _ = crate::image::clear_serving(images_root, sandbox, sandbox);
    if purge {
        let _ = fs::remove_dir_all(&paths.dir);
    }
}

/// Orchestrate a bootstrap builder: serve the scratch over NBD, drive a
/// disposable EFI builder VM (via the backend), then — under `--auto-freeze` —
/// seal the live scratch in-process. The `NbdHandle` is held for the whole
/// sequence so the VM can reach the server and the seal sees the live index.
fn run_sandbox_bootstrap(
    command: SandboxBootstrapCommand,
    backend: &impl HostBackend,
) -> Result<String> {
    use crate::image;
    let images_root = image::images_root();
    let (name, scratch_tag) = image::parse_image_ref(&command.image)?;
    if scratch_tag != image::SCRATCH_TAG {
        return Err(PetriError::invalid_argument(format!(
            "--bootstrap operates on the scratch overlay; expected '{name}:scratch', got '{}'",
            command.image
        )));
    }

    let provision_script = match &command.provision {
        Some(path) => Some(fs::read_to_string(path).map_err(|source| PetriError::Io {
            path: path.clone(),
            source,
        })?),
        None => None,
    };

    // Start the in-process NBD server exporting the scratch; the handle keeps it
    // alive until dropped at the end of this function.
    let handle = image::serve_scratch(&images_root, &name)?;
    let url = handle.url().to_string();
    let sandbox_id = command.id.as_str().to_string();
    if let Some(port) = image::nbd_port_from_url(&url) {
        image::mark_serving(&images_root, &name, port, &sandbox_id)?;
    }

    let nocloud_disk = fs::canonicalize(&command.disk).unwrap_or_else(|_| command.disk.clone());
    let params = crate::backend::BootstrapBuilderParams {
        instance_id: command.id.clone(),
        nocloud_disk,
        data_disk_url: url,
        provision_script: provision_script.clone(),
        run_provision: command.auto_freeze,
        ready_timeout_secs: 1800,
    };

    let run = match backend.run_bootstrap_builder(params) {
        Ok(run) => run,
        Err(err) => {
            let _ = image::clear_serving(&images_root, &name, &sandbox_id);
            return Err(err);
        }
    };

    if command.auto_freeze {
        if run.succeeded {
            // record_frozen_layer rolls a fresh scratch (clearing nbd_port), so
            // no separate clear_serving is needed on success.
            let tag = command
                .tag
                .expect("--auto-freeze requires --tag (validated at parse)");
            return image::auto_freeze(
                &images_root,
                &name,
                &handle,
                &tag,
                image::LayerProvenance {
                    provision_script,
                    boot: None,
                },
            );
        }
        let _ = image::clear_serving(&images_root, &name, &sandbox_id);
        return Err(PetriError::Cli(format!(
            "provision failed (exit {:?}); {name}:scratch left unfrozen{}",
            run.provision_exit_code,
            run.provision_output
                .map(|out| format!("\n{out}"))
                .unwrap_or_default()
        )));
    }

    let _ = image::clear_serving(&images_root, &name, &sandbox_id);
    Ok(format!(
        "bootstrap builder for '{name}:scratch' finished; scratch left unfrozen (use --auto-freeze --tag <tag> to seal)"
    ))
}

fn find_nocloud_kernel_version(image: &Path) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};

    const ROOT_OFFSET: u64 = 134_217_728;
    const SCAN_LEN: usize = 64 * 1024 * 1024;
    const MARKER: &[u8] = b"vmlinuz-";

    let mut f = fs::File::open(image).map_err(|source| PetriError::Io {
        path: image.to_path_buf(),
        source,
    })?;
    f.seek(SeekFrom::Start(ROOT_OFFSET))
        .map_err(|source| PetriError::Io {
            path: image.to_path_buf(),
            source,
        })?;
    let mut data = vec![0u8; SCAN_LEN];
    let n = f.read(&mut data).map_err(|source| PetriError::Io {
        path: image.to_path_buf(),
        source,
    })?;
    data.truncate(n);

    let mut offset = 0;
    while offset + MARKER.len() <= data.len() {
        match data[offset..]
            .windows(MARKER.len())
            .position(|w| w == MARKER)
        {
            None => break,
            Some(rel) => {
                let start = offset + rel + MARKER.len();
                let ver_bytes: Vec<u8> = data[start..]
                    .iter()
                    .copied()
                    .take_while(|&b| {
                        b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'+'
                    })
                    .collect();
                let ver = String::from_utf8_lossy(&ver_bytes).to_string();
                if ver.contains('.') && ver.len() >= 5 {
                    return Ok(ver);
                }
                offset += rel + 1;
            }
        }
    }

    Err(PetriError::Cli(format!(
        "vmlinuz version not found in {} (searched {} MiB at root partition)",
        image.display(),
        SCAN_LEN >> 20
    )))
}

fn parse_hdiutil_attach_output(stdout: &str) -> Option<(String, String)> {
    let mut base_disk: Option<String> = None;
    let mut esp_dev: Option<String> = None;

    for line in stdout.lines() {
        let mut cols = line.split_whitespace();
        let dev = match cols.next() {
            Some(d) if d.starts_with("/dev/disk") => d,
            _ => continue,
        };
        let suffix = &dev["/dev/disk".len()..];
        if base_disk.is_none() && !suffix.contains('s') {
            base_disk = Some(dev.to_string());
            continue;
        }
        let content = cols.next().unwrap_or("");
        if esp_dev.is_none()
            && (content.eq_ignore_ascii_case("efi")
                || content.to_ascii_uppercase().starts_with("C12A7328"))
        {
            esp_dev = Some(dev.to_string());
        }
    }

    match (base_disk, esp_dev) {
        (Some(b), Some(e)) => Some((b, e)),
        _ => None,
    }
}

fn patch_grub_cfg(nocloud_src: &Path, mount_point: &str) -> Result<()> {
    let grub_path = PathBuf::from(mount_point).join("EFI/debian/grub.cfg");

    let cfg = fs::read_to_string(&grub_path).map_err(|source| PetriError::Io {
        path: grub_path.clone(),
        source,
    })?;
    let uuid = cfg
        .lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            if parts.next() == Some("search.fs_uuid") {
                parts.next().map(str::to_string)
            } else {
                None
            }
        })
        .ok_or_else(|| {
            PetriError::Cli(format!(
                "search.fs_uuid not found in {}",
                grub_path.display()
            ))
        })?;

    let kver = find_nocloud_kernel_version(nocloud_src)?;

    let new_cfg = format!(
        "search.fs_uuid {uuid} root\n\
         set prefix=($root)/boot/grub\n\
         insmod part_gpt\n\
         insmod ext2\n\
         insmod linux\n\
         linux ($root)/boot/vmlinuz-{kver} root=UUID={uuid} rw console=hvc0 \
         systemd.firstboot=0 \
         systemd.mount-extra=petri-artifacts:/mnt/petri-artifacts:virtiofs \
         systemd.run=/mnt/petri-artifacts/provision.sh \
         systemd.run_success_action=poweroff \
         systemd.run_failure_action=poweroff\n\
         initrd ($root)/boot/initrd.img-{kver}\n\
         boot\n"
    );

    fs::write(&grub_path, new_cfg.as_bytes()).map_err(|source| PetriError::Io {
        path: grub_path.clone(),
        source,
    })?;

    Ok(())
}

fn patch_nocloud_esp_mounted(nocloud_src: &Path, esp_dev: &str) -> Result<()> {
    let status = ProcessCommand::new("diskutil")
        .args(["mount", esp_dev])
        .status()
        .map_err(|source| PetriError::Io {
            path: PathBuf::from("diskutil"),
            source,
        })?;
    if !status.success() {
        return Err(PetriError::Cli(format!("diskutil mount {esp_dev} failed")));
    }

    let info = ProcessCommand::new("diskutil")
        .args(["info", esp_dev])
        .output()
        .map_err(|source| PetriError::Io {
            path: PathBuf::from("diskutil"),
            source,
        })?;
    let info_text = String::from_utf8_lossy(&info.stdout).to_string();
    let mount_point = info_text
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("Mount Point:")
                .map(|v| v.trim().to_string())
        })
        .filter(|mp| !mp.is_empty())
        .ok_or_else(|| {
            PetriError::Cli(format!(
                "Mount Point not found in diskutil info:\n{info_text}"
            ))
        })?;

    let result = patch_grub_cfg(nocloud_src, &mount_point);

    let _ = ProcessCommand::new("diskutil")
        .args(["unmount", esp_dev])
        .output();

    result
}

fn patch_nocloud_esp(nocloud_src: &Path, patched_out: &Path) -> Result<()> {
    let status = ProcessCommand::new("cp")
        .arg("-c")
        .arg(nocloud_src)
        .arg(patched_out)
        .status()
        .map_err(|source| PetriError::Io {
            path: PathBuf::from("cp"),
            source,
        })?;
    if !status.success() {
        return Err(PetriError::Cli(format!(
            "cp -c {} -> {} failed: {:?}",
            nocloud_src.display(),
            patched_out.display(),
            status.code()
        )));
    }

    let out = ProcessCommand::new("hdiutil")
        .args([
            "attach",
            "-imagekey",
            "diskimage-class=CRawDiskImage",
            "-nomount",
        ])
        .arg(patched_out)
        .output()
        .map_err(|source| PetriError::Io {
            path: PathBuf::from("hdiutil"),
            source,
        })?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return Err(PetriError::Cli(format!("hdiutil attach failed: {msg}")));
    }
    let attach_stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let (base_disk, esp_dev) = parse_hdiutil_attach_output(&attach_stdout).ok_or_else(|| {
        PetriError::Cli(format!(
            "could not parse hdiutil attach output:\n{attach_stdout}"
        ))
    })?;

    let result = patch_nocloud_esp_mounted(nocloud_src, &esp_dev);

    let _ = ProcessCommand::new("hdiutil")
        .args(["detach", "-force", &base_disk])
        .output();

    result
}

const DEFAULT_PROVISION_SCRIPT: &str = include_str!("../../petri-nbd/examples/provision.sh");

/// `petri image create --from-nocloud <disk>`: drive petri-vz directly (no
/// MacosBackend). Stages provision.sh into a temp artifacts dir, boots the
/// nocloud EFI VM with `--data-disk` (NBD scratch) and `--artifacts-dir`,
/// waits for petri-vz to exit via `--exit-on-guest-stop`, then seals.
/// Shared boot-and-seal path for both `image create --from-nocloud` and
/// `image rebuild`: serves the named image's scratch over in-process NBD,
/// patches the nocloud EFI boot disk to inject `systemd.run=provision.sh`,
/// waits for petri-vz to exit via `--exit-on-guest-stop`, then seals the
/// scratch as a frozen layer tagged `tag`.
fn run_nocloud_provision_and_seal(
    images_root: &Path,
    name: &str,
    provision_script: String,
    nocloud_disk: PathBuf,
    tag: &str,
) -> Result<String> {
    use crate::image;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let handle = image::serve_scratch(images_root, name)?;
    let url = handle.url().to_string();
    let sandbox_id = format!("petri-build-{}", unique_build_id()?);
    if let Some(port) = image::nbd_port_from_url(&url) {
        image::mark_serving(images_root, name, port, &sandbox_id)?;
    }

    let instance_id = format!("petri-build-{}", unique_build_id()?);
    let instance_dir = crate::backend::instances_dir().join(&instance_id);
    let artifacts_dir = instance_dir.join("artifacts");
    let workspace_dir = instance_dir.join("workspace");
    let config_dir = instance_dir.join("config");
    let console_log = instance_dir.join("guest-console.log");
    let helper_stderr = instance_dir.join("petri-vz.stderr.log");
    let helper_stdout = instance_dir.join("petri-vz.stdout.log");
    let efi_store = instance_dir.join("efi-variable-store");
    // Short temp-dir path — the 104-byte sun_path limit, see
    // `backend::short_control_socket_path`.
    let control_sock = crate::backend::short_control_socket_path(&instance_id);

    for dir in [&artifacts_dir, &workspace_dir, &config_dir] {
        fs::create_dir_all(dir).map_err(|source| PetriError::Io {
            path: dir.clone(),
            source,
        })?;
    }

    let script_path = artifacts_dir.join("provision.sh");
    fs::write(&script_path, &provision_script).map_err(|source| PetriError::Io {
        path: script_path.clone(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755));
    }

    // Copy petri-guest into the artifacts share so provision scripts can install
    // it into the target rootfs. Best-effort: skip silently if not found.
    let guest_bin: PathBuf = std::env::var_os("PETRI_GUEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("petri-guest"));
    if guest_bin.is_file() {
        let dst = artifacts_dir.join("petri-guest");
        let _ = fs::copy(&guest_bin, &dst);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dst, fs::Permissions::from_mode(0o755));
        }
    }

    let helper = crate::backend::resolve_petri_vz()?;
    let nocloud_disk = fs::canonicalize(&nocloud_disk).unwrap_or(nocloud_disk);

    let patched_disk = instance_dir.join("boot.raw");
    patch_nocloud_esp(&nocloud_disk, &patched_disk)?;

    let stdout_file = fs::File::create(&helper_stdout).map_err(|source| PetriError::Io {
        path: helper_stdout.clone(),
        source,
    })?;
    let stderr_file = fs::File::create(&helper_stderr).map_err(|source| PetriError::Io {
        path: helper_stderr.clone(),
        source,
    })?;

    let mut child = ProcessCommand::new(&helper)
        .arg("--instance-id")
        .arg(&instance_id)
        .arg("--control-socket")
        .arg(&control_sock)
        .arg("--boot-mode")
        .arg("efi")
        .arg("--disk")
        .arg(&patched_disk)
        .arg("--efi-variable-store")
        .arg(&efi_store)
        .arg("--data-disk")
        .arg(&url)
        .arg("--artifacts-dir")
        .arg(&artifacts_dir)
        .arg("--workspace")
        .arg(&workspace_dir)
        .arg("--config-dir")
        .arg(&config_dir)
        .arg("--console-log")
        .arg(&console_log)
        .arg("--enable-network")
        .arg("--exit-on-guest-stop")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .map_err(|source| PetriError::Io {
            path: helper.clone(),
            source,
        })?;

    let timeout_secs = 3600u64;
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let exit_status = loop {
        match child.try_wait().map_err(|source| PetriError::Io {
            path: helper.clone(),
            source,
        })? {
            Some(status) => break status,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = image::clear_serving(images_root, name, &sandbox_id);
                    return Err(PetriError::Cli(format!(
                        "bootstrap builder for '{name}' timed out after {timeout_secs}s (console: {})",
                        console_log.display()
                    )));
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    };

    let _ = fs::remove_file(&patched_disk);

    if exit_status.success() {
        // provision.sh copies the guest kernel/initrd into the writable
        // workspace share; if present, seal them with the layer so it can boot
        // as a sandbox via Linux direct boot.
        let kernel = workspace_dir.join("vmlinuz");
        let initrd = workspace_dir.join("initrd");
        let boot = (kernel.is_file() && initrd.is_file()).then(|| image::BootFiles {
            kernel,
            initrd,
            cmdline: SANDBOX_KERNEL_CMDLINE.to_string(),
        });
        image::auto_freeze(
            images_root,
            name,
            &handle,
            tag,
            image::LayerProvenance {
                provision_script: Some(provision_script),
                boot,
            },
        )
    } else {
        let _ = image::clear_serving(images_root, name, &sandbox_id);
        Err(PetriError::Cli(format!(
            "bootstrap builder for '{name}' exited with failure (console: {})",
            console_log.display()
        )))
    }
}

/// Kernel command line for booting a frozen layer as a sandbox. The rootfs is a
/// bare ext4 served as the virtio-block boot disk (`vda`), with the console on
/// the virtio console (`hvc0`).
const SANDBOX_KERNEL_CMDLINE: &str = "root=/dev/vda rw console=hvc0";

/// `petri internal serve-nbd`: serve a sandbox's layered disk over NBD until
/// terminated. Holds a non-blocking advisory `flock` (single-writer guard) for
/// the process lifetime, publishes the NBD URL to `port_file`, then parks. Runs
/// as a detached child of `sandbox create` so the export outlives that command;
/// `sandbox kill` SIGTERMs it, which drops the handle + lock. Unix-only (the
/// microVM backends are Unix hosts).
#[cfg(unix)]
fn run_internal_serve_nbd(image: &str, port_file: &Path, lock_file: &Path) -> Result<String> {
    use std::os::unix::io::AsRawFd;

    // 1. Single-writer guard. Non-blocking: a held lock means this sandbox is
    //    already running, so report that rather than blocking.
    let lock = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_file)
        .map_err(|source| PetriError::Io {
            path: lock_file.to_path_buf(),
            source,
        })?;
    // SAFETY: `lock` owns a valid open fd for the call; LOCK_NB makes flock
    // return immediately with EWOULDBLOCK if another holder exists.
    let rc = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(PetriError::Cli(format!(
            "sandbox '{image}' is already running (NBD service lock held)"
        )));
    }

    // 2. Serve the layered disk (frozen base read-only + this sandbox's scratch
    //    read-write). The handle keeps the in-process server alive.
    let images_root = crate::image::images_root();
    let handle = crate::image::serve_scratch(&images_root, image)?;
    let url = handle.url().to_string();

    // 3. Publish the URL atomically (write-then-rename) so the parent can detect
    //    readiness and read the port.
    let tmp = port_file.with_extension("tmp");
    fs::write(&tmp, &url).map_err(|source| PetriError::Io {
        path: tmp.clone(),
        source,
    })?;
    fs::rename(&tmp, port_file).map_err(|source| PetriError::Io {
        path: port_file.to_path_buf(),
        source,
    })?;

    // 4. Park until terminated. `handle` and `lock` stay owned by this frame, so
    //    process exit (SIGTERM from `sandbox kill`) tears down the server and
    //    releases the advisory lock.
    let _held = (handle, lock);
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

#[cfg(not(unix))]
fn run_internal_serve_nbd(_image: &str, _port_file: &Path, _lock_file: &Path) -> Result<String> {
    Err(PetriError::Cli(
        "sandbox NBD service requires a Unix host".to_string(),
    ))
}

/// `petri sandbox create <id> --base <name>:<tag>`: boot a sandbox from a frozen
/// layer. Ensures a per-sandbox scratch over the base, spawns the detached NBD
/// daemon serving it, then boots the VM via Linux direct boot through the normal
/// backend create path. The scratch is ephemeral (fresh each boot) until the
/// storage layer gains scratch persistence.
fn run_sandbox_create_from_base(
    command: SandboxCreateFromBaseCommand,
    backend: &impl HostBackend,
) -> Result<String> {
    use crate::image;
    let images_root = image::images_root();
    let sandbox = command.id.as_str().to_string();
    let (base_name, base_tag) = image::parse_image_ref(&command.base)?;

    // 1. The base must be a frozen, sandbox-bootable layer (kernel captured).
    let (kernel, initrd, cmdline) = image::boot_files_for(&images_root, &base_name, &base_tag)?;

    // 2. Ensure this sandbox's scratch sits over the frozen base.
    let paths = image::ImagePaths::new(&images_root, &sandbox);
    if paths.exists() {
        image::reset_scratch_over_base(&images_root, &sandbox, &base_name, &base_tag)?;
    } else {
        image::create(&images_root, &sandbox, Some((&base_name, &base_tag)), None)?;
    }

    // 3. Spawn the detached NBD daemon and wait for its URL.
    let (nbd_url, daemon_pid) = spawn_nbd_daemon(&sandbox, &paths.dir)?;
    if let Some(port) = image::nbd_port_from_url(&nbd_url) {
        let _ = image::mark_serving(&images_root, &sandbox, port, &sandbox);
    }

    // 4. Boot via the normal backend create path (Linux direct boot).
    let config = InstanceConfig::new(
        command.id.clone(),
        "macos",
        command.workspace,
        command.policy,
    )
    .with_metadata(command.metadata)
    .with_direct_boot(crate::instance::DirectBoot {
        nbd_url,
        kernel,
        initrd,
        cmdline,
    });

    match backend.create(config) {
        Ok(handle) => Ok(handle.id.to_string()),
        Err(err) => {
            // Boot failed: tear the daemon down so the lock/port don't leak.
            let _ = kill_pid(daemon_pid);
            let _ = image::clear_serving(&images_root, &sandbox, &sandbox);
            Err(err)
        }
    }
}

/// Spawn `petri internal serve-nbd` detached, serving `sandbox`'s layered disk.
/// Returns the published NBD URL and the daemon PID once it is ready. The daemon
/// outlives this process; `sandbox kill` terminates it via the recorded PID.
fn spawn_nbd_daemon(sandbox: &str, image_dir: &Path) -> Result<(String, u32)> {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let exe = std::env::current_exe().map_err(|source| PetriError::Io {
        path: PathBuf::from("<current-exe>"),
        source,
    })?;
    let lock_file = image_dir.join("nbd.lock");
    let port_file = image_dir.join("nbd.port");
    let pid_file = image_dir.join("nbd.pid");
    let log_path = image_dir.join("nbd.log");
    let _ = fs::remove_file(&port_file);

    let log = fs::File::create(&log_path).map_err(|source| PetriError::Io {
        path: log_path.clone(),
        source,
    })?;
    let log_err = log.try_clone().map_err(|source| PetriError::Io {
        path: log_path.clone(),
        source,
    })?;

    let mut cmd = ProcessCommand::new(&exe);
    cmd.arg("internal")
        .arg("serve-nbd")
        .arg("--image")
        .arg(sandbox)
        .arg("--port-file")
        .arg(&port_file)
        .arg("--lock-file")
        .arg(&lock_file)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|source| PetriError::Io {
        path: exe.clone(),
        source,
    })?;

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().map_err(|source| PetriError::Io {
            path: exe.clone(),
            source,
        })? {
            let reason = fs::read_to_string(&log_path).unwrap_or_default();
            return Err(PetriError::Cli(format!(
                "NBD service for '{sandbox}' exited early ({status}): {}",
                reason.trim()
            )));
        }
        if let Ok(url) = fs::read_to_string(&port_file) {
            let url = url.trim().to_string();
            if !url.is_empty() {
                let _ = fs::write(&pid_file, child.id().to_string());
                return Ok((url, child.id()));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(PetriError::Cli(format!(
                "NBD service for '{sandbox}' did not become ready within 20s (log: {})",
                log_path.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Send SIGTERM to `pid` (best-effort daemon shutdown). Unix-only.
fn kill_pid(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        // SAFETY: kill(2) with a pid and signal number has no memory-safety
        // preconditions; an invalid pid just returns an error we ignore.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    }
    #[cfg(not(unix))]
    let _ = pid;
    Ok(())
}

fn run_image_create_from_nocloud(
    images_root: &Path,
    name: &str,
    nocloud_disk: PathBuf,
    tag: &str,
    provision_path: Option<&Path>,
    size_gib: Option<u64>,
    base: Option<&str>,
) -> Result<String> {
    use crate::image;

    image::validate_freeze_tag(tag)?;
    let base_ref = base.map(crate::image::parse_image_ref).transpose()?;
    let base_tuple = base_ref.as_ref().map(|(n, t)| (n.as_str(), t.as_str()));
    image::create(images_root, name, base_tuple, size_gib)?;

    let provision_script = match provision_path {
        Some(path) => fs::read_to_string(path).map_err(|source| PetriError::Io {
            path: path.to_path_buf(),
            source,
        })?,
        None => DEFAULT_PROVISION_SCRIPT.to_string(),
    };

    run_nocloud_provision_and_seal(images_root, name, provision_script, nocloud_disk, tag)
}

fn run_policy_command(command: PolicyCommand) -> Result<String> {
    let root = crate::policy::policies_root();
    match command {
        PolicyCommand::List => crate::policy::list(&root),
        PolicyCommand::Show { name } => crate::policy::show(&root, &name),
        PolicyCommand::Path { name } => crate::policy::path(&root, &name),
        PolicyCommand::Create { name, from, force } => {
            crate::policy::create(&root, &name, from.as_deref(), force)
        }
        PolicyCommand::Edit { name } => crate::policy::edit(&root, &name),
        PolicyCommand::Remove { name } => crate::policy::remove(&root, &name),
    }
}

fn run_image_command(command: ImageCommand, backend: &impl HostBackend) -> Result<String> {
    let images_root = crate::image::images_root();
    match command {
        ImageCommand::Create {
            name,
            base,
            size_gib,
            from_nocloud,
            tag,
            provision,
        } => {
            if let Some(nocloud_disk) = from_nocloud {
                run_image_create_from_nocloud(
                    &images_root,
                    &name,
                    nocloud_disk,
                    tag.as_deref().unwrap_or("base"),
                    provision.as_deref(),
                    size_gib,
                    base.as_deref(),
                )
            } else {
                let base = base
                    .as_deref()
                    .map(crate::image::parse_image_ref)
                    .transpose()?;
                let base_ref = base.as_ref().map(|(n, t)| (n.as_str(), t.as_str()));
                crate::image::create(&images_root, &name, base_ref, size_gib)
            }
        }
        ImageCommand::List => crate::image::list(&images_root),
        ImageCommand::Inspect { reference } => {
            let (name, tag) = crate::image::parse_image_ref(&reference)?;
            crate::image::inspect(&images_root, &name, &tag)
        }
        ImageCommand::Freeze {
            reference,
            tag,
            provision,
            force,
        } => {
            let (name, scratch_tag) = crate::image::parse_image_ref(&reference)?;
            if scratch_tag != crate::image::SCRATCH_TAG {
                return Err(PetriError::invalid_argument(format!(
                    "freeze operates on the scratch overlay; expected '{name}:scratch', got '{reference}'"
                )));
            }
            crate::image::freeze(&images_root, &name, &tag, provision.as_deref(), force)
        }
        ImageCommand::Stop { reference } => {
            let (name, scratch_tag) = crate::image::parse_image_ref(&reference)?;
            if scratch_tag != crate::image::SCRATCH_TAG {
                return Err(PetriError::invalid_argument(format!(
                    "stop operates on the scratch overlay; expected '{name}:scratch', got '{reference}'"
                )));
            }
            crate::image::stop(&images_root, &name)
        }
        ImageCommand::Delete { reference, force } => {
            let (name, tag) = crate::image::parse_image_ref(&reference)?;
            crate::image::delete(&images_root, &name, &tag, force)
        }
        ImageCommand::ShowProvision { reference } => {
            let (name, tag) = crate::image::parse_image_ref(&reference)?;
            crate::image::show_provision(&images_root, &name, &tag)
        }
        ImageCommand::Rebuild {
            reference,
            base,
            tag,
            disk,
        } => {
            let (name, src_tag) = crate::image::parse_image_ref(&reference)?;
            run_image_rebuild(&images_root, &name, &src_tag, &base, &tag, disk, backend)
        }
    }
}

/// `petri image rebuild <name>:<tag> --base <name>:<tag> --tag <new> --disk
/// <nocloud>`: re-provision a frozen layer from its stored script using the
/// same exit-on-guest-stop path as `image create --from-nocloud`.
fn run_image_rebuild(
    images_root: &Path,
    name: &str,
    src_tag: &str,
    base: &str,
    new_tag: &str,
    disk: PathBuf,
    _backend: &impl HostBackend,
) -> Result<String> {
    use crate::image;
    image::validate_freeze_tag(new_tag)?;
    let script = image::provision_for_rebuild(images_root, name, src_tag)?;
    let (base_name, base_tag) = image::parse_image_ref(base)?;
    image::reset_scratch_over_base(images_root, name, &base_name, &base_tag)?;
    run_nocloud_provision_and_seal(images_root, name, script, disk, new_tag)
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
        if fields.contains(&"EFI") {
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
        "  policy   manage reusable policy templates",
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
        &policy_usage(),
        &stop_usage(),
        &teardown_usage(),
    ]
    .join("\n")
}

fn policy_usage() -> String {
    [
        "usage: petri policy <command> [options]",
        "",
        "commands:",
        "  list    list built-in and user policy templates",
        "  show    print a template's TOML (petri policy show <name>)",
        "  path    print a template's resolved file path (petri policy path <name>)",
        "  create  create a user template (petri policy create <name> [--from <template>] [--force])",
        "  edit    edit a template in $EDITOR, forking a built-in if needed (petri policy edit <name>)",
        "  remove  remove a user template (petri policy remove <name>)",
        "",
        "built-in templates: locked-down, developer, yolo, fetch.",
        "templates resolve by name wherever --policy is accepted,",
        "e.g. 'petri sandbox create trixie --workspace . --policy developer'.",
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
    "usage: petri sandbox create [base] --workspace <path> --policy <path|template> [--id <id>] [--image <path>] [--backend macos|stub] [--metadata key=value,key2=value2]\n       petri sandbox create <name> --base <image>:<tag> --workspace <path> --policy <path|template> [--metadata key=value,...]\n       petri sandbox create --bootstrap <name>:scratch --disk <nocloud> [--provision <path>] [--auto-freeze --tag <tag>]\n       --policy accepts a file path or a template name (see 'petri policy list')".to_string()
}

fn sandbox_connect_usage() -> String {
    "usage: petri sandbox connect <sandbox-id>".to_string()
}

fn sandbox_exec_usage() -> String {
    "usage: petri sandbox exec [--cwd <path>] [--env key=value[,key2=value2]] [--timeout-ms <ms>] [--max-output-bytes <bytes>] <sandbox-id> <command> [args...]".to_string()
}

fn sandbox_kill_usage() -> String {
    "usage: petri sandbox kill [--purge] [--all | <sandbox-id>...]".to_string()
}

fn create_usage() -> String {
    "usage: petri create --id <id> --workspace <path> --policy <path|template> [--image <path>] [--backend macos|stub]".to_string()
}

fn dispatch_usage() -> String {
    "usage: petri dispatch --id <id> [--tool bash_command|lsp_hover|lsp_definition|lsp_references|lsp_diagnostics|lsp_rename]\n  bash_command: --command <name> --cwd <path> [--arg <value>]...\n  lsp_*: --args-json '<json args object>'\n  common: [--request-id <id>] [--timeout-ms <ms>] [--max-output-bytes <bytes>]".to_string()
}

fn image_usage() -> String {
    format!(
        "usage: petri image <command> [options]\n\ncommands:\n  build    {}\n  create   {}\n  list     petri image list\n  inspect  petri image inspect <name>:<tag>\n  freeze   {}\n  stop     petri image stop <name>:scratch\n  delete   petri image delete <name>:<tag> [--force]\n  show-provision  petri image show-provision <name>:<tag>\n  rebuild  {}\n\n{}",
        image_build_usage(),
        image_create_usage(),
        image_freeze_usage(),
        image_rebuild_usage(),
        "Set PETRI_IMAGE_BUILD_SCRIPT to override the bundled builder path."
    )
}

fn image_create_usage() -> String {
    "usage: petri image create <name> [--base <name>:<tag>] [--size <gib>]\n       petri image create <name> --from-nocloud <image.raw> [--tag <tag>] [--provision <script>] [--size <gib>]\n       petri image create <name> --base <name>:<tag> --from-nocloud <image.raw> [--tag <tag>] [--provision <script>]".to_string()
}

fn image_freeze_usage() -> String {
    "usage: petri image freeze <name>:scratch --tag <tag> [--provision <path>] [--force]"
        .to_string()
}

fn image_rebuild_usage() -> String {
    "usage: petri image rebuild <name>:<tag> --base <name>:<tag> --tag <new-tag> --disk <nocloud>"
        .to_string()
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
    use crate::instance::{InstanceHandle, LifecycleState};

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    struct ConnectFakeBackend {
        instances: Vec<InstanceHandle>,
    }

    impl HostBackend for ConnectFakeBackend {
        fn name(&self) -> &str {
            "fake"
        }
        fn create(&self, _config: InstanceConfig) -> Result<InstanceHandle> {
            unimplemented!("create not used in connect tests")
        }
        fn list(&self) -> Result<Vec<InstanceHandle>> {
            Ok(self.instances.clone())
        }
        fn dispatch(
            &self,
            _instance_id: &InstanceId,
            _request: DispatchRequest,
        ) -> Result<crate::dispatch::DispatchResult> {
            unimplemented!("dispatch not used in connect tests")
        }
        fn stop(&self, _instance_id: &InstanceId) -> Result<()> {
            Ok(())
        }
        fn teardown(&self, _instance_id: &InstanceId) -> Result<()> {
            Ok(())
        }
    }

    fn connect_backend(state: crate::instance::LifecycleState) -> ConnectFakeBackend {
        ConnectFakeBackend {
            instances: vec![InstanceHandle {
                id: InstanceId::new("dev-1").unwrap(),
                backend: "macos".to_string(),
                state,
                metadata: BTreeMap::new(),
            }],
        }
    }

    #[test]
    fn parses_sandbox_connect_command() {
        let command = parse(args(&["sandbox", "connect", "dev-1"])).unwrap();
        let Command::SandboxConnect(command) = command else {
            panic!("expected sandbox connect command");
        };
        assert_eq!(command.instance_id.as_str(), "dev-1");
    }

    #[test]
    fn sandbox_connect_reports_running_instance() {
        let backend = connect_backend(LifecycleState::Ready);
        let output = run(args(&["sandbox", "connect", "dev-1"]), &backend).unwrap();
        assert!(output.contains("connected to sandbox dev-1"), "{output}");
    }

    #[test]
    fn sandbox_connect_rejects_stopped_instance() {
        let backend = connect_backend(LifecycleState::TornDown);
        let err = run(args(&["sandbox", "connect", "dev-1"]), &backend)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not running"), "{err}");
    }

    #[test]
    fn sandbox_connect_rejects_unknown_instance() {
        let backend = ConnectFakeBackend { instances: vec![] };
        let err = run(args(&["sandbox", "connect", "missing"]), &backend)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no sandbox with id"), "{err}");
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
    fn parses_sandbox_create_from_base() {
        let command = parse(args(&[
            "sandbox",
            "create",
            "my-sbx",
            "--base",
            "debian:trixie",
            "--workspace",
            "/workspace",
            "--policy",
            "policy.toml",
        ]))
        .unwrap();
        let Command::SandboxCreateFromBase(cmd) = command else {
            panic!("expected sandbox create-from-base command");
        };
        assert_eq!(cmd.id.as_str(), "my-sbx");
        assert_eq!(cmd.base, "debian:trixie");
        assert_eq!(cmd.workspace, PathBuf::from("/workspace"));
        assert_eq!(cmd.policy, PathBuf::from("policy.toml"));
    }

    #[test]
    fn sandbox_create_base_and_image_are_exclusive() {
        let err = parse(args(&[
            "sandbox",
            "create",
            "--base",
            "debian:trixie",
            "--image",
            "/tmp/bundle",
            "--workspace",
            "/workspace",
            "--policy",
            "policy.toml",
        ]))
        .unwrap_err()
        .to_string();
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn parses_internal_serve_nbd() {
        let command = parse(args(&[
            "internal",
            "serve-nbd",
            "--image",
            "my-sbx",
            "--port-file",
            "/tmp/nbd.port",
            "--lock-file",
            "/tmp/nbd.lock",
        ]))
        .unwrap();
        let Command::Internal(InternalCommand::ServeNbd {
            image,
            port_file,
            lock_file,
        }) = command
        else {
            panic!("expected internal serve-nbd command");
        };
        assert_eq!(image, "my-sbx");
        assert_eq!(port_file, PathBuf::from("/tmp/nbd.port"));
        assert_eq!(lock_file, PathBuf::from("/tmp/nbd.lock"));
    }

    #[test]
    fn parses_image_create_with_base_and_size() {
        let command = parse(args(&[
            "image",
            "create",
            "rootfs",
            "--base",
            "other:trixie",
            "--size",
            "16",
        ]))
        .unwrap();
        let Command::Image(ImageCommand::Create {
            name,
            base,
            size_gib,
            from_nocloud,
            tag,
            provision,
        }) = command
        else {
            panic!("expected image create command");
        };
        assert_eq!(name, "rootfs");
        assert_eq!(base.as_deref(), Some("other:trixie"));
        assert_eq!(size_gib, Some(16));
        assert!(from_nocloud.is_none());
        assert!(tag.is_none());
        assert!(provision.is_none());
    }

    #[test]
    fn parses_image_create_from_nocloud() {
        let command = parse(args(&[
            "image",
            "create",
            "base",
            "--from-nocloud",
            "/tmp/debian.raw",
            "--tag",
            "trixie",
        ]))
        .unwrap();
        let Command::Image(ImageCommand::Create {
            name,
            base,
            size_gib,
            from_nocloud,
            tag,
            provision,
        }) = command
        else {
            panic!("expected image create command");
        };
        assert_eq!(name, "base");
        assert!(base.is_none());
        assert!(size_gib.is_none());
        assert_eq!(from_nocloud, Some(PathBuf::from("/tmp/debian.raw")));
        assert_eq!(tag.as_deref(), Some("trixie"));
        assert!(provision.is_none());
    }

    #[test]
    fn image_create_from_nocloud_with_base_parses() {
        let cmd = parse(args(&[
            "image",
            "create",
            "node",
            "--base",
            "base:trixie",
            "--from-nocloud",
            "/tmp/debian.raw",
            "--provision",
            "/tmp/install-node.sh",
            "--tag",
            "trixie",
        ]));
        assert!(cmd.is_ok());
    }

    #[test]
    fn parses_image_freeze_command() {
        let command = parse(args(&[
            "image",
            "freeze",
            "rootfs:scratch",
            "--tag",
            "v1",
            "--force",
        ]))
        .unwrap();
        let Command::Image(ImageCommand::Freeze {
            reference,
            tag,
            provision,
            force,
        }) = command
        else {
            panic!("expected image freeze command");
        };
        assert_eq!(reference, "rootfs:scratch");
        assert_eq!(tag, "v1");
        assert!(provision.is_none());
        assert!(force);
    }

    #[test]
    fn parses_image_rebuild_command() {
        let command = parse(args(&[
            "image", "rebuild", "app:v1", "--base", "app:base", "--tag", "v2", "--disk", "seed.img",
        ]))
        .unwrap();
        let Command::Image(ImageCommand::Rebuild {
            reference,
            base,
            tag,
            disk,
        }) = command
        else {
            panic!("expected image rebuild command");
        };
        assert_eq!(reference, "app:v1");
        assert_eq!(base, "app:base");
        assert_eq!(tag, "v2");
        assert_eq!(disk, PathBuf::from("seed.img"));
    }

    #[test]
    fn parses_sandbox_bootstrap_command() {
        let command = parse(args(&[
            "sandbox",
            "create",
            "--bootstrap",
            "img:scratch",
            "--disk",
            "seed.img",
            "--provision",
            "p.sh",
            "--auto-freeze",
            "--tag",
            "v1",
        ]))
        .unwrap();
        let Command::SandboxBootstrap(cmd) = command else {
            panic!("expected sandbox bootstrap command");
        };
        assert_eq!(cmd.image, "img:scratch");
        assert_eq!(cmd.disk, PathBuf::from("seed.img"));
        assert_eq!(cmd.provision, Some(PathBuf::from("p.sh")));
        assert!(cmd.auto_freeze);
        assert_eq!(cmd.tag.as_deref(), Some("v1"));
    }

    #[test]
    fn sandbox_bootstrap_auto_freeze_requires_provision_and_tag() {
        let err = parse(args(&[
            "sandbox",
            "create",
            "--bootstrap",
            "img:scratch",
            "--disk",
            "s.img",
            "--auto-freeze",
        ]))
        .unwrap_err()
        .to_string();
        assert_eq!(err, "--auto-freeze requires --provision");

        let err = parse(args(&[
            "sandbox",
            "create",
            "--bootstrap",
            "img:scratch",
            "--disk",
            "s.img",
            "--auto-freeze",
            "--provision",
            "p.sh",
        ]))
        .unwrap_err()
        .to_string();
        assert_eq!(err, "--auto-freeze requires --tag");
    }

    #[test]
    fn sandbox_data_disk_attach_is_stubbed() {
        let err = parse(args(&["sandbox", "create", "--data-disk", "img:scratch"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("persistent daemon"), "{err}");
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
    fn parses_sandbox_list_metadata() {
        let command = parse(args(&[
            "sandbox",
            "list",
            "--metadata",
            "env=prod,team=core",
        ]))
        .unwrap();
        let Command::SandboxList(command) = command else {
            panic!("expected sandbox list command");
        };
        assert_eq!(
            command.metadata.get("env").map(String::as_str),
            Some("prod")
        );
        assert_eq!(
            command.metadata.get("team").map(String::as_str),
            Some("core")
        );
    }

    #[test]
    fn parses_sandbox_create_metadata_into_config() {
        let workspace = std::env::temp_dir();
        let command = parse(args(&[
            "sandbox",
            "create",
            "--workspace",
            workspace.to_str().unwrap(),
            "--policy",
            "policy.toml",
            "--id",
            "dev-1",
            "--metadata",
            "env=prod",
        ]))
        .unwrap();
        let Command::Create(command) = command else {
            panic!("expected create command");
        };
        assert_eq!(
            command.config.metadata.get("env").map(String::as_str),
            Some("prod")
        );
    }

    fn list_backend(instances: Vec<InstanceHandle>) -> ConnectFakeBackend {
        ConnectFakeBackend { instances }
    }

    fn handle_with_metadata(id: &str, metadata: &[(&str, &str)]) -> InstanceHandle {
        InstanceHandle {
            id: InstanceId::new(id).unwrap(),
            backend: "macos".to_string(),
            state: LifecycleState::Ready,
            metadata: metadata
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn sandbox_list_filters_by_metadata() {
        let backend = list_backend(vec![
            handle_with_metadata("dev-1", &[("env", "prod")]),
            handle_with_metadata("dev-2", &[("env", "dev")]),
            handle_with_metadata("dev-3", &[]),
        ]);
        let output = run(
            args(&[
                "sandbox",
                "list",
                "--metadata",
                "env=prod",
                "--format",
                "json",
            ]),
            &backend,
        )
        .unwrap();
        assert!(output.contains("dev-1"), "{output}");
        assert!(!output.contains("dev-2"), "{output}");
        assert!(!output.contains("dev-3"), "{output}");
    }

    #[test]
    fn sandbox_list_metadata_no_match_is_empty() {
        let backend = list_backend(vec![handle_with_metadata("dev-1", &[("env", "prod")])]);
        let output = run(
            args(&["sandbox", "list", "--metadata", "env=staging"]),
            &backend,
        )
        .unwrap();
        assert_eq!(output, "no sandboxes");
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
