//! `hinteros-system job <id>`: runs one job in its own unit (`hinteros-job@<id>.service`), so a
//! switch that restarts `hinteros-system` cannot kill it. Reads only its spec; reports through
//! `progress.ndjson` and `status.json` (see [`crate::job`]).
//!
//! Apply flow (device-config.md): preflight → (fetch and verify) → copy `/etc/hinteros` to
//! staging and edit it → `nix build` the toplevel → set the profile and
//! `switch-to-configuration switch` → replace `/etc/hinteros` with staging. A failure or a
//! cancel before activation leaves the box unchanged.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use hinteros_api::common::check_version;
use hinteros_api::system::{
    AppliedResult, CatalogApp, ChannelCheckResult, JobResult, JobState, JobSubject, RemovedApp,
    Stage, UpdateCheckResult, is_job_id,
};
use hinteros_api::{ErrorCode, ErrorObject};
use rustix::fs::{FlockOperation, flock};
use rustix::process::{Pid, Signal, kill_process_group};
use serde_json::{Value, json};
use tracing::{error, info, warn};

use crate::flake;
use crate::fsutil;
use crate::generations;
use crate::job::{self, Action, Apply, JobSpec, NotFastForward, ProgressLine, Snapshot, Status};
use crate::nixlog;
use crate::update::{Git, GitError, SeenTips};

mod apps;
mod profiles;
mod servers;
mod storage;

/// Files of a config dir that belong to a generation (the snapshot; device-config.nix).
pub const MANAGED: &[&str] = &["flake.nix", "flake.lock", "state.json"];

/// Format of `terms-accepted.json`.
pub const TERMS_VERSION: u32 = 1;

/// Progress lines within a stage at most this often.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// How long a child's pipes may stay open after it exited (a process it left still holds
/// them) before the rest of its process group is killed.
const PIPE_GRACE: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 5 });

pub fn main(run_dir: &Path, id: &str) -> ExitCode {
    if !is_job_id(id) {
        error!("bad job id {id:?}");
        return ExitCode::FAILURE;
    }
    let dir = run_dir.join("jobs").join(id);
    match run(&dir) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("job {id}: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(dir: &Path) -> anyhow::Result<()> {
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join(job::LOCK))?;
    flock(&lock, FlockOperation::NonBlockingLockExclusive)
        .map_err(|e| anyhow::anyhow!("job is already running ({e})"))?;
    if dir.join(job::STATUS).exists() {
        anyhow::bail!("job already finished");
    }
    let spec: JobSpec = job::read_json(&dir.join(job::SPEC))
        .ok_or_else(|| anyhow::anyhow!("cannot read {}", dir.join(job::SPEC).display()))?;
    fs::write(dir.join(job::PID), std::process::id().to_string())?;
    info!(id = %spec.id, kind = ?spec.kind, uid = spec.uid, "job started");

    let mut r = Runner::new(spec, dir.to_owned());
    let before = r.snapshot();
    let outcome = match r.spec.action.clone() {
        Action::Apply(a) => r.apply(&a).and_then(|o| {
            if a.rebuild_apps {
                r.with_apps_stage(o)
            } else {
                Ok(o)
            }
        }),
        Action::Rollback {
            generation,
            rebuild_apps,
        } => r.rollback(generation).and_then(|o| {
            if rebuild_apps {
                r.with_apps_stage(o).and_then(|o| r.readd_features(o))
            } else {
                Ok(o)
            }
        }),
        Action::UpdateCheck(_) => r.update_check(),
        Action::ChannelCheck(_) => r.channel_check(),
        Action::Apps(j) => r.apps_job(&j),
        Action::ProfileCreate(j) => r.profile_create(&j),
        Action::ProfileRemove(j) => r.profile_remove(&j),
        Action::GuestEnd(j) => r.guest_end(&j),
        Action::StorageMeasure(j) => r.storage_measure(&j),
        Action::Cleanup(j) => r.cleanup(&j),
        Action::Services(j) => r.services_job(&j),
        Action::Jellyfin(j) => r.jellyfin_job(&j),
        Action::Plex(j) => r.plex_job(&j),
        Action::Tailscale(j) => r.tailscale_job(&j),
        Action::Cloudflare(j) => r.cloudflare_job(&j),
        Action::ClearData(d) => {
            r.set_stage(Stage::Finishing);
            delete_data(&d);
            Ok((None, false))
        }
    };
    let subject = (r.subject != r.spec.subject).then(|| r.subject.clone());
    let staging = r.spec.paths.staging();
    let status = match outcome {
        Ok((result, restart)) => Status {
            state: JobState::Succeeded,
            stage: r.stage,
            error: None,
            result,
            session_restart_required: restart,
            finished_at: job::now(),
            before,
            after: Some(r.snapshot()),
            subject: subject.clone(),
        },
        Err(Failure::Cancelled) => {
            let _ = fsutil::remove(&staging);
            let stage = r.stage.unwrap_or(Stage::Preparing);
            Status {
                state: JobState::Cancelled,
                stage: r.stage,
                error: Some(ErrorObject {
                    message: "cancelled".into(),
                    ..job::job_failed_because(stage, "cancelled", "cancelled")
                }),
                result: None,
                session_restart_required: false,
                finished_at: job::now(),
                before,
                after: None,
                subject: subject.clone(),
            }
        }
        Err(Failure::Error(e)) => {
            let _ = fsutil::remove(&staging);
            warn!("job failed: {e}");
            Status {
                state: JobState::Failed,
                stage: r.stage,
                error: Some(e),
                result: None,
                session_restart_required: false,
                finished_at: job::now(),
                before,
                after: Some(r.snapshot()),
                subject: subject.clone(),
            }
        }
    };
    let _ = fs::remove_file(dir.join(job::ACTIVATING));
    fsutil::write_atomic(&dir.join(job::STATUS), &serde_json::to_vec(&status)?, 0o600)?;
    info!(state = ?status.state, "job finished");
    drop(lock);
    Ok(())
}

#[derive(Debug)]
pub enum Failure {
    Cancelled,
    Error(ErrorObject),
}

impl From<ErrorObject> for Failure {
    fn from(e: ErrorObject) -> Self {
        Failure::Error(e)
    }
}

type Outcome = Result<(Option<JobResult>, bool), Failure>;

struct Runner {
    spec: JobSpec,
    dir: PathBuf,
    stage: Option<Stage>,
    progress: Option<fs::File>,
    /// The last line written and when.
    last_progress: Option<(Instant, ProgressLine)>,
    /// A line held back by the rate limit (written by [`Runner::tick`]).
    pending: Option<ProgressLine>,
    /// The last time a child process printed anything (Unix seconds).
    activity_at: Option<i64>,
    /// What the job is about (the spec's, refined as the job learns more).
    subject: JobSubject,
    /// logind inhibitor (delay, then block during activation); dropped at the end.
    inhibitor: Option<OwnedFd>,
    /// Report every stage as this one (the `box` stage wraps a whole apply flow).
    stage_override: Option<Stage>,
}

impl Runner {
    fn new(spec: JobSpec, dir: PathBuf) -> Self {
        let progress = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(dir.join(job::PROGRESS))
            .ok();
        Self {
            subject: spec.subject.clone(),
            spec,
            dir,
            stage: None,
            progress,
            last_progress: None,
            pending: None,
            activity_at: None,
            inhibitor: None,
            stage_override: None,
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            generation: generations::current_generation(&self.spec.paths.profile),
            rev: flake::read_lock(&self.spec.paths.config_dir.join("flake.lock"))
                .ok()
                .and_then(|l| l.rev),
        }
    }

    fn fail(&self, hint: impl AsRef<str>) -> Failure {
        Failure::Error(job::job_failed(
            self.stage.unwrap_or(Stage::Preparing),
            hint.as_ref(),
        ))
    }

    fn set_stage(&mut self, stage: Stage) {
        let stage = self.stage_override.unwrap_or(stage);
        if self.stage != Some(stage) {
            self.stage = Some(stage);
            self.report(ProgressLine {
                stage,
                percent: None,
                detail: None,
                activity_at: None,
                subject: None,
            });
        }
    }

    fn report(&mut self, line: ProgressLine) {
        self.report_at(line, Instant::now());
    }

    /// Stage changes and subject updates are written at once; progress within a stage at most
    /// every [`PROGRESS_INTERVAL`] (a held-back line is written by [`Self::tick_at`]).
    fn report_at(&mut self, mut line: ProgressLine, now: Instant) {
        if let Some(s) = self.stage_override {
            line.stage = s;
        }
        line.activity_at = self.activity_at;
        if let Some((at, last)) = &self.last_progress
            && last.stage == line.stage
            && line.subject.is_none()
            && now.saturating_duration_since(*at) < PROGRESS_INTERVAL
        {
            self.pending = Some(line);
            return;
        }
        self.write_line(line, now);
    }

    fn write_line(&mut self, line: ProgressLine, now: Instant) {
        self.pending = None;
        if let Some(f) = self.progress.as_mut()
            && let Ok(mut text) = serde_json::to_string(&line)
        {
            text.push('\n');
            let _ = f.write_all(text.as_bytes());
        }
        self.last_progress = Some((now, line));
    }

    /// A child process printed something.
    fn touch(&mut self) {
        self.activity_at = Some(job::now());
    }

    /// Called while a child runs: write a held-back line once the rate limit allows, and
    /// otherwise, every [`jobs::HEARTBEAT`], the last line again when there was output since
    /// (so `last_activity_at` stays current while nix compiles quietly).
    fn tick_at(&mut self, now: Instant) {
        let Some((at, last)) = &self.last_progress else {
            return;
        };
        let since = now.saturating_duration_since(*at);
        if let Some(line) = self.pending.take() {
            if since >= PROGRESS_INTERVAL {
                let line = ProgressLine {
                    activity_at: self.activity_at,
                    ..line
                };
                self.write_line(line, now);
            } else {
                self.pending = Some(line);
            }
            return;
        }
        if since >= crate::jobs::HEARTBEAT && last.activity_at < self.activity_at {
            let line = ProgressLine {
                activity_at: self.activity_at,
                subject: None,
                ..last.clone()
            };
            self.write_line(line, now);
        }
    }

    /// The job learnt more about what it does (e.g. the verified target of a channel switch).
    fn set_subject(&mut self, f: impl FnOnce(&mut JobSubject)) {
        let mut subject = self.subject.clone();
        f(&mut subject);
        if subject == self.subject {
            return;
        }
        self.subject = subject.clone();
        let base = self.last_progress.as_ref().map(|(_, l)| l.clone());
        let line = match base {
            Some(l) => ProgressLine {
                subject: Some(subject),
                ..l
            },
            None => ProgressLine {
                stage: self.stage.unwrap_or(Stage::Preparing),
                percent: None,
                detail: None,
                activity_at: None,
                subject: Some(subject),
            },
        };
        self.report(line);
    }

    /// Report nix's progress (a build or download stage).
    fn nix_progress(&mut self, progress: &nixlog::Progress) {
        let stage = progress.stage.unwrap_or(Stage::Evaluating);
        let detail = progress.detail();
        self.set_stage(stage);
        self.report(ProgressLine {
            stage,
            percent: progress.percent(),
            detail: (!detail.is_empty()).then_some(detail),
            activity_at: None,
            subject: None,
        });
    }

    fn cancel_requested(&self) -> bool {
        self.dir.join(job::CANCEL).exists()
    }

    fn check_cancel(&self) -> Result<(), Failure> {
        if self.cancel_requested() {
            Err(Failure::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Past this point the job cannot be cancelled: create the marker, then look for a cancel
    /// request that came first.
    fn enter_activation(&mut self) -> Result<(), Failure> {
        let created = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.dir.join(job::ACTIVATING));
        // A second activation in the same job (a rollback's `box` stage): already past the
        // point of no cancelling.
        let created = match created {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            other => other.map(drop),
        };
        if let Err(e) = created {
            return Err(self.fail(format!("cannot mark activation: {e}")));
        }
        if self.cancel_requested() {
            let _ = fs::remove_file(self.dir.join(job::ACTIVATING));
            return Err(Failure::Cancelled);
        }
        self.set_stage(Stage::Activating);
        self.inhibit("block");
        Ok(())
    }

    /// Hold a logind `shutdown:sleep` inhibitor (best effort: a missing logind only loses the
    /// protection against a reboot mid-switch).
    fn inhibit(&mut self, mode: &str) {
        self.inhibitor = None;
        let bus = self.spec.bus.clone();
        let mode = mode.to_owned();
        let taken = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(zbus::Error::from)
            .and_then(|rt| {
                rt.block_on(async move {
                    let conn = match &bus {
                        Some(a) => {
                            zbus::connection::Builder::address(a.as_str())?
                                .build()
                                .await?
                        }
                        None => zbus::Connection::system().await?,
                    };
                    let reply = tokio::time::timeout(
                        Duration::from_secs(5),
                        conn.call_method(
                            Some("org.freedesktop.login1"),
                            "/org/freedesktop/login1",
                            Some("org.freedesktop.login1.Manager"),
                            "Inhibit",
                            &(
                                "shutdown:sleep",
                                "HinterOS",
                                "Applying a system change",
                                mode.as_str(),
                            ),
                        ),
                    )
                    .await
                    .map_err(|_| zbus::Error::Failure("timed out".into()))??;
                    let fd: zbus::zvariant::OwnedFd = reply.body().deserialize()?;
                    Ok::<OwnedFd, zbus::Error>(fd.into())
                })
            });
        match taken {
            Ok(fd) => self.inhibitor = Some(fd),
            Err(e) => warn!("no logind inhibitor: {e}"),
        }
    }

    /// Run a command in its own process group; stderr lines go to `on_line` (and our stderr,
    /// i.e. the unit's journal). While `cancellable`, a cancel request kills the group.
    fn exec(
        &mut self,
        cmd: Command,
        cancellable: bool,
        on_line: &mut dyn FnMut(&mut Self, &str),
    ) -> Result<String, Failure> {
        self.exec_with(cmd, cancellable, 1 << 20, on_line)
    }

    /// [`Self::exec`], keeping up to `limit` bytes of stdout (the rest is read and dropped).
    ///
    /// Both pipes are drained to the end whatever they carry (a non-UTF-8 stderr line is
    /// read lossily), so the child never gets SIGPIPE from us. Once the child has exited,
    /// the pipes get [`PIPE_GRACE`] to reach EOF; something left in the process group that
    /// still holds them is then killed, and a descendant that left the group is given up on.
    fn exec_with(
        &mut self,
        mut cmd: Command,
        cancellable: bool,
        limit: u64,
        on_line: &mut dyn FnMut(&mut Self, &str),
    ) -> Result<String, Failure> {
        let program = format!("{:?}", cmd.get_program());
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| self.fail(format!("cannot run {program}: {e}")))?;
        let pgid = Pid::from_child(&child);
        let mut stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.by_ref().take(limit).read_to_end(&mut buf);
            let _ = std::io::copy(&mut stdout, &mut std::io::sink());
            let _ = out_tx.send(buf);
        });
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let mut stderr = BufReader::new(stderr);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match stderr.read_until(b'\n', &mut buf) {
                    Ok(0) => break,
                    Ok(_) => {
                        if buf.last() == Some(&b'\n') {
                            buf.pop();
                        }
                        // Nobody listens any more: keep draining all the same.
                        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });
        let mut tail: Vec<String> = Vec::new();
        let keep = |tail: &mut Vec<String>, line: &str| {
            if !line.starts_with("@nix ") && !line.trim().is_empty() {
                tail.push(line.chars().take(400).collect());
                if tail.len() > 20 {
                    tail.remove(0);
                }
            }
        };
        let mut cancelled = false;
        let status = loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => {
                    if !line.starts_with("@nix ") {
                        eprintln!("{line}");
                    }
                    keep(&mut tail, &line);
                    self.touch();
                    on_line(self, &line);
                    self.tick_at(Instant::now());
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => self.tick_at(Instant::now()),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // stderr is closed but the child runs on: wait without spinning.
                    std::thread::sleep(Duration::from_millis(100));
                    self.tick_at(Instant::now());
                }
            }
            if cancellable && !cancelled && self.cancel_requested() {
                cancelled = true;
                let _ = kill_process_group(pgid, Signal::TERM);
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline {
                    if let Ok(Some(_)) = child.try_wait() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                let _ = kill_process_group(pgid, Signal::KILL);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(e) => return Err(self.fail(format!("waiting for {program}: {e}"))),
            }
        };
        // Drain what is left of stderr, and collect stdout.
        let mut out = None;
        let mut stderr_done = false;
        let mut deadline = Instant::now() + PIPE_GRACE;
        let mut killed = false;
        loop {
            if !stderr_done {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(line) => {
                        keep(&mut tail, &line);
                        on_line(self, &line);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => stderr_done = true,
                }
            }
            if out.is_none() {
                out = match out_rx.try_recv() {
                    Ok(buf) => Some(buf),
                    Err(mpsc::TryRecvError::Disconnected) => Some(Vec::new()),
                    Err(mpsc::TryRecvError::Empty) => None,
                };
            }
            if stderr_done && out.is_some() {
                break;
            }
            if Instant::now() >= deadline {
                if killed {
                    warn!("{program} exited, but a process it started still holds its output");
                    break;
                }
                warn!("{program} exited and left processes running; stopping them");
                // The leader is reaped, so the group id is free once its last member is gone.
                // Accepted: pids grow up to pid_max (4194304) before they wrap, so a new group
                // with this id within PIPE_GRACE is not a real case.
                let _ = kill_process_group(pgid, Signal::KILL);
                killed = true;
                deadline = Instant::now() + PIPE_GRACE;
            }
            if stderr_done {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        if let Some(line) = self.pending.take() {
            self.write_line(line, Instant::now());
        }
        let out = out.unwrap_or_default();
        if cancelled {
            return Err(Failure::Cancelled);
        }
        if !status.success() {
            let hint = tail
                .iter()
                .rev()
                .find(|l| l.contains("error"))
                .or(tail.last())
                .cloned()
                .unwrap_or_else(|| format!("{program} failed ({status})"));
            return Err(self.fail(hint));
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    fn git(&self, source: &job::UpdateSource) -> Git {
        Git {
            git: self.spec.tools.git.clone(),
            ssh_keygen: self.spec.tools.ssh_keygen.clone(),
            git_dir: self.spec.paths.git_dir(),
            scheme: if source.scheme == "file" {
                "file"
            } else {
                "https"
            },
        }
    }

    fn git_error(&self, source: &job::UpdateSource, e: GitError) -> Failure {
        match e {
            GitError::Unreachable(msg) => {
                warn!("update server unreachable: {msg}");
                Failure::Error(ErrorObject::with_data(
                    ErrorCode::UpdateUnreachable,
                    "cannot reach the update server",
                    json!({ "host": source.host }),
                ))
            }
            GitError::Rejected(r) => Failure::Error(r.error()),
            GitError::Failed(msg) => self.fail(msg),
        }
    }

    // -----------------------------------------------------------------------------------------

    fn update_check(&mut self) -> Outcome {
        let Action::UpdateCheck(source) = self.spec.action.clone() else {
            unreachable!()
        };
        self.set_stage(Stage::Preparing);
        let git = self.git(&source);
        let tips_path = self.spec.paths.channel_tips();
        let mut tips = SeenTips::load(&tips_path);
        let mut stages = Vec::new();
        let checked = git.check(
            &source.repo,
            &source.branch,
            source.current_rev.as_deref(),
            true,
            tips.get(&source.branch),
            &source.allowed_signers,
            &mut |s| stages.push(s),
        );
        for s in stages {
            self.set_stage(s);
        }
        // Record the verified tips: this channel's, and (unless the server is unreachable) the
        // other channels', so a later switch is checked against them.
        let mut changed = false;
        if let Ok(Some(a)) = &checked {
            changed |= record_tip(&git, &mut tips, &source.branch, &a.rev);
        }
        if !matches!(checked, Err(GitError::Unreachable(_))) {
            for branch in &source.others {
                changed |= record_other(&git, &source, &mut tips, branch);
            }
        }
        if changed && let Err(e) = tips.save(&tips_path) {
            warn!("cannot store the verified channel tips: {e}");
        }
        let mut available = checked.map_err(|e| self.git_error(&source, e))?;
        if let (Some(a), Some(cat)) = (available.as_mut(), &source.catalog) {
            self.set_stage(Stage::Evaluating);
            if let Some((new, removed)) = self.catalog_diff(cat, &a.rev)? {
                a.new_apps = Some(new);
                a.removed_apps = Some(removed);
            }
        }
        let result = UpdateCheckResult {
            channel: source.channel,
            current_rev: source.current_rev.clone(),
            available,
            checked_at: job::now(),
        };
        if let Ok(bytes) = fsutil::cache_bytes(&result) {
            let _ = fs::create_dir_all(&self.spec.paths.state_dir);
            if let Err(e) = fsutil::write_atomic(&self.spec.paths.last_check(), &bytes, 0o600) {
                warn!("cannot store the check result: {e}");
            }
        }
        Ok((Some(JobResult::UpdateCheck(result)), false))
    }

    /// `channel.check`: fetch and verify the target channel's tip. It may be older than the
    /// locked revision (`older`), never older than the newest tip recorded for its branch.
    fn channel_check(&mut self) -> Outcome {
        let Action::ChannelCheck(source) = self.spec.action.clone() else {
            unreachable!()
        };
        self.set_stage(Stage::Preparing);
        let git = self.git(&source);
        let tips_path = self.spec.paths.channel_tips();
        let mut tips = SeenTips::load(&tips_path);
        self.set_stage(Stage::Fetching);
        let tip = git
            .init()
            .and_then(|()| git.fetch_tip(&source.repo, &source.branch))
            .map_err(|e| self.git_error(&source, e))?;
        // The locked revision must be local to compare with: fetch the branch the box follows
        // when it is not (best effort; else the target is described on its own).
        let current = source.current_rev.as_deref();
        if let Some(c) = current
            && !git.has_commit(c)
        {
            for branch in &source.others {
                if let Err(e) = git.fetch(&source.repo, branch) {
                    warn!("cannot fetch {branch}: {e}");
                }
            }
        }
        self.check_cancel()?;
        self.set_stage(Stage::Verifying);
        let target = git
            .describe(
                &tip,
                current,
                false,
                tips.get(&source.branch),
                &source.allowed_signers,
            )
            .map_err(|e| self.git_error(&source, e))?;
        // Not known when the locked revision is still not local.
        let older = match current.filter(|c| git.has_commit(c)) {
            Some(c) => !git
                .is_ancestor(c, &tip)
                .map_err(|e| self.git_error(&source, e))?,
            None => false,
        };
        if record_tip(&git, &mut tips, &source.branch, &tip)
            && let Err(e) = tips.save(&tips_path)
        {
            warn!("cannot store the verified channel tips: {e}");
        }
        self.set_subject(|s| {
            s.rev = Some(tip.clone());
            s.version = target.version.clone();
        });
        let result = ChannelCheckResult {
            channel: source.channel,
            current_rev: source.current_rev.clone(),
            target,
            older,
            checked_at: job::now(),
        };
        let bytes = fsutil::cache_bytes(&result)
            .map_err(|e| self.fail(format!("encoding the check result: {e}")))?;
        let _ = fs::create_dir_all(&self.spec.paths.state_dir);
        // channel.set accepts only this revision: without the file the switch cannot go on.
        fsutil::write_atomic(&self.spec.paths.channel_check(), &bytes, 0o600)
            .map_err(|e| self.fail(format!("storing the check result: {e}")))?;
        Ok((Some(JobResult::ChannelCheck(result)), false))
    }

    /// The apps the update at `rev` adds and removes: `lib.catalogIds` of the target revision
    /// (`nix eval`, pure metadata, no import-from-derivation) against the running generation's
    /// index. `None` if the target's catalog cannot be read (the update is still offered).
    fn catalog_diff(
        &mut self,
        cat: &job::CatalogCheck,
        rev: &str,
    ) -> Result<Option<CatalogChanges>, Failure> {
        let mut cmd = self.nix_command();
        cmd.args([
            "eval",
            "--json",
            "--option",
            "allow-import-from-derivation",
            "false",
        ])
        .arg(format!("{}&rev={rev}#lib.catalogIds", cat.input_url));
        let out = match self.exec(cmd, true, &mut |_, _| {}) {
            Ok(out) => out,
            Err(Failure::Cancelled) => return Err(Failure::Cancelled),
            Err(Failure::Error(e)) => {
                warn!("cannot read the target's catalog: {}", e.message);
                return Ok(None);
            }
        };
        let target: serde_json::Value = match serde_json::from_str(out.trim()) {
            Ok(v) => v,
            Err(e) => {
                warn!("lib.catalogIds is not JSON: {e}");
                return Ok(None);
            }
        };
        let current = hinteros_api::catalog::Index::load(&cat.index)
            .map_err(|e| warn!("no current catalog: {e}"))
            .ok();
        Ok(catalog_changes(current.as_ref(), &target, &cat.installed))
    }

    fn apply(&mut self, a: &Apply) -> Outcome {
        self.set_stage(Stage::Preparing);
        self.inhibit("delay");
        let paths = self.spec.paths.clone();
        let tools = self.spec.tools.clone();
        if !paths.config_dir.join("flake.nix").is_file() {
            return Err(self.fail(format!("{} has no flake.nix", paths.config_dir.display())));
        }
        if !a.terms.is_empty() {
            record_terms(&paths.terms_accepted(), &a.terms)
                .map_err(|e| self.fail(format!("recording the accepted terms: {e}")))?;
            for t in &a.terms {
                info!(id = %t.id, version = %t.version, by = %t.by, "terms acceptance recorded");
            }
        }
        let old_toplevel = fs::canonicalize(&paths.profile).ok();
        let old_generation = generations::current_generation(&paths.profile);

        // Fetch and verify the target revision.
        let mut lock_url = None;
        if let Some(u) = &a.update {
            self.check_cancel()?;
            self.set_stage(Stage::Fetching);
            let git = self.git(&u.source);
            // Without a checked revision, the tip that was fetched (no race with ls-remote).
            let tip = git
                .init()
                .and_then(|()| git.fetch_tip(&u.source.repo, &u.source.branch))
                .map_err(|e| self.git_error(&u.source, e))?;
            self.check_cancel()?;
            self.set_stage(Stage::Verifying);
            let rev = u.rev.clone().unwrap_or(tip);
            let require_ff = u.not_fast_forward != NotFastForward::Allow;
            // Never older than the newest tip this box verified on the target branch (also on
            // a switch, which may go back from the locked revision).
            let tips_path = paths.channel_tips();
            let mut tips = SeenTips::load(&tips_path);
            let described = git
                .describe(
                    &rev,
                    u.source.current_rev.as_deref(),
                    require_ff,
                    tips.get(&u.source.branch),
                    &u.source.allowed_signers,
                )
                .map_err(|e| self.git_error(&u.source, e))?;
            if record_tip(&git, &mut tips, &u.source.branch, &rev)
                && let Err(e) = tips.save(&tips_path)
            {
                warn!("cannot store the verified channel tips: {e}");
            }
            // The verified target names the job (a channel switch learns it only now).
            self.set_subject(|s| {
                s.rev = Some(rev.clone());
                if described.version.is_some() {
                    s.version = described.version.clone();
                }
            });
            lock_url = Some(format!("{}&rev={rev}", u.input_url));
        }

        // /etc/hinteros must describe the running system: a job that activated but could not
        // replace it left the old config, which would build the old revision again. Restore it
        // from that generation's snapshot first, as hinteros-config-sync does at boot. Only
        // when the running system is the profile's (not after a failed switch, nor when an
        // older generation was booted from the boot menu).
        if let Some(snapshot) = old_toplevel.as_ref().map(|t| t.join("hinteros"))
            && fs::canonicalize(&paths.current_system).ok() == old_toplevel
            && config_behind(&snapshot, &paths.config_dir)
        {
            warn!("/etc/hinteros does not match the system's snapshot; restoring it");
            let _lock = crate::state::lock(&paths.state_lock())
                .map_err(|e| self.fail(format!("locking state.json: {e}")))?;
            sync_config(
                &snapshot,
                &paths.config_dir,
                &paths.staging(),
                &self.spec.id,
            )
            .map_err(|e| self.fail(format!("restoring the config snapshot: {e}")))?;
        }

        // Staging: a copy of /etc/hinteros with the edit applied.
        self.check_cancel()?;
        self.set_stage(Stage::Evaluating);
        let staging = paths.staging();
        fsutil::remove(&staging).map_err(|e| self.fail(format!("clearing staging: {e}")))?;
        if let Some(parent) = staging.parent() {
            let _ = fs::DirBuilder::new()
                .recursive(true)
                .mode(0o755)
                .create(parent);
        }
        fsutil::copy_dir(&paths.config_dir, &staging)
            .map_err(|e| self.fail(format!("copying the config to staging: {e}")))?;
        if let Some(state) = &a.state {
            fsutil::write_atomic(
                &staging.join("state.json"),
                &state.to_canonical_json(),
                0o644,
            )
            .map_err(|e| self.fail(format!("writing state.json: {e}")))?;
        }
        if let Some(u) = &a.update
            && let Some(from) = &u.retarget_from
        {
            let path = staging.join("flake.nix");
            let text = fs::read_to_string(&path)
                .map_err(|e| self.fail(format!("reading flake.nix: {e}")))?;
            let text = flake::retarget_flake_nix(&text, from, &u.input_url)
                .map_err(|e| self.fail(e.to_string()))?;
            fsutil::write_atomic(&path, text.as_bytes(), 0o644)
                .map_err(|e| self.fail(format!("writing flake.nix: {e}")))?;
        }
        if a.facter {
            let mut cmd = Command::new(&tools.nixos_facter);
            cmd.arg("-o").arg(staging.join("facter.json"));
            self.exec(cmd, true, &mut |_, _| {})?;
        }
        let flake_ref = format!("path:{}", staging.display());
        if let Some(url) = &lock_url {
            let mut cmd = self.nix_command();
            cmd.args(["flake", "lock", "--override-input", "hinteros", url])
                .arg(&flake_ref);
            self.exec(cmd, true, &mut |_, _| {})?;
        }

        // Build.
        self.check_cancel()?;
        let mut cmd = self.nix_command();
        cmd.args([
            "build",
            "--log-format",
            "internal-json",
            "-v",
            "--no-link",
            "--print-out-paths",
        ])
        .arg(format!(
            "{flake_ref}#nixosConfigurations.hinteros.config.system.build.toplevel"
        ));
        let mut progress = nixlog::Progress::default();
        let out = self.exec(cmd, true, &mut |r, line| {
            if progress.line(line) {
                r.nix_progress(&progress);
            }
        });
        let out = match out {
            Err(Failure::Error(_)) if progress.last_error.is_some() => {
                return Err(self.fail(progress.last_error.clone().unwrap_or_default()));
            }
            other => other?,
        };
        let toplevel = out
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| l.starts_with('/'))
            .map(PathBuf::from)
            .ok_or_else(|| self.fail("nix build printed no output path"))?;

        // Activate. A new Jellyfin version: its data backed up first (server.md).
        self.check_cancel()?;
        self.enter_activation()?;
        let jellyfin_stopped = self.jellyfin_backup(old_toplevel.as_deref(), &toplevel)?;
        // `nix-env --set` reuses the newest generation when it has the same toplevel (after a
        // rollback): only a generation this job made is deleted when the switch fails.
        let generations_before: Vec<u32> = generations::links(&paths.profile)
            .iter()
            .map(|l| l.generation)
            .collect();
        let activated = self.activate(&toplevel);
        if activated.is_err() {
            self.restore(old_generation, &generations_before, old_toplevel.as_deref());
        }
        self.jellyfin_start(jellyfin_stopped);
        activated?;

        // Finish: /etc/hinteros now describes the running system. The time zone is set in
        // place, without a job (time_zone.set): the live one wins over the staged copy's.
        self.set_stage(Stage::Finishing);
        {
            let _lock = crate::state::lock(&paths.state_lock())
                .map_err(|e| self.fail(format!("locking state.json: {e}")))?;
            match crate::timezone::carry_live_zone(&paths.state_json(), &staging.join("state.json"))
            {
                Ok(true) => info!("kept the time zone set while the job ran"),
                Ok(false) => {}
                Err(e) => warn!("cannot carry the live time zone into the new config: {e}"),
            }
            fsutil::replace_dir(&staging, &paths.config_dir, &self.spec.id)
                .map_err(|e| self.fail(format!("replacing {}: {e}", paths.config_dir.display())))?;
        }
        let _ = fsutil::remove(&staging);
        if let Some(d) = &a.delete_data {
            delete_data(d);
        }
        let after = self.snapshot();
        let restart = session_restart_required(old_toplevel.as_deref(), &toplevel);
        Ok((
            Some(JobResult::Applied(AppliedResult {
                generation: after.generation,
                rev: after.rev,
                apps_not_updated: None,
                features_readded: None,
            })),
            restart,
        ))
    }

    fn nix_command(&self) -> Command {
        let mut cmd = Command::new(&self.spec.tools.nix);
        cmd.args(["--extra-experimental-features", "nix-command flakes"]);
        cmd
    }

    fn activate(&mut self, toplevel: &Path) -> Result<(), Failure> {
        let mut cmd = Command::new(&self.spec.tools.nix_env);
        cmd.arg("-p")
            .arg(&self.spec.paths.profile)
            .arg("--set")
            .arg(toplevel);
        self.exec(cmd, false, &mut |_, _| {})?;
        let cmd = Command::new(toplevel.join("bin/switch-to-configuration"));
        let mut cmd = cmd;
        cmd.arg("switch");
        self.exec(cmd, false, &mut |_, _| {})?;
        Ok(())
    }

    /// After a failed activation: point the profile back at the old generation, delete the
    /// generation that failed if the job made it (so it is no rollback target; one that was
    /// there before, in `before`, stays), and make the old system the boot default again (best
    /// effort; the failure is reported either way). Without a generation number, the profile
    /// is set to the old system.
    fn restore(&mut self, old_generation: Option<u32>, before: &[u32], old: Option<&Path>) {
        let Some(old) = old else { return };
        warn!(
            "activation failed; restoring the profile to {}",
            old.display()
        );
        let profile = self.spec.paths.profile.clone();
        let failed = generations::current_generation(&profile)
            .filter(|g| old_generation != Some(*g) && !before.contains(g));
        let mut cmd = Command::new(&self.spec.tools.nix_env);
        cmd.arg("-p").arg(&profile);
        match old_generation {
            Some(g) => cmd.arg("--switch-generation").arg(g.to_string()),
            None => cmd.arg("--set").arg(old),
        };
        if self.exec(cmd, false, &mut |_, _| {}).is_err() {
            return;
        }
        if old_generation.is_some()
            && let Some(f) = failed
        {
            let mut cmd = Command::new(&self.spec.tools.nix_env);
            cmd.arg("-p")
                .arg(&profile)
                .arg("--delete-generations")
                .arg(f.to_string());
            if let Err(Failure::Error(e)) = self.exec(cmd, false, &mut |_, _| {}) {
                warn!("the failed generation {f} stays: {}", e.message);
            }
        }
        let mut cmd = Command::new(old.join("bin/switch-to-configuration"));
        cmd.arg("boot");
        let _ = self.exec(cmd, false, &mut |_, _| {});
    }

    /// A rollback's switch: the profile to `generation`, then its `switch-to-configuration
    /// switch`; when the switch fails, the profile back to `old_generation` and its system the
    /// boot default again (best effort; the failure is reported either way).
    fn switch_generation(
        &mut self,
        generation: u32,
        old_generation: Option<u32>,
        old_toplevel: Option<&Path>,
        target: &Path,
    ) -> Result<(), Failure> {
        let profile = self.spec.paths.profile.clone();
        let mut cmd = Command::new(&self.spec.tools.nix_env);
        cmd.arg("-p")
            .arg(&profile)
            .arg("--switch-generation")
            .arg(generation.to_string());
        self.exec(cmd, false, &mut |_, _| {})?;
        let mut cmd = Command::new(target.join("bin/switch-to-configuration"));
        cmd.arg("switch");
        if let Err(e) = self.exec(cmd, false, &mut |_, _| {}) {
            if let Some(old) = old_generation {
                let mut cmd = Command::new(&self.spec.tools.nix_env);
                cmd.arg("-p")
                    .arg(&profile)
                    .arg("--switch-generation")
                    .arg(old.to_string());
                if self.exec(cmd, false, &mut |_, _| {}).is_ok()
                    && let Some(old) = old_toplevel
                {
                    let mut cmd = Command::new(old.join("bin/switch-to-configuration"));
                    cmd.arg("boot");
                    let _ = self.exec(cmd, false, &mut |_, _| {});
                }
            }
            return Err(e);
        }
        Ok(())
    }

    fn rollback(&mut self, generation: u32) -> Outcome {
        self.set_stage(Stage::Preparing);
        self.inhibit("delay");
        let paths = self.spec.paths.clone();
        let old_generation = generations::current_generation(&paths.profile);
        let old_toplevel = fs::canonicalize(&paths.profile).ok();
        let target = generations::links(&paths.profile)
            .into_iter()
            .find(|l| l.generation == generation)
            .and_then(|l| l.toplevel)
            .ok_or_else(|| {
                Failure::Error(ErrorObject::new(
                    ErrorCode::NotFound,
                    format!("generation {generation} is not on disk"),
                ))
            })?;
        self.check_cancel()?;
        self.enter_activation()?;
        let jellyfin_stopped = self.jellyfin_backup(old_toplevel.as_deref(), &target)?;
        let switched =
            self.switch_generation(generation, old_generation, old_toplevel.as_deref(), &target);
        self.jellyfin_start(jellyfin_stopped);
        switched?;
        self.set_stage(Stage::Finishing);
        {
            let _lock = crate::state::lock(&paths.state_lock())
                .map_err(|e| self.fail(format!("locking state.json: {e}")))?;
            sync_config(
                &target.join("hinteros"),
                &paths.config_dir,
                &paths.staging(),
                &self.spec.id,
            )
            .map_err(|e| self.fail(format!("restoring the config snapshot: {e}")))?;
            // The switch set /etc/localtime from the state.json before the restore: follow the
            // restored one (the live time zone if one was set, else the snapshot's, or UTC).
            match crate::timezone::apply_state(&paths, self.spec.bus.as_deref()) {
                Ok(zone) => info!(zone, "time zone of the restored state applied"),
                Err(e) => warn!("cannot apply the restored time zone: {e}"),
            }
        }
        let after = self.snapshot();
        let restart = session_restart_required(old_toplevel.as_deref(), &target);
        Ok((
            Some(JobResult::Applied(AppliedResult {
                generation: after.generation,
                rev: after.rev,
                apps_not_updated: None,
                features_readded: None,
            })),
            restart,
        ))
    }
}

/// Record a verified `rev` as `branch`'s newest tip if it descends from the recorded one;
/// returns whether the record changed.
fn record_tip(git: &Git, tips: &mut SeenTips, branch: &str, rev: &str) -> bool {
    tips.record(git, branch, rev).unwrap_or_else(|e| {
        warn!("cannot record the tip of {branch}: {e}");
        false
    })
}

/// `update.check`: fetch and verify another channel's tip and record it. Failures are logged
/// only (the check is about the followed channel). Returns whether the record changed.
fn record_other(git: &Git, source: &job::UpdateSource, tips: &mut SeenTips, branch: &str) -> bool {
    let tip = match git
        .fetch_tip(&source.repo, branch)
        .and_then(|tip| git.verify(&tip, &source.allowed_signers).map(|_| tip))
    {
        Ok(tip) => tip,
        Err(e) => {
            warn!("channel branch {branch} not recorded: {e}");
            return false;
        }
    };
    let changed = record_tip(git, tips, branch, &tip);
    if !changed && tips.get(branch) != Some(tip.as_str()) {
        warn!("channel branch {branch} not recorded: {tip} is older than the recorded tip");
    }
    changed
}

/// What `hinteros-config-sync` does at boot, for a rollback from Settings: make the managed
/// files of `config_dir` equal the generation's snapshot (other files, e.g. facter.json, stay),
/// except a time zone in the live `state.json`, which is kept (`time_zone.set` changes it in
/// place, without a generation; time-zone.md). The caller holds [`crate::state::lock`].
pub fn sync_config(
    snapshot: &Path,
    config_dir: &Path,
    scratch: &Path,
    tag: &str,
) -> std::io::Result<()> {
    if !snapshot.is_dir() {
        info!("the target generation has no config snapshot; leaving the config as it is");
        return Ok(());
    }
    fsutil::remove(scratch)?;
    if config_dir.is_dir() {
        fsutil::copy_dir(config_dir, scratch)?;
    } else {
        fs::DirBuilder::new().mode(0o755).create(scratch)?;
    }
    for f in MANAGED {
        let _ = fs::remove_file(scratch.join(f));
        let src = snapshot.join(f);
        if src.is_file() {
            fs::copy(&src, scratch.join(f))?;
            fs::set_permissions(
                scratch.join(f),
                std::os::unix::fs::PermissionsExt::from_mode(0o644),
            )?;
        }
    }
    let live = config_dir.join("state.json");
    let staged = scratch.join("state.json");
    if staged.is_file()
        && crate::state::State::load(&live).is_ok_and(|(s, _)| s.settings.time_zone.is_some())
    {
        match crate::timezone::carry_live_zone(&live, &staged) {
            Ok(true) => info!("kept the live time zone"),
            Ok(false) => {}
            Err(e) => warn!("cannot keep the live time zone: {e}"),
        }
    }
    fsutil::replace_dir(scratch, config_dir, tag)?;
    fsutil::remove(scratch)
}

/// Whether `config_dir`'s `flake.nix` or `flake.lock` differs from the generation's `snapshot`
/// (`state.json` may differ in its live time zone). No snapshot: no. A stale `state.json` alone
/// (a settings-only job that switched but could not replace /etc/hinteros) is not healed here:
/// the service made this job's state from /etc/hinteros before the job ran, so only
/// hinteros-config-sync at the next boot can bring it in line (the failed job was reported).
fn config_behind(snapshot: &Path, config_dir: &Path) -> bool {
    ["flake.nix", "flake.lock"].iter().any(|f| {
        fs::read(snapshot.join(f))
            .is_ok_and(|want| fs::read(config_dir.join(f)).ok().as_ref() != Some(&want))
    })
}

/// Whether the new system has a different session daemon or system service package than the
/// old one (the launcher should then offer a session restart).
pub fn session_restart_required(old: Option<&Path>, new: &Path) -> bool {
    let Some(old) = old else { return false };
    ["hinteros-session", "hinteros-system"].iter().any(|bin| {
        let a = fs::canonicalize(old.join("sw/bin").join(bin)).ok();
        let b = fs::canonicalize(new.join("sw/bin").join(bin)).ok();
        a.is_some() && b.is_some() && a != b
    })
}

/// Apps an update adds and removes.
pub type CatalogChanges = (Vec<CatalogApp>, Vec<RemovedApp>);

/// Compare the target's `lib.catalogIds` with the running catalog. The target is an object of
/// id to `{name, kind?, status, statusReason?}` (or to just the status string); unknown fields
/// are ignored. New: an app (`kind` absent or `"app"`; a service, or a kind this build does not
/// know, is never in the Store) available in the target and not available now. Removed:
/// installed, and removed from or missing in the target. `None` if the target is not such an
/// object.
pub fn catalog_changes(
    current: Option<&hinteros_api::catalog::Index>,
    target: &serde_json::Value,
    installed: &[String],
) -> Option<CatalogChanges> {
    use hinteros_api::catalog::{Status, is_app_id};
    let map = target.as_object()?;
    let field = |v: &serde_json::Value, k: &str| -> Option<String> {
        v.get(k)
            .and_then(|s| s.as_str())
            .map(|s| s.chars().take(200).collect())
    };
    let status = |v: &serde_json::Value| -> Option<String> {
        v.as_str().map(str::to_owned).or_else(|| field(v, "status"))
    };
    let mut new = Vec::new();
    for (id, v) in map {
        if !is_app_id(id)
            || status(v).as_deref() != Some("available")
            || v.get("kind").is_some_and(|k| k.as_str() != Some("app"))
        {
            continue;
        }
        let available_now = current
            .and_then(|c| c.get(id))
            .is_some_and(|e| e.status == Status::Available);
        if !available_now {
            let name: String = field(v, "name").unwrap_or_else(|| id.clone());
            new.push(CatalogApp {
                id: id.clone(),
                name: name.chars().take(40).collect(),
            });
        }
    }
    let mut removed = Vec::new();
    for id in installed {
        let gone = match map.get(id) {
            None => true,
            Some(v) => status(v).as_deref() == Some("removed"),
        };
        if gone {
            let v = map.get(id);
            let name = v
                .and_then(|v| field(v, "name"))
                .or_else(|| current.and_then(|c| c.get(id)).map(|e| e.name.clone()))
                .unwrap_or_else(|| id.clone());
            removed.push(RemovedApp {
                id: id.clone(),
                name: name.chars().take(40).collect(),
                reason: v.and_then(|v| field(v, "statusReason")),
            });
        }
    }
    removed.sort_by(|a, b| a.id.cmp(&b.id));
    Some((new, removed))
}

/// Delete the removed apps' data in every profile home (after the switch; best effort, every
/// outcome logged to the job's journal).
fn delete_data(d: &job::DeleteData) {
    for home in &d.homes {
        for rel in &d.paths {
            match crate::datadel::delete_in_home(&home.dir, home.uid, rel) {
                Ok(crate::datadel::Deleted::Removed(n)) => info!(
                    user = %home.user, home = %home.dir.display(), path = %rel, entries = n,
                    "deleted app data"
                ),
                Ok(crate::datadel::Deleted::Missing) => info!(
                    user = %home.user, path = %rel, "no app data to delete"
                ),
                Err(e) => warn!(user = %home.user, path = %rel, "not deleting app data: {e}"),
            }
        }
    }
}

/// Read `terms-accepted.json` as a JSON object (other fields stay in it). Missing: `None`. A
/// damaged or newer file is an error: these records must not be lost.
pub fn read_terms(path: &Path) -> std::io::Result<Option<serde_json::Map<String, Value>>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    check_version(path, &bytes, TERMS_VERSION)?;
    let invalid = |e: String| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not a terms file: {e}", path.display()),
        )
    };
    let doc: serde_json::Map<String, Value> =
        serde_json::from_slice(&bytes).map_err(|e| invalid(e.to_string()))?;
    if !doc.get("accepted").is_some_and(Value::is_object) {
        return Err(invalid("no accepted".into()));
    }
    Ok(Some(doc))
}

/// Write `terms-accepted.json` (root-only, atomically); the installer writes the same format.
pub fn write_terms(path: &Path, doc: &serde_json::Map<String, Value>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    let mut bytes = serde_json::to_vec_pretty(doc)?;
    bytes.push(b'\n');
    fsutil::write_atomic(path, &bytes, 0o600)
}

/// Add accepted terms to `terms-accepted.json`: `{"version": 1, "accepted": {"<id>":
/// [{"version", "accepted_at" (RFC 3339 UTC), "by"}]}}`, a list per id so that accepting a new
/// version keeps the history. Under an `flock` on `terms-accepted.json.lock`: the service
/// (`apps.accept_terms`) and jobs both record, and no record may be lost.
pub fn record_terms(path: &Path, records: &[job::TermsRecord]) -> std::io::Result<()> {
    let _lock = fsutil::lock_beside(path)?;
    let mut doc = read_terms(path)?.unwrap_or_else(|| {
        let mut doc = serde_json::Map::new();
        doc.insert("version".into(), json!(TERMS_VERSION));
        doc.insert("accepted".into(), json!({}));
        doc
    });
    let accepted = doc["accepted"].as_object_mut().expect("read_terms checked");
    for r in records {
        let mut entry = json!({
            "version": r.version,
            "accepted_at": rfc3339(r.accepted_at),
            "by": r.by,
        });
        if let Some(p) = &r.profile {
            entry["profile"] = json!(p);
        }
        match accepted.entry(r.id.clone()).or_insert_with(|| json!([])) {
            Value::Array(list) => list.push(entry),
            other => *other = json!([entry]),
        }
    }
    write_terms(path, &doc)
}

/// The acceptances in `terms-accepted.json`: (id, version, by, profile). A file this daemon
/// cannot read has none (logged).
pub fn accepted_terms(path: &Path) -> Vec<(String, String, String, Option<String>)> {
    let doc = match read_terms(path) {
        Ok(Some(doc)) => doc,
        Ok(None) => return Vec::new(),
        Err(e) => {
            warn!("{e}");
            return Vec::new();
        }
    };
    let s = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_owned);
    doc["accepted"]
        .as_object()
        .into_iter()
        .flatten()
        .flat_map(|(id, list)| {
            list.as_array().into_iter().flatten().filter_map(move |r| {
                Some((
                    id.clone(),
                    s(r, "version")?,
                    s(r, "by").unwrap_or_default(),
                    s(r, "profile"),
                ))
            })
        })
        .collect()
}

/// Unix seconds as RFC 3339 UTC (`2026-10-06T12:00:00Z`).
pub fn rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_runner(d: &Path) -> Runner {
        let paths = crate::config::Paths {
            config_dir: d.join("etc"),
            state_dir: d.join("var"),
            run_dir: d.join("run"),
            profile: d.join("profiles/system"),
            current_system: d.join("current"),
            booted_system: d.join("booted"),
            boot_entries: d.join("boot/loader/entries"),
            apps_profiles: d.join("nixprofiles"),
            catalog_index: None,
            users: Default::default(),
            proc_dir: "/proc".into(),
            zoneinfo_dir: d.join("zoneinfo"),
            localtime: d.join("localtime"),
            server: Default::default(),
        };
        let tools = crate::config::Tools {
            nix: "nix".into(),
            nix_env: "nix-env".into(),
            git: "git".into(),
            ssh_keygen: "ssh-keygen".into(),
            nixos_facter: "nixos-facter".into(),
            nix_store: "nix-store".into(),
        };
        let spec = JobSpec {
            id: "0123456789abcdef".into(),
            kind: hinteros_api::system::JobKind::Rebuild,
            uid: 1000,
            created_at: 0,
            paths,
            tools,
            bus: None,
            action: Action::Apply(Apply::default()),
            subject: JobSubject::default(),
        };
        Runner::new(spec, d.to_owned())
    }

    fn written(d: &Path) -> Vec<ProgressLine> {
        fs::read_to_string(d.join(job::PROGRESS))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Progress within a stage is rate limited but never lost; while a child prints output
    /// and nothing countable changes, the last line is repeated every 2 s with the new
    /// activity time.
    #[test]
    fn progress_rate_limit_and_heartbeat() {
        let d = tempfile::tempdir().unwrap();
        let mut r = test_runner(d.path());
        let ms = |n: u64| Duration::from_millis(n);
        let t0 = Instant::now();
        let line = |p: u8| ProgressLine {
            stage: Stage::Building,
            percent: Some(p),
            detail: None,
            activity_at: None,
            subject: None,
        };
        r.report_at(line(10), t0);
        r.report_at(line(20), t0 + ms(100));
        assert_eq!(written(d.path()).len(), 1, "held back");
        r.tick_at(t0 + ms(200));
        assert_eq!(written(d.path()).len(), 1);
        r.tick_at(t0 + ms(250));
        assert_eq!(
            written(d.path())[1].percent,
            Some(20),
            "written once allowed"
        );
        // No output: no heartbeat.
        r.tick_at(t0 + ms(3000));
        assert_eq!(written(d.path()).len(), 2);
        // Output (nix compiling): the last line again, with the activity time.
        r.activity_at = Some(123);
        r.tick_at(t0 + ms(3100));
        let lines = written(d.path());
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2].percent, Some(20));
        assert_eq!(lines[2].activity_at, Some(123));
        // No new output since: nothing more.
        r.tick_at(t0 + ms(4000));
        r.tick_at(t0 + ms(5000));
        assert_eq!(written(d.path()).len(), 3);
        r.activity_at = Some(125);
        r.tick_at(t0 + ms(5000));
        assert_eq!(
            written(d.path()).len(),
            3,
            "less than 2 s since the last line"
        );
        r.tick_at(t0 + ms(5100));
        assert_eq!(written(d.path())[3].activity_at, Some(125));
        // A stage change is written at once; a subject update too, with the last values.
        r.report_at(
            ProgressLine {
                stage: Stage::Activating,
                ..line(0)
            },
            t0 + ms(5150),
        );
        r.set_subject(|s| s.rev = Some("c6b69ce".into()));
        let lines = written(d.path());
        assert_eq!(lines[4].stage, Stage::Activating);
        assert_eq!(lines[5].stage, Stage::Activating);
        assert_eq!(
            lines[5].subject.as_ref().unwrap().rev.as_deref(),
            Some("c6b69ce")
        );
        r.set_subject(|s| s.rev = Some("c6b69ce".into()));
        assert_eq!(written(d.path()).len(), 6, "no change, no line");
    }

    #[test]
    fn catalog_changes_between_generations() {
        let current = hinteros_api::catalog::Index::parse(
            br#"{"indexVersion":4,"entries":[
              {"id":"steam","name":"Steam","status":"available"},
              {"id":"old","name":"Old","status":"available"},
              {"id":"later","name":"Later","status":"retired","statusReason":"x"}]}"#,
            "/".into(),
        )
        .unwrap();
        let target = json!({
            "steam": {"name": "Steam", "status": "available"},
            "old": {"name": "Old", "status": "removed", "statusReason": "Discontinued."},
            "later": {"name": "Later", "status": "available"},
            "moonlight": {"name": "Moonlight", "status": "available", "kind": "app", "later": 1},
            // Services, and kinds a later catalog adds, are not new apps.
            "vpn": {"name": "VPN", "status": "available", "kind": "service"},
            "gadget": {"name": "Gadget", "status": "available", "kind": "widget"},
            "hidden": "retired",
            "BAD": "available"
        });
        let installed = vec!["steam".to_string(), "old".into(), "vanished".into()];
        let (new, removed) = catalog_changes(Some(&current), &target, &installed).unwrap();
        let ids: Vec<&str> = new.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["later", "moonlight"]);
        assert_eq!(new[1].name, "Moonlight");
        assert_eq!(
            serde_json::to_value(&removed).unwrap(),
            json!([
                {"id": "old", "name": "Old", "reason": "Discontinued."},
                {"id": "vanished", "name": "vanished", "reason": null}
            ])
        );
        assert!(catalog_changes(None, &json!([]), &installed).is_none());
        // Without a current index every available target app is new.
        let (new, _) = catalog_changes(None, &json!({"a": "available"}), &[]).unwrap();
        assert_eq!(new[0].name, "a");
    }

    /// A non-UTF-8 stderr line does not stop the reader: the child would die of SIGPIPE on
    /// its next line.
    #[test]
    fn exec_reads_non_utf8_stderr() {
        let d = tempfile::tempdir().unwrap();
        let mut r = test_runner(d.path());
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            r"printf 'bad \377\n' >&2; sleep 0.3
              i=0; while [ $i -lt 2000 ]; do echo line $i >&2; i=$((i+1)); done; echo out",
        ]);
        let mut lines = Vec::new();
        let out = r
            .exec(cmd, false, &mut |_, l| lines.push(l.to_owned()))
            .unwrap();
        assert_eq!(out, "out\n");
        assert_eq!(lines[0], "bad \u{FFFD}");
        assert_eq!(lines.len(), 2001);
        assert_eq!(lines[2000], "line 1999");
    }

    /// A process the child leaves behind with its pipes does not hold the job forever.
    #[test]
    fn exec_stops_leftover_processes() {
        let d = tempfile::tempdir().unwrap();
        let mut r = test_runner(d.path());
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 60 & echo done"]);
        let t0 = Instant::now();
        let out = r.exec(cmd, false, &mut |_, _| {}).unwrap();
        assert_eq!(out, "done\n");
        assert!(t0.elapsed() < Duration::from_secs(30), "{:?}", t0.elapsed());
    }

    /// A rollback restores the snapshot's managed files, but keeps a live time zone (as the
    /// boot-time sync does); other files stay.
    #[test]
    fn sync_config_keeps_the_live_time_zone() {
        let d = tempfile::tempdir().unwrap();
        let (config, snapshot) = (d.path().join("etc"), d.path().join("snap"));
        fs::create_dir_all(&config).unwrap();
        fs::create_dir_all(&snapshot).unwrap();
        let state = |zone: Option<&str>| {
            let mut s = crate::state::State::parse(include_bytes!(
                "../tests/fixtures/state/vm-system-a.json"
            ))
            .unwrap();
            s.settings.time_zone = zone.map(str::to_owned);
            s.to_canonical_json()
        };
        let zone = |dir: &Path| {
            crate::state::State::load(&dir.join("state.json"))
                .unwrap()
                .0
                .settings
                .time_zone
        };
        fs::write(snapshot.join("flake.lock"), "old").unwrap();
        fs::write(snapshot.join("state.json"), state(Some("America/Chicago"))).unwrap();
        fs::write(config.join("flake.lock"), "new").unwrap();
        fs::write(config.join("facter.json"), "{}").unwrap();
        fs::write(config.join("state.json"), state(Some("Europe/Berlin"))).unwrap();
        let scratch = d.path().join("scratch");
        sync_config(&snapshot, &config, &scratch, "t").unwrap();
        assert_eq!(
            fs::read_to_string(config.join("flake.lock")).unwrap(),
            "old"
        );
        assert!(config.join("facter.json").exists());
        assert_eq!(zone(&config).as_deref(), Some("Europe/Berlin"));
        // No live zone: the snapshot's.
        fs::write(config.join("state.json"), state(None)).unwrap();
        sync_config(&snapshot, &config, &scratch, "t").unwrap();
        assert_eq!(zone(&config).as_deref(), Some("America/Chicago"));
    }

    #[test]
    fn config_behind_its_snapshot() {
        let d = tempfile::tempdir().unwrap();
        let (config, snapshot) = (d.path().join("etc"), d.path().join("snap"));
        assert!(!config_behind(&snapshot, &config), "no snapshot");
        fs::create_dir_all(&config).unwrap();
        fs::create_dir_all(&snapshot).unwrap();
        for dir in [&config, &snapshot] {
            fs::write(dir.join("flake.nix"), "{}").unwrap();
            fs::write(dir.join("flake.lock"), "r2").unwrap();
        }
        fs::write(config.join("state.json"), "live zone").unwrap();
        assert!(!config_behind(&snapshot, &config));
        fs::write(config.join("flake.lock"), "r1").unwrap();
        assert!(config_behind(&snapshot, &config));
        fs::remove_file(config.join("flake.lock")).unwrap();
        assert!(config_behind(&snapshot, &config));
    }

    #[test]
    fn rfc3339_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_791_300_000), "2026-10-06T15:20:00Z");
    }

    #[test]
    fn terms_record_keeps_history() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("terms-accepted.json");
        // What the installer may already have written.
        fs::write(
            &path,
            r#"{"version":1,"later":1,"accepted":{"youtube":[{"version":"1","accepted_at":"2026-01-01T00:00:00Z","by":"installer"}]}}"#,
        )
        .unwrap();
        let rec = |v: &str| job::TermsRecord {
            id: "steam".into(),
            version: v.into(),
            accepted_at: 1_791_300_000,
            by: "hinteros-p1".into(),
            profile: None,
        };
        record_terms(&path, &[rec("2026-09-10")]).unwrap();
        record_terms(&path, &[rec("2027-01-01")]).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["accepted"]["youtube"][0]["by"], "installer");
        assert_eq!(v["later"], 1);
        assert_eq!(
            v["accepted"]["steam"],
            json!([
                {"version": "2026-09-10", "accepted_at": "2026-10-06T15:20:00Z", "by": "hinteros-p1"},
                {"version": "2027-01-01", "accepted_at": "2026-10-06T15:20:00Z", "by": "hinteros-p1"}
            ])
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // A damaged file is not overwritten.
        fs::write(&path, "[]").unwrap();
        assert!(record_terms(&path, &[rec("x")]).is_err());
        // A newer file is read as no acceptances, and not overwritten either.
        let newer = r#"{"version":2,"accepted":{"steam":[{"version":"1"}]}}"#;
        fs::write(&path, newer).unwrap();
        assert!(accepted_terms(&path).is_empty());
        let e = record_terms(&path, &[rec("x")]).unwrap_err();
        assert!(e.to_string().contains("has version 2"), "{e}");
        assert_eq!(fs::read_to_string(&path).unwrap(), newer);
    }

    #[test]
    fn terms_recorded_at_the_same_time_are_all_kept() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("terms-accepted.json");
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let rec = job::TermsRecord {
                        id: format!("app{i}"),
                        version: "1".into(),
                        accepted_at: 0,
                        by: "hinteros-p1".into(),
                        profile: None,
                    };
                    record_terms(&path, &[rec]).unwrap();
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(accepted_terms(&path).len(), 8);
    }
}
