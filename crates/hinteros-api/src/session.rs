//! The session API (`schema/session-api.json`): what the launcher talks to.

use serde::{Deserialize, Serialize};

use crate::common::{Hello, Ping};
use crate::{ApiInfo, ApiVersion, ErrorCode, Event, EventInfo, Method, MethodInfo};

pub const VERSION: ApiVersion = ApiVersion::new(0, 1);

/// The schema document this module implements.
pub const SCHEMA: &str = include_str!("../../../schema/session-api.json");

/// Default socket path, relative to `$XDG_RUNTIME_DIR`.
pub const SOCKET_IN_RUNTIME_DIR: &str = "hinteros/session.sock";

pub use crate::common::{HelloParams, HelloResult, Peer, PingParams, PingResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventName {
    HomePressed,
}

impl EventName {
    pub const ALL: &[EventName] = &[EventName::HomePressed];
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

pub static API: ApiInfo = ApiInfo {
    api: "session",
    version: VERSION,
    methods: &[
        MethodInfo::of::<Hello>(),
        MethodInfo::of::<Ping>(),
        MethodInfo::of::<Subscribe>(),
        MethodInfo::of::<Unsubscribe>(),
        MethodInfo::of::<AppsList>(),
    ],
    events: &[EventInfo::of::<HomePressed>()],
    errors: ErrorCode::ALL,
};
