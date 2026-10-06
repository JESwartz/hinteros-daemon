//! The session API (`schema/session-api.json`): what the launcher talks to.

use serde::{Deserialize, Serialize};

use crate::common::{Hello, Ping};
use crate::{ApiInfo, ApiVersion, ErrorCode, Event, EventInfo, Method, MethodInfo};

pub const VERSION: ApiVersion = ApiVersion::new(0, 2);

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
}

impl EventName {
    pub const ALL: &[EventName] = &[
        EventName::HomePressed,
        EventName::AppStarted,
        EventName::AppExited,
        EventName::FocusChanged,
        EventName::InputNav,
    ];

    /// The event's wire name (its JSON-RPC notification method).
    pub const fn wire_name(self) -> &'static str {
        match self {
            EventName::HomePressed => HomePressed::NAME,
            EventName::AppStarted => AppStarted::NAME,
            EventName::AppExited => AppExited::NAME,
            EventName::FocusChanged => FocusChanged::NAME,
            EventName::InputNav => InputNav::NAME,
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
}

impl AppSource {
    pub const ALL: &[AppSource] = &[AppSource::Command, AppSource::Steam];
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
}

impl ExitReason {
    pub const ALL: &[ExitReason] = &[
        ExitReason::Exited,
        ExitReason::Crashed,
        ExitReason::LaunchTimeout,
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
    ],
    events: &[
        EventInfo::of::<HomePressed>(),
        EventInfo::of::<AppStarted>(),
        EventInfo::of::<AppExited>(),
        EventInfo::of::<FocusChanged>(),
        EventInfo::of::<InputNav>(),
    ],
    errors: ErrorCode::ALL,
};

/// A required field whose value may be `null`. (A plain `Option` field would also accept the
/// field being absent.)
fn nullable<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(d)
}
