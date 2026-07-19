//! End-to-end host-to-guest dispatch test (#14).
//!
//! Boots a real microVM through the macOS Apple Virtualization backend, mounts a
//! workspace, dispatches commands over vsock via the SDK, and asserts the full
//! round trip:
//!
//! - a command runs in the guest and its stdout comes back to the host;
//! - a file written by the guest into `/workspace` is visible on the host;
//! - the default `network_enabled = false` policy leaves the guest with no
//!   network device attached (only loopback exists).
//!
//! This is `#[ignore]`d by default: it requires macOS, a built `petri-vz`
//! helper, and a built base image bundle. When those prerequisites are missing
//! the test skips with a message rather than failing, mirroring
//! `petri-guest/tests/lsp_real_server.rs`.
//!
//! Run it explicitly once the prerequisites are in place:
//!
//! ```sh
//! # 1. Build the Swift helper (macOS only) and codesign it with the
//! #    virtualization entitlement — an unsigned helper is rejected at VM
//! #    config time ("doesn't have the com.apple.security.virtualization
//! #    entitlement").
//! (cd crates/petri-vz && swift build)
//! codesign --force --sign - \
//!   --entitlements crates/petri-vz/petri-vz.entitlements \
//!   crates/petri-vz/.build/debug/petri-vz
//! # (or point PETRI_VZ_BIN at an already-signed helper)
//! # 2. Build the base image bundle:
//! ./scripts/build-base-image.sh           # writes target/petri-images/base/
//! # 3. Run the test:
//! cargo test -p petri --test e2e_dispatch -- --ignored --nocapture
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use petri::{CommandOptions, InstanceId, MacosBackend, Sandbox, SandboxOptions, Status};

/// Repo root, derived the same way the backend derives its fallback
/// (`CARGO_MANIFEST_DIR/../..`).
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// Resolve a usable `petri-vz` helper binary, or `None` to skip.
///
/// Prefers `PETRI_VZ_BIN`, then a `petri-vz` on `PATH`, then the Swift build
/// output under `crates/petri-vz/.build/{release,debug}/`.
fn resolve_helper() -> Option<PathBuf> {
    if let Some(bin) = std::env::var_os("PETRI_VZ_BIN") {
        let path = PathBuf::from(bin);
        return path.is_file().then_some(path);
    }

    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("petri-vz");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let build_dir = repo_root().join("crates/petri-vz/.build");
    for profile in ["release", "debug"] {
        let candidate = build_dir.join(profile).join("petri-vz");
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    None
}

/// Resolve a built base image bundle directory, or `None` to skip.
///
/// Prefers `PETRI_BASE_IMAGE`, then `target/petri-images/base/`. A directory
/// only counts if it holds the `petri-image.json` manifest the backend loads.
fn resolve_image() -> Option<PathBuf> {
    let dir = std::env::var_os("PETRI_BASE_IMAGE")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root().join("target/petri-images/base"));
    dir.join("petri-image.json").is_file().then_some(dir)
}

/// Tear the VM down when the guard drops, so a failed assertion never leaks a
/// running helper/VM. Disarm with [`VmGuard::disarm`] after a clean teardown.
struct VmGuard {
    backend: MacosBackend,
    id: InstanceId,
    armed: bool,
}

impl VmGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for VmGuard {
    fn drop(&mut self) {
        if self.armed {
            use petri::HostBackend;
            let _ = self.backend.teardown(&self.id);
        }
    }
}

#[test]
#[ignore = "requires macOS, a built petri-vz helper, and a built base image bundle"]
fn e2e_host_to_guest_dispatch() {
    if !cfg!(target_os = "macos") {
        eprintln!("skipping e2e_host_to_guest_dispatch: Apple Virtualization is macOS-only");
        return;
    }
    let Some(helper) = resolve_helper() else {
        eprintln!(
            "skipping e2e_host_to_guest_dispatch: no petri-vz helper \
             (set PETRI_VZ_BIN or build crates/petri-vz)"
        );
        return;
    };
    let Some(image) = resolve_image() else {
        eprintln!(
            "skipping e2e_host_to_guest_dispatch: no base image bundle \
             (set PETRI_BASE_IMAGE or run ./scripts/build-base-image.sh)"
        );
        return;
    };

    // macOS caps Unix-domain socket paths at ~104 bytes, and the backend's
    // control socket lives at `<state_dir>/<id>/petri-vz.sock`. Keep the base
    // path and id short (under /tmp, not the long /var/folders temp dir) so the
    // socket path stays well under the limit.
    let suffix = unique_suffix() % 100_000_000;
    let scratch = PathBuf::from(format!("/tmp/pz{suffix}"));
    let state_dir = scratch.join("s");
    let workspace = scratch.join("ws");
    fs::create_dir_all(&state_dir).unwrap();
    fs::create_dir_all(&workspace).unwrap();

    // A file the guest can read back, proving the host workspace is mounted.
    fs::write(workspace.join("seed.txt"), "seed-from-host\n").unwrap();

    // Default-posture policy: network off (no device attached), a small command
    // allowlist covering everything the test dispatches.
    let policy_path = scratch.join("policy.toml");
    fs::write(
        &policy_path,
        r#"[policy]
network_enabled = false
allowed_commands = ["echo", "ls", "cat", "sh"]
max_runtime_secs = 30
max_output_bytes = 1048576
workspace_path = "/workspace"
"#,
    )
    .unwrap();

    let backend = MacosBackend::new(&state_dir, &helper);
    let sandbox_id = format!("e{suffix}");
    let options = SandboxOptions::new(&workspace, &policy_path)
        .with_image(&image)
        .with_id(&sandbox_id);

    // Boot the VM. create() blocks until the guest reports ready.
    let sandbox =
        Sandbox::create(backend.clone(), options).expect("create should boot the VM to Ready");
    let mut guard = VmGuard {
        backend: backend.clone(),
        id: InstanceId::new(&sandbox_id).unwrap(),
        armed: true,
    };
    assert!(sandbox.is_running().unwrap(), "sandbox should be running");

    // 1. Dispatch a command over vsock and observe its stdout on the host.
    let echo = sandbox
        .commands()
        .run(
            "echo",
            CommandOptions {
                args: vec!["hello-from-guest".to_string()],
                ..CommandOptions::default()
            },
        )
        .expect("echo dispatch should succeed");
    assert_eq!(echo.status, Status::Success, "echo result: {echo:?}");
    assert_eq!(echo.stdout, "hello-from-guest\n");

    // 1b. The workspace mount is readable from the guest.
    let read_seed = sandbox
        .commands()
        .run(
            "cat",
            CommandOptions {
                args: vec!["/workspace/seed.txt".to_string()],
                ..CommandOptions::default()
            },
        )
        .expect("cat dispatch should succeed");
    assert_eq!(
        read_seed.status,
        Status::Success,
        "cat result: {read_seed:?}"
    );
    assert_eq!(read_seed.stdout, "seed-from-host\n");

    // 2. The guest writes a file into /workspace; the host observes it.
    let write = sandbox
        .commands()
        .run(
            "sh",
            CommandOptions {
                args: vec![
                    "-c".to_string(),
                    "echo guest-wrote-this > /workspace/from-guest.txt".to_string(),
                ],
                ..CommandOptions::default()
            },
        )
        .expect("workspace write dispatch should succeed");
    assert_eq!(write.status, Status::Success, "write result: {write:?}");
    let observed = fs::read_to_string(workspace.join("from-guest.txt"))
        .expect("host should see the file the guest wrote");
    assert_eq!(observed, "guest-wrote-this\n");

    // 3. Default network-off policy: no network device is attached, so the guest
    //    sees only the loopback interface.
    let interfaces = sandbox
        .commands()
        .run(
            "ls",
            CommandOptions {
                args: vec!["/sys/class/net".to_string()],
                ..CommandOptions::default()
            },
        )
        .expect("interface listing dispatch should succeed");
    assert_eq!(
        interfaces.status,
        Status::Success,
        "interface listing: {interfaces:?}"
    );
    let names: Vec<&str> = interfaces.stdout.split_whitespace().collect();
    assert_eq!(
        names,
        vec!["lo"],
        "network-off guest should have only loopback, saw: {names:?}"
    );

    // Clean teardown.
    sandbox.kill().expect("kill should tear the VM down");
    guard.disarm();
    assert!(!sandbox.is_running().unwrap(), "sandbox should be stopped");

    let _ = fs::remove_dir_all(&scratch);
}

/// Path helper sanity: the resolvers agree with the manifest/helper layout even
/// when the artifacts are absent, so the skip path is exercised in plain `cargo
/// test` (non-`--ignored`) runs.
#[test]
fn resolvers_probe_expected_locations() {
    // resolve_image only returns a dir that actually holds the manifest.
    if let Some(dir) = resolve_image() {
        assert!(dir.join("petri-image.json").is_file());
    }
    // resolve_helper only returns an existing file.
    if let Some(helper) = resolve_helper() {
        assert!(Path::new(&helper).is_file());
    }
}
