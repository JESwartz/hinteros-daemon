//! Starting app processes and noticing when they end.
//!
//! [`Spawner`] is the seam: [`ProcessSpawner`] starts real processes (apps under our
//! [`crate::reaper`], Steam directly), tests use a recording mock.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::sync::mpsc;
use tracing::{debug, warn};

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

/// Starts real processes with tokio and reports their exit.
pub struct ProcessSpawner {
    /// Our own executable, which provides the `reaper` subcommand.
    reaper_exe: PathBuf,
    env: Vec<(String, String)>,
    exits: mpsc::UnboundedSender<ChildExit>,
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
        }
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
        cmd.envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .kill_on_drop(false);
        let mut child = cmd.spawn()?;
        let pid = child.id().unwrap_or_default();
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
}
