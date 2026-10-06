//! `state.json`: the typed device state, mirroring the system repo's `state.nix` allowlist.
//!
//! The service reads the file into [`State`] (unknown fields are an error, as in `state.nix`),
//! changes it through typed operations only, and writes it back from the struct with
//! [`State::to_canonical_json`]. Client JSON never reaches the file. `apps` (M8) and
//! `profiles` (M7) are kept and round-tripped; their methods will edit them here too.

use std::path::Path;

use hinteros_api::system::{Base, Channel};
use hinteros_api::{ErrorCode, ErrorObject};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct State {
    pub schema_version: u32,
    #[serde(default = "default_base")]
    pub base: Base,
    #[serde(default)]
    pub settings: StateSettings,
    /// Catalog app ids installed from the Store (M8: `apps.install`/`apps.remove`).
    #[serde(default)]
    pub apps: Vec<String>,
    /// Profiles (M7).
    #[serde(default)]
    pub profiles: Vec<Profile>,
}

fn default_base() -> Base {
    Base::Unstable
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateSettings {
    #[serde(default)]
    pub ssh: SshState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SshState {
    #[serde(default)]
    pub enable: bool,
    /// Kept as found (key login moved to the backlog; no method writes it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorized_keys: Option<Vec<String>>,
    /// Kept as found (dropped from the API; no method writes it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_admin: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub user: String,
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("reading {0}: {1}")]
    Io(String, std::io::Error),
    #[error("state.json is invalid: {0}")]
    Invalid(String),
}

impl State {
    pub fn parse(bytes: &[u8]) -> Result<Self, StateError> {
        let state: State =
            serde_json::from_slice(bytes).map_err(|e| StateError::Invalid(e.to_string()))?;
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
        if self.schema_version != SCHEMA_VERSION {
            return Err(format!(
                "unsupported schemaVersion {} (supported: {SCHEMA_VERSION})",
                self.schema_version
            ));
        }
        for app in &self.apps {
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
            let name_ok =
                (1..=64).contains(&p.name.chars().count()) && !p.name.chars().any(char::is_control);
            if !(id_ok && user_ok && name_ok) {
                return Err(format!("bad profile {:?}", p.id));
            }
        }
        Ok(())
    }

    /// The bytes the service writes: pretty JSON with a trailing newline, fields in a fixed
    /// order (struct order), so the same state always gives the same revision.
    pub fn to_canonical_json(&self) -> Vec<u8> {
        let mut out = serde_json::to_vec_pretty(self).expect("state serializes");
        out.push(b'\n');
        out
    }
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

/// `channel.set`: `Ok(None)` if `to` is the current channel; beta → stable is a downgrade.
/// (A target tip that does not descend from the locked revision also needs the
/// acknowledgement; the job checks that after fetching.)
pub fn channel_change(
    from: Option<Channel>,
    to: Channel,
    acknowledged: bool,
) -> Result<Option<Channel>, ErrorObject> {
    if from == Some(to) {
        return Ok(None);
    }
    if from == Some(Channel::Beta) && to == Channel::Stable && !acknowledged {
        return Err(ErrorObject::new(
            ErrorCode::NeedsAcknowledgement,
            "switching from beta to stable is a downgrade",
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
    "ssh": { "enable": true, "authorizedKeys": [] }
  },
  "apps": ["steam"],
  "profiles": [{ "id": "p1", "name": "Dev", "user": "hinteros" }]
}"#;

    #[test]
    fn reads_the_real_schema_and_round_trips() {
        let s = State::parse(DEVBOX.as_bytes()).unwrap();
        assert!(s.settings.ssh.enable);
        assert_eq!(s.settings.ssh.authorized_keys, Some(vec![]));
        assert_eq!(s.apps, ["steam"]);
        let out = s.to_canonical_json();
        let again = State::parse(&out).unwrap();
        assert_eq!(again, s);
        assert_eq!(again.to_canonical_json(), out, "canonical form is stable");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v["settings"]["ssh"]["authorizedKeys"],
            serde_json::json!([])
        );
        assert!(v["settings"]["ssh"].get("allowAdmin").is_none());
        // Minimal file: defaults.
        let s = State::parse(br#"{"schemaVersion":1}"#).unwrap();
        assert_eq!(s.base, Base::Unstable);
        assert!(!s.settings.ssh.enable);
    }

    #[test]
    fn rejects_what_state_nix_rejects() {
        for bad in [
            r#"{"schemaVersion":2}"#,
            r#"{"schemaVersion":1,"extra":1}"#,
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

        assert_eq!(
            channel_change(Some(Channel::Beta), Channel::Beta, false).unwrap(),
            None
        );
        assert_eq!(
            channel_change(Some(Channel::Stable), Channel::Beta, false).unwrap(),
            Some(Channel::Beta)
        );
        assert_eq!(
            channel_change(Some(Channel::Beta), Channel::Stable, false)
                .unwrap_err()
                .kind(),
            Some(ErrorCode::NeedsAcknowledgement)
        );
        assert_eq!(
            channel_change(Some(Channel::Beta), Channel::Stable, true).unwrap(),
            Some(Channel::Stable)
        );
        assert_eq!(
            channel_change(None, Channel::Stable, false).unwrap(),
            Some(Channel::Stable)
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
