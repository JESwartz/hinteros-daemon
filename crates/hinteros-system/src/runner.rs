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
    AppliedResult, JobResult, JobState, Stage, UpdateCheckResult, is_job_id,
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
        Action::Apply(a) => r.apply(&a),
        Action::Rollback { generation } => r.rollback(generation),
        Action::UpdateCheck(_) => r.update_check(),
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
        if self.stage != Some(stage) {
            self.stage = Some(stage);
            self.report(ProgressLine {
                stage,
                percent: None,
                detail: None,
            });
        }
    }

    fn report(&mut self, line: ProgressLine) {
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
        mut cmd: Command,
        cancellable: bool,
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
            let _ = stdout.by_ref().take(1 << 20).read_to_end(&mut buf);
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
        let available = checked.map_err(|e| self.git_error(&source, e, NotFastForward::Reject))?;
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

    fn apply(&mut self, a: &Apply) -> Outcome {
        self.set_stage(Stage::Preparing);
        self.inhibit("delay");
        let paths = self.spec.paths.clone();
        let tools = self.spec.tools.clone();
        if !paths.config_dir.join("flake.nix").is_file() {
            return Err(self.fail(format!("{} has no flake.nix", paths.config_dir.display())));
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
        let after = self.snapshot();
        let restart = session_restart_required(old_toplevel.as_deref(), &toplevel);
        Ok((
            Some(JobResult::Applied(AppliedResult {
                generation: after.generation,
                rev: after.rev,
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
