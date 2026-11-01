//! The Jellyfin app signs in to this box's own Jellyfin server by itself (server.md, spike S1
//! "seed page"). Before `apps.launch` of the catalog app `jellyfin-desktop` (not when it runs already, not
//! without `lan`): ask the system service for this profile's token (`jellyfin.client_token`,
//! refused while `jellyfin-server` is off or not set up), then point the client's settings at
//! the server's seed page (`/web/hinteros/signin.html`, the system repo's), which writes
//! jellyfin-web's own login into the page's storage and opens the web UI.
//!
//! The client is jellyfin-desktop 2.x: `~/.local/share/jellyfin-desktop/profiles.json`
//! (`defaultProfile`, 32 hex digits) names `profiles/<id>/jellyfin-desktop.conf`, JSON
//! `{"version": 7, "sections": {...}}`. Only three keys are ours: `path.startupurl_desktop`
//! (the seed URL; the client loads it instead of its server picker), `main.userWebClient` (the
//! saved server) and `main.layout` (`tv`). Everything else, and the `version` the app wrote, is
//! kept: a version the client does not know makes it move the file to `.broken`, so we never
//! write one of our own over it. Files 0600, directories 0700.
//!
//! Any failure launches the app unchanged (logged). The token is never logged.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hinteros_api::system::{JellyfinClientToken, JellyfinClientTokenResult};
use hinteros_api::{ErrorCode, Method};
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::shell::Msg;
use crate::system::Relay;

/// The catalog app this is for.
pub const APP_ID: &str = "jellyfin-desktop";
/// This box's Jellyfin server, as the app reaches it.
pub const SERVER: &str = "http://127.0.0.1:8096";
/// The seed page on the server (the system repo binds it into Jellyfin's web directory).
pub const SEED_PAGE: &str = "/web/hinteros/signin.html";
/// The conf version jellyfin-desktop 2.0 writes, for a conf we make.
const CONF_VERSION: u64 = 7;
/// The client profile we make when the app has none yet ("hinteros" in hex, padded).
const OUR_PROFILE: &str = "68696e7465726f730000000000000000";
/// A launch never waits longer than this for the token.
const TOKEN_WAIT: Duration = Duration::from_secs(10);

/// Whether this request is a launch of the app ([`APP_ID`]).
pub fn wants(method: &str, params: Option<&Value>) -> bool {
    method == hinteros_api::session::AppsLaunch::NAME
        && params.and_then(|p| p.get("id")).and_then(Value::as_str) == Some(APP_ID)
}

/// Before the launch: seed the app's settings when the box's server is on (best effort).
pub async fn before_launch(shell: &mpsc::UnboundedSender<Msg>, relay: &Relay) {
    let (reply, rx) = oneshot::channel();
    if shell
        .send(Msg::SeedHome {
            id: APP_ID.into(),
            reply,
        })
        .is_err()
    {
        return;
    }
    let Ok(Some(home)) = rx.await else {
        return;
    };
    let token =
        match tokio::time::timeout(TOKEN_WAIT, relay.call(JellyfinClientToken::NAME, None)).await {
            Ok(Ok(v)) => v,
            Ok(Err(e))
                if e.code == ErrorCode::Unavailable.code()
                    || e.code == ErrorCode::SystemUnavailable.code() =>
            {
                debug!("Jellyfin app: no sign-in from this box ({})", e.message);
                return;
            }
            Ok(Err(e)) => {
                warn!("Jellyfin app: no sign-in token: {}", e.message);
                return;
            }
            Err(_) => {
                warn!(
                    "Jellyfin app: no sign-in token in {} s",
                    TOKEN_WAIT.as_secs()
                );
                return;
            }
        };
    let token: JellyfinClientTokenResult = match serde_json::from_value(token) {
        Ok(t) => t,
        Err(e) => {
            warn!("Jellyfin app: bad sign-in token reply: {e}");
            return;
        }
    };
    match tokio::task::spawn_blocking(move || seed(&home, &token)).await {
        Ok(Ok(conf)) => {
            info!(conf = %conf.display(), "Jellyfin app: signs in to this box's server")
        }
        Ok(Err(e)) => warn!("Jellyfin app: settings not written, it starts unchanged: {e}"),
        Err(e) => warn!("Jellyfin app: {e}"),
    }
}

/// The seed page's URL. The token goes in the fragment (never sent to the server) as
/// `AccessToken=`, which the client's own log masks.
pub fn seed_url(t: &JellyfinClientTokenResult) -> String {
    let e = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    format!(
        "{SERVER}{SEED_PAGE}#s={}&u={}&d={}&AccessToken={}",
        e(&t.server_id),
        e(&t.user_id),
        e(&t.device_id),
        e(&t.access_token)
    )
}

fn is_profile_id(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Write the client's settings in the app's data home `home` (its sandbox home, or the profile's
/// home). Returns the conf's path.
pub fn seed(home: &Path, t: &JellyfinClientTokenResult) -> io::Result<PathBuf> {
    let base = home.join(".local/share/jellyfin-desktop");
    mkdirs(&base)?;
    let profiles_json = base.join("profiles.json");
    // The client's own choice: profiles.json, else the first profile directory, else ours.
    let named = std::fs::read(&profiles_json)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["defaultProfile"].as_str().map(str::to_owned))
        .filter(|id| is_profile_id(id));
    let id = match named {
        Some(id) => id,
        None => {
            let mut dirs: Vec<String> = std::fs::read_dir(base.join("profiles"))
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|n| is_profile_id(n))
                .collect();
            dirs.sort();
            let id = dirs
                .into_iter()
                .next()
                .unwrap_or_else(|| OUR_PROFILE.into());
            write_private(
                &profiles_json,
                &serde_json::to_vec(&json!({ "defaultProfile": id }))?,
            )?;
            id
        }
    };
    let dir = base.join("profiles").join(&id);
    mkdirs(&dir)?;
    let profile_json = dir.join("profile.json");
    if !profile_json.exists() {
        write_private(&profile_json, br#"{"name":"Default"}"#)?;
    }
    let conf = dir.join("jellyfin-desktop.conf");
    let before = match std::fs::read(&conf) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let text = patch_conf(before.as_deref(), &seed_url(t));
    write_private(&conf, text.as_bytes())?;
    Ok(conf)
}

/// The conf with our three keys set; the rest (and the version) as it was. A conf that is not
/// a JSON object is replaced by one with only our keys.
pub fn patch_conf(before: Option<&[u8]>, url: &str) -> String {
    let mut root = before
        .and_then(|b| serde_json::from_slice::<Value>(b).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default();
    if !root.get("version").is_some_and(Value::is_u64) {
        root.insert("version".into(), CONF_VERSION.into());
    }
    let sections = section(&mut root, "sections");
    section(sections, "main").extend([
        ("userWebClient".to_owned(), Value::from(SERVER)),
        ("layout".to_owned(), Value::from("tv")),
    ]);
    section(sections, "path").insert("startupurl_desktop".into(), url.into());
    let mut text = serde_json::to_string_pretty(&Value::Object(root)).unwrap_or_default();
    text.push('\n');
    text
}

/// The object at `key` (made, or replacing a non-object).
fn section<'a>(m: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    let v = m
        .entry(key)
        .and_modify(|v| {
            if !v.is_object() {
                *v = Value::Object(Map::new());
            }
        })
        .or_insert_with(|| Value::Object(Map::new()));
    v.as_object_mut().expect("an object")
}

fn mkdirs(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// Write `path` 0600 through a temporary file in the same directory (never a half-written conf).
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".hinteros-tmp");
    let tmp = PathBuf::from(tmp);
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn token() -> JellyfinClientTokenResult {
        JellyfinClientTokenResult {
            server_id: "0123456789abcdef0123456789abcdef".into(),
            user_id: "fedcba9876543210fedcba9876543210".into(),
            device_id: "hinteros-p1".into(),
            access_token: "a+b/c".into(),
        }
    }

    fn read(p: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
    }

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn wants_only_the_jellyfin_launch() {
        assert!(wants(
            "apps.launch",
            Some(&json!({"id": "jellyfin-desktop"}))
        ));
        assert!(!wants("apps.launch", Some(&json!({"id": "youtube"}))));
        assert!(!wants(
            "apps.launch",
            Some(&json!({"command": ["jellyfin"]}))
        ));
        assert!(!wants("apps.launch", None));
        assert!(!wants(
            "apps.close",
            Some(&json!({"id": "jellyfin-desktop"}))
        ));
    }

    #[test]
    fn the_url_carries_the_login_in_the_fragment() {
        assert_eq!(
            seed_url(&token()),
            "http://127.0.0.1:8096/web/hinteros/signin.html#s=0123456789abcdef0123456789abcdef\
             &u=fedcba9876543210fedcba9876543210&d=hinteros-p1&AccessToken=a%2Bb%2Fc"
        );
    }

    #[test]
    fn a_new_home_gets_a_profile_and_a_conf() {
        let t = tempfile::tempdir().unwrap();
        let conf = seed(t.path(), &token()).unwrap();
        let base = t.path().join(".local/share/jellyfin-desktop");
        assert_eq!(
            read(&base.join("profiles.json")),
            json!({"defaultProfile": OUR_PROFILE})
        );
        assert_eq!(
            conf,
            base.join("profiles")
                .join(OUR_PROFILE)
                .join("jellyfin-desktop.conf")
        );
        assert_eq!(
            read(&base.join("profiles").join(OUR_PROFILE).join("profile.json")),
            json!({"name": "Default"})
        );
        let c = read(&conf);
        assert_eq!(c["version"], 7);
        assert_eq!(c["sections"]["main"]["userWebClient"], SERVER);
        assert_eq!(c["sections"]["main"]["layout"], "tv");
        assert_eq!(
            c["sections"]["path"]["startupurl_desktop"],
            seed_url(&token())
        );
        for p in [&conf, &base.join("profiles.json")] {
            assert_eq!(mode(p), 0o600, "{}", p.display());
        }
        assert_eq!(mode(&base), 0o700);
        assert_eq!(mode(conf.parent().unwrap()), 0o700);
    }

    #[test]
    fn the_apps_own_profile_and_settings_are_kept() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().join(".local/share/jellyfin-desktop");
        let id = "0123456789abcdef0123456789abcdef";
        let dir = base.join("profiles").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            base.join("profiles.json"),
            format!(r#"{{"defaultProfile":"{id}"}}"#),
        )
        .unwrap();
        std::fs::write(dir.join("profile.json"), r#"{"name":"Mine"}"#).unwrap();
        std::fs::write(
            dir.join("jellyfin-desktop.conf"),
            r#"{"version": 8, "sections": {"main": {"fullscreen": true, "layout": "desktop"}, "audio": {"channels": "5.1"}, "path": "x"}}"#,
        )
        .unwrap();
        let conf = seed(t.path(), &token()).unwrap();
        assert_eq!(conf, dir.join("jellyfin-desktop.conf"));
        let c = read(&conf);
        assert_eq!(c["version"], 8, "the app's version stays");
        assert_eq!(c["sections"]["main"]["fullscreen"], true);
        assert_eq!(c["sections"]["main"]["layout"], "tv");
        assert_eq!(c["sections"]["audio"]["channels"], "5.1");
        assert_eq!(
            c["sections"]["path"]["startupurl_desktop"],
            seed_url(&token())
        );
        assert_eq!(read(&dir.join("profile.json")), json!({"name": "Mine"}));
        assert_eq!(mode(&conf), 0o600);
        // Seeding again changes only the URL.
        let mut t2 = token();
        t2.access_token = "new".into();
        seed(t.path(), &t2).unwrap();
        assert_eq!(
            read(&conf)["sections"]["path"]["startupurl_desktop"],
            seed_url(&t2)
        );
        assert_eq!(read(&conf)["version"], 8);
    }

    #[test]
    fn without_profiles_json_the_first_profile_directory_is_used() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().join(".local/share/jellyfin-desktop");
        for id in [
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "not-a-profile",
        ] {
            std::fs::create_dir_all(base.join("profiles").join(id)).unwrap();
        }
        let conf = seed(t.path(), &token()).unwrap();
        assert!(conf.starts_with(base.join("profiles/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")));
        assert_eq!(
            read(&base.join("profiles.json")),
            json!({"defaultProfile": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"})
        );
    }

    #[test]
    fn a_broken_conf_is_replaced() {
        for before in [
            &b"not json"[..],
            b"[1]",
            br#"{"version": "7", "sections": 3}"#,
        ] {
            let c: Value = serde_json::from_str(&patch_conf(Some(before), "u")).unwrap();
            assert_eq!(c["version"], 7);
            assert_eq!(c["sections"]["path"]["startupurl_desktop"], "u");
        }
    }
}
