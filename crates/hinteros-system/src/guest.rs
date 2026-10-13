//! End guest session (profiles.md "Guest", apps.md owner answers): the guest account, checked
//! before anything of it is touched, the "guest is being wiped" marker, and the guest's terms
//! acceptances.
//!
//! The wipe itself is the `guest_end` job (`runner/profiles.rs`). Everything here refuses rather
//! than guesses: only an account named `hinteros-guest`, with exactly the configured uid, whose
//! uid maps back to that name, with a home `/…/hinteros-guest`, is ever the guest.

use std::path::{Component, Path, PathBuf};

use crate::config::Paths;
use crate::fsutil;
use crate::state::GUEST_USER;
use crate::users::{User, Users};

/// The guest account, if the user database has exactly the account we expect.
pub fn account(users: &Users, expected_uid: u32) -> Result<User, String> {
    let u = users
        .by_name(GUEST_USER)
        .ok_or_else(|| format!("there is no {GUEST_USER} account"))?;
    if u.uid != expected_uid {
        return Err(format!(
            "{GUEST_USER} has uid {}, expected {expected_uid}",
            u.uid
        ));
    }
    if u.uid < 1000 {
        return Err(format!("{GUEST_USER} has a system uid ({})", u.uid));
    }
    match users.by_uid(u.uid) {
        Some(back) if back.name == GUEST_USER => {}
        Some(back) => {
            return Err(format!(
                "uid {} is also {}; refusing to touch it",
                u.uid, back.name
            ));
        }
        None => return Err(format!("uid {} has no account", u.uid)),
    }
    check_home(&u.home)?;
    Ok(u)
}

/// A guest home: absolute, not `/`, no `.`/`..`, named `hinteros-guest`.
fn check_home(home: &Path) -> Result<(), String> {
    let plain = home.is_absolute()
        && home
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)));
    if !plain || home.file_name().and_then(|n| n.to_str()) != Some(GUEST_USER) {
        return Err(format!(
            "{GUEST_USER}'s home {} is not a guest home",
            home.display()
        ));
    }
    Ok(())
}

fn marker(paths: &Paths) -> PathBuf {
    paths.run_dir.join("guest-ending")
}

/// The guest is being wiped right now (a `guest_end` job is running): the picker hides it and
/// PAM refuses it. A marker left by a job that has finished (or died: the service records a
/// lost job's status) counts for nothing.
pub fn ending(paths: &Paths) -> bool {
    let Ok(id) = std::fs::read_to_string(marker(paths)) else {
        return false;
    };
    let id = id.trim();
    let Some(dir) = crate::job::dir(paths, id) else {
        return false;
    };
    dir.is_dir() && !dir.join(crate::job::STATUS).exists()
}

pub fn mark(paths: &Paths, job_id: &str) -> std::io::Result<()> {
    fsutil::write_atomic(&marker(paths), job_id.as_bytes(), 0o644)
}

pub fn unmark(paths: &Paths) {
    let _ = std::fs::remove_file(marker(paths));
}

/// Remove the terms acceptances the guest made (`by: hinteros-guest`, no profile), so the next
/// guest is asked again. Returns how many went. The audit journal keeps the history.
pub fn drop_terms(path: &Path) -> std::io::Result<usize> {
    let mut doc: serde_json::Map<String, serde_json::Value> = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{}: {e}", path.display()),
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut dropped = 0;
    for list in doc.values_mut() {
        if let serde_json::Value::Array(entries) = list {
            let before = entries.len();
            entries.retain(|r| {
                !(r.get("by").and_then(|b| b.as_str()) == Some(GUEST_USER)
                    && r.get("profile").is_none_or(serde_json::Value::is_null))
            });
            dropped += before - entries.len();
        }
    }
    doc.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
    if dropped > 0 {
        let mut bytes = serde_json::to_vec_pretty(&doc)?;
        bytes.push(b'\n');
        fsutil::write_atomic(path, &bytes, 0o600)?;
    }
    Ok(dropped)
}

/// Once per boot (a marker in the run dir): the guest's terms acceptances from before the boot
/// go, as its home did (tmpfiles `D!`).
pub fn boot_cleanup(paths: &Paths) {
    let done = paths.run_dir.join("guest-boot-clean");
    if done.exists() {
        return;
    }
    match drop_terms(&paths.terms_accepted()) {
        Ok(0) => {}
        Ok(n) => tracing::info!(
            dropped = n,
            "the last boot's guest terms acceptances removed"
        ),
        Err(e) => tracing::warn!("cannot clear the guest's terms acceptances: {e}"),
    }
    let _ = std::fs::write(done, b"");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn users(text: &str) -> (tempfile::TempDir, Users) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("passwd");
        std::fs::write(&p, text).unwrap();
        (
            d,
            Users {
                passwd: Some(p),
                group: None,
            },
        )
    }

    #[test]
    fn only_the_expected_guest_account() {
        let good = "hinteros:x:1000:100::/home/hinteros:/bin/sh\n\
                    hinteros-guest:x:20000:100::/home/hinteros-guest:/bin/sh\n";
        let (_d, u) = users(good);
        assert_eq!(account(&u, 20000).unwrap().uid, 20000);
        // Another uid than the box's guest uid.
        assert!(account(&u, 20001).unwrap_err().contains("expected 20001"));
        // No such account (a renamed one is not the guest).
        let (_d, u) = users("hinteros-guest2:x:20000:100::/home/hinteros-guest:/bin/sh\n");
        assert!(account(&u, 20000).is_err());
        // The uid belongs to someone else first.
        let (_d, u) = users(
            "hinteros-p2:x:20000:100::/home/hinteros-p2:/bin/sh\n\
             hinteros-guest:x:20000:100::/home/hinteros-guest:/bin/sh\n",
        );
        assert!(account(&u, 20000).unwrap_err().contains("hinteros-p2"));
        // root, a system uid, odd homes.
        let (_d, u) = users("hinteros-guest:x:0:0::/home/hinteros-guest:/bin/sh\n");
        assert!(account(&u, 0).is_err());
        for home in [
            "/",
            "/home/hinteros",
            "/home/../root",
            "home/hinteros-guest",
            "",
        ] {
            let (_d, u) = users(&format!("hinteros-guest:x:20000:100::{home}:/bin/sh\n"));
            assert!(account(&u, 20000).is_err(), "{home:?}");
        }
    }

    #[test]
    fn guest_terms_go_others_stay() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("terms.json");
        std::fs::write(
            &p,
            r#"{"steam": [{"version": "1", "by": "hinteros-guest"}, {"version": "1", "by": "hinteros", "profile": "p1"}],
                "plex": [{"version": "2", "by": "hinteros-guest"}]}"#,
        )
        .unwrap();
        assert_eq!(drop_terms(&p).unwrap(), 2);
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"steam": [{"version": "1", "by": "hinteros", "profile": "p1"}]})
        );
        assert_eq!(drop_terms(&p).unwrap(), 0);
        assert_eq!(drop_terms(&d.path().join("none")).unwrap(), 0);
    }
}
