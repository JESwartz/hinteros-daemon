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
//! - A Steam game: `[game, 769, launcher]` (Steam's pre-launch and error windows show until the
//!   game has one), PASS. It runs while Steam's reaper for it is alive, whatever its windows
//!   do; launching it again then only focuses it. A Steam window over the launcher right after
//!   our own Steam request gets focus (`STEAM_ANSWER_WINDOW`).
//! - `apps.close`: SIGTERM now, SIGKILL after the close timeout, then stop tracking.
//! - An app with the keyboard input profile: InputPlumber intercepts the pad while it has focus
//!   (`GamepadOnly` with the right-stick pointer profile, else `All`) and `ui_*` events become
//!   key presses through its keymap (with repeat) instead of `input_nav`; A on a focused text
//!   field shows the on-screen keyboard. Without a virtual keyboard it falls back to the
//!   gamepad (`Pass`).
//! - The on-screen keyboard (`osk.show`, or Guide held + the summon button): shown as an
//!   overlay over whatever has focus; while it is visible InputPlumber is in `All` and
//!   `input_nav` goes only to the OSK client. Home and focus changes hide it. With a summon
//!   chord configured, Home happens when Guide is *released* without the chord button.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use hinteros_api::server::{decode_params, encode_result};
use hinteros_api::session::{
    App, AppExitedParams, AppKind, AppSource, AppStartedParams, AppsClose, AppsCloseParams,
    AppsCloseResult, AppsFocus, AppsFocusParams, AppsFocusResult, AppsLaunch, AppsLaunchParams,
    AppsLaunchResult, AppsList, AppsListResult, AppsRunning, AppsRunningResult, EventName,
    ExitReason, FocusChangedParams, GamesList, GamesListResult, HomePressedParams, InputNavParams,
    KEYS_TYPE_MAX_CHARS, KeysAction, KeysActionParams, KeysActionResult, KeysKey, KeysKeyParams,
    KeysKeyResult, KeysType, KeysTypeParams, KeysTypeResult, LaunchRequest, NavAction, NavPhase,
    OskHide, OskShow, OskState, RunningApp, STEAM_CLIENT_APP_ID, STEAM_CLIENT_GAMESCOPE_APP_ID,
    STEAM_RUNGAMEID, SteamAttentionParams, SteamAttentionQuery, SteamFocus, SteamFocusResult,
    SteamWindow,
};
use hinteros_api::{ErrorCode, ErrorObject, Method};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::apps::{ChildExit, Spawner};
use crate::config::{AppConfig, Config, FieldSummon};
use crate::focus::{
    FocusControl, GamescopeEvent, GamescopeState, baselayer_for, baselayer_for_steam_game,
};
use crate::icons::{IconStore, Refresher};
use crate::input::pointer::{CursorChange, CursorControl, NoCursor, PointerState};
use crate::input::{
    Debounce, InputControl, Intent, InterceptMode, MouseButton, RawInput, Repeat, translate,
};
use crate::keys::{KeyCombo, KeyInjector, KeyOp, Keymap, plan_action, plan_text};
use crate::procs::{ProcessControl, Sig};
use crate::steam::library;
use crate::textfocus::FieldFocus;
use crate::{Audience, Broadcast};

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
    /// The config was reloaded: this is the new app list (running apps are left alone).
    SetApps(Vec<AppConfig>),
    /// The usable Steam binary changed (`None`: Steam is not installed).
    SetSteam(Option<PathBuf>),
    /// A text field in a web app gained or lost focus (see [`crate::textfocus`]); `pid` is the
    /// reporting process (Firefox's native-messaging host).
    TextFocus {
        pid: Option<u32>,
        focus: FieldFocus,
    },
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
    OskShow::NAME,
    OskHide::NAME,
    KeysType::NAME,
    KeysKey::NAME,
    KeysAction::NAME,
];

/// Steam windows that appear this soon after our own Steam request are Steam's answer to it.
pub const STEAM_ANSWER_WINDOW: Duration = Duration::from_secs(10);

/// A Guide press older than this no longer starts a chord (its release may have been lost).
const GUIDE_HOLD_MAX: Duration = Duration::from_secs(10);

/// How long after a text field loses focus an automatically shown keyboard hides: moving to
/// the next field (or into a frame) reports `none` and then `text` again.
pub const OSK_AUTO_HIDE_DELAY: Duration = Duration::from_millis(250);

/// Where controller navigation goes right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// `input_nav` to the launcher.
    Launcher,
    /// `input_nav` to the on-screen keyboard client.
    Osk,
    /// Key presses into the focused app (keyboard input profile).
    Keys,
    /// Nowhere: the focused app has the pad (InputPlumber `Pass`).
    App,
}

/// Settings the shell needs, taken from [`Config`].
#[derive(Debug, Clone)]
pub struct ShellConfig {
    pub launcher_app_id: u32,
    /// The Steam binary, resolved (`None`: not installed, so no built-in `steam` app).
    pub steam_path: Option<PathBuf>,
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
    pub osk_enabled: bool,
    /// Show the on-screen keyboard when a text field gets focus in a keyboard-profile app.
    pub osk_auto_show: bool,
    /// A on a focused text field (keyboard-profile app) shows the on-screen keyboard.
    pub osk_field_accept: bool,
    /// The right stick moves a pointer in keyboard-profile apps (InputPlumber profile).
    pub pointer_enabled: bool,
    /// Stick chatter filter for directions (0: off).
    pub direction_debounce: Duration,
    /// The button that, pressed while Guide is held, shows/hides the on-screen keyboard.
    pub osk_summon: Option<NavAction>,
    /// Resolved app icons (filled in by `icons::start`, possibly later).
    pub icons: IconStore,
    /// Told when an app exits (re-reads its Firefox favicon).
    pub icon_refresh: Refresher,
}

impl From<&Config> for ShellConfig {
    fn from(c: &Config) -> Self {
        Self {
            launcher_app_id: c.launcher.app_id,
            steam_path: crate::apps::steam_binary(&c.steam.path),
            steam_dir: c.steam.dir.clone(),
            gamescope: c.gamescope.enable,
            steam_launch_timeout: Duration::from_secs(c.steam.launch_timeout_s),
            focus_loss_debounce: Duration::from_millis(c.gamescope.focus_loss_debounce_ms),
            close_timeout: Duration::from_millis(c.close.timeout_ms),
            repeat_delay: c.input.repeat_delay(),
            repeat_interval: c.input.repeat_interval(),
            apps: c.apps.clone(),
            osk_enabled: c.osk.enable,
            osk_auto_show: c.osk.auto_show,
            osk_field_accept: c.osk.field_summon == FieldSummon::Accept,
            pointer_enabled: c.pointer.enable,
            direction_debounce: c.input.direction_debounce(),
            osk_summon: c.osk.summon_button().ok().flatten(),
            icons: IconStore::default(),
            icon_refresh: Refresher::default(),
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
    /// A browser with an address bar: View is Ctrl+L and the on-screen keyboard.
    address_bar: bool,
    /// The right stick is a pointer (keyboard profile).
    pointer: bool,
    /// The keyboard input profile's keymap (`None`: gamepad).
    keymap: Option<Keymap>,
}

/// How an app takes controller input (from its config).
#[derive(Debug, Default)]
struct AppInput {
    keymap: Option<Keymap>,
    address_bar: bool,
    pointer: bool,
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

/// How long to wait before writing the focus to gamescope again after a failure.
const FOCUS_RETRY: Duration = Duration::from_secs(1);

pub struct Shell {
    cfg: ShellConfig,
    focus: Box<dyn FocusControl>,
    input: Box<dyn InputControl>,
    keys: Box<dyn KeyInjector>,
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
    /// Writing the focus to gamescope failed (it is restarting?): try again at this time.
    focus_retry_at: Option<Instant>,
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
    /// When we last asked Steam to do something (launch or close a game, open or close Big
    /// Picture): Steam windows that appear soon after are its answer and get focus.
    steam_action_at: Option<Instant>,
    osk_visible: bool,
    /// The keyboard is up because a text field got focus (not because someone asked).
    osk_auto: bool,
    /// The text field lost focus: hide the automatically shown keyboard at this time.
    osk_auto_hide_at: Option<Instant>,
    /// Guide is held: since when, and whether it was used for a chord.
    guide: Option<(Instant, bool)>,
    /// Navigation buttons currently held (as far as we can see them).
    held: BTreeSet<NavAction>,
    /// Releases to drop (buttons used in a chord).
    swallow: BTreeSet<NavAction>,
    /// The raw `ui_*` name each held button last arrived as (for the chord log).
    raw_names: BTreeMap<NavAction, String>,
    /// Buttons pressed while Guide was held that made no chord (raw `ui_*` names, for the log).
    guide_others: Vec<String>,
    /// What has focus in each keyboard-profile app's page (from the Firefox extension).
    text_fields: BTreeMap<String, FieldFocus>,
    /// Stick chatter filter for directions.
    debounce: Debounce,
    /// The right-stick pointer: in use or not, hidden by us or not.
    pointer: PointerState,
    /// The pointer profile is loaded in InputPlumber (as far as we told it).
    pointer_profile: bool,
    /// Shows and hides gamescope's pointer.
    cursor: Box<dyn CursorControl>,
    /// Buttons held down as mouse buttons, with the pad they came from.
    clicking: BTreeMap<NavAction, String>,
}

impl Shell {
    pub fn new(
        cfg: ShellConfig,
        focus: Box<dyn FocusControl>,
        input: Box<dyn InputControl>,
        keys: Box<dyn KeyInjector>,
        spawner: Box<dyn Spawner>,
        procs: Box<dyn ProcessControl>,
        events: broadcast::Sender<Broadcast>,
    ) -> Self {
        let repeat = Repeat::new(cfg.repeat_delay, cfg.repeat_interval);
        let debounce = Debounce::new(cfg.direction_debounce);
        let pointer = PointerState::new();
        Self {
            text_fields: BTreeMap::new(),
            debounce,
            pointer,
            pointer_profile: false,
            cursor: Box::new(NoCursor),
            clicking: BTreeMap::new(),
            cfg,
            focus,
            input,
            keys,
            spawner,
            procs,
            events,
            apps: BTreeMap::new(),
            focused: None,
            repeat,
            gamescope: None,
            focus_lost_at: None,
            focus_retry_at: None,
            written: std::collections::VecDeque::new(),
            next_command: 1,
            steam_windows: Vec::new(),
            seen_windows: BTreeSet::new(),
            attention: Vec::new(),
            steam_return: None,
            steam_action_at: None,
            osk_visible: false,
            osk_auto: false,
            osk_auto_hide_at: None,
            guide: None,
            held: BTreeSet::new(),
            swallow: BTreeSet::new(),
            raw_names: BTreeMap::new(),
            guide_others: Vec::new(),
        }
    }

    /// Use `cursor` to hide and show gamescope's pointer (default: nothing).
    pub fn with_cursor(mut self, cursor: Box<dyn CursorControl>) -> Self {
        self.cursor = cursor;
        self
    }

    /// Apply the initial focus (the launcher) and hide the on-screen keyboard. Call once at
    /// start.
    pub fn start(&mut self) {
        self.adopt_running_steam_games(Instant::now());
        self.apply_focus();
        if let Err(e) = self.focus.set_osk_visible(false) {
            debug!("hiding the on-screen keyboard: {e:#}");
        }
    }

    pub fn osk_visible(&self) -> bool {
        self.osk_visible
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
            Msg::SetApps(apps) => self.cfg.apps = apps,
            Msg::SetSteam(path) => self.set_steam(path),
            Msg::TextFocus { pid, focus } => self.text_focus(pid, focus, now),
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
            OskShow::NAME => {
                decode_params::<OskShow>(params)?;
                self.show_osk()?;
                encode_result::<OskShow>(OskState {
                    visible: self.osk_visible,
                })
            }
            OskHide::NAME => {
                decode_params::<OskHide>(params)?;
                self.hide_osk();
                encode_result::<OskHide>(OskState {
                    visible: self.osk_visible,
                })
            }
            KeysType::NAME => {
                let p = decode_params::<KeysType>(params)?;
                encode_result::<KeysType>(self.keys_type(p)?)
            }
            KeysKey::NAME => {
                let p = decode_params::<KeysKey>(params)?;
                encode_result::<KeysKey>(self.keys_key(p)?)
            }
            KeysAction::NAME => {
                let p = decode_params::<KeysAction>(params)?;
                encode_result::<KeysAction>(self.keys_action(p)?)
            }
            _ => Err(hinteros_api::server::method_not_found(method)),
        }
    }

    fn emit<T: Serialize>(&self, name: EventName, params: T) {
        self.emit_to(name, params, Audience::All);
    }

    fn emit_to<T: Serialize>(&self, name: EventName, params: T, audience: Audience) {
        match serde_json::to_value(params) {
            // No receivers is fine: nobody is connected.
            Ok(v) => drop(self.events.send((name, v, audience))),
            Err(e) => warn!("encoding {name:?}: {e}"),
        }
    }

    fn steam_dir(&self) -> Option<PathBuf> {
        library::default_steam_dir(self.cfg.steam_dir.as_deref())
    }

    /// Steam was installed or removed (or moved): the built-in `steam` app follows. The caller
    /// announces `apps_changed`.
    fn set_steam(&mut self, path: Option<PathBuf>) {
        info!(steam = ?path, "Steam binary changed");
        self.cfg.steam_path = path;
        if self.cfg.steam_path.is_none() {
            // Nothing to bring forward any more.
            self.steam_windows(Vec::new(), Instant::now());
        }
    }

    /// The Steam binary, or `code` with "Steam is not installed".
    fn steam_binary(&self, code: ErrorCode) -> Result<String, ErrorObject> {
        self.cfg
            .steam_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .ok_or_else(|| ErrorObject::new(code, "Steam is not installed"))
    }

    fn list(&self) -> AppsListResult {
        // The built-in entry only while there is a Steam to start.
        let steam = self.cfg.steam_path.as_ref().map(|_| App {
            id: STEAM_CLIENT_APP_ID.into(),
            name: "Steam".into(),
            kind: AppKind::Native,
            icon: self.icon_path(STEAM_CLIENT_APP_ID, None),
        });
        if self.cfg.apps.is_empty() {
            let mut list = stub_apps();
            if let Some(steam) = steam {
                list.apps.insert(0, steam);
            }
            return list;
        }
        AppsListResult {
            apps: steam
                .into_iter()
                .chain(self.cfg.apps.iter().map(|a| App {
                    id: a.id.clone(),
                    name: a.name.clone(),
                    kind: a.kind,
                    icon: self.icon_path(&a.id, a.icon.as_deref()),
                }))
                .collect(),
        }
    }

    /// The resolved icon of an app, else a configured absolute path.
    fn icon_path(&self, id: &str, configured: Option<&str>) -> Option<String> {
        self.cfg
            .icons
            .get(id)
            .map(|p| p.to_string_lossy().into_owned())
            .or_else(|| {
                configured
                    .filter(|p| std::path::Path::new(p).is_absolute())
                    .map(String::from)
            })
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
        let configured = match &request {
            LaunchRequest::Id(id) => self.cfg.apps.iter().find(|a| &a.id == id),
            _ => None,
        };
        let keymap = configured.and_then(|a| a.keymap().ok().flatten());
        let input = AppInput {
            address_bar: configured.is_some_and(|a| a.address_bar),
            pointer: keymap.is_some() && configured.is_some_and(|a| a.pointer.unwrap_or(true)),
            keymap,
        };
        if input.keymap.is_some() && !self.keys.caps().keys {
            warn!("no virtual keyboard: this app gets the gamepad instead of key presses");
        }
        // Resolve to (key, name, what to run).
        let (key, name, target) = match request {
            LaunchRequest::Id(id) if id == STEAM_CLIENT_APP_ID => {
                if self.cfg.steam_path.is_none() && !self.apps.contains_key(&id) {
                    // Not in apps.list either.
                    return Err(ErrorObject::new(
                        ErrorCode::AppNotFound,
                        "no app with id 'steam': Steam is not installed",
                    ));
                }
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
        // A Steam game we lost track of (or that runs since before the daemon started): Steam
        // would only answer "Game already running".
        if let Target::Steam(steam) = &target
            && !self.procs.steam_reapers(*steam).is_empty()
        {
            info!(app = %key, "already running (Steam's reaper is alive); focusing it");
            let info = RunningApp {
                id: key.clone(),
                name,
                source: AppSource::Steam,
                gamescope_app_id: *steam,
                pid: None,
            };
            self.track(info.clone(), AppInput::default(), now);
            self.set_focus(Some(key));
            return Ok(AppsLaunchResult { app: info });
        }

        let failed = |e: std::io::Error, what: &str| {
            ErrorObject::new(
                ErrorCode::AppLaunchFailed,
                format!("cannot start {what}: {e}"),
            )
        };
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
                let steam_path = self.steam_binary(ErrorCode::AppLaunchFailed)?;
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
                self.steam_action_at = Some(now);
                RunningApp {
                    id: key.clone(),
                    name,
                    source: AppSource::Steam,
                    gamescope_app_id: steam,
                    pid: None,
                }
            }
            Target::SteamClient => {
                let steam_path = self.steam_binary(ErrorCode::AppNotFound)?;
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
                self.steam_action_at = Some(now);
                steam_client_app()
            }
        };
        info!(app = %info.id, gamescope_app_id = info.gamescope_app_id, "launched");
        self.track(info.clone(), input, now);
        self.set_focus(Some(key));
        Ok(AppsLaunchResult { app: info })
    }

    /// Start tracking a running app and announce it.
    fn track(&mut self, info: RunningApp, input: AppInput, now: Instant) {
        self.apps.insert(
            info.id.clone(),
            Running {
                info: info.clone(),
                started: now,
                seen_focusable: false,
                seen_focused: false,
                gone_since: None,
                closing: None,
                keymap: input.keymap,
                address_bar: input.address_bar,
                pointer: input.pointer,
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
                self.steam_action_at = Some(now);
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
                self.steam_action_at = Some(now);
                match self.steam_binary(ErrorCode::AppNotFound) {
                    Ok(steam_path) => {
                        let argv = [steam_path, "steam://close/bigpicture".into()];
                        if let Err(e) = self.spawner.spawn("steam-close", &argv, None) {
                            warn!("cannot ask Steam to close Big Picture: {e}");
                        }
                    }
                    Err(e) => warn!("cannot ask Steam to close Big Picture: {}", e.message),
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

    /// Whether the Steam game `id`, about to be given up (`reason`: its windows are gone, or
    /// it never showed one in time), is in fact still there: Steam's reaper for it is alive,
    /// or, while it never had a window, Steam shows windows of its own (shader pre-caching,
    /// a pre-launch or error dialog). Not for other apps, nor for games being closed.
    fn steam_game_still_running(&mut self, id: &str, reason: ExitReason) -> bool {
        let Some(app) = self.apps.get(id) else {
            return false;
        };
        if app.info.source != AppSource::Steam || app.closing.is_some() {
            return false;
        }
        let (gs_id, launching) = (
            app.info.gamescope_app_id,
            !app.seen_focusable && !app.seen_focused,
        );
        if !self.procs.steam_reapers(gs_id).is_empty() {
            debug!(
                app = id,
                ?reason,
                "no window, but Steam's reaper is alive: still running"
            );
            return true;
        }
        if launching && reason == ExitReason::LaunchTimeout && !self.steam_windows.is_empty() {
            info!(
                app = id,
                "no window yet, but Steam shows windows: still launching"
            );
            return true;
        }
        false
    }

    /// Track the Steam games that are running although we did not start them (the daemon
    /// restarted): found by their Steam reapers.
    fn adopt_running_steam_games(&mut self, now: Instant) {
        // Without gamescope there is no window tracking for them (and no Steam session).
        if self.cfg.steam_path.is_none() || !self.cfg.gamescope {
            return;
        }
        for steam in self.procs.steam_games() {
            // Our own command apps run under reapers too, with ids above the launcher's.
            if steam >= self.cfg.launcher_app_id
                || self.apps.values().any(|a| a.info.gamescope_app_id == steam)
            {
                continue;
            }
            let name = self
                .steam_dir()
                .and_then(|dir| library::find_manifest(&dir, steam))
                .map(|m| m.name)
                .unwrap_or_else(|| format!("Steam game {steam}"));
            info!(steam_app_id = steam, %name, "found running; tracking it");
            let info = RunningApp {
                id: format!("steam-{steam}"),
                name,
                source: AppSource::Steam,
                gamescope_app_id: steam,
                pid: None,
            };
            self.track(info, AppInput::default(), now);
        }
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
            self.track(steam_client_app(), AppInput::default(), now);
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
        self.held.clear();
        self.focus_lost_at = None;
        let changed = self.focused != target;
        if changed && self.osk_visible {
            // The mode is applied with the new focus below.
            self.set_osk(false, false);
        }
        self.focused = target;
        self.apply_focus();
        if self.steam_shown() {
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
            .map(|r| (r.info.gamescope_app_id, r.info.source == AppSource::Steam));
        if self.written.len() == 4 {
            self.written.pop_front();
        }
        let launcher = self.cfg.launcher_app_id;
        self.written.push_back(match target {
            Some((id, true)) => baselayer_for_steam_game(id, launcher),
            _ => baselayer_for(target.map(|(id, _)| id), launcher),
        });
        let result = match target {
            // Steam's own windows (shader pre-caching, launch errors) until the game has one.
            Some((id, true)) => self.focus.focus_steam_game(id),
            Some((id, false)) => self.focus.focus_app(id),
            None => self.focus.focus_launcher(),
        };
        match result {
            Ok(()) => self.focus_retry_at = None,
            Err(e) => {
                // gamescope may be gone or restarting; the watcher says when it is back, but
                // do not depend on that alone.
                if self.focus_retry_at.is_none() {
                    warn!("setting gamescope focus (will retry): {e:#}");
                } else {
                    debug!("setting gamescope focus (will retry): {e:#}");
                }
                self.focus_retry_at = Some(Instant::now() + FOCUS_RETRY);
            }
        }
        self.apply_input();
    }

    /// Tell InputPlumber the pointer profile and intercept mode for the current focus and
    /// on-screen keyboard (the profile first: see `input::inputplumber`).
    fn apply_input(&mut self) {
        let pointer = self.pointer_app();
        if pointer != self.pointer_profile {
            self.pointer_profile = pointer;
            self.input.set_pointer(pointer);
        }
        let change = self.pointer.focus(pointer, self.focused_app().is_some());
        self.cursor_change(change);
        self.input.set_mode(self.intercept_mode());
    }

    fn focused_app(&self) -> Option<&Running> {
        self.focused.as_ref().and_then(|id| self.apps.get(id))
    }

    /// The focused app takes key presses (keyboard profile, and there is a virtual keyboard).
    fn keys_app(&self) -> Option<&Running> {
        self.focused_app()
            .filter(|app| app.keymap.is_some() && self.keys.caps().keys)
    }

    /// The focused app has the right-stick pointer.
    fn pointer_app(&self) -> bool {
        self.cfg.pointer_enabled && self.keys_app().is_some_and(|app| app.pointer)
    }

    fn route(&self) -> Route {
        if self.osk_visible {
            return Route::Osk;
        }
        match self.focused_app() {
            None => Route::Launcher,
            Some(_) if self.keys_app().is_some() => Route::Keys,
            Some(_) => Route::App,
        }
    }

    /// The InputPlumber mode for the current focus and on-screen keyboard. With the pointer,
    /// `GamepadOnly`: the right stick's mouse events still reach the pointer.
    fn intercept_mode(&self) -> InterceptMode {
        match self.route() {
            Route::App => InterceptMode::Pass,
            Route::Keys if self.pointer_app() => InterceptMode::GamepadOnly,
            Route::Launcher | Route::Osk | Route::Keys => InterceptMode::All,
        }
    }

    fn cursor_change(&mut self, change: Option<CursorChange>) {
        match change {
            Some(CursorChange::Show) => self.cursor.set_visible(true),
            Some(CursorChange::Hide) => self.cursor.set_visible(false),
            None => {}
        }
    }

    /// `osk.show`.
    fn show_osk(&mut self) -> Result<(), ErrorObject> {
        if !self.cfg.osk_enabled {
            return Err(ErrorObject::new(
                ErrorCode::OskUnavailable,
                "the on-screen keyboard is disabled ([osk] enable = false)",
            ));
        }
        if !self.osk_visible {
            self.set_osk(true, true);
        }
        Ok(())
    }

    /// `osk.hide`.
    fn hide_osk(&mut self) {
        if self.osk_visible {
            self.set_osk(false, true);
        }
    }

    fn toggle_osk(&mut self) {
        if self.osk_visible {
            self.hide_osk();
        } else if let Err(e) = self.show_osk() {
            info!("{}", e.message);
        }
    }

    /// A text field in a web app gained or lost focus (reported by the Firefox extension's
    /// host, `pid`). Remembered per keyboard-profile app whose process tree the host is in (an
    /// unknown pid counts for the focused app), for A ([`Self::field_accept`]). With
    /// `auto_show`, also shows the keyboard right away when that app has focus; a keyboard
    /// shown for a field (either way) hides again [`OSK_AUTO_HIDE_DELAY`] after the field
    /// loses focus.
    fn text_focus(&mut self, pid: Option<u32>, focus: FieldFocus, now: Instant) {
        let keyboard_apps: Vec<(String, Option<u32>)> = self
            .apps
            .values()
            .filter(|a| a.keymap.is_some())
            .map(|a| (a.info.id.clone(), a.info.pid))
            .collect();
        let owner = match pid {
            None => self
                .focused
                .clone()
                .filter(|id| keyboard_apps.iter().any(|(a, _)| a == id)),
            Some(pid) => keyboard_apps
                .iter()
                .find(|(_, root)| {
                    root.is_some_and(|root| {
                        root == pid || self.procs.descendants(root).contains(&pid)
                    })
                })
                .map(|(id, _)| id.clone()),
        };
        let Some(id) = owner else {
            debug!(
                ?pid,
                ?focus,
                "text field focus from no keyboard-profile app; ignored"
            );
            return;
        };
        self.text_fields.insert(id.clone(), focus);
        if self.focused.as_deref() != Some(id.as_str()) || !self.cfg.osk_enabled {
            debug!(app = %id, ?focus, "text field focus in the background");
            return;
        }
        if focus.editable() {
            self.osk_auto_hide_at = None;
            if self.cfg.osk_auto_show && !self.osk_visible {
                info!(app = %id, ?focus, "text field focused → on-screen keyboard");
                self.set_osk(true, true);
                self.osk_auto = true;
            }
        } else if self.osk_visible && self.osk_auto && self.osk_auto_hide_at.is_none() {
            debug!(app = %id, "text field lost focus; hiding the keyboard soon");
            self.osk_auto_hide_at = Some(now + OSK_AUTO_HIDE_DELAY);
        }
    }

    /// A in a keyboard-profile app: whether a text field has focus there, so that A shows the
    /// on-screen keyboard instead of pressing Enter.
    fn field_focused(&self) -> bool {
        self.cfg.osk_enabled
            && self.cfg.osk_field_accept
            && self
                .focused
                .as_ref()
                .and_then(|id| self.text_fields.get(id))
                .is_some_and(|f| f.editable())
    }

    /// Show or hide the on-screen keyboard; with `apply_mode`, switch InputPlumber to match.
    fn set_osk(&mut self, visible: bool, apply_mode: bool) {
        info!(visible, "on-screen keyboard");
        self.osk_visible = visible;
        // Whoever shows or hides it now owns it (text_focus marks its own).
        self.osk_auto = false;
        self.osk_auto_hide_at = None;
        self.repeat.clear();
        self.held.clear();
        if let Err(e) = self.focus.set_osk_visible(visible) {
            warn!("showing/hiding the on-screen keyboard: {e:#}");
        }
        if apply_mode {
            self.apply_input();
        }
        self.emit(EventName::OskVisibility, OskState { visible });
    }

    fn keys_type(&mut self, p: KeysTypeParams) -> Result<KeysTypeResult, ErrorObject> {
        let n = p.text.chars().count();
        if n == 0 || n > KEYS_TYPE_MAX_CHARS {
            return Err(ErrorObject::new(
                ErrorCode::InvalidParams,
                format!("text must have 1 to {KEYS_TYPE_MAX_CHARS} characters"),
            ));
        }
        let caps = self.keys.caps();
        if !caps.any() {
            return Err(ErrorObject::new(
                ErrorCode::KeyboardUnavailable,
                "no virtual keyboard (/dev/uinput) and no gamescope input method",
            ));
        }
        let (ops, untypeable) = plan_text(&p.text, caps);
        if !untypeable.is_empty() {
            info!(?untypeable, "cannot type some characters");
        }
        self.keys.send(ops);
        Ok(KeysTypeResult { untypeable })
    }

    fn keys_key(&mut self, p: KeysKeyParams) -> Result<KeysKeyResult, ErrorObject> {
        if !self.keys.caps().keys {
            return Err(ErrorObject::new(
                ErrorCode::KeyboardUnavailable,
                "no virtual keyboard (/dev/uinput)",
            ));
        }
        let mut modifiers = p.modifiers;
        let mut seen = BTreeSet::new();
        modifiers.retain(|m| seen.insert(*m));
        self.keys.send(vec![KeyOp::tap(&KeyCombo {
            key: p.key,
            modifiers,
        })]);
        Ok(KeysKeyResult {})
    }

    fn keys_action(&mut self, p: KeysActionParams) -> Result<KeysActionResult, ErrorObject> {
        let op = plan_action(p.action, self.keys.caps()).ok_or_else(|| {
            let name = serde_json::to_value(p.action).unwrap_or_default();
            ErrorObject::new(
                ErrorCode::KeyboardUnavailable,
                format!(
                    "no virtual keyboard (/dev/uinput) for '{}'",
                    name.as_str().unwrap_or_default()
                ),
            )
        })?;
        self.keys.send(vec![op]);
        Ok(KeysActionResult {})
    }

    /// Whether the user sees Steam's windows: Steam has focus, or a focused Steam game shows
    /// them in front of the launcher (it has no window yet, or gamescope shows Steam).
    fn steam_shown(&self) -> bool {
        match self.focused_app() {
            Some(app) if app.info.id == STEAM_CLIENT_APP_ID => true,
            Some(app) if app.info.source == AppSource::Steam => {
                (!app.seen_focusable && !app.seen_focused)
                    || self
                        .gamescope
                        .as_ref()
                        .is_some_and(|g| g.focused_app == Some(STEAM_CLIENT_GAMESCOPE_APP_ID))
            }
            _ => false,
        }
    }

    /// New list of Steam's windows from the gamescope watcher. A new window while the launcher
    /// has focus, within [`STEAM_ANSWER_WINDOW`] of our own Steam request, is Steam's answer to
    /// it (an error such as "Game already running", a pre-launch dialog): Steam gets focus, with
    /// the pad (PASS), until its windows close (or Home). Other new windows only raise
    /// `steam_attention`.
    fn steam_windows(&mut self, mut windows: Vec<SteamWindow>, now: Instant) {
        if self.cfg.steam_path.is_none() {
            // No Steam app to bring them forward as (and no Steam to have made them).
            windows.clear();
        }
        self.seen_windows
            .retain(|w| windows.iter().any(|s| s.window == *w));
        if self.steam_shown() {
            self.seen_windows.extend(windows.iter().map(|w| w.window));
        }
        let new = windows
            .iter()
            .any(|w| !self.steam_windows.iter().any(|s| s.window == w.window));
        self.steam_windows = windows;
        let answer = self
            .steam_action_at
            .is_some_and(|at| now.duration_since(at) <= STEAM_ANSWER_WINDOW);
        if new && answer && self.focused.is_none() {
            info!("a Steam window answers our Steam request → Steam gets focus");
            if let Err(e) = self.steam_focus(now) {
                debug!("{}", e.message);
            }
        }
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
        if self.osk_visible {
            // The launcher's mode is applied by set_focus.
            self.set_osk(false, false);
        }
        self.emit(EventName::HomePressed, HomePressedParams {});
        self.set_focus(None);
    }

    /// Guide pressed or released. Without a summon chord, Home on press. With one, Home on
    /// release unless the summon button was pressed while Guide was held (then the on-screen
    /// keyboard toggles instead). In `Pass` mode InputPlumber only sends us Guide, and switches
    /// to `All` by itself on the press, so the chord button pressed *after* Guide reaches us.
    ///
    /// Every step is logged at info level with the raw `ui_*` names, so that a pad whose
    /// buttons arrive under other names than expected shows in the journal.
    fn guide(&mut self, pressed: bool, now: Instant) {
        let summon = self.cfg.osk_summon.filter(|_| self.cfg.osk_enabled);
        let Some(summon) = summon else {
            if pressed {
                info!("guide pressed → Home (no chord configured)");
                self.home();
            }
            return;
        };
        if pressed {
            self.guide_others.clear();
            // The button already held (visible to us in `All` mode) also makes a chord.
            let chord = self.held.contains(&summon);
            if chord {
                let raw = self.raw_names.get(&summon).cloned().unwrap_or_default();
                info!(
                    button = %raw,
                    "chord {}+guide → on-screen keyboard",
                    nav_name(summon)
                );
                self.toggle_osk();
            } else {
                info!(
                    "guide pressed: chord started (guide+{} toggles the on-screen keyboard)",
                    nav_name(summon)
                );
            }
            self.guide = Some((now, chord));
        } else if let Some((_, chord)) = self.guide.take() {
            let others = std::mem::take(&mut self.guide_others);
            if chord {
                info!("guide released after the chord: no Home");
            } else if others.is_empty() {
                info!("guide released alone → Home");
                self.home();
            } else {
                info!(buttons = ?others, "guide released (no chord) → Home");
                self.home();
            }
        } else {
            debug!("guide released without a press");
        }
    }

    /// A button pressed while Guide is held that is not the chord button.
    fn guide_other(&mut self, raw: &str, action: Option<NavAction>, now: Instant) {
        let held = self
            .guide
            .is_some_and(|(since, _)| now.duration_since(since) < GUIDE_HOLD_MAX);
        if !held || self.cfg.osk_summon.is_none() || !self.cfg.osk_enabled {
            return;
        }
        let name = action.map_or_else(|| "unmapped".to_string(), nav_name);
        info!(button = %raw, "guide+{name} → ignored (not a chord)");
        self.guide_others.push(raw.to_string());
    }

    fn input(&mut self, input: RawInput, now: Instant) {
        let Some(intent) = translate(&input.event) else {
            debug!(event = %input.event, "unmapped input");
            if input.pressed() {
                self.guide_other(&input.event, None, now);
            }
            return;
        };
        let pressed = input.pressed();
        let action = match intent {
            Intent::Home => return self.guide(pressed, now),
            Intent::Pointer => {
                if pressed && self.pointer_app() {
                    let change = self.pointer.motion();
                    self.cursor_change(change);
                }
                return;
            }
            Intent::Nav(action) => action,
        };
        if self
            .debounce
            .drop_event(action, &input.source, pressed, now)
        {
            debug!(?action, pressed, "stick chatter dropped");
            return;
        }
        if pressed {
            let guide_held = self
                .guide
                .is_some_and(|(since, _)| now.duration_since(since) < GUIDE_HOLD_MAX);
            if guide_held && self.cfg.osk_summon == Some(action) && self.cfg.osk_enabled {
                info!(
                    button = %input.event,
                    "chord guide+{} → on-screen keyboard",
                    nav_name(action)
                );
                self.guide = self.guide.map(|(since, _)| (since, true));
                self.swallow.insert(action);
                self.toggle_osk();
                return;
            }
            self.guide_other(&input.event, Some(action), now);
            self.held.insert(action);
            self.raw_names.insert(action, input.event.clone());
        } else {
            self.held.remove(&action);
            if let Some(source) = self.clicking.remove(&action) {
                self.input.mouse_button(&source, MouseButton::Left, false);
                return;
            }
            if self.swallow.remove(&action) {
                return;
            }
        }
        let route = self.route();
        // B closes the on-screen keyboard, whoever has focus (the keyboard never sees it).
        if route == Route::Osk && action == NavAction::Back {
            if pressed {
                info!("back → on-screen keyboard hidden");
                self.swallow.insert(action);
                self.hide_osk();
            }
            return;
        }
        if route == Route::Keys && pressed && self.keys_button(action, &input.source) {
            if !self.clicking.contains_key(&action) {
                self.swallow.insert(action);
            }
            return;
        }
        // The keyboard button in a keyboard-profile app summons the on-screen keyboard.
        if action == NavAction::Keyboard && route == Route::Keys {
            if pressed {
                self.toggle_osk();
            }
            return;
        }
        if route == Route::App {
            // The app gets the pad itself (Pass mode); only stray events land here.
            return;
        }
        if pressed {
            self.repeat.press(action, &input.source, now);
        } else {
            self.repeat.release(action, &input.source);
        }
        match route {
            Route::Keys if pressed => self.send_mapped(action),
            Route::Keys | Route::App => {}
            Route::Launcher | Route::Osk => {
                let phase = if pressed {
                    NavPhase::Press
                } else {
                    NavPhase::Release
                };
                self.nav(action, phase, route);
            }
        }
    }

    /// A button pressed in a keyboard-profile app that means more than its key: a click (RT) with
    /// the pointer, the on-screen keyboard for a text field, the address bar. Returns whether
    /// it was handled (its release is then dropped, or ends the click).
    fn keys_button(&mut self, action: NavAction, source: &str) -> bool {
        let pointer = self.pointer_app();
        if pointer && action.is_direction() {
            // D-pad or left stick: the pointer is done.
            let change = self.pointer.navigated();
            self.cursor_change(change);
            return false;
        }
        let Some(app) = self.keys_app() else {
            return false;
        };
        let mapped = |a: NavAction| app.keymap.as_ref().is_some_and(|m| m.contains_key(&a));
        let (rt_mapped, view_mapped) = (mapped(NavAction::TriggerRight), mapped(NavAction::View));
        let address_bar = app.address_bar;
        // RT is the left click whenever there is a pointer (unless mapped); A never clicks (it
        // is Enter, or the on-screen keyboard on a text field).
        let click = pointer && action == NavAction::TriggerRight && !rt_mapped;
        if click {
            debug!(?action, "click");
            let change = self.pointer.motion();
            self.cursor_change(change);
            self.input.mouse_button(source, MouseButton::Left, true);
            self.clicking.insert(action, source.to_string());
            // Its release ends the click (see `clicking`).
            return true;
        }
        if action == NavAction::Accept && self.field_focused() {
            info!(app = ?self.focused, "accept on a text field → on-screen keyboard");
            if !self.osk_visible {
                self.set_osk(true, true);
                self.osk_auto = true;
            }
            return true;
        }
        if action == NavAction::View && address_bar && !view_mapped {
            info!(app = ?self.focused, "view → address bar and on-screen keyboard");
            self.keys
                .send(vec![KeyOp::tap(&"ctrl+l".parse().expect("valid combo"))]);
            if self.cfg.osk_enabled && !self.osk_visible {
                self.set_osk(true, true);
            }
            return true;
        }
        false
    }

    /// `input_nav` to the launcher or the on-screen keyboard.
    fn nav(&self, action: NavAction, phase: NavPhase, route: Route) {
        let audience = if route == Route::Osk {
            Audience::Osk
        } else {
            Audience::Launcher
        };
        self.emit_to(
            EventName::InputNav,
            InputNavParams { action, phase },
            audience,
        );
    }

    /// The key the focused keyboard-profile app maps `action` to, if any.
    fn send_mapped(&mut self, action: NavAction) {
        let combo = self
            .focused_app()
            .and_then(|a| a.keymap.as_ref())
            .and_then(|m| m.get(&action))
            .cloned();
        if let Some(combo) = combo {
            debug!(?action, key = %combo, "key");
            if action.is_direction() {
                // Held or repeating: never queued behind other navigation.
                self.keys.send_nav(KeyOp::tap(&combo));
            } else {
                self.keys.send(vec![KeyOp::tap(&combo)]);
            }
        }
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
        self.text_fields.remove(id);
        let Some(app) = self.apps.remove(id) else {
            return;
        };
        let reason = if app.closing.is_some() {
            ExitReason::Closed
        } else {
            reason
        };
        info!(app = id, ?reason, ?exit_code, "exited");
        self.cfg.icon_refresh.app_exited(id);
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
                if self.osk_visible
                    && let Err(e) = self.focus.set_osk_visible(true)
                {
                    warn!("showing the on-screen keyboard: {e:#}");
                }
            }
            GamescopeEvent::Disconnected => {
                self.gamescope = None;
                self.focus_lost_at = None;
                self.steam_windows(Vec::new(), now);
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
                if self.steam_shown() {
                    self.seen_windows
                        .extend(self.steam_windows.iter().map(|w| w.window));
                    self.update_attention();
                }
            }
            GamescopeEvent::SteamWindows(windows) => self.steam_windows(windows, now),
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
            .chain(self.focus_retry_at)
            .chain(self.osk_auto_hide_at)
            .min()
    }

    pub fn tick(&mut self, now: Instant) {
        if self.focus_retry_at.is_some_and(|at| now >= at) {
            self.focus_retry_at = None;
            self.apply_focus();
        }
        if self.osk_auto_hide_at.is_some_and(|at| now >= at) {
            self.osk_auto_hide_at = None;
            if self.osk_visible && self.osk_auto {
                info!("text field no longer focused → keyboard hidden");
                self.set_osk(false, true);
            }
        }
        if let Some(action) = self.repeat.tick(now) {
            match self.route() {
                route @ (Route::Launcher | Route::Osk) => self.nav(action, NavPhase::Repeat, route),
                Route::Keys => self.send_mapped(action),
                Route::App => self.repeat.clear(),
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
            if self.steam_game_still_running(&id, reason) {
                let app = self.apps.get_mut(&id).expect("listed above");
                match reason {
                    // Look again after another debounce.
                    ExitReason::Exited => app.gone_since = Some(now),
                    // Give it another launch timeout.
                    _ => app.started = now,
                }
                continue;
            }
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
                if self.steam_game_still_running(&id, ExitReason::Exited) {
                    // Between windows (a launcher, a loading screen): Steam's windows or the
                    // launcher show meanwhile; look again later.
                    self.focus_lost_at = Some(now + self.cfg.focus_loss_debounce);
                } else if native {
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

/// A navigation action's wire name (`context`, `secondary`, ...), for logs.
fn nav_name(action: NavAction) -> String {
    serde_json::to_value(action)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| format!("{action:?}"))
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
    use crate::config::InputProfile;
    use crate::keys::KeyCaps;

    /// Everything the shell did to the outside world, in order.
    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        FocusLauncher,
        FocusApp(u32),
        /// `[game, 769, launcher]`.
        FocusSteamGame(u32),
        Mode(InterceptMode),
        Spawn {
            key: String,
            argv: Vec<String>,
            app_id: Option<u32>,
        },
        Signal(u32, Sig),
        Osk(bool),
        Keys(Vec<KeyOp>),
        Pointer(bool),
        Click(bool),
        Cursor(bool),
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
        fn focus_steam_game(&mut self, app_id: u32) -> anyhow::Result<()> {
            self.0.push(Call::FocusSteamGame(app_id));
            Ok(())
        }
        fn set_osk_visible(&mut self, visible: bool) -> anyhow::Result<()> {
            self.0.push(Call::Osk(visible));
            Ok(())
        }
    }

    struct MockKeys {
        log: Log,
        caps: KeyCaps,
    }
    impl KeyInjector for MockKeys {
        fn caps(&self) -> KeyCaps {
            self.caps
        }
        fn send(&mut self, ops: Vec<KeyOp>) {
            self.log.push(Call::Keys(ops));
        }
    }

    struct MockInput(Log);
    impl InputControl for MockInput {
        fn set_mode(&mut self, mode: InterceptMode) {
            self.0.push(Call::Mode(mode));
        }
        fn set_pointer(&mut self, on: bool) {
            self.0.push(Call::Pointer(on));
        }
        fn mouse_button(&mut self, _source: &str, button: MouseButton, pressed: bool) {
            assert_eq!(button, MouseButton::Left);
            self.0.push(Call::Click(pressed));
        }
    }

    struct MockCursor(Log);
    impl CursorControl for MockCursor {
        fn set_visible(&mut self, visible: bool) {
            self.0.push(Call::Cursor(visible));
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
        fn steam_games(&mut self) -> Vec<u32> {
            let t = self.table.lock().unwrap();
            t.steam_reapers
                .iter()
                .filter(|(_, pids)| !pids.is_empty())
                .map(|(id, _)| *id)
                .collect()
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

    fn app_config(id: &str, command: Option<&str>, steam_app_id: Option<u32>) -> AppConfig {
        AppConfig {
            id: id.into(),
            name: id.to_uppercase(),
            kind: AppKind::Native,
            icon: None,
            desktop_id: None,
            icon_url: None,
            site_url: None,
            firefox_profile: None,
            command: command.map(|c| vec![c.into()]),
            steam_app_id,
            input: InputProfile::Gamepad,
            back_key: None,
            keymap: BTreeMap::new(),
            address_bar: false,
            pointer: None,
        }
    }

    const ALL_KEYS: KeyCaps = KeyCaps {
        keys: true,
        ime: true,
    };

    impl T {
        fn new() -> Self {
            Self::with(|_| {})
        }

        fn with(f: impl FnOnce(&mut ShellConfig)) -> Self {
            Self::with_keys(ALL_KEYS, f)
        }

        fn with_keys(caps: KeyCaps, f: impl FnOnce(&mut ShellConfig)) -> Self {
            let mut cfg = ShellConfig {
                launcher_app_id: L,
                steam_path: Some("/fake/steam".into()),
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
                        name: "Jellyfin".into(),
                        ..app_config("jellyfin", Some("jellyfin-desktop"), None)
                    },
                    AppConfig {
                        name: "Portal".into(),
                        ..app_config("portal", None, Some(400))
                    },
                    AppConfig {
                        input: InputProfile::Keyboard,
                        back_key: Some("backspace".into()),
                        keymap: BTreeMap::from([("context".into(), "f".into())]),
                        ..app_config("youtube", Some("firefox"), None)
                    },
                ],
                osk_enabled: true,
                osk_auto_show: false,
                osk_field_accept: true,
                pointer_enabled: true,
                direction_debounce: MS(40),
                osk_summon: None,
                icons: IconStore::default(),
                icon_refresh: Refresher::default(),
            };
            f(&mut cfg);
            let log = Log::default();
            let procs = Arc::new(Mutex::new(ProcTable::default()));
            let (tx, events) = broadcast::channel(256);
            let mut shell = Shell::new(
                cfg,
                Box::new(MockFocus(log.clone())),
                Box::new(MockInput(log.clone())),
                Box::new(MockKeys {
                    log: log.clone(),
                    caps,
                }),
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
            )
            .with_cursor(Box::new(MockCursor(log.clone())));
            shell.start();
            assert_eq!(
                log.take(),
                [
                    Call::FocusLauncher,
                    Call::Mode(InterceptMode::All),
                    Call::Osk(false)
                ]
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
            self.events_to()
                .into_iter()
                .map(|(n, p, _)| (n, p))
                .collect()
        }

        fn events_to(&mut self) -> Vec<Broadcast> {
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
                Call::FocusSteamGame(570),
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
    fn apps_list_reports_resolved_icons() {
        let icons = IconStore::default();
        icons.set("jellyfin", Some("/icons/jellyfin.png".into()));
        icons.set("steam", Some("/icons/steam.svg".into()));
        let mut t = T::with(|c| {
            c.icons = icons.clone();
            c.apps[1].icon = Some("/configured/portal.png".into());
            c.apps[2].icon = Some("theme-name".into());
        });
        let list = t.call("apps.list", json!({})).unwrap();
        let icon = |i: usize| list["apps"][i]["icon"].as_str().map(String::from);
        assert_eq!(icon(0).as_deref(), Some("/icons/steam.svg"));
        assert_eq!(icon(1).as_deref(), Some("/icons/jellyfin.png"));
        // A configured absolute path stands even if the resolver has nothing.
        assert_eq!(icon(2).as_deref(), Some("/configured/portal.png"));
        // A theme name alone is not a path.
        assert_eq!(icon(3), None);
    }

    #[test]
    fn without_steam_there_is_no_steam_app() {
        let mut t = T::with(|c| c.steam_path = None);
        let ids = |t: &mut T| -> Vec<String> {
            let list = t.call("apps.list", json!({})).unwrap();
            list["apps"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a["id"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(ids(&mut t), ["jellyfin", "portal", "youtube"]);
        // Launching it is "no such app", not a failed spawn; Steam games cannot start either.
        let e = t.call("apps.launch", json!({"id": "steam"})).unwrap_err();
        assert_eq!(e.code, ErrorCode::AppNotFound.code());
        assert!(
            e.message.contains("Steam is not installed"),
            "{}",
            e.message
        );
        let e = t.call("apps.launch", json!({"id": "portal"})).unwrap_err();
        assert_eq!(e.code, ErrorCode::AppLaunchFailed.code());
        assert!(
            e.message.contains("Steam is not installed"),
            "{}",
            e.message
        );
        assert!(t.log.take().is_empty(), "nothing spawned");
        // No attention for windows that look like Steam's.
        steam_windows(&mut t, &[(5, "Steam")]);
        assert!(t.events().is_empty());
        assert_eq!(
            t.call("steam.attention", json!({})).unwrap(),
            json!({"windows": []})
        );
        assert_eq!(
            t.call("steam.focus", json!({})).unwrap_err().code,
            ErrorCode::SteamNoWindows.code()
        );

        // Installed later: the entry comes back and launches.
        t.shell
            .handle(Msg::SetSteam(Some("/new/steam".into())), t.now);
        assert_eq!(ids(&mut t), ["steam", "jellyfin", "portal", "youtube"]);
        let app = t.launch(json!({"id": "steam"}));
        assert_eq!(app.id, "steam");
        assert_eq!(
            t.log.take()[0],
            Call::Spawn {
                key: "steam".into(),
                argv: vec!["/new/steam".into(), "-gamepadui".into()],
                app_id: None,
            }
        );
        // And its windows count again.
        t.call("apps.focus", json!({"id": null})).unwrap();
        t.events();
        // Long after our own Steam request: only attention, no focus.
        t.advance(STEAM_ANSWER_WINDOW + MS(1));
        steam_windows(&mut t, &[(6, "Friends")]);
        assert!(
            t.events()
                .iter()
                .any(|(n, p)| *n == EventName::SteamAttention && p["windows"][0]["window"] == 6)
        );

        // Removed again: attention clears; the running entry stays focusable until it ends.
        t.shell.handle(Msg::SetSteam(None), t.now);
        assert_eq!(
            t.events(),
            [ev(EventName::SteamAttention, json!({"windows": []}))]
        );
        assert_eq!(ids(&mut t), ["jellyfin", "portal", "youtube"]);
        assert_eq!(t.launch(json!({"id": "steam"})).id, "steam");
    }

    #[test]
    fn a_reloaded_app_list_leaves_running_apps_alone() {
        let mut t = T::new();
        let ids = |t: &mut T| -> Vec<String> {
            let list = t.call("apps.list", json!({})).unwrap();
            list["apps"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a["id"].as_str().unwrap().to_string())
                .collect()
        };
        let before = ids(&mut t);
        let first = before[1].clone();
        let launched = t.launch(json!({"id": first}));
        t.shell.handle(
            Msg::SetApps(vec![app_config("new", Some("true"), None)]),
            t.now,
        );
        assert_eq!(ids(&mut t), ["steam", "new"]);
        // The removed app keeps running, cannot be launched again, and can still be closed.
        let running = t.call("apps.running", json!({})).unwrap();
        assert_eq!(running["apps"][0]["id"], launched.id);
        let e = t.call("apps.launch", json!({"id": first})).unwrap_err();
        assert_eq!(e.code, ErrorCode::AppNotFound.code());
        t.call("apps.close", json!({"id": launched.id})).unwrap();
        t.launch(json!({"id": "new"}));
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
            [Call::FocusSteamGame(620), Call::Mode(InterceptMode::Pass)]
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
    fn tap(key: &str) -> KeyOp {
        KeyOp::tap(&key.parse().unwrap())
    }

    fn keys(ops: &[&str]) -> Call {
        Call::Keys(ops.iter().map(|k| tap(k)).collect())
    }

    #[test]
    fn keyboard_profile_turns_the_pad_into_keys() {
        let mut t = T::new();
        let app = t.launch(json!({"id": "youtube"}));
        // InputPlumber intercepts the pad (the daemon translates), except for the right
        // stick, which the pointer profile turns into mouse motion (GamepadOnly).
        assert_eq!(
            t.log.take()[1..],
            [
                Call::FocusApp(app.gamescope_app_id),
                Call::Pointer(true),
                Call::Mode(InterceptMode::GamepadOnly)
            ]
        );
        t.events();

        t.input("ui_down", 1.0);
        t.advance(MS(450)); // repeat at 400
        t.input("ui_down", 0.0);
        t.input("ui_accept", 1.0);
        t.input("ui_accept", 0.0);
        t.input("ui_back", 1.0);
        t.input("ui_back", 0.0);
        t.input("ui_context", 1.0); // mapped in [apps.keymap]
        t.input("ui_action", 1.0); // not mapped
        assert_eq!(
            t.log.take(),
            [
                // The D-pad hides the pointer (until the right stick moves).
                Call::Cursor(false),
                keys(&["down"]),
                keys(&["down"]),
                keys(&["enter"]),
                keys(&["backspace"]),
                keys(&["f"]),
            ]
        );
        assert!(t.events().is_empty(), "no input_nav for the app");

        // Guide is still Home; the launcher gets input_nav again.
        t.input("ui_guide", 1.0);
        assert_eq!(t.shell.focused(), None);
        t.events();
        t.advance(MS(100)); // not stick chatter
        t.input("ui_down", 1.0);
        assert_eq!(
            t.events_to(),
            [(
                EventName::InputNav,
                json!({"action": "down", "phase": "press"}),
                Audience::Launcher
            )]
        );
        assert!(!t.log.take().iter().any(|c| matches!(c, Call::Keys(_))));
    }

    #[test]
    fn keyboard_profile_without_uinput_falls_back_to_the_gamepad() {
        let ime_only = KeyCaps {
            keys: false,
            ime: true,
        };
        let mut t = T::with_keys(ime_only, |_| {});
        t.launch(json!({"id": "youtube"}));
        assert_eq!(t.log.take().last(), Some(&Call::Mode(InterceptMode::Pass)));
        t.input("ui_down", 1.0);
        assert!(t.log.take().is_empty());
    }

    #[test]
    fn osk_show_hide_routes_nav_and_restores_the_mode() {
        let mut t = T::new();
        t.launch(json!({"id": "jellyfin"}));
        t.events();
        t.log.take();

        let r = t.call("osk.show", json!({})).unwrap();
        assert_eq!(r, json!({"visible": true}));
        assert!(t.shell.osk_visible());
        assert_eq!(
            t.log.take(),
            [Call::Osk(true), Call::Mode(InterceptMode::All)]
        );
        assert_eq!(
            t.events(),
            [ev(EventName::OskVisibility, json!({"visible": true}))]
        );
        // Again: nothing changes.
        t.call("osk.show", json!({})).unwrap();
        assert!(t.log.take().is_empty() && t.events().is_empty());

        // Navigation goes to the OSK client only, with repeat.
        t.input("ui_right", 1.0);
        t.advance(MS(400));
        t.input("ui_right", 0.0);
        let to_osk = |action: &str, phase: &str| {
            (
                EventName::InputNav,
                json!({"action": action, "phase": phase}),
                Audience::Osk,
            )
        };
        assert_eq!(
            t.events_to(),
            [
                to_osk("right", "press"),
                to_osk("right", "repeat"),
                to_osk("right", "release")
            ]
        );

        // Hidden: the gamepad app gets the pad again.
        assert_eq!(
            t.call("osk.hide", json!({})).unwrap(),
            json!({"visible": false})
        );
        assert_eq!(
            t.log.take(),
            [Call::Osk(false), Call::Mode(InterceptMode::Pass)]
        );
        assert_eq!(
            t.events(),
            [ev(EventName::OskVisibility, json!({"visible": false}))]
        );
        t.call("osk.hide", json!({})).unwrap();
        assert!(t.events().is_empty());
        t.input("ui_right", 1.0);
        assert!(t.events().is_empty());
    }

    #[test]
    fn home_and_focus_changes_hide_the_osk() {
        let mut t = T::new();
        t.launch(json!({"id": "jellyfin"}));
        t.call("osk.show", json!({})).unwrap();
        t.events();
        t.log.take();
        t.input("ui_guide", 1.0);
        assert_eq!(
            t.events(),
            [
                ev(EventName::OskVisibility, json!({"visible": false})),
                ev(EventName::HomePressed, json!({})),
                ev(EventName::FocusChanged, json!({"app": null})),
            ]
        );
        assert_eq!(
            t.log.take(),
            [
                Call::Osk(false),
                Call::FocusLauncher,
                Call::Mode(InterceptMode::All)
            ]
        );

        // Shown over the launcher, then an app is launched.
        t.call("osk.show", json!({})).unwrap();
        t.launch(json!({"command": ["game"]}));
        assert!(!t.shell.osk_visible());
        assert!(
            t.events()
                .contains(&ev(EventName::OskVisibility, json!({"visible": false})))
        );
        // gamescope restarts: the visibility is re-sent only while shown.
        t.call("osk.show", json!({})).unwrap();
        t.log.take();
        t.shell
            .handle(Msg::Gamescope(GamescopeEvent::Connected), t.now);
        assert_eq!(
            t.log.take(),
            [
                Call::FocusApp(L + 2),
                Call::Mode(InterceptMode::All),
                Call::Osk(true)
            ]
        );
    }

    #[test]
    fn osk_disabled() {
        let mut t = T::with(|c| {
            c.osk_enabled = false;
            c.osk_summon = Some(NavAction::Context);
        });
        assert_eq!(
            t.call("osk.show", json!({})).unwrap_err().code,
            ErrorCode::OskUnavailable.code()
        );
        assert_eq!(
            t.call("osk.hide", json!({})).unwrap(),
            json!({"visible": false})
        );
        // No chord either: Guide is Home on press.
        t.input("ui_guide", 1.0);
        assert_eq!(t.events()[0], ev(EventName::HomePressed, json!({})));
    }

    #[test]
    fn guide_chord_summons_the_osk_in_a_game() {
        let mut t = T::with(|c| c.osk_summon = Some(NavAction::Context));
        t.launch(json!({"id": "portal"}));
        t.events();
        t.log.take();

        // Pass mode: Guide arrives (InputPlumber flips to All itself), then Y while held.
        t.input("ui_guide", 1.0);
        assert!(t.events().is_empty(), "Home waits for the release");
        t.input("ui_context", 1.0);
        assert!(t.shell.osk_visible());
        t.input("ui_context", 0.0);
        t.input("ui_guide", 0.0);
        assert_eq!(
            t.events(),
            [ev(EventName::OskVisibility, json!({"visible": true}))]
        );
        assert_eq!(t.shell.focused(), Some("portal"));
        assert_eq!(
            t.log.take(),
            [Call::Osk(true), Call::Mode(InterceptMode::All)]
        );

        // The same chord hides it and gives the game the pad back.
        t.input("ui_guide", 1.0);
        t.input("ui_context", 1.0);
        t.input("ui_guide", 0.0);
        t.input("ui_context", 0.0);
        assert!(!t.shell.osk_visible());
        assert_eq!(
            t.log.take(),
            [Call::Osk(false), Call::Mode(InterceptMode::Pass)]
        );
        assert_eq!(
            t.events(),
            [ev(EventName::OskVisibility, json!({"visible": false}))]
        );

        // Guide alone: Home on release.
        t.input("ui_guide", 1.0);
        assert_eq!(t.shell.focused(), Some("portal"));
        t.input("ui_guide", 0.0);
        assert_eq!(t.shell.focused(), None);
        assert_eq!(t.events()[0], ev(EventName::HomePressed, json!({})));
    }

    impl T {
        fn text_focus(&mut self, pid: u32, focus: FieldFocus) {
            self.shell.handle(
                Msg::TextFocus {
                    pid: Some(pid),
                    focus,
                },
                self.now,
            );
        }
    }

    /// The keyboard-profile app `youtube` (Firefox) running with a native-messaging host (pid
    /// 555) in its process tree.
    /// `[osk] auto_show = true` (off by default).
    fn auto_show(c: &mut ShellConfig) {
        c.osk_auto_show = true;
    }

    fn firefox_with_host(f: impl FnOnce(&mut ShellConfig)) -> T {
        let mut t = T::with(f);
        let app = t.launch(json!({"id": "youtube"}));
        let root = app.pid.unwrap();
        t.procs
            .lock()
            .unwrap()
            .trees
            .insert(root, vec![root + 1, 555]);
        t.events();
        t.log.take();
        t
    }

    #[test]
    fn text_field_focus_shows_and_hides_the_keyboard() {
        let mut t = firefox_with_host(auto_show);
        t.text_focus(555, FieldFocus::Text);
        assert!(t.shell.osk_visible());
        assert_eq!(
            t.log.take(),
            [Call::Osk(true), Call::Mode(InterceptMode::All)]
        );
        assert_eq!(
            t.events(),
            [ev(EventName::OskVisibility, json!({"visible": true}))]
        );
        // Moving to the next field (none, then text): no flicker.
        t.text_focus(555, FieldFocus::None);
        t.advance(MS(100));
        t.text_focus(555, FieldFocus::Password);
        t.advance(MS(1000));
        assert!(t.shell.osk_visible());
        assert!(t.log.take().is_empty() && t.events().is_empty());

        // Leaving the field: hidden after the delay, keyboard profile again.
        t.text_focus(555, FieldFocus::None);
        t.advance(OSK_AUTO_HIDE_DELAY - MS(1));
        assert!(t.shell.osk_visible());
        t.advance(MS(1));
        assert!(!t.shell.osk_visible());
        assert_eq!(
            t.log.take(),
            [Call::Osk(false), Call::Mode(InterceptMode::GamepadOnly)]
        );
        assert_eq!(
            t.events(),
            [ev(EventName::OskVisibility, json!({"visible": false}))]
        );
        assert_eq!(t.shell.deadline(), None);
    }

    #[test]
    fn text_field_focus_leaves_a_keyboard_someone_asked_for() {
        let mut t = firefox_with_host(auto_show);
        // Shown by hand, then a field comes and goes: it stays.
        t.call("osk.show", json!({})).unwrap();
        t.text_focus(555, FieldFocus::Text);
        t.text_focus(555, FieldFocus::None);
        t.advance(Duration::from_secs(1));
        assert!(t.shell.osk_visible());
        t.call("osk.hide", json!({})).unwrap();

        // Shown automatically, hidden by hand while the field still has focus: stays hidden
        // until a field gets focus again.
        t.text_focus(555, FieldFocus::Text);
        assert!(t.shell.osk_visible());
        t.call("osk.hide", json!({})).unwrap();
        t.text_focus(555, FieldFocus::None);
        t.advance(Duration::from_secs(1));
        assert!(!t.shell.osk_visible());
        t.text_focus(555, FieldFocus::Text);
        assert!(t.shell.osk_visible());

        // Shown automatically, then Home: hidden with the focus change, nothing pending.
        t.input("ui_guide", 1.0);
        assert!(!t.shell.osk_visible());
        t.text_focus(555, FieldFocus::None);
        t.advance(Duration::from_secs(1));
        assert_eq!(t.shell.deadline(), None);
        // A field in the background app does nothing while the launcher has focus.
        t.log.take();
        t.text_focus(555, FieldFocus::Text);
        assert!(!t.shell.osk_visible());
        assert!(t.log.take().is_empty());
    }

    #[test]
    fn text_field_focus_needs_the_focused_keyboard_app() {
        // A gamepad-profile app (Jellyfin) gets no automatic keyboard.
        let mut t = T::with(auto_show);
        let app = t.launch(json!({"id": "jellyfin"}));
        t.procs
            .lock()
            .unwrap()
            .trees
            .insert(app.pid.unwrap(), vec![555]);
        t.text_focus(555, FieldFocus::Text);
        assert!(!t.shell.osk_visible());

        // A host outside the focused app's process tree (another Firefox) is ignored.
        let mut t = firefox_with_host(auto_show);
        t.text_focus(999, FieldFocus::Text);
        assert!(!t.shell.osk_visible());
        // An unknown pid is believed (the socket only takes the session user).
        t.shell.handle(
            Msg::TextFocus {
                pid: None,
                focus: FieldFocus::Text,
            },
            t.now,
        );
        assert!(t.shell.osk_visible());

        // Turned off, or no keyboard at all.
        for f in [
            (|c: &mut ShellConfig| c.osk_auto_show = false) as fn(&mut ShellConfig),
            |c: &mut ShellConfig| {
                c.osk_auto_show = true;
                c.osk_enabled = false;
            },
        ] {
            let mut t = firefox_with_host(f);
            t.text_focus(555, FieldFocus::Text);
            assert!(!t.shell.osk_visible());
        }
    }

    #[test]
    fn guide_chord_from_the_launcher() {
        let mut t = T::with(|c| c.osk_summon = Some(NavAction::Context));
        // Y held first (seen in All mode), then Guide: also a chord.
        t.input("ui_context", 1.0);
        t.input("ui_guide", 1.0);
        assert!(t.shell.osk_visible());
        t.input("ui_guide", 0.0);
        t.input("ui_context", 0.0);
        let events = t.events_to();
        assert_eq!(
            events[0].2,
            Audience::Launcher,
            "Y press went to the launcher"
        );
        assert_eq!(
            (events[1].0, events[1].1.clone()),
            (EventName::OskVisibility, json!({"visible": true}))
        );
        // Its release goes to whoever has navigation now (the OSK), no Home.
        assert_eq!(
            events[2],
            (
                EventName::InputNav,
                json!({"action": "context", "phase": "release"}),
                Audience::Osk
            )
        );
        assert_eq!(events.len(), 3);

        // A stale Guide press (its release lost) no longer starts a chord.
        let mut t = T::with(|c| c.osk_summon = Some(NavAction::Context));
        t.input("ui_guide", 1.0);
        t.advance(Duration::from_secs(11));
        t.input("ui_context", 1.0);
        assert!(!t.shell.osk_visible());
    }

    #[test]
    fn keyboard_button_toggles_the_osk_in_a_keyboard_app() {
        let mut t = T::new();
        t.launch(json!({"id": "youtube"}));
        t.input("ui_osk", 1.0);
        t.input("ui_osk", 0.0);
        assert!(t.shell.osk_visible());
        // While visible the OSK gets everything, including the keyboard button.
        t.events();
        t.input("ui_osk", 1.0);
        assert_eq!(
            t.events_to(),
            [(
                EventName::InputNav,
                json!({"action": "keyboard", "phase": "press"}),
                Audience::Osk
            )]
        );
    }

    #[test]
    fn keys_methods() {
        let mut t = T::new();
        let r = t.call("keys.type", json!({"text": "Hi!é\u{1}"})).unwrap();
        assert_eq!(r, json!({"untypeable": ["\u{1}"]}));
        assert_eq!(
            t.log.take(),
            [Call::Keys(vec![
                tap("shift+h"),
                tap("i"),
                tap("shift+1"),
                KeyOp::Ime("é".into()),
            ])]
        );
        t.call(
            "keys.key",
            json!({"key": "l", "modifiers": ["ctrl", "ctrl"]}),
        )
        .unwrap();
        t.call("keys.action", json!({"action": "backspace"}))
            .unwrap();
        assert_eq!(t.log.take(), [keys(&["ctrl+l"]), keys(&["backspace"])]);
        let code = |r: Result<Value, ErrorObject>| r.unwrap_err().code;
        assert_eq!(
            code(t.call("keys.type", json!({"text": ""}))),
            ErrorCode::InvalidParams.code()
        );
        assert_eq!(
            code(t.call("keys.key", json!({"key": "hyper"}))),
            ErrorCode::InvalidParams.code()
        );

        // Input method only: text and some actions work, keys.key does not.
        let mut t = T::with_keys(
            KeyCaps {
                keys: false,
                ime: true,
            },
            |_| {},
        );
        assert_eq!(
            t.call("keys.type", json!({"text": "ab"})).unwrap(),
            json!({"untypeable": []})
        );
        t.call("keys.action", json!({"action": "enter"})).unwrap();
        assert_eq!(
            code(t.call("keys.action", json!({"action": "tab"}))),
            ErrorCode::KeyboardUnavailable.code()
        );
        assert_eq!(
            code(t.call("keys.key", json!({"key": "f5"}))),
            ErrorCode::KeyboardUnavailable.code()
        );

        // Nothing at all.
        let mut t = T::with_keys(KeyCaps::default(), |_| {});
        for (method, params) in [
            ("keys.type", json!({"text": "a"})),
            ("keys.key", json!({"key": "a"})),
            ("keys.action", json!({"action": "enter"})),
        ] {
            assert_eq!(
                code(t.call(method, params)),
                ErrorCode::KeyboardUnavailable.code(),
                "{method}"
            );
        }
        assert!(t.log.take().is_empty());
    }

    /// Key taps in the log (other calls dropped).
    fn key_calls(log: Vec<Call>) -> Vec<Call> {
        log.into_iter()
            .filter(|c| matches!(c, Call::Keys(_)))
            .collect()
    }

    /// The stick swept left and right a few times, crossing InputPlumber's threshold with
    /// chatter each time (press/release bursts a few ms apart), 120 ms per sweep.
    fn sweep(t: &mut T, sweeps: usize) {
        for i in 0..sweeps {
            let dir = if i % 2 == 0 { "ui_left" } else { "ui_right" };
            for _ in 0..3 {
                t.input(dir, 1.0);
                t.advance(MS(4));
                t.input(dir, 0.0);
                t.advance(MS(4));
            }
            t.input(dir, 1.0);
            t.advance(MS(90));
            t.input(dir, 0.0);
            t.advance(MS(6));
        }
    }

    #[test]
    fn rapid_back_and_forth_never_queues_moves() {
        // Keyboard-profile app: one key per sweep, none after the last release.
        let mut t = T::new();
        t.launch(json!({"id": "youtube"}));
        t.log.take();
        sweep(&mut t, 10);
        let keys = key_calls(t.log.take());
        assert_eq!(keys.len(), 10, "{keys:?}");
        for (i, k) in keys.iter().enumerate() {
            let want = if i % 2 == 0 { "left" } else { "right" };
            assert_eq!(k, &super::tests::keys(&[want]));
        }
        assert_eq!(t.shell.deadline(), None, "no repeat pending");
        t.advance(Duration::from_secs(3));
        assert!(t.log.take().is_empty(), "nothing after the last release");

        // The on-screen keyboard (input_nav): one press and one release per sweep, no repeat.
        t.call("osk.show", json!({})).unwrap();
        t.events();
        t.advance(MS(100));
        sweep(&mut t, 10);
        let nav: Vec<(String, String)> = t
            .events()
            .into_iter()
            .filter(|(n, _)| *n == EventName::InputNav)
            .map(|(_, p)| {
                (
                    p["action"].as_str().unwrap().to_string(),
                    p["phase"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(nav.len(), 20, "{nav:?}");
        assert!(nav.iter().all(|(_, phase)| phase != "repeat"));
        t.advance(Duration::from_secs(3));
        assert!(t.events().is_empty());

        // Held: repeats; a new direction takes over at once; a release stops it at once.
        t.input("ui_down", 1.0);
        t.advance(MS(450));
        t.input("ui_down", 0.0);
        t.input("ui_up", 1.0);
        t.advance(MS(50));
        t.input("ui_up", 0.0);
        t.advance(Duration::from_secs(2));
        let phases: Vec<String> = t
            .events()
            .into_iter()
            .map(|(_, p)| {
                format!(
                    "{} {}",
                    p["action"].as_str().unwrap(),
                    p["phase"].as_str().unwrap()
                )
            })
            .collect();
        assert_eq!(
            phases,
            [
                "down press",
                "down repeat",
                "down release",
                "up press",
                "up release"
            ]
        );
    }

    #[test]
    fn accept_on_a_text_field_summons_the_osk_and_back_closes_it() {
        let mut t = firefox_with_host(|_| {});
        // Focus alone does nothing (auto_show is off).
        t.text_focus(555, FieldFocus::Text);
        assert!(!t.shell.osk_visible());
        assert!(t.log.take().is_empty());

        // A: the keyboard, no Enter.
        t.input("ui_accept", 1.0);
        assert!(t.shell.osk_visible());
        t.input("ui_accept", 0.0);
        assert_eq!(
            t.log.take(),
            [Call::Osk(true), Call::Mode(InterceptMode::All)]
        );
        let to_osk = t.events_to();
        assert_eq!(
            to_osk,
            [(
                EventName::OskVisibility,
                json!({"visible": true}),
                Audience::All
            )],
            "the A release is not passed to the keyboard"
        );

        // B closes it; neither the keyboard nor the app sees B.
        t.input("ui_back", 1.0);
        assert!(!t.shell.osk_visible());
        t.input("ui_back", 0.0);
        assert_eq!(
            t.log.take(),
            [Call::Osk(false), Call::Mode(InterceptMode::GamepadOnly)]
        );
        assert_eq!(
            t.events(),
            [ev(EventName::OskVisibility, json!({"visible": false}))]
        );

        // The field is left: A is Enter again.
        t.text_focus(555, FieldFocus::None);
        t.input("ui_accept", 1.0);
        t.input("ui_accept", 0.0);
        assert_eq!(t.log.take(), [keys(&["enter"])]);

        // A field of a background app does not count; neither with field_summon = none.
        t.text_focus(555, FieldFocus::Password);
        t.call("apps.launch", json!({"id": "jellyfin"})).unwrap();
        t.call("apps.launch", json!({"id": "youtube"})).unwrap();
        t.log.take();
        t.input("ui_accept", 1.0);
        assert!(t.shell.osk_visible(), "remembered for the app");
        let mut t = firefox_with_host(|c| c.osk_field_accept = false);
        t.text_focus(555, FieldFocus::Text);
        t.input("ui_accept", 1.0);
        assert!(!t.shell.osk_visible());
        assert_eq!(t.log.take(), [keys(&["enter"])]);
    }

    #[test]
    fn back_closes_the_osk_over_anything() {
        // Over a game: the game gets the pad back.
        let mut t = T::new();
        t.launch(json!({"id": "portal"}));
        t.call("osk.show", json!({})).unwrap();
        t.log.take();
        t.events();
        t.input("ui_back", 1.0);
        t.input("ui_back", 0.0);
        assert!(!t.shell.osk_visible());
        assert_eq!(
            t.log.take(),
            [Call::Osk(false), Call::Mode(InterceptMode::Pass)]
        );
        assert_eq!(
            t.events(),
            [ev(EventName::OskVisibility, json!({"visible": false}))]
        );
        // Over the launcher: B after that goes to the launcher as usual.
        let mut t = T::new();
        t.call("osk.show", json!({})).unwrap();
        t.events();
        t.input("ui_back", 1.0);
        t.input("ui_back", 0.0);
        assert!(!t.shell.osk_visible());
        t.input("ui_back", 1.0);
        assert_eq!(
            t.events_to()[1..],
            [(
                EventName::InputNav,
                json!({"action": "back", "phase": "press"}),
                Audience::Launcher
            )]
        );
        // Other buttons still reach the keyboard (X, Y: its Backspace and Shift).
        t.call("osk.show", json!({})).unwrap();
        t.events();
        t.input("ui_action", 1.0);
        t.input("ui_context", 1.0);
        assert_eq!(
            t.events_to()
                .iter()
                .filter(|e| e.2 == Audience::Osk)
                .count(),
            2
        );
    }

    #[test]
    fn view_opens_the_address_bar_with_the_osk() {
        let browser = AppConfig {
            input: InputProfile::Keyboard,
            address_bar: true,
            pointer: Some(false),
            ..app_config("browser", Some("firefox"), None)
        };
        let mut t = T::with(|c| c.apps.push(browser));
        t.launch(json!({"id": "browser"}));
        assert_eq!(
            t.log.take().last(),
            Some(&Call::Mode(InterceptMode::All)),
            "no pointer for this app"
        );
        t.input("ui_select", 1.0);
        t.input("ui_select", 0.0);
        assert!(t.shell.osk_visible());
        assert_eq!(
            t.log.take(),
            [
                keys(&["ctrl+l"]),
                Call::Osk(true),
                Call::Mode(InterceptMode::All)
            ]
        );
        // Kiosk apps (no address bar) leave View alone (unmapped).
        let mut t = T::new();
        t.launch(json!({"id": "youtube"}));
        t.log.take();
        t.input("ui_select", 1.0);
        assert!(!t.shell.osk_visible());
        assert!(t.log.take().is_empty());
    }

    #[test]
    fn right_stick_pointer_clicks_and_hides() {
        let mut t = T::new();
        t.launch(json!({"id": "youtube"}));
        t.log.take();
        // A: Enter.
        t.input("ui_accept", 1.0);
        t.input("ui_accept", 0.0);
        assert_eq!(t.log.take(), [keys(&["enter"])]);

        // The right stick moves (activity events): A is still Enter, never a click.
        t.input("ui_quick2", 1.0);
        t.input("ui_quick2", 0.0);
        t.advance(MS(500));
        t.input("ui_accept", 1.0);
        t.advance(MS(80));
        t.input("ui_accept", 0.0);
        assert_eq!(t.log.take(), [keys(&["enter"])]);

        // RT is the left click; held, it drags (press ... release, with the stick moving).
        t.input("ui_r2", 1.0);
        t.input("ui_quick2", 1.0);
        t.advance(MS(400));
        t.input("ui_quick2", 0.0);
        t.input("ui_r2", 0.0);
        assert_eq!(t.log.take(), [Call::Click(true), Call::Click(false)]);

        // The pointer never hides on inactivity.
        t.advance(Duration::from_secs(60));
        assert!(t.log.take().is_empty());

        // The D-pad: the pointer hides at once and the arrow key goes to the app.
        t.input("ui_down", 1.0);
        t.input("ui_down", 0.0);
        assert_eq!(t.log.take(), [Call::Cursor(false), keys(&["down"])]);
        t.input("ui_accept", 1.0);
        t.input("ui_accept", 0.0);
        assert_eq!(t.log.take(), [keys(&["enter"])]);
        // The left stick (also arrow keys) keeps it hidden: no second hide.
        t.input("ui_left", 1.0);
        t.input("ui_left", 0.0);
        assert_eq!(t.log.take(), [keys(&["left"])]);
        // Moving the right stick shows it again, and it stays.
        t.input("ui_quick2", 1.0);
        assert_eq!(t.log.take(), [Call::Cursor(true)]);
        t.advance(Duration::from_secs(60));
        t.input("ui_quick2", 0.0);
        t.input("ui_quick2", 1.0);
        assert!(t.log.take().is_empty());
        // RT on a hidden pointer shows it and clicks.
        t.input("ui_down", 1.0);
        t.input("ui_down", 0.0);
        t.log.take();
        t.input("ui_r2", 1.0);
        t.input("ui_r2", 0.0);
        assert_eq!(
            t.log.take(),
            [Call::Cursor(true), Call::Click(true), Call::Click(false)]
        );

        // Shown, then Home: the profile unloaded, and the pointer hidden on the launcher.
        t.input("ui_quick2", 1.0);
        t.log.take();
        t.input("ui_guide", 1.0);
        let log = t.log.take();
        assert!(log.contains(&Call::Pointer(false)), "{log:?}");
        assert!(log.contains(&Call::Cursor(false)), "{log:?}");
        assert_eq!(log.last(), Some(&Call::Mode(InterceptMode::All)));
        // Back in the app: shown again. Hidden by the D-pad, then Home: it stays hidden.
        t.launch(json!({"id": "youtube"}));
        assert!(t.log.take().contains(&Call::Cursor(true)));
        t.input("ui_left", 1.0);
        t.input("ui_left", 0.0);
        assert_eq!(t.log.take(), [Call::Cursor(false), keys(&["left"])]);
        t.input("ui_guide", 1.0);
        assert!(!t.log.take().iter().any(|c| matches!(c, Call::Cursor(_))));

        // Games: no pointer, no profile, the pad as it is; gamescope's pointer back (a mouse).
        t.launch(json!({"id": "portal"}));
        let log = t.log.take();
        assert!(!log.iter().any(|c| matches!(c, Call::Pointer(_))));
        assert!(log.contains(&Call::Cursor(true)), "{log:?}");
        assert_eq!(log.last(), Some(&Call::Mode(InterceptMode::Pass)));
        // Pointer off in the config: All, no profile.
        let mut t = T::with(|c| c.pointer_enabled = false);
        t.launch(json!({"id": "youtube"}));
        assert_eq!(t.log.take().last(), Some(&Call::Mode(InterceptMode::All)));
        t.input("ui_quick2", 1.0);
        t.input("ui_accept", 1.0);
        assert_eq!(t.log.take(), [keys(&["enter"])]);
    }

    #[test]
    fn shader_precaching_does_not_end_the_launch() {
        let mut t = T::new();
        t.launch(json!({"id": "portal"}));
        // [game, 769, launcher]: Steam's pre-launch window shows while the game has none, and
        // the pad (PASS) reaches it so it can be skipped.
        assert_eq!(
            t.log.take()[1..],
            [Call::FocusSteamGame(400), Call::Mode(InterceptMode::Pass)]
        );
        t.events();
        steam_windows(&mut t, &[(50, "Processing Vulkan shaders")]);
        t.gamescope(Some(769), &[769, L]);
        assert!(t.events().is_empty(), "no attention: it shows already");
        // Longer than the launch timeout (60 s here): Steam visibly works on it.
        t.advance(Duration::from_secs(100));
        assert_eq!(t.shell.focused(), Some("portal"));
        // The dialog is done, the game's reaper runs but the game has no window yet.
        t.procs.lock().unwrap().steam_reapers.insert(400, vec![900]);
        steam_windows(&mut t, &[]);
        t.gamescope(Some(L), &[L]);
        t.advance(Duration::from_secs(200));
        assert_eq!(t.shell.focused(), Some("portal"));
        assert!(t.events().is_empty(), "never exited");
        // The game's window: shown (gamescope follows the order written at launch).
        t.gamescope(Some(400), &[400, L]);
        // A loading gap between two windows of the game, reaper alive: still the game.
        t.gamescope(Some(L), &[L]);
        t.advance(Duration::from_secs(5));
        assert_eq!(t.shell.focused(), Some("portal"));
        assert!(t.events().is_empty());
        // The game quits for real: its reaper ends, the launcher is back.
        t.procs.lock().unwrap().steam_reapers.clear();
        t.advance(Duration::from_secs(5));
        assert_eq!(t.shell.focused(), None);
        assert!(t.events().contains(&ev(
            EventName::AppExited,
            json!({"id": "portal", "reason": "exited"})
        )));

        // Without a reaper or Steam windows a launch still times out.
        let mut t = T::new();
        t.launch(json!({"id": "portal"}));
        t.events();
        t.advance(Duration::from_secs(61));
        assert!(t.events().contains(&ev(
            EventName::AppExited,
            json!({"id": "portal", "reason": "launch_timeout"})
        )));
    }

    #[test]
    fn a_running_steam_game_is_focused_not_launched_again() {
        let mut t = T::new();
        // Running although we do not track it (lost track, or it ran before the daemon).
        t.procs.lock().unwrap().steam_reapers.insert(400, vec![900]);
        let app = t.launch(json!({"id": "portal"}));
        assert_eq!(app.id, "portal");
        assert_eq!(
            t.log.take(),
            [Call::FocusSteamGame(400), Call::Mode(InterceptMode::Pass)],
            "no steam://rungameid again"
        );
        // By URL too.
        let app = t.launch(json!({"url": "steam://rungameid/400"}));
        assert_eq!(app.id, "portal");
        assert!(!t.log.take().iter().any(|c| matches!(c, Call::Spawn { .. })));

        // Daemon restart: running Steam games are tracked again (our own command apps, with
        // ids above the launcher's, are not Steam games).
        let mut t = T::new();
        {
            let mut table = t.procs.lock().unwrap();
            table.steam_reapers.insert(620, vec![901]);
            table.steam_reapers.insert(L + 7, vec![902]);
        }
        t.shell.start();
        let running = t.call("apps.running", json!({})).unwrap();
        let ids: Vec<&str> = running["apps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["steam-620"]);
        assert_eq!(t.shell.focused(), None, "adopting does not take focus");
        t.log.take();
        t.launch(json!({"url": "steam://rungameid/620"}));
        assert!(!t.log.take().iter().any(|c| matches!(c, Call::Spawn { .. })));
    }

    #[test]
    fn steams_answer_to_our_request_gets_focus() {
        let mut t = T::new();
        t.launch(json!({"id": "portal"}));
        // Home right away; Steam then answers the launch with an error dialog over the
        // launcher: Steam gets focus and the pad.
        t.input("ui_guide", 1.0);
        t.input("ui_guide", 0.0);
        t.call("apps.close", json!({"id": "portal"})).unwrap();
        t.events();
        t.log.take();
        t.advance(Duration::from_secs(2));
        steam_windows(&mut t, &[(60, "Game already running")]);
        assert_eq!(t.shell.focused(), Some("steam"));
        assert_eq!(
            t.log.take(),
            [Call::FocusApp(769), Call::Mode(InterceptMode::Pass)]
        );
        assert!(
            !t.events()
                .iter()
                .any(|(n, p)| *n == EventName::SteamAttention && p["windows"] != json!([])),
            "no banner: it has focus"
        );
        // A dismisses it (in Steam); its windows go: back to the launcher.
        t.gamescope(Some(769), &[769, L]);
        steam_windows(&mut t, &[]);
        t.gamescope(Some(L), &[L]);
        t.advance(Duration::from_secs(2));
        assert_eq!(t.shell.focused(), None);

        // A Steam pop-up long after any request of ours: only the banner.
        t.advance(STEAM_ANSWER_WINDOW);
        t.events();
        steam_windows(&mut t, &[(61, "Friends")]);
        assert_eq!(t.shell.focused(), None);
        assert!(
            t.events()
                .iter()
                .any(|(n, p)| *n == EventName::SteamAttention && p["windows"][0]["window"] == 61)
        );
    }
}
