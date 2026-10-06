# HinterOS Daemon

The Rust system services behind the launcher, built as two binaries:

- **`hinteros-session`** (unprivileged, runs in the user's session): controller input and the
  global Home button, launching and focusing apps in gamescope, typing into apps and the
  on-screen keyboard. Serves the launcher on `$XDG_RUNTIME_DIR/hinteros/session.sock`.
- **`hinteros-system`** (root, small, narrow API): applies settings from `state.json`, rebuilds,
  updates, rollback and power. Listens on `/run/hinteros/system.sock`, used only by the session
  daemon, and authenticates callers (`SO_PEERCRED` + polkit).

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

## Development

```sh
nix develop          # Rust toolchain, rust-analyzer, cargo-watch; installs git hooks
cargo watch -x test  # inner loop (gamescope/InputPlumber tests need Xvfb and dbus-daemon, else skip)
HINTEROS_TEST_UINPUT=1 cargo test uinput  # also create a real uinput device (VMs; grabbed before any key)
nix flake check      # build, tests (with Xvfb and dbus-daemon), clippy, rustfmt, nixfmt/statix/deadnix
nix build            # both binaries
```

Try it by hand: run
`cargo run -p hinteros-session -- --socket /tmp/s.sock --no-gamescope --no-inputplumber`,
connect with `socat - UNIX-CONNECT:/tmp/s.sock`, send
`{"jsonrpc":"2.0","id":1,"method":"hello","params":{"api_version":"0.5","client":{"name":"me","version":"0"}}}`,
and `kill -USR1` the daemon to emit `home_pressed` (after `subscribe`). Launch something with
`{"jsonrpc":"2.0","id":2,"method":"apps.launch","params":{"command":["xterm"]}}`.

The systemd units live in the system repo, which consumes `overlays.default`.

Part of [HinterOS](https://justsomeshed.com/JSwartz/HinterOS/hinteros). Licensed under GPL-3.0-only, see [LICENSE](LICENSE).
