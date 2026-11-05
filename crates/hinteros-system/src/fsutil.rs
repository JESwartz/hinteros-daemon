//! File helpers for the apply flow: copy a config dir, write files atomically, and replace a
//! directory in one step (`renameat2(RENAME_EXCHANGE)`), so readers see the old or the new
//! `/etc/hinteros`, never a mix.
//!
//! The root helpers (jobs, the PAM helper, `reset-pin`, the time zone helper) write into the
//! directories of the unprivileged service (`/var/lib/hinteros`, `/run/hinteros`; README,
//! "Privileges"). There, as root, these helpers follow no symlink anywhere in a path (the
//! service could put one there and make root write elsewhere) and give what they create to the
//! directory's owner, so the service can read and replace it. Without root nothing changes.

use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::{
    AtFlags, CWD, FileType, Mode, OFlags, RenameFlags, fchown, fstat, mkdirat, openat, renameat,
    renameat_with, unlinkat,
};
use rustix::process::{Gid, Uid, geteuid};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Whether paths are opened the careful way (see the module docs): as root.
fn guarded() -> bool {
    geteuid().is_root()
}

/// Open directory `dir` for the `*at` calls; `guard`: no symlink in any component.
fn open_dir(dir: &Path, guard: bool) -> io::Result<OwnedFd> {
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let flags = OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC;
    let fd = if guard {
        walk(dir, flags)
    } else {
        openat(CWD, dir, flags, Mode::empty())
    };
    fd.map_err(|e| io::Error::new(io::Error::from(e).kind(), format!("{}: {e}", dir.display())))
}

/// `dir` opened one component at a time with `O_NOFOLLOW`: a symlink (or `..`) anywhere fails.
/// Not `openat2(RESOLVE_NO_SYMLINKS)`: the helper units' seccomp filters refuse it.
fn walk(dir: &Path, flags: OFlags) -> rustix::io::Result<OwnedFd> {
    use std::path::Component;
    let mut fd = openat(
        CWD,
        if dir.is_absolute() { "/" } else { "." },
        flags,
        Mode::empty(),
    )?;
    for c in dir.components() {
        match c {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                fd = openat(&fd, name, flags | OFlags::NOFOLLOW, Mode::empty())?;
            }
            Component::ParentDir | Component::Prefix(_) => return Err(rustix::io::Errno::LOOP),
        }
    }
    Ok(fd)
}

/// Directory `path` for the `*at` calls (as root: no symlink in the path), so that later calls
/// stay in the directory that was opened even if the path changes.
pub fn dir(path: &Path) -> io::Result<OwnedFd> {
    open_dir(path, guarded())
}

fn split(path: &Path) -> io::Result<(&Path, &std::ffi::OsStr)> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    Ok((path.parent().unwrap_or(Path::new(".")), name))
}

/// Root in a directory that is not root's: give `fd` (just made) to the directory's owner.
fn hand_over(dir: &impl AsFd, fd: &impl AsFd, guard: bool) -> io::Result<()> {
    if !guard {
        return Ok(());
    }
    let d = fstat(dir)?;
    if d.st_uid == 0 {
        return Ok(());
    }
    let f = fstat(fd)?;
    if f.st_uid == d.st_uid && f.st_gid == d.st_gid {
        return Ok(());
    }
    // SAFETY of the ids: they come from the kernel's stat of the directory.
    fchown(
        fd,
        Some(Uid::from_raw(d.st_uid)),
        Some(Gid::from_raw(d.st_gid)),
    )?;
    Ok(())
}

/// Open `path` with `flags` (`CREATE` makes it with `mode`). As root: no symlink in the path,
/// a regular file only (a FIFO put there must not hang the helper: opened `O_NONBLOCK`, then
/// checked), and a file of root's in a directory that is not is given to the directory's owner.
pub fn open(path: &Path, flags: OFlags, mode: u32) -> io::Result<fs::File> {
    open_with(path, flags, mode, guarded())
}

fn open_with(path: &Path, flags: OFlags, mode: u32, guard: bool) -> io::Result<fs::File> {
    let flags = flags | OFlags::CLOEXEC;
    if !guard {
        let fd = openat(CWD, path, flags, Mode::from_raw_mode(mode))?;
        return Ok(fs::File::from(fd));
    }
    let (dir, name) = split(path)?;
    let d = open_dir(dir, true)?;
    let fd = openat(
        &d,
        name,
        flags | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::from_raw_mode(mode),
    )?;
    if FileType::from_raw_mode(fstat(&fd)?.st_mode) != FileType::RegularFile {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{}: not a regular file", path.display()),
        ));
    }
    if flags.contains(OFlags::CREATE) {
        hand_over(&d, &fd, true)?;
    }
    Ok(fs::File::from(fd))
}

/// The whole file (as root: no symlink in the path, a regular file only).
pub fn read(path: &Path) -> io::Result<Vec<u8>> {
    read_limited(path, u64::MAX)
}

/// At most `max` bytes of the file (as root: as [`read`]).
pub fn read_limited(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let f = open(path, OFlags::RDONLY, 0)?;
    let mut out = Vec::new();
    f.take(max).read_to_end(&mut out)?;
    Ok(out)
}

/// The whole file as text (as [`read`]).
pub fn read_to_string(path: &Path) -> io::Result<String> {
    String::from_utf8(read(path)?).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// `create_dir_all` with `mode` for what it makes. As root: no symlink in the path, and each
/// directory made in a directory that is not root's is given to that directory's owner.
pub fn create_dir_all(path: &Path, mode: u32) -> io::Result<()> {
    create_dir_all_with(path, mode, guarded())
}

fn create_dir_all_with(path: &Path, mode: u32, guard: bool) -> io::Result<()> {
    if !guard {
        return fs::DirBuilder::new()
            .recursive(true)
            .mode(mode)
            .create(path);
    }
    let mut missing = Vec::new();
    let mut top = path;
    while fs::symlink_metadata(top).is_err() {
        let (parent, name) = split(top)?;
        missing.push(name);
        top = parent;
    }
    let mut d = open_dir(top, true)?;
    for name in missing.into_iter().rev() {
        match mkdirat(&d, name, Mode::from_raw_mode(mode)) {
            Err(rustix::io::Errno::EXIST) | Ok(()) => {}
            Err(e) => return Err(e.into()),
        }
        let sub = openat(
            &d,
            name,
            OFlags::DIRECTORY | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        hand_over(&d, &sub, true)?;
        d = sub;
    }
    Ok(())
}

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

/// Write `bytes` to `path` through a temporary file in the same directory and a rename (as
/// root: see the module docs).
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    write_atomic_with(path, bytes, mode, guarded())
}

fn write_atomic_with(path: &Path, bytes: &[u8], mode: u32, guard: bool) -> io::Result<()> {
    let (dir, name) = split(path)?;
    let d = open_dir(dir, guard)?;
    let tmp = format!(".{}.tmp-{}", name.to_string_lossy(), std::process::id());
    match unlinkat(&d, tmp.as_str(), AtFlags::empty()) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => {}
        Err(e) => return Err(e.into()),
    }
    let fd = openat(
        &d,
        tmp.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(mode),
    )?;
    let mut f = fs::File::from(fd);
    let written = f
        .write_all(bytes)
        .and_then(|()| f.set_permissions(fs::Permissions::from_mode(mode)))
        .and_then(|()| hand_over(&d, &f, guard))
        .and_then(|()| f.sync_all())
        .and_then(|()| Ok(renameat(&d, tmp.as_str(), &d, name)?));
    if let Err(e) = written {
        let _ = unlinkat(&d, tmp.as_str(), AtFlags::empty());
        return Err(e);
    }
    // The rename itself must last too.
    Ok(rustix::fs::fsync(&d)?)
}

/// An exclusive `flock` on `<path>.lock` (0600, next to `path`) until the file is dropped: the
/// processes that read-modify-write one file (the service, jobs, the PAM helper) take turns.
pub fn lock_beside(path: &Path) -> io::Result<fs::File> {
    let name = format!(
        "{}.lock",
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    let lock = path.with_file_name(name);
    if let Some(dir) = lock.parent() {
        create_dir_all(dir, 0o777)?;
    }
    lock_file(&lock)
}

/// An exclusive `flock` on `path` itself (made 0600 if missing) until the file is dropped.
pub fn lock_file(path: &Path) -> io::Result<fs::File> {
    let f = open(path, OFlags::RDWR | OFlags::CREATE, 0o600)?;
    rustix::fs::flock(&f, rustix::fs::FlockOperation::LockExclusive)?;
    Ok(f)
}

/// Replace directory `target` with a copy of `source`: copy next to `target` (same filesystem),
/// fsync every file and directory of the copy, exchange the two in one rename, then delete the
/// old contents.
pub fn replace_dir(source: &Path, target: &Path, tag: &str) -> io::Result<()> {
    let parent = target.parent().unwrap_or(Path::new("/"));
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let new: PathBuf = parent.join(format!(".{name}.new-{tag}"));
    remove(&new)?;
    copy_dir(source, &new)?;
    sync_tree(&new)?;
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

/// fsync the files of `dir` (as [`copy_dir`] made them), its directories, then `dir`.
fn sync_tree(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            sync_tree(&entry.path())?;
        } else if ty.is_file() {
            fs::File::open(entry.path())?.sync_all()?;
        }
    }
    sync_dir(dir)
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
    parse_cache(&read(path).ok()?)
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

    /// As root (here: the careful mode as any user): a symlink anywhere in the path is refused,
    /// one where the temporary file goes is replaced, never followed.
    #[test]
    fn the_careful_mode_follows_no_symlink() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real");
        fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, d.path().join("link")).unwrap();
        let outside = d.path().join("outside");
        fs::write(&outside, "keep").unwrap();

        assert!(write_atomic_with(&d.path().join("link/a"), b"1", 0o600, true).is_err());
        assert!(!real.join("a").exists());
        let tmp = real.join(format!(".a.tmp-{}", std::process::id()));
        std::os::unix::fs::symlink(&outside, &tmp).unwrap();
        write_atomic_with(&real.join("a"), b"1", 0o600, true).unwrap();
        assert_eq!(fs::read(real.join("a")).unwrap(), b"1");
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");

        std::os::unix::fs::symlink(&outside, real.join("s")).unwrap();
        let flags = OFlags::RDWR | OFlags::CREATE;
        assert!(open_with(&real.join("s"), flags, 0o600, true).is_err());
        assert!(open_with(&d.path().join("link/b"), flags, 0o600, true).is_err());
        open_with(&real.join("b"), flags, 0o600, true).unwrap();
        assert!(
            open_with(&real.join("s"), flags, 0o600, false).is_ok(),
            "else as before"
        );

        create_dir_all_with(&real.join("x/y"), 0o700, true).unwrap();
        assert!(real.join("x/y").is_dir());
        assert!(create_dir_all_with(&d.path().join("link/z"), 0o700, true).is_err());
        assert!(
            write_atomic_with(&real.join("x/../a"), b"1", 0o600, true).is_err(),
            "no .."
        );
        assert!(!real.join("z").exists());
    }

    /// As root: only a regular file is opened; a FIFO in its place fails at once (no hang).
    #[test]
    fn the_careful_mode_opens_regular_files_only() {
        let d = tempfile::tempdir().unwrap();
        let fifo = d.path().join("spec.json");
        rustix::fs::mknodat(CWD, &fifo, FileType::Fifo, Mode::from_raw_mode(0o600), 0).unwrap();
        assert!(open_with(&fifo, OFlags::RDONLY, 0, true).is_err());
        assert!(open_with(&fifo, OFlags::RDWR | OFlags::CREATE, 0o600, true).is_err());
        assert!(open_with(d.path(), OFlags::RDONLY, 0, true).is_err());
        fs::write(d.path().join("f"), "abc").unwrap();
        let f = open_with(&d.path().join("f"), OFlags::RDONLY, 0, true).unwrap();
        let mut out = String::new();
        f.take(2).read_to_string(&mut out).unwrap();
        assert_eq!(out, "ab");
    }
}
