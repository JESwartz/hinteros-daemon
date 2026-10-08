//! Profile jobs (profiles.md "Creating a profile", "Removing a profile").

use std::fs;
use std::path::Path;

use hinteros_api::system::Stage;
use tracing::{info, warn};

use super::{Outcome, Runner};
use crate::appreg::{self, Owner, Registry};
use crate::job::{self, Apply, ProfileCreateJob, ProfileRemoveJob};
use crate::pins::Pins;
use crate::profiles::ProfilesFile;
use crate::{datadel, pam, public, usage};

impl Runner {
    pub(super) fn profile_create(&mut self, j: &ProfileCreateJob) -> Outcome {
        // The account (a rebuild that substitutes nothing).
        let outcome = self.apply(&Apply {
            state: Some(j.state.clone()),
            ..Default::default()
        })?;
        self.set_stage(Stage::Finishing);
        let paths = self.spec.paths.clone();
        // Metadata and PIN only after the account exists: a failed job leaves neither.
        let mut file = ProfilesFile::load(&paths.profiles_json())
            .map_err(|e| self.fail(format!("profiles.json: {e}")))?;
        file.profiles.insert(j.id.clone(), j.meta.clone());
        file.save(&paths.profiles_json())
            .map_err(|e| self.fail(format!("writing profiles.json: {e}")))?;
        Pins::new(paths.pins_dir())
            .set_hash(&j.id, &j.pin_hash)
            .map_err(|e| self.fail(format!("writing the PIN: {e}")))?;
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
        if let Ok(mut file) = ProfilesFile::load(&paths.profiles_json()) {
            file.profiles.remove(&j.id);
            if let Err(e) = file.save(&paths.profiles_json()) {
                warn!("profiles.json: {e}");
            }
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
        if u.remove(&j.id).is_some()
            && let Ok(bytes) = serde_json::to_vec_pretty(&u)
        {
            let _ = crate::fsutil::write_atomic(&paths.usage_json(), &bytes, 0o600);
        }
        Registry::reconcile(&paths, &j.state);
        appreg::publish(&paths, &j.state);
        info!(profile = %j.id, user = %j.user, freed_bytes = freed, "profile removed");
        Ok(outcome)
    }
}

/// Delete a removed account's home: every entry with the no-symlink walk of `datadel`, then the
/// (now empty) directory itself.
fn delete_home(home: &Path, uid: u32) {
    let entries: Vec<String> = match fs::read_dir(home) {
        Ok(e) => e
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(e) => {
            warn!(home = %home.display(), "cannot read the home: {e}");
            return;
        }
    };
    for name in entries {
        if let Err(e) = datadel::delete_in_home(home, uid, &name) {
            warn!(home = %home.display(), entry = %name, "not deleted: {e}");
        }
    }
    match fs::remove_dir(home) {
        Ok(()) => info!(home = %home.display(), "home deleted"),
        Err(e) => warn!(home = %home.display(), "home not removed: {e}"),
    }
}
