//! The service's view of jobs: one mutating job at a time, started as a transient instance of
//! `hinteros-job@.service` over systemd's D-Bus API, followed through its job directory, and
//! re-attached after a restart of the service (a switch restarts it while the job runs on; after a
//! rollback the older service reads what it can of a later version's job and waits for the rest).

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hinteros_api::system::{
    EventName, Job, JobFinishedParams, JobKind, JobProgressParams, JobStartedParams, JobState,
    JobSubject, Stage, is_job_id,
};
use hinteros_api::{ErrorCode, ErrorObject};
use rustix::fs::{FlockOperation, flock};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use crate::config::Paths;
use crate::fsutil;
use crate::job::{self, JobSpec, ProgressLine, Status};

/// Finished jobs kept (in `/run`, so until reboot).
pub const KEEP_FINISHED: usize = 10;
/// A job that has not taken its lock this long after being created is considered lost.
const START_GRACE_S: i64 = 60;
/// While a job runs, a `job_progress` at least this often (a heartbeat repeats the last one).
pub const HEARTBEAT: Duration = Duration::from_secs(2);

struct Active {
    id: String,
    kind: JobKind,
    uid: u32,
    created_at: i64,
    offset: u64,
    stage: Option<Stage>,
    subject: JobSubject,
    /// The latest progress (what a heartbeat repeats and `jobs.get` shows).
    last: Option<JobProgressParams>,
    /// When the last `job_progress` was emitted.
    emitted_at: Option<Instant>,
}

impl Active {
    fn new(spec: &JobSpec) -> Self {
        Self::of(&spec.id, Head::from(spec), spec.kind)
    }

    fn of(id: &str, head: Head, kind: JobKind) -> Self {
        Self {
            id: id.to_owned(),
            kind,
            uid: head.uid,
            created_at: head.created_at,
            offset: 0,
            stage: None,
            subject: head.subject.unwrap_or_default(),
            last: None,
            emitted_at: None,
        }
    }
}

/// What the service needs of a job's `spec.json`, read leniently: the job unit runs on through
/// the switch it makes, so after a rollback an older service reads a later version's spec (the
/// job reads its full [`JobSpec`] itself, strictly: the same binary wrote it).
#[derive(Debug, Clone, Default, Deserialize)]
struct Head {
    #[serde(default)]
    uid: u32,
    #[serde(default)]
    created_at: i64,
    /// `None`: a kind this binary does not know.
    #[serde(default, deserialize_with = "job::lenient")]
    kind: Option<JobKind>,
    #[serde(default, deserialize_with = "job::lenient")]
    subject: Option<JobSubject>,
}

impl From<&JobSpec> for Head {
    fn from(spec: &JobSpec) -> Self {
        Self {
            uid: spec.uid,
            created_at: spec.created_at,
            kind: Some(spec.kind),
            subject: Some(spec.subject.clone()),
        }
    }
}

impl Head {
    fn finished(&self, id: &str, kind: JobKind, status: &Status) -> Job {
        finished_job(
            id,
            kind,
            self.uid,
            self.created_at,
            &self.subject.clone().unwrap_or_default(),
            status,
        )
    }
}

/// A finished job, for the audit log.
pub struct Finished {
    pub job: Job,
    pub status: Status,
}

pub struct Jobs {
    paths: Paths,
    active: Option<Active>,
    finished: VecDeque<Job>,
    /// An unfinished job this binary cannot follow (its spec is a later version's, after a
    /// rollback, or damaged) while its process holds the lock: nothing else starts meanwhile.
    foreign: Option<String>,
}

impl Jobs {
    /// Load the job directories left by a previous instance.
    pub fn load(paths: Paths) -> Self {
        let mut jobs = Self {
            paths,
            active: None,
            finished: VecDeque::new(),
            foreign: None,
        };
        let dir = jobs.paths.jobs_dir();
        let _ = fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir);
        let mut found: Vec<(i64, String, Option<Head>)> = fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let id = e.file_name().to_str()?.to_owned();
                if !is_job_id(&id) {
                    return None;
                }
                let head: Option<Head> = job::read_json(&e.path().join(job::SPEC));
                Some((head.as_ref().map_or(0, |h| h.created_at), id, head))
            })
            .collect();
        found.sort_by_key(|(t, _, _)| *t);
        for (_, id, head) in found {
            let jd = dir.join(&id);
            let kind = head.as_ref().and_then(|h| h.kind);
            match (job::read_json::<Status>(&jd.join(job::STATUS)), kind, head) {
                (Some(status), Some(kind), Some(head)) => {
                    jobs.push_finished(head.finished(&id, kind, &status));
                }
                // Finished, but not a job this binary can show; its directory stays until reboot.
                _ if jd.join(job::STATUS).exists() => {}
                (None, Some(kind), Some(head)) => {
                    if let Some(prev) = jobs.active.take() {
                        // Two unfinished jobs: the older one is lost.
                        jobs.lose(&prev.id, prev.stage, "superseded");
                    }
                    jobs.active = Some(Active::of(&id, head, kind));
                }
                _ => {
                    if locked(&jd.join(job::LOCK)) {
                        warn!(job = %id, "a running job this version cannot read; waiting for it");
                        jobs.foreign = Some(id);
                    }
                }
            }
        }
        jobs.prune();
        jobs
    }

    fn dir(&self, id: &str) -> PathBuf {
        self.paths.jobs_dir().join(id)
    }

    /// The running job, if any (also one this binary cannot follow).
    pub fn busy(&self) -> Option<&str> {
        self.active
            .as_ref()
            .map(|a| a.id.as_str())
            .or(self.foreign.as_deref())
    }

    pub fn busy_error(&self) -> Option<ErrorObject> {
        self.busy().map(|id| {
            ErrorObject::with_data(
                ErrorCode::JobBusy,
                "another job is running",
                json!({ "job_id": id }),
            )
        })
    }

    /// Write a new job's directory and make it the active job (the caller starts the unit).
    pub fn create(&mut self, spec: &JobSpec) -> std::io::Result<()> {
        if let Some(e) = self.busy() {
            return Err(std::io::Error::other(format!("job {e} is running")));
        }
        let dir = self.dir(&spec.id);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(self.paths.jobs_dir())?;
        fs::DirBuilder::new().mode(0o700).create(&dir)?;
        let bytes = serde_json::to_vec_pretty(spec).map_err(std::io::Error::other)?;
        fsutil::write_atomic(&dir.join(job::SPEC), &bytes, 0o600)?;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join(job::PROGRESS))?;
        self.active = Some(Active::new(spec));
        Ok(())
    }

    /// The unit could not be started: forget the job.
    pub fn abandon(&mut self, id: &str) {
        if self.busy() == Some(id) {
            self.active = None;
        }
        let _ = fsutil::remove(&self.dir(id));
    }

    pub fn started_event(spec: &JobSpec) -> (EventName, Value) {
        let p = JobStartedParams {
            job_id: spec.id.clone(),
            kind: spec.kind,
            uid: spec.uid,
            subject: spec.subject.clone(),
            started_at: spec.created_at,
        };
        (
            EventName::JobStarted,
            serde_json::to_value(p).unwrap_or_default(),
        )
    }

    /// Follow the active job: new progress lines become `job_progress`, a status (or a job
    /// that died without one) `job_finished`.
    pub fn poll(&mut self) -> (Vec<(EventName, Value)>, Option<Finished>) {
        self.poll_at(Instant::now())
    }

    /// [`Self::poll`] at `now` (heartbeats are due [`HEARTBEAT`] after the last `job_progress`).
    pub fn poll_at(&mut self, now: Instant) -> (Vec<(EventName, Value)>, Option<Finished>) {
        let mut events = Vec::new();
        if let Some(id) = &self.foreign
            && !locked(&self.dir(id).join(job::LOCK))
        {
            self.foreign = None;
        }
        let Some(active) = self.active.as_mut() else {
            return (events, None);
        };
        let dir = self.paths.jobs_dir().join(&active.id);
        // Progress lines: emit stage changes and the latest line.
        let mut lines: Vec<ProgressLine> = Vec::new();
        if let Ok(mut f) = fs::File::open(dir.join(job::PROGRESS))
            && f.seek(SeekFrom::Start(active.offset)).is_ok()
        {
            let mut reader = BufReader::new(f);
            let mut buf = String::new();
            while let Ok(n) = reader.read_line(&mut buf) {
                if n == 0 || !buf.ends_with('\n') {
                    break;
                }
                active.offset += n as u64;
                if let Ok(line) = serde_json::from_str::<ProgressLine>(buf.trim_end()) {
                    lines.push(line);
                }
                buf.clear();
            }
        }
        let last = lines.len().saturating_sub(1);
        let mut subject_changed = false;
        for (i, line) in lines.into_iter().enumerate() {
            let stage_change = active.stage != Some(line.stage);
            active.stage = Some(line.stage);
            if let Some(s) = line.subject
                && s != active.subject
            {
                active.subject = s;
                subject_changed = true;
            }
            let p = JobProgressParams {
                job_id: active.id.clone(),
                kind: active.kind,
                stage: line.stage,
                percent: line.percent,
                detail: line.detail,
                started_at: active.created_at,
                last_activity_at: line
                    .activity_at
                    .or(active.last.as_ref().and_then(|l| l.last_activity_at)),
                subject: None,
            };
            if stage_change || i == last {
                let mut p = p.clone();
                if subject_changed {
                    p.subject = Some(active.subject.clone());
                    subject_changed = false;
                }
                events.push((
                    EventName::JobProgress,
                    serde_json::to_value(p).unwrap_or_default(),
                ));
                active.emitted_at = Some(now);
            }
            active.last = Some(p);
        }

        let status = match job::read_json::<Status>(&dir.join(job::STATUS)) {
            Some(s) => Some(s),
            None if !locked(&dir.join(job::LOCK)) => {
                // Not running and no status: still starting, or lost.
                let starting =
                    !dir.join(job::PID).exists() && job::now() - active.created_at < START_GRACE_S;
                if starting {
                    None
                } else if let Some(s) = job::read_json::<Status>(&dir.join(job::STATUS)) {
                    Some(s) // finished between the two reads
                } else if dir.join(job::STATUS).exists() {
                    // Written, but this binary cannot read it: never overwrite the outcome.
                    Some(Status::lost(
                        active.stage,
                        "the job's result cannot be read",
                    ))
                } else {
                    let s = Status::lost(active.stage, "the job ended without a result");
                    let _ = fsutil::write_atomic(
                        &dir.join(job::STATUS),
                        &serde_json::to_vec(&s).unwrap_or_default(),
                        0o600,
                    );
                    Some(s)
                }
            }
            None => None,
        };
        let Some(status) = status else {
            // Running: a heartbeat when nothing was emitted for a while.
            if let Some(last) = &active.last
                && active
                    .emitted_at
                    .is_none_or(|t| now.saturating_duration_since(t) >= HEARTBEAT)
            {
                events.push((
                    EventName::JobProgress,
                    serde_json::to_value(last).unwrap_or_default(),
                ));
                active.emitted_at = Some(now);
            }
            return (events, None);
        };
        let active = self.active.take().expect("active job");
        let job = finished_job(
            &active.id,
            active.kind,
            active.uid,
            active.created_at,
            &active.subject,
            &status,
        );
        let p = JobFinishedParams {
            job_id: active.id.clone(),
            kind: active.kind,
            ok: status.state == JobState::Succeeded,
            error: status.error.clone(),
            result: status.result.clone(),
            session_restart_required: status.session_restart_required,
            subject: job.subject.clone(),
            started_at: job.started_at,
            finished_at: status.finished_at,
        };
        events.push((
            EventName::JobFinished,
            serde_json::to_value(p).unwrap_or_default(),
        ));
        self.push_finished(job.clone());
        self.prune();
        (events, Some(Finished { job, status }))
    }

    fn lose(&mut self, id: &str, stage: Option<Stage>, why: &str) {
        let dir = self.dir(id);
        let s = Status::lost(stage, why);
        let _ = fsutil::write_atomic(
            &dir.join(job::STATUS),
            &serde_json::to_vec(&s).unwrap_or_default(),
            0o600,
        );
        if let Some(head) = job::read_json::<Head>(&dir.join(job::SPEC))
            && let Some(kind) = head.kind
        {
            self.push_finished(head.finished(id, kind, &s));
        }
    }

    fn push_finished(&mut self, job: Job) {
        self.finished.push_front(job);
    }

    /// Keep the last [`KEEP_FINISHED`] finished jobs (and their directories).
    fn prune(&mut self) {
        while self.finished.len() > KEEP_FINISHED {
            if let Some(old) = self.finished.pop_back() {
                let _ = fsutil::remove(&self.dir(&old.job_id));
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<Job> {
        if let Some(a) = &self.active
            && a.id == id
        {
            return Some(running(a));
        }
        self.finished.iter().find(|j| j.job_id == id).cloned()
    }

    pub fn list(&self) -> Vec<Job> {
        self.active
            .iter()
            .map(running)
            .chain(self.finished.iter().cloned())
            .collect()
    }

    /// Ask the running job to stop. Refused once it is activating.
    pub fn cancel(&mut self, id: &str) -> Result<(), ErrorObject> {
        let Some(active) = self.active.as_ref().filter(|a| a.id == id) else {
            return Err(if self.finished.iter().any(|j| j.job_id == id) {
                ErrorObject::with_data(
                    ErrorCode::JobBusy,
                    "the job has already finished",
                    json!({ "job_id": id }),
                )
            } else {
                ErrorObject::new(ErrorCode::NotFound, "no such job")
            });
        };
        let dir = self.paths.jobs_dir().join(&active.id);
        let busy = || {
            ErrorObject::with_data(
                ErrorCode::JobBusy,
                "the job is activating and can no longer be cancelled",
                json!({ "job_id": id }),
            )
        };
        if dir.join(job::ACTIVATING).exists() {
            return Err(busy());
        }
        // The job creates `activating` and then looks for `cancel`; we create `cancel` and then
        // look for `activating`. At least one side sees the other.
        let created = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(dir.join(job::CANCEL));
        if let Err(e) = created {
            warn!("cannot request cancel: {e}");
            return Err(ErrorObject::new(ErrorCode::InternalError, "cannot cancel"));
        }
        if dir.join(job::ACTIVATING).exists() {
            return Err(busy());
        }
        Ok(())
    }
}

fn running(a: &Active) -> Job {
    Job {
        job_id: a.id.clone(),
        kind: a.kind,
        uid: a.uid,
        state: JobState::Running,
        stage: a.stage,
        created_at: a.created_at,
        finished_at: None,
        error: None,
        result: None,
        session_restart_required: false,
        subject: a.subject.clone(),
        started_at: a.created_at,
        percent: a.last.as_ref().and_then(|l| l.percent),
        detail: a.last.as_ref().and_then(|l| l.detail.clone()),
        last_activity_at: a.last.as_ref().and_then(|l| l.last_activity_at),
    }
}

fn finished_job(
    id: &str,
    kind: JobKind,
    uid: u32,
    created_at: i64,
    subject: &JobSubject,
    status: &Status,
) -> Job {
    Job {
        job_id: id.to_owned(),
        kind,
        uid,
        state: status.state,
        stage: status.stage,
        created_at,
        finished_at: Some(status.finished_at),
        error: status.error.clone(),
        result: status.result.clone(),
        session_restart_required: status.session_restart_required,
        subject: status.subject.clone().unwrap_or_else(|| subject.clone()),
        started_at: created_at,
        percent: None,
        detail: None,
        last_activity_at: None,
    }
}

/// Whether a job process holds `lock`.
fn locked(lock: &Path) -> bool {
    let Ok(f) = fs::File::open(lock) else {
        return false;
    };
    match flock(&f, FlockOperation::NonBlockingLockShared) {
        Ok(()) => {
            let _ = flock(&f, FlockOperation::Unlock);
            false
        }
        Err(_) => true,
    }
}

/// Start `hinteros-job@<id>.service` through systemd's D-Bus API.
pub async fn start_unit(conn: &zbus::Connection, id: &str) -> zbus::Result<()> {
    conn.call_method(
        Some("org.freedesktop.systemd1"),
        "/org/freedesktop/systemd1",
        Some("org.freedesktop.systemd1.Manager"),
        "StartUnit",
        &(job::unit_name(id), "fail"),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Tools;
    use crate::job::{Action, Apply};

    fn paths(d: &Path) -> Paths {
        Paths {
            config_dir: d.join("etc"),
            state_dir: d.join("var"),
            run_dir: d.join("run"),
            profile: d.join("profiles/system"),
            current_system: d.join("current"),
            booted_system: d.join("booted"),
            apps_profiles: d.join("nixprofiles"),
            catalog_index: None,
            users: Default::default(),
            proc_dir: "/proc".into(),
            zoneinfo_dir: d.join("zoneinfo"),
            localtime: d.join("localtime"),
            server: Default::default(),
        }
    }

    fn spec(d: &Path, id: &str) -> JobSpec {
        JobSpec {
            id: id.into(),
            kind: JobKind::Rebuild,
            uid: 1000,
            created_at: job::now(),
            paths: paths(d),
            tools: Tools {
                nix: "nix".into(),
                nix_env: "nix-env".into(),
                git: "git".into(),
                ssh_keygen: "ssh-keygen".into(),
                nixos_facter: "nixos-facter".into(),
                nix_store: "nix-store".into(),
            },
            bus: None,
            action: Action::Apply(Apply::default()),
            subject: Default::default(),
        }
    }

    #[test]
    fn lifecycle() {
        let d = tempfile::tempdir().unwrap();
        let mut jobs = Jobs::load(paths(d.path()));
        assert!(jobs.busy().is_none());
        let s = spec(d.path(), "0123456789abcdef");
        jobs.create(&s).unwrap();
        assert_eq!(jobs.busy(), Some("0123456789abcdef"));
        assert!(jobs.create(&spec(d.path(), "1123456789abcdef")).is_err());
        assert_eq!(jobs.busy_error().unwrap().kind(), Some(ErrorCode::JobBusy));
        let jd = d.path().join("run/jobs/0123456789abcdef");

        // Progress.
        fs::write(
            jd.join(job::PROGRESS),
            "{\"stage\":\"preparing\"}\n{\"stage\":\"building\"}\n{\"stage\":\"building\",\"percent\":5}\n{\"stage\"",
        )
        .unwrap();
        let (ev, fin) = jobs.poll();
        assert!(fin.is_none(), "still starting (no lock, no pid, young)");
        let stages: Vec<_> = ev.iter().map(|(_, p)| p["stage"].clone()).collect();
        assert_eq!(stages, ["preparing", "building", "building"]);
        assert_eq!(
            jobs.get("0123456789abcdef").unwrap().stage,
            Some(Stage::Building)
        );

        // Cancel works before activation, not after.
        jobs.cancel("0123456789abcdef").unwrap();
        assert!(jd.join(job::CANCEL).exists());
        fs::write(jd.join(job::ACTIVATING), "").unwrap();
        assert_eq!(
            jobs.cancel("0123456789abcdef").unwrap_err().kind(),
            Some(ErrorCode::JobBusy)
        );
        assert_eq!(
            jobs.cancel("ffffffffffffffff").unwrap_err().kind(),
            Some(ErrorCode::NotFound)
        );

        // A pid but no lock: the job died.
        fs::write(jd.join(job::PID), "1").unwrap();
        let (ev, fin) = jobs.poll();
        let fin = fin.unwrap();
        assert_eq!(fin.job.state, JobState::Failed);
        assert_eq!(ev.last().unwrap().0, EventName::JobFinished);
        assert!(jobs.busy().is_none());
        assert_eq!(jobs.list().len(), 1);

        // Reload from disk.
        let jobs = Jobs::load(paths(d.path()));
        assert_eq!(jobs.list()[0].job_id, "0123456789abcdef");
        assert!(jobs.busy().is_none());
    }

    fn append(path: &Path, line: &str) {
        use std::io::Write;
        let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(f, "{line}").unwrap();
    }

    /// While the job runs: a `job_progress` at least every [`HEARTBEAT`] (repeating the last
    /// one), the latest progress and subject in `jobs.get`, also after a service restart.
    #[test]
    fn heartbeat_and_latest_progress() {
        let d = tempfile::tempdir().unwrap();
        let mut jobs = Jobs::load(paths(d.path()));
        let mut s = spec(d.path(), "0123456789abcdef");
        s.subject.enabled = Some(true);
        jobs.create(&s).unwrap();
        let jd = d.path().join("run/jobs/0123456789abcdef");
        // The job process: holds the lock.
        let lock = fs::File::create(jd.join(job::LOCK)).unwrap();
        flock(&lock, FlockOperation::NonBlockingLockExclusive).unwrap();
        fs::write(jd.join(job::PID), "1").unwrap();
        let ms = |n: u64| Duration::from_millis(n);
        let t0 = Instant::now();

        // Nothing written yet: nothing to repeat.
        assert!(jobs.poll_at(t0).0.is_empty());
        assert!(jobs.poll_at(t0 + ms(5000)).0.is_empty());
        let t0 = t0 + ms(5000);

        append(
            &jd.join(job::PROGRESS),
            r#"{"stage":"building","percent":50,"detail":{"builds_done":0,"builds_total":2,"building":{"name":"hinteros-daemon","phase":"build"}},"activity_at":100}"#,
        );
        let (ev, _) = jobs.poll_at(t0);
        assert_eq!(ev.len(), 1);
        let p = &ev[0].1;
        assert_eq!(ev[0].0, EventName::JobProgress);
        assert_eq!(p["kind"], "rebuild");
        assert_eq!(p["started_at"], s.created_at);
        assert_eq!(p["last_activity_at"], 100);
        assert_eq!(p["detail"]["building"]["name"], "hinteros-daemon");
        assert!(p.get("subject").is_none());
        let first = p.clone();

        // Nothing new: quiet until 2 s, then the same event again, every 2 s.
        assert!(jobs.poll_at(t0 + ms(250)).0.is_empty());
        assert!(jobs.poll_at(t0 + ms(1999)).0.is_empty());
        let (ev, _) = jobs.poll_at(t0 + ms(2000));
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].1, first);
        assert!(jobs.poll_at(t0 + ms(3000)).0.is_empty());
        assert_eq!(jobs.poll_at(t0 + ms(4100)).0.len(), 1);

        // A new line (no activity time: the last one stays) with a refined subject; the clock
        // restarts from it.
        append(
            &jd.join(job::PROGRESS),
            r#"{"stage":"building","percent":60,"subject":{"enabled":false}}"#,
        );
        let (ev, _) = jobs.poll_at(t0 + ms(4500));
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].1["percent"], 60);
        assert_eq!(ev[0].1["last_activity_at"], 100);
        assert_eq!(ev[0].1["subject"], json!({"enabled": false}));
        assert!(jobs.poll_at(t0 + ms(6000)).0.is_empty());
        let (ev, _) = jobs.poll_at(t0 + ms(6500));
        assert_eq!(ev.len(), 1);
        assert!(
            ev[0].1.get("subject").is_none(),
            "a heartbeat repeats no subject"
        );

        let check = |jobs: &Jobs| {
            let j = jobs.get("0123456789abcdef").unwrap();
            assert_eq!(j.state, JobState::Running);
            assert_eq!(j.stage, Some(Stage::Building));
            assert_eq!(j.percent, Some(60));
            assert_eq!(j.detail, None, "the latest line had none");
            assert_eq!(j.last_activity_at, Some(100));
            assert_eq!(j.subject.enabled, Some(false));
            assert_eq!(j.started_at, s.created_at);
            assert_eq!(jobs.list()[0], j);
        };
        check(&jobs);

        // The service restarts while the job runs: the same state once it has read the lines.
        let mut jobs = Jobs::load(paths(d.path()));
        jobs.poll_at(t0 + ms(7000));
        check(&jobs);

        // The job ends: the subject it last knew, no progress any more.
        drop(lock);
        let status = Status::lost(Some(Stage::Building), "x");
        fs::write(jd.join(job::STATUS), serde_json::to_vec(&status).unwrap()).unwrap();
        let (ev, fin) = jobs.poll_at(t0 + ms(7100));
        let fin = fin.unwrap();
        assert_eq!(fin.job.subject.enabled, Some(false));
        assert_eq!(fin.job.percent, None);
        let (name, p) = ev.last().unwrap();
        assert_eq!(*name, EventName::JobFinished);
        assert_eq!(p["subject"], json!({"enabled": false}));
        assert_eq!(p["finished_at"], status.finished_at);
        assert!(
            jobs.poll_at(t0 + ms(20_000)).0.is_empty(),
            "no heartbeat after the end"
        );
    }

    /// After a rollback the older service re-attaches to the job a later version started (it
    /// runs on through the switch): a spec and status with what this binary does not know.
    #[test]
    fn a_later_versions_job_keeps_the_box_busy() {
        let d = tempfile::tempdir().unwrap();
        let jobs_dir = d.path().join("run/jobs");
        let write_job = |id: &str, spec: Value| {
            let jd = jobs_dir.join(id);
            fs::create_dir_all(&jd).unwrap();
            fs::write(jd.join(job::SPEC), spec.to_string()).unwrap();
            fs::write(jd.join(job::PROGRESS), "").unwrap();
            let lock = fs::File::create(jd.join(job::LOCK)).unwrap();
            flock(&lock, FlockOperation::NonBlockingLockExclusive).unwrap();
            fs::write(jd.join(job::PID), "1").unwrap();
            (jd, lock)
        };
        // A known kind, but fields this binary does not know (a later Paths, a new action).
        let mut s = serde_json::to_value(spec(d.path(), "0123456789abcdef")).unwrap();
        s["paths"]["later"] = json!("/x");
        s["action"] = json!({"type": "later_action"});
        s["subject"]["later"] = json!(1);
        let (jd, lock) = write_job("0123456789abcdef", s);
        let mut jobs = Jobs::load(paths(d.path()));
        assert_eq!(jobs.busy(), Some("0123456789abcdef"));
        assert!(jobs.poll().1.is_none());
        // Its status, with a result and a stage this binary does not know: shown, never replaced.
        drop(lock);
        let status = json!({"state": "succeeded", "stage": "later_stage", "error": null,
            "result": {"later": true}, "session_restart_required": false, "finished_at": 7,
            "later": 1});
        fs::write(jd.join(job::STATUS), status.to_string()).unwrap();
        let fin = jobs.poll().1.unwrap();
        assert_eq!(fin.job.state, JobState::Succeeded);
        assert_eq!(fin.job.stage, Some(Stage::Unknown));
        assert_eq!(fin.job.result, None);
        assert!(jobs.busy().is_none());
        let on_disk: Value =
            serde_json::from_slice(&fs::read(jd.join(job::STATUS)).unwrap()).unwrap();
        assert_eq!(on_disk, status);

        // A kind this binary does not know: followed as `unknown`.
        let mut s = serde_json::to_value(spec(d.path(), "1123456789abcdef")).unwrap();
        s["kind"] = json!("later_kind");
        let (jd, lock) = write_job("1123456789abcdef", s);
        let mut jobs = Jobs::load(paths(d.path()));
        assert_eq!(jobs.busy(), Some("1123456789abcdef"));
        assert_eq!(jobs.get("1123456789abcdef").unwrap().kind, JobKind::Unknown);
        drop(lock);
        fs::write(jd.join(job::STATUS), status.to_string()).unwrap();
        assert!(jobs.poll().1.is_some());

        // A spec this binary cannot read at all: not shown, but nothing else starts while its
        // process holds the lock.
        let (jd, lock) = write_job("2123456789abcdef", json!({}));
        fs::write(jd.join(job::SPEC), "{\"id\":").unwrap();
        let mut jobs = Jobs::load(paths(d.path()));
        assert_eq!(jobs.busy(), Some("2123456789abcdef"));
        assert!(jobs.create(&spec(d.path(), "3123456789abcdef")).is_err());
        assert!(jobs.get("2123456789abcdef").is_none());
        assert!(jobs.poll().1.is_none());
        assert_eq!(jobs.busy(), Some("2123456789abcdef"));
        // It ends: the box is free again. (A process another test is starting can hold a copy of
        // the lock fd until its exec, so poll again like the monitor does.)
        drop(lock);
        for _ in 0..100 {
            jobs.poll();
            if jobs.busy().is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(jobs.busy().is_none());
        jobs.create(&spec(d.path(), "3123456789abcdef")).unwrap();
    }

    #[test]
    fn keeps_ten_finished() {
        let d = tempfile::tempdir().unwrap();
        let mut jobs = Jobs::load(paths(d.path()));
        for i in 0..12u8 {
            let id = format!("{i:016x}");
            jobs.create(&spec(d.path(), &id)).unwrap();
            let jd = d.path().join("run/jobs").join(&id);
            let status = Status::lost(None, "x");
            fs::write(jd.join(job::STATUS), serde_json::to_vec(&status).unwrap()).unwrap();
            let (_, fin) = jobs.poll();
            assert!(fin.is_some());
        }
        assert_eq!(jobs.list().len(), KEEP_FINISHED);
        assert_eq!(
            fs::read_dir(d.path().join("run/jobs")).unwrap().count(),
            KEEP_FINISHED
        );
        assert_eq!(jobs.list()[0].job_id, format!("{:016x}", 11));
    }
}
