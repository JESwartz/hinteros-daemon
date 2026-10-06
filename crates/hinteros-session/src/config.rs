//! Configuration: a TOML file (all keys optional) plus command-line overrides.
//!
//! ```toml
//! [launcher]
//! app_id = 4100000000          # gamescope app id of the launcher's windows
//! command = ["hinteros-launcher"]  # optional: the daemon starts and restarts the launcher
//!
//! [steam]
//! path = "steam"               # binary used for steam:// URLs and Big Picture
//! dir = "~/.local/share/Steam" # library and artwork [default: found under $HOME]
//! autostart = false            # run `steam -silent` when the daemon starts
//! launch_timeout_s = 300       # give up on a game that never shows a window
//! tag_windows = true           # tag Steam's windows STEAM_GAME=769 so they can be shown
//! library_poll_s = 60          # library rescan besides inotify (0: off)
//! library_debounce_ms = 500
//!
//! [close]
//! timeout_ms = 5000            # apps.close: SIGKILL after this
//!
//! [gamescope]
//! enable = true
//! display = ":0"               # gamescope's Xwayland [default: $DISPLAY, else :0]
//! wayland_display = "gamescope-0"  # passed to launched apps [default: $WAYLAND_DISPLAY, else gamescope-0]
//! focus_loss_debounce_ms = 1500
//!
//! [inputplumber]
//! enable = true
//! bus = "unix:path=/run/dbus/system_bus_socket"  # default: the system bus
//!
//! [input]
//! repeat_delay_ms = 400
//! repeat_interval_ms = 100
//!
//! [keys]
//! uinput = true                # virtual keyboard (needs /dev/uinput, the 'uinput' group)
//! gamescope_ime = true         # gamescope's input method for characters not on a US keyboard
//! key_delay_ms = 12
//! ime_delay_ms = 80
//!
//! [osk]
//! enable = true
//! command = ["hinteros-osk"]   # started (and restarted) by the daemon if it is on PATH
//! summon = "guide+context"     # hold Guide and press Y; or "none"
//!
//! [[apps]]                     # apps.list entries; launchable with apps.launch {id}
//! id = "jellyfin"
//! name = "Jellyfin"
//! command = ["jellyfin-desktop"]
//!
//! [[apps]]
//! id = "youtube"
//! name = "YouTube"
//! kind = "web"
//! command = ["firefox", "--kiosk", "https://www.youtube.com/tv"]
//! input = "keyboard"           # controller -> key presses (default "gamepad": the pad itself)
//! back_key = "escape"          # what B sends [default: escape]
//! [apps.keymap]                # more (or other) actions -> keys
//! context = "f"
//!
//! [[apps]]
//! id = "portal-2"
//! name = "Portal 2"
//! steam_app_id = 620
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use hinteros_api::session::{AppKind, Key, NavAction, STEAM_CLIENT_APP_ID};
use serde::Deserialize;

use crate::keys::{KeyCombo, Keymap, default_keymap};

/// Default gamescope app id of the launcher. Steam app ids are far below this, and Steam's
/// non-Steam shortcuts use random ids with the top bit set, so collisions are unlikely.
pub const DEFAULT_LAUNCHER_APP_ID: u32 = 4_100_000_000;

/// gamescope's app id for Steam's own windows (Big Picture, dialogs); never used for anything else.
pub use hinteros_api::session::STEAM_CLIENT_GAMESCOPE_APP_ID as STEAM_BIG_PICTURE_APP_ID;

#[derive(Parser, Debug, Default)]
#[command(version, about)]
pub struct Cli {
    /// Socket to listen on [default: $XDG_RUNTIME_DIR/hinteros/session.sock]
    #[arg(long, env = "HINTEROS_SESSION_SOCKET")]
    pub socket: Option<PathBuf>,

    /// Config file [default: $XDG_CONFIG_HOME/hinteros/session.toml, if it exists]
    #[arg(long, env = "HINTEROS_SESSION_CONFIG")]
    pub config: Option<PathBuf>,

    /// gamescope app id of the launcher's windows
    #[arg(long)]
    pub launcher_app_id: Option<u32>,

    /// Start (and restart) the launcher from the daemon: program and arguments, split on spaces
    #[arg(long)]
    pub launcher_command: Option<String>,

    /// steam binary used for steam://rungameid/<id> launches
    #[arg(long, env = "HINTEROS_STEAM")]
    pub steam_path: Option<PathBuf>,

    /// Steam's directory, for games.list [default: ~/.local/share/Steam or ~/.steam/steam]
    #[arg(long, env = "HINTEROS_STEAM_DIR")]
    pub steam_dir: Option<PathBuf>,

    /// Control gamescope focus (needs gamescope's Xwayland)
    #[arg(long, overrides_with = "no_gamescope")]
    pub gamescope: bool,
    /// Don't control gamescope focus (dev desktops)
    #[arg(long)]
    pub no_gamescope: bool,

    /// gamescope's Xwayland display
    #[arg(long, env = "HINTEROS_GAMESCOPE_DISPLAY")]
    pub gamescope_display: Option<String>,

    /// Take controller input from InputPlumber
    #[arg(long, overrides_with = "no_inputplumber")]
    pub inputplumber: bool,
    /// Don't use InputPlumber (dev desktops)
    #[arg(long)]
    pub no_inputplumber: bool,

    /// D-Bus address to find InputPlumber on [default: the system bus]
    #[arg(long, env = "HINTEROS_INPUTPLUMBER_BUS")]
    pub inputplumber_bus: Option<String>,

    /// Don't create the virtual keyboard (dev desktops: it would type into your session)
    #[arg(long)]
    pub no_uinput: bool,

    /// Instead of a virtual keyboard, append the key events to this file (tests)
    #[arg(long, env = "HINTEROS_FAKE_KEYBOARD", hide = true)]
    pub fake_keyboard: Option<PathBuf>,

    /// Start (and restart) the on-screen keyboard: program and arguments, split on spaces
    #[arg(long)]
    pub osk_command: Option<String>,
}

fn flag(on: bool, off: bool) -> Option<bool> {
    match (on, off) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub launcher: LauncherConfig,
    pub steam: SteamConfig,
    pub gamescope: GamescopeConfig,
    pub inputplumber: InputPlumberConfig,
    pub input: InputConfig,
    pub close: CloseConfig,
    pub keys: KeysConfig,
    pub osk: OskConfig,
    pub apps: Vec<AppConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LauncherConfig {
    pub app_id: u32,
    pub command: Option<Vec<String>>,
}

impl Default for LauncherConfig {
    fn default() -> Self {
        Self {
            app_id: DEFAULT_LAUNCHER_APP_ID,
            command: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SteamConfig {
    pub path: PathBuf,
    /// Steam's directory (library, artwork). `None`: found under `$HOME`.
    pub dir: Option<PathBuf>,
    pub autostart: bool,
    pub launch_timeout_s: u64,
    /// Tag Steam's own windows with `STEAM_GAME = 769` so they can be shown.
    pub tag_windows: bool,
    /// Rescan the library this often besides inotify (0: inotify only).
    pub library_poll_s: u64,
    /// Wait this long after a library change before rescanning.
    pub library_debounce_ms: u64,
}

impl Default for SteamConfig {
    fn default() -> Self {
        Self {
            path: "steam".into(),
            dir: None,
            autostart: false,
            launch_timeout_s: 300,
            tag_windows: true,
            library_poll_s: 60,
            library_debounce_ms: 500,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GamescopeConfig {
    pub enable: bool,
    pub display: Option<String>,
    pub wayland_display: Option<String>,
    pub focus_loss_debounce_ms: u64,
}

impl Default for GamescopeConfig {
    fn default() -> Self {
        Self {
            enable: true,
            display: None,
            wayland_display: None,
            focus_loss_debounce_ms: 1500,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputPlumberConfig {
    pub enable: bool,
    pub bus: Option<String>,
}

impl Default for InputPlumberConfig {
    fn default() -> Self {
        Self {
            enable: true,
            bus: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputConfig {
    pub repeat_delay_ms: u64,
    pub repeat_interval_ms: u64,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            repeat_delay_ms: 400,
            repeat_interval_ms: 100,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CloseConfig {
    /// How long apps.close waits for an app to quit before killing it.
    pub timeout_ms: u64,
}

impl Default for CloseConfig {
    fn default() -> Self {
        Self { timeout_ms: 5000 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KeysConfig {
    /// Create the virtual keyboard (uinput).
    pub uinput: bool,
    /// Use gamescope's input method for characters the US keyboard does not have.
    pub gamescope_ime: bool,
    pub key_delay_ms: u64,
    pub ime_delay_ms: u64,
    /// Append key events to this file instead of creating a device (tests; `--fake-keyboard`).
    #[serde(skip)]
    pub fake: Option<PathBuf>,
}

impl Default for KeysConfig {
    fn default() -> Self {
        Self {
            uinput: true,
            gamescope_ime: true,
            key_delay_ms: 12,
            ime_delay_ms: 80,
            fake: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OskConfig {
    pub enable: bool,
    /// The on-screen keyboard client; started if it can be found.
    pub command: Vec<String>,
    /// Controller chord that shows/hides it: `guide+<action>` (hold Guide, press the button) or
    /// `none`.
    pub summon: String,
}

impl Default for OskConfig {
    fn default() -> Self {
        Self {
            enable: true,
            command: vec!["hinteros-osk".into()],
            summon: "guide+context".into(),
        }
    }
}

impl OskConfig {
    /// The button pressed with Guide to summon the keyboard (`None`: no chord).
    pub fn summon_button(&self) -> Result<Option<NavAction>> {
        let s = self.summon.trim().to_ascii_lowercase();
        if s == "none" || s.is_empty() {
            return Ok(None);
        }
        let button = s.strip_prefix("guide+").with_context(|| {
            format!(
                "osk.summon '{}' must be 'guide+<button>' or 'none'",
                self.summon
            )
        })?;
        let action =
            nav_action(button).with_context(|| format!("osk.summon: unknown button '{button}'"))?;
        ensure!(
            !action.is_direction(),
            "osk.summon: use a button, not a direction"
        );
        Ok(Some(action))
    }
}

/// Parse a [`NavAction`] by its API name (`accept`, `page_left`, ...).
pub fn nav_action(name: &str) -> Option<NavAction> {
    serde_json::from_value(serde_json::Value::String(name.into())).ok()
}

/// How an app takes controller input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputProfile {
    /// The pad itself (InputPlumber PASS; Steam Input applies).
    #[default]
    Gamepad,
    /// Controller actions become key presses on the virtual keyboard (TV web apps).
    Keyboard,
}

impl InputConfig {
    pub fn repeat_delay(&self) -> Duration {
        Duration::from_millis(self.repeat_delay_ms)
    }
    pub fn repeat_interval(&self) -> Duration {
        Duration::from_millis(self.repeat_interval_ms)
    }
}

/// An `apps.list` entry. Exactly one of `command` and `steam_app_id`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub id: String,
    pub name: String,
    #[serde(default = "native")]
    pub kind: AppKind,
    pub icon: Option<String>,
    pub command: Option<Vec<String>>,
    pub steam_app_id: Option<u32>,
    #[serde(default)]
    pub input: InputProfile,
    /// What `back` sends in keyboard mode [default: escape].
    pub back_key: Option<String>,
    /// Controller action -> key (`"ctrl+l"`, `"none"` to unmap), on top of the defaults.
    #[serde(default)]
    pub keymap: BTreeMap<String, String>,
}

fn native() -> AppKind {
    AppKind::Native
}

impl AppConfig {
    /// The keymap for the keyboard profile (`None` for gamepad apps).
    pub fn keymap(&self) -> Result<Option<Keymap>> {
        if self.input != InputProfile::Keyboard {
            return Ok(None);
        }
        let back = match &self.back_key {
            Some(k) => k.parse::<KeyCombo>().map_err(anyhow::Error::msg)?,
            None => Key::Escape.into(),
        };
        let mut map = default_keymap(back);
        for (action, key) in &self.keymap {
            let action = nav_action(action)
                .with_context(|| format!("unknown controller action '{action}'"))?;
            if key.trim().eq_ignore_ascii_case("none") {
                map.remove(&action);
            } else {
                map.insert(action, key.parse::<KeyCombo>().map_err(anyhow::Error::msg)?);
            }
        }
        Ok(Some(map))
    }
}

/// Whether `id` is a valid app id (`^[a-z0-9][a-z0-9._-]*$`).
pub fn valid_app_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
}

impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        let config: Config = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    /// Load the file named on the command line, or the default one if it exists, then apply
    /// the command-line overrides.
    pub fn load(cli: &Cli) -> Result<Self> {
        let path = cli.config.clone().or_else(|| {
            let base = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))?;
            Some(base.join("hinteros/session.toml")).filter(|p| p.exists())
        });
        let mut config = match &path {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?;
                Self::parse(&text).with_context(|| format!("in {}", path.display()))?
            }
            None => Self::default(),
        };
        config.apply(cli);
        config.validate()?;
        Ok(config)
    }

    pub fn apply(&mut self, cli: &Cli) {
        if let Some(id) = cli.launcher_app_id {
            self.launcher.app_id = id;
        }
        if let Some(cmd) = &cli.launcher_command {
            self.launcher.command = Some(cmd.split_whitespace().map(String::from).collect());
        }
        if let Some(path) = &cli.steam_path {
            self.steam.path = path.clone();
        }
        if let Some(dir) = &cli.steam_dir {
            self.steam.dir = Some(dir.clone());
        }
        if let Some(on) = flag(cli.gamescope, cli.no_gamescope) {
            self.gamescope.enable = on;
        }
        if let Some(display) = &cli.gamescope_display {
            self.gamescope.display = Some(display.clone());
        }
        if let Some(on) = flag(cli.inputplumber, cli.no_inputplumber) {
            self.inputplumber.enable = on;
        }
        if let Some(bus) = &cli.inputplumber_bus {
            self.inputplumber.bus = Some(bus.clone());
        }
        if cli.no_uinput {
            self.keys.uinput = false;
        }
        if let Some(path) = &cli.fake_keyboard {
            self.keys.fake = Some(path.clone());
        }
        if let Some(cmd) = &cli.osk_command {
            self.osk.command = cmd.split_whitespace().map(String::from).collect();
        }
    }

    pub fn validate(&self) -> Result<()> {
        let id = self.launcher.app_id;
        ensure!(
            id != 0 && id != STEAM_BIG_PICTURE_APP_ID,
            "launcher.app_id must not be 0 or {STEAM_BIG_PICTURE_APP_ID}"
        );
        if let Some(cmd) = &self.launcher.command {
            ensure!(!cmd.is_empty(), "launcher.command is empty");
        }
        self.osk.summon_button()?;
        let mut seen = BTreeSet::new();
        for app in &self.apps {
            ensure!(valid_app_id(&app.id), "invalid app id '{}'", app.id);
            ensure!(
                app.id != STEAM_CLIENT_APP_ID,
                "app id '{STEAM_CLIENT_APP_ID}' is reserved for the built-in Steam entry"
            );
            ensure!(seen.insert(&app.id), "duplicate app id '{}'", app.id);
            ensure!(!app.name.is_empty(), "app '{}' has an empty name", app.id);
            match (&app.command, app.steam_app_id) {
                (Some(cmd), None) if !cmd.is_empty() => {}
                (None, Some(steam)) if steam != 0 => {}
                _ => bail!(
                    "app '{}' needs exactly one of a non-empty 'command' or a 'steam_app_id'",
                    app.id
                ),
            }
            ensure!(
                app.steam_app_id.is_none() || app.input == InputProfile::Gamepad,
                "app '{}': Steam games always use the gamepad input profile",
                app.id
            );
            if app.input == InputProfile::Gamepad {
                ensure!(
                    app.back_key.is_none() && app.keymap.is_empty(),
                    "app '{}': back_key and keymap need input = \"keyboard\"",
                    app.id
                );
            }
            app.keymap().with_context(|| format!("app '{}'", app.id))?;
        }
        Ok(())
    }

    /// The Xwayland display gamescope serves X11 clients on.
    pub fn gamescope_display(&self) -> String {
        self.gamescope
            .display
            .clone()
            .or_else(|| std::env::var("DISPLAY").ok().filter(|d| !d.is_empty()))
            .unwrap_or_else(|| ":0".into())
    }

    /// gamescope's Wayland display (a socket name in `$XDG_RUNTIME_DIR`, or a path).
    pub fn wayland_display(&self) -> String {
        self.gamescope
            .wayland_display
            .clone()
            .or_else(|| {
                std::env::var("WAYLAND_DISPLAY")
                    .ok()
                    .filter(|d| !d.is_empty())
            })
            .unwrap_or_else(|| "gamescope-0".into())
    }

    /// Environment for apps the daemon starts, so they land in gamescope. Empty when gamescope
    /// integration is off (apps inherit the daemon's environment).
    pub fn child_env(&self) -> Vec<(String, String)> {
        if !self.gamescope.enable {
            return Vec::new();
        }
        let wayland = self.wayland_display();
        vec![
            ("DISPLAY".into(), self.gamescope_display()),
            ("WAYLAND_DISPLAY".into(), wayland.clone()),
            ("GAMESCOPE_WAYLAND_DISPLAY".into(), wayland),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_without_a_file() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.launcher.app_id, DEFAULT_LAUNCHER_APP_ID);
        assert_eq!(c.steam.path, PathBuf::from("steam"));
        assert!(c.gamescope.enable && c.inputplumber.enable);
        assert!(c.apps.is_empty());
    }

    #[test]
    fn full_file() {
        let c = Config::parse(
            r#"
            [launcher]
            app_id = 42
            command = ["hinteros-launcher", "--kiosk"]
            [steam]
            path = "/run/current-system/sw/bin/steam"
            dir = "/srv/steam"
            autostart = true
            tag_windows = false
            [close]
            timeout_ms = 100
            [gamescope]
            enable = false
            display = ":1"
            [inputplumber]
            enable = false
            [input]
            repeat_delay_ms = 300
            [[apps]]
            id = "jellyfin"
            name = "Jellyfin"
            command = ["jellyfin-desktop"]
            [[apps]]
            id = "portal-2"
            name = "Portal 2"
            kind = "native"
            steam_app_id = 620
            "#,
        )
        .unwrap();
        assert_eq!(c.launcher.app_id, 42);
        assert_eq!(c.launcher.command.as_ref().unwrap().len(), 2);
        assert!(c.steam.autostart && !c.steam.tag_windows);
        assert_eq!(c.steam.dir, Some(PathBuf::from("/srv/steam")));
        assert_eq!(c.close.timeout_ms, 100);
        assert_eq!(c.gamescope_display(), ":1");
        assert!(!c.gamescope.enable && !c.inputplumber.enable);
        assert_eq!(c.input.repeat_delay(), Duration::from_millis(300));
        assert_eq!(c.input.repeat_interval(), Duration::from_millis(100));
        assert_eq!(c.apps[1].steam_app_id, Some(620));
        assert!(c.child_env().is_empty(), "no gamescope, inherit env");
    }

    #[test]
    fn rejects_bad_files() {
        for bad in [
            "[launcher]\napp_id = 0",
            "[launcher]\napp_id = 769",
            "[launcher]\ncommand = []",
            "[gamescope]\nenabled = true", // typo
            "[[apps]]\nid = \"Bad\"\nname = \"x\"\ncommand = [\"x\"]",
            "[[apps]]\nid = \"a\"\nname = \"x\"",
            "[[apps]]\nid = \"steam\"\nname = \"x\"\ncommand = [\"x\"]",
            "[close]\ntimeout = 1",
            "[[apps]]\nid = \"a\"\nname = \"x\"\ncommand = [\"x\"]\nsteam_app_id = 1",
            "[[apps]]\nid = \"a\"\nname = \"x\"\ncommand = [\"x\"]\n[[apps]]\nid = \"a\"\nname = \"y\"\ncommand = [\"y\"]",
        ] {
            assert!(Config::parse(bad).is_err(), "accepted: {bad}");
        }
    }

    #[test]
    fn cli_overrides_file() {
        let mut c = Config::parse("[gamescope]\nenable = true\n").unwrap();
        let cli = Cli::parse_from([
            "hinteros-session",
            "--no-gamescope",
            "--no-inputplumber",
            "--launcher-app-id",
            "7",
            "--launcher-command",
            "launcher --flag",
            "--steam-path",
            "/tmp/fake-steam",
            "--steam-dir",
            "/tmp/fake-steam-dir",
            "--gamescope-display",
            ":5",
        ]);
        c.apply(&cli);
        c.validate().unwrap();
        assert!(!c.gamescope.enable && !c.inputplumber.enable);
        assert_eq!(c.launcher.app_id, 7);
        assert_eq!(
            c.launcher.command,
            Some(vec!["launcher".into(), "--flag".into()])
        );
        assert_eq!(c.steam.path, PathBuf::from("/tmp/fake-steam"));
        assert_eq!(c.steam.dir, Some(PathBuf::from("/tmp/fake-steam-dir")));
        assert_eq!(c.gamescope_display(), ":5");

        let cli = Cli::parse_from(["hinteros-session", "--no-gamescope", "--gamescope"]);
        c.apply(&cli);
        assert!(
            c.gamescope.enable,
            "the last of --gamescope/--no-gamescope wins"
        );
    }

    #[test]
    fn child_env_points_at_gamescope() {
        let mut c = Config::default();
        c.gamescope.display = Some(":3".into());
        c.gamescope.wayland_display = Some("gamescope-1".into());
        let env = c.child_env();
        assert!(env.contains(&("DISPLAY".into(), ":3".into())));
        assert!(env.contains(&("WAYLAND_DISPLAY".into(), "gamescope-1".into())));
    }

    #[test]
    fn app_ids() {
        assert!(valid_app_id("steam-570") && valid_app_id("cmd-1") && valid_app_id("a.b_c"));
        assert!(!valid_app_id("") && !valid_app_id("-a") && !valid_app_id("A"));
    }
    #[test]
    fn keyboard_profiles_and_keymaps() {
        let c = Config::parse(
            r#"
            [[apps]]
            id = "youtube"
            name = "YouTube"
            kind = "web"
            command = ["firefox", "--kiosk", "https://www.youtube.com/tv"]
            input = "keyboard"
            back_key = "backspace"
            [apps.keymap]
            context = "ctrl+l"
            accept = "space"
            right = "none"
            [[apps]]
            id = "jellyfin"
            name = "Jellyfin"
            command = ["jellyfin-desktop"]
            "#,
        )
        .unwrap();
        let map = c.apps[0].keymap().unwrap().unwrap();
        assert_eq!(map[&NavAction::Back], Key::Backspace.into());
        assert_eq!(map[&NavAction::Accept], Key::Space.into());
        assert_eq!(map[&NavAction::Context], "ctrl+l".parse().unwrap());
        assert_eq!(map[&NavAction::Up], Key::Up.into());
        assert!(!map.contains_key(&NavAction::Right));
        assert_eq!(c.apps[1].input, InputProfile::Gamepad);
        assert_eq!(c.apps[1].keymap().unwrap(), None);

        // Defaults: Escape for back.
        let c = Config::parse(
            "[[apps]]\nid = \"a\"\nname = \"A\"\ncommand = [\"a\"]\ninput = \"keyboard\"\n",
        )
        .unwrap();
        let map = c.apps[0].keymap().unwrap().unwrap();
        assert_eq!(map[&NavAction::Back], Key::Escape.into());
        assert_eq!(map.len(), 6);
    }

    #[test]
    fn rejects_bad_keyboard_config() {
        let app = "[[apps]]\nid = \"a\"\nname = \"A\"\n";
        for bad in [
            "command = [\"a\"]\ninput = \"mouse\"",
            "command = [\"a\"]\ninput = \"keyboard\"\nback_key = \"hyper+x\"",
            "command = [\"a\"]\ninput = \"keyboard\"\n[apps.keymap]\njump = \"space\"",
            "command = [\"a\"]\ninput = \"keyboard\"\n[apps.keymap]\naccept = \"nope\"",
            "command = [\"a\"]\nback_key = \"escape\"",
            "steam_app_id = 620\ninput = \"keyboard\"",
        ] {
            let text = format!("{app}{bad}\n");
            assert!(Config::parse(&text).is_err(), "accepted: {text}");
        }
    }

    #[test]
    fn osk_and_keys_sections() {
        let c = Config::default();
        assert!(c.osk.enable && c.keys.uinput && c.keys.gamescope_ime);
        assert_eq!(c.osk.command, ["hinteros-osk"]);
        assert_eq!(c.osk.summon_button().unwrap(), Some(NavAction::Context));
        let c = Config::parse(
            "[osk]\nenable = false\ncommand = [\"my-osk\", \"--x\"]\nsummon = \"none\"\n[keys]\nuinput = false\nkey_delay_ms = 5\n",
        )
        .unwrap();
        assert!(!c.osk.enable && !c.keys.uinput);
        assert_eq!(c.keys.key_delay_ms, 5);
        assert_eq!(c.osk.summon_button().unwrap(), None);
        let c = Config::parse("[osk]\nsummon = \"Guide+Secondary\"\n").unwrap();
        assert_eq!(c.osk.summon_button().unwrap(), Some(NavAction::Secondary));
        for bad in [
            "[osk]\nsummon = \"context\"",
            "[osk]\nsummon = \"guide+jump\"",
            "[osk]\nsummon = \"guide+up\"",
            "[keys]\nfake = \"/tmp/x\"",
        ] {
            assert!(Config::parse(bad).is_err(), "accepted: {bad}");
        }
        let mut c = Config::default();
        let cli = Cli::parse_from([
            "hinteros-session",
            "--no-uinput",
            "--fake-keyboard",
            "/tmp/keys.log",
            "--osk-command",
            "osk --flag",
        ]);
        c.apply(&cli);
        assert!(!c.keys.uinput);
        assert_eq!(c.keys.fake, Some(PathBuf::from("/tmp/keys.log")));
        assert_eq!(c.osk.command, ["osk", "--flag"]);
    }
}
