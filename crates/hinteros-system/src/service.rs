//! The socket service: connections, the authorization gate and the method handlers.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine;
use hinteros_api::codec::{LineReader, ReadError, close_gracefully, write_message};
use hinteros_api::common::{Peer as ApiPeer, Ping};
use hinteros_api::jsonrpc;
use hinteros_api::server::{self, Connection, Step, method_not_found};
use hinteros_api::system::{
    self, AppsAcceptTerms, AppsAcceptTermsParams, AppsClearData, AppsClearDataParams,
    AppsGenerations, AppsInstall, AppsInstallParams, AppsList, AppsRemove, AppsRemoveParams,
    AppsRollback, AppsRollbackParams, BaseSet, BaseSetParams, ChannelCheck, ChannelCheckParams,
    ChannelCheckResult, ChannelSet, ChannelSetParams, EventName, GenerationsList,
    GenerationsListResult, HardwareRedetect, HardwareRedetectParams, JobIdParams, JobKind, JobRef,
    JobsCancel, JobsGet, JobsGetResult, JobsList, JobsListResult, Rebuild, Rollback,
    RollbackParams, Settings, SettingsGet, SettingsGetResult, SettingsSet, SettingsSetParams,
    SshInfo, SshSet, SshSetParams, SshSettings, Subscribe, SubscriptionParams, SubscriptionResult,
    SystemInfo, SystemInfoResult, Unsubscribe, UpdateApply, UpdateApplyParams, UpdateCheck,
    UpdateCheckReply, UpdateCheckResult, is_job_id, is_rev,
};
use hinteros_api::{ErrorCode, ErrorObject, Method};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::audit::{Audit, Decision};
use crate::authz::{self, Bus, RateLimit};
use crate::config::{Paths, ServeArgs};
use crate::flake::{self, ChannelTable, Release};
use crate::generations;
use crate::job::{self, Action, Apply, ApplyUpdate, JobSpec, NotFastForward, UpdateSource};
use crate::jobs::{self, Jobs};
use crate::peer::Peer;
use crate::state::{self, State};
use crate::store;

mod apps;
mod lan;
mod profiles;
mod servers;
mod storage;
mod subject;
mod timezone;

const NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// Mutating requests one connection may have in flight (an interactive polkit check can wait
/// minutes for the user; the connection keeps serving other requests meanwhile).
const MAX_IN_FLIGHT: usize = 4;

type Broadcast = (EventName, Value);

pub struct Service {
    args: ServeArgs,
    paths: Paths,
    bus: Bus,
    audit: Audit,
    rate: RateLimit,
    /// `update.check` has its own bucket: the launcher checks in the background (at startup,
    /// relayed with the session user's uid), and that must never use up the budget of the
    /// user's own actions. Its result is cached for 60 s anyway.
    check_rate: RateLimit,
    jobs: Mutex<Jobs>,
    events: broadcast::Sender<Broadcast>,
    /// Ctrl + Super + D ([`crate::keyboard`]): the uid that gets `keyboard_home` (the seat0 owner
    /// at the time of the press; only its connections, never the greeter socket's).
    keyboard_home: broadcast::Sender<u32>,
    connections: Mutex<HashMap<u32, usize>>,
    users: crate::users::Users,
    /// Owners `ensure_envs` already gave the starter env to (once per service run).
    env_tried: Mutex<BTreeSet<String>>,
    /// The time zone last announced (`time_zone_changed` only when it differs).
    last_zone: Mutex<Option<String>>,
    /// One `time_zone.set` at a time (one request file).
    tz_apply: tokio::sync::Mutex<()>,
    /// `lan_filter.*`: their own bucket (the session daemon calls at start and at each launch
    /// while the filter is not on), one change at a time.
    lan_rate: RateLimit,
    lan_apply: tokio::sync::Mutex<()>,
    /// When `profiles.list` last started the usage scan (at most every
    /// [`profiles::USAGE_SCAN_EVERY`]).
    usage_scan_at: Mutex<Option<Instant>>,
}

pub async fn serve(args: ServeArgs) -> Result<()> {
    // Before binding: a client may signal us as soon as the socket accepts.
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    // 0660: only root and the socket's group (set by the unit) can connect at all.
    startup(&args);
    profiles::republish_runtime(&args.paths());
    let listener = server::bind_unix(&args.socket, 0o660, 0o755)
        .with_context(|| format!("binding {}", args.socket.display()))?;
    info!(socket = %args.socket.display(), api_version = %system::VERSION, "listening");

    let paths = args.paths();
    let (events, _) = broadcast::channel(256);
    let (keyboard_home, _) = broadcast::channel(16);
    let zone = State::load(&paths.state_json())
        .ok()
        .map(|(s, _)| crate::timezone::effective(&paths.zoneinfo_dir, &s, job::now()).zone);
    let svc = Arc::new(Service {
        bus: Bus::new(args.bus.clone()),
        audit: Audit::new(args.journal_socket.clone()),
        rate: RateLimit::new(args.rate_per_minute, args.rate_burst),
        check_rate: RateLimit::new(args.rate_per_minute, args.rate_burst),
        jobs: Mutex::new(Jobs::load(paths.clone())),
        events,
        keyboard_home,
        connections: Mutex::new(HashMap::new()),
        users: args.users(),
        env_tried: Mutex::new(BTreeSet::new()),
        last_zone: Mutex::new(zone),
        tz_apply: tokio::sync::Mutex::new(()),
        // A session start, a restart and a few launches while it is not on fit in the burst.
        lan_rate: RateLimit::new(args.rate_per_minute.max(30), args.rate_burst.max(10)),
        lan_apply: tokio::sync::Mutex::new(()),
        usage_scan_at: Mutex::new(None),
        paths,
        args,
    });
    let greeter = match bind_greeter(&svc.args) {
        Ok(l) => l,
        Err(e) => {
            warn!("no greeter socket: {e:#}");
            None
        }
    };
    if let Some(l) = greeter {
        let svc = svc.clone();
        tokio::spawn(async move {
            loop {
                match l.accept().await {
                    Ok((stream, _)) => {
                        let svc = svc.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle(svc, stream, true).await {
                                debug!("greeter connection ended: {e:#}");
                            }
                        });
                    }
                    Err(e) => warn!("greeter accept failed: {e}"),
                }
            }
        });
    }
    match bind_keyboard(&svc.args) {
        Ok(Some(l)) => {
            tokio::spawn(serve_keyboard(svc.clone(), l));
        }
        Ok(None) => {}
        Err(e) => warn!("no keyboard socket: {e:#}"),
    }
    // Inactive profile sessions frozen (`crate::freeze`): at once, so a restart re-applies it.
    let freezer = svc.args.freeze_inactive.then(|| {
        crate::freeze::Freezer::spawn(svc.args.bus.clone(), svc.users.clone(), svc.paths.clone())
    });
    if let Some(id) = svc.jobs.lock().unwrap().busy() {
        info!(job = id, "re-attached to a running job");
    }
    svc.clear_admin_backoff();
    svc.profiles_changed();
    tokio::spawn(monitor(svc.clone()));

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let svc = svc.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle(svc, stream, false).await {
                            debug!("connection ended: {e:#}");
                        }
                    });
                }
                Err(e) => warn!("accept failed: {e}"),
            },
            _ = term.recv() => break,
            _ = int.recv() => break,
        }
    }
    info!("shutting down (running jobs continue in their own units)");
    // Nobody is left frozen while no service watches the seat (a stopped service, a rollback to
    // one that does not freeze); a restarted service freezes again at once.
    if let Some(f) = freezer {
        f.thaw_all().await;
    }
    let _ = std::fs::remove_file(&svc.args.socket);
    Ok(())
}

/// Before serving: the registry is checked against the env manifests and the session links are
/// published.
fn startup(args: &ServeArgs) {
    let paths = args.paths();
    if let Ok((state, _)) = State::load(&paths.state_json()) {
        crate::appreg::Registry::reconcile(&paths, &state);
        crate::appreg::publish(&paths, &state);
    }
    crate::guest::boot_cleanup(&paths);
}

/// Follow the running job and broadcast its events (at most 4 per second).
async fn monitor(svc: Arc<Service>) {
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut n: u64 = 0;
    loop {
        tick.tick().await;
        if n.is_multiple_of(4) {
            svc.ensure_envs().await;
        }
        n += 1;
        svc.poll_jobs();
    }
}

/// Counts a uid's connections; the slot is freed on drop.
struct Slot<'a> {
    svc: &'a Service,
    uid: u32,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let mut map = self.svc.connections.lock().unwrap();
        if let Some(n) = map.get_mut(&self.uid) {
            *n = n.saturating_sub(1);
        }
    }
}

/// `greeter.sock` (0660, group `hinteros-greeter`): the greeter's own few methods.
fn bind_greeter(args: &ServeArgs) -> Result<Option<tokio::net::UnixListener>> {
    bind_for_group(args, &args.greeter_socket, &args.greeter_group, "greeter")
}

/// `keyboard.sock` (0660, group `hinteros-keyboard`): the keyboard reader's `home` lines.
fn bind_keyboard(args: &ServeArgs) -> Result<Option<tokio::net::UnixListener>> {
    bind_for_group(
        args,
        &args.keyboard_socket,
        &args.keyboard_group,
        "keyboard",
    )
}

/// A socket only root and `group` may connect to; none without a path or the group.
fn bind_for_group(
    args: &ServeArgs,
    path: &Option<std::path::PathBuf>,
    group: &str,
    what: &str,
) -> Result<Option<tokio::net::UnixListener>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let Some(gid) = args.users().group_gid(group) else {
        info!(group, "no {what} group; no {what} socket");
        return Ok(None);
    };
    let l = server::bind_unix(path, 0o660, 0o755)
        .with_context(|| format!("binding {}", path.display()))?;
    rustix::fs::chown(path, None, Some(rustix::fs::Gid::from_raw(gid)))
        .with_context(|| format!("giving {} to group {gid}", path.display()))?;
    info!(socket = %path.display(), "listening for the {what}");
    Ok(Some(l))
}

/// The keyboard reader's connections ([`crate::keyboard`]): one `home` line per Ctrl + Super + D.
/// Anything else closes the connection.
async fn serve_keyboard(svc: Arc<Service>, listener: tokio::net::UnixListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let svc = svc.clone();
                tokio::spawn(async move {
                    let (r, _w) = stream.into_split();
                    let mut reader = LineReader::with_limit(r, 64);
                    loop {
                        match reader.next_line().await {
                            Ok(Some(line)) if line == crate::keyboard::HOME_LINE => {
                                svc.keyboard_home().await;
                            }
                            Ok(Some(_)) | Err(_) => {
                                warn!("keyboard socket: unexpected input; closing");
                                break;
                            }
                            Ok(None) => break,
                        }
                    }
                });
            }
            Err(e) => warn!("keyboard accept failed: {e}"),
        }
    }
}

async fn handle(svc: Arc<Service>, stream: UnixStream, greeter: bool) -> Result<()> {
    // Dropped when this connection ends: an interactive polkit check for it stops then.
    let (open, closed) = tokio::sync::watch::channel(());
    let peer = Arc::new(Peer::of(&stream, closed)?);
    info!(
        uid = peer.uid,
        gid = peer.gid,
        pid = ?peer.pid,
        pidfd = peer.pidfd.is_some(),
        exe = %peer.exe_display(),
        "connection"
    );
    let slot = {
        let mut map = svc.connections.lock().unwrap();
        let n = map.entry(peer.uid).or_insert(0);
        if *n >= svc.args.max_connections_per_uid {
            None
        } else {
            *n += 1;
            Some(Slot {
                svc: &svc,
                uid: peer.uid,
            })
        }
    };
    let Some(_slot) = slot else {
        warn!(uid = peer.uid, "too many connections; closing");
        let (r, mut w) = stream.into_split();
        close_gracefully(&mut LineReader::new(r), &mut w).await;
        return Ok(());
    };

    let (r, mut w) = stream.into_split();
    let mut reader = LineReader::with_limit(r, system::MAX_MESSAGE_BYTES);
    let mut conn = Connection::new(system::VERSION, ApiPeer::new(NAME, VERSION));
    let mut subscriptions: BTreeSet<EventName> = BTreeSet::new();
    let mut events = svc.events.subscribe();
    let mut keyboard_home = svc.keyboard_home.subscribe();
    // Answers of mutating requests, which run as tasks (responses may come out of order; JSON-RPC
    // clients match them by id).
    let (answers_tx, mut answers) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let in_flight = Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT));
    let hello_deadline = tokio::time::Instant::now() + HELLO_TIMEOUT;

    loop {
        let handshaken = conn.negotiated().is_some();
        tokio::select! {
            line = reader.next_line() => {
                let line = match line {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(e @ (ReadError::TooLong | ReadError::NotUtf8)) => {
                        svc.audit.request(peer.uid, peer.pid, &peer.exe_display(), "-", None, Decision::Invalid, None, &e.to_string());
                        let err = ErrorObject::new(ErrorCode::ParseError, e.to_string());
                        write_message(&mut w, &jsonrpc::error_response(None, err)).await?;
                        break;
                    }
                    Err(ReadError::Io(e)) => return Err(e.into()),
                };
                match conn.step(&line) {
                    Step::Reply(reply) => write_message(&mut w, &reply).await?,
                    Step::ReplyAndClose(reply) => {
                        warn!(uid = peer.uid, reply = %reply["error"]["message"], "closing connection");
                        write_message(&mut w, &reply).await?;
                        break;
                    }
                    Step::Ignore => {}
                    Step::Dispatch(req) => {
                        let id = req.id.expect("dispatched requests have an id");
                        if greeter && !system::GREETER_METHODS.contains(&req.method.as_str()) {
                            write_message(&mut w, &jsonrpc::response(&id, Err(method_not_found(&req.method)))).await?;
                            continue;
                        }
                        // The session daemon's own `lan_filter.set` and `lan_filter.clear` wait for
                        // a unit too: tasks. Any other name is `method_not_found` (in dispatch).
                        let own = [system::LanFilterSet::NAME, system::LanFilterClear::NAME]
                            .contains(&req.method.as_str());
                        if own || system::MUTATING.contains(&req.method.as_str()) {
                            let Ok(permit) = in_flight.clone().try_acquire_owned() else {
                                let err = authz::rate_limited(Duration::from_secs(1));
                                write_message(&mut w, &jsonrpc::response(&id, Err(err))).await?;
                                continue;
                            };
                            let (svc, peer, tx) = (svc.clone(), peer.clone(), answers_tx.clone());
                            tokio::spawn(async move {
                                let outcome = if own {
                                    svc.lan_filter(&peer, &req.method, req.params).await
                                } else {
                                    mutating(&svc, &peer, &req.method, req.params, greeter).await
                                };
                                let _ = tx.send(jsonrpc::response(&id, outcome));
                                drop(permit);
                            });
                        } else {
                            let outcome = dispatch(&svc, &peer, &req.method, req.params, &mut subscriptions).await;
                            write_message(&mut w, &jsonrpc::response(&id, outcome)).await?;
                        }
                    }
                }
            }
            Some(answer) = answers.recv() => write_message(&mut w, &answer).await?,
            _ = tokio::time::sleep_until(hello_deadline), if !handshaken => {
                debug!(uid = peer.uid, "no hello in time; closing");
                break;
            }
            home = keyboard_home.recv() => match home {
                Ok(uid) if uid == peer.uid && !greeter && subscriptions.contains(&EventName::KeyboardHome) => {
                    write_message(&mut w, &jsonrpc::notification(EventName::KeyboardHome.wire_name(), json!({}))).await?;
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            },
            event = events.recv() => match event {
                Ok((name, mut params)) if subscriptions.contains(&name) => {
                    // The Tailscale sign-in URL only to whoever started the sign-in.
                    if name == EventName::JobProgress {
                        let job_uid = params["job_id"].as_str().and_then(|id| svc.jobs.lock().unwrap().uid_of(id));
                        if greeter || !jobs::sees_auth_url(peer.uid, job_uid) {
                            jobs::strip_auth_url(&mut params);
                        }
                    }
                    write_message(&mut w, &jsonrpc::notification(name.wire_name(), params)).await?;
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => warn!("client lagging, dropped {n} events"),
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    // Every `break` above is a deliberate close; make sure the client gets what we sent.
    // The slot is free as soon as we stop serving; draining must not hold it.
    drop(_slot);
    drop(open);
    close_gracefully(&mut reader, &mut w).await;
    Ok(())
}

/// Decode params strictly (absent means `{}`); unknown fields name the field.
fn decode<M: Method>(params: Option<Value>) -> Result<M::Params, ErrorObject>
where
    M::Params: DeserializeOwned,
{
    let params = match params {
        None => Value::Object(Default::default()),
        Some(v @ Value::Object(_)) => v,
        Some(_) => {
            return Err(ErrorObject::new(
                ErrorCode::InvalidParams,
                "params must be an object",
            ));
        }
    };
    serde_json::from_value(params).map_err(|e| state::serde_error(&e.to_string()))
}

fn encode<M: Method>(r: M::Result) -> Result<Value, ErrorObject> {
    server::encode_result::<M>(r)
}

fn internal(e: impl std::fmt::Display) -> ErrorObject {
    warn!("internal error: {e}");
    ErrorObject::new(ErrorCode::InternalError, e.to_string())
}

/// The read-only methods (`handle` runs every mutating one through [`mutating`], in a task).
async fn dispatch(
    svc: &Arc<Service>,
    peer: &Peer,
    method: &str,
    params: Option<Value>,
    subs: &mut BTreeSet<EventName>,
) -> Result<Value, ErrorObject> {
    match method {
        Ping::NAME => server::call::<Ping>(params, server::ping),
        Subscribe::NAME => {
            let p: SubscriptionParams = decode::<Subscribe>(params)?;
            subs.extend(p.events);
            encode::<Subscribe>(SubscriptionResult {
                events: subs.iter().copied().collect(),
            })
        }
        Unsubscribe::NAME => {
            let p: SubscriptionParams = decode::<Unsubscribe>(params)?;
            for e in &p.events {
                subs.remove(e);
            }
            encode::<Unsubscribe>(SubscriptionResult {
                events: subs.iter().copied().collect(),
            })
        }
        SystemInfo::NAME => {
            decode::<SystemInfo>(params)?;
            encode::<SystemInfo>(svc.info()?)
        }
        SettingsGet::NAME => {
            decode::<SettingsGet>(params)?;
            let (state, revision) = svc.load_state()?;
            encode::<SettingsGet>(SettingsGetResult {
                revision,
                settings: Settings {
                    base: state.base.known_or(system::Base::Unstable),
                    ssh: SshSettings {
                        enabled: state.settings.ssh.enable,
                    },
                    requirements: state.requirements,
                },
            })
        }
        GenerationsList::NAME => {
            decode::<GenerationsList>(params)?;
            let p = &svc.paths;
            // The box features the profiles' apps need now: a rollback re-adds those a
            // generation lacks.
            let needed = State::load(&p.state_json())
                .ok()
                .map(|(s, _)| crate::appreg::Registry::scan(p, &s).needed_features());
            encode::<GenerationsList>(GenerationsListResult {
                generations: generations::list(
                    &p.profile,
                    &p.current_system,
                    &p.booted_system,
                    &p.boot_entries,
                    &p.boot_index(),
                    &p.profiles_json(),
                    needed.as_ref(),
                ),
            })
        }
        JobsGet::NAME => {
            let p: JobIdParams = decode::<JobsGet>(params)?;
            check_job_id(&p.job_id)?;
            let job = svc.jobs.lock().unwrap().get(&p.job_id);
            let mut job =
                job.ok_or_else(|| ErrorObject::new(ErrorCode::NotFound, "no such job"))?;
            jobs::redact_job(&mut job, peer.uid);
            encode::<JobsGet>(JobsGetResult { job })
        }
        JobsList::NAME => {
            decode::<JobsList>(params)?;
            let mut list = svc.jobs.lock().unwrap().list();
            for j in &mut list {
                jobs::redact_job(j, peer.uid);
            }
            encode::<JobsList>(JobsListResult { jobs: list })
        }
        AppsList::NAME => {
            decode::<AppsList>(params)?;
            encode::<AppsList>(svc.apps_list(peer)?)
        }
        AppsGenerations::NAME => {
            decode::<AppsGenerations>(params)?;
            encode::<AppsGenerations>(svc.apps_generations(peer)?)
        }
        system::ProfilesList::NAME => {
            decode::<system::ProfilesList>(params)?;
            encode::<system::ProfilesList>(svc.profiles_list(peer).await?)
        }
        system::StarterSetGet::NAME => {
            decode::<system::StarterSetGet>(params)?;
            encode::<system::StarterSetGet>(svc.starter_get()?)
        }
        system::StorageGet::NAME => {
            decode::<system::StorageGet>(params)?;
            encode::<system::StorageGet>(svc.storage_get())
        }
        system::CleanupGet::NAME => {
            decode::<system::CleanupGet>(params)?;
            encode::<system::CleanupGet>(svc.cleanup_get()?)
        }
        system::TimeZoneGet::NAME => {
            decode::<system::TimeZoneGet>(params)?;
            encode::<system::TimeZoneGet>(svc.time_zone_get()?)
        }
        system::TimeZoneList::NAME => {
            decode::<system::TimeZoneList>(params)?;
            encode::<system::TimeZoneList>(svc.time_zone_list().await?)
        }
        system::ServicesList::NAME => {
            decode::<system::ServicesList>(params)?;
            encode::<system::ServicesList>(svc.services_list().await?)
        }
        system::JellyfinClientToken::NAME => {
            decode::<system::JellyfinClientToken>(params)?;
            encode::<system::JellyfinClientToken>(svc.jellyfin_client_token(peer).await?)
        }
        system::MediaFolders::NAME => {
            let p = decode::<system::MediaFolders>(params)?;
            encode::<system::MediaFolders>(svc.media_folders(p).await?)
        }
        _ => Err(method_not_found(method)),
    }
}

fn check_job_id(id: &str) -> Result<(), ErrorObject> {
    if is_job_id(id) {
        Ok(())
    } else {
        Err(ErrorObject::invalid_field(
            "job_id",
            "bad_format",
            "job_id must be 16 hex digits",
        ))
    }
}

/// A short, safe summary of the params for the audit log: PINs are replaced by `***`.
fn summarize(params: &Option<Value>) -> String {
    let text = match params {
        Some(Value::Object(map)) => {
            let mut map = map.clone();
            for f in system::PIN_FIELDS.iter().chain(system::SECRET_FIELDS) {
                if let Some(v) = map.get_mut(*f) {
                    *v = Value::String("***".into());
                }
            }
            Value::Object(map).to_string()
        }
        Some(_) => "(not an object)".into(),
        None => "{}".into(),
    };
    text.chars().take(300).collect()
}

/// The gate for every mutating method: params, rate limit, peer, seat, polkit; then the
/// handler. Every outcome is audited.
async fn mutating(
    svc: &Arc<Service>,
    peer: &Peer,
    method: &str,
    params: Option<Value>,
    greeter: bool,
) -> Result<Value, ErrorObject> {
    let action = system::polkit_action(method).expect("every mutating method has an action");
    let summary = summarize(&params);
    let exe = peer.exe_display();
    let audit = |decision: Decision, job: Option<&str>| {
        svc.audit.request(
            peer.uid,
            peer.pid,
            &exe,
            method,
            Some(action),
            decision,
            job,
            &summary,
        );
    };
    let request = match Request::decode(method, params) {
        Ok(r) => r,
        Err(e) => {
            audit(Decision::Invalid, None);
            return Err(e);
        }
    };
    let bucket = if method == UpdateCheck::NAME {
        &svc.check_rate
    } else {
        &svc.rate
    };
    if let Err(retry) = bucket.take(peer.uid, Instant::now()) {
        audit(Decision::RateLimited, None);
        return Err(authz::rate_limited(retry));
    }
    let interaction = system::INTERACTIVE_ACTIONS
        .contains(&action)
        .then(|| svc.args.auth_timeout());
    if let Err(denial) = authz::authorize(&svc.bus, peer, action, interaction).await {
        audit(denial.decision(), None);
        return Err(denial.error(action));
    }
    match svc.handle(peer, request, greeter).await {
        Ok(value) => {
            let job = value.get("job_id").and_then(Value::as_str);
            audit(Decision::Allowed, job);
            Ok(value)
        }
        Err(e) => {
            audit(Decision::of_error(&e), None);
            info!(method, error = %e.message, "request refused");
            Err(e)
        }
    }
}

/// A decoded mutating request.
pub(crate) enum Request {
    SettingsSet(SettingsSetParams),
    SshSet(SshSetParams),
    Rebuild,
    HardwareRedetect(HardwareRedetectParams),
    Rollback(RollbackParams),
    UpdateCheck,
    UpdateApply(UpdateApplyParams),
    ChannelCheck(ChannelCheckParams),
    ChannelSet(ChannelSetParams),
    BaseSet(BaseSetParams),
    AppsInstall(AppsInstallParams),
    AppsRemove(AppsRemoveParams),
    AppsRollback(AppsRollbackParams),
    AppsAcceptTerms(AppsAcceptTermsParams),
    AppsClearData(AppsClearDataParams),
    Profiles(profiles::ProfilesRequest),
    Storage(storage::StorageRequest),
    TimeZoneSet(system::TimeZoneSetParams),
    /// The Server section.
    Servers(servers::ServersRequest),
    JobsCancel(JobIdParams),
}

impl Request {
    pub(crate) fn decode(method: &str, mut params: Option<Value>) -> Result<Self, ErrorObject> {
        if let Some(r) = servers::ServersRequest::decode(method, &mut params) {
            return r.map(Self::Servers);
        }
        Ok(match method {
            SettingsSet::NAME => {
                // Name the offending field of `changes` before the typed decode.
                if let Some(changes) = params.as_ref().and_then(|p| p.get("changes")) {
                    state::check_changes(changes)?;
                }
                let p: SettingsSetParams = decode::<SettingsSet>(params)?;
                check_revision(&p.revision)?;
                Self::SettingsSet(p)
            }
            SshSet::NAME => Self::SshSet(decode::<SshSet>(params)?),
            Rebuild::NAME => {
                decode::<Rebuild>(params)?;
                Self::Rebuild
            }
            HardwareRedetect::NAME => {
                let p: HardwareRedetectParams = decode::<HardwareRedetect>(params)?;
                check_admin_pin(p.admin_pin.as_deref())?;
                Self::HardwareRedetect(p)
            }
            Rollback::NAME => {
                let p: RollbackParams = decode::<Rollback>(params)?;
                check_admin_pin(p.admin_pin.as_deref())?;
                if p.generation == Some(0) {
                    return Err(ErrorObject::invalid_field(
                        "generation",
                        "out_of_range",
                        "generation must be at least 1",
                    ));
                }
                Self::Rollback(p)
            }
            UpdateCheck::NAME => {
                decode::<UpdateCheck>(params)?;
                Self::UpdateCheck
            }
            UpdateApply::NAME => {
                let p: UpdateApplyParams = decode::<UpdateApply>(params)?;
                check_admin_pin(p.admin_pin.as_deref())?;
                if !is_rev(&p.rev) {
                    return Err(ErrorObject::invalid_field(
                        "rev",
                        "bad_format",
                        "rev must be 40 hex digits",
                    ));
                }
                Self::UpdateApply(p)
            }
            ChannelCheck::NAME => Self::ChannelCheck(decode::<ChannelCheck>(params)?),
            ChannelSet::NAME => {
                let p: ChannelSetParams = decode::<ChannelSet>(params)?;
                check_admin_pin(p.admin_pin.as_deref())?;
                if p.rev.as_deref().is_some_and(|r| !is_rev(r)) {
                    return Err(ErrorObject::invalid_field(
                        "rev",
                        "bad_format",
                        "rev must be 40 hex digits",
                    ));
                }
                Self::ChannelSet(p)
            }
            BaseSet::NAME => {
                let p: BaseSetParams = decode::<BaseSet>(params)?;
                check_admin_pin(p.admin_pin.as_deref())?;
                Self::BaseSet(p)
            }
            AppsInstall::NAME => {
                let p: AppsInstallParams = decode::<AppsInstall>(params)?;
                check_revision(&p.revision)?;
                store::check_ids(&p.ids)?;
                store::check_accepted_terms(&p.accepted_terms)?;
                Self::AppsInstall(p)
            }
            AppsRemove::NAME => {
                let p: AppsRemoveParams = decode::<AppsRemove>(params)?;
                check_revision(&p.revision)?;
                store::check_ids(&p.ids)?;
                Self::AppsRemove(p)
            }
            AppsRollback::NAME => {
                let p: AppsRollbackParams = decode::<AppsRollback>(params)?;
                if p.generation == 0 {
                    return Err(ErrorObject::invalid_field(
                        "generation",
                        "out_of_range",
                        "generation must be at least 1",
                    ));
                }
                Self::AppsRollback(p)
            }
            AppsAcceptTerms::NAME => {
                let p: AppsAcceptTermsParams = decode::<AppsAcceptTerms>(params)?;
                if !hinteros_api::catalog::is_app_id(&p.id) {
                    return Err(ErrorObject::invalid_field(
                        "id",
                        "bad_format",
                        "id must be a catalog id",
                    ));
                }
                if !hinteros_api::catalog::is_terms_version(&p.version) {
                    return Err(ErrorObject::invalid_field(
                        "version",
                        "bad_format",
                        "version must match ^[A-Za-z0-9._-]{1,32}$",
                    ));
                }
                Self::AppsAcceptTerms(p)
            }
            AppsClearData::NAME => {
                let p: AppsClearDataParams = decode::<AppsClearData>(params)?;
                if !hinteros_api::catalog::is_app_id(&p.id) {
                    return Err(ErrorObject::invalid_field(
                        "id",
                        "bad_format",
                        "id must be a catalog id",
                    ));
                }
                Self::AppsClearData(p)
            }
            JobsCancel::NAME => {
                let p: JobIdParams = decode::<JobsCancel>(params)?;
                check_job_id(&p.job_id)?;
                Self::JobsCancel(p)
            }
            system::TimeZoneSet::NAME => {
                let p = decode::<system::TimeZoneSet>(params)?;
                timezone::validate(&p)?;
                Self::TimeZoneSet(p)
            }
            system::StorageMeasure::NAME
            | system::StorageTree::NAME
            | system::StorageCleanupSystem::NAME
            | system::StorageCleanupProfiles::NAME
            | system::CleanupSet::NAME => {
                use storage::StorageRequest as S;
                let r = match method {
                    system::StorageMeasure::NAME => {
                        decode::<system::StorageMeasure>(params)?;
                        S::Measure
                    }
                    system::StorageTree::NAME => S::Tree(decode::<system::StorageTree>(params)?),
                    system::StorageCleanupSystem::NAME => {
                        S::CleanupSystem(decode::<system::StorageCleanupSystem>(params)?)
                    }
                    system::StorageCleanupProfiles::NAME => {
                        S::CleanupProfiles(decode::<system::StorageCleanupProfiles>(params)?)
                    }
                    _ => S::CleanupSet(decode::<system::CleanupSet>(params)?),
                };
                r.validate()?;
                Self::Storage(r)
            }
            m => {
                use profiles::ProfilesRequest as P;
                let r = match m {
                    system::ProfilesCreate::NAME => {
                        P::Create(decode::<system::ProfilesCreate>(params)?)
                    }
                    system::ProfilesSetupAdmin::NAME => {
                        P::SetupAdmin(decode::<system::ProfilesSetupAdmin>(params)?)
                    }
                    system::ProfilesRename::NAME => {
                        P::Rename(decode::<system::ProfilesRename>(params)?)
                    }
                    system::ProfilesSetAppearance::NAME => {
                        P::SetAppearance(decode::<system::ProfilesSetAppearance>(params)?)
                    }
                    system::ProfilesSetPin::NAME => {
                        P::SetPin(decode::<system::ProfilesSetPin>(params)?)
                    }
                    system::ProfilesSetLocked::NAME => {
                        P::SetLocked(decode::<system::ProfilesSetLocked>(params)?)
                    }
                    system::ProfilesResetPin::NAME => {
                        P::ResetPin(decode::<system::ProfilesResetPin>(params)?)
                    }
                    system::ProfilesRemove::NAME => {
                        P::Remove(decode::<system::ProfilesRemove>(params)?)
                    }
                    system::ProfilesSwitch::NAME => {
                        P::Switch(decode::<system::ProfilesSwitch>(params)?)
                    }
                    system::ProfilesSwitchCancel::NAME => {
                        decode::<system::ProfilesSwitchCancel>(params)?;
                        P::SwitchCancel
                    }
                    system::ProfilesUnlock::NAME => {
                        P::Unlock(decode::<system::ProfilesUnlock>(params)?)
                    }
                    system::GuestSet::NAME => P::GuestSet(decode::<system::GuestSet>(params)?),
                    system::GuestEnd::NAME => {
                        decode::<system::GuestEnd>(params)?;
                        P::GuestEnd
                    }
                    system::DisplaySaveMode::NAME => {
                        P::SaveMode(decode::<system::DisplaySaveMode>(params)?)
                    }
                    system::AudioSaveOutput::NAME => {
                        P::SaveOutput(decode::<system::AudioSaveOutput>(params)?)
                    }
                    system::StarterSetSet::NAME => {
                        P::StarterSet(decode::<system::StarterSetSet>(params)?)
                    }
                    _ => return Err(method_not_found(method)),
                };
                r.validate()?;
                Self::Profiles(r)
            }
        })
    }
}

/// An optional `admin_pin` of a box-wide action: 4 to 8 digits when present.
pub(crate) fn check_admin_pin(pin: Option<&str>) -> Result<(), ErrorObject> {
    match pin {
        Some(p) if !crate::pins::is_pin(p) => Err(ErrorObject::invalid_field(
            "admin_pin",
            "bad_format",
            "a PIN is 4 to 8 digits",
        )),
        _ => Ok(()),
    }
}

pub(crate) fn check_revision(revision: &str) -> Result<(), ErrorObject> {
    if revision.len() != 64
        || !revision
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(ErrorObject::invalid_field(
            "revision",
            "bad_format",
            "revision must be 64 hex digits",
        ));
    }
    Ok(())
}

fn state_conflict() -> ErrorObject {
    ErrorObject::with_data(
        ErrorCode::StateConflict,
        "state.json changed since settings.get",
        json!({ "reason": "revision" }),
    )
}

fn profiles_method_name(r: &profiles::ProfilesRequest) -> &'static str {
    use profiles::ProfilesRequest as P;
    match r {
        P::Create(_) => system::ProfilesCreate::NAME,
        P::SetupAdmin(_) => system::ProfilesSetupAdmin::NAME,
        P::Rename(_) => system::ProfilesRename::NAME,
        P::SetAppearance(_) => system::ProfilesSetAppearance::NAME,
        P::SetPin(_) => system::ProfilesSetPin::NAME,
        P::SetLocked(_) => system::ProfilesSetLocked::NAME,
        P::ResetPin(_) => system::ProfilesResetPin::NAME,
        P::Remove(_) => system::ProfilesRemove::NAME,
        P::Switch(_) => system::ProfilesSwitch::NAME,
        P::SwitchCancel => system::ProfilesSwitchCancel::NAME,
        P::Unlock(_) => system::ProfilesUnlock::NAME,
        P::GuestSet(_) => system::GuestSet::NAME,
        P::GuestEnd => system::GuestEnd::NAME,
        P::SaveMode(_) => system::DisplaySaveMode::NAME,
        P::SaveOutput(_) => system::AudioSaveOutput::NAME,
        P::StarterSet(_) => system::StarterSetSet::NAME,
    }
}

fn job_ref(id: Option<String>) -> Result<Value, ErrorObject> {
    serde_json::to_value(JobRef { job_id: id }).map_err(internal)
}

impl Service {
    /// Ctrl + Super + D: `keyboard_home` to the session of whoever owns seat0's active session
    /// now (nobody on a text console without a session; the picker ignores it).
    async fn keyboard_home(&self) {
        match authz::seat_owner(&self.bus).await {
            Ok(Some(uid)) => {
                info!(uid, "Ctrl+Super+D: keyboard_home");
                let _ = self.keyboard_home.send(uid);
            }
            Ok(None) => debug!("Ctrl+Super+D: no active session"),
            Err(e) => {
                warn!("Ctrl+Super+D: cannot ask logind: {e}");
                self.bus.reset().await;
            }
        }
    }

    /// Follow the running job: broadcast its events and, once its result is in, act on its end.
    /// The monitor calls it 4 times a second; a request that a running job would refuse calls it
    /// first, so a job that has just written its result no longer counts as running.
    pub(super) fn poll_jobs(&self) {
        let (events, finished) = self.jobs.lock().unwrap().poll();
        for e in events {
            let _ = self.events.send(e);
        }
        let Some(f) = finished else {
            return;
        };
        if f.job.kind == JobKind::GuestEnd {
            // The guest may need the starter env again (when the job could not give it).
            self.env_tried.lock().unwrap().remove("guest");
        }
        self.time_zone_recheck();
        if matches!(
            f.job.kind,
            JobKind::ProfilesCreate
                | JobKind::ProfilesRemove
                | JobKind::GuestSet
                | JobKind::GuestEnd
                | JobKind::Rollback
        ) {
            self.profiles_changed();
        }
        let outcome = serde_json::to_value(f.job.state)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let kind = serde_json::to_value(f.job.kind)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let opt = |v: Option<String>| v.unwrap_or_default();
        let after = f.status.after.clone().unwrap_or_default();
        self.audit.job(
            &f.job.job_id,
            &kind,
            f.job.uid,
            &outcome,
            &[
                ("REV_BEFORE", opt(f.status.before.rev.clone())),
                ("REV_AFTER", opt(after.rev)),
                (
                    "GENERATION_BEFORE",
                    opt(f.status.before.generation.map(|g| g.to_string())),
                ),
                (
                    "GENERATION_AFTER",
                    opt(after.generation.map(|g| g.to_string())),
                ),
                (
                    "ERROR",
                    f.job
                        .error
                        .as_ref()
                        .map(|e| e.message.clone())
                        .unwrap_or_default(),
                ),
            ],
        );
    }

    /// The running job's `job_busy` error, if a job is still running (see [`Self::poll_jobs`]).
    pub(super) fn job_busy(&self) -> Option<ErrorObject> {
        self.poll_jobs();
        self.jobs.lock().unwrap().busy_error()
    }

    fn load_state(&self) -> Result<(State, String), ErrorObject> {
        State::load(&self.paths.state_json()).map_err(internal)
    }

    fn policy_dir(&self) -> std::path::PathBuf {
        // Resolved now, so a job verifies against the generation that was running when it was
        // requested.
        let dir = self.args.policy_dir();
        std::fs::canonicalize(&dir).unwrap_or(dir)
    }

    fn channel_table(&self) -> Option<ChannelTable> {
        match ChannelTable::load(&self.policy_dir()) {
            Ok(t) => Some(t),
            Err(e) => {
                debug!("no channel table: {e:#}");
                None
            }
        }
    }

    fn info(&self) -> Result<SystemInfoResult, ErrorObject> {
        let policy = self.policy_dir();
        let release = Release::load(&policy);
        let lock = flake::read_lock(&self.paths.config_dir.join("flake.lock")).unwrap_or_default();
        let table = self.channel_table();
        let state = State::load(&self.paths.state_json()).ok().map(|(s, _)| s);
        let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|h| h.trim().chars().take(64).collect())
            .unwrap_or_default();
        Ok(SystemInfoResult {
            version: release.version,
            channel: flake::current_channel(&lock, table.as_ref()),
            base: state
                .as_ref()
                .and_then(|s| s.base.known())
                .unwrap_or(system::Base::Unstable),
            rev: lock.rev,
            generation: generations::current_generation(&self.paths.profile),
            hostname,
            dev: release.dev,
            ssh: SshInfo {
                enabled: state.is_some_and(|s| s.settings.ssh.enable),
                host_key_fingerprints: host_key_fingerprints(&self.args.ssh_host_key_dir),
            },
        })
    }

    /// Memory and disk preflight for jobs that build.
    fn preflight(&self) -> Result<(), ErrorObject> {
        self.preflight_with(0)
    }

    /// The preflight, with `extra_disk` bytes needed on top of the rebuild margin (new apps).
    fn preflight_with(&self, extra_disk: u64) -> Result<(), ErrorObject> {
        let insufficient = |kind: &str, needed: u64, available: u64| {
            ErrorObject::with_data(
                ErrorCode::InsufficientResources,
                format!("not enough {kind} to rebuild"),
                json!({ "kind": kind, "needed_bytes": needed, "available_bytes": available }),
            )
        };
        if self.args.min_memory_bytes > 0 {
            let available = mem_available().unwrap_or(0);
            if available < self.args.min_memory_bytes {
                return Err(insufficient(
                    "memory",
                    self.args.min_memory_bytes,
                    available,
                ));
            }
        }
        let needed = self.args.min_disk_bytes.saturating_add(extra_disk);
        if needed > 0 {
            let available = rustix::fs::statvfs(&self.args.store_dir)
                .map(|s| s.f_bavail.saturating_mul(s.f_frsize))
                .unwrap_or(0);
            if available < needed {
                return Err(insufficient("disk", needed, available));
            }
        }
        Ok(())
    }

    fn spec(&self, kind: JobKind, uid: u32, action: Action) -> JobSpec {
        JobSpec {
            id: job::new_id(),
            kind,
            uid,
            created_at: job::now(),
            action,
            subject: system::JobSubject::default(),
        }
    }

    /// Create the job and start its unit; returns its id.
    async fn start(&self, spec: JobSpec) -> Result<String, ErrorObject> {
        // A job that has just ended is taken in first (see `poll_jobs`).
        self.poll_jobs();
        {
            let mut jobs = self.jobs.lock().unwrap();
            if let Some(e) = jobs.busy_error() {
                return Err(e);
            }
            jobs.create(&spec).map_err(internal)?;
        }
        let started = match self.bus.get().await {
            Ok(conn) => crate::jobs::start_unit(&conn, &spec.id).await,
            Err(e) => Err(e),
        };
        if let Err(e) = started {
            self.bus.reset().await;
            self.jobs.lock().unwrap().abandon(&spec.id);
            return Err(internal(format!(
                "cannot start {}: {e}",
                job::unit_name(&spec.id)
            )));
        }
        info!(job = %spec.id, kind = ?spec.kind, uid = spec.uid, "job started");
        let _ = self.events.send(Jobs::started_event(&spec));
        Ok(spec.id)
    }

    fn update_source(
        &self,
        table: &ChannelTable,
        channel: system::Channel,
        current_rev: Option<String>,
    ) -> Result<UpdateSource, ErrorObject> {
        let branch = table
            .branch(channel)
            .ok_or_else(|| internal(format!("the channel table has no {}", channel.name())))?;
        Ok(UpdateSource {
            repo: table.repo.clone(),
            scheme: table.scheme().unwrap_or("https").to_owned(),
            host: table.host(),
            branch: branch.to_owned(),
            channel,
            current_rev,
            catalog: None,
            others: Vec::new(),
        })
    }

    /// The last update check, if it is for the channel and revision the box is on.
    fn last_check(&self, channel: system::Channel, rev: Option<&str>) -> Option<UpdateCheckResult> {
        let r: UpdateCheckResult = crate::fsutil::read_cache(&self.paths.last_check())?;
        (r.channel == channel && r.current_rev.as_deref() == rev).then_some(r)
    }

    /// The last `channel.check` result, if it is for `channel` and the revision the box is on.
    fn last_channel_check(
        &self,
        channel: system::Channel,
        rev: Option<&str>,
    ) -> Option<ChannelCheckResult> {
        let r: ChannelCheckResult = crate::fsutil::read_cache(&self.paths.channel_check())?;
        (r.channel == channel && r.current_rev.as_deref() == rev).then_some(r)
    }

    async fn handle(
        &self,
        peer: &Peer,
        request: Request,
        greeter: bool,
    ) -> Result<Value, ErrorObject> {
        let uid = peer.uid;
        match request {
            Request::Profiles(r) => {
                let method = profiles_method_name(&r);
                self.profiles_request(peer, method, r, greeter).await
            }
            Request::SettingsSet(p) => {
                let (state, revision) = self.load_state()?;
                if p.revision != revision {
                    return Err(state_conflict());
                }
                // No settable fields yet: an empty change set is nothing to do. Each future
                // field applies to `new` here.
                let new = state.clone();
                if new == state {
                    return job_ref(None);
                }
                let new = self.requirements_update(&new).unwrap_or(new);
                self.preflight()?;
                let spec = self.spec(
                    JobKind::Settings,
                    uid,
                    Action::Apply(Apply {
                        state: Some(new),
                        ..Default::default()
                    }),
                );
                job_ref(Some(self.start(spec).await?))
            }
            Request::SshSet(p) => {
                let (mut state, _) = self.load_state()?;
                if state.settings.ssh.enable == p.enabled {
                    return job_ref(None);
                }
                state.settings.ssh.enable = p.enabled;
                let state = self.requirements_update(&state).unwrap_or(state);
                self.preflight()?;
                let spec = self
                    .spec(
                        JobKind::Ssh,
                        uid,
                        Action::Apply(Apply {
                            state: Some(state),
                            ..Default::default()
                        }),
                    )
                    .about(system::JobSubject {
                        enabled: Some(p.enabled),
                        ..Default::default()
                    });
                job_ref(Some(self.start(spec).await?))
            }
            Request::Rebuild => {
                let (state, _) = self.load_state()?;
                self.preflight()?;
                let spec = self.spec(
                    JobKind::Rebuild,
                    uid,
                    Action::Apply(Apply {
                        state: self.requirements_update(&state),
                        ..Default::default()
                    }),
                );
                job_ref(Some(self.start(spec).await?))
            }
            Request::HardwareRedetect(p) => {
                self.check_box_admin(uid, HardwareRedetect::NAME, p.admin_pin.as_deref())
                    .await?;
                let (state, _) = self.load_state()?;
                self.preflight()?;
                let spec = self.spec(
                    JobKind::HardwareRedetect,
                    uid,
                    Action::Apply(Apply {
                        facter: true,
                        state: self.requirements_update(&state),
                        ..Default::default()
                    }),
                );
                job_ref(Some(self.start(spec).await?))
            }
            Request::BaseSet(p) => {
                self.check_box_admin(uid, BaseSet::NAME, p.admin_pin.as_deref())
                    .await?;
                let (mut state, _) = self.load_state()?;
                let ack = p.acknowledge_downgrade.unwrap_or(false);
                let Some(base) = state::base_change(&state.base, p.base, ack)? else {
                    return job_ref(None);
                };
                let from_base = state.base.known();
                state.base = base.into();
                let state = self.requirements_update(&state).unwrap_or(state);
                self.preflight()?;
                let spec = self
                    .spec(
                        JobKind::Base,
                        uid,
                        Action::Apply(Apply {
                            rebuild_apps: true,
                            state: Some(state),
                            ..Default::default()
                        }),
                    )
                    .about(system::JobSubject {
                        base: Some(base),
                        from_base,
                        ..Default::default()
                    });
                job_ref(Some(self.start(spec).await?))
            }
            Request::Rollback(p) => {
                self.check_box_admin(uid, Rollback::NAME, p.admin_pin.as_deref())
                    .await?;
                let gens = generations::list(
                    &self.paths.profile,
                    &self.paths.current_system,
                    &self.paths.booted_system,
                    &self.paths.boot_entries,
                    &self.paths.boot_index(),
                    &self.paths.profiles_json(),
                    None,
                );
                let current = generations::current_generation(&self.paths.profile);
                let target = match p.generation {
                    Some(g) => g,
                    None => gens
                        .iter()
                        .map(|g| g.generation)
                        .filter(|g| current.is_some_and(|c| *g < c))
                        .max()
                        .ok_or_else(|| {
                            ErrorObject::new(ErrorCode::NotFound, "there is no earlier generation")
                        })?,
                };
                if !gens.iter().any(|g| g.generation == target) || Some(target) == current {
                    return Err(ErrorObject::new(
                        ErrorCode::NotFound,
                        format!(
                            "generation {target} is not on disk, has no boot entry or is the current one"
                        ),
                    ));
                }
                if let Some(e) = self.job_busy() {
                    return Err(e);
                }
                let spec = self
                    .spec(
                        JobKind::Rollback,
                        uid,
                        Action::Rollback {
                            generation: target,
                            rebuild_apps: true,
                        },
                    )
                    .about(system::JobSubject {
                        generation: Some(target),
                        from_generation: current,
                        ..Default::default()
                    });
                job_ref(Some(self.start(spec).await?))
            }
            Request::UpdateCheck => {
                let table = self
                    .channel_table()
                    .ok_or_else(|| internal("this system has no update channel table"))?;
                let lock = flake::read_lock(&self.paths.config_dir.join("flake.lock"))
                    .map_err(internal)?;
                let channel = flake::current_channel(&lock, Some(&table))
                    .ok_or_else(|| internal("/etc/hinteros does not follow a known channel"))?;
                if let Some(cached) = self.last_check(channel, lock.rev.as_deref())
                    && job::now().saturating_sub(cached.checked_at)
                        < self.args.update_check_cache().as_secs() as i64
                {
                    return serde_json::to_value(UpdateCheckReply {
                        job_id: None,
                        result: Some(cached),
                    })
                    .map_err(internal);
                }
                let mut source = self.update_source(&table, channel, lock.rev.clone())?;
                // The other channels' tips are verified and recorded too, before any switch.
                source.others = table
                    .branches
                    .values()
                    .filter(|b| **b != source.branch)
                    .cloned()
                    .collect();
                source.catalog = Some(job::CatalogCheck {
                    installed: State::load(&self.paths.state_json())
                        .map(|(s, _)| self.installed_anywhere(&s))
                        .unwrap_or_default(),
                    input_url: table.input_url(&source.branch, None),
                });
                let subject = system::JobSubject {
                    channel: Some(channel),
                    from_rev: lock.rev.clone(),
                    ..Default::default()
                };
                let spec = self
                    .spec(JobKind::UpdateCheck, uid, Action::UpdateCheck(source))
                    .about(subject);
                let id = self.start(spec).await?;
                serde_json::to_value(UpdateCheckReply {
                    job_id: Some(id),
                    result: None,
                })
                .map_err(internal)
            }
            Request::UpdateApply(p) => {
                self.check_box_admin(uid, UpdateApply::NAME, p.admin_pin.as_deref())
                    .await?;
                let table = self
                    .channel_table()
                    .ok_or_else(|| internal("this system has no update channel table"))?;
                let lock = flake::read_lock(&self.paths.config_dir.join("flake.lock"))
                    .map_err(internal)?;
                let channel = flake::current_channel(&lock, Some(&table))
                    .ok_or_else(|| internal("/etc/hinteros does not follow a known channel"))?;
                let verified = self
                    .last_check(channel, lock.rev.as_deref())
                    .and_then(|c| c.available)
                    .filter(|a| a.rev == p.rev);
                let Some(verified) = verified else {
                    return Err(ErrorObject::with_data(
                        ErrorCode::StateConflict,
                        "rev is not the last verified update",
                        json!({ "reason": "update_rev" }),
                    ));
                };
                self.preflight()?;
                let source = self.update_source(&table, channel, lock.rev.clone())?;
                let update = ApplyUpdate {
                    input_url: table.input_url(&source.branch, None),
                    source,
                    rev: Some(p.rev.clone()),
                    not_fast_forward: NotFastForward::Reject,
                    retarget_from: None,
                };
                let subject = system::JobSubject {
                    rev: Some(p.rev),
                    version: verified.version,
                    channel: Some(channel),
                    from_rev: lock.rev.clone(),
                    from_version: self.running_version(),
                    ..Default::default()
                };
                let state = self.load_state().ok().map(|(s, _)| s);
                let spec = self
                    .spec(
                        JobKind::UpdateApply,
                        uid,
                        Action::Apply(Apply {
                            update: Some(update),
                            state: state.as_ref().and_then(|s| self.requirements_update(s)),
                            rebuild_apps: state.is_some(),
                            ..Default::default()
                        }),
                    )
                    .about(subject);
                job_ref(Some(self.start(spec).await?))
            }
            Request::ChannelCheck(p) => {
                let table = self
                    .channel_table()
                    .ok_or_else(|| internal("this system has no update channel table"))?;
                let lock = flake::read_lock(&self.paths.config_dir.join("flake.lock"))
                    .map_err(internal)?;
                let from = flake::current_channel(&lock, Some(&table));
                if from == Some(p.channel) {
                    return job_ref(None);
                }
                let mut source = self.update_source(&table, p.channel, lock.rev.clone())?;
                // The branch the box follows: fetched if the locked revision is not local.
                source.others = from
                    .and_then(|c| table.branch(c))
                    .map(str::to_owned)
                    .into_iter()
                    .collect();
                let subject = system::JobSubject {
                    channel: Some(p.channel),
                    from_channel: from,
                    from_rev: lock.rev.clone(),
                    ..Default::default()
                };
                let spec = self
                    .spec(JobKind::ChannelCheck, uid, Action::ChannelCheck(source))
                    .about(subject);
                job_ref(Some(self.start(spec).await?))
            }
            Request::ChannelSet(p) => {
                self.check_box_admin(uid, ChannelSet::NAME, p.admin_pin.as_deref())
                    .await?;
                let table = self
                    .channel_table()
                    .ok_or_else(|| internal("this system has no update channel table"))?;
                let lock = flake::read_lock(&self.paths.config_dir.join("flake.lock"))
                    .map_err(internal)?;
                let from = flake::current_channel(&lock, Some(&table));
                let to = p.channel;
                if from == Some(to) {
                    return job_ref(None);
                }
                // Only the revision channel.check verified, and the owner confirmed, for this
                // channel and the locked revision.
                let verified = self
                    .last_channel_check(to, lock.rev.as_deref())
                    .filter(|c| p.rev.as_deref() == Some(c.target.rev.as_str()));
                let Some(verified) = verified else {
                    return Err(ErrorObject::with_data(
                        ErrorCode::StateConflict,
                        "rev is not the last verified channel check",
                        json!({ "reason": "channel_rev" }),
                    ));
                };
                let from_branch = lock
                    .branch
                    .clone()
                    .ok_or_else(|| internal("/etc/hinteros/flake.lock does not follow a branch"))?;
                self.preflight()?;
                let source = self.update_source(&table, to, lock.rev.clone())?;
                let rev = verified.target.rev.clone();
                let update = ApplyUpdate {
                    input_url: table.input_url(&source.branch, None),
                    retarget_from: Some(table.input_url(&from_branch, None)),
                    source,
                    rev: Some(rev.clone()),
                    // The owner confirmed this revision; the job still refuses one older than
                    // the newest verified tip of the target branch.
                    not_fast_forward: NotFastForward::Allow,
                };
                let subject = system::JobSubject {
                    rev: Some(rev),
                    version: verified.target.version,
                    channel: Some(to),
                    from_channel: from,
                    from_rev: lock.rev.clone(),
                    from_version: self.running_version(),
                    ..Default::default()
                };
                let state = self.load_state().ok().map(|(s, _)| s);
                let spec = self
                    .spec(
                        JobKind::Channel,
                        uid,
                        Action::Apply(Apply {
                            update: Some(update),
                            state: state.as_ref().and_then(|s| self.requirements_update(s)),
                            rebuild_apps: state.is_some(),
                            ..Default::default()
                        }),
                    )
                    .about(subject);
                job_ref(Some(self.start(spec).await?))
            }
            Request::AppsInstall(p) => self.apps_install(peer, p).await,
            Request::AppsRemove(p) => self.apps_remove(peer, p).await,
            Request::Storage(r) => self.storage_request(peer, r).await,
            Request::Servers(r) => self.servers_request(peer, r).await,
            Request::TimeZoneSet(p) => self.time_zone_set(peer, p).await,
            Request::AppsRollback(p) => self.apps_rollback(peer, p).await,
            Request::AppsAcceptTerms(p) => self.apps_accept_terms(peer, p),
            Request::AppsClearData(p) => self.apps_clear_data(peer, p).await,
            Request::JobsCancel(p) => {
                // Only who started the job, root or the admin profile cancels it: an update the
                // admin approved with the PIN is not undone from another profile or the guest.
                let started_by = self.jobs.lock().unwrap().get(&p.job_id).map(|j| j.uid);
                if let Some(owner) = started_by
                    && uid != 0
                    && uid != owner
                    && !self.caller_is_admin_profile(uid)
                {
                    return Err(ErrorObject::with_data(
                        ErrorCode::PermissionDenied,
                        "only who started the job, or the admin profile, can cancel it",
                        json!({ "action": system::polkit_action(system::JobsCancel::NAME), "reason": "profile" }),
                    ));
                }
                self.jobs.lock().unwrap().cancel(&p.job_id)?;
                info!(job = %p.job_id, uid, "cancel requested");
                job_ref(Some(p.job_id))
            }
        }
    }
}

impl Service {
    /// The running generation's catalog index (resolved now, never the caller's copy).
    fn catalog(&self) -> Result<hinteros_api::catalog::Index, ErrorObject> {
        hinteros_api::catalog::Index::load(&self.args.catalog_index()).map_err(|e| {
            warn!("catalog: {e}");
            ErrorObject::new(
                ErrorCode::InternalError,
                format!("this system has no usable app catalog: {e}"),
            )
        })
    }
}

fn mem_available() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text.lines().find(|l| l.starts_with("MemAvailable:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// SHA256 fingerprints of `ssh_host_*_key.pub` in `dir` (sorted).
pub fn host_key_fingerprints(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with("ssh_host_") && n.ends_with("_key.pub")
        })
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path()).ok()?;
            let data = text.split_whitespace().nth(1)?;
            let blob = base64::engine::general_purpose::STANDARD
                .decode(data)
                .ok()?;
            Some(format!(
                "SHA256:{}",
                base64::engine::general_purpose::STANDARD_NO_PAD.encode(Sha256::digest(&blob))
            ))
        })
        .collect();
    out.sort();
    out.truncate(8);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_key_fingerprint() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("ssh_host_ed25519_key.pub"),
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINraSens892Rb1jPRy00F9HKBgAjXCmwsXHmcTtlB7ep root@box\n",
        )
        .unwrap();
        std::fs::write(d.path().join("ssh_host_ed25519_key"), "private").unwrap();
        assert_eq!(
            host_key_fingerprints(d.path()),
            ["SHA256:2CixtE95MNbZHanQGv6hP2D3qfguu9QZfjca2OaupKI"]
        );
    }

    #[test]
    fn request_decoding() {
        let bad = [
            ("ssh.set", json!({"enabled": true, "uid": 0})),
            ("ssh.set", json!({"enabled": "yes"})),
            ("rebuild", json!({"path": "/"})),
            ("update.apply", json!({"rev": "HEAD"})),
            (
                "update.apply",
                json!({"rev": "0f3c5a9e2b7d4c1a8e6f9b0d2c4e6a8b1d3f5a7C"}),
            ),
            ("rollback", json!({"generation": 0})),
            ("rollback", json!({"generation": -1})),
            ("channel.set", json!({"channel": "https://evil/x.git"})),
            (
                "base.set",
                json!({"base": "stable", "acknowledge_downgrade": "yes"}),
            ),
            ("jobs.cancel", json!({"job_id": "../../x"})),
            ("settings.set", json!({"revision": "x", "changes": {}})),
            (
                "settings.set",
                json!({"revision": "0".repeat(64), "changes": {"nix": {}}}),
            ),
            ("settings.set", json!({"revision": "0".repeat(64)})),
        ];
        for (m, p) in bad {
            let e = Request::decode(m, Some(p.clone()))
                .err()
                .unwrap_or_else(|| panic!("{m} {p} accepted"));
            assert_eq!(e.kind(), Some(ErrorCode::InvalidParams), "{m} {p}");
        }
        let e = Request::decode("ssh.set", Some(json!({"enabled": true, "uid": 0})))
            .err()
            .unwrap();
        assert_eq!(e.data.unwrap()["field"], "uid");
        let e = Request::decode(
            "settings.set",
            Some(json!({"revision": "0".repeat(64), "changes": {"nix": 1}})),
        )
        .err()
        .unwrap();
        assert_eq!(e.data.unwrap()["field"], "changes.nix");
        assert!(Request::decode("ssh.set", Some(json!({"enabled": false}))).is_ok());
        assert!(Request::decode("rebuild", None).is_ok());
        assert!(Request::decode("ssh.set", Some(json!([true]))).is_err());
    }
}
