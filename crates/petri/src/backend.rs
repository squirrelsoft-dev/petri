use crate::dispatch::{DispatchRequest, DispatchResult};
use crate::error::{PetriError, Result};
use petri_protocol::policy::Policy;
use crate::instance::{
    GUEST_WORKSPACE_PATH, InstanceConfig, InstanceHandle, InstanceId, LifecycleState,
};

use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

const MACOS_BACKEND: &str = "macos";
const WORKSPACE_TAG: &str = "workspace";
const CONFIG_TAG: &str = "petri-config";
const GUEST_POLICY_PATH: &str = "/run/petri/policy.toml";
const DEFAULT_DISPATCH_PORT: u32 = 7777;

pub trait HostBackend {
    fn name(&self) -> &str;
    fn create(&self, config: InstanceConfig) -> Result<InstanceHandle>;
    fn list(&self) -> Result<Vec<InstanceHandle>>;
    fn dispatch(
        &self,
        instance_id: &InstanceId,
        request: DispatchRequest,
    ) -> Result<DispatchResult>;
    fn stop(&self, instance_id: &InstanceId) -> Result<()>;
    fn teardown(&self, instance_id: &InstanceId) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct PetriBackend {
    macos: MacosBackend,
    stub: StubBackend,
}

impl Default for PetriBackend {
    fn default() -> Self {
        Self {
            macos: MacosBackend::default(),
            stub: StubBackend,
        }
    }
}

impl HostBackend for PetriBackend {
    fn name(&self) -> &str {
        "petri"
    }

    fn create(&self, config: InstanceConfig) -> Result<InstanceHandle> {
        match config.backend.as_str() {
            MACOS_BACKEND => self.macos.create(config),
            "stub" => self.stub.create(config),
            backend => Err(PetriError::Backend {
                backend: backend.to_string(),
                message: "unknown backend; expected 'macos' or 'stub'".to_string(),
            }),
        }
    }

    fn list(&self) -> Result<Vec<InstanceHandle>> {
        self.macos.list()
    }

    fn dispatch(
        &self,
        instance_id: &InstanceId,
        request: DispatchRequest,
    ) -> Result<DispatchResult> {
        if self.macos.has_instance(instance_id) {
            self.macos.dispatch(instance_id, request)
        } else {
            self.stub.dispatch(instance_id, request)
        }
    }

    fn stop(&self, instance_id: &InstanceId) -> Result<()> {
        if self.macos.has_instance(instance_id) {
            self.macos.stop(instance_id)
        } else {
            self.stub.stop(instance_id)
        }
    }

    fn teardown(&self, instance_id: &InstanceId) -> Result<()> {
        if self.macos.has_instance(instance_id) {
            self.macos.teardown(instance_id)
        } else {
            self.stub.teardown(instance_id)
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StubBackend;

impl HostBackend for StubBackend {
    fn name(&self) -> &str {
        "stub"
    }

    fn create(&self, config: InstanceConfig) -> Result<InstanceHandle> {
        config.validate()?;
        Err(self.unavailable("instance creation"))
    }

    fn list(&self) -> Result<Vec<InstanceHandle>> {
        Ok(Vec::new())
    }

    fn dispatch(
        &self,
        _instance_id: &InstanceId,
        _request: DispatchRequest,
    ) -> Result<DispatchResult> {
        Err(self.unavailable("dispatch"))
    }

    fn stop(&self, _instance_id: &InstanceId) -> Result<()> {
        Err(self.unavailable("stop"))
    }

    fn teardown(&self, _instance_id: &InstanceId) -> Result<()> {
        Err(self.unavailable("teardown"))
    }
}

impl StubBackend {
    fn unavailable(&self, operation: &'static str) -> PetriError {
        PetriError::BackendUnavailable {
            backend: self.name().to_string(),
            operation,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MacosBackend {
    state_dir: PathBuf,
    helper_binary: PathBuf,
    guest_binary: PathBuf,
}

impl Default for MacosBackend {
    fn default() -> Self {
        Self {
            state_dir: default_state_dir(),
            helper_binary: default_helper_binary(),
            guest_binary: default_guest_binary(),
        }
    }
}

impl MacosBackend {
    pub fn new(state_dir: impl Into<PathBuf>, helper_binary: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
            helper_binary: helper_binary.into(),
            guest_binary: default_guest_binary(),
        }
    }

    fn has_instance(&self, instance_id: &InstanceId) -> bool {
        self.state_path(instance_id).is_file()
    }

    fn state_path(&self, instance_id: &InstanceId) -> PathBuf {
        self.instance_dir(instance_id).join("instance.json")
    }

    fn instance_dir(&self, instance_id: &InstanceId) -> PathBuf {
        self.state_dir.join(instance_id.as_str())
    }

    fn control_socket_path(&self, instance_id: &InstanceId) -> PathBuf {
        self.instance_dir(instance_id).join("petri-vz.sock")
    }

    fn config_dir(&self, instance_id: &InstanceId) -> PathBuf {
        self.instance_dir(instance_id).join("config")
    }

    fn guest_policy_path(&self, instance_id: &InstanceId) -> PathBuf {
        self.config_dir(instance_id).join("policy.toml")
    }

    fn helper_stdout_path(&self, instance_id: &InstanceId) -> PathBuf {
        self.instance_dir(instance_id).join("petri-vz.stdout.log")
    }

    fn helper_stderr_path(&self, instance_id: &InstanceId) -> PathBuf {
        self.instance_dir(instance_id).join("petri-vz.stderr.log")
    }

    fn guest_console_path(&self, instance_id: &InstanceId) -> PathBuf {
        self.instance_dir(instance_id).join("guest-console.log")
    }

    fn load_state(&self, instance_id: &InstanceId) -> Result<RuntimeState> {
        let path = self.state_path(instance_id);
        let input = fs::read_to_string(&path).map_err(|source| PetriError::Io {
            path: path.clone(),
            source,
        })?;
        serde_json::from_str(&input)
            .map_err(|err| backend_error(format!("failed to parse {}: {err}", path.display())))
    }

    fn write_state(&self, state: &RuntimeState) -> Result<()> {
        let instance_dir = self.instance_dir(&state.id);
        fs::create_dir_all(&instance_dir).map_err(|source| PetriError::Io {
            path: instance_dir.clone(),
            source,
        })?;
        let path = self.state_path(&state.id);
        let payload = serde_json::to_string_pretty(state)
            .map_err(|err| backend_error(format!("failed to encode runtime state: {err}")))?;
        fs::write(&path, payload).map_err(|source| PetriError::Io { path, source })
    }

    fn transition_state(
        &self,
        mut state: RuntimeState,
        to: LifecycleState,
        operation: &'static str,
    ) -> Result<RuntimeState> {
        state.lifecycle = state.lifecycle.transition(to, operation)?;
        self.write_state(&state)?;
        Ok(state)
    }

    fn remove_state(&self, instance_id: &InstanceId) -> Result<()> {
        let instance_dir = self.instance_dir(instance_id);
        match fs::remove_dir_all(&instance_dir) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(PetriError::Io {
                path: instance_dir,
                source,
            }),
        }
    }

    fn list_states(&self) -> Result<Vec<RuntimeState>> {
        let entries = match fs::read_dir(&self.state_dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(PetriError::Io {
                    path: self.state_dir.clone(),
                    source,
                });
            }
        };

        let mut states = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| PetriError::Io {
                path: self.state_dir.clone(),
                source,
            })?;
            let path = entry.path().join("instance.json");
            if !path.is_file() {
                continue;
            }
            let input = fs::read_to_string(&path).map_err(|source| PetriError::Io {
                path: path.clone(),
                source,
            })?;
            let state = serde_json::from_str(&input).map_err(|err| {
                backend_error(format!("failed to parse {}: {err}", path.display()))
            })?;
            states.push(state);
        }

        states.sort_by(|left: &RuntimeState, right| left.id.as_str().cmp(right.id.as_str()));
        Ok(states)
    }

    fn create_real_vm(&self, config: InstanceConfig) -> Result<InstanceHandle> {
        let image_bundle = config.image.as_ref().ok_or_else(|| PetriError::InvalidConfig(
            "macos backend requires --image <image-bundle>; set PETRI_MACOS_BACKEND_FALLBACK=loopback only for local development".to_string(),
        ))?;

        let image = ImageBundle::load(image_bundle)?;
        let policy = fs::canonicalize(&config.policy).map_err(|source| PetriError::Io {
            path: config.policy.clone(),
            source,
        })?;
        let policy_doc = load_host_policy(&policy)?;
        let workspace_contract = config.workspace_contract()?;
        let workspace = workspace_contract.host_path;
        let config_dir = self.config_dir(&config.id);
        fs::create_dir_all(&config_dir).map_err(|source| PetriError::Io {
            path: config_dir.clone(),
            source,
        })?;

        let guest_policy = self.guest_policy_path(&config.id);
        fs::copy(&policy, &guest_policy).map_err(|source| PetriError::Io {
            path: guest_policy.clone(),
            source,
        })?;

        let helper_stdout_path = self.helper_stdout_path(&config.id);
        let helper_stderr_path = self.helper_stderr_path(&config.id);
        let guest_console_path = self.guest_console_path(&config.id);
        let bootstrap_log_path = workspace.join("target").join("petri-builder-bootstrap.log");
        let helper_stdout =
            fs::File::create(&helper_stdout_path).map_err(|source| PetriError::Io {
                path: helper_stdout_path.clone(),
                source,
            })?;
        let helper_stderr =
            fs::File::create(&helper_stderr_path).map_err(|source| PetriError::Io {
                path: helper_stderr_path.clone(),
                source,
            })?;

        let control_socket = self.control_socket_path(&config.id);
        let _ = fs::remove_file(&control_socket);
        let helper_binary = resolve_helper_binary(&self.helper_binary)?;
        let mut command = Command::new(&helper_binary);
        command
            .arg("--instance-id")
            .arg(config.id.as_str())
            .arg("--control-socket")
            .arg(&control_socket)
            .arg("--boot-mode")
            .arg(image.manifest.boot_mode.as_str())
            .arg("--disk")
            .arg(&image.disk)
            .arg("--workspace")
            .arg(&workspace)
            .arg("--config-dir")
            .arg(&config_dir)
            .arg("--dispatch-port")
            .arg(image.manifest.dispatch_port.to_string())
            .arg("--guest-ready-timeout-secs")
            .arg(image.manifest.ready_timeout_secs.to_string())
            .arg("--console-log")
            .arg(&guest_console_path)
            .stdin(Stdio::null())
            .stdout(Stdio::from(helper_stdout))
            .stderr(Stdio::from(helper_stderr));

        if let Some(kernel) = &image.kernel {
            command.arg("--kernel").arg(kernel);
        }
        if let Some(initrd) = &image.initrd {
            command.arg("--initrd").arg(initrd);
        }
        if let Some(command_line) = &image.manifest.kernel_command_line {
            command.arg("--command-line").arg(command_line);
        }
        if image.manifest.boot_mode == BootMode::Efi {
            command
                .arg("--efi-variable-store")
                .arg(self.instance_dir(&config.id).join("efi-variable-store"));
        }
        for disk in &image.auxiliary_disks {
            command.arg("--auxiliary-disk").arg(disk);
        }
        if policy_doc.network_enabled {
            command.arg("--enable-network");
        }

        #[cfg(unix)]
        command.process_group(0);

        let mut child = command.spawn().map_err(|source| PetriError::Io {
            path: helper_binary.clone(),
            source,
        })?;

        let booting = LifecycleState::Provisioning.transition(LifecycleState::Booting, "create")?;
        let state = RuntimeState {
            id: config.id.clone(),
            backend: self.name().to_string(),
            lifecycle: booting,
            pid: child.id(),
            control_socket: Some(control_socket.clone()),
            dispatch_addr: None,
            workspace,
            host_policy: policy,
            guest_policy,
            image: Some(image.bundle_dir),
            transport: GuestTransport::Vsock,
            vm: MacosVmSpec::from_image(&image.manifest),
        };
        self.write_state(&state)?;

        wait_for_helper_ready(
            &control_socket,
            Duration::from_secs(image.manifest.ready_timeout_secs),
            &mut child,
            &HelperReadyProgress {
                helper_stderr_path: &helper_stderr_path,
                guest_console_path: &guest_console_path,
                bootstrap_log_path: &bootstrap_log_path,
            },
        )
        .inspect_err(|_| {
            let _ = self.transition_state(state.clone(), LifecycleState::Failed, "create");
            let _ = terminate_process(child.id());
        })
        .map_err(|err| {
            backend_error(format!(
                "{err}; helper stdout: {}; helper stderr: {}; guest console: {}",
                helper_stdout_path.display(),
                helper_stderr_path.display(),
                guest_console_path.display()
            ))
        })?;

        let state = self.transition_state(state, LifecycleState::Ready, "create")?;

        Ok(InstanceHandle {
            id: state.id,
            backend: state.backend,
            state: state.lifecycle,
        })
    }

    fn create_loopback(&self, config: InstanceConfig) -> Result<InstanceHandle> {
        let policy = fs::canonicalize(&config.policy).map_err(|source| PetriError::Io {
            path: config.policy.clone(),
            source,
        })?;
        let workspace_contract = config.workspace_contract()?;
        let workspace = workspace_contract.host_path;
        let guest_binary = resolve_guest_binary(&self.guest_binary)?;
        let listen_addr = reserve_loopback_addr()?;
        let mut command = Command::new(&guest_binary);
        command
            .arg("--policy")
            .arg(&policy)
            .arg("--listen")
            .arg(listen_addr.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        command.process_group(0);

        let child = command.spawn().map_err(|source| PetriError::Io {
            path: guest_binary.clone(),
            source,
        })?;

        let booting = LifecycleState::Provisioning.transition(LifecycleState::Booting, "create")?;
        let state = RuntimeState {
            id: config.id.clone(),
            backend: self.name().to_string(),
            lifecycle: booting,
            pid: child.id(),
            control_socket: None,
            dispatch_addr: Some(listen_addr.to_string()),
            workspace,
            host_policy: policy.clone(),
            guest_policy: policy,
            image: config.image.clone(),
            transport: GuestTransport::TcpLoopback,
            vm: MacosVmSpec::loopback(config.image.clone()),
        };
        self.write_state(&state)?;

        wait_for_guest(&listen_addr, Duration::from_secs(5)).inspect_err(|_| {
            let _ = self.transition_state(state.clone(), LifecycleState::Failed, "create");
            let _ = terminate_process(child.id());
        })?;

        let state = self.transition_state(state, LifecycleState::Ready, "create")?;

        Ok(InstanceHandle {
            id: state.id,
            backend: state.backend,
            state: state.lifecycle,
        })
    }
}

impl HostBackend for MacosBackend {
    fn name(&self) -> &str {
        MACOS_BACKEND
    }

    fn create(&self, config: InstanceConfig) -> Result<InstanceHandle> {
        config.validate()?;

        if config.backend != self.name() {
            return Err(backend_error(format!(
                "cannot create backend '{}'",
                config.backend
            )));
        }

        if loopback_fallback_enabled() {
            self.create_loopback(config)
        } else {
            self.create_real_vm(config)
        }
    }

    fn list(&self) -> Result<Vec<InstanceHandle>> {
        self.list_states().map(|states| {
            states
                .into_iter()
                .map(|state| InstanceHandle {
                    id: state.id,
                    backend: state.backend,
                    state: state.lifecycle,
                })
                .collect()
        })
    }

    fn dispatch(
        &self,
        instance_id: &InstanceId,
        request: DispatchRequest,
    ) -> Result<DispatchResult> {
        let state = self.load_state(instance_id)?;
        let running = self.transition_state(state, LifecycleState::RunningDispatch, "dispatch")?;
        let result = match running.transport {
            GuestTransport::Vsock => {
                let response: HelperResponse = send_helper_request(
                    &required_control_socket(&running)?,
                    &HelperRequest::Dispatch { request },
                )?;
                response.into_dispatch_result()
            }
            GuestTransport::TcpLoopback => {
                let mut stream =
                    TcpStream::connect(required_dispatch_addr(&running)?).map_err(|source| {
                        backend_error(format!(
                            "failed to connect to guest at {}: {source}",
                            required_dispatch_addr(&running).unwrap_or("<missing>")
                        ))
                    })?;

                serde_json::to_writer(&mut stream, &request).map_err(|err| {
                    backend_error(format!("failed to encode dispatch request: {err}"))
                })?;
                stream.write_all(b"\n").map_err(|source| {
                    backend_error(format!("failed to write dispatch frame: {source}"))
                })?;
                stream.flush().map_err(|source| {
                    backend_error(format!("failed to flush dispatch frame: {source}"))
                })?;

                let mut response = String::new();
                BufReader::new(stream)
                    .read_line(&mut response)
                    .map_err(|source| {
                        backend_error(format!("failed to read guest response: {source}"))
                    })?;

                if response.is_empty() {
                    return Err(backend_error(
                        "guest closed the dispatch connection without a response".to_string(),
                    ));
                }

                serde_json::from_str(&response)
                    .map_err(|err| backend_error(format!("failed to decode guest response: {err}")))
            }
        };

        match result {
            Ok(result) => {
                let _ = self.transition_state(running, LifecycleState::Ready, "dispatch")?;
                Ok(result)
            }
            Err(err) => {
                let _ = self.transition_state(running, LifecycleState::Failed, "dispatch");
                Err(err)
            }
        }
    }

    fn stop(&self, instance_id: &InstanceId) -> Result<()> {
        let state = self.load_state(instance_id)?;
        let stopping = self.transition_state(state, LifecycleState::Stopping, "stop")?;
        let result = match stopping.transport {
            GuestTransport::Vsock => {
                let response: HelperResponse = send_helper_request(
                    &required_control_socket(&stopping)?,
                    &HelperRequest::Stop,
                )?;
                response.ensure_ok()
            }
            GuestTransport::TcpLoopback => terminate_process(stopping.pid),
        };

        match result {
            Ok(()) => {
                let _ = self.transition_state(stopping, LifecycleState::TornDown, "stop")?;
                Ok(())
            }
            Err(err) => {
                let _ = self.transition_state(stopping, LifecycleState::Failed, "stop");
                Err(err)
            }
        }
    }

    fn teardown(&self, instance_id: &InstanceId) -> Result<()> {
        if let Ok(state) = self.load_state(instance_id) {
            let state = if state.lifecycle == LifecycleState::TornDown {
                state
            } else {
                self.transition_state(state, LifecycleState::TornDown, "teardown")?
            };
            match state.transport {
                GuestTransport::Vsock => {
                    let _ = send_helper_request::<HelperResponse>(
                        &required_control_socket(&state)?,
                        &HelperRequest::Teardown,
                    );
                    let _ = terminate_process(state.pid);
                }
                GuestTransport::TcpLoopback => {
                    let _ = terminate_process(state.pid);
                }
            }
        }
        self.remove_state(instance_id)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RuntimeState {
    id: InstanceId,
    backend: String,
    #[serde(default = "default_runtime_lifecycle")]
    lifecycle: LifecycleState,
    pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    control_socket: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dispatch_addr: Option<String>,
    workspace: PathBuf,
    host_policy: PathBuf,
    guest_policy: PathBuf,
    image: Option<PathBuf>,
    transport: GuestTransport,
    vm: MacosVmSpec,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum GuestTransport {
    Vsock,
    TcpLoopback,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct MacosVmSpec {
    framework: String,
    boot_image: Option<PathBuf>,
    workspace_mount_tag: String,
    guest_workspace_path: String,
    config_mount_tag: String,
    guest_policy_path: String,
    dispatch_transport: String,
    dispatch_port: u32,
    guest_program: String,
}

impl MacosVmSpec {
    fn from_image(manifest: &ImageManifest) -> Self {
        Self {
            framework: "Virtualization.framework".to_string(),
            boot_image: Some(manifest.disk.clone()),
            workspace_mount_tag: WORKSPACE_TAG.to_string(),
            guest_workspace_path: GUEST_WORKSPACE_PATH.to_string(),
            config_mount_tag: CONFIG_TAG.to_string(),
            guest_policy_path: GUEST_POLICY_PATH.to_string(),
            dispatch_transport: "vsock".to_string(),
            dispatch_port: manifest.dispatch_port,
            guest_program: format!(
                "petri-guest --policy {GUEST_POLICY_PATH} --transport vsock --vsock-port {}",
                manifest.dispatch_port
            ),
        }
    }

    fn loopback(image: Option<PathBuf>) -> Self {
        Self {
            framework: "local-process-fallback".to_string(),
            boot_image: image,
            workspace_mount_tag: WORKSPACE_TAG.to_string(),
            guest_workspace_path: GUEST_WORKSPACE_PATH.to_string(),
            config_mount_tag: CONFIG_TAG.to_string(),
            guest_policy_path: GUEST_POLICY_PATH.to_string(),
            dispatch_transport: "tcp_loopback".to_string(),
            dispatch_port: DEFAULT_DISPATCH_PORT,
            guest_program: "petri-guest --listen 127.0.0.1:<ephemeral>".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
struct ImageBundle {
    bundle_dir: PathBuf,
    manifest: ImageManifest,
    kernel: Option<PathBuf>,
    disk: PathBuf,
    initrd: Option<PathBuf>,
    auxiliary_disks: Vec<PathBuf>,
}

impl ImageBundle {
    fn load(path: &Path) -> Result<Self> {
        let bundle_dir = fs::canonicalize(path).map_err(|source| PetriError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if !bundle_dir.is_dir() {
            return Err(PetriError::InvalidConfig(format!(
                "image bundle must be a directory: {}",
                bundle_dir.display()
            )));
        }

        let manifest_path = bundle_dir.join("petri-image.json");
        let input = fs::read_to_string(&manifest_path).map_err(|source| PetriError::Io {
            path: manifest_path.clone(),
            source,
        })?;
        let manifest: ImageManifest = serde_json::from_str(&input).map_err(|err| {
            PetriError::InvalidConfig(format!(
                "failed to parse {}: {err}",
                manifest_path.display()
            ))
        })?;
        manifest.validate()?;

        let kernel = manifest
            .kernel
            .as_ref()
            .map(|path| canonical_bundle_file(&bundle_dir, path, "kernel"))
            .transpose()?;
        let disk = canonical_bundle_file(&bundle_dir, &manifest.disk, "disk")?;
        let initrd = manifest
            .initrd
            .as_ref()
            .map(|path| canonical_bundle_file(&bundle_dir, path, "initrd"))
            .transpose()?;
        let auxiliary_disks = manifest
            .auxiliary_disks
            .iter()
            .enumerate()
            .map(|(index, path)| {
                canonical_bundle_file(&bundle_dir, path, &format!("auxiliary_disks[{index}]"))
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            bundle_dir,
            manifest,
            kernel,
            disk,
            initrd,
            auxiliary_disks,
        })
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ImageManifest {
    architecture: String,
    #[serde(default)]
    boot_mode: BootMode,
    #[serde(default)]
    kernel: Option<PathBuf>,
    disk: PathBuf,
    #[serde(default)]
    initrd: Option<PathBuf>,
    #[serde(default)]
    kernel_command_line: Option<String>,
    #[serde(default = "default_dispatch_port")]
    dispatch_port: u32,
    #[serde(default = "default_ready_timeout_secs")]
    ready_timeout_secs: u64,
    #[serde(default)]
    auxiliary_disks: Vec<PathBuf>,
}

impl ImageManifest {
    fn validate(&self) -> Result<()> {
        if self.architecture.is_empty() {
            return Err(PetriError::InvalidConfig(
                "image architecture must be non-empty".to_string(),
            ));
        }
        if self.boot_mode == BootMode::Linux {
            if self
                .kernel
                .as_ref()
                .map(|path| path.as_os_str().is_empty())
                .unwrap_or(true)
            {
                return Err(PetriError::InvalidConfig(
                    "linux boot image kernel path must be non-empty".to_string(),
                ));
            }
            if self.kernel_command_line.as_deref().unwrap_or("").is_empty() {
                return Err(PetriError::InvalidConfig(
                    "linux boot image kernel_command_line must be non-empty".to_string(),
                ));
            }
        } else if self.kernel.is_some()
            || self.initrd.is_some()
            || self.kernel_command_line.is_some()
        {
            return Err(PetriError::InvalidConfig(
                "efi boot image must not set kernel, initrd, or kernel_command_line".to_string(),
            ));
        }
        if self.disk.as_os_str().is_empty() {
            return Err(PetriError::InvalidConfig(
                "image disk path must be non-empty".to_string(),
            ));
        }
        if self.dispatch_port == 0 {
            return Err(PetriError::InvalidConfig(
                "image dispatch_port must be positive".to_string(),
            ));
        }
        if self.ready_timeout_secs == 0 {
            return Err(PetriError::InvalidConfig(
                "image ready_timeout_secs must be positive".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum BootMode {
    Linux,
    Efi,
}

impl Default for BootMode {
    fn default() -> Self {
        Self::Linux
    }
}

impl BootMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Efi => "efi",
        }
    }
}

#[derive(Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HelperRequest {
    Status,
    Dispatch { request: DispatchRequest },
    Stop,
    Teardown,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum HelperResponse {
    Ready,
    Stopped,
    TeardownComplete,
    DispatchResult { result: DispatchResult },
    Starting,
    Error { message: String },
}

impl HelperResponse {
    fn ensure_ok(self) -> Result<()> {
        match self {
            Self::Ready | Self::Stopped | Self::TeardownComplete => Ok(()),
            Self::Starting => Err(backend_error(
                "helper reports VM is still starting".to_string(),
            )),
            Self::DispatchResult { .. } => Err(backend_error(
                "helper returned a dispatch result for a lifecycle request".to_string(),
            )),
            Self::Error { message } => Err(backend_error(message)),
        }
    }

    fn into_dispatch_result(self) -> Result<DispatchResult> {
        match self {
            Self::DispatchResult { result } => Ok(result),
            Self::Error { message } => Err(backend_error(message)),
            other => Err(backend_error(format!(
                "helper returned non-dispatch response: {other:?}"
            ))),
        }
    }
}

fn load_host_policy(path: &Path) -> Result<Policy> {
    let file = fs::File::open(path).map_err(|source| PetriError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Policy::load(file)
        .map_err(|err| backend_error(format!("failed to load policy {}: {err}", path.display())))
}

fn default_state_dir() -> PathBuf {
    env::var_os("PETRI_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".petri").join("instances"))
        })
        .unwrap_or_else(|| env::temp_dir().join("petri").join("instances"))
}

fn default_helper_binary() -> PathBuf {
    env::var_os("PETRI_VZ_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("petri-vz"))
}

fn default_guest_binary() -> PathBuf {
    env::var_os("PETRI_GUEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("petri-guest"))
}

fn default_dispatch_port() -> u32 {
    DEFAULT_DISPATCH_PORT
}

fn default_ready_timeout_secs() -> u64 {
    90
}

fn default_runtime_lifecycle() -> LifecycleState {
    LifecycleState::Ready
}

fn loopback_fallback_enabled() -> bool {
    env::var("PETRI_MACOS_BACKEND_FALLBACK")
        .map(|value| value == "loopback")
        .unwrap_or(false)
}

fn canonical_bundle_file(bundle_dir: &Path, relative: &Path, name: &str) -> Result<PathBuf> {
    if relative.is_absolute() {
        return Err(PetriError::InvalidConfig(format!(
            "image {name} path must be relative to the image bundle"
        )));
    }

    let path = bundle_dir.join(relative);
    let canonical = fs::canonicalize(&path).map_err(|source| PetriError::Io {
        path: path.clone(),
        source,
    })?;

    if !canonical.starts_with(bundle_dir) {
        return Err(PetriError::InvalidConfig(format!(
            "image {name} path escapes image bundle: {}",
            relative.display()
        )));
    }

    Ok(canonical)
}

fn resolve_helper_binary(configured: &Path) -> Result<PathBuf> {
    if configured.components().count() <= 1 && !configured.is_absolute() {
        let repo_helper = repo_root_fallback()
            .join("crates")
            .join("petri-vz")
            .join(".build")
            .join("debug")
            .join(configured);
        if repo_helper.is_file() {
            return Ok(repo_helper);
        }
    }

    resolve_sibling_binary(configured)
}

fn resolve_guest_binary(configured: &Path) -> Result<PathBuf> {
    resolve_sibling_binary(configured)
}

fn resolve_sibling_binary(configured: &Path) -> Result<PathBuf> {
    if configured.components().count() > 1 || configured.is_absolute() {
        return Ok(configured.to_path_buf());
    }

    if let Ok(current_exe) = env::current_exe() {
        if let Some(dir) = current_exe.parent() {
            let sibling = dir.join(configured);
            if sibling.is_file() {
                return Ok(sibling);
            }
        }
    }

    Ok(configured.to_path_buf())
}

fn repo_root_fallback() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn required_control_socket(state: &RuntimeState) -> Result<PathBuf> {
    state
        .control_socket
        .clone()
        .ok_or_else(|| backend_error("runtime state is missing helper control socket".to_string()))
}

fn required_dispatch_addr(state: &RuntimeState) -> Result<&str> {
    state
        .dispatch_addr
        .as_deref()
        .ok_or_else(|| backend_error("runtime state is missing dispatch address".to_string()))
}

fn wait_for_helper_ready(
    control_socket: &Path,
    timeout: Duration,
    child: &mut std::process::Child,
    progress: &HelperReadyProgress<'_>,
) -> Result<()> {
    let started = Instant::now();
    let mut next_progress = Instant::now() + Duration::from_secs(5);
    let mut helper_stderr_offset = file_len(&progress.helper_stderr_path).unwrap_or(0);
    let mut guest_console_offset = file_len(&progress.guest_console_path).unwrap_or(0);
    let mut bootstrap_log_offset = 0;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|source| backend_error(format!("failed to poll helper process: {source}")))?
        {
            return Err(backend_error(format!(
                "helper process exited before VM became ready: {status}"
            )));
        }

        let response =
            send_helper_request::<HelperResponse>(control_socket, &HelperRequest::Status);
        if Instant::now() >= next_progress {
            let status = match &response {
                Ok(response) => format!("{response:?}"),
                Err(err) => format!("status unavailable: {err}"),
            };
            eprintln!(
                "waiting for guest vsock readiness ({:.0}s/{:.0}s); helper status: {status}",
                started.elapsed().as_secs_f64(),
                timeout.as_secs_f64(),
            );
            print_new_log_lines(
                "petri-builder",
                progress.bootstrap_log_path,
                &mut bootstrap_log_offset,
            );
            print_new_log_lines(
                "guest-console",
                progress.guest_console_path,
                &mut guest_console_offset,
            );
            print_new_log_lines(
                "petri-vz",
                progress.helper_stderr_path,
                &mut helper_stderr_offset,
            );
            next_progress = Instant::now() + Duration::from_secs(10);
        }

        match response {
            Ok(HelperResponse::Ready) => return Ok(()),
            Ok(HelperResponse::Error { message }) => return Err(backend_error(message)),
            Ok(_) | Err(_) if started.elapsed() < timeout => {
                thread::sleep(Duration::from_millis(100))
            }
            Ok(other) => {
                return Err(backend_error(format!(
                    "helper did not become ready before timeout; last status: {other:?}"
                )));
            }
            Err(err) => {
                return Err(backend_error(format!(
                    "helper did not become ready before timeout: {err}"
                )));
            }
        }
    }
}

struct HelperReadyProgress<'a> {
    helper_stderr_path: &'a Path,
    guest_console_path: &'a Path,
    bootstrap_log_path: &'a Path,
}

fn file_len(path: &Path) -> std::io::Result<u64> {
    fs::metadata(path).map(|metadata| metadata.len())
}

fn print_new_log_lines(label: &str, path: &Path, offset: &mut u64) {
    let Ok(mut file) = fs::File::open(path) else {
        return;
    };
    let Ok(metadata) = file.metadata() else {
        return;
    };
    if metadata.len() < *offset {
        *offset = 0;
    }
    if metadata.len() == *offset {
        return;
    }
    if file.seek(SeekFrom::Start(*offset)).is_err() {
        return;
    }
    let mut output = String::new();
    if file.read_to_string(&mut output).is_err() {
        return;
    }
    *offset = metadata.len();
    for line in output.lines() {
        eprintln!("[{label}] {line}");
    }
}

#[cfg(unix)]
fn send_helper_request<T>(control_socket: &Path, request: &HelperRequest) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let mut stream = UnixStream::connect(control_socket).map_err(|source| PetriError::Io {
        path: control_socket.to_path_buf(),
        source,
    })?;
    serde_json::to_writer(&mut stream, request)
        .map_err(|err| backend_error(format!("failed to encode helper request: {err}")))?;
    stream
        .write_all(b"\n")
        .map_err(|source| backend_error(format!("failed to write helper request: {source}")))?;
    stream
        .flush()
        .map_err(|source| backend_error(format!("failed to flush helper request: {source}")))?;

    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .map_err(|source| backend_error(format!("failed to read helper response: {source}")))?;
    if response.is_empty() {
        return Err(backend_error(
            "helper closed the control connection without a response".to_string(),
        ));
    }
    serde_json::from_str(&response)
        .map_err(|err| backend_error(format!("failed to decode helper response: {err}")))
}

#[cfg(not(unix))]
fn send_helper_request<T>(_control_socket: &Path, _request: &HelperRequest) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    Err(backend_error(
        "macos helper control sockets require a Unix platform".to_string(),
    ))
}

fn reserve_loopback_addr() -> Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|source| {
        backend_error(format!("failed to reserve guest dispatch port: {source}"))
    })?;
    listener.local_addr().map_err(|source| {
        backend_error(format!(
            "failed to read reserved guest dispatch port: {source}"
        ))
    })
}

fn wait_for_guest(addr: &SocketAddr, timeout: Duration) -> Result<()> {
    let started = Instant::now();
    loop {
        match TcpStream::connect(addr) {
            Ok(_) => return Ok(()),
            Err(err) if started.elapsed() >= timeout => {
                return Err(backend_error(format!(
                    "guest did not become ready at {addr}: {err}"
                )));
            }
            Err(_) => thread::sleep(Duration::from_millis(25)),
        }
    }
}

fn terminate_process(pid: u32) -> Result<()> {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .map_err(|source| {
            backend_error(format!(
                "failed to invoke kill for guest process {pid}: {source}"
            ))
        })?;

    if status.success() {
        Ok(())
    } else {
        Err(backend_error(format!(
            "failed to terminate guest process {pid}: kill exited with {status}"
        )))
    }
}

fn backend_error(message: String) -> PetriError {
    PetriError::Backend {
        backend: MACOS_BACKEND.to_string(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!("petri-{name}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn runtime_state(id: InstanceId, lifecycle: LifecycleState) -> RuntimeState {
        RuntimeState {
            id,
            backend: MACOS_BACKEND.to_string(),
            lifecycle,
            pid: 0,
            control_socket: None,
            dispatch_addr: Some("127.0.0.1:9".to_string()),
            workspace: PathBuf::from("/workspace"),
            host_policy: PathBuf::from("/policy.toml"),
            guest_policy: PathBuf::from("/run/petri/policy.toml"),
            image: None,
            transport: GuestTransport::TcpLoopback,
            vm: MacosVmSpec::loopback(None),
        }
    }

    #[test]
    fn image_bundle_loads_manifest_and_resolves_files() {
        let dir = temp_dir("image-bundle");
        fs::write(dir.join("vmlinuz"), b"kernel").unwrap();
        fs::write(dir.join("root.img"), b"disk").unwrap();
        fs::write(
            dir.join("petri-image.json"),
            r#"{
                "architecture": "aarch64",
                "kernel": "vmlinuz",
                "disk": "root.img",
                "kernel_command_line": "console=hvc0",
                "dispatch_port": 7777
            }"#,
        )
        .unwrap();

        let bundle = ImageBundle::load(&dir).unwrap();

        assert_eq!(bundle.manifest.architecture, "aarch64");
        assert_eq!(bundle.manifest.dispatch_port, DEFAULT_DISPATCH_PORT);
        assert_eq!(
            bundle.kernel,
            Some(fs::canonicalize(dir.join("vmlinuz")).unwrap())
        );
        assert_eq!(bundle.disk, fs::canonicalize(dir.join("root.img")).unwrap());
    }

    #[test]
    fn image_bundle_loads_efi_manifest() {
        let dir = temp_dir("image-bundle-efi");
        fs::write(dir.join("root.img"), b"disk").unwrap();
        fs::write(
            dir.join("petri-image.json"),
            r#"{
                "architecture": "aarch64",
                "boot_mode": "efi",
                "disk": "root.img",
                "dispatch_port": 7777
            }"#,
        )
        .unwrap();

        let bundle = ImageBundle::load(&dir).unwrap();

        assert_eq!(bundle.manifest.boot_mode, BootMode::Efi);
        assert_eq!(bundle.kernel, None);
        assert_eq!(bundle.disk, fs::canonicalize(dir.join("root.img")).unwrap());
    }

    #[test]
    fn image_bundle_rejects_efi_manifest_without_disk() {
        let dir = temp_dir("image-bundle-efi-missing-disk");
        fs::write(
            dir.join("petri-image.json"),
            r#"{
                "architecture": "aarch64",
                "boot_mode": "efi",
                "disk": ""
            }"#,
        )
        .unwrap();

        let err = ImageBundle::load(&dir).unwrap_err().to_string();

        assert!(err.contains("disk path must be non-empty"));
    }

    #[test]
    fn image_bundle_rejects_absolute_member_paths() {
        let dir = temp_dir("image-bundle-absolute");
        fs::write(
            dir.join("petri-image.json"),
            r#"{
                "architecture": "aarch64",
                "kernel": "/tmp/vmlinuz",
                "disk": "root.img",
                "kernel_command_line": "console=hvc0"
            }"#,
        )
        .unwrap();

        let err = ImageBundle::load(&dir).unwrap_err().to_string();

        assert!(err.contains("kernel path must be relative"));
    }

    #[test]
    fn macos_vm_spec_records_required_mvp_surfaces() {
        let manifest = ImageManifest {
            architecture: "aarch64".to_string(),
            boot_mode: BootMode::Linux,
            kernel: Some(PathBuf::from("vmlinuz")),
            disk: PathBuf::from("root.img"),
            initrd: None,
            kernel_command_line: Some("console=hvc0".to_string()),
            dispatch_port: DEFAULT_DISPATCH_PORT,
            ready_timeout_secs: default_ready_timeout_secs(),
            auxiliary_disks: Vec::new(),
        };

        let spec = MacosVmSpec::from_image(&manifest);

        assert_eq!(spec.framework, "Virtualization.framework");
        assert_eq!(spec.boot_image, Some(PathBuf::from("root.img")));
        assert_eq!(spec.workspace_mount_tag, WORKSPACE_TAG);
        assert_eq!(spec.guest_workspace_path, GUEST_WORKSPACE_PATH);
        assert_eq!(spec.config_mount_tag, CONFIG_TAG);
        assert_eq!(spec.guest_policy_path, GUEST_POLICY_PATH);
        assert_eq!(spec.dispatch_transport, "vsock");
        assert_eq!(spec.dispatch_port, DEFAULT_DISPATCH_PORT);
        assert!(spec.guest_program.contains("petri-guest"));
    }

    #[test]
    fn macos_backend_lists_persisted_lifecycle_state() {
        let state_dir = temp_dir("lifecycle-list");
        let backend = MacosBackend::new(&state_dir, "petri-vz");
        let id = InstanceId::new("lifecycle-list").unwrap();
        backend
            .write_state(&runtime_state(id.clone(), LifecycleState::Stopping))
            .unwrap();

        let instances = backend.list().unwrap();

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].id, id);
        assert_eq!(instances[0].state, LifecycleState::Stopping);
    }

    #[test]
    fn macos_backend_rejects_dispatch_from_torn_down_state() {
        let state_dir = temp_dir("lifecycle-invalid-dispatch");
        let backend = MacosBackend::new(&state_dir, "petri-vz");
        let id = InstanceId::new("lifecycle-invalid-dispatch").unwrap();
        backend
            .write_state(&runtime_state(id.clone(), LifecycleState::TornDown))
            .unwrap();
        let request = DispatchRequest::bash_command(
            "request-1",
            "pwd",
            Vec::new(),
            PathBuf::from("/workspace"),
            std::collections::BTreeMap::new(),
            None,
            None,
        );

        let err = backend.dispatch(&id, request).unwrap_err();

        assert_eq!(
            err.to_string(),
            "invalid lifecycle transition during dispatch: torn_down -> running_dispatch"
        );
    }

    fn write_policy(dir: &Path, network_enabled: bool) -> PathBuf {
        let path = dir.join("policy.toml");
        fs::write(
            &path,
            format!(
                "[policy]\nnetwork_enabled = {network_enabled}\nallowed_commands = [\"git\"]\n\
                 max_runtime_secs = 60\nmax_output_bytes = 1048576\nworkspace_path = \"/workspace\"\n"
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn load_host_policy_reads_network_disabled() {
        let dir = temp_dir("policy-network-disabled");
        let path = write_policy(&dir, false);

        let policy = load_host_policy(&path).unwrap();

        assert!(!policy.network_enabled);
    }

    #[test]
    fn load_host_policy_reads_network_enabled() {
        let dir = temp_dir("policy-network-enabled");
        let path = write_policy(&dir, true);

        let policy = load_host_policy(&path).unwrap();

        assert!(policy.network_enabled);
    }

    #[test]
    fn load_host_policy_surfaces_parse_errors() {
        let dir = temp_dir("policy-invalid");
        let path = dir.join("policy.toml");
        fs::write(&path, "not = valid = policy").unwrap();

        let err = load_host_policy(&path).unwrap_err().to_string();

        assert!(err.contains("failed to load policy"));
    }
}
