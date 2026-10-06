//! The daemon against a fake InputPlumber (same D-Bus names, interfaces and properties as the
//! real one, see hinteros-docs/spikes/04-input-layer.md) on a private dbus-daemon. Skipped when
//! `dbus-daemon` is not on PATH, unless HINTEROS_REQUIRE_TEST_TOOLS is set.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use hinteros_api::client::Client;
use serde_json::json;
use zbus::names::BusName;

mod common;

use common::*;

const SERVICE: &str = "org.shadowblip.InputPlumber";
const ROOT: &str = "/org/shadowblip/InputPlumber";
const MANAGER: &str = "/org/shadowblip/InputPlumber/Manager";
const TARGET: &str = "/org/shadowblip/InputPlumber/devices/target/dbus0";

fn device(n: u32) -> String {
    format!("{ROOT}/CompositeDevice{n}")
}

struct Manager(Arc<AtomicBool>);

#[zbus::interface(name = "org.shadowblip.InputManager")]
impl Manager {
    #[zbus(property)]
    fn manage_all_devices(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    #[zbus(property)]
    fn set_manage_all_devices(&mut self, value: bool) {
        self.0.store(value, Ordering::SeqCst);
    }
}

struct Composite(Arc<AtomicU32>);

#[zbus::interface(name = "org.shadowblip.Input.CompositeDevice")]
impl Composite {
    #[zbus(property)]
    fn intercept_mode(&self) -> u32 {
        self.0.load(Ordering::SeqCst)
    }
    #[zbus(property)]
    fn set_intercept_mode(&mut self, value: u32) {
        self.0.store(value, Ordering::SeqCst);
    }
}

struct DBusDevice;

#[zbus::interface(name = "org.shadowblip.Input.DBusDevice")]
impl DBusDevice {
    #[zbus(property)]
    fn name(&self) -> String {
        "DBusDevice".into()
    }
}

/// A fake InputPlumber: one manager, one composite device and its D-Bus target.
struct Fake {
    conn: zbus::Connection,
    manage_all: Arc<AtomicBool>,
    modes: Vec<Arc<AtomicU32>>,
}

impl Fake {
    async fn start(address: &str) -> Self {
        let manage_all = Arc::new(AtomicBool::new(false));
        let mode = Arc::new(AtomicU32::new(0));
        let conn = zbus::connection::Builder::address(address)
            .unwrap()
            .serve_at(ROOT, zbus::fdo::ObjectManager)
            .unwrap()
            .serve_at(MANAGER, Manager(manage_all.clone()))
            .unwrap()
            .serve_at(device(0), Composite(mode.clone()))
            .unwrap()
            .serve_at(TARGET, DBusDevice)
            .unwrap()
            .name(SERVICE)
            .unwrap()
            .build()
            .await
            .unwrap();
        Self {
            conn,
            manage_all,
            modes: vec![mode],
        }
    }

    fn mode(&self, n: usize) -> u32 {
        self.modes[n].load(Ordering::SeqCst)
    }

    /// Plug in another pad.
    async fn add_device(&mut self) {
        let mode = Arc::new(AtomicU32::new(0));
        let path = device(self.modes.len() as u32);
        self.conn
            .object_server()
            .at(path, Composite(mode.clone()))
            .await
            .unwrap();
        self.modes.push(mode);
    }

    async fn press(&self, event: &str, value: f64) {
        self.conn
            .emit_signal(
                None::<BusName<'_>>,
                TARGET,
                "org.shadowblip.Input.DBusDevice",
                "InputEvent",
                &(event, value),
            )
            .await
            .unwrap();
    }
}

async fn ready(fake: &Fake, device: usize, mode: u32) {
    eventually(&format!("device {device} in mode {mode}"), async || {
        (fake.manage_all.load(Ordering::SeqCst) && fake.mode(device) == mode).then_some(())
    })
    .await;
}

async fn nav(c: &mut Client) -> (String, String) {
    let (name, params) = next_event(c).await;
    assert_eq!(name, "input_nav", "{params}");
    (
        params["action"].as_str().unwrap().into(),
        params["phase"].as_str().unwrap().into(),
    )
}

#[tokio::test]
async fn inputplumber_modes_nav_and_home() {
    let Some(bus) = Bus::start() else { return };
    let address = bus.address.clone();
    // The daemon starts first and waits for InputPlumber.
    let d = Daemon::start_with(|_| {
        vec![
            "--inputplumber".into(),
            "--inputplumber-bus".into(),
            address.into(),
        ]
    });
    let mut c = d.client().await;
    call(
        &mut c,
        "subscribe",
        json!({ "events": ["home_pressed", "input_nav", "focus_changed", "app_started"] }),
    )
    .await
    .unwrap();

    let mut fake = Fake::start(&bus.address).await;
    // Launcher focused: all pads managed, mode ALL (2).
    ready(&fake, 0, 2).await;

    // A held direction: press, repeats, release.
    fake.press("ui_right", 1.0).await;
    assert_eq!(nav(&mut c).await, ("right".into(), "press".into()));
    assert_eq!(nav(&mut c).await, ("right".into(), "repeat".into()));
    fake.press("ui_right", 0.0).await;
    loop {
        match nav(&mut c).await {
            (a, p) if a == "right" && p == "repeat" => {}
            (a, p) => {
                assert_eq!((a.as_str(), p.as_str()), ("right", "release"));
                break;
            }
        }
    }
    fake.press("ui_accept", 1.0).await;
    assert_eq!(nav(&mut c).await, ("accept".into(), "press".into()));
    fake.press("ui_accept", 0.0).await;
    assert_eq!(nav(&mut c).await, ("accept".into(), "release".into()));

    // An app gets focus: mode PASS (1), no more nav events.
    call(&mut c, "apps.launch", json!({ "command": ["sleep", "30"] }))
        .await
        .unwrap();
    assert_eq!(next_event(&mut c).await.0, "app_started");
    assert_eq!(next_event(&mut c).await.0, "focus_changed");
    ready(&fake, 0, 1).await;
    fake.press("ui_accept", 1.0).await;

    // Guide: home_pressed (on release: Guide is also the on-screen keyboard chord), the
    // launcher gets focus, mode ALL again.
    fake.press("ui_guide", 1.0).await;
    fake.press("ui_guide", 0.0).await;
    assert_eq!(next_event(&mut c).await.0, "home_pressed");
    assert_eq!(
        next_event(&mut c).await,
        ("focus_changed".into(), json!({ "app": null }))
    );
    ready(&fake, 0, 2).await;

    // A second pad gets the current mode.
    fake.add_device().await;
    ready(&fake, 1, 2).await;

    // InputPlumber restarts and forgets everything: the daemon sets it up again.
    drop(fake);
    let mut fake = Fake::start(&bus.address).await;
    ready(&fake, 0, 2).await;
    fake.add_device().await;
    ready(&fake, 1, 2).await;
    fake.press("ui_left", 1.0).await;
    assert_eq!(nav(&mut c).await, ("left".into(), "press".into()));
}

/// The key events written by `--fake-keyboard`, as `(code, down)`.
fn key_log(path: &std::path::Path) -> Vec<(u16, bool)> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| {
            let (what, code) = l.split_once(' ').unwrap();
            (code.parse().unwrap(), what == "down")
        })
        .collect()
}

/// Taps of `codes`, as the fake keyboard logs them.
fn taps(codes: &[u16]) -> Vec<(u16, bool)> {
    codes
        .iter()
        .flat_map(|&c| [(c, true), (c, false)])
        .collect()
}

#[tokio::test]
async fn keyboard_profile_and_on_screen_keyboard() {
    let Some(bus) = Bus::start() else { return };
    let address = bus.address.clone();
    let keys_file = std::sync::Arc::new(std::sync::Mutex::new(std::path::PathBuf::new()));
    let kf = keys_file.clone();
    let d = Daemon::start_with(|dir| {
        let config = dir.join("session.toml");
        std::fs::write(
            &config,
            r#"
[keys]
key_delay_ms = 0
[[apps]]
id = "youtube"
name = "YouTube"
kind = "web"
command = ["sleep", "30"]
input = "keyboard"
[apps.keymap]
context = "ctrl+l"
"#,
        )
        .unwrap();
        let log = dir.join("keys.log");
        *kf.lock().unwrap() = log.clone();
        vec![
            "--config".into(),
            config.into(),
            "--inputplumber".into(),
            "--inputplumber-bus".into(),
            address.into(),
            "--fake-keyboard".into(),
            log.into(),
        ]
    });
    let keys_file = keys_file.lock().unwrap().clone();
    let fake = Fake::start(&bus.address).await;
    ready(&fake, 0, 2).await;

    let mut launcher = d.client().await;
    let mut osk = d.client_named("hinteros-osk").await;
    for c in [&mut launcher, &mut osk] {
        call(
            c,
            "subscribe",
            json!({ "events": ["input_nav", "osk_visibility", "home_pressed", "focus_changed"] }),
        )
        .await
        .unwrap();
    }

    // The launcher has navigation; the OSK client gets none while it is hidden.
    fake.press("ui_accept", 1.0).await;
    assert_eq!(nav(&mut launcher).await, ("accept".into(), "press".into()));

    // A keyboard-profile app: InputPlumber stays in ALL, the pad becomes keys.
    call(&mut launcher, "apps.launch", json!({ "id": "youtube" }))
        .await
        .unwrap();
    assert_eq!(
        event_named(&mut launcher, "focus_changed").await,
        json!({ "app": "youtube" })
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(fake.mode(0), 2);
    for (event, value) in [
        ("ui_accept", 0.0),
        ("ui_down", 1.0),
        ("ui_down", 0.0),
        ("ui_accept", 1.0),
        ("ui_accept", 0.0),
        ("ui_back", 1.0),
        ("ui_back", 0.0),
        ("ui_context", 1.0),
        ("ui_context", 0.0),
    ] {
        fake.press(event, value).await;
    }
    let mut expected = taps(&[108, 28, 1]);
    expected.extend([(29, true), (38, true), (38, false), (29, false)]);
    eventually("the keys", async || {
        (key_log(&keys_file) == expected).then_some(())
    })
    .await;

    // keys.type from a client goes to the same keyboard.
    let r = call(&mut osk, "keys.type", json!({ "text": "Hé" }))
        .await
        .unwrap();
    assert_eq!(r, json!({ "untypeable": ["é"] }));
    expected.extend([(42, true), (35, true), (35, false), (42, false)]);
    eventually("the typed text", async || {
        (key_log(&keys_file) == expected).then_some(())
    })
    .await;

    // The on-screen keyboard: input_nav to the OSK client only.
    assert_eq!(
        call(&mut osk, "osk.show", json!({})).await.unwrap(),
        json!({ "visible": true })
    );
    for c in [&mut launcher, &mut osk] {
        assert_eq!(
            event_named(c, "osk_visibility").await,
            json!({ "visible": true })
        );
    }
    fake.press("ui_right", 1.0).await;
    fake.press("ui_right", 0.0).await;
    assert_eq!(nav(&mut osk).await, ("right".into(), "press".into()));
    loop {
        match nav(&mut osk).await {
            (a, p) if a == "right" && p == "repeat" => {}
            other => {
                assert_eq!(other, ("right".into(), "release".into()));
                break;
            }
        }
    }
    // No key went to the app meanwhile.
    assert_eq!(key_log(&keys_file), expected);

    // Hidden again: keys for the app, nothing for the OSK.
    call(&mut osk, "osk.hide", json!({})).await.unwrap();
    assert_eq!(
        event_named(&mut osk, "osk_visibility").await,
        json!({ "visible": false })
    );
    fake.press("ui_up", 1.0).await;
    fake.press("ui_up", 0.0).await;
    expected.extend(taps(&[103]));
    eventually("the up key", async || {
        (key_log(&keys_file) == expected).then_some(())
    })
    .await;

    // Guide + Y summons it from the controller; Guide alone is Home (on release).
    fake.press("ui_guide", 1.0).await;
    fake.press("ui_context", 1.0).await;
    fake.press("ui_context", 0.0).await;
    fake.press("ui_guide", 0.0).await;
    assert_eq!(
        event_named(&mut osk, "osk_visibility").await,
        json!({ "visible": true })
    );
    fake.press("ui_guide", 1.0).await;
    fake.press("ui_guide", 0.0).await;
    assert_eq!(
        event_named(&mut osk, "osk_visibility").await,
        json!({ "visible": false })
    );
    assert_eq!(next_event(&mut osk).await.0, "home_pressed");
    assert_eq!(event_named(&mut launcher, "home_pressed").await, json!({}));
    assert_eq!(
        next_event(&mut launcher).await,
        ("focus_changed".into(), json!({ "app": null }))
    );
    // Navigation is the launcher's again.
    fake.press("ui_left", 1.0).await;
    assert_eq!(nav(&mut launcher).await, ("left".into(), "press".into()));
    assert_eq!(key_log(&keys_file), expected);
}
