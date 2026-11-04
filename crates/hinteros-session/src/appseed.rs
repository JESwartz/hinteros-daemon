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
//! write one of our own over it. Files 0600, directories 0700. The app owns its home, so no
//! symlink there is ever followed (see [`seed`]).
//!
//! Any failure launches the app unchanged (logged). The token is never logged.

use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hinteros_api::system::{JellyfinClientToken, JellyfinClientTokenResult};
use hinteros_api::{ErrorCode, Method};
use rustix::fs::{
    AtFlags, Dir, FileType, Mode, OFlags, fstat, mkdirat, openat, renameat, statat, unlinkat,
};
use rustix::io::Errno;
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
/// The client's files we read are small; a larger one is not the client's.
const MAX_FILE: u64 = 1 << 20;
/// The conf's file name in a client profile directory.
const CONF: &str = "jellyfin-desktop.conf";
const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

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
///
/// The sandboxed app owns everything below `home` and could plant symlinks there that point
/// out of its sandbox (an absolute link resolves to the real home out here). So no path is
/// resolved: every component is opened relative to its parent with `O_NOFOLLOW`, files are read
/// only when regular (and small), written through a temporary file and `renameat` in the same
/// directory. A symlink, or anything that is not what we expect, fails the seed.
pub fn seed(home: &Path, t: &JellyfinClientTokenResult) -> io::Result<PathBuf> {
    let home_fd = rustix::fs::open(home, DIR_FLAGS, Mode::empty())?;
    let mut base_fd = home_fd;
    for name in [".local", "share", "jellyfin-desktop"] {
        base_fd = subdir(&base_fd, name)?;
    }
    let base = home.join(".local/share/jellyfin-desktop");
    // The client's own choice: profiles.json, else the first profile directory, else ours.
    let named = read_small(&base_fd, "profiles.json")?
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["defaultProfile"].as_str().map(str::to_owned))
        .filter(|id| is_profile_id(id));
    let profiles_fd = subdir(&base_fd, "profiles")?;
    let id = match named {
        Some(id) => id,
        None => {
            let mut dirs: Vec<String> = Vec::new();
            let mut entries = Dir::read_from(&profiles_fd)?;
            while let Some(entry) = entries.read() {
                if let Ok(n) = entry?.file_name().to_str()
                    && is_profile_id(n)
                {
                    dirs.push(n.to_owned());
                }
            }
            dirs.sort();
            let id = dirs
                .into_iter()
                .next()
                .unwrap_or_else(|| OUR_PROFILE.into());
            write_private(
                &base_fd,
                "profiles.json",
                &serde_json::to_vec(&json!({ "defaultProfile": id }))?,
            )?;
            id
        }
    };
    let dir_fd = subdir(&profiles_fd, &id)?;
    match statat(&dir_fd, "profile.json", AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => {}
        Err(Errno::NOENT) => write_private(&dir_fd, "profile.json", br#"{"name":"Default"}"#)?,
        Err(e) => return Err(e.into()),
    }
    let before = read_small(&dir_fd, CONF)?;
    let text = patch_conf(before.as_deref(), &seed_url(t));
    write_private(&dir_fd, CONF, text.as_bytes())?;
    Ok(base.join("profiles").join(&id).join(CONF))
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

/// The subdirectory `name` of `dir`, made 0700 when missing; never through a symlink.
fn subdir(dir: &OwnedFd, name: &str) -> io::Result<OwnedFd> {
    match mkdirat(dir, name, Mode::from_raw_mode(0o700)) {
        Ok(()) | Err(Errno::EXIST) => {}
        Err(e) => return Err(e.into()),
    }
    Ok(openat(dir, name, DIR_FLAGS, Mode::empty())?)
}

/// The regular file `name` in `dir` (`None` when missing); never through a symlink, never a
/// FIFO or device, at most [`MAX_FILE`] bytes.
fn read_small(dir: &OwnedFd, name: &str) -> io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let fd = match openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let st = fstat(&fd)?;
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
        return Err(io::Error::other(format!("{name} is not a regular file")));
    }
    if st.st_size as u64 > MAX_FILE {
        return Err(io::Error::other(format!("{name} is too large")));
    }
    let mut bytes = Vec::new();
    std::fs::File::from(fd)
        .take(MAX_FILE)
        .read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

/// Write `name` in `dir` 0600 through a temporary file in the same directory (never a
/// half-written conf). The rename replaces a symlink at `name`, never its target.
fn write_private(dir: &OwnedFd, name: &str, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let tmp = format!("{name}.hinteros-tmp");
    let _ = unlinkat(dir, tmp.as_str(), AtFlags::empty());
    let fd = openat(
        dir,
        tmp.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?;
    let mut f = std::fs::File::from(fd);
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    Ok(renameat(dir, tmp.as_str(), dir, name)?)
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
    fn symlinks_the_app_planted_are_never_followed() {
        use std::os::unix::fs::symlink;
        let id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let profiles_json = format!(r#"{{"defaultProfile":"{id}"}}"#);
        // Each case: a home with one planted link to `outside`, which must stay as it was.
        type Plant = fn(&Path, &Path);
        let cases: [(&str, Plant); 4] = [
            ("the conf", |base, outside| {
                std::fs::create_dir_all(base.join("profiles/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"))
                    .unwrap();
                symlink(
                    outside.join("secret.json"),
                    base.join("profiles/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/jellyfin-desktop.conf"),
                )
                .unwrap();
            }),
            ("the profile directory", |base, outside| {
                std::fs::create_dir_all(base.join("profiles")).unwrap();
                symlink(
                    outside,
                    base.join("profiles/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                )
                .unwrap();
            }),
            ("the data directory", |base, outside| {
                std::fs::remove_dir_all(base).unwrap();
                symlink(outside, base).unwrap();
            }),
            ("profiles.json", |base, outside| {
                std::fs::create_dir_all(base).unwrap();
                symlink(outside.join("secret.json"), base.join("profiles.json")).unwrap();
            }),
        ];
        for (what, plant) in cases {
            let home = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let secret = outside.path().join("secret.json");
            std::fs::write(&secret, r#"{"secret": 1}"#).unwrap();
            let base = home.path().join(".local/share/jellyfin-desktop");
            std::fs::create_dir_all(&base).unwrap();
            if what != "profiles.json" {
                std::fs::write(base.join("profiles.json"), &profiles_json).unwrap();
            }
            plant(&base, outside.path());
            assert!(seed(home.path(), &token()).is_err(), "{what}");
            let left: Vec<_> = std::fs::read_dir(outside.path())
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(left, ["secret.json"], "{what}: nothing written outside");
            assert_eq!(
                std::fs::read(&secret).unwrap(),
                br#"{"secret": 1}"#,
                "{what}"
            );
            // Nothing of the secret came into the home either.
            let conf = base.join("profiles").join(id).join("jellyfin-desktop.conf");
            if let Ok(m) = std::fs::symlink_metadata(&conf) {
                assert!(m.file_type().is_symlink(), "{what}: the link is left alone");
            }
        }
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
