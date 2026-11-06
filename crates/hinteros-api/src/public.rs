//! The public profile list `/run/hinteros/profiles-public.json` written by the
//! system service (0644; the greeter is not a `hinteros` member), read by the greeter and the
//! splash. One type for both sides, so they cannot drift.
//!
//! Reading is tolerant: unknown fields are ignored, and a field or profile that does not parse is
//! left out (logged), so a change on the service's side never empties the picker. A profile's
//! `locked` that is not known reads as locked (the picker asks for the PIN; PAM decides anyway).

use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use tracing::warn;

/// The `profiles-public.json` version written. A newer file is read as far as this version
/// knows it.
pub const PUBLIC_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PublicView {
    /// [`PUBLIC_VERSION`] when written.
    #[serde(default, deserialize_with = "lenient")]
    pub version: u32,
    #[serde(default, deserialize_with = "lenient_list")]
    pub profiles: Vec<PublicProfile>,
    #[serde(default, deserialize_with = "lenient")]
    pub guest: PublicGuest,
    #[serde(default, deserialize_with = "lenient")]
    pub first_boot: bool,
    /// First-boot mode: the spare account the greeter enters.
    #[serde(default, deserialize_with = "lenient")]
    pub spare: Option<String>,
    /// The profile used last (the picker's focus).
    #[serde(default, deserialize_with = "lenient")]
    pub last_used: Option<String>,
    /// A live switch hint.
    #[serde(default, deserialize_with = "lenient")]
    pub next: Option<Next>,
    /// The picker's look: the largest text scale and every accessibility option any profile has.
    #[serde(default, deserialize_with = "lenient")]
    pub appearance: Option<PublicAppearance>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublicProfile {
    pub id: String,
    pub user: String,
    pub name: String,
    #[serde(default, deserialize_with = "lenient")]
    pub color: String,
    /// It has a PIN and is locked. Not known: locked.
    #[serde(default = "yes")]
    pub locked: bool,
    #[serde(default)]
    pub admin: bool,
    /// The end of its wrong-PIN wait, while there is one.
    #[serde(default, deserialize_with = "lenient")]
    pub locked_until: Option<i64>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PublicGuest {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, deserialize_with = "lenient")]
    pub user: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Next {
    /// A profile id or `guest`.
    pub target: String,
    /// The greeter may enter without asking (unlocked, guest, or a token).
    #[serde(default)]
    pub fast: bool,
    pub expires: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublicAppearance {
    pub text_scale: f64,
    pub high_contrast: bool,
    pub reduce_motion: bool,
}

impl Default for PublicAppearance {
    fn default() -> Self {
        Self {
            text_scale: 1.0,
            high_contrast: false,
            reduce_motion: false,
        }
    }
}

/// A value that does not parse is left at its default, not the whole file.
fn lenient<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let value = Value::deserialize(d)?;
    Ok(serde_json::from_value(value).unwrap_or_else(|e| {
        warn!("profiles-public.json: a field is ignored: {e}");
        T::default()
    }))
}

/// A list whose entries that do not parse are left out.
fn lenient_list<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let Value::Array(items) = Value::deserialize(d)? else {
        warn!("profiles-public.json: a list is not a list; ignored");
        return Ok(Vec::new());
    };
    Ok(items
        .into_iter()
        .filter_map(|item| {
            serde_json::from_value(item)
                .inspect_err(|e| warn!("profiles-public.json: an entry is ignored: {e}"))
                .ok()
        })
        .collect())
}

impl PublicView {
    /// Read the file. Missing: nothing to show yet (the service writes it when it starts).
    /// Unreadable or not JSON: nothing either (logged).
    pub fn load(path: &Path) -> Self {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                warn!(path = %path.display(), "cannot read the public profile list: {e}");
                return Self::default();
            }
        };
        if let Err(e) = crate::common::check_version(path, &bytes, PUBLIC_VERSION) {
            warn!("{e}; reading the fields this version knows");
        }
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            warn!(path = %path.display(), "cannot parse the public profile list: {e}");
            Self::default()
        })
    }

    /// The user of a profile id or `guest`.
    pub fn user_of(&self, id: &str) -> Option<String> {
        if id == "guest" {
            return self
                .guest
                .enabled
                .then(|| self.guest.user.clone().unwrap_or("hinteros-guest".into()));
        }
        self.profiles
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.user.clone())
    }

    /// The user the greeter may enter without any UI: first-boot mode's spare, or a live
    /// switch hint the service marked fast.
    pub fn fast_path(&self, now: i64) -> Option<String> {
        if self.first_boot {
            return self.spare.clone();
        }
        let next = self.next.as_ref().filter(|n| n.fast && n.expires > now)?;
        self.user_of(&next.target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_path_choices() {
        let v: PublicView = serde_json::from_str(
            r#"{"version":1,"profiles":[
                 {"id":"p1","user":"hinteros-p1","name":"J","color":"teal","locked":false,"admin":true,"locked_until":null},
                 {"id":"p3","user":"hinteros-p3","name":"S","color":"red","locked":true,"admin":false,"locked_until":null}],
               "guest":{"enabled":true,"user":"hinteros-guest"},"first_boot":false,"spare":null,
               "last_used":"p1","next":{"target":"p3","fast":false,"expires":100},
               "appearance":{"text_scale":1.0,"high_contrast":false,"reduce_motion":false}}"#,
        )
        .unwrap();
        assert_eq!(v.fast_path(50), None, "locked target: the picker");
        let mut fast = v.clone();
        fast.next.as_mut().unwrap().fast = true;
        assert_eq!(fast.fast_path(50).as_deref(), Some("hinteros-p3"));
        assert_eq!(fast.fast_path(200), None, "expired");
        let mut first = v.clone();
        first.first_boot = true;
        first.spare = Some("hinteros-p1".into());
        assert_eq!(first.fast_path(0).as_deref(), Some("hinteros-p1"));
        assert_eq!(v.user_of("guest").as_deref(), Some("hinteros-guest"));
        assert_eq!(PublicView::default().fast_path(0), None);
    }

    /// What the writer sends is what the reader reads, field for field (nulls included).
    #[test]
    fn round_trips() {
        let v = PublicView {
            version: PUBLIC_VERSION,
            profiles: vec![PublicProfile {
                id: "p1".into(),
                user: "hinteros-p1".into(),
                name: "J".into(),
                color: "teal".into(),
                locked: false,
                admin: true,
                locked_until: None,
            }],
            guest: PublicGuest {
                enabled: true,
                user: Some("hinteros-guest".into()),
            },
            first_boot: false,
            spare: None,
            last_used: Some("p1".into()),
            next: None,
            appearance: Some(PublicAppearance::default()),
        };
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["profiles"][0]["locked_until"], Value::Null);
        assert_eq!(json["next"], Value::Null);
        assert_eq!(json["appearance"]["text_scale"], 1.0);
        assert_eq!(serde_json::from_value::<PublicView>(json).unwrap(), v);
    }

    #[test]
    fn reads_what_it_can() {
        let dir = std::env::temp_dir().join(format!("hinteros-public-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("profiles-public.json");
        let _ = std::fs::remove_file(&path);
        assert!(PublicView::load(&path).profiles.is_empty(), "missing");
        // A newer service: unknown fields, a profile this version cannot read, fields of
        // another type.
        std::fs::write(
            &path,
            r#"{"version":2,"profiles":[
                 {"id":"p1","user":"hinteros-p1","name":"J","color":null,"admin":true,"kind":"x"},
                 {"id":"p2","name":"no user"},
                 {"id":"p3","user":"hinteros-p3","name":"S","color":"red","locked":false,"admin":false,"locked_until":"soon"}],
               "guest":{"enabled":true,"user":"hinteros-guest"},"first_boot":"no","spare":null,
               "last_used":"p1","next":{"target":"p3","fast":true,"expires":"later"},
               "appearance":{"text_scale":1.5,"high_contrast":true,"reduce_motion":false},
               "new":{"a":1}}"#,
        )
        .unwrap();
        let v = PublicView::load(&path);
        let ids: Vec<_> = v.profiles.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["p1", "p3"]);
        assert_eq!(v.profiles[0].color, "");
        assert!(v.profiles[0].locked, "not known: locked");
        assert!(!v.profiles[1].locked);
        assert_eq!(v.profiles[1].locked_until, None);
        assert!(!v.first_boot);
        assert!(v.next.is_none());
        assert_eq!(v.user_of("guest").as_deref(), Some("hinteros-guest"));
        assert_eq!(v.appearance.map(|a| a.text_scale), Some(1.5));
        // Not JSON at all: nothing.
        std::fs::write(&path, "{").unwrap();
        assert!(PublicView::load(&path).profiles.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
