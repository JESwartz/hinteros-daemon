//! `hinteros-system usage` (`hinteros-usage.service`, root, read-only homes, idle I/O): disk use
//! per profile into `/var/lib/hinteros/usage.json`, a cache (`{id|guest: {total_bytes,
//! steam_games_bytes, at}}`). The walk never follows a symlink and stays on the home's
//! filesystem. App store paths are shared and not counted here.

use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::path::Path;

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, fstat, openat, statat};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::config::Paths;
use crate::fsutil;
use crate::state::{self, State};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub total_bytes: u64,
    pub steam_games_bytes: u64,
    pub at: i64,
}

pub type UsageFile = BTreeMap<String, Usage>;

pub fn load(path: &Path) -> UsageFile {
    fsutil::read_cache(path).unwrap_or_default()
}

pub fn save(path: &Path, usage: &UsageFile) -> std::io::Result<()> {
    let bytes = fsutil::cache_bytes(usage).map_err(std::io::Error::other)?;
    fsutil::write_atomic(path, &bytes, 0o600)
}

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Bytes under the open directory `fd` (allocated blocks), itself included, on `dev` only.
/// Every entry is looked at relative to its directory's descriptor, so a directory swapped
/// for a symlink during the walk is not followed.
fn du(fd: &OwnedFd, dev: u64) -> u64 {
    let Ok(st) = fstat(fd) else {
        return 0;
    };
    if st.st_dev != dev {
        return 0;
    }
    let mut total = st.st_blocks as u64 * 512;
    let Ok(dup) = rustix::io::dup(fd) else {
        return total;
    };
    let Ok(mut entries) = Dir::new(dup) else {
        return total;
    };
    while let Some(Ok(entry)) = entries.read() {
        let name = entry.file_name();
        if name == c"." || name == c".." {
            continue;
        }
        let Ok(st) = statat(fd, name, AtFlags::SYMLINK_NOFOLLOW) else {
            continue;
        };
        if FileType::from_raw_mode(st.st_mode) == FileType::Directory {
            // Swapped for a symlink since the stat: the open fails and it is not counted.
            if let Ok(child) = openat(fd, name, DIR_FLAGS, Mode::empty()) {
                total += du(&child, dev);
            }
        } else if st.st_dev == dev {
            total += st.st_blocks as u64 * 512;
        }
    }
    total
}

/// Open `rel` under the directory `fd` one component at a time, never following a symlink.
fn open_beneath(fd: &OwnedFd, rel: &str) -> Option<OwnedFd> {
    let mut cur = rustix::io::dup(fd).ok()?;
    for c in rel.split('/') {
        cur = openat(&cur, c, DIR_FLAGS, Mode::empty()).ok()?;
    }
    Some(cur)
}

/// Measure one home.
pub fn measure(home: &Path, now: i64) -> Option<Usage> {
    let fd = rustix::fs::open(home, DIR_FLAGS, Mode::empty()).ok()?;
    let dev = fstat(&fd).ok()?.st_dev;
    let steam = open_beneath(&fd, ".local/share/Steam/steamapps/common");
    Some(Usage {
        total_bytes: du(&fd, dev),
        steam_games_bytes: steam.map_or(0, |s| du(&s, dev)),
        at: now,
    })
}

pub fn main(paths: &Paths) -> std::process::ExitCode {
    let Ok((state, _)) = State::load(&paths.state_json()) else {
        warn!("no readable state.json");
        return std::process::ExitCode::FAILURE;
    };
    let now = crate::job::now();
    let mut out = UsageFile::new();
    let mut homes: Vec<(String, String)> = state
        .accounts()
        .into_iter()
        .map(|a| (a.id, a.user))
        .collect();
    if state.guest_enabled() {
        homes.push(("guest".into(), state::GUEST_USER.into()));
    }
    for (key, user) in homes {
        let Some(u) = paths.users.by_name(&user) else {
            continue;
        };
        if let Some(usage) = measure(&u.home, now) {
            out.insert(key, usage);
        }
    }
    match save(&paths.usage_json(), &out) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            warn!("cannot write usage.json: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_without_following_symlinks() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        std::fs::create_dir_all(home.join(".local/share/Steam/steamapps/common/Game")).unwrap();
        std::fs::write(
            home.join(".local/share/Steam/steamapps/common/Game/data"),
            vec![1u8; 100_000],
        )
        .unwrap();
        std::fs::write(home.join("note"), vec![1u8; 10_000]).unwrap();
        let outside = d.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("big"), vec![1u8; 1_000_000]).unwrap();
        std::os::unix::fs::symlink(&outside, home.join("link")).unwrap();
        let u = measure(&home, 5).unwrap();
        assert!(u.steam_games_bytes >= 100_000, "{u:?}");
        assert!(
            u.total_bytes >= 110_000 && u.total_bytes < 1_000_000,
            "{u:?}"
        );
        assert!(measure(&d.path().join("nope"), 5).is_none());
        assert!(measure(&home.join("link"), 5).is_none());
    }

    /// A symlink part-way along the Steam path is not followed.
    #[test]
    fn steam_path_does_not_follow_a_symlink() {
        let d = tempfile::tempdir().unwrap();
        let other = d.path().join("other");
        std::fs::create_dir_all(other.join(".local/share/Steam/steamapps/common/Game")).unwrap();
        std::fs::write(
            other.join(".local/share/Steam/steamapps/common/Game/data"),
            vec![1u8; 100_000],
        )
        .unwrap();
        let home = d.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::os::unix::fs::symlink(other.join(".local"), home.join(".local")).unwrap();
        let u = measure(&home, 5).unwrap();
        assert_eq!(u.steam_games_bytes, 0, "{u:?}");
        assert!(u.total_bytes < 100_000, "{u:?}");
    }
}
