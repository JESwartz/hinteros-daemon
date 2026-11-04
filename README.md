# HinterOS Daemon

The Rust system services behind the launcher of [HinterOS](https://hinteros.org), built as two
binaries:

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
  a long press with Steam in front (below). Ctrl + Super (Windows key) + D on any keyboard is
  Home too, at once like SIGUSR1. The daemon never reads keyboards (no profile account may: a
  game could log the admin password): `hinteros-system keyboard-home` does, in its own unit, and
  the system service sends `keyboard_home` to the daemon of the active seat0 session over the
  system relay (`system.rs`; see "Keyboard Home" under "System service").
  `[input] keyboard_home = false` ignores it; the greeter has no relay.
  ALL is also what keeps the pad from sandboxed apps while the launcher asks for a PIN or a
  password: an app with the `gamepad` permission can only read the virtual pads (InputPlumber
  grabs and hides the physical ones), and in ALL their events go only to us. A new pad starts
  in InputPlumber's default mode (NONE: all to its virtual pad), so its mode is set first,
  before the pad is classified.
  Re-applies everything when InputPlumber restarts or a pad appears. Without
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
  `speed_pps` and `curve`: HinterOS patches InputPlumber so that a mapping can ask for a response
  curve, speed_pps * m^curve past a 0.20 radial deadzone; only this profile does, so games keep
  the stick as it is) and uses intercept mode
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
  (hold Guide, press Y) toggle it; `osk_changed` reports it. While it is visible InputPlumber
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
  takes B itself (`osk_changed false`, the keyboard client never sees B, nor its release),
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
  `osk_changed` carries `password: true` while the keyboard is up for a
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
  entry uses Steam's own `steam.desktop`. Desktop files and icons are searched in the profile's
  app env (`[apps] profile`, its `share`, first; the session's `PATH` and `XDG_DATA_DIRS` never
  have it), `$XDG_DATA_HOME`, `$XDG_DATA_DIRS`, `/run/current-system/sw/share`,
  `/etc/profiles/per-user/$USER/share`, `~/.nix-profile/share` and the Flatpak exports. An
  absolute `Icon=` in a desktop file counts only when the file is inside the same package
  (`/nix/store` path) as the desktop file, or inside the data directory it was found in; themes
  searched are the active one (GTK's
  `gtk-icon-theme-name`, or `icons.theme`), what it inherits, and `hicolor`, plus `pixmaps/`.
  The best file wins: the largest PNG of at least 256 px, else an SVG, else the largest
  smaller PNG. Web apps: the daemon fetches `icon_url` (with its required `icon_sha256`, the catalog's
  `icon.sha256`, keeps it only if its SHA-256 matches; else it logs and retries later), or finds the icon of `site_url` (web app
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
  modes through gamescope's Wayland output, Xwayland and mode save file. See
  "Runtime settings" below.
- **System relay** (`system.rs`): one connection to the system service (see below), handshaken
  and subscribed to its job events; the session API's `system.*` methods are passed through
  unchanged and `job_*` events forwarded. While the service is down or speaks another version,
  they fail with `system_unavailable {reason: down|version_mismatch}`.
- **Admin password** (`auth/`): a polkit authentication agent for this process; prompts go to
  the launcher (`authentication_requested`, `auth.respond`, `auth.cancel`). See
  "Admin password, session restart and Store" below.
- **Session restart** (`restart.rs`): `session.restart` ends the greetd session through logind
  (greetd starts it again) and restarts the daemon's own user unit.
- **Store** (`store.rs`): `store.list` merges the catalog index with the installed apps;
  `store_changed`; `system.apps.install`/`.remove` are relayed. Catalog icons are resolved like app
  icons (`icon.path`). **Reload** (`reload.rs`): the app list follows the config file and
  the catalog without a restart; see "Store icons and the app list without a restart".
- **Reaper** (`reaper.rs`): `hinteros-session reaper SteamLaunch AppId=<id> -- <command>` runs
  a command as a child subreaper named `reaper`, which is how gamescope (Steam mode) learns the
  app id of a process's windows, X11 and native Wayland alike. The daemon starts command apps
  this way; the launcher must be started this way too (see below). `Cgroup=<dir>` before `--`:
  move into that cgroup first, or start nothing (exit 125); see "`lan`" below.
- **Sandbox** (`sandbox.rs`, `permissions.rs`): catalog apps run in bwrap below the reaper, with
  the permissions the profile granted (`apps.set_permission`); see "App sandbox and permissions".

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
apps_display = "auto+2"        # first app display (each app under the reaper gets its own); "auto+<n>": the user's n-th display after the lowest [default: display, shared]
app_displays = 4               # how many when $XDG_RUNTIME_DIR/hinteros/app-displays is missing; the profile setting's default (1 to 8)
wayland_display = "gamescope-0"  # given to Steam (not to apps) [default: $WAYLAND_DISPLAY, else gamescope-0]
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
keyboard_home = true           # the system's keyboard_home (Ctrl+Super+D) is Home

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
curve = 2.5                    # speed = speed_pps * m^curve, m = deflection past the deadzone; 1 = linear
activity_deadzone = 0.25       # right stick movement that counts as using the pointer
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

[sandbox]                      # catalog apps in bwrap (not Steam); see "App sandbox and permissions"
enable = false
bwrap = "bwrap"                # a path, or a name looked up in PATH
pulse_socket = "pulse/native"  # sound for apps, relative to XDG_RUNTIME_DIR
pulse_record_socket = "pulse/native"  # with mic; the same as pulse_socket: mic is not listed

[[apps]]                       # what apps.list returns after the built-in "steam" (a stub list if none)
id = "jellyfin-desktop"
name = "Jellyfin"
command = ["jellyfin-desktop"]
desktop_id = "org.jellyfin.JellyfinDesktop"   # icon from its .desktop file (or: icon = "/path.png")

[[apps]]
id = "youtube"
name = "YouTube"
kind = "web"
command = ["firefox", "--kiosk", "--new-instance", "https://www.youtube.com/tv"]
site_url = "https://www.youtube.com/tv"     # icon from the site (or: icon_url = "https://.../icon.png")
# icon_sha256 = "<64 hex digits>"           # required with icon_url: refuse a download with another SHA-256
# firefox_profile = "~/.local/share/hinteros/firefox/youtube"  # fallback: the favicon Firefox stored
input = "keyboard"             # "gamepad" (default) or "keyboard"
back_key = "escape"            # what B sends [default: escape]; e.g. "backspace", "alt+left"
pointer = true                 # keyboard profile: right stick = mouse pointer [default: true]
address_bar = false            # plain Firefox (not kiosk): View = Ctrl+L + on-screen keyboard, LB/RB = back/forward
permissions = { mic = "optional" }  # sandbox permissions (catalog): "required" or "optional"
# sandbox = false              # run without the sandbox [default: true]

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
  nix run git+https://git.hinteros.org/hinteros-launcher.git -- --windowed
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
  gamescope is the only X server. No Xauthority is needed. Launched apps get `DISPLAY` from the
  config, since a user service does not inherit gamescope's. Only Steam also gets
  `WAYLAND_DISPLAY` and `GAMESCOPE_WAYLAND_DISPLAY`: apps run as X11 clients and never see
  gamescope's private Wayland protocols (input method, action bindings, control).
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

### Runtime settings

Settings that need no rebuild, served by the session daemon as the session user and authorised by
the polkit rules of the system repo's `runtime.nix` (NetworkManager `network-control`,
`enable-disable-wifi`, `wifi.scan`, `settings.modify.own` and `.system`; logind suspend, reboot,
power-off, for the active local session user). Code in `crates/hinteros-session/src/runtime/`;
every method and event is in `schema/session-api.json` with examples.

| Area | Methods | Events | Through |
|---|---|---|---|
| Wi-Fi | `network.status`, `wifi.scan`, `wifi.list`, `wifi.connect {ssid, password?}`, `wifi.disconnect`, `wifi.forget {ssid}`, `wifi.enable {enabled}` | `network_changed`, `wifi_list_changed` | NetworkManager (D-Bus) |
| Bluetooth | `bluetooth.status`, `.scan {enabled, timeout_s?}`, `.devices`, `.pair {address}`, `.confirm {address, accept}`, `.connect`, `.disconnect`, `.forget` | `bluetooth_changed`, `bluetooth_confirm` | BlueZ (D-Bus) |
| Audio | `audio.outputs`, `.set_output {id}`, `.volume {percent?}`, `.mute {muted}` (all return the same state). The outputs are the sinks and the outputs of each ALSA card's other available profiles (id: the sink name it will have, `alsa_output.<card>.<mapping>`; no surround variants); `set_output` on one of those switches the card's profile first (`set-card-profile`, keeping the input), as does the box-wide output at session start | `audio_changed` | `pactl` (PipeWire's pulse server) |
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

`hinteros-system` (`crates/hinteros-system/src/`) runs as root and serves system API 1.0
(`schema/system-api.json`, design in `hinteros-docs/design/system-api.md`). Any process of a
`hinteros` user can connect; the session daemon relays it to the launcher but gets no extra
trust.

- **Methods:** read-only `system.info`, `settings.get`, `generations.list`, `jobs.get`,
  `jobs.list`; mutating `settings.set {revision, changes}` (closed, typed `changes`; empty), `ssh.set {enabled}`, `rebuild`, `hardware.redetect`, `rollback {generation?}`,
  `update.check`, `update.apply {rev}`, `channel.check {channel}`, `channel.set {channel, rev?}`,
  `base.set {base, acknowledge_downgrade?}`, `apps.install {ids, revision, accepted_terms?}`,
  `apps.remove {ids, revision, delete_data}` (see "Store backend" below), `jobs.cancel {job_id}`. Events `job_started`,
  `job_progress`, `job_finished {…, session_restart_required}` after `subscribe`.
- **Authorization** (`peer.rs`, `authz.rs`): uid/gid/pid from `SO_PEERCRED` and a pidfd from
  `SO_PEERPIDFD` at accept. Mutating methods need: the pidfd (and the process alive), the caller
  owning seat0's active logind session (or root), and polkit `CheckAuthorization` saying yes for
  the method's action, with a `unix-process` subject carrying the pidfd, pid, start time and uid,
  without user interaction except for `org.hinteros.system.ssh` (see "Admin password" below).
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
  `nix flake lock --override-input`. `channel.set` rewrites the string of `inputs.hinteros.url`
  in `flake.nix` and nothing else, in any layout (one plain string, defined once, equal to
  `git+<repo>?ref=<branch>` for the current branch and `repo` of `channels.json`, else an
  error and the file stays) and locks the revision `channel.check`
  verified on the new branch (see "Channel switches" below).
  `base.set` edits `state.json` (unstable → stable needs the acknowledgement).
- **Rollback**: `nix-env --switch-generation N` + `switch-to-configuration switch`, then
  `/etc/hinteros`'s `flake.nix`, `flake.lock` and `state.json` from the generation's snapshot
  (`$out/hinteros/`, as `hinteros-config-sync` does at boot).
- **`hardware.redetect`**: `nixos-facter -o staging/facter.json`, then the apply flow.
- **Keyboard Home** (`keyboard.rs`): Ctrl + Super + D on a keyboard is Home. No profile
  account may read keyboards (a game would log the admin password typed into the launcher's
  prompt or on the admin console), so `hinteros-system keyboard-home` is the only reader, in its
  own unit (system repo: `hinteros-keyboard`, a dynamic user in groups `input` and
  `hinteros-keyboard`, devices read-only, no network, no capabilities). It opens every
  `/dev/input/event*` keyboard read-only and never grabs it (the focused app gets the keys too),
  skips the session daemon's virtual keyboard (and anything named "hinteros"), keeps only the
  state of Ctrl, Super and D, and fires once per D press while a Ctrl and a Super are held on
  that keyboard (hotplug every 2 s). Each press is one `home` line on `keyboard.sock`
  (`--keyboard-socket`, root:`--keyboard-group` 0660; anything else closes the connection; a
  press while the service is down is dropped). The service sends `keyboard_home` (params `{}`)
  only to the connections of the uid owning seat0's active session at that moment, never to the
  greeter socket; the session daemon goes Home. Nobody owns seat0 on a text console without a
  session: no event.
- **Inactive profiles frozen** (`--freeze-inactive`, `freeze.rs`): logind moves device ACLs at
  every switch but cannot revoke an fd opened before (`/dev/uinput`), so a game of a profile
  that is not in front could type into the admin console (Ctrl+Alt+F2) or another session. The
  service freezes `user-<uid>.slice` (systemd `FreezeUnit`) of every profile account (members
  of `hinteros` and the guest; never the admin account, the greeter, root or a system user)
  whose logind user state is `online` or `lingering` (sessions, none active) once it has been
  `active` since it logged in (or was online when the service started), unless a session is
  `opening` or `user@<uid>.service` is `activating` (a login waits for both), and thaws it
  (`ThawUnit`) when it is `active` again, `closing` (systemd cannot stop a frozen unit) or
  gone. Every logind signal checks at once (seat0's `ActiveSession` on a VT switch), plus a
  check every 2 s (a restart re-applies it). `PrepareForShutdown` and stopping the service thaw
  everything. `pin end-others` thaws the accounts it ends, before and after `TerminateUser`.
  A slice frozen by `user.slice` during suspend is frozen by us too, so the resume leaves it
  frozen. A frozen profile's sleep inhibitor (`delay`) holds a suspend from a text console up to
  logind's `InhibitDelayMaxSec`.

### Command line

All paths default to the box's; tests point them at a temporary root.

| Flag | Default |
|---|---|
| `--socket` (`HINTEROS_SYSTEM_SOCKET`) | `/run/hinteros/system.sock` |
| `--config-dir` | `/etc/hinteros` |
| `--state-dir` | `/var/lib/hinteros` (`staging/`, `git/`, `update-check.json`, `channel-check.json`, `channel-tips.json`) |
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
| `--keyboard-socket`, `--keyboard-group` | `/run/hinteros/keyboard.sock`, `hinteros-keyboard` (no socket without the group) |
| `--freeze-inactive` | off (the system repo sets it): freeze profiles whose session is not the active one |
| `--lan-filter` (`HINTEROS_LAN_FILTER`) | off (the system repo sets it with `lan`): `lan_filter.*` work; without it they fail |
| `--cgroup-root` | `/sys/fs/cgroup` (where `lan_filter.set` checks the caller's cgroup) |

`hinteros-system job [--run-dir DIR] <id>` runs one job (the job unit's `ExecStart`).

`hinteros-system keyboard-home [--socket PATH] [--input-dir DIR]` reads the keyboards for
Ctrl + Super + D (see "Keyboard Home"; the `hinteros-keyboard` unit's `ExecStart`).

`hinteros-system lan-filter-apply [--run-dir DIR] [--cgroup-root DIR] [--resolv-conf FILE]
[--proc-dir DIR] [--nft PATH]` loads the local network filter (see "Local network filter"; the
`hinteros-lan-filter` unit's `ExecStart`).

`hinteros-system check-state FILE...` parses each file with the same `State::parse` the service
uses, without starting the service (no root, sockets or D-Bus). It prints `ok <file>` or
`invalid <file>: <reason>` per file. Exit status: 0 if all files are valid, 1 if any is invalid, 2 for usage errors or a file
that cannot be read (that file is reported as `invalid <file>: reading: ...`). The system repo
runs it in a Nix check over every state.json the installer can write.

### What the system module must provide

For `daemon.nix` (and the VM tests) in the system repo:

- **Group** `hinteros`, with the session user (later: every profile user) in it.
- **`hinteros-system.service`**: `ExecStart=hinteros-system`, `User=root`, `Group=hinteros`,
  `RuntimeDirectory=hinteros` with `RuntimeDirectoryMode=0755` and
  **`RuntimeDirectoryPreserve=yes`** (the job directories in `/run/hinteros/jobs` must survive a
  restart of the service during a switch), `UMask=0077` or `0007`. It reads `/etc/hinteros`,
  `/var/lib/hinteros/update-check.json` and `channel-check.json`, `/proc`, the profile links and `/run/current-system`;
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
- **Keyboards**: no profile account may read a keyboard (no uaccess rule for keyboards, no
  group `input`). `hinteros-keyboard.service`: `ExecStart=hinteros-system keyboard-home`,
  `DynamicUser`, `SupplementaryGroups=input hinteros-keyboard`, `DeviceAllow=char-input r`,
  no network, no capabilities. Group `hinteros-keyboard`, and `hinteros-system.service` in it
  (it chgrps `keyboard.sock` without `CAP_CHOWN`). `hinteros-system --freeze-inactive`.
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
  `update/channels.json` = `{"repo": "https://git.hinteros.org/hinteros.git",
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

### What the launcher must do

The `system.*` methods are relayed unchanged. Subscribe to
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

### What the launcher must do

The new methods map
onto the launcher's `lib/src/system` repository interfaces:

| Interface | Calls |
|---|---|
| `NetworkRepository.status / scan / connect / disconnect / forget / setWifiEnabled` | `network.status`, `wifi.scan` (a fresh scan, strongest first), `wifi.connect`, `wifi.disconnect`, `wifi.forget`, `wifi.enable`; `wrong_password` (4003) is `wrongPassword`, `device_failed` (4002) is `deviceFailed`; `WifiNetwork.saved` is `known`, `secured` is `security != open`, `signal` is 0 to 100 |
| `BluetoothRepository.devices / current / scanning` | `bluetooth.devices`, `bluetooth_changed` (`{status, devices}`), `bluetooth.status`; `startScan`/`stopScan` are `bluetooth.scan`; `pair`/`connect`/`disconnect`/`forget` take the device's `address` (`BtDevice.id`); `kind: gamepad` is `controller`; show `bluetooth_confirm {passkey}` and answer with `bluetooth.confirm` |
| `DisplayAudioRepository.displayModes / setDisplayMode` | `display.modes` (`preferred`, `current`; refresh in whole Hz), `display.set_mode`, then the 15 s dialog: `display.keep` or `display.revert`; check `can_set` first |
| `DisplayAudioRepository.audio / setAudioOutput / setVolume / setMuted` | `audio.outputs` (`AudioState`: outputs with `default` for `current`, `volume` 0 to 100, `muted`), `audio.set_output`, `audio.volume`, `audio.mute`; `audio_changed` |
| `SystemRepository.power` | `power.suspend` (sleep), `power.reboot` (restart), `power.poweroff` (shutdown); `power.capabilities` to hide what is not offered; `power_inhibited` (4005) while an update switches over; `unavailable` (4001) is `SystemErrorKind.unavailable` |

## Admin password, session restart and Store

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

`apps.install {ids, revision, accepted_terms?}` and `apps.remove {ids, revision,
delete_data}` (polkit `org.hinteros.system.apps`, seat check, rate limit, audit), in
`crates/hinteros-system/src/store.rs`:

- ids (1 to 8) are checked against the **running generation's** catalog index
  (`--catalog-index`, default `<current-system>/sw/share/hinteros/catalog.json`, index version 4,
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
  (0600, written atomically): `{"version": 1, "accepted": {"<id>": [{"version", "accepted_at" (RFC 3339 UTC), "by" (user name
  or "installer")}]}}`, a list per id so re-acceptance keeps the history (the installer writes the
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

`store.list` merges the index (`[store] index`) with `system.settings.get`'s apps:
available apps plus installed ones whatever their status, by name, with `state`
(`available|installed|installing|removing|retired|unavailable`; installing/removing from apps jobs
relayed through this daemon), all catalog fields in snake_case, terms with the absolute path of the
text so the launcher shows it offline, and `more_after_update` from the last update check's
`new_apps`. `store_changed` after an apps job starts or finishes, any other switching job finishes,
or `more_after_update` changes. `unavailable {service: catalog}` without an index (or an unknown
index version); `system_unavailable` while the system service is down. `system.apps.install` and
`system.apps.remove` are relayed. Verified against fake roots and fakes; needs the system repo's
`store-install` VM test (real build, `apps.list` reload).

### Store icons and the app list without a restart

**Store icons.** Every `store.list` entry's `icon` object gains an optional `path`: the resolved
icon on this box, absent until there is one. The daemon resolves the catalog index's `icon` fields
like an app's: `desktopId` (when the app happens to be installed, through its desktop file and the
icon themes), else `iconUrl` or `siteUrl` downloaded into the same cache
(`$XDG_CACHE_HOME/hinteros/icons/<id>.<ext>`, https only, the same limits, `icons.refresh_days`
refresh, and the same 1 min / 10 min / 1 h / 6 h retries, in the same background thread). An
installed app's own `[[apps]]` entry wins for the same id. `store_changed` is sent when a catalog
icon arrives (and `apps_changed` when an app's does). `icons.fetch = false` turns downloads off
for both. The catalog is re-read on a new generation.

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

### What the system repo must provide

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

### What the launcher must do

Say hello with client name `hinteros-launcher`, match responses by id (they may come out of
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
- On-screen keyboard: mask the typed-text preview only while `osk_changed.password` is
  true; show it plainly otherwise (also when no field was reported).
- Updates (0.11): show `available.version` ("HinterOS 0.6.1") on the update screen and banner
  (the short rev when it is absent), and `available.security_summary`, when non-null, under the
  severity on the security notice and the update screen.

## Profiles and per-profile apps (M7)

Design: `hinteros-docs/design/profiles.md` and `apps.md`. Cross-repo contracts (paths, file
formats, flags): `m7/DECISIONS.md` (D1–D17) while M7 is being built.

### Apps per profile (system service)

- **Schema 2** (`state.rs`): `state.json` without `apps`, with `requirements` (box features:
  `graphics32`, `steam-devices`), `services`, `profiles: [{id, user}]` (1–8; `p1` = `hinteros`,
  else `hinteros-p<N>`) and `settings.guest.enable`.
- **Envs** (`appreg.rs`, `runner/apps.rs`): a profile's apps are the root-owned Nix profile
  `/nix/var/nix/profiles/hinteros/apps/<user>` (`--apps-profiles-dir`), the starter set's
  `…/starter`, the guest's a single link `/run/hinteros/apps/hinteros-guest` (an indirect GC root
  via `nix-store --add-root --indirect --realise`). The service publishes
  `/run/hinteros/apps/<user>` → the profile's Nix profile at start and after every job.
  `/run/hinteros/apps` is 0750 root:hinteros, not the 0755 of the M7 design (D4): every
  session account is in `hinteros`, and other users cannot list who has which apps. Whoever
  publishes (a job runs as root:root)
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
  account, root or the admin account gets `permission_denied {reason: profile}`.
- **Box features**: an install or rollback whose apps need a feature that is off runs the `box`
  stage first (the apply flow with `requirements` = every needed feature plus the new ones).
  Every other job that rebuilds the system writes `requirements` = what the owners' envs need,
  so unneeded features go only then. Update, channel, base and rollback jobs end with an `apps`
  stage that rebuilds every owner's env against the new system (tombstoned ids dropped); an env
  that fails, or has an app the catalog does not have (an older catalog after a rollback), keeps
  its generation and is listed in `apps_not_updated`, the job still succeeds.
- **Rollback re-adds features**: after a rollback's `apps` stage, features the envs need that
  the restored generation lacks are turned back on in a `box` stage (the apply flow on the restored
  configuration with `requirements` = what the envs need; a new generation). The result lists
  them in `features_readded`; `generations.list` entries name them beforehand (`readds_features`).

### Profiles, PINs, greeter (system service)

- `profiles.json` (`profiles.rs`), PINs (`pins.rs`: argon2id PHC strings in `pins/<id>`, backoff
  in `pins/<id>.state` under `flock`, 3 free failures then 1 s doubling to 5 min, the lock counted
  from when the hash check ended; **no wait for the admin profile's PIN**, and a stale wait file of
  the admin is ignored and cleared at service start; instead **10 wrong admin-PIN tries a boot**
  (owner): `pins/<id>.tries` = `{boot_id, failures}` (the kernel's
  `/proc/sys/kernel/random/boot_id`, 0600, same `flock`), counted from every path (system.sock
  and the PAM helper) before the hash check, so tries at the same time never pass the limit; after
  the tenth, every check of the admin PIN, the right one too, is refused until the boot id changes
  (a reboot; a service restart keeps it). A right PIN before that, a new PIN and `reset-pin` reset
  the count; the try that starts the lock is audited (`HINTEROS_PIN_OUTCOME=lockout_started`)), the public view `/run/hinteros/profiles-public.json` (0644) and
  the switch hint with one-time tokens `/run/hinteros/switch.json` (0600) (`public.rs`),
  `runtime.json` → `/run/hinteros/gamescope-modes` and `audio-output`, `usage.json`
  (`hinteros-system usage`, started over D-Bus when `profiles.list` sees data older than an hour).
- Methods (`service/profiles.rs`): `profiles.list/create/setup_admin/rename/set_appearance/
  set_pin/set_locked/reset_pin/remove/switch/switch_cancel/unlock`, `guest.set`, `starter_set.get/set`,
  `display.save_mode`, `audio.save_output`; event `profiles_changed`. Which PIN each checks:
  own changes `self`, anything on another profile or the box `admin`; a profile without a PIN yet
  passes. `pin_required` (3016) names it, with `retry_after_ms` during the lock and
  `reboot_required: true` while the admin PIN is refused until a reboot; the audit has
  `HINTEROS_PIN_CHECK`/`_PIN_OUTCOME` and every PIN field is `***` in `HINTEROS_PARAMS`.
  `profiles.create`/`remove`/`guest.set`/`starter_set.set` are jobs (`profiles_create`,
  `profiles_remove`, `guest_set`, `starter_set`); a create writes metadata, PIN hash and the
  starter env only after the account's switch; a removal ends the account's processes, switches
  without it, then deletes the home with `datadel`'s no-symlink walk, the PIN, the metadata and the
  app links. Profiles (and the guest) without an env get the starter env while the box is idle.
- **Admin PIN for box-wide actions**: `rollback`, `update.apply`, `channel.set`, `base.set`
  and `hardware.redetect` take `admin_pin`; it is checked first (`pin_required
  {pin: admin}` when missing or wrong, or without an admin profile), from every profile, the
  guest included (installing a system update always needs it). Not for root; an
  admin profile without a PIN yet passes. `update.check` stays open.
- **End guest session** (`guest.rs`, `runner/profiles.rs`): `guest.end` (the guest only;
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
  the PIN on stdin; prints only `hinteros:wrong` / `hinteros:locked:<s>` / `hinteros:reboot`, which
  the session daemon passes on as `greeter_result {reboot_required: true}`). `splash`
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
  inhibitor; `lock_changed`, `session.lock_state`, `session.unlock`), `profiles_changed {profiles}`.
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

### What the launcher must do

Pass `admin_pin` to `system.rollback`, `system.update.apply` (every
profile, the guest too), `system.channel.set`, `system.base.set` and `system.hardware.redetect` (ask on `pin_required {pin: admin}`); say
"wiped now" for End guest session; show `readds_features` on the rollback screen. New: the methods and events above (schema:
`schema/session-api.json`), all `system.*` profile, apps and starter-set relays; the first-boot
wizard when `profile.current.setup_required`; "Set your PIN" when `!pin_set`; the terms dialog
for `apps.list` entries with `needs_terms`; pass `system.apps.list`'s
`revision` to `system.apps.install/remove`.

## Storage and clean-up

Design: `hinteros-docs/design/storage.md`. Every method below is relayed as `system.<method>`.

### Measuring (`storage.rs`, `runner/storage.rs`)

- `storage.measure` (polkit `org.hinteros.system.settings`, any profile, no PIN) starts job
  `storage_measure` (a running one is returned instead of a second). Stages `preparing`,
  `measuring` (with `percent`), `finishing`; cancellable. It reads statvfs of `--store-dir`,
  `nix path-info --all -s` (NAR sizes), `nix-store --gc --print-live` (garbage = all − live),
  `nix-store -qR` of every system and app generation, and walks each home (`state.json`'s
  accounts, then the guest): no symlink followed, no other filesystem entered, no file read
  except Steam's `libraryfolders.vdf` and `appmanifest_*.acf` (games named one by one, each
  library folder that is a real folder inside the home), hard links counted once. `--srv-dir`
  (`/srv`) is walked the same way for its total only.
- Trees: children always add up to their parent (remainders are `common`, `shared`, `files`,
  `other`); home trees are cut at 6 folder levels, max(1 MiB, 0.5 %) per folder, 16 folders per
  folder, 400 nodes; names at most 128 characters. Top level: `system` (generation / common),
  `apps` (`app_env` per owner outside the system, generation / common; `shared` = in two or more
  owners' envs), one `profile` per account, `guest`, `media` (`/srv`, a total, when not empty),
  `other`, `free`.
- **Btrfs.** Whether a home or `/srv` is on the store's filesystem comes from the mount source in
  `/proc/self/mountinfo` of the longest mount point above it: every subvolume has its own
  `st_dev` and `f_fsid`. NAR sizes and `st_blocks` are uncompressed, so with `compress=zstd` the
  known total can pass what the disk uses (total − free): the known segments on that filesystem,
  with their children and detailed trees, are then scaled down in proportion to fill it exactly
  (largest remainder, children still add up) and `other` is 0. Up to that, nothing is scaled.
  The clean-up estimates, `garbage_bytes` and `profiles.list`'s `usage` stay uncompressed sizes.
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
  single link untouched), then GC.
- Both serialise with the system's `nix-gc.service` through Nix's own GC and profile locks.

### Automatic clean-up setting

`state.json` `settings.cleanup {enable, frequency: daily|weekly|monthly, keepSystem, keepProfiles
(1..10)}`, every field optional (defaults on, weekly, 5, 5), schemas 1 and 2, checked by
`State::parse` and so by `check-state`. `cleanup.get` (read-only) returns it with the defaults
filled in; `cleanup.set {enabled, frequency, keep_system, keep_profiles, admin_pin?}` (admin PIN,
polkit `org.hinteros.system.settings`) writes all four fields through the apply flow (job
`cleanup_set`, `null` when nothing changes, also for the defaults while absent).

### What the system repo must do

- `state.nix`: accept `settings.cleanup` as above; `nix.gc.automatic` from `enable`, `dates` from
  `frequency`, the system preStart `--delete-generations +keepSystem` and
  `boot.loader.systemd-boot.configurationLimit` from `keepSystem`, the app-env preStart from
  `keepProfiles` (absent: today's values). Suggested: skip the booted generation in the preStart.
- `hinteros-job@.service`'s `path` needs `nix-store` (already there with `nix`). No new polkit
  action (existing ones are reused).

### What the launcher must do

Settings → Storage: show `system.storage.get`'s measurement at once with its
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

## Job progress

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

`building.percent` is how far that one derivation is, from
the last standard stdenv phase it started: unpack 0, patch 5, updateAutotoolsGnuConfigScripts 8,
configure 10, build 20, check 80, install 88, fixup 94, installCheck 98, dist 99 (`PHASES`). An
unknown phase keeps the last value, it never goes back and stays below 100; absent before the
first standard phase and for builders without phases (`runCommand`, trivial builders). The
builds part of the job's `percent` is `(builds_done + the running builds' fractions) /
builds_total`. Nix counts a build as done just before its build activity stops: the most
advanced running build then stops counting by its fraction (when another one stops first, it was
that one); a build that stops before nix counts it counts as 1 until nix does.

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
check learns its target `rev` and `version` once verified: one `job_progress` then carries the
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
| `channel_check` | `channel` (target), `from_channel`, `from_rev`; `rev`, `version` once verified |
| `channel` | `channel` (target), `from_channel`, `from_rev`, `from_version`, `rev`, `version` (the confirmed revision, from the start) |
| `base` | `base`, `from_base` |
| `rollback` | `generation`, `from_generation` |
| `apps_install` | `owner`, `profile` (a profile), `apps` (only the new ones); the automatic starter env of a new profile or the guest (uid 0): `owner`, `profile`?, `from_owner: starter` |
| `apps_remove` | `owner`, `profile`?, `apps`, `delete_data` |
| `apps_rollback` | `owner`, `profile`?, `generation`, `from_generation` |
| `starter_set` | `owner: starter`, `apps` (the whole new set) |
| `profiles_create`, `profiles_remove` | `profile` |
| `guest_end` | `owner: guest` |
| `cleanup_system`, `cleanup_profiles` | `keep` |
| `cleanup_set` | `enabled`, `frequency`, `keep_system`, `keep_profiles` |
| `services_enable` | `services` |
| `services_disable` | `services`, `delete_data` |
| `jellyfin_library_add` | `library {name, kind, folder}`; `library.id` once Jellyfin made it (in `job_progress`) |
| `jellyfin_library_remove` | `library {id, name}` |
| `plex_claim`, `tailscale_login`, `tailscale_logout`, `cloudflare_set_token`, `cloudflare_clear_token` | none (the kind says it) |

`Job` also has `started_at` (equals `created_at`) and, for a running job, the latest `percent`,
`detail` and `last_activity_at` (null once finished); `job_finished` has `started_at` and
`finished_at`. The job's spec and `status.json` carry the subject, so it survives a restart of the
service.

### What the launcher must do

On the job page and
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

## Time zone

Design: `hinteros-docs/design/time-zone.md`. Code: `timezone.rs` (zones, offsets, list, the
link), `service/timezone.rs` (the methods).

- `state.json` `settings.timeZone`: an IANA name, absent means `UTC`, schemas 1 and 2. `State::parse`
  (and so `check-state`) checks its shape only (the system repo's rule: `/`-separated parts
  `[A-Z][A-Za-z0-9_+-]*`, at most 64 bytes); a well-formed zone tzdata does not have reads as UTC.
- Offsets come from the system's tzdata (`--zoneinfo-dir`, `/etc/zoneinfo`; parsed with `tz-rs`),
  the files `/etc/localtime` points into, never from a database in the daemon.
  `offset_s` is the offset now; `standard_offset_s` the lower of the zone's two
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
  state was computed) is kept. A rollback restores the generation's `state.json` but keeps a
  live zone, as `hinteros-config-sync` does at boot, and then points `/etc/localtime` at the
  zone that applies (the switch ran before the restore); the service sends `time_zone_changed`
  after any job when the zone that applies changed.

### What the system repo must do

- `state.nix`: accept `settings.timeZone` (a string; the shape above). `time.timeZone = null`;
  an activation script points `/etc/localtime` at `/etc/zoneinfo/<zone>` from the live
  `/etc/hinteros/state.json` (read at run time; UTC when tzdata lacks it).
- `hinteros-config-sync`: `time_zone.set` changes `/etc/hinteros/state.json` in place without a
  new generation, so the running generation's snapshot differs in `settings.timeZone` only.
  The sync must not undo that at the next boot: compare `state.json` without `settings.timeZone`
  and keep the live zone when it restores, after a rollback too (as the daemon's own rollback
  does).
- `hinteros-time-zone-apply.service`: oneshot, root, `ExecStart=hinteros-system
  time-zone-apply`, `ProtectSystem=strict`, `ReadWritePaths=/etc /run/hinteros`, no network.
  The service only needs `/run/hinteros` writable (no `/etc`) and may start this unit (it is
  root; systemd's manager API). No new polkit action.

### What the launcher must do

`system.time_zone.get` → `{zone, offset_s, standard_offset_s,
offset_valid_until}`; `system.time_zone.list` → `{zones: [{zone, offset_s,
standard_offset_s}]}`; `system.time_zone.set {zone, admin_pin?}` → the new `TimeZoneInfo`
(no PIN in the first-boot wizard; in Settings ask for the admin PIN on `pin_required`). Event
`time_zone_changed` (a `TimeZoneInfo`): subscribe to it. The clock shows UTC plus
`offset_s` and gets the zone again at `offset_valid_until` and on `time_zone_changed`.

## Admin PIN limit

The admin profile's PIN has no wait (DECISIONS J4), so without a limit any process on the box
could try every 4-digit PIN through system.sock in hours. The owner's rule: "After 10 failed
attempts require that the box needs to be restarted before it can be attempted again."

- Every check of the admin profile's PIN counts: `admin_pin` of the box-wide actions, `pin`
  where an admin method asks for it (`pin: admin`), the admin profile's own changes (`pin: self`),
  a switch to it (`pin: target`) and the greeter (PAM helper). The count is per boot
  (`pins/<id>.tries`, see "Profiles, PINs, greeter" above), so restarting `hinteros-system` does
  not reset it; a reboot does.
- The tenth wrong try and every check after it, also with the right PIN: `pin_required {pin,
  reboot_required: true}` (message "too many wrong PINs: restart the box"); the PAM helper prints
  `hinteros:reboot` and the greeter gets `greeter_result {ok: false, reboot_required: true}`.
  The try that starts it is audited (`HINTEROS_PIN_OUTCOME=lockout_started`, the profile, the
  method or `greeter`, the uid).
- Recovery without a reboot: `sudo hinteros-system reset-pin <admin>` (root) or
  `profiles.reset_pin` on the admin profile (polkit `profiles.recover`: the admin account's
  password); both set or remove the PIN, which resets the count. A missing PIN is not a try.

### What the launcher must do

On `pin_required` with `reboot_required: true`, and on `greeter_result` with
`reboot_required: true`, show that the box must restart (no retry countdown) and take no more
PIN input on that pad.

## Channel switches

Code: `update.rs` (`SeenTips`, `Rejection::OlderThanSeen`), `runner.rs` (`update_check`,
`channel_check`, `apply`), `service.rs` (`channel.check`, `channel.set`).

`channel.set` never locks whatever signed commit the target branch points at: a server that
serves an old release with a known hole on that branch must not get it installed.

- **Newest verified tips** (`/var/lib/hinteros/channel-tips.json`, root only, `{"version": 1, "tips": {"<branch>":
  "<rev>"}}`): every job that verifies a channel tip records it there, but only forward (a
  revision that descends from the recorded one). `update.check` also fetches and verifies the
  other branches of the channel table (one extra `git fetch` each; failures are only logged), so
  the record exists before a switch. Rollbacks and switches never lower it.
- Every verification (`update.check`, `update.apply`, `channel.check`, `channel.set`) also
  requires the revision to descend from (or equal) the recorded tip of its branch, else
  `update_verification_failed {reason: older_than_seen}`. Within the followed channel the
  fast-forward rule from the locked revision stays as it was.
- `channel.check {channel}` (polkit `org.hinteros.system.update.check`, no PIN; job
  `channel_check`; `job_id` null for the current channel): fetches and verifies the target's tip.
  It may be older than the locked revision. Result `ChannelCheckResult {channel, current_rev,
  target: AvailableUpdate, older, checked_at}` (`older`: `target.rev` does not descend from the
  locked revision; `new_apps`/`removed_apps` are null), also kept in
  `/var/lib/hinteros/channel-check.json`. The job's subject gets `rev` and `version`.
- `channel.set {channel, rev?, admin_pin?}`: the admin PIN first; `job_id` null for the current
  channel; otherwise `rev` must be the last `channel.check` result's `target.rev` for that channel
  and the locked revision (`state_conflict {reason: channel_rev}`). The job re-verifies `rev`
  (signer and record), locks it on the target branch and runs the apply flow. The subject has
  `rev` and `version` from the start. `acknowledge_downgrade` is gone (an unknown field).
- The promote flow never trips the record: `promote` only fast-forwards `beta` and `stable`
  (hotfixes are released onto `stable`), and `main` is not a channel. A branch that is ever
  rewound on the server is refused by every box that saw the newer tip.

### What the launcher must do

To switch channels: `system.channel.check {channel}`, wait for its job, show
`result.target.version` and `result.target.date` (and, when `result.older`, that the version is
older than the one on the box), and on confirm call `system.channel.set {channel, rev:
result.target.rev, admin_pin}`. `update_verification_failed {reason: older_than_seen}` (from either
job): the server offers an older version than one the box already saw; nothing changed.
`state_conflict {reason: channel_rev}`: check again. Job kind `channel_check`.

## App sandbox and permissions

Design: `hinteros-docs/design/sandbox.md`. Code: `sandbox.rs` (the bwrap command line),
`permissions.rs` (the profile's choices), `apps.rs` (`ProcessSpawner`), `reaper.rs`, `lan.rs`
(the session side of `lan`), and in the system service `lanfilter.rs` and `service/lan.rs`.

- **Sandbox** (`[sandbox] enable`, default off; the system turns it on): every configured
  command app with `sandbox = true` (the default) runs as reaper -> bwrap -> app. Not Steam, not
  Steam games, not `sandbox = false` apps, not `apps.launch {command}`. bwrap gets
  `--unshare-all --share-net --die-with-parent --new-session`; read-only `/nix/store`, `/etc`,
  `/sys`, `/run/current-system`, `/run/opengl-driver{,-32}`, `/run/systemd/resolve` and
  `/run/nscd` if present, `/bin` and `/usr` if present, the profile's app env (the
  `[apps] profile` link, resolved); a new `/proc`, a minimal `/dev` with the GPU render nodes
  (`/dev/dri/renderD*`, not the display nodes `card*`), empty `/tmp` and `/run`.
  Home: `~/.local/share/hinteros/sandbox/<id>` (created 0700 before the spawn) at the real `$HOME`
  path; the app's data starts fresh there and `firefox_profile` is resolved
  inside it (icons read the favicon there). Runtime dir: a new `$XDG_RUNTIME_DIR` with only
  `pulse/native` and, for web and keyboard-profile apps, `hinteros/osk.sock`. Display: only
  `/tmp/.X11-unix/X<n>`; `DISPLAY`, `HOME`, `XDG_RUNTIME_DIR` set, Wayland displays, D-Bus
  addresses and other sockets unset. Nothing else of `/run` (`/run/hinteros`, `/run/dbus`,
  `session.sock`, `bus`, `pipewire-0`, `gamescope-0`), `/dev/uinput` or the real home is there.
- bwrap is looked up (`[sandbox] bwrap`, a path or a `PATH` name) before each sandboxed spawn; if
  it is missing the launch fails (`app_launch_failed`, "no sandbox: bwrap not found"), it never
  runs the app unsandboxed. The daemon's unit must not have `ProtectKernelTunables` (bwrap
  cannot mount `/proc` then).
- **System call filter** (`sandbox/seccomp.rs`): bwrap loads a seccomp BPF program (`--seccomp
  <fd>`, a pipe the daemon fills and the reaper and bwrap inherit) before it starts the app. A
  Flatpak-style denylist: keyrings, io_uring, `bpf`, `perf_event_open`, `userfaultfd`, NUMA
  policy, modules, kexec, reboot, swap, clocks, `syslog`, quotas, `acct`, `open_by_handle_at`,
  `iopl`/`ioperm`, `vhangup`, obsolete calls (`EPERM`); the new mount API (`ENOSYS`); `ioctl`
  `TIOCSTI`/`TIOCLINUX`; `personality` other than Linux/32-bit/`UNAME26`/query; sockets other
  than Unix, IPv4, IPv6, netlink (`EAFNOSUPPORT`). Only the x86-64 ABI (i386 kills the process,
  x32 gets `ENOSYS`). User namespaces, `mount`, `pivot_root`, `chroot` stay allowed (Firefox,
  Chromium and Plex's FHS bwrap sandbox themselves inside). No filter, no launch
  (`app_launch_failed`, "no sandbox: no system call filter"); bwrap that cannot load it starts
  nothing.
- `--new-session` puts the app out of reach of the reaper's group signal, so the reaper sends
  forwarded signals to the process group of each process below it whose parent is bwrap, and
  not to bwrap (bwrap dies on SIGTERM and takes the sandbox with it; it gets the signal only
  before it has started the app). So `apps.close` stays a clean SIGTERM first, an app can still
  ignore it, and the SIGKILL after `close.timeout_ms` goes to every process below the reaper.
- **Permissions**: the catalog declares them (`permissions = { gamepad = "required"; mic =
  "optional"; }`, in the app-env manifest entry), `gamepad` (`/dev/input`, `/run/udev`), `mic`
  (`[sandbox] pulse_record_socket` at `pulse/native` instead of `pulse_socket`), `downloads`
  (`~/Downloads`, created, read-write). The profile's opt-outs are in
  `$XDG_STATE_HOME/hinteros/permissions.json` (`{"version":1,"denied":{"<id>":["mic"]}}`,
  written atomically); no record means every permission, a required one is never denied, a
  change applies at the next start. Only enforced permissions are listed: none while the sandbox
  is off, and not `mic` while `pulse_record_socket` equals `pulse_socket` (both default
  `pulse/native`), and not `lan` without `[sandbox] lan_filter`. `[sandbox]` changes need a
  daemon restart.
- **`lan`** (`[sandbox] lan_filter`, default off; the system turns it on): a sandboxed app without
  it (not declared, or denied) reaches neither the home network nor this box's own services; DNS
  to the box's nameservers and the internet work. The daemon's unit has `Delegate=yes` (and no
  `ProtectControlGroups`): at start the daemon moves itself into the leaf `<unit cgroup>/daemon`
  and creates `<unit cgroup>/nolan`, then calls the system API's `lan_filter.set {cgroup}` (at
  once, then after 1 s doubling to 30 s, and at every launch while it is not on). The daemon
  always stays in `daemon`. An app without `lan` gets `reaper ... Cgroup=<nolan dir> -- ...`: the
  reaper writes its own pid to `nolan`'s `cgroup.procs` before it starts anything (so bwrap and
  the app run there too) and then writes `1` to its stdin, a pipe the launch waits on (5 s). If
  it cannot move, it starts nothing and exits 125; the launch fails. A sandboxed app cannot
  leave `nolan` (`/sys` is read-only inside, a cgroup namespace reaches only below its root).
  Fail closed: until the system says the filter is on (or when the cgroups cannot be set up, or
  the reaper cannot enter `nolan`) such a launch fails (`app_launch_failed`, "no local network
  filter"), it never runs with the network open. Apps
  with `lan`, Steam, `sandbox = false` apps and `apps.launch {command}` are not filtered. At
  shutdown, after the apps, the daemon calls `lan_filter.clear`.
- `apps.list` and `store.list` entries carry `permissions: [{name, required, granted}]`
  (display order gamepad, mic, downloads; empty for Steam and unsandboxed apps; for a Store app
  that is not installed, the stored choice). `apps.set_permission {id, permission, granted}`
  takes installed apps and catalog apps not installed yet; errors `app_not_found`,
  `unknown_permission`, `permission_required`; emits `apps_changed` and `store_changed`.
- Apps get no `WAYLAND_DISPLAY` / `GAMESCOPE_WAYLAND_DISPLAY` with or without the sandbox
  (gamescope's private protocols); only Steam keeps them.
- **Own display** (`displays.rs`; `[gamescope] apps_display`, the system sets `"auto+2"`): every
  app under the reaper (sandboxed or not, also `apps.launch {command}`) gets an Xwayland of its
  own as `DISPLAY` and, in the sandbox, only that socket, so no app can read, grab or type into
  another (or into the launcher and the OSK, which stay on `display`). Steam and its games keep
  `display` and take none. The app displays are `apps_display` and the ones after it
  (`"auto+2"`, `"auto+3"`, ...); how many this session has is in
  `$XDG_RUNTIME_DIR/hinteros/app-displays` (written by the compositor wrapper, which starts
  gamescope with `--xwayland-count` 2 + that number), else `app_displays`. Each is resolved at
  each launch (this user's live X displays, lowest first). A launch takes the first one that
  exists, that no running app of ours has, and on which no reaper is left (`DISPLAY` in
  `/proc/<pid>/environ`: an app given up after SIGKILL, or one from before a daemon restart). A
  reaper lives until every process below it has ended, so a display is free once its app's
  whole process tree is gone (`app_exited` follows the reaper's exit). None free:
  `no_app_display` (2009) with `data.apps`, the running apps that hold one; nothing is closed
  automatically. None exists: `app_launch_failed` ("no app display"), never falling back to
  `display`. gamescope's Xwaylands must not listen on abstract sockets (the system patches
  that), else `--share-net` still reaches every display. One watcher per app display unmaps GL
  dummies (`unmap_gl_dummies`). Under cage (no `[gamescope]`) apps keep the session's display
  and nothing limits them.
- **How many** (`apps.displays.get` / `apps.displays.set {count}`): the profile's setting, 1 to 8,
  default `app_displays`, in `$XDG_STATE_HOME/hinteros/app-displays` (one number, written
  atomically). Both answer `{count, active, min, max}`: `active` is what this session started
  with (`null` without app displays). gamescope makes its Xwaylands at start only, so a new
  `count` applies at the next session start (`session.restart`).

### What the system repo must do

`session.toml`: `[sandbox] enable = true`, `bwrap = "<bubblewrap>/bin/bwrap"`, and, once a
play-only pulse socket exists, `pulse_socket` / `pulse_record_socket` (relative to
`XDG_RUNTIME_DIR`). For `lan`: `[sandbox] lan_filter = true`, `Delegate=yes` and no
`ProtectControlGroups` on the session daemon's unit, `HINTEROS_LAN_FILTER=true` for hinteros-system, and the
oneshot `hinteros-lan-filter.service` running `hinteros-system lan-filter-apply --nft <nft>` as
root with `CAP_NET_ADMIN` and `AF_NETLINK` (not in a private network namespace), started again
when `/etc/resolv.conf` or the network changes. Drop `ProtectKernelTunables` from the session daemon's unit. Under gamescope:
`--xwayland-count` 2 + the profile's `$XDG_STATE_HOME/hinteros/app-displays` (default 4), that
number written to `$XDG_RUNTIME_DIR/hinteros/app-displays` before gamescope starts, Xwaylands
without abstract sockets, every one but `#1` sized to the output and without `LIBEI_SOCKET` from
`#2` on, and `[gamescope] apps_display = "auto+2"`, `app_displays = 4`.

### What the launcher must do

Show `permissions` on the install sheet and the app page (required ones
locked); call `apps.set_permission` for each optional permission whose switch differs from
`granted`, then `system.apps.install`. When `full_access` is true (Steam, `sandbox = false`, or the
sandbox is off), the install sheet warns that the app can use everything the profile can.
On `no_app_display` from `apps.launch`, ask which app of `data.apps` to close (or Cancel), then
`apps.close` it, wait for its `app_exited` and launch again. Settings: `apps.displays.get` /
`apps.displays.set`; when `count` differs from `active`, offer `session.restart`.

## Local network filter

The system side of the `lan` permission (design: `hinteros-docs/design/sandbox.md`; the session
side is in "App sandbox and permissions"). Code: `lanfilter.rs`, `service/lan.rs`.

- `lan_filter.set {cgroup}` and `lan_filter.clear {}`: the session daemon's own calls, not
  relayed. They only restrict the caller's own processes, so no seat or polkit check; rate
  limited (a bucket of their own, at least 30 a minute, burst 10) and audited (no action).
  `cgroup` must be below `/user.slice/user-<uid>.slice/user@<uid>.service/` of the caller's uid
  (parts of `[A-Za-z0-9._@:-]`, no `.` or `..`, at most 16 below the manager, 512 bytes), a
  directory under `--cgroup-root` owned by the caller; else `invalid_params {field: cgroup,
  reason: bad_format | not_found | not_owned}` (checked before the rate limit). One cgroup per
  uid; a new call replaces it.
- The service writes `<run-dir>/lan-filter.json`
  (`{"version":1,"cgroups":{"<uid>":"user.slice/..."}}`, 0600, atomically; an unreadable file is
  an error, never an empty filter), drops the cgroups that are gone, then restarts
  `hinteros-lan-filter.service` over D-Bus (`RestartUnit`) and waits for its job (30 s; a failed
  job, e.g. a run the path unit started that the restart ended, is tried once more); a failure is
  `internal_error` and the table stays what it was. Without
  `--lan-filter`: `internal_error`. The service itself has no network and no `CAP_NET_ADMIN`.
- `lan-filter-apply` (the unit) reads the state file (cgroups that are gone are left out), the
  nameservers of `/etc/resolv.conf` (scope ids dropped) and the on-link routes of
  `/proc/net/route` and `/proc/net/ipv6_route` (up, no gateway, not `lo`, not default), and loads
  `table inet hinteros_lan` with `nft -f -` in one transaction (no cgroup: the table is removed).
  Its output hook sends the sockets of each cgroup (`socket cgroupv2 level <n> "<path>"`, so the
  cgroups below it too) to a chain that accepts port 53 (UDP, TCP) to the nameservers, then
  refuses (TCP: reset, so `connect` gets `ECONNREFUSED`; anything else: dropped, so the sender
  gets `EPERM`) every destination in 10/8, 172.16/12, 192.168/16, 169.254/16, 100.64/10, 127/8,
  224/4, 255.255.255.255, fc00::/7, fe80::/10, ff00::/8, ::1 and the on-link networks (nested
  ones merged), and every address of type local, broadcast or multicast (`fib daddr type`: the
  box's own public or global addresses too). Everything else passes: the internet.
- nftables resolves a cgroup path to its id when it loads the table: a cgroup removed and created
  again (a session daemon restart) is matched only after the next load, which the new daemon's
  `lan_filter.set` does before any app without `lan` starts.
- The system repo runs the unit again when `/etc/resolv.conf` changes (a path unit) and when
  NetworkManager changes a connection (a dispatcher hook): new nameservers, new on-link networks.

Why this and not cgroup BPF hooks (`connect4/6`, `sendmsg4/6` from hinteros-system): those need a
BPF loader and `CAP_BPF`/`CAP_SYS_ADMIN` in the service, pinned programs to outlive a restart,
and they see neither ICMP nor raw sockets. Not a network namespace per app with pasta: a
userspace copy of every packet (video streams) on a small box, pasta forwards to the home network
anyway (it filters ports, not destinations), and multicast, IPv6 and DNS need their own handling.
nftables matches every protocol in the kernel, on any firewall backend (its own table), with
`nft` doing the work in a small unit of its own.

## Server section (M10)

Design: `hinteros-docs/design/server.md`. Box-wide catalog services (`kind = service`) and their
setup flows. Code: `servers/` (HTTP client, Jellyfin, Plex, Tailscale, Cloudflare, media folders,
the local IP), `service/servers.rs`, `runner/servers.rs`. Every method is relayed as
`system.<method>`.

- **Catalog:** service entries' `ports`, `setup` (`jellyfin-server|plex-media-server|tailscale|cloudflare-tunnel`; another
  value is on/off only) and `stateDirs` (checked like `dataPaths`: relative to `/var/lib`, no
  `.`/`..`, `[A-Za-z0-9._-]` parts); all optional.
- **`services.list`** (read-only, open): `{revision, services: [{id, name, available, enabled,
  status, setup, terms, detail, icon?, has_data}]}` for every available service and every enabled
  one. `icon`: the catalog entry's `icon` (`desktop_id`, `icon_url` with `sha256`, `site_url`,
  `firefox_profile`; absent when the catalog no longer has the entry); the session daemon adds
  `icon.path` when relaying it, resolved and cached like `store.list`'s catalog icons (the
  catalog's service entries are resolved too; `store_changed` when one arrives). `has_data`:
  `delete_data` has something to delete (the entry has `stateDirs`; Jellyfin always). `status`:
  the unit's `ActiveState` (`jellyfin.service`, `plex.service`, `tailscaled.service`,
  `hinteros-cloudflared.service`). `detail` (enabled services with a setup, no secrets): Jellyfin
  `{setup_done, url, libraries, users}` (libraries live from Jellyfin, else the last list a job or
  read cached in `libraries.json`: Jellyfin restarts during updates, and the dashboard can change
  libraries, so a cache alone would go stale), Plex `{claimed, server_name, url}` from
  `Preferences.xml` (the token never leaves `servers/plex.rs`), Tailscale `{state, name, ip4,
  dns_name, tailnet, key_expiry}` from the LocalAPI socket, Cloudflare `{token_set, connected,
  local_ip}` (cloudflared's `/ready`). `url`/`local_ip`: the box's IPv4 in the default route's
  subnet, from `/proc/net/route` and `/proc/net/fib_trie` (no socket, no netlink). Each service is
  asked once with a 1 s timeout on the blocking pool (the service's runtime is single-threaded);
  when the service has no loopback socket (the system repo allows `AF_INET` only while Jellyfin,
  Plex or Cloudflare is on) the fields fall back to false/null/cache.
- **`services.enable {ids, pin, revision, accepted_terms?}` / `services.disable {ids, pin,
  revision, delete_data}`** (jobs `services_enable`, `services_disable`): admin PIN,
  `state.json` `services` through the apply flow. Plex's terms as for apps (recorded by the
  caller's user with the admin profile's id). `delete_data`, after the switch: each `stateDirs`
  entry below `/var/lib`, for Jellyfin also `/var/cache/jellyfin` and
  `/var/lib/hinteros/services/jellyfin-server` (so `users.json` goes with Jellyfin's users), with
  `datadel::delete_under` (no symlink followed, the base included; one filesystem); the job
  refuses a base outside `/var/lib`/`/var/cache`. Enabling Jellyfin then waits up to 3 min for
  the setup unit's marker (stage `configuring`) and makes the profiles' users.
- **`media.folders {folder_id?}`** (read-only): one level below `/srv/media`. A folder id is the
  path relative to it (no empty, `.`, `..` or hidden part, no control characters), opened part
  by part with `O_NOFOLLOW | O_DIRECTORY` every time it is used (the request, and again in the
  job): symlinks are neither listed nor entered.
- **Jellyfin** (`servers/jellyfin.rs`): HTTP on loopback with `Authorization: MediaBrowser
  Token="<api-key>"` (Jellyfin 10.11+/12 refuse the legacy `X-Emby-Token` unless legacy
  authorization is on; an API key counts as administrator). `jellyfin.libraries.add {pin, name,
  kind, folder}` (job: `POST /Library/VirtualFolders` with the path in
  `LibraryOptions.PathInfos`, since one `paths=` value is split at commas; `collectionType`
  movies/tvshows/music, none for mixed; a duplicate name is refused first, as Jellyfin would
  rename it `Name2`), `jellyfin.libraries.remove {pin, library_id}` (job: by the library's name;
  the media stay). Profile users: `users.json` (0600, under `users.lock`) maps a profile id to
  `{jellyfin_user_id, name, password, access_token?}`; `sync` makes one user per profile (not the
  guest), named after the profile (letters, digits, space, `_-'.@+`; unique in any case with
  " 2"… added; `Profile <n>` when nothing is left), not an administrator and without media
  deletion (the policy is posted back whole), renames it with the profile (the whole user with its
  `Configuration` is posted back), deletes it with the profile; dashboard-made users are left
  alone. It runs after profile create/remove jobs, after a rename (in the background), at the end
  of `services_enable`, in every Jellyfin job and in `jellyfin.users.reveal {pin}` (no job; the
  admin's and each profile's password; never logged). `jellyfin.client_token` (no PIN, read-only
  for the caller): the caller's own profile only (from its uid; guest and spare accounts get
  `permission_denied {reason: profile}`): `{server_id, user_id, device_id: "hinteros-<id>",
  access_token}`, minted with `AuthenticateByName` as that device (Jellyfin ends the device's older
  token), kept in `users.json`, minted again when `GET /System/Info` answers 401. The session
  daemon signs the Jellyfin app in with it (below).
- **The Jellyfin app signs in by itself** (spike S1, session side, `appseed.rs`): before
  `apps.launch {id: "jellyfin-desktop"}` (not while it runs, not without `lan`) the session daemon calls
  `jellyfin.client_token` (10 s at most; `unavailable` = the server is off or not set up: nothing
  to do) and writes the client's settings in the app's data home (its sandbox home): in
  `~/.local/share/jellyfin-desktop/`, `profiles.json`'s `defaultProfile` (else the first
  `profiles/<32 hex>`, else one it makes, with `profile.json`) names
  `profiles/<id>/jellyfin-desktop.conf`, where it sets only `path.startupurl_desktop` (the server's
  seed page `http://127.0.0.1:8096/web/hinteros/signin.html#s=<server id>&u=<user id>&d=<device
  id>&AccessToken=<token>`), `main.userWebClient` (`http://127.0.0.1:8096`) and `main.layout`
  (`tv`); the `version` the app wrote stays (7 in a new file: another version makes the client
  move the file to `.broken`). Files 0600, directories 0700, written atomically. Any failure
  launches the app unchanged; the token is never logged. The launch is answered after the seed
  (out of order, like the relayed methods).
- **Jellyfin backup** (`servers/backup.rs`): an apply job (update, rebuild, any state change) or a
  rollback that switches between two systems that both run Jellyfin, in different versions
  (`jellyfin-<version>` in their `etc/systemd/system/jellyfin.service`), first stops
  `jellyfin.service` (D-Bus, waits 90 s), copies `/var/lib/jellyfin/data` with modes, owners and
  links to `/var/lib/hinteros/services/jellyfin-server/backup` (through `backup.new`: one copy,
  replaced each time) and starts Jellyfin again after the switch (also when the switch failed). A
  failed stop or copy fails the job before the switch. Restoring is by hand.
- **`plex.claim {pin, code}`** (job): `claim-` and 20 of `[A-Za-z0-9_-]` (what plex.tv/claim gives;
  Plex documents no exact format, so a change shows as `invalid_params`). The job posts it to
  Plex's own `POST /myplex/claim?token=` on 127.0.0.1:32400 with `X-Plex-Client-Identifier` (the
  server's `ProcessedMachineIdentifier`) and succeeds once `PlexOnlineToken` is set (30 s).
- **`tailscale.login {pin}`** (job): `tailscale up --reset --json --timeout=600s` (defaults: no
  exit node, routes or Tailscale SSH; `--force-reauth` for an expired key); its pretty-printed
  JSON objects are decoded as a stream; each new `AuthURL` (https, printable ASCII, ≤ 512) is
  `job_progress {stage: signing_in, detail: {auth_url}}`, sent (and shown by `jobs.get` and
  `jobs.list`) only to connections of the uid that started the job and of root: whoever opens
  the URL first adds the box to their tailnet; done at `{"BackendState":"Running"}`
  and exit 0. `jobs.cancel` kills the CLI. `tailscale.logout {pin}` (job) runs `tailscale
  logout`. Both answer `job_id: null` when the state is already the asked one.
- **`cloudflare.set_token {pin, token}`**: checked strictly (cloudflared's `ParseToken`: padded
  standard base64 of `{"a": 32 hex, "t": UUID, "s": base64 of 32..256 bytes, "e"?: endpoint}`,
  no other or repeated key, ≤ 512); the service writes it 0600 atomically (the directory 0700) so
  it is never in a job spec; the job restarts `hinteros-cloudflared.service` (D-Bus) and waits 30 s
  for `/ready`. `cloudflare.clear_token {pin}` (job) stops the unit and deletes the file.
- polkit action `org.hinteros.system.services` for every write (the admin PIN is the service's
  check); `services.list`, `media.folders`, `jellyfin.client_token` are reads. PINs, claim codes
  and tokens are `***` in `HINTEROS_PARAMS`. Job failures carry `job_failed.data.reason`
  (`unreachable`, `not_ready`, `refused`, `timeout`, `not_connected`); methods on a service that
  is off or not set up: `unavailable {service: jellyfin|plex|tailscale|cloudflare, reason: off |
  not_ready}`.

Flags (all paths default to the box's; tests point them at a temporary root and fakes):
`--media-dir` (`/srv/media`), `--var-lib-dir`, `--var-cache-dir`, `--jellyfin-addr`
(`127.0.0.1:8096`), `--plex-addr` (`127.0.0.1:32400`), `--plex-preferences`,
`--tailscale-socket` (`/run/tailscale/tailscaled.sock`), `--tailscale` (the CLI, for jobs),
`--cloudflared-metrics` (`127.0.0.1:20241`), `--tailscale-login-timeout-s` (600). Every address
must be loopback (the HTTP client refuses anything else).

### What the system repo must provide

As agreed (system repo `m10-server`): `/var/lib/hinteros/services/<id>/` while a service is on;
Jellyfin's `setup-done`, `admin-password`, `api-key` (no trailing newline needed: they are
trimmed); Plex on 127.0.0.1:32400 and its `Preferences.xml`; `hinteros-cloudflared.service`
reading `/var/lib/hinteros/services/cloudflare-tunnel/token` with metrics on 127.0.0.1:20241;
`tailscale` on the `PATH` of `hinteros-job@` and the socket `/run/tailscale/tailscaled.sock`;
`/srv/media`; `AF_INET` + `IPAddressAllow=localhost` for `hinteros-system` while Jellyfin, Plex or
Cloudflare is on. Also: the polkit action `org.hinteros.system.services` in the policy file
(granted to `hinteros` by the existing rule); `hinteros-system` (not only the job unit) must
reach `/run/tailscale/tailscaled.sock` (`services.list`, `tailscale.*`'s pre-check: `AF_UNIX`,
read access) and read `/var/lib/plex/Plex Media Server/Preferences.xml` and `/proc/net/route`,
`/proc/net/fib_trie`; `hinteros-job@` needs `systemd` D-Bus `RestartUnit`/`StopUnit` on
`hinteros-cloudflared.service` (root: allowed), loopback to Jellyfin/Plex/cloudflared, and
writes under `/var/lib`, `/var/cache` (delete_data). For the Jellyfin app: Jellyfin serves
`/web/hinteros/signin.html` (the seed page, which stores `jellyfin_credentials` and `_deviceId2`
from the fragment and opens `/web/index.html`). For the backup: `jellyfin.service`'s `ExecStart`
names the server's store path (`<hash>-jellyfin-<version>`). VM tests (system repo):
`server-api` (every method through the session relay) and `server-jellyfin-signin`.

### What the launcher must do

Server tab: `system.services.list` (re-query on `job_finished` of the new kinds,
on `store_changed`, since a service's icon may have arrived, and after reconnecting; the icon is
`icon.path`, else a letter tile); "Turn on" with the admin PIN (`terms` shown in full, then
`accepted_terms`); job pages from `job_progress` (the new stages `configuring` and `signing_in`;
for `tailscale_login` the QR of `detail.auth_url`, also in `system.jobs.get`); Jellyfin libraries
(`system.media.folders` to pick a folder, `system.jellyfin.libraries.add/remove`), Accounts
(`system.jellyfin.users.reveal`), Plex claim code (`claim-` prefilled), Cloudflare token, Turn
off (+ delete data when `has_data`, second confirmation); `job_failed.data.reason` and
`unavailable {service, reason}` for the messages.

## Controller settings

Per profile (`controller.rs`), in `$XDG_STATE_HOME/hinteros/controller.json`: only what the user
changed, exactly as `input.settings.set` got it (written atomically). A missing file is the
defaults; an unreadable or wrong one too, with a warning (the session never fails on it).

```json
{
  "general":  { "accept": "b", "back": "a" },
  "keyboard": { "back": "backspace" },
  "apps":     { "youtube": { "context": "space" } },
  "pointer":  { "speed": 150, "swap_sticks": true, "left_click": "rt", "right_click": "lt" }
}
```

- **Buttons** are physical, by Xbox label (after the Xbox label swap): `a b x y lb rb lt rt view
  menu`. Guide (Home and its chords) and the directions (D-pad and left stick arrive as the
  same `ui_*` events) are never remapped.
- **`general`** (action -> button over the defaults `accept=a back=b secondary=x context=y
  page_left=lb page_right=rb trigger_left=lt trigger_right=rt view=view menu=menu`): the result
  must give each of the ten buttons exactly one action. The shell maps the ten buttons through
  it before anything reads them: launcher navigation (`input_nav` carries the remapped action),
  keyboard-mode apps, the on-screen keyboard, the Guide chords (`[osk] summon` names an
  action). Apps that get the pad (Steam, games, InputPlumber `Pass`) are not affected. A release
  goes with the action of its press, also when the settings changed in between.
- **Keys of a keyboard-mode app**, lowest to highest: the default keymap (arrows, Enter,
  Escape), `keyboard` (all apps), the app's own (`back_key`, the address bar's Alt+Left/Right,
  `[apps.keymap]`), `apps.<id>`. `none` unmaps. Keys are `KeyCombo` names (`enter`,
  `alt+left`, `ctrl+l`).
- **`pointer`**: `speed` is a percentage of `[pointer] speed_pps` (25 to 400 in steps of 25,
  default 100); `swap_sticks` makes the left stick the pointer (mouse motion and the activity
  event) and maps the right stick's directions to the D-pad buttons (deadzone 0.6, InputPlumber's
  own D-Bus threshold), so they arrive as `ui_up` etc.; both go into the InputPlumber profile.
  `left_click` / `right_click` (default `rt` / `lt`, never the same) are physical buttons, checked
  before `general`; they click unless the app's own `[apps.keymap]` or `apps.<id>` maps their
  action.
- **Live**: `input.settings.set` updates the keymaps of running apps at once; a pointer change
  reloads the pointer profile if it is loaded (a pointer app has focus), else it is used at the
  next load.

Methods (launcher only):

- `input.settings.get {}` -> `{settings, defaults, buttons, actions, key_actions, keys,
  modifiers, speed, apps}`: `apps` are the profile's keyboard-mode apps, each with only its own
  keymap; the launcher computes an app's keys as `defaults.keyboard` + `settings.keyboard` +
  `apps[].keymap` + `settings.apps[id]`, like the session.
- `input.settings.set {settings}` (the whole stored form) -> the same result. `invalid_params`
  with `data {field, reason}` (`unknown_action`, `unknown_button`, `unknown_key`,
  `unknown_app`, `not_bijective`, `same_buttons`, `bad_speed`); nothing is saved then. `apps`
  takes the keyboard-mode apps, and an entry already saved for an app that is gone, sent back
  unchanged (so the launcher can send the whole form back).
- `input.capture {}` -> `{}`: the next press of one of the ten buttons or a direction is not
  acted on; it goes out as `input_captured {button}` (physical, before `general`; its release
  is dropped too) and capture ends. Guide ends it with `{button: null}` and goes Home as usual;
  so does 10 s without a press. A second call restarts the 10 s.

### What the launcher must do

Controller page: `input.settings.get`, edit the stored form, send all of it
with `input.settings.set`; press to assign with `input.capture` and `input_captured` (when the
pressed button already has another action, swap the two); "Reset to defaults" sends `{}` (or
drops one part).

## state.json

`schemaVersion` is 2, the only value read or written; any other is an error that names what is
supported. Required: `schemaVersion`, `base`, `stateVersion`, `requirements`, `services`,
`profiles`. `profiles[]` is `{id}`: the user is `hinteros-<id>` (`hinteros-p1` too) and the uid
20000 + N; the group stays `hinteros`.

Unknown fields load, at every level, and are written back after the known ones, sorted: a box
that switches to an older channel still has to read the newer file. A change that is not safe
to ignore bumps `schemaVersion`. Bad values of known fields are errors. `check-state` follows.
The checks on load are about shape, so a later 1.x may add values without a new schema: an
unknown `base` or `settings.cleanup.frequency` loads and is written back as it was (the API shows
`unstable` and `weekly`), a well-formed box feature this daemon does not know stays in
`requirements`, and the 8-profile limit applies to `profiles.create`, not to loading.

`base.set` to a base that is not built (`state::BUILT_BASES`, `unstable` only) is refused with
`unavailable {service: base, reason: not_built}`, before the downgrade check.

### What the launcher must do

`system.settings.get`: no `settings.apps`; `settings.requirements`
is always there. `migrate_apps` is gone from `JobKind`. The `unavailable` `service` value
`profiles` is gone (`base` takes its place). Profile users are `hinteros-p<N>` for p1 too.

## API versions

Both APIs are 1.0. A client works with a server of the same major and an equal or higher minor
(`ApiVersion::is_compatible_with`); `hello` answers the client's version, and anything else is
`version_mismatch`. Major 0 has no rule of its own.

- **A minor adds:** methods, events, fields, enum values in what a server sends. A client ignores
  what it does not know. Every enum a client only reads has an `Unknown` value
  (`#[serde(other)]`; Jellyfin's library kind has `other`, `Connectivity` and `UnitState` their
  own `unknown`), `ServiceInfo.setup`, `ServiceInfo.detail` and `result` of a job read as null
  when they are not known, and an error code outside `ErrorCode` is `ErrorObject::kind() == None`.
- **Results and events never deny unknown fields.** Only `input.settings.*` still does, in
  `settings`: `ControllerSettings` is the `controller.json` format.
- **Requests stay closed where that protects something.** A server's minor is never lower than
  its client's, so it sees only fields it knows. Every system API params type denies unknown
  fields (a root service). In the session API, so do the ones that carry a credential or choose
  an account: `session.switch_profile`, `session.unlock`, `greeter.pin`, `greeter.login`,
  `greeter.create`, `wifi.connect`, `input.settings.set`, the runtime methods without params
  (`power.reboot {force: true}` must not reboot anyway), and `hello` in both APIs.
- **Enums a client also sends are closed:** a new value of `Channel`, `Base`,
  `CleanupFrequency`, `LibraryKind`, `Key`, `Modifier`, `KeyAction`, `ControllerButton` or either
  `EventName` is a new major. A catch-all there would let an unknown value through to the server
  or into `state.json`.

The schema documents list exactly what their version sends (`additionalProperties: false`), so
the conformance tests catch a server that sends what is not documented. An item a later minor
adds has `since`; an item without it is part of x.0.

## Persisted JSON files

Every JSON file the daemons keep has a top-level `version`, today 1 (`common::check_version`).
Unknown fields are ignored. A newer file is not understood; what follows depends on the file:

| File | Newer file | Unknown fields on a rewrite |
|---|---|---|
| `profiles.json`, `services/jellyfin-server/users.json` | load and save are errors; the file stays | kept |
| `terms-accepted.json` | reads as no acceptances; recording is an error; the file stays | kept |
| `starter.json` | reads as none; saving is an error (the job fails); the file stays | kept |
| `channel-tips.json` | reads as empty; saving fails with a warning; the file stays | kept |
| `runtime.json` | defaults; a change holds until the next boot; the file stays | kept |
| `permissions.json`, `controller.json` (session) | defaults; a change holds in memory; the file stays | dropped |
| `apps/registry.json`, `storage.json` | discarded and overwritten (rebuilt from the env manifests, the next measure) | dropped |
| `usage.json`, `update-check.json`, `channel-check.json`, `libraries.json`, `pins/<id>.state`, `pins/<id>.tries` | discarded and overwritten | dropped |

`{"version":1,"accepted":{"<id>":[{version, accepted_at, by, profile?}]}}` is
`terms-accepted.json`; `{"version":1,"tips":{"<branch>":"<rev>"}}` is `channel-tips.json`;
`{"version":1,"display_modes":{...},"audio_output":...}` is `runtime.json`. The files in the last
row are caches, `{"version":1,"data":...}`. In `/run` and gone at boot, so left out:
`lan-filter.json`, `switch.json`, `profiles-public.json`, job specs and status.

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
`{"jsonrpc":"2.0","id":1,"method":"hello","params":{"api_version":"1.0","client":{"name":"me","version":"0"}}}`,
and `kill -USR1` the daemon to emit `home_pressed` (after `subscribe`). Launch something with
`{"jsonrpc":"2.0","id":2,"method":"apps.launch","params":{"command":["xterm"]}}`.

The systemd units live in the system repo, which consumes `overlays.default`.

Part of [HinterOS](https://hinteros.org). Licensed under GPL-3.0-only, see [LICENSE](LICENSE).
