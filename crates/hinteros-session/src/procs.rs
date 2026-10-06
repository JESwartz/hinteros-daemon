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

    fn steam_running(&mut self) -> bool {
        list().iter().any(|p| p.comm == "steam")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
