//! The session's side of profiles (profiles.md "Switching profiles on a TV"):
//! `profile.current`, the profile switch (close every app without a silent force-close, close
//! Steam, end the session), the sleep lock of a locked profile, and `profiles_changed`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hinteros_api::server::{decode_params, encode_result};
use hinteros_api::session::{
    AppRef, EventName, LockStateParams, ProfileCurrent, ProfileCurrentResult, ProfileEmpty,
    ProfilesChangedParams, SessionEndGuest, SessionLockState, SessionSwitchCancel,
    SessionSwitchForceClose, SessionSwitchProfile, SessionSwitchWait, SessionUnlock,
    SwitchBlockedParams, SwitchProgressParams, SwitchStage,
};
use hinteros_api::system::{self, ProfilesListResult};
use hinteros_api::{ErrorCode, ErrorObject, Method};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{info, warn};

use crate::shell::Msg;
use crate::system::Relay;
use crate::{Audience, Broadcast};

/// The session methods this module answers.
pub const METHODS: &[&str] = &[
    ProfileCurrent::NAME,
    SessionSwitchProfile::NAME,
    SessionSwitchWait::NAME,
    SessionSwitchForceClose::NAME,
    SessionSwitchCancel::NAME,
    SessionLockState::NAME,
    SessionUnlock::NAME,
    SessionEndGuest::NAME,
];

/// How often a running switch renews its hint (the service keeps it 60 s).
const RENEW_EVERY: Duration = Duration::from_secs(25);

/// How long End guest session retries `system.guest.end` while another job runs.
const GUEST_END_BUSY_WAIT: Duration = Duration::from_secs(15);
/// How long the session waits for the guest_end job to end it before ending it itself.
const GUEST_END_GRACE: Duration = Duration::from_secs(20);

/// What a switch waits for.
#[derive(Debug, Clone)]
pub struct SwitchConfig {
    pub wait: Duration,
    pub steam_wait: Duration,
    /// The Steam binary (`steam -shutdown`), if Steam is installed.
    pub steam: Option<PathBuf>,
    /// D-Bus address for logind (`None`: the system bus).
    pub bus: Option<String>,
    /// Where processes are looked up (Steam's).
    pub proc_dir: PathBuf,
    /// End the logind session at the end (off in tests).
    pub end_session: bool,
}

enum Ctl {
    Wait,
    ForceClose(String),
    Cancel,
}

struct Inner {
    relay: Relay,
    shell: mpsc::UnboundedSender<Msg>,
    events: broadcast::Sender<Broadcast>,
    cfg: SwitchConfig,
    locked: AtomicBool,
    switch: Mutex<Option<mpsc::UnboundedSender<Ctl>>>,
}

/// Handle; cheap to clone.
#[derive(Clone)]
pub struct Profiles {
    inner: Arc<Inner>,
}

fn invalid(field: &str, reason: &str, message: &str) -> ErrorObject {
    ErrorObject::invalid_field(field, reason, message)
}

impl Profiles {
    pub fn spawn(
        relay: Relay,
        shell: mpsc::UnboundedSender<Msg>,
        events: broadcast::Sender<Broadcast>,
        cfg: SwitchConfig,
    ) -> Self {
        let p = Self {
            inner: Arc::new(Inner {
                relay,
                shell,
                events: events.clone(),
                cfg,
                locked: AtomicBool::new(false),
                switch: Mutex::new(None),
            }),
        };
        // The system's profiles_changed carries nothing: re-query and send the list.
        let me = p.clone();
        let mut rx = events.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok((EventName::ProfilesChanged, _, Audience::Internal)) => {
                        me.announce_profiles().await;
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        p
    }

    fn send<T: serde::Serialize>(&self, name: EventName, params: T) {
        let _ = self.inner.events.send((
            name,
            serde_json::to_value(params).unwrap_or_default(),
            Audience::All,
        ));
    }

    async fn list(&self) -> Result<ProfilesListResult, ErrorObject> {
        let v = self
            .inner
            .relay
            .call(system::ProfilesList::NAME, None)
            .await?;
        serde_json::from_value(v)
            .map_err(|e| ErrorObject::new(ErrorCode::InternalError, e.to_string()))
    }

    async fn announce_profiles(&self) {
        match self.list().await {
            Ok(l) => self.send(
                EventName::ProfilesChanged,
                ProfilesChangedParams {
                    profiles: l.profiles,
                },
            ),
            Err(e) => warn!("profiles changed, but cannot list them: {}", e.message),
        }
    }

    pub async fn call(&self, method: &str, params: Option<Value>) -> Result<Value, ErrorObject> {
        match method {
            ProfileCurrent::NAME => {
                decode_params::<ProfileCurrent>(params)?;
                encode_result::<ProfileCurrent>(self.current().await?)
            }
            SessionSwitchProfile::NAME => {
                let p = decode_params::<SessionSwitchProfile>(params)?;
                let pin = p.pin.map(zeroize::Zeroizing::new);
                self.switch(Some(p.id), pin, false).await?;
                encode_result::<SessionSwitchProfile>(ProfileEmpty {})
            }
            SessionEndGuest::NAME => {
                decode_params::<SessionEndGuest>(params)?;
                let cur = self.current().await?;
                if !cur.guest {
                    return Err(ErrorObject::with_data(
                        ErrorCode::PermissionDenied,
                        "only the guest can end the guest session",
                        json!({"action": "org.hinteros.system.profiles", "reason": "profile"}),
                    ));
                }
                // Close everything like a switch, then the system service ends this session and
                // wipes the guest now (system.guest.end).
                self.switch(None, None, true).await?;
                encode_result::<SessionEndGuest>(ProfileEmpty {})
            }
            SessionSwitchWait::NAME => {
                decode_params::<SessionSwitchWait>(params)?;
                self.control(Ctl::Wait)?;
                encode_result::<SessionSwitchWait>(ProfileEmpty {})
            }
            SessionSwitchForceClose::NAME => {
                let p = decode_params::<SessionSwitchForceClose>(params)?;
                self.control(Ctl::ForceClose(p.app))?;
                encode_result::<SessionSwitchForceClose>(ProfileEmpty {})
            }
            SessionSwitchCancel::NAME => {
                decode_params::<SessionSwitchCancel>(params)?;
                self.control(Ctl::Cancel)?;
                encode_result::<SessionSwitchCancel>(ProfileEmpty {})
            }
            SessionLockState::NAME => {
                decode_params::<SessionLockState>(params)?;
                encode_result::<SessionLockState>(LockStateParams {
                    locked: self.inner.locked.load(Ordering::SeqCst),
                })
            }
            SessionUnlock::NAME => {
                let p = decode_params::<SessionUnlock>(params)?;
                self.inner
                    .relay
                    .call(system::ProfilesUnlock::NAME, Some(json!({ "pin": p.pin })))
                    .await?;
                if self.inner.locked.swap(false, Ordering::SeqCst) {
                    info!("unlocked");
                    self.send(EventName::LockChanged, LockStateParams { locked: false });
                }
                encode_result::<SessionUnlock>(ProfileEmpty {})
            }
            m => Err(hinteros_api::server::method_not_found(m)),
        }
    }

    async fn current(&self) -> Result<ProfileCurrentResult, ErrorObject> {
        let l = self.list().await?;
        let me = l
            .caller
            .id
            .as_ref()
            .and_then(|id| l.profiles.iter().find(|p| &p.id == id));
        Ok(ProfileCurrentResult {
            id: l.caller.id.clone(),
            name: me.map(|p| p.name.clone()),
            color: me.map(|p| p.color.clone()),
            admin: me.is_some_and(|p| p.admin),
            locked: me.is_some_and(|p| p.locked),
            guest: l.caller.guest,
            setup_required: l.caller.spare && l.first_boot,
            pin_set: me.is_some_and(|p| p.pin_set),
            screen_locked: self.inner.locked.load(Ordering::SeqCst),
        })
    }

    fn control(&self, c: Ctl) -> Result<(), ErrorObject> {
        let guard = self.inner.switch.lock().unwrap();
        match guard.as_ref() {
            Some(tx) if tx.send(c).is_ok() => Ok(()),
            _ => Err(invalid("switch", "no_switch", "no switch is running")),
        }
    }

    /// Start a switch to `target` (`None`: to the picker without a target). A locked target's
    /// PIN goes to `system.profiles.switch` first: wrong or missing, the error comes back and
    /// nothing closes; right, the hint carries a one-time token (DECISIONS J1).
    async fn switch(
        &self,
        target: Option<String>,
        pin: Option<zeroize::Zeroizing<String>>,
        end_guest: bool,
    ) -> Result<(), ErrorObject> {
        // Take the slot before the first await: two requests at once must not both start one.
        let (tx, rx) = mpsc::unbounded_channel();
        {
            let mut slot = self.inner.switch.lock().unwrap();
            if slot.is_some() {
                return Err(invalid("id", "busy", "a switch is already running"));
            }
            *slot = Some(tx);
        }
        if let Some(id) = &target
            && let Err(e) = self
                .check_target(id, pin.as_ref().map(|p| p.as_str()))
                .await
        {
            *self.inner.switch.lock().unwrap() = None;
            return Err(e);
        }
        let me = self.clone();
        tokio::spawn(async move {
            let ended = me.run_switch(rx, target).await;
            *me.inner.switch.lock().unwrap() = None;
            if ended && end_guest {
                me.end_guest().await;
            } else if ended {
                me.end_session().await;
            }
        });
        Ok(())
    }

    /// Before a switch to `id`: not this profile, and `system.profiles.switch` takes it (and
    /// `pin`, for a locked one).
    async fn check_target(&self, id: &str, pin: Option<&str>) -> Result<(), ErrorObject> {
        let cur = self.current().await?;
        if cur.id.as_deref() == Some(id) || (cur.guest && id == "guest") {
            return Err(invalid("id", "same", "that is this profile"));
        }
        let mut params = json!({ "id": id });
        if let Some(pin) = pin {
            params["pin"] = json!(pin);
        }
        self.inner
            .relay
            .call(system::ProfilesSwitch::NAME, Some(params))
            .await?;
        Ok(())
    }

    async fn shell<T>(&self, make: impl FnOnce(oneshot::Sender<T>) -> Msg) -> Option<T> {
        let (tx, rx) = oneshot::channel();
        self.inner.shell.send(make(tx)).ok()?;
        rx.await.ok()
    }

    async fn running(&self) -> Vec<AppRef> {
        self.shell(|reply| Msg::RunningApps { reply })
            .await
            .unwrap_or_default()
    }

    /// Steam's own processes of this user (the client, not its games).
    fn steam_pids(&self) -> Vec<i32> {
        let uid = rustix::process::getuid().as_raw();
        let Ok(entries) = std::fs::read_dir(&self.inner.cfg.proc_dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| {
                let pid: i32 = e.file_name().to_str()?.parse().ok()?;
                let status = std::fs::read_to_string(e.path().join("status")).ok()?;
                let name = status
                    .lines()
                    .next()?
                    .strip_prefix("Name:")?
                    .trim()
                    .to_owned();
                let owner: u32 = status
                    .lines()
                    .find(|l| l.starts_with("Uid:"))?
                    .split_whitespace()
                    .nth(1)?
                    .parse()
                    .ok()?;
                (owner == uid && name == "steam").then_some(pid)
            })
            .collect()
    }

    /// Renew the switch hint (and a locked target's token) while the switch runs, so it is still
    /// live when the session ends; the service renews a token only for the caller that got it.
    async fn renew_hint(&self, target: &str) {
        if let Err(e) = self
            .inner
            .relay
            .call(system::ProfilesSwitch::NAME, Some(json!({ "id": target })))
            .await
        {
            warn!("cannot renew the switch hint: {}", e.message);
        }
    }

    /// Close everything. `true`: done, end the session; `false`: cancelled.
    async fn run_switch(
        &self,
        mut ctl: mpsc::UnboundedReceiver<Ctl>,
        target: Option<String>,
    ) -> bool {
        let mut renewed = Instant::now();
        let cfg = self.inner.cfg.clone();
        self.send(
            EventName::SwitchProgress,
            SwitchProgressParams {
                stage: SwitchStage::ClosingApps,
                app: None,
            },
        );
        let _ = self.inner.shell.send(Msg::Home);
        let _ = self.shell(|reply| Msg::CloseAll { reply }).await;
        let steam_running = !self.steam_pids().is_empty();
        if steam_running && let Some(steam) = &cfg.steam {
            self.send(
                EventName::SwitchProgress,
                SwitchProgressParams {
                    stage: SwitchStage::ClosingSteam,
                    app: Some(AppRef {
                        id: "steam".into(),
                        name: "Steam".into(),
                    }),
                },
            );
            match tokio::process::Command::new(steam)
                .arg("-shutdown")
                .stdin(std::process::Stdio::null())
                .spawn()
            {
                Ok(mut c) => {
                    tokio::spawn(async move {
                        let _ = c.wait().await;
                    });
                }
                Err(e) => warn!("cannot ask Steam to shut down: {e}"),
            }
        }
        let started = Instant::now();
        let mut deadline = started + cfg.wait;
        let mut steam_deadline = started + cfg.steam_wait;
        let mut reported: Vec<String> = Vec::new();
        loop {
            let apps = self.running().await;
            let steam = self.steam_pids();
            if apps.is_empty() && steam.is_empty() {
                break;
            }
            if let Some(t) = &target
                && renewed.elapsed() >= RENEW_EVERY
            {
                renewed = Instant::now();
                self.renew_hint(t).await;
            }
            let now = Instant::now();
            let mut blocked: Vec<AppRef> = Vec::new();
            if now >= deadline {
                blocked.extend(apps.iter().cloned());
            }
            if !steam.is_empty() && now >= steam_deadline {
                blocked.push(AppRef {
                    id: "steam".into(),
                    name: "Steam".into(),
                });
            }
            for app in &blocked {
                if !reported.contains(&app.id) {
                    reported.push(app.id.clone());
                    self.send(
                        EventName::SwitchBlocked,
                        SwitchBlockedParams {
                            app: app.clone(),
                            waited_s: now.duration_since(started).as_secs() as u32,
                        },
                    );
                }
            }
            tokio::select! {
                c = ctl.recv() => match c {
                    Some(Ctl::Wait) => {
                        let now = Instant::now();
                        deadline = now + cfg.wait;
                        steam_deadline = now + cfg.steam_wait;
                        reported.clear();
                    }
                    Some(Ctl::ForceClose(id)) => {
                        if id == "steam" {
                            for pid in self.steam_pids() {
                                if let Some(pid) = rustix::process::Pid::from_raw(pid) {
                                    let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
                                }
                            }
                        } else {
                            let _ = self.inner.shell.send(Msg::ForceClose { id: id.clone() });
                        }
                        info!(app = %id, "force-closed for the switch");
                        reported.retain(|r| r != &id);
                    }
                    Some(Ctl::Cancel) | None => {
                        info!("switch cancelled");
                        // Apps that did not quit yet stay; they can be closed again.
                        let _ = self.inner.shell.send(Msg::CancelCloseAll);
                        if target.is_some()
                            && let Err(e) = self
                                .inner
                                .relay
                                .call(system::ProfilesSwitchCancel::NAME, Some(json!({})))
                                .await
                        {
                            warn!("cannot drop the switch hint: {}", e.message);
                        }
                        self.send(EventName::SwitchProgress, SwitchProgressParams { stage: SwitchStage::Cancelled, app: None });
                        return false;
                    }
                },
                () = tokio::time::sleep(Duration::from_millis(250)) => {}
            }
        }
        // The greeter reads the hint right after this session ends: make sure it is fresh.
        if let Some(t) = &target
            && renewed.elapsed() >= Duration::from_secs(5)
        {
            self.renew_hint(t).await;
        }
        self.send(
            EventName::SwitchProgress,
            SwitchProgressParams {
                stage: SwitchStage::Ending,
                app: None,
            },
        );
        true
    }

    /// Hide the on-screen keyboard and end this logind session; greetd's PAM (`end-others`)
    /// removes whatever is left of this profile before the next session opens.
    async fn end_session(&self) {
        let _ = self
            .shell(|reply| Msg::Request {
                method: hinteros_api::session::OskHide::NAME.into(),
                params: None,
                reply,
            })
            .await;
        if !self.inner.cfg.end_session {
            info!("not ending the session (tests)");
            let _ = self.inner.shell.send(Msg::CancelCloseAll);
            return;
        }
        match crate::restart::terminate_own_session(self.inner.cfg.bus.as_deref()).await {
            Ok(()) => info!("session ended for the switch"),
            Err(e) => {
                warn!("cannot end the session: {e}");
                // Still here: apps may start again.
                let _ = self.inner.shell.send(Msg::CancelCloseAll);
            }
        }
    }

    /// End guest session, once every app is closed: `system.guest.end` ends this session and
    /// wipes the guest now (a job outside this session). While another job runs it is retried
    /// for a while; if it cannot start, or this session is still there long after it started,
    /// the session ends as on a switch and the guest is wiped at the next boot instead.
    async fn end_guest(&self) {
        let _ = self
            .shell(|reply| Msg::Request {
                method: hinteros_api::session::OskHide::NAME.into(),
                params: None,
                reply,
            })
            .await;
        let give_up = Instant::now() + GUEST_END_BUSY_WAIT;
        loop {
            match self
                .inner
                .relay
                .call(system::GuestEnd::NAME, Some(json!({})))
                .await
            {
                Ok(v) => {
                    info!(job = %v["job_id"], "the guest is being wiped; the job ends this session");
                    if !self.inner.cfg.end_session {
                        let _ = self.inner.shell.send(Msg::CancelCloseAll);
                        return;
                    }
                    tokio::time::sleep(GUEST_END_GRACE).await;
                    warn!("still running after guest.end; ending the session");
                    break;
                }
                Err(e) if e.kind() == Some(ErrorCode::JobBusy) && Instant::now() < give_up => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Err(e) => {
                    warn!(
                        "cannot wipe the guest now ({}); it is wiped at the next restart",
                        e.message
                    );
                    break;
                }
            }
        }
        self.end_session().await;
    }

    /// Before the box sleeps: a locked profile's launcher comes to the front in lock mode.
    pub fn sleeping(&self, locked_profile: bool) {
        if locked_profile && !self.inner.locked.swap(true, Ordering::SeqCst) {
            info!("locking the profile for sleep");
            let _ = self.inner.shell.send(Msg::Home);
            self.send(EventName::LockChanged, LockStateParams { locked: true });
        }
    }

    /// Watch logind's PrepareForSleep with a delay inhibitor: the lock is up before the box
    /// sleeps.
    pub fn spawn_sleep_watch(&self) {
        let me = self.clone();
        tokio::spawn(async move {
            if let Err(e) = me.sleep_watch().await {
                warn!("no sleep lock: {e}");
            }
        });
    }

    async fn sleep_watch(&self) -> zbus::Result<()> {
        use futures_util::StreamExt;
        let conn = match &self.inner.cfg.bus {
            Some(a) => {
                zbus::connection::Builder::address(a.as_str())?
                    .build()
                    .await?
            }
            None => zbus::Connection::system().await?,
        };
        const LOGIND: &str = "org.freedesktop.login1";
        // Only logind's: any client on the bus can send a signal by that name.
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(LOGIND)?
            .path("/org/freedesktop/login1")?
            .interface("org.freedesktop.login1.Manager")?
            .member("PrepareForSleep")?
            .build();
        let mut stream = zbus::MessageStream::for_match_rule(rule, &conn, None).await?;
        let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
        let inhibit = |conn: zbus::Connection| async move {
            let reply = conn
                .call_method(
                    Some(LOGIND),
                    "/org/freedesktop/login1",
                    Some("org.freedesktop.login1.Manager"),
                    "Inhibit",
                    &("sleep", "HinterOS", "Lock the profile", "delay"),
                )
                .await?;
            let fd: zbus::zvariant::OwnedFd = reply.body().deserialize()?;
            Ok::<_, zbus::Error>(fd)
        };
        let mut held = inhibit(conn.clone()).await.ok();
        while let Some(msg) = stream.next().await {
            let Ok(msg) = msg else { continue };
            // The bus checks the rule's sender too; zbus cannot match a well-known name itself.
            let owner = dbus
                .get_name_owner(zbus::names::BusName::try_from(LOGIND)?)
                .await
                .ok();
            let sender = msg.header().sender().map(|s| s.as_str().to_owned());
            if owner.is_none_or(|o| sender.as_deref() != Some(o.as_str())) {
                warn!("PrepareForSleep not from logind; ignored");
                continue;
            }
            let Ok(going) = msg.body().deserialize::<bool>() else {
                continue;
            };
            if going {
                let locked = self.current().await.is_ok_and(|c| c.locked && c.pin_set);
                self.sleeping(locked);
                // Give the launcher a moment to draw the lock, then let the box sleep.
                tokio::time::sleep(Duration::from_millis(300)).await;
                held = None;
            } else if held.is_none() {
                held = inhibit(conn.clone()).await.ok();
            }
        }
        drop(held);
        Ok(())
    }
}
