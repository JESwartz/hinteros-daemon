//! Noticing Steam library changes: inotify on the directories that hold the library, a
//! debounce, and a periodic rescan as a fallback (it also notices Steam being installed, a new
//! library, and artwork that lands in subdirectories we don't watch). Only a change in the
//! [`library::scan`] result counts.

use std::collections::BTreeSet;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use hinteros_api::session::Game;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::inotify;
use rustix::io::Errno;
use tracing::{debug, info, warn};

use super::library;

/// The directories to watch for the library of `steam`.
pub fn watched_dirs(steam: &Path) -> BTreeSet<PathBuf> {
    let mut dirs: BTreeSet<PathBuf> = [
        steam.to_path_buf(),
        steam.join("steamapps"),
        steam.join("config"),
        steam.join("appcache/librarycache"),
    ]
    .into_iter()
    .collect();
    dirs.extend(
        library::libraries(steam)
            .into_iter()
            .map(|l| l.join("steamapps")),
    );
    // Non-Steam games: each Steam user's shortcuts.vdf (and grid artwork next to it).
    dirs.extend(library::user_config_dirs(steam));
    dirs.retain(|d| d.is_dir());
    dirs
}

/// Watch the library of the Steam directory `configured` (or the one found under `$HOME`) on a
/// background thread. Calls `changed` after the scan result changed (not for the first scan);
/// stops when it returns `false`.
pub fn spawn(
    configured: Option<PathBuf>,
    debounce: Duration,
    poll_interval: Duration,
    mut changed: impl FnMut(&[Game]) -> bool + Send + 'static,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("steam-library".into())
        .spawn(move || {
            let scan = || {
                library::default_steam_dir(configured.as_deref())
                    .map(|dir| (library::scan(&dir), watched_dirs(&dir)))
                    .unwrap_or_default()
            };
            let (mut last, mut dirs) = scan();
            info!(games = last.len(), "Steam library");
            let mut fd = watch(&dirs);
            let mut due: Option<Instant> = None;
            loop {
                let now = Instant::now();
                let timeout = match due {
                    Some(at) => Some(at.saturating_duration_since(now)),
                    None if poll_interval.is_zero() => None,
                    None => Some(poll_interval),
                };
                let ready = match &fd {
                    Some(fd) => wait(fd, timeout),
                    None => {
                        thread::sleep(timeout.unwrap_or(Duration::from_secs(3600)));
                        false
                    }
                };
                if ready {
                    if let Some(fd) = &fd {
                        drain(fd);
                    }
                    // The first event starts the debounce; later ones don't extend it, so a
                    // download that rewrites its manifest every second still gets rescanned.
                    if due.is_none() {
                        due = Some(Instant::now() + debounce);
                    }
                    continue;
                }
                if due.is_some_and(|at| Instant::now() < at) {
                    continue;
                }
                due = None;
                let (games, new_dirs) = scan();
                if new_dirs != dirs {
                    debug!(?new_dirs, "watching other Steam directories");
                    dirs = new_dirs;
                    fd = watch(&dirs);
                }
                if games != last {
                    info!(games = games.len(), "Steam library changed");
                    last = games;
                    if !changed(&last) {
                        return;
                    }
                }
            }
        })
        .expect("spawning the Steam library watcher thread")
}

/// An inotify instance watching `dirs`; `None` if there is nothing to watch (or no inotify).
pub(crate) fn watch(dirs: &BTreeSet<PathBuf>) -> Option<OwnedFd> {
    if dirs.is_empty() {
        return None;
    }
    let fd = match inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK) {
        Ok(fd) => fd,
        Err(e) => {
            warn!("inotify unavailable, rescanning periodically instead: {e}");
            return None;
        }
    };
    let mask = inotify::WatchFlags::CREATE
        | inotify::WatchFlags::DELETE
        | inotify::WatchFlags::MOVED_TO
        | inotify::WatchFlags::MOVED_FROM
        | inotify::WatchFlags::CLOSE_WRITE
        | inotify::WatchFlags::DELETE_SELF
        | inotify::WatchFlags::MOVE_SELF
        | inotify::WatchFlags::ONLYDIR;
    for dir in dirs {
        if let Err(e) = inotify::add_watch(&fd, dir, mask) {
            debug!(dir = %dir.display(), "cannot watch: {e}");
        }
    }
    Some(fd)
}

/// Wait until `fd` is readable or `timeout` passes. `true` if readable.
fn wait(fd: &OwnedFd, timeout: Option<Duration>) -> bool {
    let ts = timeout.map(|d| Timespec {
        tv_sec: d.as_secs().try_into().unwrap_or(i64::MAX),
        tv_nsec: d.subsec_nanos().into(),
    });
    let mut fds = [PollFd::new(fd, PollFlags::IN)];
    match poll(&mut fds, ts.as_ref()) {
        Ok(n) => n > 0,
        Err(Errno::INTR) => false,
        Err(e) => {
            warn!("polling inotify: {e}");
            thread::sleep(Duration::from_secs(1));
            false
        }
    }
}

pub(crate) fn drain(fd: &OwnedFd) {
    let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 4096];
    let mut reader = inotify::Reader::new(fd, &mut buf);
    while reader.next().is_ok() {}
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc;

    use super::*;

    fn manifest(appid: u32, name: &str) -> String {
        format!(
            "\"AppState\"\n{{\n\t\"appid\"\t\t\"{appid}\"\n\t\"name\"\t\t\"{name}\"\n\t\"StateFlags\"\t\t\"4\"\n}}\n"
        )
    }

    #[test]
    fn reports_installs_and_removals_once() {
        let dir = tempfile::tempdir().unwrap();
        let steam = dir.path().join("Steam");
        fs::create_dir_all(steam.join("steamapps")).unwrap();
        fs::write(
            steam.join("steamapps/appmanifest_10.acf"),
            manifest(10, "A"),
        )
        .unwrap();

        let (tx, rx) = mpsc::channel::<Vec<u32>>();
        spawn(
            Some(steam.clone()),
            Duration::from_millis(50),
            Duration::from_secs(3600), // inotify only
            move |games| tx.send(games.iter().map(|g| g.appid).collect()).is_ok(),
        );
        thread::sleep(Duration::from_millis(200));
        let next = || {
            rx.recv_timeout(Duration::from_secs(10))
                .expect("no change reported")
        };

        // Steam writes a temp file and renames it into place.
        let tmp = steam.join("steamapps/appmanifest_20.acf.123.tmp");
        fs::write(&tmp, manifest(20, "B")).unwrap();
        fs::rename(&tmp, steam.join("steamapps/appmanifest_20.acf")).unwrap();
        assert_eq!(next(), [10, 20]);

        // Rewriting a manifest without a visible change is not reported.
        fs::write(
            steam.join("steamapps/appmanifest_20.acf"),
            manifest(20, "B"),
        )
        .unwrap();
        fs::remove_file(steam.join("steamapps/appmanifest_10.acf")).unwrap();
        assert_eq!(next(), [20]);
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    }

    #[test]
    fn periodic_rescan_finds_a_late_steam_install() {
        let dir = tempfile::tempdir().unwrap();
        let steam = dir.path().join("Steam");
        let (tx, rx) = mpsc::channel::<usize>();
        spawn(
            Some(steam.clone()),
            Duration::from_millis(10),
            Duration::from_millis(100),
            move |games| tx.send(games.len()).is_ok(),
        );
        thread::sleep(Duration::from_millis(300));
        fs::create_dir_all(steam.join("steamapps")).unwrap();
        fs::write(
            steam.join("steamapps/appmanifest_10.acf"),
            manifest(10, "A"),
        )
        .unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), 1);
    }
}
