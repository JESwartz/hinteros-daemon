//! One client connection: framing, handshake, subscriptions, and dispatch (core methods here,
//! `apps.*` to the [`crate::shell::Shell`]).

use std::collections::BTreeSet;

use anyhow::Result;
use hinteros_api::codec::{LineReader, ReadError, write_message};
use hinteros_api::common::{Peer, Ping};
use hinteros_api::server::{self, Connection, Step, call, method_not_found};
use hinteros_api::session::{self, EventName, Subscribe, SubscriptionResult, Unsubscribe};
use hinteros_api::{ErrorCode, ErrorObject, Method, jsonrpc};
use serde_json::Value;
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::runtime::Runtime;
use crate::shell::{METHODS, Msg};
use crate::system::Relay;
use crate::{Audience, Broadcast};

pub const NAME: &str = "hinteros-session";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Serve one client until it disconnects.
pub async fn handle(
    stream: UnixStream,
    mut events: broadcast::Receiver<Broadcast>,
    shell: mpsc::UnboundedSender<Msg>,
    relay: Relay,
    runtime: Runtime,
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
                        let outcome = dispatch(
                            &req.method,
                            req.params,
                            &mut subscriptions,
                            &shell,
                            &relay,
                            &runtime,
                        ).await;
                        write_message(&mut w, &jsonrpc::response(&id, outcome)).await?;
                    }
                }
                if let (false, Some((version, client))) = (was_handshaken, conn.negotiated()) {
                    info!(client = %client.name, client_version = %client.version, api_version = %version, "handshake done");
                }
            }
            event = events.recv() => match event {
                Ok((name, params, audience)) if subscriptions.contains(&name) && for_us(audience, &conn) => {
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

/// Whether an event for `audience` goes to this connection.
fn for_us(audience: Audience, conn: &Connection) -> bool {
    let osk = conn
        .negotiated()
        .is_some_and(|(_, client)| client.name == session::OSK_CLIENT_NAME);
    match audience {
        Audience::All => true,
        Audience::Osk => osk,
        Audience::Launcher => !osk,
    }
}

async fn dispatch(
    method: &str,
    params: Option<Value>,
    subscriptions: &mut BTreeSet<EventName>,
    shell: &mpsc::UnboundedSender<Msg>,
    relay: &Relay,
    runtime: &Runtime,
) -> Result<Value, ErrorObject> {
    if Runtime::handles(method) {
        return runtime.call(method, params).await;
    }
    if let Some(target) = session::relay_target(method) {
        return relay.call(target, params).await;
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
        m if METHODS.contains(&m) => {
            let (reply, result) = oneshot::channel();
            let unavailable =
                || ErrorObject::new(ErrorCode::InternalError, "session daemon is shutting down");
            shell
                .send(Msg::Request {
                    method: m.into(),
                    params,
                    reply,
                })
                .map_err(|_| unavailable())?;
            result.await.map_err(|_| unavailable())?
        }
        _ => Err(method_not_found(method)),
    }
}
