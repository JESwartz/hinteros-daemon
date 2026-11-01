//! `state.json`: the typed device state, mirroring the system repo's `state.nix`.
//!
//! The service reads the file into [`State`], changes it through typed operations only, and
//! writes it back from the struct with [`State::to_canonical_json`]. Client JSON never reaches
//! the file.
//!
//! Only schema 2 exists (device-config.md): `requirements` (box features), `services`,
//! `profiles: [{id}]` (the accounts; names live in `profiles.json`), `settings.guest.enable`.
//!
//! Fields this daemon does not know load and are written back (each struct's `extra`): a box
//! that switches to an older channel still has to read the newer file. A change that is not
//! safe to ignore bumps `schemaVersion`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use hinteros_api::system::{Base, CleanupFrequency, MAX_KEEP};
use hinteros_api::{ErrorCode, ErrorObject};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The only `schemaVersion` the service reads and writes.
pub const SCHEMA_VERSION: u32 = 2;

/// The bases the system repo builds; `base.set` refuses others.
pub const BUILT_BASES: &[Base] = &[Base::Unstable];

/// Most accounts `state.json` may declare (the guest does not count).
pub const MAX_PROFILES: usize = 8;

/// The guest account.
pub const GUEST_USER: &str = "hinteros-guest";

/// The guest account's uid (profiles.nix).
pub const GUEST_UID: u32 = 20000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub schema_version: u32,
    pub base: Base,
    /// NixOS `system.stateVersion`, installer-written; never settable through the API.
    pub state_version: String,
    #[serde(default)]
    pub settings: StateSettings,
    /// Disk layout, installer-written; never settable through the API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<DiskState>,
    /// Box features that are on (apps.md "Box requirements").
    pub requirements: Vec<String>,
    /// Enabled services (M10).
    pub services: Vec<String>,
    /// The accounts.
    pub profiles: Vec<Profile>,
    /// Fields this daemon does not know, here and in the structs below.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSettings {
    /// Installer-written; not settable through `settings.set`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default)]
    pub ssh: SshState,
    /// The admin account (installer-written; not settable through `settings.set`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin: Option<AdminState>,
    /// The guest account (`guest.set`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest: Option<GuestState>,
    /// The automatic clean-up (`cleanup.set`; storage.md). Absent: [`CleanupState::default`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup: Option<CleanupState>,
    /// The box's time zone, an IANA name (`time_zone.set`; time-zone.md). Absent: `UTC`. Set
    /// in place by the service, without a rebuild: the system reads it at activation time.
    #[serde(default, rename = "timeZone", skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// `settings.cleanup` (storage.md): every field optional, these defaults (the policy before the
/// setting existed: weekly, 5 system generations).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupState {
    #[serde(default = "yes")]
    pub enable: bool,
    #[serde(default = "default_frequency")]
    pub frequency: CleanupFrequency,
    #[serde(default = "default_keep")]
    pub keep_system: u32,
    #[serde(default = "default_keep")]
    pub keep_profiles: u32,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Default for CleanupState {
    fn default() -> Self {
        Self {
            enable: true,
            frequency: default_frequency(),
            keep_system: default_keep(),
            keep_profiles: default_keep(),
            extra: BTreeMap::new(),
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
pub struct GuestState {
    #[serde(default = "yes")]
    pub enable: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshState {
    #[serde(default)]
    pub enable: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminState {
    pub user: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskState {
    pub layout: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// A `profiles[]` entry: the id says the account ([`user_for`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// An account: a `state.json` profile entry and its user.
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

/// The user name of profile `p<N>`: `hinteros-p<N>`, with uid 20000 + N (profiles.nix).
pub fn user_for(id: &str) -> String {
    format!("hinteros-{id}")
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("reading {0}: {1}")]
    Io(String, std::io::Error),
    #[error("state.json is invalid: {0}")]
    Invalid(String),
}

fn check_schema(version: u32) -> Result<(), String> {
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    Err(format!(
        "unsupported schemaVersion {version} (supported: {SCHEMA_VERSION})"
    ))
}

impl State {
    pub fn parse(bytes: &[u8]) -> Result<Self, StateError> {
        // The schema first: a newer file should say so, not that a field is missing.
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Header {
            schema_version: u32,
        }
        let invalid = |e: serde_json::Error| StateError::Invalid(e.to_string());
        let header: Header = serde_json::from_slice(bytes).map_err(invalid)?;
        check_schema(header.schema_version).map_err(StateError::Invalid)?;
        let state: State = serde_json::from_slice(bytes).map_err(invalid)?;
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
        check_schema(self.schema_version)?;
        if self.state_version.is_empty() {
            return Err("stateVersion is empty".into());
        }
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
        let mut seen = BTreeSet::new();
        for f in &self.requirements {
            if !hinteros_api::catalog::is_box_feature(f) || !seen.insert(f) {
                return Err(format!("bad requirement {f:?}"));
            }
        }
        for s in &self.services {
            if !is_app_id(s) {
                return Err(format!("bad service id {s:?}"));
            }
        }
        if self.profiles.is_empty() || self.profiles.len() > MAX_PROFILES {
            return Err(format!("1 to {MAX_PROFILES} profiles"));
        }
        let mut ids = BTreeSet::new();
        for p in &self.profiles {
            if !is_profile_id(&p.id) {
                return Err(format!("bad profile {:?}", p.id));
            }
            if !ids.insert(&p.id) {
                return Err(format!("duplicate profile {:?}", p.id));
            }
        }
        Ok(())
    }

    /// The accounts this state declares.
    pub fn accounts(&self) -> Vec<Account> {
        self.profiles
            .iter()
            .map(|p| Account {
                id: p.id.clone(),
                user: user_for(&p.id),
            })
            .collect()
    }

    /// The automatic clean-up setting (defaults when absent).
    pub fn cleanup(&self) -> CleanupState {
        self.settings.cleanup.clone().unwrap_or_default()
    }

    /// The time zone `state.json` names (`UTC` when absent). The system's tzdata may not have
    /// it; then `UTC` applies (time-zone.md).
    pub fn time_zone(&self) -> &str {
        self.settings
            .time_zone
            .as_deref()
            .unwrap_or(hinteros_api::system::DEFAULT_TIME_ZONE)
    }

    /// Whether the guest account exists (`settings.guest.enable`, default true).
    pub fn guest_enabled(&self) -> bool {
        self.settings.guest.as_ref().is_none_or(|g| g.enable)
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

/// `base.set`: `Ok(None)` if `to` is the current base; a base that is not built is refused;
/// unstable → stable is a downgrade.
pub fn base_change(from: Base, to: Base, acknowledged: bool) -> Result<Option<Base>, ErrorObject> {
    base_change_within(BUILT_BASES, from, to, acknowledged)
}

fn base_change_within(
    built: &[Base],
    from: Base,
    to: Base,
    acknowledged: bool,
) -> Result<Option<Base>, ErrorObject> {
    if from == to {
        return Ok(None);
    }
    if !built.contains(&to) {
        return Err(ErrorObject::with_data(
            ErrorCode::Unavailable,
            "this base is not built",
            json!({ "service": "base", "reason": "not_built" }),
        ));
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

    /// A minimal valid file, without its closing brace.
    const BASE: &str = r#"{"schemaVersion":2,"base":"unstable","stateVersion":"26.11","requirements":[],"services":[],"profiles":[{"id":"p1"}]"#;

    const DEVBOX: &str = r#"{
  "schemaVersion": 2,
  "base": "unstable",
  "stateVersion": "26.11",
  "settings": {
    "ssh": { "enable": true }
  },
  "requirements": ["graphics32"],
  "services": ["jellyfin-server"],
  "profiles": [{ "id": "p1" }, { "id": "p12" }]
}"#;

    fn parse(tail: &str) -> Result<State, StateError> {
        State::parse(format!("{BASE}{tail}}}").as_bytes())
    }

    #[test]
    fn reads_the_real_schema_and_round_trips() {
        let s = State::parse(DEVBOX.as_bytes()).unwrap();
        assert!(s.settings.ssh.enable);
        assert_eq!(s.requirements, ["graphics32"]);
        assert_eq!(s.services, ["jellyfin-server"]);
        assert_eq!(s.state_version, "26.11");
        let out = s.to_canonical_json();
        let again = State::parse(&out).unwrap();
        assert_eq!(again, s);
        assert_eq!(again.to_canonical_json(), out, "canonical form is stable");
        // Minimal file: defaults.
        let s = parse("").unwrap();
        assert!(!s.settings.ssh.enable);
        assert!(s.guest_enabled());
    }

    #[test]
    fn accounts_come_from_the_id() {
        let s = State::parse(DEVBOX.as_bytes()).unwrap();
        assert_eq!(
            s.accounts(),
            [
                Account {
                    id: "p1".into(),
                    user: "hinteros-p1".into()
                },
                Account {
                    id: "p12".into(),
                    user: "hinteros-p12".into()
                }
            ]
        );
        let text = String::from_utf8(s.to_canonical_json()).unwrap();
        assert!(!text.contains("\"user\""), "{text}");
        let s = parse(r#","settings":{"guest":{"enable":false}}"#).unwrap();
        assert!(!s.guest_enabled());
    }

    #[test]
    fn rejects_what_state_nix_rejects() {
        for bad in [
            r#"[]"#,
            r#"{}"#,
            // Required fields.
            r#"{"schemaVersion":2,"stateVersion":"26.11","requirements":[],"services":[],"profiles":[{"id":"p1"}]}"#,
            r#"{"schemaVersion":2,"base":"unstable","requirements":[],"services":[],"profiles":[{"id":"p1"}]}"#,
            r#"{"schemaVersion":2,"base":"unstable","stateVersion":"26.11","services":[],"profiles":[{"id":"p1"}]}"#,
            r#"{"schemaVersion":2,"base":"unstable","stateVersion":"26.11","requirements":[],"profiles":[{"id":"p1"}]}"#,
            r#"{"schemaVersion":2,"base":"unstable","stateVersion":"26.11","requirements":[],"services":[]}"#,
            // Bad values of known fields.
            r#"{"schemaVersion":2,"base":"23.11","stateVersion":"26.11","requirements":[],"services":[],"profiles":[{"id":"p1"}]}"#,
            r#"{"schemaVersion":2,"base":"unstable","stateVersion":"","requirements":[],"services":[],"profiles":[{"id":"p1"}]}"#,
            r#"{"schemaVersion":2,"base":"unstable","stateVersion":26,"requirements":[],"services":[],"profiles":[{"id":"p1"}]}"#,
            r#"{"schemaVersion":2,"base":"unstable","stateVersion":"26.11","requirements":[],"services":[],"profiles":[]}"#,
            r#"{"schemaVersion":2,"base":"unstable","stateVersion":"26.11","requirements":["x86"],"services":[],"profiles":[{"id":"p1"}]}"#,
            r#"{"schemaVersion":2,"base":"unstable","stateVersion":"26.11","requirements":["graphics32","graphics32"],"services":[],"profiles":[{"id":"p1"}]}"#,
            r#"{"schemaVersion":2,"base":"unstable","stateVersion":"26.11","requirements":[],"services":["../x"],"profiles":[{"id":"p1"}]}"#,
            r#"{"schemaVersion":2,"base":"unstable","stateVersion":"26.11","requirements":[],"services":[],"profiles":[{"id":"p1"},{"id":"p1"}]}"#,
            r#"{"schemaVersion":2,"base":"unstable","stateVersion":"26.11","requirements":[],"services":[],"profiles":[{"id":"p01"}]}"#,
            r#"{"schemaVersion":2,"base":"unstable","stateVersion":"26.11","requirements":[],"services":[],"profiles":[{"user":"root"}]}"#,
            r#"{"schemaVersion":2,"base":"unstable","stateVersion":"26.11","requirements":[],"services":[],"profiles":[{"id":"p1"}],"settings":{"ssh":{"enable":1}}}"#,
            r#"{"schemaVersion":2,"base":"unstable","stateVersion":"26.11","requirements":[],"services":[],"profiles":[{"id":"p1"}],"settings":{"admin":{}}}"#,
            r#"{"schemaVersion":2,"base":"unstable","stateVersion":"26.11","requirements":[],"services":[],"profiles":[{"id":"p1"}],"disk":{"size":1}}"#,
        ] {
            assert!(State::parse(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn another_schema_is_an_error_that_says_so() {
        for (bad, version) in [
            (r#"{"schemaVersion":3}"#, 3),
            (r#"{"schemaVersion":1,"apps":[],"profiles":[]}"#, 1),
            (
                r#"{"schemaVersion":3,"base":"unstable","stateVersion":"26.11","requirements":[],"services":[],"profiles":[{"id":"p1"}]}"#,
                3,
            ),
        ] {
            let e = State::parse(bad.as_bytes()).unwrap_err().to_string();
            assert!(
                e.contains(&format!("unsupported schemaVersion {version}"))
                    && e.contains("supported: 2"),
                "{e}"
            );
        }
        let mut s = parse("").unwrap();
        s.schema_version = 1;
        assert!(s.validate().is_err());
    }

    #[test]
    fn unknown_fields_load_and_are_written_back() {
        let file = br#"{"zeta":{"b":1,"a":[true]},"schemaVersion":2,"alpha":"new",
            "base":"unstable","stateVersion":"26.11","requirements":[],"services":[],
            "disk":{"layout":"x","size":1},
            "settings":{"future":3,
                "ssh":{"enable":true,"port":22},
                "admin":{"user":"james","shell":"zsh"},
                "guest":{"enable":false,"idle":5},
                "cleanup":{"keepSystem":2,"keep_system":9}},
            "profiles":[{"id":"p1","user":"hinteros-p1","color":"teal"}]}"#;
        let s = State::parse(file).unwrap();
        assert_eq!(s.extra["alpha"], "new");
        assert_eq!(s.settings.extra["future"], 3);
        assert_eq!(s.settings.ssh.extra["port"], 22);
        assert_eq!(s.settings.admin.as_ref().unwrap().extra["shell"], "zsh");
        assert_eq!(s.settings.guest.as_ref().unwrap().extra["idle"], 5);
        assert_eq!(s.cleanup().extra["keep_system"], 9);
        assert_eq!(s.cleanup().keep_system, 2);
        assert_eq!(s.disk.as_ref().unwrap().extra["size"], 1);
        assert_eq!(s.profiles[0].extra["color"], "teal");
        // Every one of them survives a rewrite, in a fixed place: extras after known fields,
        // sorted, whatever the file's order was.
        let out = s.to_canonical_json();
        let written: Value = serde_json::from_slice(&out).unwrap();
        let mut original: Value = serde_json::from_slice(file).unwrap();
        original["settings"]["ssh"]["enable"] = json!(true);
        original["settings"]["cleanup"] = json!({"enable": true, "frequency": "weekly", "keepSystem": 2, "keepProfiles": 5, "keep_system": 9});
        assert_eq!(written, original);
        let again = State::parse(&out).unwrap();
        assert_eq!(again, s);
        assert_eq!(again.to_canonical_json(), out);
        let text = String::from_utf8(out).unwrap();
        let (alpha, zeta) = (
            text.find("\"alpha\"").unwrap(),
            text.find("\"zeta\"").unwrap(),
        );
        assert!(
            text.find("\"profiles\"").unwrap() < alpha && alpha < zeta,
            "{text}"
        );
        // An edit through the struct keeps them too.
        let mut edited = again;
        edited.settings.ssh.enable = false;
        let text = String::from_utf8(edited.to_canonical_json()).unwrap();
        assert!(
            text.contains("\"port\": 22") && text.contains("\"alpha\""),
            "{text}"
        );
    }

    #[test]
    fn cleanup_setting() {
        // Absent: today's policy, and nothing is written.
        let s = parse("").unwrap();
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
        let s = parse(r#","settings":{"cleanup":{"keepSystem":2,"frequency":"daily"}}"#).unwrap();
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
        for bad in [
            r#"{"keepSystem":0}"#,
            r#"{"keepProfiles":11}"#,
            r#"{"frequency":"yearly"}"#,
            r#"{"enable":"yes"}"#,
            r#"{"keepSystem":-1}"#,
        ] {
            let tail = format!(r#","settings":{{"cleanup":{bad}}}"#);
            assert!(parse(&tail).is_err(), "{bad}");
        }
    }

    #[test]
    fn time_zone_setting() {
        // Absent: UTC, and nothing is written.
        let s = parse("").unwrap();
        assert_eq!(s.time_zone(), "UTC");
        assert!(
            !String::from_utf8(s.to_canonical_json())
                .unwrap()
                .contains("timeZone")
        );
        let s = parse(r#","settings":{"timeZone":"America/Chicago"}"#).unwrap();
        assert_eq!(s.time_zone(), "America/Chicago");
        let text = String::from_utf8(s.to_canonical_json()).unwrap();
        assert!(text.contains(r#""timeZone": "America/Chicago""#), "{text}");
        // A well-formed name tzdata may not have is kept (UTC applies at run time).
        parse(r#","settings":{"timeZone":"Mars/Olympus_Mons"}"#).unwrap();
        for bad in [
            r#""../../etc/shadow""#,
            r#""/etc/localtime""#,
            r#""""#,
            "3",
            "null x",
        ] {
            let tail = format!(r#","settings":{{"timeZone":{bad}}}"#);
            assert!(parse(&tail).is_err(), "{bad}");
        }
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
        assert_eq!(user_for("p1"), "hinteros-p1");
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
        // Stable is not built: refused whatever was acknowledged, and it names why.
        for ack in [false, true] {
            let e = base_change(Base::Unstable, Base::Stable, ack).unwrap_err();
            assert_eq!(e.kind(), Some(ErrorCode::Unavailable));
            assert_eq!(
                e.data.unwrap(),
                json!({"service": "base", "reason": "not_built"})
            );
        }
        // The day it is built, the downgrade needs acknowledging.
        assert_eq!(
            base_change_within(Base::ALL, Base::Unstable, Base::Stable, false)
                .unwrap_err()
                .kind(),
            Some(ErrorCode::NeedsAcknowledgement)
        );
        assert_eq!(
            base_change_within(Base::ALL, Base::Unstable, Base::Stable, true).unwrap(),
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
