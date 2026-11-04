//! Jellyfin: its HTTP API on loopback (with the API key the system repo's setup unit made) and
//! one Jellyfin user per HinterOS profile.
//!
//! The setup unit (`hinteros-jellyfin-setup.service`) writes, in
//! `/var/lib/hinteros/services/jellyfin-server/` (0700 root): `setup-done` (the first-run wizard is
//! done), `admin-password` (user `admin`) and `api-key`. API keys count as administrator in
//! Jellyfin and are sent as `Authorization: MediaBrowser Token="<key>"` (Jellyfin 10.11+ refuses
//! the legacy `X-Emby-Token`). JSON is PascalCase; ids are 32 lowercase hex digits. The key,
//! passwords and tokens go only to a server that runs as the [`USER`] account (port 8096 is not
//! privileged: while Jellyfin is stopped, any local process could listen on it).
//!
//! Profile users: `users.json` (0600) maps a profile id to `{jellyfin_user_id, name, password}`.
//! [`sync`] makes Jellyfin match the profiles: a user for every profile (named after the
//! profile, made valid and unique), renamed with it, deleted with it (Jellyfin's own data of
//! that user only; media files are never touched). Users made in Jellyfin's dashboard are left
//! alone. New users are not administrators and may not delete media.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hinteros_api::common::{check_file_version, check_version};
use hinteros_api::system::{JellyfinLibrary, LibraryKind, LibraryKindShown};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use super::http::{self, Body, Target};
use crate::fsutil;

/// The catalog id.
pub const ID: &str = "jellyfin-server";
/// The administrator the setup unit makes.
pub const ADMIN_USER: &str = "admin";
/// The system account Jellyfin runs as (NixOS `services.jellyfin.user`).
pub const USER: &str = "jellyfin";
/// Longest library name.
pub const MAX_LIBRARY_NAME: usize = 64;

/// The files of `/var/lib/hinteros/services/jellyfin-server`.
#[derive(Debug, Clone)]
pub struct Files {
    pub dir: PathBuf,
}

impl Files {
    pub fn new(services_dir: &Path) -> Self {
        Self {
            dir: services_dir.join(ID),
        }
    }
    pub fn setup_done(&self) -> bool {
        self.dir.join("setup-done").exists()
    }
    pub fn users_json(&self) -> PathBuf {
        self.dir.join("users.json")
    }
    fn lock_path(&self) -> PathBuf {
        self.dir.join("users.lock")
    }
    pub fn libraries_cache(&self) -> PathBuf {
        self.dir.join("libraries.json")
    }
    fn secret(&self, name: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.dir.join(name)).ok()?;
        let t = text.trim();
        (!t.is_empty() && t.len() <= 256 && t.bytes().all(|b| b.is_ascii_graphic()))
            .then(|| t.to_owned())
    }
    pub fn admin_password(&self) -> Option<String> {
        self.secret("admin-password")
    }
    /// The API key: letters and digits only (it goes into a header).
    pub fn api_key(&self) -> Option<String> {
        self.secret("api-key")
            .filter(|k| k.bytes().all(|b| b.is_ascii_alphanumeric()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Setup has not finished (no marker or no API key).
    #[error("Jellyfin setup is not done")]
    NotReady,
    #[error("Jellyfin does not answer: {0}")]
    Unreachable(String),
    #[error("Jellyfin refused: {0}")]
    Refused(String),
    /// `state.json` or `profiles.json` cannot be read (damaged, missing, or newer than this
    /// binary): no sync, since an empty profile list would delete every user.
    #[error("the profiles cannot be read: {0}")]
    Profiles(String),
}

impl Error {
    /// The `job_failed` reason.
    pub fn reason(&self) -> &'static str {
        match self {
            Error::NotReady => "not_ready",
            Error::Unreachable(_) => "unreachable",
            Error::Refused(_) | Error::Profiles(_) => "refused",
        }
    }
}

/// `^[0-9a-f]{32}$`: a Jellyfin item or user id.
pub fn is_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A library name: 1 to [`MAX_LIBRARY_NAME`] characters after trimming, no control or format
/// characters, none of `/\:*?"<>|` (Jellyfin makes a folder of that name).
pub fn check_library_name(name: &str) -> Result<String, &'static str> {
    let t = name.trim();
    let n = t.chars().count();
    if n == 0 || n > MAX_LIBRARY_NAME {
        return Err("bad_length");
    }
    if t.chars().any(|c| {
        c.is_control()
            || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
            || matches!(c as u32, 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F | 0xFEFF)
    }) || t == "."
        || t == ".."
    {
        return Err("bad_name");
    }
    Ok(t.to_owned())
}

/// Jellyfin's `collectionType` for a new library (`None`: no type, Jellyfin's mixed content).
pub fn collection_type(kind: LibraryKind) -> Option<&'static str> {
    match kind {
        LibraryKind::Movies => Some("movies"),
        LibraryKind::Shows => Some("tvshows"),
        LibraryKind::Music => Some("music"),
        LibraryKind::Mixed => None,
    }
}

fn kind_shown(collection_type: Option<&str>) -> LibraryKindShown {
    match collection_type {
        Some("movies") => LibraryKindShown::Movies,
        Some("tvshows") => LibraryKindShown::Shows,
        Some("music") => LibraryKindShown::Music,
        None | Some("") | Some("mixed") => LibraryKindShown::Mixed,
        Some(_) => LibraryKindShown::Other,
    }
}

/// One entry of `GET /Library/VirtualFolders`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct VirtualFolder {
    pub name: String,
    #[serde(default)]
    pub locations: Vec<String>,
    #[serde(default)]
    pub collection_type: Option<String>,
    #[serde(default)]
    pub item_id: Option<String>,
}

/// The API's view of the libraries (entries without a valid id are left out).
pub fn libraries(folders: &[VirtualFolder], media_dir: &Path) -> Vec<JellyfinLibrary> {
    folders
        .iter()
        .filter_map(|f| {
            let id = f.item_id.as_deref().filter(|i| is_id(i))?;
            Some(JellyfinLibrary {
                id: id.to_owned(),
                name: f
                    .name
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(128)
                    .collect(),
                kind: kind_shown(f.collection_type.as_deref()),
                folder: f
                    .locations
                    .first()
                    .and_then(|l| super::media::id_of(media_dir, l)),
            })
        })
        .collect()
}

/// The libraries as Jellyfin last listed them, for `services.list` while Jellyfin restarts.
pub fn write_cache(files: &Files, folders: &[VirtualFolder]) {
    if let Ok(bytes) = fsutil::cache_bytes(&folders)
        && let Err(e) = fsutil::write_atomic(&files.libraries_cache(), &bytes, 0o600)
    {
        warn!("cannot cache the Jellyfin libraries: {e}");
    }
}

/// A Jellyfin user, as far as the sync needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: String,
    pub name: String,
}

/// What [`sync`] needs from Jellyfin (the HTTP client, or a fake in tests).
pub trait Api {
    fn users(&self) -> Result<Vec<User>, Error>;
    /// Make a user (not an administrator, no media deletion); returns its id.
    fn create_user(&self, name: &str, password: &str) -> Result<String, Error>;
    fn rename_user(&self, id: &str, name: &str) -> Result<(), Error>;
    fn delete_user(&self, id: &str) -> Result<(), Error>;
}

/// The HTTP client.
pub struct Client {
    pub addr: SocketAddr,
    key: String,
    /// Jellyfin's uid: requests go only to a server running as it.
    owner: u32,
    pub timeout: Duration,
}

impl Client {
    /// A client once setup is done (`Err(NotReady)` before, or without a [`USER`] account).
    pub fn new(
        files: &Files,
        addr: SocketAddr,
        users: &crate::users::Users,
        timeout: Duration,
    ) -> Result<Self, Error> {
        if !files.setup_done() {
            return Err(Error::NotReady);
        }
        let key = files.api_key().ok_or(Error::NotReady)?;
        let owner = users.by_name(USER).ok_or(Error::NotReady)?.uid;
        Ok(Self {
            addr,
            key,
            owner,
            timeout,
        })
    }

    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<http::Response, Error> {
        let auth = format!("MediaBrowser Token=\"{}\"", self.key);
        let bytes = body.map(|b| b.to_string().into_bytes());
        let r = http::request(
            Target::TcpOwnedBy(self.addr, self.owner),
            method,
            path,
            &[("Authorization", &auth), ("Accept", "application/json")],
            bytes.as_deref().map(|b| Body {
                content_type: "application/json",
                bytes: b,
            }),
            self.timeout,
        )
        .map_err(|e| Error::Unreachable(e.to_string()))?;
        if r.ok() {
            Ok(r)
        } else {
            let text: String = String::from_utf8_lossy(&r.body)
                .chars()
                .filter(|c| !c.is_control())
                .take(200)
                .collect();
            Err(Error::Refused(format!(
                "{method} {} HTTP {}: {text}",
                path.split('?').next().unwrap_or(path),
                r.status
            )))
        }
    }

    fn json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        let r = self.call("GET", path, None)?;
        serde_json::from_slice(&r.body).map_err(|e| Error::Refused(format!("GET {path}: {e}")))
    }

    /// Jellyfin is up (`GET /health` says `Healthy`, open without a key). While it starts,
    /// Jellyfin 10.11+ runs a stand-in server on the same port that answers `/health` with
    /// `Degraded` and even `/System/Info/Public`; the real server comes up only after it stops.
    pub fn ping(addr: SocketAddr, timeout: Duration) -> bool {
        http::request(Target::Tcp(addr), "GET", "/health", &[], None, timeout)
            .is_ok_and(|r| r.ok() && r.body.trim_ascii() == b"Healthy")
    }

    pub fn virtual_folders(&self) -> Result<Vec<VirtualFolder>, Error> {
        self.json("/Library/VirtualFolders")
    }

    /// Add a library on one folder (the path goes in the body: a single `paths` query value
    /// would be split at commas).
    pub fn add_library(&self, name: &str, kind: LibraryKind, path: &Path) -> Result<(), Error> {
        let mut q = format!(
            "/Library/VirtualFolders?name={}&refreshLibrary=true",
            http::encode(name)
        );
        if let Some(t) = collection_type(kind) {
            q.push_str(&format!("&collectionType={t}"));
        }
        let path = path
            .to_str()
            .ok_or_else(|| Error::Refused("not a UTF-8 path".into()))?;
        let body = json!({ "LibraryOptions": { "PathInfos": [ { "Path": path } ] } });
        self.call("POST", &q, Some(&body)).map(drop)
    }

    /// Remove a library by name (Jellyfin's own folder for it goes; media files stay).
    pub fn remove_library(&self, name: &str) -> Result<(), Error> {
        let q = format!(
            "/Library/VirtualFolders?name={}&refreshLibrary=true",
            http::encode(name)
        );
        self.call("DELETE", &q, None).map(drop)
    }
}

impl Api for Client {
    fn users(&self) -> Result<Vec<User>, Error> {
        let list: Vec<Value> = self.json("/Users")?;
        Ok(list
            .iter()
            .filter_map(|u| {
                Some(User {
                    id: u["Id"].as_str().filter(|i| is_id(i))?.to_owned(),
                    name: u["Name"].as_str()?.to_owned(),
                })
            })
            .collect())
    }

    fn create_user(&self, name: &str, password: &str) -> Result<String, Error> {
        let r = self.call(
            "POST",
            "/Users/New",
            Some(&json!({ "Name": name, "Password": password })),
        )?;
        let user: Value = serde_json::from_slice(&r.body)
            .map_err(|e| Error::Refused(format!("POST /Users/New: {e}")))?;
        let id = user["Id"]
            .as_str()
            .filter(|i| is_id(i))
            .ok_or_else(|| Error::Refused("POST /Users/New: no id".into()))?
            .to_owned();
        // New users are no administrators and may not delete media already (Jellyfin's
        // defaults); say so explicitly. The whole policy goes back (missing fields reset).
        let mut policy = user["Policy"].clone();
        if policy.is_object() {
            policy["IsAdministrator"] = json!(false);
            policy["EnableContentDeletion"] = json!(false);
            policy["EnableContentDeletionFromFolders"] = json!([]);
            self.call("POST", &format!("/Users/{id}/Policy"), Some(&policy))?;
        }
        Ok(id)
    }

    fn rename_user(&self, id: &str, name: &str) -> Result<(), Error> {
        // The whole user goes back (its Configuration is required).
        let mut user: Value = self.json(&format!("/Users/{id}"))?;
        user["Name"] = json!(name);
        self.call("POST", &format!("/Users?userId={id}"), Some(&user))
            .map(drop)
    }

    fn delete_user(&self, id: &str) -> Result<(), Error> {
        match self.call("DELETE", &format!("/Users/{id}"), None) {
            Err(Error::Refused(m)) if m.contains("HTTP 404") => Ok(()),
            other => other.map(drop),
        }
    }
}

/// One profile's Jellyfin user (`users.json`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub jellyfin_user_id: String,
    pub name: String,
    pub password: String,
    /// The access token `jellyfin.client_token` made for device `hinteros-<profile id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl std::fmt::Debug for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Record")
            .field("jellyfin_user_id", &self.jellyfin_user_id)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Format of `users.json`.
pub const USERS_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsersFile {
    pub version: u32,
    /// Profile id → its user.
    pub users: BTreeMap<String, Record>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl UsersFile {
    /// Read it (missing: empty). A newer file is an error, and `save` leaves it alone: the
    /// passwords in it are not to be had again.
    pub fn load(path: &Path) -> io::Result<Self> {
        match std::fs::read(path) {
            Ok(b) => {
                check_version(path, &b, USERS_VERSION)?;
                serde_json::from_slice(&b)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self {
                version: USERS_VERSION,
                ..Default::default()
            }),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        check_file_version(path, USERS_VERSION)?;
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        fsutil::write_atomic(path, &bytes, 0o600)
    }
}

/// A profile and its display name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub id: String,
    pub name: String,
}

/// A Jellyfin user name from a profile name: Jellyfin allows letters, digits, `_`, space and
/// `-'.@+` (`^(?!\s)[\w\ \-'._@+]+(?<!\s)$`), so anything else is dropped; runs of spaces become
/// one; at most 48 characters (room for a " 99" suffix). Empty (or `.`/`..`): `Profile <n>`.
pub fn user_name_for(display: &str, profile_id: &str) -> String {
    let mut out = String::new();
    for c in display.chars() {
        let keep = c.is_ascii_alphanumeric()
            || matches!(c, '_' | '-' | '\'' | '.' | '@' | '+')
            || (!c.is_ascii() && c.is_alphabetic() && !c.is_numeric());
        if keep {
            out.push(c);
        } else if (c == ' ' || c.is_whitespace()) && !out.ends_with(' ') && !out.is_empty() {
            out.push(' ');
        }
    }
    let out: String = out.trim().chars().take(48).collect();
    let out = out.trim_end().to_owned();
    if out.is_empty() || out == "." || out == ".." {
        let n = crate::state::profile_number(profile_id).unwrap_or(0);
        format!("Profile {n}")
    } else {
        out
    }
}

/// The names a profile's user may have, in order of preference.
fn candidates(base: &str) -> impl Iterator<Item = String> + '_ {
    std::iter::once(base.to_owned()).chain((2..100).map(move |n| format!("{base} {n}")))
}

fn key(name: &str) -> String {
    name.to_uppercase()
}

/// A step of the sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// A profile that is gone: delete its user (if Jellyfin still has it) and its record.
    Delete {
        profile: String,
        user_id: Option<String>,
    },
    Rename {
        profile: String,
        user_id: String,
        name: String,
    },
    /// The user exists under this name already (renamed in the dashboard): record it.
    Adopt {
        profile: String,
        name: String,
    },
    Create {
        profile: String,
        name: String,
    },
}

/// What makes Jellyfin match `profiles` (pure; [`sync`] applies it). Deletes come first (their
/// names become free), then each profile in id order; names are unique case-insensitively among
/// every Jellyfin user, the admin and dashboard-made users included.
pub fn plan(profiles: &[Profile], stored: &UsersFile, existing: &[User]) -> Vec<Op> {
    let mut ops = Vec::new();
    let by_id: BTreeMap<&str, &User> = existing.iter().map(|u| (u.id.as_str(), u)).collect();
    let wanted: BTreeSet<&str> = profiles.iter().map(|p| p.id.as_str()).collect();
    // Current names, by owner (None: not ours).
    let mut taken: BTreeMap<String, Option<String>> = existing
        .iter()
        .map(|u| {
            let owner = stored
                .users
                .iter()
                .find(|(_, r)| r.jellyfin_user_id == u.id)
                .map(|(p, _)| p.clone());
            (key(&u.name), owner)
        })
        .collect();
    taken.entry(key(ADMIN_USER)).or_insert(None);
    for (profile, r) in &stored.users {
        if !wanted.contains(profile.as_str()) {
            let user_id = by_id.get(r.jellyfin_user_id.as_str()).map(|u| u.id.clone());
            if let Some(u) = by_id.get(r.jellyfin_user_id.as_str()) {
                taken.remove(&key(&u.name));
            }
            ops.push(Op::Delete {
                profile: profile.clone(),
                user_id,
            });
        }
    }
    let mut sorted: Vec<&Profile> = profiles.iter().collect();
    sorted.sort_by_key(|p| {
        (
            crate::state::profile_number(&p.id).unwrap_or(u32::MAX),
            p.id.clone(),
        )
    });
    for p in sorted {
        let base = user_name_for(&p.name, &p.id);
        let free_for = |taken: &BTreeMap<String, Option<String>>, n: &str| match taken.get(&key(n))
        {
            None => true,
            Some(owner) => owner.as_deref() == Some(p.id.as_str()),
        };
        let current = stored
            .users
            .get(&p.id)
            .and_then(|r| by_id.get(r.jellyfin_user_id.as_str()).map(|u| (r, *u)));
        match current {
            Some((r, u)) => {
                let keep = candidates(&base).any(|c| key(&c) == key(&u.name));
                if keep {
                    if r.name != u.name {
                        ops.push(Op::Adopt {
                            profile: p.id.clone(),
                            name: u.name.clone(),
                        });
                    }
                } else if let Some(name) = candidates(&base).find(|c| free_for(&taken, c)) {
                    taken.remove(&key(&u.name));
                    taken.insert(key(&name), Some(p.id.clone()));
                    ops.push(Op::Rename {
                        profile: p.id.clone(),
                        user_id: u.id.clone(),
                        name,
                    });
                }
            }
            None => {
                if let Some(name) = candidates(&base).find(|c| !taken.contains_key(&key(c))) {
                    taken.insert(key(&name), Some(p.id.clone()));
                    ops.push(Op::Create {
                        profile: p.id.clone(),
                        name,
                    });
                }
            }
        }
    }
    ops
}

/// A random password: 16 characters without look-alikes (about 79 bits), to type on a TV.
pub fn new_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut out = String::with_capacity(16);
    while out.len() < 16 {
        let mut b = [0u8; 32];
        rustix::rand::getrandom(&mut b, rustix::rand::GetRandomFlags::empty()).expect("getrandom");
        for x in b {
            // 248 = 8 * 31: no modulo bias.
            if x < 248 && out.len() < 16 {
                out.push(ALPHABET[(x % 31) as usize] as char);
            }
        }
    }
    out
}

/// What a sync did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Synced {
    pub created: usize,
    pub renamed: usize,
    pub deleted: usize,
}

/// Make Jellyfin's users match `profiles` and record them (under `users.lock`). Each step is
/// recorded as soon as it is done; a failure stops the sync (the next one carries on).
pub fn sync(api: &dyn Api, files: &Files, profiles: &[Profile]) -> Result<Synced, Error> {
    let lock = crate::state::lock(&files.lock_path())
        .map_err(|e| Error::Refused(format!("locking users.json: {e}")))?;
    let path = files.users_json();
    let mut stored =
        UsersFile::load(&path).map_err(|e| Error::Refused(format!("users.json: {e}")))?;
    stored.version = 1;
    // Every box has an admin profile: none at all means the profiles were not read, and the
    // plan would delete every user (their watch history goes with them).
    if profiles.is_empty() && !stored.users.is_empty() {
        return Err(Error::Profiles("no profiles".into()));
    }
    let existing = api.users()?;
    let ops = plan(profiles, &stored, &existing);
    let mut done = Synced::default();
    let save = |s: &UsersFile| {
        s.save(&path)
            .map_err(|e| Error::Refused(format!("writing users.json: {e}")))
    };
    for op in ops {
        match op {
            Op::Delete { profile, user_id } => {
                if let Some(id) = user_id {
                    api.delete_user(&id)?;
                    done.deleted += 1;
                }
                stored.users.remove(&profile);
                info!(profile = %profile, "Jellyfin user deleted");
            }
            Op::Rename {
                profile,
                user_id,
                name,
            } => {
                api.rename_user(&user_id, &name)?;
                if let Some(r) = stored.users.get_mut(&profile) {
                    r.name = name.clone();
                }
                done.renamed += 1;
                info!(profile = %profile, name = %name, "Jellyfin user renamed");
            }
            Op::Adopt { profile, name } => {
                if let Some(r) = stored.users.get_mut(&profile) {
                    r.name = name;
                }
            }
            Op::Create { profile, name } => {
                let password = new_password();
                let id = api.create_user(&name, &password)?;
                stored.users.insert(
                    profile.clone(),
                    Record {
                        jellyfin_user_id: id,
                        name: name.clone(),
                        password,
                        access_token: None,
                        extra: Default::default(),
                    },
                );
                done.created += 1;
                info!(profile = %profile, name = %name, "Jellyfin user made");
            }
        }
        save(&stored)?;
    }
    if done == Synced::default() && !path.exists() {
        save(&stored)?;
    }
    drop(lock);
    Ok(done)
}

/// The device a profile's token belongs to.
pub fn device_id(profile_id: &str) -> String {
    format!("hinteros-{profile_id}")
}

/// A token Jellyfin hands out (it goes into a header): 1 to 128 letters and digits.
fn is_token(s: &str) -> bool {
    (1..=128).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

impl Client {
    /// Jellyfin's server id (`GET /System/Info/Public`).
    pub fn server_id(&self) -> Result<String, Error> {
        let r = http::request(
            Target::TcpOwnedBy(self.addr, self.owner),
            "GET",
            "/System/Info/Public",
            &[],
            None,
            self.timeout,
        )
        .map_err(|e| Error::Unreachable(e.to_string()))?;
        let v: Value =
            serde_json::from_slice(&r.body).map_err(|e| Error::Refused(e.to_string()))?;
        v["Id"]
            .as_str()
            .filter(|i| is_id(i))
            .map(str::to_owned)
            .ok_or_else(|| Error::Refused("no server id".into()))
    }

    /// Whether Jellyfin still takes `token` (`GET /System/Info`: 401 when it does not).
    pub fn token_valid(&self, token: &str) -> Result<bool, Error> {
        let auth = format!("MediaBrowser Token=\"{token}\"");
        let r = http::request(
            Target::TcpOwnedBy(self.addr, self.owner),
            "GET",
            "/System/Info",
            &[("Authorization", &auth)],
            None,
            self.timeout,
        )
        .map_err(|e| Error::Unreachable(e.to_string()))?;
        match r.status {
            200..=299 => Ok(true),
            401 | 403 => Ok(false),
            s => Err(Error::Refused(format!("GET /System/Info HTTP {s}"))),
        }
    }

    /// Sign a user in as device `device`: (access token, user id). The server ends that device's
    /// older token, so each profile has one live token.
    pub fn authenticate(
        &self,
        user: &str,
        password: &str,
        device: &str,
    ) -> Result<(String, String), Error> {
        let auth = format!(
            "MediaBrowser Client=\"Jellyfin Desktop\", Device=\"HinterOS\", DeviceId=\"{device}\", Version=\"{}\"",
            env!("CARGO_PKG_VERSION")
        );
        let body = json!({ "Username": user, "Pw": password }).to_string();
        let r = http::request(
            Target::TcpOwnedBy(self.addr, self.owner),
            "POST",
            "/Users/AuthenticateByName",
            &[("Authorization", &auth), ("Accept", "application/json")],
            Some(Body {
                content_type: "application/json",
                bytes: body.as_bytes(),
            }),
            self.timeout,
        )
        .map_err(|e| Error::Unreachable(e.to_string()))?;
        if !r.ok() {
            return Err(Error::Refused(format!(
                "POST /Users/AuthenticateByName HTTP {}",
                r.status
            )));
        }
        let v: Value =
            serde_json::from_slice(&r.body).map_err(|e| Error::Refused(e.to_string()))?;
        let token = v["AccessToken"].as_str().filter(|t| is_token(t));
        let id = v["User"]["Id"].as_str().filter(|i| is_id(i));
        match (token, id) {
            (Some(t), Some(i)) => Ok((t.to_owned(), i.to_owned())),
            _ => Err(Error::Refused("no token in the answer".into())),
        }
    }
}

/// `jellyfin.client_token` for profile `id`: its user made first if needed, the kept token if
/// Jellyfin still takes it, else a new one (kept in `users.json`).
pub fn client_token(
    c: &Client,
    files: &Files,
    profiles: &[Profile],
    id: &str,
) -> Result<hinteros_api::system::JellyfinClientTokenResult, Error> {
    let has_user = UsersFile::load(&files.users_json()).is_ok_and(|f| f.users.contains_key(id));
    if !has_user {
        sync(c, files, profiles)?;
    }
    let server_id = c.server_id()?;
    let lock = crate::state::lock(&files.lock_path())
        .map_err(|e| Error::Refused(format!("locking users.json: {e}")))?;
    let path = files.users_json();
    let mut stored =
        UsersFile::load(&path).map_err(|e| Error::Refused(format!("users.json: {e}")))?;
    let Some(r) = stored.users.get_mut(id) else {
        return Err(Error::Refused("the profile has no Jellyfin user".into()));
    };
    let device = device_id(id);
    let kept = match &r.access_token {
        Some(t) if c.token_valid(t)? => Some(t.clone()),
        _ => None,
    };
    let token = match kept {
        Some(t) => t,
        None => {
            let (t, user_id) = c.authenticate(&r.name, &r.password, &device)?;
            r.jellyfin_user_id = user_id;
            r.access_token = Some(t.clone());
            stored
                .save(&path)
                .map_err(|e| Error::Refused(format!("writing users.json: {e}")))?;
            info!(profile = %id, "Jellyfin token made");
            t
        }
    };
    let user_id = stored.users[id].jellyfin_user_id.clone();
    drop(lock);
    Ok(hinteros_api::system::JellyfinClientTokenResult {
        server_id,
        user_id,
        device_id: device,
        access_token: token,
    })
}

/// The profiles that get a Jellyfin user: every profile with metadata (not the guest, not a
/// spare account), with its name now. A `state.json` or `profiles.json` that cannot be read
/// (damaged, missing, newer than this binary) is an error, never "no profiles".
pub fn profiles(paths: &crate::config::Paths) -> Result<Vec<Profile>, Error> {
    let (state, _) = crate::state::State::load(&paths.state_json())
        .map_err(|e| Error::Profiles(e.to_string()))?;
    let path = paths.profiles_json();
    if !crate::profiles::ProfilesFile::exists(&path) {
        return Err(Error::Profiles(format!("{}: missing", path.display())));
    }
    let file =
        crate::profiles::ProfilesFile::load(&path).map_err(|e| Error::Profiles(e.to_string()))?;
    Ok(file
        .visible(&state)
        .into_iter()
        .map(|(a, m)| Profile {
            id: a.id,
            name: m.name.clone(),
        })
        .collect())
}

/// Jellyfin is on in `state.json`.
pub fn enabled(paths: &crate::config::Paths) -> bool {
    crate::state::State::load(&paths.state_json())
        .is_ok_and(|(s, _)| s.services.iter().any(|i| i == ID))
}

/// A sync when Jellyfin is on and set up; failures are logged only (best effort: after profile
/// jobs, renames and service jobs; `jellyfin.users.reveal` and every Jellyfin job sync again).
pub fn sync_if_ready(paths: &crate::config::Paths, server: &crate::config::ServerPaths) {
    if !enabled(paths) {
        return;
    }
    let files = Files::new(&paths.services_dir());
    match Client::new(
        &files,
        server.jellyfin,
        &paths.users,
        Duration::from_secs(5),
    ) {
        Ok(c) => match profiles(paths).and_then(|p| sync(&c, &files, &p)) {
            Ok(s) if s != Synced::default() => info!(?s, "Jellyfin users synced"),
            Ok(_) => {}
            Err(e) => warn!("Jellyfin users not synced: {e}"),
        },
        Err(e) => info!("Jellyfin users not synced: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn p(id: &str, name: &str) -> Profile {
        Profile {
            id: id.into(),
            name: name.into(),
        }
    }

    fn u(id: &str, name: &str) -> User {
        User {
            id: id.into(),
            name: name.into(),
        }
    }

    fn rec(id: &str, name: &str) -> Record {
        Record {
            jellyfin_user_id: id.into(),
            name: name.into(),
            password: "pw".into(),
            access_token: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn users_file_keeps_unknown_fields_and_refuses_a_newer_one() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("users.json");
        std::fs::write(
            &path,
            r#"{"version":1,"later":1,"users":{"p1":{"jellyfin_user_id":"a","name":"A","password":"pw","mood":"ok"}}}"#,
        )
        .unwrap();
        UsersFile::load(&path).unwrap().save(&path).unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["later"], 1);
        assert_eq!(v["users"]["p1"]["mood"], "ok");
        let newer = r#"{"version":2,"users":{}}"#;
        std::fs::write(&path, newer).unwrap();
        assert!(UsersFile::load(&path).is_err());
        assert!(UsersFile::default().save(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
    }

    fn hex(n: u8) -> String {
        format!("{n:032x}")
    }

    /// One connection per body, like Jellyfin's stand-in server during startup, then the real one.
    fn answers(bodies: &'static [&'static str]) -> SocketAddr {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for body in bodies {
                let (mut s, _) = l.accept().unwrap();
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.ends_with(b"\r\n\r\n") {
                    let n = s.read(&mut buf).unwrap();
                    assert!(n > 0);
                    req.extend_from_slice(&buf[..n]);
                }
                assert!(req.starts_with(b"GET /health "));
                let r = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                s.write_all(r.as_bytes()).unwrap();
            }
        });
        addr
    }

    #[test]
    fn ping_waits_for_the_real_server() {
        let addr = answers(&["Degraded", "Healthy"]);
        let t = Duration::from_secs(2);
        assert!(!Client::ping(addr, t));
        assert!(Client::ping(addr, t));
    }

    #[test]
    fn user_names() {
        assert_eq!(user_name_for("Alice", "p1"), "Alice");
        assert_eq!(user_name_for("  Jo   Ann  ", "p2"), "Jo Ann");
        assert_eq!(user_name_for("Zoë 🎮", "p2"), "Zoë");
        assert_eq!(
            user_name_for("O'Neil-Smith.jr@home+1", "p2"),
            "O'Neil-Smith.jr@home+1"
        );
        assert_eq!(user_name_for("🎮🎮", "p3"), "Profile 3");
        assert_eq!(user_name_for("..", "p4"), "Profile 4");
        assert_eq!(user_name_for("a/b\\c:d", "p2"), "abcd");
        assert_eq!(user_name_for("Ⅻ²", "p5"), "Profile 5");
        assert_eq!(user_name_for(&"x".repeat(80), "p2").len(), 48);
    }

    #[test]
    fn library_names_and_ids() {
        assert_eq!(
            check_library_name("  Kids' films "),
            Ok("Kids' films".into())
        );
        assert_eq!(
            check_library_name("Films, 4K & HDR"),
            Ok("Films, 4K & HDR".into())
        );
        for bad in [
            "",
            "   ",
            "a/b",
            "a\\b",
            "x:y",
            "a\nb",
            "..",
            "q?",
            "\u{202E}x",
        ] {
            assert!(check_library_name(bad).is_err(), "{bad:?}");
        }
        assert!(check_library_name(&"é".repeat(64)).is_ok());
        assert!(check_library_name(&"é".repeat(65)).is_err());
        assert!(is_id("f137a2dd21bbc1b99aa5c0f6bf02a805"));
        for bad in [
            "F137A2DD21BBC1B99AA5C0F6BF02A805",
            "f137a2dd-21bb-c1b9-9aa5-c0f6bf02a805",
            "",
            "../x",
        ] {
            assert!(!is_id(bad), "{bad}");
        }
        assert_eq!(collection_type(LibraryKind::Shows), Some("tvshows"));
        assert_eq!(collection_type(LibraryKind::Mixed), None);
    }

    #[test]
    fn virtual_folders_to_libraries() {
        let json = r#"[{"Name":"Movies","Locations":["/srv/media/Movies"],"CollectionType":"movies","LibraryOptions":{},"ItemId":"f137a2dd21bbc1b99aa5c0f6bf02a805","RefreshStatus":"Idle"},
          {"Name":"Shows","Locations":["/srv/media/TV/All"],"CollectionType":"tvshows","ItemId":"a137a2dd21bbc1b99aa5c0f6bf02a805"},
          {"Name":"Mixed","Locations":["/mnt/usb"],"ItemId":"b137a2dd21bbc1b99aa5c0f6bf02a805"},
          {"Name":"Books","Locations":[],"CollectionType":"books","ItemId":"c137a2dd21bbc1b99aa5c0f6bf02a805"},
          {"Name":"Pending","Locations":["/srv/media/x"]}]"#;
        let folders: Vec<VirtualFolder> = serde_json::from_str(json).unwrap();
        let libs = libraries(&folders, Path::new("/srv/media"));
        let got: Vec<_> = libs
            .iter()
            .map(|l| (l.name.as_str(), l.kind, l.folder.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                ("Movies", LibraryKindShown::Movies, Some("Movies")),
                ("Shows", LibraryKindShown::Shows, Some("TV/All")),
                ("Mixed", LibraryKindShown::Mixed, None),
                ("Books", LibraryKindShown::Other, None),
            ]
        );
    }

    #[test]
    fn plan_creates_renames_and_deletes() {
        let existing = vec![
            u(&hex(1), "admin"),
            u(&hex(2), "Alice"),
            u(&hex(3), "Bob"),
            u(&hex(9), "Carol"),
        ];
        let mut stored = UsersFile::default();
        stored.users.insert("p1".into(), rec(&hex(2), "Alice"));
        stored.users.insert("p2".into(), rec(&hex(3), "Bob"));
        stored.users.insert("p4".into(), rec(&hex(4), "Gone"));
        // p2 renamed to Robert; p3 new, named like the dashboard's Carol; p4 removed (its
        // Jellyfin user already deleted by hand); p5 named "Admin".
        let profiles = [
            p("p1", "Alice"),
            p("p2", "Robert"),
            p("p3", "carol"),
            p("p5", "Admin"),
        ];
        let ops = plan(&profiles, &stored, &existing);
        assert_eq!(
            ops,
            [
                Op::Delete {
                    profile: "p4".into(),
                    user_id: None
                },
                Op::Rename {
                    profile: "p2".into(),
                    user_id: hex(3),
                    name: "Robert".into()
                },
                Op::Create {
                    profile: "p3".into(),
                    name: "carol 2".into()
                },
                Op::Create {
                    profile: "p5".into(),
                    name: "Admin 2".into()
                },
            ]
        );
        // A user renamed in the dashboard to a variant of the name is kept and recorded.
        let mut s2 = UsersFile::default();
        s2.users.insert("p1".into(), rec(&hex(2), "Alice"));
        let ops = plan(&[p("p1", "Alice")], &s2, &[u(&hex(2), "ALICE 2")]);
        assert_eq!(
            ops,
            [Op::Adopt {
                profile: "p1".into(),
                name: "ALICE 2".into()
            }]
        );
        // Our user deleted in the dashboard: made again.
        let ops = plan(&[p("p1", "Alice")], &s2, &[]);
        assert_eq!(
            ops,
            [Op::Create {
                profile: "p1".into(),
                name: "Alice".into()
            }]
        );
        // Nothing to do.
        assert!(plan(&[p("p1", "Alice")], &s2, &[u(&hex(2), "Alice")]).is_empty());
    }

    #[test]
    fn plan_swaps_without_clashes() {
        let existing = vec![u(&hex(2), "Ann"), u(&hex(3), "Bob")];
        let mut stored = UsersFile::default();
        stored.users.insert("p2".into(), rec(&hex(2), "Ann"));
        stored.users.insert("p3".into(), rec(&hex(3), "Bob"));
        let ops = plan(&[p("p2", "Bob"), p("p3", "Ann")], &stored, &existing);
        // p2 cannot take Bob while p3 has it; p3 then takes Ann, which p2 gave up.
        assert_eq!(
            ops,
            [
                Op::Rename {
                    profile: "p2".into(),
                    user_id: hex(2),
                    name: "Bob 2".into()
                },
                Op::Rename {
                    profile: "p3".into(),
                    user_id: hex(3),
                    name: "Ann".into()
                },
            ]
        );
        // The names stay as they are on the next run.
        let mut after = stored.clone();
        after.users.get_mut("p2").unwrap().name = "Bob 2".into();
        after.users.get_mut("p3").unwrap().name = "Ann".into();
        let ops = plan(
            &[p("p2", "Bob"), p("p3", "Ann")],
            &after,
            &[u(&hex(2), "Bob 2"), u(&hex(3), "Ann")],
        );
        assert!(ops.is_empty(), "{ops:?}");
    }

    struct Fake {
        users: RefCell<Vec<User>>,
        next: RefCell<u8>,
        fail_create: bool,
    }

    impl Api for Fake {
        fn users(&self) -> Result<Vec<User>, Error> {
            Ok(self.users.borrow().clone())
        }
        fn create_user(&self, name: &str, _password: &str) -> Result<String, Error> {
            if self.fail_create {
                return Err(Error::Refused("no".into()));
            }
            let mut n = self.next.borrow_mut();
            *n += 1;
            let id = hex(100 + *n);
            self.users.borrow_mut().push(u(&id, name));
            Ok(id)
        }
        fn rename_user(&self, id: &str, name: &str) -> Result<(), Error> {
            for x in self.users.borrow_mut().iter_mut() {
                if x.id == id {
                    x.name = name.into();
                }
            }
            Ok(())
        }
        fn delete_user(&self, id: &str) -> Result<(), Error> {
            self.users.borrow_mut().retain(|x| x.id != id);
            Ok(())
        }
    }

    #[test]
    fn sync_records_every_step() {
        let d = tempfile::tempdir().unwrap();
        let files = Files {
            dir: d.path().to_owned(),
        };
        let fake = Fake {
            users: RefCell::new(vec![u(&hex(1), "admin")]),
            next: RefCell::new(0),
            fail_create: false,
        };
        let s = sync(&fake, &files, &[p("p1", "Alice"), p("p2", "Bob")]).unwrap();
        assert_eq!(s.created, 2);
        let f = UsersFile::load(&files.users_json()).unwrap();
        assert_eq!(f.users["p1"].name, "Alice");
        assert_eq!(f.users["p2"].password.len(), 16);
        assert_ne!(f.users["p1"].password, f.users["p2"].password);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(files.users_json())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // Rename and remove.
        let s = sync(&fake, &files, &[p("p1", "Alicia")]).unwrap();
        assert_eq!((s.renamed, s.deleted), (1, 1));
        let f = UsersFile::load(&files.users_json()).unwrap();
        assert_eq!(f.users.keys().collect::<Vec<_>>(), ["p1"]);
        assert_eq!(f.users["p1"].name, "Alicia");
        let names: Vec<_> = fake.users.borrow().iter().map(|x| x.name.clone()).collect();
        assert_eq!(names, ["admin", "Alicia"]);
        // Idempotent.
        assert_eq!(
            sync(&fake, &files, &[p("p1", "Alicia")]).unwrap(),
            Synced::default()
        );
        // A failing Jellyfin keeps what was recorded.
        let failing = Fake {
            users: RefCell::new(fake.users.borrow().clone()),
            next: RefCell::new(0),
            fail_create: true,
        };
        assert!(sync(&failing, &files, &[p("p1", "Alicia"), p("p3", "C")]).is_err());
        assert!(
            UsersFile::load(&files.users_json())
                .unwrap()
                .users
                .contains_key("p1")
        );
        // Secrets stay out of debug output.
        assert!(!format!("{:?}", f.users["p1"]).contains(&f.users["p1"].password));
    }

    #[test]
    fn unreadable_profiles_delete_nobody() {
        let d = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths {
            config_dir: d.path().join("etc"),
            state_dir: d.path().join("var"),
            run_dir: d.path().join("run"),
            profile: d.path().join("profiles/system"),
            current_system: d.path().join("current"),
            booted_system: d.path().join("booted"),
            apps_profiles: d.path().join("nixprofiles"),
            catalog_index: None,
            users: Default::default(),
            proc_dir: "/proc".into(),
            zoneinfo_dir: d.path().join("zoneinfo"),
            localtime: d.path().join("localtime"),
            server: Default::default(),
        };
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        // No state.json, then no profiles.json, then a newer one (a rollback): an error each.
        assert!(matches!(profiles(&paths), Err(Error::Profiles(_))));
        let state = crate::state::State::parse(
            br#"{"schemaVersion":2,"base":"unstable","stateVersion":"26.11","requirements":[],"services":[],
               "profiles":[{"id":"p1"},{"id":"p2"}]}"#,
        )
        .unwrap();
        std::fs::write(paths.state_json(), state.to_canonical_json()).unwrap();
        assert!(matches!(profiles(&paths), Err(Error::Profiles(_))));
        std::fs::write(paths.profiles_json(), r#"{"version":99,"profiles":{}}"#).unwrap();
        assert!(matches!(profiles(&paths), Err(Error::Profiles(_))));
        crate::profiles::test_file(&state)
            .save(&paths.profiles_json())
            .unwrap_err();
        std::fs::remove_file(paths.profiles_json()).unwrap();
        crate::profiles::test_file(&state)
            .save(&paths.profiles_json())
            .unwrap();
        let got: Vec<String> = profiles(&paths)
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(got, ["p1", "p2"]);

        // An empty list with users recorded: refused, nothing deleted.
        let files = Files {
            dir: d.path().join("jf"),
        };
        std::fs::create_dir_all(&files.dir).unwrap();
        let fake = Fake {
            users: RefCell::new(vec![u(&hex(1), "admin")]),
            next: RefCell::new(0),
            fail_create: false,
        };
        sync(&fake, &files, &[p("p1", "Alice")]).unwrap();
        assert!(matches!(sync(&fake, &files, &[]), Err(Error::Profiles(_))));
        assert_eq!(fake.users.borrow().len(), 2);
        assert!(
            UsersFile::load(&files.users_json())
                .unwrap()
                .users
                .contains_key("p1")
        );
    }

    #[test]
    fn passwords() {
        let a = new_password();
        assert_eq!(a.len(), 16);
        assert!(
            a.bytes()
                .all(|b| b"abcdefghjkmnpqrstuvwxyz23456789".contains(&b))
        );
        assert_ne!(a, new_password());
    }
}
