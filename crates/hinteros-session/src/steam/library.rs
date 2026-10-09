//! The session user's installed Steam games, read from Steam's files (no Steam API, no network).
//!
//! - **Steam directory:** `steam.dir` if configured, else the first of `~/.local/share/Steam`,
//!   `~/.steam/steam`, `~/.steam/root` and the Flatpak location that has a `steamapps`
//!   directory.
//! - **Libraries:** the Steam directory itself plus every `path` in
//!   `steamapps/libraryfolders.vdf` (`config/libraryfolders.vdf` in old installs; both the
//!   current block format and the old `"1" "<path>"` format). A relative path is taken relative
//!   to the Steam directory (Steam never writes one; it makes in-tree test fixtures possible).
//! - **Games:** `<library>/steamapps/appmanifest_<appid>.acf` (exact name; Steam's `.tmp` files
//!   are ignored, as are manifests that don't parse, e.g. half-written ones).
//! - **Install state** from the manifest's `StateFlags` (Steam's `EAppState` bits), see
//!   [`install_state`].
//! - **Tools are left out** ([`is_tool`]): known app ids of Proton, Steam Linux Runtime and
//!   Steamworks redistributables, plus name prefixes for versions we don't know yet. (Steam's
//!   authoritative "type: Tool" lives in the binary `appinfo.vdf`, which we don't parse.)
//! - **Artwork** from `<steam>/appcache/librarycache`, see [`artwork`].

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use hinteros_api::session::{Game, GameArtwork, InstallState};
use tracing::debug;

use super::vdf;

/// Where to look for the Steam directory, in order, relative to `$HOME`.
pub const STEAM_DIR_CANDIDATES: &[&str] = &[
    ".local/share/Steam",
    ".steam/steam",
    ".steam/root",
    ".var/app/com.valvesoftware.Steam/.local/share/Steam",
];

/// Steam's directory: `configured` if set (whether or not it exists yet), else the first
/// candidate under `home` that has a `steamapps` directory.
pub fn steam_dir(configured: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = configured {
        return Some(dir.to_path_buf());
    }
    let home = home?;
    STEAM_DIR_CANDIDATES
        .iter()
        .map(|c| home.join(c))
        .find(|dir| dir.join("steamapps").is_dir())
}

/// The Steam directory for this process: `configured`, else found under `$HOME`.
pub fn default_steam_dir(configured: Option<&Path>) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    steam_dir(configured, home.as_deref())
}

/// Every existing library directory (canonical, deduplicated), the Steam directory first.
pub fn libraries(steam: &Path) -> Vec<PathBuf> {
    let mut paths = vec![steam.to_path_buf()];
    for file in ["steamapps/libraryfolders.vdf", "config/libraryfolders.vdf"] {
        let Ok(text) = fs::read_to_string(steam.join(file)) else {
            continue;
        };
        match vdf::parse(&text) {
            Ok(doc) => {
                paths.extend(library_paths(&doc).into_iter().map(|p| steam.join(p)));
                break;
            }
            Err(e) => debug!(file, "unreadable library list: {e}"),
        }
    }
    let mut seen = BTreeSet::new();
    paths
        .into_iter()
        .filter_map(|p| fs::canonicalize(&p).ok())
        .filter(|p| p.join("steamapps").is_dir() && seen.insert(p.clone()))
        .collect()
}

/// The `path`s listed in a `libraryfolders.vdf` document.
fn library_paths(doc: &vdf::Obj) -> Vec<String> {
    let Some(root) = doc.obj("libraryfolders") else {
        return Vec::new();
    };
    root.iter()
        .filter(|(k, _)| !k.is_empty() && k.bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|(_, v)| match v {
            vdf::Value::Obj(o) => o.str("path").map(str::to_owned),
            vdf::Value::Str(s) => Some(s.clone()),
        })
        .filter(|p| !p.is_empty())
        .collect()
}

/// What we use from an `appmanifest_<id>.acf`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub appid: u32,
    pub name: String,
    pub state_flags: u32,
    pub size_bytes: u64,
    pub last_played: Option<u64>,
}

pub fn parse_manifest(text: &str) -> Option<Manifest> {
    let doc = vdf::parse(text).ok()?;
    let app = doc.obj("AppState")?;
    let appid: u32 = app.num("appid").filter(|&id| id != 0)?;
    let name = app
        .str("name")
        .or_else(|| app.str("installdir"))
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("App {appid}"));
    Some(Manifest {
        appid,
        name,
        state_flags: app.num("StateFlags").unwrap_or(0),
        size_bytes: app.num("SizeOnDisk").unwrap_or(0),
        last_played: app.num("LastPlayed").filter(|&t| t != 0),
    })
}

/// Steam's `EAppState` bits.
pub mod flags {
    pub const UNINSTALLED: u32 = 1;
    pub const UPDATE_REQUIRED: u32 = 2;
    pub const FULLY_INSTALLED: u32 = 4;
    pub const FILES_MISSING: u32 = 32;
    pub const FILES_CORRUPT: u32 = 128;
    pub const UPDATE_RUNNING: u32 = 256;
    pub const UPDATE_PAUSED: u32 = 512;
    pub const UPDATE_STARTED: u32 = 1024;
    pub const UNINSTALLING: u32 = 2048;
    pub const RECONFIGURING: u32 = 65536;
    pub const VALIDATING: u32 = 131_072;
    pub const ADDING_FILES: u32 = 262_144;
    pub const PREALLOCATING: u32 = 524_288;
    pub const DOWNLOADING: u32 = 1_048_576;
    pub const STAGING: u32 = 2_097_152;
    pub const COMMITTING: u32 = 4_194_304;
    pub const UPDATE_STOPPING: u32 = 8_388_608;

    /// Something is being downloaded, written or checked right now.
    pub const ACTIVITY: u32 = UPDATE_RUNNING
        | UPDATE_STARTED
        | RECONFIGURING
        | VALIDATING
        | ADDING_FILES
        | PREALLOCATING
        | DOWNLOADING
        | STAGING
        | COMMITTING
        | UPDATE_STOPPING;
}

/// The install state for `StateFlags`; `None` for a manifest of something not installed (0,
/// or "uninstalled" with nothing going on).
///
/// - uninstalling → `Uninstalling`
/// - fully installed + activity → `Updating`; + update required/paused, files missing or
///   corrupt → `UpdateRequired` (Steam updates or repairs it on launch); else `Installed`
/// - not fully installed, with an update required, paused or running → `Installing`
pub fn install_state(state_flags: u32) -> Option<InstallState> {
    use flags::*;
    let has = |bits: u32| state_flags & bits != 0;
    if has(UNINSTALLING) {
        Some(InstallState::Uninstalling)
    } else if has(FULLY_INSTALLED) {
        Some(if has(ACTIVITY) {
            InstallState::Updating
        } else if has(UPDATE_REQUIRED | UPDATE_PAUSED | FILES_MISSING | FILES_CORRUPT) {
            InstallState::UpdateRequired
        } else {
            InstallState::Installed
        })
    } else if has(UPDATE_REQUIRED | UPDATE_PAUSED | ACTIVITY) {
        Some(InstallState::Installing)
    } else {
        None
    }
}

/// App ids of Steam tools that are not games.
pub const TOOL_APP_IDS: &[u32] = &[
    228_980,   // Steamworks Common Redistributables
    1007,      // Steamworks SDK Redist
    1_070_560, // Steam Linux Runtime 1.0 (scout)
    1_391_110, // Steam Linux Runtime 2.0 (soldier)
    1_628_350, // Steam Linux Runtime 3.0 (sniper)
    4_183_110, // Steam Linux Runtime 4.0
    1_493_710, // Proton Experimental
    2_180_100, // Proton Hotfix
    1_161_040, // Proton BattlEye Runtime
    1_826_330, // Proton EasyAntiCheat Runtime
    858_280,   // Proton 3.7
    930_400,   // Proton 3.16
    961_940,   // Proton 4.2
    1_054_830, // Proton 4.11
    1_113_280, // Proton 5.0
    1_245_040, // Proton 5.13
    1_420_170, // Proton 6.3
    1_580_130, // Proton 7.0
    2_348_590, // Proton 8.0
    2_805_730, // Proton 9.0
    3_658_110, // Proton 10.0
    250_820,   // SteamVR
];

/// Name prefixes (lower case) of tools whose app ids we don't know yet.
pub const TOOL_NAME_PREFIXES: &[&str] = &[
    "proton ",
    "proton-",
    "steam linux runtime",
    "steamworks common redistributables",
    "steamworks sdk redist",
    "steamvr",
];

pub fn is_tool(appid: u32, name: &str) -> bool {
    let name = name.trim().to_lowercase();
    TOOL_APP_IDS.contains(&appid)
        || name == "proton experimental"
        || TOOL_NAME_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Installed games in every library of `steam`, sorted by name (case-insensitive), then app id.
/// An app id found in more than one library is listed once (the first library wins).
pub fn scan(steam: &Path) -> Vec<Game> {
    let steam = fs::canonicalize(steam).unwrap_or_else(|_| steam.to_path_buf());
    let cache = steam.join("appcache/librarycache");
    let mut seen = BTreeSet::new();
    let mut games = Vec::new();
    for library in libraries(&steam) {
        let Ok(entries) = fs::read_dir(library.join("steamapps")) else {
            continue;
        };
        let mut manifests: Vec<(u32, PathBuf)> = entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = e.file_name();
                let id = name
                    .to_str()?
                    .strip_prefix("appmanifest_")?
                    .strip_suffix(".acf")?;
                if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                Some((id.parse().ok()?, e.path()))
            })
            .collect();
        manifests.sort();
        for (file_appid, path) in manifests {
            let Some(m) = fs::read_to_string(&path)
                .ok()
                .and_then(|t| parse_manifest(&t))
            else {
                debug!(path = %path.display(), "skipping unreadable manifest");
                continue;
            };
            if m.appid != file_appid || is_tool(m.appid, &m.name) || seen.contains(&m.appid) {
                continue;
            }
            let Some(install_state) = install_state(m.state_flags) else {
                continue;
            };
            seen.insert(m.appid);
            games.push(Game {
                appid: m.appid,
                name: m.name,
                install_state,
                size_bytes: m.size_bytes,
                last_played: m.last_played,
                artwork: artwork(&cache, m.appid),
            });
        }
    }
    games.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.appid.cmp(&b.appid))
    });
    games
}

/// The name of a non-Steam game added to Steam ("Add a Non-Steam Game"), by its shortcut app
/// id, from any user's `userdata/<user>/config/shortcuts.vdf` (binary VDF).
pub fn shortcut_name(steam: &Path, appid: u32) -> Option<String> {
    let users = fs::read_dir(steam.join("userdata")).ok()?;
    users.flatten().find_map(|user| {
        let data = fs::read(user.path().join("config/shortcuts.vdf")).ok()?;
        shortcuts(&data)
            .into_iter()
            .find_map(|(id, name)| (id == appid).then_some(name))
    })
}

/// `(appid, AppName)` of every shortcut in a binary `shortcuts.vdf`: a `shortcuts` map of
/// numbered entries; a map is `00 key\0 ... 08`, a string `01 key\0 value\0`, an int32
/// `02 key\0 <4 bytes LE>`. Stops at anything else (a damaged file gives what came before).
fn shortcuts(data: &[u8]) -> Vec<(u32, String)> {
    fn cstr(data: &[u8], i: &mut usize) -> Option<String> {
        let len = data.get(*i..)?.iter().position(|&b| b == 0)?;
        let s = String::from_utf8_lossy(&data[*i..*i + len]).into_owned();
        *i += len + 1;
        Some(s)
    }
    let mut out = Vec::new();
    let (mut appid, mut name) = (None, None);
    let (mut i, mut depth) = (0, 0u32);
    while let Some(&tag) = data.get(i) {
        i += 1;
        match tag {
            0x00 if cstr(data, &mut i).is_some() => depth += 1,
            0x01 => {
                let (Some(key), Some(value)) = (cstr(data, &mut i), cstr(data, &mut i)) else {
                    break;
                };
                if depth == 2 && key.eq_ignore_ascii_case("appname") {
                    name = Some(value);
                }
            }
            0x02 => {
                let Some(key) = cstr(data, &mut i) else { break };
                let Some(bytes) = data.get(i..i + 4) else {
                    break;
                };
                i += 4;
                if depth == 2 && key.eq_ignore_ascii_case("appid") {
                    appid = Some(u32::from_le_bytes(bytes.try_into().expect("4 bytes")));
                }
            }
            0x08 if depth > 0 => {
                if depth == 2
                    && let (Some(id), Some(n)) = (appid.take(), name.take())
                {
                    out.push((id, n));
                }
                depth -= 1;
            }
            _ => break,
        }
    }
    out
}

/// The manifest of `appid` in any library of `steam`, if installed there.
pub fn find_manifest(steam: &Path, appid: u32) -> Option<Manifest> {
    libraries(steam).into_iter().find_map(|lib| {
        let text =
            fs::read_to_string(lib.join(format!("steamapps/appmanifest_{appid}.acf"))).ok()?;
        parse_manifest(&text).filter(|m| m.appid == appid)
    })
}

/// File names of one kind of artwork in each of Steam's cache layouts, best first.
struct Kind {
    /// In `<appid>/` and its `<hash>/` subdirectories (current layouts).
    in_dir: &'static [&'static str],
    /// `<appid>_<suffix>` directly in the cache (layout before 2024).
    flat: &'static [&'static str],
}

const PORTRAIT: Kind = Kind {
    in_dir: &[
        "library_capsule.jpg",
        "library_600x900.jpg",
        "library_600x900_2x.jpg",
    ],
    flat: &["library_600x900.jpg", "library_600x900_2x.jpg"],
};
const HEADER: Kind = Kind {
    in_dir: &["library_header.jpg", "header.jpg"],
    flat: &["header.jpg"],
};
const HERO: Kind = Kind {
    in_dir: &["library_hero.jpg"],
    flat: &["library_hero.jpg"],
};
const LOGO: Kind = Kind {
    in_dir: &["logo.png"],
    flat: &["logo.png"],
};

/// A game's artwork in Steam's library cache (`<steam>/appcache/librarycache`). Steam has used
/// three layouts; we look in all of them, newest first:
///
/// 1. `<appid>/<sha1>/library_capsule.jpg`, `library_header.jpg`, `library_hero.jpg`,
///    `logo.png` (one hash directory per asset version; the newest file wins);
/// 2. `<appid>/library_600x900.jpg`, `header.jpg`, `library_hero.jpg`, `logo.png`, and the
///    icon as `<appid>/<sha1>.jpg`;
/// 3. `<appid>_library_600x900.jpg`, `<appid>_header.jpg`, `<appid>_library_hero.jpg`,
///    `<appid>_logo.png`, `<appid>_icon.jpg`.
pub fn artwork(cache: &Path, appid: u32) -> GameArtwork {
    let dir = cache.join(appid.to_string());
    let hashed: Vec<PathBuf> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()) && is_sha1(&e.file_name()))
        .map(|e| e.path())
        .collect();
    let find = |kind: &Kind| -> Option<String> {
        let newest_hashed = kind.in_dir.iter().find_map(|name| {
            hashed
                .iter()
                .map(|h| h.join(name))
                .filter_map(|p| Some((modified(&p)?, p)))
                .max()
                .map(|(_, p)| p)
        });
        newest_hashed
            .or_else(|| {
                kind.in_dir
                    .iter()
                    .map(|n| dir.join(n))
                    .find(|p| p.is_file())
            })
            .or_else(|| {
                kind.flat
                    .iter()
                    .map(|n| cache.join(format!("{appid}_{n}")))
                    .find(|p| p.is_file())
            })
            .map(|p| p.to_string_lossy().into_owned())
    };
    let icon = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.strip_suffix(".jpg")
                .is_some_and(|stem| is_sha1(std::ffi::OsStr::new(stem)))
        })
        .filter_map(|e| Some((modified(&e.path())?, e.path())))
        .max()
        .map(|(_, p)| p)
        .or_else(|| Some(cache.join(format!("{appid}_icon.jpg"))).filter(|p| p.is_file()))
        .map(|p| p.to_string_lossy().into_owned());
    GameArtwork {
        portrait: find(&PORTRAIT),
        header: find(&HEADER),
        hero: find(&HERO),
        logo: find(&LOGO),
        icon,
    }
}

fn is_sha1(name: &std::ffi::OsStr) -> bool {
    let name = name.as_encoded_bytes();
    name.len() == 40 && name.iter().all(|b| b.is_ascii_hexdigit())
}

fn modified(p: &Path) -> Option<SystemTime> {
    let meta = fs::metadata(p).ok()?;
    meta.is_file().then(|| meta.modified().ok()).flatten()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The in-tree fixture: a fake home with a Steam install and a second library.
    pub fn fixture_home() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/steam-home")
    }

    /// A binary `shortcuts.vdf` as Steam writes it, for `(appid, name)` entries.
    fn shortcuts_vdf(entries: &[(u32, &str)]) -> Vec<u8> {
        let mut v = b"\0shortcuts\0".to_vec();
        for (n, (id, name)) in entries.iter().enumerate() {
            v.extend(format!("\0{n}\0").bytes());
            v.extend(b"\x02appid\0");
            v.extend(id.to_le_bytes());
            v.extend(format!("\x01AppName\0{name}\0").bytes());
            v.extend(b"\x01Exe\0\"/games/x\"\0\x02IsHidden\0\0\0\0\0\0tags\0\x08\x08");
        }
        v.extend(b"\x08\x08");
        v
    }

    #[test]
    fn shortcut_names_from_binary_vdf() {
        let data = shortcuts_vdf(&[(2_396_848_268, "Zombie Force"), (2_147_483_649, "Other")]);
        assert_eq!(
            shortcuts(&data),
            [
                (2_396_848_268, "Zombie Force".to_string()),
                (2_147_483_649, "Other".to_string())
            ]
        );
        // Damaged: what came before.
        assert_eq!(shortcuts(&data[..40]), []);
        assert_eq!(shortcuts(b""), []);

        let steam = tempfile::tempdir().unwrap();
        let config = steam.path().join("userdata/1234/config");
        fs::create_dir_all(&config).unwrap();
        fs::write(config.join("shortcuts.vdf"), &data).unwrap();
        assert_eq!(
            shortcut_name(steam.path(), 2_396_848_268).as_deref(),
            Some("Zombie Force")
        );
        assert_eq!(shortcut_name(steam.path(), 7), None);
    }

    #[test]
    fn install_states() {
        use InstallState::*;
        for (state_flags, expected) in [
            (0, None),
            (1, None),
            (4, Some(Installed)),
            (6, Some(UpdateRequired)),       // update queued
            (4 | 512, Some(UpdateRequired)), // update paused
            (4 | 32, Some(UpdateRequired)),  // files missing
            (4 | 2 | 1024, Some(Updating)),
            (4 | 1_048_576, Some(Updating)),
            (1026, Some(Installing)),    // first install started
            (2 | 512, Some(Installing)), // first install paused
            (4 | 2048, Some(Uninstalling)),
        ] {
            assert_eq!(install_state(state_flags), expected, "{state_flags}");
        }
    }

    #[test]
    fn tools() {
        assert!(is_tool(228_980, "Steamworks Common Redistributables"));
        assert!(is_tool(1, "Proton 11.0"));
        assert!(is_tool(1, "Proton Experimental"));
        assert!(is_tool(1, "Steam Linux Runtime 5.0 (whatever)"));
        assert!(is_tool(2_805_730, "renamed"));
        assert!(!is_tool(620, "Portal 2"));
        assert!(!is_tool(1, "Protonaut"));
    }

    #[test]
    fn manifests() {
        let m = parse_manifest(
            r#""AppState"
            {
                "appid"		"105600"
                "Universe"		"1"
                "name"		"Terraria"
                "StateFlags"		"4"
                "installdir"		"Terraria"
                "LastUpdated"		"1788166757"
                "LastPlayed"		"1786300589"
                "SizeOnDisk"		"831365954"
                "InstalledDepots" { "105602" { "manifest" "42" "size" "831365954" } }
            }"#,
        )
        .unwrap();
        assert_eq!(
            m,
            Manifest {
                appid: 105_600,
                name: "Terraria".into(),
                state_flags: 4,
                size_bytes: 831_365_954,
                last_played: Some(1_786_300_589),
            }
        );
        let m = parse_manifest(r#""AppState" { "appid" "7" "installdir" "Dir" "LastPlayed" "0" }"#)
            .unwrap();
        assert_eq!(
            (m.name.as_str(), m.last_played, m.size_bytes),
            ("Dir", None, 0)
        );
        assert_eq!(
            parse_manifest(r#""AppState" { "appid" "7" }"#)
                .unwrap()
                .name,
            "App 7"
        );
        assert!(parse_manifest(r#""AppState" { "appid" "0" }"#).is_none());
        assert!(parse_manifest(r#""AppState" { "name" "x" }"#).is_none());
        assert!(parse_manifest(r#""AppState" { "appid" "7""#).is_none());
    }

    #[test]
    fn library_folder_formats() {
        let new = vdf::parse(
            r#""libraryfolders" { "0" { "path" "/a" "apps" { "1" "2" } } "1" { "path" "/b" } "contentstatsid" "-1" }"#,
        )
        .unwrap();
        assert_eq!(library_paths(&new), ["/a", "/b"]);
        let old = vdf::parse(
            r#""LibraryFolders" { "TimeNextStatsReport" "1" "ContentStatsID" "2" "1" "/mnt/games" }"#,
        )
        .unwrap();
        assert_eq!(library_paths(&old), ["/mnt/games"]);
        assert!(library_paths(&vdf::parse(r#""x" {}"#).unwrap()).is_empty());
    }

    #[test]
    fn finds_the_steam_dir() {
        let home = fixture_home();
        let steam = steam_dir(None, Some(&home)).unwrap();
        assert_eq!(steam, home.join(".local/share/Steam"));
        assert_eq!(
            steam_dir(Some(Path::new("/x")), Some(&home)),
            Some(PathBuf::from("/x"))
        );
        assert_eq!(steam_dir(None, Some(Path::new("/nonexistent"))), None);
        assert_eq!(steam_dir(None, None), None);
    }

    #[test]
    fn scans_the_fixture() {
        let steam = fixture_home().join(".local/share/Steam");
        let libs = libraries(&steam);
        assert_eq!(libs.len(), 2, "{libs:?}");
        assert!(libs[1].ends_with("SteamLibrary"));

        let games = scan(&steam);
        let ids: Vec<(u32, &str, InstallState)> = games
            .iter()
            .map(|g| (g.appid, g.name.as_str(), g.install_state))
            .collect();
        assert_eq!(
            ids,
            [
                (1_086_940, "Baldur's Gate 3", InstallState::Updating),
                (570, "Dota 2", InstallState::Installing),
                (1_145_360, "Hades", InstallState::UpdateRequired),
                (620, "Portal 2", InstallState::Installed),
            ]
        );
        let cache = steam.canonicalize().unwrap();
        let cache = cache.join("appcache/librarycache");
        let s = |p: &str| Some(cache.join(p).to_string_lossy().into_owned());

        let portal = &games[3];
        assert_eq!(portal.size_bytes, 12_976_394_567);
        assert_eq!(portal.last_played, Some(1_786_300_589));
        // Newest layout: hashed subdirectories win over the per-appid files.
        assert_eq!(
            portal.artwork,
            GameArtwork {
                portrait: s("620/7bd61d60b8e93c0e89e4072f27530fff7a802c93/library_capsule.jpg"),
                header: s("620/74d59d9f11bd9a2516106cb5522107cfcce0240f/library_header.jpg"),
                hero: s("620/b21199296d8ca5aff27b5b3b33c00aa9e3d7518b/library_hero.jpg"),
                logo: s("620/8a7f41b4461c1a4437dd9803079e974cf22f0be8/logo.png"),
                icon: s("620/2e478fc6874d06ae5baf0d147f6f21203291aa02.jpg"),
            }
        );
        let hades = &games[2];
        assert_eq!(hades.last_played, None);
        assert_eq!(
            hades.artwork,
            GameArtwork {
                portrait: s("1145360/library_600x900.jpg"),
                header: s("1145360/header.jpg"),
                hero: s("1145360/library_hero.jpg"),
                logo: s("1145360/logo.png"),
                icon: s("1145360/e033e23c29a192a17c16a7645a2b423ac64ff447.jpg"),
            }
        );
        let dota = &games[1];
        assert_eq!(
            dota.artwork,
            GameArtwork {
                portrait: s("570_library_600x900.jpg"),
                header: s("570_header.jpg"),
                hero: None,
                logo: None,
                icon: s("570_icon.jpg"),
            }
        );
        assert_eq!(games[0].artwork, GameArtwork::default());

        assert_eq!(find_manifest(&steam, 570).unwrap().name, "Dota 2");
        assert_eq!(find_manifest(&steam, 228_980).unwrap().appid, 228_980);
        assert!(find_manifest(&steam, 999).is_none());
    }

    #[test]
    fn newest_hashed_artwork_wins() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path();
        let old = cache.join("9/1111111111111111111111111111111111111111");
        let new = cache.join("9/2222222222222222222222222222222222222222");
        for d in [&old, &new] {
            fs::create_dir_all(d).unwrap();
            fs::write(d.join("library_hero.jpg"), b"x").unwrap();
        }
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let set = |p: &Path, t| {
            fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(t)
                .unwrap()
        };
        set(&old.join("library_hero.jpg"), t);
        set(
            &new.join("library_hero.jpg"),
            t + std::time::Duration::from_secs(60),
        );
        let art = artwork(cache, 9);
        assert_eq!(
            art.hero,
            Some(new.join("library_hero.jpg").to_string_lossy().into_owned())
        );
        set(
            &old.join("library_hero.jpg"),
            t + std::time::Duration::from_secs(120),
        );
        assert_eq!(
            artwork(cache, 9).hero,
            Some(old.join("library_hero.jpg").to_string_lossy().into_owned())
        );
        // Not a hash: ignored.
        fs::create_dir_all(cache.join("9/notahash")).unwrap();
        fs::write(cache.join("9/notahash/logo.png"), b"x").unwrap();
        assert_eq!(artwork(cache, 9).logo, None);
        assert_eq!(artwork(cache, 10), GameArtwork::default());
    }
}
