//! `state.json`: the typed device state, mirroring the system repo's `state.nix` allowlist.
//!
//! The service reads the file into [`State`] (unknown fields are an error, as in `state.nix`),
//! changes it through typed operations only, and writes it back from the struct with
//! [`State::to_canonical_json`]. Client JSON never reaches the file.
//!
//! Two schemas are read (device-config.md, apps.md "Migration"):
//! - **1** (before M7): box-wide `apps`, `profiles: [{id, name, user}]` (`[]` means p1 =
//!   `hinteros`).
//! - **2** (M7): no `apps` (apps are per profile, in Nix profiles), `requirements` (box
//!   features), `services`, `profiles: [{id, user}]` (the accounts; names live in
//!   `profiles.json`), `settings.guest.enable`.

use std::path::Path;

use hinteros_api::system::{Base, CleanupFrequency, MAX_KEEP};
use hinteros_api::{ErrorCode, ErrorObject};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The schema the service writes when it migrates (and for every new state).
pub const SCHEMA_VERSION: u32 = 2;

/// Schemas the service reads.
pub const SCHEMA_VERSIONS: &[u32] = &[1, 2];

/// Most accounts `state.json` may declare (the guest does not count).
pub const MAX_PROFILES: usize = 8;

/// The first account's user on every box.
pub const P1_USER: &str = "hinteros";

/// The guest account.
pub const GUEST_USER: &str = "hinteros-guest";

/// The guest account's uid (profiles.nix).
pub const GUEST_UID: u32 = 20000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct State {
    pub schema_version: u32,
    #[serde(default = "default_base")]
    pub base: Base,
    /// NixOS `system.stateVersion`, installer-written; never settable through the API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_version: Option<String>,
    #[serde(default)]
    pub settings: StateSettings,
    /// Disk layout, installer-written; never settable through the API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<DiskState>,
    /// Schema 1: catalog app ids installed box-wide from the Store (M8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps: Option<Vec<String>>,
    /// Schema 2: box features that are on (apps.md "Box requirements").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirements: Option<Vec<String>>,
    /// Schema 2: enabled services (M10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub services: Option<Vec<String>>,
    /// The accounts (schema 2: `{id, user}`; schema 1 also had `name`).
    #[serde(default)]
    pub profiles: Vec<Profile>,
}

fn default_base() -> Base {
    Base::Unstable
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateSettings {
    /// Installer-written; not settable through `settings.set`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default)]
    pub ssh: SshState,
    /// The admin account (installer-written; not settable through `settings.set`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin: Option<AdminState>,
    /// Schema 2: the guest account (`guest.set`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest: Option<GuestState>,
    /// The automatic clean-up (`cleanup.set`; storage.md). Absent: [`CleanupState::default`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup: Option<CleanupState>,
    /// The box's time zone, an IANA name (`time_zone.set`; time-zone.md). Absent: `UTC`. Set
    /// in place by the service, without a rebuild: the system reads it at activation time.
    #[serde(default, rename = "timeZone", skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
}

/// `settings.cleanup` (storage.md): every field optional, these defaults (the policy before the
/// setting existed: weekly, 5 system generations).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CleanupState {
    #[serde(default = "yes")]
    pub enable: bool,
    #[serde(default = "default_frequency")]
    pub frequency: CleanupFrequency,
    #[serde(default = "default_keep")]
    pub keep_system: u32,
    #[serde(default = "default_keep")]
    pub keep_profiles: u32,
}

impl Default for CleanupState {
    fn default() -> Self {
        Self {
            enable: true,
            frequency: default_frequency(),
            keep_system: default_keep(),
            keep_profiles: default_keep(),
        }
    }
}

fn default_frequency() -> CleanupFrequency {
    CleanupFrequency::Weekly
}

fn default_keep() -> u32 {
    5
}

impl CleanupState {
    pub fn validate(&self) -> Result<(), String> {
        for (name, n) in [
            ("keepSystem", self.keep_system),
            ("keepProfiles", self.keep_profiles),
        ] {
            if !(1..=MAX_KEEP).contains(&n) {
                return Err(format!("settings.cleanup.{name} must be 1 to {MAX_KEEP}"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestState {
    #[serde(default = "yes")]
    pub enable: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshState {
    #[serde(default)]
    pub enable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminState {
    pub user: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskState {
    pub layout: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub id: String,
    /// Schema 1 only (names live in `profiles.json` from schema 2 on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub user: String,
}

/// An account: a `state.json` profile entry (schema 1's `[]` is p1 = `hinteros`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub user: String,
}

/// `^p[1-9][0-9]{0,2}$`: a profile id.
pub fn is_profile_id(s: &str) -> bool {
    let b = s.as_bytes();
    (2..=4).contains(&b.len())
        && b[0] == b'p'
        && (b'1'..=b'9').contains(&b[1])
        && b[2..].iter().all(u8::is_ascii_digit)
}

/// The number of a profile id (`p12` → 12).
pub fn profile_number(id: &str) -> Option<u32> {
    is_profile_id(id).then(|| id[1..].parse().ok()).flatten()
}

/// The user name of profile `p<N>`: `hinteros` for p1, `hinteros-p<N>` otherwise.
pub fn user_for(id: &str) -> String {
    if id == "p1" {
        P1_USER.into()
    } else {
        format!("hinteros-{id}")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("reading {0}: {1}")]
    Io(String, std::io::Error),
    #[error("state.json is invalid: {0}")]
    Invalid(String),
}

/// Turn a serde error into [`StateError`]; an unknown field is logged by name, since it means the
/// daemon and the system repo (`state.nix`) disagree about the schema.
fn explain(e: serde_json::Error) -> StateError {
    let msg = e.to_string();
    let field = msg
        .split_once("unknown field `")
        .and_then(|(_, rest)| rest.split_once('`'))
        .map(|(f, _)| printable(f));
    if let Some(field) = field {
        tracing::error!(
            field = %field,
            "state.json has a field this daemon does not know; the daemon and the system repo \
             are out of sync (update the daemon, or the installer wrote a newer schema)"
        );
        return StateError::Invalid(format!(
            "{msg} (daemon and system repo are out of sync: unknown field {field:?})"
        ));
    }
    StateError::Invalid(msg)
}

impl State {
    pub fn parse(bytes: &[u8]) -> Result<Self, StateError> {
        let state: State = serde_json::from_slice(bytes).map_err(explain)?;
        state.validate().map_err(StateError::Invalid)?;
        Ok(state)
    }

    /// Read `path`; returns the state and the file's revision.
    pub fn load(path: &Path) -> Result<(Self, String), StateError> {
        let bytes =
            std::fs::read(path).map_err(|e| StateError::Io(path.display().to_string(), e))?;
        Ok((Self::parse(&bytes)?, revision(&bytes)))
    }

    /// Invariants the service relies on (and `state.nix` checks again).
    pub fn validate(&self) -> Result<(), String> {
        if let Some(c) = &self.settings.cleanup {
            c.validate()?;
        }
        if let Some(z) = &self.settings.time_zone
            && !hinteros_api::system::is_zone_name(z)
        {
            return Err(format!(
                "settings.timeZone {:?} is not a time zone name",
                printable(z)
            ));
        }
        match self.schema_version {
            1 => self.validate_1(),
            2 => self.validate_2(),
            v => Err(format!(
                "unsupported schemaVersion {v} (supported: {SCHEMA_VERSIONS:?})"
            )),
        }
    }

    fn validate_2(&self) -> Result<(), String> {
        if self.apps.is_some() {
            return Err("schema 2 has no apps (apps are per profile)".into());
        }
        let requirements = self
            .requirements
            .as_ref()
            .ok_or("schema 2 needs requirements")?;
        let mut seen = std::collections::BTreeSet::new();
        for f in requirements {
            if !hinteros_api::catalog::is_box_feature(f) || !seen.insert(f) {
                return Err(format!("bad requirement {f:?}"));
            }
        }
        let services = self.services.as_ref().ok_or("schema 2 needs services")?;
        for s in services {
            if !is_app_id(s) {
                return Err(format!("bad service id {s:?}"));
            }
        }
        if self.profiles.is_empty() || self.profiles.len() > MAX_PROFILES {
            return Err(format!("1 to {MAX_PROFILES} profiles"));
        }
        let mut ids = std::collections::BTreeSet::new();
        for p in &self.profiles {
            if !is_profile_id(&p.id) || p.user != user_for(&p.id) || p.name.is_some() {
                return Err(format!("bad profile {:?}", p.id));
            }
            if !ids.insert(&p.id) {
                return Err(format!("duplicate profile {:?}", p.id));
            }
        }
        Ok(())
    }

    fn validate_1(&self) -> Result<(), String> {
        if self.requirements.is_some() || self.services.is_some() || self.settings.guest.is_some() {
            return Err("requirements, services and settings.guest need schema 2".into());
        }
        for app in self.apps.iter().flatten() {
            if !is_app_id(app) {
                return Err(format!("bad app id {app:?}"));
            }
        }
        for p in &self.profiles {
            let id_ok = !p.id.is_empty()
                && p.id.len() <= 32
                && p.id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            let user_ok = !p.user.is_empty()
                && p.user.len() <= 32
                && p.user.bytes().enumerate().all(|(i, b)| {
                    b.is_ascii_lowercase()
                        || b == b'_'
                        || (i > 0 && (b.is_ascii_digit() || b == b'-'))
                });
            let name_ok = p.name.as_ref().is_some_and(|n| {
                (1..=64).contains(&n.chars().count()) && !n.chars().any(char::is_control)
            });
            if !(id_ok && user_ok && name_ok) {
                return Err(format!("bad profile {:?}", p.id));
            }
        }
        Ok(())
    }

    pub fn is_schema2(&self) -> bool {
        self.schema_version >= 2
    }

    /// Schema 1's box-wide apps (empty on schema 2).
    pub fn apps(&self) -> &[String] {
        self.apps.as_deref().unwrap_or(&[])
    }

    /// Schema 2's enabled box features (empty on schema 1).
    pub fn requirements(&self) -> &[String] {
        self.requirements.as_deref().unwrap_or(&[])
    }

    /// The accounts this state declares (schema 1's `[]` is p1 = `hinteros`).
    pub fn accounts(&self) -> Vec<Account> {
        if self.profiles.is_empty() {
            return vec![Account {
                id: "p1".into(),
                user: P1_USER.into(),
            }];
        }
        self.profiles
            .iter()
            .map(|p| Account {
                id: p.id.clone(),
                user: p.user.clone(),
            })
            .collect()
    }

    /// The automatic clean-up setting (defaults when absent).
    pub fn cleanup(&self) -> CleanupState {
        self.settings.cleanup.unwrap_or_default()
    }

    /// The time zone `state.json` names (`UTC` when absent). The system's tzdata may not have
    /// it; then `UTC` applies (time-zone.md).
    pub fn time_zone(&self) -> &str {
        self.settings
            .time_zone
            .as_deref()
            .unwrap_or(hinteros_api::system::DEFAULT_TIME_ZONE)
    }

    /// Whether the guest account exists (schema 2, `settings.guest.enable`, default true).
    pub fn guest_enabled(&self) -> bool {
        self.is_schema2() && self.settings.guest.as_ref().is_none_or(|g| g.enable)
    }

    /// The schema-2 state a schema-1 state migrates to (apps.md "The v1 → v2 migration"):
    /// `apps` removed, `requirements` as given, no services, the accounts as `{id, user}`.
    pub fn migrated(&self, requirements: Vec<String>) -> State {
        let mut s = self.clone();
        s.schema_version = 2;
        s.apps = None;
        s.requirements = Some(requirements);
        s.services = Some(Vec::new());
        s.profiles = self
            .accounts()
            .into_iter()
            .map(|a| Profile {
                id: a.id,
                name: None,
                user: a.user,
            })
            .collect();
        s
    }

    /// The bytes the service writes: pretty JSON with a trailing newline, fields in a fixed
    /// order (struct order), so the same state always gives the same revision.
    pub fn to_canonical_json(&self) -> Vec<u8> {
        let mut out = serde_json::to_vec_pretty(self).expect("state serializes");
        out.push(b'\n');
        out
    }
}

/// Hold the `state.json` lock (blocking): the service holds it while it reads, changes and
/// writes `state.json` in place, a job while it carries the live time zone into its staged
/// config and replaces `/etc/hinteros` with it (or restores a snapshot). Released on drop.
pub fn lock(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)?;
    Ok(file)
}

/// `[a-z0-9][a-z0-9-]{0,31}` (catalog.md).
pub fn is_app_id(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// The revision of a `state.json`: SHA-256 of its bytes, hex.
pub fn revision(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One settable field of `settings.set`'s `changes` (a closed tree).
pub enum Field {
    Bool(&'static str),
    Object(&'static str, &'static [Field]),
}

/// What `settings.set` may change. Empty in API 0.2; each new `state.json` setting adds an
/// entry here, a field in `hinteros_api::system::SettingsChanges` and in the schema.
pub const SETTABLE: &[Field] = &[];

/// Check `changes` against [`SETTABLE`], naming the first offending field
/// (`invalid_params` with `data: {field, reason}`).
pub fn check_changes(changes: &Value) -> Result<(), ErrorObject> {
    walk("changes", changes, SETTABLE)
}

fn walk(path: &str, v: &Value, fields: &[Field]) -> Result<(), ErrorObject> {
    let Value::Object(map) = v else {
        return Err(ErrorObject::invalid_field(
            path,
            "not_an_object",
            format!("{path} must be an object"),
        ));
    };
    for (key, value) in map {
        let here = format!("{path}.{}", printable(key));
        let field = fields.iter().find(|f| match f {
            Field::Bool(n) | Field::Object(n, _) => n == key,
        });
        match (field, value) {
            (None, _) => {
                return Err(ErrorObject::invalid_field(
                    &here,
                    "unknown_field",
                    format!("unknown field {here}"),
                ));
            }
            (Some(Field::Bool(_)), Value::Bool(_)) => {}
            (Some(Field::Bool(_)), _) => {
                return Err(ErrorObject::invalid_field(
                    &here,
                    "not_a_bool",
                    format!("{here} must be true or false"),
                ));
            }
            (Some(Field::Object(_, inner)), _) => walk(&here, value, inner)?,
        }
    }
    Ok(())
}

/// A field name for messages: at most 64 characters, controls replaced.
fn printable(s: &str) -> String {
    s.chars()
        .take(64)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// `base.set`: `Ok(None)` if `to` is the current base; unstable → stable is a downgrade.
pub fn base_change(from: Base, to: Base, acknowledged: bool) -> Result<Option<Base>, ErrorObject> {
    if from == to {
        return Ok(None);
    }
    if from == Base::Unstable && to == Base::Stable && !acknowledged {
        return Err(ErrorObject::new(
            ErrorCode::NeedsAcknowledgement,
            "unstable to stable is a downgrade",
        ));
    }
    Ok(Some(to))
}

/// A serde "unknown field" message as `invalid_params` data, so clients learn which field.
pub fn serde_error(msg: &str) -> ErrorObject {
    let field = msg
        .split_once("unknown field `")
        .and_then(|(_, rest)| rest.split_once('`'))
        .map(|(f, _)| printable(f));
    match field {
        Some(f) => {
            ErrorObject::invalid_field(&f, "unknown_field", format!("invalid params: {msg}"))
        }
        None => ErrorObject::new(ErrorCode::InvalidParams, format!("invalid params: {msg}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVBOX: &str = r#"{
  "schemaVersion": 1,
  "base": "unstable",
  "settings": {
    "ssh": { "enable": true }
  },
  "apps": ["steam"],
  "profiles": [{ "id": "p1", "name": "Dev", "user": "hinteros" }]
}"#;

    #[test]
    fn reads_the_real_schema_and_round_trips() {
        let s = State::parse(DEVBOX.as_bytes()).unwrap();
        assert!(s.settings.ssh.enable);
        assert_eq!(s.apps(), ["steam"]);
        let out = s.to_canonical_json();
        let again = State::parse(&out).unwrap();
        assert_eq!(again, s);
        assert_eq!(again.to_canonical_json(), out, "canonical form is stable");
        // Minimal file: defaults.
        let s = State::parse(br#"{"schemaVersion":1}"#).unwrap();
        assert_eq!(s.base, Base::Unstable);
        assert!(!s.settings.ssh.enable);
    }

    #[test]
    fn rejects_what_state_nix_rejects() {
        for bad in [
            r#"{"schemaVersion":3}"#,
            r#"{"schemaVersion":2}"#,
            r#"{"schemaVersion":2,"requirements":[],"services":[],"profiles":[]}"#,
            r#"{"schemaVersion":2,"requirements":[],"services":[],"apps":[],"profiles":[{"id":"p1","user":"hinteros"}]}"#,
            r#"{"schemaVersion":2,"requirements":["x86"],"services":[],"profiles":[{"id":"p1","user":"hinteros"}]}"#,
            r#"{"schemaVersion":2,"requirements":[],"services":[],"profiles":[{"id":"p2","user":"hinteros"}]}"#,
            r#"{"schemaVersion":2,"requirements":[],"services":[],"profiles":[{"id":"p1","name":"x","user":"hinteros"}]}"#,
            r#"{"schemaVersion":2,"requirements":[],"services":[],"profiles":[{"id":"p1","user":"hinteros"},{"id":"p1","user":"hinteros"}]}"#,
            r#"{"schemaVersion":2,"requirements":[],"services":[],"profiles":[{"id":"p01","user":"hinteros-p01"}]}"#,
            r#"{"schemaVersion":1,"requirements":[]}"#,
            r#"{"schemaVersion":1,"settings":{"guest":{"enable":true}}}"#,
            r#"{"schemaVersion":1,"profiles":[{"id":"p1","user":"hinteros"}]}"#,
            r#"{"schemaVersion":1,"extra":1}"#,
            r#"{"schemaVersion":1,"disk":{"layout":"x","size":1}}"#,
            r#"{"schemaVersion":1,"settings":{"admin":{}}}"#,
            r#"{"schemaVersion":1,"settings":{"ssh":{"authorizedKeys":[]}}}"#,
            r#"{"schemaVersion":1,"settings":{"nix":{}}}"#,
            r#"{"schemaVersion":1,"settings":{"ssh":{"enable":true,"port":22}}}"#,
            r#"{"schemaVersion":1,"base":"23.11"}"#,
            r#"{"schemaVersion":1,"apps":["../x"]}"#,
            r#"{"schemaVersion":1,"apps":["Steam"]}"#,
            r#"{"schemaVersion":1,"profiles":[{"id":"p1","name":"x","user":"root;rm"}]}"#,
            r#"{"schemaVersion":1,"profiles":[{"id":"p1","name":"x","user":"u","uid":0}]}"#,
            r#"[]"#,
        ] {
            assert!(State::parse(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn schema_2_and_the_migration() {
        let s1 = State::parse(DEVBOX.as_bytes()).unwrap();
        assert!(!s1.is_schema2() && !s1.guest_enabled());
        assert_eq!(
            s1.accounts(),
            [Account {
                id: "p1".into(),
                user: "hinteros".into()
            }]
        );
        let s2 = s1.migrated(vec!["graphics32".into()]);
        s2.validate().unwrap();
        let text = String::from_utf8(s2.to_canonical_json()).unwrap();
        assert!(
            !text.contains("apps") && !text.contains("\"name\""),
            "{text}"
        );
        let again = State::parse(text.as_bytes()).unwrap();
        assert_eq!(again, s2);
        assert!(again.guest_enabled());
        assert_eq!(again.requirements(), ["graphics32"]);
        // profiles: [] (every box the old installer made) is p1 = hinteros.
        let empty = State::parse(br#"{"schemaVersion":1,"apps":["youtube"],"profiles":[]}"#)
            .unwrap()
            .migrated(Vec::new());
        assert_eq!(empty.profiles[0].user, "hinteros");
        empty.validate().unwrap();
        let s = State::parse(
            br#"{"schemaVersion":2,"requirements":[],"services":[],"settings":{"guest":{"enable":false}},
                "profiles":[{"id":"p1","user":"hinteros"},{"id":"p12","user":"hinteros-p12"}]}"#,
        )
        .unwrap();
        assert!(!s.guest_enabled());
        assert_eq!(s.accounts()[1].user, "hinteros-p12");
    }

    #[test]
    fn cleanup_setting() {
        let base = r#"{"schemaVersion":2,"requirements":[],"services":[],"profiles":[{"id":"p1","user":"hinteros"}]"#;
        // Absent: today's policy, and nothing is written.
        let s = State::parse(format!("{base}}}").as_bytes()).unwrap();
        assert_eq!(s.cleanup(), CleanupState::default());
        assert_eq!(
            (
                s.cleanup().enable,
                s.cleanup().frequency,
                s.cleanup().keep_system,
                s.cleanup().keep_profiles
            ),
            (true, CleanupFrequency::Weekly, 5, 5)
        );
        assert!(
            !String::from_utf8(s.to_canonical_json())
                .unwrap()
                .contains("cleanup")
        );
        // Partly given: the rest defaults; written back in full, camelCase.
        let s = State::parse(
            format!(r#"{base},"settings":{{"cleanup":{{"keepSystem":2,"frequency":"daily"}}}}}}"#)
                .as_bytes(),
        )
        .unwrap();
        let c = s.cleanup();
        assert_eq!(
            (c.enable, c.frequency, c.keep_system, c.keep_profiles),
            (true, CleanupFrequency::Daily, 2, 5)
        );
        let text = String::from_utf8(s.to_canonical_json()).unwrap();
        assert!(
            text.contains("\"keepSystem\": 2") && text.contains("\"keepProfiles\": 5"),
            "{text}"
        );
        assert_eq!(State::parse(text.as_bytes()).unwrap(), s);
        // Schema 1 too.
        State::parse(br#"{"schemaVersion":1,"settings":{"cleanup":{"enable":false}}}"#).unwrap();
        for bad in [
            r#"{"keepSystem":0}"#,
            r#"{"keepProfiles":11}"#,
            r#"{"frequency":"yearly"}"#,
            r#"{"keep_system":3}"#,
            r#"{"enable":"yes"}"#,
            r#"{"keepSystem":-1}"#,
        ] {
            let text = format!(r#"{base},"settings":{{"cleanup":{bad}}}}}"#);
            assert!(State::parse(text.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn time_zone_setting() {
        // Absent: UTC, and nothing is written.
        let s = State::parse(br#"{"schemaVersion":1}"#).unwrap();
        assert_eq!(s.time_zone(), "UTC");
        assert!(
            !String::from_utf8(s.to_canonical_json())
                .unwrap()
                .contains("timeZone")
        );
        let base = r#"{"schemaVersion":2,"requirements":[],"services":[],"profiles":[{"id":"p1","user":"hinteros"}]"#;
        let s = State::parse(
            format!(r#"{base},"settings":{{"timeZone":"America/Chicago"}}}}"#).as_bytes(),
        )
        .unwrap();
        assert_eq!(s.time_zone(), "America/Chicago");
        let text = String::from_utf8(s.to_canonical_json()).unwrap();
        assert!(text.contains(r#""timeZone": "America/Chicago""#), "{text}");
        // A well-formed name tzdata may not have is kept (UTC applies at run time).
        State::parse(br#"{"schemaVersion":1,"settings":{"timeZone":"Mars/Olympus_Mons"}}"#)
            .unwrap();
        for bad in [
            r#""../../etc/shadow""#,
            r#""/etc/localtime""#,
            r#""""#,
            "3",
            "null x",
        ] {
            let text = format!(r#"{{"schemaVersion":1,"settings":{{"timeZone":{bad}}}}}"#);
            assert!(State::parse(text.as_bytes()).is_err(), "{bad}");
        }
        assert!(
            State::parse(br#"{"schemaVersion":1,"settings":{"time_zone":"UTC"}}"#).is_err(),
            "camelCase only"
        );
    }

    #[test]
    fn profile_ids() {
        for ok in ["p1", "p9", "p10", "p999"] {
            assert!(is_profile_id(ok), "{ok}");
        }
        for bad in ["p0", "p01", "p1000", "p", "q1", "P1", "p1a", ""] {
            assert!(!is_profile_id(bad), "{bad}");
        }
        assert_eq!(profile_number("p12"), Some(12));
        assert_eq!(user_for("p1"), "hinteros");
        assert_eq!(user_for("p3"), "hinteros-p3");
    }

    #[test]
    fn revision_is_sha256_hex() {
        let r = revision(b"");
        assert_eq!(
            r,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn changes_are_closed() {
        check_changes(&serde_json::json!({})).unwrap();
        let e = check_changes(&serde_json::json!({"substituters": ["x"]})).unwrap_err();
        assert_eq!(e.data.unwrap()["field"], "changes.substituters");
        let e = check_changes(&serde_json::json!([])).unwrap_err();
        assert_eq!(e.data.unwrap()["reason"], "not_an_object");
        // The walker itself, with a non-empty tree.
        const TREE: &[Field] = &[Field::Object("ssh", &[Field::Bool("x")])];
        walk("changes", &serde_json::json!({"ssh": {"x": true}}), TREE).unwrap();
        let e = walk("changes", &serde_json::json!({"ssh": {"x": 1}}), TREE).unwrap_err();
        assert_eq!(
            e.data.unwrap(),
            serde_json::json!({"field": "changes.ssh.x", "reason": "not_a_bool"})
        );
        let e = walk("changes", &serde_json::json!({"ssh": {"y": true}}), TREE).unwrap_err();
        assert_eq!(e.data.unwrap()["field"], "changes.ssh.y");
    }

    #[test]
    fn version_rules() {
        assert_eq!(
            base_change(Base::Unstable, Base::Unstable, false).unwrap(),
            None
        );
        assert_eq!(
            base_change(Base::Stable, Base::Unstable, false).unwrap(),
            Some(Base::Unstable)
        );
        assert_eq!(
            base_change(Base::Unstable, Base::Stable, false)
                .unwrap_err()
                .kind(),
            Some(ErrorCode::NeedsAcknowledgement)
        );
        assert_eq!(
            base_change(Base::Unstable, Base::Stable, true).unwrap(),
            Some(Base::Stable)
        );
    }

    #[test]
    fn serde_errors_name_the_field() {
        let e = serde_error("unknown field `uid`, expected `enabled`");
        assert_eq!(
            e.data.unwrap(),
            serde_json::json!({"field": "uid", "reason": "unknown_field"})
        );
        assert!(serde_error("missing field `enabled`").data.is_none());
    }
}
