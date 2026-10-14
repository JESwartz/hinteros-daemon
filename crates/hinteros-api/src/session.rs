//! The session API (`schema/session-api.json`): what the launcher talks to.

use serde::{Deserialize, Serialize};

use crate::common::{Hello, Ping};
use crate::{ApiInfo, ApiVersion, ErrorCode, Event, EventInfo, Method, MethodInfo};

pub const VERSION: ApiVersion = ApiVersion::new(0, 17);

/// The schema document this module implements.
pub const SCHEMA: &str = include_str!("../../../schema/session-api.json");

/// Default socket path, relative to `$XDG_RUNTIME_DIR`.
pub const SOCKET_IN_RUNTIME_DIR: &str = "hinteros/session.sock";

pub use crate::common::{HelloParams, HelloResult, Peer, PingParams, PingResult};

// Runtime settings (0.7).
pub use crate::runtime::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventName {
    HomePressed,
    AppStarted,
    AppExited,
    FocusChanged,
    InputNav,
    GamesChanged,
    AppsChanged,
    SteamAttention,
    OskVisibility,
    JobStarted,
    JobProgress,
    JobFinished,
    NetworkChanged,
    WifiListChanged,
    BluetoothChanged,
    BluetoothConfirm,
    AudioChanged,
    AuthenticationRequested,
    AuthenticationFinished,
    StoreChanged,
    SwitchProgress,
    SwitchBlocked,
    LockState,
    ProfilesChanged,
    GreeterPrompt,
    GreeterResult,
    TimeZoneChanged,
}

impl EventName {
    pub const ALL: &[EventName] = &[
        EventName::HomePressed,
        EventName::AppStarted,
        EventName::AppExited,
        EventName::FocusChanged,
        EventName::InputNav,
        EventName::GamesChanged,
        EventName::AppsChanged,
        EventName::SteamAttention,
        EventName::OskVisibility,
        EventName::JobStarted,
        EventName::JobProgress,
        EventName::JobFinished,
        EventName::NetworkChanged,
        EventName::WifiListChanged,
        EventName::BluetoothChanged,
        EventName::BluetoothConfirm,
        EventName::AudioChanged,
        EventName::AuthenticationRequested,
        EventName::AuthenticationFinished,
        EventName::StoreChanged,
        EventName::SwitchProgress,
        EventName::SwitchBlocked,
        EventName::LockState,
        EventName::ProfilesChanged,
        EventName::GreeterPrompt,
        EventName::GreeterResult,
        EventName::TimeZoneChanged,
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
            EventName::AppsChanged => AppsChanged::NAME,
            EventName::SteamAttention => SteamAttention::NAME,
            EventName::OskVisibility => OskVisibility::NAME,
            EventName::JobStarted => JobStarted::NAME,
            EventName::JobProgress => JobProgress::NAME,
            EventName::JobFinished => JobFinished::NAME,
            EventName::NetworkChanged => NetworkChanged::NAME,
            EventName::WifiListChanged => WifiListChanged::NAME,
            EventName::BluetoothChanged => BluetoothChanged::NAME,
            EventName::BluetoothConfirm => BluetoothConfirmEvent::NAME,
            EventName::AudioChanged => AudioChanged::NAME,
            EventName::AuthenticationRequested => AuthenticationRequested::NAME,
            EventName::AuthenticationFinished => AuthenticationFinished::NAME,
            EventName::StoreChanged => StoreChanged::NAME,
            EventName::SwitchProgress => SwitchProgress::NAME,
            EventName::SwitchBlocked => SwitchBlocked::NAME,
            EventName::LockState => LockState::NAME,
            EventName::ProfilesChanged => ProfilesChanged::NAME,
            EventName::GreeterPrompt => GreeterPrompt::NAME,
            EventName::GreeterResult => GreeterResult::NAME,
            EventName::TimeZoneChanged => TimeZoneChanged::NAME,
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
    /// 0.12: vendor terms this profile has not accepted yet (an app from the starter set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_terms: Option<StoreTerms>,
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppsChangedParams {}

pub struct AppsChanged;
impl Event for AppsChanged {
    const NAME: &'static str = "apps_changed";
    type Params = AppsChangedParams;
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
    /// The keyboard is up for a password field (0.10): the keyboard masks what it shows of
    /// the typed text. Only sent when true.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub password: bool,
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

    pub const fn name(self) -> &'static str {
        match self {
            Modifier::Shift => "shift",
            Modifier::Ctrl => "ctrl",
            Modifier::Alt => "alt",
            Modifier::Meta => "meta",
        }
    }

    pub fn from_name(name: &str) -> Option<Modifier> {
        Self::ALL.iter().copied().find(|m| m.name() == name)
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
        MethodInfo::of::<RelaySystemInfo>(),
        MethodInfo::of::<RelaySettingsGet>(),
        MethodInfo::of::<RelaySettingsSet>(),
        MethodInfo::of::<RelaySshSet>(),
        MethodInfo::of::<RelayGenerationsList>(),
        MethodInfo::of::<RelayRebuild>(),
        MethodInfo::of::<RelayHardwareRedetect>(),
        MethodInfo::of::<RelayRollback>(),
        MethodInfo::of::<RelayUpdateCheck>(),
        MethodInfo::of::<RelayUpdateApply>(),
        MethodInfo::of::<RelayChannelSet>(),
        MethodInfo::of::<RelayBaseSet>(),
        MethodInfo::of::<RelayAppsInstall>(),
        MethodInfo::of::<RelayAppsRemove>(),
        MethodInfo::of::<RelayAppsList>(),
        MethodInfo::of::<RelayAppsGenerations>(),
        MethodInfo::of::<RelayAppsRollback>(),
        MethodInfo::of::<RelayAppsAcceptTerms>(),
        MethodInfo::of::<RelayProfilesList>(),
        MethodInfo::of::<RelayProfilesCreate>(),
        MethodInfo::of::<RelayProfilesSetupAdmin>(),
        MethodInfo::of::<RelayProfilesRename>(),
        MethodInfo::of::<RelayProfilesSetAppearance>(),
        MethodInfo::of::<RelayProfilesSetPin>(),
        MethodInfo::of::<RelayProfilesSetLocked>(),
        MethodInfo::of::<RelayProfilesResetPin>(),
        MethodInfo::of::<RelayProfilesRemove>(),
        MethodInfo::of::<RelayProfilesSwitch>(),
        MethodInfo::of::<RelayProfilesSwitchCancel>(),
        MethodInfo::of::<RelayProfilesUnlock>(),
        MethodInfo::of::<RelayGuestSet>(),
        MethodInfo::of::<RelayGuestEnd>(),
        MethodInfo::of::<RelayDisplaySaveMode>(),
        MethodInfo::of::<RelayAudioSaveOutput>(),
        MethodInfo::of::<RelayStarterSetGet>(),
        MethodInfo::of::<RelayStarterSetSet>(),
        MethodInfo::of::<RelayStorageGet>(),
        MethodInfo::of::<RelayStorageMeasure>(),
        MethodInfo::of::<RelayStorageTree>(),
        MethodInfo::of::<RelayStorageCleanupSystem>(),
        MethodInfo::of::<RelayStorageCleanupProfiles>(),
        MethodInfo::of::<RelayCleanupGet>(),
        MethodInfo::of::<RelayCleanupSet>(),
        MethodInfo::of::<RelayTimeZoneGet>(),
        MethodInfo::of::<RelayTimeZoneList>(),
        MethodInfo::of::<RelayTimeZoneSet>(),
        MethodInfo::of::<RelayJobsGet>(),
        MethodInfo::of::<RelayJobsList>(),
        MethodInfo::of::<RelayJobsCancel>(),
        MethodInfo::of::<NetworkStatusMethod>(),
        MethodInfo::of::<WifiScan>(),
        MethodInfo::of::<WifiListMethod>(),
        MethodInfo::of::<WifiConnect>(),
        MethodInfo::of::<WifiDisconnect>(),
        MethodInfo::of::<WifiForget>(),
        MethodInfo::of::<WifiEnable>(),
        MethodInfo::of::<BluetoothStatus>(),
        MethodInfo::of::<BluetoothScan>(),
        MethodInfo::of::<BluetoothDevices>(),
        MethodInfo::of::<BluetoothPair>(),
        MethodInfo::of::<BluetoothConfirm>(),
        MethodInfo::of::<BluetoothConnect>(),
        MethodInfo::of::<BluetoothDisconnect>(),
        MethodInfo::of::<BluetoothForget>(),
        MethodInfo::of::<AudioOutputs>(),
        MethodInfo::of::<AudioSetOutput>(),
        MethodInfo::of::<AudioVolume>(),
        MethodInfo::of::<AudioMute>(),
        MethodInfo::of::<DisplayModesMethod>(),
        MethodInfo::of::<DisplaySetMode>(),
        MethodInfo::of::<DisplayKeep>(),
        MethodInfo::of::<DisplayRevert>(),
        MethodInfo::of::<PowerCapabilitiesMethod>(),
        MethodInfo::of::<PowerSuspend>(),
        MethodInfo::of::<PowerReboot>(),
        MethodInfo::of::<PowerPoweroff>(),
        MethodInfo::of::<AuthRespond>(),
        MethodInfo::of::<AuthCancel>(),
        MethodInfo::of::<SessionRestart>(),
        MethodInfo::of::<ProfileCurrent>(),
        MethodInfo::of::<SessionSwitchProfile>(),
        MethodInfo::of::<SessionSwitchWait>(),
        MethodInfo::of::<SessionSwitchForceClose>(),
        MethodInfo::of::<SessionSwitchCancel>(),
        MethodInfo::of::<SessionLockState>(),
        MethodInfo::of::<SessionUnlock>(),
        MethodInfo::of::<SessionEndGuest>(),
        MethodInfo::of::<GreeterList>(),
        MethodInfo::of::<GreeterLogin>(),
        MethodInfo::of::<GreeterPin>(),
        MethodInfo::of::<GreeterCancel>(),
        MethodInfo::of::<GreeterCreate>(),
        MethodInfo::of::<StoreList>(),
    ],
    events: &[
        EventInfo::of::<HomePressed>(),
        EventInfo::of::<AppStarted>(),
        EventInfo::of::<AppExited>(),
        EventInfo::of::<FocusChanged>(),
        EventInfo::of::<InputNav>(),
        EventInfo::of::<GamesChanged>(),
        EventInfo::of::<AppsChanged>(),
        EventInfo::of::<SteamAttention>(),
        EventInfo::of::<OskVisibility>(),
        EventInfo::of::<JobStarted>(),
        EventInfo::of::<JobProgress>(),
        EventInfo::of::<JobFinished>(),
        EventInfo::of::<NetworkChanged>(),
        EventInfo::of::<WifiListChanged>(),
        EventInfo::of::<BluetoothChanged>(),
        EventInfo::of::<BluetoothConfirmEvent>(),
        EventInfo::of::<AudioChanged>(),
        EventInfo::of::<AuthenticationRequested>(),
        EventInfo::of::<AuthenticationFinished>(),
        EventInfo::of::<StoreChanged>(),
        EventInfo::of::<SwitchProgress>(),
        EventInfo::of::<SwitchBlocked>(),
        EventInfo::of::<LockState>(),
        EventInfo::of::<ProfilesChanged>(),
        EventInfo::of::<GreeterPrompt>(),
        EventInfo::of::<GreeterResult>(),
        EventInfo::of::<TimeZoneChanged>(),
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

// ---------------------------------------------------------------------------------------------
// System relay (0.6): methods forwarded unchanged to the system service, and its job events.

macro_rules! relay {
    ($($name:ident = $wire:literal => $system:ty;)*) => {
        $(
            #[doc = concat!("`", $wire, "`: relayed to the system API's `", stringify!($system), "`.")]
            pub struct $name;
            impl Method for $name {
                const NAME: &'static str = $wire;
                type Params = <$system as Method>::Params;
                type Result = <$system as Method>::Result;
            }
        )*

        /// Session method name → system method name, for every relayed method.
        pub const RELAY: &[(&str, &str)] = &[$(($wire, <$system as Method>::NAME)),*];
    };
}

relay! {
    RelaySystemInfo = "system.info" => crate::system::SystemInfo;
    RelaySettingsGet = "system.settings.get" => crate::system::SettingsGet;
    RelaySettingsSet = "system.settings.set" => crate::system::SettingsSet;
    RelaySshSet = "system.ssh.set" => crate::system::SshSet;
    RelayGenerationsList = "system.generations.list" => crate::system::GenerationsList;
    RelayRebuild = "system.rebuild" => crate::system::Rebuild;
    RelayHardwareRedetect = "system.hardware.redetect" => crate::system::HardwareRedetect;
    RelayRollback = "system.rollback" => crate::system::Rollback;
    RelayUpdateCheck = "system.update.check" => crate::system::UpdateCheck;
    RelayUpdateApply = "system.update.apply" => crate::system::UpdateApply;
    RelayChannelSet = "system.channel.set" => crate::system::ChannelSet;
    RelayBaseSet = "system.base.set" => crate::system::BaseSet;
    RelayAppsInstall = "system.apps.install" => crate::system::AppsInstall;
    RelayAppsRemove = "system.apps.remove" => crate::system::AppsRemove;
    RelayAppsList = "system.apps.list" => crate::system::AppsList;
    RelayAppsGenerations = "system.apps.generations" => crate::system::AppsGenerations;
    RelayAppsRollback = "system.apps.rollback" => crate::system::AppsRollback;
    RelayAppsAcceptTerms = "system.apps.accept_terms" => crate::system::AppsAcceptTerms;
    RelayProfilesList = "system.profiles.list" => crate::system::ProfilesList;
    RelayProfilesCreate = "system.profiles.create" => crate::system::ProfilesCreate;
    RelayProfilesSetupAdmin = "system.profiles.setup_admin" => crate::system::ProfilesSetupAdmin;
    RelayProfilesRename = "system.profiles.rename" => crate::system::ProfilesRename;
    RelayProfilesSetAppearance = "system.profiles.set_appearance" => crate::system::ProfilesSetAppearance;
    RelayProfilesSetPin = "system.profiles.set_pin" => crate::system::ProfilesSetPin;
    RelayProfilesSetLocked = "system.profiles.set_locked" => crate::system::ProfilesSetLocked;
    RelayProfilesResetPin = "system.profiles.reset_pin" => crate::system::ProfilesResetPin;
    RelayProfilesRemove = "system.profiles.remove" => crate::system::ProfilesRemove;
    RelayProfilesSwitch = "system.profiles.switch" => crate::system::ProfilesSwitch;
    RelayProfilesSwitchCancel = "system.profiles.switch_cancel" => crate::system::ProfilesSwitchCancel;
    RelayProfilesUnlock = "system.profiles.unlock" => crate::system::ProfilesUnlock;
    RelayGuestSet = "system.guest.set" => crate::system::GuestSet;
    RelayGuestEnd = "system.guest.end" => crate::system::GuestEnd;
    RelayDisplaySaveMode = "system.display.save_mode" => crate::system::DisplaySaveMode;
    RelayAudioSaveOutput = "system.audio.save_output" => crate::system::AudioSaveOutput;
    RelayStarterSetGet = "system.starter_set.get" => crate::system::StarterSetGet;
    RelayStarterSetSet = "system.starter_set.set" => crate::system::StarterSetSet;
    RelayStorageGet = "system.storage.get" => crate::system::StorageGet;
    RelayStorageMeasure = "system.storage.measure" => crate::system::StorageMeasure;
    RelayStorageTree = "system.storage.tree" => crate::system::StorageTree;
    RelayStorageCleanupSystem = "system.storage.cleanup_system" => crate::system::StorageCleanupSystem;
    RelayStorageCleanupProfiles = "system.storage.cleanup_profiles" => crate::system::StorageCleanupProfiles;
    RelayCleanupGet = "system.cleanup.get" => crate::system::CleanupGet;
    RelayCleanupSet = "system.cleanup.set" => crate::system::CleanupSet;
    RelayTimeZoneGet = "system.time_zone.get" => crate::system::TimeZoneGet;
    RelayTimeZoneList = "system.time_zone.list" => crate::system::TimeZoneList;
    RelayTimeZoneSet = "system.time_zone.set" => crate::system::TimeZoneSet;
    RelayJobsGet = "system.jobs.get" => crate::system::JobsGet;
    RelayJobsList = "system.jobs.list" => crate::system::JobsList;
    RelayJobsCancel = "system.jobs.cancel" => crate::system::JobsCancel;
}

/// The system method a session method relays to, if it is a relay method.
pub fn relay_target(method: &str) -> Option<&'static str> {
    RELAY.iter().find(|(s, _)| *s == method).map(|(_, t)| *t)
}

pub struct JobStarted;
impl Event for JobStarted {
    const NAME: &'static str = "job_started";
    type Params = crate::system::JobStartedParams;
}

pub struct JobProgress;
impl Event for JobProgress {
    const NAME: &'static str = "job_progress";
    type Params = crate::system::JobProgressParams;
}

pub struct JobFinished;
impl Event for JobFinished {
    const NAME: &'static str = "job_finished";
    type Params = crate::system::JobFinishedParams;
}

/// 0.16: relayed from the system service's `time_zone_changed`.
pub struct TimeZoneChanged;
impl Event for TimeZoneChanged {
    const NAME: &'static str = "time_zone_changed";
    type Params = crate::system::TimeZoneInfo;
}

// ---------------------------------------------------------------------------------------------
// polkit authentication agent (0.8)

/// The `hello` client name of the launcher. Only connections with this name get
/// `authentication_requested` and may answer it (`auth.respond`, `auth.cancel`).
pub const LAUNCHER_CLIENT_NAME: &str = "hinteros-launcher";

/// Longest password `auth.respond` takes, in bytes (PAM's `PAM_MAX_RESP_SIZE` less its NUL).
pub const AUTH_PASSWORD_MAX_BYTES: usize = 511;

/// A polkit cookie: what the agent got from polkit, `^[A-Za-z0-9._-]{1,128}$`.
pub fn is_auth_cookie(s: &str) -> bool {
    (1..=128).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A user polkit accepts the authentication of (polkit's `unix-user` identity).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthIdentity {
    pub uid: u32,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthenticationRequestedParams {
    pub cookie: String,
    /// The polkit action, e.g. `org.hinteros.system.ssh`.
    pub action: String,
    /// polkit's message for the action (from its `.policy` file).
    pub message: String,
    /// Whose password polkit accepts; the daemon authenticates as the first.
    pub identities: Vec<AuthIdentity>,
}

pub struct AuthenticationRequested;
impl Event for AuthenticationRequested {
    const NAME: &'static str = "authentication_requested";
    type Params = AuthenticationRequestedParams;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthOutcome {
    /// The password was right; polkit authorizes the action.
    Authorized,
    /// Too many wrong passwords, or the helper failed.
    Failed,
    /// `auth.cancel`, polkit cancelled it, or nobody answered in time.
    Cancelled,
}

impl AuthOutcome {
    pub const ALL: &[AuthOutcome] = &[
        AuthOutcome::Authorized,
        AuthOutcome::Failed,
        AuthOutcome::Cancelled,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthenticationFinishedParams {
    pub cookie: String,
    pub outcome: AuthOutcome,
}

pub struct AuthenticationFinished;
impl Event for AuthenticationFinished {
    const NAME: &'static str = "authentication_finished";
    type Params = AuthenticationFinishedParams;
}

/// `auth.respond`. The password is never logged (`Debug` hides it).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthRespondParams {
    pub cookie: String,
    pub password: String,
}

impl std::fmt::Debug for AuthRespondParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthRespondParams")
            .field("cookie", &self.cookie)
            .field("password", &"<hidden>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthRespondResult {
    /// The password was accepted.
    pub ok: bool,
    /// Tries left for this request (0: it has failed, `authentication_finished` follows).
    pub attempts_left: u32,
}

pub struct AuthRespond;
impl Method for AuthRespond {
    const NAME: &'static str = "auth.respond";
    type Params = AuthRespondParams;
    type Result = AuthRespondResult;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthCancelParams {
    pub cookie: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthCancelResult {}

pub struct AuthCancel;
impl Method for AuthCancel {
    const NAME: &'static str = "auth.cancel";
    type Params = AuthCancelParams;
    type Result = AuthCancelResult;
}

// ---------------------------------------------------------------------------------------------
// Session restart (0.8)

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRestartParams {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRestartResult {}

pub struct SessionRestart;
impl Method for SessionRestart {
    const NAME: &'static str = "session.restart";
    type Params = SessionRestartParams;
    type Result = SessionRestartResult;
}

// ---------------------------------------------------------------------------------------------
// Profiles (0.12, profiles.md)

/// Params of the profile methods that take none (closed).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileNoParams {}

/// An empty result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileEmpty {}

/// `profile.current`: who this session is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileCurrentResult {
    /// The profile (or spare account) id; null for the guest.
    #[serde(deserialize_with = "nullable")]
    pub id: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub name: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub color: Option<String>,
    pub admin: bool,
    /// The profile asks for its PIN to enter (and after sleep).
    pub locked: bool,
    pub guest: bool,
    /// A spare account in first-boot mode: the launcher runs the first-boot wizard, which
    /// creates the admin profile (`system.profiles.setup_admin`).
    pub setup_required: bool,
    /// false: a migrated profile without a PIN yet ("Set your PIN").
    pub pin_set: bool,
    /// The sleep lock is up right now.
    pub screen_locked: bool,
}

pub struct ProfileCurrent;
impl Method for ProfileCurrent {
    const NAME: &'static str = "profile.current";
    type Params = ProfileNoParams;
    type Result = ProfileCurrentResult;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwitchProfileParams {
    /// A profile id or `guest`.
    pub id: String,
    /// The target's PIN, for a locked profile: checked before anything closes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
}

pub struct SessionSwitchProfile;
impl Method for SessionSwitchProfile {
    const NAME: &'static str = "session.switch_profile";
    type Params = SwitchProfileParams;
    type Result = ProfileEmpty;
}

pub struct SessionSwitchWait;
impl Method for SessionSwitchWait {
    const NAME: &'static str = "session.switch_wait";
    type Params = ProfileNoParams;
    type Result = ProfileEmpty;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwitchForceCloseParams {
    /// The app id from `switch_blocked`.
    pub app: String,
}

pub struct SessionSwitchForceClose;
impl Method for SessionSwitchForceClose {
    const NAME: &'static str = "session.switch_force_close";
    type Params = SwitchForceCloseParams;
    type Result = ProfileEmpty;
}

pub struct SessionSwitchCancel;
impl Method for SessionSwitchCancel {
    const NAME: &'static str = "session.switch_cancel";
    type Params = ProfileNoParams;
    type Result = ProfileEmpty;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockStateParams {
    pub locked: bool,
}

pub struct SessionLockState;
impl Method for SessionLockState {
    const NAME: &'static str = "session.lock_state";
    type Params = ProfileNoParams;
    type Result = LockStateParams;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUnlockParams {
    pub pin: String,
}

pub struct SessionUnlock;
impl Method for SessionUnlock {
    const NAME: &'static str = "session.unlock";
    type Params = SessionUnlockParams;
    type Result = ProfileEmpty;
}

pub struct SessionEndGuest;
impl Method for SessionEndGuest {
    const NAME: &'static str = "session.end_guest";
    type Params = ProfileNoParams;
    type Result = ProfileEmpty;
}

/// An app named in the switch events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppRef {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitchStage {
    ClosingApps,
    ClosingSteam,
    Ending,
    /// `session.switch_cancel`: the profile stays.
    Cancelled,
}

impl SwitchStage {
    pub const ALL: &[SwitchStage] = &[
        SwitchStage::ClosingApps,
        SwitchStage::ClosingSteam,
        SwitchStage::Ending,
        SwitchStage::Cancelled,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchProgressParams {
    pub stage: SwitchStage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<AppRef>,
}

pub struct SwitchProgress;
impl Event for SwitchProgress {
    const NAME: &'static str = "switch_progress";
    type Params = SwitchProgressParams;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchBlockedParams {
    pub app: AppRef,
    pub waited_s: u32,
}

pub struct SwitchBlocked;
impl Event for SwitchBlocked {
    const NAME: &'static str = "switch_blocked";
    type Params = SwitchBlockedParams;
}

pub struct LockState;
impl Event for LockState {
    const NAME: &'static str = "lock_state";
    type Params = LockStateParams;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfilesChangedParams {
    /// As `system.profiles.list`'s `profiles`.
    pub profiles: Vec<crate::system::ProfileInfo>,
}

pub struct ProfilesChanged;
impl Event for ProfilesChanged {
    const NAME: &'static str = "profiles_changed";
    type Params = ProfilesChangedParams;
}

// ---------------------------------------------------------------------------------------------
// Greeter mode (0.12): the picker

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GreeterProfile {
    pub id: String,
    pub name: String,
    pub color: String,
    pub locked: bool,
    pub admin: bool,
    #[serde(deserialize_with = "nullable_i64")]
    pub locked_until: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GreeterAppearance {
    pub text_scale: f64,
    pub high_contrast: bool,
    pub reduce_motion: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GreeterListResult {
    pub profiles: Vec<GreeterProfile>,
    /// The guest is on.
    pub guest: bool,
    pub first_boot: bool,
    /// The profile to focus: the switch target, else the last used one.
    #[serde(deserialize_with = "nullable")]
    pub focus: Option<String>,
    /// A pending switch to this profile (an unexpired hint): the picker goes straight to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch_to: Option<String>,
    pub appearance: GreeterAppearance,
}

pub struct GreeterList;
impl Method for GreeterList {
    const NAME: &'static str = "greeter.list";
    type Params = ProfileNoParams;
    type Result = GreeterListResult;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GreeterLoginParams {
    /// A profile id or `guest`.
    pub id: String,
}

pub struct GreeterLogin;
impl Method for GreeterLogin {
    const NAME: &'static str = "greeter.login";
    type Params = GreeterLoginParams;
    type Result = ProfileEmpty;
}

pub struct GreeterPin;
impl Method for GreeterPin {
    const NAME: &'static str = "greeter.pin";
    type Params = SessionUnlockParams;
    type Result = ProfileEmpty;
}

pub struct GreeterCancel;
impl Method for GreeterCancel {
    const NAME: &'static str = "greeter.cancel";
    type Params = ProfileNoParams;
    type Result = ProfileEmpty;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GreeterCreateParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    pub pin: String,
    pub locked: bool,
}

pub struct GreeterCreate;
impl Method for GreeterCreate {
    const NAME: &'static str = "greeter.create";
    type Params = GreeterCreateParams;
    type Result = crate::system::ProfilesCreateResult;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GreeterPromptParams {
    /// `pin`: show the PIN pad for the profile being entered.
    pub kind: String,
}

pub struct GreeterPrompt;
impl Event for GreeterPrompt {
    const NAME: &'static str = "greeter_prompt";
    type Params = GreeterPromptParams;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GreeterResultParams {
    /// true: the session starts and the greeter ends.
    pub ok: bool,
    /// The PIN backoff: wait this long.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked_seconds: Option<u32>,
    /// 0.17: the admin profile's PIN is refused until the box restarts (10 wrong tries this
    /// boot).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reboot_required: bool,
}

pub struct GreeterResult;
impl Event for GreeterResult {
    const NAME: &'static str = "greeter_result";
    type Params = GreeterResultParams;
}

fn nullable_i64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    Option::<i64>::deserialize(d)
}

// ---------------------------------------------------------------------------------------------
// Store (0.8)

/// Where an app is, as the Store shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreState {
    /// Not installed; can be installed.
    Available,
    Installed,
    /// An `apps.install` job for it is running.
    Installing,
    /// An `apps.remove` job for it is running.
    Removing,
    /// Installed, and retired in the catalog: keeps working, can be removed.
    Retired,
    /// Installed, and removed from the catalog: the next update takes it away.
    Unavailable,
}

impl StoreState {
    pub const ALL: &[StoreState] = &[
        StoreState::Available,
        StoreState::Installed,
        StoreState::Installing,
        StoreState::Removing,
        StoreState::Retired,
        StoreState::Unavailable,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogStatus {
    Available,
    Retired,
    Removed,
}

impl CatalogStatus {
    pub const ALL: &[CatalogStatus] = &[
        CatalogStatus::Available,
        CatalogStatus::Retired,
        CatalogStatus::Removed,
    ];
}

/// Where the session daemon finds the app's icon (the catalog's `icon`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreIcon {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
    pub firefox_profile: bool,
    /// The resolved icon on this box (an absolute path to an image), when there is one: from
    /// the installed app's desktop file, or downloaded and cached from `icon_url` /
    /// `site_url`. Absent until resolved; `store_changed` is sent when it appears (0.9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// The vendor's terms, shown in full before install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreTerms {
    pub title: String,
    /// Pass it back in `system.apps.install`'s `accepted_terms`.
    pub version: String,
    pub url: String,
    pub retrieved: String,
    /// Absolute path of the full text on this box (plain UTF-8), readable offline.
    pub file: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreApp {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub description: String,
    /// `games`, `video`, `music`, `photos`, `utilities`; anything else means "Other".
    pub category: String,
    pub homepage: String,
    pub kind: AppKind,
    pub size_mib: u64,
    pub icon: StoreIcon,
    pub requires: Vec<String>,
    pub depends: Vec<String>,
    pub conflicts: Vec<String>,
    pub installer: bool,
    pub on_iso: bool,
    pub installer_default: bool,
    /// What "also delete its data" deletes, relative to each profile's home.
    pub data_paths: Vec<String>,
    #[serde(deserialize_with = "nullable_terms")]
    pub terms: Option<StoreTerms>,
    pub builds_on_device: bool,
    pub free: bool,
    pub redistributable: bool,
    pub status: CatalogStatus,
    #[serde(deserialize_with = "nullable")]
    pub status_reason: Option<String>,
    pub state: StoreState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreListParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreListResult {
    /// The catalog index version the daemon read.
    pub index_version: u32,
    /// Available apps, and installed ones whatever their catalog status, by name.
    pub apps: Vec<StoreApp>,
    /// The last update check found apps that the next update makes installable.
    pub more_after_update: bool,
}

pub struct StoreList;
impl Method for StoreList {
    const NAME: &'static str = "store.list";
    type Params = StoreListParams;
    type Result = StoreListResult;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreChangedParams {}

pub struct StoreChanged;
impl Event for StoreChanged {
    const NAME: &'static str = "store_changed";
    type Params = StoreChangedParams;
}

fn nullable_terms<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<StoreTerms>, D::Error> {
    Option::<StoreTerms>::deserialize(d)
}
