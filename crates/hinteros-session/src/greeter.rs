//! The picker (profiles.md "The pieces"): `hinteros-session greet` (the greeter session's
//! command: the fast path into a profile without any UI, else exec the picker) and the
//! greeter-mode daemon's `greeter.*` methods, which wrap greetd's IPC and the system service's
//! `greeter.sock` (profile creation).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hinteros_api::client::Client;
use hinteros_api::common::Peer;
use hinteros_api::server::{decode_params, encode_result};
use hinteros_api::session::{
    EventName, GreeterAppearance, GreeterCancel, GreeterCreate, GreeterList, GreeterListResult,
    GreeterLogin, GreeterPin, GreeterProfile, GreeterPromptParams, GreeterResultParams,
    ProfileEmpty,
};
use hinteros_api::{ErrorCode, ErrorObject, Method, system};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tracing::{info, warn};
use zeroize::Zeroizing;

use crate::greetd::{
    AuthMessageType, Greetd, Request, Response, Secret, locked_s, reboot_required,
};
use crate::{Audience, Broadcast};

pub const METHODS: &[&str] = &[
    GreeterList::NAME,
    GreeterLogin::NAME,
    GreeterPin::NAME,
    GreeterCancel::NAME,
    GreeterCreate::NAME,
];

/// The `profiles-public.json` version this reads. A newer file is read as far as this
/// version knows it.
const PUBLIC_VERSION: u32 = 1;

/// The service's `profiles-public.json` (only what the greeter reads; a copy of
/// `hinteros-system`'s `public::PublicView`). Tolerant: unknown fields are ignored, and a
/// field or profile that does not parse is left out (logged), so a change on the service's
/// side never empties the picker.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PublicView {
    #[serde(default, deserialize_with = "lenient_list")]
    pub profiles: Vec<PublicProfile>,
    #[serde(default, deserialize_with = "lenient")]
    pub guest: PublicGuest,
    #[serde(default, deserialize_with = "lenient")]
    pub first_boot: bool,
    #[serde(default, deserialize_with = "lenient")]
    pub spare: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub last_used: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub next: Option<Next>,
    #[serde(default, deserialize_with = "lenient")]
    pub appearance: Option<Appearance>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PublicProfile {
    pub id: String,
    pub user: String,
    pub name: String,
    #[serde(default, deserialize_with = "lenient")]
    pub color: String,
    /// Not known: locked (the picker asks for the PIN; PAM decides anyway).
    #[serde(default = "yes")]
    pub locked: bool,
    #[serde(default)]
    pub admin: bool,
    #[serde(default, deserialize_with = "lenient")]
    pub locked_until: Option<i64>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PublicGuest {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, deserialize_with = "lenient")]
    pub user: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Next {
    pub target: String,
    #[serde(default)]
    pub fast: bool,
    pub expires: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Appearance {
    pub text_scale: f64,
    pub high_contrast: bool,
    pub reduce_motion: bool,
}

/// A value that does not parse is left at its default, not the whole file.
fn lenient<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let value = Value::deserialize(d)?;
    Ok(serde_json::from_value(value).unwrap_or_else(|e| {
        warn!("profiles-public.json: a field is ignored: {e}");
        T::default()
    }))
}

/// A list whose entries that do not parse are left out.
fn lenient_list<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let Value::Array(items) = Value::deserialize(d)? else {
        warn!("profiles-public.json: a list is not a list; ignored");
        return Ok(Vec::new());
    };
    Ok(items
        .into_iter()
        .filter_map(|item| {
            serde_json::from_value(item)
                .inspect_err(|e| warn!("profiles-public.json: an entry is ignored: {e}"))
                .ok()
        })
        .collect())
}

impl PublicView {
    /// Read the file. Missing: nothing to show yet (the service writes it when it starts).
    pub fn load(path: &Path) -> Self {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                warn!(path = %path.display(), "cannot read the public profile list: {e}");
                return Self::default();
            }
        };
        if let Err(e) = hinteros_api::common::check_version(path, &bytes, PUBLIC_VERSION) {
            warn!("{e}; reading the fields this version knows");
        }
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            warn!(path = %path.display(), "cannot parse the public profile list: {e}");
            Self::default()
        })
    }

    /// The user of a profile id or `guest`.
    pub fn user_of(&self, id: &str) -> Option<String> {
        if id == "guest" {
            return self
                .guest
                .enabled
                .then(|| self.guest.user.clone().unwrap_or("hinteros-guest".into()));
        }
        self.profiles
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.user.clone())
    }

    /// The user the greeter may enter without any UI: first-boot mode's spare, or a live
    /// switch hint the service marked fast.
    pub fn fast_path(&self, now: i64) -> Option<String> {
        if self.first_boot {
            return self.spare.clone();
        }
        let next = self.next.as_ref().filter(|n| n.fast && n.expires > now)?;
        self.user_of(&next.target)
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// How a greetd conversation ended (so far).
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// greetd asks for a secret: the PIN.
    Prompt,
    /// The session starts.
    Started,
    /// Refused (wrong PIN, a lock, an account that may not enter). `reboot`: the admin
    /// profile's PIN is refused until the box restarts.
    Failed { locked_s: Option<u32>, reboot: bool },
}

/// Drive a conversation from `resp` until it needs the PIN or ends; starts `cmd` on success.
pub fn advance(conn: &mut Greetd, mut resp: Response, cmd: &[String]) -> std::io::Result<Outcome> {
    let mut locked = None;
    let mut reboot = false;
    loop {
        resp = match resp {
            Response::AuthMessage {
                auth_message_type: AuthMessageType::Info | AuthMessageType::Error,
                auth_message,
            } => {
                locked = locked.or(locked_s(&auth_message));
                reboot |= reboot_required(&auth_message);
                conn.request(&Request::PostAuthMessageResponse { response: None })?
            }
            Response::AuthMessage { .. } => return Ok(Outcome::Prompt),
            Response::Success => {
                match conn.request(&Request::StartSession {
                    cmd: cmd.to_vec(),
                    env: Vec::new(),
                })? {
                    Response::Success => return Ok(Outcome::Started),
                    other => {
                        warn!("greetd refused to start the session: {other:?}");
                        let _ = conn.request(&Request::CancelSession);
                        return Ok(Outcome::Failed {
                            locked_s: locked,
                            reboot,
                        });
                    }
                }
            }
            Response::Error { description, .. } => {
                info!("greetd: {description}");
                let _ = conn.request(&Request::CancelSession);
                return Ok(Outcome::Failed {
                    locked_s: locked,
                    reboot,
                });
            }
        };
    }
}

/// `hinteros-session greet [--config FILE] -- <picker command…>`: the fast path, else exec the
/// picker.
pub fn greet_main(args: &[String]) -> i32 {
    let (opts, picker) = match args.iter().position(|a| a == "--") {
        Some(i) => (&args[..i], &args[i + 1..]),
        None => (args, &[][..]),
    };
    let mut config_path: Option<PathBuf> = None;
    let mut it = opts.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" => config_path = it.next().map(PathBuf::from),
            other => {
                eprintln!("greet: unknown argument {other:?}");
                return 2;
            }
        }
    }
    let cli = crate::config::Cli {
        config: config_path,
        greeter: true,
        ..Default::default()
    };
    let config = match crate::config::Config::load(&cli) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("greet: {e:#}");
            crate::config::Config::default()
        }
    };
    // No compositor and no launcher before this decision: a switch the hint lets through goes
    // from the old session to the new one with one display mode set (the new session's).
    let view = PublicView::load(&config.greeter.public);
    let now = now();
    match view.fast_path(now) {
        Some(user) => match fast(&user, &config.greeter.session_command) {
            Ok(true) => {
                info!(%user, "fast path: the session starts, no picker");
                return 0;
            }
            Ok(false) => info!(%user, "fast path refused by PAM; the picker asks"),
            Err(e) => warn!(%user, "fast path failed: {e}"),
        },
        None => match &view.next {
            Some(n) if n.expires > now => {
                info!(target = %n.target, "switch hint without a token: the picker asks")
            }
            _ => info!("no switch hint: the picker"),
        },
    }
    exec_picker(picker)
}

fn fast(user: &str, cmd: &[String]) -> std::io::Result<bool> {
    let mut conn = Greetd::from_env()?;
    let resp = conn.request(&Request::CreateSession {
        username: user.into(),
    })?;
    match advance(&mut conn, resp, cmd)? {
        Outcome::Started => Ok(true),
        Outcome::Prompt => {
            let _ = conn.request(&Request::CancelSession);
            Ok(false)
        }
        Outcome::Failed { .. } => Ok(false),
    }
}

fn exec_picker(picker: &[String]) -> i32 {
    let Some((program, rest)) = picker.split_first() else {
        eprintln!("greet: no picker command after --");
        return 2;
    };
    use std::os::unix::process::CommandExt;
    let e = std::process::Command::new(program).args(rest).exec();
    eprintln!("greet: cannot run {program}: {e}");
    1
}

/// The greeter-mode daemon's `greeter.*`.
#[derive(Clone)]
pub struct Greeter {
    inner: Arc<Inner>,
}

struct Inner {
    public: PathBuf,
    socket: PathBuf,
    session_command: Vec<String>,
    events: broadcast::Sender<Broadcast>,
    conn: Mutex<Option<Greetd>>,
}

fn unavailable(reason: &str) -> ErrorObject {
    ErrorObject::with_data(
        ErrorCode::Unavailable,
        format!("greetd: {reason}"),
        json!({"service": "logind", "reason": reason}),
    )
}

impl Greeter {
    pub fn new(
        config: &crate::config::GreeterSection,
        events: broadcast::Sender<Broadcast>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                public: config.public.clone(),
                socket: config.socket.clone(),
                session_command: config.session_command.clone(),
                events,
                conn: Mutex::new(None),
            }),
        }
    }

    fn send<T: serde::Serialize>(&self, name: EventName, params: T) {
        let _ = self.inner.events.send((
            name,
            serde_json::to_value(params).unwrap_or_default(),
            Audience::LauncherClient,
        ));
    }

    fn report(&self, outcome: Outcome) {
        match outcome {
            Outcome::Prompt => self.send(
                EventName::GreeterPrompt,
                GreeterPromptParams { kind: "pin".into() },
            ),
            Outcome::Started => {
                *self.inner.conn.lock().unwrap() = None;
                self.send(
                    EventName::GreeterResult,
                    GreeterResultParams {
                        ok: true,
                        locked_s: None,
                        reboot_required: false,
                    },
                );
            }
            Outcome::Failed { locked_s, reboot } => {
                *self.inner.conn.lock().unwrap() = None;
                self.send(
                    EventName::GreeterResult,
                    GreeterResultParams {
                        ok: false,
                        locked_s,
                        reboot_required: reboot,
                    },
                );
            }
        }
    }

    /// Run a greetd step on a blocking thread with the conversation.
    async fn step(
        &self,
        f: impl FnOnce(&mut Option<Greetd>, &[String]) -> std::io::Result<Outcome> + Send + 'static,
    ) -> Result<(), ErrorObject> {
        let inner = self.inner.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let mut guard = inner.conn.lock().unwrap();
            f(&mut guard, &inner.session_command)
        })
        .await
        .map_err(|e| unavailable(&e.to_string()))?
        .map_err(|e| unavailable(&e.to_string()))?;
        self.report(outcome);
        Ok(())
    }

    pub async fn call(
        &self,
        method: &str,
        params: Option<Value>,
        client: Option<&str>,
    ) -> Result<Value, ErrorObject> {
        // The name the client gave in its handshake: not a security boundary (see `auth`);
        // greetd and PAM check the PIN.
        if client != Some(hinteros_api::session::LAUNCHER_CLIENT_NAME) {
            return Err(ErrorObject::new(
                ErrorCode::NotAuthorized,
                "only the launcher may use the greeter",
            ));
        }
        match method {
            GreeterList::NAME => {
                decode_params::<GreeterList>(params)?;
                encode_result::<GreeterList>(self.list())
            }
            GreeterLogin::NAME => {
                let p = decode_params::<GreeterLogin>(params)?;
                let view = PublicView::load(&self.inner.public);
                let user = view
                    .user_of(&p.id)
                    .ok_or_else(|| ErrorObject::new(ErrorCode::NotFound, "no such profile"))?;
                self.step(move |conn, cmd| {
                    if let Some(mut old) = conn.take() {
                        let _ = old.request(&Request::CancelSession);
                    }
                    let mut c = Greetd::from_env()?;
                    let resp = c.request(&Request::CreateSession { username: user })?;
                    let outcome = advance(&mut c, resp, cmd)?;
                    if outcome == Outcome::Prompt {
                        *conn = Some(c);
                    }
                    Ok(outcome)
                })
                .await?;
                encode_result::<GreeterLogin>(ProfileEmpty {})
            }
            GreeterPin::NAME => {
                let p = decode_params::<GreeterPin>(params)?;
                let pin = Zeroizing::new(p.pin);
                self.step(move |conn, cmd| {
                    let Some(c) = conn.as_mut() else {
                        return Ok(Outcome::Failed {
                            locked_s: None,
                            reboot: false,
                        });
                    };
                    let outcome = c
                        .request(&Request::PostAuthMessageResponse {
                            response: Some(Secret::new(pin)),
                        })
                        .and_then(|resp| advance(c, resp, cmd));
                    // An error (a timeout, a short read) leaves the conversation out of step
                    // with greetd: drop it too. greetd cancels a session whose connection
                    // closes.
                    if !matches!(outcome, Ok(Outcome::Prompt)) {
                        *conn = None;
                    }
                    outcome
                })
                .await?;
                encode_result::<GreeterPin>(ProfileEmpty {})
            }
            GreeterCancel::NAME => {
                decode_params::<GreeterCancel>(params)?;
                let inner = self.inner.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if let Some(mut c) = inner.conn.lock().unwrap().take() {
                        let _ = c.request(&Request::CancelSession);
                    }
                })
                .await;
                encode_result::<GreeterCancel>(ProfileEmpty {})
            }
            GreeterCreate::NAME => {
                let p = decode_params::<GreeterCreate>(params)?;
                self.create(p).await
            }
            m => Err(hinteros_api::server::method_not_found(m)),
        }
    }

    fn list(&self) -> GreeterListResult {
        let v = PublicView::load(&self.inner.public);
        let switch_to = v
            .next
            .as_ref()
            .filter(|n| n.expires > now())
            .map(|n| n.target.clone());
        let focus = switch_to.clone().or(v.last_used.clone());
        let a = v.appearance.clone().unwrap_or(Appearance {
            text_scale: 1.0,
            high_contrast: false,
            reduce_motion: false,
        });
        GreeterListResult {
            profiles: v
                .profiles
                .iter()
                .map(|p| GreeterProfile {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    color: p.color.clone(),
                    locked: p.locked,
                    admin: p.admin,
                    locked_until: p.locked_until.filter(|t| *t > now()),
                })
                .collect(),
            guest: v.guest.enabled,
            first_boot: v.first_boot,
            focus,
            switch_to,
            appearance: GreeterAppearance {
                text_scale: a.text_scale,
                high_contrast: a.high_contrast,
                reduce_motion: a.reduce_motion,
            },
        }
    }

    /// `profiles.create` over greeter.sock; its job events are forwarded.
    async fn create(
        &self,
        p: hinteros_api::session::GreeterCreateParams,
    ) -> Result<Value, ErrorObject> {
        let down = |e: &dyn std::fmt::Display| {
            ErrorObject::with_data(
                ErrorCode::SystemUnavailable,
                format!("the system service: {e}"),
                json!({"reason": "down"}),
            )
        };
        let mut c = Client::connect(&self.inner.socket)
            .await
            .map_err(|e| down(&e))?;
        c.hello(
            system::VERSION,
            Peer::new(crate::server::NAME, crate::server::VERSION),
        )
        .await
        .map_err(|e| down(&e))?;
        let rpc = |e: hinteros_api::client::ClientError| match e {
            hinteros_api::client::ClientError::Rpc(e) => e,
            other => down(&other),
        };
        c.call_raw(
            system::Subscribe::NAME,
            json!({"events": ["job_started", "job_progress", "job_finished"]}),
        )
        .await
        .map_err(rpc)?;
        let settings = c
            .call_raw(system::SettingsGet::NAME, json!({}))
            .await
            .map_err(rpc)?;
        let mut params = json!({
            "name": p.name,
            "pin": p.pin,
            "locked": p.locked,
            "revision": settings["revision"],
        });
        if let Some(color) = p.color {
            params["color"] = json!(color);
        }
        let result = c
            .call_raw(system::ProfilesCreate::NAME, params)
            .await
            .map_err(rpc)?;
        let job = result["job_id"].as_str().unwrap_or_default().to_owned();
        let events = self.inner.events.clone();
        tokio::spawn(async move {
            while let Ok((name, params)) = c.next_event_raw().await {
                let ev = match name.as_str() {
                    "job_started" => EventName::JobStarted,
                    "job_progress" => EventName::JobProgress,
                    "job_finished" => EventName::JobFinished,
                    _ => continue,
                };
                let done = ev == EventName::JobFinished && params["job_id"] == json!(job);
                let _ = events.send((ev, params, Audience::All));
                if done {
                    break;
                }
            }
        });
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_path_choices() {
        let v: PublicView = serde_json::from_str(
            r#"{"version":1,"profiles":[
                 {"id":"p1","user":"hinteros-p1","name":"J","color":"teal","locked":false,"admin":true,"locked_until":null},
                 {"id":"p3","user":"hinteros-p3","name":"S","color":"red","locked":true,"admin":false,"locked_until":null}],
               "guest":{"enabled":true,"user":"hinteros-guest"},"first_boot":false,"spare":null,
               "last_used":"p1","next":{"target":"p3","fast":false,"expires":100},
               "appearance":{"text_scale":1.0,"high_contrast":false,"reduce_motion":false}}"#,
        )
        .unwrap();
        assert_eq!(v.fast_path(50), None, "locked target: the picker");
        let mut fast = v.clone();
        fast.next.as_mut().unwrap().fast = true;
        assert_eq!(fast.fast_path(50).as_deref(), Some("hinteros-p3"));
        assert_eq!(fast.fast_path(200), None, "expired");
        let mut first = v.clone();
        first.first_boot = true;
        first.spare = Some("hinteros-p1".into());
        assert_eq!(first.fast_path(0).as_deref(), Some("hinteros-p1"));
        assert_eq!(v.user_of("guest").as_deref(), Some("hinteros-guest"));
        assert_eq!(PublicView::default().fast_path(0), None);
    }

    #[test]
    fn public_view_reads_what_it_can() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles-public.json");
        assert!(PublicView::load(&path).profiles.is_empty(), "missing");
        // A newer service: unknown fields, a profile this version cannot read, fields of
        // another type.
        std::fs::write(
            &path,
            r#"{"version":2,"profiles":[
                 {"id":"p1","user":"hinteros-p1","name":"J","color":null,"admin":true,"kind":"x"},
                 {"id":"p2","name":"no user"},
                 {"id":"p3","user":"hinteros-p3","name":"S","color":"red","locked":false,"admin":false,"locked_until":"soon"}],
               "guest":{"enabled":true,"user":"hinteros-guest"},"first_boot":"no","spare":null,
               "last_used":"p1","next":{"target":"p3","fast":true,"expires":"later"},
               "appearance":{"text_scale":1.5,"high_contrast":true,"reduce_motion":false},
               "new":{"a":1}}"#,
        )
        .unwrap();
        let v = PublicView::load(&path);
        let ids: Vec<_> = v.profiles.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["p1", "p3"]);
        assert_eq!(v.profiles[0].color, "");
        assert!(v.profiles[0].locked, "not known: locked");
        assert!(!v.profiles[1].locked);
        assert_eq!(v.profiles[1].locked_until, None);
        assert!(!v.first_boot);
        assert!(v.next.is_none());
        assert_eq!(v.user_of("guest").as_deref(), Some("hinteros-guest"));
        assert_eq!(v.appearance.map(|a| a.text_scale), Some(1.5));
        // Not JSON at all: nothing.
        std::fs::write(&path, "{").unwrap();
        assert!(PublicView::load(&path).profiles.is_empty());
    }
}
