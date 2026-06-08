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
            script: Some(render_ruleset(&[], &[], false)),
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
            let has_domains = !skipped_domains.is_empty();
            RulesetPlan {
                script: Some(render_ruleset(&v4, &v6, has_domains)),
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

/// Names of the dynamic, per-element-timeout sets the DNS proxy populates with
/// IPs it resolves for allowlisted domains. Kept distinct from the static
/// `allow4`/`allow6` literal sets so domain answers self-expire on their TTL
/// without disturbing the configured IP/CIDR entries.
pub const RESOLVED4: &str = "resolved4";
pub const RESOLVED6: &str = "resolved6";

/// Render an idempotent `nft -f` script: ensure-then-delete clears any prior
/// petri table, then a fresh definition with a default-drop `output` chain that
/// accepts loopback, established/related flows, and the allowlisted sets.
///
/// When `has_domains` is set, a DNS proxy is running: the chain additionally
/// forces all name resolution through it (drop egress to ports 53/853 except
/// from the proxy itself, which runs as root) and accepts the dynamic
/// proxy-populated sets. Domain IPs land in `resolved4`/`resolved6` with a
/// per-element timeout (= record TTL); the `ct state established,related accept`
/// above the set lookup means an entry expiring only blocks *future*
/// connections, never an in-flight transfer.
fn render_ruleset(v4: &[String], v6: &[String], has_domains: bool) -> String {
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
    if has_domains {
        out.push_str(&format!(
            "\tset {RESOLVED4} {{\n\t\ttype ipv4_addr\n\t\tflags timeout\n\t}}\n"
        ));
        out.push_str(&format!(
            "\tset {RESOLVED6} {{\n\t\ttype ipv6_addr\n\t\tflags timeout\n\t}}\n"
        ));
    }

    out.push_str("\tchain output {\n");
    out.push_str("\t\ttype filter hook output priority 0; policy drop;\n");
    out.push_str("\t\toifname \"lo\" accept\n");
    out.push_str("\t\tct state established,related accept\n");
    if has_domains {
        // Force DNS through the local proxy: the root proxy may reach upstream
        // resolvers; everything else is denied so resolution cannot route
        // around it (loopback to the proxy is already accepted above).
        out.push_str("\t\tmeta skuid 0 udp dport { 53, 853 } accept\n");
        out.push_str("\t\tmeta skuid 0 tcp dport { 53, 853 } accept\n");
        out.push_str("\t\tudp dport { 53, 853 } drop\n");
        out.push_str("\t\ttcp dport { 53, 853 } drop\n");
    }
    if !v4.is_empty() {
        out.push_str("\t\tip daddr @allow4 accept\n");
    }
    if !v6.is_empty() {
        out.push_str("\t\tip6 daddr @allow6 accept\n");
    }
    if has_domains {
        out.push_str(&format!("\t\tip daddr @{RESOLVED4} accept\n"));
        out.push_str(&format!("\t\tip6 daddr @{RESOLVED6} accept\n"));
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

/// Apply a specific level for the given network policy. Shared by boot and
/// `set_mode`.
#[cfg(target_os = "linux")]
pub fn apply(
    network: &crate::policy::NetworkPolicy,
    level: NetworkLevel,
) -> Result<(), NetfilterError> {
    let plan = plan(level, &network.allowlist);
    if !plan.skipped_domains.is_empty() {
        eprintln!(
            "petri-guest: network allowlist domains enforced via DNS proxy: {}",
            plan.skipped_domains.join(", ")
        );
    }
    let Some(script) = plan.script else {
        return Ok(());
    };
    run_nft(&script)
}

/// Add proxy-resolved domain IPs to the dynamic `resolved4`/`resolved6` sets,
/// each with a per-element timeout equal to its DNS record TTL (clamped to a
/// floor so a near-zero TTL cannot blackhole the connection the answer is for).
/// A no-op when there is nothing to add.
///
/// Returns `Err` if the table/sets are absent — e.g. the active level is not
/// `allowlist`, so the answer is irrelevant; callers (the proxy) log and
/// continue rather than treating it as fatal.
#[cfg(target_os = "linux")]
pub fn add_resolved(
    v4: &[(Ipv4Addr, u32)],
    v6: &[(Ipv6Addr, u32)],
) -> Result<(), NetfilterError> {
    let Some(script) = render_add_elements(v4, v6) else {
        return Ok(());
    };
    run_nft(&script)
}

/// Lowest timeout (seconds) we assign a resolved element, so a tiny record TTL
/// cannot expire the entry before the workload's `connect()` lands.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const MIN_TIMEOUT_SECS: u32 = 10;

/// Render `add element` statements for the resolved sets, or `None` when both
/// lists are empty. Pure (and so host-testable); the live `add_resolved` caller
/// is Linux-only, hence the host build sees no non-test use.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn render_add_elements(v4: &[(Ipv4Addr, u32)], v6: &[(Ipv6Addr, u32)]) -> Option<String> {
    if v4.is_empty() && v6.is_empty() {
        return None;
    }
    let mut out = String::new();
    if !v4.is_empty() {
        out.push_str(&format!(
            "add element inet {TABLE} {RESOLVED4} {{ {} }}\n",
            render_timed_elements(v4)
        ));
    }
    if !v6.is_empty() {
        out.push_str(&format!(
            "add element inet {TABLE} {RESOLVED6} {{ {} }}\n",
            render_timed_elements(v6)
        ));
    }
    Some(out)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn render_timed_elements<A: std::fmt::Display>(elems: &[(A, u32)]) -> String {
    elems
        .iter()
        .map(|(ip, ttl)| format!("{ip} timeout {}s", (*ttl).max(MIN_TIMEOUT_SECS)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Feed an `nft -f` script to `nft` on stdin and map a non-zero exit to an error.
#[cfg(target_os = "linux")]
fn run_nft(script: &str) -> Result<(), NetfilterError> {
    use std::io::Write;
    use std::process::{Command, Stdio};

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

    #[test]
    fn allowlist_with_domains_adds_proxy_sets_and_dns_gating() {
        let script = plan(NetworkLevel::Allowlist, &["example.com".to_string()])
            .script
            .unwrap();
        // Dynamic timeout sets the proxy populates.
        assert!(script.contains("set resolved4"));
        assert!(script.contains("set resolved6"));
        assert!(script.contains("flags timeout"));
        assert!(script.contains("ip daddr @resolved4 accept"));
        assert!(script.contains("ip6 daddr @resolved6 accept"));
        // DNS is forced through the proxy: root may reach upstream, others denied.
        assert!(script.contains("meta skuid 0 udp dport { 53, 853 } accept"));
        assert!(script.contains("udp dport { 53, 853 } drop"));
        assert!(script.contains("tcp dport { 53, 853 } drop"));
    }

    #[test]
    fn allowlist_without_domains_has_no_proxy_sets_or_dns_gating() {
        let script = plan(NetworkLevel::Allowlist, &["1.1.1.1".to_string()])
            .script
            .unwrap();
        assert!(!script.contains("resolved4"));
        assert!(!script.contains("dport { 53, 853 }"));
    }

    #[test]
    fn add_elements_applies_per_element_ttl_with_floor() {
        let v4 = vec![
            (Ipv4Addr::new(93, 184, 215, 14), 300),
            (Ipv4Addr::new(1, 2, 3, 4), 1), // below the floor
        ];
        let v6 = vec![(Ipv6Addr::LOCALHOST, 120)];
        let script = render_add_elements(&v4, &v6).unwrap();

        assert!(script.contains("add element inet petri resolved4"));
        assert!(script.contains("93.184.215.14 timeout 300s"));
        // A 1s TTL is clamped up to the floor so the entry outlives the connect().
        assert!(script.contains(&format!("1.2.3.4 timeout {MIN_TIMEOUT_SECS}s")));
        assert!(script.contains("add element inet petri resolved6"));
        assert!(script.contains("timeout 120s"));
    }

    #[test]
    fn add_elements_is_none_when_empty() {
        assert!(render_add_elements(&[], &[]).is_none());
    }
}
