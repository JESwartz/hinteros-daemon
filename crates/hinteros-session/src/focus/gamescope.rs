//! [`FocusControl`] for gamescope, through the X properties on its Xwayland root window.
//!
//! No auth is needed: gamescope's Xwayland accepts local connections from the same user.

use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, PropMode, Window,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::{FocusControl, GamescopeEvent, GamescopeState, baselayer_for};

x11rb::atom_manager! {
    /// The gamescope root-window properties we use.
    pub Atoms: AtomsCookie {
        GAMESCOPE_PID,
        GAMESCOPE_FOCUSABLE_APPS,
        GAMESCOPE_FOCUSED_APP,
        GAMESCOPECTRL_BASELAYER_APPID,
        GAMESCOPECTRL_BASELAYER_WINDOW,
    }
}

/// One connection to gamescope's Xwayland.
struct Conn {
    conn: RustConnection,
    root: Window,
    atoms: Atoms,
}

impl Conn {
    /// Connect to `display` and check that it really is gamescope (it sets `GAMESCOPE_PID`).
    fn open(display: &str) -> Result<Self> {
        let (conn, screen) =
            x11rb::connect(Some(display)).with_context(|| format!("connecting to X {display}"))?;
        let root = conn.setup().roots[screen].root;
        let atoms = Atoms::new(&conn)?.reply()?;
        let c = Self { conn, root, atoms };
        if c.cardinals(c.atoms.GAMESCOPE_PID)?.is_empty() {
            bail!("X display {display} is not gamescope's Xwayland (no GAMESCOPE_PID on the root)");
        }
        Ok(c)
    }

    fn cardinals(&self, atom: u32) -> Result<Vec<u32>> {
        let reply = self
            .conn
            .get_property(false, self.root, atom, AtomEnum::CARDINAL, 0, 4096)?
            .reply()?;
        Ok(reply.value32().map(Iterator::collect).unwrap_or_default())
    }

    fn set_cardinals(&self, atom: u32, values: &[u32]) -> Result<()> {
        self.conn
            .change_property32(
                PropMode::REPLACE,
                self.root,
                atom,
                AtomEnum::CARDINAL,
                values,
            )?
            .check()?;
        Ok(())
    }

    fn state(&self) -> Result<GamescopeState> {
        Ok(GamescopeState {
            focused_app: self
                .cardinals(self.atoms.GAMESCOPE_FOCUSED_APP)?
                .first()
                .copied()
                .filter(|&id| id != 0),
            focusable_apps: self.cardinals(self.atoms.GAMESCOPE_FOCUSABLE_APPS)?,
            baselayer_apps: self.cardinals(self.atoms.GAMESCOPECTRL_BASELAYER_APPID)?,
        })
    }

    /// Write the focus order. A `GAMESCOPECTRL_BASELAYER_WINDOW` would override it, so it is
    /// removed.
    fn set_baselayer(&self, apps: &[u32]) -> Result<()> {
        self.conn
            .delete_property(self.root, self.atoms.GAMESCOPECTRL_BASELAYER_WINDOW)?
            .check()?;
        self.set_cardinals(self.atoms.GAMESCOPECTRL_BASELAYER_APPID, apps)
    }
}

/// Focus control through gamescope's root-window properties. Connects lazily and reconnects
/// after errors, so gamescope may start after (or restart under) the daemon.
pub struct GamescopeFocus {
    display: String,
    launcher_app_id: u32,
    conn: Option<Conn>,
}

impl GamescopeFocus {
    pub fn new(display: impl Into<String>, launcher_app_id: u32) -> Self {
        Self {
            display: display.into(),
            launcher_app_id,
            conn: None,
        }
    }

    fn with<T>(&mut self, f: impl FnOnce(&Conn) -> Result<T>) -> Result<T> {
        let conn = match &mut self.conn {
            Some(conn) => conn,
            slot => slot.insert(Conn::open(&self.display)?),
        };
        let result = f(conn);
        if result.is_err() {
            self.conn = None;
        }
        result
    }

    /// Read all focus properties at once.
    pub fn state(&mut self) -> Result<GamescopeState> {
        self.with(Conn::state)
    }

    fn set_focus(&mut self, target: Option<u32>) -> Result<()> {
        let apps = baselayer_for(target, self.launcher_app_id);
        debug!(?apps, "GAMESCOPECTRL_BASELAYER_APPID");
        self.with(|c| c.set_baselayer(&apps))
    }
}

impl FocusControl for GamescopeFocus {
    fn focusable_apps(&mut self) -> Result<Vec<u32>> {
        self.with(|c| c.cardinals(c.atoms.GAMESCOPE_FOCUSABLE_APPS))
    }

    fn focused_app(&mut self) -> Result<Option<u32>> {
        Ok(self.state()?.focused_app)
    }

    fn focus_launcher(&mut self) -> Result<()> {
        self.set_focus(None)
    }

    fn focus_app(&mut self, app_id: u32) -> Result<()> {
        self.set_focus(Some(app_id))
    }
}

/// Watch gamescope's focus properties on a background thread and report changes. Retries the
/// connection every `retry` until `events` is closed.
pub fn spawn_watcher(
    dpy: String,
    retry: Duration,
    events: mpsc::UnboundedSender<GamescopeEvent>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("gamescope-watch".into())
        .spawn(move || {
            let mut warned = false;
            while !events.is_closed() {
                match watch_once(&dpy, &events) {
                    Ok(()) => return,
                    Err(e) if !warned => {
                        warn!(display = %dpy, "gamescope focus unavailable, retrying: {e:#}");
                        warned = true;
                    }
                    Err(e) => debug!(display = %dpy, "gamescope still unavailable: {e:#}"),
                }
                thread::sleep(retry);
            }
        })
        .expect("spawning the gamescope watcher thread")
}

/// One connection's worth of watching. `Ok` means the receiver is gone.
fn watch_once(dpy: &str, events: &mpsc::UnboundedSender<GamescopeEvent>) -> Result<()> {
    let c = Conn::open(dpy)?;
    c.conn
        .change_window_attributes(
            c.root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?
        .check()?;
    info!(display = %dpy, "watching gamescope focus");
    let send = |e| events.send(e).is_ok();
    if !send(GamescopeEvent::Connected) || !send(GamescopeEvent::State(c.state()?)) {
        return Ok(());
    }
    let watched = [
        c.atoms.GAMESCOPE_FOCUSED_APP,
        c.atoms.GAMESCOPE_FOCUSABLE_APPS,
        c.atoms.GAMESCOPECTRL_BASELAYER_APPID,
    ];
    let mut last = None;
    let result = loop {
        match c.conn.wait_for_event() {
            Ok(Event::PropertyNotify(ev)) if ev.window == c.root && watched.contains(&ev.atom) => {
                let state = match c.state() {
                    Ok(s) => s,
                    Err(e) => break Err(e),
                };
                if last.as_ref() != Some(&state) {
                    if !send(GamescopeEvent::State(state.clone())) {
                        return Ok(());
                    }
                    last = Some(state);
                }
            }
            Ok(_) => {}
            Err(e) => break Err(e.into()),
        }
    };
    let _ = events.send(GamescopeEvent::Disconnected);
    result
}

#[cfg(test)]
mod tests {
    //! Against a real X server (Xvfb) standing in for gamescope's Xwayland: we set the
    //! properties gamescope would publish and check what the controller reads and writes.
    //! Skipped when `Xvfb` is not on PATH.

    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};

    use super::*;

    struct Xvfb {
        child: Child,
        display: String,
    }

    impl Xvfb {
        fn start() -> Option<Self> {
            let mut child = match Command::new("Xvfb")
                .args([
                    "-displayfd",
                    "1",
                    "-nolisten",
                    "tcp",
                    "-screen",
                    "0",
                    "64x64x24",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => child,
                Err(e) => {
                    assert!(
                        std::env::var_os("HINTEROS_REQUIRE_TEST_TOOLS").is_none(),
                        "Xvfb: {e}"
                    );
                    eprintln!("Xvfb not found, skipping");
                    return None;
                }
            };
            let mut line = String::new();
            BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            Some(Self {
                child,
                display: format!(":{}", line.trim()),
            })
        }

        /// A raw connection playing gamescope.
        fn gamescope(&self) -> Conn {
            let (conn, screen) = x11rb::connect(Some(&self.display)).unwrap();
            let root = conn.setup().roots[screen].root;
            let atoms = Atoms::new(&conn).unwrap().reply().unwrap();
            Conn { conn, root, atoms }
        }
    }

    impl Drop for Xvfb {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[test]
    fn reads_and_writes_gamescope_properties() {
        let Some(x) = Xvfb::start() else { return };
        let mut focus = GamescopeFocus::new(&x.display, 4000);

        // Not gamescope yet: refuses.
        let err = focus.focus_launcher().unwrap_err();
        assert!(format!("{err:#}").contains("not gamescope"), "{err:#}");

        let gs = x.gamescope();
        gs.set_cardinals(gs.atoms.GAMESCOPE_PID, &[1234]).unwrap();
        gs.set_cardinals(gs.atoms.GAMESCOPECTRL_BASELAYER_WINDOW, &[0x200001])
            .unwrap();
        gs.set_cardinals(gs.atoms.GAMESCOPE_FOCUSABLE_APPS, &[570, 4000])
            .unwrap();
        gs.set_cardinals(gs.atoms.GAMESCOPE_FOCUSED_APP, &[4000])
            .unwrap();

        assert_eq!(focus.focusable_apps().unwrap(), vec![570, 4000]);
        assert_eq!(focus.focused_app().unwrap(), Some(4000));

        focus.focus_app(570).unwrap();
        assert_eq!(
            gs.cardinals(gs.atoms.GAMESCOPECTRL_BASELAYER_APPID)
                .unwrap(),
            vec![570, 4000]
        );
        assert!(
            gs.cardinals(gs.atoms.GAMESCOPECTRL_BASELAYER_WINDOW)
                .unwrap()
                .is_empty(),
            "a base layer window would override the app list"
        );
        focus.focus_launcher().unwrap();
        assert_eq!(
            gs.cardinals(gs.atoms.GAMESCOPECTRL_BASELAYER_APPID)
                .unwrap(),
            vec![4000]
        );

        // An empty FOCUSED_APP (gamescope writes 0 items) is "nothing focused".
        gs.set_cardinals(gs.atoms.GAMESCOPE_FOCUSED_APP, &[])
            .unwrap();
        assert_eq!(focus.focused_app().unwrap(), None);
    }

    #[test]
    fn watcher_reports_changes() {
        let Some(x) = Xvfb::start() else { return };
        let gs = x.gamescope();
        gs.set_cardinals(gs.atoms.GAMESCOPE_PID, &[1]).unwrap();
        gs.set_cardinals(gs.atoms.GAMESCOPE_FOCUSED_APP, &[4000])
            .unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        spawn_watcher(x.display.clone(), Duration::from_millis(50), tx);
        let next = |rx: &mut mpsc::UnboundedReceiver<GamescopeEvent>| {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                if let Ok(e) = rx.try_recv() {
                    return e;
                }
                assert!(std::time::Instant::now() < deadline, "no event");
                thread::sleep(Duration::from_millis(10));
            }
        };
        assert_eq!(next(&mut rx), GamescopeEvent::Connected);
        let GamescopeEvent::State(s) = next(&mut rx) else {
            panic!("expected state")
        };
        assert_eq!(s.focused_app, Some(4000));

        gs.set_cardinals(gs.atoms.GAMESCOPE_FOCUSABLE_APPS, &[570])
            .unwrap();
        loop {
            match next(&mut rx) {
                GamescopeEvent::State(s) if s.focusable_apps == [570] => break,
                GamescopeEvent::State(_) => {}
                other => panic!("unexpected {other:?}"),
            }
        }
    }
}
