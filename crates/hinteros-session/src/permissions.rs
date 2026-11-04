//! App permissions: what each sandboxed app may use, per profile (`apps.set_permission`).
//!
//! The catalog declares each app's permissions ([`hinteros_api::catalog::PERMISSIONS`]), each
//! `required` or `optional`. The profile's choices are kept in
//! `$XDG_STATE_HOME/hinteros/permissions.json` (default `~/.local/state/hinteros/`):
//! `{"version":1,"denied":{"<app id>":["mic"]}}`. Only opt-outs are stored, so an app with no
//! record has every permission (existing installs and starter sets keep working). A required
//! permission is never denied. A change applies the next time the app starts.
//!
//! The file fails closed: when it is there but cannot be read, does not parse, or is newer than
//! this daemon (a rollback), every optional permission is denied, no change is accepted and the
//! file is never written. Apps still start, with their required permissions only. The format
//! grows only by new fields (ignored here, kept on a rewrite), so `version` stays 1 unless a
//! change cannot be read by 1.0.
//!
//! Only enforced permissions are listed: none while the sandbox is off, not `mic` while the
//! sound socket for `mic` is the same as the one every app gets, and not `lan` without the local
//! network filter (`[sandbox] lan_filter`, [`crate::lan`]).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hinteros_api::catalog::{Need, PERMISSIONS};
use hinteros_api::common::check_version;
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
    /// The local network. Always true while `lan` is not enforced: then nothing keeps an app off
    /// it.
    pub lan: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct File {
    version: u32,
    #[serde(default)]
    denied: BTreeMap<String, BTreeSet<String>>,
    /// Fields a later 1.x added: kept on a rewrite.
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
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
        .filter(|p| *p != "lan" || cfg.lan_filter)
        .collect()
}

/// The stored choices; cheap to clone (the shell and the Store share one).
#[derive(Debug, Clone)]
pub struct Permissions {
    path: Option<PathBuf>,
    enforced: Vec<&'static str>,
    stored: Arc<Mutex<File>>,
    /// Why the file cannot be used (see the module doc), when it cannot.
    locked: Option<Arc<str>>,
}

impl Default for Permissions {
    /// Nothing enforced, nothing stored.
    fn default() -> Self {
        Self::new(None, Vec::new())
    }
}

impl Permissions {
    /// Read `path` (missing: nothing denied). A file that is unreadable, broken or newer locks
    /// the choices: every optional permission denied, no change accepted, nothing written.
    pub fn new(path: Option<PathBuf>, enforced: Vec<&'static str>) -> Self {
        let (stored, locked) = match path.as_deref().map(load) {
            None => (File::default(), None),
            Some(Ok(f)) => (f, None),
            Some(Err(e)) => {
                warn!("app permissions locked, optional ones denied: {e}");
                (File::default(), Some(Arc::from(e)))
            }
        };
        Self {
            path,
            enforced,
            stored: Arc::new(Mutex::new(stored)),
            locked,
        }
    }

    pub fn from_config(cfg: &SandboxConfig) -> Self {
        Self::new(default_path(), enforced(cfg))
    }

    /// Whether apps run in the sandbox on this box (`[sandbox] enable`).
    pub fn sandboxed(&self) -> bool {
        !self.enforced.is_empty()
    }

    /// An app's permissions for `apps.list` / `store.list`: the declared ones this box
    /// enforces, in display order.
    pub fn list(&self, id: &str, declared: &BTreeMap<String, Need>) -> Vec<AppPermission> {
        let stored = self.stored.lock().unwrap();
        let denied = stored.denied.get(id);
        self.enforced
            .iter()
            .filter_map(|name| {
                let required = declared.get(*name)?.required();
                Some(AppPermission {
                    name: (*name).to_owned(),
                    required,
                    granted: required
                        || (self.locked.is_none() && !denied.is_some_and(|d| d.contains(*name))),
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
            lan: has("lan") || !self.enforced.contains(&"lan"),
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
            ErrorObject::new(ErrorCode::AppNotFound, format!("no app with id '{id}'"))
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
        if let Some(why) = &self.locked {
            // Never written over: recovery is from the console.
            return Err(ErrorObject::new(
                ErrorCode::InternalError,
                format!("app permissions cannot change: {why}"),
            ));
        }
        let mut stored = self.stored.lock().unwrap();
        let mut next = stored.clone();
        let set = next.denied.entry(id.to_owned()).or_default();
        let changed = if granted {
            set.remove(permission)
        } else {
            set.insert(permission.to_owned())
        };
        if set.is_empty() {
            next.denied.remove(id);
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
        *stored = next;
        Ok(true)
    }
}

/// The file at `path` (missing: nothing denied); `Err` (why) when it is there but cannot be used.
fn load(path: &Path) -> Result<File, String> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(File::default()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    check_version(path, &bytes, VERSION).map_err(|e| e.to_string())?;
    serde_json::from_slice::<File>(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// Write the file atomically (a temporary file in the same directory, then rename).
fn save(path: &Path, stored: &File) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let file = File {
        version: VERSION,
        ..stored.clone()
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
        let lan = SandboxConfig {
            lan_filter: true,
            ..on()
        };
        assert_eq!(enforced(&lan), ["gamepad", "mic", "downloads", "lan"]);
        let off = SandboxConfig {
            lan_filter: true,
            ..SandboxConfig::default()
        };
        assert!(enforced(&off).is_empty(), "sandbox off, filter or not");
    }

    #[test]
    fn lan_is_granted_only_when_declared_and_not_denied() {
        let lan = enforced(&SandboxConfig {
            lan_filter: true,
            ..on()
        });
        let p = Permissions::new(None, lan.clone());
        let optional = declared(&[("lan", Need::Optional)]);
        let required = declared(&[("lan", Need::Required)]);
        let none = declared(&[("gamepad", Need::Required)]);
        assert!(p.grants("a", &optional).lan, "granted by default");
        assert!(p.grants("a", &required).lan);
        assert!(!p.grants("a", &none).lan, "not declared: no local network");
        assert!(p.set("a", Some(&optional), "lan", false).unwrap());
        assert!(!p.grants("a", &optional).lan, "denied");
        assert_eq!(
            p.set("a", Some(&required), "lan", false)
                .unwrap_err()
                .kind()
                .unwrap(),
            ErrorCode::PermissionRequired
        );
        assert_eq!(
            p.list("a", &optional),
            [AppPermission {
                name: "lan".into(),
                required: false,
                granted: false
            }]
        );
        // Not enforced (no filter): not listed, and nothing keeps any app off the network.
        let p = Permissions::new(None, enforced(&on()));
        assert!(p.list("a", &optional).is_empty());
        assert!(p.grants("a", &none).lan && p.grants("a", &optional).lan);
        assert_eq!(
            p.set("a", Some(&optional), "lan", false)
                .unwrap_err()
                .kind()
                .unwrap(),
            ErrorCode::UnknownPermission
        );
        assert!(Permissions::default().grants("a", &none).lan);
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
                downloads: true,
                // Not enforced here: nothing keeps the app off the local network.
                lan: true,
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
            ErrorCode::AppNotFound
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
        assert_eq!(
            off.grants("app", &d),
            Grants {
                lan: true,
                ..Grants::default()
            },
            "nothing granted but the network, which nothing restricts then"
        );
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
    }

    #[test]
    fn a_file_that_cannot_be_used_fails_closed() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("permissions.json");
        let d = declared(&[
            ("gamepad", Need::Required),
            ("mic", Need::Optional),
            ("downloads", Need::Optional),
        ]);
        let lan = enforced(&SandboxConfig {
            lan_filter: true,
            ..on()
        });
        let ld = declared(&[("gamepad", Need::Required), ("lan", Need::Optional)]);
        // Broken, newer (a rollback) or unreadable (a directory): optional ones denied, the
        // required one kept, no change accepted, the file left as it was.
        for bad in [
            "{",
            "",
            r#"{"denied":{}}"#,
            r#"{"version":1,"denied":{"app":"mic"}}"#,
            r#"{"version":2,"denied":{}}"#,
        ] {
            std::fs::write(&path, bad).unwrap();
            let p = Permissions::new(Some(path.clone()), enforced(&on()));
            assert_eq!(
                p.grants("app", &d),
                Grants {
                    gamepad: true,
                    lan: true,
                    ..Grants::default()
                },
                "{bad}"
            );
            assert!(p.list("app", &d).iter().all(|a| a.granted == a.required));
            for granted in [true, false] {
                assert_eq!(
                    p.set("app", Some(&d), "mic", granted)
                        .unwrap_err()
                        .kind()
                        .unwrap(),
                    ErrorCode::InternalError,
                    "{bad}"
                );
            }
            assert!(!p.set("app", Some(&d), "gamepad", true).unwrap());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), bad);
            let p = Permissions::new(Some(path.clone()), lan.clone());
            assert!(!p.grants("app", &ld).lan, "{bad}: no local network");
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let p = Permissions::new(Some(path.clone()), enforced(&on()));
        assert!(!p.grants("app", &d).mic && p.grants("app", &d).gamepad);
        assert!(path.is_dir());
    }

    #[test]
    fn fields_a_later_version_added_are_kept() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("permissions.json");
        std::fs::write(
            &path,
            r#"{"version":1,"denied":{"app":["mic"]},"granted_at":{"app":3}}"#,
        )
        .unwrap();
        let p = Permissions::new(Some(path.clone()), enforced(&on()));
        let d = declared(&[("mic", Need::Optional), ("downloads", Need::Optional)]);
        assert!(!p.grants("app", &d).mic && p.grants("app", &d).downloads);
        assert!(p.set("app", Some(&d), "downloads", false).unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"version\":1,\"denied\":{\"app\":[\"downloads\",\"mic\"]},\"granted_at\":{\"app\":3}}\n"
        );
    }
}
