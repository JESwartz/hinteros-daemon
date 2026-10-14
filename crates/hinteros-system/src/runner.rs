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

use hinteros_api::system::{
    AppliedResult, CatalogApp, JobResult, JobState, RemovedApp, Stage, UpdateCheckResult, is_job_id,
};
use hinteros_api::{ErrorCode, ErrorObject};
use rustix::fs::{FlockOperation, flock};
use rustix::process::{Pid, Signal, kill_process_group};
use serde_json::json;
use tracing::{error, info, warn};

use crate::flake;
use crate::fsutil;
use crate::generations;
use crate::job::{self, Action, Apply, JobSpec, NotFastForward, ProgressLine, Snapshot, Status};
use crate::nixlog;
use crate::update::{Git, GitError, Rejection};

mod apps;
mod profiles;
mod storage;

/// Files of a config dir that belong to a generation (the snapshot; device-config.nix).
pub const MANAGED: &[&str] = &["flake.nix", "flake.lock", "state.json"];

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
        Action::Apps(j) => r.apps_job(&j),
        Action::MigrateApps(m) => r.migrate(&m),
        Action::ProfileCreate(j) => r.profile_create(&j),
        Action::ProfileRemove(j) => r.profile_remove(&j),
        Action::GuestEnd(j) => r.guest_end(&j),
        Action::StorageMeasure(j) => r.storage_measure(&j),
        Action::Cleanup(j) => r.cleanup(&j),
    };
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
        },
        Err(Failure::Cancelled) => {
            let _ = fsutil::remove(&staging);
            let stage = r.stage.unwrap_or(Stage::Preparing);
            Status {
                state: JobState::Cancelled,
                stage: r.stage,
                error: Some(ErrorObject {
                    message: "cancelled".into(),
                    ..job::job_failed(stage, "cancelled")
                }),
                result: None,
                session_restart_required: false,
                finished_at: job::now(),
                before,
                after: None,
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
            }
        }
    };
    let _ = fs::remove_file(dir.join(job::ACTIVATING));
    fsutil::write_atomic(&dir.join(job::STATUS), &serde_json::to_vec(&status)?, 0o600)?;
    info!(state = ?status.state, "job finished");
    drop(lock);
    Ok(())
}

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
    last_progress: Option<(Instant, ProgressLine)>,
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
            spec,
            dir,
            stage: None,
            progress,
            last_progress: None,
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
            });
        }
    }

    fn report(&mut self, mut line: ProgressLine) {
        if let Some(s) = self.stage_override {
            line.stage = s;
        }
        let now = Instant::now();
        // Stage changes always; progress within a stage at most 4 times a second.
        if let Some((at, last)) = &self.last_progress
            && last.stage == line.stage
            && now.duration_since(*at) < Duration::from_millis(250)
        {
            return;
        }
        if let Some(f) = self.progress.as_mut()
            && let Ok(mut text) = serde_json::to_string(&line)
        {
            text.push('\n');
            let _ = f.write_all(text.as_bytes());
        }
        self.last_progress = Some((now, line));
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
        let out_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.by_ref().take(limit).read_to_end(&mut buf);
            let _ = std::io::copy(&mut stdout, &mut std::io::sink());
            buf
        });
        let (tx, rx) = mpsc::channel::<String>();
        let err_thread = std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut tail: Vec<String> = Vec::new();
        let mut cancelled = false;
        let status = loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => {
                    if !line.starts_with("@nix ") {
                        eprintln!("{line}");
                        if !line.trim().is_empty() {
                            tail.push(line.chars().take(400).collect());
                            if tail.len() > 20 {
                                tail.remove(0);
                            }
                        }
                    }
                    on_line(self, &line);
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {}
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
                Ok(Some(status)) => {
                    // Drain what is left of stderr.
                    while let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
                        if !line.starts_with("@nix ") && !line.trim().is_empty() {
                            tail.push(line.chars().take(400).collect());
                        }
                        on_line(self, &line);
                    }
                    break status;
                }
                Ok(None) => {}
                Err(e) => return Err(self.fail(format!("waiting for {program}: {e}"))),
            }
        };
        let _ = err_thread.join();
        let out = out_thread.join().unwrap_or_default();
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

    fn git_error(&self, source: &job::UpdateSource, e: GitError, nff: NotFastForward) -> Failure {
        match e {
            GitError::Unreachable(msg) => {
                warn!("update server unreachable: {msg}");
                Failure::Error(ErrorObject::with_data(
                    ErrorCode::UpdateUnreachable,
                    "cannot reach the update server",
                    json!({ "host": source.host }),
                ))
            }
            GitError::Rejected(Rejection::NotFastForward)
                if nff == NotFastForward::NeedsAcknowledgement =>
            {
                Failure::Error(ErrorObject::new(
                    ErrorCode::NeedsAcknowledgement,
                    "the target revision does not descend from the installed one",
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
        let mut stages = Vec::new();
        let checked = git.check(
            &source.repo,
            &source.branch,
            source.current_rev.as_deref(),
            true,
            &source.allowed_signers,
            &mut |s| stages.push(s),
        );
        for s in stages {
            self.set_stage(s);
        }
        let mut available =
            checked.map_err(|e| self.git_error(&source, e, NotFastForward::Reject))?;
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
        if let Ok(bytes) = serde_json::to_vec(&result) {
            let _ = fs::create_dir_all(&self.spec.paths.state_dir);
            if let Err(e) = fsutil::write_atomic(&self.spec.paths.last_check(), &bytes, 0o600) {
                warn!("cannot store the check result: {e}");
            }
        }
        Ok((Some(JobResult::UpdateCheck(result)), false))
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

        // Fetch and verify the target revision.
        let mut lock_url = None;
        if let Some(u) = &a.update {
            self.check_cancel()?;
            self.set_stage(Stage::Fetching);
            let git = self.git(&u.source);
            let fetched = git
                .init()
                .and_then(|()| git.fetch(&u.source.repo, &u.source.branch));
            fetched.map_err(|e| self.git_error(&u.source, e, u.not_fast_forward))?;
            self.check_cancel()?;
            self.set_stage(Stage::Verifying);
            let rev = match &u.rev {
                Some(r) => r.clone(),
                None => git
                    .ls_remote(&u.source.repo, &u.source.branch)
                    .map_err(|e| self.git_error(&u.source, e, u.not_fast_forward))?,
            };
            let require_ff = u.not_fast_forward != NotFastForward::Allow;
            git.describe(
                &rev,
                u.source.current_rev.as_deref(),
                require_ff,
                &u.source.allowed_signers,
            )
            .map_err(|e| self.git_error(&u.source, e, u.not_fast_forward))?;
            lock_url = Some(format!("{}&rev={rev}", u.input_url));
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
                let stage = progress.stage.unwrap_or(Stage::Evaluating);
                let detail = progress.detail();
                r.set_stage(stage);
                r.report(ProgressLine {
                    stage,
                    percent: progress.percent(),
                    detail: (!detail.is_empty()).then_some(detail),
                });
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

        // Activate.
        self.check_cancel()?;
        self.enter_activation()?;
        if let Err(e) = self.activate(&toplevel) {
            self.restore(old_toplevel.as_deref());
            return Err(e);
        }

        // Finish: /etc/hinteros now describes the running system.
        self.set_stage(Stage::Finishing);
        fsutil::replace_dir(&staging, &paths.config_dir, &self.spec.id)
            .map_err(|e| self.fail(format!("replacing {}: {e}", paths.config_dir.display())))?;
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

    /// After a failed activation: point the profile back at the old system and make it the
    /// boot default again (best effort; the failure is reported either way).
    fn restore(&mut self, old: Option<&Path>) {
        let Some(old) = old else { return };
        warn!(
            "activation failed; restoring the profile to {}",
            old.display()
        );
        let mut cmd = Command::new(&self.spec.tools.nix_env);
        cmd.arg("-p")
            .arg(&self.spec.paths.profile)
            .arg("--set")
            .arg(old);
        if self.exec(cmd, false, &mut |_, _| {}).is_ok() {
            let mut cmd = Command::new(old.join("bin/switch-to-configuration"));
            cmd.arg("boot");
            let _ = self.exec(cmd, false, &mut |_, _| {});
        }
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
        let mut cmd = Command::new(&self.spec.tools.nix_env);
        cmd.arg("-p")
            .arg(&paths.profile)
            .arg("--switch-generation")
            .arg(generation.to_string());
        self.exec(cmd, false, &mut |_, _| {})?;
        let mut cmd = Command::new(target.join("bin/switch-to-configuration"));
        cmd.arg("switch");
        if let Err(e) = self.exec(cmd, false, &mut |_, _| {}) {
            if let Some(old) = old_generation {
                let mut cmd = Command::new(&self.spec.tools.nix_env);
                cmd.arg("-p")
                    .arg(&paths.profile)
                    .arg("--switch-generation")
                    .arg(old.to_string());
                if self.exec(cmd, false, &mut |_, _| {}).is_ok()
                    && let Some(old) = &old_toplevel
                {
                    let mut cmd = Command::new(old.join("bin/switch-to-configuration"));
                    cmd.arg("boot");
                    let _ = self.exec(cmd, false, &mut |_, _| {});
                }
            }
            return Err(e);
        }
        self.set_stage(Stage::Finishing);
        sync_config(
            &target.join("hinteros"),
            &paths.config_dir,
            &paths.staging(),
            &self.spec.id,
        )
        .map_err(|e| self.fail(format!("restoring the config snapshot: {e}")))?;
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

/// What `hinteros-config-sync` does at boot, for a rollback from Settings: make the managed
/// files of `config_dir` equal the generation's snapshot (other files, e.g. facter.json, stay).
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
    fsutil::replace_dir(scratch, config_dir, tag)?;
    fsutil::remove(scratch)
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

/// Compare the target's `lib.catalogIds` with the running catalog. The target is an object of
/// id to `{name, status, statusReason?}` (or to just the status string). New: available in the
/// target and not available now. Removed: installed, and removed from or missing in the
/// target. `None` if the target is not such an object.
/// Apps an update adds and removes.
pub type CatalogChanges = (Vec<CatalogApp>, Vec<RemovedApp>);

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
        if !is_app_id(id) || status(v).as_deref() != Some("available") {
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

/// Add accepted terms to `terms-accepted.json` (root-only):
/// `{"<id>": [{"version", "accepted_at" (RFC 3339 UTC), "by"}]}`, a list per id so that
/// accepting a new version keeps the history. Written atomically; the installer writes the
/// same format.
pub fn record_terms(path: &Path, records: &[job::TermsRecord]) -> std::io::Result<()> {
    let mut doc: serde_json::Map<String, serde_json::Value> = match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} is not a JSON object: {e}", path.display()),
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Default::default(),
        Err(e) => return Err(e),
    };
    for r in records {
        let mut entry = json!({
            "version": r.version,
            "accepted_at": rfc3339(r.accepted_at),
            "by": r.by,
        });
        if let Some(p) = &r.profile {
            entry["profile"] = json!(p);
        }
        match doc.entry(r.id.clone()).or_insert_with(|| json!([])) {
            serde_json::Value::Array(list) => list.push(entry),
            other => *other = json!([entry]),
        }
    }
    if let Some(parent) = path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    let mut bytes = serde_json::to_vec_pretty(&doc)?;
    bytes.push(b'\n');
    fsutil::write_atomic(path, &bytes, 0o600)
}

/// The acceptances in `terms-accepted.json`: (id, version, by, profile).
pub fn accepted_terms(path: &Path) -> Vec<(String, String, String, Option<String>)> {
    let Some(doc) = fs::read(path).ok().and_then(|b| {
        serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&b).ok()
    }) else {
        return Vec::new();
    };
    let s = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_owned);
    doc.iter()
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

    #[test]
    fn catalog_changes_between_generations() {
        let current = hinteros_api::catalog::Index::parse(
            br#"{"indexVersion":2,"apps":[
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
            "moonlight": {"name": "Moonlight", "status": "available"},
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
            r#"{"youtube":[{"version":"1","accepted_at":"2026-01-01T00:00:00Z","by":"installer"}]}"#,
        )
        .unwrap();
        let rec = |v: &str| job::TermsRecord {
            id: "steam".into(),
            version: v.into(),
            accepted_at: 1_791_300_000,
            by: "hinteros".into(),
            profile: None,
        };
        record_terms(&path, &[rec("2026-09-10")]).unwrap();
        record_terms(&path, &[rec("2027-01-01")]).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["youtube"][0]["by"], "installer");
        assert_eq!(
            v["steam"],
            json!([
                {"version": "2026-09-10", "accepted_at": "2026-10-06T15:20:00Z", "by": "hinteros"},
                {"version": "2027-01-01", "accepted_at": "2026-10-06T15:20:00Z", "by": "hinteros"}
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
    }
}
