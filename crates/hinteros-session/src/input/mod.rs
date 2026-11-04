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

use hinteros_api::session::{ControllerButton, NavAction};

pub use pointer::{MouseButton, PointerTuning};

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
    /// The profile's pointer settings: the pointer profile loaded from now on (at once if it is
    /// loaded).
    fn set_pointer_tuning(&mut self, _tuning: PointerTuning) {}
    /// Press or release a mouse button on the pointer of the pad `source` (a D-Bus target).
    fn mouse_button(&mut self, _source: &str, _button: MouseButton, _pressed: bool) {}
    /// Tap Guide (press, [`GUIDE_TAP_HOLD`], release) on the virtual pad of the pad `source`,
    /// for Steam: a long Guide press while Steam is in front. The pad is first put in `mode`
    /// (the focused app's, `Pass`), which InputPlumber left for `All` on the user's Guide
    /// press; only then the tap, so that it goes to the virtual pad and not back to us.
    /// InputPlumber's `SendEvent` writes straight to the targets, past the `Pass` mode's Guide
    /// intercept (that only looks at events from the physical pad), but it still honours
    /// `All`/`GamepadOnly`, which would send the tap to us as a new `ui_guide`. So the tap is
    /// only sent in `Pass` or `None`.
    fn guide_tap(&mut self, _source: &str, _mode: InterceptMode) {}
}

/// How long the Guide tap to Steam holds the button (InputPlumber's own chords space their
/// events 80 ms apart; Steam must see the press in at least one of its polls).
pub const GUIDE_TAP_HOLD: Duration = Duration::from_millis(80);

/// InputPlumber's capability name for the Guide button (`SendEvent`).
pub const GUIDE_CAPABILITY: &str = "Gamepad:Button:Guide";

impl InterceptMode {
    /// Whether an event written to the pad's targets in this mode reaches the virtual pad
    /// (rather than going to D-Bus, i.e. to us).
    pub fn reaches_pad(self) -> bool {
        matches!(self, InterceptMode::None | InterceptMode::Pass)
    }
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

/// The physical button behind an InputPlumber D-Bus event (after the Xbox label swap), for the
/// ten buttons the profile can remap. Guide, the directions and the rest:
/// `None`.
pub fn button(event: &str) -> Option<ControllerButton> {
    use ControllerButton::*;
    Some(match event {
        "ui_accept" => A,
        "ui_back" => B,
        "ui_action" => X,
        "ui_context" => Y,
        "ui_l1" => Lb,
        "ui_r1" => Rb,
        "ui_l2" => Lt,
        "ui_r2" => Rt,
        "ui_select" => View,
        "ui_option" => Menu,
        _ => return None,
    })
}

/// The direction of a D-Bus event (D-pad and left stick alike), by its API name.
pub fn direction(event: &str) -> Option<&'static str> {
    Some(match event {
        "ui_up" => "up",
        "ui_down" => "down",
        "ui_left" => "left",
        "ui_right" => "right",
        _ => return None,
    })
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

    /// Like [`Self::release`]; returns when the stopped repeat was due next (for
    /// [`Self::resume`]).
    pub fn stop(&mut self, action: NavAction, source: &str) -> Option<Instant> {
        let next = self
            .held
            .as_ref()
            .filter(|h| h.action == action && h.source == source)
            .map(|h| h.next);
        if next.is_some() {
            self.held = None;
        }
        next
    }

    /// Go on with a repeat [`Self::stop`] stopped (the release was stick chatter), due at
    /// `next` (not before `now`). Not when another repeat took over meanwhile.
    pub fn resume(&mut self, action: NavAction, source: &str, next: Instant, now: Instant) {
        if action.is_direction() && self.held.is_none() {
            self.held = Some(Held {
                action,
                source: source.into(),
                next: next.max(now),
            });
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
/// press/release pairs. So a direction's release is held back for `window`: a press of the
/// same direction on the same pad in that time is chatter, and both are dropped (the hold goes
/// on; a repeat it stopped goes on too). Otherwise the release is passed on when the time is
/// up, or at once when that pad presses another direction. The repeat itself stops at the
/// release (the caller stops it), so a real release never repeats once more. Nobody presses
/// the same D-pad direction twice within a few tens of milliseconds.
#[derive(Debug)]
pub struct Debounce {
    window: Duration,
    pending: std::collections::HashMap<(NavAction, String), Pending>,
}

/// A direction release held back.
#[derive(Debug)]
struct Pending {
    /// When it is passed on.
    at: Instant,
    input: RawInput,
    /// When the repeat it stopped was due next (`None`: it stopped none).
    repeat: Option<Instant>,
}

impl Debounce {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            pending: Default::default(),
        }
    }

    /// A release of `action` (`input`): whether it is held back. `repeat`: when the repeat
    /// the release stopped was due next.
    pub fn hold_release(
        &mut self,
        action: NavAction,
        input: &RawInput,
        now: Instant,
        repeat: Option<Instant>,
    ) -> bool {
        if !action.is_direction() || self.window.is_zero() {
            return false;
        }
        self.pending.insert(
            (action, input.source.clone()),
            Pending {
                at: now + self.window,
                input: input.clone(),
                repeat,
            },
        );
        true
    }

    /// A press of `action` on `source`: `Some` when it is chatter (drop it, and its held-back
    /// release), with when the repeat that release stopped is due (resume it).
    pub fn chatter(&mut self, action: NavAction, source: &str) -> Option<Option<Instant>> {
        self.pending
            .remove(&(action, source.to_string()))
            .map(|p| p.repeat)
    }

    /// The releases held back for `source` (to pass on before its next press), oldest first.
    pub fn take_pad(&mut self, source: &str) -> Vec<(NavAction, RawInput)> {
        self.take(|(_, s), _| s == source)
    }

    /// The releases whose time is up, oldest first.
    pub fn due(&mut self, now: Instant) -> Vec<(NavAction, RawInput)> {
        self.take(|_, p| p.at <= now)
    }

    /// When the next held-back release is due.
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.values().map(|p| p.at).min()
    }

    fn take(
        &mut self,
        mut which: impl FnMut(&(NavAction, String), &Pending) -> bool,
    ) -> Vec<(NavAction, RawInput)> {
        let keys: Vec<_> = self
            .pending
            .iter()
            .filter(|(k, p)| which(k, p))
            .map(|(k, _)| k.clone())
            .collect();
        let mut out: Vec<(Instant, NavAction, RawInput)> = keys
            .into_iter()
            .filter_map(|k| self.pending.remove(&k).map(|p| (p.at, k.0, p.input)))
            .collect();
        out.sort_by_key(|(at, ..)| *at);
        out.into_iter().map(|(_, a, i)| (a, i)).collect()
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
    fn buttons_behind_events() {
        // Each of the ten buttons arrives as one event, whose own action is the button's
        // default action.
        let events = [
            "ui_accept",
            "ui_back",
            "ui_action",
            "ui_context",
            "ui_l1",
            "ui_r1",
            "ui_l2",
            "ui_r2",
            "ui_select",
            "ui_option",
        ];
        let buttons: Vec<ControllerButton> = events.iter().filter_map(|e| button(e)).collect();
        assert_eq!(buttons, ControllerButton::ALL);
        for e in events {
            let b = button(e).unwrap();
            assert_eq!(translate(e), Some(Intent::Nav(b.default_action())), "{e}");
        }
        for e in ["ui_guide", "ui_up", "ui_quick", "ui_osk", "ui_quick2", "x"] {
            assert_eq!(button(e), None, "{e}");
        }
        assert_eq!(direction("ui_up"), Some("up"));
        assert_eq!(direction("ui_right"), Some("right"));
        assert_eq!(direction("ui_accept"), None);
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
        let raw = |source: &str, event: &str| RawInput {
            source: source.into(),
            event: event.into(),
            value: 0.0,
        };
        let mut d = Debounce::new(ms(40));
        let (l, r) = (NavAction::Left, NavAction::Right);
        assert_eq!(d.deadline(), None);
        // A release waits; a re-press of it 8 ms later is chatter: both dropped, and the
        // repeat the release stopped goes on. Again and again.
        let mut t = t0 + ms(100);
        for _ in 0..10 {
            assert!(d.hold_release(l, &raw("p", "ui_left"), t, Some(t0 + ms(500))));
            assert_eq!(d.deadline(), Some(t + ms(40)));
            t += ms(8);
            assert!(d.due(t).is_empty());
            assert_eq!(d.chatter(l, "p"), Some(Some(t0 + ms(500))));
            t += ms(8);
        }
        assert_eq!(d.deadline(), None);
        // A real release: passed on once its time is up.
        assert!(d.hold_release(l, &raw("p", "ui_left"), t, None));
        assert!(d.due(t + ms(39)).is_empty());
        assert_eq!(d.due(t + ms(40)), [(l, raw("p", "ui_left"))]);
        assert_eq!(d.chatter(l, "p"), None);
        // Another direction on the same pad takes the waiting releases first; another pad's
        // stay.
        assert!(d.hold_release(l, &raw("p", "ui_left"), t, None));
        assert!(d.hold_release(r, &raw("q", "ui_right"), t + ms(1), None));
        assert_eq!(d.chatter(r, "p"), None);
        assert_eq!(d.take_pad("p"), [(l, raw("p", "ui_left"))]);
        assert_eq!(d.deadline(), Some(t + ms(41)));
        // Buttons, and no window: never held back.
        let mut off = Debounce::new(Duration::ZERO);
        assert!(!off.hold_release(l, &raw("p", "ui_left"), t0, None));
        assert!(!d.hold_release(NavAction::Accept, &raw("p", "ui_accept"), t0, None));
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

    #[test]
    fn repeat_stops_and_resumes() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut r = Repeat::new(ms(400), ms(100));
        r.press(NavAction::Up, "p", t0);
        assert_eq!(r.stop(NavAction::Down, "p"), None);
        assert_eq!(r.stop(NavAction::Up, "q"), None);
        assert_eq!(r.stop(NavAction::Up, "p"), Some(t0 + ms(400)));
        assert_eq!(r.deadline(), None);
        // Chatter: it goes on when it was due (an overdue one at once).
        r.resume(NavAction::Up, "p", t0 + ms(400), t0 + ms(10));
        assert_eq!(r.deadline(), Some(t0 + ms(400)));
        r.stop(NavAction::Up, "p");
        r.resume(NavAction::Up, "p", t0 + ms(400), t0 + ms(450));
        assert_eq!(r.tick(t0 + ms(450)), Some(NavAction::Up));
        // Another repeat that took over meanwhile stays.
        r.press(NavAction::Left, "q", t0 + ms(500));
        r.resume(NavAction::Up, "p", t0 + ms(400), t0 + ms(500));
        assert_eq!(r.deadline(), Some(t0 + ms(900)));
    }
}
