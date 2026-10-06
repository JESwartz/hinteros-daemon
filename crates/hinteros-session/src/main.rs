//! HinterOS session daemon (M0 skeleton).
//!
//! Serves the session API (`schema/session-api.json`) on a Unix socket to clients running as
//! the same user. M0: handshake, `ping`, event subscriptions, a stub `apps.list`, and a
//! `home_pressed` event raised on SIGUSR1 (stands in for the controller's Home button).

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use hinteros_api::codec::{LineReader, ReadError, write_message};
use hinteros_api::common::{Peer, Ping};
use hinteros_api::jsonrpc;
use hinteros_api::server::{self, Connection, Step, call, method_not_found};
use hinteros_api::session::{
    self, App, AppKind, AppsList, AppsListResult, EventName, HomePressedParams, Subscribe,
    SubscriptionResult, Unsubscribe,
};
use hinteros_api::{ErrorCode, ErrorObject, Method};
use serde_json::Value;
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

const NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Socket to listen on [default: $XDG_RUNTIME_DIR/hinteros/session.sock]
    #[arg(long, env = "HINTEROS_SESSION_SOCKET")]
    socket: Option<PathBuf>,
}

/// An event as broadcast to every connection: name plus encoded params.
type Broadcast = (EventName, Value);

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
    let path = match args.socket {
        Some(p) => p,
        None => {
            let dir = std::env::var_os("XDG_RUNTIME_DIR")
                .context("XDG_RUNTIME_DIR is not set; pass --socket")?;
            PathBuf::from(dir).join(session::SOCKET_IN_RUNTIME_DIR)
        }
    };

    let listener = server::bind_unix(&path, 0o600, 0o700)
        .with_context(|| format!("binding {}", path.display()))?;
    info!(socket = %path.display(), api_version = %session::VERSION, "listening");

    let (events, _) = broadcast::channel::<Broadcast>(64);
    let mut usr1 = signal(SignalKind::user_defined1())?;
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let rx = events.subscribe();
                    tokio::spawn(async move {
                        if let Err(e) = handle(stream, rx).await {
                            debug!("connection ended: {e:#}");
                        }
                    });
                }
                Err(e) => warn!("accept failed: {e}"),
            },
            _ = usr1.recv() => {
                info!("SIGUSR1: Home pressed");
                let params = serde_json::to_value(HomePressedParams {})?;
                // No receivers is fine: nobody is connected.
                let _ = events.send((EventName::HomePressed, params));
            }
            _ = term.recv() => break,
            _ = int.recv() => break,
        }
    }
    info!("shutting down");
    let _ = std::fs::remove_file(&path);
    Ok(())
}

async fn handle(stream: UnixStream, mut events: broadcast::Receiver<Broadcast>) -> Result<()> {
    // Defence in depth: the socket is 0600 in a 0700 directory, but only our own user may talk to us.
    let cred = stream.peer_cred()?;
    let me = rustix::process::geteuid().as_raw();
    if cred.uid() != me {
        warn!(uid = cred.uid(), pid = ?cred.pid(), "rejecting connection from another user");
        return Ok(());
    }
    debug!(pid = ?cred.pid(), "client connected");

    let (r, mut w) = stream.into_split();
    let mut reader = LineReader::new(r);
    let mut conn = Connection::new(session::VERSION, Peer::new(NAME, VERSION));
    let mut subscriptions = BTreeSet::new();

    loop {
        tokio::select! {
            line = reader.next_line() => {
                let line = match line {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(e @ (ReadError::TooLong | ReadError::NotUtf8)) => {
                        let err = ErrorObject::new(ErrorCode::ParseError, e.to_string());
                        write_message(&mut w, &jsonrpc::error_response(None, err)).await?;
                        break;
                    }
                    Err(ReadError::Io(e)) => return Err(e.into()),
                };
                let was_handshaken = conn.negotiated().is_some();
                match conn.step(&line) {
                    Step::Reply(reply) => write_message(&mut w, &reply).await?,
                    Step::ReplyAndClose(reply) => {
                        warn!(reply = %reply["error"]["message"], "closing connection");
                        write_message(&mut w, &reply).await?;
                        break;
                    }
                    Step::Ignore => debug!("ignoring client notification"),
                    Step::Dispatch(req) => {
                        let id = req.id.expect("dispatched requests have an id");
                        let outcome = dispatch(&req.method, req.params, &mut subscriptions);
                        write_message(&mut w, &jsonrpc::response(&id, outcome)).await?;
                    }
                }
                if let (false, Some((version, client))) = (was_handshaken, conn.negotiated()) {
                    info!(client = %client.name, client_version = %client.version, api_version = %version, "handshake done");
                }
            }
            event = events.recv() => match event {
                Ok((name, params)) if subscriptions.contains(&name) => {
                    write_message(&mut w, &jsonrpc::notification(name.wire_name(), params)).await?;
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => warn!("client lagging, dropped {n} events"),
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    Ok(())
}

fn dispatch(
    method: &str,
    params: Option<Value>,
    subscriptions: &mut BTreeSet<EventName>,
) -> Result<Value, ErrorObject> {
    match method {
        Ping::NAME => call::<Ping>(params, server::ping),
        Subscribe::NAME => call::<Subscribe>(params, |p| {
            subscriptions.extend(p.events);
            Ok(SubscriptionResult {
                events: subscriptions.iter().copied().collect(),
            })
        }),
        Unsubscribe::NAME => call::<Unsubscribe>(params, |p| {
            for e in &p.events {
                subscriptions.remove(e);
            }
            Ok(SubscriptionResult {
                events: subscriptions.iter().copied().collect(),
            })
        }),
        AppsList::NAME => call::<AppsList>(params, |_| Ok(stub_apps())),
        _ => Err(method_not_found(method)),
    }
}

/// M0 stand-in for the real app list (desktop entries, Steam library, catalog apps; M2/M3).
fn stub_apps() -> AppsListResult {
    let app = |id: &str, name: &str, kind| App {
        id: id.into(),
        name: name.into(),
        kind,
        icon: None,
    };
    AppsListResult {
        apps: vec![
            app("steam", "Steam", AppKind::Native),
            app("jellyfin", "Jellyfin", AppKind::Native),
            app("youtube", "YouTube", AppKind::Web),
            app("crunchyroll", "Crunchyroll", AppKind::Web),
            app("firefox", "Firefox", AppKind::Native),
        ],
    }
}
