//! Who is on the other end of a connection: `SO_PEERCRED` at accept time, plus a pidfd from
//! `SO_PEERPIDFD` so the process can be named to polkit without the PID being recycled.

use std::os::fd::{AsFd, OwnedFd};
use std::path::PathBuf;

use nix::sys::socket::{getsockopt, sockopt};
use rustix::event::{PollFd, PollFlags, poll};
use tokio::net::UnixStream;

#[derive(Debug)]
pub struct Peer {
    pub uid: u32,
    pub gid: u32,
    pub pid: Option<u32>,
    /// `None` on kernels without `SO_PEERPIDFD` (< 6.5): mutating methods are refused.
    pub pidfd: Option<OwnedFd>,
    /// `/proc/<pid>/exe` at accept time (audit only).
    pub exe: Option<PathBuf>,
}

impl Peer {
    pub fn of(stream: &UnixStream) -> std::io::Result<Self> {
        let cred = stream.peer_cred()?;
        let pidfd = getsockopt(&stream.as_fd(), sockopt::PeerPidfd).ok();
        let pid = cred.pid().and_then(|p| u32::try_from(p).ok());
        let exe = pid.and_then(|p| std::fs::read_link(format!("/proc/{p}/exe")).ok());
        Ok(Self {
            uid: cred.uid(),
            gid: cred.gid(),
            pid,
            pidfd,
            exe,
        })
    }

    /// Whether the peer process still exists (a pidfd becomes readable when it exits).
    pub fn alive(&self) -> bool {
        let Some(fd) = &self.pidfd else {
            return false;
        };
        let mut fds = [PollFd::new(fd, PollFlags::IN)];
        match poll(&mut fds, Some(&rustix::time::Timespec::default())) {
            Ok(0) => true,
            Ok(_) => fds[0].revents().is_empty(),
            Err(_) => false,
        }
    }

    /// The process start time (`/proc/<pid>/stat` field 22), which polkit uses with the pid.
    /// Read while the pidfd shows the process alive, so it belongs to the same process.
    pub fn start_time(&self) -> Option<u64> {
        let pid = self.pid?;
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_comm = stat.rsplit_once(')')?.1;
        // Fields after comm start at field 3 (state); start time is field 22.
        let t = after_comm.split_whitespace().nth(22 - 3)?.parse().ok()?;
        self.alive().then_some(t)
    }

    pub fn exe_display(&self) -> String {
        self.exe
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "?".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn identifies_the_peer() {
        let (a, _b) = UnixStream::pair().unwrap();
        let p = Peer::of(&a).unwrap();
        assert_eq!(p.uid, rustix::process::getuid().as_raw());
        assert_eq!(p.pid, Some(std::process::id()));
        assert!(p.pidfd.is_some(), "this kernel has SO_PEERPIDFD");
        assert!(p.alive());
        assert!(p.start_time().is_some());
        assert!(p.exe.is_some());
    }
}
