//! The daemon against a fake gamescope: an Xvfb whose root window carries the properties
//! gamescope publishes. The test plays gamescope (it updates GAMESCOPE_FOCUSED_APP and
//! GAMESCOPE_FOCUSABLE_APPS) and checks what the daemon writes to
//! GAMESCOPECTRL_BASELAYER_APPID and reports over the socket. Skipped when `Xvfb` is not on
//! PATH, unless HINTEROS_REQUIRE_TEST_TOOLS is set.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::json;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateWindowAux, PropMode, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

mod common;

use common::*;

const LAUNCHER: u32 = 4_100_000_000;

/// The daemon finds Steam's games by their reapers in the host's `/proc`, and while Steam is in
/// front every live Steam reaper counts as a game Steam started itself (its window goes in
/// front of Steam's). Tests run in parallel in one process tree, so one test's fake Steam game
/// (`reaper SteamLaunch AppId=…`) would be another test's "Steam game". The tests that start
/// such reapers, and those that put Steam in front, take this lock.
static STEAM_REAPERS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct FakeGamescope {
    xvfb: Child,
    display: String,
    conn: RustConnection,
    root: Window,
}

impl FakeGamescope {
    fn start() -> Option<Self> {
        let mut xvfb = match Command::new("Xvfb")
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
        BufReader::new(xvfb.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(!line.is_empty(), "Xvfb exited before it was ready");
        let display = format!(":{}", line.trim());
        let (conn, screen) = x11rb::connect(Some(&display)).unwrap();
        let root = conn.setup().roots[screen].root;
        let fake = Self {
            xvfb,
            display,
            conn,
            root,
        };
        fake.set("GAMESCOPE_PID", &[std::process::id()]);
        fake.set("GAMESCOPE_FOCUSED_APP", &[]);
        fake.set("GAMESCOPE_FOCUSABLE_APPS", &[]);
        Some(fake)
    }

    fn atom(&self, name: &str) -> u32 {
        self.conn
            .intern_atom(false, name.as_bytes())
            .unwrap()
            .reply()
            .unwrap()
            .atom
    }

    fn set(&self, name: &str, values: &[u32]) {
        self.conn
            .change_property32(
                PropMode::REPLACE,
                self.root,
                self.atom(name),
                AtomEnum::CARDINAL,
                values,
            )
            .unwrap()
            .check()
            .unwrap();
    }

    fn get(&self, name: &str) -> Vec<u32> {
        let reply = self
            .conn
            .get_property(false, self.root, self.atom(name), AtomEnum::CARDINAL, 0, 64)
            .unwrap()
            .reply()
            .unwrap();
        reply.value32().map(Iterator::collect).unwrap_or_default()
    }

    async fn baselayer_becomes(&self, expected: &[u32]) {
        eventually(&format!("BASELAYER_APPID = {expected:?}"), async || {
            (self.get("GAMESCOPECTRL_BASELAYER_APPID") == expected).then_some(())
        })
        .await;
    }
}

impl Drop for FakeGamescope {
    fn drop(&mut self) {
        let _ = self.xvfb.kill();
        let _ = self.xvfb.wait();
    }
}

#[tokio::test]
async fn focus_follows_launches_home_and_closed_windows() {
    let Some(gs) = FakeGamescope::start() else {
        return;
    };
    let display = gs.display.clone();
    let d = Daemon::start_with(|dir| {
        let config = dir.join("session.toml");
        std::fs::write(&config, "[gamescope]\nfocus_loss_debounce_ms = 200\n").unwrap();
        let steam = dir.join("steam");
        script(&steam, "exit 0");
        vec![
            "--config".into(),
            config.into(),
            "--gamescope".into(),
            "--gamescope-display".into(),
            display.into(),
            "--steam-path".into(),
            steam.into(),
        ]
    });
    // At start the launcher has focus.
    gs.baselayer_becomes(&[LAUNCHER]).await;

    let mut c = d.client().await;
    call(
        &mut c,
        "subscribe",
        json!({ "events": ["home_pressed", "app_started", "app_exited", "focus_changed"] }),
    )
    .await
    .unwrap();

    // A Steam game: shown as soon as it has a window, launcher until then.
    call(
        &mut c,
        "apps.launch",
        json!({ "url": "steam://rungameid/570" }),
    )
    .await
    .unwrap();
    assert_eq!(next_event(&mut c).await.0, "app_started");
    assert_eq!(
        next_event(&mut c).await,
        ("focus_changed".into(), json!({ "app": "steam-570" }))
    );
    gs.baselayer_becomes(&[570, 769, LAUNCHER]).await;

    // gamescope maps the game's window and focuses it.
    gs.set("GAMESCOPE_FOCUSABLE_APPS", &[570]);
    gs.set("GAMESCOPE_FOCUSED_APP", &[570]);

    // Home: the launcher is shown, the game keeps running.
    d.signal(rustix::process::Signal::USR1);
    assert_eq!(next_event(&mut c).await.0, "home_pressed");
    assert_eq!(
        next_event(&mut c).await,
        ("focus_changed".into(), json!({ "app": null }))
    );
    gs.baselayer_becomes(&[LAUNCHER]).await;
    gs.set("GAMESCOPE_FOCUSED_APP", &[LAUNCHER]);

    // Back to the game, which then quits: its window goes away.
    call(&mut c, "apps.focus", json!({ "id": "steam-570" }))
        .await
        .unwrap();
    assert_eq!(next_event(&mut c).await.0, "focus_changed");
    gs.baselayer_becomes(&[570, 769, LAUNCHER]).await;
    gs.set("GAMESCOPE_FOCUSED_APP", &[570]);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    gs.set("GAMESCOPE_FOCUSABLE_APPS", &[]);
    gs.set("GAMESCOPE_FOCUSED_APP", &[LAUNCHER]);
    assert_eq!(
        next_event(&mut c).await,
        (
            "app_exited".into(),
            json!({ "id": "steam-570", "reason": "exited" })
        )
    );
    assert_eq!(
        next_event(&mut c).await,
        ("focus_changed".into(), json!({ "app": null }))
    );
    gs.baselayer_becomes(&[LAUNCHER]).await;
}

impl FakeGamescope {
    /// A top-level window like one of Steam's (or a game's), mapped.
    fn window(&self, w: &TestWindow) -> Window {
        let id = self.conn.generate_id().unwrap();
        self.conn
            .create_window(
                0,
                id,
                self.root,
                0,
                0,
                400,
                300,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new().override_redirect(u32::from(w.override_redirect)),
            )
            .unwrap();
        let utf8 = self.atom("UTF8_STRING");
        if let Some(class) = w.class {
            let value = format!("{class}\0{class}\0");
            self.conn
                .change_property8(
                    PropMode::REPLACE,
                    id,
                    AtomEnum::WM_CLASS,
                    AtomEnum::STRING,
                    value.as_bytes(),
                )
                .unwrap();
        }
        self.conn
            .change_property8(
                PropMode::REPLACE,
                id,
                self.atom("_NET_WM_NAME"),
                utf8,
                w.title.as_bytes(),
            )
            .unwrap();
        if let Some(pid) = w.pid {
            self.set_on(id, "_NET_WM_PID", &[pid]);
        }
        if let Some(game) = w.steam_game {
            self.set_on(id, "STEAM_GAME", &[game]);
        }
        if w.popup_menu_type {
            let popup = self.atom("_NET_WM_WINDOW_TYPE_POPUP_MENU");
            self.conn
                .change_property32(
                    PropMode::REPLACE,
                    id,
                    self.atom("_NET_WM_WINDOW_TYPE"),
                    AtomEnum::ATOM,
                    &[popup],
                )
                .unwrap();
        }
        self.conn.map_window(id).unwrap();
        self.conn.flush().unwrap();
        id
    }

    fn set_on(&self, window: Window, name: &str, values: &[u32]) {
        self.conn
            .change_property32(
                PropMode::REPLACE,
                window,
                self.atom(name),
                AtomEnum::CARDINAL,
                values,
            )
            .unwrap()
            .check()
            .unwrap();
    }

    fn get_on(&self, window: Window, name: &str) -> Vec<u32> {
        let reply = self
            .conn
            .get_property(false, window, self.atom(name), AtomEnum::CARDINAL, 0, 64)
            .unwrap()
            .reply()
            .unwrap();
        reply.value32().map(Iterator::collect).unwrap_or_default()
    }

    fn destroy(&self, window: Window) {
        self.conn.destroy_window(window).unwrap();
        self.conn.flush().unwrap();
    }
}

#[derive(Default)]
struct TestWindow {
    class: Option<&'static str>,
    title: &'static str,
    pid: Option<u32>,
    steam_game: Option<u32>,
    override_redirect: bool,
    /// Set `_NET_WM_WINDOW_TYPE` to `_NET_WM_WINDOW_TYPE_POPUP_MENU`.
    popup_menu_type: bool,
}

fn start_daemon(gs: &FakeGamescope, extra_config: &str) -> Daemon {
    let display = gs.display.clone();
    let extra_config = extra_config.to_string();
    Daemon::start_with(|dir| {
        let config = dir.join("session.toml");
        std::fs::write(
            &config,
            format!("[gamescope]\nfocus_loss_debounce_ms = 200\n{extra_config}"),
        )
        .unwrap();
        vec![
            "--config".into(),
            config.into(),
            "--gamescope".into(),
            "--gamescope-display".into(),
            display.into(),
        ]
    })
}

#[tokio::test]
async fn steam_popups_are_tagged_announced_and_shown() {
    let _steam = STEAM_REAPERS.lock().await;
    let Some(gs) = FakeGamescope::start() else {
        return;
    };
    // A process named like Steam's web helper, to own a window by _NET_WM_PID.
    let dir = tempfile::tempdir().unwrap();
    let helper_path = dir.path().join("steamwebhelper");
    script(&helper_path, "sleep 30");
    let mut helper = std::process::Command::new(&helper_path).spawn().unwrap();
    let helper_pid = helper.id();

    let d = start_daemon(&gs, "");
    gs.baselayer_becomes(&[LAUNCHER]).await;
    let mut c = d.client().await;
    call(
        &mut c,
        "subscribe",
        json!({ "events": ["steam_attention", "focus_changed", "app_started", "app_exited"] }),
    )
    .await
    .unwrap();
    assert_eq!(
        call(&mut c, "steam.attention", json!({})).await.unwrap(),
        json!({ "windows": [] })
    );

    let game = gs.window(&TestWindow {
        class: Some("steam_app_620"),
        title: "Portal 2",
        steam_game: Some(620),
        ..Default::default()
    });
    let menu = gs.window(&TestWindow {
        class: Some("steamwebhelper"),
        title: "",
        override_redirect: true,
        ..Default::default()
    });
    let guard = gs.window(&TestWindow {
        class: Some("steamwebhelper"),
        title: "Steam Guard",
        ..Default::default()
    });
    let update = gs.window(&TestWindow {
        title: "Steam - Update",
        pid: Some(helper_pid),
        ..Default::default()
    });

    // Both dialogs are announced (the menu and the game's window are not).
    let expected = json!({ "windows": [
        { "window": guard, "title": "Steam Guard", "pid": std::process::id() },
        { "window": update, "title": "Steam - Update", "pid": helper_pid },
    ] });
    let mut expected_no_xres = expected.clone();
    expected_no_xres["windows"][0]
        .as_object_mut()
        .unwrap()
        .remove("pid");
    loop {
        let params = event_named(&mut c, "steam_attention").await;
        if params == expected || params == expected_no_xres {
            break;
        }
    }
    // Steam's windows are tagged as app 769, the game's keeps its id.
    for w in [menu, guard, update] {
        assert_eq!(gs.get_on(w, "STEAM_GAME"), [769], "window {w}");
    }
    assert_eq!(gs.get_on(game, "STEAM_GAME"), [620]);

    // The launcher brings Steam forward.
    let r = call(&mut c, "steam.focus", json!({})).await.unwrap();
    assert_eq!(r["app"]["id"], "steam");
    assert_eq!(
        event_named(&mut c, "focus_changed").await,
        json!({ "app": "steam" })
    );
    assert_eq!(
        event_named(&mut c, "steam_attention").await,
        json!({ "windows": [] })
    );
    gs.baselayer_becomes(&[769, LAUNCHER]).await;

    // The user deals with them; they close and the launcher is back.
    gs.destroy(guard);
    gs.destroy(update);
    gs.destroy(menu);
    // The daemon reads gamescope's state when a property changes, and changes that follow
    // each other quickly are read as one. It must have seen Steam listed and focused before
    // that vanishes, or the app is not known to have shown a window at all. Real gamescope
    // changes state over time; so does this one, repeating the cycle (rather than
    // sleeping and hoping) until the daemon has reacted.
    let cycle = async {
        loop {
            gs.set("GAMESCOPE_FOCUSABLE_APPS", &[620, 769]);
            gs.set("GAMESCOPE_FOCUSED_APP", &[769]);
            tokio::time::sleep(Duration::from_millis(150)).await;
            gs.set("GAMESCOPE_FOCUSABLE_APPS", &[620]);
            gs.set("GAMESCOPE_FOCUSED_APP", &[LAUNCHER]);
            // Longer than the focus-loss debounce.
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    };
    let exited = tokio::select! {
        e = event_named(&mut c, "app_exited") => e,
        () = cycle => unreachable!(),
    };
    gs.set("GAMESCOPE_FOCUSABLE_APPS", &[620]);
    gs.set("GAMESCOPE_FOCUSED_APP", &[LAUNCHER]);
    assert_eq!(exited, json!({ "id": "steam", "reason": "exited" }));
    assert_eq!(
        event_named(&mut c, "focus_changed").await,
        json!({ "app": null })
    );
    gs.baselayer_becomes(&[LAUNCHER]).await;
    let e = call(&mut c, "steam.focus", json!({})).await.unwrap_err();
    assert_eq!(e["code"], 2003);

    let _ = helper.kill();
    let _ = helper.wait();
}

#[tokio::test]
async fn closing_a_steam_game_ends_its_process_tree() {
    let _steam = STEAM_REAPERS.lock().await;
    let Some(gs) = FakeGamescope::start() else {
        return;
    };
    const GAME: u32 = 1_000_123;
    let display = gs.display.clone();
    let d = Daemon::start_with(|dir| {
        // A fake Steam that starts the game under a Steam-style reaper (ours, which matches
        // Steam's command line) and returns, like `steam -silent steam://rungameid/<id>`.
        let steam = dir.join("steam");
        script(
            &steam,
            &format!(
                "id=${{2#steam://rungameid/}}\n\
                 {reaper} reaper SteamLaunch AppId=$id -- sh -c \"trap '' TERM; echo \\$\\$ > {dir}/game.pid; sleep 300\" >/dev/null 2>&1 &",
                reaper = env!("CARGO_BIN_EXE_hinteros-session"),
                dir = dir.display(),
            ),
        );
        let config = dir.join("session.toml");
        std::fs::write(
            &config,
            "[gamescope]\nfocus_loss_debounce_ms = 200\n[close]\ntimeout_ms = 300\n",
        )
        .unwrap();
        vec![
            "--config".into(),
            config.into(),
            "--gamescope".into(),
            "--gamescope-display".into(),
            display.into(),
            "--steam-path".into(),
            steam.into(),
        ]
    });
    let mut c = d.client().await;
    call(&mut c, "subscribe", json!({ "events": ["app_exited"] }))
        .await
        .unwrap();
    call(
        &mut c,
        "apps.launch",
        json!({ "url": format!("steam://rungameid/{GAME}") }),
    )
    .await
    .unwrap();
    let pid_file = d.dir.path().join("game.pid");
    let game_pid: u32 = eventually("the game to start", async || {
        std::fs::read_to_string(&pid_file).ok()?.trim().parse().ok()
    })
    .await;
    gs.set("GAMESCOPE_FOCUSABLE_APPS", &[GAME]);
    gs.set("GAMESCOPE_FOCUSED_APP", &[GAME]);
    // The daemon must have seen the game before it goes, or nothing says that it went.
    eventually("the daemon to see the game", async || {
        d.log().contains("Steam's reaper for it runs").then_some(())
    })
    .await;

    call(
        &mut c,
        "apps.close",
        json!({ "id": format!("steam-{GAME}") }),
    )
    .await
    .unwrap();
    // It ignores SIGTERM, so it takes the SIGKILL after the timeout.
    eventually("the game to be killed", async || {
        (!alive(game_pid)).then_some(())
    })
    .await;
    gs.set("GAMESCOPE_FOCUSABLE_APPS", &[]);
    gs.set("GAMESCOPE_FOCUSED_APP", &[LAUNCHER]);
    assert_eq!(
        next_event(&mut c).await,
        (
            "app_exited".into(),
            json!({ "id": format!("steam-{GAME}"), "reason": "closed" })
        )
    );
}

#[tokio::test]
async fn on_screen_keyboard_is_an_overlay_shown_by_opacity() {
    let Some(gs) = FakeGamescope::start() else {
        return;
    };
    // A stand-in OSK program: records that it was started, and with which socket.
    let tmp = tempfile::tempdir().unwrap();
    let started = tmp.path().join("osk-started");
    let osk_program = tmp.path().join("hinteros-osk");
    script(
        &osk_program,
        &format!("echo \"$HINTEROS_SESSION_SOCKET\" > {}", started.display()),
    );
    let d = start_daemon(
        &gs,
        &format!("[osk]\ncommand = [\"{}\"]\n", osk_program.display()),
    );
    gs.baselayer_becomes(&[LAUNCHER]).await;
    let socket = eventually("the OSK program to start", async || {
        std::fs::read_to_string(&started).ok()
    })
    .await;
    assert_eq!(socket.trim(), d.socket.to_str().unwrap());

    // Its window becomes an external overlay, hidden (opacity 0); other windows are untouched.
    let osk = gs.window(&TestWindow {
        class: Some("hinteros-osk"),
        title: "HinterOS keyboard",
        ..Default::default()
    });
    let app = gs.window(&TestWindow {
        class: Some("firefox"),
        title: "YouTube",
        ..Default::default()
    });
    let state = |w| {
        (
            gs.get_on(w, "GAMESCOPE_EXTERNAL_OVERLAY"),
            gs.get_on(w, "_NET_WM_WINDOW_OPACITY"),
        )
    };
    let hidden = (vec![1], vec![0]);
    let shown = (vec![1], vec![0xffff_ffff]);
    eventually("the OSK window to be tagged", async || {
        (state(osk) == hidden).then_some(())
    })
    .await;

    let mut c = d.client().await;
    call(&mut c, "subscribe", json!({ "events": ["osk_changed"] }))
        .await
        .unwrap();
    assert_eq!(
        call(&mut c, "osk.show", json!({})).await.unwrap(),
        json!({ "visible": true })
    );
    assert_eq!(
        next_event(&mut c).await,
        ("osk_changed".into(), json!({ "visible": true }))
    );
    eventually("the OSK window to be shown", async || {
        (state(osk) == shown).then_some(())
    })
    .await;
    assert_eq!(gs.get("HINTEROS_OSK_VISIBLE"), [1]);
    // Showing never changes gamescope focus.
    assert_eq!(gs.get("GAMESCOPECTRL_BASELAYER_APPID"), [LAUNCHER]);

    // The OSK restarts while shown: its new window is shown at once.
    gs.destroy(osk);
    let osk = gs.window(&TestWindow {
        class: Some("Hinteros-osk"),
        title: "HinterOS keyboard",
        ..Default::default()
    });
    eventually("the new OSK window to be shown", async || {
        (state(osk) == shown).then_some(())
    })
    .await;

    assert_eq!(
        call(&mut c, "osk.hide", json!({})).await.unwrap(),
        json!({ "visible": false })
    );
    assert_eq!(
        next_event(&mut c).await,
        ("osk_changed".into(), json!({ "visible": false }))
    );
    eventually("the OSK window to be hidden", async || {
        (state(osk) == hidden).then_some(())
    })
    .await;
    assert_eq!(state(app), (vec![], vec![]));
}

impl FakeGamescope {
    /// gamescope restarting: the X server dies and another one comes up on the same display,
    /// with the properties of a fresh gamescope.
    fn restart(&mut self) {
        rustix::process::kill_process(
            rustix::process::Pid::from_raw(i32::try_from(self.xvfb.id()).unwrap()).unwrap(),
            rustix::process::Signal::TERM,
        )
        .unwrap();
        self.xvfb.wait().unwrap();
        let mut xvfb = Command::new("Xvfb")
            .arg(&self.display)
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
            .unwrap();
        let mut line = String::new();
        BufReader::new(xvfb.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(!line.is_empty(), "Xvfb exited before it was ready");
        self.xvfb = xvfb;
        let (conn, screen) = x11rb::connect(Some(&self.display)).unwrap();
        self.root = conn.setup().roots[screen].root;
        self.conn = conn;
        self.set("GAMESCOPE_PID", &[std::process::id() + 1]);
        self.set("GAMESCOPE_FOCUSED_APP", &[]);
        self.set("GAMESCOPE_FOCUSABLE_APPS", &[]);
    }
}

#[tokio::test]
async fn focus_is_applied_again_after_gamescope_restarts() {
    let Some(mut gs) = FakeGamescope::start() else {
        return;
    };
    let d = start_daemon(&gs, "");
    gs.baselayer_becomes(&[LAUNCHER]).await;
    let mut c = d.client().await;
    call(
        &mut c,
        "apps.launch",
        json!({ "url": "steam://rungameid/570" }),
    )
    .await
    .unwrap();
    gs.baselayer_becomes(&[570, 769, LAUNCHER]).await;

    // greetd restarts gamescope and its Xwayland: the daemon's connections are dead.
    gs.restart();
    assert!(gs.get("GAMESCOPECTRL_BASELAYER_APPID").is_empty());
    gs.baselayer_becomes(&[570, 769, LAUNCHER]).await;

    // And once more, to be sure it keeps working.
    gs.restart();
    gs.baselayer_becomes(&[570, 769, LAUNCHER]).await;
}

impl FakeGamescope {
    fn is_mapped(&self, window: Window) -> bool {
        use x11rb::protocol::xproto::MapState;
        self.conn
            .get_window_attributes(window)
            .unwrap()
            .reply()
            .unwrap()
            .map_state
            != MapState::UNMAPPED
    }
}

/// A launched app (under the daemon's reaper) and the windows GTK3 would make for it.
async fn gtk_dummy_scenario(
    extra_config: &str,
) -> (FakeGamescope, Daemon, Vec<(&'static str, Window)>) {
    let gs = FakeGamescope::start().expect("Xvfb");
    let d = start_daemon(&gs, extra_config);
    gs.baselayer_becomes(&[LAUNCHER]).await;
    let mut c = d.client().await;
    let app = call(
        &mut c,
        "apps.launch",
        json!({ "command": ["sleep", "60"], "name": "GTK app" }),
    )
    .await
    .unwrap()["app"]
        .clone();
    let reaper = u32::try_from(app["pid"].as_u64().unwrap()).unwrap();
    // Windows are created by this process; _NET_WM_PID says who they belong to.
    let ours = |class, title, override_redirect, pid| TestWindow {
        class,
        title,
        override_redirect,
        pid,
        ..Default::default()
    };
    // Things that must be left alone, each looking a bit like the dummy.
    let menu = gs.window(&TestWindow {
        popup_menu_type: true,
        ..ours(None, "", true, Some(reaper))
    });
    let steam = gs.window(&ours(Some("steamwebhelper"), "", true, Some(reaper)));
    let classed = gs.window(&ours(Some("gtk-popup"), "", true, Some(reaper)));
    let stranger = gs.window(&ours(None, "", true, None)); // not an app of ours
    // The app's real window, then its dummy.
    let real = gs.window(&ours(Some("gtkapp"), "GTK app", false, Some(reaper)));
    let dummy = gs.window(&ours(None, "", true, Some(reaper)));
    let windows = vec![
        ("menu", menu),
        ("steam", steam),
        ("classed", classed),
        ("stranger", stranger),
        ("real", real),
        ("dummy", dummy),
    ];
    (gs, d, windows)
}

#[tokio::test]
async fn an_invisible_override_redirect_window_over_an_app_is_unmapped() {
    if FakeGamescope::start().is_none() {
        return;
    }
    let (gs, _d, w) = gtk_dummy_scenario("").await;
    let id = |name: &str| w.iter().find(|(n, _)| *n == name).unwrap().1;
    eventually("the dummy is unmapped", async || {
        (!gs.is_mapped(id("dummy"))).then_some(())
    })
    .await;
    for keep in ["menu", "steam", "classed", "stranger", "real"] {
        assert!(gs.is_mapped(id(keep)), "{keep} was left alone");
    }
}

#[tokio::test]
async fn gl_dummies_are_left_alone_when_turned_off() {
    if FakeGamescope::start().is_none() {
        return;
    }
    let (gs, _d, w) = gtk_dummy_scenario("unmap_gl_dummies = false\n").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    for (name, window) in w {
        assert!(gs.is_mapped(window), "{name}");
    }
}

/// A daemon on `gs` whose Steam is `steam`.
fn start_daemon_with_steam(gs: &FakeGamescope, steam: &std::path::Path, extra: &str) -> Daemon {
    let display = gs.display.clone();
    let steam = steam.to_path_buf();
    let extra = extra.to_string();
    Daemon::start_with(|dir| {
        let config = dir.join("session.toml");
        std::fs::write(
            &config,
            format!("[gamescope]\nfocus_loss_debounce_ms = 200\n{extra}"),
        )
        .unwrap();
        vec![
            "--config".into(),
            config.into(),
            "--gamescope".into(),
            "--gamescope-display".into(),
            display.into(),
            "--steam-path".into(),
            steam.into(),
        ]
    })
}

/// Steam's pre-launch window (shader pre-caching) while a game starts: shown, the launch is
/// not given up; launching the running game again only focuses it, also from a daemon that
/// started after the game.
#[tokio::test]
async fn steam_game_launch_survives_shader_precaching_and_is_not_launched_twice() {
    let _steam = STEAM_REAPERS.lock().await;
    let Some(gs) = FakeGamescope::start() else {
        return;
    };
    // Unique per run: a game left over from an aborted run must not count.
    let game_id = 1_000_000 + std::process::id() % 1_000_000;
    let steam_dir = tempfile::tempdir().unwrap();
    let dir = steam_dir.path();
    let _cleanup = KillGame(dir.join("game.pid"));
    // A fake Steam: logs every launch request and starts the game (without a window) under a
    // Steam-style reaper, like `steam -silent steam://rungameid/<id>`.
    let steam = dir.join("steam");
    script(
        &steam,
        &format!(
            "echo \"$*\" >> {dir}/requests\n\
             case \"$2\" in steam://rungameid/*) id=${{2#steam://rungameid/}}\n\
             {reaper} reaper SteamLaunch AppId=$id -- sh -c \"echo \\$\\$ > {dir}/game.pid; exec sleep 600\" >/dev/null 2>&1 & ;; esac",
            reaper = env!("CARGO_BIN_EXE_hinteros-session"),
            dir = dir.display(),
        ),
    );
    let requests = || {
        std::fs::read_to_string(dir.join("requests"))
            .unwrap_or_default()
            .lines()
            .count()
    };
    // The fake Steam must start its reaper within the timeout: it has that long on a loaded
    // machine, and the test waits as long.
    let launch_timeout = Duration::from_secs(5);
    let d = start_daemon_with_steam(&gs, &steam, "[steam]\nlaunch_timeout_s = 5\n");
    gs.baselayer_becomes(&[LAUNCHER]).await;
    let mut c = d.client().await;
    call(
        &mut c,
        "subscribe",
        json!({ "events": ["app_exited", "focus_changed", "steam_attention"] }),
    )
    .await
    .unwrap();
    let launched = std::time::Instant::now();
    call(
        &mut c,
        "apps.launch",
        json!({ "url": format!("steam://rungameid/{game_id}") }),
    )
    .await
    .unwrap();
    assert_eq!(
        next_event(&mut c).await,
        (
            "focus_changed".into(),
            json!({ "app": format!("steam-{game_id}") })
        )
    );
    // The game first, Steam's own windows while it has none, then the launcher.
    gs.baselayer_becomes(&[game_id, 769, LAUNCHER]).await;
    let game_pid: u32 = eventually("the game to start", async || {
        std::fs::read_to_string(dir.join("game.pid"))
            .ok()?
            .trim()
            .parse()
            .ok()
    })
    .await;
    // Steam's shader pre-caching window; gamescope shows it (app 769).
    let shaders = gs.window(&TestWindow {
        class: Some("steam"),
        title: "Processing Vulkan shaders",
        ..Default::default()
    });
    eventually("the window tagged as Steam's", async || {
        (gs.get_on(shaders, "STEAM_GAME") == [769]).then_some(())
    })
    .await;
    gs.set("GAMESCOPE_FOCUSABLE_APPS", &[769]);
    gs.set("GAMESCOPE_FOCUSED_APP", &[769]);
    // Well past the launch timeout: still launching, still focused.
    let past = launched + launch_timeout + Duration::from_secs(2);
    let quiet = tokio::time::timeout_at(past.into(), next_event(&mut c)).await;
    assert!(quiet.is_err(), "unexpected {quiet:?}");
    gs.destroy(shaders);
    gs.set("GAMESCOPE_FOCUSABLE_APPS", &[]);
    gs.set("GAMESCOPE_FOCUSED_APP", &[LAUNCHER]);
    // No window, but its reaper is alive: still running.
    let quiet = tokio::time::timeout(Duration::from_secs(2), next_event(&mut c)).await;
    assert!(quiet.is_err(), "unexpected {quiet:?}");

    // Play again: focused, not requested from Steam again.
    let r = call(
        &mut c,
        "apps.launch",
        json!({ "url": format!("steam://rungameid/{game_id}") }),
    )
    .await
    .unwrap();
    assert_eq!(r["app"]["id"], format!("steam-{game_id}"));
    assert_eq!(requests(), 1);

    // The daemon restarts while the game runs: it finds the game and does not launch it again.
    drop(c);
    drop(d);
    let d = start_daemon_with_steam(&gs, &steam, "");
    let mut c = d.client().await;
    // (Games of other tests running at the same time may be adopted too.)
    eventually("the game to be adopted", async || {
        let r = call(&mut c, "apps.running", json!({})).await.ok()?;
        let id = format!("steam-{game_id}");
        r["apps"]
            .as_array()?
            .iter()
            .any(|a| a["id"] == id)
            .then_some(())
    })
    .await;
    call(
        &mut c,
        "apps.launch",
        json!({ "url": format!("steam://rungameid/{game_id}") }),
    )
    .await
    .unwrap();
    gs.baselayer_becomes(&[game_id, 769, LAUNCHER]).await;
    assert_eq!(requests(), 1, "never 'Game already running'");
    assert!(alive(game_pid));
}

/// A Steam window that answers our own request while the launcher has focus (here: an error
/// after a launch the user left with Home) gets focus and the pad.
#[tokio::test]
async fn steams_answer_over_the_launcher_gets_focus() {
    let _steam = STEAM_REAPERS.lock().await;
    let Some(gs) = FakeGamescope::start() else {
        return;
    };
    let steam_dir = tempfile::tempdir().unwrap();
    let steam = steam_dir.path().join("steam");
    script(&steam, "exit 0");
    let d = start_daemon_with_steam(&gs, &steam, "");
    gs.baselayer_becomes(&[LAUNCHER]).await;
    let mut c = d.client().await;
    call(&mut c, "subscribe", json!({ "events": ["focus_changed"] }))
        .await
        .unwrap();
    call(
        &mut c,
        "apps.launch",
        json!({ "url": "steam://rungameid/620" }),
    )
    .await
    .unwrap();
    assert_eq!(
        next_event(&mut c).await,
        ("focus_changed".into(), json!({ "app": "steam-620" }))
    );
    call(&mut c, "apps.focus", json!({ "id": null }))
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut c).await,
        ("focus_changed".into(), json!({ "app": null }))
    );
    let error = gs.window(&TestWindow {
        class: Some("steamwebhelper"),
        title: "Steam - Error",
        ..Default::default()
    });
    assert_eq!(
        next_event(&mut c).await,
        ("focus_changed".into(), json!({ "app": "steam" }))
    );
    gs.baselayer_becomes(&[769, LAUNCHER]).await;
    gs.destroy(error);
}

/// Ends the fake game (and so its reaper) whose pid is in the file, also when a test fails.
struct KillGame(std::path::PathBuf);

impl Drop for KillGame {
    fn drop(&mut self) {
        if let Ok(pid) = std::fs::read_to_string(&self.0) {
            let _ = std::process::Command::new("kill").arg(pid.trim()).status();
        }
    }
}
