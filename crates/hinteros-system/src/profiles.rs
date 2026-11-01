//! Profiles (profiles.md): `profiles.json` (names, colours, lock flags, the admin flag, the id
//! counter) and who a caller is.
//!
//! An **account** is a `state.json` profile entry (a NixOS user); it is a **profile** once
//! `profiles.json` has its metadata, otherwise a **spare**. The guest (`hinteros-guest`) is
//! neither. `profiles.json` is root-only, written atomically, and never rolled back (names and
//! the counter survive a system rollback).

use std::collections::BTreeMap;
use std::path::Path;

use hinteros_api::common::{check_file_version, check_version};
use hinteros_api::{ErrorCode, ErrorObject};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::fsutil;
use crate::state::{self, Account, State};
use crate::users::Users;

pub const FILE_VERSION: u32 = 1;

/// Profile colours (a closed set; the launcher draws them).
pub const COLORS: &[&str] = &[
    "teal", "blue", "purple", "pink", "red", "orange", "yellow", "green",
];

pub fn is_color(s: &str) -> bool {
    COLORS.contains(&s)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Appearance {
    pub text_scale: f64,
    pub high_contrast: bool,
    pub reduce_motion: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            text_scale: 1.0,
            high_contrast: false,
            reduce_motion: false,
            extra: BTreeMap::new(),
        }
    }
}

/// One profile's metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub name: String,
    pub color: String,
    #[serde(default)]
    pub admin: bool,
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub last_used_at: Option<i64>,
    #[serde(default)]
    pub appearance: Appearance,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfilesFile {
    pub version: u32,
    /// The next profile number (never goes back).
    pub next: u32,
    pub profiles: BTreeMap<String, Meta>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Default for ProfilesFile {
    fn default() -> Self {
        Self {
            version: FILE_VERSION,
            next: 1,
            profiles: BTreeMap::new(),
            extra: BTreeMap::new(),
        }
    }
}

impl ProfilesFile {
    /// Read it (a missing file is an empty one). A damaged file or a newer one is an error:
    /// nothing must overwrite it blindly.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => {
                check_version(path, &bytes, FILE_VERSION)?;
                serde_json::from_slice(&bytes).map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("{}: {e}", path.display()),
                    )
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    pub fn exists(path: &Path) -> bool {
        path.exists()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        check_file_version(path, FILE_VERSION)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        fsutil::write_atomic(path, &bytes, 0o600)
    }

    /// The metadata of the accounts in `state` that are profiles (other ids are kept in the
    /// file and ignored: a rollback can bring their accounts back).
    pub fn visible<'a>(&'a self, state: &State) -> Vec<(Account, &'a Meta)> {
        state
            .accounts()
            .into_iter()
            .filter_map(|a| self.profiles.get(&a.id).map(|m| (a, m)))
            .collect()
    }

    /// The admin profile among the running accounts.
    pub fn admin(&self, state: &State) -> Option<(Account, &Meta)> {
        self.visible(state).into_iter().find(|(_, m)| m.admin)
    }

    /// No admin profile exists: first-boot mode.
    pub fn first_boot(&self, state: &State) -> bool {
        self.admin(state).is_none()
    }

    /// The accounts without metadata.
    pub fn spares(&self, state: &State) -> Vec<Account> {
        state
            .accounts()
            .into_iter()
            .filter(|a| !self.profiles.contains_key(&a.id))
            .collect()
    }

    /// Take the next profile id: never below one plus the highest number in `state.json`,
    /// `profiles.json` or the user database, and the counter only increases.
    pub fn take_id(&mut self, state: &State, users: &Users) -> String {
        let mut n = self.next.max(1);
        let highest = state
            .accounts()
            .iter()
            .filter_map(|a| state::profile_number(&a.id))
            .chain(
                self.profiles
                    .keys()
                    .filter_map(|id| state::profile_number(id)),
            )
            .chain(users.profile_numbers())
            .max()
            .unwrap_or(0);
        n = n.max(highest + 1);
        self.next = n + 1;
        format!("p{n}")
    }
}

/// Display names: trimmed, 1 to 32 characters, no control or format characters.
pub fn check_name(name: &str) -> Result<String, ErrorObject> {
    let t = name.trim();
    let n = t.chars().count();
    if n == 0 || n > 32 || t.chars().any(|c| c.is_control() || is_format_char(c)) {
        return Err(ErrorObject::invalid_field(
            "name",
            "bad_name",
            "names are 1 to 32 characters, without control characters",
        ));
    }
    Ok(t.to_owned())
}

/// Unicode general category Cf (format characters: bidi controls, zero-width characters…).
fn is_format_char(c: char) -> bool {
    matches!(c as u32,
        0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x0890..=0x0891 | 0x08E2
        | 0x180E | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F
        | 0xFEFF | 0xFFF9..=0xFFFB | 0x110BD | 0x110CD | 0x13430..=0x1343F
        | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F)
}

pub fn check_color(color: &str) -> Result<(), ErrorObject> {
    if is_color(color) {
        Ok(())
    } else {
        Err(ErrorObject::invalid_field(
            "color",
            "bad_color",
            format!("color must be one of {}", COLORS.join(", ")),
        ))
    }
}

/// What a caller is to the profile model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// An account with metadata.
    Profile,
    /// An account without metadata.
    Spare,
    /// The guest account (while it is enabled).
    Guest,
    /// Anyone else (root, the admin account, a system user).
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub uid: u32,
    pub user: Option<crate::users::User>,
    /// The account's profile id (profiles and spares).
    pub id: Option<String>,
    pub role: Role,
}

impl Caller {
    /// The app owner key (`p<N>` or `guest`), for profiles, spares' none.
    pub fn owner_key(&self) -> Option<String> {
        match self.role {
            Role::Profile => self.id.clone(),
            Role::Guest => Some("guest".into()),
            _ => None,
        }
    }

    pub fn user_name(&self) -> String {
        self.user
            .as_ref()
            .map_or_else(|| self.uid.to_string(), |u| u.name.clone())
    }
}

/// Resolve a caller from its uid (`SO_PEERCRED`): an account of the running `state.json`, or
/// the guest.
pub fn resolve(uid: u32, state: &State, file: &ProfilesFile, users: &Users) -> Caller {
    let user = users.by_uid(uid);
    let name = user.as_ref().map(|u| u.name.clone());
    if let Some(n) = &name {
        if let Some(a) = state.accounts().into_iter().find(|a| &a.user == n) {
            let role = if file.profiles.contains_key(&a.id) {
                Role::Profile
            } else {
                Role::Spare
            };
            return Caller {
                uid,
                user,
                id: Some(a.id),
                role,
            };
        }
        if n == state::GUEST_USER && state.guest_enabled() {
            return Caller {
                uid,
                user,
                id: None,
                role: Role::Guest,
            };
        }
    }
    Caller {
        uid,
        user,
        id: None,
        role: Role::Other,
    }
}

/// `permission_denied {action, reason: profile}`: the caller is not a profile (a spare, the
/// admin account, root…).
pub fn not_a_profile(action: &str) -> ErrorObject {
    ErrorObject::with_data(
        ErrorCode::PermissionDenied,
        "only a profile can do this",
        json!({ "action": action, "reason": "profile" }),
    )
}

/// For tests: every account is a profile, unlocked, without a PIN; the first is the admin.
#[cfg(test)]
pub fn test_file(state: &State) -> ProfilesFile {
    let mut f = ProfilesFile::default();
    for (i, a) in state.accounts().into_iter().enumerate() {
        f.next = state::profile_number(&a.id).unwrap_or_default() + 1;
        let meta = Meta {
            name: format!("Profile {}", i + 1),
            color: COLORS[0].into(),
            admin: i == 0,
            locked: false,
            created_at: 0,
            last_used_at: None,
            appearance: Appearance::default(),
            extra: BTreeMap::new(),
        };
        f.profiles.insert(a.id, meta);
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(json: &str) -> State {
        State::parse(json.as_bytes()).unwrap()
    }

    #[test]
    fn names() {
        assert_eq!(check_name("  Sam ").unwrap(), "Sam");
        assert_eq!(check_name("Zoë 🎮").unwrap(), "Zoë 🎮");
        for bad in [
            "",
            "   ",
            &"x".repeat(33),
            "a\nb",
            "a\u{202e}b",
            "a\u{200b}",
        ] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
        assert!(check_name(&"é".repeat(32)).is_ok());
    }

    #[test]
    fn counter_never_goes_back() {
        let state = st(
            r#"{"schemaVersion":2,"base":"unstable","stateVersion":"26.11","requirements":[],"services":[],
               "profiles":[{"id":"p1"},{"id":"p4"}]}"#,
        );
        let d = tempfile::tempdir().unwrap();
        let passwd = d.path().join("passwd");
        std::fs::write(
            &passwd,
            "hinteros-p6:x:20006:100::/home/hinteros-p6:/bin/sh\n",
        )
        .unwrap();
        let users = Users {
            passwd: Some(passwd),
            group: None,
        };
        let mut f = ProfilesFile::default();
        assert_eq!(f.take_id(&state, &users), "p7", "above passwd's p6");
        assert_eq!(f.take_id(&state, &users), "p8");
        let mut f = ProfilesFile {
            next: 20,
            ..Default::default()
        };
        assert_eq!(f.take_id(&state, &users), "p20");
        let mut f = ProfilesFile::default();
        f.profiles
            .insert("p9".into(), test_file(&state).profiles["p1"].clone());
        assert_eq!(f.take_id(&state, &Users::default()), "p10");
    }

    #[test]
    fn a_newer_file_is_refused_and_kept() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("profiles.json");
        let newer = r#"{"version":2,"next":1,"profiles":{}}"#;
        std::fs::write(&path, newer).unwrap();
        let e = ProfilesFile::load(&path).unwrap_err();
        assert!(e.to_string().contains("has version 2"), "{e}");
        assert!(ProfilesFile::default().save(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
    }

    #[test]
    fn unknown_fields_are_kept() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("profiles.json");
        std::fs::write(
            &path,
            r#"{"version":1,"next":2,"later":1,"profiles":{"p1":{"name":"A","color":"teal","mood":"ok",
                "appearance":{"text_scale":1.0,"high_contrast":false,"reduce_motion":false,"zoom":2}}}}"#,
        )
        .unwrap();
        ProfilesFile::load(&path).unwrap().save(&path).unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["later"], 1);
        assert_eq!(v["profiles"]["p1"]["mood"], "ok");
        assert_eq!(v["profiles"]["p1"]["appearance"]["zoom"], 2);
    }
    #[test]
    fn callers() {
        let d = tempfile::tempdir().unwrap();
        let passwd = d.path().join("passwd");
        std::fs::write(
            &passwd,
            "hinteros-p1:x:1000:100::/home/hinteros-p1:/bin/sh\n\
             hinteros-p2:x:20002:100::/home/hinteros-p2:/bin/sh\n\
             hinteros-guest:x:20000:100::/home/hinteros-guest:/bin/sh\n\
             admin:x:1001:100::/home/admin:/bin/sh\n",
        )
        .unwrap();
        let users = Users {
            passwd: Some(passwd),
            group: None,
        };
        let state = st(
            r#"{"schemaVersion":2,"base":"unstable","stateVersion":"26.11","requirements":[],"services":[],
               "profiles":[{"id":"p1"},{"id":"p2"}]}"#,
        );
        let mut f = ProfilesFile::default();
        f.profiles
            .insert("p1".into(), test_file(&state).profiles["p1"].clone());
        let c = resolve(1000, &state, &f, &users);
        assert_eq!((&c.role, c.id.as_deref()), (&Role::Profile, Some("p1")));
        assert_eq!(c.owner_key().as_deref(), Some("p1"));
        let c = resolve(20002, &state, &f, &users);
        assert_eq!(c.role, Role::Spare);
        assert_eq!(c.owner_key(), None);
        let c = resolve(20000, &state, &f, &users);
        assert_eq!(c.role, Role::Guest);
        assert_eq!(c.owner_key().as_deref(), Some("guest"));
        assert_eq!(resolve(1001, &state, &f, &users).role, Role::Other);
        assert_eq!(resolve(4242, &state, &f, &users).role, Role::Other);
    }
}
