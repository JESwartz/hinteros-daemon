//! `hinteros-system usage` (`hinteros-usage.service`, root, read-only homes, idle I/O): disk use
//! per profile into `/var/lib/hinteros/usage.json`, a cache (`{id|guest: {total_bytes,
//! steam_games_bytes, at}}`). The walk never follows a symlink and stays on the home's
//! filesystem. App store paths are shared and not counted here.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

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

/// Bytes under `dir` (allocated blocks), not following symlinks, on `dev` only.
pub fn du(dir: &Path, dev: u64) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return 0;
    };
    if meta.dev() != dev {
        return 0;
    }
    let mut total = meta.blocks() * 512;
    if meta.is_dir()
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        for e in entries.flatten() {
            total += du(&e.path(), dev);
        }
    }
    total
}

/// Measure one home.
pub fn measure(home: &Path, now: i64) -> Option<Usage> {
    let meta = std::fs::symlink_metadata(home).ok()?;
    if !meta.is_dir() {
        return None;
    }
    let dev = meta.dev();
    Some(Usage {
        total_bytes: du(home, dev),
        steam_games_bytes: du(&home.join(".local/share/Steam/steamapps/common"), dev),
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
    }
}
