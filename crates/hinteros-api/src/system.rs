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

pub const VERSION: ApiVersion = ApiVersion::new(0, 4);

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
    Evaluating,
    Downloading,
    Building,
    Activating,
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
        Stage::Evaluating,
        Stage::Downloading,
        Stage::Building,
        Stage::Activating,
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
}

impl EventName {
    pub const ALL: &[EventName] = &[
        EventName::JobStarted,
        EventName::JobProgress,
        EventName::JobFinished,
    ];

    pub const fn wire_name(self) -> &'static str {
        match self {
            EventName::JobStarted => JobStarted::NAME,
            EventName::JobProgress => JobProgress::NAME,
            EventName::JobFinished => JobFinished::NAME,
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

pub struct HardwareRedetect;
impl Method for HardwareRedetect {
    const NAME: &'static str = "hardware.redetect";
    type Params = NoParams;
    type Result = JobRef;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u32>,
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
}

impl ProgressDetail {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobProgressParams {
    pub job_id: String,
    pub stage: Stage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<ProgressDetail>,
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
        MethodInfo::of::<JobsGet>(),
        MethodInfo::of::<JobsList>(),
        MethodInfo::of::<JobsCancel>(),
    ],
    events: &[
        EventInfo::of::<JobStarted>(),
        EventInfo::of::<JobProgress>(),
        EventInfo::of::<JobFinished>(),
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
    JobsCancel::NAME,
];

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
];

/// Actions the service checks with polkit's `AllowUserInteraction`: their default is
/// `auth_admin`, and polkit asks the caller's authentication agent (the session daemon's) for
/// the admin password while the call waits. Every other action is checked without interaction.
pub const INTERACTIVE_ACTIONS: &[&str] = &["org.hinteros.system.ssh"];
