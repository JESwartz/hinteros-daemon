//! Storage (storage.md): the last measurement, a home's detailed tree, the
//! measure job, the two clean-ups and the automatic clean-up setting.
//!
//! Who sees what: the top level and every profile's total (`storage.get`) are open to every
//! connection; a home's detailed tree (folder and game names) only to that account itself, or
//! with the admin PIN. The trees live in `storage.json` (root, 0600) and leave the service only
//! through `storage.tree`.

use hinteros_api::system::{
    self, CleanupParams, CleanupSetParams, CleanupSettings, JobKind, MAX_KEEP, StorageGetResult,
    StorageTreeParams, StorageTreeResult,
};
use hinteros_api::{ErrorCode, ErrorObject, Method};
use serde_json::Value;
use tracing::info;

use super::{Service, internal, job_ref};
use crate::appreg;
use crate::job::{self, Action, CleanupJob, CleanupTarget, MeasureHome, MeasureJob};
use crate::peer::Peer;
use crate::profiles::Role;
use crate::state::{self, CleanupState, State};
use crate::storage::Stored;

/// A decoded storage request (formats already checked).
pub enum StorageRequest {
    Measure,
    Tree(StorageTreeParams),
    CleanupSystem(CleanupParams),
    CleanupProfiles(CleanupParams),
    CleanupSet(CleanupSetParams),
}

fn check_keep(field: &str, keep: u32) -> Result<(), ErrorObject> {
    if (1..=MAX_KEEP).contains(&keep) {
        Ok(())
    } else {
        Err(ErrorObject::invalid_field(
            field,
            "out_of_range",
            format!("{field} must be 1 to {MAX_KEEP}"),
        ))
    }
}

/// `p<N>` or `guest`.
fn check_tree_id(id: &str) -> Result<(), ErrorObject> {
    if state::is_profile_id(id) || id == "guest" {
        Ok(())
    } else {
        Err(ErrorObject::invalid_field(
            "id",
            "bad_format",
            "id is a profile id or guest",
        ))
    }
}

impl StorageRequest {
    /// Validation of the formats (before the rate limit and polkit).
    pub fn validate(&self) -> Result<(), ErrorObject> {
        match self {
            StorageRequest::Measure => Ok(()),
            StorageRequest::Tree(p) => {
                check_tree_id(&p.id)?;
                super::check_admin_pin(p.admin_pin.as_deref())
            }
            StorageRequest::CleanupSystem(p) | StorageRequest::CleanupProfiles(p) => {
                check_keep("keep", p.keep)?;
                super::check_admin_pin(p.admin_pin.as_deref())
            }
            StorageRequest::CleanupSet(p) => {
                check_keep("keep_system", p.keep_system)?;
                check_keep("keep_profiles", p.keep_profiles)?;
                super::check_admin_pin(p.admin_pin.as_deref())
            }
        }
    }

    pub fn method(&self) -> &'static str {
        match self {
            StorageRequest::Measure => system::StorageMeasure::NAME,
            StorageRequest::Tree(_) => system::StorageTree::NAME,
            StorageRequest::CleanupSystem(_) => system::StorageCleanupSystem::NAME,
            StorageRequest::CleanupProfiles(_) => system::StorageCleanupProfiles::NAME,
            StorageRequest::CleanupSet(_) => system::CleanupSet::NAME,
        }
    }
}

/// The API view of the setting.
pub fn cleanup_settings(c: CleanupState) -> CleanupSettings {
    CleanupSettings {
        enabled: c.enable,
        frequency: c.frequency,
        keep_system: c.keep_system,
        keep_profiles: c.keep_profiles,
    }
}

impl Service {
    fn stored(&self) -> Option<Stored> {
        Stored::load(&self.paths.storage_json())
    }

    pub(super) fn storage_get(&self) -> StorageGetResult {
        StorageGetResult {
            measurement: self.stored().map(|s| s.measurement),
        }
    }

    pub(super) fn cleanup_get(&self) -> Result<CleanupSettings, ErrorObject> {
        let (state, _) = self.load_state()?;
        Ok(cleanup_settings(state.cleanup()))
    }

    /// Whether `id` (a profile id or `guest`) is the caller's own home.
    fn own_home(&self, uid: u32, state: &State, id: &str) -> bool {
        let caller = self.caller(uid, state);
        match caller.role {
            Role::Profile | Role::Spare => caller.id.as_deref() == Some(id),
            Role::Guest => id == "guest",
            Role::Other => false,
        }
    }

    pub(super) async fn storage_request(
        &self,
        peer: &Peer,
        r: StorageRequest,
    ) -> Result<Value, ErrorObject> {
        let uid = peer.uid;
        let method = r.method();
        match r {
            StorageRequest::Measure => {
                // One measurement at a time: a running one is the answer.
                self.poll_jobs();
                {
                    let jobs = self.jobs.lock().unwrap();
                    if let Some(id) = jobs.busy() {
                        let running = jobs
                            .get(id)
                            .is_some_and(|j| j.kind == JobKind::StorageMeasure);
                        if running {
                            return job_ref(Some(id.to_owned()));
                        }
                    }
                }
                let (state, _) = self.load_state()?;
                let spec = self.spec(
                    JobKind::StorageMeasure,
                    uid,
                    Action::StorageMeasure(self.measure_job(&state)),
                );
                job_ref(Some(self.start(spec).await?))
            }
            StorageRequest::Tree(p) => {
                let (state, _) = self.load_state()?;
                if !self.own_home(uid, &state, &p.id) {
                    self.check_box_admin(uid, method, p.admin_pin.as_deref())
                        .await?;
                }
                let stored = self
                    .stored()
                    .ok_or_else(|| ErrorObject::new(ErrorCode::NotFound, "nothing measured yet"))?;
                let root = stored.homes.get(&p.id).cloned().ok_or_else(|| {
                    ErrorObject::new(ErrorCode::NotFound, "that home was not measured")
                })?;
                serde_json::to_value(StorageTreeResult {
                    id: p.id,
                    measured_at: stored.measurement.measured_at,
                    root,
                })
                .map_err(internal)
            }
            StorageRequest::CleanupSystem(p) => {
                self.check_box_admin(uid, method, p.admin_pin.as_deref())
                    .await?;
                let spec = self.spec(
                    JobKind::CleanupSystem,
                    uid,
                    Action::Cleanup(CleanupJob {
                        target: CleanupTarget::System,
                        keep: p.keep,
                        store_dir: self.args.store_dir.clone(),
                        owners: Vec::new(),
                    }),
                );
                let spec = spec.about(system::JobSubject {
                    keep: Some(p.keep),
                    ..Default::default()
                });
                let id = self.start(spec).await?;
                info!(job = %id, keep = p.keep, "system clean-up");
                job_ref(Some(id))
            }
            StorageRequest::CleanupProfiles(p) => {
                self.check_box_admin(uid, method, p.admin_pin.as_deref())
                    .await?;
                let (state, _) = self.load_state()?;
                let spec = self.spec(
                    JobKind::CleanupProfiles,
                    uid,
                    Action::Cleanup(CleanupJob {
                        target: CleanupTarget::Profiles,
                        keep: p.keep,
                        store_dir: self.args.store_dir.clone(),
                        owners: appreg::owners(&state)
                            .into_iter()
                            .filter(appreg::Owner::has_generations)
                            .collect(),
                    }),
                );
                let spec = spec.about(system::JobSubject {
                    keep: Some(p.keep),
                    ..Default::default()
                });
                let id = self.start(spec).await?;
                info!(job = %id, keep = p.keep, "profiles clean-up");
                job_ref(Some(id))
            }
            StorageRequest::CleanupSet(p) => {
                self.check_box_admin(uid, method, p.admin_pin.as_deref())
                    .await?;
                let (state, _) = self.load_state()?;
                let wanted = CleanupState {
                    enable: p.enabled,
                    frequency: p.frequency,
                    keep_system: p.keep_system,
                    keep_profiles: p.keep_profiles,
                    extra: state.cleanup().extra,
                };
                if state.cleanup() == wanted {
                    return job_ref(None);
                }
                let mut new = state.clone();
                new.settings.cleanup = Some(wanted.clone());
                let new = self.requirements_update(&new).unwrap_or(new);
                self.preflight()?;
                let spec = self.spec(
                    JobKind::CleanupSet,
                    uid,
                    Action::Apply(job::Apply {
                        state: Some(new),
                        ..Default::default()
                    }),
                );
                let spec = spec.about(system::JobSubject {
                    enabled: Some(p.enabled),
                    frequency: Some(p.frequency),
                    keep_system: Some(p.keep_system),
                    keep_profiles: Some(p.keep_profiles),
                    ..Default::default()
                });
                let id = self.start(spec).await?;
                info!(job = %id, ?wanted, "automatic clean-up set");
                job_ref(Some(id))
            }
        }
    }

    /// What to measure: the accounts' homes in `state.json` order, then the guest's.
    fn measure_job(&self, state: &State) -> MeasureJob {
        let home_of = |user: &str| {
            self.users
                .by_name(user)
                .map(|u| u.home)
                .filter(|h| h.is_absolute() && h != std::path::Path::new("/"))
        };
        let mut homes: Vec<MeasureHome> = state
            .accounts()
            .into_iter()
            .map(|a| MeasureHome {
                home: home_of(&a.user),
                id: a.id,
            })
            .collect();
        if state.guest_enabled() {
            homes.push(MeasureHome {
                id: "guest".into(),
                home: home_of(state::GUEST_USER),
            });
        }
        MeasureJob {
            store_dir: self.args.store_dir.clone(),
            homes,
            srv_dir: self.args.srv_dir.clone(),
            owners: appreg::owners(state),
        }
    }
}
