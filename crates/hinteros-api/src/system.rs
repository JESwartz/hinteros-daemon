//! The internal system API (`schema/system-api.json`): spoken between the session daemon (and
//! any other process of a `hinteros` user) and the privileged system service. See
//! `hinteros-docs/design/system-api.md`.
//!
//! Every params type here rejects unknown fields: the service is a root process and its inputs
//! are closed (a forged `uid` or a stray `path` is an error, not ignored). A client never sends
//! more than its server knows (the server's minor is equal or higher). Results and events take
//! unknown fields and values: an older client reads a newer server.

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

use crate::common::{Hello, Ping};
use crate::{ApiInfo, ApiVersion, ErrorCode, ErrorObject, Event, EventInfo, Method, MethodInfo};

pub const VERSION: ApiVersion = ApiVersion::new(1, 0);

/// The schema document this module implements.
pub const SCHEMA: &str = include_str!("../../../schema/system-api.json");

pub const DEFAULT_SOCKET: &str = "/run/hinteros/system.sock";

/// Largest message on the system socket (the session socket allows 1 MiB).
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// A required field whose value may be `null`. (A plain `Option` field would also accept the
/// field being absent.)
fn nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    Option::<T>::deserialize(d)
}

/// A nullable field a later minor can fill with something this build does not know: that reads
/// as `None`.
fn lenient<'de, D: Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<Option<T>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(value).ok())
}

/// Server-issued job id: 16 lowercase hex digits.
pub fn is_job_id(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A full git commit id: 40 lowercase hex digits.
pub fn is_rev(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

// ---------------------------------------------------------------------------------------------
// Enums

/// Update channel (the branch of the system repo the box follows).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Stable,
    Beta,
}

impl Channel {
    pub const ALL: &[Channel] = &[Channel::Stable, Channel::Beta];

    pub const fn name(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.name() == s)
    }
}

/// nixpkgs base of the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Base {
    Stable,
    Unstable,
}

impl Base {
    pub const ALL: &[Base] = &[Base::Stable, Base::Unstable];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Settings,
    Ssh,
    Rebuild,
    HardwareRedetect,
    UpdateCheck,
    UpdateApply,
    Rollback,
    Channel,
    Base,
    AppsInstall,
    AppsRemove,
    /// A profile's apps back to an earlier generation.
    AppsRollback,
    /// A new starter env.
    StarterSet,
    /// A new account (a rebuild).
    ProfilesCreate,
    /// An account and its home removed (a rebuild).
    ProfilesRemove,
    /// The guest account on or off (a rebuild).
    GuestSet,
    /// The guest session ended and the guest wiped now (home, app env, sign-ins).
    GuestEnd,
    /// Measure what uses the disk (`storage.measure`).
    StorageMeasure,
    /// Old system generations removed, then garbage collected.
    CleanupSystem,
    /// Old app-env generations of every profile removed, then garbage collected.
    CleanupProfiles,
    /// The automatic clean-up setting changed (a rebuild).
    CleanupSet,
    /// The tip of another channel verified before `channel.set`.
    ChannelCheck,
    /// Services turned on (a rebuild).
    ServicesEnable,
    /// Services turned off (a rebuild), their data deleted on request.
    ServicesDisable,
    /// A Jellyfin library added.
    JellyfinLibraryAdd,
    /// A Jellyfin library removed (the media files stay).
    JellyfinLibraryRemove,
    /// Plex linked to the owner's account.
    PlexClaim,
    /// Tailscale sign-in (waits for the person at the sign-in URL).
    TailscaleLogin,
    /// Tailscale sign-out.
    TailscaleLogout,
    /// The Cloudflare tunnel token written and the tunnel restarted.
    CloudflareSetToken,
    /// The Cloudflare tunnel stopped and its token deleted.
    CloudflareClearToken,
    /// An app's data deleted in the caller's home (`apps.clear_data`).
    AppsClearData,
    /// A value of a later minor (`#[serde(other)]`). This build never sends it.
    #[serde(other)]
    Unknown,
}

impl JobKind {
    pub const ALL: &[JobKind] = &[
        JobKind::Settings,
        JobKind::Ssh,
        JobKind::Rebuild,
        JobKind::HardwareRedetect,
        JobKind::UpdateCheck,
        JobKind::UpdateApply,
        JobKind::Rollback,
        JobKind::Channel,
        JobKind::Base,
        JobKind::AppsInstall,
        JobKind::AppsRemove,
        JobKind::AppsRollback,
        JobKind::StarterSet,
        JobKind::ProfilesCreate,
        JobKind::ProfilesRemove,
        JobKind::GuestSet,
        JobKind::GuestEnd,
        JobKind::StorageMeasure,
        JobKind::CleanupSystem,
        JobKind::CleanupProfiles,
        JobKind::CleanupSet,
        JobKind::ChannelCheck,
        JobKind::ServicesEnable,
        JobKind::ServicesDisable,
        JobKind::JellyfinLibraryAdd,
        JobKind::JellyfinLibraryRemove,
        JobKind::PlexClaim,
        JobKind::TailscaleLogin,
        JobKind::TailscaleLogout,
        JobKind::CloudflareSetToken,
        JobKind::CloudflareClearToken,
        JobKind::AppsClearData,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    /// A value of a later minor (`#[serde(other)]`). This build never sends it.
    #[serde(other)]
    Unknown,
}

impl JobState {
    pub const ALL: &[JobState] = &[
        JobState::Running,
        JobState::Succeeded,
        JobState::Failed,
        JobState::Cancelled,
    ];
}

/// Job stages, in order. Not every job has every stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Preparing,
    Fetching,
    Verifying,
    /// The system rebuild that turns on box features an app needs (nix progress still
    /// attached).
    Box,
    /// Measuring the disk (`storage_measure`; with `percent`).
    Measuring,
    Evaluating,
    Downloading,
    Building,
    Activating,
    /// Removing old generations (clean-ups; no longer cancellable).
    Cleaning,
    /// Collecting garbage (clean-ups).
    Collecting,
    /// After a system switch, every profile's app env is rebuilt against the new system.
    Apps,
    /// Talking to a local service (Jellyfin, Plex, Tailscale, cloudflared).
    Configuring,
    /// Waiting for the person to sign in at `detail.auth_url` (Tailscale).
    SigningIn,
    Finishing,
    /// A value of a later minor (`#[serde(other)]`). This build never sends it.
    #[serde(other)]
    Unknown,
}

impl Stage {
    pub const ALL: &[Stage] = &[
        Stage::Preparing,
        Stage::Fetching,
        Stage::Verifying,
        Stage::Box,
        Stage::Measuring,
        Stage::Evaluating,
        Stage::Downloading,
        Stage::Building,
        Stage::Activating,
        Stage::Cleaning,
        Stage::Collecting,
        Stage::Apps,
        Stage::Configuring,
        Stage::SigningIn,
        Stage::Finishing,
    ];
}

/// Severity from a signed commit's `Security:` trailer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
    /// A value of a later minor (`#[serde(other)]`). This build never sends it.
    #[serde(other)]
    Unknown,
}

impl Severity {
    pub const ALL: &[Severity] = &[
        Severity::Low,
        Severity::Medium,
        Severity::High,
        Severity::Critical,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventName {
    JobStarted,
    JobProgress,
    JobFinished,
    /// Profiles changed (a job, a rename, a PIN, lock or appearance change): re-query.
    ProfilesChanged,
    /// The box's time zone changed (`time_zone.set`, or a rollback restored another).
    TimeZoneChanged,
    /// Ctrl + Super + D on a keyboard: Home (only to the seat0 owner's connections).
    KeyboardHome,
}

impl EventName {
    pub const ALL: &[EventName] = &[
        EventName::JobStarted,
        EventName::JobProgress,
        EventName::JobFinished,
        EventName::ProfilesChanged,
        EventName::TimeZoneChanged,
        EventName::KeyboardHome,
    ];

    pub const fn wire_name(self) -> &'static str {
        match self {
            EventName::JobStarted => JobStarted::NAME,
            EventName::JobProgress => JobProgress::NAME,
            EventName::JobFinished => JobFinished::NAME,
            EventName::ProfilesChanged => ProfilesChanged::NAME,
            EventName::TimeZoneChanged => TimeZoneChanged::NAME,
            EventName::KeyboardHome => KeyboardHome::NAME,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Core

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionParams {
    pub events: Vec<EventName>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionResult {
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

/// Params of every method that takes none.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoParams {}

/// Result of every method that starts a job. `job_id` is null when there was nothing to do
/// (the requested state is already the current one).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRef {
    #[serde(deserialize_with = "nullable")]
    pub job_id: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Read-only

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshInfo {
    pub enabled: bool,
    pub host_key_fingerprints: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemInfoResult {
    /// HinterOS release of the running generation (`unknown` if it does not say).
    pub version: String,
    /// Channel of `/etc/hinteros` (null without a locked channel, e.g. the dev box).
    #[serde(deserialize_with = "nullable")]
    pub channel: Option<Channel>,
    pub base: Base,
    /// Locked system repo revision.
    #[serde(deserialize_with = "nullable")]
    pub rev: Option<String>,
    /// Current system generation number.
    #[serde(deserialize_with = "nullable")]
    pub generation: Option<u32>,
    pub hostname: String,
    pub dev: bool,
    pub ssh: SshInfo,
}

pub struct SystemInfo;
impl Method for SystemInfo {
    const NAME: &'static str = "system.info";
    type Params = NoParams;
    type Result = SystemInfoResult;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshSettings {
    /// sshd runs (login: the admin account with its password).
    pub enabled: bool,
}

/// The user-settable view of `state.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub base: Base,
    pub ssh: SshSettings,
    /// The box features that are on (read-only).
    pub requirements: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsGetResult {
    /// Hash of the current `state.json`; pass it back to `settings.set`.
    pub revision: String,
    pub settings: Settings,
}

pub struct SettingsGet;
impl Method for SettingsGet {
    const NAME: &'static str = "settings.get";
    type Params = NoParams;
    type Result = SettingsGetResult;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Generation {
    pub generation: u32,
    /// When the generation was created (Unix time, seconds).
    pub date: i64,
    #[serde(deserialize_with = "nullable")]
    pub rev: Option<String>,
    pub current: bool,
    pub booted: bool,
    /// The accounts in that generation's `state.json`; `None` when it cannot be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profiles: Option<Vec<GenerationProfile>>,
    /// Box features a rollback to this generation would turn back on (the profiles' apps
    /// need them, its `state.json` lacks them). Absent when none, or unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readds_features: Option<Vec<String>>,
}

/// A profile of a generation's `state.json`, with its current name (`None`: not a profile now).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationProfile {
    pub id: String,
    #[serde(deserialize_with = "nullable")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationsListResult {
    pub generations: Vec<Generation>,
}

pub struct GenerationsList;
impl Method for GenerationsList {
    const NAME: &'static str = "generations.list";
    type Params = NoParams;
    type Result = GenerationsListResult;
}

// ---------------------------------------------------------------------------------------------
// Settings and SSH

/// The closed set of `state.json` fields `settings.set` may change. Each new setting adds a
/// typed field here (bool, enum, bounded integer), never a free-form value. Empty: SSH has its
/// own method and base/channel theirs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsChanges {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsSetParams {
    pub revision: String,
    pub changes: SettingsChanges,
}

pub struct SettingsSet;
impl Method for SettingsSet {
    const NAME: &'static str = "settings.set";
    type Params = SettingsSetParams;
    type Result = JobRef;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshSetParams {
    pub enabled: bool,
}

pub struct SshSet;
impl Method for SshSet {
    const NAME: &'static str = "ssh.set";
    type Params = SshSetParams;
    type Result = JobRef;
}

// ---------------------------------------------------------------------------------------------
// Rebuild, update, rollback, channel, base

pub struct Rebuild;
impl Method for Rebuild {
    const NAME: &'static str = "rebuild";
    type Params = NoParams;
    type Result = JobRef;
}

/// The admin profile's PIN guards the box-wide actions (`hardware.redetect`, `rollback`,
/// `update.apply`, `channel.set`, `base.set`): `pin_required {pin: admin}` without it or when it is wrong, on a
/// caller that is not root.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HardwareRedetectParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_pin: Option<String>,
}

pub struct HardwareRedetect;
impl Method for HardwareRedetect {
    const NAME: &'static str = "hardware.redetect";
    type Params = HardwareRedetectParams;
    type Result = JobRef;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u32>,
    /// The admin profile's PIN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_pin: Option<String>,
}

pub struct Rollback;
impl Method for Rollback {
    const NAME: &'static str = "rollback";
    type Params = RollbackParams;
    type Result = JobRef;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableUpdate {
    pub rev: String,
    /// Commit time of `rev` (Unix time, seconds).
    pub date: i64,
    /// Subject line of `rev`.
    pub summary: String,
    /// Principal of the allowed signer that signed `rev`.
    pub signer: String,
    /// Highest `Security:` trailer severity among the new commits, or null.
    #[serde(deserialize_with = "nullable")]
    pub security: Option<Severity>,
    /// Number of new commits.
    pub commits: u32,
    /// Catalog apps the update makes installable (in the target's catalog, not available in the
    /// running one), sorted by id; null if the target's catalog could not be read.
    #[serde(default, deserialize_with = "nullable")]
    pub new_apps: Option<Vec<CatalogApp>>,
    /// Installed apps the update removes (tombstones or missing in the target's catalog); null
    /// if the target's catalog could not be read.
    #[serde(default, deserialize_with = "nullable")]
    pub removed_apps: Option<Vec<RemovedApp>>,
    /// The release `rev` carries: its `release/version` file if that is a semantic version,
    /// otherwise the first 12 hex digits of `rev`. Absent from services before M6.
    #[serde(default, deserialize_with = "nullable")]
    pub version: Option<String>,
    /// The newest `Security-Summary:` trailer among the new commits (one line, at most 120
    /// characters), or null.
    #[serde(default, deserialize_with = "nullable")]
    pub security_summary: Option<String>,
}

/// A catalog app named in an update check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogApp {
    pub id: String,
    pub name: String,
}

/// An installed app that an update removes, with the catalog's reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovedApp {
    pub id: String,
    pub name: String,
    #[serde(deserialize_with = "nullable")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateCheckResult {
    pub channel: Channel,
    #[serde(deserialize_with = "nullable")]
    pub current_rev: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub available: Option<AvailableUpdate>,
    /// When the check ran (Unix time, seconds).
    pub checked_at: i64,
}

/// `update.check`: a new job, or (within the cache window) the last result and no job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateCheckReply {
    #[serde(deserialize_with = "nullable")]
    pub job_id: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub result: Option<UpdateCheckResult>,
}

pub struct UpdateCheck;
impl Method for UpdateCheck {
    const NAME: &'static str = "update.check";
    type Params = NoParams;
    type Result = UpdateCheckReply;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateApplyParams {
    pub rev: String,
    /// The admin profile's PIN (installing a system update is box-wide: needed from every
    /// profile, the guest's included).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_pin: Option<String>,
}

pub struct UpdateApply;
impl Method for UpdateApply {
    const NAME: &'static str = "update.apply";
    type Params = UpdateApplyParams;
    type Result = JobRef;
}

/// `channel.check`: verify the tip of another channel before `channel.set`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelCheckParams {
    pub channel: Channel,
}

pub struct ChannelCheck;
impl Method for ChannelCheck {
    const NAME: &'static str = "channel.check";
    type Params = ChannelCheckParams;
    type Result = JobRef;
}

/// The result of a `channel_check` job: the verified tip of the target channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelCheckResult {
    /// The target channel.
    pub channel: Channel,
    /// The locked revision when the check ran.
    #[serde(deserialize_with = "nullable")]
    pub current_rev: Option<String>,
    /// The target's verified tip (`commits` and `security` count the commits the installed
    /// revision does not have).
    pub target: AvailableUpdate,
    /// The target does not descend from the installed revision (the switch may go back to an
    /// older version).
    pub older: bool,
    /// When the check ran (Unix time, seconds).
    pub checked_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelSetParams {
    pub channel: Channel,
    /// The target revision the owner confirmed: the last `channel.check` result's
    /// `target.rev` for this channel (needed unless `channel` is the current one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// The admin profile's PIN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_pin: Option<String>,
}

pub struct ChannelSet;
impl Method for ChannelSet {
    const NAME: &'static str = "channel.set";
    type Params = ChannelSetParams;
    type Result = JobRef;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaseSetParams {
    pub base: Base,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledge_downgrade: Option<bool>,
    /// The admin profile's PIN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_pin: Option<String>,
}

pub struct BaseSet;
impl Method for BaseSet {
    const NAME: &'static str = "base.set";
    type Params = BaseSetParams;
    type Result = JobRef;
}

// ---------------------------------------------------------------------------------------------
// Store (M8)

/// `apps.install`: catalog ids (1 to 8) to install, with their dependencies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppsInstallParams {
    pub ids: Vec<String>,
    /// `settings.get`'s revision.
    pub revision: String,
    /// The terms version the user accepted, per app id, for every app being installed that
    /// has vendor terms (it must equal the catalog's `terms.version`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub accepted_terms: BTreeMap<String, String>,
}

pub struct AppsInstall;
impl Method for AppsInstall {
    const NAME: &'static str = "apps.install";
    type Params = AppsInstallParams;
    type Result = JobRef;
}

/// `apps.remove`: installed catalog ids (1 to 8) to remove.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppsRemoveParams {
    pub ids: Vec<String>,
    pub revision: String,
    /// After the switch, also delete each app's `dataPaths` in every profile's home.
    pub delete_data: bool,
}

pub struct AppsRemove;
impl Method for AppsRemove {
    const NAME: &'static str = "apps.remove";
    type Params = AppsRemoveParams;
    type Result = JobRef;
}

// ---------------------------------------------------------------------------------------------
// Apps per profile

/// `apps.list`: the caller's current app env.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppsListResult {
    /// Stands for the caller's current app generation (64 hex); `apps.install`/`apps.remove`
    /// take it as `revision`.
    pub revision: String,
    /// The Nix profile generation (null: no apps yet, or the guest's single link).
    #[serde(deserialize_with = "nullable")]
    pub generation: Option<u32>,
    /// Catalog ids in the env, sorted.
    pub ids: Vec<String>,
    /// Ids whose vendor terms this profile has not accepted at the catalog's version (apps
    /// from the starter set): the launcher asks on first launch.
    pub needs_terms: Vec<String>,
    /// Box features the env needs.
    #[serde(rename = "box")]
    pub box_features: Vec<String>,
}

pub struct AppsList;
impl Method for AppsList {
    const NAME: &'static str = "apps.list";
    type Params = NoParams;
    type Result = AppsListResult;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppGeneration {
    pub generation: u32,
    /// When it was made (Unix time, seconds).
    pub date: i64,
    pub ids: Vec<String>,
    /// The system revision the env was built against.
    #[serde(deserialize_with = "nullable")]
    pub system_rev: Option<String>,
    pub current: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppsGenerationsResult {
    /// Newest first.
    pub generations: Vec<AppGeneration>,
}

pub struct AppsGenerations;
impl Method for AppsGenerations {
    const NAME: &'static str = "apps.generations";
    type Params = NoParams;
    type Result = AppsGenerationsResult;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppsRollbackParams {
    pub generation: u32,
}

pub struct AppsRollback;
impl Method for AppsRollback {
    const NAME: &'static str = "apps.rollback";
    type Params = AppsRollbackParams;
    type Result = JobRef;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppsAcceptTermsParams {
    pub id: String,
    pub version: String,
}

/// An empty result object.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Empty {}

pub struct AppsAcceptTerms;
impl Method for AppsAcceptTerms {
    const NAME: &'static str = "apps.accept_terms";
    type Params = AppsAcceptTermsParams;
    type Result = Empty;
}

/// `apps.clear_data`: delete a catalog app's `dataPaths` in the caller's home, installed
/// or not (data a remove left behind).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppsClearDataParams {
    pub id: String,
}

pub struct AppsClearData;
impl Method for AppsClearData {
    const NAME: &'static str = "apps.clear_data";
    type Params = AppsClearDataParams;
    type Result = JobRef;
}

// ---------------------------------------------------------------------------------------------
// Profiles (profiles.md)

/// `^p[1-9][0-9]{0,2}$`.
pub fn is_profile_id(s: &str) -> bool {
    let b = s.as_bytes();
    (2..=4).contains(&b.len())
        && b[0] == b'p'
        && (b'1'..=b'9').contains(&b[1])
        && b[2..].iter().all(u8::is_ascii_digit)
}

/// Disk use of one profile's home (`usage.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileUsage {
    pub total_bytes: u64,
    pub steam_games_bytes: u64,
    /// When it was measured (Unix seconds).
    pub at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileInfo {
    pub id: String,
    pub name: String,
    pub color: String,
    pub user: String,
    pub admin: bool,
    pub locked: bool,
    /// The PIN backoff lock (Unix seconds), while it is on.
    #[serde(deserialize_with = "nullable")]
    pub locked_until: Option<i64>,
    /// Its session is the one on screen.
    pub active: bool,
    #[serde(deserialize_with = "nullable")]
    pub last_used_at: Option<i64>,
    #[serde(deserialize_with = "nullable")]
    pub usage: Option<ProfileUsage>,
    /// false: a profile without a PIN yet ("Set your PIN").
    pub pin_set: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestInfo {
    pub enabled: bool,
    #[serde(deserialize_with = "nullable")]
    pub usage: Option<ProfileUsage>,
}

/// Who called.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallerInfo {
    /// The caller's profile (or spare account) id.
    #[serde(deserialize_with = "nullable")]
    pub id: Option<String>,
    pub user: String,
    pub guest: bool,
    /// An account without a profile (first-boot mode creates the admin profile on it).
    pub spare: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfilesListResult {
    pub profiles: Vec<ProfileInfo>,
    pub guest: GuestInfo,
    pub max: u32,
    pub first_boot: bool,
    pub caller: CallerInfo,
}

pub struct ProfilesList;
impl Method for ProfilesList {
    const NAME: &'static str = "profiles.list";
    type Params = NoParams;
    type Result = ProfilesListResult;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilesCreateParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    pub pin: String,
    pub locked: bool,
    /// `settings.get`'s revision (the account is a state.json change).
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfilesCreateResult {
    pub job_id: String,
    pub id: String,
}

pub struct ProfilesCreate;
impl Method for ProfilesCreate {
    const NAME: &'static str = "profiles.create";
    type Params = ProfilesCreateParams;
    type Result = ProfilesCreateResult;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilesSetupAdminParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    pub pin: String,
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileIdResult {
    pub id: String,
}

pub struct ProfilesSetupAdmin;
impl Method for ProfilesSetupAdmin {
    const NAME: &'static str = "profiles.setup_admin";
    type Params = ProfilesSetupAdminParams;
    type Result = ProfileIdResult;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilesRenameParams {
    pub id: String,
    pub name: String,
    /// The admin PIN, for another profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
}

pub struct ProfilesRename;
impl Method for ProfilesRename {
    const NAME: &'static str = "profiles.rename";
    type Params = ProfilesRenameParams;
    type Result = Empty;
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilesSetAppearanceParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_scale: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub high_contrast: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reduce_motion: Option<bool>,
}

pub struct ProfilesSetAppearance;
impl Method for ProfilesSetAppearance {
    const NAME: &'static str = "profiles.set_appearance";
    type Params = ProfilesSetAppearanceParams;
    type Result = Empty;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilesSetPinParams {
    pub pin: String,
    /// Required once a PIN is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_pin: Option<String>,
}

pub struct ProfilesSetPin;
impl Method for ProfilesSetPin {
    const NAME: &'static str = "profiles.set_pin";
    type Params = ProfilesSetPinParams;
    type Result = Empty;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilesSetLockedParams {
    pub locked: bool,
    pub pin: String,
}

pub struct ProfilesSetLocked;
impl Method for ProfilesSetLocked {
    const NAME: &'static str = "profiles.set_locked";
    type Params = ProfilesSetLockedParams;
    type Result = Empty;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilesResetPinParams {
    pub id: String,
    pub new_pin: String,
    /// The admin PIN (not for the admin profile itself: the admin account's password then).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_pin: Option<String>,
}

pub struct ProfilesResetPin;
impl Method for ProfilesResetPin {
    const NAME: &'static str = "profiles.reset_pin";
    type Params = ProfilesResetPinParams;
    type Result = Empty;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilesRemoveParams {
    pub id: String,
    pub confirm_id: String,
    pub pin: String,
    pub revision: String,
}

pub struct ProfilesRemove;
impl Method for ProfilesRemove {
    const NAME: &'static str = "profiles.remove";
    type Params = ProfilesRemoveParams;
    type Result = JobRef;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilesSwitchParams {
    /// A profile id or `guest`.
    pub id: String,
    /// The target's PIN: needed for a locked profile other than the caller's own. Right: the
    /// hint carries a one-time token for the target, so the greeter enters it without asking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
}

pub struct ProfilesSwitch;
impl Method for ProfilesSwitch {
    const NAME: &'static str = "profiles.switch";
    type Params = ProfilesSwitchParams;
    type Result = Empty;
}

/// Drop the caller's switch hint and its token (the switch was cancelled).
pub struct ProfilesSwitchCancel;
impl Method for ProfilesSwitchCancel {
    const NAME: &'static str = "profiles.switch_cancel";
    type Params = NoParams;
    type Result = Empty;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinParams {
    pub pin: String,
}

pub struct ProfilesUnlock;
impl Method for ProfilesUnlock {
    const NAME: &'static str = "profiles.unlock";
    type Params = PinParams;
    type Result = Empty;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestSetParams {
    pub enabled: bool,
    pub admin_pin: String,
}

pub struct GuestSet;
impl Method for GuestSet {
    const NAME: &'static str = "guest.set";
    type Params = GuestSetParams;
    type Result = JobRef;
}

/// End guest session. Only the guest may call it (from its own session); the job ends the
/// guest's session itself (every process of the guest account), then wipes the guest's home,
/// app env, terms acceptances, usage entry and switch hint, and gives it the starter env again.
pub struct GuestEnd;
impl Method for GuestEnd {
    const NAME: &'static str = "guest.end";
    type Params = NoParams;
    type Result = JobRef;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisplaySaveModeParams {
    /// The display as gamescope names it in its mode save file (`<make> <model>`).
    pub display: String,
    pub width: u32,
    pub height: u32,
    pub refresh: u32,
}

pub struct DisplaySaveMode;
impl Method for DisplaySaveMode {
    const NAME: &'static str = "display.save_mode";
    type Params = DisplaySaveModeParams;
    type Result = Empty;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioSaveOutputParams {
    /// A PipeWire node name, `^[A-Za-z0-9._:-]{1,128}$`.
    pub node: String,
}

pub struct AudioSaveOutput;
impl Method for AudioSaveOutput {
    const NAME: &'static str = "audio.save_output";
    type Params = AudioSaveOutputParams;
    type Result = Empty;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StarterSetGetResult {
    pub ids: Vec<String>,
    #[serde(deserialize_with = "nullable")]
    pub updated_at: Option<i64>,
}

pub struct StarterSetGet;
impl Method for StarterSetGet {
    const NAME: &'static str = "starter_set.get";
    type Params = NoParams;
    type Result = StarterSetGetResult;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StarterSetSetParams {
    pub ids: Vec<String>,
    /// The admin PIN.
    pub pin: String,
}

pub struct StarterSetSet;
impl Method for StarterSetSet {
    const NAME: &'static str = "starter_set.set";
    type Params = StarterSetSetParams;
    type Result = JobRef;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfilesChangedParams {}

pub struct ProfilesChanged;
impl Event for ProfilesChanged {
    const NAME: &'static str = "profiles_changed";
    type Params = ProfilesChangedParams;
}

// ---------------------------------------------------------------------------------------------
// Storage (storage.md): what uses the disk, clean-up, automatic clean-up

/// Most generations a clean-up (or the automatic one) may keep.
pub const MAX_KEEP: u32 = 10;

/// What a storage tree node stands for (the launcher names it; the daemon gives no text).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// The whole filesystem (the top-level root).
    Disk,
    /// HinterOS system generations.
    System,
    /// The profiles' app envs.
    Apps,
    /// A profile's home (`id`).
    Profile,
    /// The guest's home.
    Guest,
    /// `/srv`: its total only.
    Media,
    /// What none of the others explains.
    Other,
    /// Free space.
    Free,
    /// One generation (system, or of an app env): what only it holds.
    Generation,
    /// What two or more generations of the parent hold.
    Common,
    /// One owner's app envs (`id`: a profile id, `starter` or `guest`).
    AppEnv,
    /// App store paths in the envs of two or more owners (counted once).
    Shared,
    /// A folder (`name`).
    Dir,
    /// Files directly in the parent folder, and folders too small or deep to show.
    Files,
    /// The installed Steam games of a home.
    Steam,
    /// One Steam game (`name`, `app_id`).
    SteamGame,
    /// A value of a later minor (`#[serde(other)]`). This build never sends it.
    #[serde(other)]
    Unknown,
}

impl NodeKind {
    pub const ALL: &[NodeKind] = &[
        NodeKind::Disk,
        NodeKind::System,
        NodeKind::Apps,
        NodeKind::Profile,
        NodeKind::Guest,
        NodeKind::Media,
        NodeKind::Other,
        NodeKind::Free,
        NodeKind::Generation,
        NodeKind::Common,
        NodeKind::AppEnv,
        NodeKind::Shared,
        NodeKind::Dir,
        NodeKind::Files,
        NodeKind::Steam,
        NodeKind::SteamGame,
    ];
}

/// One segment of the storage ring. Children add up exactly to `bytes` (remainders are nodes
/// of their own). The `disk` root is the filesystem, plus any home or `/srv` on another one. On a
/// compressed filesystem the sizes can be scaled down to what the disk really uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageNode {
    pub kind: NodeKind,
    pub bytes: u64,
    pub children: Vec<StorageNode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub booted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<u32>,
}

impl StorageNode {
    pub fn new(kind: NodeKind, bytes: u64) -> Self {
        Self {
            kind,
            bytes,
            children: Vec::new(),
            id: None,
            name: None,
            generation: None,
            date: None,
            current: None,
            booted: None,
            app_id: None,
        }
    }
}

/// The store's filesystem (statvfs), like df.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskUsage {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
}

/// What one clean-up with `keep` would remove and free now (an estimate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupEstimate {
    pub keep: u32,
    /// Generations it would remove.
    pub generations: u32,
    /// Bytes it would free (garbage included).
    pub bytes: u64,
}

/// For keep 1 to 10 (index `keep - 1`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupEstimates {
    pub system: Vec<CleanupEstimate>,
    pub profiles: Vec<CleanupEstimate>,
}

/// The last measurement, as every profile may see it (no names of files, folders or games).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Measurement {
    /// When it finished (Unix seconds).
    pub measured_at: i64,
    pub disk: DiskUsage,
    /// The top level (`kind: disk`); profile, guest and media nodes without children.
    pub root: StorageNode,
    /// Store paths no GC root reaches now.
    pub garbage_bytes: u64,
    pub estimates: CleanupEstimates,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageGetResult {
    /// Null until the first measurement has finished.
    #[serde(deserialize_with = "nullable")]
    pub measurement: Option<Measurement>,
}

/// `storage.get`: the last measurement (read-only, open to everyone).
pub struct StorageGet;
impl Method for StorageGet {
    const NAME: &'static str = "storage.get";
    type Params = NoParams;
    type Result = StorageGetResult;
}

/// `storage.measure`: start a `storage_measure` job (or get the running one's id).
pub struct StorageMeasure;
impl Method for StorageMeasure {
    const NAME: &'static str = "storage.measure";
    type Params = NoParams;
    type Result = JobRef;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageTreeParams {
    /// A profile id or `guest`.
    pub id: String,
    /// The admin PIN: needed for any tree but the caller's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_pin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageTreeResult {
    pub id: String,
    pub measured_at: i64,
    /// `kind: profile` (with `id`) or `kind: guest`: the home's folders and Steam games.
    pub root: StorageNode,
}

/// `storage.tree`: one home's detailed tree (own: open; another's: admin PIN).
pub struct StorageTree;
impl Method for StorageTree {
    const NAME: &'static str = "storage.tree";
    type Params = StorageTreeParams;
    type Result = StorageTreeResult;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupParams {
    /// Generations to keep, 1 to 10 (the running and booted ones are always kept).
    pub keep: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_pin: Option<String>,
}

/// `storage.cleanup_system`: job `cleanup_system` (admin PIN every time).
pub struct StorageCleanupSystem;
impl Method for StorageCleanupSystem {
    const NAME: &'static str = "storage.cleanup_system";
    type Params = CleanupParams;
    type Result = JobRef;
}

/// `storage.cleanup_profiles`: job `cleanup_profiles` (admin PIN every time).
pub struct StorageCleanupProfiles;
impl Method for StorageCleanupProfiles {
    const NAME: &'static str = "storage.cleanup_profiles";
    type Params = CleanupParams;
    type Result = JobRef;
}

/// How often the automatic clean-up runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupFrequency {
    Daily,
    Weekly,
    Monthly,
}

impl CleanupFrequency {
    pub const ALL: &[CleanupFrequency] = &[
        CleanupFrequency::Daily,
        CleanupFrequency::Weekly,
        CleanupFrequency::Monthly,
    ];
}

/// The automatic clean-up (`state.json` `settings.cleanup`, defaults when absent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupSettings {
    pub enabled: bool,
    pub frequency: CleanupFrequency,
    pub keep_system: u32,
    pub keep_profiles: u32,
}

/// `cleanup.get`: the automatic clean-up setting (read-only).
pub struct CleanupGet;
impl Method for CleanupGet {
    const NAME: &'static str = "cleanup.get";
    type Params = NoParams;
    type Result = CleanupSettings;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupSetParams {
    pub enabled: bool,
    pub frequency: CleanupFrequency,
    pub keep_system: u32,
    pub keep_profiles: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_pin: Option<String>,
}

/// `cleanup.set`: job `cleanup_set` (a rebuild; admin PIN).
pub struct CleanupSet;
impl Method for CleanupSet {
    const NAME: &'static str = "cleanup.set";
    type Params = CleanupSetParams;
    type Result = JobRef;
}

/// `storage_measure` job result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeasuredResult {
    pub measured_at: i64,
}

/// Clean-up job result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanedResult {
    /// Free space after minus before on the store's filesystem (never negative).
    pub freed_bytes: u64,
    pub generations_removed: u32,
}

// ---------------------------------------------------------------------------------------------
// Time zone (time-zone.md)

/// The zone that applies when `state.json` names none.
pub const DEFAULT_TIME_ZONE: &str = "UTC";

/// Longest zone name accepted (the longest IANA name is 32 bytes).
pub const MAX_ZONE_NAME_BYTES: usize = 64;

/// The shape of an IANA zone name (`UTC`, `America/Chicago`), the system repo's rule
/// (`nixos/lib/time-zone.nix`): `/`-separated parts that each start with an ASCII capital letter
/// followed by ASCII letters, digits, `_`, `+` and `-`; at most [`MAX_ZONE_NAME_BYTES`]. This
/// keeps out `..`, absolute paths and tzdata's other files (`posix/`, `right/`, `zone1970.tab`,
/// `posixrules`). Whether the zone exists is a separate question (the system's tzdata).
pub fn is_zone_name(s: &str) -> bool {
    s.len() <= MAX_ZONE_NAME_BYTES
        && s.split('/').all(|part| {
            let b = part.as_bytes();
            !b.is_empty()
                && b[0].is_ascii_uppercase()
                && b[1..]
                    .iter()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'+' | b'-'))
        })
}

/// The box's time zone and its offsets now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeZoneInfo {
    /// An IANA name (`UTC` when `state.json` names none).
    pub zone: String,
    /// Seconds east of UTC now (DST included).
    pub offset_s: i32,
    /// The zone's standard offset now (without DST): the band the map shows.
    pub standard_offset_s: i32,
    /// Unix time of the next offset change; null when there is none.
    #[serde(deserialize_with = "nullable")]
    pub offset_valid_until: Option<i64>,
}

/// A zone in `time_zone.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeZoneEntry {
    pub zone: String,
    pub offset_s: i32,
    pub standard_offset_s: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeZoneListResult {
    /// tzdata's `zone1970.tab` zones and `UTC`, sorted by name.
    pub zones: Vec<TimeZoneEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeZoneSetParams {
    pub zone: String,
    /// Needed except in first-boot mode (no admin profile yet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_pin: Option<String>,
}

/// `time_zone.get`: the box's time zone (read-only).
pub struct TimeZoneGet;
impl Method for TimeZoneGet {
    const NAME: &'static str = "time_zone.get";
    type Params = NoParams;
    type Result = TimeZoneInfo;
}

/// `time_zone.list`: every zone the picker offers (read-only).
pub struct TimeZoneList;
impl Method for TimeZoneList {
    const NAME: &'static str = "time_zone.list";
    type Params = NoParams;
    type Result = TimeZoneListResult;
}

/// `time_zone.set`: at once, no job (admin PIN except in first-boot mode).
pub struct TimeZoneSet;
impl Method for TimeZoneSet {
    const NAME: &'static str = "time_zone.set";
    type Params = TimeZoneSetParams;
    type Result = TimeZoneInfo;
}

/// `time_zone_changed`: the new zone and its offsets.
pub struct TimeZoneChanged;
impl Event for TimeZoneChanged {
    const NAME: &'static str = "time_zone_changed";
    type Params = TimeZoneInfo;
}

// ---------------------------------------------------------------------------------------------
// Keyboard Home

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyboardHomeParams {}

/// `keyboard_home`: Ctrl + Super + D was pressed on a keyboard. Only `hinteros-system
/// keyboard-home` (its own unit) reads keyboards; the service sends this to the connections of
/// the uid owning seat0's active session, never to the greeter socket.
pub struct KeyboardHome;
impl Event for KeyboardHome {
    const NAME: &'static str = "keyboard_home";
    type Params = KeyboardHomeParams;
}

// ---------------------------------------------------------------------------------------------
// Local network filter

/// Longest cgroup path `lan_filter.set` takes.
pub const MAX_CGROUP_PATH_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LanFilterSetParams {
    /// The caller's cgroup for apps without `lan`, as in `/proc/self/cgroup`
    /// (`/user.slice/user-<uid>.slice/user@<uid>.service/...`).
    pub cgroup: String,
}

/// `lan_filter.set`: the processes in the caller's cgroup `cgroup` (and below it) cannot reach
/// the local network (but DNS to the box's resolvers). One cgroup per uid: a new call replaces
/// it. The session daemon's own call, not relayed.
pub struct LanFilterSet;
impl Method for LanFilterSet {
    const NAME: &'static str = "lan_filter.set";
    type Params = LanFilterSetParams;
    type Result = Empty;
}

/// `lan_filter.clear`: the caller's filter is removed (its session ends).
pub struct LanFilterClear;
impl Method for LanFilterClear {
    const NAME: &'static str = "lan_filter.clear";
    type Params = NoParams;
    type Result = Empty;
}

// ---------------------------------------------------------------------------------------------
// Server section (server.md): box-wide services, Jellyfin, Plex, Tailscale, Cloudflare

pub use crate::catalog::Setup;

/// Most services one `services.enable` / `services.disable` may name.
pub const MAX_SERVICE_IDS: usize = 8;

/// A service unit's state, as systemd's `ActiveState` says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitState {
    Active,
    Reloading,
    Inactive,
    Failed,
    Activating,
    Deactivating,
    Maintenance,
    Refreshing,
    /// A value this code does not know, or the unit is not loaded (`inactive` in practice).
    #[serde(other)]
    Unknown,
}

impl UnitState {
    pub const ALL: &[UnitState] = &[
        UnitState::Active,
        UnitState::Reloading,
        UnitState::Inactive,
        UnitState::Failed,
        UnitState::Activating,
        UnitState::Deactivating,
        UnitState::Maintenance,
        UnitState::Refreshing,
        UnitState::Unknown,
    ];

    pub fn from_systemd(s: &str) -> Self {
        serde_json::from_value(serde_json::Value::String(s.to_owned())).unwrap_or(Self::Unknown)
    }
}

/// The vendor terms of a service (Plex), with the absolute path of the text on the box.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceTerms {
    pub title: String,
    pub version: String,
    pub url: String,
    /// The text, next to the running generation's catalog index.
    pub file: String,
}

/// What a Jellyfin library holds, as `services.list` shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryKindShown {
    Movies,
    Shows,
    Music,
    /// Mixed movies and shows (no collection type, or Jellyfin's `mixed`).
    Mixed,
    /// A type made in Jellyfin's own dashboard (books, photos, music videos, ...), or a value of
    /// a later minor.
    #[serde(other)]
    Other,
}

impl LibraryKindShown {
    pub const ALL: &[LibraryKindShown] = &[
        LibraryKindShown::Movies,
        LibraryKindShown::Shows,
        LibraryKindShown::Music,
        LibraryKindShown::Mixed,
        LibraryKindShown::Other,
    ];
}

/// What a new Jellyfin library holds (`jellyfin.libraries.add`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryKind {
    Movies,
    Shows,
    Music,
    Mixed,
}

impl LibraryKind {
    pub const ALL: &[LibraryKind] = &[
        LibraryKind::Movies,
        LibraryKind::Shows,
        LibraryKind::Music,
        LibraryKind::Mixed,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JellyfinLibrary {
    /// Jellyfin's item id (32 lowercase hex digits).
    pub id: String,
    pub name: String,
    pub kind: LibraryKindShown,
    /// The `media.folders` id of its (first) folder; null when it is not in `/srv/media`.
    #[serde(deserialize_with = "nullable")]
    pub folder: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JellyfinUserRef {
    pub profile_id: String,
    /// The profile's Jellyfin user name.
    pub jellyfin_user: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JellyfinDetail {
    /// HinterOS made the first-run setup (admin user, API key).
    pub setup_done: bool,
    /// `http://<local IP>:8096`; null without a local network address.
    #[serde(deserialize_with = "nullable")]
    pub url: Option<String>,
    pub libraries: Vec<JellyfinLibrary>,
    pub users: Vec<JellyfinUserRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlexDetail {
    /// Linked to a Plex account (`plex.claim`).
    pub claimed: bool,
    #[serde(deserialize_with = "nullable")]
    pub server_name: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TailscaleState {
    /// Not signed in (`tailscale.login`).
    NeedsLogin,
    /// Signed in; the tailnet's admin must approve the device.
    NeedsApproval,
    Starting,
    Running,
    Stopped,
    /// tailscaled does not answer.
    Unavailable,
    /// A value of a later minor (`#[serde(other)]`). This build never sends it.
    #[serde(other)]
    Unknown,
}

impl TailscaleState {
    pub const ALL: &[TailscaleState] = &[
        TailscaleState::NeedsLogin,
        TailscaleState::NeedsApproval,
        TailscaleState::Starting,
        TailscaleState::Running,
        TailscaleState::Stopped,
        TailscaleState::Unavailable,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TailscaleDetail {
    pub state: TailscaleState,
    /// The device's name in the tailnet.
    #[serde(deserialize_with = "nullable")]
    pub name: Option<String>,
    /// The tailnet IPv4 address.
    #[serde(deserialize_with = "nullable")]
    pub ip4: Option<String>,
    /// The MagicDNS name (without the trailing dot).
    #[serde(deserialize_with = "nullable")]
    pub dns_name: Option<String>,
    /// The tailnet's name.
    #[serde(deserialize_with = "nullable")]
    pub tailnet: Option<String>,
    /// When the device's key expires (Unix seconds); null: never, or unknown.
    #[serde(deserialize_with = "nullable")]
    pub key_expiry: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudflareDetail {
    pub token_set: bool,
    /// cloudflared has a connection to Cloudflare.
    pub connected: bool,
    /// The box's address on its local network, for the private-network route.
    #[serde(deserialize_with = "nullable")]
    pub local_ip: Option<String>,
}

/// Per-setup detail of an enabled service (`setup` says which).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServiceDetail {
    Jellyfin(JellyfinDetail),
    Plex(PlexDetail),
    Tailscale(TailscaleDetail),
    Cloudflare(CloudflareDetail),
}

/// Where a service's real icon is: the catalog entry's `icon`, as `store.list` shows an app's
/// (HinterOS ships no logos).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceIcon {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
    /// Hex SHA-256 of the `icon_url` file: a download with another one is not kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default)]
    pub firefox_profile: bool,
    /// The resolved icon on this box (an absolute path to an image). Only the session daemon
    /// sets it, in `system.services.list` (the same cache as `store.list`'s `icon.path`);
    /// absent until resolved, and `store_changed` is sent when it appears.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceInfo {
    pub id: String,
    pub name: String,
    /// The catalog offers it (status available).
    pub available: bool,
    /// In `state.json` `services`.
    pub enabled: bool,
    /// The service's unit (enabled services only).
    #[serde(deserialize_with = "nullable")]
    pub status: Option<UnitState>,
    /// The setup flow (null: on/off only).
    #[serde(deserialize_with = "lenient")]
    pub setup: Option<Setup>,
    /// Vendor terms to accept before `services.enable`.
    #[serde(deserialize_with = "nullable")]
    pub terms: Option<ServiceTerms>,
    /// Per-setup detail (enabled services with a setup only).
    #[serde(deserialize_with = "lenient")]
    pub detail: Option<ServiceDetail>,
    /// The catalog entry's icon; absent when the catalog no longer has the entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<ServiceIcon>,
    /// `services.disable {delete_data: true}` has something to delete (the entry has
    /// `stateDirs`; Jellyfin always).
    #[serde(default)]
    pub has_data: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServicesListResult {
    /// The hash of `state.json`, for `services.enable`/`services.disable`.
    pub revision: String,
    pub services: Vec<ServiceInfo>,
}

/// `services.list`: every service the catalog offers, and the enabled ones (read-only).
pub struct ServicesList;
impl Method for ServicesList {
    const NAME: &'static str = "services.list";
    type Params = NoParams;
    type Result = ServicesListResult;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServicesEnableParams {
    pub ids: Vec<String>,
    /// The admin PIN.
    pub pin: String,
    /// `services.list`'s revision.
    pub revision: String,
    /// The terms version accepted, per service id with vendor terms.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub accepted_terms: BTreeMap<String, String>,
}

/// `services.enable`: job `services_enable` (a rebuild).
pub struct ServicesEnable;
impl Method for ServicesEnable {
    const NAME: &'static str = "services.enable";
    type Params = ServicesEnableParams;
    type Result = JobRef;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServicesDisableParams {
    pub ids: Vec<String>,
    pub pin: String,
    pub revision: String,
    /// After the switch, also delete each service's data (`stateDirs`).
    pub delete_data: bool,
}

/// `services.disable`: job `services_disable` (a rebuild).
pub struct ServicesDisable;
impl Method for ServicesDisable {
    const NAME: &'static str = "services.disable";
    type Params = ServicesDisableParams;
    type Result = JobRef;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaFoldersParams {
    /// A folder id from an earlier answer; absent: the top of `/srv/media`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaFolder {
    /// The folder's path relative to `/srv/media`.
    pub id: String,
    pub name: String,
    /// It has folders of its own.
    pub has_children: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaFoldersResult {
    #[serde(deserialize_with = "nullable")]
    pub folder_id: Option<String>,
    pub folders: Vec<MediaFolder>,
}

/// `media.folders`: one level of folders under `/srv/media` (read-only).
pub struct MediaFolders;
impl Method for MediaFolders {
    const NAME: &'static str = "media.folders";
    type Params = MediaFoldersParams;
    type Result = MediaFoldersResult;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JellyfinLibrariesAddParams {
    pub pin: String,
    pub name: String,
    pub kind: LibraryKind,
    /// A `media.folders` id.
    pub folder: String,
}

/// `jellyfin.libraries.add`: job `jellyfin_library_add`.
pub struct JellyfinLibrariesAdd;
impl Method for JellyfinLibrariesAdd {
    const NAME: &'static str = "jellyfin.libraries.add";
    type Params = JellyfinLibrariesAddParams;
    type Result = JobRef;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JellyfinLibrariesRemoveParams {
    pub pin: String,
    pub library_id: String,
}

/// `jellyfin.libraries.remove`: job `jellyfin_library_remove` (the media files stay).
pub struct JellyfinLibrariesRemove;
impl Method for JellyfinLibrariesRemove {
    const NAME: &'static str = "jellyfin.libraries.remove";
    type Params = JellyfinLibrariesRemoveParams;
    type Result = JobRef;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JellyfinAccount {
    pub name: String,
    pub password: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JellyfinProfileAccount {
    pub profile_id: String,
    /// The profile's name now.
    pub profile_name: String,
    pub name: String,
    pub password: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JellyfinUsersRevealResult {
    /// The admin user; null when setup has not written it yet.
    #[serde(deserialize_with = "nullable")]
    pub admin: Option<JellyfinAccount>,
    pub users: Vec<JellyfinProfileAccount>,
}

impl std::fmt::Debug for JellyfinUsersRevealResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Passwords never reach a log.
        f.debug_struct("JellyfinUsersRevealResult")
            .field("admin", &self.admin.as_ref().map(|a| &a.name))
            .field("users", &self.users.len())
            .finish()
    }
}

/// `jellyfin.users.reveal`: the admin's and every profile's Jellyfin password (admin PIN; no
/// job). Makes missing profile users first.
pub struct JellyfinUsersReveal;
impl Method for JellyfinUsersReveal {
    const NAME: &'static str = "jellyfin.users.reveal";
    type Params = PinParams;
    type Result = JellyfinUsersRevealResult;
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlexClaimParams {
    pub pin: String,
    /// From plex.tv/claim: `claim-` and 20 letters, digits, `_` or `-`.
    pub code: String,
}

impl std::fmt::Debug for PlexClaimParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PlexClaimParams { .. }")
    }
}

/// `plex.claim`: job `plex_claim` (links the server to the owner's Plex account).
pub struct PlexClaim;
impl Method for PlexClaim {
    const NAME: &'static str = "plex.claim";
    type Params = PlexClaimParams;
    type Result = JobRef;
}

/// `tailscale.login`: job `tailscale_login` (the sign-in URL comes in `job_progress`).
pub struct TailscaleLogin;
impl Method for TailscaleLogin {
    const NAME: &'static str = "tailscale.login";
    type Params = PinParams;
    type Result = JobRef;
}

/// `tailscale.logout`: job `tailscale_logout`.
pub struct TailscaleLogout;
impl Method for TailscaleLogout {
    const NAME: &'static str = "tailscale.logout";
    type Params = PinParams;
    type Result = JobRef;
}

/// The caller's own Jellyfin session (spike S1: the Jellyfin app on the box signs in by itself).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JellyfinClientTokenResult {
    /// Jellyfin's server id (32 hex digits).
    pub server_id: String,
    /// The profile's Jellyfin user id.
    pub user_id: String,
    /// `hinteros-<profile id>`: the device the token belongs to (one live token per profile).
    pub device_id: String,
    pub access_token: String,
}

impl std::fmt::Debug for JellyfinClientTokenResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JellyfinClientTokenResult")
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

/// `jellyfin.client_token`: a Jellyfin access token for the caller's own profile (no PIN; the
/// guest and other accounts are refused). Made with the profile's password, kept, made again
/// when Jellyfin no longer takes it.
pub struct JellyfinClientToken;
impl Method for JellyfinClientToken {
    const NAME: &'static str = "jellyfin.client_token";
    type Params = NoParams;
    type Result = JellyfinClientTokenResult;
}

/// Longest Cloudflare tunnel token accepted.
pub const MAX_TUNNEL_TOKEN_BYTES: usize = 512;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudflareSetTokenParams {
    pub pin: String,
    /// The tunnel token from the Zero Trust dashboard.
    pub token: String,
}

impl std::fmt::Debug for CloudflareSetTokenParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CloudflareSetTokenParams { .. }")
    }
}

/// `cloudflare.set_token`: job `cloudflare_set_token`.
pub struct CloudflareSetToken;
impl Method for CloudflareSetToken {
    const NAME: &'static str = "cloudflare.set_token";
    type Params = CloudflareSetTokenParams;
    type Result = JobRef;
}

/// `cloudflare.clear_token`: job `cloudflare_clear_token`.
pub struct CloudflareClearToken;
impl Method for CloudflareClearToken {
    const NAME: &'static str = "cloudflare.clear_token";
    type Params = PinParams;
    type Result = JobRef;
}

/// The library a Jellyfin job is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectLibrary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<LibraryKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Jobs

/// Result of a job that changed the system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedResult {
    /// Generation after the job.
    #[serde(deserialize_with = "nullable")]
    pub generation: Option<u32>,
    /// Locked revision after the job.
    #[serde(deserialize_with = "nullable")]
    pub rev: Option<String>,
    /// App envs the final `apps` stage could not rebuild (they keep their previous
    /// generation, which still works). Absent when every env was rebuilt or there was none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps_not_updated: Option<Vec<AppNotUpdated>>,
    /// Box features a rollback turned back on because the profiles' apps need them and the
    /// restored generation lacked them (a `box` stage after the rollback). Absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features_readded: Option<Vec<String>>,
}

/// An owner's env that the `apps` stage left at its old generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppNotUpdated {
    /// A profile id, `starter` or `guest`.
    pub owner: String,
    /// The ids of the env that was not rebuilt.
    pub ids: Vec<String>,
    /// What failed (a log line).
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JobResult {
    UpdateCheck(UpdateCheckResult),
    Applied(AppliedResult),
    /// `storage_measure`.
    Measured(MeasuredResult),
    /// `cleanup_system`, `cleanup_profiles`.
    Cleaned(CleanedResult),
    /// `channel_check`.
    ChannelCheck(ChannelCheckResult),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub job_id: String,
    pub kind: JobKind,
    /// Who started it.
    pub uid: u32,
    pub state: JobState,
    /// Current (running) or last reached stage.
    #[serde(deserialize_with = "nullable")]
    pub stage: Option<Stage>,
    pub created_at: i64,
    #[serde(deserialize_with = "nullable")]
    pub finished_at: Option<i64>,
    #[serde(deserialize_with = "nullable")]
    pub error: Option<ErrorObject>,
    #[serde(deserialize_with = "lenient")]
    pub result: Option<JobResult>,
    pub session_restart_required: bool,
    /// What the job is about.
    pub subject: JobSubject,
    /// When the job's process started (`created_at` until it has).
    pub started_at: i64,
    /// The latest percent (running jobs; null when finished).
    #[serde(deserialize_with = "nullable")]
    pub percent: Option<u8>,
    /// The latest progress detail (running jobs; null when finished).
    #[serde(deserialize_with = "nullable")]
    pub detail: Option<ProgressDetail>,
    /// The last time the job's process produced output (running jobs).
    #[serde(deserialize_with = "nullable")]
    pub last_activity_at: Option<i64>,
}

/// A profile a job is for, with its display name when the job started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectProfile {
    pub id: String,
    pub name: String,
}

/// What a job is about. Every field is optional; which ones each kind sets is in the
/// README ("What a job is about").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSubject {
    /// Apps jobs: whose apps (a profile id, `guest` or `starter`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// The profile the job is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<SubjectProfile>,
    /// Apps installed (the new ones), removed or the new starter set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps: Option<Vec<CatalogApp>>,
    /// Apps remove: their data is deleted too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_data: Option<bool>,
    /// An app env copied from another owner (`starter`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_owner: Option<String>,
    /// Rollbacks: the target generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u32>,
    /// Rollbacks: the generation before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_generation: Option<u32>,
    /// Update and channel switch: the target revision (a channel switch: once verified).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// The target's release version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The (target) channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<Channel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_channel: Option<Channel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_rev: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<Base>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_base: Option<Base>,
    /// The new on/off value (ssh, guest, automatic clean-up).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Clean-ups: generations kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency: Option<CleanupFrequency>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_system: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_profiles: Option<u32>,
    /// Services turned on or off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub services: Option<Vec<CatalogApp>>,
    /// The Jellyfin library added or removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library: Option<SubjectLibrary>,
}

impl JobSubject {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobIdParams {
    pub job_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobsGetResult {
    pub job: Job,
}

pub struct JobsGet;
impl Method for JobsGet {
    const NAME: &'static str = "jobs.get";
    type Params = JobIdParams;
    type Result = JobsGetResult;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobsListResult {
    /// The running job (if any) and the last finished ones, newest first.
    pub jobs: Vec<Job>,
}

pub struct JobsList;
impl Method for JobsList {
    const NAME: &'static str = "jobs.list";
    type Params = NoParams;
    type Result = JobsListResult;
}

pub struct JobsCancel;
impl Method for JobsCancel {
    const NAME: &'static str = "jobs.cancel";
    type Params = JobIdParams;
    type Result = JobRef;
}

// ---------------------------------------------------------------------------------------------
// Events

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStartedParams {
    pub job_id: String,
    pub kind: JobKind,
    pub uid: u32,
    pub subject: JobSubject,
    pub started_at: i64,
}

pub struct JobStarted;
impl Event for JobStarted {
    const NAME: &'static str = "job_started";
    type Params = JobStartedParams;
}

/// Nix progress, where known.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressDetail {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths_done: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_done: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_total: Option<u64>,
    /// Derivations built so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builds_done: Option<u64>,
    /// Derivations nix expects to build (may grow).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builds_total: Option<u64>,
    /// Derivations building at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builds_running: Option<u64>,
    /// The most recently started derivation that is still building.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub building: Option<Building>,
    /// `tailscale_login`, stage `signing_in`: the URL to sign in at (the launcher shows it
    /// as a QR code).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_url: Option<String>,
}

/// A derivation being built.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Building {
    /// From the `.drv` name, e.g. `hinteros-daemon`.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Nix's build phase without `Phase` (`unpack`, `configure`, `build`, `install`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// How far this derivation is (0..=99), from the last standard stdenv phase it
    /// started; absent before the first one and for builders without phases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<u8>,
}

impl ProgressDetail {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobProgressParams {
    pub job_id: String,
    pub kind: JobKind,
    pub stage: Stage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<ProgressDetail>,
    pub started_at: i64,
    /// The last time the job's process produced output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<i64>,
    /// The job's subject, only when it changed during the job (replaces the old one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<JobSubject>,
}

pub struct JobProgress;
impl Event for JobProgress {
    const NAME: &'static str = "job_progress";
    type Params = JobProgressParams;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobFinishedParams {
    pub job_id: String,
    pub kind: JobKind,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub result: Option<JobResult>,
    /// The switch replaced the system service or the session daemon: offer a session restart.
    pub session_restart_required: bool,
    pub subject: JobSubject,
    pub started_at: i64,
    pub finished_at: i64,
}

pub struct JobFinished;
impl Event for JobFinished {
    const NAME: &'static str = "job_finished";
    type Params = JobFinishedParams;
}

pub static API: ApiInfo = ApiInfo {
    api: "system",
    version: VERSION,
    methods: &[
        MethodInfo::of::<Hello>(),
        MethodInfo::of::<Ping>(),
        MethodInfo::of::<Subscribe>(),
        MethodInfo::of::<Unsubscribe>(),
        MethodInfo::of::<SystemInfo>(),
        MethodInfo::of::<SettingsGet>(),
        MethodInfo::of::<SettingsSet>(),
        MethodInfo::of::<SshSet>(),
        MethodInfo::of::<GenerationsList>(),
        MethodInfo::of::<Rebuild>(),
        MethodInfo::of::<HardwareRedetect>(),
        MethodInfo::of::<Rollback>(),
        MethodInfo::of::<UpdateCheck>(),
        MethodInfo::of::<UpdateApply>(),
        MethodInfo::of::<ChannelCheck>(),
        MethodInfo::of::<ChannelSet>(),
        MethodInfo::of::<BaseSet>(),
        MethodInfo::of::<AppsInstall>(),
        MethodInfo::of::<AppsRemove>(),
        MethodInfo::of::<AppsList>(),
        MethodInfo::of::<AppsGenerations>(),
        MethodInfo::of::<AppsRollback>(),
        MethodInfo::of::<AppsAcceptTerms>(),
        MethodInfo::of::<AppsClearData>(),
        MethodInfo::of::<ProfilesList>(),
        MethodInfo::of::<ProfilesCreate>(),
        MethodInfo::of::<ProfilesSetupAdmin>(),
        MethodInfo::of::<ProfilesRename>(),
        MethodInfo::of::<ProfilesSetAppearance>(),
        MethodInfo::of::<ProfilesSetPin>(),
        MethodInfo::of::<ProfilesSetLocked>(),
        MethodInfo::of::<ProfilesResetPin>(),
        MethodInfo::of::<ProfilesRemove>(),
        MethodInfo::of::<ProfilesSwitch>(),
        MethodInfo::of::<ProfilesSwitchCancel>(),
        MethodInfo::of::<ProfilesUnlock>(),
        MethodInfo::of::<GuestSet>(),
        MethodInfo::of::<GuestEnd>(),
        MethodInfo::of::<DisplaySaveMode>(),
        MethodInfo::of::<AudioSaveOutput>(),
        MethodInfo::of::<StarterSetGet>(),
        MethodInfo::of::<StarterSetSet>(),
        MethodInfo::of::<StorageGet>(),
        MethodInfo::of::<StorageMeasure>(),
        MethodInfo::of::<StorageTree>(),
        MethodInfo::of::<StorageCleanupSystem>(),
        MethodInfo::of::<StorageCleanupProfiles>(),
        MethodInfo::of::<CleanupGet>(),
        MethodInfo::of::<CleanupSet>(),
        MethodInfo::of::<TimeZoneGet>(),
        MethodInfo::of::<TimeZoneList>(),
        MethodInfo::of::<TimeZoneSet>(),
        MethodInfo::of::<LanFilterSet>(),
        MethodInfo::of::<LanFilterClear>(),
        MethodInfo::of::<ServicesList>(),
        MethodInfo::of::<ServicesEnable>(),
        MethodInfo::of::<ServicesDisable>(),
        MethodInfo::of::<MediaFolders>(),
        MethodInfo::of::<JellyfinLibrariesAdd>(),
        MethodInfo::of::<JellyfinLibrariesRemove>(),
        MethodInfo::of::<JellyfinUsersReveal>(),
        MethodInfo::of::<JellyfinClientToken>(),
        MethodInfo::of::<PlexClaim>(),
        MethodInfo::of::<TailscaleLogin>(),
        MethodInfo::of::<TailscaleLogout>(),
        MethodInfo::of::<CloudflareSetToken>(),
        MethodInfo::of::<CloudflareClearToken>(),
        MethodInfo::of::<JobsGet>(),
        MethodInfo::of::<JobsList>(),
        MethodInfo::of::<JobsCancel>(),
    ],
    events: &[
        EventInfo::of::<JobStarted>(),
        EventInfo::of::<JobProgress>(),
        EventInfo::of::<JobFinished>(),
        EventInfo::of::<ProfilesChanged>(),
        EventInfo::of::<TimeZoneChanged>(),
        EventInfo::of::<KeyboardHome>(),
    ],
    errors: ErrorCode::SYSTEM,
};

/// Methods that change the system (seat check, polkit, rate limit, audit). Everything else is
/// read-only.
pub const MUTATING: &[&str] = &[
    SettingsSet::NAME,
    SshSet::NAME,
    Rebuild::NAME,
    HardwareRedetect::NAME,
    Rollback::NAME,
    UpdateCheck::NAME,
    UpdateApply::NAME,
    ChannelCheck::NAME,
    ChannelSet::NAME,
    BaseSet::NAME,
    AppsInstall::NAME,
    AppsRemove::NAME,
    AppsRollback::NAME,
    AppsAcceptTerms::NAME,
    AppsClearData::NAME,
    ProfilesCreate::NAME,
    ProfilesSetupAdmin::NAME,
    ProfilesRename::NAME,
    ProfilesSetAppearance::NAME,
    ProfilesSetPin::NAME,
    ProfilesSetLocked::NAME,
    ProfilesResetPin::NAME,
    ProfilesRemove::NAME,
    ProfilesSwitch::NAME,
    ProfilesSwitchCancel::NAME,
    ProfilesUnlock::NAME,
    GuestSet::NAME,
    GuestEnd::NAME,
    DisplaySaveMode::NAME,
    AudioSaveOutput::NAME,
    StarterSetSet::NAME,
    StorageMeasure::NAME,
    StorageTree::NAME,
    StorageCleanupSystem::NAME,
    StorageCleanupProfiles::NAME,
    CleanupSet::NAME,
    TimeZoneSet::NAME,
    ServicesEnable::NAME,
    ServicesDisable::NAME,
    JellyfinLibrariesAdd::NAME,
    JellyfinLibrariesRemove::NAME,
    JellyfinUsersReveal::NAME,
    PlexClaim::NAME,
    TailscaleLogin::NAME,
    TailscaleLogout::NAME,
    CloudflareSetToken::NAME,
    CloudflareClearToken::NAME,
    JobsCancel::NAME,
];

/// What `greeter.sock` serves (besides `hello`): the greeter's own needs only.
pub const GREETER_METHODS: &[&str] = &[
    crate::common::Ping::NAME,
    // For profiles.create's revision.
    SettingsGet::NAME,
    Subscribe::NAME,
    Unsubscribe::NAME,
    JobsGet::NAME,
    ProfilesCreate::NAME,
];

/// Methods whose params carry PINs (never logged; the audit summary redacts them).
pub const PIN_FIELDS: &[&str] = &["pin", "current_pin", "new_pin", "admin_pin"];

/// Other secret params (a Plex claim code, a Cloudflare tunnel token): never logged either.
pub const SECRET_FIELDS: &[&str] = &["code", "token"];

/// The polkit action that guards a mutating method.
pub fn polkit_action(method: &str) -> Option<&'static str> {
    Some(match method {
        SettingsSet::NAME => "org.hinteros.system.settings",
        SshSet::NAME => "org.hinteros.system.ssh",
        Rebuild::NAME | HardwareRedetect::NAME => "org.hinteros.system.rebuild",
        // A check like update.check (no PIN), so the policy file needs no change.
        UpdateCheck::NAME | ChannelCheck::NAME => "org.hinteros.system.update.check",
        UpdateApply::NAME => "org.hinteros.system.update.apply",
        Rollback::NAME => "org.hinteros.system.rollback",
        ChannelSet::NAME | BaseSet::NAME => "org.hinteros.system.channel",
        JobsCancel::NAME => "org.hinteros.system.jobs.cancel",
        AppsInstall::NAME
        | AppsRemove::NAME
        | AppsRollback::NAME
        | AppsAcceptTerms::NAME
        | AppsClearData::NAME => "org.hinteros.system.apps",
        ProfilesCreate::NAME => "org.hinteros.system.profiles.create",
        ProfilesSetupAdmin::NAME
        | ProfilesRename::NAME
        | ProfilesSetAppearance::NAME
        | ProfilesSetPin::NAME
        | ProfilesSetLocked::NAME
        | ProfilesResetPin::NAME
        | ProfilesRemove::NAME
        | ProfilesSwitch::NAME
        | ProfilesSwitchCancel::NAME
        | ProfilesUnlock::NAME
        | GuestEnd::NAME => "org.hinteros.system.profiles",
        GuestSet::NAME | StarterSetSet::NAME => "org.hinteros.system.starter",
        DisplaySaveMode::NAME | AudioSaveOutput::NAME => "org.hinteros.system.settings",
        // Existing actions, so the policy file needs no change.
        StorageMeasure::NAME | StorageTree::NAME | CleanupSet::NAME => {
            "org.hinteros.system.settings"
        }
        StorageCleanupSystem::NAME | StorageCleanupProfiles::NAME => "org.hinteros.system.rebuild",
        // An existing action, so the policy file needs no change.
        TimeZoneSet::NAME => "org.hinteros.system.settings",
        // The admin PIN is checked by the service.
        ServicesEnable::NAME
        | ServicesDisable::NAME
        | JellyfinLibrariesAdd::NAME
        | JellyfinLibrariesRemove::NAME
        | JellyfinUsersReveal::NAME
        | PlexClaim::NAME
        | TailscaleLogin::NAME
        | TailscaleLogout::NAME
        | CloudflareSetToken::NAME
        | CloudflareClearToken::NAME => "org.hinteros.system.services",
        _ => return None,
    })
}

/// Every polkit action id the service checks (for the `.policy` file and its tests).
pub const POLKIT_ACTIONS: &[&str] = &[
    "org.hinteros.system.settings",
    "org.hinteros.system.ssh",
    "org.hinteros.system.rebuild",
    "org.hinteros.system.update.check",
    "org.hinteros.system.update.apply",
    "org.hinteros.system.rollback",
    "org.hinteros.system.channel",
    "org.hinteros.system.jobs.cancel",
    "org.hinteros.system.apps",
    "org.hinteros.system.profiles",
    "org.hinteros.system.profiles.create",
    "org.hinteros.system.profiles.recover",
    "org.hinteros.system.starter",
    "org.hinteros.system.services",
];

/// Actions the service checks with polkit's `AllowUserInteraction`: their default is
/// `auth_admin`, and polkit asks the caller's authentication agent (the session daemon's) for
/// the admin password while the call waits. Every other action is checked without interaction.
pub const INTERACTIVE_ACTIONS: &[&str] = &[
    "org.hinteros.system.ssh",
    "org.hinteros.system.profiles.recover",
];

/// Whether a call may wait for the admin password: its action is interactive, or it checks one
/// (`profiles.reset_pin` of the admin profile checks `profiles.recover`).
pub fn may_interact(method: &str) -> bool {
    method == ProfilesResetPin::NAME
        || polkit_action(method).is_some_and(|a| INTERACTIVE_ACTIONS.contains(&a))
}
