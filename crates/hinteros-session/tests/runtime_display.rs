//! Display modes against a fake gamescope: an Xvfb whose root window takes the
//! `GAMESCOPE_DISPLAY_MODE_NUDGE` property, and a Wayland display (wayland-server) that serves
//! `wl_output` and `gamescope_control` like gamescope 3.16.31 does. The fake does what
//! gamescope does on a nudge: it looks its display up in `GAMESCOPE_MODE_SAVE_FILE` and
//! switches (or, to play gamescope with `-W/-H/-r`, ignores it). Skipped when `Xvfb` is not on
//! PATH, unless HINTEROS_REQUIRE_TEST_TOOLS is set.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use wayland_server::protocol::wl_output::{self, WlOutput};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, ListeningSocket, New,
};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, PropMode, Property,
};
use x11rb::wrapper::ConnectionExt as _;

mod common;

use common::*;

#[allow(missing_docs, clippy::all, unused_imports)]
mod control_protocol {
    use wayland_server;
    use wayland_server::protocol::*;

    pub mod __interfaces {
        use wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocol/gamescope-control.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("protocol/gamescope-control.xml");
}

use control_protocol::gamescope_control::{self as ctl, GamescopeControl};

const MODES: &str = "3840x2160@60\n1920x1080@60\n1920x1080@120\n1280x720@60\n";
const DISPLAY: &str = "Sony TV";

#[derive(Clone, Copy, PartialEq, Debug)]
struct M(u32, u32, u32);

struct Shared {
    current: Mutex<M>,
    nudges: AtomicU32,
    /// Whether a nudge makes it re-read the save file (false: started with -W/-H/-r).
    honours: AtomicBool,
    internal: AtomicBool,
    stop: AtomicBool,
}

struct Compositor {
    shared: Arc<Shared>,
}

struct NoData;
impl ClientData for NoData {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
}

impl GlobalDispatch<WlOutput, ()> for Compositor {
    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WlOutput>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let out = init.init(resource, ());
        let M(w, h, r) = *state.shared.current.lock().unwrap();
        out.mode(
            wl_output::Mode::Current | wl_output::Mode::Preferred,
            w as i32,
            h as i32,
            (r * 1000) as i32,
        );
        out.done();
    }
}

impl Dispatch<WlOutput, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlOutput,
        _: wl_output::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<GamescopeControl, ()> for Compositor {
    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<GamescopeControl>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let control = init.init(resource, ());
        let flags = if state.shared.internal.load(Ordering::SeqCst) {
            ctl::DisplayFlag::InternalDisplay
        } else {
            ctl::DisplayFlag::empty()
        };
        control.active_display_info("HDMI-A-1".into(), "Sony".into(), "TV".into(), flags, vec![]);
    }
}

impl Dispatch<GamescopeControl, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &GamescopeControl,
        _: ctl::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

struct FakeGamescope {
    xvfb: Child,
    display: String,
    wayland: PathBuf,
    shared: Arc<Shared>,
    threads: Vec<thread::JoinHandle<()>>,
    dir: tempfile::TempDir,
}

impl FakeGamescope {
    fn start(honours: bool, internal: bool) -> Option<Self> {
        let mut xvfb = match Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
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
        BufReader::new(xvfb.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let display = format!(":{}", line.trim());
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("edid.modes"), MODES).unwrap();
        let shared = Arc::new(Shared {
            current: Mutex::new(M(3840, 2160, 60)),
            nudges: AtomicU32::new(0),
            honours: AtomicBool::new(honours),
            internal: AtomicBool::new(internal),
            stop: AtomicBool::new(false),
        });
        let wayland = dir.path().join("gamescope-0");
        let mut threads = Vec::new();

        // The compositor.
        let (s, path) = (shared.clone(), wayland.clone());
        threads.push(thread::spawn(move || {
            let mut display = Display::<Compositor>::new().unwrap();
            let dh = display.handle();
            dh.create_global::<Compositor, WlOutput, ()>(4, ());
            dh.create_global::<Compositor, GamescopeControl, ()>(7, ());
            let socket = ListeningSocket::bind_absolute(path).unwrap();
            let mut state = Compositor { shared: s.clone() };
            while !s.stop.load(Ordering::SeqCst) {
                while let Ok(Some(stream)) = socket.accept() {
                    display
                        .handle()
                        .insert_client(stream, Arc::new(NoData))
                        .unwrap();
                }
                display.dispatch_clients(&mut state).unwrap();
                display.flush_clients().unwrap();
                thread::sleep(Duration::from_millis(5));
            }
        }));

        // gamescope's Xwayland handling of the nudge.
        let (conn, screen) = x11rb::connect(Some(&display)).unwrap();
        let root = conn.setup().roots[screen].root;
        let atom = |name: &str| {
            conn.intern_atom(false, name.as_bytes())
                .unwrap()
                .reply()
                .unwrap()
                .atom
        };
        let (nudge, pid, focused, focusable) = (
            atom("GAMESCOPE_DISPLAY_MODE_NUDGE"),
            atom("GAMESCOPE_PID"),
            atom("GAMESCOPE_FOCUSED_APP"),
            atom("GAMESCOPE_FOCUSABLE_APPS"),
        );
        conn.change_property32(
            PropMode::REPLACE,
            root,
            pid,
            AtomEnum::CARDINAL,
            &[std::process::id()],
        )
        .unwrap()
        .check()
        .unwrap();
        conn.change_property32(PropMode::REPLACE, root, focused, AtomEnum::CARDINAL, &[])
            .unwrap()
            .check()
            .unwrap();
        conn.change_property32(PropMode::REPLACE, root, focusable, AtomEnum::CARDINAL, &[])
            .unwrap()
            .check()
            .unwrap();
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .unwrap()
        .check()
        .unwrap();
        let (s, save) = (shared.clone(), dir.path().join("modes.save"));
        threads.push(thread::spawn(move || {
            while !s.stop.load(Ordering::SeqCst) {
                match conn.poll_for_event().unwrap() {
                    Some(Event::PropertyNotify(ev))
                        if ev.atom == nudge && ev.state == Property::NEW_VALUE =>
                    {
                        conn.delete_property(root, nudge).unwrap();
                        conn.flush().unwrap();
                        s.nudges.fetch_add(1, Ordering::SeqCst);
                        if !s.honours.load(Ordering::SeqCst) {
                            continue;
                        }
                        // setup_best_connector: the saved mode of this (external) display.
                        let text = std::fs::read_to_string(&save).unwrap_or_default();
                        let wanted = text
                            .lines()
                            .find_map(|l| l.strip_prefix(&format!("{DISPLAY}:")));
                        if let Some(mode) = wanted.and_then(parse) {
                            *s.current.lock().unwrap() = mode;
                        }
                    }
                    Some(_) => {}
                    None => thread::sleep(Duration::from_millis(10)),
                }
            }
        }));
        Some(Self {
            xvfb,
            display,
            wayland,
            shared,
            threads,
            dir,
        })
    }

    fn save_file(&self) -> PathBuf {
        self.dir.path().join("modes.save")
    }

    fn current(&self) -> M {
        *self.shared.current.lock().unwrap()
    }

    fn nudges(&self) -> u32 {
        self.shared.nudges.load(Ordering::SeqCst)
    }

    /// The daemon, configured the way the system repo configures the session.
    fn daemon(&self, apply_timeout_ms: u64, with_save_file: bool) -> Daemon {
        let display = self.display.clone();
        let wayland = self.wayland.clone();
        let dir = self.dir.path().to_owned();
        Daemon::start_env(
            |d| {
                let config = d.join("session.toml");
                std::fs::write(
                    &config,
                    format!(
                        "[gamescope]\nwayland_display = \"{}\"\n[display]\napply_timeout_ms = {apply_timeout_ms}\n",
                        wayland.display()
                    ),
                )
                .unwrap();
                vec![
                    "--gamescope".into(),
                    "--gamescope-display".into(),
                    display.into(),
                    "--config".into(),
                    config.into(),
                ]
            },
            |_| {
                let mut env = vec![(
                    "GAMESCOPE_PATCHED_EDID_FILE".to_owned(),
                    dir.join("edid").into_os_string(),
                )];
                if with_save_file {
                    env.push((
                        "GAMESCOPE_MODE_SAVE_FILE".to_owned(),
                        dir.join("modes.save").into_os_string(),
                    ));
                }
                env
            },
        )
    }
}

fn parse(s: &str) -> Option<M> {
    let (size, r) = s.split_once('@')?;
    let (w, h) = size.split_once('x')?;
    Some(M(
        w.parse().ok()?,
        h.parse().ok()?,
        r.split_whitespace().next()?.parse().ok()?,
    ))
}

impl Drop for FakeGamescope {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        let _ = self.xvfb.kill();
        let _ = self.xvfb.wait();
    }
}

fn mode(w: u32, h: u32, r: u32) -> Value {
    json!({"width": w, "height": h, "refresh": r})
}

fn current_of(modes: &Value) -> Vec<(u64, u64, u64)> {
    modes["modes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["current"] == true)
        .map(|m| {
            (
                m["width"].as_u64().unwrap(),
                m["height"].as_u64().unwrap(),
                m["refresh"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn lists_modes_and_the_current_one() {
    let Some(gs) = FakeGamescope::start(true, false) else {
        return;
    };
    let d = gs.daemon(3000, true);
    let mut c = d.client().await;
    let modes = call(&mut c, "display.modes", json!({})).await.unwrap();
    assert_eq!(modes["display"], "Sony TV");
    assert_eq!(modes["can_set"], true);
    assert_eq!(modes["modes"].as_array().unwrap().len(), 4);
    assert_eq!(current_of(&modes), [(3840, 2160, 60)]);
    // The first line of gamescope's list is the display's choice.
    assert_eq!(modes["modes"][0]["preferred"], true);
    assert_eq!(modes["modes"][1]["preferred"], false);
    assert_eq!(
        modes["modes"][2],
        json!({"width": 1920, "height": 1080, "refresh": 120, "current": false, "preferred": false})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn set_mode_writes_the_save_file_nudges_and_waits_for_gamescope() {
    let Some(gs) = FakeGamescope::start(true, false) else {
        return;
    };
    // gamescope already knows other displays and a broadcast mode for this one.
    std::fs::write(
        gs.save_file(),
        "Other 24:1280x720@50 2\nSony TV:3840x2160@60 1\n",
    )
    .unwrap();
    let d = gs.daemon(3000, true);
    let mut c = d.client().await;

    let r = call(&mut c, "display.set_mode", mode(1920, 1080, 120))
        .await
        .unwrap();
    assert_eq!(
        r,
        json!({"previous": mode(3840, 2160, 60), "current": mode(1920, 1080, 120)})
    );
    assert_eq!(gs.current(), M(1920, 1080, 120));
    assert_eq!(gs.nudges(), 1);
    // The save file is what makes it stick: this display's line, the others and the extra field kept.
    assert_eq!(
        std::fs::read_to_string(gs.save_file()).unwrap(),
        "Other 24:1280x720@50 2\nSony TV:1920x1080@120 1\n"
    );
    let modes = call(&mut c, "display.modes", json!({})).await.unwrap();
    assert_eq!(current_of(&modes), [(1920, 1080, 120)]);

    // The mode in use again: nothing to do, nothing to revert.
    let before = gs.nudges();
    let r = call(&mut c, "display.set_mode", mode(1920, 1080, 120))
        .await
        .unwrap();
    assert_eq!(r["previous"], r["current"]);
    assert_eq!(gs.nudges(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_keep_or_revert_flow() {
    let Some(gs) = FakeGamescope::start(true, false) else {
        return;
    };
    let d = gs.daemon(3000, true);
    let mut c = d.client().await;

    // Nothing to revert yet.
    let e = call(&mut c, "display.revert", json!({})).await.unwrap_err();
    assert_eq!(e["code"], 4002);
    assert_eq!(e["data"]["reason"], "nothing_to_revert");

    // Change, then revert (the 15 seconds ran out): the old mode, in gamescope and in the file.
    call(&mut c, "display.set_mode", mode(1280, 720, 60))
        .await
        .unwrap();
    assert_eq!(gs.current(), M(1280, 720, 60));
    let r = call(&mut c, "display.revert", json!({})).await.unwrap();
    assert_eq!(r, json!({"current": mode(3840, 2160, 60)}));
    assert_eq!(gs.current(), M(3840, 2160, 60));
    assert!(
        std::fs::read_to_string(gs.save_file())
            .unwrap()
            .contains("Sony TV:3840x2160@60")
    );
    // Reverting is a one-shot.
    let e = call(&mut c, "display.revert", json!({})).await.unwrap_err();
    assert_eq!(e["data"]["reason"], "nothing_to_revert");

    // Change twice before deciding: revert goes back to the mode before both.
    call(&mut c, "display.set_mode", mode(1920, 1080, 60))
        .await
        .unwrap();
    let r = call(&mut c, "display.set_mode", mode(1280, 720, 60))
        .await
        .unwrap();
    assert_eq!(r["previous"], mode(3840, 2160, 60));
    let r = call(&mut c, "display.revert", json!({})).await.unwrap();
    assert_eq!(r["current"], mode(3840, 2160, 60));
    assert_eq!(gs.current(), M(3840, 2160, 60));

    // Change and keep: nothing to revert after.
    call(&mut c, "display.set_mode", mode(1920, 1080, 60))
        .await
        .unwrap();
    assert_eq!(
        call(&mut c, "display.keep", json!({})).await.unwrap(),
        json!({})
    );
    let e = call(&mut c, "display.revert", json!({})).await.unwrap_err();
    assert_eq!(e["data"]["reason"], "nothing_to_revert");
    assert_eq!(gs.current(), M(1920, 1080, 60));
    // Keeping with nothing pending is harmless.
    call(&mut c, "display.keep", json!({})).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_modes_are_refused_before_anything_is_touched() {
    let Some(gs) = FakeGamescope::start(true, false) else {
        return;
    };
    let d = gs.daemon(3000, true);
    let mut c = d.client().await;
    let e = call(&mut c, "display.set_mode", mode(1000, 700, 60))
        .await
        .unwrap_err();
    assert_eq!(e["code"], 3010);
    // A refresh rate the display lacks at that size.
    let e = call(&mut c, "display.set_mode", mode(3840, 2160, 120))
        .await
        .unwrap_err();
    assert_eq!(e["code"], 3010);
    assert_eq!(gs.nudges(), 0);
    assert!(!gs.save_file().exists());
    // Params are closed and sane.
    let e = call(
        &mut c,
        "display.set_mode",
        json!({"width": 1920, "height": 1080}),
    )
    .await
    .unwrap_err();
    assert_eq!(e["code"], -32602);
    let e = call(
        &mut c,
        "display.set_mode",
        json!({"width": 0, "height": 1080, "refresh": 60}),
    )
    .await;
    assert!(e.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn gamescope_that_ignores_the_nudge_gets_its_file_back() {
    // Started with -W/-H/-r: those win over the save file.
    let Some(gs) = FakeGamescope::start(false, false) else {
        return;
    };
    std::fs::write(gs.save_file(), "Sony TV:3840x2160@60\nOther:1x1@1\n").unwrap();
    let d = gs.daemon(700, true);
    let mut c = d.client().await;
    let e = call(&mut c, "display.set_mode", mode(1280, 720, 60))
        .await
        .unwrap_err();
    assert_eq!(e["code"], 4002, "{e}");
    assert_eq!(e["data"]["reason"], "not_applied");
    assert!(gs.nudges() >= 1);
    assert_eq!(gs.current(), M(3840, 2160, 60));
    // The file is as it was: a restart must not surprise anyone.
    assert_eq!(
        std::fs::read_to_string(gs.save_file()).unwrap(),
        "Sony TV:3840x2160@60\nOther:1x1@1\n"
    );
    // And nothing is left to revert.
    let e = call(&mut c, "display.revert", json!({})).await.unwrap_err();
    assert_eq!(e["data"]["reason"], "nothing_to_revert");
}

#[tokio::test(flavor = "multi_thread")]
async fn internal_panels_and_missing_configuration() {
    // An internal display's mode is not taken from the save file.
    let Some(gs) = FakeGamescope::start(true, true) else {
        return;
    };
    let d = gs.daemon(700, true);
    let mut c = d.client().await;
    let modes = call(&mut c, "display.modes", json!({})).await.unwrap();
    assert_eq!(modes["can_set"], false);
    let e = call(&mut c, "display.set_mode", mode(1920, 1080, 60))
        .await
        .unwrap_err();
    assert_eq!(e["code"], 4001);
    assert_eq!(e["data"]["service"], "display");
    assert_eq!(gs.nudges(), 0);
    drop(d);

    // No save file configured: modes can be listed but not changed.
    let Some(gs) = FakeGamescope::start(true, false) else {
        return;
    };
    let d = gs.daemon(700, false);
    let mut c = d.client().await;
    let modes = call(&mut c, "display.modes", json!({})).await.unwrap();
    assert_eq!(modes["can_set"], false);
    assert_eq!(current_of(&modes), [(3840, 2160, 60)]);
    let e = call(&mut c, "display.set_mode", mode(1920, 1080, 60))
        .await
        .unwrap_err();
    assert_eq!(e["code"], 4001);
    assert!(
        e["data"]["reason"]
            .as_str()
            .unwrap()
            .contains("GAMESCOPE_MODE_SAVE_FILE"),
        "{e}"
    );
    assert_eq!(gs.nudges(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_gamescope() {
    // Gamescope integration is off (the default for tests, and a desktop preview).
    let d = Daemon::start();
    let mut c = d.client().await;
    for (method, params) in [
        ("display.modes", json!({})),
        ("display.set_mode", mode(1920, 1080, 60)),
    ] {
        let e = call(&mut c, method, params).await.unwrap_err();
        assert_eq!(e["code"], 4001, "{method}: {e}");
        assert_eq!(e["data"]["service"], "display");
    }
    // keep is always fine; revert has nothing to do.
    call(&mut c, "display.keep", json!({})).await.unwrap();
    let e = call(&mut c, "display.revert", json!({})).await.unwrap_err();
    assert_eq!(e["code"], 4002);
    let _ = Path::new("");
}

#[tokio::test(flavor = "multi_thread")]
async fn gamescope_without_a_wayland_display() {
    // Xwayland is there but the Wayland socket is not: modes cannot be known.
    let Some(gs) = FakeGamescope::start(true, false) else {
        return;
    };
    let d = Daemon::start_env(
        |dir| {
            let config = dir.join("session.toml");
            std::fs::write(
                &config,
                format!(
                    "[gamescope]\nwayland_display = \"{}/nothing\"\n",
                    dir.display()
                ),
            )
            .unwrap();
            vec![
                "--gamescope".into(),
                "--gamescope-display".into(),
                gs.display.clone().into(),
                "--config".into(),
                config.into(),
            ]
        },
        |_| Vec::new(),
    );
    let mut c = d.client().await;
    let e = call(&mut c, "display.modes", json!({})).await.unwrap_err();
    assert_eq!(e["code"], 4001);
    assert_eq!(e["data"]["service"], "display");
}
