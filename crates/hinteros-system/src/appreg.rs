//! Per-profile apps (apps.md): who owns which app env, where the envs' Nix profiles are, the
//! links sessions read, the registry and the starter set.
//!
//! - A profile's apps are the root-owned Nix profile `<apps-profiles>/apps/<user>`; the starter
//!   set's is `<apps-profiles>/starter`; the guest's env is a single link
//!   `/run/hinteros/apps/hinteros-guest` (gone at reboot).
//! - `/run/hinteros/apps/<user>` links to the profile's Nix profile: the one path a session
//!   reads, whatever kind of owner it is.
//! - The env manifests are the truth; `registry.json` caches each owner's ids and box features
//!   and is rewritten when it differs from them (after a crash, or a manual `nix-env`).

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{DirBuilderExt, symlink};
use std::path::{Path, PathBuf};

use hinteros_api::manifest::Manifest;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::config::Paths;
use crate::fsutil;
use crate::generations;
use crate::state::{self, State};

pub const REGISTRY_VERSION: u32 = 1;
pub const STARTER_VERSION: u32 = 1;

/// Someone who has an app env.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Owner {
    Profile { id: String, user: String },
    Starter,
    Guest,
}

impl Owner {
    /// The registry key: the profile id, `starter` or `guest`.
    pub fn key(&self) -> String {
        match self {
            Owner::Profile { id, .. } => id.clone(),
            Owner::Starter => "starter".into(),
            Owner::Guest => "guest".into(),
        }
    }

    pub fn user(&self) -> Option<&str> {
        match self {
            Owner::Profile { user, .. } => Some(user),
            Owner::Starter => None,
            Owner::Guest => Some(state::GUEST_USER),
        }
    }

    /// The Nix profile (or, for the guest, the single link) holding the env.
    pub fn link(&self, paths: &Paths) -> PathBuf {
        match self {
            Owner::Profile { user, .. } => paths.app_profile(user),
            Owner::Starter => paths.starter_profile(),
            Owner::Guest => paths.guest_link(),
        }
    }

    /// Whether the env has generations (`nix-env -p`); the guest's link has none.
    pub fn has_generations(&self) -> bool {
        !matches!(self, Owner::Guest)
    }
}

/// Every owner on the box: the accounts of `state`, the starter set and (when enabled) the
/// guest.
pub fn owners(state: &State) -> Vec<Owner> {
    let mut out: Vec<Owner> = state
        .accounts()
        .into_iter()
        .map(|a| Owner::Profile {
            id: a.id,
            user: a.user,
        })
        .collect();
    out.push(Owner::Starter);
    if state.guest_enabled() {
        out.push(Owner::Guest);
    }
    out
}

/// An owner's current env.
#[derive(Debug, Clone, PartialEq)]
pub struct Current {
    pub generation: Option<u32>,
    /// The env's store path.
    pub env: PathBuf,
    pub manifest: Manifest,
}

/// The owner's current env, if it has one with a readable manifest.
pub fn current(owner: &Owner, paths: &Paths) -> Option<Current> {
    let link = owner.link(paths);
    let env = std::fs::canonicalize(&link).ok()?;
    let manifest = match Manifest::load(&env) {
        Ok(m) => m,
        Err(e) => {
            warn!(owner = %owner.key(), "unreadable app env: {e}");
            return None;
        }
    };
    let generation = if owner.has_generations() {
        generations::current_generation(&link)
    } else {
        None
    };
    Some(Current {
        generation,
        env,
        manifest,
    })
}

/// The caller's app `revision` (64 hex): stands for its current generation and env.
pub fn revision(current: Option<&Current>) -> String {
    let text = match current {
        Some(c) => format!(
            "{}:{}",
            c.generation.map(|g| g.to_string()).unwrap_or_default(),
            c.env.display()
        ),
        None => "none".into(),
    };
    state::hex(&Sha256::digest(text.as_bytes()))
}

/// One generation of an owner's Nix profile.
#[derive(Debug, Clone, PartialEq)]
pub struct AppGen {
    pub generation: u32,
    pub date: i64,
    pub env: PathBuf,
    pub manifest: Option<Manifest>,
    pub current: bool,
}

/// The owner's generations, newest first.
pub fn generations(owner: &Owner, paths: &Paths) -> Vec<AppGen> {
    if !owner.has_generations() {
        return Vec::new();
    }
    let link = owner.link(paths);
    let current = generations::current_generation(&link);
    generations::links(&link)
        .into_iter()
        .filter_map(|l| {
            let env = l.toplevel?;
            Some(AppGen {
                generation: l.generation,
                date: l.date,
                manifest: Manifest::load(&env).ok(),
                env,
                current: Some(l.generation) == current,
            })
        })
        .collect()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u32>,
    pub ids: Vec<String>,
    #[serde(default, rename = "box")]
    pub box_features: Vec<String>,
}

/// `registry.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    pub version: u32,
    pub owners: BTreeMap<String, OwnerEntry>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            version: REGISTRY_VERSION,
            owners: BTreeMap::new(),
        }
    }
}

impl Registry {
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        fsutil::write_atomic(path, &bytes, 0o600)
    }

    /// The registry as the env manifests say it is.
    pub fn scan(paths: &Paths, state: &State) -> Self {
        let mut r = Self::default();
        for owner in owners(state) {
            if let Some(c) = current(&owner, paths) {
                r.owners.insert(
                    owner.key(),
                    OwnerEntry {
                        user: owner.user().map(str::to_owned),
                        generation: c.generation,
                        ids: c.manifest.ids,
                        box_features: c.manifest.box_features,
                    },
                );
            }
        }
        r
    }

    /// Rewrite the file if it differs from the manifests; returns the truth.
    pub fn reconcile(paths: &Paths, state: &State) -> Self {
        let truth = Self::scan(paths, state);
        let path = paths.registry();
        if Self::load(&path) != truth || !path.exists() {
            if path.exists() {
                info!("the app registry differed from the env manifests; rewriting it");
            }
            if let Err(e) = truth.save(&path) {
                warn!("cannot write {}: {e}", path.display());
            }
        }
        truth
    }

    /// Box features some owner's current env needs.
    pub fn needed_features(&self) -> BTreeSet<String> {
        self.owners
            .values()
            .flat_map(|o| o.box_features.iter().cloned())
            .collect()
    }
}

/// Make `/run/hinteros/apps/<user>` point at each account's Nix profile (and drop links of users
/// that are no longer accounts; the guest's own link stays).
pub fn publish(paths: &Paths, state: &State) {
    let dir = paths.apps_run_dir();
    if let Err(e) = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(&dir)
    {
        warn!("cannot create {}: {e}", dir.display());
        return;
    }
    let accounts = state.accounts();
    for a in &accounts {
        let link = dir.join(&a.user);
        let target = paths.app_profile(&a.user);
        if std::fs::read_link(&link).ok().as_deref() == Some(target.as_path()) {
            continue;
        }
        let tmp = dir.join(format!(".{}.tmp", a.user));
        let _ = std::fs::remove_file(&tmp);
        let made = symlink(&target, &tmp).and_then(|()| std::fs::rename(&tmp, &link));
        if let Err(e) = made {
            warn!("cannot publish {}: {e}", link.display());
        }
    }
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.')
                || name == state::GUEST_USER
                || accounts.iter().any(|a| a.user == name)
            {
                continue;
            }
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// `starter.json`: the starter set's ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StarterFile {
    pub version: u32,
    pub ids: Vec<String>,
    #[serde(default)]
    pub updated_at: Option<i64>,
    #[serde(default)]
    pub by: Option<String>,
}

impl StarterFile {
    pub fn load(path: &Path) -> Option<Self> {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        fsutil::write_atomic(path, &bytes, 0o600)
    }
}

/// Sorted, unique ids.
pub fn normalize(ids: impl IntoIterator<Item = String>) -> Vec<String> {
    let set: BTreeSet<String> = ids.into_iter().collect();
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::users::Users;

    pub fn env(store: &Path, ids: &[&str], features: &[&str]) -> PathBuf {
        let name = if ids.is_empty() {
            "empty".to_string()
        } else {
            ids.join("+")
        };
        let out = store.join(format!("env-{name}"));
        std::fs::create_dir_all(out.join("share/hinteros")).unwrap();
        std::fs::write(
            out.join("share/hinteros/apps.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": 1, "ids": ids, "apps": [], "box": features
            }))
            .unwrap(),
        )
        .unwrap();
        out
    }

    fn paths(root: &Path) -> Paths {
        Paths {
            config_dir: root.join("etc"),
            state_dir: root.join("var"),
            run_dir: root.join("run"),
            profile: root.join("profiles/system"),
            current_system: root.join("current"),
            booted_system: root.join("booted"),
            apps_profiles: root.join("nixprofiles"),
            catalog_index: None,
            users: Users::default(),
        }
    }

    fn set(profile: &Path, generation: u32, env: &Path) {
        std::fs::create_dir_all(profile.parent().unwrap()).unwrap();
        let name = profile.file_name().unwrap().to_string_lossy().into_owned();
        let link = profile.with_file_name(format!("{name}-{generation}-link"));
        symlink(env, &link).unwrap();
        let _ = std::fs::remove_file(profile);
        symlink(link.file_name().unwrap(), profile).unwrap();
    }

    #[test]
    fn registry_follows_the_manifests_and_links_are_published() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        let state = State::parse(
            br#"{"schemaVersion":2,"requirements":[],"services":[],
               "profiles":[{"id":"p1","user":"hinteros"},{"id":"p3","user":"hinteros-p3"}]}"#,
        )
        .unwrap();
        let store = d.path().join("store");
        let a = env(
            &store,
            &["steam", "youtube"],
            &["graphics32", "steam-devices"],
        );
        let b = env(&store, &["youtube"], &[]);
        set(&p.app_profile("hinteros"), 1, &b);
        set(&p.app_profile("hinteros"), 2, &a);
        set(&p.starter_profile(), 1, &b);
        // A stale registry is rewritten from the manifests.
        Registry {
            version: 1,
            owners: BTreeMap::from([("p9".into(), OwnerEntry::default())]),
        }
        .save(&p.registry())
        .unwrap();
        let r = Registry::reconcile(&p, &state);
        assert_eq!(Registry::load(&p.registry()), r);
        assert_eq!(r.owners["p1"].generation, Some(2));
        assert_eq!(r.owners["p1"].ids, ["steam", "youtube"]);
        assert_eq!(r.owners["starter"].ids, ["youtube"]);
        assert!(!r.owners.contains_key("p3") && !r.owners.contains_key("p9"));
        assert_eq!(
            r.needed_features().into_iter().collect::<Vec<_>>(),
            ["graphics32", "steam-devices"]
        );
        let gens = generations(
            &Owner::Profile {
                id: "p1".into(),
                user: "hinteros".into(),
            },
            &p,
        );
        assert_eq!(gens.len(), 2);
        assert!(gens[0].current && gens[0].generation == 2);
        assert_eq!(gens[1].manifest.as_ref().unwrap().ids, ["youtube"]);

        std::fs::create_dir_all(p.apps_run_dir()).unwrap();
        symlink("/x", p.apps_run_dir().join("hinteros-p7")).unwrap();
        publish(&p, &state);
        assert_eq!(
            std::fs::read_link(p.apps_run_dir().join("hinteros")).unwrap(),
            p.app_profile("hinteros")
        );
        assert_eq!(
            std::fs::read_link(p.apps_run_dir().join("hinteros-p3")).unwrap(),
            p.app_profile("hinteros-p3")
        );
        assert!(!p.apps_run_dir().join("hinteros-p7").exists());
        // The session sees the env through the published link.
        assert_eq!(
            Manifest::load(&p.apps_run_dir().join("hinteros"))
                .unwrap()
                .ids,
            ["steam", "youtube"]
        );
        let c = current(
            &Owner::Profile {
                id: "p1".into(),
                user: "hinteros".into(),
            },
            &p,
        );
        assert_ne!(revision(c.as_ref()), revision(None));
        assert_eq!(revision(None).len(), 64);
    }
}
