//! Per-profile apps (apps.md "Install, remove, rollback"):
//! `apps.list`, `apps.generations`, `apps.install`, `apps.remove`, `apps.rollback`,
//! `apps.accept_terms`, `apps.clear_data` and the box-feature bookkeeping of system jobs.

use std::collections::BTreeSet;

use hinteros_api::system::{
    AppGeneration, AppsAcceptTermsParams, AppsClearDataParams, AppsGenerationsResult,
    AppsInstallParams, AppsListResult, AppsRemoveParams, AppsRollbackParams, Empty, JobKind,
    JobSubject,
};
use hinteros_api::{ErrorCode, ErrorObject};
use serde_json::Value;
use tracing::info;

use super::{Service, internal, job_ref, state_conflict};
use crate::appreg::{self, Owner, Registry};
use crate::job::{self, Action, AppsJob, AppsOp, DeleteData, Home, TermsRecord};
use crate::peer::Peer;
use crate::profiles::{self, Caller, ProfilesFile, Role};
use crate::state::State;
use crate::{runner, store};

const APPS_ACTION: &str = "org.hinteros.system.apps";

impl Service {
    /// `profiles.json` now; a file that cannot be read is locked down (fails closed).
    pub(super) fn profiles_file(&self) -> ProfilesFile {
        ProfilesFile::load_or_locked(&self.paths.profiles_json())
    }

    pub(super) fn caller(&self, uid: u32, state: &State) -> Caller {
        profiles::resolve(uid, state, &self.profiles_file(), &self.users)
    }

    /// The caller's app owner: a profile or the guest (spares and other users have no apps).
    pub(super) fn app_owner(
        &self,
        uid: u32,
        state: &State,
    ) -> Result<(Caller, Owner), ErrorObject> {
        let caller = self.caller(uid, state);
        let owner = match (&caller.role, &caller.id, &caller.user) {
            (Role::Profile, Some(id), Some(u)) => Owner::Profile {
                id: id.clone(),
                user: u.name.clone(),
            },
            (Role::Guest, _, _) => Owner::Guest,
            _ => return Err(profiles::not_a_profile(APPS_ACTION)),
        };
        Ok((caller, owner))
    }

    /// The state an apply flow should write: `requirements` = the features some env needs
    /// (unneeded ones are dropped by jobs that rebuild anyway). `None`: no change.
    pub(super) fn requirements_update(&self, state: &State) -> Option<State> {
        let needed: Vec<String> = Registry::scan(&self.paths, state)
            .needed_features()
            .into_iter()
            .collect();
        (state.requirements != needed).then(|| {
            let mut s = state.clone();
            s.requirements = needed;
            s
        })
    }

    /// The `box` stage's state when `wanted` features are not all on: requirements become every
    /// needed feature plus `wanted`.
    pub(super) fn box_state(&self, state: &State, wanted: &[String]) -> Option<State> {
        let enabled: BTreeSet<&String> = state.requirements.iter().collect();
        if wanted.iter().all(|f| enabled.contains(f)) {
            return None;
        }
        let mut needed = Registry::scan(&self.paths, state).needed_features();
        needed.extend(wanted.iter().cloned());
        let mut s = state.clone();
        s.requirements = needed.into_iter().collect();
        Some(s)
    }

    /// Ids of the env that need their vendor terms accepted by this caller.
    fn needs_terms(&self, caller: &Caller, ids: &[String]) -> Vec<String> {
        let Ok(index) = self.catalog() else {
            return Vec::new();
        };
        let accepted = runner::accepted_terms(&self.paths.terms_accepted());
        let user = caller.user_name();
        ids.iter()
            .filter(|id| {
                let Some(t) = index.app(id).and_then(|e| e.terms.as_ref()) else {
                    return false;
                };
                !accepted.iter().any(|(aid, v, by, profile)| {
                    aid == *id
                        && *v == t.version
                        && (profile.is_some() && profile == &caller.id
                            || profile.is_none() && *by == user)
                })
            })
            .cloned()
            .collect()
    }

    pub(super) fn apps_list(&self, peer: &Peer) -> Result<AppsListResult, ErrorObject> {
        let (state, _) = self.load_state()?;
        let caller = self.caller(peer.uid, &state);
        if caller.role == Role::Spare {
            return Ok(AppsListResult {
                revision: appreg::revision(None),
                generation: None,
                ids: Vec::new(),
                needs_terms: Vec::new(),
                box_features: Vec::new(),
            });
        }
        let (caller, owner) = self.app_owner(peer.uid, &state)?;
        let cur = appreg::current(&owner, &self.paths);
        let ids = cur
            .as_ref()
            .map(|c| c.manifest.ids.clone())
            .unwrap_or_default();
        Ok(AppsListResult {
            revision: appreg::revision(cur.as_ref()),
            generation: cur.as_ref().and_then(|c| c.generation),
            needs_terms: self.needs_terms(&caller, &ids),
            box_features: cur.map(|c| c.manifest.box_features).unwrap_or_default(),
            ids,
        })
    }

    pub(super) fn apps_generations(
        &self,
        peer: &Peer,
    ) -> Result<AppsGenerationsResult, ErrorObject> {
        let (state, _) = self.load_state()?;
        let (_, owner) = self.app_owner(peer.uid, &state)?;
        Ok(AppsGenerationsResult {
            generations: appreg::generations(&owner, &self.paths)
                .into_iter()
                .map(|g| AppGeneration {
                    generation: g.generation,
                    date: g.date,
                    ids: g
                        .manifest
                        .as_ref()
                        .map(|m| m.ids.clone())
                        .unwrap_or_default(),
                    system_rev: g.manifest.and_then(|m| m.system_rev),
                    current: g.current,
                })
                .collect(),
        })
    }

    pub(super) async fn apps_install(
        &self,
        peer: &Peer,
        p: AppsInstallParams,
    ) -> Result<Value, ErrorObject> {
        let (state, _) = self.load_state()?;
        let (caller, owner) = self.app_owner(peer.uid, &state)?;
        let cur = appreg::current(&owner, &self.paths);
        if p.revision != appreg::revision(cur.as_ref()) {
            return Err(state_conflict());
        }
        let index = self.catalog()?;
        let installed = cur.map(|c| c.manifest.ids).unwrap_or_default();
        let plan = store::plan_install(&index, &installed, &p.ids, &p.accepted_terms)?;
        if plan.new.is_empty() {
            return job_ref(None);
        }
        self.preflight_with(plan.size_mib.saturating_mul(1 << 20))?;
        let by = caller.user_name();
        let now = job::now();
        let terms: Vec<TermsRecord> = plan
            .terms
            .iter()
            .map(|(id, version)| TermsRecord {
                id: id.clone(),
                version: version.clone(),
                accepted_at: now,
                by: by.clone(),
                profile: caller.id.clone(),
            })
            .collect();
        let box_state = self.box_state(&state, &index.box_features(&plan.new));
        let ids = appreg::normalize(installed.into_iter().chain(plan.new.iter().cloned()));
        let subject = JobSubject {
            apps: Some(self.subject_apps(&plan.new)),
            ..self.owner_subject(&owner)
        };
        let spec = self
            .spec(
                JobKind::AppsInstall,
                peer.uid,
                Action::Apps(AppsJob {
                    owner: owner.clone(),
                    op: AppsOp::Set { ids },
                    box_state,
                    terms: terms.clone(),
                    delete_data: None,
                    starter: None,
                }),
            )
            .about(subject);
        let id = self.start(spec).await?;
        for t in &terms {
            self.audit
                .terms_accepted(peer.uid, &by, caller.id.as_deref(), &t.id, &t.version, &id);
        }
        info!(job = %id, owner = %owner.key(), apps = ?plan.new, "installing apps");
        job_ref(Some(id))
    }

    pub(super) async fn apps_remove(
        &self,
        peer: &Peer,
        p: AppsRemoveParams,
    ) -> Result<Value, ErrorObject> {
        let (state, _) = self.load_state()?;
        let (caller, owner) = self.app_owner(peer.uid, &state)?;
        let cur = appreg::current(&owner, &self.paths);
        if p.revision != appreg::revision(cur.as_ref()) {
            return Err(state_conflict());
        }
        let index = match self.catalog() {
            Ok(i) => Some(i),
            Err(e) if p.delete_data => return Err(e),
            Err(_) => None,
        };
        let installed = cur.map(|c| c.manifest.ids).unwrap_or_default();
        let removing = store::plan_remove(index.as_ref(), &installed, &p.ids)?;
        let ids: Vec<String> = installed
            .into_iter()
            .filter(|i| !removing.contains(i))
            .collect();
        // Only the caller's own home (other profiles' data is never touched).
        let delete_data = match (&index, p.delete_data) {
            (Some(index), true) => Some(DeleteData {
                homes: self.own_homes(peer, &caller),
                paths: store::data_paths(index, &removing, &ids),
            }),
            _ => None,
        };
        let subject = JobSubject {
            apps: Some(self.subject_apps(&removing)),
            delete_data: Some(p.delete_data),
            ..self.owner_subject(&owner)
        };
        let spec = self
            .spec(
                JobKind::AppsRemove,
                peer.uid,
                Action::Apps(AppsJob {
                    owner: owner.clone(),
                    op: AppsOp::Set { ids },
                    box_state: None,
                    terms: Vec::new(),
                    delete_data,
                    starter: None,
                }),
            )
            .about(subject);
        let id = self.start(spec).await?;
        info!(job = %id, owner = %owner.key(), apps = ?removing, delete_data = p.delete_data, "removing apps");
        job_ref(Some(id))
    }

    /// The caller's own home, the only one whose app data it may delete.
    fn own_homes(&self, peer: &Peer, caller: &Caller) -> Vec<Home> {
        match &self.args.test_profile_home {
            Some(dir) => vec![Home {
                user: "test".into(),
                uid: peer.uid,
                dir: dir.clone(),
            }],
            None => caller
                .user
                .iter()
                .filter(|u| {
                    u.uid != 0 && u.home.is_absolute() && u.home != std::path::Path::new("/")
                })
                .map(|u| Home {
                    user: u.name.clone(),
                    uid: u.uid,
                    dir: u.home.clone(),
                })
                .collect(),
        }
    }

    /// `apps.clear_data`: a job that deletes one app's `dataPaths` in the caller's home,
    /// whether the app is installed or not.
    pub(super) async fn apps_clear_data(
        &self,
        peer: &Peer,
        p: AppsClearDataParams,
    ) -> Result<Value, ErrorObject> {
        let (state, _) = self.load_state()?;
        let (caller, owner) = self.app_owner(peer.uid, &state)?;
        let index = self.catalog()?;
        if index.app(&p.id).is_none() {
            return Err(ErrorObject::new(
                ErrorCode::NotFound,
                format!("{} is not in the catalog", p.id),
            ));
        }
        let ids = vec![p.id.clone()];
        let installed = appreg::current(&owner, &self.paths)
            .map(|c| c.manifest.ids)
            .unwrap_or_default();
        let delete = DeleteData {
            homes: self.own_homes(peer, &caller),
            paths: store::data_paths(&index, &ids, &installed),
        };
        let subject = JobSubject {
            apps: Some(self.subject_apps(&ids)),
            delete_data: Some(true),
            ..self.owner_subject(&owner)
        };
        let spec = self
            .spec(JobKind::AppsClearData, peer.uid, Action::ClearData(delete))
            .about(subject);
        let id = self.start(spec).await?;
        info!(job = %id, owner = %owner.key(), app = %p.id, "clearing app data");
        job_ref(Some(id))
    }

    pub(super) async fn apps_rollback(
        &self,
        peer: &Peer,
        p: AppsRollbackParams,
    ) -> Result<Value, ErrorObject> {
        let (state, _) = self.load_state()?;
        let (_, owner) = self.app_owner(peer.uid, &state)?;
        let gens = appreg::generations(&owner, &self.paths);
        let target = gens
            .iter()
            .find(|g| g.generation == p.generation && !g.current)
            .ok_or_else(|| {
                ErrorObject::new(
                    ErrorCode::NotFound,
                    format!(
                        "app generation {} is not on disk or is the current one",
                        p.generation
                    ),
                )
            })?;
        let wanted = target
            .manifest
            .as_ref()
            .map(|m| m.box_features.clone())
            .unwrap_or_default();
        let box_state = self.box_state(&state, &wanted);
        if box_state.is_some() {
            self.preflight()?;
        }
        let subject = JobSubject {
            generation: Some(p.generation),
            from_generation: gens.iter().find(|g| g.current).map(|g| g.generation),
            ..self.owner_subject(&owner)
        };
        let spec = self
            .spec(
                JobKind::AppsRollback,
                peer.uid,
                Action::Apps(AppsJob {
                    owner,
                    op: AppsOp::Rollback {
                        generation: p.generation,
                    },
                    box_state,
                    terms: Vec::new(),
                    delete_data: None,
                    starter: None,
                }),
            )
            .about(subject);
        job_ref(Some(self.start(spec).await?))
    }

    pub(super) fn apps_accept_terms(
        &self,
        peer: &Peer,
        p: AppsAcceptTermsParams,
    ) -> Result<Value, ErrorObject> {
        let (state, _) = self.load_state()?;
        let (caller, owner) = self.app_owner(peer.uid, &state)?;
        let ids = appreg::current(&owner, &self.paths)
            .map(|c| c.manifest.ids)
            .unwrap_or_default();
        if !ids.contains(&p.id) {
            return Err(ErrorObject::new(
                ErrorCode::NotFound,
                format!("{} is not installed in this profile", p.id),
            ));
        }
        let index = self.catalog()?;
        let terms = index.app(&p.id).and_then(|e| e.terms.as_ref());
        match terms {
            Some(t) if t.version == p.version => {}
            Some(t) => {
                return Err(ErrorObject::invalid_field(
                    "version",
                    "not_current",
                    format!("the current terms version of {} is {}", p.id, t.version),
                ));
            }
            None => {
                return Err(ErrorObject::invalid_field(
                    "id",
                    "no_terms",
                    format!("{} has no vendor terms", p.id),
                ));
            }
        }
        if !self
            .needs_terms(&caller, std::slice::from_ref(&p.id))
            .is_empty()
        {
            let by = caller.user_name();
            runner::record_terms(
                &self.paths.terms_accepted(),
                &[TermsRecord {
                    id: p.id.clone(),
                    version: p.version.clone(),
                    accepted_at: job::now(),
                    by: by.clone(),
                    profile: caller.id.clone(),
                }],
            )
            .map_err(internal)?;
            self.audit
                .terms_accepted(peer.uid, &by, caller.id.as_deref(), &p.id, &p.version, "");
        }
        serde_json::to_value(Empty {}).map_err(internal)
    }

    /// Every id some owner has.
    pub(super) fn installed_anywhere(&self, state: &State) -> Vec<String> {
        appreg::normalize(
            Registry::scan(&self.paths, state)
                .owners
                .into_values()
                .flat_map(|o| o.ids),
        )
    }
}
