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
//! dev_gamepad = false          # development only: read pads via evdev, no InputPlumber
//!
//! [icons]
//! fetch = true                 # download web app icons (https only)
//! refresh_days = 30
//! # cache_dir = "~/.cache/hinteros/icons"  theme = "Papirus"
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
//! auto_show = true             # show it when a text field gets focus (Firefox extension)
//!
//! [[apps]]                     # apps.list entries; launchable with apps.launch {id}
//! id = "jellyfin"
//! name = "Jellyfin"
//! command = ["jellyfin-desktop"]
//! desktop_id = "org.jellyfin.JellyfinDesktop"  # icon from its .desktop file and icon theme
//!
//! [[apps]]
//! id = "youtube"
//! name = "YouTube"
//! kind = "web"
//! command = ["firefox", "--kiosk", "https://www.youtube.com/tv"]
//! site_url = "https://www.youtube.com/tv"  # icon from the site's manifest/favicon (or icon_url)
//! # firefox_profile = "~/.local/share/hinteros/firefox/youtube"  # fallback: the favicon Firefox stored
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

#[derive(Parser, Debug, Default, Clone)]
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

    /// Development only: read gamepads directly (evdev, no grab) instead of through InputPlumber
    #[arg(long)]
    pub dev_gamepad: bool,

    /// Don't download web app icons
    #[arg(long)]
    pub no_icon_fetch: bool,

    /// Allow http:// icon URLs (tests against a local server)
    #[arg(long, env = "HINTEROS_ICON_ALLOW_HTTP", hide = true)]
    pub icon_allow_http: bool,

    /// Search only these data directories (colon-separated) for desktop files and icons (tests)
    #[arg(long, env = "HINTEROS_ICON_DIRS", hide = true, value_delimiter = ':')]
    pub icon_dirs: Option<Vec<PathBuf>>,

    /// The system service's socket, for the relayed system.* methods
    #[arg(long, env = "HINTEROS_SYSTEM_SOCKET")]
    pub system_socket: Option<PathBuf>,

    /// Don't serve runtime settings (Wi-Fi, Bluetooth, audio, display, power)
    #[arg(long)]
    pub no_runtime: bool,

    /// D-Bus address of the bus NetworkManager, BlueZ and logind are on [default: the system bus]
    #[arg(long, env = "HINTEROS_RUNTIME_BUS")]
    pub runtime_bus: Option<String>,

    /// The pactl binary used for audio
    #[arg(long, env = "HINTEROS_PACTL")]
    pub pactl: Option<PathBuf>,

    /// Don't register a polkit authentication agent (desktop previews)
    #[arg(long)]
    pub no_auth_agent: bool,

    /// D-Bus address of polkit's bus for the agent [default: the system bus]
    #[arg(long, env = "HINTEROS_AUTH_BUS")]
    pub auth_bus: Option<String>,

    /// polkit's agent helper binary
    #[arg(long, env = "HINTEROS_POLKIT_AGENT_HELPER")]
    pub polkit_agent_helper: Option<PathBuf>,

    /// polkit's agent helper socket
    #[arg(long, env = "HINTEROS_POLKIT_AGENT_HELPER_SOCKET")]
    pub polkit_agent_helper_socket: Option<PathBuf>,

    /// D-Bus address of the user's systemd, for session.restart [default: the session bus]
    #[arg(long, env = "HINTEROS_SESSION_USER_BUS")]
    pub session_user_bus: Option<String>,

    /// The App Store catalog index
    #[arg(long, env = "HINTEROS_CATALOG_INDEX")]
    pub catalog_index: Option<PathBuf>,
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
    pub icons: IconsConfig,
    pub close: CloseConfig,
    pub keys: KeysConfig,
    pub osk: OskConfig,
    pub system: SystemConfig,
    pub runtime: RuntimeConfig,
    pub auth: AuthConfig,
    pub session: SessionConfig,
    pub store: StoreConfig,
    pub audio: AudioConfig,
    pub display: DisplayConfig,
    pub bluetooth: BluetoothConfig,
    pub apps: Vec<AppConfig>,
}

/// Runtime settings: Wi-Fi (NetworkManager), Bluetooth (BlueZ), power (logind), all over D-Bus.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeConfig {
    /// Serve the runtime settings methods (`network.*`, `wifi.*`, `bluetooth.*`, `audio.*`,
    /// `display.*`, `power.*`). Off: they answer `unavailable`.
    pub enable: bool,
    /// D-Bus address of the bus those services are on [default: the system bus].
    pub bus: Option<String>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            enable: true,
            bus: None,
        }
    }
}

/// Audio, through PipeWire's PulseAudio interface with `pactl`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    /// The `pactl` binary.
    pub pactl: PathBuf,
    /// How often changes made elsewhere are looked for (only while a client is connected).
    pub poll_ms: u64,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            pactl: "pactl".into(),
            poll_ms: 2000,
        }
    }
}

/// The display's modes, as far as gamescope shares them (see `runtime::display`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DisplayConfig {
    /// gamescope's mode save file (its `GAMESCOPE_MODE_SAVE_FILE`) [default: that variable].
    pub save_file: Option<PathBuf>,
    /// The mode list gamescope writes: `<GAMESCOPE_PATCHED_EDID_FILE>.modes` [default: from
    /// that variable].
    pub modes_file: Option<PathBuf>,
    /// How long `display.set_mode` waits for gamescope to switch before it gives up and puts
    /// the old mode back.
    pub apply_timeout_ms: u64,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            save_file: None,
            modes_file: None,
            apply_timeout_ms: 6000,
        }
    }
}

/// Bluetooth pairing.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BluetoothConfig {
    /// `bluetooth.pair` gives up after this long.
    pub pair_timeout_s: u64,
    /// A numeric comparison is rejected if `bluetooth.confirm` does not answer in this time.
    pub confirm_timeout_s: u64,
}

impl Default for BluetoothConfig {
    fn default() -> Self {
        Self {
            pair_timeout_s: 45,
            confirm_timeout_s: 30,
        }
    }
}

/// The relay to the privileged system service (`system.*` methods and job events).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SystemConfig {
    pub enable: bool,
    pub socket: PathBuf,
}

impl Default for SystemConfig {
    fn default() -> Self {
        Self {
            enable: true,
            socket: PathBuf::from(hinteros_api::system::DEFAULT_SOCKET),
        }
    }
}

/// The polkit authentication agent (`auth.*`, `authentication_requested`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// Register the agent with polkit for this process.
    pub enable: bool,
    /// D-Bus address of polkit's bus [default: the system bus].
    pub bus: Option<String>,
    /// polkit's setuid helper (used when the helper socket does not exist).
    pub helper: PathBuf,
    /// polkit's socket-activated helper (`polkit-agent-helper.socket`), preferred when it exists:
    /// it works without setuid, so under `NoNewPrivileges`.
    pub helper_socket: PathBuf,
    /// A request nobody answers is cancelled after this long.
    pub prompt_timeout_s: u64,
    /// Wrong passwords per request before it fails.
    pub max_attempts: u32,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enable: true,
            bus: None,
            helper: "/run/wrappers/bin/polkit-agent-helper-1".into(),
            helper_socket: "/run/polkit/agent-helper.socket".into(),
            prompt_timeout_s: 120,
            max_attempts: 3,
        }
    }
}

/// `session.restart`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    /// PAM services of sessions `session.restart` may end (greetd's session and its
    /// `default_session`, which greetd runs as the greeter); never a desktop session.
    pub restart_services: Vec<String>,
    /// This daemon's systemd user unit, restarted after the session [default: from
    /// /proc/self/cgroup; none outside a user unit].
    pub unit: Option<String>,
    /// D-Bus address of the user's systemd [default: the session bus].
    pub user_bus: Option<String>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            restart_services: vec!["greetd".into(), "greetd-greeter".into()],
            unit: None,
            user_bus: None,
        }
    }
}

/// The App Store (`store.list`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StoreConfig {
    /// The running generation's catalog index.
    pub index: PathBuf,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            index: hinteros_api::catalog::DEFAULT_INDEX.into(),
        }
    }
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
    /// Development only: read gamepads directly through evdev (see `input::devpad`). Ignored
    /// while InputPlumber is enabled.
    pub dev_gamepad: bool,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            repeat_delay_ms: 400,
            repeat_interval_ms: 100,
            dev_gamepad: false,
        }
    }
}

/// Where app icons come from (see `icons`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IconsConfig {
    /// Download web app icons (`site_url`, `icon_url`).
    pub fetch: bool,
    /// Refetch a cached web icon after this many days.
    pub refresh_days: u64,
    /// Where downloaded icons are cached [default: $XDG_CACHE_HOME/hinteros/icons].
    pub cache_dir: Option<PathBuf>,
    /// Icon theme to search first [default: GTK's setting, then hicolor].
    pub theme: Option<String>,
    /// Replace the data directories searched for `.desktop` files and icons (default: `$XDG_DATA_HOME`,
    /// `$XDG_DATA_DIRS` and the system and user profiles).
    pub data_dirs: Option<Vec<PathBuf>>,
    /// Accept http:// icon URLs (tests only; `--icon-allow-http`).
    #[serde(skip)]
    pub allow_http: bool,
}

impl Default for IconsConfig {
    fn default() -> Self {
        Self {
            fetch: true,
            refresh_days: 30,
            cache_dir: None,
            theme: None,
            data_dirs: None,
            allow_http: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CloseConfig {
    /// How long apps.close waits for an app to quit before killing it.
    pub timeout_ms: u64,
    /// How long the daemon waits at shutdown (SIGTERM) for the apps it started, the launcher and
    /// the on-screen keyboard to quit before killing them.
    pub shutdown_grace_ms: u64,
}

impl Default for CloseConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 5000,
            shutdown_grace_ms: 3000,
        }
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
    /// Show it when a text field gets focus in a keyboard-profile app (Firefox, through the
    /// HinterOS extension and its native-messaging host), and hide it when the field loses
    /// focus.
    pub auto_show: bool,
}

impl Default for OskConfig {
    fn default() -> Self {
        Self {
            enable: true,
            command: vec!["hinteros-osk".into()],
            summon: "guide+context".into(),
            auto_show: true,
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
    /// An absolute path to an image, or an icon theme name.
    pub icon: Option<String>,
    /// The app's freedesktop `.desktop` id (`org.jellyfin.JellyfinDesktop`): its `Icon=` is looked up
    /// in the installed icon themes.
    pub desktop_id: Option<String>,
    /// Web apps: an https URL of the icon image.
    pub icon_url: Option<String>,
    /// Web apps: an https page whose manifest, apple-touch-icon or favicon is the icon.
    pub site_url: Option<String>,
    /// Web apps: the app's Firefox profile directory (`~/` allowed). When the site refuses the icon
    /// download, the favicon Firefox stored there for `site_url` is used. Needs `site_url`.
    pub firefox_profile: Option<PathBuf>,
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

/// `~/x` as `$HOME/x`; other paths unchanged.
pub fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) if !home.is_empty() => Path::new(&home).join(rest),
        _ => path.to_path_buf(),
    }
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

    /// The config file: the one named on the command line, else the default location (which
    /// may not exist yet).
    pub fn config_path(cli: &Cli) -> Option<PathBuf> {
        cli.config.clone().or_else(|| {
            let base = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))?;
            Some(base.join("hinteros/session.toml"))
        })
    }

    /// Load the file named on the command line, or the default one if it exists, then apply
    /// the command-line overrides.
    pub fn load(cli: &Cli) -> Result<Self> {
        let path = Self::config_path(cli).filter(|p| cli.config.is_some() || p.exists());
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
        if cli.dev_gamepad {
            self.input.dev_gamepad = true;
        }
        if let Some(dirs) = &cli.icon_dirs {
            self.icons.data_dirs = Some(dirs.clone());
        }
        if cli.icon_allow_http {
            self.icons.allow_http = true;
        }
        if cli.no_icon_fetch {
            self.icons.fetch = false;
        }
        if let Some(cmd) = &cli.osk_command {
            self.osk.command = cmd.split_whitespace().map(String::from).collect();
        }
        if let Some(socket) = &cli.system_socket {
            self.system.socket = socket.clone();
        }
        if cli.no_runtime {
            self.runtime.enable = false;
        }
        if let Some(bus) = &cli.runtime_bus {
            self.runtime.bus = Some(bus.clone());
        }
        if let Some(pactl) = &cli.pactl {
            self.audio.pactl = pactl.clone();
        }
        if cli.no_auth_agent {
            self.auth.enable = false;
        }
        if let Some(bus) = &cli.auth_bus {
            self.auth.bus = Some(bus.clone());
        }
        if let Some(p) = &cli.polkit_agent_helper {
            self.auth.helper = p.clone();
        }
        if let Some(p) = &cli.polkit_agent_helper_socket {
            self.auth.helper_socket = p.clone();
        }
        if let Some(bus) = &cli.session_user_bus {
            self.session.user_bus = Some(bus.clone());
        }
        if let Some(p) = &cli.catalog_index {
            self.store.index = p.clone();
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
            for (what, url) in [("icon_url", &app.icon_url), ("site_url", &app.site_url)] {
                if let Some(url) = url {
                    ensure!(
                        url.starts_with("https://"),
                        "app '{}': {what} must be an https:// URL",
                        app.id
                    );
                }
            }
            ensure!(
                app.firefox_profile.is_none() || app.site_url.is_some(),
                "app '{}': firefox_profile needs site_url",
                app.id
            );
            if let Some(id) = &app.desktop_id {
                ensure!(
                    !id.is_empty() && !id.contains('/'),
                    "app '{}': invalid desktop_id",
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

    #[test]
    fn icon_and_dev_gamepad_settings() {
        let c = Config::parse(
            "[icons]\nrefresh_days = 7\n[input]\ndev_gamepad = true\n\
             [[apps]]\nid = \"yt\"\nname = \"YT\"\ncommand = [\"x\"]\nsite_url = \"https://www.youtube.com/tv\"\n\
             [[apps]]\nid = \"jf\"\nname = \"JF\"\ncommand = [\"x\"]\ndesktop_id = \"org.jellyfin.JellyfinDesktop\"",
        )
        .unwrap();
        assert_eq!(c.icons.refresh_days, 7);
        assert!(c.icons.fetch && c.input.dev_gamepad);
        assert_eq!(
            c.apps[0].site_url.as_deref(),
            Some("https://www.youtube.com/tv")
        );
        assert!(!Config::default().input.dev_gamepad);
        for bad in [
            "[[apps]]\nid = \"a\"\nname = \"A\"\ncommand = [\"x\"]\nicon_url = \"http://x.test/i.png\"",
            "[[apps]]\nid = \"a\"\nname = \"A\"\ncommand = [\"x\"]\ndesktop_id = \"a/b\"",
            "[[apps]]\nid = \"a\"\nname = \"A\"\ncommand = [\"x\"]\nfirefox_profile = \"/p\"",
        ] {
            assert!(Config::parse(bad).is_err(), "accepted: {bad}");
        }
        let mut c = Config::default();
        c.apply(&Cli::parse_from([
            "hinteros-session",
            "--dev-gamepad",
            "--no-icon-fetch",
        ]));
        assert!(c.input.dev_gamepad && !c.icons.fetch);
    }

    #[test]
    fn firefox_profile_needs_a_site_and_expands_home() {
        let c = Config::parse(
            "[[apps]]\nid = \"cr\"\nname = \"CR\"\ncommand = [\"x\"]\nsite_url = \"https://www.crunchyroll.com/\"\n\
             firefox_profile = \"~/.local/share/hinteros/firefox/cr\"",
        )
        .unwrap();
        let profile = c.apps[0].firefox_profile.as_deref().unwrap();
        assert_eq!(profile, Path::new("~/.local/share/hinteros/firefox/cr"));
        if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
            assert_eq!(
                expand_home(profile),
                Path::new(&home).join(".local/share/hinteros/firefox/cr")
            );
        }
        assert_eq!(expand_home(Path::new("/abs/p")), Path::new("/abs/p"));
    }
}
