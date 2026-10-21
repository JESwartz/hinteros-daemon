//! Jellyfin's data before a version change (server.md, "Jellyfin"): a new Jellyfin migrates its
//! database on the first start and an older one cannot read it after that, so before an update
//! or rollback switches to a system with another Jellyfin version, the job stops Jellyfin and
//! copies `/var/lib/jellyfin/data` to `/var/lib/hinteros/services/jellyfin-server/backup`. One
//! copy, replaced each time, no history. Restoring it is a manual step (stop Jellyfin, copy it
//! back with its owner kept).

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Jellyfin's data directory below `/var/lib`.
pub const DATA: &str = "jellyfin/data";
/// The backup's name in the service's HinterOS directory.
pub const BACKUP: &str = "backup";
/// The unit to stop for the copy.
pub const UNIT: &str = "jellyfin.service";

/// The Jellyfin version a system runs (`12.1` from `/nix/store/<hash>-jellyfin-12.1/...` in its
/// `jellyfin.service`), `None` when it has no Jellyfin.
pub fn version(toplevel: &Path) -> Option<String> {
    let unit = fs::read_to_string(toplevel.join("etc/systemd/system").join(UNIT)).ok()?;
    let exec = unit
        .lines()
        .find_map(|l| l.trim().strip_prefix("ExecStart="))?;
    exec.split(|c: char| c.is_whitespace() || c == '\'' || c == '"')
        .flat_map(|word| word.split('/'))
        .find_map(|part| {
            let (hash, name) = part.split_once('-')?;
            let v = name.strip_prefix("jellyfin-")?;
            (hash.len() == 32
                && hash.bytes().all(|b| b.is_ascii_alphanumeric())
                && v.starts_with(|c: char| c.is_ascii_digit()))
            .then(|| v.to_owned())
        })
}

/// Whether switching from `old` to `new` changes the Jellyfin version while both run Jellyfin.
pub fn changes(old: Option<&Path>, new: &Path) -> Option<(String, String)> {
    let from = version(old?)?;
    let to = version(new)?;
    (from != to).then_some((from, to))
}

/// Replace `backup` with a copy of `data` (through `<backup>.new`; on failure the old backup
/// stays and the partial copy is removed).
pub fn replace(data: &Path, backup: &Path) -> io::Result<u64> {
    let mut new = backup.as_os_str().to_os_string();
    new.push(".new");
    let new = PathBuf::from(new);
    crate::fsutil::remove(&new)?;
    let copied = match copy_tree(data, &new) {
        Ok(n) => n,
        Err(e) => {
            let _ = crate::fsutil::remove(&new);
            return Err(e);
        }
    };
    crate::fsutil::remove(backup)?;
    fs::rename(&new, backup)?;
    Ok(copied)
}

/// Copy a tree with its modes and owners: directories, regular files and symlinks (as links;
/// nothing is followed); other file types are left out. Returns the number of entries.
pub fn copy_tree(from: &Path, to: &Path) -> io::Result<u64> {
    let meta = fs::symlink_metadata(from)?;
    if !meta.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            from.display()
        )));
    }
    fs::create_dir(to)?;
    let mut n = 1;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dest = to.join(entry.file_name());
        let ty = entry.file_type()?;
        if ty.is_dir() {
            n += copy_tree(&src, &dest)?;
        } else if ty.is_file() {
            fs::copy(&src, &dest)?;
            keep(&fs::symlink_metadata(&src)?, &dest)?;
            n += 1;
        } else if ty.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&src)?, &dest)?;
            let m = fs::symlink_metadata(&src)?;
            std::os::unix::fs::lchown(&dest, Some(m.uid()), Some(m.gid()))?;
            n += 1;
        }
    }
    keep(&meta, to)?;
    Ok(n)
}

/// Owner first (a chown clears setuid bits), then the mode.
fn keep(meta: &fs::Metadata, path: &Path) -> io::Result<()> {
    std::os::unix::fs::lchown(path, Some(meta.uid()), Some(meta.gid()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(meta.mode() & 0o7777))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system(dir: &Path, exec: Option<&str>) -> PathBuf {
        let units = dir.join("etc/systemd/system");
        fs::create_dir_all(&units).unwrap();
        if let Some(exec) = exec {
            fs::write(
                units.join(UNIT),
                format!(
                    "[Unit]\nDescription=Jellyfin\n\n[Service]\nExecStart={exec}\nUser=jellyfin\n"
                ),
            )
            .unwrap();
        }
        dir.to_owned()
    }

    #[test]
    fn version_from_the_unit() {
        let t = tempfile::tempdir().unwrap();
        let a = system(
            &t.path().join("a"),
            Some(
                "/nix/store/4vmcvpc24spxp0f0yrnx77c11kd3aylr-jellyfin-12.1/bin/jellyfin --datadir '/var/lib/jellyfin' --configdir '/var/lib/jellyfin/config'",
            ),
        );
        let b = system(
            &t.path().join("b"),
            Some(
                "'/nix/store/0000000000000000000000000000000a-jellyfin-12.2/bin/jellyfin' --datadir /var/lib/jellyfin",
            ),
        );
        let same = system(
            &t.path().join("same"),
            Some("/nix/store/1111111111111111111111111111111b-jellyfin-12.1/bin/jellyfin"),
        );
        let none = system(&t.path().join("none"), None);
        // Another package named jellyfin-something is not a version.
        let web = system(
            &t.path().join("web"),
            Some("/nix/store/2222222222222222222222222222222c-jellyfin-web-12.1/bin/x"),
        );
        assert_eq!(version(&a).as_deref(), Some("12.1"));
        assert_eq!(version(&b).as_deref(), Some("12.2"));
        assert_eq!(version(&none), None);
        assert_eq!(version(&web), None);
        assert_eq!(changes(Some(&a), &b), Some(("12.1".into(), "12.2".into())));
        assert_eq!(
            changes(Some(&a), &same),
            None,
            "a rebuild of the same version"
        );
        assert_eq!(changes(Some(&a), &none), None, "Jellyfin turned off");
        assert_eq!(changes(Some(&none), &a), None, "Jellyfin turned on");
        assert_eq!(changes(None, &a), None);
    }

    #[test]
    fn replace_keeps_modes_and_links_and_drops_the_old_copy() {
        let t = tempfile::tempdir().unwrap();
        let data = t.path().join("data");
        fs::create_dir_all(data.join("metadata/x")).unwrap();
        fs::write(data.join("jellyfin.db"), "db").unwrap();
        fs::set_permissions(data.join("jellyfin.db"), fs::Permissions::from_mode(0o640)).unwrap();
        fs::write(data.join("metadata/x/a"), "a").unwrap();
        std::os::unix::fs::symlink("../jellyfin.db", data.join("metadata/link")).unwrap();
        fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).unwrap();
        let backup = t.path().join("backup");
        fs::create_dir_all(backup.join("stale")).unwrap();

        assert_eq!(replace(&data, &backup).unwrap(), 6);
        assert!(!backup.join("stale").exists());
        assert_eq!(
            fs::read_to_string(backup.join("jellyfin.db")).unwrap(),
            "db"
        );
        assert_eq!(
            fs::read_to_string(backup.join("metadata/x/a")).unwrap(),
            "a"
        );
        let mode = |p: &Path| fs::symlink_metadata(p).unwrap().mode() & 0o7777;
        assert_eq!(mode(&backup.join("jellyfin.db")), 0o640);
        assert_eq!(mode(&backup), 0o700);
        assert_eq!(
            fs::read_link(backup.join("metadata/link")).unwrap(),
            Path::new("../jellyfin.db")
        );
        assert!(!t.path().join("backup.new").exists());

        // A failed copy keeps the old backup.
        assert!(replace(&t.path().join("missing"), &backup).is_err());
        assert!(backup.join("jellyfin.db").exists());
        assert!(!t.path().join("backup.new").exists());
    }
}
