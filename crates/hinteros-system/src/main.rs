//! HinterOS privileged system service (M0 skeleton).
//!
//! Runs as root and serves the internal system API (`schema/system-api.json`) to the session
//! daemon. Every connection is identified with SO_PEERCRED. M0 only logs the caller and answers
//! the handshake and `ping`; authorization is a TODO (M5).

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use hinteros_api::codec::{LineReader, ReadError, write_message};
use hinteros_api::common::{Peer, Ping};
use hinteros_api::jsonrpc;
use hinteros_api::server::{self, Connection, Step, call, method_not_found};
use hinteros_api::system;
use hinteros_api::{ErrorCode, ErrorObject, Method};
use serde_json::Value;
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};
use tracing::{debug, info, warn};

const NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Socket to listen on
    #[arg(long, env = "HINTEROS_SYSTEM_SOCKET", default_value = system::DEFAULT_SOCKET)]
    socket: PathBuf,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    let args = Args::parse();

    // 0660: only root and the socket's group (set by the systemd unit) can connect at all;
    // each request is still authorized per caller (TODO(M5): polkit).
    let listener = server::bind_unix(&args.socket, 0o660, 0o755)
        .with_context(|| format!("binding {}", args.socket.display()))?;
    info!(socket = %args.socket.display(), api_version = %system::VERSION, "listening");

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    tokio::spawn(async move {
                        if let Err(e) = handle(stream).await {
                            debug!("connection ended: {e:#}");
                        }
                    });
                }
                Err(e) => warn!("accept failed: {e}"),
            },
            _ = term.recv() => break,
            _ = int.recv() => break,
        }
    }
    info!("shutting down");
    let _ = std::fs::remove_file(&args.socket);
    Ok(())
}

async fn handle(stream: UnixStream) -> Result<()> {
    let cred = stream.peer_cred()?;
    // TODO(M5): authorize with polkit (CheckAuthorization with a unix-process subject built
    // from a pidfd via SO_PEERPIDFD, so the PID cannot be recycled), per privileged action.
    info!(uid = cred.uid(), gid = cred.gid(), pid = ?cred.pid(), "connection");

    let (r, mut w) = stream.into_split();
    let mut reader = LineReader::new(r);
    let mut conn = Connection::new(system::VERSION, Peer::new(NAME, VERSION));
    loop {
        let line = match reader.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(e @ (ReadError::TooLong | ReadError::NotUtf8)) => {
                let err = ErrorObject::new(ErrorCode::ParseError, e.to_string());
                write_message(&mut w, &jsonrpc::error_response(None, err)).await?;
                break;
            }
            Err(ReadError::Io(e)) => return Err(e.into()),
        };
        match conn.step(&line) {
            Step::Reply(reply) => write_message(&mut w, &reply).await?,
            Step::ReplyAndClose(reply) => {
                warn!(uid = cred.uid(), reply = %reply["error"]["message"], "closing connection");
                write_message(&mut w, &reply).await?;
                break;
            }
            Step::Ignore => {}
            Step::Dispatch(req) => {
                let id = req.id.expect("dispatched requests have an id");
                info!(uid = cred.uid(), method = %req.method, "request");
                let outcome = dispatch(&req.method, req.params);
                write_message(&mut w, &jsonrpc::response(&id, outcome)).await?;
            }
        }
    }
    Ok(())
}

fn dispatch(method: &str, params: Option<Value>) -> Result<Value, ErrorObject> {
    match method {
        Ping::NAME => call::<Ping>(params, server::ping),
        _ => Err(method_not_found(method)),
    }
}
