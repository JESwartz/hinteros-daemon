# HinterOS Daemon

The Rust system services behind the launcher, built as two binaries:

- **`hinteros-session`** (unprivileged, runs in the user's session): controller input and the
  global Home button, launching and focusing apps in gamescope. Serves the launcher on
  `$XDG_RUNTIME_DIR/hinteros/session.sock`.
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
  `input_nav`, with key repeat for held directions) and PASS while an app has focus (the pad
  goes to Steam/the game; Guide still comes to us). Guide sends `home_pressed` and brings the
  launcher back. Re-applies everything when InputPlumber restarts or a pad appears. Without
  InputPlumber it logs and carries on.
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

[[apps]]                       # what apps.list returns after the built-in "steam" (a stub list if none)
id = "jellyfin"
name = "Jellyfin"
command = ["jellyfin-desktop"]

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

On a dev desktop run with `--no-gamescope --no-inputplumber` (otherwise it would grab your
controllers through a running InputPlumber).

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

## Development

```sh
nix develop          # Rust toolchain, rust-analyzer, cargo-watch; installs git hooks
cargo watch -x test  # inner loop (gamescope/InputPlumber tests need Xvfb and dbus-daemon, else skip)
nix flake check      # build, tests (with Xvfb and dbus-daemon), clippy, rustfmt, nixfmt/statix/deadnix
nix build            # both binaries
```

Try it by hand: run
`cargo run -p hinteros-session -- --socket /tmp/s.sock --no-gamescope --no-inputplumber`,
connect with `socat - UNIX-CONNECT:/tmp/s.sock`, send
`{"jsonrpc":"2.0","id":1,"method":"hello","params":{"api_version":"0.3","client":{"name":"me","version":"0"}}}`,
and `kill -USR1` the daemon to emit `home_pressed` (after `subscribe`). Launch something with
`{"jsonrpc":"2.0","id":2,"method":"apps.launch","params":{"command":["xterm"]}}`.

The systemd units live in the system repo, which consumes `overlays.default`.

Part of [HinterOS](https://justsomeshed.com/JSwartz/HinterOS/hinteros). Licensed under GPL-3.0-only, see [LICENSE](LICENSE).
