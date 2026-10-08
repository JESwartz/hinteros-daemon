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
pub mod pointer;

use std::time::{Duration, Instant};

use hinteros_api::session::NavAction;

pub use pointer::MouseButton;

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
    /// Load the right-stick pointer profile on every pad (or put the previous profile back).
    /// Applied before the next mode.
    fn set_pointer(&mut self, _on: bool) {}
    /// Press or release a mouse button on the pointer of the pad `source` (a D-Bus target).
    fn mouse_button(&mut self, _source: &str, _button: MouseButton, _pressed: bool) {}
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
    /// The right stick moved (pointer profile, [`pointer::ACTIVITY_EVENT`]).
    Pointer,
    Nav(NavAction),
}

/// Map an InputPlumber D-Bus event name to an intent. Unknown names (volume keys, touch,
/// custom events) are ignored.
pub fn translate(event: &str) -> Option<Intent> {
    use NavAction::*;
    let nav = match event {
        "ui_guide" => return Some(Intent::Home),
        pointer::ACTIVITY_EVENT => return Some(Intent::Pointer),
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

/// `[input] xbox_label_swap`: whether to swap `ui_action` and `ui_context` for Xbox pads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LabelSwap {
    /// Decide per pad with [`xbox_layout`].
    #[default]
    Auto,
    /// Swap on every pad.
    On,
    /// Never swap.
    Off,
}

/// Whether a composite device uses the Xbox button layout that InputPlumber (0.81) maps
/// positionally: xpad reports the top button as `BTN_Y` (=`BTN_WEST`) and the left one as
/// `BTN_X` (=`BTN_NORTH`), so physical Y arrives as `ui_action` and X as `ui_context`, the
/// other way round from every other pad. `name` is the CompositeDevice `Name` (the device
/// config's name), `drivers` the kernel drivers of its event sources (`xpad`, `playstation`,
/// `microsoft`, ...; empty when unknown). Returns the reason when the labels need swapping.
///
/// Driver first: `xpad` is the case seen on real hardware. A Bluetooth Xbox pad (config name
/// "Microsoft X-Box One pad" too, but driver `microsoft` and a hidraw source that InputPlumber
/// decodes itself) is not known to share the quirk, so it is left alone unless forced with
/// `xbox_label_swap = "on"`. Without any driver information the config name decides.
pub fn xbox_layout(name: &str, drivers: &[String]) -> Option<&'static str> {
    if drivers.iter().any(|d| d == "xpad") {
        return Some("xpad driver");
    }
    if !drivers.is_empty() {
        return None;
    }
    // Configs: "Microsoft X-Box pad", "... 360 pad", "... One pad", "... One Elite pad".
    name.starts_with("Microsoft X-Box")
        .then_some("Xbox device config")
}

/// `ui_action` and `ui_context` exchanged; everything else unchanged.
pub fn swap_xbox_label(event: &str) -> &str {
    match event {
        "ui_action" => "ui_context",
        "ui_context" => "ui_action",
        other => other,
    }
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

/// Drops the chatter of a stick held near InputPlumber's direction threshold.
///
/// InputPlumber (0.81, `src/input/target/dbus.rs:31,87-170`) turns the left stick into
/// `ui_left`/`ui_right`/`ui_up`/`ui_down` at one threshold (0.60) for both press and release,
/// without hysteresis, so a stick resting near it, or swept through it, sends a burst of
/// press/release pairs. A press of a direction that comes within `window` of its own release
/// (same pad) is taken as chatter: it and its release are dropped. Releases always go through
/// at once, so a release still stops a repeat immediately. Nobody presses the same D-pad
/// direction twice within a few tens of milliseconds.
#[derive(Debug)]
pub struct Debounce {
    window: Duration,
    last_release: std::collections::HashMap<(NavAction, String), Instant>,
    suppressed: std::collections::HashSet<(NavAction, String)>,
}

impl Debounce {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            last_release: Default::default(),
            suppressed: Default::default(),
        }
    }

    /// Whether to drop this event.
    pub fn drop_event(
        &mut self,
        action: NavAction,
        source: &str,
        pressed: bool,
        now: Instant,
    ) -> bool {
        if !action.is_direction() || self.window.is_zero() {
            return false;
        }
        let key = (action, source.to_string());
        if pressed {
            let chatter = self
                .last_release
                .get(&key)
                .is_some_and(|&t| now.duration_since(t) < self.window);
            if chatter {
                self.suppressed.insert(key);
            }
            chatter
        } else {
            self.last_release.insert(key.clone(), now);
            self.suppressed.remove(&key)
        }
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
    fn xbox_layout_detection() {
        let d = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(xbox_layout("Microsoft X-Box One pad", &d(&["xpad"])).is_some());
        assert!(xbox_layout("Whatever", &d(&["xpad", "usbhid"])).is_some());
        assert!(xbox_layout("Microsoft X-Box 360 pad", &[]).is_some());
        assert!(xbox_layout("Microsoft X-Box One Elite pad", &[]).is_some());
        // Bluetooth Xbox (hid-microsoft), PlayStation, Switch, 8BitDo: untouched.
        assert!(xbox_layout("Microsoft X-Box One pad", &d(&["microsoft"])).is_none());
        assert!(
            xbox_layout(
                "Sony Interactive Entertainment DualSense Wireless Controller",
                &[]
            )
            .is_none()
        );
        assert!(xbox_layout("Sony DualSense", &d(&["playstation"])).is_none());
        assert!(xbox_layout("Nintendo Co., Ltd. Pro Controller", &d(&["nintendo"])).is_none());
        assert!(xbox_layout("8BitDo 8BitDo Pro 2", &[]).is_none());
        assert!(xbox_layout("", &[]).is_none());
        assert_eq!(swap_xbox_label("ui_action"), "ui_context");
        assert_eq!(swap_xbox_label("ui_context"), "ui_action");
        assert_eq!(swap_xbox_label("ui_accept"), "ui_accept");
    }

    #[test]
    fn debounce_drops_stick_chatter() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut d = Debounce::new(ms(40));
        let (l, r) = (NavAction::Left, NavAction::Right);
        assert!(!d.drop_event(l, "p", true, t0));
        assert!(!d.drop_event(l, "p", false, t0 + ms(100)));
        // Chatter: re-pressed 8 ms after the release, again and again; all dropped.
        let mut t = t0 + ms(100);
        for _ in 0..10 {
            t += ms(8);
            assert!(d.drop_event(l, "p", true, t));
            t += ms(8);
            assert!(d.drop_event(l, "p", false, t));
        }
        // Calm for a moment, then a real press.
        assert!(!d.drop_event(l, "p", true, t + ms(40)));
        assert!(!d.drop_event(l, "p", false, t + ms(200)));
        // Another direction, another pad, or a button: not affected.
        assert!(!d.drop_event(r, "p", true, t + ms(201)));
        assert!(!d.drop_event(l, "q", true, t + ms(201)));
        assert!(!d.drop_event(NavAction::Accept, "p", true, t + ms(201)));
        assert!(!d.drop_event(NavAction::Accept, "p", false, t + ms(202)));
        assert!(!d.drop_event(NavAction::Accept, "p", true, t + ms(203)));
        // Off.
        let mut off = Debounce::new(Duration::ZERO);
        assert!(!off.drop_event(l, "p", false, t0));
        assert!(!off.drop_event(l, "p", true, t0));
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
