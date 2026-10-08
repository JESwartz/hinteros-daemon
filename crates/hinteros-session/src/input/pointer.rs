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
//! Clicks: `CompositeDevice.SendEvent("Mouse:Button:Left", <bool>)`, which in `GAMEPAD_ONLY`
//! also goes to the mouse target. Hiding the pointer: gamescope hides it on its own after
//! `--hide-cursor-delay` without motion, and immediately with its `paint_cursor_plane` convar
//! (`gamescopectl paint_cursor_plane 0`); see [`CursorControl`].

use std::time::{Duration, Instant};

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
/// pixels per second at full deflection (InputPlumber applies a fixed 0.20 deadzone, linear),
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

/// Whether the right-stick pointer is "in use": it moved within `idle` and no D-pad or left
/// stick navigation came since. Decides what A does (click or Enter) and when to hide the
/// pointer.
#[derive(Debug)]
pub struct PointerState {
    idle: Duration,
    last_motion: Option<Instant>,
    /// We hid the pointer (and must show it again before anyone else needs it).
    hidden: bool,
}

/// What the caller should do with gamescope's pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorChange {
    Show,
    Hide,
}

impl PointerState {
    pub fn new(idle: Duration) -> Self {
        Self {
            idle,
            last_motion: None,
            hidden: false,
        }
    }

    pub fn active(&self, now: Instant) -> bool {
        self.last_motion
            .is_some_and(|t| now.duration_since(t) <= self.idle)
    }

    /// The right stick moved.
    pub fn motion(&mut self, now: Instant) -> Option<CursorChange> {
        self.last_motion = Some(now);
        std::mem::take(&mut self.hidden).then_some(CursorChange::Show)
    }

    /// D-pad or left stick navigation: the pointer is done.
    pub fn navigated(&mut self, now: Instant) -> Option<CursorChange> {
        let was_active = self.active(now);
        self.last_motion = None;
        if was_active && !self.hidden {
            self.hidden = true;
            Some(CursorChange::Hide)
        } else {
            None
        }
    }

    /// The pointer mode ends (focus moved away): give the pointer back to gamescope's rules.
    pub fn reset(&mut self) -> Option<CursorChange> {
        self.last_motion = None;
        std::mem::take(&mut self.hidden).then_some(CursorChange::Show)
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
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut p = PointerState::new(ms(3000));
        assert!(!p.active(t0));
        // Navigation without a pointer in use: nothing to hide.
        assert_eq!(p.navigated(t0), None);
        assert_eq!(p.motion(t0), None);
        assert!(p.active(t0 + ms(3000)));
        assert!(!p.active(t0 + ms(3001)));
        // D-pad while in use: hide once; the next motion shows it again.
        assert_eq!(p.navigated(t0 + ms(10)), Some(CursorChange::Hide));
        assert!(!p.active(t0 + ms(10)));
        assert_eq!(p.navigated(t0 + ms(20)), None);
        assert_eq!(p.motion(t0 + ms(30)), Some(CursorChange::Show));
        assert_eq!(p.motion(t0 + ms(40)), None);
        // Leaving pointer mode while hidden restores it.
        assert_eq!(p.navigated(t0 + ms(50)), Some(CursorChange::Hide));
        assert_eq!(p.reset(), Some(CursorChange::Show));
        assert_eq!(p.reset(), None);
    }
}
