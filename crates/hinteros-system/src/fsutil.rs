//! File helpers for the apply flow: copy a config dir, write files atomically, and replace a
//! directory in one step (`renameat2(RENAME_EXCHANGE)`), so readers see the old or the new
//! `/etc/hinteros`, never a mix.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::{CWD, RenameFlags, renameat_with};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Copy the regular files and directories of `from` into a fresh `to` (symlinks and other file
/// types are skipped: a config dir has none).
pub fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o755).create(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest = to.join(entry.file_name());
        if ty.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else if ty.is_file() {
            fs::copy(entry.path(), &dest)?;
            fs::set_permissions(&dest, fs::Permissions::from_mode(0o644))?;
        }
    }
    Ok(())
}

/// Remove `path` if it exists (a directory recursively).
pub fn remove(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Write `bytes` to `path` through a temporary file in the same directory and a rename.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, path)
}

/// Replace directory `target` with a copy of `source`: copy next to `target` (same filesystem),
/// fsync, exchange the two in one rename, then delete the old contents.
pub fn replace_dir(source: &Path, target: &Path, tag: &str) -> io::Result<()> {
    let parent = target.parent().unwrap_or(Path::new("/"));
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let new: PathBuf = parent.join(format!(".{name}.new-{tag}"));
    remove(&new)?;
    copy_dir(source, &new)?;
    sync_dir(&new)?;
    if target.exists() {
        renameat_with(CWD, &new, CWD, target, RenameFlags::EXCHANGE)?;
        // `new` now holds the old contents.
        remove(&new)?;
    } else {
        fs::rename(&new, target)?;
    }
    sync_dir(parent)
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// Format of the files that only hold what can be had again: `{"version": 1, "data": ...}`.
pub const CACHE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Cache<T> {
    version: u32,
    data: T,
}

/// The data of a cache file. Damaged, newer or missing: `None`, and the next write replaces it.
pub fn read_cache<T: DeserializeOwned>(path: &Path) -> Option<T> {
    parse_cache(&std::fs::read(path).ok()?)
}

pub fn parse_cache<T: DeserializeOwned>(bytes: &[u8]) -> Option<T> {
    let c: Cache<T> = serde_json::from_slice(bytes).ok()?;
    (c.version == CACHE_VERSION).then_some(c.data)
}

pub fn cache_bytes<T: Serialize>(data: &T) -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec(&Cache {
        version: CACHE_VERSION,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cache_of_another_version_is_gone() {
        let bytes = cache_bytes(&vec![1, 2]).unwrap();
        assert_eq!(parse_cache::<Vec<u32>>(&bytes), Some(vec![1, 2]));
        assert_eq!(
            parse_cache::<Vec<u32>>(br#"{"version":2,"data":[1]}"#),
            None
        );
        assert_eq!(parse_cache::<Vec<u32>>(b"not json"), None);
        let later = br#"{"version":1,"data":[1],"later":true}"#;
        assert_eq!(parse_cache::<Vec<u32>>(later), Some(vec![1]));
    }

    #[test]
    fn replaces_a_directory() {
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("hinteros");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("state.json"), "old").unwrap();
        fs::write(target.join("gone"), "x").unwrap();
        let source = d.path().join("staging");
        fs::create_dir_all(source.join("sub")).unwrap();
        fs::write(source.join("state.json"), "new").unwrap();
        fs::write(source.join("sub/f"), "f").unwrap();
        replace_dir(&source, &target, "t").unwrap();
        assert_eq!(
            fs::read_to_string(target.join("state.json")).unwrap(),
            "new"
        );
        assert_eq!(fs::read_to_string(target.join("sub/f")).unwrap(), "f");
        assert!(!target.join("gone").exists());
        let left: Vec<_> = fs::read_dir(d.path()).unwrap().collect();
        assert_eq!(left.len(), 2, "no leftovers next to the target");

        write_atomic(&target.join("a"), b"1", 0o600).unwrap();
        assert_eq!(fs::read(target.join("a")).unwrap(), b"1");
        assert_eq!(
            fs::metadata(target.join("a")).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
