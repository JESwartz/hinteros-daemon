//! The background thread that performs [`KeyOp`]s, and the backends it drives.
//!
//! Key events are paced (`key_delay` between events, 12 ms in the spike was reliable) and input
//! method characters more slowly (`ime_delay`, one character per commit like Steam's keyboard).
//! The input method connection is opened on first use and reopened after errors.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{debug, warn};

use super::code::KEY_CAPSLOCK;
use super::{ImeAction, KeyCaps, KeyInjector, KeyOp};

/// A keyboard device: press or release one key.
pub trait KeySink: Send {
    fn key(&mut self, code: u16, down: bool) -> io::Result<()>;
}

/// An input method connection.
pub trait ImeSink: Send {
    /// Commit one string.
    fn text(&mut self, text: &str) -> Result<()>;
    fn action(&mut self, action: ImeAction) -> Result<()>;
}

/// Opens an input method connection.
pub type ImeConnect = Box<dyn FnMut() -> Result<Box<dyn ImeSink>> + Send>;

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub key_delay: Duration,
    pub ime_delay: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            key_delay: Duration::from_millis(12),
            ime_delay: Duration::from_millis(80),
        }
    }
}

/// Performs the ops one after another.
pub struct Worker {
    keys: Option<Box<dyn KeySink>>,
    ime_connect: Option<ImeConnect>,
    ime: Option<Box<dyn ImeSink>>,
    /// Don't retry a failed input method connection before this.
    ime_retry_at: Option<Instant>,
    /// Caps Lock is on: the virtual keyboard pressed it an odd number of times.
    caps_lock: bool,
    timing: Timing,
}

impl Worker {
    pub fn new(
        keys: Option<Box<dyn KeySink>>,
        ime_connect: Option<ImeConnect>,
        timing: Timing,
    ) -> Self {
        Self {
            keys,
            ime_connect,
            ime: None,
            ime_retry_at: None,
            caps_lock: false,
            timing,
        }
    }

    pub fn caps(&self) -> KeyCaps {
        KeyCaps {
            keys: self.keys.is_some(),
            ime: self.ime_connect.is_some(),
        }
    }

    pub fn run(&mut self, op: &KeyOp) {
        match op {
            KeyOp::Tap { code, mods } => {
                let Some(keys) = &mut self.keys else {
                    debug!(code, "no virtual keyboard; dropping key");
                    return;
                };
                let delay = self.timing.key_delay;
                let mut events: Vec<(u16, bool)> = mods.iter().map(|&m| (m, true)).collect();
                events.push((*code, true));
                events.push((*code, false));
                events.extend(mods.iter().rev().map(|&m| (m, false)));
                for (code, down) in events {
                    match keys.key(code, down) {
                        Ok(()) if code == KEY_CAPSLOCK && down => self.caps_lock ^= true,
                        Ok(()) => {}
                        Err(e) => warn!(code, down, "virtual keyboard: {e}"),
                    }
                    thread::sleep(delay);
                }
            }
            KeyOp::Ime(text) => self.with_ime(|ime| ime.text(text)),
            KeyOp::ImeAction(action) => self.with_ime(|ime| ime.action(*action)),
            KeyOp::CapsLockOff => {
                if self.caps_lock {
                    debug!("caps lock off before typing");
                    self.run(&KeyOp::Tap {
                        code: KEY_CAPSLOCK,
                        mods: Vec::new(),
                    });
                }
            }
        }
    }

    fn with_ime(&mut self, f: impl Fn(&mut dyn ImeSink) -> Result<()>) {
        let Some(connect) = &mut self.ime_connect else {
            debug!("no input method; dropping input");
            return;
        };
        // A connection that broke (gamescope restarted) is dropped and the operation is tried
        // once more on a new one.
        let mut reconnected = false;
        loop {
            if self.ime.is_none() {
                if self.ime_retry_at.is_some_and(|at| Instant::now() < at) {
                    debug!("input method unavailable; dropping input");
                    return;
                }
                match connect() {
                    Ok(ime) => self.ime = Some(ime),
                    Err(e) => {
                        warn!("gamescope input method unavailable: {e:#}");
                        self.ime_retry_at = Some(Instant::now() + Duration::from_secs(5));
                        return;
                    }
                }
            }
            let ime = self.ime.as_mut().expect("connected above");
            match f(ime.as_mut()) {
                Ok(()) => break,
                Err(e) => {
                    warn!("gamescope input method: {e:#}");
                    self.ime = None;
                    if std::mem::replace(&mut reconnected, true) {
                        break;
                    }
                }
            }
        }
        thread::sleep(self.timing.ime_delay);
    }
}

/// A navigation tap that could not start within this long after it was asked for is
/// dropped: the user has moved on, and replaying it later is the "queue of moves" bug.
pub const STALE_NAV: Duration = Duration::from_millis(150);

/// Something for the worker thread to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub ops: Vec<KeyOp>,
    /// A direction tap: superseded by any newer one, dropped when stale.
    pub nav: bool,
    pub queued: Instant,
}

/// Take the next job to run from `pending` (in order), dropping navigation taps that a newer
/// navigation tap supersedes or that are older than [`STALE_NAV`] at `now`. Text and other
/// jobs are never dropped and keep their order.
pub fn next_job(pending: &mut VecDeque<Job>, now: Instant) -> Option<Job> {
    let newest_nav = pending.iter().rposition(|j| j.nav);
    let mut i = 0;
    pending.retain(|j| {
        let keep = !j.nav || (Some(i) == newest_nav && now.duration_since(j.queued) <= STALE_NAV);
        if !keep {
            debug!("dropping a stale navigation key");
        }
        i += 1;
        keep
    });
    pending.pop_front()
}

/// [`KeyInjector`] backed by a [`Worker`] on its own thread.
pub struct ThreadInjector {
    caps: KeyCaps,
    tx: mpsc::Sender<Job>,
}

impl ThreadInjector {
    pub fn spawn(mut worker: Worker) -> Self {
        let caps = worker.caps();
        let (tx, rx) = mpsc::channel::<Job>();
        thread::Builder::new()
            .name("keys".into())
            .spawn(move || {
                let mut pending = VecDeque::new();
                loop {
                    if pending.is_empty() {
                        match rx.recv() {
                            Ok(job) => pending.push_back(job),
                            Err(_) => return,
                        }
                    }
                    // Look at everything that arrived meanwhile before choosing.
                    pending.extend(rx.try_iter());
                    if let Some(job) = next_job(&mut pending, Instant::now()) {
                        for op in &job.ops {
                            worker.run(op);
                        }
                    }
                }
            })
            .expect("spawning the key injection thread");
        Self { caps, tx }
    }

    fn queue(&mut self, ops: Vec<KeyOp>, nav: bool) {
        let job = Job {
            ops,
            nav,
            queued: Instant::now(),
        };
        if !job.ops.is_empty() && self.tx.send(job).is_err() {
            warn!("key injection thread is gone");
        }
    }
}

impl KeyInjector for ThreadInjector {
    fn caps(&self) -> KeyCaps {
        self.caps
    }

    fn send(&mut self, ops: Vec<KeyOp>) {
        self.queue(ops, false);
    }

    fn send_nav(&mut self, op: KeyOp) {
        self.queue(vec![op], true);
    }
}

/// A stand-in keyboard for tests and dev desktops (`--fake-keyboard <file>`): appends
/// `down <code>` / `up <code>` lines to a file instead of creating a device.
pub struct FileSink {
    path: PathBuf,
}

impl FileSink {
    pub fn new(path: PathBuf) -> io::Result<Self> {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self { path })
    }
}

impl KeySink for FileSink {
    fn key(&mut self, code: u16, down: bool) -> io::Result<()> {
        let mut f = std::fs::OpenOptions::new().append(true).open(&self.path)?;
        writeln!(f, "{} {code}", if down { "down" } else { "up" })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::keys::code::*;

    /// Records everything sent to it.
    #[derive(Clone, Default)]
    pub(crate) struct Recorder(pub Arc<Mutex<Vec<String>>>);

    impl Recorder {
        pub(crate) fn take(&self) -> Vec<String> {
            std::mem::take(&mut self.0.lock().unwrap())
        }
    }

    impl KeySink for Recorder {
        fn key(&mut self, code: u16, down: bool) -> io::Result<()> {
            let what = if down { "down" } else { "up" };
            self.0.lock().unwrap().push(format!("{what} {code}"));
            Ok(())
        }
    }

    impl ImeSink for Recorder {
        fn text(&mut self, text: &str) -> Result<()> {
            if text == "fail" {
                anyhow::bail!("broken");
            }
            self.0.lock().unwrap().push(format!("ime {text}"));
            Ok(())
        }
        fn action(&mut self, action: ImeAction) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(format!("ime-action {}", action as u32));
            Ok(())
        }
    }

    const FAST: Timing = Timing {
        key_delay: Duration::ZERO,
        ime_delay: Duration::ZERO,
    };

    #[test]
    fn taps_with_modifiers_in_order() {
        let rec = Recorder::default();
        let mut w = Worker::new(Some(Box::new(rec.clone())), None, FAST);
        assert_eq!(
            w.caps(),
            KeyCaps {
                keys: true,
                ime: false
            }
        );
        w.run(&KeyOp::Tap {
            code: 20,
            mods: vec![KEY_LEFTCTRL, KEY_LEFTSHIFT],
        });
        assert_eq!(
            rec.take(),
            ["down 29", "down 42", "down 20", "up 20", "up 42", "up 29"]
        );
        // No input method: dropped, nothing breaks.
        w.run(&KeyOp::Ime("é".into()));
        assert!(rec.take().is_empty());
    }

    #[test]
    fn caps_lock_is_turned_off_before_text() {
        let rec = Recorder::default();
        let mut w = Worker::new(Some(Box::new(rec.clone())), None, FAST);
        let caps = KeyOp::Tap {
            code: KEY_CAPSLOCK,
            mods: vec![],
        };
        // Off: nothing to do.
        w.run(&KeyOp::CapsLockOff);
        assert!(rec.take().is_empty());
        // A bound caps_lock key turned it on: one tap turns it off again, once.
        w.run(&caps);
        rec.take();
        w.run(&KeyOp::CapsLockOff);
        assert_eq!(rec.take(), ["down 58", "up 58"]);
        w.run(&KeyOp::CapsLockOff);
        assert!(rec.take().is_empty());
        // Pressed twice: off again.
        w.run(&caps);
        w.run(&caps);
        rec.take();
        w.run(&KeyOp::CapsLockOff);
        assert!(rec.take().is_empty());
    }

    #[test]
    fn input_method_connects_lazily_and_reconnects() {
        let rec = Recorder::default();
        let connects = Arc::new(Mutex::new(0));
        let (r, n) = (rec.clone(), connects.clone());
        let connect: ImeConnect = Box::new(move || {
            *n.lock().unwrap() += 1;
            Ok(Box::new(r.clone()) as Box<dyn ImeSink>)
        });
        let mut w = Worker::new(None, Some(connect), FAST);
        assert_eq!(*connects.lock().unwrap(), 0);
        w.run(&KeyOp::Ime("é".into()));
        w.run(&KeyOp::ImeAction(ImeAction::DeleteLeft));
        assert_eq!(rec.take(), ["ime é", "ime-action 2"]);
        assert_eq!(*connects.lock().unwrap(), 1);
        // An error drops the connection and is tried once more on a new one (here it fails
        // again); the next op reconnects.
        w.run(&KeyOp::Ime("fail".into()));
        assert_eq!(*connects.lock().unwrap(), 2);
        w.run(&KeyOp::Ime("x".into()));
        assert_eq!(rec.take(), ["ime x"]);
        assert_eq!(*connects.lock().unwrap(), 3);
        // Keys without a keyboard are dropped.
        w.run(&KeyOp::Tap {
            code: 1,
            mods: vec![],
        });
        assert!(rec.take().is_empty());
    }

    #[test]
    fn failed_connections_are_retried_later() {
        let tries = Arc::new(Mutex::new(0));
        let t = tries.clone();
        let connect: ImeConnect = Box::new(move || {
            *t.lock().unwrap() += 1;
            anyhow::bail!("no gamescope")
        });
        let mut w = Worker::new(None, Some(connect), FAST);
        w.run(&KeyOp::Ime("a".into()));
        w.run(&KeyOp::Ime("b".into()));
        assert_eq!(*tries.lock().unwrap(), 1, "no retry within 5 s");
    }

    #[test]
    fn navigation_taps_never_queue() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let job = |code: u16, nav: bool, at: Duration| Job {
            ops: vec![KeyOp::Tap { code, mods: vec![] }],
            nav,
            queued: t0 + at,
        };
        // Left, right, left, right arrived while a long text was being typed: only the newest
        // direction is left, text keeps its place and order.
        let mut pending = VecDeque::from([
            job(KEY_LEFT, true, ms(0)),
            job(30, false, ms(1)),
            job(KEY_RIGHT, true, ms(2)),
            job(31, false, ms(3)),
            job(KEY_LEFT, true, ms(4)),
            job(KEY_RIGHT, true, ms(5)),
        ]);
        let mut ran = vec![];
        while let Some(j) = next_job(&mut pending, t0 + ms(10)) {
            ran.push(j.ops);
        }
        let tap = |code| vec![KeyOp::Tap { code, mods: vec![] }];
        assert_eq!(ran, [tap(30), tap(31), tap(KEY_RIGHT)]);

        // A direction that waited too long is dropped; a fresh one runs.
        let mut pending = VecDeque::from([job(KEY_UP, true, ms(0))]);
        assert_eq!(next_job(&mut pending, t0 + STALE_NAV + ms(1)), None);
        let mut pending = VecDeque::from([job(KEY_UP, true, ms(0))]);
        assert!(next_job(&mut pending, t0 + STALE_NAV).is_some());
        // Non-navigation jobs are never stale.
        let mut pending = VecDeque::from([job(KEY_ENTER, false, ms(0))]);
        assert!(next_job(&mut pending, t0 + Duration::from_secs(60)).is_some());
    }

    #[test]
    fn thread_injector_and_file_sink() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.log");
        let sink = FileSink::new(path.clone()).unwrap();
        let mut inj = ThreadInjector::spawn(Worker::new(Some(Box::new(sink)), None, FAST));
        assert!(inj.caps().keys && !inj.caps().ime);
        inj.send(vec![KeyOp::Tap {
            code: KEY_ENTER,
            mods: vec![],
        }]);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let text = std::fs::read_to_string(&path).unwrap();
            if text == "down 28\nup 28\n" {
                break;
            }
            assert!(Instant::now() < deadline, "got {text:?}");
            thread::sleep(Duration::from_millis(10));
        }
    }
}
