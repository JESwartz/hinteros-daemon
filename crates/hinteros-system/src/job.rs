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
    Channel, JobKind, JobResult, JobState, ProgressDetail, Stage, is_job_id,
};
use hinteros_api::{ErrorCode, ErrorObject};
use serde::{Deserialize, Serialize};
use serde_json::json;

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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    /// Copy `/etc/hinteros` to staging, edit, build, switch, swap back.
    Apply(Apply),
    /// Switch to an existing generation, then restore `/etc/hinteros` from its snapshot.
    Rollback { generation: u32 },
    /// Fetch and verify the channel tip.
    UpdateCheck(UpdateSource),
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
}

/// What happens when the target does not descend from the locked revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotFastForward {
    /// `update_verification_failed {reason: not_fast_forward}` (updates within a channel).
    Reject,
    /// `needs_acknowledgement` (a channel switch without `acknowledge_downgrade`).
    NeedsAcknowledgement,
    /// Acknowledged.
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
        }
    }
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
