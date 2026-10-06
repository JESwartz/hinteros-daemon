//! HinterOS privileged system service binary. See the library docs (`lib.rs`) and the README.
//!
//! `hinteros-system` serves the socket; `hinteros-system job <id>` runs one job (what the
//! `hinteros-job@<id>.service` unit executes).

use std::process::ExitCode;

use clap::Parser;
use hinteros_system::config::{Cli, Command};
use hinteros_system::{runner, service};
use tracing::error;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Job { run_dir, id }) => runner::main(&run_dir, &id),
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
