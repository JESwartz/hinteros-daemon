//! App permissions: what each sandboxed app may use, per profile (`apps.set_permission`).
//!
//! The catalog declares each app's permissions ([`hinteros_api::catalog::PERMISSIONS`]), each
//! `required` or `optional`. The profile's choices are kept in
//! `$XDG_STATE_HOME/hinteros/permissions.json` (default `~/.local/state/hinteros/`):
//! `{"version":1,"denied":{"<app id>":["mic"]}}`. Only opt-outs are stored, so an app with no
//! record has every permission (existing installs and starter sets keep working). A required
//! permission is never denied. A change applies the next time the app starts.
//!
//! Only enforced permissions are listed: none while the sandbox is off, and not `mic` while
//! the sound socket for `mic` is the same as the one every app gets.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hinteros_api::catalog::{Need, PERMISSIONS};
use hinteros_api::session::AppPermission;
use hinteros_api::{ErrorCode, ErrorObject};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::config::SandboxConfig;

/// The file format version.
pub const VERSION: u32 = 1;

/// What a sandboxed app gets (see [`crate::sandbox`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Grants {
    /// `/dev/input` and `/run/udev`.
    pub gamepad: bool,
    /// The sound socket that may record.
    pub mic: bool,
    /// The profile's `~/Downloads`.
    pub downloads: bool,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct File {
    version: u32,
    #[serde(default)]
    denied: BTreeMap<String, BTreeSet<String>>,
}

/// The default file: `$XDG_STATE_HOME/hinteros/permissions.json`, else under
/// `~/.local/state`.
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| Path::new(&h).join(".local/state"))
        })?;
    Some(base.join("hinteros/permissions.json"))
}

/// The permissions this box enforces, in display order.
pub fn enforced(cfg: &SandboxConfig) -> Vec<&'static str> {
    if !cfg.enable {
        return Vec::new();
    }
    PERMISSIONS
        .iter()
        .copied()
        .filter(|p| *p != "mic" || cfg.pulse_record_socket != cfg.pulse_socket)
        .collect()
}

/// The stored choices; cheap to clone (the shell and the Store share one).
#[derive(Debug, Clone)]
pub struct Permissions {
    path: Option<PathBuf>,
    enforced: Vec<&'static str>,
    denied: Arc<Mutex<BTreeMap<String, BTreeSet<String>>>>,
}

impl Default for Permissions {
    /// Nothing enforced, nothing stored.
    fn default() -> Self {
        Self::new(None, Vec::new())
    }
}

impl Permissions {
    /// Read `path` (missing: nothing denied; unreadable: logged, nothing denied).
    pub fn new(path: Option<PathBuf>, enforced: Vec<&'static str>) -> Self {
        let denied = path.as_deref().map(load).unwrap_or_default();
        Self {
            path,
            enforced,
            denied: Arc::new(Mutex::new(denied)),
        }
    }

    pub fn from_config(cfg: &SandboxConfig) -> Self {
        Self::new(default_path(), enforced(cfg))
    }

    /// An app's permissions for `apps.list` / `store.list`: the declared ones this box
    /// enforces, in display order.
    pub fn list(&self, id: &str, declared: &BTreeMap<String, Need>) -> Vec<AppPermission> {
        let denied = self.denied.lock().unwrap();
        let denied = denied.get(id);
        self.enforced
            .iter()
            .filter_map(|name| {
                let required = declared.get(*name)?.required();
                Some(AppPermission {
                    name: (*name).to_owned(),
                    required,
                    granted: required || !denied.is_some_and(|d| d.contains(*name)),
                })
            })
            .collect()
    }

    /// What the app gets when it starts.
    pub fn grants(&self, id: &str, declared: &BTreeMap<String, Need>) -> Grants {
        let list = self.list(id, declared);
        let has = |name: &str| list.iter().any(|p| p.name == name && p.granted);
        Grants {
            gamepad: has("gamepad"),
            mic: has("mic"),
            downloads: has("downloads"),
        }
    }

    /// `apps.set_permission` for app `id`, which declares `declared` (`None`: no such app).
    /// Returns whether anything changed.
    pub fn set(
        &self,
        id: &str,
        declared: Option<&BTreeMap<String, Need>>,
        permission: &str,
        granted: bool,
    ) -> Result<bool, ErrorObject> {
        let declared = declared.ok_or_else(|| {
            ErrorObject::new(ErrorCode::UnknownApp, format!("no app with id '{id}'"))
        })?;
        let need = declared
            .get(permission)
            .filter(|_| self.enforced.contains(&permission))
            .ok_or_else(|| {
                ErrorObject::new(
                    ErrorCode::UnknownPermission,
                    format!("{id} does not use {permission}"),
                )
            })?;
        if need.required() {
            return if granted {
                Ok(false)
            } else {
                Err(ErrorObject::new(
                    ErrorCode::PermissionRequired,
                    format!("{id} needs {permission}"),
                ))
            };
        }
        let mut denied = self.denied.lock().unwrap();
        let mut next = denied.clone();
        let set = next.entry(id.to_owned()).or_default();
        let changed = if granted {
            set.remove(permission)
        } else {
            set.insert(permission.to_owned())
        };
        if set.is_empty() {
            next.remove(id);
        }
        if !changed {
            return Ok(false);
        }
        if let Some(path) = &self.path {
            save(path, &next).map_err(|e| {
                ErrorObject::new(
                    ErrorCode::InternalError,
                    format!("cannot save {}: {e}", path.display()),
                )
            })?;
        }
        *denied = next;
        Ok(true)
    }
}

fn load(path: &Path) -> BTreeMap<String, BTreeSet<String>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return BTreeMap::new(),
        Err(e) => {
            warn!(file = %path.display(), "ignoring app permissions: {e}");
            return BTreeMap::new();
        }
    };
    match serde_json::from_slice::<File>(&bytes) {
        Ok(f) if f.version == VERSION => f.denied,
        Ok(f) => {
            warn!(file = %path.display(), version = f.version, "ignoring app permissions: unknown version");
            BTreeMap::new()
        }
        Err(e) => {
            warn!(file = %path.display(), "ignoring app permissions: {e}");
            BTreeMap::new()
        }
    }
}

/// Write the file atomically (a temporary file in the same directory, then rename).
fn save(path: &Path, denied: &BTreeMap<String, BTreeSet<String>>) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let file = File {
        version: VERSION,
        denied: denied.clone(),
    };
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(&mut tmp, &file)?;
    tmp.write_all(b"\n")?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared(pairs: &[(&str, Need)]) -> BTreeMap<String, Need> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn on() -> SandboxConfig {
        SandboxConfig {
            enable: true,
            pulse_record_socket: "pulse/record".into(),
            ..SandboxConfig::default()
        }
    }

    #[test]
    fn only_enforced_permissions_are_listed() {
        assert!(
            enforced(&SandboxConfig::default()).is_empty(),
            "sandbox off"
        );
        assert_eq!(enforced(&on()), ["gamepad", "mic", "downloads"]);
        let same = SandboxConfig {
            enable: true,
            ..SandboxConfig::default()
        };
        assert_eq!(
            enforced(&same),
            ["gamepad", "downloads"],
            "no record socket"
        );
    }

    #[test]
    fn lists_in_display_order_and_stores_only_denials() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("state/hinteros/permissions.json");
        let p = Permissions::new(Some(path.clone()), enforced(&on()));
        let d = declared(&[
            ("downloads", Need::Optional),
            ("mic", Need::Optional),
            ("gamepad", Need::Required),
            ("lan", Need::Optional),
        ]);
        let view = |p: &Permissions| -> Vec<(String, bool, bool)> {
            p.list("app", &d)
                .into_iter()
                .map(|a| (a.name, a.required, a.granted))
                .collect()
        };
        let all = [
            ("gamepad".to_string(), true, true),
            ("mic".to_string(), false, true),
            ("downloads".to_string(), false, true),
        ];
        assert_eq!(view(&p), all, "no record: everything granted");
        assert!(!path.exists());

        assert!(p.set("app", Some(&d), "mic", false).unwrap());
        assert!(!p.set("app", Some(&d), "mic", false).unwrap(), "no change");
        assert_eq!(view(&p)[1], ("mic".to_string(), false, false));
        assert_eq!(
            p.grants("app", &d),
            Grants {
                gamepad: true,
                mic: false,
                downloads: true
            }
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"version\":1,\"denied\":{\"app\":[\"mic\"]}}\n"
        );
        // Another instance reads it back; granting again removes the record.
        let p2 = Permissions::new(Some(path.clone()), enforced(&on()));
        assert!(!view(&p2)[1].2);
        assert!(p2.set("app", Some(&d), "mic", true).unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"version\":1,\"denied\":{}}\n"
        );
        assert_eq!(view(&p2), all);
    }

    #[test]
    fn set_refuses_what_it_should() {
        let p = Permissions::new(None, enforced(&on()));
        let d = declared(&[("gamepad", Need::Required), ("mic", Need::Optional)]);
        let code = |r: Result<bool, ErrorObject>| r.unwrap_err().kind().unwrap();
        assert_eq!(
            code(p.set("nope", None, "mic", false)),
            ErrorCode::UnknownApp
        );
        assert_eq!(
            code(p.set("app", Some(&d), "downloads", false)),
            ErrorCode::UnknownPermission
        );
        assert_eq!(
            code(p.set("app", Some(&d), "lan", false)),
            ErrorCode::UnknownPermission
        );
        assert_eq!(
            code(p.set("app", Some(&d), "gamepad", false)),
            ErrorCode::PermissionRequired
        );
        assert!(!p.set("app", Some(&d), "gamepad", true).unwrap());
        // mic not enforced: unknown.
        let same = Permissions::new(
            None,
            enforced(&SandboxConfig {
                enable: true,
                ..SandboxConfig::default()
            }),
        );
        assert_eq!(
            code(same.set("app", Some(&d), "mic", false)),
            ErrorCode::UnknownPermission
        );
        assert!(same.list("app", &d).iter().all(|a| a.name != "mic"));
        // Sandbox off: nothing listed, nothing granted.
        let off = Permissions::default();
        assert!(off.list("app", &d).is_empty());
        assert_eq!(off.grants("app", &d), Grants::default());
    }

    #[test]
    fn a_denied_permission_that_became_required_is_granted() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("permissions.json");
        std::fs::write(&path, r#"{"version":1,"denied":{"app":["gamepad","mic"]}}"#).unwrap();
        let p = Permissions::new(Some(path.clone()), enforced(&on()));
        let d = declared(&[("gamepad", Need::Required), ("mic", Need::Optional)]);
        let g = p.grants("app", &d);
        assert!(g.gamepad && !g.mic);
        // Broken or future files: everything granted.
        for bad in ["{", r#"{"version":2,"denied":{"app":["mic"]}}"#] {
            std::fs::write(&path, bad).unwrap();
            let p = Permissions::new(Some(path.clone()), enforced(&on()));
            assert!(p.grants("app", &d).mic, "{bad}");
        }
    }
}
