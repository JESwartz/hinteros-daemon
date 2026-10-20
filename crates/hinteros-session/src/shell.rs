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
//! - Home (Guide, Ctrl+Super+D on a keyboard, or SIGUSR1): `home_pressed`, then the launcher
//!   gets focus. Apps keep running. The keyboard chord (the system service's `keyboard_home`,
//!   [`crate::system`]) and SIGUSR1 act at once (no long press).
//!   Guide held for [`GUIDE_LONG_PRESS`] while Steam or a Steam game is in front goes to Steam
//!   instead (one Guide tap on the virtual pad: its in-game or Big Picture menu), no Home.
//! - If the focused app's windows go away for longer than the debounce (it closed its window,
//!   or a Steam game quit), the launcher gets focus.
//! - Steam itself is the app `steam` (gamescope app id 769): opened as Big Picture through
//!   `apps.launch`, or brought forward with `steam.focus` when one of its windows wants
//!   attention (`steam_attention`); when its windows close, focus goes back to whoever had it.
//! - A Steam game: `[game, 769, launcher]` (Steam's pre-launch and error windows show until the
//!   game has one), PASS. It runs while Steam's reaper for it is alive, whatever its windows
//!   do; launching it again then only focuses it. A Steam window over the launcher right after
//!   our own Steam request gets focus (`STEAM_ANSWER_WINDOW`). A game whose window carries
//!   another app id (the one its Steam API reports, e.g. 480) has that id as an alias:
//!   `[game, alias, 769, launcher]`, written as soon as that window appears; it ends once its
//!   reaper and all its windows are gone. With Big Picture open, Steam writes focus orders of
//!   its own; one that hides the focused game gets ours again (rate-limited, never while
//!   Steam shows its menu after our Guide tap; see `Shell::check_order`).
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
    AppsCloseResult, AppsDisplaysGet, AppsDisplaysSet, AppsFocus, AppsFocusParams, AppsFocusResult,
    AppsLaunch, AppsLaunchParams, AppsLaunchResult, AppsList, AppsListResult, AppsRunning,
    AppsRunningResult, AppsSetPermission, AppsSetPermissionParams, AppsSetPermissionResult,
    EventName, ExitReason, FocusChangedParams, GamesList, GamesListResult, HomePressedParams,
    InputNavParams, KEYS_TYPE_MAX_CHARS, KeysAction, KeysActionParams, KeysActionResult, KeysKey,
    KeysKeyParams, KeysKeyResult, KeysType, KeysTypeParams, KeysTypeResult, LaunchRequest,
    NavAction, NavPhase, OskHide, OskShow, OskState, RunningApp, STEAM_CLIENT_APP_ID,
    STEAM_CLIENT_GAMESCOPE_APP_ID, STEAM_RUNGAMEID, SteamAttentionParams, SteamAttentionQuery,
    SteamFocus, SteamFocusResult, SteamWindow,
};
use hinteros_api::{ErrorCode, ErrorObject, Method};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::apps::{ChildExit, Spawner};
use crate::config::{AppConfig, Config, FieldSummon};
use crate::displays::DisplayPool;
use crate::focus::{
    FocusControl, GamescopeEvent, GamescopeState, baselayer_for, baselayer_for_steam_client,
    baselayer_for_steam_game,
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
    /// Home at once: SIGUSR1, or Ctrl+Super+D on a keyboard (the system's `keyboard_home`). Guide
    /// goes through `Input` (long press and the on-screen keyboard chord).
    Home,
    Input(RawInput),
    Gamescope(GamescopeEvent),
    ChildExit(ChildExit),
    /// The config was reloaded: this is the new app list (running apps are left alone).
    SetApps(Vec<AppConfig>),
    /// The usable Steam binary changed (`None`: Steam is not installed).
    SetSteam(Option<PathBuf>),
    /// Apps whose vendor terms this profile has not accepted (from `system.apps.list`).
    SetNeedsTerms(std::collections::BTreeMap<String, hinteros_api::session::StoreTerms>),
    /// A profile switch: SIGTERM to every app, never followed by a SIGKILL on its own.
    CloseAll {
        reply: oneshot::Sender<()>,
    },
    /// The apps still running (a profile switch waits for them).
    RunningApps {
        reply: oneshot::Sender<Vec<hinteros_api::session::AppRef>>,
    },
    /// "Force close" in the not-responding pop-up of a switch.
    ForceClose {
        id: String,
    },
    /// A text field in a web app gained or lost focus (see [`crate::textfocus`]); `pid` is the
    /// reporting process (Firefox's native-messaging host, or Firefox itself for its own
    /// toolbar fields), `reporter` the connection it came on (one Firefox has two: page fields
    /// through the extension, the address bar and other browser fields directly). `clicked`:
    /// the user just clicked into the field with the pointer (it may have had focus already).
    TextFocus {
        pid: Option<u32>,
        reporter: u64,
        focus: FieldFocus,
        clicked: bool,
    },
}

/// The methods the shell answers (the connection handles the rest).
pub const METHODS: &[&str] = &[
    AppsList::NAME,
    AppsSetPermission::NAME,
    AppsDisplaysGet::NAME,
    AppsDisplaysSet::NAME,
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

/// How many gamescope app ids above the launcher's are our command apps' (see
/// `Shell::own_app_id`): far more apps than ever run at once.
const OWN_APP_IDS: u32 = 4096;

/// Steam (with Big Picture open) writes its own focus orders, and may put its own window in
/// front of a Steam game we show. We write ours again at most this often...
const REASSERT_GAP: Duration = Duration::from_millis(500);
/// ...and at most [`REASSERT_MAX`] times in this long; after that Steam keeps its order (until
/// the focus changes, or Steam shows the game by itself).
const REASSERT_WINDOW: Duration = Duration::from_secs(30);
const REASSERT_MAX: usize = 6;

/// State for writing our focus order again when Steam changed it (see `Shell::check_order`).
#[derive(Debug, Default)]
struct OrderGuard {
    /// When we wrote our order again (within [`REASSERT_WINDOW`]).
    times: std::collections::VecDeque<Instant>,
    /// Look again at this time (a write held back by [`REASSERT_GAP`]).
    at: Option<Instant>,
    /// Gave up: Steam kept changing it.
    gave_up: bool,
    /// We sent Steam a Guide tap (its menu): Steam's order goes; the order at that time, so
    /// that Steam showing the game again (another order) ends it.
    steam_menu: Option<Vec<u32>>,
}

/// The app that a focus order shows: the first one with a window (the launcher always has
/// one).
fn shown_by(order: &[u32], focusable: &[u32], launcher: u32) -> Option<u32> {
    order
        .iter()
        .copied()
        .find(|id| *id == launcher || focusable.contains(id))
}

/// What to tell gamescope (see [`FocusControl`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum FocusPlan {
    Launcher,
    App(u32),
    SteamGame(u32),
    /// Steam (or a Steam game) with these games' windows in front of Steam's.
    SteamClient(Vec<u32>),
}

/// A Guide press older than this no longer starts a chord (its release may have been lost).
const GUIDE_HOLD_MAX: Duration = Duration::from_secs(10);

/// Guide held this long while Steam (or a Steam game) is in front is a long press: Steam gets
/// a Guide tap (its in-game menu, or Big Picture's main menu) instead of Home. Half a second
/// is well above a deliberate tap (100-250 ms) and the usual long-press threshold (Android,
/// the Steam Deck's own buttons), yet short enough that the menu feels like it answers the
/// hold.
pub const GUIDE_LONG_PRESS: Duration = Duration::from_millis(500);

/// Guide events this soon after our Guide tap to Steam are ignored: should InputPlumber ever
/// hand the injected tap back to us (it must not, see [`InputControl::guide_tap`]), it would
/// otherwise be a short press, i.e. Home.
const GUIDE_TAP_ECHO: Duration = Duration::from_millis(750);

/// Guide is held.
#[derive(Debug, Clone)]
struct GuideHold {
    since: Instant,
    /// It was used for the on-screen keyboard chord (its release is not Home).
    chord: bool,
    /// The pad it came from (a D-Bus target), for the Guide tap to Steam.
    source: String,
    /// When the hold becomes a long press for Steam (`None`: it cannot, or no longer can:
    /// Steam was not in front at the press, or another button was pressed meanwhile).
    steam_at: Option<Instant>,
}

/// How long after a text field loses focus an automatically shown keyboard hides: moving to
/// the next field (or into a frame) reports `none` and then `text` again.
pub const OSK_AUTO_HIDE_DELAY: Duration = Duration::from_millis(250);

/// A text field that gets focus this soon after our left click (RT) in its app was clicked
/// into: the keyboard shows (focus that comes by itself, from arrow keys or a page focusing a
/// field on load, does not show it). Reporters also say `clicked` themselves (see
/// [`crate::textfocus`]); this covers clicks that focus a field without landing on it (a
/// label, the padding around the address bar). Focus reports take a few milliseconds.
pub const OSK_CLICK_WINDOW: Duration = Duration::from_millis(600);

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
    /// Profile mode (`[apps] profile`): no stand-in apps when the profile has none.
    pub profile_mode: bool,
    /// Apps whose vendor terms this profile has not accepted.
    pub needs_terms: std::collections::BTreeMap<String, hinteros_api::session::StoreTerms>,
    /// The profile's app permissions (shared with the Store).
    pub permissions: crate::permissions::Permissions,
    /// The catalog index (`apps.set_permission` for apps not installed yet).
    pub catalog_index: PathBuf,
    /// `[sandbox] enable`: catalog apps run in bwrap ([`crate::sandbox`]).
    pub sandbox: bool,
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
            profile_mode: c.icons.profile_share.is_some(),
            needs_terms: Default::default(),
            permissions: crate::permissions::Permissions::from_config(&c.sandbox),
            catalog_index: c.store.index.clone(),
            sandbox: c.sandbox.enable,
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
    /// gamescope listed it (or one of its `aliases`) in GAMESCOPE_FOCUSABLE_APPS (it has an
    /// Xwayland window).
    seen_focusable: bool,
    /// gamescope reported it (or one of its `aliases`) as GAMESCOPE_FOCUSED_APP.
    seen_focused: bool,
    /// A Steam game: Steam's reaper for it was alive at some point (so once it is gone, the
    /// game has ended).
    reaper_seen: bool,
    /// A Steam game: other app ids its windows carry (the one its Steam API reports, e.g.
    /// 480 for a shortcut with Spacewar's steam_appid.txt; gamescope: "appid clash"), in the
    /// order they were found.
    aliases: Vec<u32>,
    /// Tracked by its windows: since when they are gone (exit after the debounce).
    gone_since: Option<Instant>,
    closing: Option<Closing>,
    /// A browser with an address bar: View is Ctrl+L and the on-screen keyboard.
    address_bar: bool,
    /// The right stick is a pointer (keyboard profile).
    pointer: bool,
    /// The keyboard input profile's keymap (`None`: gamepad).
    keymap: Option<Keymap>,
    /// The app display it was started on (resolved), if it has one of its own.
    display: Option<String>,
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
    /// Writing our focus order again after Steam changed it (see [`Shell::check_order`]).
    order: OrderGuard,
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
    /// Games Steam started from its library while it was in front: Steam gets focus back
    /// when they end.
    from_steam: BTreeSet<String>,
    /// Steam games we track although we did not start them (found by their Steam reapers).
    adopted: BTreeSet<String>,
    /// When we last asked Steam to do something (launch or close a game, open or close Big
    /// Picture): Steam windows that appear soon after are its answer and get focus.
    steam_action_at: Option<Instant>,
    osk_visible: bool,
    /// The keyboard is up because a text field got focus (not because someone asked).
    osk_auto: bool,
    /// The text field lost focus: hide the automatically shown keyboard at this time.
    osk_auto_hide_at: Option<Instant>,
    /// Whether the keyboard is up for a password field (the last `osk_visibility.password`).
    osk_password: bool,
    /// Guide is held (see [`GuideHold`]).
    guide: Option<GuideHold>,
    /// When we last sent Steam a Guide tap (a long press): Guide events right after it are
    /// not ours ([`GUIDE_TAP_ECHO`]).
    guide_tap_at: Option<Instant>,
    /// Navigation buttons currently held (as far as we can see them).
    held: BTreeSet<NavAction>,
    /// Releases to drop (buttons used in a chord).
    swallow: BTreeSet<NavAction>,
    /// The raw `ui_*` name each held button last arrived as (for the chord log).
    raw_names: BTreeMap<NavAction, String>,
    /// Buttons pressed while Guide was held that made no chord (raw `ui_*` names, for the log).
    guide_others: Vec<String>,
    /// The text fields with focus in each keyboard-profile app, per reporter (the Firefox
    /// extension for the page, Firefox's chrome code for the address bar). No entry: none.
    text_fields: BTreeMap<String, BTreeMap<u64, FieldFocus>>,
    /// Stick chatter filter for directions.
    debounce: Debounce,
    /// The right-stick pointer: in use or not, hidden by us or not.
    pointer: PointerState,
    /// The pointer profile is loaded in InputPlumber (as far as we told it).
    pointer_profile: bool,
    /// Shows and hides gamescope's pointer.
    cursor: Box<dyn CursorControl>,
    /// Buttons held down as mouse buttons: the pad they came from and the mouse button.
    clicking: BTreeMap<NavAction, (String, MouseButton)>,
    /// Our last left click (RT) in a keyboard-profile app: the app and when.
    left_click: Option<(String, Instant)>,
    /// The app displays (one per app we start) and their setting.
    displays: DisplayPool,
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
            left_click: None,
            displays: DisplayPool::new(None, 4).with_files(None, None),
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
            order: OrderGuard::default(),
            written: std::collections::VecDeque::new(),
            next_command: 1,
            steam_windows: Vec::new(),
            seen_windows: BTreeSet::new(),
            attention: Vec::new(),
            steam_return: None,
            from_steam: BTreeSet::new(),
            adopted: BTreeSet::new(),
            steam_action_at: None,
            osk_visible: false,
            osk_auto: false,
            osk_auto_hide_at: None,
            osk_password: false,
            guide: None,
            guide_tap_at: None,
            held: BTreeSet::new(),
            swallow: BTreeSet::new(),
            raw_names: BTreeMap::new(),
            guide_others: Vec::new(),
        }
    }

    /// Give each app we start a display of its own from `pool` (default: none, apps share the
    /// daemon's display).
    pub fn with_displays(mut self, pool: DisplayPool) -> Self {
        self.displays = pool;
        self
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
            Msg::SetNeedsTerms(map) => self.cfg.needs_terms = map,
            Msg::CloseAll { reply } => {
                let ids: Vec<String> = self.apps.keys().cloned().collect();
                for id in ids {
                    let _ = self.close(AppsCloseParams { id: id.clone() }, now);
                    // No silent force-close: the user decides (switch_blocked).
                    if let Some(app) = self.apps.get_mut(&id)
                        && let Some(c) = app.closing.as_mut()
                    {
                        c.deadline = now + Duration::from_secs(365 * 24 * 3600);
                    }
                }
                let _ = reply.send(());
            }
            Msg::RunningApps { reply } => {
                let _ = reply.send(
                    self.apps
                        .values()
                        .map(|a| hinteros_api::session::AppRef {
                            id: a.info.id.clone(),
                            name: a.info.name.clone(),
                        })
                        .collect(),
                );
            }
            Msg::ForceClose { id } => self.force_close(&id, now),
            Msg::TextFocus {
                pid,
                reporter,
                focus,
                clicked,
            } => self.text_focus(pid, reporter, focus, clicked, now),
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
            AppsSetPermission::NAME => {
                let p = decode_params::<AppsSetPermission>(params)?;
                encode_result::<AppsSetPermission>(self.set_permission(p)?)
            }
            AppsDisplaysGet::NAME => {
                decode_params::<AppsDisplaysGet>(params)?;
                encode_result::<AppsDisplaysGet>(self.displays.info())
            }
            AppsDisplaysSet::NAME => {
                let p = decode_params::<AppsDisplaysSet>(params)?;
                self.displays.set(p.count).map_err(|e| {
                    let code = if e.kind() == std::io::ErrorKind::InvalidInput {
                        ErrorCode::InvalidParams
                    } else {
                        ErrorCode::InternalError
                    };
                    ErrorObject::new(code, e.to_string())
                })?;
                info!(count = p.count, "app displays set (next session)");
                encode_result::<AppsDisplaysSet>(self.displays.info())
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
                    password: self.osk_password,
                })
            }
            OskHide::NAME => {
                decode_params::<OskHide>(params)?;
                self.hide_osk();
                encode_result::<OskHide>(OskState {
                    visible: self.osk_visible,
                    password: self.osk_password,
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
            needs_terms: self.cfg.needs_terms.get(STEAM_CLIENT_APP_ID).cloned(),
            permissions: Vec::new(),
        });
        if self.cfg.apps.is_empty() && !self.cfg.profile_mode {
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
                    needs_terms: self.cfg.needs_terms.get(&a.id).cloned(),
                    permissions: self.app_permissions(a),
                }))
                .collect(),
        }
    }

    /// A configured app's permissions (none for Steam games and unsandboxed apps).
    fn app_permissions(&self, a: &AppConfig) -> Vec<hinteros_api::session::AppPermission> {
        if a.steam_app_id.is_some() || !a.sandbox {
            return Vec::new();
        }
        self.cfg.permissions.list(&a.id, &a.permissions)
    }

    /// `apps.set_permission`: an installed app, else a catalog app (not installed yet).
    fn set_permission(
        &mut self,
        p: AppsSetPermissionParams,
    ) -> Result<AppsSetPermissionResult, ErrorObject> {
        let none = BTreeMap::new();
        let installed = self.cfg.apps.iter().find(|a| a.id == p.id).map(|a| {
            if a.steam_app_id.is_none() && a.sandbox {
                a.permissions.clone()
            } else {
                none.clone()
            }
        });
        let declared = installed.or_else(|| {
            let index = hinteros_api::catalog::Index::load(&self.cfg.catalog_index).ok()?;
            let e = index.app(&p.id)?;
            Some(if e.sandbox && e.id != STEAM_CLIENT_APP_ID {
                e.permissions.clone()
            } else {
                none.clone()
            })
        });
        let changed =
            self.cfg
                .permissions
                .set(&p.id, declared.as_ref(), &p.permission, p.granted)?;
        if changed {
            info!(app = %p.id, permission = %p.permission, granted = p.granted, "permission set");
        }
        self.emit(EventName::AppsChanged, serde_json::json!({}));
        self.emit(EventName::StoreChanged, serde_json::json!({}));
        Ok(AppsSetPermissionResult {})
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
        if let LaunchRequest::Id(id) = &request
            && let Some(t) = self.cfg.needs_terms.get(id)
            && !self.apps.contains_key(id)
        {
            return Err(ErrorObject::with_data(
                ErrorCode::TermsRequired,
                format!("{id} needs its terms accepted (version {})", t.version),
                serde_json::json!({ "id": id, "version": t.version }),
            ));
        }
        let configured = match &request {
            LaunchRequest::Id(id) => self.cfg.apps.iter().find(|a| &a.id == id),
            _ => None,
        };
        // Catalog apps run sandboxed (not Steam games, not `sandbox = false` apps).
        let sandbox = configured
            .filter(|a| self.cfg.sandbox && a.sandbox && a.command.is_some())
            .map(|a| crate::sandbox::Spec {
                id: a.id.clone(),
                grants: self.cfg.permissions.grants(&a.id, &a.permissions),
                kiosk: a.kind == AppKind::Web || a.input == crate::config::InputProfile::Keyboard,
            });
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
                let name = self.steam_game_name(id);
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
        let mut app_display = None;
        let info = match target {
            Target::Command(argv) => {
                app_display = self.free_display()?;
                let gamescope_app_id = self.allocate_app_id();
                let pid = self
                    .spawner
                    .spawn(
                        &key,
                        &argv,
                        Some(gamescope_app_id),
                        sandbox.as_ref(),
                        app_display.as_deref(),
                    )
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
                    // A non-Steam shortcut launches by its 64-bit game id.
                    format!("{STEAM_RUNGAMEID}{}", library::steam_game_id(steam)),
                ];
                // Not under our reaper: the game runs under Steam's own reaper, which tags it
                // with its Steam app id, and an outer reaper would override that.
                self.spawner
                    .spawn(&key, &argv, None, None, None)
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
                    .spawn(&key, &[steam_path.clone(), arg.into()], None, None, None)
                    .map_err(|e| failed(e, &steam_path))?;
                // Opened from the launcher: closing it goes back there.
                self.steam_return = None;
                self.steam_action_at = Some(now);
                steam_client_app()
            }
        };
        info!(app = %info.id, gamescope_app_id = info.gamescope_app_id, on = app_display.as_deref().unwrap_or("-"), "launched");
        self.track(info.clone(), input, now);
        if let Some(app) = self.apps.get_mut(&key) {
            app.display = app_display;
        }
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
                reaper_seen: false,
                aliases: Vec::new(),
                gone_since: None,
                closing: None,
                keymap: input.keymap,
                address_bar: input.address_bar,
                pointer: input.pointer,
                display: None,
            },
        );
        self.emit(EventName::AppStarted, AppStartedParams { app: info });
    }

    /// A free app display for an app about to start (`None`: apps share the daemon's display):
    /// the first one that exists, that no running app of ours has, and on which no process we
    /// started is left (an app we gave up on after SIGKILL, or one from before a daemon
    /// restart, holds its display until it is gone). None free: `no_app_display` with the apps
    /// that hold one, for the launcher to offer closing one.
    fn free_display(&mut self) -> Result<Option<String>, ErrorObject> {
        if !self.displays.enabled() {
            return Ok(None);
        }
        let displays = self.displays.displays();
        if displays.is_empty() {
            return Err(ErrorObject::new(
                ErrorCode::AppLaunchFailed,
                format!(
                    "cannot start it: no app display ({})",
                    self.displays.first().unwrap_or_default()
                ),
            ));
        }
        for d in &displays {
            let held = self.apps.values().any(|r| r.display.as_ref() == Some(d));
            if held {
                continue;
            }
            let left = self.procs.display_users(d);
            if left.is_empty() {
                return Ok(Some(d.clone()));
            }
            info!(display = %d, processes = ?left, "app display still in use by an app we no longer track");
        }
        let holders: Vec<RunningApp> = self
            .apps
            .values()
            .filter(|r| r.display.is_some())
            .map(|r| r.info.clone())
            .collect();
        Err(ErrorObject::with_data(
            ErrorCode::NoAppDisplay,
            format!("every app display is in use ({})", displays.len()),
            serde_json::json!({ "apps": holders }),
        ))
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
                        let argv = [steam_path, "-shutdown".into()];
                        if let Err(e) = self.spawner.spawn("steam-close", &argv, None, None, None) {
                            warn!("cannot ask Steam to shut down: {e}");
                        }
                    }
                    Err(e) => warn!("cannot ask Steam to shut down: {}", e.message),
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
    /// The games Steam (Big Picture) runs that it started itself, from its library: no app of
    /// ours. Each has a Steam reaper with the game's (or shortcut's) app id, and its window
    /// may carry yet another id (the one its Steam API reports, e.g. 480 for Spacewar), so the
    /// focusable windows that are nobody else's count too while such a reaper is alive.
    fn steam_client_games(&mut self) -> Vec<u32> {
        if !self.apps.contains_key(STEAM_CLIENT_APP_ID) {
            return Vec::new();
        }
        let mut games = Vec::new();
        for id in self.steam_reaper_games() {
            games.push(id);
            // The windows of a game we track carry its id or one of its aliases.
            if let Some(app) = self
                .apps
                .values()
                .find(|a| a.info.source == AppSource::Steam && a.info.gamescope_app_id == id)
            {
                for alias in &app.aliases {
                    if !games.contains(alias) {
                        games.push(*alias);
                    }
                }
            }
        }
        if !games.is_empty() {
            for id in self.unclaimed_windows() {
                if !games.contains(&id) {
                    games.push(id);
                }
            }
        }
        games
    }

    /// Whether `id` is one we give our command apps ([`Self::allocate_app_id`]: the lowest free
    /// ids just above the launcher's). Their reapers look like Steam's, so a live one that is
    /// not in our list (left by the daemon before a restart; in tests, another daemon's) would
    /// pass for a game Steam runs. Non-Steam shortcuts' ids are spread over 2^31..2^32, so one
    /// landing in this window is very unlikely.
    fn own_app_id(&self, id: u32) -> bool {
        id.checked_sub(self.cfg.launcher_app_id)
            .is_some_and(|d| (1..=OWN_APP_IDS).contains(&d))
    }

    /// The app ids of the live Steam reapers that are Steam games (not our own command apps,
    /// [`Self::own_app_id`]).
    fn steam_reaper_games(&mut self) -> Vec<u32> {
        let launcher = self.cfg.launcher_app_id;
        let commands: Vec<u32> = self
            .apps
            .values()
            .filter(|a| !tracked_by_windows(a.info.source))
            .map(|a| a.info.gamescope_app_id)
            .collect();
        self.procs
            .steam_games()
            .into_iter()
            .filter(|id| {
                *id != 0 && *id != launcher && !commands.contains(id) && !self.own_app_id(*id)
            })
            .collect()
    }

    /// Focusable windows that belong to none of our apps, Steam or the launcher: a Steam game's
    /// window under another id than its reaper's (the id its Steam API reports).
    fn unclaimed_windows(&self) -> Vec<u32> {
        let Some(state) = &self.gamescope else {
            return Vec::new();
        };
        let launcher = self.cfg.launcher_app_id;
        state
            .focusable_apps
            .iter()
            .copied()
            .filter(|id| {
                *id != 0
                    && *id != launcher
                    && *id != STEAM_CLIENT_GAMESCOPE_APP_ID
                    && !self.own_app_id(*id)
                    && !self
                        .apps
                        .values()
                        .any(|a| a.info.gamescope_app_id == *id || a.aliases.contains(id))
            })
            .collect()
    }

    /// A Steam game we track whose window carries another app id than its reaper's: those
    /// ids (they go in front of Steam's window). Its aliases once known; until then, while its
    /// reaper is alive, the windows that are nobody else's.
    fn steam_game_windows(&mut self, gs_id: u32) -> Vec<u32> {
        let Some(app) = self
            .apps
            .values()
            .find(|a| a.info.source == AppSource::Steam && a.info.gamescope_app_id == gs_id)
        else {
            return Vec::new();
        };
        if !app.aliases.is_empty() {
            return app.aliases.clone();
        }
        if self.procs.steam_reapers(gs_id).is_empty() {
            return Vec::new();
        }
        self.unclaimed_windows()
    }

    /// Follow the Steam games we track (on each gamescope state): note that Steam's reaper for
    /// each is alive, and claim the windows that are nobody else's for the game while it is
    /// the only Steam game running (its aliases).
    fn observe_steam_games(&mut self) {
        if self.gamescope.is_none()
            || !self
                .apps
                .values()
                .any(|a| a.info.source == AppSource::Steam && a.closing.is_none())
        {
            return;
        }
        let live = self.steam_reaper_games();
        for app in self.apps.values_mut() {
            if app.info.source == AppSource::Steam
                && !app.reaper_seen
                && live.contains(&app.info.gamescope_app_id)
            {
                info!(app = %app.info.id, "Steam's reaper for it runs");
                app.reaper_seen = true;
            }
        }
        // Its reaper came and went, and it has no window (under its id or an alias): it ended.
        let state = self.gamescope.as_ref().expect("checked above");
        let ended: Vec<String> = self
            .apps
            .values()
            .filter(|a| {
                a.info.source == AppSource::Steam
                    && a.closing.is_none()
                    && a.reaper_seen
                    && !live.contains(&a.info.gamescope_app_id)
                    && !std::iter::once(a.info.gamescope_app_id)
                        .chain(a.aliases.iter().copied())
                        .any(|id| {
                            state.focusable_apps.contains(&id) || state.focused_app == Some(id)
                        })
            })
            .map(|a| a.info.id.clone())
            .collect();
        for id in ended {
            info!(app = %id, "its Steam reaper and its windows are gone");
            self.exited(&id, ExitReason::Exited, None);
        }
        let [only] = live[..] else { return };
        let unclaimed = self.unclaimed_windows();
        if unclaimed.is_empty() {
            return;
        }
        let Some(app) = self.apps.values_mut().find(|a| {
            a.info.source == AppSource::Steam
                && a.closing.is_none()
                && a.info.gamescope_app_id == only
        }) else {
            return;
        };
        for alias in unclaimed {
            info!(
                app = %app.info.id,
                alias,
                "its window has another app id (its Steam API's): that id is the game's too"
            );
            app.aliases.push(alias);
        }
    }

    fn steam_game_still_running(&mut self, id: &str, reason: ExitReason) -> bool {
        let Some(app) = self.apps.get(id) else {
            return false;
        };
        if app.closing.is_some() {
            return false;
        }
        if app.info.source == AppSource::SteamClient {
            // A game Steam started from its library has the screen (its window has its own
            // app id), so Steam's window went away: Steam is still the app in front.
            let playing = !self.steam_client_games().is_empty();
            if playing {
                debug!(?reason, "no Steam window, but Steam runs a game of its own");
            }
            return playing;
        }
        if app.info.source != AppSource::Steam {
            return false;
        }
        // Once its reaper has come and gone, the game has ended (Steam windows are not its).
        let (gs_id, launching) = (
            app.info.gamescope_app_id,
            !app.seen_focusable && !app.seen_focused && !app.reaper_seen,
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
        info!(app = id, ?reason, "no window and no Steam reaper: ended");
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
            // Our own command apps run under reapers too, with ids just above the launcher's.
            if steam != self.cfg.launcher_app_id && !self.own_app_id(steam) {
                self.adopt_steam_game(steam, now);
            }
        }
    }

    /// Track the games Steam starts from its own library (Big Picture, a non-Steam shortcut),
    /// so they are in apps.running (Continue on Home) and can be focused and closed. Games
    /// started while Steam was in front take focus, and give it back to Steam when they end.
    fn adopt_steam_client_games(&mut self, now: Instant) {
        if self.cfg.steam_path.is_none()
            || !self.cfg.gamescope
            || !self.apps.contains_key(STEAM_CLIENT_APP_ID)
        {
            return;
        }
        for steam in self.steam_reaper_games() {
            let Some(id) = self.adopt_steam_game(steam, now) else {
                continue;
            };
            if self.focused.as_deref() == Some(STEAM_CLIENT_APP_ID) {
                self.from_steam.insert(id.clone());
                self.set_focus(Some(id));
            }
        }
        // An adopted game that never had a window of its own id is gone with its reaper.
        let gone: Vec<String> = self
            .adopted
            .iter()
            .filter(|id| {
                self.apps
                    .get(*id)
                    .is_some_and(|a| !a.seen_focusable && a.closing.is_none())
            })
            .cloned()
            .collect();
        for id in gone {
            let gs_id = self.apps[&id].info.gamescope_app_id;
            if self.procs.steam_reapers(gs_id).is_empty() {
                info!(app = %id, "its Steam reaper is gone");
                self.exited(&id, ExitReason::Exited, None);
            }
        }
    }

    /// A Steam game's name: its manifest's, else its non-Steam shortcut's, else its number.
    fn steam_game_name(&self, steam: u32) -> String {
        let dir = self.steam_dir();
        dir.as_deref()
            .and_then(|dir| library::find_manifest(dir, steam))
            .map(|m| m.name)
            .or_else(|| {
                dir.as_deref()
                    .and_then(|dir| library::shortcut_name(dir, steam))
            })
            .unwrap_or_else(|| format!("Steam game {steam}"))
    }

    /// Track a running Steam game we did not start; `None` if it is tracked already.
    fn adopt_steam_game(&mut self, steam: u32, now: Instant) -> Option<String> {
        if self.apps.values().any(|a| a.info.gamescope_app_id == steam) {
            return None;
        }
        let name = self.steam_game_name(steam);
        info!(steam_app_id = steam, %name, "found running; tracking it");
        let id = format!("steam-{steam}");
        let info = RunningApp {
            id: id.clone(),
            name,
            source: AppSource::Steam,
            gamescope_app_id: steam,
            pid: None,
        };
        self.track(info, AppInput::default(), now);
        self.adopted.insert(id.clone());
        Some(id)
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
        // Asked for anew: Steam's earlier orders (and its menu) no longer count.
        self.order = OrderGuard::default();
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
    /// What gamescope should show now, and the focus order that is.
    fn focus_plan(&mut self) -> (FocusPlan, Vec<u32>) {
        let target = self
            .focused
            .as_ref()
            .and_then(|id| self.apps.get(id))
            .map(|r| (r.info.gamescope_app_id, r.info.source == AppSource::Steam));
        let launcher = self.cfg.launcher_app_id;
        // Steam with games it started itself: their windows in front of Steam's. A Steam game
        // whose window has another id than its own: the game, then that window.
        let client_games = match target {
            _ if self.focused.as_deref() == Some(STEAM_CLIENT_APP_ID) => self.steam_client_games(),
            Some((id, true)) => {
                let windows = self.steam_game_windows(id);
                if windows.is_empty() {
                    windows
                } else {
                    [id].into_iter().chain(windows).collect()
                }
            }
            _ => Vec::new(),
        };
        match target {
            _ if !client_games.is_empty() => {
                let order = baselayer_for_steam_client(&client_games, launcher);
                (FocusPlan::SteamClient(client_games), order)
            }
            // Steam's own windows (shader pre-caching, launch errors) until the game has one.
            Some((id, true)) => (
                FocusPlan::SteamGame(id),
                baselayer_for_steam_game(id, launcher),
            ),
            Some((id, false)) => (FocusPlan::App(id), baselayer_for(Some(id), launcher)),
            None => (FocusPlan::Launcher, baselayer_for(None, launcher)),
        }
    }

    fn apply_focus(&mut self) {
        let (plan, order) = self.focus_plan();
        if self.written.len() == 4 {
            self.written.pop_front();
        }
        self.written.push_back(order);
        let result = match plan {
            FocusPlan::SteamClient(games) => self.focus.focus_steam_client(&games),
            FocusPlan::SteamGame(id) => self.focus.focus_steam_game(id),
            FocusPlan::App(id) => self.focus.focus_app(id),
            FocusPlan::Launcher => self.focus.focus_launcher(),
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
    /// unknown pid counts for the focused app), for A ([`Self::field_accept`]). When that app
    /// has focus, also shows the keyboard right away if the user clicked into the field with
    /// the pointer (`clicked`, or focus within [`OSK_CLICK_WINDOW`] of our left click there),
    /// or always with `auto_show`; a keyboard
    /// shown for a field (either way) hides again [`OSK_AUTO_HIDE_DELAY`] after the field
    /// loses focus.
    ///
    /// One app may have several reporters (Firefox's page and its address bar), each with its
    /// own state: the page reporting `none` because the address bar took focus (before or
    /// after the address bar reports `text`) leaves a field focused. The field counts as lost
    /// (and the keyboard hides) only once no reporter has one.
    fn text_focus(
        &mut self,
        pid: Option<u32>,
        reporter: u64,
        focus: FieldFocus,
        clicked: bool,
        now: Instant,
    ) {
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
        let fields = self.text_fields.entry(id.clone()).or_default();
        if focus.editable() {
            fields.insert(reporter, focus);
        } else {
            fields.remove(&reporter);
        }
        let any_field = !fields.is_empty();
        if !any_field {
            self.text_fields.remove(&id);
        }
        if self.focused.as_deref() != Some(id.as_str()) || !self.cfg.osk_enabled {
            debug!(app = %id, ?focus, "text field focus in the background");
            return;
        }
        if any_field {
            self.follow_field_kind();
        }
        if focus.editable() {
            self.osk_auto_hide_at = None;
            // Focus right after our left click in this app came from that click (used up).
            let after_click = self
                .left_click
                .take_if(|(app, at)| {
                    *app == id && now.saturating_duration_since(*at) <= OSK_CLICK_WINDOW
                })
                .is_some();
            let why = if clicked {
                "clicked"
            } else if after_click {
                "focused by a click"
            } else if self.cfg.osk_auto_show {
                "focused"
            } else {
                ""
            };
            if !why.is_empty() && !self.osk_visible {
                info!(app = %id, ?focus, "text field {why} → on-screen keyboard");
                self.set_osk(true, true);
                self.osk_auto = true;
            }
        } else if any_field {
            debug!(app = %id, reporter, "field lost focus here; another one still has it");
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
                .is_some_and(|id| self.text_fields.contains_key(id))
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
        self.osk_password = visible && self.password_field();
        self.emit(
            EventName::OskVisibility,
            OskState {
                visible,
                password: self.osk_password,
            },
        );
    }

    /// Whether the focused app reports a password field with focus.
    fn password_field(&self) -> bool {
        self.focused
            .as_ref()
            .and_then(|id| self.text_fields.get(id))
            .is_some_and(|f| f.values().any(|&f| f == FieldFocus::Password))
    }

    /// While the keyboard is up, a field of the other kind (text / password) got focus: tell
    /// the keyboard (`osk_visibility` again, still visible), so it masks or unmasks. Losing
    /// focus changes nothing (what was typed into a password field stays masked).
    fn follow_field_kind(&mut self) {
        let password = self.password_field();
        if self.osk_visible && password != self.osk_password {
            debug!(password, "on-screen keyboard: field kind changed");
            self.osk_password = password;
            self.emit(
                EventName::OskVisibility,
                OskState {
                    visible: true,
                    password,
                },
            );
        }
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
        self.close_steam_keyboard();
        self.set_focus(None);
    }

    /// Steam's on-screen keyboard is on Steam's overlay layer, which gamescope shows over every
    /// app, the launcher too. Home from Steam or a Steam game closes it (the URL SDL uses).
    fn close_steam_keyboard(&mut self) {
        let from_steam = self.focused_app().is_some_and(|app| {
            matches!(app.info.source, AppSource::Steam | AppSource::SteamClient)
        });
        if !from_steam || !self.cfg.gamescope || !self.procs.steam_running() {
            return;
        }
        let Some(steam) = self.cfg.steam_path.as_ref() else {
            return;
        };
        let argv = [
            steam.to_string_lossy().into_owned(),
            "steam://close/keyboard".into(),
        ];
        if let Err(e) = self
            .spawner
            .spawn("steam-keyboard", &argv, None, None, None)
        {
            warn!("closing the Steam keyboard: {e}");
        }
    }

    /// Whether a long Guide press goes to Steam now: Steam (Big Picture) or a Steam game (one
    /// we launched, or one Steam started itself) has focus with the pad (`Pass`), and no
    /// on-screen keyboard is up. Not when the launcher (or a non-Steam app) is in front with
    /// Steam in the background: Steam's menu would open behind the launcher, unseen, and from
    /// the launcher Steam is one press away anyway.
    fn steam_takes_guide(&self) -> bool {
        self.route() == Route::App
            && self.focused_app().is_some_and(|app| {
                app.info.id == STEAM_CLIENT_APP_ID
                    || matches!(app.info.source, AppSource::Steam | AppSource::SteamClient)
            })
    }

    /// Guide pressed or released.
    ///
    /// - A short press (released within [`GUIDE_LONG_PRESS`]) is Home.
    /// - Held past [`GUIDE_LONG_PRESS`] while Steam or a Steam game is in front
    ///   ([`Self::steam_takes_guide`]) and Steam runs, Steam gets one Guide tap through
    ///   InputPlumber ([`Self::long_guide`], from [`Shell::tick`]) and there is no Home.
    ///   Otherwise a long press is Home too.
    /// - With a summon chord configured, Guide + the summon button toggles the on-screen
    ///   keyboard (never Home, never Steam).
    ///
    /// Without a chord configured and nothing for Steam, Home happens at once on the press;
    /// otherwise on the release. In `Pass` mode InputPlumber only sends us Guide, and switches
    /// to `All` by itself on the press, so the buttons pressed *after* Guide reach us.
    ///
    /// Every step is logged at info level with the raw `ui_*` names, so that a pad whose
    /// buttons arrive under other names than expected shows in the journal.
    fn guide(&mut self, pressed: bool, source: &str, now: Instant) {
        if self
            .guide_tap_at
            .is_some_and(|at| now.duration_since(at) < GUIDE_TAP_ECHO)
        {
            info!(pressed, "guide right after our Guide tap to Steam: ignored");
            self.guide = None;
            return;
        }
        let summon = self.cfg.osk_summon.filter(|_| self.cfg.osk_enabled);
        if pressed {
            let steam = self.steam_takes_guide();
            if summon.is_none() && !steam {
                info!("guide pressed → Home (no chord configured, Steam not in front)");
                self.guide = None;
                self.home();
                return;
            }
            self.guide_others.clear();
            // The button already held (visible to us in `All` mode) also makes a chord.
            let chord = summon.is_some_and(|summon| self.held.contains(&summon));
            if let Some(summon) = summon.filter(|_| chord) {
                let raw = self.raw_names.get(&summon).cloned().unwrap_or_default();
                info!(
                    button = %raw,
                    "chord {}+guide → on-screen keyboard",
                    nav_name(summon)
                );
                self.toggle_osk();
            } else if let Some(summon) = summon {
                info!(
                    steam,
                    "guide pressed: chord started (guide+{} toggles the on-screen keyboard)",
                    nav_name(summon)
                );
            } else {
                info!("guide pressed with Steam in front: Home on release, Steam's Guide if held");
            }
            self.guide = Some(GuideHold {
                since: now,
                chord,
                source: source.to_string(),
                steam_at: (steam && !chord).then(|| now + GUIDE_LONG_PRESS),
            });
        } else if let Some(hold) = self.guide.take() {
            let others = std::mem::take(&mut self.guide_others);
            if hold.chord {
                info!("guide released after the chord: no Home");
            } else if others.is_empty() {
                info!(held = ?now.duration_since(hold.since), "guide released alone → Home");
                self.home();
            } else {
                info!(buttons = ?others, "guide released (no chord) → Home");
                self.home();
            }
        } else {
            debug!("guide released without a press");
        }
    }

    /// Guide held for [`GUIDE_LONG_PRESS`]: Steam's Guide if Steam (or a Steam game) is still
    /// in front and Steam runs; otherwise the release is Home.
    fn long_guide(&mut self, now: Instant) {
        let Some(hold) = self.guide.as_mut() else {
            return;
        };
        hold.steam_at = None;
        let source = hold.source.clone();
        if !self.steam_takes_guide() {
            info!("guide held, but Steam is no longer in front → Home on release");
            return;
        }
        if !self.procs.steam_running() {
            info!("guide held, but Steam is not running → Home on release");
            return;
        }
        info!(
            held = ?GUIDE_LONG_PRESS,
            app = ?self.focused,
            "guide held → Guide to Steam (no Home)"
        );
        // The pad is in `All` now (InputPlumber switched on the press); the tap puts it back
        // in the focused app's mode first, so that the tap reaches Steam.
        self.input.guide_tap(&source, self.intercept_mode());
        self.guide = None;
        self.guide_others.clear();
        self.guide_tap_at = Some(now);
        // Steam shows its menu by putting its window first: leave its order alone.
        self.order.steam_menu = Some(
            self.gamescope
                .as_ref()
                .map(|g| g.baselayer_apps.clone())
                .unwrap_or_default(),
        );
        self.order.at = None;
    }

    /// A button pressed while Guide is held that is not the chord button: no long press for
    /// Steam any more (it is Home on release, as before).
    fn guide_other(&mut self, raw: &str, action: Option<NavAction>, now: Instant) {
        let Some(hold) = self
            .guide
            .as_mut()
            .filter(|hold| now.duration_since(hold.since) < GUIDE_HOLD_MAX)
        else {
            return;
        };
        hold.steam_at = None;
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
            Intent::Home => return self.guide(pressed, &input.source, now),
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
                .as_ref()
                .is_some_and(|hold| now.duration_since(hold.since) < GUIDE_HOLD_MAX);
            if guide_held && self.cfg.osk_summon == Some(action) && self.cfg.osk_enabled {
                info!(
                    button = %input.event,
                    "chord guide+{} → on-screen keyboard",
                    nav_name(action)
                );
                if let Some(hold) = self.guide.as_mut() {
                    hold.chord = true;
                    hold.steam_at = None;
                }
                self.swallow.insert(action);
                self.toggle_osk();
                return;
            }
            self.guide_other(&input.event, Some(action), now);
            self.held.insert(action);
            self.raw_names.insert(action, input.event.clone());
        } else {
            self.held.remove(&action);
            if let Some((source, button)) = self.clicking.remove(&action) {
                self.input.mouse_button(&source, button, false);
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
        if route == Route::Keys && pressed && self.keys_button(action, &input.source, now) {
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

    /// A button pressed in a keyboard-profile app that means more than its key: a click (RT left,
    /// LT right) with
    /// the pointer, the on-screen keyboard for a text field, the address bar. Returns whether
    /// it was handled (its release is then dropped, or ends the click).
    fn keys_button(&mut self, action: NavAction, source: &str, now: Instant) -> bool {
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
        let view_mapped = mapped(NavAction::View);
        let address_bar = app.address_bar;
        let app_id = app.info.id.clone();
        // RT is the left click and LT the right click whenever there is a pointer (unless the
        // app maps them); A never clicks (it is Enter, or the on-screen keyboard on a text field).
        let click = match action {
            NavAction::TriggerRight => Some(MouseButton::Left),
            NavAction::TriggerLeft => Some(MouseButton::Right),
            _ => None,
        }
        .filter(|_| pointer && !mapped(action));
        if let Some(button) = click {
            debug!(?action, ?button, "click");
            let change = self.pointer.motion();
            self.cursor_change(change);
            self.input.mouse_button(source, button, true);
            self.clicking.insert(action, (source.to_string(), button));
            if button == MouseButton::Left {
                self.left_click = Some((app_id, now));
            }
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
        self.adopted.remove(id);
        let to_steam = self.from_steam.remove(id) && self.apps.contains_key(STEAM_CLIENT_APP_ID);
        if self.focused.as_deref() == Some(id) {
            let back = if id == STEAM_CLIENT_APP_ID {
                self.steam_return
                    .take()
                    .filter(|r| self.apps.contains_key(r))
            } else if to_steam {
                // Started from Steam's library: back to Steam.
                Some(STEAM_CLIENT_APP_ID.to_string())
            } else {
                None
            };
            self.set_focus(back);
        } else if id == STEAM_CLIENT_APP_ID {
            // Steam writes its own focus order to show itself (its "Shutting down" screen
            // while something else is in front); once it is gone nothing in that order has a
            // window and gamescope keeps Steam's last frame: put ours back.
            self.apply_focus();
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
                        focusable = ?state.focusable_apps,
                        focused = ?state.focused_app,
                        "someone else changed GAMESCOPECTRL_BASELAYER_APPID (Steam?)"
                    );
                }
                self.gamescope = Some(state);
                self.observe_steam_games();
                self.observe(now);
                self.adopt_steam_client_games(now);
                if self.steam_shown() {
                    self.seen_windows
                        .extend(self.steam_windows.iter().map(|w| w.window));
                    self.update_attention();
                }
                self.check_order(now);
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
            let ids = || std::iter::once(gs).chain(app.aliases.iter().copied());
            let listed = ids().any(|id| state.focusable_apps.contains(&id));
            let is_focused = state.focused_app.is_some_and(|f| ids().any(|id| id == f));
            app.seen_focusable |= listed;
            app.seen_focused |= is_focused;
            if tracked_by_windows(app.info.source) {
                // Gone when the Xwayland windows it had are gone. (A native Wayland game is
                // never listed; the focus-loss rule below covers it.) A Steam game whose
                // reaper ran: look whether that is still so (see `tick`), window or not.
                let had = app.seen_focusable || app.reaper_seen;
                let gone = had && !listed && !is_focused;
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

    /// Keep gamescope's focus order right while Steam or a Steam game has focus (on each
    /// gamescope state, and at `order.at`):
    /// - Our order changed (a game's window appeared under an alias): write it.
    /// - Steam wrote its own order (it does while Big Picture is open, and may put its own
    ///   window in front of the game) and that shows another app than ours would: write ours
    ///   again, at most every [`REASSERT_GAP`] and [`REASSERT_MAX`] times per
    ///   [`REASSERT_WINDOW`], then leave it to Steam. Not while Steam shows its menu (our
    ///   Guide tap), and only for a Steam game: Steam with focus orders its windows itself.
    /// - The launcher has focus and Steam's order shows Steam (it does when a game ends or is
    ///   closed): the screen showed Steam while the input went to the launcher, and A started
    ///   whatever the launcher had selected. Ours again, the same way; when Steam does not stop,
    ///   Steam gets focus, so that the input goes where the picture is. Steam's dialogs still
    ///   come to the launcher as `steam_attention`.
    fn check_order(&mut self, now: Instant) {
        let steam_game = match self.focused_app() {
            None => false,
            Some(app) if app.info.source == AppSource::Steam => true,
            Some(app) if app.info.source == AppSource::SteamClient => false,
            Some(_) => return,
        };
        let launcher_focused = self.focused.is_none();
        let (_, ours) = self.focus_plan();
        if self.written.back() != Some(&ours) {
            info!(order = ?ours, app = ?self.focused, "focus order changes");
            self.order.gave_up = false;
            self.apply_focus();
            return;
        }
        let Some(state) = &self.gamescope else { return };
        let found = &state.baselayer_apps;
        if !(steam_game || launcher_focused) || found.is_empty() || self.written.contains(found) {
            return;
        }
        let launcher = self.cfg.launcher_app_id;
        let theirs = shown_by(found, &state.focusable_apps, launcher);
        let wanted = shown_by(&ours, &state.focusable_apps, launcher);
        if theirs == wanted {
            // Steam shows what we would.
            if self.order.steam_menu.as_ref() != Some(found) {
                self.order.steam_menu = None;
            }
            self.order.gave_up = false;
            return;
        }
        if self.order.steam_menu.is_some() || self.order.gave_up {
            debug!(
                ?found,
                ?theirs,
                "Steam's focus order stays (its menu, or gave up)"
            );
            return;
        }
        self.order
            .times
            .retain(|t| now.duration_since(*t) < REASSERT_WINDOW);
        if let Some(last) = self.order.times.back()
            && now < *last + REASSERT_GAP
        {
            self.order.at = Some(*last + REASSERT_GAP);
            return;
        }
        if self.order.times.len() >= REASSERT_MAX {
            if launcher_focused && self.apps.contains_key(STEAM_CLIENT_APP_ID) {
                warn!(
                    ?found,
                    "Steam keeps showing itself over the launcher; Steam gets focus"
                );
                self.set_focus(Some(STEAM_CLIENT_APP_ID.to_string()));
                return;
            }
            warn!(
                ?found,
                ours = ?ours,
                "Steam keeps changing the focus order; it stays as Steam wrote it"
            );
            self.order.gave_up = true;
            self.order.at = None;
            return;
        }
        info!(
            ?found,
            ours = ?ours,
            focusable = ?state.focusable_apps,
            ?theirs,
            ?wanted,
            "Steam's focus order hides {}: writing ours again",
            if launcher_focused { "the launcher" } else { "the game" }
        );
        self.order.times.push_back(now);
        self.order.at = None;
        self.apply_focus();
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
            .chain(self.order.at)
            .chain(self.osk_auto_hide_at)
            .chain(self.guide.as_ref().and_then(|hold| hold.steam_at))
            .min()
    }

    pub fn tick(&mut self, now: Instant) {
        if self
            .guide
            .as_ref()
            .and_then(|hold| hold.steam_at)
            .is_some_and(|at| now >= at)
        {
            self.long_guide(now);
        }
        if self.focus_retry_at.is_some_and(|at| now >= at) {
            self.focus_retry_at = None;
            self.apply_focus();
        }
        if self.order.at.is_some_and(|at| now >= at) {
            self.order.at = None;
            self.check_order(now);
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
                    // launcher show meanwhile; or Steam runs a game of its own: look again
                    // later.
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
        needs_terms: None,
        permissions: Vec::new(),
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
        /// `[games..., 769, launcher]`.
        FocusSteamClient(Vec<u32>),
        Mode(InterceptMode),
        Spawn {
            key: String,
            argv: Vec<String>,
            app_id: Option<u32>,
        },
        /// The next spawn runs in this sandbox.
        Sandbox(crate::sandbox::Spec),
        /// The next spawn runs on this app display.
        Display(String),
        Signal(u32, Sig),
        Osk(bool),
        Keys(Vec<KeyOp>),
        Pointer(bool),
        Click(bool),
        RightClick(bool),
        Cursor(bool),
        /// A Guide tap to Steam, the pad first in this mode.
        GuideTap(InterceptMode),
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
        fn focus_steam_client(&mut self, games: &[u32]) -> anyhow::Result<()> {
            self.0.push(Call::FocusSteamClient(games.to_vec()));
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
            self.0.push(match button {
                MouseButton::Left => Call::Click(pressed),
                MouseButton::Right => Call::RightClick(pressed),
            });
        }
        fn guide_tap(&mut self, _source: &str, mode: InterceptMode) {
            self.0.push(Call::GuideTap(mode));
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
            sandbox: Option<&crate::sandbox::Spec>,
            display: Option<&str>,
        ) -> std::io::Result<u32> {
            if self.fail {
                return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "nope"));
            }
            if let Some(spec) = sandbox {
                self.log.push(Call::Sandbox(spec.clone()));
            }
            if let Some(d) = display {
                self.log.push(Call::Display(d.into()));
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
        /// Reapers left on an X display (by display).
        display_users: BTreeMap<String, Vec<u32>>,
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
        fn display_users(&mut self, display: &str) -> Vec<u32> {
            let t = self.table.lock().unwrap();
            t.display_users.get(display).cloned().unwrap_or_default()
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
            icon_sha256: None,
            site_url: None,
            firefox_profile: None,
            command: command.map(|c| vec![c.into()]),
            steam_app_id,
            input: InputProfile::Gamepad,
            back_key: None,
            keymap: BTreeMap::new(),
            address_bar: false,
            pointer: None,
            permissions: BTreeMap::new(),
            sandbox: true,
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
                profile_mode: false,
                needs_terms: Default::default(),
                permissions: Default::default(),
                catalog_index: "/nonexistent/catalog.json".into(),
                sandbox: false,
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

        /// A gamescope state with the focus order someone wrote (Steam, or us).
        fn gamescope_order(&mut self, focused: Option<u32>, focusable: &[u32], order: &[u32]) {
            let state = GamescopeState {
                focused_app: focused,
                focusable_apps: focusable.to_vec(),
                baselayer_apps: order.to_vec(),
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

    /// App displays `auto+2`.. as `:12`.. (up to `auto+5`), with this session's number.
    fn with_displays(t: &mut T, dir: &std::path::Path, active: u32) {
        std::fs::write(dir.join("active"), format!("{active}\n")).unwrap();
        t.shell.displays = DisplayPool::new(Some("auto+2".into()), 4)
            .with_files(Some(dir.join("state")), Some(dir.join("active")))
            .with_resolver(Arc::new(|s: &str| {
                let k: u32 = s.strip_prefix("auto+")?.parse().ok()?;
                (k <= 5).then(|| format!(":{}", 10 + k))
            }));
    }

    fn displays_of(log: Vec<Call>) -> Vec<String> {
        log.into_iter()
            .filter_map(|c| match c {
                Call::Display(d) => Some(d),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn each_app_gets_a_display_of_its_own_until_it_exits() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = T::new();
        with_displays(&mut t, dir.path(), 2);
        let jellyfin = t.launch(json!({"id": "jellyfin"}));
        let youtube = t.launch(json!({"id": "youtube"}));
        assert_eq!(displays_of(t.log.take()), [":12", ":13"]);
        // Launching a running app only focuses it.
        t.launch(json!({"id": "jellyfin"}));
        assert!(displays_of(t.log.take()).is_empty());

        // Full: nothing starts; the error names the apps that hold one.
        t.events();
        let e = t
            .call("apps.launch", json!({"command": ["/usr/bin/xterm"]}))
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::NoAppDisplay.code());
        assert_eq!(e.data, Some(json!({"apps": [jellyfin, youtube]})));
        assert!(
            !t.log.take().iter().any(|c| matches!(c, Call::Spawn { .. })),
            "nothing started"
        );
        assert!(t.events().is_empty());
        // Steam and its games never take one.
        t.launch(json!({"url": "steam://rungameid/620"}));
        t.launch(json!({"id": "steam"}));
        assert!(displays_of(t.log.take()).is_empty());

        // Closing frees the display once the app has exited, not before.
        t.call("apps.close", json!({"id": "jellyfin"})).unwrap();
        let e = t
            .call("apps.launch", json!({"command": ["/usr/bin/xterm"]}))
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::NoAppDisplay.code());
        exit(&mut t, "jellyfin", jellyfin.pid.unwrap(), 0);
        let xterm = t.launch(json!({"command": ["/usr/bin/xterm"]}));
        assert_eq!(displays_of(t.log.take()), [":12"]);
        assert_eq!(xterm.id, "cmd-1");
    }

    #[test]
    fn a_display_with_processes_left_is_not_free() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = T::new();
        with_displays(&mut t, dir.path(), 2);
        // A reaper from before a daemon restart is still on :12.
        t.procs
            .lock()
            .unwrap()
            .display_users
            .insert(":12".into(), vec![77]);
        let jellyfin = t.launch(json!({"id": "jellyfin"}));
        assert_eq!(displays_of(t.log.take()), [":13"]);
        // Only apps we track can be closed: the error lists just those.
        let e = t.call("apps.launch", json!({"id": "youtube"})).unwrap_err();
        assert_eq!(e.data, Some(json!({"apps": [jellyfin]})));
        // Gone: :12 is free again.
        t.procs.lock().unwrap().display_users.clear();
        t.launch(json!({"id": "youtube"}));
        assert_eq!(displays_of(t.log.take()), [":12"]);

        // An app we gave up on after SIGKILL keeps its display while its processes are left.
        t.call("apps.close", json!({"id": "youtube"})).unwrap();
        t.advance(MS(10_001));
        assert!(!t.shell.apps.contains_key("youtube"));
        t.procs
            .lock()
            .unwrap()
            .display_users
            .insert(":12".into(), vec![102]);
        let e = t.call("apps.launch", json!({"id": "youtube"})).unwrap_err();
        assert_eq!(e.code, ErrorCode::NoAppDisplay.code());
    }

    #[test]
    fn no_app_display_at_all_fails_the_launch() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = T::new();
        with_displays(&mut t, dir.path(), 2);
        t.shell.displays = t.shell.displays.clone().with_resolver(Arc::new(|_| None));
        let e = t
            .call("apps.launch", json!({"id": "jellyfin"}))
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::AppLaunchFailed.code());
        assert!(
            e.message.contains("no app display (auto+2)"),
            "{}",
            e.message
        );
    }

    #[test]
    fn apps_displays_get_and_set() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = T::new();
        // No pool: the setting works, nothing is active.
        t.shell.displays =
            DisplayPool::new(None, 4).with_files(Some(dir.path().join("state")), None);
        assert_eq!(
            t.call("apps.displays.get", json!({})).unwrap(),
            json!({"count": 4, "active": null, "min": 1, "max": 8})
        );
        with_displays(&mut t, dir.path(), 4);
        assert_eq!(
            t.call("apps.displays.set", json!({"count": 6})).unwrap(),
            json!({"count": 6, "active": 4, "min": 1, "max": 8})
        );
        assert_eq!(
            t.call("apps.displays.get", json!({})).unwrap(),
            json!({"count": 6, "active": 4, "min": 1, "max": 8})
        );
        for bad in [0, 9] {
            let e = t
                .call("apps.displays.set", json!({ "count": bad }))
                .unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidParams.code());
        }
        // This session keeps 4 displays.
        for id in ["jellyfin", "youtube"] {
            t.launch(json!({ "id": id }));
        }
        assert_eq!(displays_of(t.log.take()), [":12", ":13"]);
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
    fn apps_list_reports_permissions_and_set_permission_changes_them() {
        use hinteros_api::catalog::Need;
        let t = tempfile::tempdir().unwrap();
        let index = t.path().join("catalog.json");
        std::fs::write(
            &index,
            r#"{"indexVersion":3,"apps":[
              {"id":"kodi","kind":"app","type":"native","name":"Kodi","status":"available",
               "permissions":{"gamepad":"required","downloads":"optional"}},
              {"id":"steam","kind":"app","type":"native","name":"Steam","status":"available",
               "permissions":{"gamepad":"optional"}}]}"#,
        )
        .unwrap();
        let perms = crate::permissions::Permissions::new(
            Some(t.path().join("permissions.json")),
            vec!["gamepad", "mic", "downloads"],
        );
        let mut t = T::with(|c| {
            c.permissions = perms.clone();
            c.catalog_index = index.clone();
            c.apps[0].permissions = BTreeMap::from([
                ("gamepad".into(), Need::Required),
                ("mic".into(), Need::Optional),
            ]);
            // A Steam game and an unsandboxed app list nothing.
            c.apps[1].permissions = c.apps[0].permissions.clone();
            c.apps[2].permissions = c.apps[0].permissions.clone();
            c.apps[2].sandbox = false;
        });
        let perms_of = |t: &mut T, i: usize| {
            t.call("apps.list", json!({})).unwrap()["apps"][i]["permissions"].clone()
        };
        assert_eq!(perms_of(&mut t, 0), json!([]), "steam");
        assert_eq!(
            perms_of(&mut t, 1),
            json!([
                {"name": "gamepad", "required": true, "granted": true},
                {"name": "mic", "required": false, "granted": true}
            ])
        );
        assert_eq!(perms_of(&mut t, 2), json!([]), "steam game");
        assert_eq!(perms_of(&mut t, 3), json!([]), "sandbox = false");

        t.events();
        let set = |t: &mut T, id: &str, perm: &str, granted: bool| {
            t.call(
                "apps.set_permission",
                json!({"id": id, "permission": perm, "granted": granted}),
            )
        };
        assert_eq!(set(&mut t, "jellyfin", "mic", false).unwrap(), json!({}));
        assert_eq!(perms_of(&mut t, 1)[1]["granted"], false);
        let names: Vec<EventName> = t.events().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, [EventName::AppsChanged, EventName::StoreChanged]);
        // A catalog app that is not installed yet.
        set(&mut t, "kodi", "downloads", false).unwrap();
        assert!(
            !perms
                .grants(
                    "kodi",
                    &BTreeMap::from([("downloads".into(), Need::Optional)])
                )
                .downloads
        );
        let code = |r: Result<Value, ErrorObject>| r.unwrap_err().code;
        assert_eq!(
            code(set(&mut t, "jellyfin", "gamepad", false)),
            ErrorCode::PermissionRequired.code()
        );
        assert_eq!(
            code(set(&mut t, "jellyfin", "downloads", false)),
            ErrorCode::UnknownPermission.code()
        );
        assert_eq!(
            code(set(&mut t, "youtube", "mic", false)),
            ErrorCode::UnknownPermission.code(),
            "unsandboxed"
        );
        assert_eq!(
            code(set(&mut t, "steam", "gamepad", false)),
            ErrorCode::UnknownPermission.code(),
            "Steam is never sandboxed"
        );
        assert_eq!(
            code(set(&mut t, "nope", "mic", false)),
            ErrorCode::UnknownApp.code()
        );
    }

    #[test]
    fn catalog_apps_launch_sandboxed_with_their_grants() {
        use crate::permissions::Grants;
        use hinteros_api::catalog::Need;
        let perms = crate::permissions::Permissions::new(None, vec!["gamepad", "mic", "downloads"]);
        let mut t = T::with(|c| {
            c.sandbox = true;
            c.permissions = perms.clone();
            c.apps[0].permissions = BTreeMap::from([
                ("gamepad".into(), Need::Required),
                ("downloads".into(), Need::Optional),
            ]);
            c.apps[2].permissions = BTreeMap::from([("mic".into(), Need::Optional)]);
            c.apps.push(AppConfig {
                sandbox: false,
                ..app_config("open", Some("open-app"), None)
            });
        });
        t.call(
            "apps.set_permission",
            json!({"id": "jellyfin", "permission": "downloads", "granted": false}),
        )
        .unwrap();
        let sandboxes = |t: &mut T| -> Vec<crate::sandbox::Spec> {
            t.log
                .take()
                .into_iter()
                .filter_map(|c| match c {
                    Call::Sandbox(s) => Some(s),
                    _ => None,
                })
                .collect()
        };
        t.launch(json!({"id": "jellyfin"}));
        assert_eq!(
            sandboxes(&mut t),
            [crate::sandbox::Spec {
                id: "jellyfin".into(),
                grants: Grants {
                    gamepad: true,
                    mic: false,
                    downloads: false,
                    // `lan` is not enforced here.
                    lan: true,
                },
                kiosk: false,
            }]
        );
        // A keyboard-profile app gets the text field socket.
        t.launch(json!({"id": "youtube"}));
        let s = sandboxes(&mut t);
        assert!(s[0].kiosk && s[0].grants.mic && !s[0].grants.gamepad);
        // Not sandboxed: sandbox = false, a Steam game, a plain command.
        t.launch(json!({"id": "open"}));
        t.launch(json!({"id": "portal"}));
        t.launch(json!({"command": ["sh"]}));
        assert!(sandboxes(&mut t).is_empty());
        // Sandbox off: none.
        let mut t = T::with(|c| c.permissions = perms.clone());
        t.launch(json!({"id": "jellyfin"}));
        assert!(sandboxes(&mut t).is_empty());
    }

    #[test]
    fn only_apps_with_lan_get_the_local_network() {
        use hinteros_api::catalog::Need;
        let perms = crate::permissions::Permissions::new(None, vec!["gamepad", "lan"]);
        let mut t = T::with(|c| {
            c.sandbox = true;
            c.permissions = perms.clone();
            c.apps[2].permissions = BTreeMap::from([("lan".into(), Need::Optional)]);
        });
        let lan = |t: &mut T, id: &str| -> bool {
            let pid = t.launch(json!({ "id": id })).pid.unwrap();
            let grants = t
                .log
                .take()
                .into_iter()
                .find_map(|c| match c {
                    Call::Sandbox(s) => Some(s.grants),
                    _ => None,
                })
                .unwrap();
            exit(t, id, pid, 0);
            grants.lan
        };
        assert!(!lan(&mut t, "jellyfin"), "does not declare lan");
        assert!(lan(&mut t, "youtube"), "declares it, granted by default");
        let list = t.call("apps.list", json!({})).unwrap();
        let youtube = list["apps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == "youtube")
            .unwrap();
        assert_eq!(
            youtube["permissions"],
            json!([{"name": "lan", "required": false, "granted": true}])
        );
        t.call(
            "apps.set_permission",
            json!({"id": "youtube", "permission": "lan", "granted": false}),
        )
        .unwrap();
        assert!(!lan(&mut t, "youtube"), "denied");
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
    fn a_game_steam_starts_itself_is_tracked_and_focused() {
        const SHORTCUT: u32 = 2_396_848_268;
        const GAME: &str = "steam-2396848268";
        let mut t = T::new();
        t.launch(json!({"id": "steam"}));
        t.gamescope(Some(769), &[769]);
        steam_windows(&mut t, &[(10, "Steam Big Picture Mode")]);
        t.events();
        t.log.take();
        // A non-Steam shortcut from Steam's library: its reaper has the shortcut's id, its
        // window the id its Steam API reports (480); Steam's window goes away meanwhile.
        t.procs
            .lock()
            .unwrap()
            .steam_reapers
            .insert(SHORTCUT, vec![500]);
        steam_windows(&mut t, &[]);
        t.gamescope(Some(480), &[480]);
        // Tracked (Continue lists it) and in front, with its window.
        assert!(
            t.events()
                .iter()
                .any(|(n, p)| *n == EventName::AppStarted && p["app"]["id"] == GAME)
        );
        assert_eq!(t.shell.focused(), Some(GAME));
        assert!(
            t.log
                .take()
                .contains(&Call::FocusSteamClient(vec![SHORTCUT, 480]))
        );
        let running = t.call("apps.running", json!({})).unwrap();
        assert!(
            running["apps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["id"] == GAME)
        );
        // Well past the launch timeout and the focus-loss debounce: still the game.
        t.advance(Duration::from_secs(120));
        assert_eq!(t.shell.focused(), Some(GAME), "not back to the launcher");
        assert!(!t.log.take().contains(&Call::FocusLauncher));
        // Home, then back to it from Continue: the game again.
        t.input("ui_guide", 1.0);
        t.input("ui_guide", 0.0);
        assert_eq!(t.shell.focused(), None);
        t.gamescope(Some(L), &[480]);
        t.log.take();
        t.call("apps.focus", json!({"id": GAME})).unwrap();
        assert!(
            t.log
                .take()
                .contains(&Call::FocusSteamClient(vec![SHORTCUT, 480]))
        );
        t.gamescope(Some(480), &[480]);
        // The game quits: no longer tracked, and Steam (where it was started) is in front.
        t.procs.lock().unwrap().steam_reapers.remove(&SHORTCUT);
        t.events();
        t.gamescope(Some(769), &[769]);
        assert!(
            t.events()
                .iter()
                .any(|(n, p)| *n == EventName::AppExited && p["id"] == GAME)
        );
        assert_eq!(t.shell.focused(), Some("steam"));
        t.advance(Duration::from_secs(5));
        assert_eq!(t.shell.focused(), Some("steam"));
    }

    #[test]
    fn steam_stays_in_front_while_it_runs_a_game_of_its_own() {
        // A Steam game we track (started from Home) does not count as Steam's own; one we
        // don't (between its reaper appearing and our adopting it) keeps Steam running.
        let mut t = T::new();
        t.launch(json!({"id": "steam"}));
        t.gamescope(Some(769), &[769]);
        t.procs.lock().unwrap().steam_reapers.insert(570, vec![501]);
        assert_eq!(t.shell.steam_client_games(), vec![570]);
        t.gamescope(Some(570), &[570]);
        assert_eq!(t.shell.focused(), Some("steam-570"));
        assert!(t.shell.steam_client_games().contains(&570));
    }

    #[test]
    fn our_own_reapers_are_never_steams_games() {
        // A command app's reaper from before a daemon restart (id above the launcher's, not
        // ours any more) does not count as a game Steam runs.
        let mut t = T::new();
        t.launch(json!({"id": "steam"}));
        t.gamescope(Some(769), &[769]);
        t.procs
            .lock()
            .unwrap()
            .steam_reapers
            .insert(L + 7, vec![600]);
        assert!(t.shell.steam_client_games().is_empty());
        t.procs.lock().unwrap().steam_reapers.insert(570, vec![601]);
        assert_eq!(t.shell.steam_client_games(), vec![570]);
        // A non-Steam shortcut's id may well be above the launcher's: still Steam's.
        let shortcut = L + 100_000;
        t.procs
            .lock()
            .unwrap()
            .steam_reapers
            .insert(shortcut, vec![602]);
        assert_eq!(t.shell.steam_client_games(), vec![570, shortcut]);
    }

    /// Zombie Force: a non-Steam shortcut whose Steam API says it is Spacewar (its
    /// steam_appid.txt), so its window ends up under 480, not under the shortcut's id.
    const ZOMBIE: u32 = 2_396_848_268;
    const ZOMBIE_APP: &str = "steam-2396848268";
    /// The first id Steam writes in its focus orders (no window of its own).
    const STEAM_FIRST: u32 = 413_091;

    /// Zombie Force launched from Home (Steam runs in the background); with `big_picture`,
    /// Steam's Big Picture is open behind the launcher. Log and events cleared.
    fn zombie_force(big_picture: bool) -> T {
        let mut t = T::new();
        t.procs.lock().unwrap().steam_running = true;
        if big_picture {
            t.launch(json!({"id": "steam"}));
            t.gamescope(Some(769), &[769, L]);
            steam_windows(&mut t, &[(10, "Steam Big Picture Mode")]);
            t.input("ui_guide", 1.0);
            t.input("ui_guide", 0.0);
            assert_eq!(t.shell.focused(), None);
            t.gamescope(Some(L), &[769, L]);
        }
        let app = t.launch(json!({"url": format!("steam://rungameid/{ZOMBIE}")}));
        assert_eq!(app.id, ZOMBIE_APP);
        assert_eq!(t.shell.focused(), Some(ZOMBIE_APP));
        t.events();
        t.log.take();
        t
    }

    /// The focus orders written (Mode and other calls left out).
    fn focus_calls(log: Vec<Call>) -> Vec<Call> {
        log.into_iter()
            .filter(|c| {
                matches!(
                    c,
                    Call::FocusLauncher
                        | Call::FocusApp(_)
                        | Call::FocusSteamGame(_)
                        | Call::FocusSteamClient(_)
                )
            })
            .collect()
    }

    /// Steam starts the game: its reaper, then its window, under the shortcut's id for a
    /// moment with `own_first` (gamescope: "appid clash was 2396848268 now 480"), then under
    /// 480. `others`: the other focusable apps (the first one focused); `order`: the focus
    /// order gamescope reports meanwhile (Steam's, with Big Picture open; empty: ours).
    fn zombie_force_window(t: &mut T, others: &[u32], own_first: bool, order: &[u32]) {
        let with = |ids: &[u32]| -> Vec<u32> { others.iter().chain(ids).copied().collect() };
        t.procs
            .lock()
            .unwrap()
            .steam_reapers
            .insert(ZOMBIE, vec![500]);
        t.gamescope_order(Some(others[0]), &with(&[]), order);
        if own_first {
            t.gamescope_order(Some(others[0]), &with(&[ZOMBIE]), order);
        }
        t.gamescope_order(Some(others[0]), &with(&[480]), order);
    }

    #[test]
    fn a_game_under_another_app_id_shows_as_soon_as_its_window_does() {
        // Cold start: Steam runs without Big Picture and leaves the focus order alone.
        for own_first in [false, true] {
            let mut t = zombie_force(false);
            // Steam's own window while it launches it (an interstitial).
            t.gamescope(Some(769), &[769, L]);
            zombie_force_window(&mut t, &[769, L], own_first, &[]);
            // Its window goes in front of Steam's as soon as it is there.
            assert_eq!(
                focus_calls(t.log.take()),
                [Call::FocusSteamClient(vec![ZOMBIE, 480])],
                "own_first: {own_first}"
            );
            // Written once.
            t.gamescope(Some(480), &[769, 480, L]);
            t.advance(Duration::from_secs(10));
            assert!(focus_calls(t.log.take()).is_empty());
            assert_eq!(t.shell.focused(), Some(ZOMBIE_APP));
            assert!(t.events().is_empty());
        }
    }

    #[test]
    fn a_game_under_another_app_id_gets_the_screen_while_big_picture_is_open() {
        for own_first in [false, true] {
            let mut t = zombie_force(true);
            // Big Picture writes its own focus orders while it starts the game.
            t.gamescope_order(Some(769), &[769, L], &[STEAM_FIRST, ZOMBIE, 769]);
            let steams = [STEAM_FIRST, 769, 480];
            zombie_force_window(&mut t, &[769, L], own_first, &steams);
            // Steam's window would be in front of the game: ours again, game first (and its
            // window under its own id, while it has one).
            let mut expected = vec![Call::FocusSteamClient(vec![ZOMBIE, 480])];
            if own_first {
                expected.insert(0, Call::FocusSteamGame(ZOMBIE));
            }
            assert_eq!(
                focus_calls(t.log.take()),
                expected,
                "own_first: {own_first}"
            );
            let ours = [ZOMBIE, 480, 769, L];
            t.gamescope_order(Some(480), &[769, 480, L], &ours);
            // Steam writes an order with the game first: left alone.
            t.advance(Duration::from_secs(1));
            t.gamescope_order(Some(480), &[769, 480, L], &[STEAM_FIRST, 480, 769]);
            t.advance(Duration::from_secs(1));
            assert!(focus_calls(t.log.take()).is_empty());
            // Steam's window first again: ours again.
            t.gamescope_order(Some(769), &[769, 480, L], &steams);
            t.advance(Duration::from_secs(1));
            assert_eq!(
                focus_calls(t.log.take()),
                [Call::FocusSteamClient(vec![ZOMBIE, 480])]
            );
            assert_eq!(t.shell.focused(), Some(ZOMBIE_APP));
        }
    }

    #[test]
    fn steam_rewriting_the_focus_order_is_not_fought_forever() {
        let mut t = zombie_force(true);
        let steams = [STEAM_FIRST, 769, 480];
        zombie_force_window(&mut t, &[769, L], false, &steams);
        t.log.take();
        // Steam puts its window first again ten times a second, for a minute.
        let mut writes = 0;
        for _ in 0..600 {
            t.gamescope_order(Some(769), &[769, 480, L], &steams);
            t.advance(MS(100));
            writes += focus_calls(t.log.take()).len();
        }
        assert!((1..=20).contains(&writes), "{writes} writes");
        // A quick pair of Steam orders gets one answer, not two.
        let mut t = zombie_force(true);
        zombie_force_window(&mut t, &[769, L], false, &steams);
        t.log.take();
        t.advance(Duration::from_secs(1));
        t.gamescope_order(Some(769), &[769, 480, L], &steams);
        t.gamescope_order(Some(769), &[769, 480, L], &[STEAM_FIRST, 769]);
        assert_eq!(focus_calls(t.log.take()).len(), 1);
    }

    #[test]
    fn steams_menu_over_a_game_under_another_app_id_is_left_alone() {
        let mut t = zombie_force(true);
        let steams = [STEAM_FIRST, 769, 480];
        zombie_force_window(&mut t, &[769, L], false, &steams);
        t.gamescope_order(Some(480), &[769, 480, L], &[ZOMBIE, 480, 769, L]);
        t.advance(Duration::from_secs(1));
        t.log.take();
        // Guide held: Steam's menu, which Steam shows by putting its window first.
        t.input("ui_guide", 1.0);
        t.advance(GUIDE_LONG_PRESS);
        t.input("ui_guide", 0.0);
        assert_eq!(t.log.take(), [Call::GuideTap(InterceptMode::Pass)]);
        t.gamescope_order(Some(769), &[769, 480, L], &steams);
        t.advance(Duration::from_secs(5));
        assert!(focus_calls(t.log.take()).is_empty());
        assert_eq!(t.shell.focused(), Some(ZOMBIE_APP));
        // Back in the game (Steam puts it first); later Steam's window first by itself: ours.
        t.gamescope_order(Some(480), &[769, 480, L], &[STEAM_FIRST, 480, 769]);
        t.advance(Duration::from_secs(1));
        t.gamescope_order(Some(769), &[769, 480, L], &steams);
        t.advance(Duration::from_secs(1));
        assert_eq!(
            focus_calls(t.log.take()),
            [Call::FocusSteamClient(vec![ZOMBIE, 480])]
        );
        // Home: the launcher. Steam showing itself over it gets ours again (it must not
        // show Steam while the input goes to the launcher).
        t.input("ui_guide", 1.0);
        t.input("ui_guide", 0.0);
        assert_eq!(focus_calls(t.log.take()), [Call::FocusLauncher]);
        t.gamescope_order(Some(769), &[769, 480, L], &steams);
        t.advance(Duration::from_secs(5));
        assert_eq!(focus_calls(t.log.take()), [Call::FocusLauncher]);
        assert_eq!(t.shell.focused(), None);
        // An order of Steam's that shows the launcher anyway is left alone.
        t.gamescope_order(Some(L), &[769, 480, L], &[STEAM_FIRST, L, 769]);
        t.advance(Duration::from_secs(5));
        assert!(focus_calls(t.log.take()).is_empty());
    }

    #[test]
    fn steam_does_not_show_itself_over_the_launcher_with_the_input_on_the_launcher() {
        // A game started from the launcher is closed with Home (or ends by itself); Steam
        // then puts its own window first. The picture showed Steam, the input went to the
        // launcher, and A started the app the launcher had selected.
        let mut t = zombie_force(true);
        zombie_force_window(&mut t, &[769, L], false, &[]);
        t.advance(Duration::from_secs(1));
        t.input("ui_guide", 1.0);
        t.input("ui_guide", 0.0);
        assert_eq!(t.shell.focused(), None);
        t.log.take();
        t.gamescope_order(Some(769), &[769, 480, L], &[STEAM_FIRST, 769, 480]);
        t.advance(Duration::from_secs(1));
        assert_eq!(focus_calls(t.log.take()), [Call::FocusLauncher]);
        assert_eq!(t.shell.focused(), None);
        // Steam does not stop: Steam gets focus, so the input goes where the picture is.
        for _ in 0..100 {
            if t.shell.focused().is_some() {
                break;
            }
            t.gamescope_order(Some(769), &[769, L], &[STEAM_FIRST, 769]);
            t.advance(MS(600));
        }
        assert_eq!(t.shell.focused(), Some("steam"));
        let writes = focus_calls(t.log.take());
        assert!((2..=REASSERT_MAX + 1).contains(&writes.len()), "{writes:?}");
        assert!(
            matches!(writes.last(), Some(Call::FocusSteamClient(_))),
            "{writes:?}"
        );
    }

    #[test]
    fn a_game_under_another_app_id_that_ends_by_itself_is_exited() {
        let exited = || {
            ev(
                EventName::AppExited,
                json!({"id": ZOMBIE_APP, "reason": "exited"}),
            )
        };
        for big_picture in [false, true] {
            for own_first in [false, true] {
                let case = format!("big_picture: {big_picture}, own_first: {own_first}");
                let mut t = zombie_force(big_picture);
                let steams: &[u32] = if big_picture {
                    &[STEAM_FIRST, 769, 480]
                } else {
                    &[]
                };
                zombie_force_window(&mut t, &[769, L], own_first, steams);
                t.advance(Duration::from_secs(10));
                assert_eq!(t.shell.focused(), Some(ZOMBIE_APP), "{case}");
                assert!(t.events().is_empty(), "{case}");
                // It ends by itself: its reaper and its window go.
                t.procs.lock().unwrap().steam_reapers.remove(&ZOMBIE);
                let order: &[u32] = if big_picture {
                    &[STEAM_FIRST, 769]
                } else {
                    &[]
                };
                t.gamescope_order(Some(769), &[769, L], order);
                t.advance(Duration::from_secs(3));
                assert!(t.events().contains(&exited()), "{case}");
                assert_eq!(t.shell.focused(), None, "{case}");
            }
        }
        // The window goes first, the reaper a while later (it saves, cleans up).
        let mut t = zombie_force(false);
        zombie_force_window(&mut t, &[769, L], false, &[]);
        t.gamescope(Some(769), &[769, L]);
        t.advance(Duration::from_secs(10));
        assert!(t.events().is_empty(), "the reaper is alive: still running");
        t.procs.lock().unwrap().steam_reapers.remove(&ZOMBIE);
        t.advance(Duration::from_secs(3));
        assert!(t.events().contains(&exited()));
        // It never shows a window, but its reaper came and went: ended, no launch timeout.
        let mut t = zombie_force(true);
        t.procs
            .lock()
            .unwrap()
            .steam_reapers
            .insert(ZOMBIE, vec![500]);
        t.gamescope(Some(769), &[769, L]);
        t.advance(Duration::from_secs(5));
        t.procs.lock().unwrap().steam_reapers.remove(&ZOMBIE);
        t.advance(Duration::from_secs(3));
        assert!(t.events().contains(&exited()));
    }

    #[test]
    fn steam_big_picture_entry() {
        let mut t = T::new();
        assert_eq!(
            t.call("apps.list", json!({})).unwrap()["apps"][0],
            json!({"id": "steam", "name": "Steam", "kind": "native", "permissions": []})
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
        // Home (on the release: Steam is in front): Steam stays in the background; its window
        // does not ask for attention.
        t.input("ui_guide", 1.0);
        t.input("ui_guide", 0.0);
        assert_eq!(t.shell.focused(), None);
        t.gamescope(Some(L), &[769]);
        t.events();
        assert_eq!(
            t.call("steam.attention", json!({})).unwrap(),
            json!({"windows": []})
        );
        // Closing Steam asks it to shut down, never kills it; its window goes: closed.
        t.log.take();
        t.call("apps.close", json!({"id": "steam"})).unwrap();
        assert_eq!(
            t.log.take(),
            [Call::Spawn {
                key: "steam-close".into(),
                argv: vec!["/fake/steam".into(), "-shutdown".into()],
                app_id: None,
            }]
        );
        t.gamescope(Some(L), &[]);
        steam_windows(&mut t, &[]);
        t.advance(MS(10_000));
        let log = t.log.take();
        assert!(!log.iter().any(|c| matches!(c, Call::Signal(..))));
        // Steam wrote its own focus order for its "Shutting down" screen: ours goes back.
        assert!(log.contains(&Call::FocusLauncher), "{log:?}");
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
        t.input("ui_guide", 0.0);
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

    /// The Steam game `portal` in front, Steam running; log and events cleared.
    fn steam_game_in_front(f: impl FnOnce(&mut ShellConfig)) -> T {
        let mut t = T::with(f);
        t.procs.lock().unwrap().steam_running = true;
        t.launch(json!({"id": "portal"}));
        t.gamescope(Some(400), &[400]);
        t.events();
        t.log.take();
        t
    }

    fn home_pressed(events: &[(EventName, Value)]) -> bool {
        events.iter().any(|(n, _)| *n == EventName::HomePressed)
    }

    #[test]
    fn guide_short_press_with_steam_in_front_is_home() {
        let mut t = steam_game_in_front(|_| {});
        // Held, but not long enough: nothing yet (Home waits for the release).
        t.input("ui_guide", 1.0);
        t.advance(GUIDE_LONG_PRESS - MS(1));
        assert!(t.events().is_empty());
        assert_eq!(t.shell.focused(), Some("portal"));
        t.input("ui_guide", 0.0);
        assert_eq!(t.shell.focused(), None);
        assert!(home_pressed(&t.events()));
        // Steam's keyboard, on Steam's overlay layer, would stay over the launcher: closed.
        assert_eq!(
            t.log.take(),
            [
                Call::Spawn {
                    key: "steam-keyboard".into(),
                    argv: vec!["/fake/steam".into(), "steam://close/keyboard".into()],
                    app_id: None,
                },
                Call::FocusLauncher,
                Call::Mode(InterceptMode::All)
            ]
        );
        // Nothing left to happen later.
        t.advance(Duration::from_secs(2));
        assert!(t.log.take().is_empty());
    }

    #[test]
    fn keyboard_home_is_home_at_once() {
        // Ctrl+Super+D (the system's keyboard_home, see system.rs) is Msg::Home, like SIGUSR1.
        // Steam in front: a Guide press would wait for its release; this does not.
        let mut t = steam_game_in_front(|_| {});
        t.shell.handle(Msg::Home, t.now);
        assert_eq!(t.shell.focused(), None);
        assert!(home_pressed(&t.events()));
        let log = t.log.take();
        assert!(
            matches!(&log[0], Call::Spawn { argv, .. } if argv[1] == "steam://close/keyboard"),
            "{log:?}"
        );
        assert_eq!(
            log[1..],
            [Call::FocusLauncher, Call::Mode(InterceptMode::All)]
        );
        // No Guide tap for Steam later.
        t.advance(Duration::from_secs(2));
        assert!(t.log.take().is_empty());
        // The game keeps running.
        let r = t.call("apps.running", json!({})).unwrap();
        assert_eq!(r["apps"][0]["id"], "portal");
        assert_eq!(r["focused"], Value::Null);
    }

    #[test]
    fn guide_long_press_goes_to_steam() {
        let mut t = steam_game_in_front(|_| {});
        t.input("ui_guide", 1.0);
        t.advance(GUIDE_LONG_PRESS - MS(1));
        assert!(t.log.take().is_empty());
        // At the threshold, still held: one Guide tap for Steam, the pad back in Pass first.
        t.advance(MS(1));
        assert_eq!(t.log.take(), [Call::GuideTap(InterceptMode::Pass)]);
        // The release (if it reaches us at all): no Home.
        t.advance(MS(300));
        t.input("ui_guide", 0.0);
        assert!(t.events().is_empty());
        assert_eq!(t.shell.focused(), Some("portal"));
        assert!(t.log.take().is_empty());

        // Should the injected tap ever come back to us as ui_guide, it is not ours: no Home,
        // no second tap.
        t.input("ui_guide", 1.0);
        t.advance(MS(80));
        t.input("ui_guide", 0.0);
        t.advance(Duration::from_secs(1));
        assert!(t.events().is_empty());
        assert_eq!(t.shell.focused(), Some("portal"));
        assert!(t.log.take().is_empty());

        // Later Guide presses are ours again.
        t.input("ui_guide", 1.0);
        t.input("ui_guide", 0.0);
        assert_eq!(t.shell.focused(), None);
        assert!(home_pressed(&t.events()));
    }

    #[test]
    fn guide_long_press_in_big_picture_goes_to_steam() {
        let mut t = T::new();
        t.procs.lock().unwrap().steam_running = true;
        t.launch(json!({"id": "steam"}));
        t.gamescope(Some(769), &[769]);
        t.events();
        t.log.take();
        t.input("ui_guide", 1.0);
        t.advance(GUIDE_LONG_PRESS);
        t.input("ui_guide", 0.0);
        assert_eq!(t.log.take(), [Call::GuideTap(InterceptMode::Pass)]);
        assert_eq!(t.shell.focused(), Some("steam"));
        assert!(t.events().is_empty());
    }

    #[test]
    fn guide_long_press_without_steam_is_home() {
        // Steam not running (the game's Steam went away): a long press is Home, on release.
        let mut t = steam_game_in_front(|_| {});
        t.procs.lock().unwrap().steam_running = false;
        t.input("ui_guide", 1.0);
        t.advance(Duration::from_secs(2));
        assert!(t.log.take().is_empty());
        assert_eq!(t.shell.focused(), Some("portal"));
        t.input("ui_guide", 0.0);
        assert_eq!(t.shell.focused(), None);
        assert!(home_pressed(&t.events()));
        assert!(!t.log.take().contains(&Call::GuideTap(InterceptMode::Pass)));

        // A non-Steam app in front (Steam running in the background): Home on the press, as
        // always, and nothing for Steam however long Guide is held.
        let mut t = T::new();
        t.procs.lock().unwrap().steam_running = true;
        t.launch(json!({"id": "jellyfin"}));
        t.events();
        t.log.take();
        t.input("ui_guide", 1.0);
        assert_eq!(t.shell.focused(), None);
        assert!(home_pressed(&t.events()));
        t.advance(Duration::from_secs(2));
        t.input("ui_guide", 0.0);
        assert_eq!(
            t.log.take(),
            [Call::FocusLauncher, Call::Mode(InterceptMode::All)]
        );

        // The launcher in front, Steam in the background: Home, nothing for Steam.
        t.input("ui_guide", 1.0);
        t.advance(Duration::from_secs(2));
        t.input("ui_guide", 0.0);
        assert!(home_pressed(&t.events()));
        assert!(!t.log.take().contains(&Call::GuideTap(InterceptMode::Pass)));
    }

    #[test]
    fn guide_long_press_with_the_osk_up_is_home() {
        // The on-screen keyboard over a Steam game has the pad: Home (which hides it).
        let mut t = steam_game_in_front(|_| {});
        t.call("osk.show", json!({})).unwrap();
        t.log.take();
        t.input("ui_guide", 1.0);
        t.advance(Duration::from_secs(1));
        t.input("ui_guide", 0.0);
        assert!(!t.log.take().contains(&Call::GuideTap(InterceptMode::Pass)));
        assert!(!t.shell.osk_visible());
        assert_eq!(t.shell.focused(), None);
    }

    #[test]
    fn guide_chord_within_the_threshold_is_the_keyboard_not_steam() {
        let mut t = steam_game_in_front(|c| c.osk_summon = Some(NavAction::Context));
        t.input("ui_guide", 1.0);
        t.advance(MS(200));
        t.input("ui_context", 1.0);
        assert!(t.shell.osk_visible());
        // Held well past the threshold: still only the keyboard.
        t.advance(Duration::from_secs(1));
        t.input("ui_context", 0.0);
        t.input("ui_guide", 0.0);
        assert_eq!(
            t.log.take(),
            [Call::Osk(true), Call::Mode(InterceptMode::All)]
        );
        assert_eq!(t.shell.focused(), Some("portal"));
        assert!(!home_pressed(&t.events()));
        t.call("osk.hide", json!({})).unwrap();
        t.log.take();
        t.events();

        // With the chord configured, Guide alone, held, is still Steam's.
        t.input("ui_guide", 1.0);
        t.advance(GUIDE_LONG_PRESS);
        assert_eq!(t.log.take(), [Call::GuideTap(InterceptMode::Pass)]);
        t.input("ui_guide", 0.0);
        assert!(t.events().is_empty());

        // Another button within the threshold: no Steam Guide, Home on release (as before).
        t.advance(Duration::from_secs(1));
        t.input("ui_guide", 1.0);
        t.input("ui_accept", 1.0);
        t.input("ui_accept", 0.0);
        t.advance(Duration::from_secs(1));
        assert!(t.log.take().is_empty());
        t.input("ui_guide", 0.0);
        assert_eq!(t.shell.focused(), None);
        assert!(home_pressed(&t.events()));
    }

    impl T {
        fn text_focus(&mut self, pid: u32, focus: FieldFocus) {
            self.text_focus_from(pid, 1, focus);
        }

        fn text_focus_from(&mut self, pid: u32, reporter: u64, focus: FieldFocus) {
            self.report(pid, reporter, focus, false);
        }

        fn report(&mut self, pid: u32, reporter: u64, focus: FieldFocus, clicked: bool) {
            self.shell.handle(
                Msg::TextFocus {
                    pid: Some(pid),
                    reporter,
                    focus,
                    clicked,
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
        // Moving to the next field (none, then a password): no flicker, only the masking.
        t.text_focus(555, FieldFocus::None);
        t.advance(MS(100));
        t.text_focus(555, FieldFocus::Password);
        t.advance(MS(1000));
        assert!(t.shell.osk_visible());
        assert!(t.log.take().is_empty());
        assert_eq!(
            t.events(),
            [ev(
                EventName::OskVisibility,
                json!({"visible": true, "password": true})
            )]
        );

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
                reporter: 1,
                focus: FieldFocus::Text,
                clicked: false,
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
    fn clicking_into_a_text_field_shows_the_keyboard() {
        // auto_show off (the default): focus by itself (arrow keys, a page focusing a field on
        // load) does not show it.
        let mut t = firefox_with_host(|_| {});
        t.text_focus(555, FieldFocus::Text);
        assert!(!t.shell.osk_visible());
        t.text_focus(555, FieldFocus::None);
        t.log.take();

        // RT (left click) on a field: it reports focus a moment later → shown, and hidden
        // again once the field loses focus, like auto_show.
        t.input("ui_r2", 1.0);
        t.advance(MS(30));
        t.text_focus(555, FieldFocus::Text);
        assert!(t.shell.osk_visible());
        assert_eq!(
            t.log.take(),
            [
                Call::Click(true),
                Call::Osk(true),
                Call::Mode(InterceptMode::All)
            ]
        );
        // The release still ends the click (the keyboard took the pad meanwhile).
        t.input("ui_r2", 0.0);
        assert_eq!(t.log.take(), [Call::Click(false)]);
        t.text_focus(555, FieldFocus::None);
        t.advance(OSK_AUTO_HIDE_DELAY);
        assert!(!t.shell.osk_visible());
        t.log.take();

        // A click used up: a later focus (the page focusing a field) does not show it.
        t.text_focus(555, FieldFocus::Text);
        assert!(!t.shell.osk_visible());
        t.text_focus(555, FieldFocus::None);
        // Too long after the click: not from it (a link opened a page that focused a field).
        t.input("ui_r2", 1.0);
        t.input("ui_r2", 0.0);
        t.advance(OSK_CLICK_WINDOW + MS(1));
        t.text_focus(555, FieldFocus::Text);
        assert!(!t.shell.osk_visible());
        t.text_focus(555, FieldFocus::None);
        // LT (right click, for the context menu) does not count.
        t.input("ui_l2", 1.0);
        t.input("ui_l2", 0.0);
        t.text_focus(555, FieldFocus::Text);
        assert!(!t.shell.osk_visible());

        // Clicking into the field that already has focus: the reporter says so itself.
        t.input("ui_r2", 1.0);
        t.input("ui_r2", 0.0);
        t.advance(Duration::from_secs(2));
        t.report(555, 1, FieldFocus::Text, true);
        assert!(t.shell.osk_visible());
        // B hides it; the field keeps focus; another click shows it again.
        t.input("ui_back", 1.0);
        t.input("ui_back", 0.0);
        assert!(!t.shell.osk_visible());
        t.report(555, 1, FieldFocus::Text, true);
        assert!(t.shell.osk_visible());
        // Shown for the click: hides with the field.
        t.text_focus(555, FieldFocus::None);
        t.advance(OSK_AUTO_HIDE_DELAY);
        assert!(!t.shell.osk_visible());

        // A still shows it without a click.
        t.text_focus(555, FieldFocus::Text);
        t.input("ui_accept", 1.0);
        assert!(t.shell.osk_visible());
    }

    #[test]
    fn clicking_the_address_bar_shows_the_keyboard() {
        const PAGE: u64 = 1;
        const BAR: u64 = 2;
        let mut t = firefox_with_host(|_| {});
        let firefox = t.shell.apps["youtube"].info.pid.unwrap();
        // A page field had focus; RT on the address bar: the bar (Firefox's chrome reporter,
        // which reports no click itself when the click missed the input) reports focus.
        t.text_focus_from(555, PAGE, FieldFocus::Text);
        t.input("ui_r2", 1.0);
        t.text_focus_from(firefox, BAR, FieldFocus::Text);
        t.text_focus_from(555, PAGE, FieldFocus::None);
        t.input("ui_r2", 0.0);
        assert!(t.shell.osk_visible());
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
    fn address_bar_and_page_fields_are_separate_reporters() {
        // Reporter 1: the extension's host (pid 555); reporter 2: Firefox itself (the app's
        // root process) for the address bar.
        let mut t = firefox_with_host(auto_show);
        let firefox = t.shell.apps["youtube"].info.pid.unwrap();
        const PAGE: u64 = 1;
        const BAR: u64 = 2;

        // Page field, then a click on the address bar: the bar says text, then the page's
        // late none must not hide the keyboard.
        t.text_focus_from(555, PAGE, FieldFocus::Text);
        assert!(t.shell.osk_visible());
        t.text_focus_from(firefox, BAR, FieldFocus::Text);
        t.text_focus_from(555, PAGE, FieldFocus::None);
        t.advance(Duration::from_secs(2));
        assert!(t.shell.osk_visible());
        assert_eq!(t.shell.deadline(), None);

        // The other order (page none first) only delays the hide until the bar reports.
        t.text_focus_from(555, PAGE, FieldFocus::Text);
        t.text_focus_from(firefox, BAR, FieldFocus::None);
        t.text_focus_from(555, PAGE, FieldFocus::None);
        t.text_focus_from(firefox, BAR, FieldFocus::Text);
        t.advance(Duration::from_secs(2));
        assert!(t.shell.osk_visible());

        // Hidden by hand while the bar has focus: the page's none does not bring it back.
        t.call("osk.hide", json!({})).unwrap();
        t.text_focus_from(555, PAGE, FieldFocus::None);
        assert!(!t.shell.osk_visible());
        // A on the bar shows it.
        t.input("ui_accept", 1.0);
        t.input("ui_accept", 0.0);
        assert!(t.shell.osk_visible());
        t.call("osk.hide", json!({})).unwrap();

        // Leaving the bar (Enter loads the page): no field anywhere, A is Enter again.
        t.text_focus_from(firefox, BAR, FieldFocus::None);
        t.log.take();
        t.input("ui_accept", 1.0);
        t.input("ui_accept", 0.0);
        assert_eq!(t.log.take(), [keys(&["enter"])]);

        // A closed reporter (a "none" from the socket) clears only its own field.
        t.text_focus_from(firefox, BAR, FieldFocus::Text);
        t.text_focus_from(555, PAGE, FieldFocus::Text);
        t.text_focus_from(555, PAGE, FieldFocus::None);
        t.call("osk.hide", json!({})).unwrap();
        t.log.take();
        t.input("ui_accept", 1.0);
        assert!(t.shell.osk_visible(), "the bar still has focus");
    }

    #[test]
    fn the_keyboard_knows_a_password_field() {
        let mut t = firefox_with_host(|_| {});
        let vis = |visible: bool, password: bool| {
            let mut p = json!({ "visible": visible });
            if password {
                p["password"] = json!(true);
            }
            ev(EventName::OskVisibility, p)
        };
        // Opened by A on a password field: masked.
        t.text_focus(555, FieldFocus::Password);
        t.input("ui_accept", 1.0);
        t.input("ui_accept", 0.0);
        assert_eq!(t.events(), [vis(true, true)]);
        assert_eq!(
            t.call("osk.show", json!({})).unwrap(),
            json!({ "visible": true, "password": true })
        );
        // Losing focus keeps it masked; a text field unmasks; a password field masks again.
        t.text_focus(555, FieldFocus::None);
        assert!(t.events().is_empty());
        t.text_focus(555, FieldFocus::Text);
        assert_eq!(t.events(), [vis(true, false)]);
        t.text_focus(555, FieldFocus::Text);
        assert!(t.events().is_empty());
        t.text_focus(555, FieldFocus::Password);
        assert_eq!(t.events(), [vis(true, true)]);
        t.call("osk.hide", json!({})).unwrap();
        assert_eq!(t.events(), [vis(false, false)]);

        // Opened with no field reported (the chord, osk.show): plain.
        t.text_focus(555, FieldFocus::None);
        t.call("osk.show", json!({})).unwrap();
        assert_eq!(t.events(), [vis(true, false)]);
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

        // LT is the right click, the same way.
        t.input("ui_l2", 1.0);
        t.input("ui_quick2", 1.0);
        t.input("ui_quick2", 0.0);
        t.input("ui_l2", 0.0);
        assert_eq!(
            t.log.take(),
            [Call::RightClick(true), Call::RightClick(false)]
        );

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
