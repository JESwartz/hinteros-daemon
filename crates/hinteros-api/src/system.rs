//! The internal system API (`schema/system-api.json`): spoken between the session daemon (and
//! any other process of a `hinteros` user) and the privileged system service. See
//! `hinteros-docs/design/system-api.md`.
//!
//! Every params type here rejects unknown fields: the service is a root process and its inputs
//! are closed (a forged `uid` or a stray `path` is an error, not ignored).

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};

use crate::common::{Hello, Ping};
use crate::{ApiInfo, ApiVersion, ErrorCode, ErrorObject, Event, EventInfo, Method, MethodInfo};

pub const VERSION: ApiVersion = ApiVersion::new(0, 8);

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
    /// 0.4: a profile's apps back to an earlier generation.
    AppsRollback,
    /// 0.4: a new starter env.
    StarterSet,
    /// 0.4: the one-time move from box-wide apps (state.json schema 1) to per-profile apps.
    MigrateApps,
    /// 0.4: a new account (a rebuild).
    ProfilesCreate,
    /// 0.4: an account and its home removed (a rebuild).
    ProfilesRemove,
    /// 0.4: the guest account on or off (a rebuild).
    GuestSet,
    /// 0.5: the guest session ended and the guest wiped now (home, app env, sign-ins).
    GuestEnd,
    /// 0.6: measure what uses the disk (`storage.measure`).
    StorageMeasure,
    /// 0.6: old system generations removed, then garbage collected.
    CleanupSystem,
    /// 0.6: old app-env generations of every profile removed, then garbage collected.
    CleanupProfiles,
    /// 0.6: the automatic clean-up setting changed (a rebuild).
    CleanupSet,
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
        JobKind::MigrateApps,
        JobKind::ProfilesCreate,
        JobKind::ProfilesRemove,
        JobKind::GuestSet,
        JobKind::GuestEnd,
        JobKind::StorageMeasure,
        JobKind::CleanupSystem,
        JobKind::CleanupProfiles,
        JobKind::CleanupSet,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Succeeded,
    Failed,
    Cancelled,
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
    /// 0.4: the system rebuild that turns on box features an app needs (nix progress still
    /// attached).
    Box,
    /// 0.6: measuring the disk (`storage_measure`; with `percent`).
    Measuring,
    Evaluating,
    Downloading,
    Building,
    Activating,
    /// 0.6: removing old generations (clean-ups; no longer cancellable).
    Cleaning,
    /// 0.6: collecting garbage (clean-ups).
    Collecting,
    /// 0.4: after a system switch, every profile's app env is rebuilt against the new system.
    Apps,
    Finishing,
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
    /// 0.4: profiles changed (a job, a rename, a PIN, lock or appearance change): re-query.
    ProfilesChanged,
    /// 0.8: the box's time zone changed (`time_zone.set`, or a rollback restored another).
    TimeZoneChanged,
}

impl EventName {
    pub const ALL: &[EventName] = &[
        EventName::JobStarted,
        EventName::JobProgress,
        EventName::JobFinished,
        EventName::ProfilesChanged,
        EventName::TimeZoneChanged,
    ];

    pub const fn wire_name(self) -> &'static str {
        match self {
            EventName::JobStarted => JobStarted::NAME,
            EventName::JobProgress => JobProgress::NAME,
            EventName::JobFinished => JobFinished::NAME,
            EventName::ProfilesChanged => ProfilesChanged::NAME,
            EventName::TimeZoneChanged => TimeZoneChanged::NAME,
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
    /// Schema-1 boxes: catalog app ids installed box-wide from the Store (`state.json` `apps`),
    /// in install order. Absent on schema-2 boxes (0.4: apps are per profile, `apps.list`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps: Option<Vec<String>>,
    /// Schema-2 boxes (0.4): the box features that are on (read-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirements: Option<Vec<String>>,
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
    /// The accounts in that generation's `state.json` (0.4); `None` when it cannot be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profiles: Option<Vec<GenerationProfile>>,
    /// 0.5: box features a rollback to this generation would turn back on (the profiles' apps
    /// need them, its `state.json` lacks them). Absent when none, or unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readds_features: Option<Vec<String>>,
}

/// A profile of a generation's `state.json`, with its current name (`None`: not a profile now).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
/// typed field here (bool, enum, bounded integer), never a free-form value. Empty in 0.2: SSH
/// has its own method and base/channel theirs.
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

/// 0.5: the admin profile's PIN guards the box-wide actions (`hardware.redetect`, `rollback`,
/// `update.apply`, `channel.set`, `base.set`): `pin_required {pin: admin}` without it or when it is wrong, on a
/// schema-2 box whose caller is not root.
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
    /// 0.5: the admin profile's PIN.
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
#[serde(deny_unknown_fields)]
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
    /// 0.5: the admin profile's PIN (installing a system update is box-wide: needed from every
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelSetParams {
    pub channel: Channel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledge_downgrade: Option<bool>,
    /// 0.5: the admin profile's PIN.
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
    /// 0.5: the admin profile's PIN.
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
// Apps per profile (0.4)

/// `apps.list`: the caller's current app env.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppsListResult {
    /// Stands for the caller's current app generation (64 hex); `apps.install`/`apps.remove`
    /// take it as `revision` on schema-2 boxes.
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
#[serde(deny_unknown_fields)]
pub struct Empty {}

pub struct AppsAcceptTerms;
impl Method for AppsAcceptTerms {
    const NAME: &'static str = "apps.accept_terms";
    type Params = AppsAcceptTermsParams;
    type Result = Empty;
}

// ---------------------------------------------------------------------------------------------
// Profiles (0.4, profiles.md)

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
    /// false: a migrated profile without a PIN yet ("Set your PIN").
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

/// 0.5: End guest session. Only the guest may call it (from its own session); the job ends the
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
// Storage (0.6, storage.md): what uses the disk, clean-up, automatic clean-up

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
}

impl NodeKind {
    pub const ALL: &[NodeKind] = &[
        NodeKind::Disk,
        NodeKind::System,
        NodeKind::Apps,
        NodeKind::Profile,
        NodeKind::Guest,
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
/// of their own), except at the `disk` root when the measured parts exceed the filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct DiskUsage {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
}

/// What one clean-up with `keep` would remove and free now (an estimate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupEstimate {
    pub keep: u32,
    /// Generations it would remove.
    pub generations: u32,
    /// Bytes it would free (garbage included).
    pub bytes: u64,
}

/// For keep 1 to 10 (index `keep - 1`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupEstimates {
    pub system: Vec<CleanupEstimate>,
    pub profiles: Vec<CleanupEstimate>,
}

/// The last measurement, as every profile may see it (no names of files, folders or games).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measurement {
    /// When it finished (Unix seconds).
    pub measured_at: i64,
    pub disk: DiskUsage,
    /// The top level (`kind: disk`); profile and guest nodes without children.
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
#[serde(deny_unknown_fields)]
pub struct CleanupSettings {
    pub enable: bool,
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
    pub enable: bool,
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
#[serde(deny_unknown_fields)]
pub struct MeasuredResult {
    pub measured_at: i64,
}

/// Clean-up job result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanedResult {
    /// Free space after minus before on the store's filesystem (never negative).
    pub freed_bytes: u64,
    pub generations_removed: u32,
}

// ---------------------------------------------------------------------------------------------
// Time zone (0.8, time-zone.md)

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
#[serde(deny_unknown_fields)]
pub struct TimeZoneInfo {
    /// An IANA name (`UTC` when `state.json` names none).
    pub zone: String,
    /// Seconds east of UTC now (DST included).
    pub offset_seconds: i32,
    /// The zone's standard offset now (without DST): the band the map shows.
    pub standard_offset_seconds: i32,
    /// Unix time of the next offset change; null when there is none.
    #[serde(deserialize_with = "nullable")]
    pub offset_valid_until: Option<i64>,
}

/// A zone in `time_zone.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeZoneEntry {
    pub zone: String,
    pub offset_seconds: i32,
    pub standard_offset_seconds: i32,
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
// Jobs

/// Result of a job that changed the system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppliedResult {
    /// Generation after the job.
    #[serde(deserialize_with = "nullable")]
    pub generation: Option<u32>,
    /// Locked revision after the job.
    #[serde(deserialize_with = "nullable")]
    pub rev: Option<String>,
    /// 0.4: app envs the final `apps` stage could not rebuild (they keep their previous
    /// generation, which still works). Absent when every env was rebuilt or there was none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps_not_updated: Option<Vec<AppNotUpdated>>,
    /// 0.5: box features a rollback turned back on because the profiles' apps need them and the
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
    /// 0.6: `storage_measure`.
    Measured(MeasuredResult),
    /// 0.6: `cleanup_system`, `cleanup_profiles`.
    Cleaned(CleanedResult),
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
    #[serde(deserialize_with = "nullable")]
    pub result: Option<JobResult>,
    pub session_restart_required: bool,
    /// 0.7: what the job is about.
    pub subject: JobSubject,
    /// 0.7: when the job's process started (`created_at` until it has).
    pub started_at: i64,
    /// 0.7: the latest percent (running jobs; null when finished).
    #[serde(deserialize_with = "nullable")]
    pub percent: Option<u8>,
    /// 0.7: the latest progress detail (running jobs; null when finished).
    #[serde(deserialize_with = "nullable")]
    pub detail: Option<ProgressDetail>,
    /// 0.7: the last time the job's process produced output (running jobs).
    #[serde(deserialize_with = "nullable")]
    pub last_activity_at: Option<i64>,
}

/// 0.7: a profile a job is for, with its display name when the job started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectProfile {
    pub id: String,
    pub name: String,
}

/// 0.7: what a job is about. Every field is optional; which ones each kind sets is in the
/// README ("What a job is about").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSubject {
    /// Apps jobs: whose apps (a profile id, `guest` or `starter`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// The profile the job is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<SubjectProfile>,
    /// Apps installed (the new ones), removed, the new starter set or the migrated apps.
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
    /// 0.7.
    pub subject: JobSubject,
    /// 0.7.
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
    /// 0.7: derivations built so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builds_done: Option<u64>,
    /// 0.7: derivations nix expects to build (may grow).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builds_total: Option<u64>,
    /// 0.7: derivations building at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builds_running: Option<u64>,
    /// 0.7: the most recently started derivation that is still building.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub building: Option<Building>,
}

/// 0.7: a derivation being built.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Building {
    /// From the `.drv` name, e.g. `hinteros-daemon`.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Nix's build phase without `Phase` (`unpack`, `configure`, `build`, `install`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
}

impl ProgressDetail {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobProgressParams {
    pub job_id: String,
    /// 0.7.
    pub kind: JobKind,
    pub stage: Stage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<ProgressDetail>,
    /// 0.7.
    pub started_at: i64,
    /// 0.7: the last time the job's process produced output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<i64>,
    /// 0.7: the job's subject, only when it changed during the job (replaces the old one).
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<JobResult>,
    /// The switch replaced the system service or the session daemon: offer a session restart.
    pub session_restart_required: bool,
    /// 0.7.
    pub subject: JobSubject,
    /// 0.7.
    pub started_at: i64,
    /// 0.7.
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
        MethodInfo::of::<ChannelSet>(),
        MethodInfo::of::<BaseSet>(),
        MethodInfo::of::<AppsInstall>(),
        MethodInfo::of::<AppsRemove>(),
        MethodInfo::of::<AppsList>(),
        MethodInfo::of::<AppsGenerations>(),
        MethodInfo::of::<AppsRollback>(),
        MethodInfo::of::<AppsAcceptTerms>(),
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
    ChannelSet::NAME,
    BaseSet::NAME,
    AppsInstall::NAME,
    AppsRemove::NAME,
    AppsRollback::NAME,
    AppsAcceptTerms::NAME,
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

/// The polkit action that guards a mutating method.
pub fn polkit_action(method: &str) -> Option<&'static str> {
    Some(match method {
        SettingsSet::NAME => "org.hinteros.system.settings",
        SshSet::NAME => "org.hinteros.system.ssh",
        Rebuild::NAME | HardwareRedetect::NAME => "org.hinteros.system.rebuild",
        UpdateCheck::NAME => "org.hinteros.system.update.check",
        UpdateApply::NAME => "org.hinteros.system.update.apply",
        Rollback::NAME => "org.hinteros.system.rollback",
        ChannelSet::NAME | BaseSet::NAME => "org.hinteros.system.channel",
        JobsCancel::NAME => "org.hinteros.system.jobs.cancel",
        AppsInstall::NAME | AppsRemove::NAME | AppsRollback::NAME | AppsAcceptTerms::NAME => {
            "org.hinteros.system.apps"
        }
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
        // 0.6: existing actions, so the policy file needs no change.
        StorageMeasure::NAME | StorageTree::NAME | CleanupSet::NAME => {
            "org.hinteros.system.settings"
        }
        StorageCleanupSystem::NAME | StorageCleanupProfiles::NAME => "org.hinteros.system.rebuild",
        // 0.8: an existing action, so the policy file needs no change.
        TimeZoneSet::NAME => "org.hinteros.system.settings",
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
