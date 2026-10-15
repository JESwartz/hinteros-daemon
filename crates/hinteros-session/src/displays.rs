//! App displays: every app the daemon starts under the reaper runs on an X display of its own,
//! so that no app can read, grab or type into another one (`hinteros-docs/design/sandbox.md`).
//!
//! gamescope makes its Xwaylands at start only (`--xwayland-count`): `:0` for the home app, the
//! OSK and Steam, `:1` for Steam's games, then the app displays, starting at
//! `[gamescope] apps_display` (`"auto+2"`). How many there are is a setting of the profile
//! (`apps.displays.set`, kept in `$XDG_STATE_HOME/hinteros/app-displays`), which the
//! compositor wrapper reads before it starts gamescope and writes, as the number this session
//! has, to `$XDG_RUNTIME_DIR/hinteros/app-displays` (the system repo's `session.nix`). A change
//! applies at the next session start.
//!
//! [`DisplayPool`] knows the displays; the shell decides which is free (no running app of ours
//! has it, and no process started on it is left: see `ProcessControl::display_users`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hinteros_api::session::{APP_DISPLAYS_MAX, APP_DISPLAYS_MIN, AppDisplays};
use tracing::warn;

/// The setting's file, relative to `$XDG_STATE_HOME` (else `~/.local/state`).
pub const SETTING_FILE: &str = "hinteros/app-displays";
/// What this session has, relative to `$XDG_RUNTIME_DIR` (written by the compositor wrapper).
pub const ACTIVE_FILE: &str = "hinteros/app-displays";

/// Resolves a display setting (`"auto+<n>"`) to a display that exists now (`":<n>"`).
pub type Resolver = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The app displays of this session, and the setting for the next one.
#[derive(Clone)]
pub struct DisplayPool {
    /// The first app display (`gamescope.apps_display`); `None`: apps share the daemon's
    /// display and nothing limits them (no gamescope).
    first: Option<String>,
    /// The setting's default, and this session's number when the wrapper wrote none.
    default: u32,
    setting_file: Option<PathBuf>,
    active_file: Option<PathBuf>,
    resolve: Resolver,
}

impl std::fmt::Debug for DisplayPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DisplayPool")
            .field("first", &self.first)
            .field("default", &self.default)
            .field("setting_file", &self.setting_file)
            .field("active_file", &self.active_file)
            .finish_non_exhaustive()
    }
}

impl DisplayPool {
    /// The pool starting at `first` (`None`: no pool), with the default files and the real
    /// display lookup.
    pub fn new(first: Option<String>, default: u32) -> Self {
        Self {
            first,
            default: clamp(default),
            setting_file: state_dir().map(|d| d.join(SETTING_FILE)),
            active_file: std::env::var_os("XDG_RUNTIME_DIR")
                .filter(|d| !d.is_empty())
                .map(|d| PathBuf::from(d).join(ACTIVE_FILE)),
            resolve: Arc::new(crate::config::try_resolve_display),
        }
    }

    /// Use these files instead (tests).
    pub fn with_files(mut self, setting: Option<PathBuf>, active: Option<PathBuf>) -> Self {
        self.setting_file = setting;
        self.active_file = active;
        self
    }

    /// Resolve display settings with `resolve` instead (tests).
    pub fn with_resolver(mut self, resolve: Resolver) -> Self {
        self.resolve = resolve;
        self
    }

    /// Whether apps get displays of their own.
    pub fn enabled(&self) -> bool {
        self.first.is_some()
    }

    /// The first app display's setting, if any (for messages).
    pub fn first(&self) -> Option<&str> {
        self.first.as_deref()
    }

    /// How many app displays this session has (`None`: no pool).
    pub fn active(&self) -> Option<u32> {
        self.first.as_ref()?;
        Some(
            self.active_file
                .as_deref()
                .and_then(read_count)
                .unwrap_or(self.default),
        )
    }

    /// The setting: how many the next session starts with.
    pub fn setting(&self) -> u32 {
        self.setting_file
            .as_deref()
            .and_then(read_count)
            .unwrap_or(self.default)
    }

    /// Save the setting. `count` must be in range.
    pub fn set(&self, count: u32) -> std::io::Result<()> {
        if !(APP_DISPLAYS_MIN..=APP_DISPLAYS_MAX).contains(&count) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("count must be {APP_DISPLAYS_MIN} to {APP_DISPLAYS_MAX}"),
            ));
        }
        let path = self
            .setting_file
            .as_deref()
            .ok_or_else(|| std::io::Error::other("neither XDG_STATE_HOME nor HOME is set"))?;
        save(path, count)
    }

    /// What `apps.displays.get` answers.
    pub fn info(&self) -> AppDisplays {
        AppDisplays {
            count: self.setting(),
            active: self.active(),
            min: APP_DISPLAYS_MIN,
            max: APP_DISPLAYS_MAX,
        }
    }

    /// The app displays that exist now, in order (`"<n>"` resolved); missing ones are left out.
    pub fn displays(&self) -> Vec<String> {
        let (Some(first), Some(n)) = (&self.first, self.active()) else {
            return Vec::new();
        };
        (0..n)
            .filter_map(|i| slot_setting(first, i))
            .filter_map(|s| (self.resolve)(&s))
            .collect()
    }
}

/// The setting of app display `i` (0: `first`): `"auto+<k>"` gives `"auto+<k+i>"`, `":<n>"`
/// gives `":<n+i>"`.
pub fn slot_setting(first: &str, i: u32) -> Option<String> {
    if let Some(rest) = first.strip_prefix("auto") {
        let k: u32 = match rest {
            "" => 0,
            r => r.strip_prefix('+')?.parse().ok()?,
        };
        return Some(format!("auto+{}", k.checked_add(i)?));
    }
    let n: u32 = first.strip_prefix(':')?.parse().ok()?;
    Some(format!(":{}", n.checked_add(i)?))
}

fn clamp(n: u32) -> u32 {
    n.clamp(APP_DISPLAYS_MIN, APP_DISPLAYS_MAX)
}

/// A count file: one number in range. Missing: `None`; anything else is ignored with a warning.
fn read_count(path: &Path) -> Option<u32> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!(file = %path.display(), "ignoring the number of app displays: {e}");
            return None;
        }
    };
    match text.trim().parse::<u32>() {
        Ok(n) if (APP_DISPLAYS_MIN..=APP_DISPLAYS_MAX).contains(&n) => Some(n),
        _ => {
            warn!(file = %path.display(), "ignoring the number of app displays: not 1 to 8");
            None
        }
    }
}

/// `$XDG_STATE_HOME`, else `~/.local/state`.
fn state_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| Path::new(&h).join(".local/state"))
        })
}

/// Write the file atomically (a temporary file in the same directory, then rename).
fn save(path: &Path, count: u32) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    writeln!(tmp, "{count}")?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(dir: &Path, first: Option<&str>) -> DisplayPool {
        DisplayPool::new(first.map(String::from), 4)
            .with_files(Some(dir.join("state")), Some(dir.join("active")))
            // auto+<k> is :<10+k>, up to auto+5.
            .with_resolver(Arc::new(|s: &str| {
                let k: u32 = s.strip_prefix("auto+")?.parse().ok()?;
                (k <= 5).then(|| format!(":{}", 10 + k))
            }))
    }

    #[test]
    fn slot_settings() {
        assert_eq!(slot_setting("auto+2", 0).as_deref(), Some("auto+2"));
        assert_eq!(slot_setting("auto+2", 3).as_deref(), Some("auto+5"));
        assert_eq!(slot_setting("auto", 1).as_deref(), Some("auto+1"));
        assert_eq!(slot_setting(":2", 2).as_deref(), Some(":4"));
        assert_eq!(slot_setting("x", 0), None);
        assert_eq!(slot_setting(&format!(":{}", u32::MAX), 1), None);
    }

    #[test]
    fn setting_and_active_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = pool(dir.path(), Some("auto+2"));
        // Nothing written: the default.
        assert_eq!(p.setting(), 4);
        assert_eq!(p.active(), Some(4));
        p.set(6).unwrap();
        assert_eq!(p.setting(), 6);
        // The running session keeps what it started with.
        assert_eq!(p.active(), Some(4));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("state")).unwrap(),
            "6\n"
        );
        std::fs::write(dir.path().join("active"), "2\n").unwrap();
        assert_eq!(p.active(), Some(2));
        assert_eq!(
            p.info(),
            AppDisplays {
                count: 6,
                active: Some(2),
                min: 1,
                max: 8
            }
        );
        for bad in [0, 9] {
            assert!(p.set(bad).is_err());
        }
        assert_eq!(p.setting(), 6);
        // Garbage is ignored.
        std::fs::write(dir.path().join("active"), "12").unwrap();
        assert_eq!(p.active(), Some(4));
        std::fs::write(dir.path().join("state"), "lots").unwrap();
        assert_eq!(p.setting(), 4);
    }

    #[test]
    fn displays_that_exist() {
        let dir = tempfile::tempdir().unwrap();
        let p = pool(dir.path(), Some("auto+2"));
        assert_eq!(p.displays(), [":12", ":13", ":14", ":15"]);
        std::fs::write(dir.path().join("active"), "2").unwrap();
        assert_eq!(p.displays(), [":12", ":13"]);
        // More than gamescope made: only those that exist.
        std::fs::write(dir.path().join("active"), "8").unwrap();
        assert_eq!(p.displays(), [":12", ":13", ":14", ":15"]);
    }

    #[test]
    fn no_pool_without_a_first_display() {
        let dir = tempfile::tempdir().unwrap();
        let p = pool(dir.path(), None);
        assert!(!p.enabled());
        assert_eq!(p.active(), None);
        assert!(p.displays().is_empty());
        // The setting still works (for the next session).
        p.set(3).unwrap();
        assert_eq!(p.info().count, 3);
        assert_eq!(p.info().active, None);
    }
}
