//! E2B-style high-level sandbox SDK over the Petri host backend (#27).
//!
//! This module turns the low-level [`HostBackend`] lifecycle/dispatch surface
//! into a `Sandbox` object with feature modules, matching the language-agnostic
//! contract documented in `docs/sdk-api.md`. The Rust shape is the reference
//! implementation that TypeScript, Python, and Go clients mirror.
//!
//! v1 implements lifecycle (`create`/`connect`/`list`/`kill`/`is_running`/
//! `get_info`) and the `commands` module. The `files`, `git`, and `pty` modules
//! are named and reserved here so the surface is stable, but their operations
//! are deferred (see the issue #27 "do not implement in v1" list).

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::backend::HostBackend;
use crate::dispatch::{DispatchRequest, ErrorFrame, RequestLimits, Status};
use crate::error::{PetriError, Result};
use crate::instance::{InstanceConfig, InstanceHandle, InstanceId, LifecycleState};

/// Default backend name used when [`SandboxOptions`] does not specify one.
pub const DEFAULT_BACKEND: &str = "macos";

/// Options for [`Sandbox::create`].
///
/// `workspace` and `policy` are required by the current local backend; the
/// remaining fields mirror the cross-language `SandboxOpts` shape. `metadata` is
/// accepted and carried on the returned handle, but is not yet persisted by the
/// backend (reserved for the remote control plane).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxOptions {
    /// Explicit sandbox id. When `None`, a unique id is generated.
    pub id: Option<String>,
    /// Backend name. Defaults to [`DEFAULT_BACKEND`].
    pub backend: String,
    /// Image bundle path. When `None`, the backend's default base image is used.
    pub image: Option<PathBuf>,
    /// Host workspace directory mounted into the sandbox.
    pub workspace: PathBuf,
    /// Policy file applied at boot.
    pub policy: PathBuf,
    /// Free-form metadata. Reserved; not yet persisted by the local backend.
    pub metadata: BTreeMap<String, String>,
}

impl SandboxOptions {
    /// Construct options for the required `workspace` and `policy` inputs, with
    /// every other field defaulted.
    pub fn new(workspace: impl Into<PathBuf>, policy: impl Into<PathBuf>) -> Self {
        Self {
            id: None,
            backend: DEFAULT_BACKEND.to_string(),
            image: None,
            workspace: workspace.into(),
            policy: policy.into(),
            metadata: BTreeMap::new(),
        }
    }

    /// Set an explicit sandbox id instead of generating one.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Override the backend name.
    pub fn with_backend(mut self, backend: impl Into<String>) -> Self {
        self.backend = backend.into();
        self
    }

    /// Set an explicit image bundle path.
    pub fn with_image(mut self, image: impl Into<PathBuf>) -> Self {
        self.image = Some(image.into());
        self
    }

    /// Attach metadata to the sandbox (reserved; not yet persisted).
    pub fn with_metadata(mut self, metadata: BTreeMap<String, String>) -> Self {
        self.metadata = metadata;
        self
    }

    fn into_config(self) -> Result<InstanceConfig> {
        let id = match self.id {
            Some(id) => InstanceId::new(id)?,
            None => InstanceId::new(generate_sandbox_id()?)?,
        };
        let mut config = InstanceConfig::new(id, self.backend, self.workspace, self.policy);
        if let Some(image) = self.image {
            config = config.with_image(image);
        }
        Ok(config)
    }
}

/// Options for [`Commands::run`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandOptions {
    /// Working directory inside the sandbox. Defaults to the guest workspace.
    pub cwd: Option<PathBuf>,
    /// Extra arguments appended after the command.
    pub args: Vec<String>,
    /// Environment overrides.
    pub env: BTreeMap<String, String>,
    /// Standard input piped to the command.
    pub stdin: Option<String>,
    /// Per-request wall-clock timeout.
    pub timeout_ms: Option<u64>,
    /// Maximum captured output bytes before truncation.
    pub max_output_bytes: Option<u64>,
    /// Explicit request id for correlation. Generated when `None`.
    pub request_id: Option<String>,
}

impl CommandOptions {
    fn limits(&self) -> Option<RequestLimits> {
        (self.timeout_ms.is_some() || self.max_output_bytes.is_some()).then_some(RequestLimits {
            timeout_ms: self.timeout_ms,
            max_output_bytes: self.max_output_bytes,
        })
    }
}

/// Typed result of [`Commands::run`].
///
/// This is the SDK-facing view of a dispatch [`crate::dispatch::DispatchResult`]:
/// streams are flattened to owned strings and the exit code is unwrapped one
/// level so callers see `Option<i32>` rather than `Option<Option<i32>>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    /// Dispatch status (success, failure, timeout, rejected, …).
    pub status: Status,
    /// Process exit code, when the command ran to completion.
    pub exit_code: Option<i32>,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
    /// Whether captured output was truncated against `max_output_bytes`.
    pub output_truncated: bool,
    /// Structured error frame for non-success statuses.
    pub error: Option<ErrorFrame>,
}

impl CommandResult {
    /// Whether the command both dispatched successfully and exited zero.
    pub fn success(&self) -> bool {
        self.status == Status::Success && matches!(self.exit_code, Some(0))
    }
}

/// A handle to a single sandbox, owning the backend it was created or connected
/// through. See module docs for the implemented and reserved surface.
#[derive(Debug, Clone)]
pub struct Sandbox<B: HostBackend> {
    backend: B,
    id: InstanceId,
    backend_name: String,
    metadata: BTreeMap<String, String>,
}

impl<B: HostBackend> Sandbox<B> {
    /// Create a new sandbox from the given options and return a handle to it.
    pub fn create(backend: B, options: SandboxOptions) -> Result<Self> {
        let metadata = options.metadata.clone();
        let config = options.into_config()?;
        let handle = backend.create(config)?;
        Ok(Self {
            backend,
            id: handle.id,
            backend_name: handle.backend,
            metadata,
        })
    }

    /// Connect to an existing running sandbox by id.
    ///
    /// Errors if no sandbox with that id is known to the backend, or if it is
    /// not currently running. Connecting never tears the sandbox down.
    pub fn connect(backend: B, sandbox_id: impl Into<String>) -> Result<Self> {
        let id = InstanceId::new(sandbox_id)?;
        let handle = find_instance(&backend, &id)?.ok_or_else(|| PetriError::Backend {
            backend: backend.name().to_string(),
            message: format!("no sandbox with id '{id}'"),
        })?;
        if !handle.state.is_running() {
            return Err(PetriError::Backend {
                backend: backend.name().to_string(),
                message: format!(
                    "sandbox '{id}' is not running (state: {})",
                    handle.state.as_str()
                ),
            });
        }
        Ok(Self {
            backend,
            id: handle.id,
            backend_name: handle.backend,
            metadata: BTreeMap::new(),
        })
    }

    /// List all sandboxes known to the backend.
    pub fn list(backend: &B) -> Result<Vec<InstanceHandle>> {
        backend.list()
    }

    /// Kill a sandbox by id without holding a handle to it.
    pub fn kill_id(backend: &B, sandbox_id: &InstanceId) -> Result<()> {
        backend.teardown(sandbox_id)
    }

    /// The sandbox id.
    pub fn id(&self) -> &InstanceId {
        &self.id
    }

    /// The backend name backing this sandbox.
    pub fn backend_name(&self) -> &str {
        &self.backend_name
    }

    /// Metadata supplied at creation (reserved; not persisted by the backend).
    pub fn metadata(&self) -> &BTreeMap<String, String> {
        &self.metadata
    }

    /// The `commands` module for this sandbox.
    pub fn commands(&self) -> Commands<'_, B> {
        Commands { sandbox: self }
    }

    /// The current lifecycle handle for this sandbox, if still known.
    pub fn get_info(&self) -> Result<Option<InstanceHandle>> {
        find_instance(&self.backend, &self.id)
    }

    /// The current lifecycle state, if still known.
    pub fn state(&self) -> Result<Option<LifecycleState>> {
        Ok(self.get_info()?.map(|handle| handle.state))
    }

    /// Whether the sandbox is currently running (ready or dispatching).
    pub fn is_running(&self) -> Result<bool> {
        Ok(self.state()?.is_some_and(LifecycleState::is_running))
    }

    /// Tear the sandbox down and release its runtime state.
    pub fn kill(&self) -> Result<()> {
        self.backend.teardown(&self.id)
    }

    /// Borrow the underlying backend (escape hatch for raw protocol access).
    pub fn backend(&self) -> &B {
        &self.backend
    }
}

/// The `commands` module: run shell commands inside a sandbox.
#[derive(Debug)]
pub struct Commands<'a, B: HostBackend> {
    sandbox: &'a Sandbox<B>,
}

impl<B: HostBackend> Commands<'_, B> {
    /// Run a command inside the sandbox and return a typed result.
    pub fn run(
        &self,
        command: impl Into<String>,
        options: CommandOptions,
    ) -> Result<CommandResult> {
        let cwd = options
            .cwd
            .clone()
            .unwrap_or_else(|| PathBuf::from(crate::instance::GUEST_WORKSPACE_PATH));
        let request_id = options.request_id.clone().map(Ok).unwrap_or_else(|| {
            Ok::<_, PetriError>(format!("commands-run-{}", generate_sandbox_id()?))
        })?;
        let limits = options.limits();
        let request = DispatchRequest::bash_command(
            request_id,
            command,
            options.args,
            cwd,
            options.env,
            options.stdin,
            limits,
        );
        let result = self.sandbox.backend.dispatch(&self.sandbox.id, request)?;
        Ok(CommandResult {
            status: result.status,
            exit_code: result.exit_code.flatten(),
            stdout: result.stdout.unwrap_or_default(),
            stderr: result.stderr.unwrap_or_default(),
            output_truncated: result.output_truncated.unwrap_or(false),
            error: result.error,
        })
    }
}

fn find_instance<B: HostBackend>(backend: &B, id: &InstanceId) -> Result<Option<InstanceHandle>> {
    Ok(backend.list()?.into_iter().find(|handle| &handle.id == id))
}

fn generate_sandbox_id() -> Result<String> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| PetriError::Cli(format!("system clock is before UNIX epoch: {err}")))?
        .as_nanos();
    Ok(format!("petri-{millis}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::DispatchResult;
    use std::cell::RefCell;

    /// In-memory backend that records create/dispatch/teardown calls so SDK
    /// behavior can be asserted without booting a VM.
    #[derive(Debug, Default)]
    struct FakeBackend {
        instances: RefCell<Vec<InstanceHandle>>,
        last_request: RefCell<Option<DispatchRequest>>,
        next_result: RefCell<Option<DispatchResult>>,
    }

    impl FakeBackend {
        fn with_instance(state: LifecycleState) -> Self {
            let backend = Self::default();
            backend.instances.borrow_mut().push(InstanceHandle {
                id: InstanceId::new("dev-1").unwrap(),
                backend: "fake".to_string(),
                state,
            });
            backend
        }

        fn set_state(&self, id: &str, state: LifecycleState) {
            for handle in self.instances.borrow_mut().iter_mut() {
                if handle.id.as_str() == id {
                    handle.state = state;
                }
            }
        }
    }

    impl HostBackend for &FakeBackend {
        fn name(&self) -> &str {
            "fake"
        }

        fn create(&self, config: InstanceConfig) -> Result<InstanceHandle> {
            let handle = InstanceHandle {
                id: config.id,
                backend: config.backend,
                state: LifecycleState::Ready,
            };
            self.instances.borrow_mut().push(handle.clone());
            Ok(handle)
        }

        fn list(&self) -> Result<Vec<InstanceHandle>> {
            Ok(self.instances.borrow().clone())
        }

        fn dispatch(
            &self,
            _instance_id: &InstanceId,
            request: DispatchRequest,
        ) -> Result<DispatchResult> {
            *self.last_request.borrow_mut() = Some(request.clone());
            Ok(self.next_result.borrow_mut().take().unwrap_or_else(|| {
                DispatchResult::process(
                    request.id,
                    Status::Success,
                    1,
                    "ok\n".to_string(),
                    String::new(),
                    Some(0),
                    false,
                )
            }))
        }

        fn stop(&self, _instance_id: &InstanceId) -> Result<()> {
            Ok(())
        }

        fn teardown(&self, instance_id: &InstanceId) -> Result<()> {
            self.instances
                .borrow_mut()
                .retain(|handle| &handle.id != instance_id);
            Ok(())
        }
    }

    fn options() -> SandboxOptions {
        SandboxOptions::new("/workspace", "policy.toml").with_id("dev-1")
    }

    #[test]
    fn create_run_is_running_kill_round_trip() {
        let backend = FakeBackend::default();
        let sandbox = Sandbox::create(&backend, options()).unwrap();
        assert_eq!(sandbox.id().as_str(), "dev-1");
        assert!(sandbox.is_running().unwrap());

        let result = sandbox
            .commands()
            .run(
                "ls",
                CommandOptions {
                    args: vec!["-la".to_string()],
                    cwd: Some(PathBuf::from("/workspace")),
                    ..CommandOptions::default()
                },
            )
            .unwrap();
        assert!(result.success());
        assert_eq!(result.stdout, "ok\n");

        let request = backend.last_request.borrow().clone().unwrap();
        let args = request.args.unwrap();
        assert_eq!(args["command"], "ls");
        assert_eq!(args["argv"], serde_json::json!(["-la"]));
        assert_eq!(args["cwd"], "/workspace");

        sandbox.kill().unwrap();
        assert!(!sandbox.is_running().unwrap());
    }

    #[test]
    fn connect_requires_running_instance() {
        let backend = FakeBackend::with_instance(LifecycleState::Ready);
        let sandbox = Sandbox::connect(&backend, "dev-1").unwrap();
        assert_eq!(sandbox.id().as_str(), "dev-1");

        backend.set_state("dev-1", LifecycleState::TornDown);
        let err = Sandbox::connect(&backend, "dev-1").unwrap_err().to_string();
        assert!(err.contains("not running"), "{err}");
    }

    #[test]
    fn connect_unknown_id_errors() {
        let backend = FakeBackend::default();
        let err = Sandbox::connect(&backend, "missing")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no sandbox with id"), "{err}");
    }

    #[test]
    fn command_options_build_limits() {
        let backend = FakeBackend::default();
        let sandbox = Sandbox::create(&backend, options()).unwrap();
        sandbox
            .commands()
            .run(
                "sleep",
                CommandOptions {
                    timeout_ms: Some(1000),
                    max_output_bytes: Some(2048),
                    ..CommandOptions::default()
                },
            )
            .unwrap();
        let request = backend.last_request.borrow().clone().unwrap();
        let limits = request.limits.unwrap();
        assert_eq!(limits.timeout_ms, Some(1000));
        assert_eq!(limits.max_output_bytes, Some(2048));
    }

    #[test]
    fn non_zero_exit_is_not_success() {
        let backend = FakeBackend::default();
        *backend.next_result.borrow_mut() = Some(DispatchResult::process(
            "r".to_string(),
            Status::Failure,
            1,
            String::new(),
            "boom\n".to_string(),
            Some(1),
            false,
        ));
        let sandbox = Sandbox::create(&backend, options()).unwrap();
        let result = sandbox
            .commands()
            .run("false", CommandOptions::default())
            .unwrap();
        assert!(!result.success());
        assert_eq!(result.exit_code, Some(1));
        assert_eq!(result.stderr, "boom\n");
    }
}
