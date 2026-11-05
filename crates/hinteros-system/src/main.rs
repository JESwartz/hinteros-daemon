//! HinterOS privileged system service binary. See the library docs (`lib.rs`) and the README.
//!
//! `hinteros-system` serves the socket; `hinteros-system job <id>` runs one job (what the
//! `hinteros-job@<id>.service` unit executes).

use std::process::ExitCode;

use clap::Parser;
use hinteros_api::catalog::Index;
use hinteros_system::config::{Cli, Command};
use hinteros_system::state::{State, StateError};
use hinteros_system::{runner, service};
use tracing::error;

/// `check-state` and `check-catalog`: 0 all valid, 1 some invalid, 2 a file could not be read.
fn check_files(
    files: &[std::path::PathBuf],
    parse: impl Fn(&[u8]) -> Result<(), String>,
) -> ExitCode {
    let (mut invalid, mut io_error) = (false, false);
    for file in files {
        let shown = file.display();
        match std::fs::read(file) {
            Err(e) => {
                println!("invalid {shown}: reading: {e}");
                io_error = true;
            }
            Ok(bytes) => match parse(&bytes) {
                Ok(()) => println!("ok {shown}"),
                Err(reason) => {
                    println!("invalid {shown}: {reason}");
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

fn check_state(files: &[std::path::PathBuf]) -> ExitCode {
    check_files(files, |bytes| match State::parse(bytes) {
        Ok(_) => Ok(()),
        Err(StateError::Invalid(reason)) => Err(reason),
        Err(e) => Err(e.to_string()),
    })
}

fn check_catalog(files: &[std::path::PathBuf]) -> ExitCode {
    check_files(files, |bytes| {
        // The directory only locates terms files, which the parse does not read.
        Index::parse(bytes, std::path::PathBuf::from("/"))
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
}

fn main() -> ExitCode {
    // Handled before logging starts: the parsers log unknown fields, and the check's output is
    // the report.
    let cli = Cli::parse();
    match &cli.command {
        Some(Command::CheckState { files }) => return check_state(files),
        Some(Command::CheckCatalog { files }) => return check_catalog(files),
        _ => {}
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    match cli.command {
        Some(Command::Job { serve, id }) => runner::main(&serve, &id),
        Some(Command::Pin {
            verb,
            dirs,
            bus,
            font,
            journal_socket,
        }) => {
            let paths = dirs.paths();
            hinteros_system::pam::main(
                verb,
                &hinteros_system::pam::Ctx {
                    users: paths.users.clone(),
                    paths,
                    bus,
                    journal_socket,
                },
                font.as_deref(),
            )
        }
        Some(Command::SplashPaint(args)) => {
            hinteros_system::splash::paint_main(&args);
            ExitCode::SUCCESS
        }
        Some(Command::ResetPin {
            profile,
            dirs,
            journal_socket,
        }) => hinteros_system::resetpin::main(&dirs.paths(), profile.as_deref(), &journal_socket),
        Some(Command::Usage { dirs }) => hinteros_system::usage::main(&dirs.paths()),
        Some(Command::BootEntries {
            boot_entries,
            run_dir,
        }) => hinteros_system::generations::index_main(
            &boot_entries,
            &run_dir.join(hinteros_system::generations::BOOT_INDEX),
        ),
        Some(Command::TimeZoneApply {
            dirs,
            zoneinfo_dir,
            localtime,
        }) => {
            let mut paths = dirs.paths();
            paths.zoneinfo_dir = zoneinfo_dir;
            paths.localtime = localtime;
            hinteros_system::timezone::apply_main(&paths)
        }
        Some(Command::KeyboardHome { socket, input_dir }) => {
            hinteros_system::keyboard::main(&input_dir, &socket)
        }
        Some(Command::LanFilterApply {
            run_dir,
            cgroup_root,
            resolv_conf,
            proc_dir,
            nft,
        }) => hinteros_system::lanfilter::apply_main(&hinteros_system::lanfilter::ApplyPaths {
            run_dir,
            cgroup_root,
            resolv_conf,
            proc_dir,
            nft,
        }),
        Some(Command::CheckState { .. } | Command::CheckCatalog { .. }) => {
            unreachable!("handled above")
        }
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
