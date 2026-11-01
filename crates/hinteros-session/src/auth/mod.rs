//! polkit authentication agent: how the admin password gets from the TV to polkit.
//!
//! The daemon registers an agent with polkit (`RegisterAuthenticationAgent`) whose subject is
//! **this process** (`unix-process` with our pidfd, pid, start time and uid), not a logind
//! session: the daemon is a systemd user service outside the greetd session, and a
//! process-scoped agent is exactly what is needed, because the system service checks
//! `org.hinteros.system.ssh` with the session daemon (the relay's peer) as the subject and
//! `AllowUserInteraction`. polkit then calls our `BeginAuthentication`; we tell the launcher
//! (`authentication_requested`, only to connections named `hinteros-launcher`) and wait for
//! `auth.respond {cookie, password}` (or `auth.cancel`, polkit's `CancelAuthentication`, or the
//! prompt timeout). The password goes to polkit's helper ([`helper`]), which verifies it with
//! PAM and reports to polkit; `BeginAuthentication` then returns and polkit answers the system
//! service. Up to `max_attempts` wrong passwords per request.
//!
//! Safety: only polkit (the owner of `org.freedesktop.PolicyKit1`) may call the agent; only
//! the launcher may answer; passwords are never logged (`Debug` hides them), live in
//! `Zeroizing` buffers and are wiped after use; every wait has a time limit.

pub mod helper;

use std::collections::HashMap;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use hinteros_api::server::encode_result;
use hinteros_api::session::{
    AUTH_PASSWORD_MAX_BYTES, AuthCancel, AuthCancelParams, AuthCancelResult, AuthIdentity,
    AuthOutcome, AuthRespond, AuthRespondResult, AuthenticationFinishedParams,
    AuthenticationRequestedParams, EventName, LAUNCHER_CLIENT_NAME, Service, is_auth_cookie,
};
use hinteros_api::{ErrorCode, ErrorObject};
use serde_json::Value;
use tokio::sync::{broadcast, oneshot};
use tracing::{debug, info, warn};
use zbus::message::Header;
use zbus::zvariant::{Fd, OwnedValue, Value as ZValue};
use zeroize::Zeroizing;

use crate::config::AuthConfig;
use crate::{Audience, Broadcast};

const POLKIT: &str = "org.freedesktop.PolicyKit1";
const AUTHORITY_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";
const AUTHORITY: &str = "org.freedesktop.PolicyKit1.Authority";
/// Where our agent object lives on polkit's bus.
pub const AGENT_PATH: &str = "/org/hinteros/PolkitAgent";
/// At most this many requests wait at once.
const MAX_PENDING: usize = 4;
/// The helper gets this long per attempt (PAM delays a failure by a few seconds).
const HELPER_TIMEOUT: Duration = Duration::from_secs(30);

struct Pending {
    action: String,
    /// The identity we authenticate as (the first of the event's `identities`).
    user: AuthIdentity,
    tries: AtomicU32,
    done: Mutex<Option<oneshot::Sender<AuthOutcome>>>,
    /// One helper run at a time per request.
    busy: tokio::sync::Mutex<()>,
}

struct Inner {
    config: AuthConfig,
    events: broadcast::Sender<Broadcast>,
    pending: Mutex<HashMap<String, Arc<Pending>>>,
}

/// Handle to the agent; cheap to clone.
#[derive(Clone)]
pub struct Auth {
    inner: Arc<Inner>,
}

impl Auth {
    /// Create the agent state; [`Auth::spawn`] also registers it with polkit.
    pub fn new(config: AuthConfig, events: broadcast::Sender<Broadcast>) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                events,
                pending: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Register the agent with polkit, and again whenever polkit restarts.
    pub fn spawn(config: AuthConfig, events: broadcast::Sender<Broadcast>) -> Self {
        let auth = Self::new(config, events);
        if auth.inner.config.enable {
            tokio::spawn(register_loop(auth.clone()));
        } else {
            info!("polkit authentication agent off");
        }
        auth
    }

    fn max_attempts(&self) -> u32 {
        self.inner.config.max_attempts.max(1)
    }

    fn emit<T: serde::Serialize>(&self, name: EventName, params: &T) {
        if let Ok(v) = serde_json::to_value(params) {
            let _ = self.inner.events.send((name, v, Audience::LauncherClient));
        }
    }

    fn get(&self, cookie: &str) -> Option<Arc<Pending>> {
        self.inner.pending.lock().unwrap().get(cookie).cloned()
    }

    /// End a request: tell `BeginAuthentication` and the launcher.
    fn finish(&self, cookie: &str, outcome: AuthOutcome) -> bool {
        let Some(p) = self.inner.pending.lock().unwrap().remove(cookie) else {
            return false;
        };
        if let Some(tx) = p.done.lock().unwrap().take() {
            let _ = tx.send(outcome);
        }
        info!(action = %p.action, ?outcome, "authentication request over");
        self.emit(
            EventName::AuthenticationFinished,
            &AuthenticationFinishedParams {
                cookie: cookie.to_owned(),
                outcome,
            },
        );
        true
    }

    /// polkit's `BeginAuthentication`: wait for the user. `Ok`: authenticated.
    pub async fn begin(
        &self,
        action: String,
        message: String,
        cookie: String,
        identities: Vec<AuthIdentity>,
    ) -> Result<(), AgentError> {
        if !is_auth_cookie(&cookie) {
            return Err(AgentError::Failed("unusable cookie".into()));
        }
        let identities = order_identities(identities);
        let Some(user) = identities.first().cloned() else {
            return Err(AgentError::Failed("no identity to authenticate".into()));
        };
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.inner.pending.lock().unwrap();
            if pending.len() >= MAX_PENDING || pending.contains_key(&cookie) {
                return Err(AgentError::Failed(
                    "too many authentication requests".into(),
                ));
            }
            pending.insert(
                cookie.clone(),
                Arc::new(Pending {
                    action: action.clone(),
                    user: user.clone(),
                    tries: AtomicU32::new(0),
                    done: Mutex::new(Some(tx)),
                    busy: tokio::sync::Mutex::new(()),
                }),
            );
        }
        info!(%action, user = %user.name, "polkit asks for authentication");
        self.emit(
            EventName::AuthenticationRequested,
            &AuthenticationRequestedParams {
                cookie: cookie.clone(),
                action: action.chars().take(256).collect(),
                message: message.chars().take(1024).collect(),
                identities,
            },
        );
        let limit = Duration::from_secs(self.inner.config.prompt_timeout_s.max(1));
        let outcome = match tokio::time::timeout(limit, rx).await {
            Ok(Ok(o)) => o,
            Ok(Err(_)) => AuthOutcome::Cancelled,
            Err(_) => {
                info!(%action, "nobody answered the authentication request in time");
                self.finish(&cookie, AuthOutcome::Cancelled);
                AuthOutcome::Cancelled
            }
        };
        match outcome {
            AuthOutcome::Authorized => Ok(()),
            AuthOutcome::Cancelled => Err(AgentError::Cancelled("dismissed".into())),
            AuthOutcome::Failed | AuthOutcome::Unknown => {
                Err(AgentError::Failed("not authenticated".into()))
            }
        }
    }

    /// polkit's `CancelAuthentication`.
    pub fn cancel_from_polkit(&self, cookie: &str) {
        if self.finish(cookie, AuthOutcome::Cancelled) {
            debug!("polkit cancelled an authentication request");
        }
    }

    /// `auth.respond`.
    pub async fn respond(
        &self,
        client: Option<&str>,
        params: Option<Value>,
    ) -> Result<Value, ErrorObject> {
        only_launcher(client)?;
        let p = take_respond_params(params)?;
        let cookie = p.cookie.clone();
        let password = p.password;
        if password.len() > AUTH_PASSWORD_MAX_BYTES || password.contains(['\n', '\r', '\0']) {
            return Err(ErrorObject::invalid_field(
                "password",
                "bad_format",
                format!("at most {AUTH_PASSWORD_MAX_BYTES} bytes, no line breaks"),
            ));
        }
        let pending = self.get(&cookie).ok_or_else(|| no_request(&cookie))?;
        let _busy = pending.busy.lock().await;
        if self.get(&cookie).is_none() {
            return Err(no_request(&cookie));
        }
        let cfg = &self.inner.config;
        let verdict = helper::authenticate(
            &cfg.helper,
            &cfg.helper_socket,
            &pending.user.name,
            &cookie,
            &password,
            HELPER_TIMEOUT,
        )
        .await;
        drop(password);
        let max = self.max_attempts();
        let result = match verdict {
            Ok(true) => {
                let used = pending.tries.load(Ordering::SeqCst) + 1;
                self.finish(&cookie, AuthOutcome::Authorized);
                AuthRespondResult {
                    ok: true,
                    attempts_left: max.saturating_sub(used),
                }
            }
            Ok(false) => {
                let used = pending.tries.fetch_add(1, Ordering::SeqCst) + 1;
                let left = max.saturating_sub(used);
                info!(action = %pending.action, attempts_left = left, "wrong password");
                if left == 0 {
                    self.finish(&cookie, AuthOutcome::Failed);
                }
                AuthRespondResult {
                    ok: false,
                    attempts_left: left,
                }
            }
            Err(e) => {
                warn!("{e}");
                return Err(ErrorObject::with_data(
                    ErrorCode::Unavailable,
                    format!("polkit cannot check the password: {e}"),
                    serde_json::json!({
                        "service": Service::Polkit.name(),
                        "reason": e.to_string().chars().take(300).collect::<String>(),
                    }),
                ));
            }
        };
        encode_result::<AuthRespond>(result)
    }

    /// `auth.cancel`.
    pub fn cancel(
        &self,
        client: Option<&str>,
        params: Option<Value>,
    ) -> Result<Value, ErrorObject> {
        only_launcher(client)?;
        let p: AuthCancelParams = hinteros_api::server::decode_params::<AuthCancel>(params)?;
        if !self.finish(&p.cookie, AuthOutcome::Cancelled) {
            return Err(no_request(&p.cookie));
        }
        encode_result::<AuthCancel>(AuthCancelResult {})
    }
}

fn only_launcher(client: Option<&str>) -> Result<(), ErrorObject> {
    if client == Some(LAUNCHER_CLIENT_NAME) {
        Ok(())
    } else {
        Err(ErrorObject::new(
            ErrorCode::NotAuthorized,
            "only the launcher answers authentication requests",
        ))
    }
}

fn no_request(cookie: &str) -> ErrorObject {
    let shown: String = cookie.chars().take(128).collect();
    ErrorObject::new(
        ErrorCode::NotFound,
        format!("no authentication request {shown}"),
    )
}

/// Decode `auth.respond` without leaving copies of the password in a `serde_json::Value`: the
/// string is moved out of the params into a zeroizing buffer.
fn take_respond_params(params: Option<Value>) -> Result<RespondParams, ErrorObject> {
    let Some(Value::Object(mut map)) = params else {
        return Err(ErrorObject::new(
            ErrorCode::InvalidParams,
            "params must be an object",
        ));
    };
    let password = match map.remove("password") {
        Some(Value::String(s)) => Zeroizing::new(s),
        _ => {
            return Err(ErrorObject::invalid_field(
                "password",
                "required",
                "password is required",
            ));
        }
    };
    let cookie = match map.remove("cookie") {
        Some(Value::String(s)) if is_auth_cookie(&s) => s,
        _ => {
            return Err(ErrorObject::invalid_field(
                "cookie",
                "bad_format",
                "cookie is required",
            ));
        }
    };
    if let Some(k) = map.keys().next() {
        return Err(ErrorObject::invalid_field(
            &k.chars().take(64).collect::<String>(),
            "unknown_field",
            "unknown field",
        ));
    }
    Ok(RespondParams { cookie, password })
}

struct RespondParams {
    cookie: String,
    password: Zeroizing<String>,
}

/// The admin account first: a non-root user if polkit offers one, root last.
fn order_identities(mut ids: Vec<AuthIdentity>) -> Vec<AuthIdentity> {
    ids.sort_by_key(|i| i.uid == 0);
    ids
}

/// polkit's identities (`a(sa{sv})`): the `unix-user` ones, with their names.
pub fn identities(raw: &[(String, HashMap<String, OwnedValue>)]) -> Vec<AuthIdentity> {
    raw.iter()
        .filter(|(kind, _)| kind == "unix-user")
        .filter_map(|(_, d)| {
            let uid = d.get("uid").and_then(|v| u32::try_from(v).ok())?;
            let name = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
                .ok()
                .flatten()?
                .name;
            Some(AuthIdentity { uid, name })
        })
        .collect()
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.PolicyKit1.Error")]
pub enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Failed(String),
    Cancelled(String),
    NotAuthorized(String),
}

struct Agent {
    auth: Auth,
}

/// Whether `header`'s sender is polkit (the current owner of its well-known name).
async fn from_polkit(conn: &zbus::Connection, header: &Header<'_>) -> bool {
    let Some(sender) = header.sender() else {
        return false;
    };
    let Ok(dbus) = zbus::fdo::DBusProxy::new(conn).await else {
        return false;
    };
    let Ok(name) = zbus::names::BusName::try_from(POLKIT) else {
        return false;
    };
    match dbus.get_name_owner(name).await {
        Ok(owner) => owner.as_str() == sender.as_str(),
        Err(_) => false,
    }
}

#[zbus::interface(name = "org.freedesktop.PolicyKit1.AuthenticationAgent")]
impl Agent {
    #[allow(clippy::too_many_arguments)]
    async fn begin_authentication(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
        action_id: String,
        message: String,
        _icon_name: String,
        _details: HashMap<String, String>,
        cookie: String,
        identities: Vec<(String, HashMap<String, OwnedValue>)>,
    ) -> Result<(), AgentError> {
        if !from_polkit(conn, &header).await {
            warn!(sender = ?header.sender(), "BeginAuthentication not from polkit; refused");
            return Err(AgentError::NotAuthorized(
                "only polkit may call the agent".into(),
            ));
        }
        let ids = self::identities(&identities);
        self.auth.begin(action_id, message, cookie, ids).await
    }

    async fn cancel_authentication(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
        cookie: String,
    ) -> Result<(), AgentError> {
        if !from_polkit(conn, &header).await {
            return Err(AgentError::NotAuthorized(
                "only polkit may call the agent".into(),
            ));
        }
        self.auth.cancel_from_polkit(&cookie);
        Ok(())
    }
}

/// This process's start time (`/proc/self/stat` field 22), as polkit computes it.
fn own_start_time() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(22 - 3)?
        .parse()
        .ok()
}

/// `RegisterAuthenticationAgent` for this process.
async fn register(conn: &zbus::Connection) -> zbus::Result<()> {
    let pid = rustix::process::getpid();
    let pidfd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())
        .map_err(|e| zbus::Error::Failure(format!("pidfd_open: {e}")))?;
    let mut subject: HashMap<&str, ZValue<'_>> = HashMap::new();
    subject.insert("pidfd", ZValue::Fd(Fd::from(pidfd.as_fd())));
    subject.insert(
        "pid",
        ZValue::U32(pid.as_raw_nonzero().get().unsigned_abs()),
    );
    if let Some(t) = own_start_time() {
        subject.insert("start-time", ZValue::U64(t));
    }
    let uid = rustix::process::getuid().as_raw();
    subject.insert("uid", ZValue::I32(i32::try_from(uid).unwrap_or(-1)));
    let locale = std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".into());
    tokio::time::timeout(
        Duration::from_secs(25),
        conn.call_method(
            Some(POLKIT),
            AUTHORITY_PATH,
            Some(AUTHORITY),
            "RegisterAuthenticationAgent",
            &(("unix-process", subject), locale.as_str(), AGENT_PATH),
        ),
    )
    .await
    .map_err(|_| zbus::Error::Failure("polkit did not answer".into()))??;
    Ok(())
}

/// polkit's current unique bus name, if it runs.
async fn polkit_owner(dbus: &zbus::fdo::DBusProxy<'_>) -> Option<String> {
    let name = zbus::names::BusName::try_from(POLKIT).ok()?;
    dbus.get_name_owner(name).await.ok().map(|o| o.to_string())
}

/// Whether polkit appearing as `new_owner` means registering again: not if that is the
/// instance the agent is already registered with.
fn needs_registering(registered_with: Option<&str>, new_owner: &str) -> bool {
    registered_with != Some(new_owner)
}

/// polkit's answer when this process already has an agent ("An authentication agent already
/// exists for the given subject"): nothing to do.
fn already_registered(e: &zbus::Error) -> bool {
    e.to_string().contains("already exists")
}

async fn register_loop(auth: Auth) {
    let mut backoff = Duration::from_secs(2);
    let mut logged = false;
    loop {
        match serve(&auth).await {
            Ok(()) => {
                backoff = Duration::from_secs(2);
                logged = false;
            }
            Err(e) => {
                if !logged {
                    warn!(
                        "polkit authentication agent unavailable ({e}); actions that need the \
                         admin password answer authentication_required until it works"
                    );
                    logged = true;
                }
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// Connect, export the agent, register, and stay registered (again after each polkit
/// restart) until the connection fails.
async fn serve(auth: &Auth) -> zbus::Result<()> {
    let conn = match &auth.inner.config.bus {
        Some(a) => {
            zbus::connection::Builder::address(a.as_str())?
                .build()
                .await?
        }
        None => zbus::Connection::system().await?,
    };
    conn.object_server()
        .at(AGENT_PATH, Agent { auth: auth.clone() })
        .await?;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
    let mut owners = dbus
        .receive_name_owner_changed_with_args(&[(0, POLKIT)])
        .await?;
    match register(&conn).await {
        Err(e) if already_registered(&e) => debug!("polkit already has our authentication agent"),
        r => r?,
    }
    // Which polkit we registered with: our first call usually starts it (bus activation),
    // and the NameOwnerChanged for that start arrives afterwards. Only a different owner is
    // a restart that forgot us.
    let mut registered_with = polkit_owner(&dbus).await;
    info!(polkit = ?registered_with, "registered the polkit authentication agent for this process");
    while let Some(change) = owners.next().await {
        let Ok(args) = change.args() else { continue };
        let Some(owner) = args.new_owner().as_ref().map(ToString::to_string) else {
            continue;
        };
        if !needs_registering(registered_with.as_deref(), &owner) {
            debug!(polkit = %owner, "polkit started; already registered with it");
            continue;
        }
        info!(polkit = %owner, "polkit restarted; registering the authentication agent again");
        match register(&conn).await {
            Ok(()) => registered_with = Some(owner),
            Err(e) if already_registered(&e) => {
                debug!("polkit already has our authentication agent");
                registered_with = Some(owner);
            }
            Err(e) => warn!("registering the polkit authentication agent: {e}"),
        }
    }
    Err(zbus::Error::Failure("the bus connection ended".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_again_only_with_a_new_polkit() {
        // The NameOwnerChanged of the start our own registration caused.
        assert!(!needs_registering(Some(":1.5"), ":1.5"));
        // A restart.
        assert!(needs_registering(Some(":1.5"), ":1.9"));
        // We could not tell whom we registered with.
        assert!(needs_registering(None, ":1.9"));
        let dup = zbus::Error::Failure(
            "org.freedesktop.PolicyKit1.Error.Failed: An authentication agent already exists \
             for the given subject"
                .into(),
        );
        assert!(already_registered(&dup));
        assert!(!already_registered(&zbus::Error::Failure("denied".into())));
    }

    #[test]
    fn admin_before_root() {
        let ids = order_identities(vec![
            AuthIdentity {
                uid: 0,
                name: "root".into(),
            },
            AuthIdentity {
                uid: 1001,
                name: "admin".into(),
            },
        ]);
        assert_eq!(ids[0].name, "admin");
    }

    #[test]
    fn respond_params_are_closed_and_checked() {
        let p = take_respond_params(Some(
            serde_json::json!({"cookie": "1-a-1-b", "password": "x"}),
        ))
        .unwrap();
        assert_eq!((p.cookie.as_str(), p.password.as_str()), ("1-a-1-b", "x"));
        for bad in [
            serde_json::json!({"cookie": "1-a-1-b"}),
            serde_json::json!({"cookie": "a b", "password": "x"}),
            serde_json::json!({"cookie": "1-a-1-b", "password": "x", "user": "root"}),
            serde_json::json!([1]),
        ] {
            assert_eq!(
                take_respond_params(Some(bad)).err().unwrap().kind(),
                Some(ErrorCode::InvalidParams)
            );
        }
        assert_eq!(
            only_launcher(Some("test")).unwrap_err().kind(),
            Some(ErrorCode::NotAuthorized)
        );
        assert!(only_launcher(Some(LAUNCHER_CLIENT_NAME)).is_ok());
        assert!(own_start_time().is_some());
    }
}
