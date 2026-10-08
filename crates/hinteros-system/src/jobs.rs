//! The service's view of jobs: one mutating job at a time, started as a transient instance of
//! `hinteros-job@.service` over systemd's D-Bus API, followed through its job directory, and
//! re-attached after a restart of the service (a switch restarts it while the job runs on).

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use hinteros_api::system::{
    EventName, Job, JobFinishedParams, JobKind, JobProgressParams, JobStartedParams, JobState,
    Stage, is_job_id,
};
use hinteros_api::{ErrorCode, ErrorObject};
use rustix::fs::{FlockOperation, flock};
use serde_json::{Value, json};
use tracing::warn;

use crate::config::Paths;
use crate::fsutil;
use crate::job::{self, JobSpec, ProgressLine, Status};

/// Finished jobs kept (in `/run`, so until reboot).
pub const KEEP_FINISHED: usize = 10;
/// A job that has not taken its lock this long after being created is considered lost.
const START_GRACE_S: i64 = 60;

struct Active {
    id: String,
    kind: JobKind,
    uid: u32,
    created_at: i64,
    offset: u64,
    stage: Option<Stage>,
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
}

impl Jobs {
    /// Load the job directories left by a previous instance.
    pub fn load(paths: Paths) -> Self {
        let mut jobs = Self {
            paths,
            active: None,
            finished: VecDeque::new(),
        };
        let dir = jobs.paths.jobs_dir();
        let _ = fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir);
        let mut found: Vec<(i64, String, JobSpec)> = fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let id = e.file_name().to_str()?.to_owned();
                if !is_job_id(&id) {
                    return None;
                }
                let spec: JobSpec = job::read_json(&e.path().join(job::SPEC))?;
                Some((spec.created_at, id, spec))
            })
            .collect();
        found.sort_by_key(|(t, _, _)| *t);
        for (_, id, spec) in found {
            let jd = dir.join(&id);
            match job::read_json::<Status>(&jd.join(job::STATUS)) {
                Some(status) => jobs.push_finished(to_job(&spec, &status)),
                None => {
                    if let Some(prev) = jobs.active.take() {
                        // Two unfinished jobs: the older one is lost.
                        jobs.lose(&prev.id, prev.stage, "superseded");
                    }
                    jobs.active = Some(Active {
                        id: id.clone(),
                        kind: spec.kind,
                        uid: spec.uid,
                        created_at: spec.created_at,
                        offset: 0,
                        stage: None,
                    });
                }
            }
        }
        jobs.prune();
        jobs
    }

    fn dir(&self, id: &str) -> PathBuf {
        self.paths.jobs_dir().join(id)
    }

    /// The running job, if any.
    pub fn busy(&self) -> Option<&str> {
        self.active.as_ref().map(|a| a.id.as_str())
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
        self.active = Some(Active {
            id: spec.id.clone(),
            kind: spec.kind,
            uid: spec.uid,
            created_at: spec.created_at,
            offset: 0,
            stage: None,
        });
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
        };
        (
            EventName::JobStarted,
            serde_json::to_value(p).unwrap_or_default(),
        )
    }

    /// Follow the active job: new progress lines become `job_progress`, a status (or a job
    /// that died without one) `job_finished`.
    pub fn poll(&mut self) -> (Vec<(EventName, Value)>, Option<Finished>) {
        let mut events = Vec::new();
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
        for (i, line) in lines.into_iter().enumerate() {
            let stage_change = active.stage != Some(line.stage);
            active.stage = Some(line.stage);
            if stage_change || i == last {
                let p = JobProgressParams {
                    job_id: active.id.clone(),
                    stage: line.stage,
                    percent: line.percent,
                    detail: line.detail,
                };
                events.push((
                    EventName::JobProgress,
                    serde_json::to_value(p).unwrap_or_default(),
                ));
            }
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
            return (events, None);
        };
        let active = self.active.take().expect("active job");
        let spec_like = (
            active.id.clone(),
            active.kind,
            active.uid,
            active.created_at,
        );
        let job = job_from(&spec_like, &status);
        let p = JobFinishedParams {
            job_id: active.id.clone(),
            kind: active.kind,
            ok: status.state == JobState::Succeeded,
            error: status.error.clone(),
            result: status.result.clone(),
            session_restart_required: status.session_restart_required,
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
        if let Some(spec) = job::read_json::<JobSpec>(&dir.join(job::SPEC)) {
            self.push_finished(to_job(&spec, &s));
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
    }
}

fn to_job(spec: &JobSpec, status: &Status) -> Job {
    job_from(
        &(spec.id.clone(), spec.kind, spec.uid, spec.created_at),
        status,
    )
}

fn job_from((id, kind, uid, created_at): &(String, JobKind, u32, i64), status: &Status) -> Job {
    Job {
        job_id: id.clone(),
        kind: *kind,
        uid: *uid,
        state: status.state,
        stage: status.stage,
        created_at: *created_at,
        finished_at: Some(status.finished_at),
        error: status.error.clone(),
        result: status.result.clone(),
        session_restart_required: status.session_restart_required,
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
