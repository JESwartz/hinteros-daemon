//! Inactive profile sessions are frozen (`--freeze-inactive`).
//!
//! logind moves the device ACLs to the active session at every switch, but a file descriptor a
//! process opened before (`/dev/uinput`, a joystick) stays open. A game of a profile that is not
//! on screen could still type into the admin console (Ctrl+Alt+F2, where the admin types the
//! password for `sudo hinteros-system reset-pin`) or into another session. So the processes of
//! every profile account ([`crate::pam::accounts`]: the members of `hinteros` and the guest;
//! never the admin account, the greeter, root or a system user) whose logind user state is
//! `online` or `lingering` (it has sessions, none of them active: a text console, the picker,
//! another session is in front) are frozen with systemd's cgroup freezer: `FreezeUnit` on
//! `user-<uid>.slice` (its sessions and its user manager), once it has been in front since it
//! logged in and while none of its sessions is opening and its user manager is not starting
//! (a login waits for both: frozen there, it would hang; see [`track`]). They are thawed when
//! one of its sessions is active again, while it is `closing` (systemd refuses to stop a frozen
//! unit, so an ending session must run), and when it has no session.
//!
//! Every logind signal (seat0's `ActiveSession`, sessions and users coming and going) checks
//! at once, and a tick every [`TICK`] repairs whatever changed in between (also after a restart
//! of this service). On `PrepareForShutdown` and when the service stops, everything is thawed:
//! a stopped service must not leave a slice frozen (its next login could not start). The PAM
//! helper's `end-others` thaws the accounts it ends ([`crate::pam`]). Suspend is systemd's own
//! business: it freezes and thaws `user.slice`, which leaves a slice frozen here frozen.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use tracing::{debug, info, warn};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use crate::authz::Bus;
use crate::config::Paths;
use crate::state::State;
use crate::users::Users;

/// How often the slices are checked without a signal.
pub const TICK: Duration = Duration::from_secs(2);
/// A freeze or thaw that takes longer is given up (and tried again on the next check).
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

const LOGIND: &str = "org.freedesktop.login1";
const SYSTEMD: &str = "org.freedesktop.systemd1";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// Whether an account in this logind user state (`None`: logind does not know it) is frozen.
pub fn wants_frozen(user_state: Option<&str>) -> bool {
    matches!(user_state, Some("online" | "lingering"))
}

/// Remember which accounts have been in front since they logged in: only those are frozen. An
/// account that is `online` but was never `active` is still logging in (its user manager and
/// session start before the session is put in front; frozen there, the login would hang). On
/// the first check (a restart of the service) an `online` account counts as seen.
pub fn track(seen: &mut HashSet<u32>, uid: u32, user_state: Option<&str>, first: bool) {
    match user_state {
        Some("active") => {
            seen.insert(uid);
        }
        Some("online" | "lingering") => {
            if first {
                seen.insert(uid);
            }
        }
        _ => {
            seen.remove(&uid);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Freeze,
    Thaw,
}

/// What to do with a slice in systemd's `FreezerState` (`running`, `freezing`, `frozen`,
/// `thawing`, `freezing-by-parent`, `frozen-by-parent`). Frozen by its parent (`user.slice`
/// during suspend) counts as not frozen by us: a freeze makes it ours, so the thaw after resume
/// leaves it frozen; a slice we do not want frozen is left to the parent.
pub fn act(want_frozen: bool, freezer_state: &str) -> Option<Act> {
    let ours = matches!(freezer_state, "frozen" | "freezing");
    match (want_frozen, ours) {
        (true, false) => Some(Act::Freeze),
        (false, true) => Some(Act::Thaw),
        _ => None,
    }
}

/// The freezer task.
pub struct Freezer {
    inner: Arc<Inner>,
    task: tokio::task::JoinHandle<()>,
}

struct Inner {
    bus: Bus,
    users: Users,
    paths: Paths,
    /// Shutting down: thaw everything, freeze nothing.
    stopping: AtomicBool,
    /// The accounts that have been in front since they logged in ([`track`]).
    seen: Mutex<HashSet<u32>>,
    /// The first check is done.
    checked: AtomicBool,
}

impl Freezer {
    pub fn spawn(bus: Option<String>, users: Users, paths: Paths) -> Self {
        let inner = Arc::new(Inner {
            bus: Bus::new(bus),
            users,
            paths,
            stopping: AtomicBool::new(false),
            seen: Mutex::default(),
            checked: AtomicBool::new(false),
        });
        info!("freezing the profiles whose session is not the active one");
        let task = tokio::spawn(inner.clone().run());
        Self { inner, task }
    }

    /// The service stops: thaw every profile (a restarted service freezes again at once).
    pub async fn thaw_all(self) {
        self.inner.stopping.store(true, Ordering::SeqCst);
        self.task.abort();
        if tokio::time::timeout(2 * CALL_TIMEOUT, self.inner.check())
            .await
            .is_err()
        {
            warn!("thawing the profiles took too long");
        }
    }
}

impl Inner {
    async fn run(self: Arc<Self>) {
        let mut logged = false;
        loop {
            let signals = match self.signals().await {
                Ok(s) => Some(s),
                Err(e) => {
                    if !logged {
                        warn!("freezer: no logind signals ({e}); checking every {TICK:?}");
                        logged = true;
                    }
                    self.bus.reset().await;
                    None
                }
            };
            self.check().await;
            let Some(mut signals) = signals else {
                tokio::time::sleep(TICK).await;
                continue;
            };
            logged = false;
            let mut tick = tokio::time::interval(TICK);
            tick.reset();
            loop {
                tokio::select! {
                    msg = signals.next() => match msg {
                        Some(Ok(msg)) => {
                            self.signal(&msg);
                            self.check().await;
                        }
                        Some(Err(_)) => {}
                        None => break,
                    },
                    _ = tick.tick() => self.check().await,
                }
            }
            self.bus.reset().await;
        }
    }

    /// `PrepareForShutdown`: thaw everything, freeze nothing (until it is cancelled).
    fn signal(&self, msg: &zbus::Message) {
        let header = msg.header();
        if header
            .member()
            .is_some_and(|m| m.as_str() == "PrepareForShutdown")
            && let Ok(going) = msg.body().deserialize::<bool>()
        {
            info!(
                going,
                "shutdown: {}",
                if going {
                    "thawing every profile"
                } else {
                    "cancelled"
                }
            );
            self.stopping.store(going, Ordering::SeqCst);
        }
    }

    /// Every signal of logind (seats, sessions, users, shutdown).
    async fn signals(&self) -> zbus::Result<zbus::MessageStream> {
        let conn = self.bus.get().await?;
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(LOGIND)?
            .build();
        zbus::MessageStream::for_match_rule(rule, &conn, Some(64)).await
    }

    /// Freeze and thaw each profile's slice as its state wants.
    async fn check(&self) {
        let conn = match self.bus.get().await {
            Ok(c) => c,
            Err(e) => {
                debug!("freezer: no bus: {e}");
                return;
            }
        };
        let admin = State::load(&self.paths.state_json())
            .ok()
            .and_then(|(s, _)| s.settings.admin.map(|a| a.user));
        let stopping = self.stopping.load(Ordering::SeqCst);
        for (name, uid) in crate::pam::accounts(&self.users, admin.as_deref()) {
            let (user, user_state) = match user_state(&conn, uid).await {
                Ok(Some((path, state))) => (Some(path), Some(state)),
                Ok(None) => (None, None),
                Err(e) => {
                    debug!(user = %name, "freezer: cannot ask logind: {e}");
                    continue;
                }
            };
            let slice = slice(uid);
            let first = !self.checked.load(Ordering::SeqCst);
            let seen = {
                let mut seen = self.seen.lock().unwrap();
                track(&mut seen, uid, user_state.as_deref(), first);
                seen.contains(&uid)
            };
            let freezer_state = match unit_property(&conn, &slice, "FreezerState").await {
                Ok(Some(s)) => s,
                // Not loaded: nothing of it runs.
                Ok(None) => continue,
                Err(e) => {
                    debug!(%slice, "freezer: cannot ask systemd: {e}");
                    continue;
                }
            };
            let mut want = !stopping && seen && wants_frozen(user_state.as_deref());
            if want && let Some(user) = &user {
                want = !logging_in(&conn, uid, user).await;
            }
            let Some(action) = act(want, &freezer_state) else {
                continue;
            };
            let method = match action {
                Act::Freeze => "FreezeUnit",
                Act::Thaw => "ThawUnit",
            };
            match tokio::time::timeout(CALL_TIMEOUT, unit_call(&conn, method, &slice)).await {
                Ok(Ok(())) => match action {
                    Act::Freeze => {
                        info!(user = %name, uid, state = ?user_state, "frozen: its session is not the active one");
                    }
                    Act::Thaw => info!(user = %name, uid, state = ?user_state, "thawed"),
                },
                Ok(Err(e)) => warn!(user = %name, "{method} {slice}: {e}"),
                Err(_) => warn!(user = %name, "{method} {slice}: no answer"),
            }
        }
        self.checked.store(true, Ordering::SeqCst);
    }
}

pub fn slice(uid: u32) -> String {
    format!("user-{uid}.slice")
}

/// `FreezeUnit` / `ThawUnit` on the system manager.
async fn unit_call(conn: &zbus::Connection, method: &str, unit: &str) -> zbus::Result<()> {
    conn.call_method(
        Some(SYSTEMD),
        "/org/freedesktop/systemd1",
        Some("org.freedesktop.systemd1.Manager"),
        method,
        &(unit,),
    )
    .await?;
    Ok(())
}

/// Thaw `uid`'s slice; errors (not loaded, not frozen) are ignored. For `end-others`.
pub async fn thaw(conn: &zbus::Connection, uid: u32) {
    if let Err(e) = unit_call(conn, "ThawUnit", &slice(uid)).await {
        debug!(uid, "ThawUnit: {e}");
    }
}

/// logind's user object and `State` (`None`: logind does not know it, e.g. `NoSuchUser`).
async fn user_state(
    conn: &zbus::Connection,
    uid: u32,
) -> zbus::Result<Option<(OwnedObjectPath, String)>> {
    let path = match conn
        .call_method(
            Some(LOGIND),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "GetUser",
            &(uid,),
        )
        .await
    {
        Ok(reply) => reply.body().deserialize::<OwnedObjectPath>()?,
        Err(zbus::Error::MethodError(..)) => return Ok(None),
        Err(e) => return Err(e),
    };
    let state = property(conn, LOGIND, &path, "org.freedesktop.login1.User", "State").await?;
    Ok(Some((path, state)))
}

/// Whether the account is still logging in: a session `opening`, or its user manager
/// (`user@<uid>.service`) starting. A login waits for both; frozen, it would never finish.
async fn logging_in(conn: &zbus::Connection, uid: u32, user: &OwnedObjectPath) -> bool {
    let manager = unit_property(conn, &format!("user@{uid}.service"), "ActiveState").await;
    if manager.is_ok_and(|m| m.as_deref() == Some("activating")) {
        return true;
    }
    let sessions: Vec<(String, OwnedObjectPath)> = match conn
        .call_method(
            Some(LOGIND),
            user.as_str(),
            Some(PROPERTIES),
            "Get",
            &("org.freedesktop.login1.User", "Sessions"),
        )
        .await
        .and_then(|r| r.body().deserialize::<OwnedValue>())
        .and_then(|v| Vec::try_from(v).map_err(Into::into))
    {
        Ok(s) => s,
        Err(e) => {
            debug!(uid, "freezer: no sessions: {e}");
            return false;
        }
    };
    for (_, path) in sessions {
        let state = property(
            conn,
            LOGIND,
            &path,
            "org.freedesktop.login1.Session",
            "State",
        )
        .await;
        if state.as_deref() == Ok("opening") {
            return true;
        }
    }
    false
}

/// A property of a loaded systemd unit (`None`: not loaded).
async fn unit_property(
    conn: &zbus::Connection,
    unit: &str,
    name: &str,
) -> zbus::Result<Option<String>> {
    let path = match conn
        .call_method(
            Some(SYSTEMD),
            "/org/freedesktop/systemd1",
            Some("org.freedesktop.systemd1.Manager"),
            "GetUnit",
            &(unit,),
        )
        .await
    {
        Ok(reply) => reply.body().deserialize::<OwnedObjectPath>()?,
        Err(zbus::Error::MethodError(..)) => return Ok(None),
        Err(e) => return Err(e),
    };
    property(conn, SYSTEMD, &path, "org.freedesktop.systemd1.Unit", name)
        .await
        .map(Some)
}

async fn property(
    conn: &zbus::Connection,
    dest: &str,
    path: &OwnedObjectPath,
    iface: &str,
    name: &str,
) -> zbus::Result<String> {
    let reply = conn
        .call_method(
            Some(dest),
            path.as_str(),
            Some(PROPERTIES),
            "Get",
            &(iface, name),
        )
        .await?;
    let value: OwnedValue = reply.body().deserialize()?;
    String::try_from(value).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_profile_with_sessions_none_active_is_frozen() {
        assert!(wants_frozen(Some("online")));
        assert!(wants_frozen(Some("lingering")));
        // Its session is in front; ending (systemd cannot stop a frozen unit); gone.
        for s in [Some("active"), Some("closing"), Some("offline"), None] {
            assert!(!wants_frozen(s), "{s:?}");
        }
    }

    #[test]
    fn only_an_account_that_was_in_front_is_frozen() {
        let mut seen = HashSet::new();
        // Logging in: online before it is ever active.
        track(&mut seen, 7, Some("online"), false);
        assert!(!seen.contains(&7));
        track(&mut seen, 7, Some("active"), false);
        track(&mut seen, 7, Some("online"), false);
        assert!(seen.contains(&7));
        // Ended: forgotten, the next login starts over.
        track(&mut seen, 7, Some("closing"), false);
        assert!(!seen.contains(&7));
        track(&mut seen, 7, None, false);
        track(&mut seen, 7, Some("online"), false);
        assert!(!seen.contains(&7));
        // A restart of the service: an online account was in front before.
        track(&mut seen, 8, Some("online"), true);
        assert!(seen.contains(&8));
    }

    #[test]
    fn freeze_and_thaw_only_when_needed() {
        assert_eq!(act(true, "running"), Some(Act::Freeze));
        assert_eq!(act(true, "thawing"), Some(Act::Freeze));
        assert_eq!(act(true, "frozen"), None);
        assert_eq!(act(true, "freezing"), None);
        // Frozen only by user.slice (suspend): make it ours, so the resume leaves it frozen.
        assert_eq!(act(true, "frozen-by-parent"), Some(Act::Freeze));
        assert_eq!(act(false, "frozen"), Some(Act::Thaw));
        assert_eq!(act(false, "freezing"), Some(Act::Thaw));
        assert_eq!(act(false, "running"), None);
        assert_eq!(act(false, "thawing"), None);
        // The parent's freeze is the parent's to undo.
        assert_eq!(act(false, "frozen-by-parent"), None);
        assert_eq!(act(false, "freezing-by-parent"), None);
    }
}
