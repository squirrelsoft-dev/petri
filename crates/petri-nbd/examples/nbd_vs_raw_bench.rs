//! Milestone 5 benchmark: boot time of an NBD-backed layered disk vs. a direct
//! local disk image (the APFS-clone alternative), to quantify the runtime-cost
//! row of design §13.
//!
//! For each mode it measures per-run setup cost and time-to-`login:` (getty up,
//! i.e. userspace fully booted), averaged over N iterations.
//!
//! Usage (from the repo root):
//!   cargo run -p petri-nbd --example nbd_vs_raw_bench -- [BUNDLE_DIR] [ITERS]
//!
//! Requires the built+signed petri-vz helper (see nbd_boot_smoke). The raw mode
//! APFS-clones the base image per run (`cp -c`) so the shared base is never
//! mutated — that clone is itself the alternative's setup cost.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use petri_nbd::{
    BindMode, Geometry, ImmutableLayer, LayeredDisk, NbdServer, ScratchLayer, ServeOpts,
};

const BLOCK_SIZE: u32 = 64 * 1024;
const BOOT_MARKER: &str = "login:";
const BOOT_TIMEOUT: Duration = Duration::from_secs(60);

fn main() {
    if let Err(e) = run() {
        eprintln!("\nBENCH ERROR: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let bundle = PathBuf::from(
        args.next()
            .unwrap_or_else(|| "target/petri-images/base".into()),
    );
    let iters: usize = args.next().map(|s| s.parse().unwrap_or(2)).unwrap_or(2);

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

    println!(
        "base image : {} ({:.2} GiB)",
        disk.display(),
        base_len as f64 / (1u64 << 30) as f64
    );
    println!("iterations : {iters}\nmarker     : {BOOT_MARKER:?}\n");

    let mut raw = Vec::new();
    let mut nbd = Vec::new();
    for i in 0..iters {
        println!("--- iteration {} / {iters} ---", i + 1);
        let r = bench_raw(&helper, &kernel, &initrd, &disk, &cmdline, i)?;
        println!(
            "  raw : setup {:>6.0} ms   boot {:>6.2} s",
            r.setup_ms, r.boot_s
        );
        raw.push(r);

        let n = bench_nbd(&helper, &kernel, &initrd, &disk, &cmdline, base_len, i)?;
        println!(
            "  nbd : setup {:>6.0} ms   boot {:>6.2} s",
            n.setup_ms, n.boot_s
        );
        nbd.push(n);
    }

    let raw_setup = avg(raw.iter().map(|m| m.setup_ms));
    let raw_boot = avg(raw.iter().map(|m| m.boot_s));
    let nbd_setup = avg(nbd.iter().map(|m| m.setup_ms));
    let nbd_boot = avg(nbd.iter().map(|m| m.boot_s));

    println!("\n================ AVERAGES ({iters} runs) ================");
    println!("              setup        boot-to-login");
    println!("  raw (APFS)  {raw_setup:>6.0} ms     {raw_boot:>6.2} s");
    println!("  nbd layered {nbd_setup:>6.0} ms     {nbd_boot:>6.2} s");
    let overhead = nbd_boot - raw_boot;
    let pct = if raw_boot > 0.0 {
        overhead / raw_boot * 100.0
    } else {
        0.0
    };
    println!("\n  NBD boot overhead vs raw: {overhead:+.2} s ({pct:+.1}%)");
    Ok(())
}

struct Measure {
    setup_ms: f64,
    boot_s: f64,
}

fn bench_raw(
    helper: &Path,
    kernel: &Path,
    initrd: &Path,
    disk: &Path,
    cmdline: &str,
    iter: usize,
) -> Result<Measure, String> {
    let work = make_work(&format!("raw-{iter}"))?;
    let copy = work.join("root.img");

    // APFS clone (CoW) — the direct-disk alternative's per-run setup cost.
    let t = Instant::now();
    let status = Command::new("cp")
        .args(["-c"])
        .arg(disk)
        .arg(&copy)
        .status()
        .map_err(|e| format!("cp -c failed: {e}"))?;
    if !status.success() {
        return Err("cp -c (APFS clone) failed".into());
    }
    let setup_ms = t.elapsed().as_secs_f64() * 1000.0;

    let boot_s = boot_to_marker(
        helper,
        kernel,
        initrd,
        cmdline,
        &work,
        &["--disk", copy.to_str().unwrap()],
    )?;
    let _ = fs::remove_dir_all(&work);
    Ok(Measure { setup_ms, boot_s })
}

fn bench_nbd(
    helper: &Path,
    kernel: &Path,
    initrd: &Path,
    disk: &Path,
    cmdline: &str,
    base_len: u64,
    iter: usize,
) -> Result<Measure, String> {
    let work = make_work(&format!("nbd-{iter}"))?;

    let t = Instant::now();
    let geometry = Geometry::new(base_len, BLOCK_SIZE).map_err(|e| e.to_string())?;
    let base = ImmutableLayer::open_raw_base(disk, geometry).map_err(|e| e.to_string())?;
    let scratch =
        ScratchLayer::create(&work.join("scratch.data"), geometry).map_err(|e| e.to_string())?;
    let layered = LayeredDisk::new(vec![base], scratch).map_err(|e| e.to_string())?;
    let server = NbdServer::serve(
        layered,
        ServeOpts {
            bind: BindMode::LoopbackTcp(0),
            export_name: "petri".into(),
            read_only: false,
        },
    )
    .map_err(|e| e.to_string())?;
    let url = server.url().to_string();
    let setup_ms = t.elapsed().as_secs_f64() * 1000.0;

    let boot_s = boot_to_marker(
        helper,
        kernel,
        initrd,
        cmdline,
        &work,
        &["--nbd-disk", &url],
    )?;
    let _ = server.shutdown();
    let _ = fs::remove_dir_all(&work);
    Ok(Measure { setup_ms, boot_s })
}

/// Spawn the helper with a given boot-disk argument pair and time how long until
/// the console log contains the boot marker.
fn boot_to_marker(
    helper: &Path,
    kernel: &Path,
    initrd: &Path,
    cmdline: &str,
    work: &Path,
    disk_args: &[&str],
) -> Result<f64, String> {
    let console = work.join("console.log");
    let control = work.join("control.sock");
    let log = fs::File::create(work.join("helper.log")).map_err(|e| e.to_string())?;

    let start = Instant::now();
    let mut child = Command::new(helper)
        .args(["--instance-id", "bench"])
        .args(["--control-socket", control.to_str().unwrap()])
        .args(["--boot-mode", "linux"])
        .args(["--kernel", kernel.to_str().unwrap()])
        .args(["--initrd", initrd.to_str().unwrap()])
        .args(disk_args)
        .args(["--workspace", work.join("workspace").to_str().unwrap()])
        .args(["--config-dir", work.join("config").to_str().unwrap()])
        .args(["--console-log", console.to_str().unwrap()])
        .args(["--command-line", cmdline])
        .stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|e| format!("spawn helper: {e}"))?;

    let deadline = start + BOOT_TIMEOUT;
    let mut elapsed = None;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
        if let Ok(text) = fs::read(&console)
            && String::from_utf8_lossy(&text).contains(BOOT_MARKER)
        {
            elapsed = Some(start.elapsed().as_secs_f64());
            break;
        }
        if matches!(child.try_wait(), Ok(Some(_))) {
            return Err("helper exited before boot marker appeared".into());
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    elapsed.ok_or_else(|| format!("boot marker {BOOT_MARKER:?} not seen within {BOOT_TIMEOUT:?}"))
}

fn make_work(tag: &str) -> Result<PathBuf, String> {
    let work = PathBuf::from(format!("target/nbd-bench/{}-{tag}", std::process::id()));
    fs::create_dir_all(work.join("workspace")).map_err(|e| e.to_string())?;
    fs::create_dir_all(work.join("config")).map_err(|e| e.to_string())?;
    Ok(work)
}

fn avg(it: impl Iterator<Item = f64>) -> f64 {
    let v: Vec<f64> = it.collect();
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
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
