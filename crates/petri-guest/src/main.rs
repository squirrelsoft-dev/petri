use std::env;
use std::fs::File;
use std::io::{self, BufReader, BufWriter};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::ExitCode;

use petri_guest::policy::Policy;
use petri_guest::server;
use petri_guest::transport::VsockListenerConfig;

#[derive(Debug)]
struct Args {
    policy_path: PathBuf,
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

    match args.transport {
        Transport::Stdio => {
            let stdin = io::stdin();
            let stdout = io::stdout();
            server::serve_lines(stdin.lock(), stdout.lock(), &policy)?;
        }
        Transport::Tcp(addr) => {
            let listener = TcpListener::bind(&addr)?;
            eprintln!("petri-guest: listening on tcp {addr}");
            for stream in listener.incoming() {
                let stream = stream?;
                let reader = BufReader::new(stream.try_clone()?);
                let writer = BufWriter::new(stream);
                server::serve_lines(reader, writer, &policy)?;
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
                    server::serve_lines(reader, writer, &policy)?;
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

impl Args {
    fn parse<I>(mut args: I) -> Result<Self, String>
    where
        I: Iterator<Item = String>,
    {
        let mut policy_path = None;
        let mut transport = Transport::Stdio;

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--policy" => {
                    policy_path = Some(PathBuf::from(next_arg(&mut args, "--policy")?));
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
    "usage: petri-guest --policy <path> [--transport stdio|tcp|vsock] [--listen <addr>] [--vsock-port <port>]".to_string()
}
