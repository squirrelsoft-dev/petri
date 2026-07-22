//! End-to-end bootstrap smoke test (no guest agent): boot an EFI nocloud VM
//! with a NoCloud cidata seed that writes a marker to a blank scratch attached
//! over NBD as a `--data-disk`, then powers the VM off. After the guest writes,
//! we seal the live scratch and read the marker back — proving the bootstrap
//! data path (EFI boot → nocloud-driven write → NBD scratch → seal).
//!
//! Usage (from the repo root):
//!   cargo run -p petri-nbd --example nbd_bootstrap_smoke -- <nocloud.raw> <cidata.iso> [SECS]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use petri_nbd::{Geometry, LayeredDisk, NbdServer, ScratchLayer, ServeOpts};

#[path = "common/preflight.rs"]
mod preflight;

const BLOCK_SIZE: u32 = 64 * 1024;
const SCRATCH_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MARKER: &[u8] = b"PETRI_NBD_MARKER_OK";
const MARKER_OFFSET: u64 = 2048 * 512; // matches the seed's `dd seek=2048`

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
            .ok_or("usage: <nocloud.raw> <cidata.iso> [secs]")?,
    );
    let cidata = PathBuf::from(
        args.next()
            .ok_or("usage: <nocloud.raw> <cidata.iso> [secs]")?,
    );
    let secs: u64 = args.next().map_or(120, |s| s.parse().unwrap_or(120));
    for p in [&raw, &cidata] {
        if !p.exists() {
            return Err(format!("missing input: {}", p.display()));
        }
    }
    let helper = preflight::helper()?;

    let work = PathBuf::from(format!("target/nbd-bootstrap-smoke/{}", std::process::id()));
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

    println!("nocloud boot disk : {}  (vda)", raw.display());
    println!("cidata seed       : {}  (vdb, ro)", cidata.display());
    println!(
        "nbd scratch       : {url}  (blank {} GiB, the build target)",
        SCRATCH_BYTES >> 30
    );
    println!("\nbooting EFI VM (up to {secs}s for cloud-init to write + poweroff)...\n");

    let log_file = fs::File::create(&helper_log).map_err(|e| e.to_string())?;
    let mut child = Command::new(&helper)
        .args(["--instance-id", "bootstrap-smoke"])
        .arg("--control-socket")
        .arg(&control_sock)
        .args(["--boot-mode", "efi"])
        .arg("--disk")
        .arg(&raw)
        .arg("--efi-variable-store")
        .arg(&efivars)
        .arg("--auxiliary-disk")
        .arg(&cidata)
        .arg("--data-disk")
        .arg(&url)
        .arg("--enable-network")
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

    // Poll until the guest writes to the scratch (marker landed), then give it a
    // moment to fsync + power off.
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut wrote = false;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_secs(2));
        let len = fs::metadata(&scratch_path).map_or(0, |m| m.len());
        let status = helper_status(&control_sock).unwrap_or_default();
        print!(
            "\r  scratch={len} bytes, vm={:<8}",
            if status.is_empty() {
                "?".into()
            } else {
                status
            }
        );
        let _ = std::io::stdout().flush();
        if len > 0 {
            wrote = true;
            thread::sleep(Duration::from_secs(3));
            break;
        }
    }
    println!();

    let final_status = helper_status(&control_sock).unwrap_or_else(|| "unknown".into());

    // Seal the live scratch and read the marker back through a composed disk.
    let sealed_path = work.join("sealed.layer");
    let sealed = server
        .seal_scratch(&sealed_path, &[])
        .map_err(|e| format!("seal_scratch failed: {e}"))?;
    let verify_scratch =
        ScratchLayer::create(&work.join("verify.data"), geom).map_err(|e| e.to_string())?;
    let mut composed = LayeredDisk::new(vec![sealed], verify_scratch).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; MARKER.len()];
    composed
        .read_at(MARKER_OFFSET, &mut buf)
        .map_err(|e| format!("read-back failed: {e}"))?;
    let marker_ok = buf == MARKER;

    let _ = child.kill();
    let _ = child.wait();
    let console_text = fs::read_to_string(&console_log).unwrap_or_default();

    println!("=== FULL console log ({} bytes) ===", console_text.len());
    if console_text.is_empty() {
        println!("  <empty — guest is not directing its console to virtio hvc0>");
    } else {
        print!("{console_text}");
    }
    println!("=== end console log ===");
    println!("\n=== RESULT ===");
    println!("  guest wrote to NBD scratch  : {}", yn(wrote));
    println!("  final VM state              : {final_status}");
    println!(
        "  marker read back from seal  : {}  ({:?})",
        yn(marker_ok),
        String::from_utf8_lossy(&buf)
    );
    println!("  logs preserved at           : {}", work.display());

    server.shutdown().map_err(|e| e.to_string())?;

    if marker_ok {
        println!("\nPASS — nocloud guest wrote through to the NBD scratch and it sealed.");
        Ok(())
    } else {
        Err("marker not found in sealed scratch (see logs)".into())
    }
}

/// Query the petri-vz control socket for the current VM status string.
fn helper_status(sock: &Path) -> Option<String> {
    let mut stream = UnixStream::connect(sock).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream.write_all(b"{\"type\":\"status\"}\n").ok()?;
    let mut resp = Vec::new();
    let mut byte = [0u8; 1];
    while resp.len() < 4096 {
        match stream.read(&mut byte) {
            // EOF or a read error both end the response.
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                resp.push(byte[0]);
            }
        }
    }
    let text = String::from_utf8_lossy(&resp);
    // crude: pull the "status":"X" value
    text.split("\"status\"")
        .nth(1)
        .and_then(|s| s.split('"').nth(1))
        .map(std::string::ToString::to_string)
}

fn yn(b: bool) -> &'static str {
    if b { "YES" } else { "no" }
}
