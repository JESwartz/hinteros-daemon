//! Folders under `/srv/media` (`media.folders`) and the folder ids Jellyfin libraries take.
//!
//! A folder id is the folder's path relative to the media directory (`Movies`, `Shows/Kids`).
//! Ids come back from clients, so they are checked again every time: no empty, `.` or `..`
//! component, no hidden component, no control characters, and the folder is opened component by
//! component with `O_NOFOLLOW | O_DIRECTORY` from the media directory, so a symlink anywhere
//! (planted by whoever copies media in) is never followed out of it.

use std::ffi::CString;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use hinteros_api::system::MediaFolder;
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, openat, statat};
use rustix::io::Errno;

/// Longest folder id, in bytes.
pub const MAX_ID_BYTES: usize = 1024;
/// Most folders one answer lists.
pub const MAX_LISTED: usize = 500;
/// Entries looked at to tell whether a folder has folders of its own.
const CHILD_SCAN: usize = 2000;

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// A path component a folder id may have (and a folder listed must have).
fn is_component(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= 255
        && !c.starts_with('.')
        && !c.chars().any(|ch| ch.is_control() || ch == '/')
}

/// Whether `s` is a well-formed folder id.
pub fn is_folder_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_ID_BYTES && s.split('/').all(is_component)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Resolve {
    #[error("not a folder id")]
    BadId,
    #[error("no such folder")]
    Missing,
    #[error("a symlink or not a folder")]
    NotAFolder,
    #[error("{0}")]
    Io(Errno),
}

fn open_dir(parent: &OwnedFd, name: &str) -> Result<OwnedFd, Resolve> {
    let c = CString::new(name).map_err(|_| Resolve::BadId)?;
    match openat(parent, c.as_c_str(), DIR_FLAGS, Mode::empty()) {
        Ok(fd) => Ok(fd),
        Err(Errno::NOENT) => Err(Resolve::Missing),
        Err(Errno::LOOP | Errno::NOTDIR) => Err(Resolve::NotAFolder),
        Err(e) => Err(Resolve::Io(e)),
    }
}

/// Open the folder `id` (or the media directory itself for `None`) without following any
/// symlink.
pub fn open(media_dir: &Path, id: Option<&str>) -> Result<OwnedFd, Resolve> {
    if id.is_some_and(|i| !is_folder_id(i)) {
        return Err(Resolve::BadId);
    }
    let mut fd = match rustix::fs::open(media_dir, DIR_FLAGS, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Err(Resolve::Missing),
        Err(Errno::LOOP | Errno::NOTDIR) => return Err(Resolve::NotAFolder),
        Err(e) => return Err(Resolve::Io(e)),
    };
    for part in id.into_iter().flat_map(|i| i.split('/')) {
        fd = open_dir(&fd, part)?;
    }
    Ok(fd)
}

/// The absolute path of folder `id`, after checking that it is a real folder under the media
/// directory (what Jellyfin is given).
pub fn resolve(media_dir: &Path, id: &str) -> Result<PathBuf, Resolve> {
    open(media_dir, Some(id))?;
    Ok(media_dir.join(id))
}

/// The folder id of an absolute path inside the media directory (`None` outside it).
pub fn id_of(media_dir: &Path, path: &str) -> Option<String> {
    let rel = Path::new(path).strip_prefix(media_dir).ok()?;
    let rel = rel.to_str()?.trim_end_matches('/');
    is_folder_id(rel).then(|| rel.to_owned())
}

/// The folder names in an open directory (UTF-8, not hidden, real folders, not symlinks).
fn subfolders(fd: &OwnedFd, limit: usize) -> Result<Vec<String>, Resolve> {
    let dup = rustix::io::dup(fd).map_err(Resolve::Io)?;
    let mut dir = Dir::new(dup).map_err(Resolve::Io)?;
    let mut out = Vec::new();
    let mut seen = 0;
    while let Some(entry) = dir.read() {
        let entry = entry.map_err(Resolve::Io)?;
        seen += 1;
        if seen > limit {
            break;
        }
        let Ok(name) = entry.file_name().to_str() else {
            continue;
        };
        if !is_component(name) {
            continue;
        }
        let is_dir = match entry.file_type() {
            FileType::Directory => true,
            FileType::Unknown => statat(fd, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW)
                .is_ok_and(|st| FileType::from_raw_mode(st.st_mode) == FileType::Directory),
            _ => false,
        };
        if is_dir {
            out.push(name.to_owned());
        }
    }
    Ok(out)
}

/// One level of folders under `id` (the media directory for `None`), sorted by name, at most
/// [`MAX_LISTED`].
pub fn list(media_dir: &Path, id: Option<&str>) -> Result<Vec<MediaFolder>, Resolve> {
    let fd = open(media_dir, id)?;
    let mut names = subfolders(&fd, usize::MAX)?;
    names.sort_by_key(|n| (n.to_lowercase(), n.clone()));
    names.truncate(MAX_LISTED);
    Ok(names
        .into_iter()
        .map(|name| {
            let child = match id {
                Some(i) => format!("{i}/{name}"),
                None => name.clone(),
            };
            let has_children = open_dir(&fd, &name)
                .and_then(|c| subfolders(&c, CHILD_SCAN))
                .is_ok_and(|s| !s.is_empty());
            MediaFolder {
                id: child,
                name,
                has_children,
            }
        })
        .filter(|f| f.id.len() <= MAX_ID_BYTES)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::fs;

    #[test]
    fn folder_ids() {
        for ok in ["Movies", "Shows/Kids", "Film, 1080p", "Ünïcode/ok", "a b/c"] {
            assert!(is_folder_id(ok), "{ok}");
        }
        for bad in [
            "", "/", "/srv", "a/", "/a", "a//b", ".", "..", "a/../b", "a/./b", ".hidden", "a/.git",
            "a\nb", "a\0b", "a\u{7f}",
        ] {
            assert!(!is_folder_id(bad), "{bad:?}");
        }
        assert!(!is_folder_id(&"a".repeat(256)));
        assert!(!is_folder_id(&["a"; 600].join("/")));
    }

    #[test]
    fn lists_resolves_and_never_follows_symlinks() {
        let d = tempfile::tempdir().unwrap();
        let media = d.path().join("media");
        let outside = d.path().join("secret");
        fs::create_dir_all(media.join("Movies/Kids")).unwrap();
        fs::create_dir_all(media.join("Shows")).unwrap();
        fs::create_dir_all(media.join(".trash")).unwrap();
        fs::create_dir_all(outside.join("inner")).unwrap();
        fs::write(media.join("file.mkv"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, media.join("Escape")).unwrap();
        std::os::unix::fs::symlink(&outside, media.join("Movies/Link")).unwrap();

        let top = list(&media, None).unwrap();
        let names: Vec<_> = top
            .iter()
            .map(|f| (f.id.as_str(), f.has_children))
            .collect();
        assert_eq!(names, [("Movies", true), ("Shows", false)]);
        let kids = list(&media, Some("Movies")).unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].id, "Movies/Kids");

        assert_eq!(
            resolve(&media, "Movies/Kids").unwrap(),
            media.join("Movies/Kids")
        );
        assert_eq!(resolve(&media, "Escape"), Err(Resolve::NotAFolder));
        assert_eq!(resolve(&media, "Escape/inner"), Err(Resolve::NotAFolder));
        assert_eq!(resolve(&media, "Movies/Link"), Err(Resolve::NotAFolder));
        assert_eq!(resolve(&media, "file.mkv"), Err(Resolve::NotAFolder));
        assert_eq!(resolve(&media, "Nope"), Err(Resolve::Missing));
        assert_eq!(resolve(&media, "../secret"), Err(Resolve::BadId));
        assert_eq!(list(&media, Some("Escape")), Err(Resolve::NotAFolder));
        // The media directory itself must not be a symlink either.
        let linked = d.path().join("linked");
        std::os::unix::fs::symlink(&media, &linked).unwrap();
        assert_eq!(list(&linked, None), Err(Resolve::NotAFolder));
        assert_eq!(list(&d.path().join("none"), None), Err(Resolve::Missing));

        assert_eq!(
            id_of(&media, media.join("Movies/Kids").to_str().unwrap()).as_deref(),
            Some("Movies/Kids")
        );
        assert_eq!(id_of(&media, "/elsewhere/Movies"), None);
        assert_eq!(id_of(&media, media.to_str().unwrap()), None);
    }

    proptest! {
        /// Whatever a client sends, an accepted id never climbs out or names a hidden file.
        #[test]
        fn accepted_ids_stay_inside(s in "[a-z./\\\\ \n]{0,24}") {
            if is_folder_id(&s) {
                prop_assert!(!s.starts_with('/'));
                prop_assert!(s.split('/').all(|c| c != ".." && c != "." && !c.is_empty() && !c.starts_with('.')));
                prop_assert!(!s.contains('\n'));
            }
        }
    }
}
