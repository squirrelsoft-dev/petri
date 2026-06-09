//! In-guest verification of the NBD scratch/seal lifecycle — closes the
//! deferred M3/M4 assertions by driving real commands inside the booted guest.
//!
//! It writes a marker file to the guest *root filesystem* (which is the NBD
//! disk — `/workspace` is a separate virtio-fs share and would not exercise the
//! scratch), then checks the marker across three scenarios:
//!
//!   1. Same scratch across reboot  -> marker persists.
//!   2. Fresh scratch               -> marker absent (clean base).
//!   3. base + sealed + fresh scratch -> marker visible (sealed snapshot).
//!
//! Requires the built+signed petri-vz helper (see nbd_boot_smoke).
//!
//! Usage (from the repo root):
//!   cargo run -p petri-nbd --example nbd_inguest_verify -- [BUNDLE_DIR]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use petri_nbd::{
    BindMode, Geometry, ImmutableLayer, LayeredDisk, NbdServer, ScratchLayer, ServeOpts,
};
use serde_json::{Value, json};

const BLOCK_SIZE: u32 = 64 * 1024;
const MARKER_PATH: &str = "/persist-marker";
const MARKER_TEXT: &str = "petri-nbd-was-here";
const READY_TIMEOUT: Duration = Duration::from_secs(90);

fn main() {
    if let Err(e) = run() {
        eprintln!("\nVERIFY ERROR: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let bundle = PathBuf::from(
        args.next()
            .unwrap_or_else(|| "target/petri-images/base".into()),
    );

    let env = BootEnv::resolve(&bundle)?;
    let work = PathBuf::from(format!("target/nbd-verify/{}", std::process::id()));
    fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    write_policy(&work)?;

    let geometry = Geometry::new(env.base_len, BLOCK_SIZE).map_err(|e| e.to_string())?;

    let mut results = Vec::new();

    // ---- Scenario 1: same scratch across reboot -> marker persists ----
    println!("== scenario 1: write marker, reboot on the SAME scratch ==");
    let scratch_path = work.join("scratch1.data");
    let server = serve(&env, geometry, &scratch_path)?;
    {
        let mut vm = Vm::boot(&env, &work, "s1a", server.url())?;
        vm.wait_ready()?;
        let w = vm.dispatch(
            "sh",
            &["-c", &format!("echo {MARKER_TEXT} > {MARKER_PATH} && sync")],
        )?;
        println!(
            "   write marker: status={} exit={:?}",
            w.status, w.exit_code
        );
        vm.stop();
    }
    let persisted = {
        let mut vm = Vm::boot(&env, &work, "s1b", server.url())?;
        vm.wait_ready()?;
        let r = vm.dispatch("cat", &[MARKER_PATH])?;
        println!(
            "   reread marker: status={} stdout={:?}",
            r.status,
            r.stdout.trim()
        );
        vm.stop();
        r.stdout.contains(MARKER_TEXT)
    };
    results.push(("same scratch -> marker persists", persisted));

    // Seal this scratch for scenario 3 before tearing the server down.
    let sealed_path = work.join("sealed");
    let sealed_id = server
        .seal_scratch(&sealed_path, &[])
        .map_err(|e| format!("seal_scratch: {e}"))?
        .content_id();
    println!(
        "   sealed scratch -> layer {}",
        sealed_id.map(|i| i.to_hex()).unwrap_or_default()
    );
    server.shutdown().map_err(|e| e.to_string())?;

    // ---- Scenario 2: fresh scratch -> clean base (marker absent) ----
    println!("\n== scenario 2: fresh scratch -> clean base ==");
    let fresh_clean = {
        let server = serve(&env, geometry, &work.join("scratch2.data"))?;
        let mut vm = Vm::boot(&env, &work, "s2", server.url())?;
        vm.wait_ready()?;
        let r = vm.dispatch("cat", &[MARKER_PATH])?;
        println!(
            "   read marker: status={} exit={:?} stderr={:?}",
            r.status,
            r.exit_code,
            r.stderr.trim()
        );
        vm.stop();
        let _ = server.shutdown();
        // Clean base: the file should NOT exist (cat fails, no marker text).
        !r.stdout.contains(MARKER_TEXT)
    };
    results.push(("fresh scratch -> clean base", fresh_clean));

    // ---- Scenario 3: base + sealed + fresh scratch -> marker visible ----
    println!("\n== scenario 3: base + sealed + fresh scratch -> sealed marker visible ==");
    let sealed_visible = {
        let base = ImmutableLayer::open_raw_base(&env.disk, geometry).map_err(|e| e.to_string())?;
        let sealed = ImmutableLayer::open_sealed(&sealed_path).map_err(|e| e.to_string())?;
        let scratch = ScratchLayer::create(&work.join("scratch3.data"), geometry)
            .map_err(|e| e.to_string())?;
        let layered = LayeredDisk::new(vec![base, sealed], scratch).map_err(|e| e.to_string())?;
        let server = NbdServer::serve(
            layered,
            ServeOpts {
                bind: BindMode::LoopbackTcp(0),
                export_name: "petri".into(),
                read_only: false,
            },
        )
        .map_err(|e| e.to_string())?;
        let mut vm = Vm::boot(&env, &work, "s3", server.url())?;
        vm.wait_ready()?;
        let r = vm.dispatch("cat", &[MARKER_PATH])?;
        println!(
            "   read marker: status={} stdout={:?}",
            r.status,
            r.stdout.trim()
        );
        vm.stop();
        let _ = server.shutdown();
        r.stdout.contains(MARKER_TEXT)
    };
    results.push(("base+sealed+fresh -> sealed marker visible", sealed_visible));

    // ---- Verdict ----
    println!("\n================ VERDICT ================");
    let mut all = true;
    for (name, ok) in &results {
        println!("  [{}] {name}", if *ok { "PASS" } else { "FAIL" });
        all &= ok;
    }
    let _ = fs::remove_dir_all(&work);
    if all {
        println!("\nRESULT: PASS — scratch isolation, discard, and seal verified in-guest.");
        Ok(())
    } else {
        Err("one or more in-guest checks failed".into())
    }
}

fn serve(
    env: &BootEnv,
    geometry: Geometry,
    scratch_path: &Path,
) -> Result<petri_nbd::NbdHandle, String> {
    let base = ImmutableLayer::open_raw_base(&env.disk, geometry).map_err(|e| e.to_string())?;
    let scratch = ScratchLayer::create(scratch_path, geometry).map_err(|e| e.to_string())?;
    let layered = LayeredDisk::new(vec![base], scratch).map_err(|e| e.to_string())?;
    NbdServer::serve(
        layered,
        ServeOpts {
            bind: BindMode::LoopbackTcp(0),
            export_name: "petri".into(),
            read_only: false,
        },
    )
    .map_err(|e| e.to_string())
}

/// Resolved bundle paths + cmdline shared across boots.
struct BootEnv {
    helper: PathBuf,
    kernel: PathBuf,
    initrd: PathBuf,
    disk: PathBuf,
    cmdline: String,
    base_len: u64,
}

impl BootEnv {
    fn resolve(bundle: &Path) -> Result<Self, String> {
        let helper = PathBuf::from("crates/petri-vz/.build/debug/petri-vz");
        let kernel = bundle.join("vmlinuz");
        let initrd = bundle.join("initrd.img");
        let disk = bundle.join("root.img");
        let manifest = bundle.join("petri-image.json");
        for p in [&helper, &kernel, &disk, &manifest] {
            if !p.exists() {
                return Err(format!("missing required file: {}", p.display()));
            }
        }
        let cmdline = extract_cmdline(&manifest)?;
        let base_len = fs::metadata(&disk).map_err(|e| e.to_string())?.len();
        Ok(Self {
            helper,
            kernel,
            initrd,
            disk,
            cmdline,
            base_len,
        })
    }
}

/// A running VM helper plus its control socket.
struct Vm {
    child: Child,
    control: PathBuf,
}

impl Vm {
    fn boot(env: &BootEnv, work: &Path, tag: &str, nbd_url: &str) -> Result<Self, String> {
        let inst = work.join(tag);
        fs::create_dir_all(inst.join("workspace")).map_err(|e| e.to_string())?;
        let control = inst.join("control.sock");
        let console = inst.join("console.log");
        let log = fs::File::create(inst.join("helper.log")).map_err(|e| e.to_string())?;

        let child = Command::new(&env.helper)
            .args(["--instance-id", tag])
            .args(["--control-socket", control.to_str().unwrap()])
            .args(["--boot-mode", "linux"])
            .args(["--kernel", env.kernel.to_str().unwrap()])
            .args(["--initrd", env.initrd.to_str().unwrap()])
            .args(["--nbd-disk", nbd_url])
            .args(["--workspace", inst.join("workspace").to_str().unwrap()])
            .args(["--config-dir", work.join("config").to_str().unwrap()])
            .args(["--console-log", console.to_str().unwrap()])
            .args(["--command-line", &env.cmdline])
            .stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
            .stderr(Stdio::from(log))
            .spawn()
            .map_err(|e| format!("spawn helper: {e}"))?;
        Ok(Self { child, control })
    }

    /// Poll the control socket until the guest agent reports `ready`.
    fn wait_ready(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + READY_TIMEOUT;
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(500));
            if let Ok(resp) = self.send(&json!({"type": "status"})) {
                match resp["status"].as_str() {
                    Some("ready") => return Ok(()),
                    Some("error") => return Err(format!("helper error: {resp}")),
                    _ => {}
                }
            }
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return Err("helper exited before becoming ready".into());
            }
        }
        Err("guest did not become ready in time".into())
    }

    /// Run a bash command in the guest (cwd `/workspace`) and return the result.
    fn dispatch(&mut self, command: &str, argv: &[&str]) -> Result<DispatchOut, String> {
        let frame = json!({
            "type": "dispatch",
            "request": {
                "protocol_version": 1,
                "id": format!("verify-{command}"),
                "tool": "bash_command",
                "args": {
                    "command": command,
                    "argv": argv,
                    "cwd": "/workspace",
                    "env": {},
                    "stdin": null
                },
                "limits": { "timeout_ms": 30000, "max_output_bytes": 1048576 }
            }
        });
        let resp = self.send(&frame)?;
        if resp["status"].as_str() != Some("dispatch_result") {
            return Err(format!("unexpected dispatch response: {resp}"));
        }
        let r = &resp["result"];
        Ok(DispatchOut {
            status: r["status"].as_str().unwrap_or("?").to_string(),
            stdout: r["stdout"].as_str().unwrap_or("").to_string(),
            stderr: r["stderr"].as_str().unwrap_or("").to_string(),
            exit_code: r["exit_code"].as_i64(),
        })
    }

    fn stop(&mut self) {
        let _ = self.send(&json!({"type": "stop"}));
        thread::sleep(Duration::from_millis(300));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// One request/response over a fresh control-socket connection.
    fn send(&self, msg: &Value) -> Result<Value, String> {
        let mut s = UnixStream::connect(&self.control).map_err(|e| e.to_string())?;
        s.set_read_timeout(Some(Duration::from_secs(35))).ok();
        let mut line = serde_json::to_vec(msg).map_err(|e| e.to_string())?;
        line.push(b'\n');
        s.write_all(&line).map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = s.read(&mut chunk).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.contains(&b'\n') {
                break;
            }
        }
        let text = String::from_utf8_lossy(&buf);
        let first = text.lines().next().unwrap_or("");
        serde_json::from_str(first).map_err(|e| format!("bad response {first:?}: {e}"))
    }
}

/// Minimal policy that lets the workload write to the rootfs as root.
fn write_policy(work: &Path) -> Result<(), String> {
    let config = work.join("config");
    fs::create_dir_all(&config).map_err(|e| e.to_string())?;
    let policy = "\
[policy]
network_enabled = false
max_runtime_secs = 60
max_output_bytes = 1048576
workspace_path = \"/workspace\"
drop_privileges = false
allowed_commands = [\"sh\", \"cat\"]
";
    fs::write(config.join("policy.toml"), policy).map_err(|e| e.to_string())
}

struct DispatchOut {
    status: String,
    stdout: String,
    stderr: String,
    exit_code: Option<i64>,
}

fn extract_cmdline(manifest: &Path) -> Result<String, String> {
    let text = fs::read_to_string(manifest).map_err(|e| e.to_string())?;
    let key = "\"kernel_command_line\"";
    let start = text
        .find(key)
        .ok_or("manifest has no kernel_command_line")?;
    let after = &text[start + key.len()..];
    let q1 = after.find('"').ok_or("malformed kernel_command_line")?;
    let rest = &after[q1 + 1..];
    let q2 = rest.find('"').ok_or("malformed kernel_command_line")?;
    Ok(rest[..q2].to_string())
}
