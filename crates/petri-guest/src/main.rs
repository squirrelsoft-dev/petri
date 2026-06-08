use std::env;
use std::fs::File;
use std::io::{self, BufReader, BufWriter};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use petri_guest::lsp::{LspConfig, LspManager};
use petri_guest::netstate::ActiveNetwork;
use petri_guest::policy::Policy;
use petri_guest::server;
use petri_guest::transport::VsockListenerConfig;

#[derive(Debug)]
struct Args {
    policy_path: PathBuf,
    lsp_config_path: Option<PathBuf>,
    transport: Transport,
}

#[derive(Debug)]
enum Transport {
    Stdio,
    Tcp(String),
    Vsock { port: u32 },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("petri-guest: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse(env::args().skip(1))?;
    let policy = Policy::load(File::open(&args.policy_path)?)?;

    // The active network level is VM-global state shared between the dispatch
    // server (which moves it via `set_mode`) and the DNS proxy (which reads it).
    let active_network = Arc::new(ActiveNetwork::new(policy.network.default));

    // Install the in-guest egress ruleset before accepting any dispatch, so no
    // workload runs before network policy is in force. Fatal on failure: failing
    // to apply a required restriction must not fall back to open egress (ADR 0002).
    #[cfg(target_os = "linux")]
    {
        petri_guest::netfilter::apply_boot(&policy)?;
        start_dns_proxy(&policy, active_network.clone())?;
    }
    let lsp_config = match &args.lsp_config_path {
        Some(path) => LspConfig::load(File::open(path)?)?,
        None => LspConfig::disabled(),
    };
    // Language servers are reused across connections for the VM session and shut
    // down cleanly when the guest exits (LspManager::drop).
    let lsp = LspManager::new(lsp_config, policy.workspace_path.clone());

    match args.transport {
        Transport::Stdio => {
            let stdin = io::stdin();
            let stdout = io::stdout();
            server::serve_lines(stdin.lock(), stdout.lock(), &policy, &lsp, &active_network)?;
        }
        Transport::Tcp(addr) => {
            let listener = TcpListener::bind(&addr)?;
            eprintln!("petri-guest: listening on tcp {addr}");
            for stream in listener.incoming() {
                let stream = stream?;
                let reader = BufReader::new(stream.try_clone()?);
                let writer = BufWriter::new(stream);
                server::serve_lines(reader, writer, &policy, &lsp, &active_network)?;
            }
        }
        Transport::Vsock { port } => {
            let listener = VsockListenerConfig { port };
            eprintln!("petri-guest: listening on vsock port {port}");
            #[cfg(target_os = "linux")]
            {
                let listener = listener.bind()?;
                for stream in listener.incoming() {
                    let stream = stream?;
                    let reader = BufReader::new(stream.try_clone()?);
                    let writer = BufWriter::new(stream);
                    server::serve_lines(reader, writer, &policy, &lsp, &active_network)?;
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                listener.bind()?;
            }
        }
    }

    Ok(())
}

/// Start the in-guest DNS proxy when the policy's network allowlist contains
/// domain names (IP/CIDR entries are enforced by nftables alone and need no
/// proxy). Points the guest resolver at it and serves on a background thread.
/// A no-op when networking is disabled or the guest is not root — the same
/// conditions under which `netfilter::apply_boot` skips: without root there is
/// no enforcement to install, and binding port 53 would fail anyway.
#[cfg(target_os = "linux")]
fn start_dns_proxy(
    policy: &Policy,
    active: Arc<ActiveNetwork>,
) -> Result<(), Box<dyn std::error::Error>> {
    use petri_guest::dnsproxy;

    if !policy.network_enabled {
        return Ok(());
    }
    // SAFETY: geteuid is async-signal-safe and has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }
    let Some(matcher) = dnsproxy::DomainMatcher::from_allowlist(&policy.network.allowlist) else {
        return Ok(());
    };
    dnsproxy::force_local_resolver()?;
    dnsproxy::start(active, matcher, dnsproxy::default_upstream())?;
    eprintln!("petri-guest: DNS proxy active for domain allowlisting");
    Ok(())
}

impl Args {
    fn parse<I>(mut args: I) -> Result<Self, String>
    where
        I: Iterator<Item = String>,
    {
        let mut policy_path = None;
        let mut lsp_config_path = None;
        let mut transport = Transport::Stdio;

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--policy" => {
                    policy_path = Some(PathBuf::from(next_arg(&mut args, "--policy")?));
                }
                "--lsp-config" => {
                    lsp_config_path = Some(PathBuf::from(next_arg(&mut args, "--lsp-config")?));
                }
                "--transport" => {
                    let value = next_arg(&mut args, "--transport")?;
                    transport = match value.as_str() {
                        "stdio" => Transport::Stdio,
                        "tcp" => Transport::Tcp("127.0.0.1:7777".to_string()),
                        "vsock" => Transport::Vsock { port: 7777 },
                        _ => return Err(format!("unsupported transport '{value}'")),
                    };
                }
                "--listen" => {
                    let value = next_arg(&mut args, "--listen")?;
                    transport = Transport::Tcp(value);
                }
                "--vsock-port" => {
                    let value = next_arg(&mut args, "--vsock-port")?;
                    let port = value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid --vsock-port '{value}'"))?;
                    transport = Transport::Vsock { port };
                }
                "--help" | "-h" => return Err(usage()),
                _ => return Err(format!("unknown argument '{arg}'\n{}", usage())),
            }
        }

        let policy_path = policy_path.ok_or_else(usage)?;
        Ok(Self {
            policy_path,
            lsp_config_path,
            transport,
        })
    }
}

fn next_arg<I>(args: &mut I, flag: &str) -> Result<String, String>
where
    I: Iterator<Item = String>,
{
    args.next()
        .ok_or_else(|| format!("{flag} requires a value\n{}", usage()))
}

fn usage() -> String {
    "usage: petri-guest --policy <path> [--lsp-config <path>] [--transport stdio|tcp|vsock] [--listen <addr>] [--vsock-port <port>]".to_string()
}
