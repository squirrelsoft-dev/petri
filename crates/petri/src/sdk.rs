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
use std::path::{Path, PathBuf};
use std::sync::Arc;

use petri_nbd::{ImmutableLayer, LayeredDisk, NbdHandle, NbdServer, ScratchLayer, ServeOpts};

use crate::backend::HostBackend;
use crate::dispatch::{DispatchRequest, ErrorFrame, RequestLimits, Status};
use crate::error::{PetriError, Result};
use crate::instance::{DirectBoot, InstanceConfig, InstanceHandle, InstanceId, LifecycleState};

/// Default backend name used when [`SandboxOptions`] does not specify one.
pub const DEFAULT_BACKEND: &str = "macos";

/// Scratch-overlay file name inside a sandbox's scratch directory. Matches the
/// image store's [`crate::image::ImagePaths::scratch_data`] naming.
const SCRATCH_FILE: &str = "scratch.data";

/// Options for [`Sandbox::create`].
///
/// `workspace` and `policy` are required by the current local backend; the
/// remaining fields mirror the cross-language `SandboxOpts` shape. `metadata` is
/// persisted with the instance and can be filtered on via
/// `Sandbox::list`/`sandbox list --metadata`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxOptions {
    /// Explicit sandbox id. When `None`, a unique id is generated.
    pub id: Option<String>,
    /// Backend name. Defaults to [`DEFAULT_BACKEND`].
    pub backend: String,
    /// Image bundle path. When `None`, the backend's default base image is used.
    pub image: Option<PathBuf>,
    /// Directory holding this sandbox's scratch overlay. See
    /// [`SandboxOptions::with_scratch_overlay`].
    pub scratch_overlay: Option<PathBuf>,
    /// Host workspace directory mounted into the sandbox.
    pub workspace: PathBuf,
    /// Policy file applied at boot.
    pub policy: PathBuf,
    /// Free-form metadata persisted with the instance and filterable via list.
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
            scratch_overlay: None,
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

    /// Boot the image read-only under a fresh writable scratch overlay kept in
    /// `scratch_dir`, instead of attaching the bundle's disk directly.
    ///
    /// The guest sees one disk; every write lands in the scratch, so the image
    /// is never modified and can be shared by concurrent sandboxes. The scratch
    /// is *ephemeral*: it is recreated empty on each boot and discarded on
    /// [`Sandbox::kill`], so nothing carries over between runs. Requires
    /// [`with_image`](Self::with_image) and a bundle that can direct-boot (see
    /// [`bundle_boot_files`](crate::backend::bundle_boot_files)).
    ///
    /// The overlay is served over NBD *in-process*, so it lives exactly as long
    /// as the [`Sandbox`] handle: drop every clone and the export stops, taking
    /// the VM's disk with it. That makes a sandbox unable to outlive the process
    /// that created it — the tradeoff for not needing a detached daemon, and the
    /// reason `petri sandbox create --base` spawns one instead (its CLI process
    /// exits immediately). It also keeps [`NbdHandle::seal_scratch`] reachable
    /// in-process, which a detached daemon puts behind a control channel.
    pub fn with_scratch_overlay(mut self, scratch_dir: impl Into<PathBuf>) -> Self {
        self.scratch_overlay = Some(scratch_dir.into());
        self
    }

    /// Attach metadata to the sandbox; persisted and filterable via list.
    pub fn with_metadata(mut self, metadata: BTreeMap<String, String>) -> Self {
        self.metadata = metadata;
        self
    }

    /// Resolve to a bootable [`InstanceConfig`], plus the NBD export backing it
    /// when a scratch overlay was requested.
    ///
    /// The export is returned rather than dropped here because it must outlive
    /// this call: it *is* the VM's disk. [`Sandbox::create`] parks it in the
    /// handle it returns.
    fn into_config(self) -> Result<(InstanceConfig, Option<NbdHandle>)> {
        let id = match self.id {
            Some(id) => InstanceId::new(id)?,
            None => InstanceId::new(generate_sandbox_id()?)?,
        };
        let config = InstanceConfig::new(id, self.backend, self.workspace, self.policy)
            .with_metadata(self.metadata);

        match (self.image, self.scratch_overlay) {
            (Some(image), Some(scratch_dir)) => {
                let (direct_boot, nbd) = serve_scratch_overlay(&image, &scratch_dir)?;
                // Deliberately no `with_image`: the disk reaches the VM as the
                // NBD export, and setting both would attach the base twice —
                // once writable, which is the whole thing we are preventing.
                Ok((config.with_direct_boot(direct_boot), Some(nbd)))
            }
            (Some(image), None) => Ok((config.with_image(image), None)),
            (None, Some(_)) => Err(PetriError::InvalidConfig(
                "a scratch overlay needs an image to overlay; set image too".to_string(),
            )),
            (None, None) => Ok((config, None)),
        }
    }
}

/// Stack a fresh scratch over the bundle's disk and export it over NBD in this
/// process, yielding the direct-boot spec and the handle that keeps it alive.
///
/// The base is opened as an immutable layer, so the VM physically cannot write
/// to it: every guest write is redirected into `scratch.data`. That is what
/// lets several sandboxes share one image, and what stops a crashed VM from
/// corrupting it.
fn serve_scratch_overlay(image: &Path, scratch_dir: &Path) -> Result<(DirectBoot, NbdHandle)> {
    let boot = crate::backend::bundle_boot_files(image)?;
    let size = std::fs::metadata(&boot.disk)
        .map_err(|source| PetriError::Io {
            path: boot.disk.clone(),
            source,
        })?
        .len();
    let geometry = crate::image::default_geometry(size)?;

    std::fs::create_dir_all(scratch_dir).map_err(|source| PetriError::Io {
        path: scratch_dir.to_path_buf(),
        source,
    })?;
    let scratch_path = scratch_dir.join(SCRATCH_FILE);
    // Ephemeral: a leftover scratch from a previous boot would resurrect that
    // run's filesystem, so start empty every time.
    if let Err(source) = std::fs::remove_file(&scratch_path)
        && source.kind() != std::io::ErrorKind::NotFound
    {
        return Err(PetriError::Io {
            path: scratch_path,
            source,
        });
    }

    let io_err = |path: &Path| {
        let path = path.to_path_buf();
        move |source| PetriError::Io { path, source }
    };
    let base = ImmutableLayer::open_raw_base(&boot.disk, geometry).map_err(io_err(&boot.disk))?;
    let scratch = ScratchLayer::create(&scratch_path, geometry).map_err(io_err(&scratch_path))?;
    let disk = LayeredDisk::new(vec![base], scratch).map_err(io_err(&scratch_path))?;
    let nbd = NbdServer::serve(disk, ServeOpts::loopback()).map_err(io_err(scratch_dir))?;

    Ok((
        DirectBoot {
            nbd_url: nbd.url().to_string(),
            kernel: boot.kernel,
            initrd: boot.initrd,
            cmdline: boot.cmdline,
        },
        nbd,
    ))
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
#[derive(Clone)]
pub struct Sandbox<B: HostBackend> {
    backend: B,
    id: InstanceId,
    backend_name: String,
    metadata: BTreeMap<String, String>,
    /// The in-process NBD export serving this sandbox's disk, when it booted
    /// under a scratch overlay. Shared across clones: the export stops when the
    /// last of them drops, which would pull the disk out from under a running
    /// VM — so this outlives every clone by construction, and `kill` is what
    /// ends it. `None` for image/firmware boots, where the disk is a plain file
    /// the VM opens itself.
    nbd: Option<Arc<NbdHandle>>,
    /// Where this sandbox's scratch overlay lives, so [`Sandbox::kill`] can
    /// discard it. Set only alongside `nbd`.
    scratch_dir: Option<PathBuf>,
}

// Hand-written because `NbdHandle` owns a `LayeredDisk`, which is not `Debug`.
// The url is the useful part anyway — it identifies the export.
impl<B: HostBackend + std::fmt::Debug> std::fmt::Debug for Sandbox<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sandbox")
            .field("backend", &self.backend)
            .field("id", &self.id)
            .field("backend_name", &self.backend_name)
            .field("metadata", &self.metadata)
            .field("nbd_url", &self.nbd.as_ref().map(|nbd| nbd.url()))
            .finish()
    }
}

impl<B: HostBackend> Sandbox<B> {
    /// Create a new sandbox from the given options and return a handle to it.
    pub fn create(backend: B, options: SandboxOptions) -> Result<Self> {
        let scratch_dir = options.scratch_overlay.clone();
        let (config, nbd) = options.into_config()?;
        // A failed boot must not leave the export (and its accept thread)
        // running: `nbd` is still owned here, so `?` drops it and stops it.
        let handle = backend.create(config)?;
        Ok(Self {
            backend,
            id: handle.id,
            backend_name: handle.backend,
            metadata: handle.metadata,
            nbd: nbd.map(Arc::new),
            scratch_dir,
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
            metadata: handle.metadata,
            // An export cannot be adopted: it lives in the process that created
            // it, keyed by an in-memory handle. Connecting to a scratch-overlay
            // sandbox dispatches fine (that goes over vsock), but its `kill`
            // ends the VM only — the export stops when its creator lets go.
            nbd: None,
            // Likewise not ours to discard: the creator owns the scratch.
            scratch_dir: None,
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

    /// Metadata persisted with this sandbox (empty when none was set).
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
    ///
    /// A scratch overlay is discarded here too — it is ephemeral, and the next
    /// boot starts from the pristine image. Unlinking while the export still
    /// holds the file open is deliberate: the bytes are reclaimed when the
    /// export closes, and it saves a second cleanup path for the common case
    /// where the handle drops right after this.
    pub fn kill(&self) -> Result<()> {
        let result = self.backend.teardown(&self.id);
        if let Some(scratch_dir) = &self.scratch_dir {
            // The whole directory, not just the file: it is this sandbox's and
            // nothing else writes there, so leaving the husk behind would leak
            // one empty directory per run.
            let _ = std::fs::remove_dir_all(scratch_dir);
        }
        result
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
        let request_id = options.request_id.clone().map_or_else(
            || Ok::<_, PetriError>(format!("commands-run-{}", generate_sandbox_id()?)),
            Ok,
        )?;
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
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_root() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("petri-sdk-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A minimal direct-bootable image bundle: the four files conduit-style
    /// prebuilt bundles ship. The disk must be a whole number of 64 KiB blocks
    /// or the geometry is rejected.
    fn image_bundle(root: &Path) -> PathBuf {
        let bundle = root.join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("root.img"), vec![7u8; 128 * 1024]).unwrap();
        std::fs::write(bundle.join("vmlinuz"), b"kernel").unwrap();
        std::fs::write(bundle.join("initrd.img"), b"initrd").unwrap();
        std::fs::write(
            bundle.join("petri-image.json"),
            r#"{
                "architecture": "aarch64",
                "kernel": "vmlinuz",
                "disk": "root.img",
                "initrd": "initrd.img",
                "kernel_command_line": "root=/dev/vda1 rw"
            }"#,
        )
        .unwrap();
        bundle
    }

    fn options_over(bundle: &Path, root: &Path) -> SandboxOptions {
        SandboxOptions::new(root.join("workspace"), root.join("policy.toml"))
            .with_id("sb-1")
            .with_image(bundle)
            .with_scratch_overlay(root.join("scratch"))
    }

    #[test]
    fn scratch_overlay_boots_direct_from_an_nbd_export() {
        let root = temp_root();
        let bundle = image_bundle(&root);
        let (config, nbd) = options_over(&bundle, &root).into_config().unwrap();

        let boot = config.direct_boot.expect("direct boot");
        assert_eq!(boot.kernel, bundle.join("vmlinuz").canonicalize().unwrap());
        assert_eq!(
            boot.initrd,
            bundle.join("initrd.img").canonicalize().unwrap()
        );
        assert_eq!(boot.cmdline, "root=/dev/vda1 rw");
        assert_eq!(boot.nbd_url, nbd.expect("export").url());
        // The base must reach the VM only through the export. Setting `image`
        // too would attach it a second time, writable.
        assert!(config.image.is_none());
    }

    #[test]
    fn scratch_overlay_never_writes_to_the_base_image() {
        let root = temp_root();
        let bundle = image_bundle(&root);
        let disk = bundle.join("root.img");
        let before = std::fs::read(&disk).unwrap();

        let (_config, nbd) = options_over(&bundle, &root).into_config().unwrap();
        drop(nbd);

        assert_eq!(
            std::fs::read(&disk).unwrap(),
            before,
            "base image was modified"
        );
        assert!(root.join("scratch").join(SCRATCH_FILE).exists());
    }

    #[test]
    fn scratch_overlay_starts_empty_on_every_boot() {
        let root = temp_root();
        let bundle = image_bundle(&root);
        let scratch = root.join("scratch").join(SCRATCH_FILE);
        std::fs::create_dir_all(root.join("scratch")).unwrap();
        // A leftover scratch from a previous run would resurrect that run's
        // filesystem if it were reused rather than recreated.
        std::fs::write(&scratch, b"stale overlay from a previous boot").unwrap();

        let (_config, _nbd) = options_over(&bundle, &root).into_config().unwrap();

        let contents = std::fs::read(&scratch).unwrap();
        assert!(
            !contents.starts_with(b"stale overlay"),
            "stale scratch was reused"
        );
    }

    #[test]
    fn scratch_overlay_without_an_image_is_rejected() {
        let root = temp_root();
        let err = SandboxOptions::new(root.join("workspace"), root.join("policy.toml"))
            .with_scratch_overlay(root.join("scratch"))
            .into_config()
            .map(|(config, _)| config) // NbdHandle is not Debug; unwrap_err needs it
            .unwrap_err();
        assert!(matches!(err, PetriError::InvalidConfig(_)), "got {err:?}");
    }

    #[test]
    fn an_image_without_a_scratch_overlay_still_attaches_directly() {
        let root = temp_root();
        let bundle = image_bundle(&root);
        let (config, nbd) = SandboxOptions::new(root.join("workspace"), root.join("policy.toml"))
            .with_image(&bundle)
            .into_config()
            .unwrap();

        assert_eq!(config.image.as_deref(), Some(bundle.as_path()));
        assert!(config.direct_boot.is_none());
        assert!(nbd.is_none(), "no export should exist without an overlay");
    }

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
                metadata: BTreeMap::new(),
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
                metadata: config.metadata,
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
    fn metadata_is_persisted_through_create_and_connect() {
        let backend = FakeBackend::default();
        let metadata = BTreeMap::from([("env".to_string(), "prod".to_string())]);
        let created = Sandbox::create(&backend, options().with_metadata(metadata.clone())).unwrap();
        assert_eq!(created.metadata(), &metadata);

        // The backend recorded the metadata, so a fresh connect rehydrates it.
        let connected = Sandbox::connect(&backend, "dev-1").unwrap();
        assert_eq!(connected.metadata(), &metadata);

        // And it surfaces on the listed handle.
        let handles = Sandbox::list(&&backend).unwrap();
        let handle = handles.iter().find(|h| h.id.as_str() == "dev-1").unwrap();
        assert_eq!(handle.metadata, metadata);
        assert!(handle.matches_metadata(&metadata));
        assert!(
            !handle.matches_metadata(&BTreeMap::from([("env".to_string(), "dev".to_string())]))
        );
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
