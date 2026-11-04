//! Server section jobs (server.md): services on and off, Jellyfin libraries,
//! the Plex claim, Tailscale sign-in and sign-out, the Cloudflare tunnel. The job unit has the
//! network, but these jobs only talk to the box's own services (loopback, unix sockets) and run
//! the `tailscale` CLI.

use std::io::{BufRead, BufReader};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use hinteros_api::system::{ProgressDetail, Stage};
use rustix::process::{Pid, Signal, kill_process_group};
use serde_json::Value;
use tracing::{info, warn};

use super::{Failure, Outcome, Runner};
use crate::job::{
    self, CloudflareJob, JellyfinJob, JellyfinOp, PlexJob, ProgressLine, ServicesJob, TailscaleJob,
};
use crate::servers::{self, backup, cloudflare, jellyfin, media, plex, tailscale};

/// How long a job waits for Jellyfin or Plex to answer (they start slowly).
const SERVICE_WAIT: Duration = Duration::from_secs(90);
/// How long `services_enable` waits for Jellyfin's first-run setup before making its users.
const JELLYFIN_SETUP_WAIT: Duration = Duration::from_secs(180);
/// How long a claim may take to show in `Preferences.xml`.
const CLAIM_WAIT: Duration = Duration::from_secs(30);
/// How long cloudflared may take to connect on a new token.
const TUNNEL_WAIT: Duration = Duration::from_secs(30);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

impl Runner {
    fn failed(&self, hint: impl AsRef<str>, reason: &str) -> Failure {
        Failure::Error(job::job_failed_because(
            self.stage.unwrap_or(Stage::Preparing),
            hint.as_ref(),
            reason,
        ))
    }

    /// Check `ok` every second until it holds or `limit` passes (`Ok(false)`); a cancel request
    /// ends the wait.
    fn wait_for(&mut self, limit: Duration, mut ok: impl FnMut() -> bool) -> Result<bool, Failure> {
        let deadline = Instant::now() + limit;
        loop {
            if ok() {
                return Ok(true);
            }
            self.check_cancel()?;
            if Instant::now() >= deadline {
                return Ok(false);
            }
            self.tick_at(Instant::now());
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// A systemd manager call on `unit` (`RestartUnit`, `StopUnit`), mode `replace`.
    fn unit_call(&self, method: &str, unit: &str) -> Result<(), String> {
        let bus = self.spec.bus.clone();
        let (method, unit) = (method.to_owned(), unit.to_owned());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?
            .block_on(async move {
                let conn = match &bus {
                    Some(a) => {
                        zbus::connection::Builder::address(a.as_str())?
                            .build()
                            .await?
                    }
                    None => zbus::Connection::system().await?,
                };
                tokio::time::timeout(
                    Duration::from_secs(30),
                    conn.call_method(
                        Some("org.freedesktop.systemd1"),
                        "/org/freedesktop/systemd1",
                        Some("org.freedesktop.systemd1.Manager"),
                        method.as_str(),
                        &(unit.as_str(), "replace"),
                    ),
                )
                .await
                .map_err(|_| zbus::Error::Failure("timed out".into()))??;
                Ok::<(), zbus::Error>(())
            })
            .map_err(|e| e.to_string())
    }

    /// `services_enable` / `services_disable`: the apply flow, then data deletion and Jellyfin's
    /// users.
    pub(super) fn services_job(&mut self, j: &ServicesJob) -> Outcome {
        let outcome = self.apply(&j.apply)?;
        let paths = self.spec.paths.clone();
        for t in &j.delete {
            if !servers::delete_allowed(&t.base, &t.rel, &paths.server) {
                warn!(base = %t.base.display(), path = %t.rel, "not deleting: outside /var/lib and /var/cache");
                continue;
            }
            match crate::datadel::delete_under(&t.base, &t.rel) {
                Ok(crate::datadel::Deleted::Removed(n)) => {
                    info!(base = %t.base.display(), path = %t.rel, entries = n, "deleted service data")
                }
                Ok(crate::datadel::Deleted::Missing) => {
                    info!(base = %t.base.display(), path = %t.rel, "no service data to delete")
                }
                Err(e) => {
                    warn!(base = %t.base.display(), path = %t.rel, "not deleting service data: {e}")
                }
            }
        }
        if j.enabled.iter().any(|i| i == jellyfin::ID) {
            // Its users once the setup unit is done (best effort: reveal and every Jellyfin job
            // sync again).
            self.set_stage(Stage::Configuring);
            let files = jellyfin::Files::new(&paths.services_dir());
            let addr = paths.server.jellyfin;
            let ready = self.wait_for(JELLYFIN_SETUP_WAIT, || {
                files.setup_done() && jellyfin::Client::ping(addr, Duration::from_secs(2))
            });
            match ready {
                Ok(true) => jellyfin::sync_if_ready(&paths, &paths.server),
                Ok(false) => warn!("Jellyfin is not set up yet; its users come later"),
                Err(_) => {}
            }
        }
        Ok(outcome)
    }

    /// Jellyfin, set up and answering (waits up to [`SERVICE_WAIT`]).
    fn jellyfin_client(&mut self) -> Result<jellyfin::Client, Failure> {
        let paths = self.spec.paths.clone();
        let files = jellyfin::Files::new(&paths.services_dir());
        let addr = paths.server.jellyfin;
        let up = self.wait_for(SERVICE_WAIT, || {
            files.setup_done() && jellyfin::Client::ping(addr, Duration::from_secs(2))
        })?;
        if !files.setup_done() {
            return Err(self.failed("Jellyfin setup is not done", "not_ready"));
        }
        if !up {
            return Err(self.failed("Jellyfin does not answer", "unreachable"));
        }
        jellyfin::Client::new(&files, addr, &paths.users, HTTP_TIMEOUT)
            .map_err(|e| self.failed(e.to_string(), e.reason()))
    }

    pub(super) fn jellyfin_job(&mut self, j: &JellyfinJob) -> Outcome {
        self.set_stage(Stage::Preparing);
        let client = self.jellyfin_client()?;
        self.set_stage(Stage::Configuring);
        let paths = self.spec.paths.clone();
        let files = jellyfin::Files::new(&paths.services_dir());
        if let Err(e) = jellyfin::profiles(&paths).and_then(|p| jellyfin::sync(&client, &files, &p))
        {
            warn!("Jellyfin users not synced: {e}");
        }
        let fail = |r: &Self, e: jellyfin::Error| r.failed(e.to_string(), e.reason());
        let folders = client.virtual_folders().map_err(|e| fail(self, e))?;
        match &j.op {
            JellyfinOp::AddLibrary { name, kind, folder } => {
                let path = media::resolve(&paths.server.media_dir, folder)
                    .map_err(|e| self.failed(format!("folder {folder}: {e}"), "refused"))?;
                if folders
                    .iter()
                    .any(|f| f.name.to_lowercase() == name.to_lowercase())
                {
                    return Err(self.failed("a library has this name", "refused"));
                }
                client
                    .add_library(name, *kind, &path)
                    .map_err(|e| fail(self, e))?;
                info!(name = %name, folder = %folder, "Jellyfin library added");
                if let Ok(now) = client.virtual_folders() {
                    let id = now
                        .iter()
                        .find(|f| f.name == *name)
                        .and_then(|f| f.item_id.clone())
                        .filter(|i| jellyfin::is_id(i));
                    if id.is_some() {
                        self.set_subject(|s| {
                            if let Some(l) = s.library.as_mut() {
                                l.id = id;
                            }
                        });
                    }
                    jellyfin::write_cache(&files, &now);
                }
            }
            JellyfinOp::RemoveLibrary { library_id } => {
                let Some(f) = folders
                    .iter()
                    .find(|f| f.item_id.as_deref() == Some(library_id))
                else {
                    return Err(self.failed("no such library", "refused"));
                };
                client.remove_library(&f.name).map_err(|e| fail(self, e))?;
                info!(name = %f.name, "Jellyfin library removed (media files kept)");
                if let Ok(now) = client.virtual_folders() {
                    jellyfin::write_cache(&files, &now);
                }
            }
        }
        self.set_stage(Stage::Finishing);
        Ok((None, false))
    }

    pub(super) fn plex_job(&mut self, j: &PlexJob) -> Outcome {
        self.set_stage(Stage::Preparing);
        let server = self.spec.paths.server.clone();
        // Only a server run by the plex account gets the code (32400 is not a privileged port).
        let Some(owner) = self.spec.paths.users.by_name(plex::USER).map(|u| u.uid) else {
            return Err(self.failed("Plex does not answer", "unreachable"));
        };
        let up = self.wait_for(SERVICE_WAIT, || {
            plex::reachable(server.plex, owner, Duration::from_secs(2))
        })?;
        if !up {
            return Err(self.failed("Plex does not answer", "unreachable"));
        }
        let prefs = plex::read(&server.plex_preferences).unwrap_or_default();
        if prefs.claimed {
            info!("Plex is linked already");
            return Ok((None, false));
        }
        self.set_stage(Stage::Configuring);
        let client_id = prefs.machine_id.unwrap_or_else(|| "hinteros".into());
        plex::claim(server.plex, owner, &j.code, &client_id, HTTP_TIMEOUT)
            .map_err(|e| self.failed(e, "refused"))?;
        let linked = self.wait_for(CLAIM_WAIT, || {
            plex::read(&server.plex_preferences).is_some_and(|p| p.claimed)
        })?;
        if !linked {
            return Err(self.failed("Plex did not link to the account", "refused"));
        }
        info!("Plex linked to an account");
        self.set_stage(Stage::Finishing);
        Ok((None, false))
    }

    pub(super) fn tailscale_job(&mut self, j: &TailscaleJob) -> Outcome {
        self.set_stage(Stage::Preparing);
        let server = self.spec.paths.server.clone();
        let status = tailscale::status(&server.tailscale_socket, Duration::from_secs(5)).ok();
        if !j.login {
            self.set_stage(Stage::Configuring);
            let mut cmd = Command::new(&server.tailscale);
            cmd.arg("logout");
            self.exec(cmd, false, &mut |_, _| {})?;
            info!("Tailscale signed out");
            self.set_stage(Stage::Finishing);
            return Ok((None, false));
        }
        if status.as_ref().map(tailscale::backend_state) == Some("Running") {
            info!("Tailscale is signed in already");
            return Ok((None, false));
        }
        self.tailscale_login(
            status.as_ref(),
            Duration::from_secs(server.tailscale_login_timeout_s.max(30)),
        )?;
        let after = tailscale::status(&server.tailscale_socket, Duration::from_secs(5)).ok();
        if let Some(s) = &after
            && tailscale::backend_state(s) != "Running"
        {
            warn!(
                state = tailscale::backend_state(s),
                "Tailscale is not running after the sign-in"
            );
        }
        info!("Tailscale signed in");
        self.set_stage(Stage::Finishing);
        Ok((None, false))
    }

    /// `tailscale up --json`: its JSON objects (several lines each) are decoded as they come;
    /// each new `AuthURL` is reported (stage `signing_in`, `detail.auth_url`); the run ends with
    /// `{"BackendState": "Running"}` and exit 0, or the CLI's own timeout.
    fn tailscale_login(
        &mut self,
        status: Option<&Value>,
        timeout: Duration,
    ) -> Result<(), Failure> {
        let server = self.spec.paths.server.clone();
        let mut cmd = Command::new(&server.tailscale);
        cmd.args(tailscale::login_args(status, timeout))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        self.set_stage(Stage::Configuring);
        let mut child = cmd
            .spawn()
            .map_err(|e| self.failed(format!("cannot run tailscale: {e}"), "unreachable"))?;
        let pgid = Pid::from_child(&child);
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let (tx, rx) = mpsc::channel::<Value>();
        let out_thread = std::thread::spawn(move || {
            for v in serde_json::Deserializer::from_reader(stdout).into_iter::<Value>() {
                let Ok(v) = v else { break };
                if tx.send(v).is_err() {
                    break;
                }
            }
        });
        let tail = Arc::new(Mutex::new(Vec::<String>::new()));
        let tail2 = tail.clone();
        let err_thread = std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                eprintln!("{line}");
                let mut t = tail2.lock().unwrap();
                t.push(line.chars().take(400).collect());
                if t.len() > 10 {
                    t.remove(0);
                }
            }
        });
        let deadline = Instant::now() + timeout + Duration::from_secs(30);
        let mut url: Option<String> = None;
        let mut running = false;
        let mut cancelled = false;
        let mut overdue = false;
        let exit = loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(v) => {
                    self.touch();
                    if let Some(u) = tailscale::auth_url(&v)
                        && url.as_ref() != Some(&u)
                    {
                        info!("Tailscale sign-in URL ready");
                        url = Some(u.clone());
                        self.set_stage(Stage::SigningIn);
                        self.report(ProgressLine {
                            stage: Stage::SigningIn,
                            percent: None,
                            detail: Some(ProgressDetail {
                                auth_url: Some(u),
                                ..Default::default()
                            }),
                            activity_at: None,
                            subject: None,
                        });
                    }
                    if v["BackendState"].as_str() == Some("Running") {
                        running = true;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {}
            }
            self.tick_at(Instant::now());
            if !cancelled && (self.cancel_requested() || Instant::now() > deadline) {
                overdue = !self.cancel_requested();
                cancelled = true;
                let _ = kill_process_group(pgid, Signal::TERM);
                let until = Instant::now() + Duration::from_secs(5);
                while Instant::now() < until && matches!(child.try_wait(), Ok(None)) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                let _ = kill_process_group(pgid, Signal::KILL);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(e) => {
                    return Err(self.failed(format!("waiting for tailscale: {e}"), "unreachable"));
                }
            }
        };
        let _ = out_thread.join();
        let _ = err_thread.join();
        while let Ok(v) = rx.try_recv() {
            if v["BackendState"].as_str() == Some("Running") {
                running = true;
            }
        }
        if let Some(line) = self.pending.take() {
            self.write_line(line, Instant::now());
        }
        if cancelled && !overdue {
            return Err(Failure::Cancelled);
        }
        let hint = tail.lock().unwrap().last().cloned().unwrap_or_default();
        if overdue || hint.contains("timeout waiting") {
            return Err(self.failed("nobody signed in in time", "timeout"));
        }
        if !exit.success() || !running {
            return Err(self.failed(
                if hint.is_empty() {
                    format!("tailscale failed ({exit})")
                } else {
                    hint
                },
                "refused",
            ));
        }
        Ok(())
    }

    pub(super) fn cloudflare_job(&mut self, j: &CloudflareJob) -> Outcome {
        self.set_stage(Stage::Configuring);
        let paths = self.spec.paths.clone();
        let token = paths.services_dir().join(servers::CLOUDFLARE).join("token");
        if j.set {
            if !token.is_file() {
                return Err(self.failed("there is no token", "refused"));
            }
            self.unit_call("RestartUnit", cloudflare::UNIT)
                .map_err(|e| {
                    self.failed(
                        format!("restarting {}: {e}", cloudflare::UNIT),
                        "unreachable",
                    )
                })?;
            let metrics = paths.server.cloudflared_metrics;
            let up = self.wait_for(TUNNEL_WAIT, || {
                cloudflare::connected(metrics, Duration::from_secs(2))
            })?;
            if !up {
                return Err(self.failed(
                    "the tunnel did not connect: check the token",
                    "not_connected",
                ));
            }
            info!("Cloudflare tunnel connected");
        } else {
            if let Err(e) = self.unit_call("StopUnit", cloudflare::UNIT) {
                warn!("stopping {}: {e}", cloudflare::UNIT);
            }
            match std::fs::remove_file(&token) {
                Ok(()) => info!("Cloudflare tunnel token deleted"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(self.failed(format!("deleting the token: {e}"), "refused")),
            }
        }
        self.set_stage(Stage::Finishing);
        Ok((None, false))
    }
}

/// How long a job waits for Jellyfin to stop before its backup.
const JELLYFIN_STOP_WAIT: Duration = Duration::from_secs(90);

impl Runner {
    /// A unit's `ActiveState` (`None`: not loaded, or systemd cannot be asked).
    fn unit_state(bus: Option<String>, unit: &str) -> Option<String> {
        let unit = unit.to_owned();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?
            .block_on(async move {
                let conn = match &bus {
                    Some(a) => zbus::connection::Builder::address(a.as_str())
                        .ok()?
                        .build()
                        .await
                        .ok()?,
                    None => zbus::Connection::system().await.ok()?,
                };
                crate::freeze::unit_property(&conn, &unit, "ActiveState")
                    .await
                    .ok()
                    .flatten()
            })
    }

    /// Before a switch from `old` to `new` (an update, a rollback, any apply): when both run
    /// Jellyfin and its version changes, stop it and replace the backup of its data
    /// ([`backup`]). Returns whether Jellyfin was stopped here: the caller starts it again
    /// ([`Self::jellyfin_start`]) whatever the switch did. A failure fails the job before the
    /// switch (the box stays as it was, Jellyfin running).
    pub(super) fn jellyfin_backup(
        &mut self,
        old: Option<&Path>,
        new: &Path,
    ) -> Result<bool, Failure> {
        let Some((from, to)) = backup::changes(old, new) else {
            return Ok(false);
        };
        let paths = self.spec.paths.clone();
        let data = paths.server.var_lib.join(backup::DATA);
        if !data.is_dir() {
            return Ok(false);
        }
        info!(from, to, "Jellyfin changes version: backing up its data");
        self.unit_call("StopUnit", backup::UNIT)
            .map_err(|e| self.failed(format!("stopping Jellyfin: {e}"), "refused"))?;
        let bus = self.spec.bus.clone();
        let stopped = self.wait_for(JELLYFIN_STOP_WAIT, || {
            !matches!(
                Self::unit_state(bus.clone(), backup::UNIT).as_deref(),
                Some("active" | "activating" | "deactivating" | "reloading")
            )
        });
        if !matches!(stopped, Ok(true)) {
            self.jellyfin_start(true);
            stopped?;
            return Err(self.failed("Jellyfin did not stop", "timeout"));
        }
        let dir = paths.services_dir().join(jellyfin::ID);
        let made = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .and_then(|()| backup::replace(&data, &dir.join(backup::BACKUP)));
        match made {
            Ok(n) => {
                info!(entries = n, "Jellyfin data backed up");
                Ok(true)
            }
            Err(e) => {
                self.jellyfin_start(true);
                Err(self.failed(format!("backing up Jellyfin's data: {e}"), "refused"))
            }
        }
    }

    /// Start Jellyfin again after [`Self::jellyfin_backup`] stopped it (best effort; when the
    /// running system has no Jellyfin there is nothing to start).
    pub(super) fn jellyfin_start(&self, stopped: bool) {
        if stopped && let Err(e) = self.unit_call("StartUnit", backup::UNIT) {
            warn!("starting Jellyfin again: {e}");
        }
    }
}

impl Runner {
    /// After a profile job's switch: the profiles' Jellyfin users, once Jellyfin answers (the
    /// switch may have restarted it, e.g. for a new version; up to [`SERVICE_WAIT`]). Best
    /// effort: `jellyfin.users.reveal` and every Jellyfin job sync again.
    pub(super) fn jellyfin_sync_users(&mut self) {
        let paths = self.spec.paths.clone();
        let files = jellyfin::Files::new(&paths.services_dir());
        if !jellyfin::enabled(&paths) || !files.setup_done() {
            return;
        }
        let addr = paths.server.jellyfin;
        if !matches!(
            self.wait_for(SERVICE_WAIT, || jellyfin::Client::ping(
                addr,
                Duration::from_secs(2)
            )),
            Ok(true)
        ) {
            warn!("Jellyfin does not answer; its users come later");
            return;
        }
        jellyfin::sync_if_ready(&paths, &paths.server);
    }
}
