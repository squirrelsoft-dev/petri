//! Milestone 3 smoke test: boot a real Petri VM from a base image served over
//! NBD with a writable scratch overlay, and confirm guest writes land in the
//! scratch (not the base).
//!
//! This is an example, not a unit test: it needs a built+signed `petri-vz`
//! helper (with the `com.apple.security.network.client` entitlement), a base
//! image bundle, and the ability to run a VM on this host.
//!
//! Usage (from the repo root):
//!   cargo run -p petri-nbd --example nbd_boot_smoke -- [BUNDLE_DIR] [BOOT_SECS]
//!
//! Defaults: BUNDLE_DIR=target/petri-images/base, BOOT_SECS=45.

// Diagnostic harness: byte counts are converted to f64 only to print
// human-readable GiB, and elapsed seconds to i64 only for a countdown
// display. Neither feeds storage arithmetic, so precision and sign are
// immaterial here.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::cast_lossless
)]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use petri_nbd::{
    BindMode, Geometry, ImmutableLayer, LayeredDisk, NbdServer, ScratchLayer, ServeOpts,
};

const BLOCK_SIZE: u32 = 64 * 1024;

fn main() {
    if let Err(e) = run() {
        eprintln!("\nSMOKE TEST ERROR: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let bundle = PathBuf::from(
        args.next()
            .unwrap_or_else(|| "target/petri-images/base".into()),
    );
    let boot_secs: u64 = args.next().map(|s| s.parse().unwrap_or(45)).unwrap_or(45);

    let helper = PathBuf::from("crates/petri-vz/.build/debug/petri-vz");
    if !helper.exists() {
        return Err(format!(
            "helper not found at {}; build+sign it first:\n  swift build --package-path crates/petri-vz\n  codesign --force --sign - --entitlements crates/petri-vz/petri-vz.entitlements {}",
            helper.display(),
            helper.display()
        ));
    }

    let kernel = bundle.join("vmlinuz");
    let initrd = bundle.join("initrd.img");
    let disk = bundle.join("root.img");
    let manifest = bundle.join("petri-image.json");
    for p in [&kernel, &disk, &manifest] {
        if !p.exists() {
            return Err(format!("missing bundle file: {}", p.display()));
        }
    }
    let cmdline = extract_cmdline(&manifest)?;

    // --- Build the layered disk: read-only base + fresh writable scratch ---
    let base_len = fs::metadata(&disk).map_err(|e| e.to_string())?.len();
    if !base_len.is_multiple_of(BLOCK_SIZE as u64) {
        return Err(format!(
            "base image size {base_len} is not a multiple of the {BLOCK_SIZE}-byte block size"
        ));
    }
    let geometry = Geometry::new(base_len, BLOCK_SIZE).map_err(|e| e.to_string())?;

    let work = PathBuf::from(format!("target/nbd-smoke/{}", std::process::id()));
    fs::create_dir_all(work.join("workspace")).map_err(|e| e.to_string())?;
    fs::create_dir_all(work.join("config")).map_err(|e| e.to_string())?;
    let scratch_path = work.join("scratch.data");
    let console_log = work.join("console.log");
    let control_sock = work.join("control.sock");
    let helper_log = work.join("helper.log");

    let base_layer = ImmutableLayer::open_raw_base(&disk, geometry).map_err(|e| e.to_string())?;
    let scratch = ScratchLayer::create(&scratch_path, geometry).map_err(|e| e.to_string())?;
    let layered = LayeredDisk::new(vec![base_layer], scratch).map_err(|e| e.to_string())?;

    println!(
        "base image     : {} ({:.2} GiB)",
        disk.display(),
        base_len as f64 / (1u64 << 30) as f64
    );
    println!("scratch overlay: {}", scratch_path.display());

    // --- Start the NBD server ---
    let server = NbdServer::serve(
        layered,
        ServeOpts {
            bind: BindMode::LoopbackTcp(0),
            export_name: "petri".into(),
            read_only: false,
        },
    )
    .map_err(|e| e.to_string())?;
    let nbd_url = server.url().to_string();
    println!("nbd server     : {nbd_url}");

    // Record base identity so we can prove it was never written.
    let base_meta_before = fs::metadata(&disk).map_err(|e| e.to_string())?;
    let base_mtime_before = base_meta_before.modified().ok();

    // --- Launch the VM helper attached to the NBD disk ---
    let log_file = fs::File::create(&helper_log).map_err(|e| e.to_string())?;
    let mut child = Command::new(&helper)
        .args(["--instance-id", "nbd-smoke"])
        .args(["--control-socket", control_sock.to_str().unwrap()])
        .args(["--boot-mode", "linux"])
        .args(["--kernel", kernel.to_str().unwrap()])
        .args(["--initrd", initrd.to_str().unwrap()])
        .args(["--nbd-disk", &nbd_url])
        .args(["--workspace", work.join("workspace").to_str().unwrap()])
        .args(["--config-dir", work.join("config").to_str().unwrap()])
        .args(["--console-log", console_log.to_str().unwrap()])
        .args(["--command-line", &cmdline])
        .stdout(Stdio::from(
            log_file.try_clone().map_err(|e| e.to_string())?,
        ))
        .stderr(Stdio::from(log_file))
        .spawn()
        .map_err(|e| format!("failed to spawn helper: {e}"))?;

    println!(
        "helper pid     : {} (log: {})",
        child.id(),
        helper_log.display()
    );
    println!("\nbooting for {boot_secs}s — watching scratch growth...\n");

    // --- Observe boot + scratch growth ---
    let deadline = Instant::now() + Duration::from_secs(boot_secs);
    let mut peak_scratch = 0u64;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_secs(2));
        let scratch_len = fs::metadata(&scratch_path).map(|m| m.len()).unwrap_or(0);
        peak_scratch = peak_scratch.max(scratch_len);
        let console_len = fs::metadata(&console_log).map(|m| m.len()).unwrap_or(0);
        println!(
            "  t+{:>3}s  scratch={:>9} KiB  console={:>7} B  {}",
            (boot_secs as i64) - (deadline - Instant::now()).as_secs() as i64,
            scratch_len / 1024,
            console_len,
            if child_exited(&mut child) {
                "[helper exited]"
            } else {
                ""
            }
        );
        if child_exited(&mut child) {
            break;
        }
    }

    // --- Best-effort graceful stop, then verdict ---
    let _ = send_control(&control_sock, "{\"type\":\"stop\"}");
    thread::sleep(Duration::from_millis(500));
    let _ = child.kill();
    let _ = child.wait();
    let _ = server.shutdown();

    let final_scratch = fs::metadata(&scratch_path).map(|m| m.len()).unwrap_or(0);
    let base_meta_after = fs::metadata(&disk).map_err(|e| e.to_string())?;
    let base_mtime_after = base_meta_after.modified().ok();
    let base_unchanged =
        base_meta_after.len() == base_meta_before.len() && base_mtime_after == base_mtime_before;

    let log_text = fs::read_to_string(&helper_log).unwrap_or_default();
    let nbd_connected = log_text.contains("NBD client connected");
    let console_text = fs::read_to_string(&console_log).unwrap_or_default();
    let booted = boot_markers(&console_text);

    println!("\n================ VERDICT ================");
    println!("NBD client connected to server : {}", yn(nbd_connected));
    println!(
        "guest reached userspace        : {}  (markers: {booted:?})",
        yn(!booted.is_empty())
    );
    println!(
        "writes landed in scratch       : {}  (peak {} KiB)",
        yn(peak_scratch > 0),
        final_scratch / 1024
    );
    println!("base image left untouched      : {}", yn(base_unchanged));
    println!("helper log : {}", helper_log.display());
    println!("console log: {}", console_log.display());

    if nbd_connected && peak_scratch > 0 && base_unchanged {
        println!("\nRESULT: PASS — booted from NBD, writes isolated to scratch.");
        Ok(())
    } else {
        Err("one or more checks failed (see verdict and logs above)".into())
    }
}

fn child_exited(child: &mut std::process::Child) -> bool {
    matches!(child.try_wait(), Ok(Some(_)))
}

/// Pull `kernel_command_line` out of the bundle manifest without a JSON dep.
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

fn boot_markers(console: &str) -> Vec<&'static str> {
    let mut found = Vec::new();
    for (needle, label) in [
        ("EXT4-fs", "ext4-mount"),
        ("systemd", "systemd"),
        ("Reached target", "reached-target"),
        ("login:", "login"),
    ] {
        if console.contains(needle) {
            found.push(label);
        }
    }
    found
}

fn send_control(sock: &Path, msg: &str) -> std::io::Result<()> {
    let mut s = UnixStream::connect(sock)?;
    s.write_all(msg.as_bytes())?;
    s.write_all(b"\n")?;
    let mut buf = [0u8; 256];
    let _ = s.read(&mut buf);
    Ok(())
}

fn yn(b: bool) -> &'static str {
    if b { "yes" } else { "NO" }
}
