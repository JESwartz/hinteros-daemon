//! One client connection: framing, handshake, subscriptions, and dispatch (core methods here,
//! `apps.*` to the [`crate::shell::Shell`]).

use std::collections::BTreeSet;

use anyhow::Result;
use hinteros_api::codec::{LineReader, ReadError, close_gracefully, write_message};
use hinteros_api::common::{Peer, Ping};
use hinteros_api::server::{self, Connection, Step, call, method_not_found};
use hinteros_api::session::{self, EventName, Subscribe, SubscriptionResult, Unsubscribe};
use hinteros_api::{ErrorCode, ErrorObject, Method, jsonrpc};
use serde_json::Value;
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::auth::Auth;
use crate::restart::Restarter;
use crate::runtime::Runtime;
use crate::shell::{METHODS, Msg};
use crate::store::Store;
use crate::system::Relay;
use crate::{Audience, Broadcast};

/// Everything a connection dispatches to besides the shell; cheap to clone.
#[derive(Clone)]
pub struct Services {
    pub relay: Relay,
    pub runtime: Runtime,
    pub auth: Auth,
    pub store: Store,
    pub restarter: Restarter,
    pub profiles: crate::profile::Profiles,
    /// Greeter mode only.
    pub greeter: Option<crate::greeter::Greeter>,
}

/// Methods answered by a task of their own, so a slow one (a relayed call waiting for the admin
/// password, a Wi-Fi scan) never holds up the connection: the launcher answers the password
/// prompt on the same connection, and its controller input comes as events on it. Their
/// responses may come out of order (clients match them by id).
fn concurrent(method: &str) -> bool {
    session::relay_target(method).is_some()
        || crate::profile::METHODS.contains(&method)
        || crate::greeter::METHODS.contains(&method)
        || Runtime::waits(method)
        || matches!(
            method,
            "auth.respond" | "auth.cancel" | "store.list" | "session.restart"
        )
}

pub const NAME: &str = "hinteros-session";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Serve one client until it disconnects.
pub async fn handle(
    stream: UnixStream,
    mut events: broadcast::Receiver<Broadcast>,
    shell: mpsc::UnboundedSender<Msg>,
    services: Services,
) -> Result<()> {
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
    let (answers_tx, mut answers) = mpsc::unbounded_channel::<Value>();

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
                        if req.method == session::AppsLaunch::NAME {
                            // Without holding up the connection: a launch of an app that is
                            // still closing waits for it to go; the Jellyfin app gets its
                            // sign-in seed first (a system call).
                            let seed = crate::appseed::wants(&req.method, req.params.as_ref());
                            let (shell, relay, tx) =
                                (shell.clone(), services.relay.clone(), answers_tx.clone());
                            tokio::spawn(async move {
                                if seed {
                                    crate::appseed::before_launch(&shell, &relay).await;
                                }
                                let outcome = shell_call(&shell, &req.method, req.params).await;
                                let _ = tx.send(jsonrpc::response(&id, outcome));
                            });
                        } else if concurrent(&req.method) {
                            let client = conn.negotiated().map(|(_, c)| c.name.clone());
                            let (services, tx) = (services.clone(), answers_tx.clone());
                            tokio::spawn(async move {
                                let outcome = slow(&req.method, req.params, client.as_deref(), &services).await;
                                let _ = tx.send(jsonrpc::response(&id, outcome));
                            });
                        } else {
                            let outcome = dispatch(
                                &req.method,
                                req.params,
                                &mut subscriptions,
                                &shell,
                                &services.runtime,
                                &services.relay,
                            ).await;
                            write_message(&mut w, &jsonrpc::response(&id, outcome)).await?;
                        }
                    }
                }
                if let (false, Some((version, client))) = (was_handshaken, conn.negotiated()) {
                    info!(client = %client.name, client_version = %client.version, api_version = %version, "handshake done");
                }
                // The line may have carried a password (auth.respond): wipe it. Best effort:
                // auth.respond moves the parsed password into a Zeroizing string (never a copy
                // of the params), but the socket's read buffer keeps the bytes until it is
                // filled again.
                zeroize::Zeroize::zeroize(&mut line.into_bytes());
            }
            Some(answer) = answers.recv() => write_message(&mut w, &answer).await?,
            event = events.recv() => match event {
                Ok((name, params, audience)) if subscriptions.contains(&name) && for_us(audience, &conn) => {
                    write_message(&mut w, &jsonrpc::notification(name.wire_name(), params)).await?;
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // It missed events (an app_exited, a focus_changed) and its state is wrong:
                    // close, so that it connects again and reads the state anew.
                    warn!("client lagging, dropped {n} events; closing the connection");
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    // Every `break` above is a deliberate close; make sure the client gets what we sent.
    close_gracefully(&mut reader, &mut w).await;
    Ok(())
}

/// Whether an event for `audience` goes to this connection.
fn for_us(audience: Audience, conn: &Connection) -> bool {
    let osk = conn
        .negotiated()
        .is_some_and(|(_, client)| client.name == session::OSK_CLIENT_NAME);
    match audience {
        Audience::All => true,
        Audience::Osk => osk,
        Audience::Launcher => !osk,
        Audience::LauncherClient => conn
            .negotiated()
            .is_some_and(|(_, client)| client.name == session::LAUNCHER_CLIENT_NAME),
        Audience::Internal => false,
    }
}

async fn dispatch(
    method: &str,
    params: Option<Value>,
    subscriptions: &mut BTreeSet<EventName>,
    shell: &mpsc::UnboundedSender<Msg>,
    runtime: &Runtime,
    relay: &Relay,
) -> Result<Value, ErrorObject> {
    if Runtime::handles(method) {
        let kept = params.clone();
        let result = runtime.call(method, params).await?;
        persist_shared(method, kept.as_ref(), runtime, relay);
        return Ok(result);
    }
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
        m if METHODS.contains(&m) => shell_call(shell, m, params).await,
        _ => Err(method_not_found(method)),
    }
}

/// A method the shell answers ([`METHODS`]).
async fn shell_call(
    shell: &mpsc::UnboundedSender<Msg>,
    method: &str,
    params: Option<Value>,
) -> Result<Value, ErrorObject> {
    let (reply, result) = oneshot::channel();
    let unavailable =
        || ErrorObject::new(ErrorCode::InternalError, "session daemon is shutting down");
    shell
        .send(Msg::Request {
            method: method.into(),
            params,
            reply,
        })
        .map_err(|_| unavailable())?;
    result.await.map_err(|_| unavailable())?
}

/// The display mode and the audio output are the box's, not the profile's (profiles.md): a kept
/// mode and a chosen output are saved with the system service, which publishes them for every
/// profile's session. Best effort, in the background.
fn persist_shared(method: &str, params: Option<&Value>, runtime: &Runtime, relay: &Relay) {
    let calls: Vec<(&'static str, Value)> = match method {
        // The display the kept change is on: gamescope's save file has a line for every
        // display it has seen.
        "display.keep" => runtime
            .take_kept_display()
            .map(|(display, mode)| {
                vec![(
                    hinteros_api::system::DisplaySaveMode::NAME,
                    serde_json::json!({
                        "display": display,
                        "width": mode.width,
                        "height": mode.height,
                        "refresh": mode.refresh,
                    }),
                )]
            })
            .unwrap_or_default(),
        "audio.set_output" => params
            .and_then(|p| p.get("id"))
            .and_then(Value::as_str)
            .map(|node| {
                vec![(
                    hinteros_api::system::AudioSaveOutput::NAME,
                    serde_json::json!({ "node": node }),
                )]
            })
            .unwrap_or_default(),
        _ => return,
    };
    for (m, p) in calls {
        let relay = relay.clone();
        tokio::spawn(async move {
            if let Err(e) = relay.call(m, Some(p)).await {
                debug!("not saved for every profile ({m}): {}", e.message);
            }
        });
    }
}

/// The [`concurrent`] methods.
async fn slow(
    method: &str,
    params: Option<Value>,
    client: Option<&str>,
    s: &Services,
) -> Result<Value, ErrorObject> {
    if let Some(target) = session::relay_target(method) {
        let kept = params.clone();
        let mut result = s.relay.call(target, params).await?;
        if target == hinteros_api::system::ServicesList::NAME {
            s.store.service_icons(&mut result);
        }
        s.store.relayed(method, kept.as_ref(), &result);
        return Ok(result);
    }
    match method {
        "auth.respond" => s.auth.respond(client, params).await,
        "auth.cancel" => s.auth.cancel(client, params),
        "store.list" => {
            hinteros_api::server::decode_params::<session::StoreList>(params)?;
            s.store.list(&s.relay).await
        }
        "session.restart" => {
            hinteros_api::server::decode_params::<session::SessionRestart>(params.clone())?;
            s.restarter.restart(params).await
        }
        m if Runtime::waits(m) => {
            let kept = params.clone();
            let result = s.runtime.call(m, params).await?;
            persist_shared(m, kept.as_ref(), &s.runtime, &s.relay);
            Ok(result)
        }
        m if crate::profile::METHODS.contains(&m) => s.profiles.call(m, params).await,
        m if crate::greeter::METHODS.contains(&m) => match &s.greeter {
            Some(g) => g.call(m, params, client).await,
            None => Err(ErrorObject::with_data(
                ErrorCode::Unavailable,
                "not in greeter mode",
                serde_json::json!({"service": "logind", "reason": "not_greeter"}),
            )),
        },
        _ => Err(method_not_found(method)),
    }
}
