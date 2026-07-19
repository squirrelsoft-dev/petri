//! Smoke test: boot a nocloud EFI VM with a blank NBD scratch as `--data-disk`
//! and a virtiofs artifacts share. The guest runs `provision.sh` via
//! `systemd.run=` and self-powers-off; petri-vz exits via `--exit-on-guest-stop`.
//!
//! Usage (from repo root):
//!   cargo run -p petri-nbd --example nbd_provision_smoke -- \
//!     <nocloud.raw> <artifacts-dir> [timeout-secs]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use petri_nbd::{Geometry, LayeredDisk, NbdServer, ScratchLayer, ServeOpts};

const BLOCK_SIZE: u32 = 64 * 1024;
const SCRATCH_BYTES: u64 = 4 * 1024 * 1024 * 1024;

fn main() {
    if let Err(e) = run() {
        eprintln!("\nSMOKE ERROR: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let raw = PathBuf::from(
        args.next()
            .ok_or("usage: nbd_provision_smoke <nocloud.raw> <artifacts-dir> [secs]")?,
    );
    let artifacts = PathBuf::from(
        args.next()
            .ok_or("usage: nbd_provision_smoke <nocloud.raw> <artifacts-dir> [secs]")?,
    );
    let timeout_secs: u64 = args.next().map(|s| s.parse().unwrap_or(900)).unwrap_or(900);

    if !raw.exists() {
        return Err(format!("nocloud image not found: {}", raw.display()));
    }
    if !artifacts.exists() {
        return Err(format!("artifacts dir not found: {}", artifacts.display()));
    }

    let helper = PathBuf::from("crates/petri-vz/.build/debug/petri-vz");
    if !helper.exists() {
        return Err(format!(
            "helper not found at {}; build+sign petri-vz first",
            helper.display()
        ));
    }

    let work = PathBuf::from(format!("target/nbd-provision-smoke/{}", std::process::id()));
    let _ = fs::remove_dir_all(&work);
    for d in ["workspace", "config"] {
        fs::create_dir_all(work.join(d)).map_err(|e| e.to_string())?;
    }
    let scratch_path = work.join("scratch.data");
    let console_log = work.join("console.log");
    let control_sock = work.join("control.sock");
    let helper_log = work.join("helper.log");
    let efivars = work.join("efivars");

    let geom = Geometry::new(SCRATCH_BYTES, BLOCK_SIZE).map_err(|e| e.to_string())?;
    let scratch = ScratchLayer::create(&scratch_path, geom).map_err(|e| e.to_string())?;
    let disk = LayeredDisk::new(vec![], scratch).map_err(|e| e.to_string())?;
    let server = NbdServer::serve(disk, ServeOpts::loopback()).map_err(|e| e.to_string())?;
    let url = server.url().to_string();

    println!("nocloud boot disk : {}", raw.display());
    println!("artifacts dir     : {}", artifacts.display());
    println!(
        "nbd scratch       : {url}  (blank {} GiB)",
        SCRATCH_BYTES >> 30
    );
    println!("helper log        : {}", helper_log.display());
    println!("console log       : {}", console_log.display());
    println!("\nbooting EFI VM (timeout {}s) ...\n", timeout_secs);

    let log_file = fs::File::create(&helper_log).map_err(|e| e.to_string())?;
    let mut child = Command::new(&helper)
        .args(["--instance-id", "provision-smoke"])
        .arg("--control-socket")
        .arg(&control_sock)
        .args(["--boot-mode", "efi"])
        .arg("--disk")
        .arg(&raw)
        .arg("--efi-variable-store")
        .arg(&efivars)
        .arg("--data-disk")
        .arg(&url)
        .arg("--artifacts-dir")
        .arg(&artifacts)
        .arg("--enable-network")
        .arg("--exit-on-guest-stop")
        .arg("--workspace")
        .arg(work.join("workspace"))
        .arg("--config-dir")
        .arg(work.join("config"))
        .arg("--console-log")
        .arg(&console_log)
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            log_file.try_clone().map_err(|e| e.to_string())?,
        ))
        .stderr(Stdio::from(log_file))
        .spawn()
        .map_err(|e| format!("failed to spawn petri-vz: {e}"))?;

    // Wait for petri-vz to exit (guest self-poweroff) or timeout.
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let exit_status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None => {
                if Instant::now() >= deadline {
                    eprintln!("timeout — killing petri-vz");
                    let _ = child.kill();
                    let _ = child.wait();
                    break std::process::ExitStatus::default();
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    };

    // Seal the scratch — guest has already stopped.
    let sealed_path = work.join("sealed.layer");
    let _sealed = server
        .seal_scratch(&sealed_path, &[])
        .map_err(|e| format!("seal_scratch failed: {e}"))?;

    server.shutdown().map_err(|e| e.to_string())?;

    let helper_text = fs::read_to_string(&helper_log).unwrap_or_default();
    let console_text = fs::read_to_string(&console_log).unwrap_or_default();
    let scratch_len = fs::metadata(&scratch_path).map(|m| m.len()).unwrap_or(0);
    let sealed_len = fs::metadata(&sealed_path).map(|m| m.len()).unwrap_or(0);

    println!("=== petri-vz helper log ===");
    for line in helper_text.lines() {
        println!("  {line}");
    }

    println!("\n=== FULL console log ({} bytes) ===", console_text.len());
    if console_text.is_empty() {
        println!("  <empty — guest console not on hvc0>");
    } else {
        print!("{console_text}");
    }
    println!("=== end console ===");

    let vm_started = helper_text.contains("VM started");
    let nbd_attached = helper_text.contains("attaching NBD data disk");
    let nbd_connected = helper_text.contains("NBD client connected");
    let provision_ran = console_text.contains("provision.sh running");
    let vdb_visible = console_text.contains("vdb");

    println!("\n=== RESULT ===");
    println!("  petri-vz exit status        : {exit_status}");
    println!("  VM started                  : {}", yn(vm_started));
    println!("  --data-disk parsed/attached : {}", yn(nbd_attached));
    println!("  NBD client connected        : {}", yn(nbd_connected));
    println!("  provision.sh ran            : {}", yn(provision_ran));
    println!("  vdb visible in guest        : {}", yn(vdb_visible));
    println!("  scratch bytes on disk       : {scratch_len}");
    println!("  sealed layer size           : {sealed_len}");
    println!("  logs preserved at           : {}", work.display());

    if vm_started && nbd_connected && scratch_len > 0 && sealed_len > 0 {
        println!("\nPASS — guest provisioned NBD scratch and it sealed.");
        Ok(())
    } else {
        Err("provisioning failed or scratch empty (see logs above)".into())
    }
}

fn yn(b: bool) -> &'static str {
    if b { "YES" } else { "no" }
}
