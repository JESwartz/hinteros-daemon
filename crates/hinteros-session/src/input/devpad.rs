//! Development-only gamepad input: read pads directly through evdev, without InputPlumber.
//!
//! For trying the launcher on a desktop (see "Desktop preview" in the README). It produces
//! the same [`RawInput`] events as the InputPlumber path (`ui_accept`, `ui_guide`, ...), so
//! mapping to `input_nav`, key repeat and Guide/Home all behave identically.
//!
//! It only ever *reads*: devices are opened read-only and never grabbed (no `EVIOCGRAB`), so
//! Steam and games on the desktop keep seeing the pad. Needs read access to
//! `/dev/input/event*` (a pad on the active seat has it through logind, else group `input`).
//!
//! Layout (Xbox naming): A accept, B back, X `ui_action`, Y `ui_context`, bumpers `ui_l1/r1`,
//! triggers `ui_l2/r2`, View `ui_select`, Menu `ui_option`, Xbox button `ui_guide`; the d-pad
//! and the left stick are up/down/left/right.
//!
//! [`Mapper`] is the pure part (tested with synthetic events); [`EventSource`] is the seam for
//! the device (tests use a scripted one); [`spawn`] is the real thing.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use evdev::{AbsoluteAxisCode, EventType, KeyCode};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::RawInput;

/// An evdev event the mapper cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PadEvent {
    /// A button went down or up (autorepeat is dropped by the reader).
    Key { code: u16, pressed: bool },
    /// An axis moved.
    Abs { code: u16, value: i32 },
}

/// Minimum and maximum of each absolute axis, from the device.
pub type Ranges = HashMap<u16, (i32, i32)>;

/// Stick or trigger deflection (fraction of the range) that counts as a press, and the lower
/// one it must fall below to release again.
const PRESS: f32 = 0.5;
const RELEASE: f32 = 0.3;

fn key_event(code: u16) -> Option<&'static str> {
    Some(match KeyCode(code) {
        KeyCode::BTN_SOUTH => "ui_accept",
        KeyCode::BTN_EAST => "ui_back",
        KeyCode::BTN_NORTH => "ui_action",
        KeyCode::BTN_WEST => "ui_context",
        KeyCode::BTN_TL => "ui_l1",
        KeyCode::BTN_TR => "ui_r1",
        KeyCode::BTN_TL2 => "ui_l2",
        KeyCode::BTN_TR2 => "ui_r2",
        KeyCode::BTN_SELECT => "ui_select",
        KeyCode::BTN_START => "ui_option",
        KeyCode::BTN_MODE => "ui_guide",
        KeyCode::BTN_DPAD_UP => "ui_up",
        KeyCode::BTN_DPAD_DOWN => "ui_down",
        KeyCode::BTN_DPAD_LEFT => "ui_left",
        KeyCode::BTN_DPAD_RIGHT => "ui_right",
        _ => return None,
    })
}

/// Turns one pad's evdev events into `ui_*` press/release transitions. An event that several
/// inputs can cause (d-pad and stick both up) is held while any of them is.
#[derive(Debug, Default)]
pub struct Mapper {
    ranges: Ranges,
    /// (input, event) pairs currently down. Input: `0x1_0000 | key code` or an axis code.
    down: HashSet<(u32, &'static str)>,
}

impl Mapper {
    pub fn new(ranges: Ranges) -> Self {
        Self {
            ranges,
            down: HashSet::new(),
        }
    }

    fn set(
        &mut self,
        input: u32,
        event: &'static str,
        on: bool,
        out: &mut Vec<(&'static str, bool)>,
    ) {
        let was = self.down.iter().any(|(_, e)| *e == event);
        if on {
            self.down.insert((input, event));
        } else {
            self.down.remove(&(input, event));
        }
        let is = self.down.iter().any(|(_, e)| *e == event);
        if was != is {
            out.push((event, is));
        }
    }

    /// Deflection of `code` from -1.0 to 1.0 (centred axes) or 0.0 to 1.0 (`one_sided`).
    fn norm(&self, code: u16, value: i32, one_sided: bool) -> f32 {
        let (min, max) = self.ranges.get(&code).copied().unwrap_or((-32768, 32767));
        let span = (max - min).max(1) as f32;
        if one_sided {
            (value - min) as f32 / span
        } else {
            (value - min) as f32 / span * 2.0 - 1.0
        }
    }

    /// An axis that reads as two directions, with hysteresis.
    fn two_way(
        &mut self,
        code: u16,
        v: f32,
        (neg, pos): (&'static str, &'static str),
        out: &mut Vec<(&'static str, bool)>,
    ) {
        let input = u32::from(code);
        for (event, x) in [(neg, -v), (pos, v)] {
            let held = self.down.contains(&(input, event));
            let limit = if held { RELEASE } else { PRESS };
            self.set(input, event, x > limit, out);
        }
    }

    /// Feed one event; the resulting `(ui event, pressed)` transitions.
    pub fn event(&mut self, ev: PadEvent) -> Vec<(&'static str, bool)> {
        let mut out = Vec::new();
        match ev {
            PadEvent::Key { code, pressed } => {
                if let Some(event) = key_event(code) {
                    self.set(0x1_0000 | u32::from(code), event, pressed, &mut out);
                }
            }
            PadEvent::Abs { code, value } => match AbsoluteAxisCode(code) {
                AbsoluteAxisCode::ABS_HAT0X => {
                    self.hat(code, value, ("ui_left", "ui_right"), &mut out);
                }
                AbsoluteAxisCode::ABS_HAT0Y => {
                    self.hat(code, value, ("ui_up", "ui_down"), &mut out);
                }
                AbsoluteAxisCode::ABS_X => {
                    let v = self.norm(code, value, false);
                    self.two_way(code, v, ("ui_left", "ui_right"), &mut out);
                }
                AbsoluteAxisCode::ABS_Y => {
                    let v = self.norm(code, value, false);
                    self.two_way(code, v, ("ui_up", "ui_down"), &mut out);
                }
                AbsoluteAxisCode::ABS_Z | AbsoluteAxisCode::ABS_RZ => {
                    let event = if code == AbsoluteAxisCode::ABS_Z.0 {
                        "ui_l2"
                    } else {
                        "ui_r2"
                    };
                    let v = self.norm(code, value, true);
                    let held = self.down.contains(&(u32::from(code), event));
                    let limit = if held { RELEASE } else { PRESS };
                    self.set(u32::from(code), event, v > limit, &mut out);
                }
                _ => {}
            },
        }
        out
    }

    fn hat(
        &mut self,
        code: u16,
        value: i32,
        (neg, pos): (&'static str, &'static str),
        out: &mut Vec<(&'static str, bool)>,
    ) {
        self.set(u32::from(code), neg, value < 0, out);
        self.set(u32::from(code), pos, value > 0, out);
    }

    /// Everything still held is released (the pad went away).
    pub fn release_all(&mut self) -> Vec<(&'static str, bool)> {
        let events: HashSet<&'static str> = self.down.iter().map(|(_, e)| *e).collect();
        self.down.clear();
        let mut out: Vec<_> = events.into_iter().map(|e| (e, false)).collect();
        out.sort_unstable();
        out
    }
}

/// Where a pad's events come from.
pub trait EventSource: Send {
    /// The next batch of events, blocking. An error ends the pad (unplugged).
    fn next_batch(&mut self) -> io::Result<Vec<PadEvent>>;
}

/// Run one pad until its source fails: map events and send them. Held inputs are released at
/// the end.
pub fn run_pad(
    name: &str,
    source: &mut dyn EventSource,
    mut mapper: Mapper,
    tx: &mpsc::UnboundedSender<RawInput>,
) {
    let send = |events: Vec<(&'static str, bool)>| -> bool {
        events.into_iter().all(|(event, pressed)| {
            tx.send(RawInput {
                source: name.to_string(),
                event: event.to_string(),
                value: if pressed { 1.0 } else { 0.0 },
            })
            .is_ok()
        })
    };
    loop {
        match source.next_batch() {
            Ok(batch) => {
                for ev in batch {
                    if !send(mapper.event(ev)) {
                        return;
                    }
                }
            }
            Err(e) => {
                debug!(pad = name, "pad ended: {e}");
                send(mapper.release_all());
                return;
            }
        }
    }
}

/// A real `/dev/input/event*` device, opened read-only and never grabbed.
struct Evdev(evdev::Device);

impl EventSource for Evdev {
    fn next_batch(&mut self) -> io::Result<Vec<PadEvent>> {
        let mut out = Vec::new();
        for ev in self.0.fetch_events()? {
            match ev.event_type() {
                // Value 2 is the kernel's autorepeat; we do our own.
                EventType::KEY if ev.value() < 2 => out.push(PadEvent::Key {
                    code: ev.code(),
                    pressed: ev.value() == 1,
                }),
                EventType::ABSOLUTE => out.push(PadEvent::Abs {
                    code: ev.code(),
                    value: ev.value(),
                }),
                _ => {}
            }
        }
        Ok(out)
    }
}

/// Whether the device looks like a gamepad: it has A and a stick or d-pad. Skips InputPlumber's
/// and our own virtual devices, which would double every input.
fn is_gamepad(dev: &evdev::Device) -> bool {
    let name = dev.name().unwrap_or("");
    if name.contains("InputPlumber") || name.contains("hinteros") {
        return false;
    }
    let keys = dev
        .supported_keys()
        .is_some_and(|k| k.contains(KeyCode::BTN_SOUTH));
    let axes = dev.supported_absolute_axes().is_some_and(|a| {
        a.contains(AbsoluteAxisCode::ABS_X) || a.contains(AbsoluteAxisCode::ABS_HAT0X)
    });
    keys && axes
}

/// Watch `/dev/input` for gamepads and send their input. Pads that appear later are picked up
/// (every two seconds). Runs on plain threads: nothing here needs the async runtime.
pub fn spawn(tx: mpsc::UnboundedSender<RawInput>) {
    warn!(
        "dev gamepad input is on: reading pads directly through evdev (development only; \
         not grabbed, so other programs still get them)"
    );
    let open: Arc<Mutex<HashSet<PathBuf>>> = Arc::default();
    let result = std::thread::Builder::new()
        .name("devpad-scan".into())
        .spawn(move || {
            let mut warned = false;
            loop {
                let mut seen = 0;
                for (path, dev) in evdev::enumerate() {
                    seen += 1;
                    if !is_gamepad(&dev) || open.lock().is_ok_and(|o| o.contains(&path)) {
                        continue;
                    }
                    // enumerate() opened it read-only; it is the same fd we keep.
                    let ranges: Ranges = dev
                        .get_absinfo()
                        .map(|i| i.map(|(c, a)| (c.0, (a.minimum(), a.maximum()))).collect())
                        .unwrap_or_default();
                    let name = format!("devpad:{}", path.display());
                    info!(pad = %name, name = dev.name().unwrap_or("?"), "dev gamepad found");
                    if let Ok(mut o) = open.lock() {
                        o.insert(path.clone());
                    }
                    let (tx, open) = (tx.clone(), open.clone());
                    let reader =
                        std::thread::Builder::new()
                            .name("devpad".into())
                            .spawn(move || {
                                run_pad(&name, &mut Evdev(dev), Mapper::new(ranges), &tx);
                                if let Ok(mut o) = open.lock() {
                                    o.remove(&path);
                                }
                                info!(pad = %name, "dev gamepad gone");
                            });
                    if let Err(e) = reader {
                        warn!("cannot start a gamepad reader: {e}");
                    }
                }
                if seen == 0 && !warned {
                    warned = true;
                    warn!(
                        "no readable input devices under /dev/input; gamepads need read \
                         access (a pad on your seat has it, else join the 'input' group)"
                    );
                }
                if tx.is_closed() {
                    return;
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        });
    if let Err(e) = result {
        warn!("cannot start the gamepad scanner: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn key(k: KeyCode, pressed: bool) -> PadEvent {
        PadEvent::Key { code: k.0, pressed }
    }

    fn abs(a: AbsoluteAxisCode, value: i32) -> PadEvent {
        PadEvent::Abs { code: a.0, value }
    }

    fn xbox() -> Mapper {
        Mapper::new(Ranges::from([
            (AbsoluteAxisCode::ABS_X.0, (-32768, 32767)),
            (AbsoluteAxisCode::ABS_Y.0, (-32768, 32767)),
            (AbsoluteAxisCode::ABS_Z.0, (0, 1023)),
            (AbsoluteAxisCode::ABS_RZ.0, (0, 1023)),
        ]))
    }

    #[test]
    fn buttons_map_to_ui_events() {
        let mut m = xbox();
        let cases = [
            (KeyCode::BTN_SOUTH, "ui_accept"),
            (KeyCode::BTN_EAST, "ui_back"),
            (KeyCode::BTN_NORTH, "ui_action"),
            (KeyCode::BTN_WEST, "ui_context"),
            (KeyCode::BTN_TL, "ui_l1"),
            (KeyCode::BTN_TR, "ui_r1"),
            (KeyCode::BTN_SELECT, "ui_select"),
            (KeyCode::BTN_START, "ui_option"),
            (KeyCode::BTN_MODE, "ui_guide"),
        ];
        for (k, ui) in cases {
            assert_eq!(m.event(key(k, true)), [(ui, true)]);
            assert_eq!(m.event(key(k, false)), [(ui, false)]);
        }
        // Buttons we do not use (thumb clicks) are ignored.
        assert!(m.event(key(KeyCode::BTN_THUMBL, true)).is_empty());
        // And every result is something the shell understands.
        for (_, ui) in cases {
            assert!(super::super::translate(ui).is_some(), "{ui}");
        }
    }

    #[test]
    fn hat_and_stick_are_directions_with_hysteresis() {
        let mut m = xbox();
        let hat_x = AbsoluteAxisCode::ABS_HAT0X;
        assert_eq!(m.event(abs(hat_x, 1)), [("ui_right", true)]);
        assert_eq!(m.event(abs(hat_x, 0)), [("ui_right", false)]);
        assert_eq!(m.event(abs(hat_x, -1)), [("ui_left", true)]);
        // Straight from left to right releases left and presses right.
        let mut flip = m.event(abs(hat_x, 1));
        flip.sort_unstable();
        assert_eq!(flip, [("ui_left", false), ("ui_right", true)]);
        m.event(abs(hat_x, 0));

        let y = AbsoluteAxisCode::ABS_Y;
        // Y axis: negative is up. Below the press threshold nothing happens.
        assert!(m.event(abs(y, -10000)).is_empty());
        assert_eq!(m.event(abs(y, -20000)), [("ui_up", true)]);
        // Wobbling between the thresholds does not retrigger or release.
        assert!(m.event(abs(y, -12000)).is_empty());
        assert_eq!(m.event(abs(y, -2000)), [("ui_up", false)]);
        assert_eq!(m.event(abs(y, 30000)), [("ui_down", true)]);
        assert_eq!(
            m.event(abs(AbsoluteAxisCode::ABS_X, 30000)),
            [("ui_right", true)]
        );
    }

    #[test]
    fn stick_and_hat_share_a_direction() {
        let mut m = xbox();
        assert_eq!(
            m.event(abs(AbsoluteAxisCode::ABS_HAT0Y, -1)),
            [("ui_up", true)]
        );
        // Stick also up: still one held direction, no second press.
        assert!(m.event(abs(AbsoluteAxisCode::ABS_Y, -30000)).is_empty());
        assert!(m.event(abs(AbsoluteAxisCode::ABS_HAT0Y, 0)).is_empty());
        assert_eq!(m.event(abs(AbsoluteAxisCode::ABS_Y, 0)), [("ui_up", false)]);
    }

    #[test]
    fn triggers_press_past_half_travel() {
        let mut m = xbox();
        let z = AbsoluteAxisCode::ABS_Z;
        assert!(m.event(abs(z, 300)).is_empty());
        assert_eq!(m.event(abs(z, 800)), [("ui_l2", true)]);
        assert!(m.event(abs(z, 400)).is_empty());
        assert_eq!(m.event(abs(z, 100)), [("ui_l2", false)]);
        assert_eq!(
            m.event(abs(AbsoluteAxisCode::ABS_RZ, 1023)),
            [("ui_r2", true)]
        );
        // Digital trigger buttons share the event.
        assert!(m.event(key(KeyCode::BTN_TR2, true)).is_empty());
        assert!(m.event(abs(AbsoluteAxisCode::ABS_RZ, 0)).is_empty());
        assert_eq!(m.event(key(KeyCode::BTN_TR2, false)), [("ui_r2", false)]);
    }

    #[test]
    fn release_all_lets_go_of_everything() {
        let mut m = xbox();
        m.event(key(KeyCode::BTN_SOUTH, true));
        m.event(abs(AbsoluteAxisCode::ABS_HAT0X, 1));
        assert_eq!(m.release_all(), [("ui_accept", false), ("ui_right", false)]);
        assert!(m.release_all().is_empty());
    }

    struct Scripted(VecDeque<Vec<PadEvent>>);
    impl EventSource for Scripted {
        fn next_batch(&mut self) -> io::Result<Vec<PadEvent>> {
            self.0
                .pop_front()
                .ok_or_else(|| io::Error::other("unplugged"))
        }
    }

    #[test]
    fn a_pad_feeds_raw_input_and_releases_on_unplug() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut source = Scripted(VecDeque::from([
            vec![
                abs(AbsoluteAxisCode::ABS_HAT0X, 1),
                key(KeyCode::BTN_MODE, true),
            ],
            vec![key(KeyCode::BTN_MODE, false)],
        ]));
        run_pad("devpad:test", &mut source, xbox(), &tx);
        let got: Vec<(String, f64)> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|i| {
                assert_eq!(i.source, "devpad:test");
                (i.event, i.value)
            })
            .collect();
        assert_eq!(
            got,
            [
                ("ui_right".into(), 1.0),
                ("ui_guide".into(), 1.0),
                ("ui_guide".into(), 0.0),
                // The pad vanished with the d-pad held.
                ("ui_right".into(), 0.0),
            ]
        );
    }

    #[test]
    fn feeds_the_same_nav_and_repeat_as_inputplumber() {
        use super::super::{Intent, Repeat, translate};
        use hinteros_api::session::NavAction;
        use std::time::Instant;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut source = Scripted(VecDeque::from([vec![abs(AbsoluteAxisCode::ABS_HAT0Y, 1)]]));
        run_pad("devpad:t", &mut source, xbox(), &tx);
        let first = rx.try_recv().unwrap();
        assert_eq!(translate(&first.event), Some(Intent::Nav(NavAction::Down)));
        // The shell's Repeat works on (action, source) exactly as for InputPlumber.
        let t0 = Instant::now();
        let mut rep = Repeat::new(Duration::from_millis(400), Duration::from_millis(100));
        rep.press(NavAction::Down, &first.source, t0);
        assert_eq!(
            rep.tick(t0 + Duration::from_millis(400)),
            Some(NavAction::Down)
        );
        let release = rx.try_recv().unwrap();
        assert!(!release.pressed());
        rep.release(NavAction::Down, &release.source);
        assert_eq!(rep.deadline(), None);
    }
}
