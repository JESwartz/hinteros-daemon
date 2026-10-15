//! Starting app processes and noticing when they end.
//!
//! [`Spawner`] is the seam: [`ProcessSpawner`] starts real processes (apps under our
//! [`crate::reaper`], Steam directly), tests use a recording mock.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::procs::Children;
use crate::reaper;

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
    /// that app id. Reports the end of the process as a [`ChildExit`] with `key`.
    fn spawn(&mut self, key: &str, argv: &[String], app_id: Option<u32>) -> io::Result<u32>;
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

/// Starts real processes with tokio and reports their exit.
pub struct ProcessSpawner {
    /// Our own executable, which provides the `reaper` subcommand.
    reaper_exe: PathBuf,
    env: Vec<(String, String)>,
    exits: mpsc::UnboundedSender<ChildExit>,
    children: Children,
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
        }
    }

    /// Register the processes started here with `children` (to end them at shutdown).
    pub fn with_children(mut self, children: Children) -> Self {
        self.children = children;
        self
    }
}

impl Spawner for ProcessSpawner {
    fn spawn(&mut self, key: &str, argv: &[String], app_id: Option<u32>) -> io::Result<u32> {
        let program = argv
            .first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty command"))?;
        // Resolve up front: under the reaper a missing program would only show as exit 127.
        let program = which(program)?;
        let mut cmd = match app_id {
            Some(id) => {
                let mut cmd = tokio::process::Command::new(&self.reaper_exe);
                cmd.args(reaper::args(id, argv));
                cmd
            }
            None => {
                let mut cmd = tokio::process::Command::new(&program);
                cmd.args(&argv[1..]);
                cmd
            }
        };
        cmd.envs(spawn_env(&self.env, app_id.is_some()))
            .stdin(Stdio::null())
            .kill_on_drop(false);
        if app_id.is_some() {
            // Also when the daemon itself was started with them.
            for k in HIDDEN_FROM_APPS {
                cmd.env_remove(k);
            }
        }
        let mut child = cmd.spawn()?;
        let pid = child.id().unwrap_or_default();
        let children = self.children.clone();
        if !children.add(pid) {
            // Shutting down: no new apps.
            let _ = child.start_kill();
            return Err(io::Error::other("the session daemon is shutting down"));
        }
        debug!(key, pid, ?argv, ?app_id, "spawned");
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
