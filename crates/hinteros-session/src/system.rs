//! Relay to the privileged system service (`schema/system-api.json`).
//!
//! The session daemon keeps one connection to `/run/hinteros/system.sock`, handshakes with the
//! system API version, subscribes to the job events and forwards them to its own clients. The
//! session API's `system.*` methods (`relay: true` in the schema) are passed through with the
//! same params; results and errors come back unchanged (one 3xxx code space). The system's
//! `keyboard_home` (Ctrl + Super + D, which only the system reads: see `hinteros-system
//! keyboard-home`) is not forwarded: it is Home for the shell, like SIGUSR1. The service
//! authorizes this daemon like any other process of the user: the relay is a convenience, not a
//! trust step. While the service is down or speaks another version, relayed methods fail with
//! `system_unavailable {reason}` and the rest of the session API keeps working.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use hinteros_api::codec::{LineReader, ReadError, write_message};
use hinteros_api::common::{Hello, HelloParams, Peer};
use hinteros_api::jsonrpc::{self, Id, ServerMessage};
use hinteros_api::session::EventName;
use hinteros_api::{ErrorCode, ErrorObject, Method, system};
use serde_json::{Value, json};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::shell::Msg;
use crate::{Audience, Broadcast};

const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(10);
/// A relayed call that gets no answer in this time fails (the service answers at once; jobs
/// report through events).
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// A call whose action is checked interactively waits for the user to type the admin password
/// (the system service gives up after 300 s).
const INTERACTIVE_CALL_TIMEOUT: Duration = Duration::from_secs(330);

type Reply = oneshot::Sender<Result<Value, ErrorObject>>;

struct Call {
    method: &'static str,
    params: Option<Value>,
    reply: Reply,
}

/// Handle for relaying calls; cheap to clone.
#[derive(Clone)]
pub struct Relay {
    tx: Option<mpsc::UnboundedSender<Call>>,
}

/// Why the service cannot be used right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    Down,
    VersionMismatch,
}

pub fn unavailable(why: Unavailable) -> ErrorObject {
    let (reason, message) = match why {
        Unavailable::Down => ("down", "the system service is not running"),
        Unavailable::VersionMismatch => (
            "version_mismatch",
            "the system service speaks another API version (restart the session)",
        ),
    };
    ErrorObject::with_data(
        ErrorCode::SystemUnavailable,
        message,
        json!({ "reason": reason }),
    )
}

impl Relay {
    /// A relay that is switched off (`[system] enable = false`): every call is `down`.
    pub fn disabled() -> Self {
        Self { tx: None }
    }

    /// Start the connection task. `home`: where the system's `keyboard_home` goes (`None`:
    /// ignored, `[input] keyboard_home = false`).
    pub fn spawn(
        socket: PathBuf,
        events: broadcast::Sender<Broadcast>,
        home: Option<mpsc::UnboundedSender<Msg>>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run(socket, rx, events, home));
        Self { tx: Some(tx) }
    }

    /// Call `method` (a system API method name) with the client's params.
    pub async fn call(
        &self,
        method: &'static str,
        params: Option<Value>,
    ) -> Result<Value, ErrorObject> {
        let Some(tx) = &self.tx else {
            return Err(unavailable(Unavailable::Down));
        };
        let (reply, rx) = oneshot::channel();
        tx.send(Call {
            method,
            params,
            reply,
        })
        .map_err(|_| unavailable(Unavailable::Down))?;
        let interactive =
            system::polkit_action(method).is_some_and(|a| system::INTERACTIVE_ACTIONS.contains(&a));
        let limit = if interactive {
            INTERACTIVE_CALL_TIMEOUT
        } else {
            CALL_TIMEOUT
        };
        match tokio::time::timeout(limit, rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => Err(unavailable(Unavailable::Down)),
            // The schema has no reason of its own for this (`down`); the message says it.
            Err(_) => Err(ErrorObject::with_data(
                ErrorCode::SystemUnavailable,
                "the system service did not answer in time",
                json!({ "reason": "down" }),
            )),
        }
    }
}

async fn run(
    socket: PathBuf,
    mut calls: mpsc::UnboundedReceiver<Call>,
    events: broadcast::Sender<Broadcast>,
    home: Option<mpsc::UnboundedSender<Msg>>,
) {
    let mut backoff = BACKOFF_MIN;
    // Logged once per reason, not on every retry.
    let mut logged: Option<Unavailable> = None;
    loop {
        let why = match connect(&socket).await {
            Ok(conn) => {
                info!(socket = %socket.display(), "connected to the system service");
                logged = None;
                backoff = BACKOFF_MIN;
                serve(conn, &mut calls, &events, home.as_ref()).await;
                if calls.is_closed() {
                    return;
                }
                info!("lost the system service; reconnecting");
                Unavailable::Down
            }
            Err(why) => {
                if logged != Some(why) {
                    warn!(socket = %socket.display(), ?why, "system service unavailable; relayed methods fail until it is back");
                    logged = Some(why);
                }
                why
            }
        };
        // Answer calls at once while waiting to retry.
        let sleep = tokio::time::sleep(backoff);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                call = calls.recv() => match call {
                    Some(c) => { let _ = c.reply.send(Err(unavailable(why))); }
                    None => return,
                },
            }
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

struct Conn {
    reader: LineReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

/// Connect, handshake and subscribe to the job events.
async fn connect(socket: &PathBuf) -> Result<Conn, Unavailable> {
    let stream = UnixStream::connect(socket).await.map_err(|e| {
        debug!("connecting to {}: {e}", socket.display());
        Unavailable::Down
    })?;
    let (r, w) = stream.into_split();
    let mut conn = Conn {
        reader: LineReader::new(r),
        writer: w,
    };
    let hello = HelloParams {
        api_version: system::VERSION,
        client: Peer::new(crate::server::NAME, crate::server::VERSION),
    };
    let params = serde_json::to_value(hello).map_err(|_| Unavailable::Down)?;
    match request(&mut conn, 1, Hello::NAME, params).await {
        Ok(_) => {}
        Err(Some(e)) if e.kind() == Some(ErrorCode::VersionMismatch) => {
            return Err(Unavailable::VersionMismatch);
        }
        Err(_) => return Err(Unavailable::Down),
    }
    let names: Vec<&str> = system::EventName::ALL
        .iter()
        .map(|e| e.wire_name())
        .collect();
    request(
        &mut conn,
        2,
        system::Subscribe::NAME,
        json!({ "events": names }),
    )
    .await
    .map_err(|_| Unavailable::Down)?;
    Ok(conn)
}

/// One request during setup (no other traffic yet). `Err(None)`: connection trouble.
async fn request(
    conn: &mut Conn,
    id: i64,
    method: &str,
    params: Value,
) -> Result<Value, Option<ErrorObject>> {
    let id = Id::Number(id);
    write_message(&mut conn.writer, &jsonrpc::request(&id, method, params))
        .await
        .map_err(|_| None)?;
    loop {
        let line = tokio::time::timeout(Duration::from_secs(10), conn.reader.next_line())
            .await
            .map_err(|_| None)?
            .map_err(|_| None)?
            .ok_or(None)?;
        let value: Value = serde_json::from_str(&line).map_err(|_| None)?;
        match jsonrpc::parse_server_message(value) {
            Ok(ServerMessage::Response {
                id: Some(got),
                outcome,
            }) if got == id => {
                return outcome.map_err(Some);
            }
            Ok(_) => continue,
            Err(_) => return Err(None),
        }
    }
}

/// Relay calls and events until the connection ends.
async fn serve(
    mut conn: Conn,
    calls: &mut mpsc::UnboundedReceiver<Call>,
    events: &broadcast::Sender<Broadcast>,
    home: Option<&mpsc::UnboundedSender<Msg>>,
) {
    let mut pending: HashMap<i64, Reply> = HashMap::new();
    let mut next_id: i64 = 3;
    loop {
        tokio::select! {
            call = calls.recv() => {
                let Some(call) = call else { break };
                let id = next_id;
                next_id += 1;
                let params = call.params.unwrap_or_else(|| json!({}));
                let msg = jsonrpc::request(&Id::Number(id), call.method, params);
                // The service reads at most system::MAX_MESSAGE_BYTES a line and drops the
                // connection, every other call on it too, for a longer one.
                if too_long(&msg) {
                    let _ = call.reply.send(Err(ErrorObject::new(
                        ErrorCode::InvalidParams,
                        format!("params too long for the system service (at most {} bytes)", system::MAX_MESSAGE_BYTES),
                    )));
                    continue;
                }
                // Calls that timed out (Relay::call) wait for no answer any more.
                pending.retain(|_, reply| !reply.is_closed());
                if write_message(&mut conn.writer, &msg).await.is_err() {
                    let _ = call.reply.send(Err(unavailable(Unavailable::Down)));
                    break;
                }
                pending.insert(id, call.reply);
            }
            line = conn.reader.next_line() => {
                let line = match line {
                    Ok(Some(line)) => line,
                    Ok(None) | Err(ReadError::Io(_)) => break,
                    Err(e) => { warn!("system service sent a bad message: {e}"); break; }
                };
                let Ok(value) = serde_json::from_str::<Value>(&line) else { continue };
                match jsonrpc::parse_server_message(value) {
                    Ok(ServerMessage::Response { id: Some(Id::Number(id)), outcome }) => {
                        if let Some(reply) = pending.remove(&id) {
                            let _ = reply.send(outcome.map_err(map_error));
                        }
                    }
                    Ok(ServerMessage::Notification { method, params }) => {
                        if method == <system::KeyboardHome as hinteros_api::Event>::NAME {
                            // Only ever sent to the seat0 owner's connections, by root.
                            if let Some(home) = home {
                                info!("Ctrl+Super+D: Home pressed");
                                let _ = home.send(Msg::Home);
                            }
                        } else if method == <system::ProfilesChanged as hinteros_api::Event>::NAME {
                            // The session re-queries and sends its own (see profile.rs).
                            let _ = events.send((EventName::ProfilesChanged, params, Audience::Internal));
                        } else if let Some(name) = session_event(&method) {
                            let _ = events.send((name, params, Audience::All));
                        }
                    }
                    Ok(other) => debug!("unexpected message from the system service: {other:?}"),
                    Err(e) => debug!("bad message from the system service: {e}"),
                }
            }
        }
    }
    for (_, reply) in pending.drain() {
        let _ = reply.send(Err(unavailable(Unavailable::Down)));
    }
}

/// Whether `msg`, as one line, is longer than the system service reads.
fn too_long(msg: &Value) -> bool {
    serde_json::to_vec(msg).map_or(usize::MAX, |b| b.len()) > system::MAX_MESSAGE_BYTES
}

/// System errors are forwarded unchanged, except protocol-level ones (which mean the relay's
/// own connection is broken, not the caller's request).
fn map_error(e: ErrorObject) -> ErrorObject {
    match e.kind() {
        Some(ErrorCode::HandshakeRequired | ErrorCode::AlreadyHandshaken) => {
            unavailable(Unavailable::Down)
        }
        Some(ErrorCode::VersionMismatch) => unavailable(Unavailable::VersionMismatch),
        _ => e,
    }
}

/// The session event for a system event.
pub fn session_event(name: &str) -> Option<EventName> {
    match name {
        "job_started" => Some(EventName::JobStarted),
        "job_progress" => Some(EventName::JobProgress),
        "job_finished" => Some(EventName::JobFinished),
        "time_zone_changed" => Some(EventName::TimeZoneChanged),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_events_and_errors() {
        // profiles_changed is not forwarded as it is: the session re-queries (profile.rs);
        // keyboard_home is Home for the shell.
        for e in system::EventName::ALL.iter().filter(|e| {
            !matches!(
                e,
                system::EventName::ProfilesChanged | system::EventName::KeyboardHome
            )
        }) {
            let s = session_event(e.wire_name()).unwrap();
            assert_eq!(s.wire_name(), e.wire_name());
        }
        assert_eq!(session_event("home_pressed"), None);
        assert_eq!(session_event("keyboard_home"), None);
        let denied = ErrorObject::with_data(
            ErrorCode::PermissionDenied,
            "no",
            json!({"action": "org.hinteros.system.ssh", "reason": "seat"}),
        );
        assert_eq!(map_error(denied.clone()), denied);
        let e = map_error(ErrorObject::new(ErrorCode::HandshakeRequired, "x"));
        assert_eq!(e.kind(), Some(ErrorCode::SystemUnavailable));
        assert_eq!(e.data.unwrap()["reason"], "down");
    }

    #[test]
    fn params_the_service_would_not_read_stay_here() {
        let call = |text: String| {
            jsonrpc::request(&Id::Number(3), "apps.install", json!({ "revision": text }))
        };
        assert!(!too_long(&call("x".repeat(1000))));
        assert!(too_long(&call("x".repeat(system::MAX_MESSAGE_BYTES))));
    }
}
