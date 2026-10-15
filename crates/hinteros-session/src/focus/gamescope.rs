//! [`FocusControl`] for gamescope, through the X properties on its Xwayland root window.
//!
//! No auth is needed: gamescope's Xwayland accepts local connections from the same user.

use std::cell::OnceCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
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

use super::dummies;
use super::{
    FocusControl, GamescopeEvent, GamescopeState, baselayer_for, baselayer_for_steam_client,
    baselayer_for_steam_game,
};
use crate::osk;
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
        GAMESCOPE_EXTERNAL_OVERLAY,
        _NET_WM_WINDOW_OPACITY,
        HINTEROS_OSK_VISIBLE,
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

/// The last connection to a display that is not gamescope's yet, kept open until one is.
///
/// gamescope starts its Xwaylands with `-terminate`: an Xwayland exits when its last client
/// disconnects. Connecting before gamescope's own window manager has, finding no
/// `GAMESCOPE_PID` and disconnecting again would end that Xwayland, and gamescope would wait
/// for it forever (a black screen at boot). Holding the connection keeps it alive. One per
/// display: the apps' Xwayland has a watcher of its own, and gamescope sets `GAMESCOPE_PID` on
/// one Xwayland before its window manager connects to the next.
static NOT_YET: Mutex<BTreeMap<String, RustConnection>> = Mutex::new(BTreeMap::new());

/// One connection to gamescope's Xwayland.
struct Conn {
    conn: RustConnection,
    root: Window,
    atoms: Atoms,
}

impl Conn {
    /// Connect to `display` and check that it really is gamescope (it sets `GAMESCOPE_PID`).
    fn open(display: &str) -> Result<Self> {
        let display = &match display {
            "auto" => crate::config::resolve_display(display),
            _ => crate::config::try_resolve_display(display)
                .with_context(|| format!("X display {display} does not exist (yet)"))?,
        };
        let (conn, screen) =
            x11rb::connect(Some(display)).with_context(|| format!("connecting to X {display}"))?;
        let root = conn.setup().roots[screen].root;
        let atoms = Atoms::new(&conn)?.reply()?;
        let c = Self { conn, root, atoms };
        if c.cardinals(c.atoms.GAMESCOPE_PID)?.is_empty() {
            NOT_YET
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(display.clone(), c.conn);
            bail!("X display {display} is not gamescope's Xwayland (no GAMESCOPE_PID on the root)");
        }
        NOT_YET
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(display);
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

    fn with<T>(&mut self, f: impl Fn(&Conn) -> Result<T>) -> Result<T> {
        let had_conn = self.conn.is_some();
        let result = self.with_once(&f);
        if result.is_err() && had_conn {
            // The connection may have been to a gamescope that has since restarted (EPIPE,
            // connection closed): any error drops it, and the first try on a new one is made
            // right away.
            debug!(
                "gamescope connection failed, reconnecting: {:#}",
                result.as_ref().err().unwrap()
            );
            return self.with_once(&f);
        }
        result
    }

    fn with_once<T>(&mut self, f: &impl Fn(&Conn) -> Result<T>) -> Result<T> {
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
        self.set_baselayer(baselayer_for(target, self.launcher_app_id))
    }

    fn set_baselayer(&mut self, apps: Vec<u32>) -> Result<()> {
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

    fn focus_steam_game(&mut self, app_id: u32) -> Result<()> {
        self.set_baselayer(baselayer_for_steam_game(app_id, self.launcher_app_id))
    }

    fn focus_steam_client(&mut self, games: &[u32]) -> Result<()> {
        self.set_baselayer(baselayer_for_steam_client(games, self.launcher_app_id))
    }

    fn set_osk_visible(&mut self, visible: bool) -> Result<()> {
        debug!(visible, "{}", osk::VISIBLE_ATOM);
        self.with(|c| c.set_cardinals(c.atoms.HINTEROS_OSK_VISIBLE, &[u32::from(visible)]))
    }
}

/// What the watcher does besides reporting focus.
#[derive(Debug, Clone)]
pub struct WatchOptions {
    /// Tag Steam's own windows with `STEAM_GAME = 769` (see [`crate::steam::windows`]).
    pub tag_steam_windows: bool,
    /// The on-screen keyboard process the daemon started (0: none), to find its windows.
    pub osk_pid: Arc<AtomicU32>,
    /// Unmap invisible override-redirect windows that cover an app (see [`super::dummies`]).
    pub unmap_gl_dummies: bool,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            tag_steam_windows: true,
            osk_pid: Arc::default(),
            unmap_gl_dummies: true,
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
                match watch_once(&dpy, &opts, &events) {
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

/// Unmap GL dummies ([`dummies`]) on an app display ([`crate::displays`]), which the focus
/// watcher does not follow (focus and Steam's windows are on the first Xwayland). `dpy` says
/// which display (a setting such as `"auto+3"`), `None` while this session has no such display.
/// Retries the connection every `retry`, for the life of the daemon.
pub fn spawn_dummies_watcher(
    dpy: impl Fn() -> Option<String> + Send + 'static,
    retry: Duration,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("gamescope-apps-watch".into())
        .spawn(move || {
            let mut warned = false;
            loop {
                let Some(dpy) = dpy() else {
                    thread::sleep(retry);
                    continue;
                };
                match watch_dummies(&dpy) {
                    Ok(()) => {}
                    Err(e) if !warned => {
                        warn!(display = %dpy, "apps' Xwayland unavailable, retrying: {e:#}");
                        warned = true;
                    }
                    Err(e) => debug!(display = %dpy, "apps' Xwayland still unavailable: {e:#}"),
                }
                thread::sleep(retry);
            }
        })
        .expect("spawning the apps' Xwayland watcher thread")
}

/// One connection's worth of [`spawn_dummies_watcher`]; returns when the connection breaks.
fn watch_dummies(dpy: &str) -> Result<()> {
    let c = Conn::open(dpy)?;
    c.conn
        .change_window_attributes(
            c.root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
        )?
        .check()?;
    let xres = c
        .conn
        .extension_information(x11rb::protocol::res::X11_EXTENSION_NAME)?
        .is_some();
    for w in c.conn.query_tree(c.root)?.reply()?.children {
        c.follow(w)?;
    }
    info!(display = %dpy, "watching the apps' Xwayland for GL dummies");
    let mut seen = HashSet::new();
    let mut blank = c.scan(false, xres, 0, Some(&mut seen))?.blank;
    loop {
        let mut batch = vec![c.conn.wait_for_event()?];
        while let Some(e) = c.conn.poll_for_event()? {
            batch.push(e);
        }
        let mut rescan = false;
        for event in batch {
            match event {
                Event::PropertyNotify(ev) => {
                    rescan |= ev.window != c.root && c.is_window_atom(ev.atom);
                }
                Event::CreateNotify(ev) if ev.parent == c.root => {
                    c.follow(ev.window)?;
                    rescan = true;
                }
                Event::MapNotify(_)
                | Event::UnmapNotify(_)
                | Event::DestroyNotify(_)
                | Event::ReparentNotify(_) => rescan = true,
                Event::ConfigureNotify(ev) => rescan |= blank.contains(&ev.window),
                _ => {}
            }
        }
        if rescan {
            blank = c.scan(false, xres, 0, Some(&mut seen))?.blank;
        }
    }
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
        let opacity = prop(a._NET_WM_WINDOW_OPACITY, AtomEnum::ANY.into())?;
        let external = prop(a.GAMESCOPE_EXTERNAL_OVERLAY, AtomEnum::ANY.into())?;
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
        let wtype = window_reply(wtype.reply())?;
        let has_window_type = wtype.as_ref().is_some_and(|r| r.type_ != 0);
        let set = |r: Option<GetPropertyReply>| r.is_some_and(|r| r.type_ != 0);
        let content_hints =
            set(window_reply(opacity.reply())?) || set(window_reply(external.reply())?);
        let helper_type = wtype
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
            has_window_type,
            content_hints,
        }))
    }

    /// Read every top-level window, tag Steam's (if `tag`), and return Steam's windows (all of
    /// them), the ones that want attention, and the on-screen keyboard's.
    fn scan(
        &self,
        tag: bool,
        xres: bool,
        osk_pid: u32,
        dummies: Option<&mut HashSet<Window>>,
    ) -> Result<Scan> {
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
        if let Some(seen) = dummies {
            self.unmap_dummies(&infos, osk_pid, seen)?;
        }
        self.conn.flush()?;
        let steam = infos
            .iter()
            .filter(|i| i.is_steam())
            .map(|i| i.window)
            .collect();
        let osk = infos
            .iter()
            .filter(|i| i.input_output && osk::is_osk_window(i, osk_pid))
            .map(|i| i.window)
            .collect();
        let blank = infos
            .iter()
            .filter(|i| dummies::is_blank_override_redirect(i))
            .map(|i| i.window)
            .collect();
        Ok(Scan {
            steam,
            attention: windows::attention_candidates(&infos),
            osk,
            blank,
        })
    }

    /// Unmap the GL dummies among `infos` (see [`dummies`]). `seen` holds those already logged.
    fn unmap_dummies(
        &self,
        infos: &[WindowInfo],
        osk_pid: u32,
        seen: &mut HashSet<Window>,
    ) -> Result<()> {
        if !infos.iter().any(dummies::is_blank_override_redirect) {
            seen.clear();
            return Ok(());
        }
        let root = self.conn.get_geometry(self.root)?.reply()?;
        let table = OnceCell::new();
        let found = dummies::find(
            infos,
            (root.width.into(), root.height.into()),
            osk_pid,
            |w| {
                w.owners.iter().find_map(|(pid, _)| {
                    procs::managed_app_id_of(table.get_or_init(procs::list), *pid)
                })
            },
        );
        seen.retain(|w| infos.iter().any(|i| i.window == *w));
        for d in found {
            if seen.insert(d.window) {
                info!(
                    window = d.window,
                    "unmapped an empty override-redirect window of {} (likely a GTK GL dummy)",
                    d.owner
                );
            } else {
                debug!(
                    window = d.window,
                    app_id = d.app_id,
                    "unmapped a GL dummy again"
                );
            }
            self.conn.unmap_window(d.window)?;
        }
        self.conn.flush()?;
        Ok(())
    }

    /// The on-screen keyboard visibility the shell asked for.
    fn osk_wanted(&self) -> Result<bool> {
        Ok(self
            .cardinals(self.atoms.HINTEROS_OSK_VISIBLE)?
            .first()
            .is_some_and(|&v| v != 0))
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

/// What one scan of the top-level windows found.
struct Scan {
    steam: Vec<Window>,
    attention: Vec<SteamWindow>,
    osk: Vec<Window>,
    /// Mapped override-redirect windows with nothing on them: if one is resized to the whole
    /// screen, it may be a dummy.
    blank: Vec<Window>,
}

/// Keeps the on-screen keyboard's windows external overlays with the wanted opacity.
#[derive(Default)]
struct OskWindows {
    visible: bool,
    /// What each known OSK window was set to.
    applied: HashMap<Window, bool>,
}

impl OskWindows {
    fn apply(&mut self, c: &Conn, windows: &[Window]) -> Result<()> {
        self.applied.retain(|w, _| windows.contains(w));
        let overlay = c.atoms.GAMESCOPE_EXTERNAL_OVERLAY;
        let opacity = c.atoms._NET_WM_WINDOW_OPACITY;
        let value = if self.visible {
            osk::OPAQUE
        } else {
            osk::TRANSPARENT
        };
        for &w in windows {
            if self.applied.get(&w) == Some(&self.visible) {
                continue;
            }
            debug!(
                window = w,
                visible = self.visible,
                "on-screen keyboard window"
            );
            // Errors for a window that just went away arrive as events and are ignored.
            c.conn
                .change_property32(PropMode::REPLACE, w, overlay, AtomEnum::CARDINAL, &[1])?;
            c.conn.change_property32(
                PropMode::REPLACE,
                w,
                opacity,
                AtomEnum::CARDINAL,
                &[value],
            )?;
            self.applied.insert(w, self.visible);
        }
        c.conn.flush()?;
        Ok(())
    }
}

/// One connection's worth of watching. `Ok` means the receiver is gone. Any error (a broken or
/// closed connection, gamescope restarting) ends it; the caller reconnects.
fn watch_once(
    dpy: &str,
    opts: &WatchOptions,
    events: &mpsc::UnboundedSender<GamescopeEvent>,
) -> Result<()> {
    let mut connected = false;
    let result = watch_session(dpy, opts, events, &mut connected);
    if result.is_err() && connected {
        let _ = events.send(GamescopeEvent::Disconnected);
    }
    result
}

fn watch_session(
    dpy: &str,
    opts: &WatchOptions,
    events: &mpsc::UnboundedSender<GamescopeEvent>,
    connected: &mut bool,
) -> Result<()> {
    let c = Conn::open(dpy)?;
    let gamescope_pid = c.cardinals(c.atoms.GAMESCOPE_PID)?;
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
    *connected = true;
    if !send(GamescopeEvent::Connected) || !send(GamescopeEvent::State(c.state()?)) {
        return Ok(());
    }
    let osk_pid = || opts.osk_pid.load(Ordering::SeqCst);
    let mut gl_dummies = HashSet::new();
    let scan = c.scan(
        opts.tag_steam_windows,
        xres,
        osk_pid(),
        opts.unmap_gl_dummies.then_some(&mut gl_dummies),
    )?;
    let mut blank = scan.blank;
    let mut osk = OskWindows {
        visible: c.osk_wanted()?,
        ..OskWindows::default()
    };
    osk.apply(&c, &scan.osk)?;
    let mut osk_windows = scan.osk;
    let mut steam = scan.steam;
    if !send(GamescopeEvent::SteamWindows(scan.attention.clone())) {
        return Ok(());
    }
    let mut last_attention = scan.attention;
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
        let mut pid_changed = false;
        let mut rescan = false;
        let mut osk_changed = false;
        for event in batch {
            match event {
                Event::PropertyNotify(ev) if ev.window == c.root => {
                    focus_changed |= focus_atoms.contains(&ev.atom);
                    pid_changed |= ev.atom == c.atoms.GAMESCOPE_PID;
                    osk_changed |= ev.atom == c.atoms.HINTEROS_OSK_VISIBLE;
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
                Event::ConfigureNotify(ev) => {
                    rescan |= steam.contains(&ev.window)
                        || (opts.unmap_gl_dummies && blank.contains(&ev.window));
                }
                _ => {}
            }
        }
        if pid_changed {
            match c.cardinals(c.atoms.GAMESCOPE_PID) {
                Ok(now) if now == gamescope_pid => {}
                Ok(_) => {
                    break Err(anyhow::anyhow!(
                        "GAMESCOPE_PID changed: gamescope restarted"
                    ));
                }
                Err(e) => break Err(e),
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
        if osk_changed {
            match c.osk_wanted() {
                Ok(v) => osk.visible = v,
                Err(e) => break Err(e),
            }
        }
        if rescan {
            let scan = match c.scan(
                opts.tag_steam_windows,
                xres,
                osk_pid(),
                opts.unmap_gl_dummies.then_some(&mut gl_dummies),
            ) {
                Ok(r) => r,
                Err(e) => break Err(e),
            };
            steam = scan.steam;
            blank = scan.blank;
            osk_windows = scan.osk;
            if scan.attention != last_attention {
                if !send(GamescopeEvent::SteamWindows(scan.attention.clone())) {
                    return Ok(());
                }
                last_attention = scan.attention;
            }
        }
        if (rescan || osk_changed)
            && let Err(e) = osk.apply(&c, &osk_windows)
        {
            break Err(e);
        }
    };
    finish(events, result)
}

fn finish(_events: &mpsc::UnboundedSender<GamescopeEvent>, result: Result<()>) -> Result<()> {
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
            Self::start_on(None)
        }

        /// Start on a given display (`:7`), or any free one.
        fn start_on(display: Option<&str>) -> Option<Self> {
            let mut cmd = Command::new("Xvfb");
            if let Some(d) = display {
                cmd.arg(d);
            }
            let mut child = match cmd
                .args([
                    "-displayfd",
                    "1",
                    // Without it the server resets when its last client disconnects, and drops connections
                    // made meanwhile (ConnectionReset / UnexpectedEof in whichever test connects next).
                    "-noreset",
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
            assert!(!line.is_empty(), "Xvfb exited before it was ready");
            Some(Self {
                child,
                display: format!(":{}", line.trim()),
            })
        }

        /// Kill the server and start another on the same display (gamescope restarting).
        fn restart(&mut self) {
            let _ = rustix::process::kill_process(
                rustix::process::Pid::from_raw(self.child.id() as i32).unwrap(),
                rustix::process::Signal::TERM,
            );
            let _ = self.child.wait();
            let mut again = Self::start_on(Some(&self.display)).expect("Xvfb started before");
            assert_eq!(again.display, self.display);
            // `again` now owns the dead server and kills nothing when dropped.
            std::mem::swap(&mut self.child, &mut again.child);
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

    #[test]
    fn watcher_and_focus_recover_after_the_x_server_restarts() {
        let Some(mut x) = Xvfb::start() else { return };
        {
            let gs = x.gamescope();
            gs.set_cardinals(gs.atoms.GAMESCOPE_PID, &[1]).unwrap();
        }
        let mut focus = GamescopeFocus::new(&x.display, 4000);
        focus.focus_launcher().unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        spawn_watcher(
            x.display.clone(),
            Duration::from_millis(50),
            WatchOptions::default(),
            tx,
        );
        let wait_for = |rx: &mut mpsc::UnboundedReceiver<GamescopeEvent>, want: GamescopeEvent| {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                match rx.try_recv() {
                    Ok(e) if e == want => return,
                    Ok(_) => {}
                    Err(_) => {
                        assert!(std::time::Instant::now() < deadline, "no {want:?}");
                        thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        };
        wait_for(&mut rx, GamescopeEvent::Connected);

        x.restart();
        wait_for(&mut rx, GamescopeEvent::Disconnected);
        // Not gamescope yet (no GAMESCOPE_PID): the watcher keeps trying.
        let gs = x.gamescope();
        gs.set_cardinals(gs.atoms.GAMESCOPE_PID, &[2]).unwrap();
        wait_for(&mut rx, GamescopeEvent::Connected);

        // The controller held a connection to the old server: its first write fails, is
        // retried on a new connection, and lands.
        focus.focus_app(570).unwrap();
        assert_eq!(
            gs.cardinals(gs.atoms.GAMESCOPECTRL_BASELAYER_APPID)
                .unwrap(),
            vec![570, 4000]
        );
    }

    #[test]
    fn watcher_reconnects_when_gamescope_pid_changes() {
        let Some(x) = Xvfb::start() else { return };
        let gs = x.gamescope();
        gs.set_cardinals(gs.atoms.GAMESCOPE_PID, &[1]).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        spawn_watcher(
            x.display.clone(),
            Duration::from_millis(50),
            WatchOptions::default(),
            tx,
        );
        let mut seen = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut changed = false;
        while std::time::Instant::now() < deadline {
            while let Ok(e) = rx.try_recv() {
                seen.push(e);
            }
            if !changed && seen.contains(&GamescopeEvent::Connected) {
                gs.set_cardinals(gs.atoms.GAMESCOPE_PID, &[2]).unwrap();
                changed = true;
            }
            let connects = seen
                .iter()
                .filter(|e| **e == GamescopeEvent::Connected)
                .count();
            if connects == 2 {
                assert!(seen.contains(&GamescopeEvent::Disconnected));
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("no reconnect: {seen:?}");
    }
}
