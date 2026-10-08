//! HinterOS privileged system service binary. See the library docs (`lib.rs`) and the README.
//!
//! `hinteros-system` serves the socket; `hinteros-system job <id>` runs one job (what the
//! `hinteros-job@<id>.service` unit executes).

use std::process::ExitCode;

use clap::Parser;
use hinteros_system::config::{Cli, Command};
use hinteros_system::state::{State, StateError};
use hinteros_system::{runner, service};
use tracing::error;

/// `check-state`: 0 all valid, 1 some invalid, 2 a file could not be read.
fn check_state(files: &[std::path::PathBuf]) -> ExitCode {
    let (mut invalid, mut io_error) = (false, false);
    for file in files {
        let shown = file.display();
        match std::fs::read(file) {
            Err(e) => {
                println!("invalid {shown}: reading: {e}");
                io_error = true;
            }
            Ok(bytes) => match State::parse(&bytes) {
                Ok(_) => println!("ok {shown}"),
                Err(StateError::Invalid(reason)) => {
                    println!("invalid {shown}: {reason}");
                    invalid = true;
                }
                Err(e) => {
                    println!("invalid {shown}: {e}");
                    invalid = true;
                }
            },
        }
    }
    if io_error {
        ExitCode::from(2)
    } else if invalid {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn main() -> ExitCode {
    // Handled before logging starts: State::parse logs unknown fields, and the check's output
    // is the report.
    let cli = Cli::parse();
    if let Some(Command::CheckState { files }) = &cli.command {
        return check_state(files);
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    match cli.command {
        Some(Command::Job { run_dir, id }) => runner::main(&run_dir, &id),
        Some(Command::CheckState { .. }) => unreachable!("handled above"),
        None => {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(anyhow::Error::from)
                .and_then(|rt| rt.block_on(service::serve(cli.serve)));
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    error!("{e:#}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
