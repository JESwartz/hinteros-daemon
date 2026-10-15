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
//! apps_display = "auto+2"      # the Xwayland for apps under the reaper [default: display]
//! wayland_display = "gamescope-0"  # passed to Steam (not to apps) [default: $WAYLAND_DISPLAY, else gamescope-0]
//! focus_loss_debounce_ms = 1500
//! unmap_gl_dummies = true      # unmap invisible full-screen override-redirect windows over an app
//!
//! [inputplumber]
//! enable = true
//! bus = "unix:path=/run/dbus/system_bus_socket"  # default: the system bus
//! call_timeout_ms = 2000       # an InputPlumber call taking longer has failed (input never waits)
//! watchdog_ms = 5000           # check every pad's mode and profile this often; re-apply if wrong
//!
//! [input]
//! repeat_delay_ms = 400
//! repeat_interval_ms = 100
//! xbox_label_swap = "auto"     # "auto" | "on" | "off": swap ui_action/ui_context on Xbox (xpad) pads
//! dev_gamepad = false          # development only: read pads via evdev, no InputPlumber
//! direction_debounce_ms = 40   # a direction re-pressed this soon after its release is stick chatter
//! keyboard_home = true         # the system's keyboard_home (Ctrl+Super+D) is Home
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
//! field_summon = "accept"      # A on a focused text field shows it (no Enter); or "none"
//! auto_show = false            # show it as soon as a text field gets focus (Firefox extension)
//!
//! [pointer]                    # right stick = mouse pointer in keyboard-profile apps
//! enable = true
//! speed_pps = 1000             # pixels per second at full deflection
//! activity_deadzone = 0.25     # stick movement that counts as using the pointer
//! # idle_ms                    # ignored (older configs): A never clicks, the pointer never idles out
//! hide_command = ["gamescopectl", "paint_cursor_plane", "0"]  # on D-pad/left stick use
//! show_command = ["gamescopectl", "paint_cursor_plane", "1"]
//!
//! [sandbox]                    # catalog apps in bwrap (see `sandbox`); never Steam
//! enable = false
//! bwrap = "bwrap"              # a path, or a name looked up in PATH
//! pulse_socket = "pulse/native"         # sound for apps, relative to XDG_RUNTIME_DIR
//! pulse_record_socket = "pulse/native"  # sound that may record (mic); the same: no mic permission
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
//! pointer = true               # right stick moves a pointer, RT/LT click left/right [default: true]
//! permissions = { mic = "optional" }  # sandbox permissions from the catalog (required/optional)
//! # sandbox = false            # run without the sandbox [default: true]
//! # address_bar = true         # plain Firefox: View = Ctrl+L + on-screen keyboard, LB/RB = back/forward
//! [apps.keymap]                # more (or other) actions -> keys
//! context = "f"
//!
//! [[apps]]
//! id = "portal-2"
//! name = "Portal 2"
//! steam_app_id = 620
//! ```
//!
//! Per-profile apps (M7, state.json schema 2): instead of `[[apps]]`, the `apps` key is a
//! table naming the profile's app env; its manifest (`share/hinteros/apps.json`) holds the
//! entries (the same keys as `[[apps]]`) and Steam's path:
//!
//! ```toml
//! [apps]
//! profile = "/run/hinteros/apps/%u"   # %u: this daemon's user
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

    /// The box-wide audio output file [default: /run/hinteros/audio-output]
    #[arg(long, env = "HINTEROS_AUDIO_OUTPUT_FILE")]
    pub audio_output_file: Option<PathBuf>,

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

    /// Read the apps from this app env (link) instead of the config's `[apps] profile`
    #[arg(long, env = "HINTEROS_APPS_PROFILE", hide = true)]
    pub apps_profile: Option<PathBuf>,

    /// Tests only: a profile switch does not end the logind session
    #[arg(long, hide = true)]
    pub test_no_session_end: bool,

    /// Tests only: look for Steam's processes here instead of /proc
    #[arg(long, hide = true)]
    pub test_proc_dir: Option<PathBuf>,

    /// Greeter mode: serve the picker (greetd IPC through $GREETD_SOCK, greeter.sock); no
    /// apps, Steam, system relay or Bluetooth
    #[arg(long)]
    pub greeter: bool,
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
    pub pointer: PointerConfig,
    pub system: SystemConfig,
    pub runtime: RuntimeConfig,
    pub auth: AuthConfig,
    pub session: SessionConfig,
    pub store: StoreConfig,
    pub sandbox: SandboxConfig,
    pub audio: AudioConfig,
    pub display: DisplayConfig,
    pub bluetooth: BluetoothConfig,
    pub switch: SwitchSection,
    pub greeter: GreeterSection,
    /// `[[apps]]` (a list) or `[apps] profile = "…"` (a table).
    #[serde(rename = "apps", deserialize_with = "apps_key")]
    pub apps_key: AppsKey,
    /// The app list: `[[apps]]`, or the profile manifest's entries (filled by [`Config::load`]).
    #[serde(skip)]
    pub apps: Vec<AppConfig>,
}

/// `[switch]`: how long a profile switch waits for apps (and Steam) before asking.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SwitchSection {
    pub wait_s: u64,
    pub steam_wait_s: u64,
}

impl Default for SwitchSection {
    fn default() -> Self {
        Self {
            wait_s: 10,
            steam_wait_s: 15,
        }
    }
}

/// `[greeter]`: the picker (greeter mode and `hinteros-session greet`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GreeterSection {
    /// The service's public view of the profiles.
    pub public: PathBuf,
    /// The service's greeter socket (profile creation).
    pub socket: PathBuf,
    /// What greetd runs for a profile.
    pub session_command: Vec<String>,
}

impl Default for GreeterSection {
    fn default() -> Self {
        Self {
            public: "/run/hinteros/profiles-public.json".into(),
            socket: "/run/hinteros/greeter.sock".into(),
            session_command: vec!["/run/current-system/sw/bin/hinteros-console-session".into()],
        }
    }
}

/// The `apps` key of session.toml.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AppsKey {
    /// `[[apps]]` entries (schema-1 boxes and development).
    List(Vec<AppConfig>),
    /// `[apps] profile = "…"`: the app env (link) whose manifest has the apps; `%u` is the
    /// daemon's user name.
    Profile(String),
    /// No `apps` key.
    #[default]
    None,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppsTable {
    profile: String,
}

fn apps_key<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<AppsKey, D::Error> {
    use serde::de::Error;
    let v = toml::Value::deserialize(d)?;
    match v {
        toml::Value::Array(_) => Vec::<AppConfig>::deserialize(v)
            .map(AppsKey::List)
            .map_err(D::Error::custom),
        toml::Value::Table(_) => AppsTable::deserialize(v)
            .map(|t| AppsKey::Profile(t.profile))
            .map_err(D::Error::custom),
        _ => Err(D::Error::custom(
            "apps must be [[apps]] entries or an [apps] table with profile",
        )),
    }
}

/// This process's user name (`$USER`, else the user database).
pub fn own_user() -> Option<String> {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .or_else(|| {
            nix::unistd::User::from_uid(nix::unistd::getuid())
                .ok()
                .flatten()
                .map(|u| u.name)
        })
}

/// The profile app env with `%u` replaced.
pub fn expand_profile(template: &str) -> Option<PathBuf> {
    let user = own_user()?;
    Some(PathBuf::from(template.replace("%u", &user)))
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
    /// The box-wide audio output the system service publishes (`audio.save_output` of any
    /// profile): made the default sink when the session starts, once it exists.
    pub shared_output: Option<PathBuf>,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            pactl: "pactl".into(),
            poll_ms: 2000,
            shared_output: Some("/run/hinteros/audio-output".into()),
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

/// `[sandbox]`: run catalog apps in a bwrap sandbox ([`crate::sandbox`]). Steam and apps with
/// `sandbox = false` never are.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    pub enable: bool,
    /// bwrap: a path, or a name looked up in `$PATH`.
    pub bwrap: PathBuf,
    /// The sound socket apps get, relative to `$XDG_RUNTIME_DIR` (it should not record).
    pub pulse_socket: PathBuf,
    /// The sound socket apps with `mic` get. The same as `pulse_socket`: `mic` is not enforced,
    /// so it is not listed.
    pub pulse_record_socket: PathBuf,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enable: false,
            bwrap: "bwrap".into(),
            pulse_socket: "pulse/native".into(),
            pulse_record_socket: "pulse/native".into(),
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
    /// The Xwayland for apps (under the reaper: catalog apps, `apps.launch {command}`), so that
    /// they cannot read or type into the home app and the OSK on `display`. Not Steam (it and
    /// its games keep `display`). `None`: `display`.
    pub apps_display: Option<String>,
    pub wayland_display: Option<String>,
    pub focus_loss_debounce_ms: u64,
    /// Unmap invisible override-redirect windows that cover an app's real window (GTK3 GL
    /// dummies that gamescope enlarges to the whole output).
    pub unmap_gl_dummies: bool,
}

impl Default for GamescopeConfig {
    fn default() -> Self {
        Self {
            enable: true,
            display: None,
            apps_display: None,
            wayland_display: None,
            focus_loss_debounce_ms: 1500,
            unmap_gl_dummies: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputPlumberConfig {
    pub enable: bool,
    pub bus: Option<String>,
    /// The longest an InputPlumber method call or property access may take.
    pub call_timeout_ms: u64,
    /// How often each pad's mode and profile are checked (and re-applied if wrong).
    pub watchdog_ms: u64,
}

impl Default for InputPlumberConfig {
    fn default() -> Self {
        Self {
            enable: true,
            bus: None,
            call_timeout_ms: 2000,
            watchdog_ms: 5000,
        }
    }
}

impl InputPlumberConfig {
    pub fn call_timeout(&self) -> Duration {
        Duration::from_millis(self.call_timeout_ms.max(1))
    }

    pub fn watchdog(&self) -> Duration {
        Duration::from_millis(self.watchdog_ms.max(100))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputConfig {
    pub repeat_delay_ms: u64,
    pub repeat_interval_ms: u64,
    /// Xbox pads (xpad) arrive from InputPlumber with Y as `ui_action` and X as `ui_context`;
    /// swap them so Y is Context and X is Secondary like on other pads. `auto` detects it per
    /// pad (see `input::xbox_layout`).
    pub xbox_label_swap: crate::input::LabelSwap,
    /// Development only: read gamepads directly through evdev (see `input::devpad`). Ignored
    /// while InputPlumber is enabled.
    pub dev_gamepad: bool,
    /// A direction pressed again within this long after its release is stick chatter and
    /// ignored (InputPlumber has no hysteresis at its stick threshold); 0: off.
    pub direction_debounce_ms: u64,
    /// Ctrl + Super + D on a keyboard is Home, like a short Guide press. The daemon never reads
    /// keyboards: the system service sends `keyboard_home` for the chord ([`crate::system`]).
    pub keyboard_home: bool,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            repeat_delay_ms: 400,
            repeat_interval_ms: 100,
            xbox_label_swap: Default::default(),
            dev_gamepad: false,
            direction_debounce_ms: 40,
            keyboard_home: true,
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
    /// The profile app env's `share` (desktop files and icons of the profile's apps), searched
    /// first.
    #[serde(skip)]
    pub profile_share: Option<PathBuf>,
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
            profile_share: None,
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
    /// Show it as soon as a text field gets focus in a keyboard-profile app (Firefox, through
    /// the HinterOS extension and its native-messaging host), and hide it when the field loses
    /// focus. Off by default: `field_summon` waits for A instead.
    pub auto_show: bool,
    /// What summons it on a focused text field in a keyboard-profile app: `accept` (A, which
    /// then does not send Enter) or `none`.
    pub field_summon: FieldSummon,
}

/// `[osk] field_summon`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FieldSummon {
    /// A (accept) on a focused text field shows the keyboard instead of pressing Enter.
    #[default]
    #[serde(alias = "accept-on-field")]
    Accept,
    None,
}

impl Default for OskConfig {
    fn default() -> Self {
        Self {
            enable: true,
            command: vec!["hinteros-osk".into()],
            summon: "guide+context".into(),
            auto_show: false,
            field_summon: FieldSummon::Accept,
        }
    }
}

/// `[pointer]`: the right stick as a mouse pointer in keyboard-profile apps (see
/// `input::pointer`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PointerConfig {
    pub enable: bool,
    /// Pointer speed at full deflection, pixels per second (InputPlumber: fixed 0.20 deadzone,
    /// then speed_pps * m^k on the rescaled deflection m; k is the NixOS option
    /// `hinteros.input.pointerCurve`, so slow movements are slower, full speed is unchanged).
    pub speed_pps: u32,
    /// How far the right stick must move to count as "using the pointer" (0..1).
    pub activity_deadzone: f64,
    /// Ignored, accepted so that older configs still load. (A clicked while the right stick
    /// had moved this recently; now A never clicks and the pointer never hides on inactivity.)
    pub idle_ms: Option<u64>,
    /// Run to hide the pointer at once when the D-pad or left stick is used.
    pub hide_command: Vec<String>,
    /// Run to show it again.
    pub show_command: Vec<String>,
}

impl Default for PointerConfig {
    fn default() -> Self {
        Self {
            enable: true,
            speed_pps: 1000,
            activity_deadzone: 0.25,
            idle_ms: None,
            hide_command: ["gamescopectl", "paint_cursor_plane", "0"]
                .map(String::from)
                .into(),
            show_command: ["gamescopectl", "paint_cursor_plane", "1"]
                .map(String::from)
                .into(),
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
    pub fn direction_debounce(&self) -> Duration {
        Duration::from_millis(self.direction_debounce_ms)
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
    /// Keyboard profile, a browser with an address bar (plain Firefox, not a kiosk): View
    /// focuses the address bar (Ctrl+L) and shows the on-screen keyboard, unless the keymap
    /// maps `view` itself; LB / RB are history back / forward (Alt+Left / Alt+Right), unless
    /// the keymap maps `page_left` / `page_right` (or unmaps them with "none").
    #[serde(default)]
    pub address_bar: bool,
    /// Keyboard profile: the right stick moves a mouse pointer ([pointer]) [default: true].
    pub pointer: Option<bool>,
    /// Sandbox permissions the app declares (from the catalog): name -> required / optional.
    #[serde(default)]
    pub permissions: BTreeMap<String, hinteros_api::catalog::Need>,
    /// `false`: the app runs without the sandbox (from the catalog) [default: true].
    #[serde(default = "yes")]
    pub sandbox: bool,
}

fn native() -> AppKind {
    AppKind::Native
}

fn yes() -> bool {
    true
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
        if self.address_bar {
            // A browser: LB / RB are history back / forward.
            for (action, key) in [
                (NavAction::PageLeft, "alt+left"),
                (NavAction::PageRight, "alt+right"),
            ] {
                map.insert(action, key.parse::<KeyCombo>().map_err(anyhow::Error::msg)?);
            }
        }
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
        let mut config: Config = toml::from_str(text)?;
        if let AppsKey::List(list) = &config.apps_key {
            config.apps = list.clone();
        }
        config.validate()?;
        Ok(config)
    }

    /// The app env this config reads its apps from (`[apps] profile` or `--apps-profile`).
    pub fn apps_profile(&self) -> Option<PathBuf> {
        match &self.apps_key {
            AppsKey::Profile(t) => expand_profile(t),
            _ => None,
        }
    }

    /// Profile mode: the apps and Steam come from the env's manifest. A missing or broken
    /// manifest means no apps and no Steam (logged); the session keeps running.
    pub fn load_profile_apps(&mut self, profile: &Path) {
        self.apps.clear();
        // No Steam unless the manifest has it; autostart only with it.
        self.steam.path = PathBuf::new();
        let manifest = match hinteros_api::manifest::Manifest::load(profile) {
            Ok(m) => m,
            Err(hinteros_api::manifest::ManifestError::Missing(p)) => {
                tracing::debug!(manifest = %p.display(), "no app env yet");
                self.steam.autostart = false;
                return;
            }
            Err(e) => {
                tracing::warn!("ignoring the app env: {e}");
                self.steam.autostart = false;
                return;
            }
        };
        let mut apps = Vec::new();
        for v in manifest.apps {
            match serde_json::from_value::<AppConfig>(v) {
                Ok(a) => apps.push(a),
                Err(e) => tracing::warn!("ignoring an app of the manifest: {e}"),
            }
        }
        let probe = Config {
            apps,
            ..Config::default()
        };
        match probe.validate() {
            Ok(()) => self.apps = probe.apps,
            Err(e) => tracing::warn!("ignoring the app env's apps: {e:#}"),
        }
        match manifest.steam {
            Some(s) => self.steam.path = s.path,
            None => self.steam.autostart = false,
        }
        self.icons.profile_share = Some(profile.join("share"));
    }

    /// Sandboxed apps see their sandbox home as `$HOME`: their `firefox_profile` (`~/...`) is
    /// in it ([`crate::sandbox::outside_path`]).
    pub fn resolve_sandbox_paths(&mut self) {
        if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
            self.resolve_sandbox_paths_in(Path::new(&home));
        }
    }

    /// [`Config::resolve_sandbox_paths`] for the profile's `home`.
    pub fn resolve_sandbox_paths_in(&mut self, home: &Path) {
        if !self.sandbox.enable {
            return;
        }
        for app in self
            .apps
            .iter_mut()
            .filter(|a| a.sandbox && a.command.is_some())
        {
            if let Some(p) = &app.firefox_profile {
                app.firefox_profile =
                    Some(crate::sandbox::outside_path(&expand_home(p), home, &app.id));
            }
        }
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
        // The greeter (`--greeter`, `greet`) runs as hinteros-greeter, which has no apps (and
        // cannot read anyone's app env): nothing to look up.
        let profile = if cli.greeter {
            None
        } else {
            cli.apps_profile.clone().or_else(|| config.apps_profile())
        };
        if let Some(p) = &profile {
            config.icons.profile_share = Some(p.join("share"));
            config.load_profile_apps(p);
        }
        config.resolve_sandbox_paths();
        config.validate()?;
        Ok(config)
    }

    /// The app env link to watch for changes (profile mode).
    pub fn watched_profile(cli: &Cli) -> Option<PathBuf> {
        if cli.greeter {
            return None;
        }
        if let Some(p) = &cli.apps_profile {
            return Some(p.clone());
        }
        let path = Self::config_path(cli).filter(|p| cli.config.is_some() || p.exists())?;
        let text = std::fs::read_to_string(path).ok()?;
        Self::parse(&text).ok()?.apps_profile()
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
        if let Some(f) = &cli.audio_output_file {
            self.audio.shared_output = Some(f.clone());
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
        if let Some(d) = &self.gamescope.apps_display {
            ensure!(
                display_setting_ok(d),
                "gamescope.apps_display must be \"auto\", \"auto+<n>\" or \":<n>\""
            );
        }
        ensure!(
            (0.0..1.0).contains(&self.pointer.activity_deadzone),
            "pointer.activity_deadzone must be in [0, 1)"
        );
        ensure!(
            self.pointer.speed_pps > 0,
            "pointer.speed_pps must be positive"
        );
        for (key, p) in [
            ("pulse_socket", &self.sandbox.pulse_socket),
            ("pulse_record_socket", &self.sandbox.pulse_record_socket),
        ] {
            ensure!(
                p.is_relative()
                    && p.components()
                        .all(|c| matches!(c, std::path::Component::Normal(_))),
                "sandbox.{key} must be a path relative to XDG_RUNTIME_DIR"
            );
        }
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
                    app.back_key.is_none()
                        && app.keymap.is_empty()
                        && !app.address_bar
                        && app.pointer.is_none(),
                    "app '{}': back_key, keymap, address_bar and pointer need input = \"keyboard\"",
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

    /// The Xwayland display gamescope serves X11 clients on (`"auto"` resolved now).
    pub fn gamescope_display(&self) -> String {
        resolve_display(&self.gamescope_display_setting())
    }

    /// The configured display, possibly `"auto"`; resolve it with [`resolve_display`] at each
    /// connection (the session's displays exist only once gamescope is up).
    pub fn gamescope_display_setting(&self) -> String {
        self.gamescope
            .display
            .clone()
            .or_else(|| std::env::var("DISPLAY").ok().filter(|d| !d.is_empty()))
            .unwrap_or_else(|| ":0".into())
    }

    /// The display for apps under the reaper, possibly `"auto+<n>"` (resolve it with
    /// [`try_resolve_display`] at each spawn). `None` when apps use [`Self::child_env`]'s.
    pub fn apps_display_setting(&self) -> Option<String> {
        self.gamescope
            .enable
            .then(|| self.gamescope.apps_display.clone())
            .flatten()
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

    /// Environment for processes the daemon starts, so they land in gamescope. Empty when
    /// gamescope integration is off (they inherit the daemon's environment). Apps get it without
    /// the Wayland displays ([`crate::apps::spawn_env`]); only Steam keeps them.
    pub fn child_env(&self) -> Vec<(String, String)> {
        if !self.gamescope.enable {
            return Vec::new();
        }
        let wayland = self.wayland_display();
        vec![
            ("DISPLAY".into(), self.gamescope_display_setting()),
            ("WAYLAND_DISPLAY".into(), wayland.clone()),
            ("GAMESCOPE_WAYLAND_DISPLAY".into(), wayland),
        ]
    }
}

/// `display = "auto"`: the lowest X display whose server runs as this user, i.e. the first
/// Xwayland of this session's gamescope. The daemon is a user service, not gamescope's child, so
/// it can't inherit `DISPLAY`; and gamescope takes the first free numbers, which are not `:0`
/// while an ended session's or the picker's Xwayland is still there. `:0` until one exists.
/// `"auto+<n>"` ([`try_resolve_display`]) is returned as it is (not a display) until it exists.
/// Anything else is returned as it is.
pub fn resolve_display(setting: &str) -> String {
    try_resolve_display(setting).unwrap_or_else(|| {
        if setting == "auto" {
            ":0".into()
        } else {
            setting.to_owned()
        }
    })
}

/// A display setting resolved now; `None` for an `"auto"` one whose display does not exist
/// (yet). `"auto+<n>"`: this user's `n`-th live X display after the lowest (`"auto"` is `+0`).
/// gamescope creates its Xwaylands one after the other on the lowest free numbers, so with
/// `--xwayland-count 3` `"auto+2"` is its third Xwayland, also when another user's display sits
/// between them.
pub fn try_resolve_display(setting: &str) -> Option<String> {
    resolve_display_in(
        setting,
        Path::new("/tmp"),
        Path::new("/proc"),
        nix::unistd::geteuid().as_raw(),
    )
}

fn resolve_display_in(setting: &str, tmp: &Path, proc_dir: &Path, uid: u32) -> Option<String> {
    let Some(index) = auto_index(setting) else {
        return Some(setting.to_owned());
    };
    own_displays(tmp, proc_dir, uid)
        .get(index)
        .map(|n| format!(":{n}"))
}

/// `Some(n)` for `"auto"` (0) and `"auto+<n>"`.
fn auto_index(setting: &str) -> Option<usize> {
    match setting.strip_prefix("auto")? {
        "" => Some(0),
        rest => rest.strip_prefix('+')?.parse().ok(),
    }
}

/// `"auto"`, `"auto+<n>"` or `":<n>"`.
pub fn display_setting_ok(setting: &str) -> bool {
    auto_index(setting).is_some()
        || setting
            .strip_prefix(':')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// `env` (from [`Config::child_env`]) with a `DISPLAY` of `"auto"` resolved now, for a process
/// about to start.
pub fn live_env(env: &[(String, String)]) -> Vec<(String, String)> {
    env.iter()
        .map(|(k, v)| {
            let v = if k == "DISPLAY" {
                resolve_display(v)
            } else {
                v.clone()
            };
            (k.clone(), v)
        })
        .collect()
}

/// The lowest `n` with a `<tmp>/.X<n>-lock` owned by `uid` whose pid is alive and a
/// `<tmp>/.X11-unix/X<n>` socket.
pub fn own_display(tmp: &Path, proc_dir: &Path, uid: u32) -> Option<String> {
    own_displays(tmp, proc_dir, uid)
        .first()
        .map(|n| format!(":{n}"))
}

/// Every such `n` ([`own_display`]), lowest first.
pub fn own_displays(tmp: &Path, proc_dir: &Path, uid: u32) -> Vec<u32> {
    use std::os::unix::fs::MetadataExt;
    let mut found = Vec::new();
    let Ok(dir) = std::fs::read_dir(tmp) else {
        return found;
    };
    for e in dir.flatten() {
        let name = e.file_name();
        let Some(n) = name
            .to_str()
            .and_then(|s| s.strip_prefix(".X"))
            .and_then(|s| s.strip_suffix("-lock"))
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(meta) = std::fs::symlink_metadata(e.path()) else {
            continue;
        };
        if !meta.file_type().is_file() || meta.uid() != uid {
            continue;
        }
        let Some(pid) = std::fs::read_to_string(e.path()).ok().and_then(|s| {
            s.trim_matches(|c: char| c.is_whitespace() || c == '\0')
                .parse::<u32>()
                .ok()
        }) else {
            continue;
        };
        if !proc_dir.join(pid.to_string()).exists()
            || !tmp.join(".X11-unix").join(format!("X{n}")).exists()
        {
            continue;
        }
        found.push(n);
    }
    found.sort_unstable();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_display_is_this_users_lowest_live_one() {
        let tmp = tempfile::tempdir().unwrap();
        let proc_dir = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let me = nix::unistd::geteuid().as_raw();
        std::fs::create_dir_all(t.join(".X11-unix")).unwrap();
        for pid in ["50", "51", "60"] {
            std::fs::create_dir_all(proc_dir.path().join(pid)).unwrap();
        }
        assert_eq!(own_display(t, proc_dir.path(), me), None);
        // :0 and :1 dead (an ended session), :4 and :5 ours and alive, :3 has no socket.
        for (n, pid) in [(0, "40"), (1, "41"), (3, "60"), (4, "50"), (5, "51")] {
            std::fs::write(t.join(format!(".X{n}-lock")), format!("      {pid}\0")).unwrap();
            if n != 3 {
                std::fs::write(t.join(format!(".X11-unix/X{n}")), "").unwrap();
            }
        }
        assert_eq!(own_display(t, proc_dir.path(), me).as_deref(), Some(":4"));
        // Another user's live displays don't count.
        assert_eq!(own_display(t, proc_dir.path(), me + 1), None);
        assert_eq!(resolve_display(":7"), ":7");

        // "auto+<n>": the n-th after the lowest, skipping dead and other users' displays.
        std::fs::create_dir_all(proc_dir.path().join("52")).unwrap();
        std::fs::write(t.join(".X7-lock"), "      52\0").unwrap();
        std::fs::write(t.join(".X11-unix/X7"), "").unwrap();
        assert_eq!(own_displays(t, proc_dir.path(), me), [4, 5, 7]);
        let resolve = |s: &str| resolve_display_in(s, t, proc_dir.path(), me);
        assert_eq!(resolve("auto").as_deref(), Some(":4"));
        assert_eq!(resolve("auto+0").as_deref(), Some(":4"));
        assert_eq!(resolve("auto+2").as_deref(), Some(":7"));
        assert_eq!(resolve("auto+3"), None, "not there (yet)");
        assert_eq!(resolve(":9").as_deref(), Some(":9"));
        // Unresolved, "auto+<n>" stays a non-display rather than falling back to :0.
        assert_eq!(resolve_display("auto+99"), "auto+99");
    }

    #[test]
    fn display_settings() {
        for ok in ["auto", "auto+0", "auto+2", ":0", ":12"] {
            assert!(display_setting_ok(ok), "{ok}");
        }
        for bad in [
            "", "auto+", "auto2", "auto+-1", ":", ":x", "host:0", "auto+2 ",
        ] {
            assert!(!display_setting_ok(bad), "{bad}");
        }
        let c = Config::parse("[gamescope]\napps_display = \"auto+2\"\n").unwrap();
        assert_eq!(c.apps_display_setting().as_deref(), Some("auto+2"));
        assert!(Config::parse("[gamescope]\napps_display = \"auto2\"\n").is_err());
        let c = Config::parse("[gamescope]\nenable = false\napps_display = \":2\"\n").unwrap();
        assert_eq!(
            c.apps_display_setting(),
            None,
            "no gamescope: apps inherit the env"
        );
        assert_eq!(Config::default().apps_display_setting(), None);
    }

    #[test]
    fn apps_key_is_a_list_or_a_profile() {
        let c = Config::parse("[apps]\nprofile = \"/run/hinteros/apps/%u\"\n").unwrap();
        assert_eq!(c.apps_key, AppsKey::Profile("/run/hinteros/apps/%u".into()));
        assert!(c.apps.is_empty());
        for bad in [
            "[apps]\nprofile = \"/x\"\nextra = 1\n",
            "[apps]\n",
            "apps = 3\n",
            "[apps]\nprofile = \"/x\"\n[[apps]]\nid = \"a\"\nname = \"A\"\ncommand = [\"a\"]\n",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
        // A manifest: entries with the [[apps]] keys, Steam's path; broken entries are skipped.
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("share/hinteros")).unwrap();
        std::fs::write(
            d.path().join("share/hinteros/apps.json"),
            r#"{"version":1,"ids":["a","b"],"box":[],"steam":{"path":"/nix/store/x-steam/bin/steam"},
                "apps":[{"id":"a","name":"A","kind":"web","command":["ff"],"input":"keyboard"},
                        {"id":"b","name":"B","bogus":1}]}"#,
        )
        .unwrap();
        let mut c = Config::parse("[steam]\nautostart = true\n").unwrap();
        c.load_profile_apps(d.path());
        assert_eq!(c.apps.len(), 1);
        assert_eq!(c.apps[0].input, InputProfile::Keyboard);
        assert_eq!(c.steam.path, PathBuf::from("/nix/store/x-steam/bin/steam"));
        assert!(c.steam.autostart);
        assert_eq!(c.icons.profile_share, Some(d.path().join("share")));
        // No env yet: no apps, no Steam, no autostart.
        let mut c = Config::parse("[steam]\nautostart = true\n").unwrap();
        c.load_profile_apps(&d.path().join("nowhere"));
        assert!(c.apps.is_empty() && !c.steam.autostart);
        assert_eq!(c.steam.path, PathBuf::new());
    }

    #[test]
    fn sandbox_section_and_app_permissions() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.sandbox, SandboxConfig::default());
        assert!(!c.sandbox.enable);
        let c = Config::parse(
            "[sandbox]\nenable = true\nbwrap = \"/nix/store/x-bwrap/bin/bwrap\"\npulse_record_socket = \"pulse/record\"\n\
             [[apps]]\nid = \"a\"\nname = \"A\"\ncommand = [\"a\"]\npermissions = { mic = \"optional\", gamepad = \"required\" }\n\
             [[apps]]\nid = \"b\"\nname = \"B\"\ncommand = [\"b\"]\nsandbox = false\n",
        )
        .unwrap();
        assert!(c.sandbox.enable);
        assert_eq!(c.sandbox.pulse_record_socket, PathBuf::from("pulse/record"));
        assert!(c.apps[0].sandbox && !c.apps[1].sandbox);
        assert_eq!(
            c.apps[0].permissions["mic"],
            hinteros_api::catalog::Need::Optional
        );
        for bad in [
            "[sandbox]\npulse_socket = \"/run/user/1000/pulse/native\"\n",
            "[sandbox]\npulse_record_socket = \"../x\"\n",
            "[sandbox]\nextra = 1\n",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }

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
            call_timeout_ms = 500
            watchdog_ms = 0
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
        assert_eq!(c.inputplumber.call_timeout(), Duration::from_millis(500));
        assert_eq!(
            c.inputplumber.watchdog(),
            Duration::from_millis(100),
            "floor"
        );
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
    fn address_bar_maps_lb_rb_to_history() {
        let c = Config::parse(
            r#"
            [[apps]]
            id = "firefox"
            name = "Firefox"
            command = ["firefox"]
            input = "keyboard"
            address_bar = true
            [[apps]]
            id = "firefox-mine"
            name = "Firefox"
            command = ["firefox"]
            input = "keyboard"
            address_bar = true
            [apps.keymap]
            page_left = "none"
            page_right = "ctrl+tab"
            [[apps]]
            id = "kiosk"
            name = "Kiosk"
            command = ["firefox", "--kiosk"]
            input = "keyboard"
            "#,
        )
        .unwrap();
        let map = c.apps[0].keymap().unwrap().unwrap();
        assert_eq!(map[&NavAction::PageLeft], "alt+left".parse().unwrap());
        assert_eq!(map[&NavAction::PageRight], "alt+right".parse().unwrap());
        // The app's keymap still wins.
        let map = c.apps[1].keymap().unwrap().unwrap();
        assert!(!map.contains_key(&NavAction::PageLeft));
        assert_eq!(map[&NavAction::PageRight], "ctrl+tab".parse().unwrap());
        // No address bar, no history keys.
        let map = c.apps[2].keymap().unwrap().unwrap();
        assert!(!map.contains_key(&NavAction::PageLeft));
        assert!(!map.contains_key(&NavAction::PageRight));
    }

    #[test]
    fn field_summon_pointer_and_address_bar() {
        let c = Config::default();
        assert!(!c.osk.auto_show);
        assert_eq!(c.osk.field_summon, FieldSummon::Accept);
        assert!(c.pointer.enable);
        assert_eq!(c.pointer.idle_ms, None);
        assert_eq!(c.inputplumber.call_timeout(), Duration::from_secs(2));
        assert_eq!(c.inputplumber.watchdog(), Duration::from_secs(5));
        assert_eq!(
            c.pointer.hide_command,
            ["gamescopectl", "paint_cursor_plane", "0"]
        );
        assert_eq!(c.input.direction_debounce(), Duration::from_millis(40));
        assert!(c.input.keyboard_home);
        let c = Config::parse(
            r#"
            [osk]
            field_summon = "none"
            auto_show = true
            [pointer]
            enable = false
            speed_pps = 1500
            idle_ms = 1500
            hide_command = []
            [input]
            direction_debounce_ms = 0
            keyboard_home = false
            [[apps]]
            id = "firefox"
            name = "Firefox"
            command = ["firefox"]
            input = "keyboard"
            address_bar = true
            pointer = false
            "#,
        )
        .unwrap();
        assert_eq!(c.osk.field_summon, FieldSummon::None);
        assert!(c.osk.auto_show && !c.pointer.enable);
        assert!(!c.input.keyboard_home);
        assert_eq!((c.pointer.speed_pps, c.pointer.idle_ms), (1500, Some(1500)));
        assert!(c.pointer.hide_command.is_empty());
        assert!(c.apps[0].address_bar);
        assert_eq!(c.apps[0].pointer, Some(false));
        assert_eq!(
            Config::parse("[osk]\nfield_summon = \"accept-on-field\"\n")
                .unwrap()
                .osk
                .field_summon,
            FieldSummon::Accept
        );
        for bad in [
            "[osk]\nfield_summon = \"focus\"\n",
            "[pointer]\nactivity_deadzone = 1.5\n",
            "[[apps]]\nid = \"a\"\nname = \"A\"\ncommand = [\"a\"]\naddress_bar = true\n",
            "[[apps]]\nid = \"a\"\nname = \"A\"\ncommand = [\"a\"]\npointer = true\n",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
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

    #[test]
    fn sandboxed_firefox_profiles_are_in_the_sandbox_home() {
        let text = "[[apps]]\nid = \"cr\"\nname = \"CR\"\ncommand = [\"x\"]\nsite_url = \"https://c/\"\n\
                    firefox_profile = \"/home/tv/.local/share/hinteros/firefox/cr\"\n\
                    [[apps]]\nid = \"open\"\nname = \"O\"\ncommand = [\"x\"]\nsite_url = \"https://o/\"\n\
                    firefox_profile = \"/home/tv/ff/open\"\nsandbox = false\n";
        let home = Path::new("/home/tv");
        // Sandbox off: unchanged.
        let mut c = Config::parse(text).unwrap();
        c.resolve_sandbox_paths_in(home);
        assert_eq!(
            c.apps[0].firefox_profile.as_deref(),
            Some(Path::new("/home/tv/.local/share/hinteros/firefox/cr"))
        );
        let mut c = Config::parse(&format!("[sandbox]\nenable = true\n{text}")).unwrap();
        c.resolve_sandbox_paths_in(home);
        assert_eq!(
            c.apps[0].firefox_profile.as_deref(),
            Some(Path::new(
                "/home/tv/.local/share/hinteros/sandbox/cr/.local/share/hinteros/firefox/cr"
            ))
        );
        assert_eq!(
            c.apps[1].firefox_profile.as_deref(),
            Some(Path::new("/home/tv/ff/open")),
            "not sandboxed"
        );
    }
}
