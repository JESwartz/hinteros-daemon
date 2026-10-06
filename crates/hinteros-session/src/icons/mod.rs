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
//! 4. `firefox_profile` (web apps): the favicon Firefox itself stored in the app's kiosk profile
//!    when it visited `site_url` ([`firefox`]); for sites that refuse non-browser fetches. It is
//!    exported to the cache as `<app-id>.firefox.<ext>`, so a downloaded icon always wins, and
//!    is re-read when the app exits (Firefox has just stored the favicon it saw).
//!
//! Local lookups are quick and happen before the daemon starts serving. Downloads happen on
//! a background thread; each time the resolved set changes the daemon sends `apps_changed`.
//! Failures (offline, site changed) are logged and the app simply has no icon, or keeps its
//! stale cached one.

pub mod desktop;
pub mod firefox;
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
    /// The app's Firefox profile directory and the page it shows.
    pub firefox: Option<(PathBuf, String)>,
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
            firefox: app
                .firefox_profile
                .as_deref()
                .map(crate::config::expand_home)
                .zip(app.site_url.clone()),
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

    /// The icon exported from the app's Firefox profile, if any.
    pub fn lookup_firefox(&self, id: &str) -> Option<PathBuf> {
        EXTENSIONS
            .iter()
            .map(|ext| self.dir.join(format!("{id}.firefox.{ext}")))
            .find(|p| p.is_file())
    }

    /// Cache a downloaded icon (always rewritten: that renews its age). It replaces an
    /// exported Firefox icon.
    pub fn store(&self, id: &str, image: &web::Image) -> std::io::Result<PathBuf> {
        let (path, _) = self.write(id, image, true)?;
        self.remove_others(&format!("{id}.firefox"), None);
        Ok(path)
    }

    /// Cache an icon exported from Firefox. Whether the file's content changed.
    pub fn store_firefox(&self, id: &str, image: &web::Image) -> std::io::Result<(PathBuf, bool)> {
        self.write(&format!("{id}.firefox"), image, false)
    }

    fn write(
        &self,
        stem: &str,
        image: &web::Image,
        renew: bool,
    ) -> std::io::Result<(PathBuf, bool)> {
        fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("{stem}.{}", image.ext));
        let changed = fs::read(&path).map_or(true, |old| old != image.bytes);
        if changed || renew {
            let tmp = self.dir.join(format!(".{stem}.tmp"));
            fs::write(&tmp, &image.bytes)?;
            fs::rename(&tmp, &path)?;
        }
        // A different format than before: drop the old file.
        self.remove_others(stem, Some(image.ext));
        Ok((path, changed))
    }

    fn remove_others(&self, stem: &str, keep: Option<&str>) {
        for ext in EXTENSIONS.iter().filter(|e| Some(**e) != keep) {
            let _ = fs::remove_file(self.dir.join(format!("{stem}.{ext}")));
        }
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
        if icon.is_none() && spec.firefox.is_some() {
            // No downloaded icon (yet): what Firefox stored for the site, if anything.
            icon = cache.and_then(|c| {
                export_firefox(spec, c)
                    .map(|(path, _)| path)
                    .or_else(|| c.lookup_firefox(&spec.id))
            });
        }
        changed |= store.set(&spec.id, icon);
    }
    (changed, stale)
}

/// Export the favicon of the app's Firefox profile to the cache. The path and whether the
/// cached file changed; `None` when there is no profile, database or icon (errors are logged).
fn export_firefox(spec: &IconSpec, cache: &Cache) -> Option<(PathBuf, bool)> {
    let (profile, site) = spec.firefox.as_ref()?;
    let image = match firefox::favicon(profile, site) {
        Ok(Some(image)) => image,
        Ok(None) => {
            debug!(app = %spec.id, profile = %profile.display(), "no Firefox favicon for the site");
            return None;
        }
        Err(e) => {
            warn!(app = %spec.id, "reading the Firefox favicon: {e:#}");
            return None;
        }
    };
    match cache.store_firefox(&spec.id, &image) {
        Ok((path, changed)) => {
            if changed {
                info!(app = %spec.id, icon = %path.display(), "icon exported from the Firefox profile");
            }
            Some((path, changed))
        }
        Err(e) => {
            warn!(app = %spec.id, "caching the Firefox favicon: {e}");
            None
        }
    }
}

/// Re-read an app's Firefox favicon (after the app exited, when Firefox has just stored what
/// it saw). Only when nothing better exists: no explicit/desktop icon, no downloaded one.
/// Whether the app's icon changed.
pub fn refresh_firefox(
    spec: &IconSpec,
    dirs: &desktop::Dirs,
    cache: &Cache,
    store: &IconStore,
) -> bool {
    if spec.firefox.is_none() || spec.local(dirs).is_some() || cache.lookup(&spec.id).is_some() {
        return false;
    }
    match export_firefox(spec, cache) {
        Some((path, content_changed)) => store.set(&spec.id, Some(path)) | content_changed,
        None => false,
    }
}

/// Called when an app exited: refreshes its icon in the background and reports a change.
#[derive(Clone)]
pub struct Refresher(Arc<dyn Fn(&str) + Send + Sync>);

impl Refresher {
    /// Does nothing.
    pub fn none() -> Self {
        Self(Arc::new(|_| {}))
    }

    pub fn app_exited(&self, id: &str) {
        (self.0)(id);
    }
}

impl std::fmt::Debug for Refresher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Refresher")
    }
}

impl Default for Refresher {
    fn default() -> Self {
        Self::none()
    }
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
        let result = fetcher.fetch_icon(source);
        if result.is_err()
            && cache.lookup(&spec.id).is_none()
            && let Some((path, _)) = export_firefox(spec, cache)
        {
            // The site refuses us (or is down), but Firefox has seen it: use its icon. The
            // download is still retried later.
            changed |= store.set(&spec.id, Some(path));
        }
        match result {
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
/// background. The returned [`Refresher`] re-reads Firefox favicons when apps exit.
pub fn start(
    apps: &[AppConfig],
    cfg: &IconsConfig,
    store: &IconStore,
    changed: impl Fn() + Send + Sync + 'static,
) -> Refresher {
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
    let changed = Arc::new(changed);
    let refresher = match &cache {
        Some(cache) if specs.iter().any(|s| s.firefox.is_some()) => {
            let (specs, dirs, cache, store) =
                (specs.clone(), dirs.clone(), cache.clone(), store.clone());
            let changed = changed.clone();
            Refresher(Arc::new(move |id| {
                let Some(spec) = specs.iter().find(|s| s.id == id && s.firefox.is_some()) else {
                    return;
                };
                let (spec, dirs, cache, store, changed) = (
                    spec.clone(),
                    dirs.clone(),
                    cache.clone(),
                    store.clone(),
                    changed.clone(),
                );
                // SQLite and file copies: not on the shell's thread.
                let spawned = std::thread::Builder::new()
                    .name("icon-refresh".into())
                    .spawn(move || {
                        if refresh_firefox(&spec, &dirs, &cache, &store) {
                            changed();
                        }
                    });
                if let Err(e) = spawned {
                    warn!("cannot refresh the icon: {e}");
                }
            }))
        }
        _ => Refresher::none(),
    };
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
                move || changed(),
            );
        }
        _ if !todo.is_empty() => {
            info!("web icon fetching is off; apps without a cached icon show a placeholder")
        }
        _ => {}
    }
    refresher
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

    #[test]
    fn firefox_icon_is_the_last_resort_and_refreshes_after_exit() {
        use firefox::tests::{PNG, profile};
        let t = tempfile::tempdir().unwrap();
        let cache = Cache {
            dir: t.path().join("cache"),
            max_age: Duration::from_secs(60),
        };
        let dirs = desktop::Dirs {
            data: vec![],
            icon_roots: vec![],
            theme: None,
        };
        let site = "https://www.crunchyroll.com/";
        let empty = profile(&[]);
        let spec = IconSpec {
            // The site refuses non-browser fetches: nothing can be downloaded.
            web: Some(web::Source::Site("https://127.0.0.1:1/".into())),
            firefox: Some((empty.path().to_path_buf(), site.into())),
            ..spec("crunchy")
        };
        let store = IconStore::default();

        // Firefox has not visited the site yet: no icon, the download is wanted.
        let (changed, stale) =
            resolve_local(std::slice::from_ref(&spec), &dirs, Some(&cache), &store);
        assert!(!changed && stale.len() == 1);
        assert_eq!(store.get("crunchy"), None);
        assert!(!refresh_firefox(&spec, &dirs, &cache, &store));

        // The app runs, Firefox stores the favicon, the app exits: the icon appears.
        let visited = profile(&[(site, "https://www.crunchyroll.com/i.png", 180, PNG)]);
        let spec = IconSpec {
            firefox: Some((visited.path().to_path_buf(), site.into())),
            ..spec
        };
        assert!(refresh_firefox(&spec, &dirs, &cache, &store));
        let path = t.path().join("cache/crunchy.firefox.png");
        assert_eq!(store.get("crunchy"), Some(path.clone()));
        assert_eq!(fs::read(&path).unwrap(), PNG);
        // Nothing new the second time.
        assert!(!refresh_firefox(&spec, &dirs, &cache, &store));

        // A restart finds it again, and a failed download keeps it.
        let fresh = IconStore::default();
        let (changed, _) = resolve_local(std::slice::from_ref(&spec), &dirs, Some(&cache), &fresh);
        assert!(changed);
        assert_eq!(fresh.get("crunchy"), Some(path.clone()));
        let fetcher = web::Fetcher::new(true);
        let (_, failed) = fetch_pass(std::slice::from_ref(&spec), &fetcher, &cache, &fresh);
        assert_eq!(failed.len(), 1);
        assert_eq!(fresh.get("crunchy"), Some(path.clone()));

        // A downloaded icon beats it and replaces the exported file.
        cache.store("crunchy", &png_image()).unwrap();
        assert!(!path.exists());
        assert!(!refresh_firefox(&spec, &dirs, &cache, &store));
        let (changed, _) = resolve_local(std::slice::from_ref(&spec), &dirs, Some(&cache), &store);
        assert!(changed);
        assert_eq!(
            store.get("crunchy"),
            Some(t.path().join("cache/crunchy.png"))
        );

        // An explicit icon beats everything, and refreshing leaves it alone.
        let explicit = t.path().join("mine.png");
        fs::write(&explicit, "x").unwrap();
        let spec = IconSpec {
            icon: Some(explicit.to_string_lossy().into()),
            ..spec
        };
        let other = IconStore::default();
        resolve_local(std::slice::from_ref(&spec), &dirs, Some(&cache), &other);
        assert_eq!(other.get("crunchy"), Some(explicit));
        assert!(!refresh_firefox(&spec, &dirs, &cache, &other));
    }
}
