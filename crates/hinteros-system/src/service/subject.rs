//! What a job is about (`Job.subject`), from the service's own state when the job is
//! created: catalog names, profile names, the running release.

use hinteros_api::system::{CatalogApp, JobSubject, SubjectProfile};

use super::Service;
use crate::appreg::Owner;
use crate::flake::Release;

/// Longest name kept in a subject.
const MAX_NAME: usize = 40;

impl Service {
    /// Apps with their names in the running generation's catalog (the id when unknown).
    pub(super) fn subject_apps(&self, ids: &[String]) -> Vec<CatalogApp> {
        let index = hinteros_api::catalog::Index::load(&self.args.catalog_index()).ok();
        ids.iter()
            .map(|id| CatalogApp {
                id: id.clone(),
                name: index
                    .as_ref()
                    .and_then(|i| i.get(id))
                    .map(|e| e.name.chars().take(MAX_NAME).collect::<String>())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| id.clone()),
            })
            .collect()
    }

    /// A profile with its display name now (the id when it has none).
    pub(super) fn subject_profile(&self, id: &str) -> SubjectProfile {
        let name = self
            .profiles_file()
            .profiles
            .get(id)
            .map(|m| m.name.chars().take(MAX_NAME).collect::<String>())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| id.to_owned());
        SubjectProfile {
            id: id.to_owned(),
            name,
        }
    }

    /// An apps job's owner: `owner`, and `profile` when the owner is a profile.
    pub(super) fn owner_subject(&self, owner: &Owner) -> JobSubject {
        JobSubject {
            owner: Some(owner.key()),
            profile: match owner {
                Owner::Profile { id, .. } => Some(self.subject_profile(id)),
                Owner::Starter | Owner::Guest => None,
            },
            ..Default::default()
        }
    }

    /// The running generation's release version.
    pub(super) fn running_version(&self) -> Option<String> {
        Some(Release::load(&self.policy_dir()).version).filter(|v| v != "unknown")
    }
}
