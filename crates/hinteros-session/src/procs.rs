//! Finding and signalling the session user's processes through `/proc`: what `apps.close`
//! needs (an app's process tree, Steam's reaper for a game) and whether the Steam client runs.
//!
//! [`ProcessControl`] is the seam: [`Procs`] is the real thing, tests use a recording mock.

use std::fs;
use std::os::unix::fs::MetadataExt;

use rustix::process::{Pid, Signal, kill_process};
use tracing::debug;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sig {
    Term,
    Kill,
}

pub trait ProcessControl: Send {
    /// Send `sig` to one process.
    fn signal(&mut self, pid: u32, sig: Sig);
    /// Every process below `pid` (children, their children, ...), not `pid` itself.
    fn descendants(&mut self, pid: u32) -> Vec<u32>;
    /// Steam's reaper processes for a game: named `reaper`, with `SteamLaunch` and
    /// `AppId=<app_id>` before `--` on the command line.
    fn steam_reapers(&mut self, app_id: u32) -> Vec<u32>;
    /// The app ids of the Steam games running now: those with a live Steam reaper
    /// (`reaper SteamLaunch AppId=<id> -- ...`).
    fn steam_games(&mut self) -> Vec<u32> {
        Vec::new()
    }
    /// Whether the Steam client (a process named `steam`) is running.
    fn steam_running(&mut self) -> bool;
}

/// One process, as far as we care.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub comm: String,
}

/// Parse `/proc/<pid>/stat` (`pid (comm) state ppid ...`; comm may contain spaces and parens).
pub fn parse_stat(stat: &str) -> Option<Proc> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let pid = stat[..open].trim().parse().ok()?;
    let comm = stat.get(open + 1..close)?.to_string();
    let mut rest = stat.get(close + 1..)?.split_whitespace();
    let _state = rest.next()?;
    let ppid = rest.next()?.parse().ok()?;
    Some(Proc { pid, ppid, comm })
}

/// Whether a reaper command line (NUL-separated arguments) launches `app_id` the way Steam's
/// reaper does.
pub fn is_steam_reaper_cmdline(cmdline: &[u8], app_id: u32) -> bool {
    let wanted = format!("AppId={app_id}");
    let args: Vec<&[u8]> = cmdline.split(|&b| b == 0).collect();
    let before = args
        .iter()
        .position(|a| *a == b"--")
        .map_or(&args[..], |i| &args[..i]);
    before.contains(&&b"SteamLaunch"[..]) && before.contains(&wanted.as_bytes())
}

/// The app id in a reaper command line (`reaper SteamLaunch AppId=<id> -- ...`), if it is one.
pub fn reaper_app_id(cmdline: &[u8]) -> Option<u32> {
    let args: Vec<&[u8]> = cmdline.split(|&b| b == 0).collect();
    let before = args
        .iter()
        .position(|a| *a == b"--")
        .map_or(&args[..], |i| &args[..i]);
    if !before.contains(&&b"SteamLaunch"[..]) {
        return None;
    }
    before.iter().find_map(|a| {
        std::str::from_utf8(a)
            .ok()?
            .strip_prefix("AppId=")?
            .parse()
            .ok()
    })
}

/// The gamescope app id `pid` runs under: that of its outermost ancestor (or itself) named
/// `reaper` that launched an app (as gamescope decides it). `cmdline` reads a process's
/// command line.
pub fn managed_app_id(
    procs: &[Proc],
    pid: u32,
    cmdline: impl Fn(u32) -> Option<Vec<u8>>,
) -> Option<u32> {
    let mut found = None;
    let mut at = pid;
    // A process table has no cycles, but do not trust it.
    for _ in 0..procs.len() + 1 {
        let Some(p) = procs.iter().find(|p| p.pid == at) else {
            break;
        };
        if p.comm == "reaper"
            && let Some(id) = cmdline(p.pid).and_then(|c| reaper_app_id(&c))
        {
            found = Some(id);
        }
        if p.ppid == 0 || p.ppid == at {
            break;
        }
        at = p.ppid;
    }
    found
}

/// [`managed_app_id`] on the real `/proc`.
pub fn managed_app_id_of(procs: &[Proc], pid: u32) -> Option<u32> {
    managed_app_id(procs, pid, |p| fs::read(format!("/proc/{p}/cmdline")).ok())
}

/// Every pid in `procs` below `root`.
pub fn descendants_of(procs: &[Proc], root: u32) -> Vec<u32> {
    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for p in procs.iter().filter(|p| p.ppid == parent && p.pid != root) {
            if !found.contains(&p.pid) {
                found.push(p.pid);
                frontier.push(p.pid);
            }
        }
    }
    found
}

/// The session user's processes, from `/proc`.
pub fn list() -> Vec<Proc> {
    let me = rustix::process::geteuid().as_raw();
    let Ok(dir) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
        })
        .filter(|e| e.metadata().is_ok_and(|m| m.uid() == me))
        .filter_map(|e| parse_stat(&fs::read_to_string(e.path().join("stat")).ok()?))
        .collect()
}

/// The name of process `pid` (`/proc/<pid>/comm`).
pub fn comm(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim_end_matches('\n').to_string())
}

/// [`ProcessControl`] on the real `/proc`. Only ever signals the session user's processes.
pub struct Procs;

impl ProcessControl for Procs {
    fn signal(&mut self, pid: u32, sig: Sig) {
        let Some(p) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
            return;
        };
        let signal = match sig {
            Sig::Term => Signal::TERM,
            Sig::Kill => Signal::KILL,
        };
        if let Err(e) = kill_process(p, signal) {
            debug!(pid, ?sig, "kill: {e}");
        }
    }

    fn descendants(&mut self, pid: u32) -> Vec<u32> {
        descendants_of(&list(), pid)
    }

    fn steam_reapers(&mut self, app_id: u32) -> Vec<u32> {
        list()
            .into_iter()
            .filter(|p| p.comm == "reaper")
            .filter(|p| {
                fs::read(format!("/proc/{}/cmdline", p.pid))
                    .is_ok_and(|c| is_steam_reaper_cmdline(&c, app_id))
            })
            .map(|p| p.pid)
            .collect()
    }

    fn steam_games(&mut self) -> Vec<u32> {
        let mut ids: Vec<u32> = list()
            .into_iter()
            .filter(|p| p.comm == "reaper")
            .filter_map(|p| reaper_app_id(&fs::read(format!("/proc/{}/cmdline", p.pid)).ok()?))
            .filter(|&id| id != 0 && id != hinteros_api::session::STEAM_CLIENT_GAMESCOPE_APP_ID)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    fn steam_running(&mut self) -> bool {
        list().iter().any(|p| p.comm == "steam")
    }
}

/// The processes the daemon started and still has (apps' reapers, the launcher, the
/// on-screen keyboard), so that it can end them when it shuts down. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct Children(std::sync::Arc<std::sync::Mutex<ChildrenInner>>);

#[derive(Debug, Default)]
struct ChildrenInner {
    pids: std::collections::BTreeSet<u32>,
    closing: bool,
}

impl Children {
    /// Remember a child. `false` once the daemon is shutting down: the caller must end the
    /// child itself.
    pub fn add(&self, pid: u32) -> bool {
        let mut inner = self.0.lock().unwrap();
        if inner.closing {
            return false;
        }
        inner.pids.insert(pid);
        true
    }

    /// The child was waited for.
    pub fn remove(&self, pid: u32) {
        self.0.lock().unwrap().pids.remove(&pid);
    }

    pub fn is_closing(&self) -> bool {
        self.0.lock().unwrap().closing
    }

    fn pids(&self) -> Vec<u32> {
        self.0.lock().unwrap().pids.iter().copied().collect()
    }

    /// End every child: SIGTERM (a reaper forwards it to its app's process group), wait up to
    /// `grace` for them to be gone (the tasks that wait for them call [`Self::remove`]), then
    /// SIGKILL what is left, descendants first. No child is added afterwards.
    pub async fn shutdown(&self, grace: std::time::Duration) {
        self.0.lock().unwrap().closing = true;
        let mut procs = Procs;
        let pids = self.pids();
        if pids.is_empty() {
            return;
        }
        debug!(?pids, "ending child processes");
        for &pid in &pids {
            procs.signal(pid, Sig::Term);
        }
        let deadline = tokio::time::Instant::now() + grace;
        while !self.pids().is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let left = self.pids();
        if left.is_empty() {
            return;
        }
        debug!(
            ?left,
            "children still running after the grace period: killing"
        );
        let table = list();
        for &pid in &left {
            for d in descendants_of(&table, pid) {
                procs.signal(d, Sig::Kill);
            }
            procs.signal(pid, Sig::Kill);
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        while !self.pids().is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_app_is_the_outermost_reaper_above_a_process() {
        let p = |pid, ppid, comm: &str| Proc {
            pid,
            ppid,
            comm: comm.into(),
        };
        let procs = vec![
            p(1, 0, "init"),
            p(10, 1, "reaper"),
            p(11, 10, "sh"),
            p(12, 11, "game"),
            p(20, 1, "reaper"),
            p(21, 20, "steam"),
        ];
        let cmdline = |pid| match pid {
            10 => Some(b"hinteros-session\0reaper\0SteamLaunch\0AppId=570\0--\0game\0".to_vec()),
            20 => Some(b"reaper\0-x\0--\0AppId=9\0".to_vec()),
            _ => None,
        };
        assert_eq!(managed_app_id(&procs, 12, cmdline), Some(570));
        assert_eq!(managed_app_id(&procs, 10, cmdline), Some(570));
        assert_eq!(managed_app_id(&procs, 21, cmdline), None);
        assert_eq!(managed_app_id(&procs, 1, cmdline), None);
        assert_eq!(managed_app_id(&procs, 999, cmdline), None);
    }

    #[test]
    fn parses_stat_lines() {
        assert_eq!(
            parse_stat("4242 (Web Content (x)) S 4200 4242 4242 0 -1 4194560 ..."),
            Some(Proc {
                pid: 4242,
                ppid: 4200,
                comm: "Web Content (x)".into()
            })
        );
        assert_eq!(parse_stat("garbage"), None);
        let me = std::process::id();
        let mine = parse_stat(&fs::read_to_string("/proc/self/stat").unwrap()).unwrap();
        assert_eq!(mine.pid, me);
        assert!(list().iter().any(|p| p.pid == me));
        assert!(comm(me).is_some());
    }

    #[test]
    fn steam_reaper_command_lines() {
        let c = b"/home/u/.local/share/Steam/ubuntu12_32/reaper\0SteamLaunch\0AppId=620\0--\0/x/steam-launch-wrapper\0--\0game\0";
        assert!(is_steam_reaper_cmdline(c, 620));
        assert!(!is_steam_reaper_cmdline(c, 62));
        assert!(!is_steam_reaper_cmdline(b"reaper\0AppId=620\0--\0x\0", 620));
        assert!(!is_steam_reaper_cmdline(
            b"reaper\0SteamLaunch\0--\0AppId=620\0",
            620
        ));
    }

    #[test]
    fn walks_trees() {
        let p = |pid, ppid| Proc {
            pid,
            ppid,
            comm: String::new(),
        };
        let procs = [p(1, 0), p(10, 1), p(11, 10), p(12, 11), p(13, 10), p(20, 1)];
        let mut d = descendants_of(&procs, 10);
        d.sort();
        assert_eq!(d, [11, 12, 13]);
        assert!(descendants_of(&procs, 12).is_empty());
    }

    #[test]
    fn finds_real_children() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 30 & wait"])
            .spawn()
            .unwrap();
        let pid = child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let kids = loop {
            let kids = Procs.descendants(pid);
            // The forked child is still `sh` until it has exec'd `sleep`.
            if !kids.is_empty() && comm(kids[0]).as_deref() == Some("sleep") {
                break kids;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(kids.len(), 1);
        assert_eq!(comm(kids[0]).as_deref(), Some("sleep"));
        Procs.signal(kids[0], Sig::Kill);
        Procs.signal(pid, Sig::Term);
        let _ = child.wait();
    }
}
