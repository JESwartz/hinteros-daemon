//! The session API (`schema/session-api.json`): what the launcher talks to.

use serde::{Deserialize, Serialize};

use crate::common::{Hello, Ping};
use crate::{ApiInfo, ApiVersion, ErrorCode, Event, EventInfo, Method, MethodInfo};

pub const VERSION: ApiVersion = ApiVersion::new(0, 4);

/// The schema document this module implements.
pub const SCHEMA: &str = include_str!("../../../schema/session-api.json");

/// Default socket path, relative to `$XDG_RUNTIME_DIR`.
pub const SOCKET_IN_RUNTIME_DIR: &str = "hinteros/session.sock";

pub use crate::common::{HelloParams, HelloResult, Peer, PingParams, PingResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventName {
    HomePressed,
    AppStarted,
    AppExited,
    FocusChanged,
    InputNav,
    GamesChanged,
    SteamAttention,
    OskVisibility,
}

impl EventName {
    pub const ALL: &[EventName] = &[
        EventName::HomePressed,
        EventName::AppStarted,
        EventName::AppExited,
        EventName::FocusChanged,
        EventName::InputNav,
        EventName::GamesChanged,
        EventName::SteamAttention,
        EventName::OskVisibility,
    ];

    /// The event's wire name (its JSON-RPC notification method).
    pub const fn wire_name(self) -> &'static str {
        match self {
            EventName::HomePressed => HomePressed::NAME,
            EventName::AppStarted => AppStarted::NAME,
            EventName::AppExited => AppExited::NAME,
            EventName::FocusChanged => FocusChanged::NAME,
            EventName::InputNav => InputNav::NAME,
            EventName::GamesChanged => GamesChanged::NAME,
            EventName::SteamAttention => SteamAttention::NAME,
            EventName::OskVisibility => OskVisibility::NAME,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionParams {
    pub events: Vec<EventName>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionResult {
    /// The connection's full subscription set after the call, sorted.
    pub events: Vec<EventName>,
}

pub struct Subscribe;
impl Method for Subscribe {
    const NAME: &'static str = "subscribe";
    type Params = SubscriptionParams;
    type Result = SubscriptionResult;
}

pub struct Unsubscribe;
impl Method for Unsubscribe {
    const NAME: &'static str = "unsubscribe";
    type Params = SubscriptionParams;
    type Result = SubscriptionResult;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppsListParams {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppKind {
    Native,
    Web,
}

impl AppKind {
    pub const ALL: &[AppKind] = &[AppKind::Native, AppKind::Web];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct App {
    pub id: String,
    pub name: String,
    pub kind: AppKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppsListResult {
    pub apps: Vec<App>,
}

pub struct AppsList;
impl Method for AppsList {
    const NAME: &'static str = "apps.list";
    type Params = AppsListParams;
    type Result = AppsListResult;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomePressedParams {}

pub struct HomePressed;
impl Event for HomePressed {
    const NAME: &'static str = "home_pressed";
    type Params = HomePressedParams;
}

/// Params of `apps.launch`: exactly one of `id`, `command` or `url` (checked by the server; the
/// schema says so with `oneOf`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppsLaunchParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// URL prefix accepted by `apps.launch {url}`.
pub const STEAM_RUNGAMEID: &str = "steam://rungameid/";

/// What `apps.launch` was asked to start, after validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchRequest {
    Id(String),
    Command {
        argv: Vec<String>,
        name: Option<String>,
    },
    Steam(u32),
}

impl AppsLaunchParams {
    /// Check the "exactly one of" rule and parse the Steam URL. The error is for `invalid_params`.
    pub fn request(self) -> Result<LaunchRequest, String> {
        match (self.id, self.command, self.url) {
            (Some(id), None, None) => Ok(LaunchRequest::Id(id)),
            (None, Some(argv), None) if !argv.is_empty() => Ok(LaunchRequest::Command {
                argv,
                name: self.name,
            }),
            (None, None, Some(url)) => url
                .strip_prefix(STEAM_RUNGAMEID)
                .and_then(|id| id.parse::<u32>().ok())
                .filter(|&id| id != 0)
                .map(LaunchRequest::Steam)
                .ok_or_else(|| format!("not a {STEAM_RUNGAMEID}<appid> URL: {url}")),
            _ => Err("exactly one of 'id', 'command' (non-empty) or 'url' is required".into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppSource {
    Command,
    Steam,
    SteamClient,
}

impl AppSource {
    pub const ALL: &[AppSource] = &[AppSource::Command, AppSource::Steam, AppSource::SteamClient];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningApp {
    pub id: String,
    pub name: String,
    pub source: AppSource,
    pub gamescope_app_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppsLaunchResult {
    pub app: RunningApp,
}

pub struct AppsLaunch;
impl Method for AppsLaunch {
    const NAME: &'static str = "apps.launch";
    type Params = AppsLaunchParams;
    type Result = AppsLaunchResult;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppsRunningParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppsRunningResult {
    pub apps: Vec<RunningApp>,
    /// The focused app's id; `None` (JSON `null`) is the launcher.
    #[serde(deserialize_with = "nullable")]
    pub focused: Option<String>,
}

pub struct AppsRunning;
impl Method for AppsRunning {
    const NAME: &'static str = "apps.running";
    type Params = AppsRunningParams;
    type Result = AppsRunningResult;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppsFocusParams {
    /// The app to focus; `None` (JSON `null`) is the launcher. Required on the wire.
    #[serde(deserialize_with = "nullable")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppsFocusResult {}

pub struct AppsFocus;
impl Method for AppsFocus {
    const NAME: &'static str = "apps.focus";
    type Params = AppsFocusParams;
    type Result = AppsFocusResult;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppStartedParams {
    pub app: RunningApp,
}

pub struct AppStarted;
impl Event for AppStarted {
    const NAME: &'static str = "app_started";
    type Params = AppStartedParams;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    Exited,
    Crashed,
    LaunchTimeout,
    Closed,
}

impl ExitReason {
    pub const ALL: &[ExitReason] = &[
        ExitReason::Exited,
        ExitReason::Crashed,
        ExitReason::LaunchTimeout,
        ExitReason::Closed,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppExitedParams {
    pub id: String,
    pub reason: ExitReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

pub struct AppExited;
impl Event for AppExited {
    const NAME: &'static str = "app_exited";
    type Params = AppExitedParams;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusChangedParams {
    /// The focused app's id; `None` (JSON `null`) is the launcher.
    #[serde(deserialize_with = "nullable")]
    pub app: Option<String>,
}

pub struct FocusChanged;
impl Event for FocusChanged {
    const NAME: &'static str = "focus_changed";
    type Params = FocusChangedParams;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavAction {
    Up,
    Down,
    Left,
    Right,
    Accept,
    Back,
    Context,
    Secondary,
    Menu,
    View,
    PageLeft,
    PageRight,
    TriggerLeft,
    TriggerRight,
    Quick,
    Keyboard,
}

impl NavAction {
    pub const ALL: &[NavAction] = &[
        NavAction::Up,
        NavAction::Down,
        NavAction::Left,
        NavAction::Right,
        NavAction::Accept,
        NavAction::Back,
        NavAction::Context,
        NavAction::Secondary,
        NavAction::Menu,
        NavAction::View,
        NavAction::PageLeft,
        NavAction::PageRight,
        NavAction::TriggerLeft,
        NavAction::TriggerRight,
        NavAction::Quick,
        NavAction::Keyboard,
    ];

    /// Directions repeat while held.
    pub const fn is_direction(self) -> bool {
        matches!(
            self,
            NavAction::Up | NavAction::Down | NavAction::Left | NavAction::Right
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavPhase {
    Press,
    Repeat,
    Release,
}

impl NavPhase {
    pub const ALL: &[NavPhase] = &[NavPhase::Press, NavPhase::Repeat, NavPhase::Release];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputNavParams {
    pub action: NavAction,
    pub phase: NavPhase,
}

pub struct InputNav;
impl Event for InputNav {
    const NAME: &'static str = "input_nav";
    type Params = InputNavParams;
}

/// Id of the built-in app for Steam itself (Big Picture and Steam's other windows).
pub const STEAM_CLIENT_APP_ID: &str = "steam";

/// gamescope's app id for Steam's own windows (`window_is_steam`: `STEAM_BIGPICTURE` or app id
/// 769). The daemon tags Steam's windows with it.
pub const STEAM_CLIENT_GAMESCOPE_APP_ID: u32 = 769;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppsCloseParams {
    pub id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppsCloseResult {}

pub struct AppsClose;
impl Method for AppsClose {
    const NAME: &'static str = "apps.close";
    type Params = AppsCloseParams;
    type Result = AppsCloseResult;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GamesListParams {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallState {
    Installed,
    UpdateRequired,
    Updating,
    Installing,
    Uninstalling,
}

impl InstallState {
    pub const ALL: &[InstallState] = &[
        InstallState::Installed,
        InstallState::UpdateRequired,
        InstallState::Updating,
        InstallState::Installing,
        InstallState::Uninstalling,
    ];
}

/// Paths of a game's library artwork; `None` (JSON `null`) when Steam has not cached it. All
/// fields are required on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameArtwork {
    #[serde(deserialize_with = "nullable")]
    pub portrait: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub header: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub hero: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub logo: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub icon: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Game {
    pub appid: u32,
    pub name: String,
    pub install_state: InstallState,
    pub size_bytes: u64,
    /// Unix time in seconds; `None` (JSON `null`) if never played or unknown. Required on the wire.
    #[serde(deserialize_with = "nullable_u64")]
    pub last_played: Option<u64>,
    pub artwork: GameArtwork,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GamesListResult {
    pub games: Vec<Game>,
}

pub struct GamesList;
impl Method for GamesList {
    const NAME: &'static str = "games.list";
    type Params = GamesListParams;
    type Result = GamesListResult;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GamesChangedParams {}

pub struct GamesChanged;
impl Event for GamesChanged {
    const NAME: &'static str = "games_changed";
    type Params = GamesChangedParams;
}

/// One of Steam's own windows.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SteamWindow {
    pub window: u32,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteamAttentionParams {
    pub windows: Vec<SteamWindow>,
}

/// The `steam_attention` event.
pub struct SteamAttention;
impl Event for SteamAttention {
    const NAME: &'static str = "steam_attention";
    type Params = SteamAttentionParams;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteamAttentionQueryParams {}

/// The `steam.attention` method (same payload as the event).
pub struct SteamAttentionQuery;
impl Method for SteamAttentionQuery {
    const NAME: &'static str = "steam.attention";
    type Params = SteamAttentionQueryParams;
    type Result = SteamAttentionParams;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteamFocusParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteamFocusResult {
    pub app: RunningApp,
}

pub struct SteamFocus;
impl Method for SteamFocus {
    const NAME: &'static str = "steam.focus";
    type Params = SteamFocusParams;
    type Result = SteamFocusResult;
}

/// The `hello` client name of the on-screen keyboard. Connections with this name get
/// `input_nav` while the keyboard is visible (and only then).
pub const OSK_CLIENT_NAME: &str = "hinteros-osk";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OskParams {}

/// Result of `osk.show` / `osk.hide`, and the `osk_visibility` event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OskState {
    pub visible: bool,
}

pub struct OskShow;
impl Method for OskShow {
    const NAME: &'static str = "osk.show";
    type Params = OskParams;
    type Result = OskState;
}

pub struct OskHide;
impl Method for OskHide {
    const NAME: &'static str = "osk.hide";
    type Params = OskParams;
    type Result = OskState;
}

pub struct OskVisibility;
impl Event for OskVisibility {
    const NAME: &'static str = "osk_visibility";
    type Params = OskState;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeysTypeParams {
    pub text: String,
}

/// Longest `keys.type` text, in characters (the schema's `maxLength`).
pub const KEYS_TYPE_MAX_CHARS: usize = 4096;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeysTypeResult {
    /// Characters that were skipped, in order.
    pub untypeable: Vec<String>,
}

pub struct KeysType;
impl Method for KeysType {
    const NAME: &'static str = "keys.type";
    type Params = KeysTypeParams;
    type Result = KeysTypeResult;
}

/// Defines [`Key`] with its wire names.
macro_rules! keys {
    ($($variant:ident = $name:literal,)*) => {
        /// A key on the daemon's virtual keyboard (US layout).
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub enum Key {
            $(#[serde(rename = $name)] $variant,)*
        }

        impl Key {
            pub const ALL: &[Key] = &[$(Key::$variant,)*];

            /// The key's wire name (`"a"`, `"page_up"`, ...).
            pub const fn name(self) -> &'static str {
                match self {
                    $(Key::$variant => $name,)*
                }
            }

            pub fn from_name(name: &str) -> Option<Key> {
                match name {
                    $($name => Some(Key::$variant),)*
                    _ => None,
                }
            }
        }
    };
}

keys! {
    A = "a", B = "b", C = "c", D = "d", E = "e", F = "f", G = "g", H = "h", I = "i", J = "j",
    K = "k", L = "l", M = "m", N = "n", O = "o", P = "p", Q = "q", R = "r", S = "s", T = "t",
    U = "u", V = "v", W = "w", X = "x", Y = "y", Z = "z",
    Digit0 = "0", Digit1 = "1", Digit2 = "2", Digit3 = "3", Digit4 = "4",
    Digit5 = "5", Digit6 = "6", Digit7 = "7", Digit8 = "8", Digit9 = "9",
    F1 = "f1", F2 = "f2", F3 = "f3", F4 = "f4", F5 = "f5", F6 = "f6",
    F7 = "f7", F8 = "f8", F9 = "f9", F10 = "f10", F11 = "f11", F12 = "f12",
    Enter = "enter", Escape = "escape", Backspace = "backspace", Tab = "tab", Space = "space",
    Delete = "delete", Insert = "insert", Home = "home", End = "end",
    PageUp = "page_up", PageDown = "page_down", Up = "up", Down = "down", Left = "left", Right = "right",
    Minus = "minus", Equal = "equal", LeftBracket = "left_bracket", RightBracket = "right_bracket",
    Backslash = "backslash", Semicolon = "semicolon", Apostrophe = "apostrophe", Grave = "grave",
    Comma = "comma", Period = "period", Slash = "slash", CapsLock = "caps_lock",
    ContextMenu = "context_menu",
    Back = "back", Forward = "forward", Refresh = "refresh", PlayPause = "play_pause", Stop = "stop",
    NextTrack = "next_track", PreviousTrack = "previous_track",
    VolumeUp = "volume_up", VolumeDown = "volume_down", Mute = "mute",
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modifier {
    Shift,
    Ctrl,
    Alt,
    Meta,
}

impl Modifier {
    pub const ALL: &[Modifier] = &[
        Modifier::Shift,
        Modifier::Ctrl,
        Modifier::Alt,
        Modifier::Meta,
    ];

    pub fn from_name(name: &str) -> Option<Modifier> {
        match name {
            "shift" => Some(Modifier::Shift),
            "ctrl" => Some(Modifier::Ctrl),
            "alt" => Some(Modifier::Alt),
            "meta" => Some(Modifier::Meta),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeysKeyParams {
    pub key: Key,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<Modifier>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeysKeyResult {}

pub struct KeysKey;
impl Method for KeysKey {
    const NAME: &'static str = "keys.key";
    type Params = KeysKeyParams;
    type Result = KeysKeyResult;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyAction {
    Backspace,
    Delete,
    Enter,
    Tab,
    Escape,
    Space,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
}

impl KeyAction {
    pub const ALL: &[KeyAction] = &[
        KeyAction::Backspace,
        KeyAction::Delete,
        KeyAction::Enter,
        KeyAction::Tab,
        KeyAction::Escape,
        KeyAction::Space,
        KeyAction::Left,
        KeyAction::Right,
        KeyAction::Up,
        KeyAction::Down,
        KeyAction::Home,
        KeyAction::End,
    ];

    /// The key that performs the action.
    pub const fn key(self) -> Key {
        match self {
            KeyAction::Backspace => Key::Backspace,
            KeyAction::Delete => Key::Delete,
            KeyAction::Enter => Key::Enter,
            KeyAction::Tab => Key::Tab,
            KeyAction::Escape => Key::Escape,
            KeyAction::Space => Key::Space,
            KeyAction::Left => Key::Left,
            KeyAction::Right => Key::Right,
            KeyAction::Up => Key::Up,
            KeyAction::Down => Key::Down,
            KeyAction::Home => Key::Home,
            KeyAction::End => Key::End,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeysActionParams {
    pub action: KeyAction,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeysActionResult {}

pub struct KeysAction;
impl Method for KeysAction {
    const NAME: &'static str = "keys.action";
    type Params = KeysActionParams;
    type Result = KeysActionResult;
}

pub static API: ApiInfo = ApiInfo {
    api: "session",
    version: VERSION,
    methods: &[
        MethodInfo::of::<Hello>(),
        MethodInfo::of::<Ping>(),
        MethodInfo::of::<Subscribe>(),
        MethodInfo::of::<Unsubscribe>(),
        MethodInfo::of::<AppsList>(),
        MethodInfo::of::<AppsLaunch>(),
        MethodInfo::of::<AppsRunning>(),
        MethodInfo::of::<AppsFocus>(),
        MethodInfo::of::<AppsClose>(),
        MethodInfo::of::<GamesList>(),
        MethodInfo::of::<SteamAttentionQuery>(),
        MethodInfo::of::<SteamFocus>(),
        MethodInfo::of::<OskShow>(),
        MethodInfo::of::<OskHide>(),
        MethodInfo::of::<KeysType>(),
        MethodInfo::of::<KeysKey>(),
        MethodInfo::of::<KeysAction>(),
    ],
    events: &[
        EventInfo::of::<HomePressed>(),
        EventInfo::of::<AppStarted>(),
        EventInfo::of::<AppExited>(),
        EventInfo::of::<FocusChanged>(),
        EventInfo::of::<InputNav>(),
        EventInfo::of::<GamesChanged>(),
        EventInfo::of::<SteamAttention>(),
        EventInfo::of::<OskVisibility>(),
    ],
    errors: ErrorCode::ALL,
};

/// A required field whose value may be `null`. (A plain `Option` field would also accept the
/// field being absent.)
fn nullable<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(d)
}

fn nullable_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    Option::<u64>::deserialize(d)
}
