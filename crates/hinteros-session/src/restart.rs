//! `session.restart`: restart the TV session after an update replaced the session daemon or the
//! system service (`job_finished {session_restart_required}`).
//!
//! Mechanism: the system repo runs greetd with `default_session = initial_session` (the
//! compositor and launcher, as the session user), so greetd starts the session again as soon as
//! it ends. The daemon (a systemd user service, outside that session) asks logind to
//! **terminate the user's graphical session on seat0** (`Session.Terminate`; logind lets a user
//! end their own sessions without polkit), which ends gamescope, the launcher and everything in
//! the session scope; greetd then starts a fresh one with the new generation's programs. Then
//! it asks the user's systemd to **restart its own unit** (`RestartUnit`), so the new session
//! talks to the new daemon (apps the daemon started end with its cgroup).
//!
//! Why not something else: killing gamescope or the launcher by pid leaves logind's session,
//! Steam and games in place; restarting greetd or `loginctl terminate-user` need root or end
//! every user service; `systemctl --user restart` alone would keep the old launcher and
//! compositor. Safety: only a session whose PAM service is greetd's (`restart_services`), on
//! seat0, owned by us, is ever terminated, so a desktop preview can never end a developer's
//! desktop session. The caller gets its answer before anything is stopped.

use std::time::Duration;

use hinteros_api::ErrorObject;
use hinteros_api::server::encode_result;
use hinteros_api::session::{Service, SessionRestart, SessionRestartResult};
use serde_json::Value;
use tracing::{info, warn};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use crate::config::SessionConfig;
use crate::runtime::dbus;

const LOGIND: &str = "org.freedesktop.login1";
const SESSION: &str = "org.freedesktop.login1.Session";

#[derive(Clone)]
pub struct Restarter {
    config: SessionConfig,
    /// logind's bus (the runtime settings' bus) [default: the system bus].
    bus: Option<String>,
}

fn unavailable(reason: &str, message: impl Into<String>) -> ErrorObject {
    ErrorObject::with_data(
        hinteros_api::ErrorCode::Unavailable,
        message,
        serde_json::json!({ "service": Service::Logind.name(), "reason": reason }),
    )
}

async fn get<T>(conn: &zbus::Connection, path: &str, iface: &str, name: &str) -> zbus::Result<T>
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
    let v: OwnedValue = reply.body().deserialize()?;
    T::try_from(v).map_err(Into::into)
}

impl Restarter {
    pub fn new(config: SessionConfig, bus: Option<String>) -> Self {
        Self { config, bus }
    }

    /// The session to end: the user's display session, else seat0's active one if it is ours.
    async fn find_session(&self, conn: &zbus::Connection) -> Result<OwnedObjectPath, ErrorObject> {
        let uid = rustix::process::getuid().as_raw();
        let logind_err = |e: zbus::Error| unavailable("logind", format!("logind: {e}"));
        let user_path = format!("/org/freedesktop/login1/user/_{uid}");
        let display: Option<(String, OwnedObjectPath)> =
            get(conn, &user_path, "org.freedesktop.login1.User", "Display")
                .await
                .ok();
        let path = match display {
            Some((id, path)) if !id.is_empty() && path.as_str() != "/" => path,
            _ => {
                let (id, path): (String, OwnedObjectPath) = get(
                    conn,
                    "/org/freedesktop/login1/seat/seat0",
                    "org.freedesktop.login1.Seat",
                    "ActiveSession",
                )
                .await
                .map_err(logind_err)?;
                if id.is_empty() || path.as_str() == "/" {
                    return Err(unavailable("no_session", "no graphical session on seat0"));
                }
                path
            }
        };
        let (owner, _): (u32, OwnedObjectPath) = get(conn, path.as_str(), SESSION, "User")
            .await
            .map_err(logind_err)?;
        let (seat, _): (String, OwnedObjectPath) = get(conn, path.as_str(), SESSION, "Seat")
            .await
            .map_err(logind_err)?;
        let service: String = get(conn, path.as_str(), SESSION, "Service")
            .await
            .map_err(logind_err)?;
        if owner != uid || seat != "seat0" {
            return Err(unavailable(
                "no_session",
                "the graphical session on seat0 is not this user's",
            ));
        }
        if !self.config.restart_services.contains(&service) {
            return Err(unavailable(
                "not_greetd",
                "the graphical session was not started by greetd",
            ));
        }
        Ok(path)
    }

    /// `session.restart`: check, answer, then end the session and restart this daemon.
    pub async fn restart(&self, _params: Option<Value>) -> Result<Value, ErrorObject> {
        let conn = dbus::connect(self.bus.as_deref())
            .await
            .map_err(|e| unavailable("logind", format!("no D-Bus connection ({e})")))?;
        let session = tokio::time::timeout(Duration::from_secs(10), self.find_session(&conn))
            .await
            .map_err(|_| unavailable("timeout", "logind did not answer"))??;
        let unit = self.config.unit.clone().or_else(own_unit);
        let user_bus = self.config.user_bus.clone();
        info!(session = %session.as_str(), ?unit, "restarting the session");
        tokio::spawn(async move {
            // Let the answer reach the launcher first.
            tokio::time::sleep(Duration::from_millis(300)).await;
            match conn
                .call_method(
                    Some(LOGIND),
                    session.as_str(),
                    Some(SESSION),
                    "Terminate",
                    &(),
                )
                .await
            {
                Ok(_) => info!("logind is ending the session; greetd starts it again"),
                Err(e) => {
                    warn!("cannot end the session: {e}");
                    return;
                }
            }
            let Some(unit) = unit else {
                info!("not running as a systemd user unit; not restarting the daemon");
                return;
            };
            if let Err(e) = restart_unit(user_bus.as_deref(), &unit).await {
                warn!(%unit, "cannot restart the session daemon: {e}");
            }
        });
        encode_result::<SessionRestart>(SessionRestartResult {})
    }
}

/// End this user's graphical session on seat0 (a profile switch): greetd then starts the
/// greeter.
pub async fn terminate_own_session(bus: Option<&str>) -> Result<(), String> {
    let r = Restarter::new(SessionConfig::default(), bus.map(str::to_owned));
    let conn = dbus::connect(bus).await.map_err(|e| e.to_string())?;
    let session = r.find_session(&conn).await.map_err(|e| e.message)?;
    conn.call_method(
        Some(LOGIND),
        session.as_str(),
        Some(SESSION),
        "Terminate",
        &(),
    )
    .await
    .map(|_| ())
    .map_err(|e| e.to_string())
}

async fn restart_unit(bus: Option<&str>, unit: &str) -> zbus::Result<()> {
    let conn = match bus {
        Some(a) => zbus::connection::Builder::address(a)?.build().await?,
        None => zbus::Connection::session().await?,
    };
    info!(%unit, "restarting the session daemon's unit");
    conn.call_method(
        Some("org.freedesktop.systemd1"),
        "/org/freedesktop/systemd1",
        Some("org.freedesktop.systemd1.Manager"),
        "RestartUnit",
        &(unit, "replace"),
    )
    .await?;
    Ok(())
}

/// Our systemd user unit, from `/proc/self/cgroup`.
fn own_unit() -> Option<String> {
    unit_from_cgroup(&std::fs::read_to_string("/proc/self/cgroup").ok()?)
}

/// `0::/user.slice/user-1000.slice/user@1000.service/app.slice/hinteros-session.service` →
/// `hinteros-session.service` (only units of a user manager).
pub fn unit_from_cgroup(text: &str) -> Option<String> {
    let path = text.lines().find_map(|l| l.strip_prefix("0::"))?;
    let (_, below) = path.split_once("/user@")?;
    let unit = below.rsplit('/').find(|p| p.ends_with(".service"))?;
    (unit != below.split('/').next()?).then(|| unit.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_from_the_cgroup() {
        assert_eq!(
            unit_from_cgroup(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/hinteros-session.service\n"
            )
            .as_deref(),
            Some("hinteros-session.service")
        );
        // A login session scope, a system service, the user manager itself: none.
        for other in [
            "0::/user.slice/user-1000.slice/session-2.scope\n",
            "0::/system.slice/hinteros-system.service\n",
            "0::/user.slice/user-1000.slice/user@1000.service/init.scope\n",
            "",
        ] {
            assert_eq!(unit_from_cgroup(other), None, "{other}");
        }
    }
}
