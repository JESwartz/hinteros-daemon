//! What the service and a job unit share: the job spec (written by the service into
//! `<run-dir>/jobs/<id>/spec.json`, root-only), the progress lines and the final status.
//!
//! Files of a job directory:
//! - `spec.json`: what to do (from the service's own typed state, never client JSON);
//! - `lock`: held (flock) by the job process while it runs;
//! - `pid`: written once the job holds the lock;
//! - `progress.ndjson`: one [`ProgressLine`] per line;
//! - `cancel`: created by `jobs.cancel`; `activating`: created by the job before activation
//!   (after that, cancelling is refused);
//! - `status.json`: the outcome, written last.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use hinteros_api::system::{
    Channel, JobKind, JobResult, JobState, JobSubject, ProgressDetail, Stage, is_job_id,
};
use hinteros_api::{ErrorCode, ErrorObject};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::appreg::{Owner, StarterFile};
use crate::config::{Paths, Tools};
use crate::state::State;

pub const SPEC: &str = "spec.json";
pub const LOCK: &str = "lock";
pub const PID: &str = "pid";
pub const PROGRESS: &str = "progress.ndjson";
pub const STATUS: &str = "status.json";
pub const CANCEL: &str = "cancel";
pub const ACTIVATING: &str = "activating";

/// The systemd unit a job runs in.
pub fn unit_name(id: &str) -> String {
    format!("hinteros-job@{id}.service")
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// A fresh job id: 16 hex digits from the kernel's random pool.
pub fn new_id() -> String {
    let mut bytes = [0u8; 8];
    rustix::rand::getrandom(&mut bytes, rustix::rand::GetRandomFlags::empty()).expect("getrandom");
    crate::state::hex(&bytes)
}

pub fn dir(paths: &Paths, id: &str) -> Option<PathBuf> {
    is_job_id(id).then(|| paths.jobs_dir().join(id))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobSpec {
    pub id: String,
    pub kind: JobKind,
    pub uid: u32,
    pub created_at: i64,
    pub paths: Paths,
    pub tools: Tools,
    /// D-Bus address for logind inhibitors (`None`: the system bus).
    pub bus: Option<String>,
    pub action: Action,
    /// What the job is about (the runner may refine it, see [`ProgressLine::subject`]).
    #[serde(default)]
    pub subject: JobSubject,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// A spec is read once per job; the size difference does not matter.
#[allow(clippy::large_enum_variant)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    /// Copy `/etc/hinteros` to staging, edit, build, switch, swap back.
    Apply(Apply),
    /// Switch to an existing generation, then restore `/etc/hinteros` from its snapshot.
    Rollback {
        generation: u32,
        /// Then rebuild every app env against the restored system.
        #[serde(default)]
        rebuild_apps: bool,
    },
    /// Fetch and verify the channel tip (and record the other channels' verified tips).
    UpdateCheck(UpdateSource),
    /// Fetch and verify another channel's tip before `channel.set` (it may be older than the
    /// locked revision, never older than the recorded tip of its branch).
    ChannelCheck(UpdateSource),
    /// Change one owner's app env (install, remove, rollback, starter set, new profile).
    Apps(AppsJob),
    /// A new account (apply flow), then its metadata, PIN hash and starter env.
    ProfileCreate(ProfileCreateJob),
    /// An account's processes ended, the apply flow without it, then its home, PIN, metadata
    /// and app links deleted.
    ProfileRemove(ProfileRemoveJob),
    /// End guest session: the guest's processes ended, then its home, app env and per-guest
    /// state wiped; the guest gets the starter env again.
    GuestEnd(GuestEndJob),
    /// Measure what uses the disk into `storage.json` (storage.md).
    StorageMeasure(MeasureJob),
    /// Remove old generations (system or app envs), then collect garbage.
    Cleanup(CleanupJob),
    /// Services turned on or off (the apply flow), then their follow-up (Jellyfin users,
    /// deleting data).
    Services(ServicesJob),
    /// A Jellyfin library added or removed.
    Jellyfin(JellyfinJob),
    /// Plex linked to an account.
    Plex(PlexJob),
    /// Tailscale sign-in or sign-out.
    Tailscale(TailscaleJob),
    /// The Cloudflare tunnel restarted on a new token, or stopped and its token deleted.
    Cloudflare(CloudflareJob),
    /// One app's data deleted in the caller's home (`apps.clear_data`); nothing rebuilt.
    ClearData(DeleteData),
}

/// `services.enable` / `services.disable`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServicesJob {
    /// The apply flow (the new `services`, accepted terms).
    pub apply: Apply,
    /// Services turned on (Jellyfin: its users are made once its setup is done).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled: Vec<String>,
    /// After a successful switch: data to delete (checked again by the job).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delete: Vec<DeleteTarget>,
}

/// A service's data directory: `rel` below `base` (`/var/lib`, `/var/cache`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteTarget {
    pub base: PathBuf,
    pub rel: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum JellyfinOp {
    AddLibrary {
        name: String,
        kind: hinteros_api::system::LibraryKind,
        /// A `media.folders` id (resolved again by the job).
        folder: String,
    },
    RemoveLibrary {
        library_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JellyfinJob {
    pub op: JellyfinOp,
}

/// The claim code is short-lived (minutes) and single-use; the spec is root-only.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlexJob {
    pub code: String,
}

impl std::fmt::Debug for PlexJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PlexJob { .. }")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TailscaleJob {
    /// Sign in (else sign out).
    pub login: bool,
}

/// The token itself is written by the service (0600) before the job starts; it is never in a
/// spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudflareJob {
    /// Restart the tunnel on the new token (else stop it and delete the token).
    pub set: bool,
}

/// A home to measure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasureHome {
    /// The profile id, or `guest`.
    pub id: String,
    /// From the user database (`None`: the account has none; measured as empty).
    pub home: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasureJob {
    /// The Nix store (statvfs: the disk the ring shows).
    pub store_dir: PathBuf,
    /// The accounts in `state.json` order, then the guest (if on).
    pub homes: Vec<MeasureHome>,
    /// Measured as the `media` node.
    pub srv_dir: PathBuf,
    /// Every app-env owner.
    pub owners: Vec<Owner>,
}

/// Which generations a clean-up removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupTarget {
    /// System generations (then the boot menu is rewritten).
    System,
    /// Every app-env Nix profile with generations (profiles and the starter set).
    Profiles,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupJob {
    pub target: CleanupTarget,
    /// Generations kept (1 to 10; the running, booted and current ones always).
    pub keep: u32,
    /// The Nix store (statvfs before and after: what was freed).
    pub store_dir: PathBuf,
    /// `Profiles`: the owners whose app envs are trimmed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owners: Vec<Owner>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileCreateJob {
    /// The state with the new account.
    pub state: State,
    pub id: String,
    pub user: String,
    pub meta: crate::profiles::Meta,
    /// The PIN's argon2id PHC string (the PIN itself never reaches a file).
    pub pin_hash: String,
    /// Created from the greeter: a one-time token lets the greeter enter it.
    pub token: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestEndJob {
    /// The guest account's uid, checked by the service (and again by the job).
    pub uid: u32,
    /// Its home (from the user database).
    pub home: PathBuf,
    /// Shared directories cleared of the guest's entries (`/tmp`, `/var/tmp`, `/dev/shm`).
    pub tmp_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileRemoveJob {
    /// The state without the account.
    pub state: State,
    pub id: String,
    pub user: String,
    /// From the user database before the switch.
    pub uid: Option<u32>,
    pub home: Option<PathBuf>,
}

/// What an apps job does to its owner's env.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum AppsOp {
    /// Build the env of exactly these ids and make it the owner's new generation.
    Set { ids: Vec<String> },
    /// Switch the owner's Nix profile to an existing generation.
    Rollback { generation: u32 },
    /// Give the owner another owner's current env (a new profile or the guest: the starter
    /// env), without evaluating anything.
    CopyFrom { from: Owner },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppsJob {
    pub owner: Owner,
    pub op: AppsOp,
    /// Run the apply flow with this state first (stage `box`): box features the env needs
    /// that are off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub box_state: Option<State>,
    /// Vendor terms accepted with the request (recorded before anything is built).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub terms: Vec<TermsRecord>,
    /// App data to delete in the caller's home after the env is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_data: Option<DeleteData>,
    /// Starter set changes: write `starter.json` once the env is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starter: Option<StarterFile>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Apply {
    /// New `state.json` (serialized canonically by the job).
    pub state: Option<State>,
    /// Regenerate `facter.json` with nixos-facter.
    pub facter: bool,
    /// Verify a revision and lock the hinteros input to it.
    pub update: Option<ApplyUpdate>,
    /// Vendor terms accepted with this request (`apps.install`): recorded in
    /// `terms-accepted.json` before anything is built.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub terms: Vec<TermsRecord>,
    /// App data to delete after a successful switch (`apps.remove {delete_data: true}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_data: Option<DeleteData>,
    /// After the switch, rebuild every app env against the new system (update,
    /// channel and base switches).
    #[serde(default)]
    pub rebuild_apps: bool,
}

/// One accepted terms version, as `terms-accepted.json` records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TermsRecord {
    pub id: String,
    pub version: String,
    /// Unix seconds of the request.
    pub accepted_at: i64,
    /// The unix user who accepted (the caller).
    pub by: String,
    /// The profile id (the guest has none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// Data paths to delete in profile homes; homes and paths come from the service (passwd and
/// the running generation's catalog), never from the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteData {
    pub homes: Vec<Home>,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Home {
    pub user: String,
    pub uid: u32,
    pub dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateSource {
    pub repo: String,
    /// `https` or `file`.
    pub scheme: String,
    pub host: String,
    pub branch: String,
    pub channel: Channel,
    /// The locked revision before the job.
    pub current_rev: Option<String>,
    /// From the running generation (resolved when the job was created).
    pub allowed_signers: PathBuf,
    /// `update.check` only: compare the catalogs of the running and the target generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<CatalogCheck>,
    /// `update.check` only: the other channel branches of the table, fetched and verified to
    /// record their newest tips (`channel-tips.json`) before a switch to them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub others: Vec<String>,
}

/// What `update.check` needs to name the apps an update adds or removes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogCheck {
    /// The running generation's index (resolved when the job was created).
    pub index: PathBuf,
    /// Installed apps (`state.json`).
    pub installed: Vec<String>,
    /// The hinteros input URL of the channel without a rev; the job adds the verified one.
    pub input_url: String,
}

/// What happens when the target does not descend from the locked revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotFastForward {
    /// `update_verification_failed {reason: not_fast_forward}` (updates within a channel).
    Reject,
    /// A channel switch to the revision the owner confirmed after `channel.check` (it may be
    /// older than the locked one; never older than the recorded tip of its branch).
    Allow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyUpdate {
    pub source: UpdateSource,
    /// The verified revision to lock; `None`: the branch tip, verified by the job.
    pub rev: Option<String>,
    pub not_fast_forward: NotFastForward,
    /// The input URL (no rev) for the target branch.
    pub input_url: String,
    /// Channel switch: the input URL in flake.nix to replace with `input_url`.
    pub retarget_from: Option<String>,
}

/// One line of `progress.ndjson`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressLine {
    pub stage: Stage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<ProgressDetail>,
    /// The last time the job's process produced output (Unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_at: Option<i64>,
    /// The job's subject, when the job learnt more about it (replaces the spec's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<JobSubject>,
}

/// Generation and locked revision at one point.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub generation: Option<u32>,
    pub rev: Option<String>,
}

/// `status.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub state: JobState,
    pub stage: Option<Stage>,
    pub error: Option<ErrorObject>,
    pub result: Option<JobResult>,
    pub session_restart_required: bool,
    pub finished_at: i64,
    #[serde(default)]
    pub before: Snapshot,
    #[serde(default)]
    pub after: Option<Snapshot>,
    /// The subject as the job last knew it (`None`: the spec's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<JobSubject>,
}

impl Status {
    /// A job that ended without writing its status (killed, crashed, never started).
    pub fn lost(stage: Option<Stage>, why: &str) -> Self {
        Self {
            state: JobState::Failed,
            stage,
            error: Some(job_failed(stage.unwrap_or(Stage::Preparing), why)),
            result: None,
            session_restart_required: false,
            finished_at: now(),
            before: Snapshot::default(),
            after: None,
            subject: None,
        }
    }
}

/// `job_failed` with `reason` (why a Server section job failed: `unreachable`,
/// `not_ready`, `refused`, `timeout`, `not_connected`).
pub fn job_failed_because(stage: Stage, hint: &str, reason: &str) -> ErrorObject {
    let mut e = job_failed(stage, hint);
    if let Some(d) = e.data.as_mut() {
        d["reason"] = json!(reason);
    }
    e
}

/// `job_failed` (3003) with `{stage, log_hint}`.
pub fn job_failed(stage: Stage, hint: &str) -> ErrorObject {
    let hint: String = hint.chars().take(1000).collect();
    let stage_name = serde_json::to_value(stage)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    ErrorObject::with_data(
        ErrorCode::JobFailed,
        format!("the job failed while {stage_name}"),
        json!({ "stage": stage, "log_hint": hint }),
    )
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

impl JobSpec {
    /// The spec with what the job is about.
    pub fn about(mut self, subject: JobSubject) -> Self {
        self.subject = subject;
        self
    }
}
