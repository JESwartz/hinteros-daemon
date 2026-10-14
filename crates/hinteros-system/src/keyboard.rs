//! `hinteros-system keyboard-home`: Home from a keyboard, Ctrl + Super (the Windows key) + D,
//! like a short Guide press.
//!
//! This is the only reader of keyboards on the box. No profile account may read them (a game
//! would log the admin password typed into the launcher's prompt or on the admin console), so
//! the session daemon cannot watch for the chord itself. This runs in its own unit
//! (`hinteros-keyboard.service`): a dynamic user in group `input` and the keyboard socket's
//! group, keyboards opened read-only (the unit also allows reads only), never grabbed (no
//! `EVIOCGRAB`, so the focused app gets the keys too), no network, no capabilities. It keeps
//! no keys: only [`Chord`]'s state of Ctrl, Super and D.
//!
//! Home fires once when D goes down while at least one Ctrl and one Super are held on the same
//! keyboard, and not again until D is released. Each press is one `home` line on the system
//! service's keyboard socket (`/run/hinteros/keyboard.sock`, root:`hinteros-keyboard` 0660); the
//! service sends `keyboard_home` to the session daemon of the account owning seat0's active
//! session (`service.rs`), which goes Home at once, like SIGUSR1.
//!
//! [`Chord`] is the pure part; [`KeySource`] is the seam for tests; [`main`] is the real thing.

use std::collections::HashSet;
use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use evdev::{EventType, KeyCode};
use tracing::{debug, info, warn};

/// The line the service reads per press.
pub const HOME_LINE: &str = "home";

/// The session daemon's uinput keyboard (`hinteros-session`'s `keys::uinput::DEVICE_NAME`): it
/// types `keys.type` text and must never trigger Home. Any device named like ours is skipped.
const OWN_KEYBOARD: &str = "HinterOS virtual keyboard";

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

/// Where a keyboard's key changes come from.
pub trait KeySource: Send {
    /// The next batch of `(code, pressed)`, blocking, autorepeat dropped. An error ends the
    /// keyboard (unplugged).
    fn next_keys(&mut self) -> io::Result<Vec<(u16, bool)>>;
}

/// A real `/dev/input/event*` device, opened read-only and never grabbed.
struct Evdev(evdev::Device);

impl KeySource for Evdev {
    fn next_keys(&mut self) -> io::Result<Vec<(u16, bool)>> {
        Ok(self
            .0
            .fetch_events()?
            // Value 2 is the kernel's autorepeat.
            .filter(|ev| ev.event_type() == EventType::KEY && ev.value() < 2)
            .map(|ev| (ev.code(), ev.value() == 1))
            .collect())
    }
}

/// Run one keyboard until its source fails, sending a [`KeyboardHome`] per chord.
pub fn run_keyboard(name: &str, source: &mut dyn KeySource, tx: &mpsc::Sender<KeyboardHome>) {
    let mut chord = Chord::default();
    loop {
        match source.next_keys() {
            Ok(batch) => {
                for (code, pressed) in batch {
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

/// Whether the device is a keyboard with the chord's keys, and not the session daemon's.
fn is_keyboard(dev: &evdev::Device) -> bool {
    let name = dev.name().unwrap_or("");
    if name == OWN_KEYBOARD || name.to_lowercase().contains("hinteros") {
        return false;
    }
    dev.supported_keys().is_some_and(|k| {
        k.contains(KeyCode::KEY_D)
            && (k.contains(KeyCode::KEY_LEFTCTRL) || k.contains(KeyCode::KEY_RIGHTCTRL))
            && (k.contains(KeyCode::KEY_LEFTMETA) || k.contains(KeyCode::KEY_RIGHTMETA))
    })
}

/// The `event*` devices in `dir` that open read-only (others are skipped).
fn enumerate(dir: &Path) -> Vec<(PathBuf, evdev::Device)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_name().as_encoded_bytes().starts_with(b"event"))
        .filter_map(|e| {
            // Read-only: this reader never writes to a keyboard (no LEDs, no injection).
            let file = std::fs::File::open(e.path()).ok()?;
            let dev = evdev::Device::from_fd(file.into()).ok()?;
            Some((e.path(), dev))
        })
        .collect()
}

/// Watch `dir` for keyboards and send Home on the chord. Keyboards that appear later are picked
/// up (every two seconds). Plain threads, one per keyboard.
fn spawn_scanner(dir: PathBuf, tx: mpsc::Sender<KeyboardHome>) {
    let open: Arc<Mutex<HashSet<PathBuf>>> = Arc::default();
    let result = std::thread::Builder::new()
        .name("keyboard-scan".into())
        .spawn(move || {
            let mut told = false;
            loop {
                let mut found = 0;
                for (path, dev) in enumerate(&dir) {
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
                    info!("no readable keyboard (yet)");
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        });
    if let Err(e) = result {
        warn!("cannot start the keyboard scanner: {e}");
    }
}

/// Write one `home` line per press to the service's socket, connecting when needed. A press
/// while the service is down is dropped (logged once until it is back).
pub fn deliver(socket: &Path, homes: mpsc::Receiver<KeyboardHome>) {
    let mut conn: Option<UnixStream> = None;
    let mut down_logged = false;
    for home in homes {
        // A stale connection (the service restarted) fails on write: connect again once.
        let mut sent = false;
        for _ in 0..2 {
            if conn.is_none() {
                match UnixStream::connect(socket) {
                    Ok(s) => conn = Some(s),
                    Err(e) => {
                        if !down_logged {
                            warn!(socket = %socket.display(), "system service unavailable; Home dropped: {e}");
                            down_logged = true;
                        }
                        break;
                    }
                }
            }
            let line = format!("{HOME_LINE}\n");
            match conn.as_mut().map(|c| c.write_all(line.as_bytes())) {
                Some(Ok(())) => {
                    sent = true;
                    break;
                }
                _ => conn = None,
            }
        }
        if sent {
            down_logged = false;
            debug!(keyboard = %home.source, "Home sent to the system service");
        }
    }
}

/// `keyboard-home`: read every keyboard in `input_dir`, report the chord on `socket`.
pub fn main(input_dir: &Path, socket: &Path) -> ExitCode {
    info!(socket = %socket.display(), "reading keyboards for Ctrl+Super+D (read-only, never grabbed)");
    let (tx, rx) = mpsc::channel();
    spawn_scanner(input_dir.to_path_buf(), tx);
    deliver(socket, rx);
    // The scanner never ends; only a failure to start it gets here.
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::BufRead;

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

    /// Scripted batches, then "unplugged".
    struct Scripted(VecDeque<Vec<(u16, bool)>>);
    impl KeySource for Scripted {
        fn next_keys(&mut self) -> io::Result<Vec<(u16, bool)>> {
            self.0
                .pop_front()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unplugged"))
        }
    }

    fn key(k: KeyCode, pressed: bool) -> (u16, bool) {
        (k.0, pressed)
    }

    #[test]
    fn a_keyboard_sends_home_and_starts_clean_after_it_is_gone() {
        let (tx, rx) = mpsc::channel();
        let mut source = Scripted(VecDeque::from([
            vec![key(LCTRL, true), key(RMETA, true)],
            vec![key(D, true)],
            vec![key(D, false), key(D, true), key(D, false)],
            // Unplugged here with Ctrl and Super still held.
        ]));
        run_keyboard("keyboard:test", &mut source, &tx);
        let got: Vec<_> = rx.try_iter().collect();
        let home = KeyboardHome {
            source: "keyboard:test".into(),
        };
        assert_eq!(got, [home.clone(), home]);
        // The same keyboard back: its old modifiers are not held any more.
        let mut source = Scripted(VecDeque::from([vec![key(D, true)]]));
        run_keyboard("keyboard:test", &mut source, &tx);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn presses_go_to_the_socket_one_line_each_and_survive_a_service_restart() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("keyboard.sock");
        let home = || KeyboardHome {
            source: "keyboard:test".into(),
        };
        let (tx, rx) = mpsc::channel();
        // The service is down: the press is dropped, not queued.
        tx.send(home()).unwrap();
        let sock = socket.clone();
        let deliverer = std::thread::spawn(move || deliver(&sock, rx));
        std::thread::sleep(Duration::from_millis(100));

        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        tx.send(home()).unwrap();
        tx.send(home()).unwrap();
        let (conn, _) = listener.accept().unwrap();
        let mut lines = io::BufReader::new(conn).lines();
        assert_eq!(lines.next().unwrap().unwrap(), HOME_LINE);
        assert_eq!(lines.next().unwrap().unwrap(), HOME_LINE);

        // The service restarts: the next press reconnects.
        drop(lines);
        drop(listener);
        std::fs::remove_file(&socket).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        tx.send(home()).unwrap();
        let (conn, _) = listener.accept().unwrap();
        let mut lines = io::BufReader::new(conn).lines();
        assert_eq!(lines.next().unwrap().unwrap(), HOME_LINE);
        drop(tx);
        deliverer.join().unwrap();
    }
}
