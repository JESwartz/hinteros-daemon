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
//! Clicks (RT only; A is Enter, or the on-screen keyboard on a text field):
//! `CompositeDevice.SendEvent("Mouse:Button:Left", <bool>)`, which in `GAMEPAD_ONLY` also goes
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

/// The pointer profile: the default mappings, the right stick as mouse motion at `speed_pps`
/// pixels per second at full deflection (InputPlumber applies a fixed 0.20 deadzone, then a
/// response curve speed = speed_pps * m^k on the rescaled deflection m; k is the system's
/// `hinteros.input.pointerCurve`, default 2.5, so full speed is unchanged),
/// and right-stick activity as [`ACTIVITY_EVENT`] beyond `activity_deadzone`.
pub fn profile_yaml(speed_pps: u32, activity_deadzone: f64) -> String {
    let mut y = format!(
        "version: 1\nkind: DeviceProfile\nname: {PROFILE_NAME}\nmapping:\n{DEFAULT_MAPPINGS}"
    );
    y.push_str(&format!(
        r#"  - name: Right stick pointer
    source_event:
      gamepad:
        axis:
          name: RightStick
    target_events:
      - mouse:
          motion:
            speed_pps: {speed_pps}
"#
    ));
    for direction in ["left", "right", "up", "down"] {
        y.push_str(&format!(
            r#"  - name: Right stick activity {direction}
    source_event:
      gamepad:
        axis:
          name: RightStick
          direction: {direction}
          deadzone: {activity_deadzone}
    target_events:
      - gamepad:
          button: QuickAccess2
"#
        ));
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
/// by default). Fire and forget: the commands are reaped in the background.
pub struct CommandCursor {
    pub hide: Vec<String>,
    pub show: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl CursorControl for CommandCursor {
    fn set_visible(&mut self, visible: bool) {
        let argv = if visible { &self.show } else { &self.hide };
        let Some(program) = argv.first() else { return };
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(&argv[1..])
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .kill_on_drop(true);
        match cmd.spawn() {
            Ok(mut child) => {
                debug!(visible, ?argv, "pointer visibility");
                tokio::spawn(async move {
                    let wait = tokio::time::timeout(Duration::from_secs(5), child.wait());
                    match wait.await {
                        Ok(Ok(status)) if !status.success() => {
                            debug!(%status, "pointer visibility command failed")
                        }
                        Ok(_) => {}
                        Err(_) => warn!("pointer visibility command hangs; killing it"),
                    }
                });
            }
            Err(e) => debug!(?argv, "cannot run the pointer visibility command: {e}"),
        }
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

    #[test]
    fn profile_maps_the_right_stick() {
        let y = profile_yaml(1200, 0.25);
        assert!(y.contains("name: HinterOS pointer"));
        assert!(y.contains("speed_pps: 1200"));
        assert_eq!(y.matches("button: QuickAccess2\n").count(), 5); // 1 default + 4 ours
        assert!(y.contains("deadzone: 0.25"));
        assert!(y.contains("keyboard: KeyVolumeUp"), "default mappings kept");
        assert!(default_profile_yaml().contains("name: Default"));
        assert!(!default_profile_yaml().contains("RightStick\n"));
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
