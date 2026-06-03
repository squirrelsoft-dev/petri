use std::process::ExitCode;

use petri::StubBackend;

fn main() -> ExitCode {
    let backend = StubBackend;
    match petri::cli::run(std::env::args_os().skip(1), &backend) {
        Ok(output) => {
            println!("{output}");
            ExitCode::SUCCESS
        }
        Err(petri::PetriError::Cli(message)) if message.starts_with("usage:") => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("petri: {err}");
            ExitCode::FAILURE
        }
    }
}
