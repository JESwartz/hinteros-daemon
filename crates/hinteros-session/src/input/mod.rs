//! Controller input: where it goes, and what the launcher gets.
//!
//! InputPlumber owns the physical pads (hides them and exposes one virtual pad each). Per pad,
//! its intercept mode decides whether input flows to the virtual pad, i.e. to Steam and the
//! focused app (`Pass`), or only to us as semantic `ui_*` D-Bus events (`All`). In `Pass`, Guide
//! still comes to us and flips the pad to `All` by itself. See hinteros-docs/spikes/04-input-layer.md.
//!
//! [`InputControl`] is the seam ([`inputplumber`] is the real thing); [`translate`] and
//! [`Repeat`] turn `ui_*` events into `home_pressed` and `input_nav`.

pub mod devpad;
pub mod inputplumber;

use std::time::{Duration, Instant};

use hinteros_api::session::NavAction;

/// InputPlumber's `InterceptMode` property values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum InterceptMode {
    /// Everything goes to the virtual pad; nothing to D-Bus.
    None = 0,
    /// Everything goes to the virtual pad except Guide, which goes to D-Bus and switches the
    /// pad to `All`.
    Pass = 1,
    /// Nothing goes to the virtual pad; everything arrives as D-Bus events.
    All = 2,
    /// Gamepad events to D-Bus only; keyboard and mouse still pass.
    GamepadOnly = 3,
}

/// Switches where controller input goes.
pub trait InputControl: Send {
    fn set_mode(&mut self, mode: InterceptMode);
}

/// Input control when InputPlumber integration is off: does nothing.
pub struct NoInput;

impl InputControl for NoInput {
    fn set_mode(&mut self, _mode: InterceptMode) {}
}

/// One `org.shadowblip.Input.DBusDevice.InputEvent` signal.
#[derive(Debug, Clone, PartialEq)]
pub struct RawInput {
    /// Object path of the D-Bus target device (one per pad).
    pub source: String,
    /// `ui_accept`, `ui_guide`, ...
    pub event: String,
    /// 1.0 pressed, 0.0 released.
    pub value: f64,
}

impl RawInput {
    pub fn pressed(&self) -> bool {
        self.value >= 0.5
    }
}

/// What a `ui_*` event means to us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    Home,
    Nav(NavAction),
}

/// Map an InputPlumber D-Bus event name to an intent. Unknown names (volume keys, touch,
/// custom events) are ignored.
pub fn translate(event: &str) -> Option<Intent> {
    use NavAction::*;
    let nav = match event {
        "ui_guide" => return Some(Intent::Home),
        "ui_up" => Up,
        "ui_down" => Down,
        "ui_left" => Left,
        "ui_right" => Right,
        "ui_accept" => Accept,
        "ui_back" => Back,
        "ui_context" => Context,
        "ui_action" => Secondary,
        "ui_option" => Menu,
        "ui_select" => View,
        "ui_l1" => PageLeft,
        "ui_r1" => PageRight,
        "ui_l2" => TriggerLeft,
        "ui_r2" => TriggerRight,
        "ui_quick" => Quick,
        "ui_osk" => Keyboard,
        _ => return None,
    };
    Some(Intent::Nav(nav))
}

/// Key repeat for held directions: the most recently pressed direction repeats after `delay`,
/// then every `interval`, until it is released or another direction is pressed.
#[derive(Debug)]
pub struct Repeat {
    delay: Duration,
    interval: Duration,
    held: Option<Held>,
}

#[derive(Debug)]
struct Held {
    action: NavAction,
    source: String,
    next: Instant,
}

impl Repeat {
    pub fn new(delay: Duration, interval: Duration) -> Self {
        Self {
            delay,
            interval: interval.max(Duration::from_millis(10)),
            held: None,
        }
    }

    pub fn press(&mut self, action: NavAction, source: &str, now: Instant) {
        if action.is_direction() {
            self.held = Some(Held {
                action,
                source: source.into(),
                next: now + self.delay,
            });
        }
    }

    pub fn release(&mut self, action: NavAction, source: &str) {
        if self
            .held
            .as_ref()
            .is_some_and(|h| h.action == action && h.source == source)
        {
            self.held = None;
        }
    }

    pub fn clear(&mut self) {
        self.held = None;
    }

    /// When the next repeat is due.
    pub fn deadline(&self) -> Option<Instant> {
        self.held.as_ref().map(|h| h.next)
    }

    /// The action to repeat now, if one is due. Never bursts: a late tick yields one repeat.
    pub fn tick(&mut self, now: Instant) -> Option<NavAction> {
        let interval = self.interval;
        let held = self.held.as_mut().filter(|h| now >= h.next)?;
        held.next = (held.next + interval).max(now + interval / 2);
        Some(held.action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_inputplumber_events() {
        assert_eq!(translate("ui_guide"), Some(Intent::Home));
        assert_eq!(translate("ui_accept"), Some(Intent::Nav(NavAction::Accept)));
        assert_eq!(translate("ui_back"), Some(Intent::Nav(NavAction::Back)));
        assert_eq!(translate("ui_left"), Some(Intent::Nav(NavAction::Left)));
        assert_eq!(translate("ui_option"), Some(Intent::Nav(NavAction::Menu)));
        assert_eq!(translate("ui_volume_up"), None);
        assert_eq!(translate("something"), None);
        // Every NavAction is reachable from some ui_* event.
        let reachable: Vec<NavAction> = [
            "ui_up",
            "ui_down",
            "ui_left",
            "ui_right",
            "ui_accept",
            "ui_back",
            "ui_context",
            "ui_action",
            "ui_option",
            "ui_select",
            "ui_l1",
            "ui_r1",
            "ui_l2",
            "ui_r2",
            "ui_quick",
            "ui_osk",
        ]
        .iter()
        .filter_map(|e| match translate(e) {
            Some(Intent::Nav(a)) => Some(a),
            _ => None,
        })
        .collect();
        for a in NavAction::ALL {
            assert!(reachable.contains(a), "{a:?}");
        }
    }

    #[test]
    fn repeat_timing() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut r = Repeat::new(ms(400), ms(100));
        assert_eq!(r.deadline(), None);

        // Only directions repeat.
        r.press(NavAction::Accept, "pad0", t0);
        assert_eq!(r.deadline(), None);

        r.press(NavAction::Right, "pad0", t0);
        assert_eq!(r.deadline(), Some(t0 + ms(400)));
        assert_eq!(r.tick(t0 + ms(399)), None);
        assert_eq!(r.tick(t0 + ms(400)), Some(NavAction::Right));
        assert_eq!(r.deadline(), Some(t0 + ms(500)));
        assert_eq!(r.tick(t0 + ms(500)), Some(NavAction::Right));
        // A very late tick yields one repeat, not a burst.
        assert_eq!(r.tick(t0 + ms(2000)), Some(NavAction::Right));
        assert_eq!(r.tick(t0 + ms(2001)), None);

        // Releasing another action, or the same action on another pad, keeps repeating.
        r.release(NavAction::Left, "pad0");
        r.release(NavAction::Right, "pad1");
        assert!(r.deadline().is_some());

        // A new direction takes over; releasing the old one changes nothing.
        r.press(NavAction::Down, "pad0", t0 + ms(3000));
        r.release(NavAction::Right, "pad0");
        assert_eq!(r.deadline(), Some(t0 + ms(3400)));
        r.release(NavAction::Down, "pad0");
        assert_eq!(r.deadline(), None);
    }
}
