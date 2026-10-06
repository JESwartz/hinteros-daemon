//! The daemon against a fake InputPlumber (same D-Bus names, interfaces and properties as the
//! real one, see hinteros-docs/spikes/04-input-layer.md) on a private dbus-daemon. Skipped when
//! `dbus-daemon` is not on PATH, unless HINTEROS_REQUIRE_TEST_TOOLS is set.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
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

struct Bus {
    child: Child,
    address: String,
    _dir: tempfile::TempDir,
}

impl Bus {
    fn start() -> Option<Self> {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("bus.conf");
        std::fs::write(
            &config,
            format!(
                r#"<busconfig>
  <type>session</type>
  <listen>unix:path={}/bus</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>"#,
                dir.path().display()
            ),
        )
        .unwrap();
        let mut child = match Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address"])
            .stdout(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                assert!(
                    std::env::var_os("HINTEROS_REQUIRE_TEST_TOOLS").is_none(),
                    "dbus-daemon: {e}"
                );
                eprintln!("dbus-daemon not found, skipping");
                return None;
            }
        };
        let mut address = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        Some(Self {
            child,
            address: address.trim().into(),
            _dir: dir,
        })
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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

    // Guide: home_pressed, the launcher gets focus, mode ALL again.
    fake.press("ui_guide", 1.0).await;
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
