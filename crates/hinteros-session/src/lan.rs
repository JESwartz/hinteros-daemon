//! The `lan` permission (`[sandbox] lan_filter`, `hinteros-docs/design/sandbox.md`): a
//! sandboxed app without it cannot reach the local network (other devices at home, this box's
//! own services), only DNS to the box's nameservers and the internet.
//!
//! The daemon's unit has `Delegate=yes`, so the daemon owns its cgroup's subtree. At start it
//! moves itself into the leaf `daemon` and creates `nolan` beside it ([`Cgroups::setup`]), then
//! asks the system service to filter `nolan` (system API `lan_filter.set`): the service loads an
//! nftables table that matches the sockets of that cgroup and of every cgroup below it.
//!
//! The daemon itself never leaves `daemon`. An app without `lan` gets `Cgroup=<nolan>` on its
//! reaper's command line: the reaper moves itself into `nolan` before it starts anything (a
//! child keeps its parent's cgroup), so bwrap and the app run there too, and tells the daemon
//! (one byte on a pipe, see [`crate::reaper`]). If the reaper cannot move, it starts nothing and
//! the launch fails. A sandboxed app cannot leave `nolan`: `/sys` is read-only inside, and a
//! cgroup namespace only reaches below its root.
//!
//! Fail closed: until the system service says the filter is on, an app without `lan` does not
//! start (`app_launch_failed`, "no local network filter"); each such launch asks again. `nolan`
//! is never removed and created again while the daemon runs (nftables matches the cgroup it
//! found when it loaded the table). When the daemon ends it clears its filter.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{info, warn};

/// The leaf the daemon runs in.
pub const LEAF: &str = "daemon";

/// The cgroup apps without `lan` start in.
pub const NOLAN: &str = "nolan";

/// The cgroup v2 mount.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";

const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);

/// The cgroup v2 path in `/proc/self/cgroup`.
pub fn own_path(proc_self_cgroup: &str) -> Option<&str> {
    proc_self_cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .filter(|p| p.starts_with('/'))
}

/// The daemon's two cgroups: `<base>/daemon` (the daemon) and `<base>/nolan`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cgroups {
    root: PathBuf,
    /// The unit's cgroup, as in `/proc/self/cgroup`.
    base: String,
}

impl Cgroups {
    /// Under the cgroup mount `root`, from our cgroup `own` (the unit's, or already its
    /// `daemon` leaf): create `daemon` and `nolan` (or keep them) and move this process into
    /// `daemon`.
    pub fn setup(root: &Path, own: &str) -> io::Result<Self> {
        let base = own.strip_suffix(&format!("/{LEAF}")).unwrap_or(own);
        if base.is_empty() || base == "/" || !base.starts_with('/') {
            return Err(io::Error::other(format!("no cgroup of our own ({own})")));
        }
        let c = Self {
            root: root.to_path_buf(),
            base: base.to_owned(),
        };
        for leaf in [LEAF, NOLAN] {
            match std::fs::create_dir(c.dir(leaf)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(io::Error::new(
                        e.kind(),
                        format!("{}: {e}", c.dir(leaf).display()),
                    ));
                }
            }
        }
        c.enter(LEAF)?;
        Ok(c)
    }

    /// From `/proc/self/cgroup` and the real mount.
    pub fn setup_here() -> io::Result<Self> {
        let text = std::fs::read_to_string("/proc/self/cgroup")?;
        let own = own_path(&text)
            .ok_or_else(|| io::Error::other("no cgroup v2 path in /proc/self/cgroup"))?;
        Self::setup(Path::new(CGROUP_ROOT), own)
    }

    fn dir(&self, leaf: &str) -> PathBuf {
        self.root.join(self.base.trim_start_matches('/')).join(leaf)
    }

    /// `nolan`, as the system service takes it.
    pub fn nolan(&self) -> String {
        format!("{}/{NOLAN}", self.base)
    }

    /// `nolan`'s directory, for the reaper's `Cgroup=`.
    pub fn nolan_dir(&self) -> PathBuf {
        self.dir(NOLAN)
    }

    /// Move this process (every thread) into `leaf`.
    fn enter(&self, leaf: &str) -> io::Result<()> {
        let procs = self.dir(leaf).join("cgroup.procs");
        std::fs::write(&procs, format!("{}\n", std::process::id()))
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", procs.display())))
    }
}

/// Whether the filter is on, shared by the spawner and the task that asks the system for it.
/// Cheap to clone.
#[derive(Debug, Clone)]
pub struct LanFilter {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    /// `None`: the cgroups could not be set up; apps without `lan` never start.
    cgroups: Option<Cgroups>,
    on: AtomicBool,
    ask: mpsc::UnboundedSender<()>,
}

/// No filter: why an app without `lan` does not start.
pub fn not_on() -> io::Error {
    io::Error::other("no local network filter")
}

impl LanFilter {
    /// The filter for `cgroups`, off until [`run`] turns it on (which reads the receiver).
    pub fn new(cgroups: Option<Cgroups>) -> (Self, mpsc::UnboundedReceiver<()>) {
        let (ask, rx) = mpsc::unbounded_channel();
        let inner = Inner {
            cgroups,
            on: AtomicBool::new(false),
            ask,
        };
        (
            Self {
                inner: Arc::new(inner),
            },
            rx,
        )
    }

    /// The cgroups, if the filter is on. Else asks the system again and fails.
    pub fn ready(&self) -> io::Result<&Cgroups> {
        let cgroups = self.inner.cgroups.as_ref().ok_or_else(not_on)?;
        if self.inner.on.load(Ordering::SeqCst) {
            Ok(cgroups)
        } else {
            let _ = self.inner.ask.send(());
            Err(not_on())
        }
    }

    pub fn is_on(&self) -> bool {
        self.inner.on.load(Ordering::SeqCst)
    }

    /// The cgroup to filter, if there is one.
    pub fn nolan(&self) -> Option<String> {
        self.inner.cgroups.as_ref().map(Cgroups::nolan)
    }
}

/// Ask the system to filter `nolan` (`set(nolan)`, `lan_filter.set`) until it says yes: at
/// once, then after a growing wait or when a launch asks. Ends when every [`LanFilter`] is gone.
pub async fn run<F, Fut>(filter: LanFilter, mut ask: mpsc::UnboundedReceiver<()>, set: F)
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let Some(nolan) = filter.nolan() else {
        return;
    };
    // Only the receiver and this copy are left once the daemon drops its spawner.
    let weak = Arc::downgrade(&filter.inner);
    drop(filter);
    let mut wait = RETRY_MIN;
    let mut warned = false;
    loop {
        let Some(inner) = weak.upgrade() else { return };
        if !inner.on.load(Ordering::SeqCst) {
            match set(nolan.clone()).await {
                Ok(()) => {
                    inner.on.store(true, Ordering::SeqCst);
                    info!(cgroup = %nolan, "local network filter on: apps without lan may start");
                }
                Err(e) => {
                    if !warned {
                        warn!(cgroup = %nolan, "no local network filter yet (apps without lan do not start): {e}");
                        warned = true;
                    }
                }
            }
        }
        let on = inner.on.load(Ordering::SeqCst);
        drop(inner);
        if on {
            // Nothing to do any more but to exit with the daemon.
            while ask.recv().await.is_some() {}
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(wait) => wait = (wait * 2).min(RETRY_MAX),
            got = ask.recv() => if got.is_none() { return },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNIT: &str =
        "/user.slice/user-1000.slice/user@1000.service/app.slice/hinteros-session.service";

    #[test]
    fn own_cgroup_path() {
        assert_eq!(own_path(&format!("0::{UNIT}\n")), Some(UNIT));
        assert_eq!(own_path("1:name=systemd:/x\n0::/a/b\n"), Some("/a/b"));
        assert_eq!(own_path("12:cpu:/x\n"), None);
        assert_eq!(own_path("0::x\n"), None);
    }

    #[test]
    fn setup_moves_into_the_leaf_and_creates_nolan() {
        let t = tempfile::tempdir().unwrap();
        let unit = t.path().join(&UNIT[1..]);
        std::fs::create_dir_all(&unit).unwrap();
        let c = Cgroups::setup(t.path(), UNIT).unwrap();
        let me = format!("{}\n", std::process::id());
        let procs = |leaf: &str| std::fs::read_to_string(unit.join(leaf).join("cgroup.procs"));
        assert_eq!(procs(LEAF).unwrap(), me);
        assert!(unit.join(NOLAN).is_dir());
        assert_eq!(c.nolan(), format!("{UNIT}/nolan"));
        assert_eq!(c.nolan_dir(), unit.join(NOLAN));
        // The daemon itself never goes there.
        assert!(procs(NOLAN).is_err());
        // Again from the leaf (a restart in place): the same cgroups.
        let again = Cgroups::setup(t.path(), &format!("{UNIT}/daemon")).unwrap();
        assert_eq!(again, c);
        // No cgroup of our own, or none to create one in.
        assert!(Cgroups::setup(t.path(), "/").is_err());
        assert!(Cgroups::setup(t.path(), "/daemon").is_err());
        assert!(Cgroups::setup(t.path(), "/not/there").is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn asks_until_the_system_says_yes_and_launches_ask_again() {
        use std::sync::Mutex;
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join(&UNIT[1..])).unwrap();
        let c = Cgroups::setup(t.path(), UNIT).unwrap();
        let (filter, rx) = LanFilter::new(Some(c));
        assert!(filter.ready().is_err(), "off at first");
        let calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let answer = Arc::new(AtomicBool::new(false));
        let task = {
            let (calls, answer) = (calls.clone(), answer.clone());
            tokio::spawn(run(filter.clone(), rx, move |cgroup| {
                let (calls, answer) = (calls.clone(), answer.clone());
                async move {
                    calls.lock().unwrap().push(cgroup);
                    if answer.load(Ordering::SeqCst) {
                        Ok(())
                    } else {
                        Err("down".to_string())
                    }
                }
            }))
        };
        // (Paused time: the retry timer fires whenever the runtime is idle.)
        tokio::task::yield_now().await;
        let n = calls.lock().unwrap().len();
        assert!(n >= 1);
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .all(|c| *c == format!("{UNIT}/nolan"))
        );
        assert!(!filter.is_on());
        // A launch while it is off fails and asks again.
        let e = filter.ready().unwrap_err();
        assert_eq!(e.to_string(), "no local network filter");
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert!(calls.lock().unwrap().len() > n);
        // Then on a timer; the system says yes.
        answer.store(true, Ordering::SeqCst);
        tokio::time::sleep(RETRY_MAX).await;
        tokio::task::yield_now().await;
        assert!(filter.is_on());
        assert!(filter.ready().is_ok());
        let n = calls.lock().unwrap().len();
        tokio::time::sleep(RETRY_MAX * 4).await;
        assert_eq!(calls.lock().unwrap().len(), n, "no more calls once on");
        // The task ends with the daemon's copies.
        drop(filter);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn without_cgroups_it_never_turns_on() {
        let (filter, rx) = LanFilter::new(None);
        assert_eq!(filter.nolan(), None);
        run(filter.clone(), rx, |_| async { Ok(()) }).await;
        assert!(!filter.is_on());
        assert!(filter.ready().is_err());
    }
}
