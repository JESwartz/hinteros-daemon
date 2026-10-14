//! Home from a keyboard: Ctrl + Super (the Windows key) + D, like a short Guide press.
//!
//! Every keyboard is read through evdev, read-only and never grabbed (no `EVIOCGRAB`): the
//! focused app gets the keys too (apps practically never use this chord). Home fires once when
//! D goes down while at least one Ctrl and one Super are held on the same keyboard, and not
//! again until D is released. It goes to the shell as [`crate::shell::Msg::Home`], the same as
//! SIGUSR1: at once, never through the Guide long-press timer or the on-screen keyboard chord.
//!
//! Needs read access to `/dev/input/event*`: a keyboard on the active seat through logind
//! uaccess (the system adds the rule; systemd's own rules only cover joysticks), else group
//! `input`. A keyboard that cannot be opened is skipped. `[input] keyboard_home` turns it off.
//!
//! [`Chord`] is the pure part; the [`EventSource`] seam is shared with [`super::devpad`];
//! [`spawn`] is the real thing.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use evdev::KeyCode;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::devpad::{Evdev, EventSource, PadEvent};

/// One Home press from a keyboard (`source` names the device, for logs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyboardHome {
    pub source: String,
}

/// Ctrl + Super + D on one keyboard.
#[derive(Debug, Default)]
pub struct Chord {
    /// Ctrl and Super keys held (either side).
    mods: HashSet<u16>,
    /// D is down (whether or not it fired).
    d_down: bool,
}

fn is_ctrl(code: u16) -> bool {
    code == KeyCode::KEY_LEFTCTRL.0 || code == KeyCode::KEY_RIGHTCTRL.0
}

fn is_super(code: u16) -> bool {
    code == KeyCode::KEY_LEFTMETA.0 || code == KeyCode::KEY_RIGHTMETA.0
}

impl Chord {
    /// Feed one key change (autorepeat already dropped); true: Home.
    pub fn key(&mut self, code: u16, pressed: bool) -> bool {
        if is_ctrl(code) || is_super(code) {
            if pressed {
                self.mods.insert(code);
            } else {
                self.mods.remove(&code);
            }
            return false;
        }
        if code != KeyCode::KEY_D.0 {
            return false;
        }
        let fire = pressed && !self.d_down && self.held();
        self.d_down = pressed;
        fire
    }

    fn held(&self) -> bool {
        self.mods.iter().any(|&c| is_ctrl(c)) && self.mods.iter().any(|&c| is_super(c))
    }

    /// Forget everything held (the keyboard went away).
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Run one keyboard until its source fails, sending a [`KeyboardHome`] per chord.
pub fn run_keyboard(
    name: &str,
    source: &mut dyn EventSource,
    tx: &mpsc::UnboundedSender<KeyboardHome>,
) {
    let mut chord = Chord::default();
    loop {
        match source.next_batch() {
            Ok(batch) => {
                for ev in batch {
                    let PadEvent::Key { code, pressed } = ev else {
                        continue;
                    };
                    if chord.key(code, pressed) {
                        info!(keyboard = name, "Ctrl+Super+D: Home pressed");
                        let home = KeyboardHome {
                            source: name.to_string(),
                        };
                        if tx.send(home).is_err() {
                            return;
                        }
                    }
                }
            }
            Err(e) => {
                debug!(keyboard = name, "keyboard ended: {e}");
                chord.reset();
                return;
            }
        }
    }
}

/// Whether the device is a keyboard with the chord's keys, and not one of ours (the daemon's
/// virtual keyboard types `keys.type` text and must never trigger Home).
fn is_keyboard(dev: &evdev::Device) -> bool {
    let name = dev.name().unwrap_or("");
    if name == crate::keys::uinput::DEVICE_NAME || name.to_lowercase().contains("hinteros") {
        return false;
    }
    dev.supported_keys().is_some_and(|k| {
        k.contains(KeyCode::KEY_D)
            && (k.contains(KeyCode::KEY_LEFTCTRL) || k.contains(KeyCode::KEY_RIGHTCTRL))
            && (k.contains(KeyCode::KEY_LEFTMETA) || k.contains(KeyCode::KEY_RIGHTMETA))
    })
}

/// Watch `/dev/input` for keyboards and send Home on the chord. Keyboards that appear later are
/// picked up (every two seconds). Plain threads, like [`super::devpad::spawn`].
pub fn spawn(tx: mpsc::UnboundedSender<KeyboardHome>) {
    let open: Arc<Mutex<HashSet<PathBuf>>> = Arc::default();
    let result = std::thread::Builder::new()
        .name("keyboard-scan".into())
        .spawn(move || {
            let mut told = false;
            loop {
                let mut found = 0;
                // enumerate() skips devices it cannot open (EACCES): not on our seat.
                for (path, dev) in evdev::enumerate() {
                    if !is_keyboard(&dev) {
                        continue;
                    }
                    found += 1;
                    if open.lock().is_ok_and(|o| o.contains(&path)) {
                        continue;
                    }
                    let name = format!("keyboard:{}", path.display());
                    info!(keyboard = %name, name = dev.name().unwrap_or("?"), "keyboard found (Ctrl+Super+D is Home)");
                    if let Ok(mut o) = open.lock() {
                        o.insert(path.clone());
                    }
                    let (tx, open) = (tx.clone(), open.clone());
                    let reader = std::thread::Builder::new()
                        .name("keyboard".into())
                        .spawn(move || {
                            run_keyboard(&name, &mut Evdev(dev), &tx);
                            if let Ok(mut o) = open.lock() {
                                o.remove(&path);
                            }
                            info!(keyboard = %name, "keyboard gone");
                        });
                    if let Err(e) = reader {
                        warn!("cannot start a keyboard reader: {e}");
                    }
                }
                if found == 0 && !told {
                    told = true;
                    info!(
                        "no readable keyboard (yet); Ctrl+Super+D needs read access to the \
                         keyboard (logind uaccess on the active seat, or group 'input')"
                    );
                }
                if tx.is_closed() {
                    return;
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        });
    if let Err(e) = result {
        warn!("cannot start the keyboard scanner: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::devpad::tests::Scripted;
    use std::collections::VecDeque;

    const LCTRL: KeyCode = KeyCode::KEY_LEFTCTRL;
    const RCTRL: KeyCode = KeyCode::KEY_RIGHTCTRL;
    const LMETA: KeyCode = KeyCode::KEY_LEFTMETA;
    const RMETA: KeyCode = KeyCode::KEY_RIGHTMETA;
    const D: KeyCode = KeyCode::KEY_D;

    /// Feed `(key, pressed)` steps; the indices of the steps that fired Home.
    fn fires(chord: &mut Chord, steps: &[(KeyCode, bool)]) -> Vec<usize> {
        steps
            .iter()
            .enumerate()
            .filter(|(_, (k, p))| chord.key(k.0, *p))
            .map(|(i, _)| i)
            .collect()
    }

    #[test]
    fn every_ctrl_and_super_side_works() {
        for ctrl in [LCTRL, RCTRL] {
            for meta in [LMETA, RMETA] {
                let mut c = Chord::default();
                let steps = [(ctrl, true), (meta, true), (D, true), (D, false)];
                assert_eq!(fires(&mut c, &steps), [2], "{ctrl:?} {meta:?}");
                // Super first, then Ctrl.
                let mut c = Chord::default();
                let steps = [(meta, true), (ctrl, true), (D, true)];
                assert_eq!(fires(&mut c, &steps), [2], "{meta:?} {ctrl:?}");
            }
        }
    }

    #[test]
    fn d_before_the_modifiers_is_no_home() {
        let mut c = Chord::default();
        assert!(fires(&mut c, &[(D, true), (LCTRL, true), (LMETA, true)]).is_empty());
        // Only a new D press after that counts.
        assert_eq!(fires(&mut c, &[(D, false), (D, true)]), [1]);
    }

    #[test]
    fn once_per_d_press() {
        let mut c = Chord::default();
        assert_eq!(
            fires(&mut c, &[(LCTRL, true), (LMETA, true), (D, true)]),
            [2]
        );
        // A second down without a release (e.g. a repeat that got through) does nothing.
        assert!(!c.key(D.0, true));
        // Modifiers released and pressed again while D is still down: nothing.
        assert!(fires(&mut c, &[(LCTRL, false), (LCTRL, true)]).is_empty());
        // D released and pressed again with the modifiers held: Home again.
        assert_eq!(fires(&mut c, &[(D, false), (D, true)]), [1]);
    }

    #[test]
    fn ctrl_d_or_super_d_alone_is_nothing() {
        let mut c = Chord::default();
        assert!(
            fires(
                &mut c,
                &[(LCTRL, true), (D, true), (D, false), (LCTRL, false)]
            )
            .is_empty()
        );
        assert!(
            fires(
                &mut c,
                &[(RMETA, true), (D, true), (D, false), (RMETA, false)]
            )
            .is_empty()
        );
        // One side of Ctrl released while the other is held still counts as Ctrl held.
        let steps = [
            (LCTRL, true),
            (RCTRL, true),
            (LCTRL, false),
            (LMETA, true),
            (D, true),
        ];
        assert_eq!(fires(&mut c, &steps), [4]);
        // Other keys do nothing.
        assert!(!c.key(KeyCode::KEY_E.0, true));
    }

    #[test]
    fn reset_forgets_held_keys() {
        let mut c = Chord::default();
        fires(&mut c, &[(LCTRL, true), (LMETA, true)]);
        c.reset();
        assert!(!c.key(D.0, true));
    }

    fn key(k: KeyCode, pressed: bool) -> PadEvent {
        PadEvent::Key { code: k.0, pressed }
    }

    #[test]
    fn a_keyboard_sends_home_and_starts_clean_after_it_is_gone() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut source = Scripted(VecDeque::from([
            vec![key(LCTRL, true), key(RMETA, true)],
            vec![key(D, true)],
            vec![key(D, false), key(D, true), key(D, false)],
            // Unplugged here with Ctrl and Super still held.
        ]));
        run_keyboard("keyboard:test", &mut source, &tx);
        let got: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let home = KeyboardHome {
            source: "keyboard:test".into(),
        };
        assert_eq!(got, [home.clone(), home]);
        // The same keyboard back: its old modifiers are not held any more.
        let mut source = Scripted(VecDeque::from([vec![key(D, true)]]));
        run_keyboard("keyboard:test", &mut source, &tx);
        assert!(rx.try_recv().is_err());
    }
}
