//! The picker (profiles.md "The pieces"): `hinteros-session greet` (the greeter session's
//! command: the fast path into a profile without any UI, else exec the picker) and the
//! greeter-mode daemon's `greeter.*` methods, which wrap greetd's IPC and the system service's
//! `greeter.sock` (profile creation).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use hinteros_api::client::Client;
use hinteros_api::common::Peer;
use hinteros_api::public::PublicView;
use hinteros_api::server::{decode_params, encode_result};
use hinteros_api::session::{
    EventName, GreeterAppearance, GreeterCancel, GreeterCreate, GreeterList, GreeterListResult,
    GreeterLogin, GreeterPin, GreeterProfile, GreeterPromptParams, GreeterResultParams,
    ProfileEmpty,
};
use hinteros_api::{ErrorCode, ErrorObject, Method, system};
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
        let a = v.appearance.clone().unwrap_or_default();
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
