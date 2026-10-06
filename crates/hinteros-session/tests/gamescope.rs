//! The daemon against a fake gamescope: an Xvfb whose root window carries the properties
//! gamescope publishes. The test plays gamescope (it updates GAMESCOPE_FOCUSED_APP and
//! GAMESCOPE_FOCUSABLE_APPS) and checks what the daemon writes to
//! GAMESCOPECTRL_BASELAYER_APPID and reports over the socket. Skipped when `Xvfb` is not on
//! PATH, unless HINTEROS_REQUIRE_TEST_TOOLS is set.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

use serde_json::json;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode, Window};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

mod common;

use common::*;

const LAUNCHER: u32 = 4_100_000_000;

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
    gs.baselayer_becomes(&[570, LAUNCHER]).await;

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
    gs.baselayer_becomes(&[570, LAUNCHER]).await;
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
