use std::io::{IsTerminal, Read};
use std::process::ExitCode;

use petri::PetriBackend;

fn main() -> ExitCode {
    let backend = PetriBackend::default();
    let stdin = if std::io::stdin().is_terminal() {
        None
    } else {
        let mut input = String::new();
        match std::io::stdin().read_to_string(&mut input) {
            Ok(_) => Some(input),
            Err(err) => {
                eprintln!("petri: failed to read stdin: {err}");
                return ExitCode::FAILURE;
            }
        }
    };

    match petri::cli::run_with_stdin(std::env::args_os().skip(1), &backend, stdin) {
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
