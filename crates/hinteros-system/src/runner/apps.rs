//! Jobs on app envs (apps.md): build envs from the running system's
//! `config.hinteros.appEnv.build`, set an owner's Nix profile, the `box` stage, the final `apps`
//! stage of system jobs, and the v1 → v2 migration.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use hinteros_api::catalog::Index;
use hinteros_api::system::{AppNotUpdated, AppliedResult, JobResult, Stage};
use hinteros_api::{ErrorCode, ErrorObject};
use serde::Deserialize;
use tracing::{info, warn};

use super::{Failure, Outcome, Runner, record_terms};
use crate::appreg::{self, Owner, Registry};
use crate::job::{self, Apply, AppsJob, AppsOp, MigrateJob};
use crate::nixlog;
use crate::state::State;
use crate::{fsutil, generations};

/// A Nix string literal.
fn nix_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '$' => out.push_str("\\$"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The expression that evaluates every env of `envs.json` in one evaluation of the running
/// system's flake (`/etc/hinteros` always describes it).
pub fn env_expr(config_dir: &Path, envs_file: &Path) -> String {
    format!(
        "let d = builtins.getFlake {flake}; b = d.nixosConfigurations.hinteros.config.hinteros.appEnv.build; \
         in map (ids: let e = b ids; in {{ drv = e.drvPath; out = e.outPath; }}) \
         (builtins.fromJSON (builtins.readFile {file}))",
        flake = nix_str(&format!("path:{}", config_dir.display())),
        file = nix_str(&envs_file.display().to_string()),
    )
}

#[derive(Deserialize)]
struct Evaluated {
    drv: String,
    out: String,
}

/// A store path as nix prints it: absolute, no `..`, printable.
fn store_path_ok(p: &str) -> bool {
    p.starts_with('/')
        && !p.split('/').any(|c| c == "..")
        && p.chars().all(|c| c.is_ascii_graphic())
}

impl Runner {
    /// Report stages as `stage` until [`Runner::end_override`] (the `box` stage wraps a whole
    /// apply flow).
    fn override_stage(&mut self, stage: Stage) {
        self.set_stage(stage);
        self.stage_override = Some(stage);
    }

    fn end_override(&mut self) {
        self.stage_override = None;
    }

    /// Build the envs of `sets` (each a sorted id list). One evaluation for all of them; if it
    /// fails and there is more than one set, each set is evaluated alone so one broken env
    /// does not hold back the others. Per set: the env's store path, or why it failed.
    pub(super) fn build_envs(
        &mut self,
        sets: &[Vec<String>],
    ) -> Result<Vec<Result<PathBuf, String>>, Failure> {
        if sets.is_empty() {
            return Ok(Vec::new());
        }
        self.check_cancel()?;
        self.set_stage(Stage::Evaluating);
        let evaluated: Vec<Result<Evaluated, String>> = match self.eval_envs(sets, "envs") {
            Ok(all) => all.into_iter().map(Ok).collect(),
            Err(Failure::Cancelled) => return Err(Failure::Cancelled),
            Err(Failure::Error(e)) if sets.len() == 1 => vec![Err(e.message_hint())],
            Err(Failure::Error(_)) => {
                let mut out = Vec::new();
                for (i, set) in sets.iter().enumerate() {
                    out.push(
                        match self.eval_envs(std::slice::from_ref(set), &format!("env-{i}")) {
                            Ok(mut v) if v.len() == 1 => Ok(v.remove(0)),
                            Ok(_) => Err("nix eval returned no env".into()),
                            Err(Failure::Cancelled) => return Err(Failure::Cancelled),
                            Err(Failure::Error(e)) => Err(e.message_hint()),
                        },
                    );
                }
                out
            }
        };
        let drvs: Vec<String> = evaluated
            .iter()
            .filter_map(|e| e.as_ref().ok())
            .map(|e| format!("{}^out", e.drv))
            .collect();
        let mut build_error = None;
        if !drvs.is_empty() {
            self.check_cancel()?;
            let mut cmd = self.nix_command();
            cmd.args([
                "build",
                "--no-link",
                "--print-out-paths",
                "--keep-going",
                "--log-format",
                "internal-json",
                "-v",
            ])
            .args(&drvs);
            let mut progress = nixlog::Progress::default();
            let built = self.exec(cmd, true, &mut |r, line| {
                if progress.line(line) {
                    r.nix_progress(&progress);
                }
            });
            match built {
                Ok(_) => {}
                Err(Failure::Cancelled) => return Err(Failure::Cancelled),
                Err(Failure::Error(e)) => {
                    build_error = Some(progress.last_error.clone().unwrap_or(e.message_hint()));
                }
            }
        }
        Ok(evaluated
            .into_iter()
            .map(|e| {
                let e = e?;
                let out = PathBuf::from(&e.out);
                if out.join(hinteros_api::manifest::MANIFEST_PATH).exists() {
                    Ok(out)
                } else {
                    Err(build_error
                        .clone()
                        .unwrap_or_else(|| "the env was not built".into()))
                }
            })
            .collect())
    }

    fn eval_envs(&mut self, sets: &[Vec<String>], name: &str) -> Result<Vec<Evaluated>, Failure> {
        let file = self.dir.join(format!("{name}.json"));
        let bytes = serde_json::to_vec(sets).map_err(|e| self.fail(e.to_string()))?;
        fsutil::write_atomic(&file, &bytes, 0o644)
            .map_err(|e| self.fail(format!("writing {}: {e}", file.display())))?;
        let mut cmd = self.nix_command();
        cmd.args(["eval", "--impure", "--json", "--expr"])
            .arg(env_expr(&self.spec.paths.config_dir, &file));
        let out = self.exec(cmd, true, &mut |_, _| {})?;
        let v: Vec<Evaluated> = serde_json::from_str(out.trim())
            .map_err(|e| self.fail(format!("nix eval printed no env list: {e}")))?;
        if v.len() != sets.len()
            || v.iter()
                .any(|e| !store_path_ok(&e.drv) || !store_path_ok(&e.out))
        {
            return Err(self.fail("nix eval printed an unexpected env list"));
        }
        Ok(v)
    }

    /// Make `env` the owner's current env: a new generation of its Nix profile, or the guest's
    /// link (an indirect GC root). Nothing happens when it already is.
    pub(super) fn set_env(&mut self, owner: &Owner, env: &Path) -> Result<bool, Failure> {
        let link = owner.link(&self.spec.paths);
        if fs::canonicalize(&link).ok().as_deref() == Some(env) {
            return Ok(false);
        }
        if let Some(parent) = link.parent() {
            let _ = fs::DirBuilder::new()
                .recursive(true)
                .mode(0o755)
                .create(parent);
        }
        if owner.has_generations() {
            let mut cmd = Command::new(&self.spec.tools.nix_env);
            cmd.arg("-p").arg(&link).arg("--set").arg(env);
            self.exec(cmd, false, &mut |_, _| {})?;
        } else {
            let mut cmd = Command::new(&self.spec.tools.nix_store);
            cmd.arg("--add-root")
                .arg(&link)
                .arg("--indirect")
                .arg("--realise")
                .arg(env);
            self.exec(cmd, false, &mut |_, _| {})?;
        }
        info!(owner = %owner.key(), env = %env.display(), "app env set");
        Ok(true)
    }

    /// The state of `/etc/hinteros` (after a switch: the new one).
    fn config_state(&self) -> Option<State> {
        State::load(&self.spec.paths.state_json())
            .ok()
            .map(|(s, _)| s)
    }

    /// Write the registry from the manifests and publish the session links.
    fn refresh_registry(&self) {
        if let Some(state) = self.config_state() {
            Registry::reconcile(&self.spec.paths, &state);
            appreg::publish(&self.spec.paths, &state);
        }
    }

    fn applied(&self, apps_not_updated: Option<Vec<AppNotUpdated>>) -> JobResult {
        let after = self.snapshot();
        JobResult::Applied(AppliedResult {
            generation: after.generation,
            rev: after.rev,
            apps_not_updated,
            features_readded: None,
        })
    }

    pub(super) fn apps_job(&mut self, j: &AppsJob) -> Outcome {
        self.set_stage(Stage::Preparing);
        self.inhibit("delay");
        let paths = self.spec.paths.clone();
        if !j.terms.is_empty() {
            record_terms(&paths.terms_accepted(), &j.terms)
                .map_err(|e| self.fail(format!("recording the accepted terms: {e}")))?;
        }
        let mut restart = false;
        if let Some(state) = &j.box_state {
            self.override_stage(Stage::Box);
            let outcome = self.apply(&Apply {
                state: Some(state.clone()),
                ..Default::default()
            });
            self.end_override();
            let (_, r) = outcome?;
            restart = r;
            // The box stage left the profile's apps alone; from here on it is the env.
            self.inhibitor = None;
            self.inhibit("delay");
        }
        // A guest that still has the old starter env (it installed nothing) follows the set.
        let old_starter = (j.owner == Owner::Starter)
            .then(|| appreg::current(&Owner::Starter, &paths).map(|c| c.env))
            .flatten();
        let env = match &j.op {
            AppsOp::Set { ids } => {
                let mut built = self.build_envs(std::slice::from_ref(ids))?;
                match built.pop() {
                    Some(Ok(env)) => Some(env),
                    Some(Err(why)) => return Err(self.fail(why)),
                    None => return Err(self.fail("no env")),
                }
            }
            AppsOp::CopyFrom { from } => Some(
                appreg::current(from, &paths)
                    .ok_or_else(|| self.fail(format!("{} has no app env", from.key())))?
                    .env,
            ),
            AppsOp::Rollback { .. } => None,
        };
        self.check_cancel()?;
        if self.dir.join(job::ACTIVATING).exists() {
            // The box stage already passed the point of no cancel.
            self.set_stage(Stage::Activating);
        } else {
            self.enter_activation()?;
        }
        match (&j.op, env) {
            (AppsOp::Rollback { generation }, _) => {
                let link = j.owner.link(&paths);
                if !generations::links(&link)
                    .iter()
                    .any(|l| l.generation == *generation)
                {
                    return Err(Failure::Error(ErrorObject::new(
                        ErrorCode::NotFound,
                        format!("app generation {generation} is not on disk"),
                    )));
                }
                let mut cmd = Command::new(&self.spec.tools.nix_env);
                cmd.arg("-p")
                    .arg(&link)
                    .arg("--switch-generation")
                    .arg(generation.to_string());
                self.exec(cmd, false, &mut |_, _| {})?;
            }
            (_, Some(env)) => {
                self.set_env(&j.owner, &env)?;
            }
            (_, None) => return Err(self.fail("no env")),
        }
        self.set_stage(Stage::Finishing);
        if let Some(s) = &j.starter {
            s.save(&paths.starter_json())
                .map_err(|e| self.fail(format!("writing starter.json: {e}")))?;
        }
        if let (Some(old), Some(new)) = (old_starter, appreg::current(&Owner::Starter, &paths))
            && appreg::current(&Owner::Guest, &paths).is_some_and(|g| g.env == old)
            && let Err(Failure::Error(e)) = self.set_env(&Owner::Guest, &new.env)
        {
            warn!("the guest keeps the old starter env: {}", e.message);
        }
        self.refresh_registry();
        if let Some(d) = &j.delete_data {
            super::delete_data(d);
        }
        Ok((Some(self.applied(None)), restart))
    }

    /// The final stage of a job that switched the system to another revision: rebuild every
    /// owner's env against it, in one evaluation. An env that fails keeps its generation (it
    /// still works) and is reported.
    pub(super) fn apps_stage(&mut self) -> Result<Option<Vec<AppNotUpdated>>, Failure> {
        let Some(state) = self.config_state().filter(State::is_schema2) else {
            return Ok(None);
        };
        self.set_stage(Stage::Apps);
        let paths = self.spec.paths.clone();
        let index = Index::load(&paths.catalog_index())
            .map_err(|e| warn!("no catalog after the switch: {e}"))
            .ok();
        let mut work: Vec<(Owner, Vec<String>)> = Vec::new();
        for owner in appreg::owners(&state) {
            if let Some(c) = appreg::current(&owner, &paths) {
                // Ids the new catalog removed (tombstones) or no longer has are dropped.
                let ids: Vec<String> = c
                    .manifest
                    .ids
                    .iter()
                    .filter(|id| index.as_ref().is_none_or(|i| i.app(id).is_some()))
                    .cloned()
                    .collect();
                work.push((owner, ids));
            }
        }
        let mut sets: Vec<Vec<String>> = work.iter().map(|(_, ids)| ids.clone()).collect();
        sets.sort();
        sets.dedup();
        let built: BTreeMap<Vec<String>, Result<PathBuf, String>> = match self.build_envs(&sets) {
            Ok(b) => sets.into_iter().zip(b).collect(),
            Err(Failure::Cancelled) => BTreeMap::new(),
            Err(Failure::Error(e)) => sets
                .into_iter()
                .map(|s| (s, Err(e.message_hint())))
                .collect(),
        };
        self.set_stage(Stage::Apps);
        let mut not_updated = Vec::new();
        for (owner, ids) in work {
            let reason = match built.get(&ids) {
                Some(Ok(env)) => match self.set_env(&owner, env) {
                    Ok(_) => continue,
                    Err(Failure::Error(e)) => e.message_hint(),
                    Err(Failure::Cancelled) => "cancelled".into(),
                },
                Some(Err(why)) => why.clone(),
                None => "not built".into(),
            };
            warn!(owner = %owner.key(), %reason, "app env not updated");
            not_updated.push(AppNotUpdated {
                owner: owner.key(),
                ids,
                reason: reason.chars().take(300).collect(),
            });
        }
        self.refresh_registry();
        Ok(Some(not_updated).filter(|v| !v.is_empty()))
    }

    pub(super) fn migrate(&mut self, m: &MigrateJob) -> Outcome {
        self.set_stage(Stage::Preparing);
        self.inhibit("delay");
        let paths = self.spec.paths.clone();
        match State::load(&paths.state_json()) {
            Ok((s, _)) if s == m.from => {}
            Ok((s, _)) if s.is_schema2() => {
                info!("state.json is already schema 2; nothing to migrate");
                return Ok((Some(self.applied(None)), false));
            }
            _ => return Err(self.fail("state.json changed since the migration was queued")),
        }
        // 1. One env for the box-wide apps (everything is in the store: an evaluation and a
        //    local buildEnv).
        let env = match self.build_envs(std::slice::from_ref(&m.ids))?.pop() {
            Some(Ok(env)) => env,
            Some(Err(why)) => return Err(self.fail(why)),
            None => return Err(self.fail("no env")),
        };
        // 2. Every account and the starter set get it (idempotent: already set is skipped).
        self.set_stage(Stage::Activating);
        for owner in m.owners.iter().chain([&Owner::Starter]) {
            self.set_env(owner, &env)?;
        }
        if appreg::StarterFile::load(&paths.starter_json()).is_none() {
            appreg::StarterFile {
                version: appreg::STARTER_VERSION,
                ids: m.ids.clone(),
                updated_at: Some(job::now()),
                by: Some("migration".into()),
            }
            .save(&paths.starter_json())
            .map_err(|e| self.fail(format!("writing starter.json: {e}")))?;
        }
        if !m.terms.is_empty() {
            let accepted = super::accepted_terms(&paths.terms_accepted());
            let missing: Vec<job::TermsRecord> = m
                .terms
                .iter()
                .filter(|t| {
                    !accepted.iter().any(|(id, v, _, p)| {
                        id == &t.id && v == &t.version && p.as_deref() == t.profile.as_deref()
                    })
                })
                .cloned()
                .collect();
            if !missing.is_empty() {
                record_terms(&paths.terms_accepted(), &missing)
                    .map_err(|e| self.fail(format!("recording terms: {e}")))?;
            }
        }
        let manifest =
            hinteros_api::manifest::Manifest::load(&env).map_err(|e| self.fail(e.to_string()))?;
        let reg = Registry::scan(&paths, &m.from.migrated(Vec::new()));
        reg.save(&paths.registry())
            .map_err(|e| self.fail(format!("writing the registry: {e}")))?;
        appreg::publish(&paths, &m.from);
        // 3. state.json schema 2 through the apply flow. A failure leaves schema 1.
        let mut requirements = reg.needed_features();
        requirements.extend(manifest.box_features.iter().cloned());
        let new = m.from.migrated(requirements.into_iter().collect());
        let outcome = self.apply(&Apply {
            state: Some(new),
            ..Default::default()
        })?;
        self.refresh_registry();
        Ok(outcome)
    }
}

impl Runner {
    /// The `apps` stage after a system switch, folded into an apply result.
    pub(super) fn with_apps_stage(&mut self, outcome: (Option<JobResult>, bool)) -> Outcome {
        let not_updated = self.apps_stage()?;
        let (result, restart) = outcome;
        let result = match result {
            Some(JobResult::Applied(mut a)) => {
                a.apps_not_updated = not_updated;
                Some(JobResult::Applied(a))
            }
            other => other,
        };
        Ok((result, restart))
    }
}

impl Runner {
    /// After a system rollback (and its `apps` stage): box features the owners' current envs
    /// need that the restored generation lacks are turned back on in a `box` stage, the apply
    /// flow on the restored configuration with `requirements` = what the envs need (a second
    /// build, mostly cached; apps.md open question 2). Nothing to do on a schema-1 target or
    /// when every needed feature is on (features nobody needs stay until the next rebuild).
    pub(super) fn readd_features(&mut self, outcome: (Option<JobResult>, bool)) -> Outcome {
        let Some(state) = self.config_state().filter(State::is_schema2) else {
            return Ok(outcome);
        };
        let needed = Registry::scan(&self.spec.paths, &state).needed_features();
        let missing = missing_features(&state, &needed);
        if missing.is_empty() {
            return Ok(outcome);
        }
        info!(features = ?missing, "the rolled-back generation lacks box features the apps need; re-adding them");
        let mut new = state.clone();
        new.requirements = Some(needed.into_iter().collect());
        self.override_stage(Stage::Box);
        let applied = self.apply(&Apply {
            state: Some(new),
            ..Default::default()
        });
        self.end_override();
        let (_, restart) = applied.map_err(|f| match f {
            Failure::Error(mut e) => {
                e.message = format!(
                    "rolled back, but {} could not be turned back on: {}",
                    missing.join(", "),
                    e.message
                );
                Failure::Error(e)
            }
            other => other,
        })?;
        let (result, restarted) = outcome;
        let after = self.snapshot();
        let result = match result {
            Some(JobResult::Applied(a)) => Some(JobResult::Applied(AppliedResult {
                generation: after.generation,
                rev: after.rev,
                features_readded: Some(missing),
                ..a
            })),
            other => other,
        };
        Ok((result, restart || restarted))
    }
}

/// Features in `needed` that `state` does not turn on (sorted).
pub fn missing_features(state: &State, needed: &std::collections::BTreeSet<String>) -> Vec<String> {
    needed
        .iter()
        .filter(|f| !state.requirements().contains(f))
        .cloned()
        .collect()
}

trait Hint {
    fn message_hint(&self) -> String;
}

impl Hint for ErrorObject {
    /// The job's log hint (the nix error line), else the message.
    fn message_hint(&self) -> String {
        self.data
            .as_ref()
            .and_then(|d| d.get("log_hint"))
            .and_then(|h| h.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| self.message.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expression_quotes_paths() {
        let e = env_expr(Path::new("/etc/hinteros"), Path::new("/run/x/\"$y\".json"));
        assert!(
            e.contains(r#"builtins.getFlake "path:/etc/hinteros""#),
            "{e}"
        );
        assert!(e.contains(r#"readFile "/run/x/\"\$y\".json""#), "{e}");
        assert!(store_path_ok("/nix/store/abc-hinteros-apps"));
        assert!(!store_path_ok("relative") && !store_path_ok("/nix/../x"));
    }
}
