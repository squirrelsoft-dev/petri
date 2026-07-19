//! In-guest DNS proxy for domain allowlisting (ADR 0002, #36).
//!
//! nftables matches IPs, not names, so the `allowlist` network level enforces
//! **domain** entries through this proxy: `petri-guest` runs it and forces all
//! guest name resolution through it (the nft ruleset drops egress to ports
//! 53/853 from anyone but the root proxy). For an allowed name it forwards the
//! query upstream, adds each returned A/AAAA to the dynamic nftables set with a
//! timeout equal to the record's TTL, and relays the answer — so the workload's
//! subsequent `connect()` to that IP is accepted. A disallowed name gets
//! NXDOMAIN, no IP, and no nft entry.
//!
//! This is good-faith domain filtering, not a hard per-domain guarantee (shared
//! CDN IPs and DoH remain bypasses; see ADR 0002 "Domain Allowlisting"). The
//! proxy and ruleset rest on privilege separation, like the rest of the model.
//!
//! The request-handling logic ([`handle_query`], [`DomainMatcher`]) is pure and
//! host-testable; the socket/thread/nftables/`resolv.conf` IO is Linux-only.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};
#[cfg(target_os = "linux")]
use std::net::{SocketAddr, UdpSocket};
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::thread;
#[cfg(target_os = "linux")]
use std::time::Duration;

use hickory_proto::op::{Message, MessageType, ResponseCode};
use hickory_proto::rr::RData;
use hickory_proto::serialize::binary::{BinDecodable, BinEncodable};

#[cfg(target_os = "linux")]
use crate::netstate::ActiveNetwork;
use crate::policy::NetworkLevel;

/// Address the proxy listens on. Matches the `nameserver 127.0.0.1` written into
/// the guest's `/etc/resolv.conf`; loopback traffic to it is accepted by the nft
/// ruleset (`oifname "lo" accept`).
#[cfg(target_os = "linux")]
pub const BIND_ADDR: &str = "127.0.0.1:53";

/// Default upstream resolver allowed queries are forwarded to. The nft ruleset
/// lets the root proxy reach any resolver on 53/853, so this is overridable.
#[cfg(target_os = "linux")]
pub const DEFAULT_UPSTREAM: &str = "1.1.1.1:53";

/// How long to wait for an upstream answer before returning SERVFAIL.
#[cfg(target_os = "linux")]
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(5);

/// Max DNS-over-UDP payload we read. 4 KiB comfortably covers EDNS responses;
/// truncated answers (TC bit) are relayed as-is and the client may retry on TCP
/// (which the ruleset permits to the proxy over loopback).
#[cfg(target_os = "linux")]
const MAX_UDP: usize = 4096;

/// A compiled set of allowlisted domain patterns. Exact names match themselves;
/// a `*.suffix` pattern matches any strict subdomain of `suffix` (not the apex).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DomainMatcher {
    exact: Vec<String>,
    /// Suffixes from `*.suffix` patterns, stored without the leading `*`, i.e.
    /// `".crates.io"`, so a match is a plain `ends_with`.
    wildcard: Vec<String>,
}

impl DomainMatcher {
    /// Build a matcher from the policy's allowlist, picking out the entries that
    /// are domain names (anything that is not an IP/CIDR literal). Returns `None`
    /// when there are no domain entries, i.e. no proxy is needed.
    pub fn from_allowlist(allowlist: &[String]) -> Option<Self> {
        let mut matcher = Self::default();
        for entry in allowlist {
            if is_ip_literal(entry) {
                continue;
            }
            let name = normalize(entry);
            if let Some(suffix) = name.strip_prefix("*.") {
                matcher.wildcard.push(format!(".{suffix}"));
            } else {
                matcher.exact.push(name);
            }
        }
        if matcher.exact.is_empty() && matcher.wildcard.is_empty() {
            None
        } else {
            Some(matcher)
        }
    }

    /// Whether `name` (a queried domain, with or without a trailing dot) is
    /// allowed.
    pub fn matches(&self, name: &str) -> bool {
        let name = normalize(name);
        if self.exact.contains(&name) {
            return true;
        }
        self.wildcard.iter().any(|suffix| name.ends_with(suffix))
    }
}

/// An entry is an IP/CIDR literal (handled directly by nftables, not the proxy)
/// when its address part parses as an IP. Mirrors `netfilter`'s classification.
fn is_ip_literal(entry: &str) -> bool {
    let addr = entry.split('/').next().unwrap_or(entry);
    addr.parse::<Ipv4Addr>().is_ok() || addr.parse::<Ipv6Addr>().is_ok()
}

/// Lowercase and strip a trailing dot so wire names (`Example.COM.`) and policy
/// entries (`example.com`) compare equal.
fn normalize(name: &str) -> String {
    name.trim_end_matches('.').to_ascii_lowercase()
}

/// Forwards a raw DNS query to an upstream resolver and returns the raw answer.
/// A trait so the request-handling logic is unit-testable with a fake upstream.
pub trait Upstream: Send + Sync {
    fn query(&self, raw: &[u8]) -> io::Result<Vec<u8>>;
}

/// UDP-only upstream forwarder.
#[cfg(target_os = "linux")]
pub struct UdpUpstream {
    server: SocketAddr,
}

#[cfg(target_os = "linux")]
impl UdpUpstream {
    pub fn new(server: SocketAddr) -> Self {
        Self { server }
    }
}

#[cfg(target_os = "linux")]
impl Upstream for UdpUpstream {
    fn query(&self, raw: &[u8]) -> io::Result<Vec<u8>> {
        let socket = UdpSocket::bind(("0.0.0.0", 0))?;
        socket.set_read_timeout(Some(UPSTREAM_TIMEOUT))?;
        socket.send_to(raw, self.server)?;
        let mut buf = vec![0u8; MAX_UDP];
        let (n, _) = socket.recv_from(&mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }
}

/// A resolved IPv4 address paired with its record TTL (seconds).
type Ipv4Ttl = (Ipv4Addr, u32);
/// A resolved IPv6 address paired with its record TTL (seconds).
type Ipv6Ttl = (Ipv6Addr, u32);

/// What to send back to the client plus any IPs to admit through nftables.
#[derive(Debug, PartialEq, Eq)]
pub struct Resolution {
    pub response: Vec<u8>,
    pub add4: Vec<Ipv4Ttl>,
    pub add6: Vec<Ipv6Ttl>,
}

/// Handle one query for the given active level. Pure given an `Upstream`, so the
/// allow/deny/passthrough decisions are unit-testable without sockets or nft.
///
/// - `Full`: forward everything (no filtering), admit nothing to nft — there is
///   no ruleset to populate at `full`.
/// - `Allowlist`: forward only allowed names and admit their resolved IPs;
///   NXDOMAIN the rest.
/// - `None`: deny everything (there is no egress anyway).
pub fn handle_query(
    level: NetworkLevel,
    raw: &[u8],
    matcher: &DomainMatcher,
    upstream: &dyn Upstream,
) -> Resolution {
    let request = match Message::from_bytes(raw) {
        Ok(message) => message,
        // Unparseable input: nothing safe to forward. Reply with FormErr if we
        // can recover an id, else drop by returning an empty response.
        Err(_) => {
            return Resolution {
                response: refusal(raw, ResponseCode::FormErr),
                add4: Vec::new(),
                add6: Vec::new(),
            };
        }
    };

    let allowed = match level {
        NetworkLevel::Full => true,
        NetworkLevel::None => false,
        NetworkLevel::Allowlist => request
            .queries
            .first()
            .is_some_and(|q| matcher.matches(&q.name().to_string())),
    };

    if !allowed {
        return Resolution {
            response: deny(&request),
            add4: Vec::new(),
            add6: Vec::new(),
        };
    }

    match upstream.query(raw) {
        Ok(response) => {
            // Only populate nftables when a ruleset exists to hold the entries
            // (the `allowlist` level). At `full` there is no table.
            let (add4, add6) = if level == NetworkLevel::Allowlist {
                extract_addrs(&response)
            } else {
                (Vec::new(), Vec::new())
            };
            Resolution {
                response,
                add4,
                add6,
            }
        }
        Err(_) => Resolution {
            response: error_response(&request, ResponseCode::ServFail),
            add4: Vec::new(),
            add6: Vec::new(),
        },
    }
}

/// Build an NXDOMAIN answer to a parsed, disallowed request (echoes the question).
fn deny(request: &Message) -> Vec<u8> {
    error_response(request, ResponseCode::NXDomain)
}

/// Turn a parsed request into an error response with the given code, preserving
/// the id and question so the client matches it to its query.
fn error_response(request: &Message, code: ResponseCode) -> Vec<u8> {
    let mut response = request.clone();
    response.answers.clear();
    response.authorities.clear();
    response.additionals.clear();
    response.metadata.message_type = MessageType::Response;
    response.metadata.response_code = code;
    response.metadata.recursion_available = true;
    response
        .to_bytes()
        .unwrap_or_else(|_| refusal(&[], ResponseCode::ServFail))
}

/// Build a minimal error response when we could not even parse the request,
/// reusing the request's first two bytes (the id) when present.
fn refusal(raw: &[u8], code: ResponseCode) -> Vec<u8> {
    let id = match raw {
        [hi, lo, ..] => u16::from_be_bytes([*hi, *lo]),
        _ => 0,
    };
    Message::error_msg(id, hickory_proto::op::OpCode::Query, code)
        .to_bytes()
        .unwrap_or_default()
}

/// Pull A/AAAA records (address + TTL) out of a raw upstream response.
fn extract_addrs(response: &[u8]) -> (Vec<Ipv4Ttl>, Vec<Ipv6Ttl>) {
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    let Ok(message) = Message::from_bytes(response) else {
        return (v4, v6);
    };
    for record in &message.answers {
        let ttl = record.ttl;
        match &record.data {
            RData::A(a) => v4.push((a.0, ttl)),
            RData::AAAA(aaaa) => v6.push((aaaa.0, ttl)),
            _ => {}
        }
    }
    (v4, v6)
}

/// Point the guest resolver at the proxy. The nft ruleset is the real
/// enforcement; this just makes ordinary resolution flow through the proxy.
#[cfg(target_os = "linux")]
pub fn force_local_resolver() -> io::Result<()> {
    std::fs::write("/etc/resolv.conf", b"nameserver 127.0.0.1\n")
}

/// Bind the proxy's UDP socket and serve queries on a background thread. Binding
/// happens synchronously so a failure (e.g. port 53 in use) surfaces to the
/// caller at boot rather than vanishing into the thread.
#[cfg(target_os = "linux")]
pub fn start(
    active: Arc<ActiveNetwork>,
    matcher: DomainMatcher,
    upstream: Box<dyn Upstream>,
) -> io::Result<()> {
    let socket = UdpSocket::bind(BIND_ADDR)?;
    thread::Builder::new()
        .name("petri-dns-proxy".to_string())
        .spawn(move || serve_loop(socket, active, matcher, upstream))?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn serve_loop(
    socket: UdpSocket,
    active: Arc<ActiveNetwork>,
    matcher: DomainMatcher,
    upstream: Box<dyn Upstream>,
) {
    let mut buf = vec![0u8; MAX_UDP];
    loop {
        let (n, peer) = match socket.recv_from(&mut buf) {
            Ok(pair) => pair,
            Err(err) => {
                eprintln!("petri-guest: dns proxy recv error: {err}");
                continue;
            }
        };
        let resolution = handle_query(active.get(), &buf[..n], &matcher, upstream.as_ref());
        if let Err(err) = socket.send_to(&resolution.response, peer) {
            eprintln!("petri-guest: dns proxy send error: {err}");
        }
        if !resolution.add4.is_empty() || !resolution.add6.is_empty() {
            if let Err(err) = crate::netfilter::add_resolved(&resolution.add4, &resolution.add6) {
                // Non-fatal: the active level may have changed away from
                // allowlist between the lookup and the nft update.
                eprintln!("petri-guest: dns proxy could not admit resolved IPs: {err}");
            }
        }
    }
}

/// Convenience: the configured (or default) upstream as a boxed UDP forwarder.
#[cfg(target_os = "linux")]
pub fn default_upstream() -> Box<dyn Upstream> {
    let server: SocketAddr = DEFAULT_UPSTREAM.parse().expect("valid default upstream");
    Box::new(UdpUpstream::new(server))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::op::{Message, MessageType, OpCode, Query};
    use hickory_proto::rr::rdata::{A, AAAA};
    use hickory_proto::rr::{Name, RData, Record, RecordType};
    use std::str::FromStr;

    fn query_bytes(name: &str, rtype: RecordType) -> Vec<u8> {
        let mut message = Message::new(0x1234, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(Name::from_str(name).unwrap(), rtype));
        message.to_bytes().unwrap()
    }

    /// Upstream that answers any query with one A and one AAAA record.
    struct StubUpstream {
        a: Ipv4Addr,
        aaaa: Ipv6Addr,
        ttl: u32,
    }

    impl Upstream for StubUpstream {
        fn query(&self, raw: &[u8]) -> io::Result<Vec<u8>> {
            let request = Message::from_bytes(raw).unwrap();
            let q = request.queries.first().unwrap().clone();
            let name = q.name().clone();
            let mut response = request.clone();
            response.metadata.message_type = MessageType::Response;
            response.answers.push(Record::from_rdata(
                name.clone(),
                self.ttl,
                RData::A(A(self.a)),
            ));
            response.answers.push(Record::from_rdata(
                name,
                self.ttl,
                RData::AAAA(AAAA(self.aaaa)),
            ));
            // from_rdata defaults dns_class to IN.
            Ok(response.to_bytes().unwrap())
        }
    }

    /// Upstream that always fails — stands in for a dropped/timed-out forward.
    struct DeadUpstream;
    impl Upstream for DeadUpstream {
        fn query(&self, _raw: &[u8]) -> io::Result<Vec<u8>> {
            Err(io::Error::new(io::ErrorKind::TimedOut, "no upstream"))
        }
    }

    fn stub() -> StubUpstream {
        StubUpstream {
            a: Ipv4Addr::new(93, 184, 215, 14),
            aaaa: Ipv6Addr::new(0x2606, 0x2800, 0x21f, 0xcb07, 0, 0, 0, 1),
            ttl: 300,
        }
    }

    fn decode_rcode(bytes: &[u8]) -> ResponseCode {
        Message::from_bytes(bytes).unwrap().metadata.response_code
    }

    #[test]
    fn matcher_picks_domains_and_ignores_ip_literals() {
        let allow = vec![
            "1.1.1.1".to_string(),
            "8.8.8.0/24".to_string(),
            "2606:4700::1".to_string(),
            "api.github.com".to_string(),
            "*.crates.io".to_string(),
        ];
        let matcher = DomainMatcher::from_allowlist(&allow).unwrap();
        assert!(matcher.matches("api.github.com"));
        assert!(matcher.matches("API.GitHub.com.")); // case + trailing dot
        assert!(matcher.matches("static.crates.io"));
        assert!(matcher.matches("a.b.crates.io"));
        assert!(!matcher.matches("crates.io")); // apex not matched by *.crates.io
        assert!(!matcher.matches("github.com"));
        assert!(!matcher.matches("evil.com"));
    }

    #[test]
    fn matcher_is_none_without_domains() {
        let allow = vec!["1.1.1.1".to_string(), "10.0.0.0/8".to_string()];
        assert!(DomainMatcher::from_allowlist(&allow).is_none());
    }

    #[test]
    fn allowlist_forwards_allowed_name_and_extracts_addrs() {
        let matcher = DomainMatcher::from_allowlist(&["example.com".to_string()]).unwrap();
        let raw = query_bytes("example.com.", RecordType::A);
        let res = handle_query(NetworkLevel::Allowlist, &raw, &matcher, &stub());

        assert_eq!(decode_rcode(&res.response), ResponseCode::NoError);
        assert_eq!(res.add4, vec![(Ipv4Addr::new(93, 184, 215, 14), 300)]);
        assert_eq!(
            res.add6,
            vec![(
                Ipv6Addr::new(0x2606, 0x2800, 0x21f, 0xcb07, 0, 0, 0, 1),
                300
            )]
        );
    }

    #[test]
    fn allowlist_nxdomains_disallowed_name_and_admits_nothing() {
        let matcher = DomainMatcher::from_allowlist(&["example.com".to_string()]).unwrap();
        let raw = query_bytes("evil.com.", RecordType::A);
        let res = handle_query(NetworkLevel::Allowlist, &raw, &matcher, &stub());

        assert_eq!(decode_rcode(&res.response), ResponseCode::NXDomain);
        assert!(res.add4.is_empty() && res.add6.is_empty());
    }

    #[test]
    fn full_forwards_everything_but_admits_nothing() {
        // At full there is no nft table to populate, even for a name that is not
        // on the allowlist.
        let matcher = DomainMatcher::from_allowlist(&["example.com".to_string()]).unwrap();
        let raw = query_bytes("anything.example.org.", RecordType::A);
        let res = handle_query(NetworkLevel::Full, &raw, &matcher, &stub());

        assert_eq!(decode_rcode(&res.response), ResponseCode::NoError);
        assert!(res.add4.is_empty() && res.add6.is_empty());
    }

    #[test]
    fn none_denies_everything() {
        let matcher = DomainMatcher::from_allowlist(&["example.com".to_string()]).unwrap();
        let raw = query_bytes("example.com.", RecordType::A);
        let res = handle_query(NetworkLevel::None, &raw, &matcher, &stub());

        assert_eq!(decode_rcode(&res.response), ResponseCode::NXDomain);
        assert!(res.add4.is_empty() && res.add6.is_empty());
    }

    #[test]
    fn upstream_failure_yields_servfail() {
        let matcher = DomainMatcher::from_allowlist(&["example.com".to_string()]).unwrap();
        let raw = query_bytes("example.com.", RecordType::A);
        let res = handle_query(NetworkLevel::Allowlist, &raw, &matcher, &DeadUpstream);

        assert_eq!(decode_rcode(&res.response), ResponseCode::ServFail);
        assert!(res.add4.is_empty() && res.add6.is_empty());
    }
}
