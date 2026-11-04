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
    delete_below(home_fd, st.st_dev, shown, rel)
}

/// Delete `rel` (a catalog `stateDirs` entry: checked with
/// [`hinteros_api::catalog::is_state_dir`]) below `base` (`/var/lib`, `/var/cache`), as for app
/// data: no symlink followed anywhere (the base itself included), nothing on another filesystem.
pub fn delete_under(base: &Path, rel: &str) -> Result<Deleted, Refused> {
    if !hinteros_api::catalog::is_state_dir(rel) {
        return Err(Refused::BadPath(rel.to_owned()));
    }
    let shown = base.display().to_string();
    if !base.is_absolute() || base == Path::new("/") {
        return Err(Refused::BadHome(shown, "not an absolute directory".into()));
    }
    let fd = rustix::fs::open(base, DIR_FLAGS, Mode::empty())
        .map_err(|e| Refused::BadHome(shown.clone(), e.to_string()))?;
    let st = fstat(&fd).map_err(|e| Refused::BadHome(shown.clone(), e.to_string()))?;
    delete_below(fd, st.st_dev, shown, rel)
}

/// Delete the checked relative path `rel` below the open directory `base_fd` (on `dev`).
fn delete_below(base_fd: OwnedFd, dev: u64, shown: String, rel: &str) -> Result<Deleted, Refused> {
    let home_fd = base_fd;
    let parts: Vec<&str> = rel.split('/').collect();
    let (last, parents) = parts.split_last().expect("checked: not empty");
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

/// What [`wipe_owned`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Wiped {
    /// Files, links and directories removed.
    pub removed: u64,
    /// Entries left alone: owned by another uid, or on another filesystem.
    pub left: u64,
    /// Entries that could not be removed, and why.
    pub failed: Vec<String>,
}

/// Delete everything `uid` owns in `dir` (End guest session): every entry owned by `uid`,
/// with its whole subtree as far as `uid` owns it, without following a symlink anywhere and
/// without entering another filesystem. Entries of other uids (and what is below them) are
/// left alone, as is `dir` itself. `dir_owned`: `dir` must belong to `uid` (a home); else
/// it is a shared directory such as `/tmp`, scanned for `uid`'s top-level entries.
pub fn wipe_owned(dir: &Path, uid: u32, dir_owned: bool) -> Result<Wiped, Refused> {
    let shown = dir.display().to_string();
    if !dir.is_absolute() || dir == Path::new("/") {
        return Err(Refused::BadHome(shown, "not an absolute directory".into()));
    }
    let fd = rustix::fs::open(dir, DIR_FLAGS, Mode::empty())
        .map_err(|e| Refused::BadHome(shown.clone(), e.to_string()))?;
    let st = fstat(&fd).map_err(|e| Refused::BadHome(shown.clone(), e.to_string()))?;
    if dir_owned && st.st_uid != uid {
        return Err(Refused::BadHome(shown, format!("not owned by uid {uid}")));
    }
    let mut w = Wiped::default();
    wipe_children(&fd, st.st_dev, uid, &shown, &mut w)?;
    Ok(w)
}

/// Delete every entry of `home` (a removed account's home, which must belong to `uid`) with
/// its whole tree, by its raw name: no name is refused (unlike a data path), no symlink is
/// followed and nothing on another filesystem is touched. The home itself stays.
pub fn empty_home(home: &Path, uid: u32) -> Result<Wiped, Refused> {
    let shown = home.display().to_string();
    if !home.is_absolute() || home == Path::new("/") {
        return Err(Refused::BadHome(shown, "not an absolute home".into()));
    }
    let fd = rustix::fs::open(home, DIR_FLAGS, Mode::empty())
        .map_err(|e| Refused::BadHome(shown.clone(), e.to_string()))?;
    let st = fstat(&fd).map_err(|e| Refused::BadHome(shown.clone(), e.to_string()))?;
    if st.st_uid != uid {
        return Err(Refused::BadHome(shown, format!("not owned by uid {uid}")));
    }
    let mut w = Wiped::default();
    let dev = st.st_dev;
    for n in children(&fd, &shown)? {
        let child = format!("{shown}/{}", n.to_string_lossy());
        let result = match statat(&fd, n.as_c_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) if FileType::from_raw_mode(st.st_mode) == FileType::Directory => {
                remove_tree(&fd, &n, dev, &child, &mut w.removed)
            }
            Ok(_) => unlinkat(&fd, n.as_c_str(), AtFlags::empty())
                .map(|()| w.removed += 1)
                .map_err(|e| Refused::Io(child, e)),
            Err(Errno::NOENT) => Ok(()),
            Err(e) => Err(Refused::Io(child, e)),
        };
        if let Err(e) = result {
            w.failed.push(e.to_string());
        }
    }
    Ok(w)
}

/// The entries of the open directory `fd`, without `.` and `..`.
fn children(fd: &OwnedFd, shown: &str) -> Result<Vec<CString>, Refused> {
    let dup = rustix::io::dup(fd).map_err(|e| Refused::Io(shown.to_owned(), e))?;
    let mut entries = Dir::new(dup).map_err(|e| Refused::Io(shown.to_owned(), e))?;
    let mut out = Vec::new();
    while let Some(entry) = entries.read() {
        let entry = entry.map_err(|e| Refused::Io(shown.to_owned(), e))?;
        let n = entry.file_name();
        if n.to_bytes() != b"." && n.to_bytes() != b".." {
            out.push(n.to_owned());
        }
    }
    Ok(out)
}

/// Remove what `uid` owns below the open directory `fd` (on device `dev`).
fn wipe_children(
    fd: &OwnedFd,
    dev: u64,
    uid: u32,
    shown: &str,
    w: &mut Wiped,
) -> Result<(), Refused> {
    for n in children(fd, shown)? {
        let child = format!("{shown}/{}", n.to_string_lossy());
        let st = match statat(fd, n.as_c_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => st,
            Err(Errno::NOENT) => continue,
            Err(e) => {
                w.failed.push(format!("{child}: {e}"));
                continue;
            }
        };
        if st.st_uid != uid {
            w.left += 1;
            continue;
        }
        if FileType::from_raw_mode(st.st_mode) != FileType::Directory {
            match unlinkat(fd, n.as_c_str(), AtFlags::empty()) {
                Ok(()) => w.removed += 1,
                Err(Errno::NOENT) => {}
                Err(e) => w.failed.push(format!("{child}: {e}")),
            }
            continue;
        }
        let sub = match openat(fd, n.as_c_str(), DIR_FLAGS, Mode::empty()) {
            Ok(sub) => sub,
            Err(Errno::NOENT) => continue,
            Err(Errno::LOOP | Errno::NOTDIR) => {
                // Swapped for a link or file since we looked: that goes, never its target.
                match unlinkat(fd, n.as_c_str(), AtFlags::empty()) {
                    Ok(()) => w.removed += 1,
                    Err(e) => w.failed.push(format!("{child}: {e}")),
                }
                continue;
            }
            Err(e) => {
                w.failed.push(format!("{child}: {e}"));
                continue;
            }
        };
        let sst = fstat(&sub).map_err(|e| Refused::Io(child.clone(), e))?;
        if sst.st_dev != dev || sst.st_uid != uid {
            // A mount (or a directory swapped in by someone else): left alone.
            w.left += 1;
            continue;
        }
        wipe_children(&sub, dev, uid, &child, w)?;
        drop(sub);
        match unlinkat(fd, n.as_c_str(), AtFlags::REMOVEDIR) {
            Ok(()) => w.removed += 1,
            // Something of another uid is still inside (counted already).
            Err(Errno::NOTEMPTY | Errno::EXIST | Errno::NOENT) => {}
            Err(e) => w.failed.push(format!("{child}: {e}")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn wipes_what_the_uid_owns_and_nothing_else() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("hinteros-guest");
        let outside = d.path().join("outside");
        fs::create_dir_all(home.join(".local/share/Steam/config")).unwrap();
        fs::write(home.join(".local/share/Steam/config/loginusers.vdf"), "x").unwrap();
        fs::write(home.join(".bash_history"), "x").unwrap();
        // Names a data path would refuse are wiped too.
        fs::write(home.join("we*ird\\name"), "x").unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("precious"), "keep").unwrap();
        symlink(&outside, home.join("link-out")).unwrap();
        let w = wipe_owned(&home, me(), true).unwrap();
        assert!(w.failed.is_empty(), "{w:?}");
        assert_eq!(w.left, 0);
        assert_eq!(fs::read_dir(&home).unwrap().count(), 0, "the home is empty");
        assert!(home.is_dir(), "the home itself stays");
        assert_eq!(
            fs::read_to_string(outside.join("precious")).unwrap(),
            "keep"
        );

        // A shared directory: only the uid's top-level entries are looked at; a directory of
        // another uid (root's /, here: anything not ours) is never entered. The test can only
        // own files itself, so "another uid" is checked with uid + 1: nothing goes.
        let tmp = d.path().join("tmp");
        fs::create_dir_all(tmp.join("sub")).unwrap();
        fs::write(tmp.join("f"), "x").unwrap();
        let w = wipe_owned(&tmp, me() + 1, false).unwrap();
        assert_eq!((w.removed, w.left), (0, 2));
        assert!(tmp.join("sub").is_dir() && tmp.join("f").exists());
        // The home must belong to the uid; a link as the directory is refused.
        assert!(matches!(
            wipe_owned(&home, me() + 1, true),
            Err(Refused::BadHome(..))
        ));
        let linked = d.path().join("linked");
        symlink(&home, &linked).unwrap();
        assert!(matches!(
            wipe_owned(&linked, me(), true),
            Err(Refused::BadHome(..))
        ));
        assert!(matches!(
            wipe_owned(Path::new("/"), 0, false),
            Err(Refused::BadHome(..))
        ));
        let w = wipe_owned(&tmp, me(), false).unwrap();
        assert_eq!(w.removed, 2);
        assert_eq!(fs::read_dir(&tmp).unwrap().count(), 0);
    }

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

    #[test]
    fn service_state_dirs() {
        let d = tempfile::tempdir().unwrap();
        let var = d.path().join("var/lib");
        let outside = d.path().join("outside");
        fs::create_dir_all(var.join("jellyfin/data/x")).unwrap();
        fs::write(var.join("jellyfin/data/x/f"), "x").unwrap();
        fs::create_dir_all(var.join("hinteros/services")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep"), "x").unwrap();
        // A link where a state dir should be: the link goes, not its target.
        std::os::unix::fs::symlink(&outside, var.join("plex")).unwrap();
        // A link in the middle of the path: refused.
        std::os::unix::fs::symlink(&outside, var.join("hinteros/services/cloudflare-tunnel"))
            .unwrap();
        assert_eq!(delete_under(&var, "jellyfin"), Ok(Deleted::Removed(4)));
        assert!(!var.join("jellyfin").exists());
        assert_eq!(delete_under(&var, "plex"), Ok(Deleted::Removed(1)));
        assert!(outside.join("keep").exists());
        assert!(matches!(
            delete_under(&var, "hinteros/services/cloudflare-tunnel/token"),
            Err(Refused::NotADirectory(_))
        ));
        assert_eq!(delete_under(&var, "nothing"), Ok(Deleted::Missing));
        for bad in ["../outside", "/etc", "a/../b", ""] {
            assert!(
                matches!(delete_under(&var, bad), Err(Refused::BadPath(_))),
                "{bad}"
            );
        }
        // The base itself must not be a link.
        let linked = d.path().join("linked");
        std::os::unix::fs::symlink(&outside, &linked).unwrap();
        assert!(matches!(
            delete_under(&linked, "keep"),
            Err(Refused::BadHome(..))
        ));
        assert!(outside.join("keep").exists());
    }
}
