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
| `firefox-extension/` | The on-screen keyboard's Firefox extension (text field focus) and its native-messaging host manifest template |

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
  launcher back (on release, since Guide also starts the on-screen keyboard chord), except for
  a long press with Steam in front (below). Re-applies everything when InputPlumber restarts or a pad appears. Without
  InputPlumber it logs and carries on. `--dev-gamepad` reads
  pads directly instead, for desktop previews only (see "Desktop preview").
  Latency: every InputPlumber method and property read is a polkit check, so the signal loop
  never makes a D-Bus call per event. The pad behind a D-Bus target (for the Xbox label swap
  and clicks) comes from the composite device's `DbusDevices` property, read once per target
  and cached (also when nobody claims it); proxies never cache properties (a caching proxy
  fetches all of them on first use).
  Robustness: a hung InputPlumber call never holds up input. Every call times out
  (`[inputplumber] call_timeout_ms`, 2000), and the signal loop makes none of them (only the
  cached `DbusDevices` lookups): each pad has a worker task that loads profiles, sets modes and
  sends mouse buttons. Each worker is a watchdog: every `watchdog_ms` (5000) it reads the pad's
  `InterceptMode` and `ProfileName` and re-applies the routing when they differ from what the
  daemon asked for, after a failed or timed-out call, and when a pad that stopped answering
  answers again (logged: "stopped answering", "answers again"). A pad whose object vanishes is
  dropped, a new one gets the current routing, and InputPlumber restarting sets everything up
  again. This is what broke on real hardware: InputPlumber 0.81 panics inside
  `CompositeDevice.SendEvent` (`blocking_send` within its tokio runtime,
  `composite_device/client.rs:463`) and never replies; the daemon used to await that reply in
  the signal loop, so after the first click in Firefox no event (Guide included) got through.
  HinterOS patches InputPlumber (system repo); the daemon no longer depends on it.
  InputPlumber turns the left stick into directions at one
  threshold (0.60) without hysteresis, so a direction re-pressed within
  `[input] direction_debounce_ms` (40) of its release is chatter and dropped with its release.
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
  Non-Steam games added to Steam are listed too, from every Steam user's
  `userdata/<user>/config/shortcuts.vdf` (binary VDF; hidden ones left out): their app id is the
  shortcut's (high bit set), always installed, size 0, last played from `LastPlayTime`, artwork
  from Steam's custom `config/grid/<appid>{p,,_hero,_logo}.{png,jpg}` and the chosen icon.
  `apps.launch` with such an app id asks Steam for the 64-bit game id (`appid << 32 |
  0x02000000`), the only form `steam://rungameid` accepts for a shortcut.
- **Steam games** (`shell.rs`): focus for a game is `[game, 769, launcher]`: the game as soon
  as it has a window, Steam's own windows until then (the "Processing Vulkan shaders"
  pre-launch window, launch errors, cloud sync), the launcher otherwise; InputPlumber is in
  PASS, so the pad works in those windows too (skip shader processing). A game counts as
  running while Steam's reaper for it (`reaper SteamLaunch AppId=<id>`) is alive: missing
  windows (a loading gap) or the launch timeout do not end it then, nor does the launch timeout
  while Steam shows windows of its own before the game's first. `apps.launch` of a game whose
  reaper is alive focuses it instead of asking Steam again (which would only say "Game already
  running"), and games running when the daemon starts are tracked again. A Steam window that
  appears over the launcher within 10 s of our own Steam request (launch, close, Big Picture)
  is Steam's answer to it: Steam gets focus and the pad until its windows close (or Home);
  later pop-ups keep the `steam_attention` banner.
- **Steam itself** (`shell.rs`, `steam/windows.rs`): `apps.list` starts with the built-in
  `steam` entry while Steam is installed: `steam.path` (a path, or a name looked up in `PATH`)
  must be an executable file. Without one the entry is left out, `apps.launch {id: "steam"}`
  answers `app_not_found` ("Steam is not installed"), Steam games answer `app_launch_failed`,
  and no window counts as Steam's for `steam_attention`. Every app-list reload (see `reload.rs`:
  config change, end of a system job, the 30 s poll) looks again and sends `apps_changed` when
  Steam appears or goes away. Launching it runs `steam steam://open/bigpicture` (or `steam -gamepadui` if no
  Steam client runs) and shows Steam's windows: they are gamescope app 769, so focus is
  `[769, launcher]`, with InputPlumber in PASS. Home returns to the launcher; `apps.close`
  asks Steam to exit (`steam -shutdown`, not just leave Big Picture, which would open its
  desktop window) and never kills Steam. Games Steam starts from its own library (a Steam
  game, a non-Steam shortcut) are tracked as `steam-<appid>` once their Steam reaper shows up
  (`apps.running`, so Home lists them under Continue; a shortcut is named from
  `userdata/*/config/shortcuts.vdf`), take focus when Steam was in front, and give it back to
  Steam when they end. A window under another id than the reaper's (the one the game's
  Steam API reports, e.g. 480 from a `steam_appid.txt`) is focused with it: `[game, window,
  769, launcher]`. While such a game runs, Steam's hidden window does not count as Steam gone.
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
  keyboard app falls back to the gamepad. Directions never queue: a held direction repeats
  from one timer (a new direction replaces it, a release stops it at once), direction taps go
  to the key worker as navigation jobs that a newer one supersedes and that are dropped after
  150 ms (typed text keeps its full queue), and stick chatter is filtered (below).
- **Right-stick pointer** (`input/pointer.rs`, `[pointer]`, per app `pointer = false`): in a
  keyboard-profile app the right stick moves a mouse pointer, the left stick and D-pad stay
  arrow keys. InputPlumber does the motion: the daemon loads a device profile
  (`LoadProfileFromYaml`; InputPlumber's default mappings plus `RightStick → Mouse:Motion`,
  `speed_pps`, linear with InputPlumber's fixed 0.20 deadzone) and uses intercept mode
  GAMEPAD_ONLY (3), where the profile's mouse events still reach InputPlumber's virtual mouse
  while every gamepad event comes to the daemon over D-Bus (ALL would swallow them). The D-Bus
  target never reports the right stick, so the profile also maps any deflection beyond
  `activity_deadzone` to the unused `QuickAccess2` button, which arrives as `ui_quick2`: "the
  right stick moved". **RT is the left click and LT the right click** whenever the app has a
  pointer (unless the keymap maps `trigger_right` / `trigger_left`; press and release follow the
  trigger, so dragging works).
  **A never clicks**: it is Enter, or shows the on-screen keyboard on a focused text field.
  Clicks go through `CompositeDevice.SendEvent("Mouse:Button:Left"` (or `Right`)`, b)` (in the pad's worker,
  see "Robustness" above). The pointer **never hides on inactivity**: the D-pad or left stick
  (arrow keys) hides it at once (`hide_command`, default `gamescopectl paint_cursor_plane 0`),
  the right stick, RT or LT shows it again (`show_command`); the system runs gamescope with a
  `--hide-cursor-delay` too long to matter, so going Home from a pointer app hides it too (no
  stray pointer on the launcher) and the next app (a game: gamescope's own rules, for a real
  mouse) shows it again. Launcher and games alone never touch it. Leaving the app puts
  the profile that was loaded before back (or InputPlumber's default) before the next mode, so
  games never get a mouse on the right stick. Profiles and modes are re-applied when
  InputPlumber restarts, a pad appears, or the watchdog finds them changed. After a restart
  InputPlumber's bus name appears before its Manager object, so setting `ManageAllDevices` is
  retried (250 ms, doubling up to 5 s) until it works, and checked again every watchdog period.
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
  Guide *release*; `summon = "none"` puts it back on the press (unless Steam is in front, see
  below). The keyboard button (`ui_osk`, on pads that have one) also toggles it in
  keyboard-profile apps.
  Long press for Steam: Guide held for 500 ms (`GUIDE_LONG_PRESS` in `shell.rs`) while Steam
  (Big Picture) or a Steam game (launched by us or by Steam) has focus with the pad (PASS, no
  on-screen keyboard up) and Steam runs gives Steam one Guide tap instead of Home: its in-game
  menu over a game, its main menu in Big Picture, and from there Steam's controller settings.
  It fires at the threshold, while Guide is still held. A shorter press is Home (on release);
  a long press anywhere else (the launcher, a non-Steam app, Steam not running) is Home as
  before. A chord before the threshold (Guide+Y for the keyboard, or any other button) is
  never Steam's: Guide+Y is the keyboard, any other button makes the release Home. The tap:
  InputPlumber switched the pad to ALL on the Guide press, so the pad's worker first sets the
  focused app's mode (PASS) back, then `CompositeDevice.SendEvent("Gamepad:Button:Guide")`
  true, 80 ms, false. `SendEvent` writes straight to the virtual pad, past PASS's Guide
  intercept (which only sees the physical pad's events), but in ALL or GAMEPAD_ONLY it would
  go to D-Bus and come back to us as `ui_guide`, so the tap is only ever sent in PASS/NONE.
  Guide events within 750 ms of the tap are ignored anyway. The user's own Guide release then
  goes to the virtual pad too (a stray release for Steam, harmless), not to us.
  Xbox pads (xpad): InputPlumber maps positionally, so physical Y arrives as `ui_action` and X as
  `ui_context`; the daemon swaps them per composite device (`[input] xbox_label_swap`, detection
  by the xpad driver of the sources, else the "Microsoft X-Box ..." config name; Bluetooth Xbox
  pads, PlayStation, Switch are left alone), so Y is always the chord button.
  **B closes it**, whatever has focus (launcher, game or app): while it is visible the daemon
  takes B itself (`osk_visibility false`, the keyboard client never sees B, nor its release),
  and the previous mode comes back. The keyboard client owns the other buttons (X Backspace,
  Y Shift, ...).
  **Text fields** (`textfocus.rs`, `firefox-extension/`): the Firefox extension reports which
  text or password field has focus, per keyboard-profile app. Nothing pops up on focus by
  itself (arrow-key navigation, a page focusing a field on load): with
  `[osk] field_summon = "accept"` (the default) **A on a focused field shows the keyboard**
  instead of sending Enter (A's release is dropped too), and **clicking into a field with the
  pointer (RT; not LT) shows it**: the reporter marks a click that lands on the focused field
  (`clicked`, also when the field had focus already), and focus that arrives within 600 ms
  of the daemon's own left click in that app counts as clicked. `auto_show = true` (off by
  default) shows it as soon as a field gets focus. Either way a keyboard shown for a field hides
  250 ms after the field loses focus (no flicker between fields), unless it was shown or
  hidden by hand meanwhile. See "On-screen keyboard for Firefox text fields" below.
  `osk_visibility` carries `password: true` (session API 0.10) while the keyboard is up for a
  password field, so the keyboard masks its preview only then (and again, still visible, when a
  field of the other kind gets focus; a field losing focus changes nothing).
  **Address bar**: WebExtensions cannot see Firefox's own URL bar, so Firefox's autoConfig
  (hinteros `nixos/modules/firefox-osk-chrome.js`) reports it, and its other toolbar fields,
  straight to `osk.sock` from Firefox's main process: clicking it (RT) or A on it works like any
  text field. Each connection to `osk.sock` keeps its own state; a field has focus while any of
  them says so. An app with `address_bar = true` (plain Firefox with a toolbar, not a kiosk)
  also maps View (`ui_select`) to Ctrl+L and then shows the keyboard (unless `[apps.keymap]
  view` says otherwise), and LB / RB to history back / forward (Alt+Left / Alt+Right, unless
  the keymap maps `page_left` / `page_right`).
  Every chord step is logged at info level with the raw `ui_*` name of the other button
  (`chord guide+context → on-screen keyboard button=ui_context`, `guide released alone → Home`,
  `guide+secondary → ignored (not a chord) button=ui_action`), so a pad whose buttons arrive
  under unexpected names shows in the journal.
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
- **Admin password** (`auth/`): a polkit authentication agent for this process; prompts go to
  the launcher (`authentication_requested`, `auth.respond`, `auth.cancel`). Session API 0.8, 0.9, 0.10; see
  "Admin password, session restart and Store" below.
- **Session restart** (`restart.rs`): `session.restart` ends the greetd session through logind
  (greetd starts it again) and restarts the daemon's own user unit.
- **Store** (`store.rs`): `store.list` merges the catalog index with the installed apps;
  `store_changed`; `system.apps.install`/`.remove` are relayed. Catalog icons are resolved like app
  icons (`icon.path`, 0.9). **Reload** (`reload.rs`): the app list follows the config file and
  the catalog without a restart; see "Store icons and the app list without a restart".
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
shutdown_grace_ms = 3000         # SIGTERM of the daemon: SIGKILL its apps after this

[gamescope]
enable = true
display = ":0"                 # gamescope's Xwayland [default: $DISPLAY, else :0]
wayland_display = "gamescope-0"  # given to launched apps [default: $WAYLAND_DISPLAY, else gamescope-0]
focus_loss_debounce_ms = 1500  # how long the focused app may be without a window
unmap_gl_dummies = true     # unmap invisible full-screen override-redirect windows (GTK3 GL dummies) over an app

[inputplumber]
enable = true
bus = "unix:path=/run/dbus/system_bus_socket"  # default: the system bus
call_timeout_ms = 2000         # an InputPlumber call taking longer has failed (input never waits for one)
watchdog_ms = 5000             # check each pad's InterceptMode/ProfileName this often; re-apply if wrong

[input]
repeat_delay_ms = 400
repeat_interval_ms = 100
xbox_label_swap = "auto"       # "auto" | "on" | "off": Xbox (xpad) pads give Y as ui_action, X as ui_context
dev_gamepad = false            # development only, see "Desktop preview"
direction_debounce_ms = 40     # a direction re-pressed this soon after its release is stick chatter (0: off)

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
field_summon = "accept"        # A on a focused text field (Firefox extension) shows it, no Enter; or "none"
auto_show = false              # show it as soon as a text field gets focus in a keyboard-profile app

[pointer]                      # right stick = mouse pointer in keyboard-profile apps
enable = true
speed_pps = 1000               # pixels per second at full deflection (InputPlumber profile)
activity_deadzone = 0.25       # right stick movement that counts as using the pointer
# idle_ms                      # ignored, still accepted (older configs): A never clicks, no idle hiding
hide_command = ["gamescopectl", "paint_cursor_plane", "0"]  # D-pad/left stick used: hide at once
show_command = ["gamescopectl", "paint_cursor_plane", "1"]  # right stick moved again

[system]
enable = true                  # relay system.* to the system service
socket = "/run/hinteros/system.sock"

[auth]
enable = true                  # polkit authentication agent for this process
# bus = "unix:path=/run/dbus/system_bus_socket"   # polkit's bus [default: the system bus]
helper = "/run/wrappers/bin/polkit-agent-helper-1"   # setuid helper (fallback)
helper_socket = "/run/polkit/agent-helper.socket"     # socket-activated helper (preferred if present)
prompt_timeout_s = 120         # an unanswered prompt is cancelled
max_attempts = 3               # wrong passwords per request

[session]
restart_services = ["greetd", "greetd-greeter"]   # PAM services session.restart may end
# unit = "hinteros-session.service"   # own user unit to restart [default: from /proc/self/cgroup]
# user_bus = "unix:path=/run/user/1000/bus"      # the user's systemd [default: the session bus]

[store]
index = "/run/current-system/sw/share/hinteros/catalog.json"

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
pointer = true                 # keyboard profile: right stick = mouse pointer [default: true]
address_bar = false            # plain Firefox (not kiosk): View = Ctrl+L + on-screen keyboard, LB/RB = back/forward

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
| `--no-auth-agent` | | `auth.enable = false` (desktop previews) |
| `--auth-bus <address>` | `HINTEROS_AUTH_BUS` | `auth.bus` (tests: a private bus with a fake polkit) |
| `--polkit-agent-helper <path>` | `HINTEROS_POLKIT_AGENT_HELPER` | `auth.helper` (tests: a fake) |
| `--polkit-agent-helper-socket <path>` | `HINTEROS_POLKIT_AGENT_HELPER_SOCKET` | `auth.helper_socket` |
| `--session-user-bus <address>` | `HINTEROS_SESSION_USER_BUS` | `session.user_bus` |
| `--catalog-index <path>` | `HINTEROS_CATALOG_INDEX` | `store.index` |
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
- A `steam` binary on the daemon's `PATH`, or `steam.path`, for the built-in `steam` app (left
  out of `apps.list` while there is none).
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

### On-screen keyboard for Firefox text fields

The chain (design: `hinteros-docs/spikes/05-on-screen-keyboard.md`, "auto-summon"):

1. The extension `hinteros-osk@hinteros` (`firefox-extension/`, manifest v2, which Firefox keeps
   supporting) has a content script in every frame that follows `focusin`/`focusout` and
   reports `{focus: "text"|"password"|"none"}` (inputs that take text, textareas,
   contenteditable, open shadow roots) when it changes, and `{focus, clicked: true}` on a
   trusted primary-button click that lands on the focused field (even if nothing changed); the
   background script combines frames and passes the result on with
   `runtime.connectNative("hinteros_osk")` (`clicked` only for the active tab). `clicked` is
   optional: without it a report means not clicked, and older daemons ignore it.
2. Firefox starts the native-messaging host `hinteros-osk-native-host` (a symlink to
   `hinteros-session`; `hinteros-session osk-native-host` is the same), which reads Firefox's
   length-prefixed JSON on stdin, answers `{ok}` on stdout, and forwards each message as one line
   to the daemon's internal socket `osk.sock` (next to `session.sock`;
   `$HINTEROS_OSK_SOCKET`, which the daemon sets for the apps it starts, else
   `$XDG_RUNTIME_DIR/hinteros/osk.sock`). It reconnects when the daemon restarts.
3. The daemon attributes a report to the keyboard-profile app whose process tree the host runs
   in (an unknown pid counts for the focused app) and remembers it per app; A on a focused field
   in the focused app shows the keyboard (`field_summon`), or the report itself does when it
   is `clicked` or comes within 600 ms of the daemon's left click (RT) in that app, or always
   with `auto_show`. Reports from other processes are ignored.
. Firefox's own fields (address bar, find bar, fields of parent-process `about:` pages), which
   content scripts cannot see, are reported by privileged autoConfig code (hinteros
   `nixos/modules/firefox-osk-chrome.js`, appended to `mozilla.cfg` with
   `general.config.sandbox_enabled = false`): it follows focus in every browser window and writes
   the same lines (`client` `hinteros-firefox-chrome`) to `osk.sock` on a connection of its own,
   from Firefox's main process (the app's process tree). The daemon keeps each connection's
   state apart: a field has focus while any connection says so, and a closed connection clears
   only its own.

**What the system repo must install** (Nix packages from this flake: `hinteros-session` and
`hinteros-osk-firefox-extension`, also in `overlays.default`):

- The native-messaging host manifest: `hinteros-session` ships it as
  `lib/mozilla/native-messaging-hosts/hinteros_osk.json` (its `path` is the package's
  `bin/hinteros-osk-native-host`). With the NixOS Firefox module:
  `programs.firefox.nativeMessagingHosts.packages = [ pkgs.hinteros-session ];` (the wrapper links
  it into Firefox's own `lib/mozilla/native-messaging-hosts/`). Without the module, link that
  file into `~/.mozilla/native-messaging-hosts/` of the session user (or a wrapped Firefox's
  `lib/mozilla/native-messaging-hosts/`). The kiosk profiles need nothing else for it.
- The extension, force-installed through enterprise policies (`programs.firefox.policies`):

  ```nix
  ExtensionSettings."hinteros-osk@hinteros" = {
    installation_mode = "force_installed";
    install_url = "file://${pkgs.hinteros-osk-firefox-extension}/share/hinteros/firefox/hinteros-osk@hinteros.xpi";
  };
  ```

  (or `Extensions.Install = [ "<that .xpi path>" ]` plus `Extensions.Locked`). It needs the
  `nativeMessaging` permission and content scripts on all sites, granted at install; in private
  windows only if `private_browsing = true` is added to that entry, and on sites Mozilla
  quarantines only with `extensions.quarantineIgnoredByUser.hinteros-osk` = true (the
  hinteros system sets both; Mozilla's hard-restricted domains such as addons.mozilla.org stay
  out of reach).
- **Signing**: the .xpi is unsigned. Release Firefox (nixpkgs `firefox`) refuses unsigned
  extensions even through policies. Options: Firefox ESR (`pkgs.firefox-esr`) with
  `lockPref("xpinstall.signatures.required", false);` (`programs.firefox.autoConfig`), which
  ESR, Developer Edition, Nightly and unbranded builds honour; a Firefox built with
  `enableAddonSigning = false` (a full Firefox build, no binary cache); or signing the .xpi as an
  unlisted (self-distributed) add-on on addons.mozilla.org (`web-ext sign --channel=unlisted`) and
  shipping the signed file instead.
- The kiosk apps keep `input = "keyboard"` in `session.toml` (only those get the text-field
  keyboard) and must be started by the daemon (so the host is in their process tree and gets
  `HINTEROS_OSK_SOCKET`). `[osk] field_summon = "none"` (and `auto_show = false`) turns it off;
  the socket is only served while one of them is on.

### Pointer and on-screen keyboard: what the system and launcher repos must do

System (`hinteros-system`):

- gamescope: a `--hide-cursor-delay` long enough never to matter (gamescope's default is
  10000 ms; 0 would hide it at once): the pointer must not fade on inactivity, only the daemon
  hides it (D-pad/left stick). Moving the pointer off-screen does not hide it (it is clamped).
  For a TV, a larger pointer: `--cursor-scale-height` (gamescope scales its 36 px base by
  output height / this, so 540 gives 72 px at 1080p) and `XCURSOR_SIZE` for the apps.
- `gamescopectl` on the daemon's `PATH` (it is part of the gamescope package) for
  `paint_cursor_plane`; or other `[pointer] hide_command`/`show_command`.
- InputPlumber: the daemon loads its profile over D-Bus (`LoadProfileFromYaml`,
  `GetProfileYaml`, `SendEvent`, `ProfileName`, `InterceptMode`: covered by the stock polkit
  rule for the `inputplumber` group); the pads' default targets already include InputPlumber's
  virtual mouse (`xb360` + `mouse` + `keyboard`). A system-wide `devices.d` override must keep
  the `mouse` target. InputPlumber 0.81 needs a patch for `SendEvent` (it panics and never
  answers, so RT clicks do nothing; the daemon times the call out and carries on).
- Firefox: the extension and native-messaging host as above (the text-field keyboard needs
  them); plain Firefox entries get `address_bar = true`, kiosk entries do not.

Launcher (`hinteros-osk`, the keyboard client): B never arrives while the keyboard is visible
(the daemon hides it); map X (`secondary`) to Backspace and Y (`context`) to Shift, and do not
queue or animate moves: apply each `input_nav` press/repeat immediately, drop moves still
pending when a newer one arrives (the daemon sends one press, repeats at the configured rate and
one release per direction, at once and never batched).

### Keyboard and on-screen keyboard: what is verified and what needs hardware

Verified here: the key planning and pacing (unit tests), the input method client against a
fake compositor speaking the protocol XML (wayland-server), the overlay atoms and opacity on an
Xvfb standing in for gamescope's Xwayland, keyboard-profile translation, `input_nav` routing
and the Guide chord against the fake InputPlumber with a fake keyboard (`--fake-keyboard`),
the long-press Guide tap for Steam (shell unit tests; against the fake InputPlumber, which
loops a `SendEvent` in ALL back as `ui_guide` the way 0.81 does: the tap sets PASS first and
nothing comes back),
and creating the real uinput device (`HINTEROS_TEST_UINPUT=1`). The automatic keyboard: host
framing and forwarding (unit tests), the focus state machine (shell unit tests with fakes), and
an app starting the installed-name host the way Firefox does, end to end
(`tests/textfocus.rs`); the extension itself is only syntax-checked (`nix flake check`), not yet
run in Firefox. Spike 5 proved in VMs that
uinput typing reaches Firefox, that `gamescope_input_method` types Unicode into it, and that a
`GAMESCOPE_EXTERNAL_OVERLAY` window toggled by opacity draws over Firefox without taking focus.

Needs real gamescope and a pad: the overlay's size and position at the output resolution, the
Flutter OSK's transparent window, Guide+Y reaching the daemon with Steam running, that Steam
opens its in-game / Big Picture menu on the injected Guide tap (and whether it wants a longer
tap than 80 ms), that a held
Guide flips the pad to ALL before Y (InputPlumber behaviour, spike 4), YouTube TV's and
Crunchyroll's reaction to the keys (YouTube TV may want `back_key = "backspace"`), and the
input method on hardware.

## System service

`hinteros-system` (`crates/hinteros-system/src/`) runs as root and serves system API 0.8 (0.8: the time zone, see "Time zone" below; 0.7: job progress and what a job is about, see "Job progress" below; 0.6: storage and clean-up, see "Storage and clean-up" below; 0.4: profiles and per-profile apps; 0.5: the admin PIN for
box-wide actions, `guest.end`, rollback re-adds box features; see "Profiles and per-profile apps" below)
(`schema/system-api.json`, design in `hinteros-docs/design/system-api.md`). Any process of a
`hinteros` user can connect; the session daemon relays it to the launcher but gets no extra
trust.

- **Methods:** read-only `system.info`, `settings.get`, `generations.list`, `jobs.get`,
  `jobs.list`; mutating `settings.set {revision, changes}` (closed, typed `changes`; empty in
  0.2), `ssh.set {enabled}`, `rebuild`, `hardware.redetect`, `rollback {generation?}`,
  `update.check`, `update.apply {rev}`, `channel.set {channel, acknowledge_downgrade?}`,
  `base.set {base, acknowledge_downgrade?}`, `apps.install {ids, revision, accepted_terms?}`,
  `apps.remove {ids, revision, delete_data}` (0.3, see "Store backend" below), `jobs.cancel {job_id}`. Events `job_started`,
  `job_progress`, `job_finished {…, session_restart_required}` after `subscribe`.
- **Authorization** (`peer.rs`, `authz.rs`): uid/gid/pid from `SO_PEERCRED` and a pidfd from
  `SO_PEERPIDFD` at accept. Mutating methods need: the pidfd (and the process alive), the caller
  owning seat0's active logind session (or root), and polkit `CheckAuthorization` saying yes for
  the method's action, with a `unix-process` subject carrying the pidfd, pid, start time and uid,
  without user interaction except for `org.hinteros.system.ssh` (0.3: see "Admin password" below).
  A polkit challenge (`auth_admin`) is `authentication_required`
  (3012), anything else `permission_denied` (3002, `reason: peer|seat|polkit`). Before that:
  strict params (every params object is closed; unknown fields are `invalid_params` with
  `data: {field, reason}`), a token bucket per uid (10/min, burst 3, `rate_limited`);
  `update.check` has a bucket of its own,
  so the launcher's background checks (startup, relayed with the session user's uid; the result
  is cached 60 s anyway) never use up the budget of the user's own actions. Mutating requests run
  as tasks, at most 4 in flight per connection, so a connection keeps serving while one waits for
  the admin password. `hello`'s params are closed like every other method's (both daemons). Limits: 8
  connections per uid, 5 s to `hello`, 64 KiB per message.
- **Audit** (`audit.rs`): every mutating request and denial, and every job outcome, goes to the
  journal's native socket with `HINTEROS_AUDIT=1`, `HINTEROS_UID`, `_PID`, `_EXE`, `_METHOD`,
  `_ACTION`, `_DECISION` (`allowed|denied_seat|denied_polkit|denied_peer|auth_required|invalid|
  rate_limited`), `_JOB_ID`, `_PARAMS` (summary), and for jobs `_OUTCOME`, `_REV_BEFORE/AFTER`,
  `_GENERATION_BEFORE/AFTER`. Denials are capped at 20 per minute and uid, then summarised.
- **State** (`state.rs`): `state.json` is read into a typed `State` mirroring `state.nix`
  (`schemaVersion`, `base`, `stateVersion`, `settings.{hostname, ssh.enable, admin.user, guest, cleanup}`, `disk.layout`, `apps`,
  `profiles`; anything else refused) and written canonically from it, never from client JSON.
  `revision` is the SHA-256 of the file. `apps` (M8) and `profiles` (M7) round-trip untouched,
  ready for `apps.install/remove` and `profiles.*` to edit the same struct.
- **Jobs** (`job.rs`, `jobs.rs`, `runner.rs`): one at a time (`job_busy`). The service writes a
  root-only spec to `/run/hinteros/jobs/<id>/spec.json` and starts `hinteros-job@<id>.service`
  (systemd `StartUnit` over D-Bus), which runs `hinteros-system job <id>`; a switch that
  restarts the service doesn't touch it, and a restarted service re-attaches to it. The job
  reports through `progress.ndjson` (stages; nix `internal-json` turned into paths, bytes,
  builds, the derivation building and a percent; see "Job progress" below) and `status.json`;
  the service turns them into events (at most 4/s, and a heartbeat at least every 2 s) and
  keeps the last 10 finished jobs. A request that a running job would refuse first takes in a
  result the job has just written, so a job that has ended is never `job_busy`. Apply flow:
  memory (2 GiB) and disk (4 GiB on the store's filesystem) preflight → copy `/etc/hinteros`
  to `/var/lib/hinteros/staging` → edit (state, flake input, lock,
  facter) → `nix build …#nixosConfigurations.hinteros.config.system.build.toplevel`
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
  count as `high`), the newest `Security-Summary:` trailer as `security_summary` (one line,
  control characters removed, at most 120 characters), and the tip's `release/version` as
  `version` (if it is a semantic version of at most 32 characters, else the 12-digit short rev).
  git runs with no config files, no prompts, and only the channel's transport
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
| `--catalog-index` | `<current-system>/sw/share/hinteros/catalog.json` (resolved per request) |
| `--auth-timeout-s` | 300 (how long an interactive polkit check waits for the admin password) |
| `--zoneinfo-dir` | `/etc/zoneinfo` (the system's tzdata: TZif files and `zone1970.tab`) |
| `--localtime` | `/etc/localtime` (the link `time_zone.set` replaces) |

`hinteros-system job [--run-dir DIR] <id>` runs one job (the job unit's `ExecStart`).

`hinteros-system check-state FILE...` parses each file with the same `State::parse` the service
uses, without starting the service (no root, sockets or D-Bus). It prints `ok <file>` or
`invalid <file>: <reason>` per file (an unknown field says "daemon and system repo are out of
sync"). Exit status: 0 if all files are valid, 1 if any is invalid, 2 for usage errors or a file
that cannot be read (that file is reported as `invalid <file>: reading: ...`). The system repo
runs it in a Nix check over every state.json the installer can write.

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
  `org.hinteros.system.{settings,rebuild,update.check,update.apply,rollback,channel,jobs.cancel,apps}`
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

## Admin password, session restart and Store (session API 0.8, system API 0.3)

### Admin password: the polkit authentication agent

`ssh.set` needs `org.hinteros.system.ssh`, which is `auth_admin` (the admin password). The flow:

1. The launcher calls `system.ssh.set {enabled: true}`; the session daemon relays it.
2. `hinteros-system` checks the action with `CheckAuthorization` **with `AllowUserInteraction`**
   (only the actions in `system::INTERACTIVE_ACTIONS`, today just this one) and a `unix-process`
   subject for its peer, the session daemon. The request waits (at most `--auth-timeout-s`, 300 s;
   then `CancelCheckAuthorization`), while the connection keeps serving other requests.
3. polkit finds the agent registered for that process: the session daemon (`auth/`) registers one
   at startup with `RegisterAuthenticationAgent`, subject `unix-process` with its own pidfd, pid,
   start time and uid (again whenever polkit restarts). A process-scoped agent works although the
   daemon is a user service outside the greetd session (threat model open question 4), and it
   only ever sees requests whose subject is the daemon, i.e. calls it relayed.
4. polkit calls `BeginAuthentication` (only accepted from the owner of
   `org.freedesktop.PolicyKit1`); the daemon sends `authentication_requested {cookie, action,
   message, identities}` to connections named `hinteros-launcher` only (non-root identities first;
   it authenticates as the first) and waits up to `[auth] prompt_timeout_s` (120).
5. The launcher answers `auth.respond {cookie, password}` (only a `hinteros-launcher` connection
   may; others get `not_authorized`). The daemon runs polkit's helper for the identity: the
   socket-activated helper (`/run/polkit/agent-helper.socket`, `polkit-agent-helper.socket`) when
   it exists, else the setuid `polkit-agent-helper-1` (`[auth] helper`), writes the cookie, answers
   the first PAM prompt with the password, and reads `SUCCESS`/`FAILURE` (protocol of polkit 127's
   `polkitagenthelper-pam.c`/`polkitagentsession.c`). `{ok: true}`: the helper told polkit,
   `BeginAuthentication` returns, polkit authorizes, `ssh.set` starts its job. `{ok: false,
   attempts_left}`: try again; after 3 the request fails. `auth.cancel`, polkit's
   `CancelAuthentication` and the timeout cancel it. `authentication_finished {cookie,
   outcome: authorized|failed|cancelled}` tells the launcher.
6. `ssh.set` answers: the job id; `authentication_required` (no agent, dismissed or timed out);
   `permission_denied {reason: polkit}` (3 wrong passwords).

The password is taken out of the request into a zeroizing buffer, the request line is wiped,
it is written only to the helper's stdin (or socket) and wiped; it is never logged (a test greps
the debug log). Responses to `system.*`, `auth.*`, `store.list` and `session.restart` may come out
of order on a connection (they run as tasks, so the launcher can send `auth.respond` while
`system.ssh.set` waits; relayed interactive calls wait up to 330 s).

Verified here: the agent against a fake polkit authority on a private bus that plays polkitd
(registration subject, `BeginAuthentication`/`CancelAuthentication`, a caller that is not
polkit, a polkit restart) with a fake helper script speaking the helper's protocol; the socket
helper protocol against a fake socket; the system service's interactive check (flags,
cancellation id, timeout and `CancelCheckAuthorization`, dismissal) against the fake polkit; the
relay not blocking. **Needs a real VM test** (system repo): real polkitd and PAM with the admin
password: that polkit matches the system service's pidfd subject to the agent registered by the
user service (hash of pid and start time), that the setuid helper works from the user unit (see
`NoNewPrivileges` below) or the helper socket is used, that a wrong password gives `{ok: false}`
and three give `permission_denied`, and that `ssh.set` then really enables sshd.

### Session restart

`session.restart` (after `job_finished {session_restart_required: true}` or `system_unavailable
{reason: version_mismatch}`): the daemon answers `{}`, then asks logind to terminate the user's
graphical session on seat0 (`User.Display`, else seat0's active session; it must be ours, on
seat0, and its PAM `Service` one of `[session] restart_services`, default `greetd` and
`greetd-greeter`, else `unavailable {service: logind, reason: not_greetd}`), so a desktop session
is never ended. greetd's `default_session = initial_session` starts the session again with the new
generation's gamescope and launcher. Then it asks the user's systemd (`[session] user_bus`, default
the session bus) to `RestartUnit` its own unit (`[session] unit`, default from
`/proc/self/cgroup`), so the new launcher meets the new daemon; apps the daemon started end with
its cgroup. Why this way: logind lets a user end their own session without polkit; killing
gamescope by pid would leave the logind session and the session scope behind; restarting greetd
or terminating the user needs root or ends every user service; restarting only the unit would
keep the old launcher. Verified against a fake logind and a fake user systemd; needs a VM test that
greetd really comes back and the daemon restarts with the new store path.

### Store backend

System API 0.3, `apps.install {ids, revision, accepted_terms?}` and `apps.remove {ids, revision,
delete_data}` (polkit `org.hinteros.system.apps`, seat check, rate limit, audit), in
`crates/hinteros-system/src/store.rs`:

- ids (1 to 8) are checked against the **running generation's** catalog index
  (`--catalog-index`, default `<current-system>/sw/share/hinteros/catalog.json`, index version 2,
  read by `hinteros_api::catalog`; never the caller's copy). Unknown id: `not_found`; retired or
  removed: `invalid_params {reason: not_available}`; already installed: skipped (`job_id: null`
  when nothing is left). `depends` are added transitively. A conflict either way with an installed
  or requested app: `app_conflict` (3013, `{id, conflicts_with}`; catalog.md's 3012 was taken by
  `authentication_required`). Every new app with vendor terms needs `accepted_terms[id] ==
  terms.version`, else `terms_required` (3014, `{id, version}`); terms for apps the request does not
  install are `invalid_params`. Stale `revision`: `state_conflict`. Free space: the new apps'
  `sizeMiB` on top of the rebuild margin (`insufficient_resources`).
- Accepted terms are audited (`HINTEROS_TERMS_ID`, `_TERMS_VERSION`, `_USER`, `_PROFILE`,
  `_JOB_ID`) and the job records them before building in `/var/lib/hinteros/terms-accepted.json`
  (0600, written atomically): `{"<id>": [{"version", "accepted_at" (RFC 3339 UTC), "by" (user name
  or "installer")}]}`, a list per id so re-acceptance keeps the history (the installer writes the
  same format).
- `apps.remove`: every id must be installed (`not_found`); an app an installed app depends on
  cannot go alone (`app_conflict`, naming the dependent). Removing works without a usable index
  (a broken generation), but not with `delete_data`.
- The job writes `state.json`'s `apps` from the typed state and runs the apply flow (job kinds
  `apps_install`, `apps_remove`). With `delete_data`, after a successful switch it deletes the
  removed apps' `dataPaths` (from the index, checked again: relative, no `.`/`..`, no wildcards)
  in every profile home: the caller's home plus the homes of `state.json`'s profile users (M7),
  from the user database, never root's. `datadel.rs` opens the home with `O_NOFOLLOW` (it must be a
  directory owned by the profile's uid), every component with `openat(O_NOFOLLOW|O_DIRECTORY)`,
  removes trees with `unlinkat` relative to directory descriptors, unlinks symlinks without
  following them, refuses an intermediate symlink and never enters another filesystem; each
  outcome is logged to the job's journal.
- `settings.get` lists the installed apps; `update.check`'s `available` gains `new_apps` and
  `removed_apps`: after verifying the tip, the job runs `nix eval --json --option
  allow-import-from-derivation false "<input url>&rev=<rev>#lib.catalogIds"` and compares with the
  running index (null when that fails; the update is still offered).

Session API 0.8: `store.list` merges the index (`[store] index`) with `system.settings.get`'s apps:
available apps plus installed ones whatever their status, by name, with `state`
(`available|installed|installing|removing|retired|unavailable`; installing/removing from apps jobs
relayed through this daemon), all catalog fields in snake_case, terms with the absolute path of the
text so the launcher shows it offline, and `more_after_update` from the last update check's
`new_apps`. `store_changed` after an apps job starts or finishes, any other switching job finishes,
or `more_after_update` changes. `unavailable {service: catalog}` without an index (or an unknown
index version); `system_unavailable` while the system service is down. `system.apps.install` and
`system.apps.remove` are relayed. Verified against fake roots and fakes; needs the system repo's
`store-install` VM test (real build, `apps.list` reload).

### Store icons and the app list without a restart (session API 0.9)

**Store icons.** Every `store.list` entry's `icon` object gains an optional `path`: the resolved
icon on this box, absent until there is one. The daemon resolves the catalog index's `icon` fields
like an app's: `desktopId` (when the app happens to be installed, through its desktop file and the
icon themes), else `iconUrl` or `siteUrl` downloaded into the same cache
(`$XDG_CACHE_HOME/hinteros/icons/<id>.<ext>`, https only, the same limits, `icons.refresh_days`
refresh, and the same 1 min / 10 min / 1 h / 6 h retries, in the same background thread). An
installed app's own `[[apps]]` entry wins for the same id. `store_changed` is sent when a catalog
icon arrives (and `apps_changed` when an app's does). `icons.fetch = false` turns downloads off
for both. The catalog is re-read on a new generation. Why 0.9: the schema's `icon` object is closed
(`additionalProperties: false`), and while the major version is 0 every minor is breaking for a
client built against the old schema; the new field is the only change to an existing object.

**Reloading the app list.** The daemon watches the config file named by `--config` (default
`$XDG_CONFIG_HOME/hinteros/session.toml`, also while it does not exist yet) and the catalog
index. NixOS switches by replacing symlinks, so inotify watches every directory along the path's
symlink chain (the file's, and each directory symlink such as `/etc/static`), not only the file.
A change, the end of any system job (`job_finished`: a switch is done) or the 30 s safety-net
check makes the daemon re-read the config (the same loading as at startup, command-line overrides
included) and compare. A changed `[[apps]]` list replaces the list `apps.list` and `apps.launch`
use, icons are re-resolved (desktop files of newly installed apps appear), and `apps_changed` is
sent. Only `[[apps]]` is reloaded; everything else in the file needs a restart. A file that does
not parse or validate is logged (once) and the old list stays.

Running apps keep running: an app that was removed from the list while it runs stays in
`apps.running`, can be focused and closed, and leaves when it exits; it just cannot be launched
again. Steam, Steam games and `apps.launch {command}` are unaffected.

### Shutdown

On SIGTERM or SIGINT the daemon ends the processes it started: every app's reaper (and so the app's
process group), the launcher and the on-screen keyboard get SIGTERM, then SIGKILL (descendants
first) after `close.shutdown_grace_ms` (3000), and it exits. Steam and the games it runs are not
the daemon's and are left alone. A `reaper` also sets `PR_SET_PDEATHSIG`: if the daemon dies without
cleaning up (SIGKILL, a crash), the kernel sends each reaper SIGTERM, which it forwards to the app's
process group, so no `reaper ... sleep 60` is left behind. Verified in `tests/shutdown.rs` (SIGTERM,
a TERM-ignoring app killed after the grace period, SIGKILL of the daemon) and `tests/reload.rs`
(reload with a symlink switch, broken file, running removed app; Store icons from a local HTTP
server, `--icon-allow-http`, a hidden test flag).

### What the system repo must provide (0.8 / 0.3)

- **polkit agent helper**: on NixOS the setuid wrapper `/run/wrappers/bin/polkit-agent-helper-1`
  (`security.polkit.enable` provides it; the daemon's default `[auth] helper`). **The session
  daemon's unit has `NoNewPrivileges=true`, which defeats setuid**: either enable polkit's
  socket-activated helper (`polkit-agent-helper.socket` from the polkit package, socket at
  `/run/polkit/agent-helper.socket`, which the daemon prefers; recommended, keeps
  `NoNewPrivileges`) or drop `NoNewPrivileges` from `hinteros-session.service`.
  `security.polkit.adminIdentities` names the admin account whose password is asked (polkit sends
  the identities; root is only used if nothing else is offered).
- **polkit actions**: `org.hinteros.system.apps` (defaults `no/no/no`, granted to `hinteros` by the
  existing rule; the rule must keep excluding `org.hinteros.system.ssh`, which stays `auth_admin`
  with a `message` the launcher shows, e.g. "Authentication is required to turn remote login (SSH)
  on or off"). The user must be allowed to call `RegisterAuthenticationAgent` (polkit allows any
  user to register for its own process).
- **Catalog index** at `/run/current-system/sw/share/hinteros/catalog.json` and the **terms texts**
  next to it in `share/hinteros/terms/` (the catalog's `packages.index` in `systemPackages`,
  `pathsToLink` `/share/hinteros`).
- **`lib.catalogIds`** in the system flake: `{ "<id>": { name, status, statusReason } }` (status
  `available|retired|removed`), evaluable without building anything.
- **`/var/lib/hinteros/terms-accepted.json`**: created by the job (the job unit already has
  `StateDirectory=hinteros`); the installer writes the same format.
- **greetd** with `default_session = initial_session` (as `session.nix` has it) and the session
  daemon as the user unit `hinteros-session.service` (found from its cgroup); the daemon needs the
  user's session bus (`DBUS_SESSION_BUS_ADDRESS`, set for user units) for `RestartUnit`.
- VM tests to add: `polkit-agent` (enable SSH through the launcher path with the admin password:
  right, wrong, cancelled), `session-restart`, `store-install` (catalog.md), and hostile `apps.*`
  input in `system-escalation` (`../x`, store paths, unknown ids, terms for other apps, a symlinked
  data path pointing out of the home).

### What the launcher must do (session API 0.11; 0.12 adds profiles, below)

Say hello with 0.12 (client name `hinteros-launcher`), match responses by id (they may come out of
order), and:

- Subscribe to `authentication_requested`/`authentication_finished`. On a request show the admin
  password dialog (`adminPasswordUi` on) with `message` and the first identity's name; send
  `auth.respond`; on `{ok: false}` say "wrong password" and keep the dialog while `attempts_left >
  0`; Back sends `auth.cancel`; close it on `authentication_finished`. Keep awaiting the
  `system.ssh.set` call meanwhile; `authentication_required` after a cancel means "not changed".
- Offer "Restart session" on `session_restart_required` and call `session.restart`; the launcher
  is ended right after the answer and comes back with the new session.
- Store: `store.list` (re-query on `store_changed` and after reconnecting); install with
  `system.apps.install {ids, revision (from system.settings.get), accepted_terms}` after showing
  each new app's (and dependency's) terms text from `terms.file` in full; `terms_required` means
  show the named terms; `app_conflict` names the other app; remove with `system.apps.remove {ids,
  revision, delete_data}` after closing the app (and confirming `data_paths` twice); progress from
  the job events; "More apps after the next update" when `more_after_update`.
- On-screen keyboard (0.10): mask the typed-text preview only while `osk_visibility.password` is
  true; show it plainly otherwise (also when no field was reported).
- Updates (0.11): show `available.version` ("HinterOS 0.6.1") on the update screen and banner
  (the short rev when it is absent), and `available.security_summary`, when non-null, under the
  severity on the security notice and the update screen.

## Profiles and per-profile apps (system API 0.5, session API 0.13; M7)

Design: `hinteros-docs/design/profiles.md` and `apps.md`. Cross-repo contracts (paths, file
formats, flags): `m7/DECISIONS.md` (D1–D17) while M7 is being built.

### Apps per profile (system service)

- **Schema 2** (`state.rs`): `state.json` without `apps`, with `requirements` (box features:
  `graphics32`, `steam-devices`), `services`, `profiles: [{id, user}]` (1–8; `p1` = `hinteros`,
  else `hinteros-p<N>`) and `settings.guest.enable`. Schema 1 keeps working unchanged
  (`apps.install/remove` rebuild the box as in 0.3).
- **Envs** (`appreg.rs`, `runner/apps.rs`): a profile's apps are the root-owned Nix profile
  `/nix/var/nix/profiles/hinteros/apps/<user>` (`--apps-profiles-dir`), the starter set's
  `…/starter`, the guest's a single link `/run/hinteros/apps/hinteros-guest` (an indirect GC root
  via `nix-store --add-root --indirect --realise`). The service publishes
  `/run/hinteros/apps/<user>` → the profile's Nix profile at start and after every job.
  `/run/hinteros/apps` is 0750 root:hinteros, not the 0755 of the M7 design (D4): every
  session account is in `hinteros`, and other users cannot list who has which apps. Whoever
  publishes (a job runs as root:root: the migration is the first to publish on a schema-1 box)
  sets that mode and the group of `/run/hinteros` (the service's runtime directory). An env
  is built by `config.hinteros.appEnv.build ids` of `/etc/hinteros` (`nix eval --impure --json
  --expr` over a list of id lists, all of a job's envs in one evaluation, then `nix build
  <drv>^out … --keep-going`) and set with `nix-env -p <profile> --set` (skipped when it is
  already the current env). Its manifest `share/hinteros/apps.json` (`hinteros_api::manifest`)
  is the truth; `/var/lib/hinteros/apps/registry.json` caches owners' ids and box features and is
  rewritten from the manifests at start.
- **Methods**: `apps.list` (`{revision, generation, ids, needs_terms, box}`), `apps.generations`,
  `apps.install`/`apps.remove` on the caller's own profile (the profile's `revision` from
  `apps.list`; `delete_data` only in the caller's home), `apps.rollback {generation}`,
  `apps.accept_terms {id, version}` (records `{version, accepted_at, by, profile}`). A spare
  account, root or the admin account gets `permission_denied {reason: profile}`; schema 1 gets
  `unavailable {service: profiles, reason: schema1}` for the new methods.
- **Box features**: an install or rollback whose apps need a feature that is off runs the `box`
  stage first (the apply flow with `requirements` = every needed feature plus the new ones).
  Every other job that rebuilds the system writes `requirements` = what the owners' envs need,
  so unneeded features go only then. Update, channel, base and rollback jobs end with an `apps`
  stage that rebuilds every owner's env against the new system (tombstoned ids dropped); an env
  that fails keeps its generation and is listed in `apps_not_updated`, the job still succeeds.
- **Rollback re-adds features** (0.5): after a rollback's `apps` stage, features the envs need that
  the restored generation lacks are turned back on in a `box` stage (the apply flow on the restored
  configuration with `requirements` = what the envs need; a new generation). The result lists
  them in `features_readded`; `generations.list` entries name them beforehand (`readds_features`).
- **v1 → v2** (`service/apps.rs` `maybe_migrate`, `runner/apps.rs` `migrate`): on a schema-1 box
  whose running generation's `release.json` says `"stateSchema": 2`, the service creates
  `profiles.json` (p1: the admin profile, name from schema 1 or "Profile 1", unlocked, no PIN)
  and queues `migrate_apps` once per boot (`/run/hinteros/migrate-attempted`): one env for the
  box-wide apps (tombstones dropped) for every account and the starter set, `starter.json`, the
  terms of migrated apps copied to the accounts, then `state.json` schema 2 through the apply
  flow. Idempotent (envs already set are not set again) and resumable; a failure leaves schema 1.

### Profiles, PINs, greeter (system service)

- `profiles.json` (`profiles.rs`), PINs (`pins.rs`: argon2id PHC strings in `pins/<id>`, backoff
  in `pins/<id>.state` under `flock`, 3 free failures then 1 s doubling to 5 min, the lock counted
  from when the hash check ended; **none for the admin profile's PIN**: wrong is only wrong, and a
  stale wait file of the admin is ignored and cleared at service start), the public view `/run/hinteros/profiles-public.json` (0644) and
  the switch hint with one-time tokens `/run/hinteros/switch.json` (0600) (`public.rs`),
  `runtime.json` → `/run/hinteros/gamescope-modes` and `audio-output`, `usage.json`
  (`hinteros-system usage`, started over D-Bus when `profiles.list` sees data older than an hour).
- Methods (`service/profiles.rs`): `profiles.list/create/setup_admin/rename/set_appearance/
  set_pin/set_locked/reset_pin/remove/switch/switch_cancel/unlock`, `guest.set`, `starter_set.get/set`,
  `display.save_mode`, `audio.save_output`; event `profiles_changed`. Which PIN each checks:
  own changes `self`, anything on another profile or the box `admin`; a profile without a PIN yet
  passes. `pin_required` (3016) names it, with `retry_after_ms` during the lock; the audit has
  `HINTEROS_PIN_CHECK`/`_PIN_OUTCOME` and every PIN field is `***` in `HINTEROS_PARAMS`.
  `profiles.create`/`remove`/`guest.set`/`starter_set.set` are jobs (`profiles_create`,
  `profiles_remove`, `guest_set`, `starter_set`); a create writes metadata, PIN hash and the
  starter env only after the account's switch; a removal ends the account's processes, switches
  without it, then deletes the home with `datadel`'s no-symlink walk, the PIN, the metadata and the
  app links. Profiles (and the guest) without an env get the starter env while the box is idle.
- **Admin PIN for box-wide actions** (0.5): `rollback`, `update.apply`, `channel.set`, `base.set`
  and `hardware.redetect` take `admin_pin`; on a schema-2 box it is checked first (`pin_required
  {pin: admin}` when missing or wrong, or without an admin profile), from every profile, the
  guest included (installing a system update always needs it). Not on schema 1, not for root; an
  admin profile without a PIN yet passes. `update.check` stays open.
- **End guest session** (0.5, `guest.rs`, `runner/profiles.rs`): `guest.end` (the guest only;
  job `guest_end`) checks the account (`hinteros-guest`, `--guest-uid` 20000, the uid maps back to
  the name, home `…/hinteros-guest`), hides the guest from the picker and PAM while it runs, ends
  the guest's processes, then deletes what the uid owns in its home and in `--guest-tmp-dir`s
  (`/tmp`, `/var/tmp`, `/dev/shm`) with `datadel::wipe_owned` (no symlinks, one filesystem, other
  uids left), resets its app link to the starter env and drops its terms, usage and switch hint.
- `profiles.switch {id, pin?}`: a locked target (other than the caller's own) needs its PIN (`pin_required
  {pin: target}`, backoff unless it is the admin profile); right, the hint gets a one-time token for
  the target user (60 s, single use in `pin unlocked`), so `greet` enters it with no UI. Without
  `pin`, the caller that holds a live token for that target renews hint and token (a switch still
  closing apps). `profiles.switch_cancel` drops the caller's own hint.
- `greeter.sock` (`--greeter-socket`, group `--greeter-group`, 0660): `hello`, `ping`,
  `subscribe`, `settings.get`, `jobs.get`, `profiles.create` (a create from it grants the new
  account a token).
- `hinteros-system pin unlocked|verify|end-others|splash` (`pam.rs`): the PAM helper (`PAM_USER`;
  the PIN on stdin; prints only `hinteros:wrong` / `hinteros:locked:<s>`). `splash`
  (`splash.rs`) paints the session's VT (`PAM_TTY`, only `/dev/ttyN`/`ttyN`, opened without
  following links and checked to be a VT) during greetd's handover: console escape sequences only
  (palette background/foreground = the launcher's `#0E1118`/`#F4F6FB`, clear, cursor hidden) and
  "Loading <name>…" in block letters (`bigtext.rs`: a 5x8 font in `█▀▄`, scale 1-3, the largest
  that fits the VT's size, on one line or "Loading" above the name; the console font's own text
  when nothing fits or a character has no glyph), centred (the profile's name from profiles.json,
  "Guest"; the greeter shows the target of a fast switch, else "HinterOS"), names sanitised to
  printable characters and cut at 32. Always exits 0.
- **Forgotten PIN:** `sudo hinteros-system reset-pin [<id or exact name>]` (`resetpin.rs`, root
  only; no argument lists the profiles with their ids). Removes the profile's PIN and clears its
  wrong-PIN wait under the backoff file's `flock` (no race with a check in the service or the
  PAM helper), writes an audit entry (`HINTEROS_METHOD=reset-pin`, the profile, `SUDO_USER`;
  never a PIN) and republishes `profiles-public.json`. The service needs no signal: it reads
  `pins/` and `profiles.json` on every request. The profile is then entered from the picker
  without a PIN (also when locked), its launcher shows "Set your PIN" (`pin_set` false), and
  `profiles.set_pin` needs no current PIN until one is set. There is no in-UI recovery of a
  forgotten PIN on purpose (the owner: reset from the terminal by root); `profiles.reset_pin`
  (another profile's PIN, with the admin PIN) stays.

### Session daemon

- `session.toml` `[apps] profile = "/run/hinteros/apps/%u"` (instead of `[[apps]]`): apps, Steam's
  path and icons from the env; the whole link chain is watched. `apps.list` entries get
  `needs_terms`; `apps.launch` refuses them (`terms_required`); Steam's autostart waits for its
  terms. `store.list` uses the profile's ids and hides services.
- `profile.current`, `session.switch_profile` (+ `switch_wait`, `switch_force_close`,
  `switch_cancel`; events `switch_progress {stage: closing_apps|closing_steam|ending|cancelled}`,
  `switch_blocked {app, waited_s}`; `[switch] wait_s = 10`, `steam_wait_s = 15`; nothing is
  killed without `switch_force_close`), `session.end_guest` (closes the apps, then `system.guest.end` ends the session and wipes the guest
  now), the sleep lock (logind delay
  inhibitor; `lock_state`, `session.lock_state`, `session.unlock`), `profiles_changed {profiles}`.
  `display.keep` and `audio.set_output` also save the mode/output for every profile; at start the
  published `/run/hinteros/audio-output` (`[audio] shared_output`) becomes the default sink once
  that sink exists (retried for 60 s).
- `hinteros-session greet [--config FILE] -- <picker…>` and `hinteros-session --greeter`
  (`greeter.list/login/pin/cancel/create`, `greeter_prompt`, `greeter_result`; `[greeter] public,
  socket, session_command`). `greet` decides before any compositor runs: a live hint with `fast`
  (unlocked target, the guest, or a token) is entered over greetd IPC and the picker never starts;
  otherwise it execs the picker. `greeter.list.switch_to`: an unexpired switch hint's target (the
  picker goes straight to its PIN pad). `session.switch_profile {id, pin?}` passes the PIN to
  `system.profiles.switch` before anything closes, renews the hint every 25 s and right before
  the session ends, and drops it on `switch_cancel`. The greeter never reads an `[apps] profile` env.

Verified here against fakes (`crates/hinteros-system/tests/{apps,profiles}.rs`,
`crates/hinteros-session/tests/{reload,profiles,greeter}.rs`). Needs the system repo's VM tests
(profiles-*, apps-*, first-boot, guest, starter-set): real greetd/PAM, logind's sleep signal,
`end-others` killing a real profile's processes, real `appEnv.build`.

### What the launcher must do (session API 0.13)

Hello with `0.13`. 0.13: pass `admin_pin` to `system.rollback`, `system.update.apply` (every
profile, the guest too), `system.channel.set`, `system.base.set` and `system.hardware.redetect` (ask on `pin_required {pin: admin}`); say
"wiped now" for End guest session; show `readds_features` on the rollback screen. Everything of 0.10 stays. New: the methods and events above (schema:
`schema/session-api.json`), all `system.*` profile, apps and starter-set relays; the first-boot
wizard when `profile.current.setup_required`; "Set your PIN" when `!pin_set`; the terms dialog
for `apps.list` entries with `needs_terms`; on schema-2 boxes pass `system.apps.list`'s
`revision` to `system.apps.install/remove`.

## Storage and clean-up (system API 0.6, session API 0.14)

Design: `hinteros-docs/design/storage.md`. Every method below is relayed as `system.<method>`.

### Measuring (`storage.rs`, `runner/storage.rs`)

- `storage.measure` (polkit `org.hinteros.system.settings`, any profile, no PIN) starts job
  `storage_measure` (a running one is returned instead of a second). Stages `preparing`,
  `measuring` (with `percent`), `finishing`; cancellable. It reads statvfs of `--store-dir`,
  `nix path-info --all -s` (NAR sizes), `nix-store --gc --print-live` (garbage = all − live),
  `nix-store -qR` of every system and app generation, and walks each home (`state.json`'s
  accounts, then the guest): no symlink followed, no other filesystem entered, no file read
  except Steam's `libraryfolders.vdf` and `appmanifest_*.acf` (games named one by one, each
  library folder that is a real folder inside the home), hard links counted once.
- Trees: children always add up to their parent (remainders are `common`, `shared`, `files`,
  `other`); home trees are cut at 6 folder levels, max(1 MiB, 0.5 %) per folder, 16 folders per
  folder, 400 nodes; names at most 128 characters. Top level: `system` (generation / common),
  `apps` (`app_env` per owner outside the system, generation / common; `shared` = in two or more
  owners' envs), one `profile` per account, `guest`, `other`, `free`.
- The result goes to `/var/lib/hinteros/storage.json` (0600) with its time: `storage.get`
  (read-only, open) returns the public part (top level, every profile's total, garbage and the
  clean-up estimates for keep 1..10); `storage.tree {id, admin_pin?}` (through the mutating gate
  for the rate limit and audit) a home's detailed tree: the caller's own account (or the guest's
  own) without a PIN, any other with the admin PIN (same rules as `rollback`). The job's result
  is only `{measured_at}`; the job logs counts, never a name.

### Clean-up

- `storage.cleanup_system {keep, admin_pin?}` (job `cleanup_system`, polkit
  `org.hinteros.system.rebuild`, admin PIN every time): removes the system generations older
  than the newest `keep` (1..10) with `nix-env --delete-generations <N…>`, never the running, the
  booted or the profile's current one; then `switch-to-configuration boot` of the default
  generation (no boot entry left pointing at a removed one; if that fails, no GC), then
  `nix-store --gc`. Stages `preparing`, `cleaning` (no cancel from here), `collecting`,
  `finishing`. Result `{freed_bytes, generations_removed}` (free space after − before).
- `storage.cleanup_profiles {keep, admin_pin?}` (job `cleanup_profiles`): the same over every
  profile's app Nix profile and the starter set's (never the current generation; the guest's
  single link untouched), then GC. Schema 1: `unavailable {reason: schema1}`.
- Both serialise with the system's `nix-gc.service` through Nix's own GC and profile locks.

### Automatic clean-up setting

`state.json` `settings.cleanup {enable, frequency: daily|weekly|monthly, keepSystem, keepProfiles
(1..10)}`, every field optional (defaults on, weekly, 5, 5), schemas 1 and 2, checked by
`State::parse` and so by `check-state`. `cleanup.get` (read-only) returns it with the defaults
filled in; `cleanup.set {enable, frequency, keep_system, keep_profiles, admin_pin?}` (admin PIN,
polkit `org.hinteros.system.settings`) writes all four fields through the apply flow (job
`cleanup_set`, `null` when nothing changes, also for the defaults while absent).

### What the system repo must do (0.6)

- `state.nix`: accept `settings.cleanup` as above; `nix.gc.automatic` from `enable`, `dates` from
  `frequency`, the system preStart `--delete-generations +keepSystem` and
  `boot.loader.systemd-boot.configurationLimit` from `keepSystem`, the app-env preStart from
  `keepProfiles` (absent: today's values). Suggested: skip the booted generation in the preStart.
- `hinteros-job@.service`'s `path` needs `nix-store` (already there with `nix`). No new polkit
  action (existing ones are reused).

### What the launcher must do (session API 0.14)

Hello with `0.14`. Settings → Storage: show `system.storage.get`'s measurement at once with its
age, or start `system.storage.measure` (progress from `job_progress {stage: measuring,
percent}`; re-read `storage.get` on `job_finished`); draw the ring from `root` (names for kinds
are the launcher's; profile names from `profiles.list`); entering the caller's own profile calls
`system.storage.tree {id}`, another profile or the guest asks for the admin PIN first (on
`pin_required {pin: admin}`). Clean-ups: show `estimates.system[keep-1]` /
`estimates.profiles[keep-1]` before the admin confirms, warn below 2, call
`system.storage.cleanup_system` / `system.storage.cleanup_profiles {keep, admin_pin}`, show
`result.freed_bytes`, then measure again. Automatic clean-up: `system.cleanup.get` /
`system.cleanup.set` (admin PIN). New job kinds `storage_measure`, `cleanup_system`,
`cleanup_profiles`, `cleanup_set`; new stages `measuring`, `cleaning`, `collecting`.

## Job progress (system API 0.7, session API 0.15)

A job's page must say what is going on at every moment, also while nix compiles on the box for
many minutes, and the launcher lets the person leave the page while the job runs (a top-bar
indicator), so the whole state is readable at any time.

### Building (`nixlog.rs`)

The job's `nix build --log-format internal-json -v` output gives, in `ProgressDetail`:
`builds_done` / `builds_total` (the builds activity's progress, and the plan nix prints first:
`these N derivations will be built`; `these N paths will be fetched` likewise seeds
`paths_total`), `builds_running`, and `building {name, version?, phase?}`: the most recently
started derivation still building, its `.drv` name split by nix's rule (the version starts at
the first `-` followed by a non-letter: `hinteros-daemon-0.7.1` → `hinteros-daemon` / `0.7.1`)
and its phase (`setPhase`, without `Phase`: `unpack`, `configure`, `build`, `install`, …).
`percent` is now given while downloading and building: the mean of the known parts (paths, else
bytes; builds), each weighted equally; it never goes back (when nix finds more work it holds,
then covers the rest of the way) and stays below 100 until nix ends. It is rough: derivations
differ hugely in build time, so the count is what the launcher shows prominently.

### Heartbeat (`runner.rs`, `jobs.rs`)

The job writes a progress line at once on a stage change, at most 4 per second otherwise (a
held-back line is written later, never lost), and, while a child process prints output but
nothing countable changes, the last line again every 2 s with the new `activity_at`. The service
emits `job_progress` for every line it reads and, when it has emitted nothing for 2 s while the
job runs, repeats the last one. Every `job_progress` carries `kind`, `started_at` and
`last_activity_at` (the last time the job's process printed anything, nix build log lines
included).

### What a job is about (`service/subject.rs`)

`subject` (in `job_started`, `job_finished`, `Job`) is filled in by the service from its own
state when the job is created (catalog names from the running generation's catalog, profile
names from `profiles.json`, the running release version); the job may refine it (a channel
switch learns its target `rev` and `version` once verified: one `job_progress` then carries the
whole new `subject`, which `jobs.get` and `job_finished` keep). Fields, each optional:
`owner` (profile id, `guest`, `starter`), `profile {id, name}`, `apps [{id, name}]`,
`delete_data`, `from_owner`, `generation`, `from_generation`, `rev`, `version`, `channel`,
`from_channel`, `from_rev`, `from_version`, `base`, `from_base`, `enabled`, `keep`, `frequency`,
`keep_system`, `keep_profiles`. Per kind:

| kind | subject |
|---|---|
| `settings`, `rebuild`, `hardware_redetect`, `storage_measure` | `{}` |
| `ssh`, `guest_set` | `enabled` |
| `update_check` | `channel`, `from_rev` |
| `update_apply` | `rev`, `version`, `channel`, `from_rev`, `from_version` |
| `channel` | `channel` (target), `from_channel`, `from_rev`, `from_version`; `rev`, `version` once verified |
| `base` | `base`, `from_base` |
| `rollback` | `generation`, `from_generation` |
| `apps_install` | `owner`, `profile` (a profile), `apps` (only the new ones); the automatic starter env of a new profile or the guest (uid 0): `owner`, `profile`?, `from_owner: starter` |
| `apps_remove` | `owner`, `profile`?, `apps`, `delete_data` (schema 1: `apps`, `delete_data`) |
| `apps_rollback` | `owner`, `profile`?, `generation`, `from_generation` |
| `starter_set` | `owner: starter`, `apps` (the whole new set) |
| `migrate_apps` | `apps` |
| `profiles_create`, `profiles_remove` | `profile` |
| `guest_end` | `owner: guest` |
| `cleanup_system`, `cleanup_profiles` | `keep` |
| `cleanup_set` | `enabled`, `frequency`, `keep_system`, `keep_profiles` |

`Job` also has `started_at` (equals `created_at`) and, for a running job, the latest `percent`,
`detail` and `last_activity_at` (null once finished); `job_finished` has `started_at` and
`finished_at`. The job's spec and `status.json` carry the subject, so it survives a restart of the
service.

### What the launcher must do (session API 0.15)

Hello with `0.15` (system relays need the session built against system 0.7). On the job page and
in the top-bar indicator: name the job from `kind` + `subject` (an update as
`{version}.{rev[..7]} {channel}`, e.g. `0.7.1.c6b69ce beta`; "Installing Moonlight for Sam";
"Removing …"; the profile, generation, base or channel concerned); show elapsed time from
`started_at` (no time-left estimate); while `stage` is `building` show "Building
<building.name> (n of m)" with n = `builds_done + 1` capped at `builds_total`, the count
prominent and the name small (`building.version` / `phase` optional, small); `percent` when
present. Treat each `job_progress` as "alive"; if none came for well over 2 s, or
`now - last_activity_at` grows long, say the job is quiet rather than stalled. On (re)connect or
when opening the page later, call `system.jobs.list` (the running job is first) or
`system.jobs.get {job_id}` for the full state, then follow `job_progress` / `job_finished` by
`job_id`; a `job_progress` with `subject` replaces the job's subject. New required event fields:
`job_started {subject, started_at}`, `job_progress {kind, started_at}`, `job_finished {subject,
started_at, finished_at}`; new `Job` fields `subject`, `started_at`, `percent`, `detail`,
`last_activity_at`.

## Time zone (system API 0.8, session API 0.16)

Design: `hinteros-docs/design/time-zone.md`. Code: `timezone.rs` (zones, offsets, list, the
link), `service/timezone.rs` (the methods).

- `state.json` `settings.timeZone`: an IANA name, absent means `UTC`, schemas 1 and 2. `State::parse`
  (and so `check-state`) checks its shape only (the system repo's rule: `/`-separated parts
  `[A-Z][A-Za-z0-9_+-]*`, at most 64 bytes); a well-formed zone tzdata does not have reads as UTC.
- Offsets come from the system's tzdata (`--zoneinfo-dir`, `/etc/zoneinfo`; parsed with `tz-rs`),
  the files `/etc/localtime` points into, never from a database in the daemon.
  `offset_seconds` is the offset now; `standard_offset_seconds` the lower of the zone's two
  offsets while it observes DST (so tzdata's negative DST, Europe/Dublin, gives its winter
  offset, like the map's bands), otherwise the offset now; `offset_valid_until` the next change
  of the offset (the explicit transitions, then the TZif footer's rule), or null.
- `time_zone.get` (read-only): the zone that applies, with its offsets. `time_zone.list`
  (read-only): `zone1970.tab`'s zones plus `UTC`, sorted by name, each with its offsets (a zone
  without a usable file is left out).
- `time_zone.set {zone, admin_pin?}` (polkit `org.hinteros.system.settings`): the admin PIN
  (`pin_required {pin: admin}` when missing or wrong), except in first-boot mode (no admin
  profile yet, the rule of `profiles.setup_admin`). The zone must be a regular TZif file (not a link) under the
  zoneinfo dir (`UTC` always): a bad name is `invalid_params {field: zone, reason: bad_format}`,
  an unknown zone `invalid_params {field: zone, reason: unknown}`. No job and no rebuild, and
  the service never writes `/etc`: it writes the zone to `<run-dir>/time-zone.request` (one
  line, 0600, atomically), starts `hinteros-time-zone-apply.service` through systemd's D-Bus
  API (`StartUnit`, mode `replace`) and waits up to 30 s for its `JobRemoved` with result `done`
  (else `internal_error` "The time zone did not change."; one request at a time). The unit runs
  `hinteros-system time-zone-apply [--config-dir --state-dir --run-dir --zoneinfo-dir
  --localtime]`: it reads the request, checks the name and the TZif file again, and under the
  `state.json` lock (`<run-dir>/state.lock`) writes `settings.timeZone` (atomically, canonical
  form; only when it changes), points `--localtime` at `<zoneinfo-dir>/<zone>` (a new link
  renamed over the old one) and removes the request; on any failure it logs, exits 1 and leaves
  `state.json` as it was. The service then reads the zone back: the result is the new
  `TimeZoneInfo`, then `time_zone_changed` with the same, only when the zone changed. Works
  while a job runs.
- Jobs: a job that replaces `/etc/hinteros` takes the same lock and first copies the live
  `settings.timeZone` into its staged `state.json`, so a zone set while it ran (or after its
  state was computed) is kept. A rollback restores the generation's `state.json` (its zone) and
  then points `/etc/localtime` at that zone (the switch ran before the restore); the service
  sends `time_zone_changed` after any job when the zone that applies changed.

### What the system repo must do (0.8)

- `state.nix`: accept `settings.timeZone` (a string; the shape above). `time.timeZone = null`;
  an activation script points `/etc/localtime` at `/etc/zoneinfo/<zone>` from the live
  `/etc/hinteros/state.json` (read at run time; UTC when tzdata lacks it).
- `hinteros-config-sync`: `time_zone.set` changes `/etc/hinteros/state.json` in place without a
  new generation, so the running generation's snapshot differs in `settings.timeZone` only.
  The sync must not undo that at the next boot: compare `state.json` without `settings.timeZone`
  and keep the live zone when it restores (a rollback, which changes the generation, restores
  the snapshot's zone, as the daemon's own rollback does).
- `hinteros-time-zone-apply.service`: oneshot, root, `ExecStart=hinteros-system
  time-zone-apply`, `ProtectSystem=strict`, `ReadWritePaths=/etc /run/hinteros`, no network.
  The service only needs `/run/hinteros` writable (no `/etc`) and may start this unit (it is
  root; systemd's manager API). No new polkit action.

### What the launcher must do (session API 0.16)

Hello with `0.16`. `system.time_zone.get` → `{zone, offset_seconds, standard_offset_seconds,
offset_valid_until}`; `system.time_zone.list` → `{zones: [{zone, offset_seconds,
standard_offset_seconds}]}`; `system.time_zone.set {zone, admin_pin?}` → the new `TimeZoneInfo`
(no PIN in the first-boot wizard; in Settings ask for the admin PIN on `pin_required`). Event
`time_zone_changed` (a `TimeZoneInfo`): subscribe to it. The clock shows UTC plus
`offset_seconds` and gets the zone again at `offset_valid_until` and on `time_zone_changed`.

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
`{"jsonrpc":"2.0","id":1,"method":"hello","params":{"api_version":"0.16","client":{"name":"me","version":"0"}}}`,
and `kill -USR1` the daemon to emit `home_pressed` (after `subscribe`). Launch something with
`{"jsonrpc":"2.0","id":2,"method":"apps.launch","params":{"command":["xterm"]}}`.

The systemd units live in the system repo, which consumes `overlays.default`.

Part of [HinterOS](https://justsomeshed.com/JSwartz/HinterOS/hinteros). Licensed under GPL-3.0-only, see [LICENSE](LICENSE).
