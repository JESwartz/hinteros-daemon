//! What the service publishes for the greeter (profiles.md): `/run/hinteros/profiles-public.json`
//! (0644; the greeter is not a `hinteros` member) and the switch hint `/run/hinteros/switch.json`
//! (0600: target, origin, expiry and a one-time token that lets `hinteros-system pin unlocked`
//! enter a profile without its PIN: a resume after `session.restart`, a just-created profile).

use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::config::Paths;
use crate::fsutil;
use crate::pins::Pins;
use crate::profiles::ProfilesFile;
use crate::state::{self, State};

/// How long a hint and a token are good for.
pub const HINT_TTL_S: i64 = 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    pub user: String,
    pub expires: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchHint {
    /// A profile id or `guest`.
    pub target: String,
    pub from: Option<String>,
    pub expires: i64,
    #[serde(default)]
    pub token: Option<Token>,
}

impl SwitchHint {
    pub fn load(path: &Path) -> Option<Self> {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        fsutil::write_atomic(path, &serde_json::to_vec(self)?, 0o600)
    }

    /// The live hint at `now`.
    pub fn current(path: &Path, now: i64) -> Option<Self> {
        Self::load(path).filter(|h| h.expires > now)
    }
}

/// Write a one-time token for `user` (a new profile from the greeter, a resume).
pub fn grant_token(paths: &Paths, target: &str, user: &str, now: i64) -> std::io::Result<()> {
    SwitchHint {
        target: target.into(),
        from: None,
        expires: now + HINT_TTL_S,
        token: Some(Token {
            user: user.into(),
            expires: now + HINT_TTL_S,
        }),
    }
    .save(&paths.switch_json())
}

/// Use up `user`'s token, if it has a valid one.
pub fn take_token(path: &Path, user: &str, now: i64) -> bool {
    let Some(mut hint) = SwitchHint::load(path) else {
        return false;
    };
    let valid = hint
        .token
        .as_ref()
        .is_some_and(|t| t.user == user && t.expires > now);
    if valid {
        hint.token = None;
        if let Err(e) = hint.save(path) {
            warn!("cannot use up the token: {e}");
            return false;
        }
    }
    valid
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublicProfile {
    pub id: String,
    pub user: String,
    pub name: String,
    pub color: String,
    pub locked: bool,
    pub admin: bool,
    pub locked_until: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublicGuest {
    pub enabled: bool,
    pub user: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Next {
    pub target: String,
    /// The greeter may enter without asking (unlocked, guest, or a token).
    pub fast: bool,
    pub expires: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublicView {
    pub version: u32,
    pub profiles: Vec<PublicProfile>,
    pub guest: PublicGuest,
    pub first_boot: bool,
    pub spare: Option<String>,
    pub last_used: Option<String>,
    pub next: Option<Next>,
    pub appearance: crate::profiles::Appearance,
}

/// The public view of the box now.
pub fn view(paths: &Paths, state: &State, file: &ProfilesFile, now: i64) -> PublicView {
    let pins = Pins::new(paths.pins_dir());
    let visible = file.visible(state);
    let profiles: Vec<PublicProfile> = visible
        .iter()
        .map(|(a, m)| PublicProfile {
            id: a.id.clone(),
            user: a.user.clone(),
            name: m.name.clone(),
            color: m.color.clone(),
            locked: m.locked && pins.is_set(&a.id),
            admin: m.admin,
            locked_until: Some(pins.backoff(&a.id).locked_until).filter(|t| *t > now),
        })
        .collect();
    let mut appearance = crate::profiles::Appearance::default();
    for (_, m) in &visible {
        appearance.text_scale = appearance.text_scale.max(m.appearance.text_scale);
        appearance.high_contrast |= m.appearance.high_contrast;
        appearance.reduce_motion |= m.appearance.reduce_motion;
    }
    let next = SwitchHint::current(&paths.switch_json(), now).map(|h| {
        let fast = h.target == "guest"
            || profiles.iter().any(|p| {
                p.id == h.target
                    && (!p.locked || h.token.as_ref().is_some_and(|t| t.user == p.user))
            });
        Next {
            target: h.target,
            fast,
            expires: h.expires,
        }
    });
    let last_used = visible
        .iter()
        .filter_map(|(a, m)| m.last_used_at.map(|t| (t, a.id.clone())))
        .max()
        .map(|(_, id)| id);
    PublicView {
        version: 1,
        profiles,
        guest: PublicGuest {
            enabled: state.guest_enabled(),
            user: state::GUEST_USER.into(),
        },
        first_boot: file.first_boot(state),
        spare: file.spares(state).first().map(|a| a.user.clone()),
        last_used,
        next,
        appearance,
    }
}

/// Rewrite `profiles-public.json` (best effort: the greeter falls back to its last copy).
pub fn publish(paths: &Paths, state: &State, file: &ProfilesFile, now: i64) {
    let v = view(paths, state, file, now);
    let bytes = match serde_json::to_vec_pretty(&v) {
        Ok(b) => b,
        Err(e) => {
            warn!("public view: {e}");
            return;
        }
    };
    if let Err(e) = fsutil::write_atomic(&paths.public_json(), &bytes, 0o644) {
        warn!("cannot write {}: {e}", paths.public_json().display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::{Meta, migrated_file};

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
            users: Default::default(),
            proc_dir: "/proc".into(),
        }
    }

    #[test]
    fn view_hint_and_token() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        std::fs::create_dir_all(&p.run_dir).unwrap();
        let state = State::parse(
            br#"{"schemaVersion":2,"requirements":[],"services":[],
               "profiles":[{"id":"p1","user":"hinteros"},{"id":"p2","user":"hinteros-p2"},{"id":"p3","user":"hinteros-p3"}]}"#,
        )
        .unwrap();
        let mut file = migrated_file(&state, 0);
        file.profiles.remove("p3");
        let p2: &mut Meta = file.profiles.get_mut("p2").unwrap();
        p2.locked = true;
        p2.admin = false;
        p2.last_used_at = Some(50);
        p2.appearance.text_scale = 1.5;
        Pins::new(p.pins_dir()).set("p2", "2468").unwrap();
        let v = view(&p, &state, &file, 100);
        assert_eq!(v.profiles.len(), 2);
        assert!(v.profiles[1].locked && !v.profiles[0].locked);
        assert_eq!(v.spare.as_deref(), Some("hinteros-p3"));
        assert!(!v.first_boot);
        assert_eq!(v.last_used.as_deref(), Some("p2"));
        assert_eq!(v.appearance.text_scale, 1.5);
        assert!(v.next.is_none() && v.guest.enabled);

        // A hint to locked p2: the picker opens on its PIN pad; with a token, the fast path.
        SwitchHint {
            target: "p2".into(),
            from: Some("p1".into()),
            expires: 160,
            token: None,
        }
        .save(&p.switch_json())
        .unwrap();
        assert!(!view(&p, &state, &file, 100).next.unwrap().fast);
        grant_token(&p, "p2", "hinteros-p2", 100).unwrap();
        assert!(view(&p, &state, &file, 100).next.unwrap().fast);
        assert!(!take_token(&p.switch_json(), "hinteros", 100));
        assert!(!take_token(&p.switch_json(), "hinteros-p2", 500), "expired");
        assert!(take_token(&p.switch_json(), "hinteros-p2", 100));
        assert!(!take_token(&p.switch_json(), "hinteros-p2", 100), "used up");
        // Expired hints are gone from the view.
        assert!(view(&p, &state, &file, 1000).next.is_none());
        publish(&p, &state, &file, 100);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(p.public_json())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o644);
        assert_eq!(
            std::fs::metadata(p.switch_json())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
