//! Shared preflight for the `petri-vz` helper the NBD smoke examples drive.
//!
//! Lives in a subdirectory so Cargo does not treat it as an example target of
//! its own — example discovery picks up `examples/*.rs` and `examples/*/main.rs`,
//! and this is neither. Each example pulls it in with
//! `#[path = "common/preflight.rs"] mod preflight;`.

// Each example compiles this module separately and uses only part of it.
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;

/// The helper binary, relative to the repo root.
pub const HELPER: &str = "crates/petri-vz/.build/debug/petri-vz";

/// Entitlements the helper needs: one to create a VM at all, one for the
/// in-guest NBD client to open a connection back to the petri-nbd server.
const REQUIRED_ENTITLEMENTS: [&str; 2] = [
    "com.apple.security.virtualization",
    "com.apple.security.network.client",
];

/// Resolve the helper, verifying it both exists *and* is still entitled.
///
/// The entitlement half matters because `swift build` re-links the binary and
/// ad-hoc signs it without entitlements — signing is a separate step in
/// `scripts/build-image-builder.sh`. So any plain rebuild silently disarms the
/// helper: the file is present, the right size, and executable, but the VM
/// never starts. Checking the signature converts that into one actionable line
/// instead of a boot that hangs until the timeout.
pub fn helper() -> Result<PathBuf, String> {
    let helper = PathBuf::from(HELPER);
    if !helper.exists() {
        return Err(format!(
            "petri-vz helper not found at {}\n{}",
            helper.display(),
            build_and_sign_hint()
        ));
    }

    let output = Command::new("codesign")
        .args(["-d", "--entitlements", "-", HELPER])
        .output()
        .map_err(|err| format!("could not run codesign to inspect {HELPER}: {err}"))?;

    // codesign prints the entitlement plist to stdout; a missing or unsigned
    // binary yields no keys rather than an error we can rely on.
    let entitlements = String::from_utf8_lossy(&output.stdout);
    let missing: Vec<&str> = REQUIRED_ENTITLEMENTS
        .iter()
        .copied()
        .filter(|key| !entitlements.contains(key))
        .collect();

    if !missing.is_empty() {
        return Err(format!(
            "petri-vz at {} is missing entitlement(s): {}\n\
             A plain `swift build` re-signs the helper without them, so rebuilding\n\
             disarms it — the binary looks fine but cannot start a VM. Re-sign:\n{}",
            helper.display(),
            missing.join(", "),
            build_and_sign_hint()
        ));
    }

    Ok(helper)
}

fn build_and_sign_hint() -> String {
    format!(
        "  swift build --package-path crates/petri-vz\n  \
         codesign --force --sign - --entitlements crates/petri-vz/petri-vz.entitlements {HELPER}"
    )
}
