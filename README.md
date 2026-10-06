# HinterOS Daemon

The Rust system services behind the launcher, built as two binaries:

- **`hinteros-session`** (unprivileged, runs in the user's session): controller input and the
  global Home button, launching and focusing apps in gamescope, typing into apps and the
  on-screen keyboard. Also the
  runtime settings (Wi-Fi, Bluetooth, audio, display mode, sleep/restart/shutdown) through
  the system's own services. Serves the launcher on `$XDG_RUNTIME_DIR/hinteros/session.sock`.
- **`hinteros-system`** (root, small, narrow API): settings in `state.json`, SSH on/off,
  rebuilds, verified updates, channel and base switches, rollback. Listens on
  `/run/hinteros/system.sock`; the session daemon relays it to the launcher as `system.*`.
  Authorizes every mutating request on its own (pidfd, seat0 owner, polkit) and audits it.

This repo owns the versioned socket API schema that the launcher (and future clients such as
a phone web admin UI) are generated from or contract-tested against.

## Layout

| Path | Contents |
|---|---|
| `schema/` | The API, as language-neutral JSON: `session-api.json` (launcher-facing), `system-api.json` (internal), and the meta-schema both follow |
| `crates/hinteros-api/` | Rust wire types, NDJSON framing, handshake, client; tests that check the types against `schema/` |
| `crates/hinteros-session/` | Session daemon |
| `crates/hinteros-system/` | Privileged system service |

The protocol is JSON-RPC 2.0, one message per line, starting with a `hello` version handshake.
The design and its reasoning live in `hinteros-docs/design/api.md`.

## Session daemon

What it does, and the code that does it (`crates/hinteros-session/src/`):

- **Focus** (`focus/`): writes gamescope's `GAMESCOPECTRL_BASELAYER_APPID` root-window property
  on its Xwayland (`[app, launcher]` to show an app as soon as it has a window, `[launcher]`
  for Home) and watches `GAMESCOPE_FOCUSED_APP` / `GAMESCOPE_FOCUSABLE_APPS`. Findings behind
  this: `hinteros-docs/spikes/03-steam-launcher.md`.
- **Input** (`input/`): InputPlumber over D-Bus. Sets `ManageAllDevices`, puts every pad in
  intercept mode ALL while the launcher has focus (input arrives as `ui_*` signals and becomes
  `input_nav`, with key repeat for held directions), and PASS while an app has focus (the pad
  goes to Steam/the game; Guide still comes to us), except for keyboard-profile apps and while
  the on-screen keyboard is up (both ALL, see below). Guide sends `home_pressed` and brings the
  launcher back (on release, since Guide also starts the on-screen keyboard chord). Re-applies everything when InputPlumber restarts or a pad appears. Without
  InputPlumber it logs and carries on. `--dev-gamepad` reads
  pads directly instead, for desktop previews only (see "Desktop preview").
- **Apps** (`apps.rs`, `shell.rs`): `apps.launch` runs a command, or hands a
  `steam://rungameid/<id>` URL to `steam -silent`, and tracks it (`app_started`,
  `app_exited`, `focus_changed`). A command app has exited when its process tree has; a Steam
  game when its windows are gone. Home and `apps.focus` switch focus without stopping apps.
  `apps.close` sends SIGTERM (a command app: to its reaper, which forwards it to the app's
  process group; a Steam game: to every process under Steam's `reaper SteamLaunch AppId=<id>`,
  never to Steam), SIGKILLs what is left after `close.timeout_ms`, and reports
  `app_exited {reason: closed}` (`procs.rs`).
- **Steam library** (`steam/library.rs`, `steam/vdf.rs`, `steam/watch.rs`): `games.list` reads
  `steamapps/libraryfolders.vdf` and every library's `appmanifest_<appid>.acf` (own KeyValues
  parser) from the Steam directory (`steam.dir`, else `~/.local/share/Steam`, else
  `~/.steam/steam`): name, install state from `StateFlags`, size, last played, and artwork
  paths from `appcache/librarycache` (all three cache layouts: `<appid>/<sha1>/library_*.jpg`,
  `<appid>/library_600x900.jpg` etc., and the old flat `<appid>_header.jpg`). Proton, Steam
  Linux Runtime, Steamworks redistributables and SteamVR are left out (known app ids, plus name
  prefixes for future versions). inotify on the library directories (debounced, plus a periodic
  rescan) sends `games_changed` when the result changes. Launch a game with
  `apps.launch {url: "steam://rungameid/<appid>"}`; it is named from its manifest.
- **Steam itself** (`shell.rs`, `steam/windows.rs`): `apps.list` always starts with the built-in
  `steam` entry. Launching it runs `steam steam://open/bigpicture` (or `steam -gamepadui` if no
  Steam client runs) and shows Steam's windows: they are gamescope app 769, so focus is
  `[769, launcher]`, with InputPlumber in PASS. Home returns to the launcher; `apps.close`
  asks Steam to close Big Picture (`steam://close/bigpicture`) and never kills Steam.
- **Steam pop-ups** (`focus/gamescope.rs`, `steam/windows.rs`): the gamescope watcher also
  follows the top-level windows on gamescope's Xwayland, recognises Steam's (owner process
  `steam`/`steamwebhelper` via XRes or `_NET_WM_PID`, `WM_CLASS` `steam`/`steamwebhelper`, or
  `STEAM_GAME=769`), tags any that lack it with `STEAM_GAME=769` so gamescope can show them, and
  reports mapped Steam dialogs that appear while something else has focus as
  `steam_attention` (also `steam.attention`). `steam.focus` brings them forward as the app
  `steam`; when they close, focus returns to whoever had it. Menus, tooltips, overlays
  (`STEAM_OVERLAY`), unmapped and 1x1 windows never count.
- **Keyboard input profiles** (`shell.rs`, `keys/`): an `[[apps]]` entry with
  `input = "keyboard"` (TV web apps: Firefox kiosk YouTube TV, Crunchyroll) keeps InputPlumber
  in ALL while it has focus and turns the pad into key presses on the daemon's virtual keyboard:
  d-pad/stick to arrows (with the daemon's key repeat), A to Enter, B to `back_key` (Escape
  unless set), plus anything in `[apps.keymap]`. Guide is still Home. Steam games are always
  `gamepad` (the default: InputPlumber PASS, Steam Input applies). Without a virtual keyboard a
  keyboard app falls back to the gamepad.
- **Typing** (`keys/`): `keys.type {text}` types per character through a uinput virtual US
  keyboard (Shift for capitals and symbols; the spike's `kbd.py` table) or, for characters a US
  keyboard does not have, gamescope's private `gamescope_input_method` protocol (one character
  per commit, like Steam's keyboard); `untypeable` lists what neither could type.
  `keys.key {key, modifiers}` and `keys.action {action}` send single keys and editing actions.
  All of it goes to whatever has keyboard focus (never the on-screen keyboard), in order, from a
  worker thread. Findings behind this: `hinteros-docs/spikes/05-on-screen-keyboard.md`.
- **On-screen keyboard** (`osk.rs`, `focus/gamescope.rs`): the daemon starts and restarts
  `[osk] command` (`hinteros-osk`, a second entrypoint of the launcher, which connects to the
  same socket as client `hinteros-osk`; it gets `HINTEROS_SESSION_SOCKET`). Its X window
  (`WM_CLASS` or process `hinteros-osk`, or the started pid) becomes a gamescope
  `GAMESCOPE_EXTERNAL_OVERLAY`: painted over everything, never focused, shown and hidden with
  `_NET_WM_WINDOW_OPACITY` (it stays resident). `osk.show`/`osk.hide` or the controller chord
  (hold Guide, press Y) toggle it; `osk_visibility` reports it. While it is visible InputPlumber
  is in ALL and `input_nav` goes only to the `hinteros-osk` connection, not the launcher; hiding
  restores the previous mode. Home and focus changes hide it.
  Chord: InputPlumber has no chord events, but in PASS mode it switches the pad to ALL on the
  Guide press, so Y pressed while Guide is held reaches the daemon in every mode (Y held first
  only works where the daemon already sees Y). Because Guide may start a chord, Home happens on
  Guide *release*; `summon = "none"` puts it back on the press. The keyboard button (`ui_osk`,
  on pads that have one) also toggles it in keyboard-profile apps.
- **App icons** (`icons/`): HinterOS ships no logos; `apps.list`'s `icon` is a local file found
  on the box at runtime. Per app, first match wins: `icon` (an absolute path or an icon theme
  name), then `desktop_id` (the app's freedesktop `.desktop` file, its `Icon=`, looked up in the
  icon themes), then for web apps `icon_url` / `site_url`, then `firefox_profile`. The built-in `steam`
  entry uses Steam's own `steam.desktop`. Desktop files and icons are searched in `$XDG_DATA_HOME`,
  `$XDG_DATA_DIRS`, `/run/current-system/sw/share`, `/etc/profiles/per-user/$USER/share`,
  `~/.nix-profile/share` and the Flatpak exports; themes searched are the active one (GTK's
  `gtk-icon-theme-name`, or `icons.theme`), what it inherits, and `hicolor`, plus `pixmaps/`.
  The best file wins: the largest PNG of at least 256 px, else an SVG, else the largest
  smaller PNG. Web apps: the daemon fetches `icon_url`, or finds the icon of `site_url` (web app
  manifest icons, `apple-touch-icon`, `<link rel=icon>`, `/favicon.ico`; the largest square one
  of at least 192 px, not mask-only), over https only (also through redirects), with timeouts,
  a 1 MiB limit and an image check, in a background thread; it caches the file as
  `$XDG_CACHE_HOME/hinteros/icons/<app-id>.<ext>`, refetches after `icons.refresh_days` (30),
  retries failures (1 min, 10 min, 1 h, then every 6 h), and sends `apps_changed` when an
  icon appears. Offline or failing, the app just has no `icon` (the launcher draws a letter
  tile) or keeps the stale cached one.
  Some sites refuse non-browser fetches (Crunchyroll answers 403): for those, `firefox_profile`
  (the app's kiosk profile, `~/` allowed; needs `site_url`) names the profile whose
  `favicons.sqlite` Firefox filled when it visited the site. The daemon copies that database (and
  its `-wal`) to a temporary directory, never opening Firefox's own files, takes the largest icon
  stored for the site, and exports it as `<app-id>.firefox.<ext>` in the icon cache. It is read
  at startup, when a download fails, and again whenever the app exits (`app_exited`), sending
  `apps_changed` if the icon changed; a downloaded icon always replaces it.
- **Runtime settings** (`runtime/`): Wi-Fi through NetworkManager, Bluetooth through BlueZ
  (with a pairing agent), power through logind (all D-Bus, zbus), audio through `pactl`, display
  modes through gamescope's Wayland output, Xwayland and mode save file. Session API 0.7; see
  "Runtime settings" below.
- **System relay** (`system.rs`): one connection to the system service (see below), handshaken
  and subscribed to its job events; the session API's `system.*` methods are passed through
  unchanged and `job_*` events forwarded. While the service is down or speaks another version,
  they fail with `system_unavailable {reason: down|version_mismatch}`.
- **Reaper** (`reaper.rs`): `hinteros-session reaper SteamLaunch AppId=<id> -- <command>` runs
  a command as a child subreaper named `reaper`, which is how gamescope (Steam mode) learns the
  app id of a process's windows, X11 and native Wayland alike. The daemon starts command apps
  this way; the launcher must be started this way too (see below).

### Configuration

All optional. The config file is `--config <path>` (or `HINTEROS_SESSION_CONFIG`), else
`$XDG_CONFIG_HOME/hinteros/session.toml` if it exists. Command-line flags override it.

```toml
[launcher]
app_id = 4100000000            # gamescope app id of the launcher's windows
command = ["hinteros-launcher"]  # optional: the daemon starts and restarts the launcher itself

[steam]
path = "steam"                 # binary for steam:// URLs and Big Picture (a stub works for tests)
dir = "/home/tv/.local/share/Steam"  # library + artwork [default: ~/.local/share/Steam, ~/.steam/steam]
autostart = false              # start `steam -silent` with the daemon
launch_timeout_s = 300         # a game (or Big Picture) that never shows a window ends with launch_timeout (0: never)
tag_windows = true             # tag Steam's windows STEAM_GAME=769 so gamescope can show them
library_poll_s = 60            # rescan the library this often besides inotify (0: inotify only)
library_debounce_ms = 500      # wait after a library change before rescanning

[close]
timeout_ms = 5000              # apps.close: SIGKILL after this

[gamescope]
enable = true
display = ":0"                 # gamescope's Xwayland [default: $DISPLAY, else :0]
wayland_display = "gamescope-0"  # given to launched apps [default: $WAYLAND_DISPLAY, else gamescope-0]
focus_loss_debounce_ms = 1500  # how long the focused app may be without a window

[inputplumber]
enable = true
bus = "unix:path=/run/dbus/system_bus_socket"  # default: the system bus

[input]
repeat_delay_ms = 400
repeat_interval_ms = 100
dev_gamepad = false            # development only, see "Desktop preview"

[icons]
fetch = true                   # download web app icons (https only)
refresh_days = 30
# cache_dir = "~/.cache/hinteros/icons"   # theme = "Papirus"   # data_dirs = ["/usr/share"]

[keys]
uinput = true                  # create the virtual keyboard (/dev/uinput)
gamescope_ime = true           # gamescope's input method for non-US characters (gamescope only)
key_delay_ms = 12              # between key events
ime_delay_ms = 80              # between input method characters

[osk]
enable = true
command = ["hinteros-osk"]     # started (and restarted) if found; not under the reaper
summon = "guide+context"       # hold Guide + press this button (Y); or "none"

[system]
enable = true                  # relay system.* to the system service
socket = "/run/hinteros/system.sock"

[runtime]
enable = true                  # network.*, wifi.*, bluetooth.*, audio.*, display.*, power.*
# bus = "unix:path=/run/dbus/system_bus_socket"   # NetworkManager, BlueZ, logind [default: the system bus]

[audio]
pactl = "pactl"                # PipeWire's PulseAudio interface; found on PATH
poll_ms = 2000                 # look for changes made elsewhere (only while a client is connected)

[display]
# save_file = "/home/tv/.local/state/hinteros/gamescope-modes"   # [default: $GAMESCOPE_MODE_SAVE_FILE]
# modes_file = "/run/user/1000/gamescope-edid.modes"            # [default: $GAMESCOPE_PATCHED_EDID_FILE + ".modes"]
apply_timeout_ms = 6000        # display.set_mode: how long gamescope gets to switch

[bluetooth]
pair_timeout_s = 45            # bluetooth.pair gives up
confirm_timeout_s = 30         # a numeric comparison nobody answers is rejected

[[apps]]                       # what apps.list returns after the built-in "steam" (a stub list if none)
id = "jellyfin"
name = "Jellyfin"
command = ["jellyfin-desktop"]
desktop_id = "org.jellyfin.JellyfinDesktop"   # icon from its .desktop file (or: icon = "/path.png")

[[apps]]
id = "youtube"
name = "YouTube"
kind = "web"
command = ["firefox", "--kiosk", "--new-instance", "https://www.youtube.com/tv"]
site_url = "https://www.youtube.com/tv"     # icon from the site (or: icon_url = "https://.../icon.png")
# firefox_profile = "~/.local/share/hinteros/firefox/youtube"  # fallback: the favicon Firefox stored
input = "keyboard"             # "gamepad" (default) or "keyboard"
back_key = "escape"            # what B sends [default: escape]; e.g. "backspace", "alt+left"

[apps.keymap]                  # controller action = key, on top of the defaults ("none" unmaps)
context = "ctrl+l"             # Y
menu = "f11"                   # Start

[[apps]]
id = "portal-2"
name = "Portal 2"
steam_app_id = 620
```

| Flag | Env | Overrides |
|---|---|---|
| `--socket <path>` | `HINTEROS_SESSION_SOCKET` | socket path |
| `--config <path>` | `HINTEROS_SESSION_CONFIG` | config file |
| `--launcher-app-id <n>` | | `launcher.app_id` |
| `--launcher-command "<cmd args>"` | | `launcher.command` (split on spaces) |
| `--steam-path <path>` | `HINTEROS_STEAM` | `steam.path` |
| `--steam-dir <path>` | `HINTEROS_STEAM_DIR` | `steam.dir` (e.g. `tests/fixtures/steam-home/.local/share/Steam`) |
| `--gamescope` / `--no-gamescope` | | `gamescope.enable` |
| `--gamescope-display <d>` | `HINTEROS_GAMESCOPE_DISPLAY` | `gamescope.display` |
| `--inputplumber` / `--no-inputplumber` | | `inputplumber.enable` |
| `--inputplumber-bus <address>` | `HINTEROS_INPUTPLUMBER_BUS` | `inputplumber.bus` |
| `--no-uinput` | | `keys.uinput = false` |
| `--osk-command "<cmd args>"` | | `osk.command` (split on spaces) |
| `--system-socket <path>` | `HINTEROS_SYSTEM_SOCKET` | `system.socket` |
| `--no-runtime` | | `runtime.enable = false` |
| `--runtime-bus <address>` | `HINTEROS_RUNTIME_BUS` | `runtime.bus` (tests: a private bus with fake services) |
| `--pactl <path>` | `HINTEROS_PACTL` | `audio.pactl` (tests: a fake) |
| `--no-icon-fetch` | | `icons.fetch = false` |
| `--dev-gamepad` | | `input.dev_gamepad = true` (development only) |
| `--fake-keyboard <file>` (hidden) | `HINTEROS_FAKE_KEYBOARD` | append `down <code>`/`up <code>` lines to a file instead of creating a device (tests) |

Keymap values are a key from the API's `Key` list (`a`-`z`, `0`-`9`, `f1`-`f12`, `enter`,
`escape`/`esc`, `backspace`, `tab`, `space`, `delete`, `home`, `end`, `page_up`, `page_down`,
arrows, punctuation names, `back`, `forward`, `refresh`, `play_pause`, `stop`, `next_track`,
`previous_track`, `volume_up`, `volume_down`, `mute`, ...), optionally with modifiers
(`ctrl+`, `shift+`, `alt+`, `meta+`). Keymap names are the `input_nav` actions (`up`, `down`,
`left`, `right`, `accept`, `back`, `context`, `secondary`, `menu`, `view`, `page_left`,
`page_right`, `trigger_left`, `trigger_right`, `quick`, `keyboard`). Defaults: directions to
arrows, `accept` to `enter`, `back` to `back_key`.

On a dev desktop run with `--no-gamescope --no-inputplumber --no-uinput` (otherwise it would
grab your controllers through a running InputPlumber, and a keyboard-profile app or `keys.*`
would type into your desktop).

### Desktop preview (development only)

To try the real launcher on a desktop, with an Xbox controller and without gamescope or
InputPlumber:

```sh
nix run .#desktop-preview        # in this repo; prints the socket path and keeps running
```

It runs `hinteros-session --no-gamescope --no-inputplumber --no-uinput --dev-gamepad` on a
temporary socket (removed on exit), with your real Steam library (`~/.local/share/Steam` or
`~/.steam/steam`, or `HINTEROS_STEAM_DIR`; the daemon only reads it) and a sample app list
(Jellyfin, Firefox, YouTube, Crunchyroll, Plex, VacuumTube; your own with
`HINTEROS_SESSION_CONFIG=<file>`). Extra arguments go to `hinteros-session`. In another
terminal, start the launcher against that socket:

```sh
HINTEROS_SESSION_SOCKET=/run/user/1000/hinteros-preview.XXXXXX/session.sock \
  nix run git+https://justsomeshed.com/JSwartz/HinterOS/hinteros-launcher.git -- --windowed
```

**Launching an app or game from the launcher starts it for real on your desktop** (Steam
games through your running Steam client, apps as ordinary processes). Home returns to the
launcher but does not hide windows, and nothing is sandboxed. Keyboard input profiles and
`keys.*` are off (they need uinput), so TV web apps get no controller-to-keys mapping here.

`--dev-gamepad` / `[input] dev_gamepad = true` is the input backend behind it (`input/devpad.rs`).
It is for development only: it reads gamepads straight from `/dev/input/event*` through evdev,
read-only and never grabbed (no `EVIOCGRAB`), so Steam and games on the desktop keep seeing the
pad. It produces the same `ui_*` events as InputPlumber, so `input_nav`, key repeat
(`[input] repeat_*`) and Guide/Home (the Xbox button) behave the same: d-pad and left stick are
directions, A accept, B back, X secondary, Y context, bumpers `page_left/right`, triggers (past
half travel), View and Menu, Xbox button Home. Pads are picked up when plugged in. Reading
needs access to the device (a pad on your seat has it through logind, else join the `input`
group). It is ignored, with a warning, while InputPlumber is enabled, so on a real box it does
nothing: it never takes part in the TV setup.

### What the session must provide

- gamescope in **Steam mode** (`-e`/`--steam`): only then does it read app ids from reapers and
  publish `GAMESCOPE_FOCUSED_APP`. In that mode nothing is shown until someone writes
  `GAMESCOPECTRL_BASELAYER_APPID`, which the daemon does as soon as it reaches gamescope.
- The launcher running under the reaper with `launcher.app_id`, e.g. as gamescope's child:
  `gamescope -e … -- hinteros-session reaper SteamLaunch AppId=4100000000 -- hinteros-launcher`
  (or set `launcher.command` and let the daemon start it). Steam itself must **not** run under
  a reaper with an app id: the outermost one wins, and games must get their Steam app ids.
- The daemon reaching gamescope's Xwayland: `DISPLAY` (or `gamescope.display`), `:0` when
  gamescope is the only X server. No Xauthority is needed. Launched apps get `DISPLAY` and
  `WAYLAND_DISPLAY` from the config, since a user service does not inherit gamescope's.
- InputPlumber: polkit, the session user in group `inputplumber`, its udev rules. The daemon
  sets `ManageAllDevices` itself on every InputPlumber (re)start.
- A `steam` binary on the daemon's `PATH`, or `steam.path`.
- For keyboard input profiles and `keys.*`: write access to `/dev/uinput` (the `uinput` kernel
  module, `hardware.uinput.enable`, and the session user in the `uinput` group). If the device
  cannot be created the daemon logs why and carries on without it (keyboard apps get the
  gamepad, `keys.key` answers `keyboard_unavailable`). gamescope's XKB layout must be US
  (`XKB_DEFAULT_LAYOUT=us`), since the virtual keyboard sends US key codes.
- For non-US characters: gamescope's Wayland socket (`gamescope.wayland_display`) and its
  private `gamescope_input_method` protocol, which only exists in gamescope and may change:
  **pin gamescope** (tested against 3.16.31's `protocol/gamescope-input-method.xml`, copied to
  `crates/hinteros-session/protocol/`) and re-check that file when updating it. It is unavailable
  while Steam's own keyboard holds the input method.
- For the on-screen keyboard: the `hinteros-osk` binary on the daemon's `PATH` (or `osk.command`),
  rendering into an ARGB window on gamescope's Xwayland.
- For `games.list`: the session user's Steam directory under `$HOME` (or `steam.dir`). Fake
  libraries for tests and VMs: `crates/hinteros-session/tests/fixtures/steam-home` (a home
  with `.local/share/Steam` and a second library at `SteamLibrary`, linked by a relative path in
  `libraryfolders.vdf`, which only the daemon accepts; real Steam writes absolute ones).

### Runtime settings (session API 0.7)

Settings that need no rebuild, served by the session daemon as the session user and authorised by
the polkit rules of the system repo's `runtime.nix` (NetworkManager `network-control`,
`enable-disable-wifi`, `wifi.scan`, `settings.modify.own` and `.system`; logind suspend, reboot,
power-off, for the active local session user). Code in `crates/hinteros-session/src/runtime/`;
every method and event is in `schema/session-api.json` with examples.

| Area | Methods | Events | Through |
|---|---|---|---|
| Wi-Fi | `network.status`, `wifi.scan`, `wifi.list`, `wifi.connect {ssid, password?}`, `wifi.disconnect`, `wifi.forget {ssid}`, `wifi.enable {enabled}` | `network_changed`, `wifi_list_changed` | NetworkManager (D-Bus) |
| Bluetooth | `bluetooth.status`, `.scan {enabled, timeout_s?}`, `.devices`, `.pair {address}`, `.confirm {address, accept}`, `.connect`, `.disconnect`, `.forget` | `bluetooth_changed`, `bluetooth_confirm` | BlueZ (D-Bus) |
| Audio | `audio.outputs`, `.set_output {id}`, `.volume {percent?}`, `.mute {muted}` (all return the same state) | `audio_changed` | `pactl` (PipeWire's pulse server) |
| Display | `display.modes`, `.set_mode {width, height, refresh}`, `.keep`, `.revert` | | gamescope (Wayland output, Xwayland, mode save file) |
| Power | `power.capabilities`, `.suspend`, `.reboot`, `.poweroff` | | logind (D-Bus) |

Events carry the same object as the matching query (`network_changed` = `network.status`,
`wifi_list_changed` = `wifi.list`, `audio_changed` = `audio.outputs`, `bluetooth_changed` =
`{status, devices}`), are debounced, and only sent when something changed (signal strength moves
in steps of 5 for Wi-Fi and 10 dBm for Bluetooth). Our own changes reach other clients at once.

Errors (4xxx): `unavailable {service: networkmanager|bluez|audio|display|logind, reason}` when the
service is missing, `device_failed {reason?}` (a connection, pairing or mode change that failed),
`wrong_password`, `not_authorized` (polkit said no), `power_inhibited {who?, why?}` (logind's
inhibitor refusal, its text unchanged), plus the existing `not_found` and `invalid_params`. Each
feature degrades on its own: without NetworkManager, BlueZ, `pactl`, gamescope or logind only
that feature answers `unavailable`; the rest of the API is unaffected, which is what a desktop
preview or CI sees. Every call has a time limit (30 s; `wifi.connect` 60 s, `bluetooth.pair`
90 s), so a hung service cannot hang the launcher.

**Wi-Fi.** `wifi.connect` creates a *system* connection (`AddConnection`, then
`ActivateConnection`), so it exists before login and for every profile; a stored network needs no
password, a new password replaces the stored one. WPA/WPA2 (`wpa-psk`) and WPA3 (`sae`) and open
networks work; WEP and 802.1X are refused (`invalid_params {reason: unsupported_security}`). The
call waits for the device to be activated; a rejected key (NetworkManager asking for secrets it
has no agent for, or reason `NO_SECRETS`) is `wrong_password`, and a connection it created is
deleted again. Passwords go only into the D-Bus call; they are never logged (the `Debug` of the
params hides them, and a test greps the daemon's debug log). `wifi.scan` returns after the scan
(NetworkManager refuses scans right after another one; the current list comes back then).

**Bluetooth.** The daemon registers a BlueZ agent with capability `DisplayYesNo` the first time
the user scans or pairs (not at startup, so a desktop preview keeps its own agent), and again
after BlueZ restarts: devices without input or output (controllers, most headsets) pair as "just
works" without any prompt. A numeric comparison becomes `bluetooth_confirm {address, name,
passkey, mode: confirm}` and waits (30 s) for `bluetooth.confirm`; a passkey the user must type on
the device is `mode: display` (no answer); a legacy PIN request is answered with `0000`.
The agent says yes only while the user is scanning or pairing: any device in range could
otherwise start pairing. `bluetooth.pair` also sets `Trusted` (so a pad reconnects by itself) and
connects if the device did not. `bluetooth.scan` stops by itself after `timeout_s` (30, at most
300). `kind` comes from BlueZ's `Icon`, else the class of device, else the GAP appearance
(`gamepad`, `headset`, `speaker`, `keyboard`, `mouse`, `phone`, `computer`, `other`); `battery`
from `org.bluez.Battery1` when the device reports it.

**Audio: `pactl`, not `wpctl`.** `pactl --format=json list sinks` is structured output of a stable
protocol (PipeWire's pulse server emulates it, `pulse.enable = true` in `runtime.nix`) with each
sink's name, description, volume, mute state, ports and device properties (`device.bus`
tells Bluetooth and USB, ports and names tell HDMI), and `get-default-sink`, `set-default-sink`,
`set-sink-volume` and `set-sink-mute` cover the rest. `wpctl` has no machine-readable listing
(`wpctl status` is a tree for humans, `wpctl inspect` prints properties as text), so the output
list and kinds would need fragile parsing. There is no subscription (`pactl subscribe` would be a
child process to supervise): changes made elsewhere are found by polling every `audio.poll_ms`
while a client is connected. Volume is capped at 100 (`invalid_params` above; a sink already
above 100 reports 100). Outputs are sinks; `id` is the sink name, stable across restarts.

**Display: gamescope's mode save file plus a nudge.** What gamescope 3.16.31 offers (read from
its source in nixpkgs): there is *no* request to change the output mode at runtime, neither in
`gamescope_control` (v7) nor in `gamescope_private` (which only runs convars such as
`backend_set_dirty`); restarting it with other `-W/-H/-r` would take the session with it. But for
an *external* display with no `-W/-H/-r`, gamescope's `setup_best_connector` looks the display up
by name in the file `GAMESCOPE_MODE_SAVE_FILE` (`<make> <model>:<W>x<H>@<R>` lines) and picks that
mode, and setting the root-window property `GAMESCOPE_DISPLAY_MODE_NUDGE` on its Xwayland makes it
run `setup_best_connector` again with a forced modeset. So `display.set_mode` writes the
display's line (the other lines and gamescope's trailing field are kept) and sets the nudge,
then waits (`display.apply_timeout_ms`) until gamescope's `wl_output` shows the new mode. This is
a real runtime change, and the saved mode is also what gamescope uses at its next start, so it
persists across gamescope and session restarts with no rebuild. If gamescope does not switch
(it runs with `-W/-H/-r`, which win) the old file is restored and the call fails with
`device_failed {reason: not_applied}`. Listing: gamescope writes the connector's modes (`WxH@R`
per line, its preference order, the first is the display's own choice) to
`<GAMESCOPE_PATCHED_EDID_FILE>.modes` whenever it picks a connector; the current mode and the
display's name come from its Wayland display (`wl_output.mode`, `gamescope_control`'s
`active_display_info`). `can_set` is false for an internal panel (its mode is never read from the
file) or without the save file. Refresh rates are whole Hz (59.94 is 60).
Keep-or-revert: `display.set_mode` returns `{previous, current}` and remembers the mode it
replaced; the launcher's 15 s dialog calls `display.keep` when the user keeps it and
`display.revert` when the time runs out (also after a second change before deciding: revert goes
back to the mode before both).

**Power.** `Suspend`, `Reboot` and `PowerOff` go to logind with `interactive = false`;
`power.capabilities` reports `CanSuspend`/`CanReboot`/`CanPowerOff` (`yes`, `no`, `challenge`,
`na`). While a system job switches over, its `block` inhibitor makes logind refuse; that is
`power_inhibited` with logind's message and the `who`/`why` it names.

**What the system repo must provide for these** (beyond `runtime.nix`'s polkit rules, services
and `hardware.bluetooth`/`pipewire` settings):

- `hinteros-session.service` (or however the session starts the daemon) runs as the session user
  with access to the **system bus** (`DBUS_SYSTEM_BUS_ADDRESS` default is fine; no sandboxing of
  `/run/dbus`). The user must be allowed to talk to **org.bluez** (BlueZ's stock bus policy allows
  `at_console` users and group `lp`) and to NetworkManager and logind (their stock policies
  allow any user; polkit decides). Verify the BlueZ part in the VM test: pairing as the session
  user over the system bus.
- **`pactl`** on the daemon's `PATH` (`pkgs.pulseaudio`; the server is PipeWire's), and
  `XDG_RUNTIME_DIR` set so it finds `pipewire-pulse`'s socket. The session user's PipeWire and
  WirePlumber must be running (the default sink is WirePlumber's).
- **gamescope** started with `GAMESCOPE_PATCHED_EDID_FILE` (a file in `$XDG_RUNTIME_DIR`) and
  **`GAMESCOPE_MODE_SAVE_FILE`** (a persistent file, e.g. `$XDG_STATE_HOME/hinteros/gamescope-modes`),
  and **without `-W`, `-H`, `-r`** (and not `--prefer-output` pinned to an internal panel) so
  that the saved mode applies. The same two variables must be in the daemon's environment (or set
  `[display] save_file` and `modes_file`). gamescope in Steam mode (`-e`), where its Wayland output
  is the real output size. The daemon needs gamescope's Xwayland (`gamescope.display`) and Wayland
  display (`gamescope.wayland_display`, default `gamescope-0`, in `$XDG_RUNTIME_DIR`), the same
  as for focus control and the input method; the private `gamescope_control` protocol is pinned
  with gamescope like the input method one (`protocol/gamescope-control.xml`).
- The polkit rules of `runtime.nix` as they are; `hinteros-system`'s `block` inhibitor during
  activation (system-api.md) for `power_inhibited`.

Verified here: all of it against fakes on private buses and sockets (`tests/runtime_*.rs`:
logind, NetworkManager with scripted passwords and access points, BlueZ including the agent
being called the way BlueZ calls it, a fake `pactl`, an Xvfb plus a wayland-server compositor
that reacts to the nudge like gamescope does), never the machine's own services. Needs real
hardware: NetworkManager's exact failure reasons for a wrong WPA3 password, BlueZ pairing with a
real controller and headset (and the bus policy for the session user), that gamescope 3.16.31
really switches on the nudge on the target display (read in its source, not run), PipeWire sink
names and ports of the real HDMI and USB devices, and logind's inhibitor error text.

### Steam: what is verified and what is inferred

Verified: gamescope's rules, read in its source (`steamcompmgr.cpp`: in Steam mode only windows
with an app id or `window_is_steam` (app id 769 / `STEAM_BIGPICTURE`) are focus candidates;
`STEAM_GAME` on a window is authoritative; `GAMESCOPECTRL_BASELAYER_WINDOW` only picks among those
candidates, so it cannot show an untagged window). Observed on a desktop with Steam build
1788652215: Steam's `steamwebhelper` windows set `STEAM_GAME=769` themselves (main window, popup
menus); windows of the `steam` process itself do not; `WM_CLASS` is `steamwebhelper`/`steam`,
`Steam`/`Steam` or `steam`/`Steam`; process names are `steam` and `steamwebhelper`. The daemon
side runs against stand-ins in the tests (Xvfb windows that look like Steam's, a fake `steam`,
our own reaper playing Steam's, the in-tree fake library).

Inferred, needs real gamescope + Steam: which windows Steam opens for Steam Guard, updates,
cloud conflicts and sign-in in `-silent` mode, and their titles (in Big Picture they are drawn
inside its window instead); that `[769, launcher]` shows them with working input; that
`steam://open/bigpicture` and `steam://close/bigpicture` behave in gamescope; whether Steam in
Big Picture writes `GAMESCOPECTRL_BASELAYER_APPID` itself (the daemon logs it if so); and that
SIGTERM to the processes under Steam's reaper ends Proton games cleanly.

### Keyboard and on-screen keyboard: what is verified and what needs hardware

Verified here: the key planning and pacing (unit tests), the input method client against a
fake compositor speaking the protocol XML (wayland-server), the overlay atoms and opacity on an
Xvfb standing in for gamescope's Xwayland, keyboard-profile translation, `input_nav` routing
and the Guide chord against the fake InputPlumber with a fake keyboard (`--fake-keyboard`),
and creating the real uinput device (`HINTEROS_TEST_UINPUT=1`). Spike 5 proved in VMs that
uinput typing reaches Firefox, that `gamescope_input_method` types Unicode into it, and that a
`GAMESCOPE_EXTERNAL_OVERLAY` window toggled by opacity draws over Firefox without taking focus.

Needs real gamescope and a pad: the overlay's size and position at the output resolution, the
Flutter OSK's transparent window, Guide+Y reaching the daemon with Steam running, that a held
Guide flips the pad to ALL before Y (InputPlumber behaviour, spike 4), YouTube TV's and
Crunchyroll's reaction to the keys (YouTube TV may want `back_key = "backspace"`), and the
input method on hardware.

## System service

`hinteros-system` (`crates/hinteros-system/src/`) runs as root and serves system API 0.2
(`schema/system-api.json`, design in `hinteros-docs/design/system-api.md`). Any process of a
`hinteros` user can connect; the session daemon relays it to the launcher but gets no extra
trust.

- **Methods:** read-only `system.info`, `settings.get`, `generations.list`, `jobs.get`,
  `jobs.list`; mutating `settings.set {revision, changes}` (closed, typed `changes`; empty in
  0.2), `ssh.set {enabled}`, `rebuild`, `hardware.redetect`, `rollback {generation?}`,
  `update.check`, `update.apply {rev}`, `channel.set {channel, acknowledge_downgrade?}`,
  `base.set {base, acknowledge_downgrade?}`, `jobs.cancel {job_id}`. Events `job_started`,
  `job_progress`, `job_finished {…, session_restart_required}` after `subscribe`.
- **Authorization** (`peer.rs`, `authz.rs`): uid/gid/pid from `SO_PEERCRED` and a pidfd from
  `SO_PEERPIDFD` at accept. Mutating methods need: the pidfd (and the process alive), the caller
  owning seat0's active logind session (or root), and polkit `CheckAuthorization` saying yes for
  the method's action, with a `unix-process` subject carrying the pidfd, pid, start time and uid
  and no user interaction. A polkit challenge (`auth_admin`) is `authentication_required`
  (3012), anything else `permission_denied` (3002, `reason: peer|seat|polkit`). Before that:
  strict params (every params object is closed; unknown fields are `invalid_params` with
  `data: {field, reason}`), a token bucket per uid (10/min, burst 3, `rate_limited`). Limits: 8
  connections per uid, 5 s to `hello`, 64 KiB per message.
- **Audit** (`audit.rs`): every mutating request and denial, and every job outcome, goes to the
  journal's native socket with `HINTEROS_AUDIT=1`, `HINTEROS_UID`, `_PID`, `_EXE`, `_METHOD`,
  `_ACTION`, `_DECISION` (`allowed|denied_seat|denied_polkit|denied_peer|auth_required|invalid|
  rate_limited`), `_JOB_ID`, `_PARAMS` (summary), and for jobs `_OUTCOME`, `_REV_BEFORE/AFTER`,
  `_GENERATION_BEFORE/AFTER`. Denials are capped at 20 per minute and uid, then summarised.
- **State** (`state.rs`): `state.json` is read into a typed `State` mirroring `state.nix`
  (`schemaVersion`, `base`, `settings.ssh.{enable, authorizedKeys, allowAdmin}`, `apps`,
  `profiles`; anything else refused) and written canonically from it, never from client JSON.
  `revision` is the SHA-256 of the file. `apps` (M8) and `profiles` (M7) round-trip untouched,
  ready for `apps.install/remove` and `profiles.*` to edit the same struct.
- **Jobs** (`job.rs`, `jobs.rs`, `runner.rs`): one at a time (`job_busy`). The service writes a
  root-only spec to `/run/hinteros/jobs/<id>/spec.json` and starts `hinteros-job@<id>.service`
  (systemd `StartUnit` over D-Bus), which runs `hinteros-system job <id>`; a switch that
  restarts the service doesn't touch it, and a restarted service re-attaches to it. The job
  reports through `progress.ndjson` (stages; nix `internal-json` turned into paths/bytes and a
  download percent) and `status.json`; the service turns them into events (at most 4/s) and
  keeps the last 10 finished jobs. Apply flow: memory (2 GiB) and disk (4 GiB on the store's
  filesystem) preflight → copy `/etc/hinteros` to `/var/lib/hinteros/staging` → edit (state,
  flake input, lock, facter) → `nix build …#nixosConfigurations.hinteros.config.system.build.toplevel`
  → `nix-env -p <profile> --set` + `switch-to-configuration switch` → replace `/etc/hinteros`
  with the staging copy (`renameat2(RENAME_EXCHANGE)`). A failed switch puts the profile back
  and runs the old system's `switch-to-configuration boot`. `jobs.cancel` works until the job
  enters `activating` (it then kills the job's process group; the box is unchanged). Jobs hold a
  logind `shutdown:sleep` inhibitor: `delay`, then `block` while activating.
  `session_restart_required` is set when the new system's `sw/bin/hinteros-session` or
  `hinteros-system` resolves to a different store path.
- **Updates** (`update.rs`, `flake.rs`): the channel is the `ref` of the locked `hinteros`
  input (`/etc/hinteros/flake.lock`); the repo URL and branches come only from the running
  generation's `channels.json`. `update.check` runs `git ls-remote`, fetches the branch into
  `/var/lib/hinteros/git`, verifies the tip with `git verify-commit` (SSH signatures,
  `gpg.ssh.allowedSignersFile` = the running generation's `allowed_signers`, i.e.
  `ssh-keygen -Y verify`), requires it to descend from the locked revision, and reports the
  highest `Security: low|medium|high|critical` trailer among the new commits (unknown values
  count as `high`). git runs with no config files, no prompts, and only the channel's transport
  (`https`, or `file` for tests). Results are cached for 60 s; failures are
  `update_unreachable {host}` or `update_verification_failed {reason}` in `job_finished`.
  `update.apply {rev}` only accepts the last verified result for the current channel and
  revision, re-verifies, and locks `?ref=<branch>&rev=<rev>` with
  `nix flake lock --override-input`. `channel.set` rewrites `flake.nix`'s input URL (it must
  appear exactly once, as `"git+<repo>?ref=<branch>"`) and locks the verified tip of the new
  branch; beta → stable, or a tip that isn't a fast-forward, needs `acknowledge_downgrade`.
  `base.set` edits `state.json` (unstable → stable needs the acknowledgement).
- **Rollback**: `nix-env --switch-generation N` + `switch-to-configuration switch`, then
  `/etc/hinteros`'s `flake.nix`, `flake.lock` and `state.json` from the generation's snapshot
  (`$out/hinteros/`, as `hinteros-config-sync` does at boot).
- **`hardware.redetect`**: `nixos-facter -o staging/facter.json`, then the apply flow.

### Command line

All paths default to the box's; tests point them at a temporary root.

| Flag | Default |
|---|---|
| `--socket` (`HINTEROS_SYSTEM_SOCKET`) | `/run/hinteros/system.sock` |
| `--config-dir` | `/etc/hinteros` |
| `--state-dir` | `/var/lib/hinteros` (`staging/`, `git/`, `update-check.json`) |
| `--run-dir` (`HINTEROS_RUN_DIR`) | `/run/hinteros` (`jobs/`) |
| `--profile` | `/nix/var/nix/profiles/system` |
| `--current-system`, `--booted-system` | `/run/current-system`, `/run/booted-system` |
| `--policy-dir` | `<current-system>/sw/share/hinteros` (resolved per job, so the running generation's) |
| `--ssh-host-key-dir` | `/etc/ssh` |
| `--bus` (`HINTEROS_SYSTEM_BUS`) | the system bus (logind, polkit, systemd) |
| `--journal-socket` | `/run/systemd/journal/socket` |
| `--nix`, `--nix-env`, `--git`, `--ssh-keygen`, `--nixos-facter` | found on the job unit's `PATH` |
| `--min-memory-bytes`, `--min-disk-bytes`, `--store-dir` | 2 GiB, 4 GiB, `/nix/store` |

`hinteros-system job [--run-dir DIR] <id>` runs one job (the job unit's `ExecStart`).

### What the system module must provide

For `daemon.nix` (and the VM tests) in the system repo:

- **Group** `hinteros`, with the session user (later: every profile user) in it.
- **`hinteros-system.service`**: `ExecStart=hinteros-system`, `User=root`, `Group=hinteros`,
  `RuntimeDirectory=hinteros` with `RuntimeDirectoryMode=0755` and
  **`RuntimeDirectoryPreserve=yes`** (the job directories in `/run/hinteros/jobs` must survive a
  restart of the service during a switch), `UMask=0077` or `0007`. It reads `/etc/hinteros`,
  `/var/lib/hinteros/update-check.json`, `/proc`, the profile links and `/run/current-system`;
  writes only `/run/hinteros`. So: `ProtectSystem=strict`, `ReadWritePaths=/run/hinteros`,
  `ProtectHome=yes`, `PrivateTmp=yes`, `NoNewPrivileges=yes`,
  `RestrictAddressFamilies=AF_UNIX` (D-Bus, journal), `SystemCallFilter=@system-service`,
  `MemoryDenyWriteExecute=yes`, `CapabilityBoundingSet=CAP_SYS_PTRACE` (only so `HINTEROS_EXE`
  can read `/proc/<pid>/exe` of another user's process; with an empty set the field is `?`).
  `PATH`/`path` is irrelevant to it. `restartIfChanged` may stay on.
- **`hinteros-job@.service`** (template, root, network allowed, no `ProtectSystem`: it switches
  the system): `ExecStart=hinteros-system job %i`, `Type=exec`, `Nice=10`,
  `IOSchedulingClass=idle`, `KillMode=control-group`, `path` with `nix`, `git`, `openssh`
  (`ssh-keygen`), `nixos-facter`, `coreutils`, `SSL_CERT_FILE` for git over https, and
  **`restartIfChanged = false; stopIfChanged = false`** (`X-RestartIfChanged=false`) so the
  switch it performs never restarts the running instance. `StateDirectory=hinteros` (0700)
  for `/var/lib/hinteros` on either unit.
- **polkit** `share/polkit-1/actions/org.hinteros.system.policy` with the actions
  `org.hinteros.system.{settings,rebuild,update.check,update.apply,rollback,channel,jobs.cancel}`
  (defaults `no/no/no`) and `org.hinteros.system.ssh` (defaults `auth_admin` for
  any/inactive/active: the admin password). The rules file grants the first group to
  `hinteros` members and must **not** return YES for `org.hinteros.system.ssh`:
  ```js
  polkit.addRule(function (action, subject) {
    if (action.id.indexOf("org.hinteros.system.") === 0 &&
        action.id !== "org.hinteros.system.ssh" && subject.isInGroup("hinteros")) {
      return polkit.Result.YES;
    }
  });
  ```
  (`hardening.nix`'s `org.hinteros.system.ssh.admin` action is no longer used; the SSH toggle
  itself is `org.hinteros.system.ssh`.) `security.polkit.adminIdentities` decides whose password
  that is.
- **Files in every generation**, at `/run/current-system/sw/share/hinteros/` (a package in
  `environment.systemPackages`, `pathsToLink = ["/share/hinteros"]`):
  `release.json` = `{"version": "<HinterOS release>", "dev": <hinteros.dev.enable>}`;
  `update/channels.json` = `{"repo": "https://justsomeshed.com/JSwartz/HinterOS/hinteros.git",
  "branches": {"stable": "stable", "beta": "beta"}}`; `update/allowed_signers` (ssh-keygen
  format, `principal namespaces="git" ssh-ed25519 AAAA…`, primary and offline backup key).
- **Both daemon packages in `environment.systemPackages`** (so `sw/bin/hinteros-session` and
  `sw/bin/hinteros-system` exist for `session_restart_required`).
- **Paths:** `/etc/hinteros` (root, 0755) with `flake.nix` from the template, whose input line
  is exactly `inputs.hinteros.url = "git+<repo>?ref=<branch>";` with `<repo>` as in
  `channels.json`; `/var/lib/hinteros` (root, 0700); the config snapshot at `$out/hinteros/`
  (`device-config.nix`, as today).
- **sshd** for `ssh.set`: `state.nix` maps `settings.ssh.enable`; login is the admin account
  with its password (the service doesn't touch sshd settings).
- Power stays session-side (logind); nothing here.

VM tests to add there: `system-escalation` and `system-authz` (direct socket access as the
session user with hostile input, a non-member, a non-seat user, a rules file returning NO,
`pkcheck` per action), `system-jobs` (a rebuild that changes `hinteros-system`'s own unit;
cancel; `job_busy`; inhibitor), `update-verify` (git server + cache VMs). The daemon-side
behaviour is covered by `crates/hinteros-system/tests/` against fakes.

### What the launcher must do (session API 0.6)

Move to session API 0.6 (the `system.*` methods are relayed unchanged): subscribe to
`job_started`/`job_progress`/`job_finished` and drive progress UI from them (re-query
`system.jobs.list` after reconnecting); treat `system_unavailable` (`down`/`version_mismatch`)
as "settings unavailable" rather than an error; on `authentication_required` from
`system.ssh.set` explain that the admin password is needed (the prompt comes with a polkit
agent later); confirm `needs_acknowledgement` (channel/base downgrades) and retry with
`acknowledge_downgrade: true`; show `system.update.check`'s cached `result` or wait for the job;
when `available.security` is non-null show the full-screen security notice with the severity
(not dismissable for 5 s); pass `settings.get`'s `revision` to `settings.set` and re-read on
`state_conflict`; offer a session restart on `session_restart_required`; show `dev` as a DEV
badge and the SSH host key fingerprints in Settings → System.

### What the launcher must do (session API 0.7)

Move to session API 0.7 (additive: the 0.6 methods and events are unchanged). The new methods map
onto the launcher's `lib/src/system` repository interfaces:

| Interface | Calls |
|---|---|
| `NetworkRepository.status / scan / connect / disconnect / forget / setWifiEnabled` | `network.status`, `wifi.scan` (a fresh scan, strongest first), `wifi.connect`, `wifi.disconnect`, `wifi.forget`, `wifi.enable`; `wrong_password` (4003) is `wrongPassword`, `device_failed` (4002) is `deviceFailed`; `WifiNetwork.saved` is `known`, `secured` is `security != open`, `signal` is 0 to 100 |
| `BluetoothRepository.devices / current / scanning` | `bluetooth.devices`, `bluetooth_changed` (`{status, devices}`), `bluetooth.status`; `startScan`/`stopScan` are `bluetooth.scan`; `pair`/`connect`/`disconnect`/`forget` take the device's `address` (`BtDevice.id`); `kind: gamepad` is `controller`; show `bluetooth_confirm {passkey}` and answer with `bluetooth.confirm` |
| `DisplayAudioRepository.displayModes / setDisplayMode` | `display.modes` (`preferred`, `current`; refresh in whole Hz), `display.set_mode`, then the 15 s dialog: `display.keep` or `display.revert`; check `can_set` first |
| `DisplayAudioRepository.audio / setAudioOutput / setVolume / setMuted` | `audio.outputs` (`AudioState`: outputs with `default` for `current`, `volume` 0 to 100, `muted`), `audio.set_output`, `audio.volume`, `audio.mute`; `audio_changed` |
| `SystemRepository.power` | `power.suspend` (sleep), `power.reboot` (restart), `power.poweroff` (shutdown); `power.capabilities` to hide what is not offered; `power_inhibited` (4005) while an update switches over; `unavailable` (4001) is `SystemErrorKind.unavailable` |

## Development

```sh
nix develop          # Rust toolchain, rust-analyzer, cargo-watch; installs git hooks
cargo watch -x test  # inner loop (gamescope/InputPlumber/system tests need Xvfb, dbus-daemon, git, ssh-keygen, else skip)
HINTEROS_TEST_UINPUT=1 cargo test uinput  # also create a real uinput device (VMs; grabbed before any key)
nix flake check      # build, tests (with Xvfb and dbus-daemon), clippy, rustfmt, nixfmt/statix/deadnix
nix build            # both binaries
```

Try it by hand: run
`cargo run -p hinteros-session -- --socket /tmp/s.sock --no-gamescope --no-inputplumber`,
connect with `socat - UNIX-CONNECT:/tmp/s.sock`, send
`{"jsonrpc":"2.0","id":1,"method":"hello","params":{"api_version":"0.7","client":{"name":"me","version":"0"}}}`,
and `kill -USR1` the daemon to emit `home_pressed` (after `subscribe`). Launch something with
`{"jsonrpc":"2.0","id":2,"method":"apps.launch","params":{"command":["xterm"]}}`.

The systemd units live in the system repo, which consumes `overlays.default`.

Part of [HinterOS](https://justsomeshed.com/JSwartz/HinterOS/hinteros). Licensed under GPL-3.0-only, see [LICENSE](LICENSE).
