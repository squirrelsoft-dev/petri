//! In-guest network egress enforcement (ADR 0002, #36).
//!
//! `petri-guest` runs as root and applies an nftables ruleset matching the
//! policy's active `network` level. Workload processes run unprivileged (no
//! `CAP_NET_ADMIN`), so they cannot alter the ruleset. This is the in-guest
//! replacement for the host-side egress filter the spike ruled out on
//! throughput grounds.
//!
//! Levels:
//! - `full`  — no ruleset; egress is unrestricted (a no-op at boot).
//! - `none`  — drop all egress except loopback and established flows.
//! - `allowlist` — additionally accept traffic to the policy's listed IPs/CIDRs.
//!
//! IP and CIDR allowlist entries are enforced here. Domain entries require name
//! resolution and are enforced by the DNS-proxy layer (a follow-up); they are
//! reported as skipped rather than silently dropped.

use std::net::{Ipv4Addr, Ipv6Addr};

use crate::policy::NetworkLevel;

/// The nftables table name owned by petri. Applying a plan replaces it wholesale.
const TABLE: &str = "petri";

/// A computed nftables action for a network level: the script to feed `nft -f`
/// (absent for `full`, which imposes no restrictions at boot) plus any allowlist
/// entries that could not be enforced here (domains).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesetPlan {
    pub script: Option<String>,
    pub skipped_domains: Vec<String>,
}

/// Build the nftables plan for a network level and allowlist. Pure — does no IO,
/// so it is unit-testable without a guest kernel.
pub fn plan(level: NetworkLevel, allowlist: &[String]) -> RulesetPlan {
    match level {
        // No restrictions: ensure no petri table remains. This is what makes a
        // runtime escalation to `full` (via set_mode) actually lift a prior
        // restriction. At boot there is no table yet, so `apply_boot` skips this
        // case rather than issuing a pointless teardown.
        NetworkLevel::Full => RulesetPlan {
            script: Some(render_teardown()),
            skipped_domains: Vec::new(),
        },
        NetworkLevel::None => RulesetPlan {
            script: Some(render_ruleset(&[], &[])),
            skipped_domains: Vec::new(),
        },
        NetworkLevel::Allowlist => {
            let mut v4 = Vec::new();
            let mut v6 = Vec::new();
            let mut skipped_domains = Vec::new();
            for entry in allowlist {
                match classify(entry) {
                    Dest::V4 => v4.push(entry.clone()),
                    Dest::V6 => v6.push(entry.clone()),
                    Dest::Domain => skipped_domains.push(entry.clone()),
                }
            }
            RulesetPlan {
                script: Some(render_ruleset(&v4, &v6)),
                skipped_domains,
            }
        }
    }
}

enum Dest {
    V4,
    V6,
    Domain,
}

/// Classify an allowlist entry by its address part (the portion before any
/// `/prefix`). Anything that is not an IP literal is treated as a domain.
fn classify(entry: &str) -> Dest {
    let addr = entry.split('/').next().unwrap_or(entry);
    if addr.parse::<Ipv4Addr>().is_ok() {
        Dest::V4
    } else if addr.parse::<Ipv6Addr>().is_ok() {
        Dest::V6
    } else {
        Dest::Domain
    }
}

/// Render an `nft -f` script that removes the petri table, lifting all egress
/// restrictions. `add` then `delete` is idempotent whether or not the table
/// already exists (a bare `delete` errors when it is absent).
fn render_teardown() -> String {
    format!("add table inet {TABLE}\ndelete table inet {TABLE}\n")
}

/// Render an idempotent `nft -f` script: ensure-then-delete clears any prior
/// petri table, then a fresh definition with a default-drop `output` chain that
/// accepts loopback, established/related flows, and the allowlisted sets.
fn render_ruleset(v4: &[String], v6: &[String]) -> String {
    let mut out = String::new();
    // `add` then `delete` makes the redefinition idempotent whether or not the
    // table already exists (delete alone errors when absent).
    out.push_str(&format!("add table inet {TABLE}\n"));
    out.push_str(&format!("delete table inet {TABLE}\n"));
    out.push_str(&format!("table inet {TABLE} {{\n"));

    if !v4.is_empty() {
        out.push_str("\tset allow4 {\n\t\ttype ipv4_addr\n\t\tflags interval\n");
        out.push_str(&format!("\t\telements = {{ {} }}\n", v4.join(", ")));
        out.push_str("\t}\n");
    }
    if !v6.is_empty() {
        out.push_str("\tset allow6 {\n\t\ttype ipv6_addr\n\t\tflags interval\n");
        out.push_str(&format!("\t\telements = {{ {} }}\n", v6.join(", ")));
        out.push_str("\t}\n");
    }

    out.push_str("\tchain output {\n");
    out.push_str("\t\ttype filter hook output priority 0; policy drop;\n");
    out.push_str("\t\toifname \"lo\" accept\n");
    out.push_str("\t\tct state established,related accept\n");
    if !v4.is_empty() {
        out.push_str("\t\tip daddr @allow4 accept\n");
    }
    if !v6.is_empty() {
        out.push_str("\t\tip6 daddr @allow6 accept\n");
    }
    out.push_str("\t}\n}\n");
    out
}

/// Errors applying the ruleset to the running guest.
#[derive(Debug)]
pub enum NetfilterError {
    Spawn(std::io::Error),
    Nft { status: Option<i32>, stderr: String },
}

impl std::fmt::Display for NetfilterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(err) => write!(f, "failed to run nft: {err}"),
            Self::Nft { status, stderr } => {
                write!(f, "nft exited with {status:?}: {}", stderr.trim())
            }
        }
    }
}

impl std::error::Error for NetfilterError {}

/// Apply the policy's boot (`default`) network level to the running guest.
///
/// A no-op when `network_enabled = false` (no device is attached) or when the
/// active level is `full` (no restrictions). Restrictive levels feed an nft
/// script to `nft -f -`. Callers should treat a returned error as fatal at boot:
/// failing to install a required egress restriction must not fall back to open
/// egress.
#[cfg(target_os = "linux")]
pub fn apply_boot(policy: &crate::policy::Policy) -> Result<(), NetfilterError> {
    if !policy.network_enabled {
        return Ok(());
    }
    // Only meaningful when the guest is root; mirrors the privilege-drop guard.
    // SAFETY: geteuid is async-signal-safe and has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }
    // Boot starts from a clean slate (no petri table), so the `full` level has
    // nothing to tear down — skip it rather than shell out to nft for a no-op.
    // Restrictive levels (`none`/`allowlist`) install their ruleset.
    if policy.network.default == NetworkLevel::Full {
        return Ok(());
    }
    apply(&policy.network, policy.network.default)
}

/// Apply a specific level for the given network policy. Shared by boot and (in a
/// follow-up) `set_mode`.
#[cfg(target_os = "linux")]
pub fn apply(
    network: &crate::policy::NetworkPolicy,
    level: NetworkLevel,
) -> Result<(), NetfilterError> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let plan = plan(level, &network.allowlist);
    if !plan.skipped_domains.is_empty() {
        eprintln!(
            "petri-guest: network allowlist domains not yet enforced (pending DNS proxy): {}",
            plan.skipped_domains.join(", ")
        );
    }
    let Some(script) = plan.script else {
        return Ok(());
    };

    let mut child = Command::new("nft")
        .arg("-f")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(NetfilterError::Spawn)?;

    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(script.as_bytes())
        .map_err(NetfilterError::Spawn)?;

    let output = child.wait_with_output().map_err(NetfilterError::Spawn)?;
    if !output.status.success() {
        return Err(NetfilterError::Nft {
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_tears_down_the_table() {
        // `full` lifts restrictions by removing the petri table: add-then-delete,
        // and crucially no drop policy or allow sets left behind.
        let plan = plan(NetworkLevel::Full, &["1.1.1.1".to_string()]);
        let script = plan.script.unwrap();
        assert!(script.contains("add table inet petri"));
        assert!(script.contains("delete table inet petri"));
        assert!(!script.contains("policy drop;"));
        assert!(!script.contains("chain output"));
        assert!(plan.skipped_domains.is_empty());
    }

    #[test]
    fn none_drops_all_but_loopback_and_established() {
        let script = plan(NetworkLevel::None, &[]).script.unwrap();
        assert!(script.contains("policy drop;"));
        assert!(script.contains("oifname \"lo\" accept"));
        assert!(script.contains("ct state established,related accept"));
        // No allow sets at the none level.
        assert!(!script.contains("@allow4"));
        assert!(!script.contains("@allow6"));
    }

    #[test]
    fn allowlist_splits_v4_v6_and_defers_domains() {
        let entries = vec![
            "1.1.1.1".to_string(),
            "8.8.8.0/24".to_string(),
            "2606:4700:4700::1111".to_string(),
            "*.crates.io".to_string(),
            "example.com".to_string(),
        ];
        let plan = plan(NetworkLevel::Allowlist, &entries);
        let script = plan.script.unwrap();

        assert!(script.contains("set allow4"));
        assert!(script.contains("1.1.1.1"));
        assert!(script.contains("8.8.8.0/24"));
        assert!(script.contains("ip daddr @allow4 accept"));

        assert!(script.contains("set allow6"));
        assert!(script.contains("2606:4700:4700::1111"));
        assert!(script.contains("ip6 daddr @allow6 accept"));

        assert!(script.contains("policy drop;"));

        assert_eq!(
            plan.skipped_domains,
            vec!["*.crates.io".to_string(), "example.com".to_string()]
        );
    }

    #[test]
    fn allowlist_without_v6_omits_v6_set() {
        let script = plan(NetworkLevel::Allowlist, &["1.1.1.1".to_string()])
            .script
            .unwrap();
        assert!(script.contains("set allow4"));
        assert!(!script.contains("set allow6"));
        assert!(!script.contains("ip6 daddr"));
    }

    #[test]
    fn ruleset_is_idempotent_add_then_delete() {
        let script = plan(NetworkLevel::None, &[]).script.unwrap();
        let add = script.find("add table inet petri").unwrap();
        let delete = script.find("delete table inet petri").unwrap();
        assert!(add < delete, "add must precede delete for idempotency");
    }
}
