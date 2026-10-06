//! Icons of installed (native) apps: `.desktop` file -> `Icon=` -> icon theme lookup.
//!
//! Nothing here ships an image; it finds the files the app's own package installed. The
//! search follows the freedesktop Desktop Entry and Icon Theme specs closely enough for
//! apps: the data directories are `$XDG_DATA_HOME`, `$XDG_DATA_DIRS` and (NixOS and Flatpak
//! do not always export them) the system and per-user profile directories. Candidates come
//! from the active icon theme, the themes it inherits, and `hicolor`; the best one wins:
//! the largest PNG of at least 256 px, else an SVG, else the largest smaller PNG.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Where to look.
#[derive(Debug, Clone, Default)]
pub struct Dirs {
    /// Data directories (each has `applications/`, `icons/`, `pixmaps/`), most specific first.
    pub data: Vec<PathBuf>,
    /// Extra icon roots laid out like `icons/` (legacy `~/.icons`).
    pub icon_roots: Vec<PathBuf>,
    /// The active icon theme, if known.
    pub theme: Option<String>,
}

impl Dirs {
    /// The data directories of this session. `theme` overrides the one found in the GTK
    /// settings.
    pub fn from_env(theme: Option<&str>) -> Self {
        let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
        let home = env("HOME").map(PathBuf::from);
        let mut data = Vec::new();
        match env("XDG_DATA_HOME") {
            Some(d) => data.push(PathBuf::from(d)),
            None => data.extend(home.iter().map(|h| h.join(".local/share"))),
        }
        match env("XDG_DATA_DIRS") {
            Some(dirs) => data.extend(std::env::split_paths(&dirs)),
            None => {
                data.push("/usr/local/share".into());
                data.push("/usr/share".into());
            }
        }
        data.push("/run/current-system/sw/share".into());
        if let Some(user) = env("USER") {
            data.push(Path::new("/etc/profiles/per-user").join(user).join("share"));
        }
        if let Some(h) = &home {
            data.push(h.join(".nix-profile/share"));
            data.push(h.join(".local/share/flatpak/exports/share"));
        }
        data.push("/nix/var/nix/profiles/default/share".into());
        data.push("/var/lib/flatpak/exports/share".into());
        let mut seen = BTreeSet::new();
        data.retain(|d| d.is_absolute() && seen.insert(d.clone()));
        let config = env("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".config")));
        let theme = theme
            .map(String::from)
            .or_else(|| config.and_then(|c| gtk_icon_theme(&c)));
        Self {
            data,
            icon_roots: home.iter().map(|h| h.join(".icons")).collect(),
            theme,
        }
    }

    /// `applications/<id>.desktop` in the data directories (an `a-b` id may also be
    /// `applications/a/b.desktop`, per the spec).
    pub fn find_desktop_file(&self, desktop_id: &str) -> Option<PathBuf> {
        let id = desktop_id.strip_suffix(".desktop").unwrap_or(desktop_id);
        if id.is_empty() || id.contains("..") || id.starts_with('/') {
            return None;
        }
        let nested = id.replace('-', "/");
        self.data.iter().find_map(|d| {
            [
                d.join("applications").join(format!("{id}.desktop")),
                d.join("applications").join(format!("{nested}.desktop")),
            ]
            .into_iter()
            .find(|p| p.is_file())
        })
    }

    /// The icon file for a `.desktop` id, if its file and its icon can be found.
    pub fn resolve_desktop(&self, desktop_id: &str) -> Option<PathBuf> {
        let file = self.find_desktop_file(desktop_id)?;
        let name = desktop_icon_name(&fs::read_to_string(file).ok()?)?;
        self.lookup(&name)
    }

    /// An `Icon=` value or icon name: an absolute path as is, else the best file for the name.
    pub fn lookup(&self, icon: &str) -> Option<PathBuf> {
        let path = Path::new(icon);
        if path.is_absolute() {
            return path.is_file().then(|| path.to_path_buf());
        }
        if icon.is_empty() || icon.contains('/') {
            return None;
        }
        let name = ["png", "svg", "xpm"]
            .iter()
            .find_map(|e| icon.strip_suffix(&format!(".{e}")))
            .unwrap_or(icon);
        let mut best: Option<(Rank, PathBuf)> = None;
        let mut consider = |rank: Rank, path: PathBuf| {
            if best.as_ref().is_none_or(|(r, _)| rank > *r) {
                best = Some((rank, path));
            }
        };
        let roots: Vec<PathBuf> = self
            .data
            .iter()
            .map(|d| d.join("icons"))
            .chain(self.icon_roots.iter().cloned())
            .collect();
        for theme in self.theme_chain(&roots) {
            for root in &roots {
                for (rank, path) in theme_candidates(&root.join(&theme), name) {
                    consider(rank, path);
                }
            }
        }
        for d in &self.data {
            for ext in ["png", "svg"] {
                let p = d.join("pixmaps").join(format!("{name}.{ext}"));
                if p.is_file() {
                    // Pixmaps have no declared size; treat them as a last resort.
                    consider(Rank::of(ext == "svg", 0, true), p);
                }
            }
        }
        best.map(|(_, p)| p)
    }

    /// The active theme, the themes it inherits (transitively), then `hicolor`.
    fn theme_chain(&self, roots: &[PathBuf]) -> Vec<String> {
        let mut chain: Vec<String> = Vec::new();
        let mut todo: Vec<String> = self.theme.iter().cloned().collect();
        while let Some(t) = todo.pop() {
            if chain.contains(&t) || chain.len() >= 16 {
                continue;
            }
            for root in roots {
                if let Ok(text) = fs::read_to_string(root.join(&t).join("index.theme")) {
                    let inherits = ini_value(&text, "Icon Theme", "Inherits").unwrap_or_default();
                    todo.extend(
                        inherits
                            .split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                            .rev(),
                    );
                    break;
                }
            }
            chain.push(t);
        }
        if !chain.iter().any(|t| t == "hicolor") {
            chain.push("hicolor".into());
        }
        chain
    }
}

/// Quality of an icon file; larger is better.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Rank(u8, u32);

impl Rank {
    fn of(svg: bool, size: u32, last_resort: bool) -> Self {
        if last_resort {
            return Rank(0, if svg { 1 } else { 0 });
        }
        match (svg, size) {
            (false, s) if s >= 256 => Rank(3, s),
            (true, _) => Rank(2, 0),
            (false, s) => Rank(1, s),
        }
    }
}

/// Icon files called `name` in one theme directory: `<size>/apps/`, `apps/<size>/`,
/// `scalable/apps/`, `apps/scalable/`.
fn theme_candidates(theme_dir: &Path, name: &str) -> Vec<(Rank, PathBuf)> {
    let mut out = Vec::new();
    let Ok(top) = fs::read_dir(theme_dir) else {
        return out;
    };
    for a in top.flatten() {
        let a_name = a.file_name().to_string_lossy().into_owned();
        // fs::metadata follows symlinks (profile directories are full of them).
        if !fs::metadata(a.path()).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        if a_name == "apps" {
            if let Ok(subs) = fs::read_dir(a.path()) {
                for b in subs.flatten() {
                    let size = size_of_dir(&b.file_name().to_string_lossy());
                    push_files(&mut out, &b.path(), name, size);
                }
            }
        } else if let Some(size) = size_of_dir(&a_name) {
            push_files(&mut out, &a.path().join("apps"), name, Some(size));
        }
    }
    out
}

fn push_files(out: &mut Vec<(Rank, PathBuf)>, dir: &Path, name: &str, size: Option<DirSize>) {
    let Some(size) = size else { return };
    for (ext, svg) in [("png", false), ("svg", true)] {
        let p = dir.join(format!("{name}.{ext}"));
        if p.is_file() {
            let px = match size {
                DirSize::Pixels(s) => s,
                DirSize::Scalable => 0,
            };
            out.push((Rank::of(svg, px, false), p));
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum DirSize {
    Pixels(u32),
    Scalable,
}

/// `256x256`, `256x256@2` (512 px), `256`, `scalable`; anything else (`symbolic`, ...) no.
fn size_of_dir(name: &str) -> Option<DirSize> {
    if name == "scalable" {
        return Some(DirSize::Scalable);
    }
    let (base, scale) = match name.split_once('@') {
        Some((b, s)) => (b, s.parse::<u32>().ok()?),
        None => (name, 1),
    };
    let w = match base.split_once('x') {
        Some((w, h)) if w == h => w,
        Some(_) => return None,
        None => base,
    };
    Some(DirSize::Pixels(w.parse::<u32>().ok()?.checked_mul(scale)?))
}

/// The value of `key` in `[section]` of an INI-style file (unlocalized keys only).
pub fn ini_value(text: &str, section: &str, key: &str) -> Option<String> {
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            inside = line == format!("[{section}]");
        } else if inside
            && !line.starts_with('#')
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// `Icon=` of a `.desktop` file's `[Desktop Entry]` group.
pub fn desktop_icon_name(text: &str) -> Option<String> {
    ini_value(text, "Desktop Entry", "Icon").filter(|s| !s.is_empty())
}

/// `gtk-icon-theme-name` from GTK's settings, if set.
fn gtk_icon_theme(config_dir: &Path) -> Option<String> {
    ["gtk-4.0", "gtk-3.0"].iter().find_map(|v| {
        let text = fs::read_to_string(config_dir.join(v).join("settings.ini")).ok()?;
        ini_value(&text, "Settings", "gtk-icon-theme-name").filter(|s| !s.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(root: &Path, rel: &str) -> PathBuf {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, b"x").unwrap();
        p
    }

    fn dirs(root: &Path, theme: Option<&str>) -> Dirs {
        Dirs {
            data: vec![root.join("a"), root.join("b")],
            icon_roots: vec![],
            theme: theme.map(String::from),
        }
    }

    #[test]
    fn parses_desktop_entry_icon() {
        let text = "[Desktop Entry]\nName=X\nIcon[de]=nope\nIcon=jellyfin\n[Desktop Action a]\nIcon=other\n";
        assert_eq!(desktop_icon_name(text).as_deref(), Some("jellyfin"));
        assert_eq!(desktop_icon_name("[Other]\nIcon=x\n"), None);
    }

    #[test]
    fn finds_desktop_files_in_data_dirs() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        let f = touch(r, "b/applications/org.jellyfin.JellyfinDesktop.desktop");
        let nested = touch(r, "a/applications/kde/app.desktop");
        let d = dirs(r, None);
        assert_eq!(
            d.find_desktop_file("org.jellyfin.JellyfinDesktop"),
            Some(f.clone())
        );
        assert_eq!(
            d.find_desktop_file("org.jellyfin.JellyfinDesktop.desktop"),
            Some(f)
        );
        assert_eq!(d.find_desktop_file("kde-app"), Some(nested));
        assert_eq!(d.find_desktop_file("missing"), None);
        assert_eq!(d.find_desktop_file("../etc/x"), None);
    }

    #[test]
    fn prefers_large_png_then_svg_then_small_png() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        touch(r, "a/icons/hicolor/48x48/apps/app.png");
        touch(r, "a/icons/hicolor/128x128/apps/app.png");
        touch(r, "a/icons/hicolor/scalable/apps/app.svg");
        let d = dirs(r, None);
        assert_eq!(
            d.lookup("app"),
            Some(r.join("a/icons/hicolor/scalable/apps/app.svg"))
        );
        touch(r, "b/icons/hicolor/256x256/apps/app.png");
        touch(r, "b/icons/hicolor/512x512/apps/app.png");
        assert_eq!(
            d.lookup("app"),
            Some(r.join("b/icons/hicolor/512x512/apps/app.png"))
        );
        // Without the SVG and big PNGs, the largest small PNG.
        fs::remove_dir_all(r.join("b")).unwrap();
        fs::remove_dir_all(r.join("a/icons/hicolor/scalable")).unwrap();
        assert_eq!(
            d.lookup("app"),
            Some(r.join("a/icons/hicolor/128x128/apps/app.png"))
        );
        assert_eq!(d.lookup("other"), None);
    }

    #[test]
    fn follows_theme_inheritance_and_alternative_layouts() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        touch(r, "a/icons/Fancy/index.theme");
        fs::write(
            r.join("a/icons/Fancy/index.theme"),
            "[Icon Theme]\nName=Fancy\nInherits=Base,hicolor\n",
        )
        .unwrap();
        touch(r, "a/icons/Base/apps/512/tool.png");
        let d = dirs(r, Some("Fancy"));
        assert_eq!(
            d.lookup("tool"),
            Some(r.join("a/icons/Base/apps/512/tool.png"))
        );
        // Extension in Icon= is tolerated; `@2` sizes count double.
        touch(r, "a/icons/hicolor/256x256@2/apps/big.png");
        assert_eq!(
            d.lookup("big.png"),
            Some(r.join("a/icons/hicolor/256x256@2/apps/big.png"))
        );
    }

    #[test]
    fn pixmaps_and_absolute_paths() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        let pix = touch(r, "b/pixmaps/old.png");
        let abs = touch(r, "elsewhere/logo.png");
        let d = dirs(r, None);
        assert_eq!(d.lookup("old"), Some(pix));
        assert_eq!(d.lookup(abs.to_str().unwrap()), Some(abs));
        assert_eq!(d.lookup("/no/such/file.png"), None);
        assert_eq!(d.lookup("sub/dir"), None);
        // Theme icons beat pixmaps.
        touch(r, "a/icons/hicolor/64x64/apps/old.png");
        assert_eq!(
            d.lookup("old"),
            Some(r.join("a/icons/hicolor/64x64/apps/old.png"))
        );
    }

    #[test]
    fn resolves_through_the_desktop_file() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        let desktop = r.join("a/applications/org.example.App.desktop");
        fs::create_dir_all(desktop.parent().unwrap()).unwrap();
        fs::write(
            &desktop,
            "[Desktop Entry]\nName=App\nIcon=org.example.App\n",
        )
        .unwrap();
        let png = touch(r, "a/icons/hicolor/512x512/apps/org.example.App.png");
        let d = dirs(r, None);
        assert_eq!(d.resolve_desktop("org.example.App"), Some(png));
        assert_eq!(d.resolve_desktop("org.example.Missing"), None);
    }

    #[test]
    fn reads_gtk_theme_setting() {
        let t = tempfile::tempdir().unwrap();
        touch(t.path(), "gtk-3.0/settings.ini");
        fs::write(
            t.path().join("gtk-3.0/settings.ini"),
            "[Settings]\ngtk-icon-theme-name=Papirus\n",
        )
        .unwrap();
        assert_eq!(gtk_icon_theme(t.path()).as_deref(), Some("Papirus"));
    }
}
