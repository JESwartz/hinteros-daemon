//! Profile jobs (profiles.md "Creating a profile", "Removing a profile").

use std::fs;
use std::path::Path;

use hinteros_api::system::Stage;
use tracing::{info, warn};

use super::{Outcome, Runner};
use crate::appreg::{self, Owner, Registry};
use crate::job::{self, Apply, GuestEndJob, ProfileCreateJob, ProfileRemoveJob};
use crate::pins::Pins;
use crate::profiles::ProfilesFile;
use crate::state::{GUEST_USER, State};
use crate::{datadel, guest, pam, public, usage};

impl Runner {
    pub(super) fn profile_create(&mut self, j: &ProfileCreateJob) -> Outcome {
        // The account (a rebuild that substitutes nothing).
        let outcome = self.apply(&Apply {
            state: Some(j.state.clone()),
            ..Default::default()
        })?;
        self.set_stage(Stage::Finishing);
        let paths = self.spec.paths.clone();
        // Metadata and PIN only after the account exists: a failed job leaves neither. The PIN
        // first: a profile without its PIN would let anyone in.
        ProfilesFile::load(&paths.profiles_json())
            .map_err(|e| self.fail(format!("profiles.json: {e}")))?;
        let pins = Pins::new(paths.pins_dir());
        if let Err(e) = pins.set_hash(&j.id, &j.pin_hash) {
            pins.remove(&j.id);
            return Err(self.fail(format!("writing the PIN: {e}")));
        }
        let saved = ProfilesFile::update(&paths.profiles_json(), |f| {
            f.profiles.insert(j.id.clone(), j.meta.clone());
            Some(())
        });
        let file = match saved {
            Ok(Some((file, ()))) => file,
            Ok(None) => unreachable!("the change always writes"),
            Err(e) => {
                pins.remove(&j.id);
                return Err(self.fail(format!("writing profiles.json: {e}")));
            }
        };
        // Its apps: the starter env (nothing to evaluate).
        let owner = Owner::Profile {
            id: j.id.clone(),
            user: j.user.clone(),
        };
        if let Some(starter) = appreg::current(&Owner::Starter, &paths) {
            self.set_env(&owner, &starter.env)?;
        } else {
            info!("no starter env; the new profile starts without apps");
        }
        Registry::reconcile(&paths, &j.state);
        appreg::publish(&paths, &j.state);
        if j.token
            && let Err(e) = public::grant_token(&paths, &j.id, &j.user, job::now())
        {
            warn!("no token for the new profile: {e}");
        }
        public::publish(&paths, &j.state, &file, job::now());
        // Its Jellyfin user (best effort; reveal and Jellyfin jobs sync again).
        self.jellyfin_sync_users();
        info!(profile = %j.id, user = %j.user, "profile created");
        Ok(outcome)
    }

    pub(super) fn profile_remove(&mut self, j: &ProfileRemoveJob) -> Outcome {
        self.set_stage(Stage::Preparing);
        let paths = self.spec.paths.clone();
        // Every process of the account goes first (spike 7: NixOS leaves them running).
        if let Some(uid) = j.uid {
            let ctx = pam::Ctx {
                paths: paths.clone(),
                users: paths.users.clone(),
                bus: self.spec.bus.clone(),
                journal_socket: pam::JOURNAL_SOCKET.into(),
            };
            pam::end_uid(&ctx, &j.user, uid, &paths.proc_dir);
        }
        let outcome = self.apply(&Apply {
            state: Some(j.state.clone()),
            ..Default::default()
        })?;
        // After a successful switch only: the home, PIN, metadata, app links, usage.
        self.set_stage(Stage::Finishing);
        let mut freed = 0;
        if let (Some(home), Some(uid)) = (&j.home, j.uid) {
            freed = usage::measure(home, 0).map_or(0, |u| u.total_bytes);
            delete_home(home, uid);
        }
        Pins::new(paths.pins_dir()).remove(&j.id);
        if let Err(e) = ProfilesFile::update(&paths.profiles_json(), |f| f.profiles.remove(&j.id)) {
            warn!("profiles.json: {e}");
        }
        if let Ok(file) = ProfilesFile::load(&paths.profiles_json()) {
            public::publish(&paths, &j.state, &file, job::now());
        }
        let link = paths.app_profile(&j.user);
        if let Some(dir) = link.parent()
            && let Ok(entries) = fs::read_dir(dir)
        {
            let prefix = format!("{}-", j.user);
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let generation_link = name
                    .strip_prefix(&prefix)
                    .and_then(|r| r.strip_suffix("-link"))
                    .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()));
                if name == j.user || generation_link {
                    let _ = fs::remove_file(e.path());
                }
            }
        }
        let _ = fs::remove_file(paths.apps_run_dir().join(&j.user));
        let mut u = usage::load(&paths.usage_json());
        if u.remove(&j.id).is_some() {
            let _ = usage::save(&paths.usage_json(), &u);
        }
        Registry::reconcile(&paths, &j.state);
        appreg::publish(&paths, &j.state);
        self.jellyfin_sync_users();
        info!(profile = %j.id, user = %j.user, freed_bytes = freed, "profile removed");
        Ok(outcome)
    }
}

impl Runner {
    /// End guest session: hide the guest, end its session, then wipe it. Nothing is deleted
    /// unless the account is still exactly the guest and none of its processes are left.
    pub(super) fn guest_end(&mut self, j: &GuestEndJob) -> Outcome {
        self.set_stage(Stage::Preparing);
        let paths = self.spec.paths.clone();
        if let Err(e) = guest::mark(&paths, &self.spec.id) {
            warn!("cannot mark the guest as ending: {e}");
        }
        let state = State::load(&paths.state_json()).ok().map(|(s, _)| s);
        let republish = |state: &Option<State>| {
            if let (Some(s), Ok(file)) = (state, ProfilesFile::load(&paths.profiles_json())) {
                public::publish(&paths, s, &file, job::now());
            }
        };
        republish(&state);
        let outcome = self.guest_wipe(j, state.as_ref());
        guest::unmark(&paths);
        republish(&state);
        outcome
    }

    fn guest_wipe(&mut self, j: &GuestEndJob, state: Option<&State>) -> Outcome {
        let paths = self.spec.paths.clone();
        // The account again (the service checked it when the request came in).
        let account = guest::account(&paths.users, j.uid).map_err(|why| self.fail(&why))?;
        if account.home != j.home {
            return Err(self.fail("the guest's home changed since the request"));
        }
        // Its session and every process of the uid go first.
        let ctx = pam::Ctx {
            paths: paths.clone(),
            users: paths.users.clone(),
            bus: self.spec.bus.clone(),
            journal_socket: pam::JOURNAL_SOCKET.into(),
        };
        for attempt in 0..3 {
            if pam::processes_of(&paths.proc_dir, j.uid).is_empty() {
                break;
            }
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            pam::end_uid(&ctx, GUEST_USER, j.uid, &paths.proc_dir);
        }
        let left = pam::processes_of(&paths.proc_dir, j.uid);
        if !left.is_empty() {
            return Err(self.fail(format!(
                "the guest is still logged in ({} processes left); nothing was deleted",
                left.len()
            )));
        }
        self.set_stage(Stage::Finishing);
        // The home's contents (the directory stays: tmpfiles made it, 0700 for the guest).
        let mut removed = 0;
        let mut problems: Vec<String> = Vec::new();
        match datadel::wipe_owned(&j.home, j.uid, true) {
            Ok(w) => {
                removed += w.removed;
                problems.extend(w.failed);
            }
            Err(datadel::Refused::BadHome(_, why)) if !j.home.exists() => {
                info!("the guest has no home yet: {why}");
            }
            Err(e) => problems.push(e.to_string()),
        }
        for dir in &j.tmp_dirs {
            match datadel::wipe_owned(dir, j.uid, false) {
                Ok(w) => {
                    removed += w.removed;
                    problems.extend(w.failed);
                }
                Err(e) => warn!(dir = %dir.display(), "not cleared: {e}"),
            }
        }
        // The app env: the guest's link goes, and it starts from the starter env again.
        let link = paths.guest_link();
        match fs::remove_file(&link) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => problems.push(format!("{}: {e}", link.display())),
        }
        if state.is_some_and(State::guest_enabled)
            && let Some(starter) = appreg::current(&Owner::Starter, &paths)
            && let Err(super::Failure::Error(e)) = self.set_env(&Owner::Guest, &starter.env)
        {
            warn!("the guest did not get the starter env again: {}", e.message);
        }
        // Per-guest state: terms it accepted, its usage entry, a switch hint into it.
        match guest::drop_terms(&paths.terms_accepted()) {
            Ok(n) if n > 0 => info!(dropped = n, "the guest's terms acceptances removed"),
            Ok(_) => {}
            Err(e) => problems.push(format!("terms: {e}")),
        }
        let mut u = usage::load(&paths.usage_json());
        if u.remove("guest").is_some() {
            let _ = usage::save(&paths.usage_json(), &u);
        }
        if public::SwitchHint::load(&paths.switch_json()).is_some_and(|h| h.target == "guest") {
            let _ = fs::remove_file(paths.switch_json());
        }
        if let Some(s) = state {
            Registry::reconcile(&paths, s);
            appreg::publish(&paths, s);
        }
        if !problems.is_empty() {
            return Err(self.fail(format!(
                "the guest was only partly wiped: {}",
                problems.join("; ")
            )));
        }
        info!(removed, "guest session ended and wiped");
        Ok((None, false))
    }
}

/// Delete a removed account's home: every entry by its raw name with the no-symlink walk of
/// `datadel`, then the (now empty) directory itself.
fn delete_home(home: &Path, uid: u32) {
    match datadel::empty_home(home, uid) {
        Ok(w) => {
            for e in &w.failed {
                warn!(home = %home.display(), "not deleted: {e}");
            }
        }
        Err(e) => {
            warn!(home = %home.display(), "cannot empty the home: {e}");
            return;
        }
    }
    match fs::remove_dir(home) {
        Ok(()) => info!(home = %home.display(), "home deleted"),
        Err(e) => warn!(home = %home.display(), "home not removed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    /// Names a data path would refuse (and names that are not UTF-8) go too, so the home
    /// itself can go.
    #[test]
    fn delete_home_takes_every_name() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("hinteros-p2");
        fs::create_dir_all(home.join("~backup/sub")).unwrap();
        fs::write(home.join("~backup/sub/f"), "x").unwrap();
        fs::write(home.join("notes[1].txt"), "x").unwrap();
        fs::write(home.join(std::ffi::OsStr::from_bytes(b"caf\xe9")), "x").unwrap();
        fs::write(home.join(".bashrc"), "x").unwrap();
        let outside = d.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, home.join("link")).unwrap();
        let uid = rustix::process::getuid().as_raw();
        delete_home(&home, uid + 1);
        assert!(home.join(".bashrc").exists(), "not the account's home");
        delete_home(&home, uid);
        assert!(!home.exists());
        assert!(outside.join("keep").exists(), "links are not followed");
    }
}
