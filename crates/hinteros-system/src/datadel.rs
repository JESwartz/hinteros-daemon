//! Deleting an app's data in a profile's home (`apps.remove {delete_data: true}`), as root.
//!
//! The home belongs to the (untrusted) session user, who could plant symlinks or swap
//! directories while we work. So nothing here resolves a path: the home is opened with
//! `O_NOFOLLOW` and must be a directory owned by the profile's uid, every component of the
//! data path is opened relative to its parent with `O_NOFOLLOW | O_DIRECTORY`, and the tree is
//! removed with `unlinkat` relative to directory descriptors. A symlink anywhere is never
//! followed: as the data path's own last component it is unlinked (the link, not its target),
//! inside the tree likewise, and as an intermediate component the path is refused. Nothing on
//! another filesystem (a mount inside the home) is touched. Paths are checked with
//! [`hinteros_api::catalog::is_data_path`] (relative, no `.`/`..`, no wildcards).

use std::ffi::{CStr, CString};
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

use hinteros_api::catalog::is_data_path;
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, fstat, openat, statat, unlinkat};
use rustix::io::Errno;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Deleted {
    /// Removed, with this many files, links and directories.
    Removed(u64),
    /// Nothing there.
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    #[error("{0:?} is not a data path")]
    BadPath(String),
    #[error("home {0} is not usable: {1}")]
    BadHome(String, String),
    #[error("{0} is a symlink or not a directory; not following it")]
    NotADirectory(String),
    #[error("{0} is on another filesystem; left alone")]
    OtherFilesystem(String),
    #[error("{0}: {1}")]
    Io(String, Errno),
}

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Delete `rel` (a catalog `dataPaths` entry) under `home`, which must belong to `uid`.
pub fn delete_in_home(home: &Path, uid: u32, rel: &str) -> Result<Deleted, Refused> {
    if !is_data_path(rel) {
        return Err(Refused::BadPath(rel.to_owned()));
    }
    let shown = home.display().to_string();
    if !home.is_absolute() || home == Path::new("/") {
        return Err(Refused::BadHome(shown, "not an absolute home".into()));
    }
    let home_fd = rustix::fs::open(home, DIR_FLAGS, Mode::empty())
        .map_err(|e| Refused::BadHome(shown.clone(), e.to_string()))?;
    let st = fstat(&home_fd).map_err(|e| Refused::BadHome(shown.clone(), e.to_string()))?;
    if st.st_uid != uid {
        return Err(Refused::BadHome(shown, format!("not owned by uid {uid}")));
    }
    let dev = st.st_dev;

    let parts: Vec<&str> = rel.split('/').collect();
    let (last, parents) = parts.split_last().expect("is_data_path: not empty");
    let mut dir: OwnedFd = home_fd;
    let mut here = shown.clone();
    for part in parents {
        here = format!("{here}/{part}");
        let name = cstr(part, &here)?;
        dir = match openat(&dir, name.as_c_str(), DIR_FLAGS, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(Deleted::Missing),
            Err(Errno::LOOP | Errno::NOTDIR) => return Err(Refused::NotADirectory(here)),
            Err(e) => return Err(Refused::Io(here, e)),
        };
        let st = fstat(&dir).map_err(|e| Refused::Io(here.clone(), e))?;
        if st.st_dev != dev {
            return Err(Refused::OtherFilesystem(here));
        }
    }
    let target = format!("{here}/{last}");
    let name = cstr(last, &target)?;
    let st = match statat(&dir, name.as_c_str(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => st,
        Err(Errno::NOENT) => return Ok(Deleted::Missing),
        Err(e) => return Err(Refused::Io(target, e)),
    };
    if FileType::from_raw_mode(st.st_mode) == FileType::Directory {
        let mut count = 0;
        remove_tree(&dir, &name, dev, &target, &mut count)?;
        Ok(Deleted::Removed(count))
    } else {
        unlinkat(&dir, name.as_c_str(), AtFlags::empty()).map_err(|e| Refused::Io(target, e))?;
        Ok(Deleted::Removed(1))
    }
}

fn cstr(part: &str, shown: &str) -> Result<CString, Refused> {
    CString::new(part).map_err(|_| Refused::BadPath(shown.to_owned()))
}

/// Remove directory `name` in `parent` and everything below it, never following symlinks and
/// never entering another filesystem.
fn remove_tree(
    parent: &impl AsFd,
    name: &CStr,
    dev: u64,
    shown: &str,
    count: &mut u64,
) -> Result<(), Refused> {
    let fd = match openat(parent, name, DIR_FLAGS, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(()),
        Err(Errno::LOOP | Errno::NOTDIR) => {
            // Swapped for a link or file since we looked: remove that instead.
            unlinkat(parent, name, AtFlags::empty())
                .map_err(|e| Refused::Io(shown.to_owned(), e))?;
            *count += 1;
            return Ok(());
        }
        Err(e) => return Err(Refused::Io(shown.to_owned(), e)),
    };
    let st = fstat(&fd).map_err(|e| Refused::Io(shown.to_owned(), e))?;
    if st.st_dev != dev {
        return Err(Refused::OtherFilesystem(shown.to_owned()));
    }
    let names: Vec<CString> = {
        let dup = rustix::io::dup(&fd).map_err(|e| Refused::Io(shown.to_owned(), e))?;
        let mut entries = Dir::new(dup).map_err(|e| Refused::Io(shown.to_owned(), e))?;
        let mut out = Vec::new();
        while let Some(entry) = entries.read() {
            let entry = entry.map_err(|e| Refused::Io(shown.to_owned(), e))?;
            let n = entry.file_name();
            if n.to_bytes() != b"." && n.to_bytes() != b".." {
                out.push(n.to_owned());
            }
        }
        out
    };
    let mut first_error = None;
    for n in names {
        let child = format!("{shown}/{}", n.to_string_lossy());
        let result = match statat(&fd, n.as_c_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) if FileType::from_raw_mode(st.st_mode) == FileType::Directory => {
                remove_tree(&fd, &n, dev, &child, count)
            }
            Ok(_) => unlinkat(&fd, n.as_c_str(), AtFlags::empty())
                .map(|()| *count += 1)
                .map_err(|e| Refused::Io(child, e)),
            Err(Errno::NOENT) => Ok(()),
            Err(e) => Err(Refused::Io(child, e)),
        };
        if let Err(e) = result {
            first_error.get_or_insert(e);
        }
    }
    if let Some(e) = first_error {
        return Err(e);
    }
    unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(|e| Refused::Io(shown.to_owned(), e))?;
    *count += 1;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn me() -> u32 {
        rustix::process::getuid().as_raw()
    }

    #[test]
    fn deletes_files_trees_and_links_but_never_their_targets() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let outside = d.path().join("outside");
        fs::create_dir_all(home.join(".local/share/Steam/steamapps/common")).unwrap();
        fs::write(home.join(".local/share/Steam/steamapps/common/game"), "x").unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("precious"), "keep").unwrap();
        // Links inside the tree point out of the home: the links go, the targets stay.
        symlink(&outside, home.join(".local/share/Steam/link-dir")).unwrap();
        symlink(
            outside.join("precious"),
            home.join(".local/share/Steam/link-file"),
        )
        .unwrap();
        fs::write(home.join(".steamrc"), "x").unwrap();
        fs::write(home.join(".local/share/other"), "keep").unwrap();

        let r = delete_in_home(&home, me(), ".local/share/Steam").unwrap();
        assert_eq!(r, Deleted::Removed(6));
        assert!(!home.join(".local/share/Steam").exists());
        assert_eq!(
            fs::read_to_string(outside.join("precious")).unwrap(),
            "keep"
        );
        assert!(home.join(".local/share/other").exists());
        assert_eq!(
            delete_in_home(&home, me(), ".steamrc").unwrap(),
            Deleted::Removed(1)
        );
        assert_eq!(
            delete_in_home(&home, me(), ".steamrc").unwrap(),
            Deleted::Missing
        );
        assert_eq!(
            delete_in_home(&home, me(), "no/such/dir").unwrap(),
            Deleted::Missing
        );
    }

    #[test]
    fn a_symlinked_data_path_removes_only_the_link() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let outside = d.path().join("outside");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(outside.join("sub")).unwrap();
        fs::write(outside.join("sub/f"), "keep").unwrap();
        symlink(&outside, home.join(".steam")).unwrap();
        assert_eq!(
            delete_in_home(&home, me(), ".steam").unwrap(),
            Deleted::Removed(1)
        );
        assert!(fs::symlink_metadata(home.join(".steam")).is_err());
        assert!(outside.join("sub/f").exists());
    }

    #[test]
    fn refuses_to_follow_links_in_the_middle_or_leave_the_home() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let outside = d.path().join("outside");
        fs::create_dir_all(home.join(".config")).unwrap();
        fs::create_dir_all(outside.join("plex")).unwrap();
        fs::write(outside.join("plex/f"), "keep").unwrap();
        // .local -> outside: ".local/plex" would resolve outside the home.
        symlink(&outside, home.join(".local")).unwrap();
        let e = delete_in_home(&home, me(), ".local/plex").unwrap_err();
        assert!(matches!(e, Refused::NotADirectory(_)), "{e}");
        assert!(outside.join("plex/f").exists());
        for bad in [
            "../outside/plex",
            "/etc",
            "~/.config",
            ".config/../..",
            "*",
            "",
        ] {
            assert!(
                matches!(delete_in_home(&home, me(), bad), Err(Refused::BadPath(_))),
                "{bad:?}"
            );
        }
        // The home itself must not be a link, and must belong to the profile.
        let linked = d.path().join("linked-home");
        symlink(&home, &linked).unwrap();
        assert!(matches!(
            delete_in_home(&linked, me(), ".config"),
            Err(Refused::BadHome(..))
        ));
        assert!(matches!(
            delete_in_home(&home, me() + 1, ".config"),
            Err(Refused::BadHome(..))
        ));
        assert!(matches!(
            delete_in_home(Path::new("/"), 0, ".config"),
            Err(Refused::BadHome(..))
        ));
        assert!(home.join(".config").exists());
    }
}
