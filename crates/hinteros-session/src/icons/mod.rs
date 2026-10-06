//! App icons, resolved on the box at runtime. HinterOS never ships logos.
//!
//! Per app, in this order:
//! 1. `icon`: an absolute path, or an icon theme name ([`desktop::Dirs::lookup`]);
//! 2. `desktop_id`: the app's freedesktop `.desktop` file, its `Icon=`, looked up in the
//!    installed icon themes ([`desktop`]);
//! 3. `icon_url` / `site_url` (web apps): downloaded once from the site itself, cached under
//!    `$XDG_CACHE_HOME/hinteros/icons/<app-id>.<ext>`, refreshed when older than
//!    `icons.refresh_days` ([`web`]).
//!
//! Local lookups are quick and happen before the daemon starts serving. Downloads happen on
//! a background thread; each time the resolved set changes the daemon sends `apps_changed`.
//! Failures (offline, site changed) are logged and the app simply has no icon, or keeps its
//! stale cached one.

pub mod desktop;
pub mod web;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tracing::{debug, info, warn};

use crate::config::{AppConfig, IconsConfig};

/// Resolved icon paths by app id, shared between the shell (`apps.list`) and the fetcher.
#[derive(Debug, Clone, Default)]
pub struct IconStore(Arc<Mutex<HashMap<String, PathBuf>>>);

impl IconStore {
    pub fn get(&self, id: &str) -> Option<PathBuf> {
        self.0.lock().ok()?.get(id).cloned()
    }

    /// Set or clear an app's icon. Whether that changed anything.
    pub fn set(&self, id: &str, path: Option<PathBuf>) -> bool {
        let Ok(mut map) = self.0.lock() else {
            return false;
        };
        match path {
            Some(p) => map.insert(id.to_string(), p.clone()) != Some(p),
            None => map.remove(id).is_some(),
        }
    }
}

/// What an app's icon is resolved from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IconSpec {
    pub id: String,
    pub icon: Option<String>,
    pub desktop_id: Option<String>,
    pub web: Option<web::Source>,
}

impl IconSpec {
    pub fn from_app(app: &AppConfig) -> Self {
        Self {
            id: app.id.clone(),
            icon: app.icon.clone(),
            desktop_id: app.desktop_id.clone(),
            web: match (&app.icon_url, &app.site_url) {
                (Some(url), _) => Some(web::Source::IconUrl(url.clone())),
                (None, Some(url)) => Some(web::Source::Site(url.clone())),
                _ => None,
            },
        }
    }

    /// The built-in Steam entry: Steam's own desktop file.
    pub fn steam() -> Self {
        Self {
            id: hinteros_api::session::STEAM_CLIENT_APP_ID.into(),
            desktop_id: Some("steam".into()),
            ..Self::default()
        }
    }

    /// Everything that can be decided without the network.
    fn local(&self, dirs: &desktop::Dirs) -> Option<PathBuf> {
        self.icon
            .as_deref()
            .and_then(|i| dirs.lookup(i))
            .or_else(|| dirs.resolve_desktop(self.desktop_id.as_deref()?))
    }
}

const EXTENSIONS: [&str; 6] = ["png", "svg", "ico", "jpg", "gif", "webp"];

/// The cache of downloaded web icons.
#[derive(Debug, Clone)]
pub struct Cache {
    pub dir: PathBuf,
    pub max_age: Duration,
}

impl Cache {
    /// `$XDG_CACHE_HOME/hinteros/icons` (or `~/.cache/...`).
    pub fn default_dir() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".cache")))?;
        Some(base.join("hinteros/icons"))
    }

    /// The cached icon of `id` and whether it is still fresh.
    pub fn lookup(&self, id: &str) -> Option<(PathBuf, bool)> {
        EXTENSIONS.iter().find_map(|ext| {
            let path = self.dir.join(format!("{id}.{ext}"));
            let age = fs::metadata(&path)
                .ok()?
                .modified()
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok())
                .unwrap_or_default();
            Some((path, age < self.max_age))
        })
    }

    pub fn store(&self, id: &str, image: &web::Image) -> std::io::Result<PathBuf> {
        fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("{id}.{}", image.ext));
        let tmp = self.dir.join(format!(".{id}.tmp"));
        fs::write(&tmp, &image.bytes)?;
        fs::rename(&tmp, &path)?;
        // A different format than before: drop the old file.
        for ext in EXTENSIONS.iter().filter(|e| **e != image.ext) {
            let _ = fs::remove_file(self.dir.join(format!("{id}.{ext}")));
        }
        Ok(path)
    }
}

/// Resolve everything that needs no network: configured and desktop-file icons, and cached
/// web icons (fresh or stale). Returns the apps whose cache is missing or stale.
pub fn resolve_local(
    specs: &[IconSpec],
    dirs: &desktop::Dirs,
    cache: Option<&Cache>,
    store: &IconStore,
) -> (bool, Vec<IconSpec>) {
    let mut changed = false;
    let mut stale = Vec::new();
    for spec in specs {
        let local = spec.local(dirs);
        let mut icon = local.clone();
        if spec.web.is_some() && local.is_none() {
            // An explicit or desktop icon wins; a web icon is the fallback.
            match cache.and_then(|c| c.lookup(&spec.id)) {
                Some((path, fresh)) => {
                    icon = Some(path);
                    if !fresh {
                        stale.push(spec.clone());
                    }
                }
                None => stale.push(spec.clone()),
            }
        }
        changed |= store.set(&spec.id, icon);
    }
    (changed, stale)
}

/// Download the icons of `todo` and update the store. Returns whether the store changed and
/// the apps that failed.
pub fn fetch_pass(
    todo: &[IconSpec],
    fetcher: &web::Fetcher,
    cache: &Cache,
    store: &IconStore,
) -> (bool, Vec<IconSpec>) {
    let mut changed = false;
    let mut failed = Vec::new();
    for spec in todo {
        let Some(source) = &spec.web else { continue };
        match fetcher.fetch_icon(source) {
            Ok(image) => match cache.store(&spec.id, &image) {
                Ok(path) => {
                    info!(app = %spec.id, icon = %path.display(), "web icon cached");
                    changed |= store.set(&spec.id, Some(path));
                }
                Err(e) => {
                    warn!(app = %spec.id, "caching the icon: {e}");
                    failed.push(spec.clone());
                }
            },
            Err(e) => {
                warn!(app = %spec.id, "no web icon: {e:#}");
                failed.push(spec.clone());
            }
        }
    }
    (changed, failed)
}

/// Start the background fetcher: downloads missing and stale web icons, retries failures
/// (after 1 min, 10 min, 1 h), then re-checks staleness every 6 hours. `changed` is called
/// when the store changed.
pub fn spawn_fetcher(
    specs: Vec<IconSpec>,
    todo: Vec<IconSpec>,
    dirs: desktop::Dirs,
    cache: Cache,
    store: IconStore,
    fetcher: web::Fetcher,
    changed: impl Fn() + Send + 'static,
) {
    let spawned = std::thread::Builder::new()
        .name("icon-fetch".into())
        .spawn(move || {
            let retry = [60, 600, 3600].map(Duration::from_secs);
            let mut todo = todo;
            let mut attempt = 0;
            loop {
                if !todo.is_empty() {
                    let (did_change, failed) = fetch_pass(&todo, &fetcher, &cache, &store);
                    if did_change {
                        changed();
                    }
                    todo = failed;
                }
                let wait = if todo.is_empty() {
                    attempt = 0;
                    Duration::from_secs(6 * 3600)
                } else {
                    let w = retry[attempt.min(retry.len() - 1)];
                    attempt += 1;
                    w
                };
                std::thread::sleep(wait);
                if todo.is_empty() {
                    // Newly installed apps, expired caches.
                    let (did_change, stale) = resolve_local(&specs, &dirs, Some(&cache), &store);
                    if did_change {
                        changed();
                    }
                    debug!(stale = stale.len(), "icon cache check");
                    todo = stale;
                }
            }
        });
    if let Err(e) = spawned {
        warn!("cannot start the icon fetcher: {e}");
    }
}

/// Everything the daemon does with icons at startup: resolve locally now, fetch in the
/// background.
pub fn start(
    apps: &[AppConfig],
    cfg: &IconsConfig,
    store: &IconStore,
    changed: impl Fn() + Send + 'static,
) {
    let mut specs = vec![IconSpec::steam()];
    specs.extend(apps.iter().map(IconSpec::from_app));
    let mut dirs = desktop::Dirs::from_env(cfg.theme.as_deref());
    if let Some(only) = &cfg.data_dirs {
        dirs.data.clone_from(only);
        dirs.icon_roots.clear();
    }
    let cache = cfg
        .cache_dir
        .clone()
        .or_else(Cache::default_dir)
        .map(|dir| Cache {
            dir,
            max_age: Duration::from_secs(cfg.refresh_days * 86400),
        });
    let (_, todo) = resolve_local(&specs, &dirs, cache.as_ref(), store);
    match cache {
        Some(cache) if cfg.fetch => {
            if !todo.is_empty() {
                debug!(apps = todo.len(), "web icons to fetch");
            }
            spawn_fetcher(
                specs,
                todo,
                dirs,
                cache,
                store.clone(),
                web::Fetcher::default(),
                changed,
            );
        }
        _ if !todo.is_empty() => {
            info!("web icon fetching is off; apps without a cached icon show a placeholder")
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_image() -> web::Image {
        web::Image {
            bytes: b"\x89PNG\r\n\x1a\nx".to_vec(),
            ext: "png",
        }
    }

    #[test]
    fn cache_stores_replaces_and_ages() {
        let t = tempfile::tempdir().unwrap();
        let cache = Cache {
            dir: t.path().join("icons"),
            max_age: Duration::from_secs(3600),
        };
        assert_eq!(cache.lookup("yt"), None);
        let p = cache.store("yt", &png_image()).unwrap();
        assert_eq!(p, t.path().join("icons/yt.png"));
        assert_eq!(cache.lookup("yt"), Some((p.clone(), true)));
        let old = SystemTime::now() - Duration::from_secs(7200);
        fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert_eq!(cache.lookup("yt"), Some((p.clone(), false)));
        // A new format replaces the old file.
        let svg = web::Image {
            bytes: b"<svg/>".to_vec(),
            ext: "svg",
        };
        let p2 = cache.store("yt", &svg).unwrap();
        assert!(!p.exists());
        assert_eq!(cache.lookup("yt"), Some((p2, true)));
    }

    fn spec(id: &str) -> IconSpec {
        IconSpec {
            id: id.into(),
            ..IconSpec::default()
        }
    }

    #[test]
    fn local_resolution_order_and_staleness() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        let write = |rel: &str, text: &str| {
            let p = r.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, text).unwrap();
            p
        };
        let dirs = desktop::Dirs {
            data: vec![r.join("share")],
            icon_roots: vec![],
            theme: None,
        };
        write(
            "share/applications/native.desktop",
            "[Desktop Entry]\nIcon=native\n",
        );
        let themed = write("share/icons/hicolor/256x256/apps/native.png", "x");
        let explicit = write("mine.png", "x");
        let cache = Cache {
            dir: r.join("cache"),
            max_age: Duration::from_secs(60),
        };
        let cached = cache.store("web", &png_image()).unwrap();
        let web = Some(web::Source::Site("https://example.com".into()));
        let specs = [
            IconSpec {
                desktop_id: Some("native".into()),
                ..spec("native")
            },
            IconSpec {
                icon: Some(explicit.to_string_lossy().into()),
                desktop_id: Some("native".into()),
                ..spec("explicit")
            },
            IconSpec {
                web: web.clone(),
                ..spec("web")
            },
            IconSpec {
                web,
                ..spec("webless")
            },
            spec("none"),
        ];
        let store = IconStore::default();
        let (changed, stale) = resolve_local(&specs, &dirs, Some(&cache), &store);
        assert!(changed);
        assert_eq!(store.get("native"), Some(themed));
        assert_eq!(store.get("explicit"), Some(explicit));
        assert_eq!(store.get("web"), Some(cached));
        assert_eq!(store.get("webless"), None);
        assert_eq!(store.get("none"), None);
        // Only the uncached web app needs a download; the fresh cache does not.
        assert_eq!(
            stale.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["webless"]
        );
        // Nothing new the second time.
        assert!(!resolve_local(&specs, &dirs, Some(&cache), &store).0);
    }

    #[test]
    fn fetch_pass_caches_and_reports_changes_and_failures() {
        // A fixture server that serves one site.
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { return };
                let mut buf = [0u8; 1024];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let body: &[u8] = if req.starts_with("GET /logo.png") {
                    b"\x89PNG\r\n\x1a\nlogo"
                } else {
                    b""
                };
                let status = if body.is_empty() {
                    "404 Not Found"
                } else {
                    "200 OK"
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {status}\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(body);
            }
        });
        let t = tempfile::tempdir().unwrap();
        let cache = Cache {
            dir: t.path().join("c"),
            max_age: Duration::from_secs(60),
        };
        let store = IconStore::default();
        let ok = IconSpec {
            web: Some(web::Source::IconUrl(format!("{base}/logo.png"))),
            ..spec("ok")
        };
        let bad = IconSpec {
            web: Some(web::Source::IconUrl(format!("{base}/missing.png"))),
            ..spec("bad")
        };
        let fetcher = web::Fetcher::new(true);
        let (changed, failed) = fetch_pass(&[ok, bad.clone()], &fetcher, &cache, &store);
        assert!(changed);
        assert_eq!(failed, vec![bad]);
        assert_eq!(store.get("ok"), Some(t.path().join("c/ok.png")));
        assert_eq!(store.get("bad"), None);
        assert_eq!(
            fs::read(t.path().join("c/ok.png")).unwrap(),
            b"\x89PNG\r\n\x1a\nlogo"
        );
    }
}
