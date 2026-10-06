//! [`FocusControl`] for gamescope, through the X properties on its Xwayland root window.
//!
//! No auth is needed: gamescope's Xwayland accepts local connections from the same user.

use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hinteros_api::session::{STEAM_CLIENT_GAMESCOPE_APP_ID, SteamWindow};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::res::{ClientIdMask, ClientIdSpec, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, GetPropertyReply, MapState,
    PropMode, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::{FocusControl, GamescopeEvent, GamescopeState, baselayer_for};
use crate::procs;
use crate::steam::windows::{self, HelperType, WindowInfo};

x11rb::atom_manager! {
    /// The gamescope root-window properties and the window properties we use.
    pub Atoms: AtomsCookie {
        GAMESCOPE_PID,
        GAMESCOPE_FOCUSABLE_APPS,
        GAMESCOPE_FOCUSED_APP,
        GAMESCOPECTRL_BASELAYER_APPID,
        GAMESCOPECTRL_BASELAYER_WINDOW,
        STEAM_GAME,
        STEAM_OVERLAY,
        STEAM_BIGPICTURE,
        UTF8_STRING,
        _NET_WM_PID,
        _NET_WM_NAME,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_TOOLTIP,
        _NET_WM_WINDOW_TYPE_NOTIFICATION,
        _NET_WM_WINDOW_TYPE_MENU,
        _NET_WM_WINDOW_TYPE_DROPDOWN_MENU,
        _NET_WM_WINDOW_TYPE_POPUP_MENU,
        _NET_WM_WINDOW_TYPE_COMBO,
        _NET_WM_WINDOW_TYPE_DOCK,
        _NET_WM_WINDOW_TYPE_DESKTOP,
        _NET_WM_WINDOW_TYPE_DND,
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

/// What the watcher does besides reporting focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchOptions {
    /// Tag Steam's own windows with `STEAM_GAME = 769` (see [`crate::steam::windows`]).
    pub tag_steam_windows: bool,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            tag_steam_windows: true,
        }
    }
}

/// Watch gamescope's focus properties and Steam's windows on a background thread and report
/// changes. Retries the connection every `retry` until `events` is closed.
pub fn spawn_watcher(
    dpy: String,
    retry: Duration,
    opts: WatchOptions,
    events: mpsc::UnboundedSender<GamescopeEvent>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("gamescope-watch".into())
        .spawn(move || {
            let mut warned = false;
            while !events.is_closed() {
                match watch_once(&dpy, opts, &events) {
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

/// `Ok(None)` for X errors about one window (it went away while we looked); `Err` if the
/// connection broke.
fn window_reply<T>(r: std::result::Result<T, x11rb::errors::ReplyError>) -> Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(x11rb::errors::ReplyError::X11Error(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

impl Conn {
    /// Select property changes on a window (errors, e.g. for a window already gone, arrive
    /// as error events and are ignored).
    fn follow(&self, window: Window) -> Result<()> {
        self.conn.change_window_attributes(
            window,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        Ok(())
    }

    /// Everything [`WindowInfo`] needs about one top-level window; `None` if it is gone.
    fn read_window(&self, window: Window, xres: bool) -> Result<Option<WindowInfo>> {
        let c = &self.conn;
        let a = &self.atoms;
        let prop = |atom: u32, ty: u32| c.get_property(false, window, atom, ty, 0, 1024);
        let attrs = c.get_window_attributes(window)?;
        let geom = c.get_geometry(window)?;
        let class = prop(AtomEnum::WM_CLASS.into(), AtomEnum::STRING.into())?;
        let net_name = prop(a._NET_WM_NAME, a.UTF8_STRING)?;
        let name = prop(AtomEnum::WM_NAME.into(), AtomEnum::ANY.into())?;
        let pid = prop(a._NET_WM_PID, AtomEnum::CARDINAL.into())?;
        let game = prop(a.STEAM_GAME, AtomEnum::CARDINAL.into())?;
        let overlay = prop(a.STEAM_OVERLAY, AtomEnum::CARDINAL.into())?;
        let bigpicture = prop(a.STEAM_BIGPICTURE, AtomEnum::ANY.into())?;
        let wtype = prop(a._NET_WM_WINDOW_TYPE, AtomEnum::ATOM.into())?;
        let client = if xres {
            Some(c.res_query_client_ids(&[ClientIdSpec {
                client: window,
                mask: ClientIdMask::LOCAL_CLIENT_PID,
            }])?)
        } else {
            None
        };

        let Some(attrs) = window_reply(attrs.reply())? else {
            return Ok(None);
        };
        let Some(geom) = window_reply(geom.reply())? else {
            return Ok(None);
        };
        let first32 = |r: Option<GetPropertyReply>| -> Option<u32> {
            r.and_then(|r| r.value32().and_then(|mut v| v.next()))
        };
        let text = |r: Option<GetPropertyReply>| -> Option<String> {
            r.filter(|r| !r.value.is_empty())
                .map(|r| String::from_utf8_lossy(&r.value).into_owned())
        };
        let wm_class = window_reply(class.reply())?
            .map(|r| {
                r.value
                    .split(|&b| b == 0)
                    .filter(|s| !s.is_empty())
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .collect()
            })
            .unwrap_or_default();
        let title = text(window_reply(net_name.reply())?)
            .or(text(window_reply(name.reply())?))
            .unwrap_or_default();
        let mut pids = Vec::new();
        if let Some(client) = client
            && let Some(reply) = window_reply(client.reply())?
        {
            pids.extend(reply.ids.iter().filter_map(|id| id.value.first().copied()));
        }
        if let Some(p) = first32(window_reply(pid.reply())?) {
            pids.push(p);
        }
        pids.retain(|&p| p != 0);
        pids.dedup();
        let helper_types = [
            (a._NET_WM_WINDOW_TYPE_TOOLTIP, HelperType::Tooltip),
            (a._NET_WM_WINDOW_TYPE_NOTIFICATION, HelperType::Notification),
            (a._NET_WM_WINDOW_TYPE_MENU, HelperType::Menu),
            (a._NET_WM_WINDOW_TYPE_DROPDOWN_MENU, HelperType::Menu),
            (a._NET_WM_WINDOW_TYPE_POPUP_MENU, HelperType::Menu),
            (a._NET_WM_WINDOW_TYPE_COMBO, HelperType::Menu),
            (a._NET_WM_WINDOW_TYPE_DOCK, HelperType::Dock),
            (a._NET_WM_WINDOW_TYPE_DESKTOP, HelperType::Desktop),
            (a._NET_WM_WINDOW_TYPE_DND, HelperType::Dnd),
        ];
        let helper_type = window_reply(wtype.reply())?
            .and_then(|r| r.value32().map(Iterator::collect::<Vec<u32>>))
            .and_then(|types| {
                helper_types
                    .iter()
                    .find(|(atom, _)| types.contains(atom))
                    .map(|(_, t)| *t)
            });
        Ok(Some(WindowInfo {
            window,
            mapped: attrs.map_state == MapState::VIEWABLE,
            override_redirect: attrs.override_redirect,
            input_output: attrs.class == WindowClass::INPUT_OUTPUT,
            width: geom.width.into(),
            height: geom.height.into(),
            wm_class,
            title,
            owners: pids.into_iter().map(|p| (p, procs::comm(p))).collect(),
            steam_game: first32(window_reply(game.reply())?),
            steam_overlay: first32(window_reply(overlay.reply())?).is_some_and(|v| v != 0),
            steam_bigpicture: window_reply(bigpicture.reply())?.is_some_and(|r| r.format != 0),
            helper_type,
        }))
    }

    /// Read every top-level window, tag Steam's (if `tag`), and return Steam's windows (all of
    /// them) and the ones that want attention.
    fn scan_steam(&self, tag: bool, xres: bool) -> Result<(Vec<Window>, Vec<SteamWindow>)> {
        let tree = self.conn.query_tree(self.root)?.reply()?;
        let mut infos = Vec::new();
        for w in tree.children {
            if let Some(mut info) = self.read_window(w, xres)? {
                if tag && info.needs_tag() {
                    debug!(window = w, title = %info.title, "tagging Steam window STEAM_GAME=769");
                    self.conn.change_property32(
                        PropMode::REPLACE,
                        w,
                        self.atoms.STEAM_GAME,
                        AtomEnum::CARDINAL,
                        &[STEAM_CLIENT_GAMESCOPE_APP_ID],
                    )?;
                    info.steam_game = Some(STEAM_CLIENT_GAMESCOPE_APP_ID);
                }
                infos.push(info);
            }
        }
        self.conn.flush()?;
        let steam = infos
            .iter()
            .filter(|i| i.is_steam())
            .map(|i| i.window)
            .collect();
        Ok((steam, windows::attention_candidates(&infos)))
    }

    /// Whether a property change on a (non-root) window can change what we know about it.
    fn is_window_atom(&self, atom: u32) -> bool {
        let a = &self.atoms;
        atom == u32::from(AtomEnum::WM_CLASS)
            || atom == u32::from(AtomEnum::WM_NAME)
            || [
                a._NET_WM_NAME,
                a._NET_WM_PID,
                a.STEAM_GAME,
                a.STEAM_OVERLAY,
                a.STEAM_BIGPICTURE,
                a._NET_WM_WINDOW_TYPE,
            ]
            .contains(&atom)
    }
}

/// One connection's worth of watching. `Ok` means the receiver is gone.
fn watch_once(
    dpy: &str,
    opts: WatchOptions,
    events: &mpsc::UnboundedSender<GamescopeEvent>,
) -> Result<()> {
    let c = Conn::open(dpy)?;
    c.conn
        .change_window_attributes(
            c.root,
            &ChangeWindowAttributesAux::new()
                .event_mask(EventMask::PROPERTY_CHANGE | EventMask::SUBSTRUCTURE_NOTIFY),
        )?
        .check()?;
    let xres = c
        .conn
        .extension_information(x11rb::protocol::res::X11_EXTENSION_NAME)?
        .is_some();
    for w in c.conn.query_tree(c.root)?.reply()?.children {
        c.follow(w)?;
    }
    info!(display = %dpy, xres, "watching gamescope focus and Steam windows");
    let send = |e| events.send(e).is_ok();
    if !send(GamescopeEvent::Connected) || !send(GamescopeEvent::State(c.state()?)) {
        return Ok(());
    }
    let (mut steam, attention) = c.scan_steam(opts.tag_steam_windows, xres)?;
    if !send(GamescopeEvent::SteamWindows(attention.clone())) {
        return Ok(());
    }
    let mut last_attention = attention;
    let focus_atoms = [
        c.atoms.GAMESCOPE_FOCUSED_APP,
        c.atoms.GAMESCOPE_FOCUSABLE_APPS,
        c.atoms.GAMESCOPECTRL_BASELAYER_APPID,
    ];
    let mut last = None;
    let result = loop {
        let first = match c.conn.wait_for_event() {
            Ok(e) => e,
            Err(e) => break Err(e.into()),
        };
        // Handle a burst of events with one state read and one window scan.
        let mut batch = vec![first];
        loop {
            match c.conn.poll_for_event() {
                Ok(Some(e)) => batch.push(e),
                Ok(None) => break,
                Err(e) => return finish(events, Err(e.into())),
            }
        }
        let mut focus_changed = false;
        let mut rescan = false;
        for event in batch {
            match event {
                Event::PropertyNotify(ev) if ev.window == c.root => {
                    focus_changed |= focus_atoms.contains(&ev.atom);
                }
                Event::PropertyNotify(ev) => rescan |= c.is_window_atom(ev.atom),
                Event::CreateNotify(ev) if ev.parent == c.root => {
                    if let Err(e) = c.follow(ev.window) {
                        return finish(events, Err(e));
                    }
                    rescan = true;
                }
                Event::MapNotify(_)
                | Event::UnmapNotify(_)
                | Event::DestroyNotify(_)
                | Event::ReparentNotify(_) => rescan = true,
                Event::ConfigureNotify(ev) => rescan |= steam.contains(&ev.window),
                _ => {}
            }
        }
        if focus_changed {
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
        if rescan {
            let (now_steam, attention) = match c.scan_steam(opts.tag_steam_windows, xres) {
                Ok(r) => r,
                Err(e) => break Err(e),
            };
            steam = now_steam;
            if attention != last_attention {
                if !send(GamescopeEvent::SteamWindows(attention.clone())) {
                    return Ok(());
                }
                last_attention = attention;
            }
        }
    };
    finish(events, result)
}

fn finish(events: &mpsc::UnboundedSender<GamescopeEvent>, result: Result<()>) -> Result<()> {
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
        spawn_watcher(
            x.display.clone(),
            Duration::from_millis(50),
            WatchOptions::default(),
            tx,
        );
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
        assert_eq!(next(&mut rx), GamescopeEvent::SteamWindows(vec![]));

        gs.set_cardinals(gs.atoms.GAMESCOPE_FOCUSABLE_APPS, &[570])
            .unwrap();
        loop {
            match next(&mut rx) {
                GamescopeEvent::State(s) if s.focusable_apps == [570] => break,
                GamescopeEvent::State(_) | GamescopeEvent::SteamWindows(_) => {}
                other => panic!("unexpected {other:?}"),
            }
        }
    }
}
