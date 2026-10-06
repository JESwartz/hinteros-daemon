//! The session's state machine: running apps, which one has focus, and what controller input
//! means. Owns the [`FocusControl`], [`InputControl`] and [`Spawner`] and is driven by
//! [`Msg`]s (API requests, Home, controller input, gamescope changes, child exits) and timers
//! ([`Shell::deadline`] / [`Shell::tick`]).
//!
//! Focus rules:
//! - The launcher has focus: gamescope shows the launcher, InputPlumber is in `All` mode and
//!   `ui_*` events become `input_nav`.
//! - An app has focus: gamescope shows that app as soon as it has a window (the launcher until
//!   then), InputPlumber is in `Pass` mode, so input goes to the app (through Steam Input).
//! - Home (Guide, or SIGUSR1): `home_pressed`, then the launcher gets focus. Apps keep running.
//! - If the focused app's windows go away for longer than the debounce (it closed its window,
//!   or a Steam game quit), the launcher gets focus.
//! - Steam itself is the app `steam` (gamescope app id 769): opened as Big Picture through
//!   `apps.launch`, or brought forward with `steam.focus` when one of its windows wants
//!   attention (`steam_attention`); when its windows close, focus goes back to whoever had it.
//! - `apps.close`: SIGTERM now, SIGKILL after the close timeout, then stop tracking.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use hinteros_api::server::{decode_params, encode_result};
use hinteros_api::session::{
    App, AppExitedParams, AppKind, AppSource, AppStartedParams, AppsClose, AppsCloseParams,
    AppsCloseResult, AppsFocus, AppsFocusParams, AppsFocusResult, AppsLaunch, AppsLaunchParams,
    AppsLaunchResult, AppsList, AppsListResult, AppsRunning, AppsRunningResult, EventName,
    ExitReason, FocusChangedParams, GamesList, GamesListResult, HomePressedParams, InputNavParams,
    LaunchRequest, NavAction, NavPhase, RunningApp, STEAM_CLIENT_APP_ID,
    STEAM_CLIENT_GAMESCOPE_APP_ID, STEAM_RUNGAMEID, SteamAttentionParams, SteamAttentionQuery,
    SteamFocus, SteamFocusResult, SteamWindow,
};
use hinteros_api::{ErrorCode, ErrorObject, Method};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::Broadcast;
use crate::apps::{ChildExit, Spawner};
use crate::config::{AppConfig, Config};
use crate::focus::{FocusControl, GamescopeEvent, GamescopeState, baselayer_for};
use crate::input::{InputControl, Intent, InterceptMode, RawInput, Repeat, translate};
use crate::procs::{ProcessControl, Sig};
use crate::steam::library;

/// A request from a connection, with the channel for its result.
pub type Reply = oneshot::Sender<Result<Value, ErrorObject>>;

/// Everything that drives the shell.
#[derive(Debug)]
pub enum Msg {
    /// A method call the shell answers ([`METHODS`]): method name and raw params.
    Request {
        method: String,
        params: Option<Value>,
        reply: Reply,
    },
    /// The Home button (SIGUSR1 in M0, Guide via InputPlumber goes through `Input`).
    Home,
    Input(RawInput),
    Gamescope(GamescopeEvent),
    ChildExit(ChildExit),
}

/// The methods the shell answers (the connection handles the rest).
pub const METHODS: &[&str] = &[
    AppsList::NAME,
    AppsLaunch::NAME,
    AppsRunning::NAME,
    AppsFocus::NAME,
    AppsClose::NAME,
    GamesList::NAME,
    SteamAttentionQuery::NAME,
    SteamFocus::NAME,
];

/// Settings the shell needs, taken from [`Config`].
#[derive(Debug, Clone)]
pub struct ShellConfig {
    pub launcher_app_id: u32,
    pub steam_path: PathBuf,
    /// The configured Steam directory (`None`: found under `$HOME`).
    pub steam_dir: Option<PathBuf>,
    /// Whether gamescope reports state (decides how Steam games are tracked).
    pub gamescope: bool,
    pub steam_launch_timeout: Duration,
    pub focus_loss_debounce: Duration,
    /// How long `apps.close` waits before it kills.
    pub close_timeout: Duration,
    pub repeat_delay: Duration,
    pub repeat_interval: Duration,
    pub apps: Vec<AppConfig>,
}

impl From<&Config> for ShellConfig {
    fn from(c: &Config) -> Self {
        Self {
            launcher_app_id: c.launcher.app_id,
            steam_path: c.steam.path.clone(),
            steam_dir: c.steam.dir.clone(),
            gamescope: c.gamescope.enable,
            steam_launch_timeout: Duration::from_secs(c.steam.launch_timeout_s),
            focus_loss_debounce: Duration::from_millis(c.gamescope.focus_loss_debounce_ms),
            close_timeout: Duration::from_millis(c.close.timeout_ms),
            repeat_delay: c.input.repeat_delay(),
            repeat_interval: c.input.repeat_interval(),
            apps: c.apps.clone(),
        }
    }
}

/// An `apps.close` in progress.
#[derive(Debug, Clone, Copy)]
struct Closing {
    /// SIGKILL was sent (or, for Steam, we stopped waiting).
    killed: bool,
    deadline: Instant,
}

#[derive(Debug)]
struct Running {
    info: RunningApp,
    started: Instant,
    /// gamescope listed it in GAMESCOPE_FOCUSABLE_APPS (it has an Xwayland window).
    seen_focusable: bool,
    /// gamescope reported it as GAMESCOPE_FOCUSED_APP.
    seen_focused: bool,
    /// Tracked by its windows: since when they are gone (exit after the debounce).
    gone_since: Option<Instant>,
    closing: Option<Closing>,
}

/// Whether an app's lifetime is its windows (Steam games and Steam itself), rather than a
/// process we started.
fn tracked_by_windows(source: AppSource) -> bool {
    matches!(source, AppSource::Steam | AppSource::SteamClient)
}

/// The running-app entry for Steam itself.
fn steam_client_app() -> RunningApp {
    RunningApp {
        id: STEAM_CLIENT_APP_ID.into(),
        name: "Steam".into(),
        source: AppSource::SteamClient,
        gamescope_app_id: STEAM_CLIENT_GAMESCOPE_APP_ID,
        pid: None,
    }
}

pub struct Shell {
    cfg: ShellConfig,
    focus: Box<dyn FocusControl>,
    input: Box<dyn InputControl>,
    spawner: Box<dyn Spawner>,
    procs: Box<dyn ProcessControl>,
    events: broadcast::Sender<Broadcast>,
    apps: BTreeMap<String, Running>,
    /// `None`: the launcher.
    focused: Option<String>,
    repeat: Repeat,
    gamescope: Option<GamescopeState>,
    /// The focused app's window is gone; fall back to the launcher at this time.
    focus_lost_at: Option<Instant>,
    /// The last few focus orders we wrote; gamescope state can lag behind our writes.
    written: std::collections::VecDeque<Vec<u32>>,
    next_command: u64,
    /// Steam's mapped dialogs and main windows (from the gamescope watcher).
    steam_windows: Vec<SteamWindow>,
    /// Steam windows the user has seen (Steam had focus while they were open).
    seen_windows: BTreeSet<u32>,
    /// What steam_attention last said.
    attention: Vec<SteamWindow>,
    /// Who gets focus back when Steam, focused through steam.focus, closes its windows.
    steam_return: Option<String>,
}

impl Shell {
    pub fn new(
        cfg: ShellConfig,
        focus: Box<dyn FocusControl>,
        input: Box<dyn InputControl>,
        spawner: Box<dyn Spawner>,
        procs: Box<dyn ProcessControl>,
        events: broadcast::Sender<Broadcast>,
    ) -> Self {
        let repeat = Repeat::new(cfg.repeat_delay, cfg.repeat_interval);
        Self {
            cfg,
            focus,
            input,
            spawner,
            procs,
            events,
            apps: BTreeMap::new(),
            focused: None,
            repeat,
            gamescope: None,
            focus_lost_at: None,
            written: std::collections::VecDeque::new(),
            next_command: 1,
            steam_windows: Vec::new(),
            seen_windows: BTreeSet::new(),
            attention: Vec::new(),
            steam_return: None,
        }
    }

    /// Apply the initial focus (the launcher). Call once at start.
    pub fn start(&mut self) {
        self.apply_focus();
    }

    pub fn focused(&self) -> Option<&str> {
        self.focused.as_deref()
    }

    pub fn handle(&mut self, msg: Msg, now: Instant) {
        match msg {
            Msg::Request {
                method,
                params,
                reply,
            } => {
                let result = self.request(&method, params, now);
                let _ = reply.send(result);
            }
            Msg::Home => self.home(),
            Msg::Input(input) => self.input(input, now),
            Msg::Gamescope(event) => self.gamescope(event, now),
            Msg::ChildExit(exit) => self.child_exit(exit),
        }
    }

    pub fn request(
        &mut self,
        method: &str,
        params: Option<Value>,
        now: Instant,
    ) -> Result<Value, ErrorObject> {
        match method {
            AppsList::NAME => {
                decode_params::<AppsList>(params)?;
                encode_result::<AppsList>(self.list())
            }
            AppsLaunch::NAME => {
                let p = decode_params::<AppsLaunch>(params)?;
                encode_result::<AppsLaunch>(self.launch(p, now)?)
            }
            AppsRunning::NAME => {
                decode_params::<AppsRunning>(params)?;
                encode_result::<AppsRunning>(self.running())
            }
            AppsFocus::NAME => {
                let p = decode_params::<AppsFocus>(params)?;
                encode_result::<AppsFocus>(self.focus_request(p)?)
            }
            AppsClose::NAME => {
                let p = decode_params::<AppsClose>(params)?;
                encode_result::<AppsClose>(self.close(p, now)?)
            }
            GamesList::NAME => {
                decode_params::<GamesList>(params)?;
                encode_result::<GamesList>(GamesListResult {
                    games: self
                        .steam_dir()
                        .map(|dir| library::scan(&dir))
                        .unwrap_or_default(),
                })
            }
            SteamAttentionQuery::NAME => {
                decode_params::<SteamAttentionQuery>(params)?;
                encode_result::<SteamAttentionQuery>(SteamAttentionParams {
                    windows: self.attention.clone(),
                })
            }
            SteamFocus::NAME => {
                decode_params::<SteamFocus>(params)?;
                encode_result::<SteamFocus>(self.steam_focus(now)?)
            }
            _ => Err(hinteros_api::server::method_not_found(method)),
        }
    }

    fn emit<T: Serialize>(&self, name: EventName, params: T) {
        match serde_json::to_value(params) {
            // No receivers is fine: nobody is connected.
            Ok(v) => drop(self.events.send((name, v))),
            Err(e) => warn!("encoding {name:?}: {e}"),
        }
    }

    fn steam_dir(&self) -> Option<PathBuf> {
        library::default_steam_dir(self.cfg.steam_dir.as_deref())
    }

    fn list(&self) -> AppsListResult {
        let steam = App {
            id: STEAM_CLIENT_APP_ID.into(),
            name: "Steam".into(),
            kind: AppKind::Native,
            icon: None,
        };
        if self.cfg.apps.is_empty() {
            let mut list = stub_apps();
            list.apps.insert(0, steam);
            return list;
        }
        AppsListResult {
            apps: std::iter::once(steam)
                .chain(self.cfg.apps.iter().map(|a| App {
                    id: a.id.clone(),
                    name: a.name.clone(),
                    kind: a.kind,
                    icon: a.icon.clone(),
                }))
                .collect(),
        }
    }

    fn running(&self) -> AppsRunningResult {
        AppsRunningResult {
            apps: self.apps.values().map(|r| r.info.clone()).collect(),
            focused: self.focused.clone(),
        }
    }

    fn not_running(id: &str) -> ErrorObject {
        ErrorObject::new(
            ErrorCode::AppNotFound,
            format!("no running app with id '{id}'"),
        )
    }

    fn focus_request(&mut self, p: AppsFocusParams) -> Result<AppsFocusResult, ErrorObject> {
        if let Some(id) = &p.id
            && !self.apps.contains_key(id)
        {
            return Err(Self::not_running(id));
        }
        self.set_focus(p.id);
        Ok(AppsFocusResult {})
    }

    fn launch(
        &mut self,
        p: AppsLaunchParams,
        now: Instant,
    ) -> Result<AppsLaunchResult, ErrorObject> {
        let request = p
            .request()
            .map_err(|e| ErrorObject::new(ErrorCode::InvalidParams, e))?;
        // Resolve to (key, name, what to run).
        let (key, name, target) = match request {
            LaunchRequest::Id(id) if id == STEAM_CLIENT_APP_ID => {
                (id, "Steam".to_string(), Target::SteamClient)
            }
            LaunchRequest::Id(id) => {
                let app = self.cfg.apps.iter().find(|a| a.id == id).ok_or_else(|| {
                    let stub =
                        self.cfg.apps.is_empty() && stub_apps().apps.iter().any(|a| a.id == id);
                    if stub {
                        ErrorObject::new(
                            ErrorCode::AppLaunchFailed,
                            format!("'{id}' is a stub entry; configure [[apps]] to launch it"),
                        )
                    } else {
                        ErrorObject::new(ErrorCode::AppNotFound, format!("no app with id '{id}'"))
                    }
                })?;
                let target = match (&app.command, app.steam_app_id) {
                    (Some(argv), _) => Target::Command(argv.clone()),
                    (None, Some(steam)) => Target::Steam(steam),
                    (None, None) => unreachable!("validated config"),
                };
                (app.id.clone(), app.name.clone(), target)
            }
            LaunchRequest::Command { argv, name } => {
                let key = format!("cmd-{}", self.next_command);
                let name = name.unwrap_or_else(|| {
                    std::path::Path::new(&argv[0])
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_else(|| argv[0].clone())
                });
                (key, name, Target::Command(argv))
            }
            LaunchRequest::Steam(id) => {
                let name = self
                    .steam_dir()
                    .and_then(|dir| library::find_manifest(&dir, id))
                    .map(|m| m.name)
                    .unwrap_or_else(|| format!("Steam game {id}"));
                (format!("steam-{id}"), name, Target::Steam(id))
            }
        };
        // A Steam game started by id and by URL is the same game.
        let existing = match target {
            Target::Steam(steam) => self
                .apps
                .values()
                .find(|r| r.info.source == AppSource::Steam && r.info.gamescope_app_id == steam)
                .map(|r| r.info.id.clone()),
            Target::Command(_) | Target::SteamClient => {
                self.apps.contains_key(&key).then(|| key.clone())
            }
        };
        if let Some(id) = existing {
            info!(app = %id, "already running; focusing it");
            self.set_focus(Some(id.clone()));
            return Ok(AppsLaunchResult {
                app: self.apps[&id].info.clone(),
            });
        }

        let failed = |e: std::io::Error, what: &str| {
            ErrorObject::new(
                ErrorCode::AppLaunchFailed,
                format!("cannot start {what}: {e}"),
            )
        };
        let steam_path = self.cfg.steam_path.to_string_lossy().into_owned();
        let info = match target {
            Target::Command(argv) => {
                let gamescope_app_id = self.allocate_app_id();
                let pid = self
                    .spawner
                    .spawn(&key, &argv, Some(gamescope_app_id))
                    .map_err(|e| failed(e, &argv[0]))?;
                if key.starts_with("cmd-") {
                    self.next_command += 1;
                }
                RunningApp {
                    id: key.clone(),
                    name,
                    source: AppSource::Command,
                    gamescope_app_id,
                    pid: Some(pid),
                }
            }
            Target::Steam(steam) => {
                let argv = vec![
                    steam_path.clone(),
                    "-silent".into(),
                    format!("{STEAM_RUNGAMEID}{steam}"),
                ];
                // Not under our reaper: the game runs under Steam's own reaper, which tags it
                // with its Steam app id, and an outer reaper would override that.
                self.spawner
                    .spawn(&key, &argv, None)
                    .map_err(|e| failed(e, &steam_path))?;
                RunningApp {
                    id: key.clone(),
                    name,
                    source: AppSource::Steam,
                    gamescope_app_id: steam,
                    pid: None,
                }
            }
            Target::SteamClient => {
                // A running client opens Big Picture for the URL; otherwise start Steam
                // straight into it. Never under a reaper (games must get Steam's app ids).
                let arg = if self.procs.steam_running() {
                    "steam://open/bigpicture"
                } else {
                    "-gamepadui"
                };
                self.spawner
                    .spawn(&key, &[steam_path.clone(), arg.into()], None)
                    .map_err(|e| failed(e, &steam_path))?;
                // Opened from the launcher: closing it goes back there.
                self.steam_return = None;
                steam_client_app()
            }
        };
        info!(app = %info.id, gamescope_app_id = info.gamescope_app_id, "launched");
        self.track(info.clone(), now);
        self.set_focus(Some(key));
        Ok(AppsLaunchResult { app: info })
    }

    /// Start tracking a running app and announce it.
    fn track(&mut self, info: RunningApp, now: Instant) {
        self.apps.insert(
            info.id.clone(),
            Running {
                info: info.clone(),
                started: now,
                seen_focusable: false,
                seen_focused: false,
                gone_since: None,
                closing: None,
            },
        );
        self.emit(EventName::AppStarted, AppStartedParams { app: info });
    }

    /// A gamescope app id for a command app: above the launcher's, skipping ids in use.
    fn allocate_app_id(&self) -> u32 {
        let in_use = |id: u32| {
            id == 0
                || id == STEAM_CLIENT_GAMESCOPE_APP_ID
                || id == self.cfg.launcher_app_id
                || self.apps.values().any(|r| r.info.gamescope_app_id == id)
        };
        let mut id = self.cfg.launcher_app_id;
        loop {
            id = id.wrapping_add(1);
            if !in_use(id) {
                return id;
            }
        }
    }

    /// `apps.close`: ask the app to quit now, force it at the deadline (see [`Shell::tick`]).
    fn close(&mut self, p: AppsCloseParams, now: Instant) -> Result<AppsCloseResult, ErrorObject> {
        let Some(app) = self.apps.get(&p.id) else {
            return Err(Self::not_running(&p.id));
        };
        if app.closing.is_some() {
            return Ok(AppsCloseResult {});
        }
        let id = p.id;
        let info = app.info.clone();
        info!(app = %id, "closing");
        match info.source {
            AppSource::Command => {
                let Some(pid) = info.pid else {
                    self.exited(&id, ExitReason::Closed, None);
                    return Ok(AppsCloseResult {});
                };
                // The reaper forwards it to the app's process group.
                self.procs.signal(pid, Sig::Term);
            }
            AppSource::Steam => {
                let procs = self.steam_game_processes(info.gamescope_app_id);
                if procs.is_empty() {
                    info!(app = %id, "no game processes (yet); no longer tracking it");
                    self.exited(&id, ExitReason::Closed, None);
                    return Ok(AppsCloseResult {});
                }
                for pid in procs {
                    self.procs.signal(pid, Sig::Term);
                }
            }
            AppSource::SteamClient => {
                let steam_path = self.cfg.steam_path.to_string_lossy().into_owned();
                let argv = [steam_path, "steam://close/bigpicture".into()];
                if let Err(e) = self.spawner.spawn("steam-close", &argv, None) {
                    warn!("cannot ask Steam to close Big Picture: {e}");
                }
                if !self.cfg.gamescope {
                    // Nothing to watch: consider it closed.
                    self.exited(&id, ExitReason::Closed, None);
                    return Ok(AppsCloseResult {});
                }
            }
        }
        if let Some(app) = self.apps.get_mut(&id) {
            app.closing = Some(Closing {
                killed: false,
                deadline: now + self.cfg.close_timeout,
            });
        }
        Ok(AppsCloseResult {})
    }

    /// Every process under Steam's reaper(s) for a game.
    fn steam_game_processes(&mut self, app_id: u32) -> Vec<u32> {
        let reapers = self.procs.steam_reapers(app_id);
        reapers
            .into_iter()
            .flat_map(|r| self.procs.descendants(r))
            .collect()
    }

    /// The close deadline passed: SIGKILL what is left; the second time, stop tracking it.
    fn force_close(&mut self, id: &str, now: Instant) {
        let Some(app) = self.apps.get(id) else { return };
        let info = app.info.clone();
        if app.closing.is_some_and(|c| c.killed) {
            warn!(
                app = id,
                "still not gone after SIGKILL; no longer tracking it"
            );
            self.exited(id, ExitReason::Closed, None);
            return;
        }
        match info.source {
            AppSource::Command => {
                if let Some(pid) = info.pid {
                    let tree = self.procs.descendants(pid);
                    info!(app = id, processes = tree.len(), "did not quit; killing");
                    for p in tree {
                        self.procs.signal(p, Sig::Kill);
                    }
                }
            }
            AppSource::Steam => {
                let tree = self.steam_game_processes(info.gamescope_app_id);
                info!(app = id, processes = tree.len(), "did not quit; killing");
                for p in tree {
                    self.procs.signal(p, Sig::Kill);
                }
            }
            AppSource::SteamClient => {
                // Never kill Steam itself.
                info!(app = id, "Big Picture did not close; no longer tracking it");
                self.exited(id, ExitReason::Closed, None);
                return;
            }
        }
        if let Some(app) = self.apps.get_mut(id) {
            app.closing = Some(Closing {
                killed: true,
                deadline: now + self.cfg.close_timeout,
            });
        }
    }

    /// `steam.focus`: show Steam's windows, with Steam tracked as the app `steam`.
    fn steam_focus(&mut self, now: Instant) -> Result<SteamFocusResult, ErrorObject> {
        if self.steam_windows.is_empty() {
            return Err(ErrorObject::new(
                ErrorCode::SteamNoWindows,
                "Steam has no windows to show",
            ));
        }
        if !self.apps.contains_key(STEAM_CLIENT_APP_ID) {
            self.track(steam_client_app(), now);
        }
        let previous = self.focused.clone();
        if previous.as_deref() != Some(STEAM_CLIENT_APP_ID) {
            self.set_focus(Some(STEAM_CLIENT_APP_ID.into()));
            self.steam_return = previous;
        }
        Ok(SteamFocusResult {
            app: self.apps[STEAM_CLIENT_APP_ID].info.clone(),
        })
    }

    /// Give focus to `target` (`None`: the launcher): tell gamescope and InputPlumber, and
    /// announce it if it changed.
    fn set_focus(&mut self, target: Option<String>) {
        self.repeat.clear();
        self.focus_lost_at = None;
        let changed = self.focused != target;
        self.focused = target;
        self.apply_focus();
        if self.focused.as_deref() == Some(STEAM_CLIENT_APP_ID) {
            // The user now sees Steam's windows.
            self.seen_windows
                .extend(self.steam_windows.iter().map(|w| w.window));
        } else {
            self.steam_return = None;
        }
        if changed {
            info!(app = ?self.focused, "focus");
            self.emit(
                EventName::FocusChanged,
                FocusChangedParams {
                    app: self.focused.clone(),
                },
            );
        }
        self.update_attention();
    }

    /// (Re)apply the current focus to gamescope and InputPlumber.
    fn apply_focus(&mut self) {
        let target = self
            .focused
            .as_ref()
            .and_then(|id| self.apps.get(id))
            .map(|r| r.info.gamescope_app_id);
        if self.written.len() == 4 {
            self.written.pop_front();
        }
        self.written
            .push_back(baselayer_for(target, self.cfg.launcher_app_id));
        let result = match target {
            Some(id) => self.focus.focus_app(id),
            None => self.focus.focus_launcher(),
        };
        if let Err(e) = result {
            warn!("setting gamescope focus: {e:#}");
        }
        self.input.set_mode(match target {
            Some(_) => InterceptMode::Pass,
            None => InterceptMode::All,
        });
    }

    /// New list of Steam's windows from the gamescope watcher.
    fn steam_windows(&mut self, windows: Vec<SteamWindow>) {
        self.seen_windows
            .retain(|w| windows.iter().any(|s| s.window == *w));
        if self.focused.as_deref() == Some(STEAM_CLIENT_APP_ID) {
            self.seen_windows.extend(windows.iter().map(|w| w.window));
        }
        self.steam_windows = windows;
        self.update_attention();
    }

    /// Send steam_attention if the list of unseen Steam windows changed.
    fn update_attention(&mut self) {
        let attention: Vec<SteamWindow> = self
            .steam_windows
            .iter()
            .filter(|w| !self.seen_windows.contains(&w.window))
            .cloned()
            .collect();
        if attention != self.attention {
            info!(windows = ?attention, "Steam attention");
            self.attention = attention.clone();
            self.emit(
                EventName::SteamAttention,
                SteamAttentionParams { windows: attention },
            );
        }
    }

    fn home(&mut self) {
        info!("Home");
        self.emit(EventName::HomePressed, HomePressedParams {});
        self.set_focus(None);
    }

    fn input(&mut self, input: RawInput, now: Instant) {
        let Some(intent) = translate(&input.event) else {
            debug!(event = %input.event, "unmapped input");
            return;
        };
        match intent {
            Intent::Home => {
                if input.pressed() {
                    self.home();
                }
            }
            // Only the launcher gets navigation (an app gets the pad itself, in Pass mode).
            Intent::Nav(_) if self.focused.is_some() => {}
            Intent::Nav(action) => {
                let phase = if input.pressed() {
                    self.repeat.press(action, &input.source, now);
                    NavPhase::Press
                } else {
                    self.repeat.release(action, &input.source);
                    NavPhase::Release
                };
                self.nav(action, phase);
            }
        }
    }

    fn nav(&self, action: NavAction, phase: NavPhase) {
        self.emit(EventName::InputNav, InputNavParams { action, phase });
    }

    fn child_exit(&mut self, exit: ChildExit) {
        let Some(app) = self.apps.get(&exit.key) else {
            return;
        };
        let reason = match exit.code {
            Some(0) => ExitReason::Exited,
            _ => ExitReason::Crashed,
        };
        match app.info.source {
            AppSource::Command if app.info.pid == Some(exit.pid) => {
                self.exited(&exit.key, reason, exit.code);
            }
            AppSource::Command => {}
            // The steam command only hands the URL to the running client. Without gamescope
            // there is no way to see the game, so its end stands in for the game's.
            AppSource::Steam | AppSource::SteamClient if !self.cfg.gamescope => {
                self.exited(&exit.key, reason, exit.code);
            }
            AppSource::Steam | AppSource::SteamClient => match exit.code {
                Some(0) => debug!(app = %exit.key, "steam command done"),
                code => warn!(app = %exit.key, ?code, "steam command failed"),
            },
        }
    }

    /// An app ended (`closed` instead of `reason` if apps.close asked it to).
    fn exited(&mut self, id: &str, reason: ExitReason, exit_code: Option<i32>) {
        let Some(app) = self.apps.remove(id) else {
            return;
        };
        let reason = if app.closing.is_some() {
            ExitReason::Closed
        } else {
            reason
        };
        info!(app = id, ?reason, ?exit_code, "exited");
        self.emit(
            EventName::AppExited,
            AppExitedParams {
                id: id.into(),
                reason,
                exit_code,
            },
        );
        if self.focused.as_deref() == Some(id) {
            let back = if id == STEAM_CLIENT_APP_ID {
                self.steam_return
                    .take()
                    .filter(|r| self.apps.contains_key(r))
            } else {
                None
            };
            self.set_focus(back);
        }
    }

    fn gamescope(&mut self, event: GamescopeEvent, now: Instant) {
        match event {
            GamescopeEvent::Connected => {
                info!("gamescope connected; applying focus");
                self.apply_focus();
            }
            GamescopeEvent::Disconnected => {
                self.gamescope = None;
                self.focus_lost_at = None;
                self.steam_windows(Vec::new());
            }
            GamescopeEvent::State(state) => {
                if !state.baselayer_apps.is_empty() && !self.written.contains(&state.baselayer_apps)
                {
                    warn!(
                        found = ?state.baselayer_apps,
                        expected = ?self.written.back(),
                        "someone else changed GAMESCOPECTRL_BASELAYER_APPID (Steam?)"
                    );
                }
                self.gamescope = Some(state);
                self.observe(now);
            }
            GamescopeEvent::SteamWindows(windows) => self.steam_windows(windows),
        }
    }

    /// Update app visibility from the latest gamescope state and arm the timers.
    fn observe(&mut self, now: Instant) {
        let Some(state) = &self.gamescope else { return };
        let focused = self.focused.clone();
        for (id, app) in &mut self.apps {
            let gs = app.info.gamescope_app_id;
            let listed = state.focusable_apps.contains(&gs);
            let is_focused = state.focused_app == Some(gs);
            app.seen_focusable |= listed;
            app.seen_focused |= is_focused;
            if tracked_by_windows(app.info.source) {
                // Gone when the Xwayland windows it had are gone. (A native Wayland game is
                // never listed; the focus-loss rule below covers it.)
                let gone = app.seen_focusable && !listed && !is_focused;
                app.gone_since = match (gone, app.gone_since) {
                    (true, None) => Some(now),
                    (true, since) => since,
                    (false, _) => None,
                };
            }
            if focused.as_deref() == Some(id) {
                let lost = app.seen_focused && !is_focused;
                self.focus_lost_at = match (lost, self.focus_lost_at) {
                    (true, None) => Some(now + self.cfg.focus_loss_debounce),
                    (true, at) => at,
                    (false, _) => None,
                };
            }
        }
    }

    /// Whether a window-tracked app that never showed a window has run out of time.
    fn launch_deadline(&self, app: &Running) -> Option<Instant> {
        let timeout = self.cfg.steam_launch_timeout;
        (tracked_by_windows(app.info.source)
            && self.cfg.gamescope
            && !timeout.is_zero()
            && !app.seen_focusable
            && !app.seen_focused)
            .then(|| app.started + timeout)
    }

    /// When [`Shell::tick`] has something to do next.
    pub fn deadline(&self) -> Option<Instant> {
        let debounce = self.cfg.focus_loss_debounce;
        self.apps
            .values()
            .flat_map(|app| {
                [
                    app.gone_since.map(|since| since + debounce),
                    if app.gone_since.is_none() {
                        self.launch_deadline(app)
                    } else {
                        None
                    },
                    app.closing.map(|c| c.deadline),
                ]
            })
            .flatten()
            .chain(self.repeat.deadline())
            .chain(self.focus_lost_at)
            .min()
    }

    pub fn tick(&mut self, now: Instant) {
        if let Some(action) = self.repeat.tick(now) {
            if self.focused.is_none() {
                self.nav(action, NavPhase::Repeat);
            } else {
                self.repeat.clear();
            }
        }

        let debounce = self.cfg.focus_loss_debounce;
        let mut ended = Vec::new();
        let mut force = Vec::new();
        for (id, app) in &self.apps {
            if app.gone_since.is_some_and(|since| now >= since + debounce) {
                ended.push((id.clone(), ExitReason::Exited));
            } else if app.gone_since.is_none()
                && self.launch_deadline(app).is_some_and(|at| now >= at)
            {
                ended.push((id.clone(), ExitReason::LaunchTimeout));
            } else if app.closing.is_some_and(|c| now >= c.deadline) {
                force.push(id.clone());
            }
        }
        for (id, reason) in ended {
            self.exited(&id, reason, None);
        }
        for id in force {
            self.force_close(&id, now);
        }

        if self.focus_lost_at.is_some_and(|at| now >= at) {
            self.focus_lost_at = None;
            if let Some(id) = self.focused.clone() {
                let native = self
                    .apps
                    .get(&id)
                    .is_some_and(|a| tracked_by_windows(a.info.source) && !a.seen_focusable);
                if native {
                    // Seen only as focused, never as an Xwayland window: losing focus while
                    // first in line means it has no window any more.
                    info!(app = %id, "window is gone");
                    self.exited(&id, ExitReason::Exited, None);
                } else {
                    info!(app = %id, "focused app has no window any more; back to the launcher");
                    self.set_focus(None);
                }
            }
        }
    }
}

enum Target {
    Command(Vec<String>),
    Steam(u32),
    SteamClient,
}

/// What `apps.list` returns after the built-in Steam entry when no apps are configured (M0
/// stand-in).
pub fn stub_apps() -> AppsListResult {
    let app = |id: &str, name: &str, kind| App {
        id: id.into(),
        name: name.into(),
        kind,
        icon: None,
    };
    AppsListResult {
        apps: vec![
            app("jellyfin", "Jellyfin", AppKind::Native),
            app("youtube", "YouTube", AppKind::Web),
            app("crunchyroll", "Crunchyroll", AppKind::Web),
            app("firefox", "Firefox", AppKind::Native),
        ],
    }
}

/// Run the shell until `msgs` closes.
pub async fn run(mut shell: Shell, mut msgs: mpsc::UnboundedReceiver<Msg>) {
    shell.start();
    loop {
        let deadline = shell.deadline();
        tokio::select! {
            msg = msgs.recv() => match msg {
                Some(msg) => shell.handle(msg, Instant::now()),
                None => break,
            },
            () = sleep_until(deadline) => shell.tick(Instant::now()),
        }
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use hinteros_api::session::AppsLaunchResult;
    use serde_json::json;

    use super::*;

    /// Everything the shell did to the outside world, in order.
    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        FocusLauncher,
        FocusApp(u32),
        Mode(InterceptMode),
        Spawn {
            key: String,
            argv: Vec<String>,
            app_id: Option<u32>,
        },
        Signal(u32, Sig),
    }

    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<Call>>>);

    impl Log {
        fn push(&self, c: Call) {
            self.0.lock().unwrap().push(c);
        }
        fn take(&self) -> Vec<Call> {
            std::mem::take(&mut self.0.lock().unwrap())
        }
    }

    struct MockFocus(Log);
    impl FocusControl for MockFocus {
        fn focusable_apps(&mut self) -> anyhow::Result<Vec<u32>> {
            Ok(vec![])
        }
        fn focused_app(&mut self) -> anyhow::Result<Option<u32>> {
            Ok(None)
        }
        fn focus_launcher(&mut self) -> anyhow::Result<()> {
            self.0.push(Call::FocusLauncher);
            Ok(())
        }
        fn focus_app(&mut self, app_id: u32) -> anyhow::Result<()> {
            self.0.push(Call::FocusApp(app_id));
            Ok(())
        }
    }

    struct MockInput(Log);
    impl InputControl for MockInput {
        fn set_mode(&mut self, mode: InterceptMode) {
            self.0.push(Call::Mode(mode));
        }
    }

    struct MockSpawner {
        log: Log,
        next_pid: u32,
        fail: bool,
    }
    impl Spawner for MockSpawner {
        fn spawn(
            &mut self,
            key: &str,
            argv: &[String],
            app_id: Option<u32>,
        ) -> std::io::Result<u32> {
            if self.fail {
                return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "nope"));
            }
            self.log.push(Call::Spawn {
                key: key.into(),
                argv: argv.to_vec(),
                app_id,
            });
            self.next_pid += 1;
            Ok(self.next_pid)
        }
    }

    /// Fake process table: reaper pid -> descendants, Steam reapers by app id, Steam running.
    #[derive(Clone, Default)]
    struct ProcTable {
        trees: BTreeMap<u32, Vec<u32>>,
        steam_reapers: BTreeMap<u32, Vec<u32>>,
        steam_running: bool,
    }

    struct MockProcs {
        log: Log,
        table: Arc<Mutex<ProcTable>>,
    }
    impl ProcessControl for MockProcs {
        fn signal(&mut self, pid: u32, sig: Sig) {
            self.log.push(Call::Signal(pid, sig));
        }
        fn descendants(&mut self, pid: u32) -> Vec<u32> {
            self.table
                .lock()
                .unwrap()
                .trees
                .get(&pid)
                .cloned()
                .unwrap_or_default()
        }
        fn steam_reapers(&mut self, app_id: u32) -> Vec<u32> {
            let t = self.table.lock().unwrap();
            t.steam_reapers.get(&app_id).cloned().unwrap_or_default()
        }
        fn steam_running(&mut self) -> bool {
            self.table.lock().unwrap().steam_running
        }
    }

    const L: u32 = 4000;
    const MS: fn(u64) -> Duration = Duration::from_millis;

    struct T {
        shell: Shell,
        log: Log,
        procs: Arc<Mutex<ProcTable>>,
        events: broadcast::Receiver<Broadcast>,
        now: Instant,
    }

    impl T {
        fn new() -> Self {
            Self::with(|_| {})
        }

        fn with(f: impl FnOnce(&mut ShellConfig)) -> Self {
            let mut cfg = ShellConfig {
                launcher_app_id: L,
                steam_path: "/fake/steam".into(),
                steam_dir: Some(
                    crate::steam::library::tests::fixture_home().join(".local/share/Steam"),
                ),
                gamescope: true,
                steam_launch_timeout: Duration::from_secs(60),
                focus_loss_debounce: MS(1000),
                close_timeout: MS(5000),
                repeat_delay: MS(400),
                repeat_interval: MS(100),
                apps: vec![
                    AppConfig {
                        id: "jellyfin".into(),
                        name: "Jellyfin".into(),
                        kind: AppKind::Native,
                        icon: None,
                        command: Some(vec!["jellyfin-desktop".into()]),
                        steam_app_id: None,
                    },
                    AppConfig {
                        id: "portal".into(),
                        name: "Portal".into(),
                        kind: AppKind::Native,
                        icon: None,
                        command: None,
                        steam_app_id: Some(400),
                    },
                ],
            };
            f(&mut cfg);
            let log = Log::default();
            let procs = Arc::new(Mutex::new(ProcTable::default()));
            let (tx, events) = broadcast::channel(256);
            let mut shell = Shell::new(
                cfg,
                Box::new(MockFocus(log.clone())),
                Box::new(MockInput(log.clone())),
                Box::new(MockSpawner {
                    log: log.clone(),
                    next_pid: 100,
                    fail: false,
                }),
                Box::new(MockProcs {
                    log: log.clone(),
                    table: procs.clone(),
                }),
                tx,
            );
            shell.start();
            assert_eq!(
                log.take(),
                [Call::FocusLauncher, Call::Mode(InterceptMode::All)]
            );
            Self {
                shell,
                log,
                procs,
                events,
                now: Instant::now(),
            }
        }

        fn events(&mut self) -> Vec<(EventName, Value)> {
            let mut v = vec![];
            while let Ok(e) = self.events.try_recv() {
                v.push(e);
            }
            v
        }

        fn call(&mut self, method: &str, params: Value) -> Result<Value, ErrorObject> {
            self.shell.request(method, Some(params), self.now)
        }

        fn launch(&mut self, params: Value) -> RunningApp {
            let r = self.call("apps.launch", params).unwrap();
            serde_json::from_value::<AppsLaunchResult>(r).unwrap().app
        }

        fn input(&mut self, event: &str, value: f64) {
            self.shell.handle(
                Msg::Input(RawInput {
                    source: "/org/shadowblip/InputPlumber/devices/target/dbus0".into(),
                    event: event.into(),
                    value,
                }),
                self.now,
            );
        }

        fn gamescope(&mut self, focused: Option<u32>, focusable: &[u32]) {
            let state = GamescopeState {
                focused_app: focused,
                focusable_apps: focusable.to_vec(),
                baselayer_apps: vec![],
            };
            self.shell
                .handle(Msg::Gamescope(GamescopeEvent::State(state)), self.now);
        }

        /// Advance the clock and run the timers like `run` does.
        fn advance(&mut self, by: Duration) {
            let end = self.now + by;
            while let Some(at) = self.shell.deadline().filter(|&at| at <= end) {
                self.now = at.max(self.now);
                self.shell.tick(self.now);
            }
            self.now = end;
        }
    }

    fn ev(name: EventName, params: Value) -> (EventName, Value) {
        (name, params)
    }

    #[test]
    fn launch_command_focuses_and_home_returns() {
        let mut t = T::new();
        let app = t.launch(json!({"command": ["/usr/bin/firefox", "--kiosk"]}));
        assert_eq!(app.id, "cmd-1");
        assert_eq!(app.name, "firefox");
        assert_eq!(app.source, AppSource::Command);
        assert_eq!(app.gamescope_app_id, L + 1);
        assert_eq!(app.pid, Some(101));
        assert_eq!(
            t.log.take(),
            [
                Call::Spawn {
                    key: "cmd-1".into(),
                    argv: vec!["/usr/bin/firefox".into(), "--kiosk".into()],
                    app_id: Some(L + 1),
                },
                Call::FocusApp(L + 1),
                Call::Mode(InterceptMode::Pass),
            ]
        );
        assert_eq!(
            t.events(),
            [
                ev(EventName::AppStarted, json!({"app": app})),
                ev(EventName::FocusChanged, json!({"app": "cmd-1"})),
            ]
        );

        // Guide while the app has focus (InputPlumber was in Pass, so only Guide arrives).
        t.input("ui_guide", 1.0);
        t.input("ui_guide", 0.0);
        assert_eq!(
            t.log.take(),
            [Call::FocusLauncher, Call::Mode(InterceptMode::All)]
        );
        assert_eq!(
            t.events(),
            [
                ev(EventName::HomePressed, json!({})),
                ev(EventName::FocusChanged, json!({"app": null})),
            ]
        );

        // The app keeps running and can be focused again.
        let r = t.call("apps.running", json!({})).unwrap();
        assert_eq!(r, json!({"apps": [app], "focused": null}));
        t.call("apps.focus", json!({"id": "cmd-1"})).unwrap();
        assert_eq!(
            t.log.take(),
            [Call::FocusApp(L + 1), Call::Mode(InterceptMode::Pass)]
        );

        // It exits with an error: app_exited, launcher back.
        t.shell.handle(
            Msg::ChildExit(ChildExit {
                key: "cmd-1".into(),
                pid: 101,
                code: Some(139),
            }),
            t.now,
        );
        let events = t.events();
        assert_eq!(
            events[1..],
            [
                ev(
                    EventName::AppExited,
                    json!({"id": "cmd-1", "reason": "crashed", "exit_code": 139})
                ),
                ev(EventName::FocusChanged, json!({"app": null})),
            ]
        );
        assert_eq!(
            t.call("apps.running", json!({})).unwrap(),
            json!({"apps": [], "focused": null})
        );
        // Next command gets the next number.
        assert_eq!(t.launch(json!({"command": ["x"]})).id, "cmd-2");
    }

    #[test]
    fn steam_game_lifecycle_through_gamescope() {
        let mut t = T::new();
        let app = t.launch(json!({"url": "steam://rungameid/570"}));
        assert_eq!(app.id, "steam-570");
        assert_eq!(app.gamescope_app_id, 570);
        assert_eq!(app.pid, None);
        assert_eq!(
            t.log.take(),
            [
                Call::Spawn {
                    key: "steam-570".into(),
                    argv: vec![
                        "/fake/steam".into(),
                        "-silent".into(),
                        "steam://rungameid/570".into()
                    ],
                    app_id: None,
                },
                Call::FocusApp(570),
                Call::Mode(InterceptMode::Pass),
            ]
        );
        t.events();

        // The steam command returns at once; that is not the game ending.
        t.shell.handle(
            Msg::ChildExit(ChildExit {
                key: "steam-570".into(),
                pid: 101,
                code: Some(0),
            }),
            t.now,
        );
        assert!(t.events().is_empty());

        // Launching it again focuses the running one, by URL or by id.
        assert_eq!(t.launch(json!({"url": "steam://rungameid/570"})), app);
        assert!(
            t.log
                .take()
                .iter()
                .all(|c| !matches!(c, Call::Spawn { .. }))
        );

        // Before the game window exists gamescope shows the launcher: not a focus loss.
        t.gamescope(Some(L), &[]);
        t.advance(MS(5000));
        assert_eq!(t.shell.focused(), Some("steam-570"));

        // The game shows up, then (splash -> main window) briefly vanishes: debounced.
        t.gamescope(Some(570), &[570]);
        t.gamescope(Some(L), &[]);
        t.advance(MS(500));
        t.gamescope(Some(570), &[570]);
        t.advance(MS(5000));
        assert_eq!(t.shell.focused(), Some("steam-570"));
        assert!(t.events().is_empty());

        // Home, game in the background: still running while listed.
        t.input("ui_guide", 1.0);
        t.gamescope(Some(L), &[570]);
        t.advance(MS(5000));
        t.events();
        assert_eq!(
            t.call("apps.running", json!({})).unwrap()["apps"][0]["id"],
            "steam-570"
        );

        // The game quits: its windows are gone for longer than the debounce.
        t.gamescope(Some(L), &[]);
        t.advance(MS(999));
        assert!(t.events().is_empty());
        t.advance(MS(1));
        assert_eq!(
            t.events(),
            [ev(
                EventName::AppExited,
                json!({"id": "steam-570", "reason": "exited"})
            )]
        );
    }

    #[test]
    fn steam_game_quits_while_focused() {
        let mut t = T::new();
        t.launch(json!({"id": "portal"}));
        t.gamescope(Some(400), &[400]);
        t.events();
        t.log.take();
        t.gamescope(Some(L), &[]);
        t.advance(MS(2000));
        assert_eq!(
            t.events(),
            [
                ev(
                    EventName::AppExited,
                    json!({"id": "portal", "reason": "exited"})
                ),
                ev(EventName::FocusChanged, json!({"app": null})),
            ]
        );
        assert_eq!(
            t.log.take(),
            [Call::FocusLauncher, Call::Mode(InterceptMode::All)]
        );
    }

    #[test]
    fn steam_game_that_never_appears_times_out() {
        let mut t = T::new();
        t.launch(json!({"url": "steam://rungameid/1000001"}));
        t.events();
        t.gamescope(Some(L), &[]);
        t.advance(Duration::from_secs(59));
        assert!(t.events().is_empty());
        t.advance(Duration::from_secs(1));
        assert_eq!(
            t.events(),
            [
                ev(
                    EventName::AppExited,
                    json!({"id": "steam-1000001", "reason": "launch_timeout"})
                ),
                ev(EventName::FocusChanged, json!({"app": null})),
            ]
        );
    }

    #[test]
    fn steam_without_gamescope_follows_the_steam_command() {
        let mut t = T::with(|c| c.gamescope = false);
        t.launch(json!({"url": "steam://rungameid/7"}));
        t.events();
        t.advance(Duration::from_secs(3600));
        assert!(t.events().is_empty(), "no timeout without gamescope");
        t.shell.handle(
            Msg::ChildExit(ChildExit {
                key: "steam-7".into(),
                pid: 101,
                code: Some(0),
            }),
            t.now,
        );
        assert_eq!(
            t.events()[0],
            ev(
                EventName::AppExited,
                json!({"id": "steam-7", "reason": "exited", "exit_code": 0})
            )
        );
    }

    #[test]
    fn app_that_closes_its_window_returns_focus_but_keeps_running() {
        let mut t = T::new();
        let app = t.launch(json!({"id": "jellyfin"}));
        assert_eq!(app.id, "jellyfin");
        assert_eq!(app.name, "Jellyfin");
        t.gamescope(Some(app.gamescope_app_id), &[]); // native Wayland: focused, not listed
        t.events();
        t.log.take();
        t.gamescope(Some(L), &[]);
        t.advance(MS(1000));
        assert_eq!(
            t.events(),
            [ev(EventName::FocusChanged, json!({"app": null}))]
        );
        assert_eq!(
            t.log.take(),
            [Call::FocusLauncher, Call::Mode(InterceptMode::All)]
        );
        assert_eq!(
            t.call("apps.running", json!({})).unwrap()["apps"][0]["id"],
            "jellyfin"
        );
        // Launching it again just focuses it.
        assert_eq!(t.launch(json!({"id": "jellyfin"})), app);
    }

    #[test]
    fn nav_only_while_the_launcher_has_focus_with_repeat() {
        let mut t = T::new();
        t.input("ui_right", 1.0);
        t.advance(MS(399));
        assert_eq!(
            t.events(),
            [ev(
                EventName::InputNav,
                json!({"action": "right", "phase": "press"})
            )]
        );
        t.advance(MS(301)); // repeats at 400, 500, 600, 700
        let repeats = t.events();
        assert_eq!(repeats.len(), 4);
        assert!(repeats.iter().all(|e| *e
            == ev(
                EventName::InputNav,
                json!({"action": "right", "phase": "repeat"})
            )));
        t.input("ui_right", 0.0);
        t.advance(MS(1000));
        assert_eq!(
            t.events(),
            [ev(
                EventName::InputNav,
                json!({"action": "right", "phase": "release"})
            )]
        );

        // Buttons press and release without repeat; unknown events are ignored.
        t.input("ui_accept", 1.0);
        t.advance(MS(1000));
        t.input("ui_accept", 0.0);
        t.input("ui_volume_up", 1.0);
        assert_eq!(
            t.events(),
            [
                ev(
                    EventName::InputNav,
                    json!({"action": "accept", "phase": "press"})
                ),
                ev(
                    EventName::InputNav,
                    json!({"action": "accept", "phase": "release"})
                ),
            ]
        );

        // Holding a direction while launching: the repeat stops, the app gets the pad.
        t.input("ui_down", 1.0);
        t.launch(json!({"command": ["game"]}));
        t.events();
        t.advance(MS(2000));
        t.input("ui_up", 1.0);
        assert!(t.events().is_empty());
    }

    #[test]
    fn home_from_the_launcher_and_sigusr1() {
        let mut t = T::new();
        t.shell.handle(Msg::Home, t.now);
        // Already on the launcher: Home is announced, focus re-applied, no focus_changed.
        assert_eq!(t.events(), [ev(EventName::HomePressed, json!({}))]);
        assert_eq!(
            t.log.take(),
            [Call::FocusLauncher, Call::Mode(InterceptMode::All)]
        );
    }

    #[test]
    fn errors() {
        let mut t = T::new();
        let code = |r: Result<Value, ErrorObject>| r.unwrap_err().code;
        assert_eq!(
            code(t.call("apps.launch", json!({"id": "nope"}))),
            ErrorCode::AppNotFound.code()
        );
        assert_eq!(
            code(t.call("apps.launch", json!({}))),
            ErrorCode::InvalidParams.code()
        );
        assert_eq!(
            code(t.call("apps.focus", json!({"id": "cmd-9"}))),
            ErrorCode::AppNotFound.code()
        );
        assert_eq!(
            code(t.call("apps.focus", json!({}))),
            ErrorCode::InvalidParams.code()
        );
        assert_eq!(
            code(t.call("apps.nope", json!({}))),
            ErrorCode::MethodNotFound.code()
        );

        // Spawn failure: app_launch_failed, nothing recorded.
        let mut t = T::new();
        t.shell.spawner = Box::new(MockSpawner {
            log: t.log.clone(),
            next_pid: 1,
            fail: true,
        });
        assert_eq!(
            code(t.call("apps.launch", json!({"command": ["x"]}))),
            ErrorCode::AppLaunchFailed.code()
        );
        assert!(t.events().is_empty());
        assert!(t.log.take().is_empty());

        // Stub entries (no apps configured) can't be launched.
        let mut t = T::with(|c| c.apps.clear());
        assert_eq!(
            code(t.call("apps.launch", json!({"id": "jellyfin"}))),
            ErrorCode::AppLaunchFailed.code()
        );
        let list = t.call("apps.list", json!({})).unwrap();
        assert_eq!(list["apps"][0]["id"], "steam");
        assert_eq!(list["apps"][1]["id"], "jellyfin");
    }

    #[test]
    fn gamescope_restart_reapplies_focus() {
        let mut t = T::new();
        t.launch(json!({"command": ["game"]}));
        t.log.take();
        t.shell
            .handle(Msg::Gamescope(GamescopeEvent::Connected), t.now);
        assert_eq!(
            t.log.take(),
            [Call::FocusApp(L + 1), Call::Mode(InterceptMode::Pass)]
        );
    }

    #[test]
    fn app_ids_skip_ids_in_use() {
        let mut t = T::with(|c| c.launcher_app_id = u32::MAX);
        assert_eq!(t.launch(json!({"command": ["a"]})).gamescope_app_id, 1);
        assert_eq!(t.launch(json!({"command": ["b"]})).gamescope_app_id, 2);
    }

    fn exit(t: &mut T, key: &str, pid: u32, code: i32) {
        t.shell.handle(
            Msg::ChildExit(ChildExit {
                key: key.into(),
                pid,
                code: Some(code),
            }),
            t.now,
        );
    }

    fn steam_windows(t: &mut T, windows: &[(u32, &str)]) {
        let windows = windows
            .iter()
            .map(|&(window, title)| SteamWindow {
                window,
                title: title.into(),
                pid: Some(77),
            })
            .collect();
        t.shell
            .handle(Msg::Gamescope(GamescopeEvent::SteamWindows(windows)), t.now);
    }

    #[test]
    fn close_command_app_term_then_kill() {
        let mut t = T::new();
        let app = t.launch(json!({"command": ["stubborn"]}));
        let reaper = app.pid.unwrap();
        t.procs
            .lock()
            .unwrap()
            .trees
            .insert(reaper, vec![reaper + 1, reaper + 2]);
        t.events();
        t.log.take();

        assert_eq!(
            t.call("apps.close", json!({"id": "cmd-1"})).unwrap(),
            json!({})
        );
        assert_eq!(t.log.take(), [Call::Signal(reaper, Sig::Term)]);
        // Closing again does nothing.
        t.call("apps.close", json!({"id": "cmd-1"})).unwrap();
        t.advance(MS(4999));
        assert!(t.log.take().is_empty());
        t.advance(MS(1));
        assert_eq!(
            t.log.take(),
            [
                Call::Signal(reaper + 1, Sig::Kill),
                Call::Signal(reaper + 2, Sig::Kill)
            ]
        );
        // The reaper exits once its tree is gone: closed, with the status.
        exit(&mut t, "cmd-1", reaper, 137);
        assert_eq!(
            t.events(),
            [
                ev(
                    EventName::AppExited,
                    json!({"id": "cmd-1", "reason": "closed", "exit_code": 137})
                ),
                ev(EventName::FocusChanged, json!({"app": null})),
            ]
        );
        assert_eq!(t.shell.deadline(), None);
        assert_eq!(
            t.call("apps.close", json!({"id": "cmd-1"}))
                .unwrap_err()
                .code,
            ErrorCode::AppNotFound.code()
        );
    }

    #[test]
    fn close_command_app_that_quits_politely() {
        let mut t = T::new();
        let app = t.launch(json!({"command": ["polite"]}));
        t.call("apps.close", json!({"id": "cmd-1"})).unwrap();
        exit(&mut t, "cmd-1", app.pid.unwrap(), 0);
        t.log.take();
        t.advance(Duration::from_secs(60));
        assert!(t.log.take().is_empty(), "no kill after it exited");
        assert_eq!(
            t.events()[2],
            ev(
                EventName::AppExited,
                json!({"id": "cmd-1", "reason": "closed", "exit_code": 0})
            )
        );
    }

    #[test]
    fn close_steam_game_kills_its_tree_not_steam() {
        let mut t = T::new();
        t.launch(json!({"url": "steam://rungameid/620"}));
        t.gamescope(Some(620), &[620]);
        {
            let mut procs = t.procs.lock().unwrap();
            procs.steam_reapers.insert(620, vec![500]);
            procs.trees.insert(500, vec![501, 502]);
        }
        t.events();
        t.log.take();
        t.call("apps.close", json!({"id": "steam-620"})).unwrap();
        assert_eq!(
            t.log.take(),
            [Call::Signal(501, Sig::Term), Call::Signal(502, Sig::Term)]
        );
        t.advance(MS(5000));
        assert_eq!(
            t.log.take(),
            [Call::Signal(501, Sig::Kill), Call::Signal(502, Sig::Kill)]
        );
        // Its window goes: closed.
        t.gamescope(Some(L), &[]);
        t.advance(MS(1000));
        assert_eq!(
            t.events(),
            [
                ev(
                    EventName::AppExited,
                    json!({"id": "steam-620", "reason": "closed"})
                ),
                ev(EventName::FocusChanged, json!({"app": null})),
            ]
        );

        // A game Steam has not started yet: nothing to kill, just stop tracking it.
        t.launch(json!({"url": "steam://rungameid/570"}));
        t.events();
        t.log.take();
        t.call("apps.close", json!({"id": "steam-570"})).unwrap();
        assert!(!t.log.take().iter().any(|c| matches!(c, Call::Signal(..))));
        assert_eq!(
            t.events()[0],
            ev(
                EventName::AppExited,
                json!({"id": "steam-570", "reason": "closed"})
            )
        );
    }

    #[test]
    fn close_gives_up_after_kill() {
        let mut t = T::new();
        t.launch(json!({"url": "steam://rungameid/620"}));
        t.gamescope(Some(620), &[620]);
        t.procs.lock().unwrap().steam_reapers.insert(620, vec![500]);
        t.events();
        t.call("apps.close", json!({"id": "steam-620"})).unwrap();
        t.advance(MS(10_000));
        assert_eq!(
            t.events()[0],
            ev(
                EventName::AppExited,
                json!({"id": "steam-620", "reason": "closed"})
            )
        );
    }

    #[test]
    fn games_list_and_names_from_the_library() {
        let mut t = T::new();
        let games = t.call("games.list", json!({})).unwrap();
        let ids: Vec<&Value> = games["games"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| &g["appid"])
            .collect();
        assert_eq!(
            ids,
            [
                &json!(1_086_940),
                &json!(570),
                &json!(1_145_360),
                &json!(620)
            ]
        );
        assert_eq!(
            t.launch(json!({"url": "steam://rungameid/620"})).name,
            "Portal 2"
        );
        assert_eq!(
            t.launch(json!({"url": "steam://rungameid/9"})).name,
            "Steam game 9"
        );

        let mut t = T::with(|c| c.steam_dir = Some("/nonexistent".into()));
        assert_eq!(
            t.call("games.list", json!({})).unwrap(),
            json!({"games": []})
        );
    }

    #[test]
    fn steam_big_picture_entry() {
        let mut t = T::new();
        assert_eq!(
            t.call("apps.list", json!({})).unwrap()["apps"][0],
            json!({"id": "steam", "name": "Steam", "kind": "native"})
        );
        // Steam not running: start it straight into Big Picture.
        let app = t.launch(json!({"id": "steam"}));
        assert_eq!(app, steam_client_app());
        assert_eq!(
            t.log.take(),
            [
                Call::Spawn {
                    key: "steam".into(),
                    argv: vec!["/fake/steam".into(), "-gamepadui".into()],
                    app_id: None,
                },
                Call::FocusApp(769),
                Call::Mode(InterceptMode::Pass),
            ]
        );
        // Big Picture's window appears (tagged 769 by the watcher): seen, no attention.
        t.gamescope(Some(769), &[769]);
        steam_windows(&mut t, &[(10, "Steam Big Picture Mode")]);
        // Home: Steam stays in the background; its window does not ask for attention.
        t.input("ui_guide", 1.0);
        assert_eq!(t.shell.focused(), None);
        t.gamescope(Some(L), &[769]);
        t.events();
        assert_eq!(
            t.call("steam.attention", json!({})).unwrap(),
            json!({"windows": []})
        );
        // Closing Big Picture asks Steam, never kills it; its window goes: closed.
        t.log.take();
        t.call("apps.close", json!({"id": "steam"})).unwrap();
        assert_eq!(
            t.log.take(),
            [Call::Spawn {
                key: "steam-close".into(),
                argv: vec!["/fake/steam".into(), "steam://close/bigpicture".into()],
                app_id: None,
            }]
        );
        t.gamescope(Some(L), &[]);
        steam_windows(&mut t, &[]);
        t.advance(MS(10_000));
        assert!(!t.log.take().iter().any(|c| matches!(c, Call::Signal(..))));
        assert_eq!(
            t.events(),
            [ev(
                EventName::AppExited,
                json!({"id": "steam", "reason": "closed"})
            )]
        );

        // Steam running: the URL goes to the running client.
        t.procs.lock().unwrap().steam_running = true;
        t.launch(json!({"id": "steam"}));
        assert_eq!(
            t.log.take()[0],
            Call::Spawn {
                key: "steam".into(),
                argv: vec!["/fake/steam".into(), "steam://open/bigpicture".into()],
                app_id: None,
            }
        );
    }

    #[test]
    fn steam_popup_while_in_a_game() {
        let mut t = T::new();
        assert_eq!(
            t.call("steam.focus", json!({})).unwrap_err().code,
            ErrorCode::SteamNoWindows.code()
        );
        t.launch(json!({"url": "steam://rungameid/620"}));
        t.gamescope(Some(620), &[620]);
        t.events();
        t.log.take();

        // A Steam Guard window appears while the game has focus.
        steam_windows(&mut t, &[(42, "Steam Guard")]);
        let guard = json!({"window": 42, "title": "Steam Guard", "pid": 77});
        assert_eq!(
            t.events(),
            [ev(EventName::SteamAttention, json!({"windows": [guard]}))]
        );
        assert_eq!(
            t.call("steam.attention", json!({})).unwrap(),
            json!({"windows": [guard]})
        );
        // Same list again: no event.
        steam_windows(&mut t, &[(42, "Steam Guard")]);
        assert!(t.events().is_empty());

        // The launcher brings it forward: Steam is an app now, with focus and the pad.
        let r = t.call("steam.focus", json!({})).unwrap();
        assert_eq!(r, json!({"app": steam_client_app()}));
        assert_eq!(
            t.log.take(),
            [Call::FocusApp(769), Call::Mode(InterceptMode::Pass)]
        );
        assert_eq!(
            t.events(),
            [
                ev(EventName::AppStarted, json!({"app": steam_client_app()})),
                ev(EventName::FocusChanged, json!({"app": "steam"})),
                ev(EventName::SteamAttention, json!({"windows": []})),
            ]
        );
        t.gamescope(Some(769), &[620, 769]);

        // Dismissed: the window closes, Steam exits, focus goes back to the game.
        steam_windows(&mut t, &[]);
        t.gamescope(Some(L), &[620]);
        t.advance(MS(1000));
        assert_eq!(
            t.events(),
            [
                ev(
                    EventName::AppExited,
                    json!({"id": "steam", "reason": "exited"})
                ),
                ev(EventName::FocusChanged, json!({"app": "steam-620"})),
            ]
        );
        assert_eq!(
            t.log.take(),
            [Call::FocusApp(620), Call::Mode(InterceptMode::Pass)]
        );
    }

    #[test]
    fn steam_attention_is_cleared_when_seen_or_closed() {
        let mut t = T::new();
        steam_windows(&mut t, &[(1, "Steam - Update"), (2, "Cloud Sync Conflict")]);
        assert_eq!(t.events().len(), 1);
        // One closes.
        steam_windows(&mut t, &[(2, "Cloud Sync Conflict")]);
        assert_eq!(
            t.events(),
            [ev(
                EventName::SteamAttention,
                json!({"windows": [{"window": 2, "title": "Cloud Sync Conflict", "pid": 77}]})
            )]
        );
        // Seen via steam.focus, then Home: still open, but no longer asking.
        t.call("steam.focus", json!({})).unwrap();
        t.input("ui_guide", 1.0);
        t.events();
        assert_eq!(
            t.call("steam.attention", json!({})).unwrap(),
            json!({"windows": []})
        );
        // A new window asks again; gamescope going away clears everything.
        steam_windows(&mut t, &[(2, "Cloud Sync Conflict"), (3, "Steam Guard")]);
        assert_eq!(
            t.events(),
            [ev(
                EventName::SteamAttention,
                json!({"windows": [{"window": 3, "title": "Steam Guard", "pid": 77}]})
            )]
        );
        t.shell
            .handle(Msg::Gamescope(GamescopeEvent::Disconnected), t.now);
        assert_eq!(
            t.events(),
            [ev(EventName::SteamAttention, json!({"windows": []}))]
        );
    }
}
