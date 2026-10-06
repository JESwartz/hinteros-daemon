//! Authorization of mutating requests (system-api.md, "Authorization"):
//! 1. a pidfd for the peer, and the process still alive;
//! 2. the caller's uid owns seat0's active logind session (or is root);
//! 3. polkit `CheckAuthorization` for the method's action with a `unix-process` subject
//!    built from the pidfd (plus pid, start time and uid for older polkit), no interaction.
//!    A challenge (`auth_admin`) is refused as `authentication_required`, anything else that
//!    is not a plain yes as `permission_denied`.
//!
//! Plus a per-uid token bucket for mutating requests.

use std::collections::HashMap;
use std::os::fd::AsFd;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hinteros_api::{ErrorCode, ErrorObject};
use serde_json::json;
use tracing::{debug, warn};
use zbus::zvariant::{Fd, OwnedObjectPath, OwnedValue, Value};

use crate::audit::Decision;
use crate::peer::Peer;

const LOGIND: &str = "org.freedesktop.login1";
const POLKIT: &str = "org.freedesktop.PolicyKit1";
const POLKIT_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";
const POLKIT_IFACE: &str = "org.freedesktop.PolicyKit1.Authority";

/// A lazily opened, re-opened-on-error connection to the system bus (or a test bus).
pub struct Bus {
    address: Option<String>,
    conn: tokio::sync::Mutex<Option<zbus::Connection>>,
}

impl Bus {
    pub fn new(address: Option<String>) -> Self {
        Self {
            address,
            conn: tokio::sync::Mutex::new(None),
        }
    }

    pub async fn get(&self) -> zbus::Result<zbus::Connection> {
        let mut guard = self.conn.lock().await;
        if let Some(c) = guard.as_ref() {
            return Ok(c.clone());
        }
        let conn = match &self.address {
            Some(a) => {
                zbus::connection::Builder::address(a.as_str())?
                    .build()
                    .await?
            }
            None => zbus::Connection::system().await?,
        };
        *guard = Some(conn.clone());
        Ok(conn)
    }

    /// Forget the connection (after an error), so the next call reconnects.
    pub async fn reset(&self) {
        *self.conn.lock().await = None;
    }
}

/// Why a request was refused, for the error and the audit log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denial {
    Peer,
    Seat,
    Polkit,
    AuthRequired,
}

impl Denial {
    pub fn decision(&self) -> Decision {
        match self {
            Self::Peer => Decision::DeniedPeer,
            Self::Seat => Decision::DeniedSeat,
            Self::Polkit => Decision::DeniedPolkit,
            Self::AuthRequired => Decision::AuthRequired,
        }
    }

    pub fn error(&self, action: &str) -> ErrorObject {
        match self {
            Self::AuthRequired => ErrorObject::with_data(
                ErrorCode::AuthenticationRequired,
                format!("{action} needs interactive authentication"),
                json!({ "action": action }),
            ),
            other => {
                let (reason, message) = match other {
                    Self::Peer => ("peer", "the calling process cannot be identified safely"),
                    Self::Seat => ("seat", "caller does not own the active seat0 session"),
                    _ => ("polkit", "polkit denied the action"),
                };
                ErrorObject::with_data(
                    ErrorCode::PermissionDenied,
                    message,
                    json!({ "action": action, "reason": reason }),
                )
            }
        }
    }
}

/// Run all checks for `action` (the method's polkit action id).
pub async fn authorize(bus: &Bus, peer: &Peer, action: &str) -> Result<(), Denial> {
    if peer.pidfd.is_none() || !peer.alive() {
        return Err(Denial::Peer);
    }
    if peer.uid != 0 {
        match seat_owner(bus).await {
            Ok(Some(uid)) if uid == peer.uid => {}
            Ok(owner) => {
                debug!(uid = peer.uid, ?owner, "seat check failed");
                return Err(Denial::Seat);
            }
            Err(e) => {
                warn!("seat check: cannot ask logind: {e}");
                bus.reset().await;
                return Err(Denial::Seat);
            }
        }
    }
    match check_polkit(bus, peer, action).await {
        Ok(PolkitAnswer::Yes) => Ok(()),
        Ok(PolkitAnswer::Challenge) => Err(Denial::AuthRequired),
        Ok(PolkitAnswer::No) => Err(Denial::Polkit),
        Err(e) => {
            warn!("polkit: {e}");
            bus.reset().await;
            Err(Denial::Polkit)
        }
    }
}

/// The uid owning seat0's active session, if any.
pub async fn seat_owner(bus: &Bus) -> zbus::Result<Option<u32>> {
    let conn = bus.get().await?;
    let active: (String, OwnedObjectPath) = get_property(
        &conn,
        "/org/freedesktop/login1/seat/seat0",
        "org.freedesktop.login1.Seat",
        "ActiveSession",
    )
    .await?;
    if active.0.is_empty() || active.1.as_str() == "/" {
        return Ok(None);
    }
    let user: (u32, OwnedObjectPath) = get_property(
        &conn,
        active.1.as_str(),
        "org.freedesktop.login1.Session",
        "User",
    )
    .await?;
    Ok(Some(user.0))
}

async fn get_property<T>(
    conn: &zbus::Connection,
    path: &str,
    iface: &str,
    name: &str,
) -> zbus::Result<T>
where
    T: TryFrom<OwnedValue>,
    T::Error: Into<zbus::Error>,
{
    let reply = conn
        .call_method(
            Some(LOGIND),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(iface, name),
        )
        .await?;
    let value: OwnedValue = reply.body().deserialize()?;
    T::try_from(value).map_err(Into::into)
}

#[derive(Debug, PartialEq, Eq)]
pub enum PolkitAnswer {
    Yes,
    Challenge,
    No,
}

pub async fn check_polkit(bus: &Bus, peer: &Peer, action: &str) -> zbus::Result<PolkitAnswer> {
    let conn = bus.get().await?;
    let pidfd = peer
        .pidfd
        .as_ref()
        .ok_or_else(|| zbus::Error::Failure("no pidfd".into()))?;
    let mut details: HashMap<&str, Value<'_>> = HashMap::new();
    details.insert("pidfd", Value::Fd(Fd::from(pidfd.as_fd())));
    if let Some(pid) = peer.pid {
        details.insert("pid", Value::U32(pid));
    }
    if let Some(t) = peer.start_time() {
        details.insert("start-time", Value::U64(t));
    }
    details.insert("uid", Value::I32(i32::try_from(peer.uid).unwrap_or(-1)));
    let subject = ("unix-process", details);
    let no_details: HashMap<&str, &str> = HashMap::new();
    let reply = conn
        .call_method(
            Some(POLKIT),
            POLKIT_PATH,
            Some(POLKIT_IFACE),
            "CheckAuthorization",
            &(subject, action, no_details, 0u32, ""),
        )
        .await?;
    let (authorized, challenge, _details): (bool, bool, HashMap<String, String>) =
        reply.body().deserialize()?;
    Ok(if authorized {
        PolkitAnswer::Yes
    } else if challenge {
        PolkitAnswer::Challenge
    } else {
        PolkitAnswer::No
    })
}

/// Token buckets per uid for mutating requests.
pub struct RateLimit {
    per_minute: u32,
    burst: u32,
    buckets: Mutex<HashMap<u32, (f64, Instant)>>,
}

impl RateLimit {
    pub fn new(per_minute: u32, burst: u32) -> Self {
        Self {
            per_minute: per_minute.max(1),
            burst: burst.max(1),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Take a token for `uid`, or say how long until one is available.
    pub fn take(&self, uid: u32, now: Instant) -> Result<(), Duration> {
        let rate = f64::from(self.per_minute) / 60.0;
        let mut map = self.buckets.lock().unwrap();
        let (tokens, last) = map.entry(uid).or_insert((f64::from(self.burst), now));
        let elapsed = now.saturating_duration_since(*last).as_secs_f64();
        *tokens = (*tokens + elapsed * rate).min(f64::from(self.burst));
        *last = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - *tokens) / rate))
        }
    }
}

pub fn rate_limited(retry: Duration) -> ErrorObject {
    ErrorObject::with_data(
        ErrorCode::RateLimited,
        "too many requests",
        json!({ "retry_after_ms": u64::try_from(retry.as_millis()).unwrap_or(u64::MAX) }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket() {
        let rl = RateLimit::new(10, 3);
        let t0 = Instant::now();
        for _ in 0..3 {
            rl.take(1000, t0).unwrap();
        }
        let wait = rl.take(1000, t0).unwrap_err();
        assert!(wait <= Duration::from_secs(6) && wait > Duration::from_secs(5));
        // Another uid has its own bucket.
        rl.take(1001, t0).unwrap();
        // One token every 6 s.
        rl.take(1000, t0 + Duration::from_secs(6)).unwrap();
        assert!(rl.take(1000, t0 + Duration::from_secs(6)).is_err());
        // Never more than the burst.
        let later = t0 + Duration::from_secs(3600);
        for _ in 0..3 {
            rl.take(1000, later).unwrap();
        }
        assert!(rl.take(1000, later).is_err());
    }

    #[test]
    fn denial_errors() {
        let e = Denial::Seat.error("org.hinteros.system.ssh");
        assert_eq!(e.kind(), Some(ErrorCode::PermissionDenied));
        assert_eq!(e.data.as_ref().unwrap()["reason"], "seat");
        let e = Denial::AuthRequired.error("org.hinteros.system.ssh");
        assert_eq!(e.kind(), Some(ErrorCode::AuthenticationRequired));
        assert_eq!(e.data.unwrap()["action"], "org.hinteros.system.ssh");
    }
}
