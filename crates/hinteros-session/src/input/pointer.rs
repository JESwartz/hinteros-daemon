//! The right stick as a mouse pointer in keyboard-profile apps.
//!
//! InputPlumber (0.81) does it for us: a device profile maps `Gamepad:Axis:RightStick` to
//! `Mouse:Motion`, so its virtual "InputPlumber Mouse" moves gamescope's pointer, and the
//! composite device runs in intercept mode `GAMEPAD_ONLY` (3), where events the profile turned
//! into mouse events still reach the mouse target while every gamepad event comes to us over
//! D-Bus (`CompositeDevice::write_event`, src/input/composite_device/mod.rs:1080-1130 in
//! InputPlumber 0.81; `ALL` would send the mouse events to D-Bus too, where they are dropped).
//!
//! The D-Bus target never reports the right stick itself (`event/dbus.rs:229`: every axis but
//! the left stick and the hat is `Action::None`), so the profile also maps any deflection of
//! the right stick to the otherwise unused `QuickAccess2` button, which arrives here as
//! [`ACTIVITY_EVENT`]: "the pointer is being used". Loading a profile replaces the whole
//! default profile (`load_device_profile` clears the map), so [`profile_yaml`] repeats the
//! mappings of InputPlumber's `default.yaml`.
//!
//! The profile's controller settings ([`crate::controller`]) set the speed (a
//! percentage of `[pointer] speed_pps`), which buttons click, and can swap the sticks: the left
//! stick is then the pointer and the right stick's directions are mapped to the D-pad buttons.
//!
//! Clicks (RT left, LT right by default; A is Enter, or the on-screen keyboard on a text field):
//! `CompositeDevice.SendEvent("Mouse:Button:Left" or "Mouse:Button:Right", <bool>)`, which in `GAMEPAD_ONLY` also goes
//! to the mouse target. InputPlumber 0.81 panics in `SendEvent` and never answers (HinterOS
//! patches it); the call is made off the input path with a timeout (`super::inputplumber`).
//!
//! Showing and hiding the pointer ([`PointerState`]): never on inactivity (the system runs
//! gamescope with a `--hide-cursor-delay` too long to matter); the D-pad or left stick hides it
//! at once with gamescope's `paint_cursor_plane` convar (`gamescopectl paint_cursor_plane 0`),
//! the right stick (or RT) shows it again. As gamescope never hides it either, going Home from a
//! pointer app hides it (no stray pointer on the launcher) and the next app shows it again; see
//! [`PointerState`] and [`CursorControl`].

use std::time::Duration;

use tracing::{debug, warn};

/// The `ui_*` event the pointer profile sends while the right stick is deflected.
pub const ACTIVITY_EVENT: &str = "ui_quick2";

/// The `name` of the profile we load (recognised again after a daemon restart).
pub const PROFILE_NAME: &str = "HinterOS pointer";

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
}

impl MouseButton {
    /// InputPlumber's capability string.
    pub fn capability(self) -> &'static str {
        match self {
            MouseButton::Left => "Mouse:Button:Left",
            MouseButton::Right => "Mouse:Button:Right",
        }
    }
}

/// InputPlumber 0.81's `default.yaml` mappings, kept when our profile replaces it.
const DEFAULT_MAPPINGS: &str = r#"  - name: LeftTop
    source_event:
      gamepad:
        button: LeftTop
    target_events:
      - gamepad:
          button: LeftPaddle1
  - name: RightTop
    source_event:
      gamepad:
        button: RightTop
    target_events:
      - gamepad:
          button: RightPaddle1
  - name: Keyboard
    source_event:
      gamepad:
        button: Keyboard
    target_events:
      - gamepad:
          button: Guide
      - gamepad:
          button: North
  - name: QuickAccess2
    source_event:
      gamepad:
        button: QuickAccess2
    target_events:
      - gamepad:
          button: Screenshot
  - name: Left Dial Counter-clockwise
    source_event:
      gamepad:
        dial:
          name: LeftStickDial
          direction: counter-clockwise
    target_events:
      - keyboard: KeyVolumeDown
  - name: Left Dial Clockwise
    source_event:
      gamepad:
        dial:
          name: LeftStickDial
          direction: clockwise
    target_events:
      - keyboard: KeyVolumeUp
  - name: Right Dial Counter-clockwise
    source_event:
      gamepad:
        dial:
          name: RightStickDial
          direction: counter-clockwise
    target_events:
      - keyboard: KeyBrightnessDown
  - name: Right Dial Clockwise
    source_event:
      gamepad:
        dial:
          name: RightStickDial
          direction: clockwise
    target_events:
      - keyboard: KeyBrightnessUp
"#;

/// InputPlumber's default profile (to restore when the pointer is off and we cannot ask for
/// the one that was loaded before ours).
pub fn default_profile_yaml() -> String {
    format!("version: 1\nkind: DeviceProfile\nname: Default\nmapping:\n{DEFAULT_MAPPINGS}")
}

/// The profile's pointer settings ([`crate::controller`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerTuning {
    /// Percent of `[pointer] speed_pps`.
    pub speed: u32,
    /// The left stick moves the pointer and the right stick gives the directions.
    pub swap_sticks: bool,
}

impl Default for PointerTuning {
    fn default() -> Self {
        Self {
            speed: hinteros_api::session::POINTER_SPEED_DEFAULT,
            swap_sticks: false,
        }
    }
}

/// Stick deflection that counts as a direction when the right stick gives them (swapped
/// sticks): InputPlumber's own threshold for the left stick on D-Bus (0.60), so both feel the
/// same.
pub const DIRECTION_DEADZONE: f64 = 0.6;

/// The pointer profile: the default mappings, the pointer stick (the right one, the left one
/// with `tuning.swap_sticks`) as mouse motion at `speed_pps` * `tuning.speed` % pixels per
/// second at full deflection with response curve `curve` (InputPlumber, patched: a 0.20 radial
/// deadzone, then speed * m^curve on the rescaled deflection m, so full speed is unchanged;
/// only this profile has it, games get no curve), and pointer stick activity as
/// [`ACTIVITY_EVENT`] beyond `activity_deadzone`. With swapped sticks the right stick's
/// directions are the D-pad buttons, which reach us as `ui_up` etc. like the left stick's
/// would (the profile maps the left stick, so it no longer gives directions).
pub fn profile_yaml(
    speed_pps: u32,
    curve: f64,
    activity_deadzone: f64,
    tuning: PointerTuning,
) -> String {
    let speed_pps = u64::from(speed_pps) * u64::from(tuning.speed) / 100;
    let (stick, other) = if tuning.swap_sticks {
        ("LeftStick", "RightStick")
    } else {
        ("RightStick", "LeftStick")
    };
    let mut y = format!(
        "version: 1\nkind: DeviceProfile\nname: {PROFILE_NAME}\nmapping:\n{DEFAULT_MAPPINGS}"
    );
    y.push_str(&format!(
        r#"  - name: Pointer
    source_event:
      gamepad:
        axis:
          name: {stick}
    target_events:
      - mouse:
          motion:
            speed_pps: {speed_pps}
            curve: {curve}
"#
    ));
    for direction in ["left", "right", "up", "down"] {
        y.push_str(&format!(
            r#"  - name: Pointer activity {direction}
    source_event:
      gamepad:
        axis:
          name: {stick}
          direction: {direction}
          deadzone: {activity_deadzone}
    target_events:
      - gamepad:
          button: QuickAccess2
"#
        ));
    }
    if tuning.swap_sticks {
        for (direction, button) in [
            ("left", "DPadLeft"),
            ("right", "DPadRight"),
            ("up", "DPadUp"),
            ("down", "DPadDown"),
        ] {
            y.push_str(&format!(
                r#"  - name: Directions {direction}
    source_event:
      gamepad:
        axis:
          name: {other}
          direction: {direction}
          deadzone: {DIRECTION_DEADZONE}
    target_events:
      - gamepad:
          button: {button}
"#
            ));
        }
    }
    y
}

/// Shows and hides gamescope's pointer.
pub trait CursorControl: Send {
    fn set_visible(&mut self, visible: bool);
}

/// No pointer control (tests, no gamescope).
pub struct NoCursor;

impl CursorControl for NoCursor {
    fn set_visible(&mut self, _visible: bool) {}
}

/// Runs a command to hide and one to show the pointer (`gamescopectl paint_cursor_plane 0/1`
/// by default), one at a time in a background task: two at once could finish in either order
/// and leave the pointer in the wrong state. When requests come faster than the commands
/// finish, only the latest wanted state is run next. Needs a tokio runtime.
pub struct CommandCursor {
    wanted: tokio::sync::watch::Sender<Option<bool>>,
}

impl CommandCursor {
    pub fn new(hide: Vec<String>, show: Vec<String>, env: Vec<(String, String)>) -> Self {
        let (wanted, mut rx) = tokio::sync::watch::channel(None);
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let Some(visible) = *rx.borrow_and_update() else {
                    continue;
                };
                let argv = if visible { &show } else { &hide };
                run_cursor_command(argv, &env, visible).await;
            }
        });
        Self { wanted }
    }
}

impl CursorControl for CommandCursor {
    fn set_visible(&mut self, visible: bool) {
        self.wanted.send_replace(Some(visible));
    }
}

/// Run one pointer visibility command and wait for it (at most 5 s).
async fn run_cursor_command(argv: &[String], env: &[(String, String)], visible: bool) {
    let Some(program) = argv.first() else { return };
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(&argv[1..])
        .envs(crate::config::live_env(env))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return debug!(?argv, "cannot run the pointer visibility command: {e}"),
    };
    debug!(visible, ?argv, "pointer visibility");
    match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(status)) if !status.success() => {
            debug!(%status, "pointer visibility command failed")
        }
        Ok(_) => {}
        Err(_) => warn!("pointer visibility command hangs; killing it"),
    }
}

/// Whether gamescope's pointer is shown. In a pointer app it is never hidden for inactivity:
/// the D-pad or left stick (arrow keys) hides it, the right stick shows it again. gamescope
/// itself never hides it either (the system gives it a `--hide-cursor-delay` too long to
/// matter), so going from a pointer app to the launcher (pad only) hides it, and the next app
/// gets it back.
#[derive(Debug, Default)]
pub struct PointerState {
    /// We hid the pointer (and must show it again before anyone else needs it).
    hidden: bool,
    /// A pointer app had focus since we last hid it: the pointer may be on screen.
    used: bool,
    /// The focus the pointer was last set for: (a pointer app, any app). The launcher at start.
    focus: (bool, bool),
}

/// What the caller should do with gamescope's pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorChange {
    Show,
    Hide,
}

impl PointerState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The pointer is hidden (by us).
    pub fn hidden(&self) -> bool {
        self.hidden
    }

    /// The right stick moved, or RT clicked: the pointer shows.
    pub fn motion(&mut self) -> Option<CursorChange> {
        std::mem::take(&mut self.hidden).then_some(CursorChange::Show)
    }

    /// D-pad or left stick navigation: the pointer hides until the right stick moves.
    pub fn navigated(&mut self) -> Option<CursorChange> {
        if self.hidden {
            None
        } else {
            self.hidden = true;
            Some(CursorChange::Hide)
        }
    }

    /// The focus changed (or may have): `pointer_app` has the right-stick pointer, `app` is any
    /// app (not the launcher). Entering an app shows a pointer we hid (in a pointer app until
    /// the D-pad; elsewhere gamescope's own rules, for a real mouse); going from a pointer app
    /// to the launcher hides it, so no pointer left over from a web app sits on the home
    /// screen. Nothing happens between the launcher and other apps.
    pub fn focus(&mut self, pointer_app: bool, app: bool) -> Option<CursorChange> {
        if self.focus == (pointer_app, app) {
            return None;
        }
        self.focus = (pointer_app, app);
        if pointer_app {
            self.used = true;
        }
        if app || pointer_app {
            self.motion()
        } else if std::mem::take(&mut self.used) {
            self.navigated()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cursor_commands_run_one_at_a_time_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let gate = dir.path().join("gate");
        // The hide logs that it started, then waits for the gate file before it logs again.
        let hide = vec![
            "sh".to_string(),
            "-c".to_string(),
            format!(
                "echo start >> {log}; while [ ! -e {gate} ]; do sleep 0.01; done; echo hide >> {log}",
                log = log.display(),
                gate = gate.display()
            ),
        ];
        let show = vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("echo show >> {}", log.display()),
        ];
        // The log once it has `lines` lines (no fixed sleeps: the build box may be busy).
        let wait_lines = async |lines: usize| {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let text = std::fs::read_to_string(&log).unwrap_or_default();
                if text.lines().count() >= lines || std::time::Instant::now() > deadline {
                    return text;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        // The hide is slow: run at once, the show would finish before it.
        let mut c = CommandCursor::new(hide, show, Vec::new());
        c.set_visible(false);
        assert_eq!(wait_lines(1).await, "start\n");
        c.set_visible(true);
        std::fs::write(&gate, "").unwrap();
        assert_eq!(wait_lines(3).await, "start\nhide\nshow\n");
        // Requests that come while a command runs: only the latest runs next.
        std::fs::remove_file(&log).unwrap();
        std::fs::remove_file(&gate).unwrap();
        c.set_visible(false);
        assert_eq!(wait_lines(1).await, "start\n");
        c.set_visible(true);
        c.set_visible(false);
        std::fs::write(&gate, "").unwrap();
        assert_eq!(wait_lines(4).await, "start\nhide\nstart\nhide\n");
    }

    #[test]
    fn profile_maps_the_right_stick() {
        let y = profile_yaml(1200, 2.5, 0.25, PointerTuning::default());
        assert!(y.contains("name: HinterOS pointer"));
        assert!(y.contains("name: RightStick\n    target_events:\n      - mouse:"));
        assert!(y.contains("speed_pps: 1200\n            curve: 2.5\n"));
        assert_eq!(
            y.matches("curve:").count(),
            1,
            "only the pointer gets a curve"
        );
        assert_eq!(y.matches("button: QuickAccess2\n").count(), 5); // 1 default + 4 ours
        assert!(y.contains("deadzone: 0.25"));
        assert!(!y.contains("LeftStick\n"), "the left stick is left alone");
        assert!(!y.contains("DPad"));
        assert!(y.contains("keyboard: KeyVolumeUp"), "default mappings kept");
        assert!(default_profile_yaml().contains("name: Default"));
        assert!(!default_profile_yaml().contains("RightStick\n"));
    }

    #[test]
    fn profile_speed_and_swapped_sticks() {
        let slow = PointerTuning {
            speed: 25,
            swap_sticks: false,
        };
        assert!(profile_yaml(1000, 2.5, 0.25, slow).contains("speed_pps: 250\n"));
        let fast = PointerTuning {
            speed: 400,
            swap_sticks: false,
        };
        assert!(profile_yaml(1000, 2.5, 0.25, fast).contains("speed_pps: 4000\n"));

        let swapped = PointerTuning {
            speed: 150,
            swap_sticks: true,
        };
        let y = profile_yaml(1000, 2.5, 0.25, swapped);
        // The left stick is the pointer (motion and activity)...
        assert!(y.contains("name: LeftStick\n    target_events:\n      - mouse:"));
        assert!(y.contains("speed_pps: 1500\n            curve: 2.5\n"));
        assert_eq!(
            y.matches("name: LeftStick\n          direction:").count(),
            4
        );
        assert_eq!(y.matches("button: QuickAccess2\n").count(), 5);
        // ...and the right stick gives the D-pad directions.
        for (direction, button) in [
            ("left", "DPadLeft"),
            ("right", "DPadRight"),
            ("up", "DPadUp"),
            ("down", "DPadDown"),
        ] {
            assert!(
                y.contains(&format!(
                    "name: RightStick\n          direction: {direction}\n          deadzone: 0.6\n    target_events:\n      - gamepad:\n          button: {button}\n"
                )),
                "{direction}"
            );
        }
        assert!(!y.contains("RightStick\n    target_events"), "no motion");
    }

    #[test]
    fn pointer_state() {
        let mut p = PointerState::new();
        // The launcher at start: nothing to do.
        assert_eq!(p.focus(false, false), None);
        // A pointer app: shown until the D-pad or left stick is used, however long nothing
        // happens.
        assert_eq!(p.focus(true, true), None);
        assert!(!p.hidden());
        assert_eq!(p.motion(), None);
        // D-pad: hide once; the next motion shows it again.
        assert_eq!(p.navigated(), Some(CursorChange::Hide));
        assert!(p.hidden());
        assert_eq!(p.navigated(), None);
        // The same focus again (e.g. the on-screen keyboard came and went): still hidden.
        assert_eq!(p.focus(true, true), None);
        assert_eq!(p.motion(), Some(CursorChange::Show));
        assert_eq!(p.motion(), None);
        // Home: hidden on the launcher; any app shows it again.
        assert_eq!(p.focus(false, false), Some(CursorChange::Hide));
        assert_eq!(p.focus(false, false), None);
        assert_eq!(p.focus(false, true), Some(CursorChange::Show));
        // A game and the launcher alone never touch it.
        assert_eq!(p.focus(false, false), None);
        assert_eq!(p.focus(false, true), None);
        assert_eq!(p.focus(false, false), None);
        // Launcher to pointer app and back.
        assert_eq!(p.focus(true, true), None);
        assert_eq!(p.focus(false, false), Some(CursorChange::Hide));
        assert_eq!(p.focus(true, true), Some(CursorChange::Show));
        // Hidden by the D-pad, then Home (stays hidden), then a game: shown for the game.
        assert_eq!(p.navigated(), Some(CursorChange::Hide));
        assert_eq!(p.focus(false, false), None);
        assert_eq!(p.focus(false, true), Some(CursorChange::Show));
    }
}
