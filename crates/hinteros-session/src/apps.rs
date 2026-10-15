//! Starting app processes and noticing when they end.
//!
//! [`Spawner`] is the seam: [`ProcessSpawner`] starts real processes (apps under our
//! [`crate::reaper`], sandboxed ones in bwrap below it ([`crate::sandbox`]), Steam directly),
//! tests use a recording mock.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::lan::LanFilter;
use crate::procs::Children;
use crate::reaper;
use crate::sandbox::{self, Host, Spec};

/// A process started by a [`Spawner`] has ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildExit {
    /// The key it was spawned with.
    pub key: String,
    pub pid: u32,
    /// Exit status (128 + signal if it was killed), if known.
    pub code: Option<i32>,
}

pub trait Spawner: Send {
    /// Start `argv`. With `app_id`, under the reaper so that gamescope tags its windows with
    /// that app id; with `sandbox` too, in that sandbox below the reaper (never without it).
    /// Reports the end of the process as a [`ChildExit`] with `key`.
    fn spawn(
        &mut self,
        key: &str,
        argv: &[String],
        app_id: Option<u32>,
        sandbox: Option<&Spec>,
    ) -> io::Result<u32>;
}

/// Find `program` like `execvp` would: a path if it contains `/`, else a lookup in `$PATH`.
pub fn which(program: &str) -> io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        let p = PathBuf::from(program);
        return if executable(&p) {
            Ok(p)
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{program}: not an executable file"),
            ))
        };
    }
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(program))
        .find(|p| executable(p))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{program}: not found in PATH"),
            )
        })
}

/// The Steam binary for the configured `steam.path` (a path, or a name looked up in `$PATH`),
/// if it is an executable file: `None` means Steam is not installed.
pub fn steam_binary(configured: &Path) -> Option<PathBuf> {
    which(&configured.to_string_lossy()).ok()
}

/// Variables apps never get: gamescope's Wayland display gives access to its private protocols
/// (input method, action bindings, control). Steam keeps them.
pub const HIDDEN_FROM_APPS: &[&str] = &["WAYLAND_DISPLAY", "GAMESCOPE_WAYLAND_DISPLAY"];

/// The environment for a spawn: `env` (`"auto"` display resolved now), without
/// [`HIDDEN_FROM_APPS`] for an app (`app`: it runs under the reaper).
pub fn spawn_env(env: &[(String, String)], app: bool) -> Vec<(String, String)> {
    crate::config::live_env(env)
        .into_iter()
        .filter(|(k, _)| !(app && HIDDEN_FROM_APPS.contains(&k.as_str())))
        .collect()
}

/// `env` with `DISPLAY` set to the apps' display (`apps_display`, resolved now). Fails while
/// that display does not exist: an app never falls back to the home app's display.
pub fn with_apps_display(
    mut env: Vec<(String, String)>,
    apps_display: &str,
) -> io::Result<Vec<(String, String)>> {
    let display = crate::config::try_resolve_display(apps_display).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no app display ({apps_display})"),
        )
    })?;
    env.retain(|(k, _)| k != "DISPLAY");
    env.push(("DISPLAY".into(), display));
    Ok(env)
}

/// What sandboxed spawns need from the daemon's config (see [`crate::sandbox`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxSettings {
    /// `[sandbox] bwrap`: a path, or a name looked up in `$PATH` at each spawn.
    pub bwrap: PathBuf,
    pub pulse_socket: PathBuf,
    pub pulse_record_socket: PathBuf,
    /// The profile's app env link (`/run/hinteros/apps/<user>`).
    pub app_env: Option<PathBuf>,
    /// The text field socket (`osk.sock`).
    pub osk_socket: Option<PathBuf>,
}

/// How long a launch waits for the reaper to be in the `nolan` cgroup.
const IN_CGROUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Wait until the reaper writes `1` on `read` (it is in its `Cgroup=`). End of file: it could
/// not move there and started nothing.
fn wait_in_cgroup(read: std::io::PipeReader, timeout: Duration) -> io::Result<()> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    use std::io::Read;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the reaper did not enter the cgroup in time",
            ));
        }
        let left = Timespec::try_from(left).map_err(io::Error::other)?;
        let mut fds = [PollFd::new(&read, PollFlags::IN)];
        match poll(&mut fds, Some(&left)) {
            Ok(0) => continue,
            Ok(_) => break,
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    let mut byte = [0u8; 1];
    match (&read).read(&mut byte)? {
        1 if byte == *b"1" => Ok(()),
        _ => Err(io::Error::other("the reaper could not enter the cgroup")),
    }
}

/// Starts real processes with tokio and reports their exit.
pub struct ProcessSpawner {
    /// Our own executable, which provides the `reaper` subcommand.
    reaper_exe: PathBuf,
    env: Vec<(String, String)>,
    exits: mpsc::UnboundedSender<ChildExit>,
    children: Children,
    sandbox: Option<SandboxSettings>,
    /// `gamescope.apps_display` (possibly `"auto+<n>"`): apps run there, not on `env`'s display.
    apps_display: Option<String>,
    /// `[sandbox] lan_filter`: sandboxed apps without `lan` start in the filtered cgroup
    /// ([`crate::lan`]). Without it they do not start.
    lan: Option<LanFilter>,
}

impl ProcessSpawner {
    pub fn new(
        reaper_exe: PathBuf,
        env: Vec<(String, String)>,
        exits: mpsc::UnboundedSender<ChildExit>,
    ) -> Self {
        Self {
            reaper_exe,
            env,
            exits,
            children: Children::default(),
            sandbox: None,
            apps_display: None,
            lan: None,
        }
    }

    /// Run apps (under the reaper) on this display (`gamescope.apps_display`).
    pub fn with_apps_display(mut self, display: Option<String>) -> Self {
        self.apps_display = display;
        self
    }

    /// Run sandboxed spawns with these settings (without, they fail).
    pub fn with_sandbox(mut self, settings: SandboxSettings) -> Self {
        self.sandbox = Some(settings);
        self
    }

    /// Start sandboxed apps without `lan` in the filtered cgroup (without, they fail).
    pub fn with_lan_filter(mut self, filter: LanFilter) -> Self {
        self.lan = Some(filter);
        self
    }

    /// The bwrap command line for `argv` (`program` resolved) in `spec`'s sandbox, after
    /// preparing its home, and the system call filter bwrap reads (keep it until the spawn).
    /// Fails when bwrap or the filter is missing: an app never runs unsandboxed by mistake.
    fn sandboxed(
        &self,
        spec: &Spec,
        program: &Path,
        argv: &[String],
        env: &[(String, String)],
    ) -> io::Result<(Vec<String>, OwnedFd)> {
        let settings = self
            .sandbox
            .as_ref()
            .ok_or_else(|| io::Error::other("the sandbox is not set up"))?;
        let bwrap = which(&settings.bwrap.to_string_lossy()).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("no sandbox: bwrap not found ({})", settings.bwrap.display()),
            )
        })?;
        let var = |k: &str| {
            env.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .or_else(|| std::env::var(k).ok())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| io::Error::other(format!("no sandbox: {k} is not set")))
        };
        let seccomp = sandbox::seccomp::pipe().map_err(|e| {
            io::Error::new(e.kind(), format!("no sandbox: no system call filter ({e})"))
        })?;
        let host = Host {
            bwrap,
            home: var("HOME")?.into(),
            runtime_dir: var("XDG_RUNTIME_DIR")?.into(),
            display: var("DISPLAY")?,
            app_env: settings
                .app_env
                .as_ref()
                .and_then(|l| Some((l.clone(), std::fs::canonicalize(l).ok()?))),
            pulse_socket: settings.pulse_socket.clone(),
            pulse_record_socket: settings.pulse_record_socket.clone(),
            osk_socket: settings.osk_socket.clone(),
            render_nodes: sandbox::render_nodes(Path::new("/dev/dri")),
            seccomp_fd: Some(seccomp.as_raw_fd()),
        };
        sandbox::prepare(&host.home, spec)?;
        let argv = sandbox::args(&host, spec, program, &argv[1..]).map_err(io::Error::other)?;
        Ok((argv, seccomp))
    }

    /// Register the processes started here with `children` (to end them at shutdown).
    pub fn with_children(mut self, children: Children) -> Self {
        self.children = children;
        self
    }
}

impl Spawner for ProcessSpawner {
    fn spawn(
        &mut self,
        key: &str,
        argv: &[String],
        app_id: Option<u32>,
        sandbox: Option<&Spec>,
    ) -> io::Result<u32> {
        let program = argv
            .first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty command"))?;
        // Resolve up front: under the reaper a missing program would only show as exit 127.
        let program = which(program)?;
        let mut env = spawn_env(&self.env, app_id.is_some());
        if let (Some(_), Some(display)) = (app_id, &self.apps_display) {
            env = with_apps_display(env, display)?;
        }
        // A sandboxed app without `lan`: only while the filter is on; its reaper moves into
        // the filtered cgroup before it starts anything.
        let nolan = match sandbox {
            Some(spec) if !spec.grants.lan => Some(
                self.lan
                    .as_ref()
                    .ok_or_else(crate::lan::not_on)?
                    .ready()?
                    .nolan_dir(),
            ),
            _ => None,
        };
        // The system call filter: bwrap reads it at start (`--seccomp`), ours goes after the spawn.
        let mut seccomp = None;
        let mut cmd = match (app_id, sandbox) {
            (Some(id), Some(spec)) => {
                let (bwrap, filter) = self.sandboxed(spec, &program, argv, &env)?;
                seccomp = Some(filter);
                let mut cmd = tokio::process::Command::new(&self.reaper_exe);
                cmd.args(reaper::args_in(id, nolan.as_deref(), &bwrap));
                cmd
            }
            (None, Some(_)) => {
                return Err(io::Error::other("a sandboxed app needs an app id"));
            }
            (Some(id), None) => {
                let mut cmd = tokio::process::Command::new(&self.reaper_exe);
                cmd.args(reaper::args(id, argv));
                cmd
            }
            (None, None) => {
                let mut cmd = tokio::process::Command::new(&program);
                cmd.args(&argv[1..]);
                cmd
            }
        };
        cmd.envs(env).stdin(Stdio::null()).kill_on_drop(false);
        if app_id.is_some() {
            // Also when the daemon itself was started with them.
            for k in HIDDEN_FROM_APPS {
                cmd.env_remove(k);
            }
        }
        // The reaper says on this pipe (its stdin) when it is in nolan.
        let in_nolan = match &nolan {
            Some(_) => {
                let (read, write) = std::io::pipe()?;
                cmd.stdin(write);
                Some(read)
            }
            None => None,
        };
        let mut child = cmd.spawn()?;
        drop(seccomp);
        // Our copy of the pipe's write end goes with it: end of file once the reaper is gone.
        drop(cmd);
        if let Some(read) = in_nolan
            && let Err(e) = wait_in_cgroup(read, IN_CGROUP_TIMEOUT)
        {
            let _ = child.start_kill();
            tokio::spawn(async move { child.wait().await });
            return Err(io::Error::new(
                e.kind(),
                format!("no local network filter: {e}"),
            ));
        }
        let pid = child.id().unwrap_or_default();
        let children = self.children.clone();
        if !children.add(pid) {
            // Shutting down: no new apps.
            let _ = child.start_kill();
            return Err(io::Error::other("the session daemon is shutting down"));
        }
        debug!(
            key,
            pid,
            ?argv,
            ?app_id,
            sandboxed = sandbox.is_some(),
            lan = nolan.is_none(),
            "spawned"
        );
        let exits = self.exits.clone();
        let key = key.to_string();
        tokio::spawn(async move {
            let code = match child.wait().await {
                Ok(status) => {
                    use std::os::unix::process::ExitStatusExt;
                    status.code().or_else(|| status.signal().map(|s| 128 + s))
                }
                Err(e) => {
                    warn!(key, pid, "waiting for child: {e}");
                    None
                }
            };
            children.remove(pid);
            let _ = exits.send(ChildExit { key, pid, code });
        });
        Ok(pid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_finds_programs() {
        assert!(which("sh").is_ok());
        assert!(which("/definitely/not/here").is_err());
        assert!(which("definitely-not-a-program-hinteros").is_err());
        assert_eq!(
            which("definitely-not-a-program-hinteros")
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn apps_get_no_wayland_display() {
        let env: Vec<(String, String)> = [
            ("DISPLAY", ":1"),
            ("WAYLAND_DISPLAY", "gamescope-0"),
            ("GAMESCOPE_WAYLAND_DISPLAY", "gamescope-0"),
            ("HINTEROS_OSK_SOCKET", "/run/user/1000/hinteros/osk.sock"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let keys =
            |app| -> Vec<String> { spawn_env(&env, app).into_iter().map(|(k, _)| k).collect() };
        assert_eq!(keys(true), ["DISPLAY", "HINTEROS_OSK_SOCKET"]);
        // Steam keeps them.
        assert_eq!(keys(false).len(), 4);
    }

    #[test]
    fn apps_get_the_apps_display() {
        let env: Vec<(String, String)> = [("DISPLAY", ":0"), ("HOME", "/home/x")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let display = |e: &[(String, String)]| -> Vec<String> {
            e.iter()
                .filter(|(k, _)| k == "DISPLAY")
                .map(|(_, v)| v.clone())
                .collect()
        };
        let out = with_apps_display(env.clone(), ":2").unwrap();
        assert_eq!(display(&out), [":2"]);
        assert_eq!(out.len(), 2);
        // A display that is not there: no fallback to the home app's.
        let err = with_apps_display(env, "auto+999").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("no app display"), "{err}");
    }

    #[tokio::test]
    async fn spawned_apps_run_on_the_apps_display() {
        let t = tempfile::tempdir().unwrap();
        let out = t.path().join("display");
        let (tx, mut rx) = mpsc::unbounded_channel();
        // A stand-in reaper: runs what follows `--`.
        let reaper = t.path().join("reaper");
        std::fs::write(
            &reaper,
            "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&reaper, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let env = vec![("DISPLAY".to_string(), ":0".to_string())];
        let mut s = ProcessSpawner::new(reaper, env, tx).with_apps_display(Some(":2".into()));
        let argv = |f: &Path| -> Vec<String> {
            vec![
                "sh".into(),
                "-c".into(),
                format!("printf %s \"$DISPLAY\" > {}", f.display()),
            ]
        };
        // An app (under the reaper) gets the apps' display ...
        s.spawn("a", &argv(&out), Some(1234), None).unwrap();
        rx.recv().await.unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), ":2");
        // ... Steam (no reaper) keeps the session's.
        s.spawn("steam", &argv(&out), None, None).unwrap();
        rx.recv().await.unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), ":0");
        // An apps' display that does not exist fails the launch.
        let mut s = s.with_apps_display(Some("auto+999".into()));
        assert!(s.spawn("b", &argv(&out), Some(1235), None).is_err());
    }

    #[tokio::test]
    async fn apps_without_lan_start_only_with_the_filter_on() {
        use crate::permissions::Grants;
        let (tx, _rx) = mpsc::unbounded_channel();
        let settings = SandboxSettings {
            bwrap: "definitely-not-bwrap-hinteros".into(),
            pulse_socket: "pulse/native".into(),
            pulse_record_socket: "pulse/native".into(),
            app_env: None,
            osk_socket: None,
        };
        let spec = |lan| Spec {
            id: "a".into(),
            grants: Grants {
                lan,
                ..Grants::default()
            },
            kiosk: false,
        };
        let argv = ["sh".to_string()];
        let err = |s: &mut ProcessSpawner, lan| {
            s.spawn("a", &argv, Some(1), Some(&spec(lan)))
                .unwrap_err()
                .to_string()
        };
        let mut s = ProcessSpawner::new("reaper".into(), vec![], tx).with_sandbox(settings);
        // No filter at all, or one that is not on: no start.
        assert_eq!(err(&mut s, false), "no local network filter");
        let (filter, _ask) = LanFilter::new(None);
        let mut s = s.with_lan_filter(filter);
        assert_eq!(err(&mut s, false), "no local network filter");
        // With lan the filter does not matter (here bwrap is missing).
        assert!(err(&mut s, true).contains("bwrap not found"));
    }

    #[tokio::test]
    async fn apps_without_lan_start_only_once_their_reaper_is_in_nolan() {
        use crate::lan::Cgroups;
        use crate::permissions::Grants;
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let unit = "/user.slice/u.service";
        let unit_dir = t.path().join("cg/user.slice/u.service");
        std::fs::create_dir_all(&unit_dir).unwrap();
        let cgroups = Cgroups::setup(&t.path().join("cg"), unit).unwrap();
        let (filter, ask) = LanFilter::new(Some(cgroups));
        tokio::spawn(crate::lan::run(filter.clone(), ask, |_| async { Ok(()) }));
        while !filter.is_on() {
            tokio::task::yield_now().await;
        }
        // A stand-in reaper: what the real one does with Cgroup= (see tests/reaper.rs), then
        // stops (here bwrap is sh).
        let reaper = t.path().join("reaper");
        std::fs::write(
            &reaper,
            "#!/bin/sh\nfor a; do case \"$a\" in Cgroup=*) d=${a#Cgroup=};; --) break;; esac; done\n\
             [ -n \"$d\" ] || exit 2\necho $$ > \"$d/cgroup.procs\" || exit 125\nprintf 1 >&0\n",
        )
        .unwrap();
        std::fs::set_permissions(&reaper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let settings = SandboxSettings {
            bwrap: "sh".into(),
            pulse_socket: "pulse/native".into(),
            pulse_record_socket: "pulse/native".into(),
            app_env: None,
            osk_socket: None,
        };
        let env = [
            ("HOME", "home"),
            ("XDG_RUNTIME_DIR", "run"),
            ("DISPLAY", ":0"),
        ]
        .map(|(k, v)| {
            let v = if v.starts_with(':') {
                v.to_string()
            } else {
                t.path().join(v).display().to_string()
            };
            (k.to_string(), v)
        })
        .to_vec();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut s = ProcessSpawner::new(reaper, env, tx)
            .with_sandbox(settings)
            .with_lan_filter(filter);
        let spec = Spec {
            id: "a".into(),
            grants: Grants {
                lan: false,
                ..Grants::default()
            },
            kiosk: false,
        };
        let argv = ["sh".to_string()];
        let pid = s.spawn("a", &argv, Some(1), Some(&spec)).unwrap();
        assert_eq!(
            std::fs::read_to_string(unit_dir.join("nolan/cgroup.procs")).unwrap(),
            format!("{pid}\n")
        );
        assert_eq!(rx.recv().await.unwrap().code, Some(0));
        // The daemon itself stayed in its leaf.
        assert_eq!(
            std::fs::read_to_string(unit_dir.join("daemon/cgroup.procs")).unwrap(),
            format!("{}\n", std::process::id())
        );
        // The reaper cannot enter nolan: the launch fails.
        std::fs::remove_dir_all(unit_dir.join("nolan")).unwrap();
        let e = s.spawn("a", &argv, Some(1), Some(&spec)).unwrap_err();
        assert!(e.to_string().starts_with("no local network filter"), "{e}");
    }

    #[tokio::test]
    async fn bwrap_gets_the_system_call_filter() {
        use crate::permissions::Grants;
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        // A stand-in reaper: copies what it can read from bwrap's `--seccomp <fd>`.
        let out = t.path().join("filter");
        let reaper = t.path().join("reaper");
        std::fs::write(
            &reaper,
            format!(
                "#!/bin/sh\nwhile [ $# -gt 0 ]; do [ \"$1\" = --seccomp ] && fd=$2; shift; done\n\
                 [ -n \"$fd\" ] || exit 2\neval \"cat <&$fd\" > {}\n",
                out.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&reaper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let settings = SandboxSettings {
            bwrap: "sh".into(),
            pulse_socket: "pulse/native".into(),
            pulse_record_socket: "pulse/native".into(),
            app_env: None,
            osk_socket: None,
        };
        let env = [
            ("HOME", "home"),
            ("XDG_RUNTIME_DIR", "run"),
            ("DISPLAY", ":0"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .to_vec();
        let env: Vec<_> = env
            .into_iter()
            .map(|(k, v)| match k.as_str() {
                "DISPLAY" => (k, v),
                _ => (k, t.path().join(v).display().to_string()),
            })
            .collect();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut s = ProcessSpawner::new(reaper, env, tx).with_sandbox(settings);
        let spec = Spec {
            id: "a".into(),
            grants: Grants {
                lan: true,
                ..Grants::default()
            },
            kiosk: false,
        };
        s.spawn("a", &["sh".to_string()], Some(1), Some(&spec))
            .unwrap();
        assert_eq!(rx.recv().await.unwrap().code, Some(0));
        assert_eq!(
            std::fs::read(&out).unwrap(),
            sandbox::seccomp::program().unwrap()
        );
    }

    #[test]
    fn steam_binary_needs_an_executable_file() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let steam = t.path().join("steam");
        assert_eq!(steam_binary(&steam), None, "missing");
        std::fs::write(&steam, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&steam, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(steam_binary(&steam), None, "not executable");
        std::fs::set_permissions(&steam, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(steam_binary(&steam), Some(steam));
        assert_eq!(steam_binary(t.path()), None, "a directory");
        assert_eq!(
            steam_binary(Path::new("definitely-not-steam-hinteros")),
            None
        );
    }
}
