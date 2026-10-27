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

/// What a fake composite device was asked to do: profiles loaded (their `name:`), mouse
/// button events, and how often its `DbusDevices` property was read; the profiles themselves.
#[derive(Clone, Default)]
struct Calls {
    log: Arc<std::sync::Mutex<Vec<String>>>,
    dbus_reads: Arc<AtomicU32>,
    yamls: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Calls {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.log.lock().unwrap())
    }
}

/// How a fake composite device misbehaves, the way InputPlumber 0.81 does on real hardware.
#[derive(Clone)]
struct Faults {
    /// `SendEvent` never answers: InputPlumber 0.81 panics in that D-Bus method
    /// (`blocking_send` inside the tokio runtime, composite_device/client.rs:463), so its task
    /// dies without a reply.
    hang_send_event: Arc<AtomicBool>,
    /// Loading the pointer profile works, and then the device stops answering anything
    /// (methods, `InterceptMode`, `ProfileName`) until `wedged` is cleared. Its D-Bus target
    /// still sends events.
    wedge_after_load: Arc<AtomicBool>,
    wedged: Arc<tokio::sync::watch::Sender<bool>>,
}

impl Default for Faults {
    fn default() -> Self {
        Self {
            hang_send_event: Arc::default(),
            wedge_after_load: Arc::default(),
            wedged: Arc::new(tokio::sync::watch::channel(false).0),
        }
    }
}

impl Faults {
    /// Never returns while the device is wedged.
    async fn gate(&self) {
        let mut rx = self.wedged.subscribe();
        let _ = rx.wait_for(|w| !*w).await;
    }

    fn unwedge(&self) {
        self.wedged.send_replace(false);
    }
}

struct Composite {
    mode: Arc<AtomicU32>,
    name: String,
    targets: Vec<String>,
    profile: Arc<std::sync::Mutex<String>>,
    calls: Calls,
    faults: Faults,
    /// While true, reading `Name` and `SourceDevicePaths` (what the daemon classifies a pad by)
    /// does not return.
    held: Hold,
}

/// Shared by all pads of a [`Fake`] (see [`Composite::held`]).
type Hold = Arc<tokio::sync::watch::Sender<bool>>;

impl Composite {
    fn new(
        mode: Arc<AtomicU32>,
        name: &str,
        targets: Vec<String>,
        calls: Calls,
        held: Hold,
    ) -> Self {
        Self {
            mode,
            name: name.into(),
            targets,
            profile: Arc::new(std::sync::Mutex::new("Default".into())),
            calls,
            faults: Faults::default(),
            held,
        }
    }

    /// Wait while [`Self::held`], for a single `Get` (not the whole object in
    /// `GetManagedObjects`, which the daemon reads to find the pads).
    async fn hold(&self, header: Option<zbus::message::Header<'_>>) {
        if header.is_some_and(|h| h.member().is_some_and(|m| m.as_str() == "Get")) {
            let _ = self.held.subscribe().wait_for(|h| !*h).await;
        }
    }
}

#[zbus::interface(name = "org.shadowblip.Input.CompositeDevice")]
impl Composite {
    #[zbus(property)]
    async fn intercept_mode(&self) -> u32 {
        self.faults.gate().await;
        self.mode.load(Ordering::SeqCst)
    }
    // `&self` (interior mutability): a wedged call must not hold the interface's write lock.
    #[zbus(property)]
    async fn set_intercept_mode(&self, value: u32) {
        self.faults.gate().await;
        self.mode.store(value, Ordering::SeqCst);
    }
    #[zbus(property)]
    async fn name(&self, #[zbus(header)] header: Option<zbus::message::Header<'_>>) -> String {
        self.hold(header).await;
        self.name.clone()
    }
    // Named like InputPlumber's (zbus derives `DbusDevices` from `dbus_devices`).
    #[zbus(property)]
    fn dbus_devices(&self) -> Vec<String> {
        self.calls.dbus_reads.fetch_add(1, Ordering::SeqCst);
        self.targets.clone()
    }
    #[zbus(property)]
    async fn source_device_paths(
        &self,
        #[zbus(header)] header: Option<zbus::message::Header<'_>>,
    ) -> Vec<String> {
        self.hold(header).await;
        vec!["/dev/input/event99".into()]
    }
    #[zbus(property)]
    async fn profile_name(&self) -> String {
        self.faults.gate().await;
        self.profile.lock().unwrap().clone()
    }
    async fn get_profile_yaml(&self) -> String {
        self.faults.gate().await;
        format!(
            "version: 1\nkind: DeviceProfile\nname: {}\nmapping: []\n",
            self.profile.lock().unwrap()
        )
    }
    async fn load_profile_from_yaml(&self, yaml: String) {
        self.faults.gate().await;
        self.calls.yamls.lock().unwrap().push(yaml.clone());
        let name = yaml
            .lines()
            .find_map(|l| l.strip_prefix("name: "))
            .unwrap_or("?")
            .to_string();
        self.calls
            .log
            .lock()
            .unwrap()
            .push(format!("profile {name}"));
        if name == "HinterOS pointer" && self.faults.wedge_after_load.swap(false, Ordering::SeqCst)
        {
            self.faults.wedged.send_replace(true);
        }
        *self.profile.lock().unwrap() = name;
    }
    /// Like InputPlumber 0.81's (`write_send_event` → `write_event`): no Guide intercept (that
    /// only looks at the physical pad's events), but `All` sends everything, and
    /// `GamepadOnly` gamepad events, to the D-Bus target, i.e. back to the daemon. Gamepad
    /// events are logged with where they went.
    async fn send_event(
        &self,
        event: String,
        value: zbus::zvariant::Value<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) {
        self.faults.gate().await;
        let pressed = matches!(value, zbus::zvariant::Value::Bool(true));
        let mut entry = format!("{event} {pressed}");
        if event.starts_with("Gamepad:") {
            let mode = self.mode.load(Ordering::SeqCst);
            if mode == 2 || mode == 3 {
                entry.push_str(" → D-Bus");
                let name = match event.as_str() {
                    "Gamepad:Button:Guide" => "ui_guide",
                    _ => "ui_other",
                };
                for target in &self.targets {
                    let value = if pressed { 1.0 } else { 0.0 };
                    conn.emit_signal(
                        None::<BusName<'_>>,
                        target.as_str(),
                        "org.shadowblip.Input.DBusDevice",
                        "InputEvent",
                        &(name, value),
                    )
                    .await
                    .unwrap();
                }
            } else {
                entry.push_str(" → pad");
            }
        }
        self.calls.log.lock().unwrap().push(entry);
        if self.faults.hang_send_event.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
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
    /// Device 0's.
    calls: Calls,
    /// Device 0's.
    faults: Faults,
    /// Device 0's loaded profile.
    profile: Arc<std::sync::Mutex<String>>,
    /// Every pad's classify gate ([`Composite::held`]).
    held: Hold,
}

impl Fake {
    async fn start(address: &str) -> Self {
        Self::start_named(address, "Generic Gamepad").await
    }

    async fn start_named(address: &str, name: &str) -> Self {
        Self::start_full(address, name, true).await
    }

    /// `with_manager = false`: the bus name and the pads, but no Manager object yet
    /// ([`Self::export_manager`]), as InputPlumber looks for a moment after it starts.
    async fn start_full(address: &str, name: &str, with_manager: bool) -> Self {
        let manage_all = Arc::new(AtomicBool::new(false));
        let held = Hold::default();
        let calls = Calls::default();
        let mode = Arc::new(AtomicU32::new(0));
        let composite = Composite::new(
            mode.clone(),
            name,
            vec![TARGET.into()],
            calls.clone(),
            held.clone(),
        );
        let (faults, profile) = (composite.faults.clone(), composite.profile.clone());
        let conn = zbus::connection::Builder::address(address)
            .unwrap()
            .serve_at(ROOT, zbus::fdo::ObjectManager)
            .unwrap()
            .serve_at(device(0), composite)
            .unwrap()
            .serve_at(TARGET, DBusDevice)
            .unwrap()
            .name(SERVICE)
            .unwrap()
            .build()
            .await
            .unwrap();
        let fake = Self {
            conn,
            manage_all,
            modes: vec![mode],
            calls,
            faults,
            profile,
            held,
        };
        if with_manager {
            fake.export_manager().await;
        }
        fake
    }

    async fn export_manager(&self) {
        self.conn
            .object_server()
            .at(MANAGER, Manager(self.manage_all.clone()))
            .await
            .unwrap();
    }

    fn profile(&self) -> String {
        self.profile.lock().unwrap().clone()
    }

    /// Unplug pad `n` (its object vanishes; InputPlumber 0.81 also drops a composite device
    /// whose sources went away).
    async fn remove_device(&self, n: usize) {
        assert!(
            self.conn
                .object_server()
                .remove::<Composite, _>(device(n as u32))
                .await
                .unwrap()
        );
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
            .at(
                path,
                Composite::new(
                    mode.clone(),
                    "Generic Gamepad",
                    vec![],
                    Calls::default(),
                    self.held.clone(),
                ),
            )
            .await
            .unwrap();
        self.modes.push(mode);
    }

    /// Plug in a named pad with its own D-Bus target.
    async fn add_pad(&mut self, name: &str, target: &str) {
        let mode = Arc::new(AtomicU32::new(0));
        let path = device(self.modes.len() as u32);
        self.conn
            .object_server()
            .at(target.to_string(), DBusDevice)
            .await
            .unwrap();
        self.conn
            .object_server()
            .at(
                path,
                Composite::new(
                    mode.clone(),
                    name,
                    vec![target.into()],
                    Calls::default(),
                    self.held.clone(),
                ),
            )
            .await
            .unwrap();
        self.modes.push(mode);
    }

    async fn press(&self, event: &str, value: f64) {
        self.press_on(TARGET, event, value).await;
    }

    async fn press_on(&self, target: &str, event: &str, value: f64) {
        self.conn
            .emit_signal(
                None::<BusName<'_>>,
                target,
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

/// A pad that appears (hotplug, or InputPlumber restarted) starts in InputPlumber's default
/// mode, NONE: all its input goes to its virtual pad, which sandboxed apps with the `gamepad`
/// permission can read, also while the launcher asks for a PIN. The daemon sets the mode before
/// it classifies the pad (several polkit-checked reads on real hardware). Here classifying does
/// not end until the test lets it. (Two pads that are there when the daemon finds InputPlumber:
/// a pad plugged in later takes the same path, but the fake cannot add one while a read is
/// held, as zbus's object server is locked by the read.)
#[tokio::test(flavor = "multi_thread")]
async fn new_pad_leaves_default_mode_before_classifying() {
    let Some(bus) = Bus::start() else { return };
    let mut fake = Fake::start(&bus.address).await;
    fake.add_device().await;
    fake.held.send_replace(true);
    let address = bus.address.clone();
    let d = Daemon::start_with(|dir| {
        let config = dir.join("session.toml");
        // No timeout during the test: the held reads must not simply fail.
        std::fs::write(&config, "[inputplumber]\ncall_timeout_ms = 60000\n").unwrap();
        vec![
            "--config".into(),
            config.into(),
            "--inputplumber".into(),
            "--inputplumber-bus".into(),
            address.into(),
        ]
    });
    // Launcher focused: ALL, while the pads are still being classified.
    ready(&fake, 0, 2).await;
    ready(&fake, 1, 2).await;

    // Classified: input reaches the launcher.
    let mut c = d.client().await;
    call(&mut c, "subscribe", json!({ "events": ["input_nav"] }))
        .await
        .unwrap();
    fake.held.send_replace(false);
    eventually("the pads classified", async || {
        d.log()
            .matches("ui_action/ui_context labels")
            .nth(1)
            .map(|_| ())
    })
    .await;
    fake.press("ui_accept", 1.0).await;
    assert_eq!(nav(&mut c).await, ("accept".into(), "press".into()));
    assert_eq!((fake.mode(0), fake.mode(1)), (2, 2));
}

/// The key events written by `--fake-keyboard`, as `(code, down)`.
/// Only complete lines: the daemon may be writing one right now.
fn key_log(path: &std::path::Path) -> Vec<(u16, bool)> {
    let log = std::fs::read_to_string(path).unwrap_or_default();
    let complete = log.rfind('\n').map_or("", |end| &log[..end]);
    complete
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
    // The right stick becomes a pointer: our profile, then GAMEPAD_ONLY (3).
    assert_eq!(fake.mode(0), 3);
    assert_eq!(fake.calls.take(), ["profile HinterOS pointer"]);
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

    // Guide with another button (Xbox Y arriving as ui_action) is no chord, and the journal
    // says so with the raw name.
    fake.press("ui_guide", 1.0).await;
    fake.press("ui_action", 1.0).await;
    fake.press("ui_action", 0.0).await;
    fake.press("ui_guide", 0.0).await;
    event_named(&mut launcher, "home_pressed").await;
    let log = d.log();
    for line in [
        "chord guide+context → on-screen keyboard",
        "guide released after the chord: no Home",
        "guide released alone → Home",
        "guide+secondary → ignored (not a chord) button=ui_action",
        "guide released (no chord) → Home buttons=[\"ui_action\"]",
    ] {
        assert!(log.contains(line), "{line:?} not in the log");
    }
    assert!(log.contains("button=ui_context"));
}

#[tokio::test]
async fn xbox_pad_labels_are_swapped_per_device() {
    let Some(bus) = Bus::start() else { return };
    let address = bus.address.clone();
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
        json!({ "events": ["input_nav", "osk_visibility", "home_pressed"] }),
    )
    .await
    .unwrap();
    let mut fake = Fake::start_named(&bus.address, "Microsoft X-Box One pad").await;
    ready(&fake, 0, 2).await;
    let ds_target = format!("{ROOT}/devices/target/dbus1");
    fake.add_pad(
        "Sony Interactive Entertainment DualSense Wireless Controller",
        &ds_target,
    )
    .await;
    ready(&fake, 1, 2).await;

    // The Xbox pad: physical Y (ui_action) is Context, physical X (ui_context) is Secondary.
    for (raw, action) in [("ui_action", "context"), ("ui_context", "secondary")] {
        fake.press(raw, 1.0).await;
        assert_eq!(nav(&mut c).await, (action.into(), "press".into()));
        fake.press(raw, 0.0).await;
        assert_eq!(nav(&mut c).await, (action.into(), "release".into()));
    }
    // The DualSense is not swapped: triangle arrives as ui_context and stays Context.
    fake.press_on(&ds_target, "ui_action", 1.0).await;
    assert_eq!(nav(&mut c).await, ("secondary".into(), "press".into()));
    fake.press_on(&ds_target, "ui_action", 0.0).await;
    assert_eq!(nav(&mut c).await, ("secondary".into(), "release".into()));
    fake.press_on(&ds_target, "ui_context", 1.0).await;
    assert_eq!(nav(&mut c).await, ("context".into(), "press".into()));
    fake.press_on(&ds_target, "ui_context", 0.0).await;
    assert_eq!(nav(&mut c).await, ("context".into(), "release".into()));

    // Guide + physical Y on the Xbox pad (ui_action) summons the on-screen keyboard.
    fake.press("ui_guide", 1.0).await;
    fake.press("ui_action", 1.0).await;
    fake.press("ui_action", 0.0).await;
    fake.press("ui_guide", 0.0).await;
    assert_eq!(
        event_named(&mut c, "osk_visibility").await,
        json!({ "visible": true })
    );
    // The target-to-pad lookup is done once, not per event (each read is a polkit check in
    // InputPlumber): a burst of input reads the property no more.
    call(&mut c, "osk.hide", json!({})).await.unwrap();
    assert_eq!(
        event_named(&mut c, "osk_visibility").await,
        json!({ "visible": false })
    );
    let reads = fake.calls.dbus_reads.load(Ordering::SeqCst);
    assert!((1..=4).contains(&reads), "{reads} DbusDevices reads");
    for _ in 0..50 {
        fake.press("ui_l1", 1.0).await;
        fake.press("ui_l1", 0.0).await;
    }
    for _ in 0..100 {
        nav(&mut c).await;
    }
    assert_eq!(fake.calls.dbus_reads.load(Ordering::SeqCst), reads);
    let log = d.log();
    assert!(
        log.contains(
            "Xbox layout: swapping ui_action/ui_context labels for Microsoft X-Box One pad"
        ),
        "{log}"
    );
    assert!(
        !log.contains("Xbox layout: swapping ui_action/ui_context labels for Sony"),
        "{log}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn text_field_accept_back_and_pointer() {
    let Some(bus) = Bus::start() else { return };
    let address = bus.address.clone();
    let keys_file = Arc::new(std::sync::Mutex::new(std::path::PathBuf::new()));
    let kf = keys_file.clone();
    let d = Daemon::start_with(|dir| {
        // "Firefox": runs the native-messaging host like Firefox does and reports a focused
        // text field (32-bit native-endian length 16, then the JSON).
        let host = dir.join("hinteros-osk-native-host");
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_hinteros-session"), &host).unwrap();
        script(
            &dir.join("firefox"),
            &format!(
                r#"{{ printf '\020\000\000\000{{"focus":"text"}}'; sleep 30; }} |
  "{host}" "{dir}/hinteros_osk.json" hinteros-osk@hinteros > /dev/null"#,
                host = host.display(),
                dir = dir.display(),
            ),
        );
        let config = dir.join("session.toml");
        std::fs::write(
            &config,
            format!(
                r#"
[keys]
key_delay_ms = 0
[[apps]]
id = "youtube"
name = "YouTube"
kind = "web"
command = ["{}"]
input = "keyboard"
"#,
                dir.join("firefox").display()
            ),
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
    let mut c = d.client().await;
    call(
        &mut c,
        "subscribe",
        json!({ "events": ["osk_visibility", "focus_changed"] }),
    )
    .await
    .unwrap();
    call(&mut c, "apps.launch", json!({ "id": "youtube" }))
        .await
        .unwrap();
    ready(&fake, 0, 3).await;
    assert_eq!(fake.calls.take(), ["profile HinterOS pointer"]);
    eventually("the text field report", async || {
        d.log().contains("focus=Text").then_some(())
    })
    .await;

    // A on the focused field: the on-screen keyboard, and no Enter.
    fake.press("ui_accept", 1.0).await;
    fake.press("ui_accept", 0.0).await;
    assert_eq!(
        event_named(&mut c, "osk_visibility").await,
        json!({ "visible": true })
    );
    ready(&fake, 0, 2).await;
    // B closes it (and is no key either).
    fake.press("ui_back", 1.0).await;
    fake.press("ui_back", 0.0).await;
    assert_eq!(
        event_named(&mut c, "osk_visibility").await,
        json!({ "visible": false })
    );
    ready(&fake, 0, 3).await;

    // The right stick moved: RT is the left click (held: a drag), A still never clicks.
    fake.press("ui_quick2", 1.0).await;
    fake.press("ui_quick2", 0.0).await;
    fake.press("ui_r2", 1.0).await;
    fake.press("ui_quick2", 1.0).await;
    fake.press("ui_quick2", 0.0).await;
    fake.press("ui_r2", 0.0).await;
    eventually("the click", async || {
        let calls = fake.calls.log.lock().unwrap().clone();
        (calls == ["Mouse:Button:Left true", "Mouse:Button:Left false"]).then_some(())
    })
    .await;
    fake.calls.take();
    // The D-pad is still arrow keys.
    fake.press("ui_down", 1.0).await;
    fake.press("ui_down", 0.0).await;
    eventually("the down key", async || {
        (key_log(&keys_file) == taps(&[108])).then_some(())
    })
    .await;

    // Home: the stock profile back, then ALL.
    fake.press("ui_guide", 1.0).await;
    fake.press("ui_guide", 0.0).await;
    ready(&fake, 0, 2).await;
    eventually("the profile restored", async || {
        let calls = fake.calls.log.lock().unwrap().clone();
        (calls == ["profile Default"]).then_some(())
    })
    .await;
    assert_eq!(key_log(&keys_file), taps(&[108]), "never an Enter");
}

/// Writes a `session.toml` with a keyboard-profile "YouTube" (a `sleep`) and fast InputPlumber
/// timeouts, and returns the daemon's arguments (with `--fake-keyboard <dir>/keys.log`).
fn youtube_daemon(dir: &std::path::Path, bus: &str) -> Vec<std::ffi::OsString> {
    let config = dir.join("session.toml");
    std::fs::write(
        &config,
        r#"
[inputplumber]
call_timeout_ms = 300
watchdog_ms = 300
[keys]
key_delay_ms = 0
[[apps]]
id = "youtube"
name = "YouTube"
kind = "web"
command = ["sleep", "60"]
input = "keyboard"
"#,
    )
    .unwrap();
    vec![
        "--config".into(),
        config.into(),
        "--inputplumber".into(),
        "--inputplumber-bus".into(),
        bus.into(),
        "--fake-keyboard".into(),
        dir.join("keys.log").into(),
    ]
}

/// The watchdog test. InputPlumber 0.81 never answers `SendEvent` (it panics in it): the click
/// used to wait for that answer in the loop that also forwards events, so after the first RT
/// in Firefox nothing reached the daemon again (Guide no longer went Home, the pad stayed in
/// GAMEPAD_ONLY with the pointer profile). Now hung calls time out off the input path, and a
/// pad that stops answering (here: right after the profile load) is re-checked and put right
/// once it answers again; a pad that vanishes and comes back gets the current routing.
#[tokio::test(flavor = "multi_thread")]
async fn hung_inputplumber_never_breaks_guide() {
    let Some(bus) = Bus::start() else { return };
    let address = bus.address.clone();
    let d = Daemon::start_with(|dir| youtube_daemon(dir, &address));
    let keys_file = d.dir.path().join("keys.log");
    let mut fake = Fake::start(&bus.address).await;
    fake.faults.hang_send_event.store(true, Ordering::SeqCst);
    ready(&fake, 0, 2).await;
    let mut c = d.client().await;
    call(
        &mut c,
        "subscribe",
        json!({ "events": ["home_pressed", "focus_changed"] }),
    )
    .await
    .unwrap();

    // 1. SendEvent hangs (the real-hardware regression).
    call(&mut c, "apps.launch", json!({ "id": "youtube" }))
        .await
        .unwrap();
    ready(&fake, 0, 3).await;
    assert_eq!(fake.profile(), "HinterOS pointer");
    fake.press("ui_quick2", 1.0).await;
    fake.press("ui_r2", 1.0).await;
    fake.press("ui_r2", 0.0).await;
    eventually("the click that never returns", async || {
        let calls = fake.calls.log.lock().unwrap().clone();
        calls
            .contains(&"Mouse:Button:Left true".to_string())
            .then_some(())
    })
    .await;
    // Input still flows: arrow keys and A (Enter) reach the app...
    fake.press("ui_down", 1.0).await;
    fake.press("ui_down", 0.0).await;
    fake.press("ui_accept", 1.0).await;
    fake.press("ui_accept", 0.0).await;
    eventually("down and enter", async || {
        (key_log(&keys_file) == taps(&[108, 28])).then_some(())
    })
    .await;
    // ...and Guide goes Home: ALL, the stock profile back.
    fake.press("ui_guide", 1.0).await;
    fake.press("ui_guide", 0.0).await;
    assert_eq!(event_named(&mut c, "home_pressed").await, json!({}));
    ready(&fake, 0, 2).await;
    eventually("the profile restored", async || {
        (fake.profile() == "Default").then_some(())
    })
    .await;
    assert!(d.log().contains("mouse button"), "{}", d.log());

    // 2. The pad stops answering right after the pointer profile is loaded.
    fake.faults.hang_send_event.store(false, Ordering::SeqCst);
    fake.faults.wedge_after_load.store(true, Ordering::SeqCst);
    call(&mut c, "apps.launch", json!({ "id": "youtube" }))
        .await
        .unwrap();
    eventually("the pad wedged", async || {
        (*fake.faults.wedged.borrow() && fake.profile() == "HinterOS pointer").then_some(())
    })
    .await;
    eventually("the daemon noticing", async || {
        d.log().contains("stopped answering").then_some(())
    })
    .await;
    // Guide still goes Home while it is wedged.
    fake.press("ui_guide", 1.0).await;
    fake.press("ui_guide", 0.0).await;
    assert_eq!(event_named(&mut c, "home_pressed").await, json!({}));
    // It answers again: the watchdog puts back what the launcher needs.
    fake.faults.unwedge();
    eventually("ALL and the stock profile after recovery", async || {
        (fake.mode(0) == 2 && fake.profile() == "Default").then_some(())
    })
    .await;
    assert!(d.log().contains("answers again"), "{}", d.log());
    fake.press("ui_left", 1.0).await;
    fake.press("ui_left", 0.0).await;

    // 3. Somebody else changes the mode behind our back: the watchdog sets ours again.
    fake.modes[0].store(1, Ordering::SeqCst);
    ready(&fake, 0, 2).await;

    // 4. The pad's object vanishes and a new one appears while the app has focus: it gets the
    //    app's routing.
    call(&mut c, "apps.launch", json!({ "id": "youtube" }))
        .await
        .unwrap();
    ready(&fake, 0, 3).await;
    fake.remove_device(0).await;
    fake.add_device().await;
    ready(&fake, 1, 3).await;
    fake.press("ui_guide", 1.0).await;
    fake.press("ui_guide", 0.0).await;
    assert_eq!(event_named(&mut c, "home_pressed").await, json!({}));
    ready(&fake, 1, 2).await;
}

/// InputPlumber restarting (a system switch): its bus name appears before its Manager object,
/// so the first `ManageAllDevices` fails with UnknownObject. The daemon keeps trying until it
/// works, and its watchdog puts `ManageAllDevices` back when InputPlumber loses it.
#[tokio::test(flavor = "multi_thread")]
async fn inputplumber_name_before_its_manager() {
    let Some(bus) = Bus::start() else { return };
    let address = bus.address.clone();
    let d = Daemon::start_with(|dir| youtube_daemon(dir, &address));
    let mut c = d.client().await;

    let fake = Fake::start_full(&bus.address, "Generic Gamepad", false).await;
    // The pad is set up (mode ALL) only through the manager: nothing yet.
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    assert!(!fake.manage_all.load(Ordering::SeqCst));
    fake.export_manager().await;
    ready(&fake, 0, 2).await;

    // InputPlumber forgets ManageAllDevices: the watchdog sets it again.
    fake.manage_all.store(false, Ordering::SeqCst);
    eventually("ManageAllDevices again", async || {
        fake.manage_all.load(Ordering::SeqCst).then_some(())
    })
    .await;

    // Input still arrives.
    call(&mut c, "subscribe", json!({ "events": ["input_nav"] }))
        .await
        .unwrap();
    fake.press("ui_accept", 1.0).await;
    assert_eq!(nav(&mut c).await, ("accept".into(), "press".into()));
    assert!(
        d.log().contains("InputPlumber set up after retrying"),
        "{}",
        d.log()
    );
}

/// A long Guide press while Steam is in front: the daemon taps Guide on the virtual pad for
/// Steam. InputPlumber switched the pad to ALL on the user's Guide press; a `SendEvent` in ALL
/// would come straight back to us as `ui_guide` (a short press: Home). So the pad goes back to
/// PASS first, then the tap: it reaches the pad, and nothing comes back to the daemon.
#[tokio::test(flavor = "multi_thread")]
async fn guide_tap_for_steam_reaches_the_pad_not_us() {
    use hinteros_session::input::inputplumber::{self, PointerProfile, Settings};
    use hinteros_session::input::{InputControl, InterceptMode, LabelSwap};

    let Some(bus) = Bus::start() else { return };
    let fake = Fake::start(&bus.address).await;
    let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let mut input = inputplumber::spawn(
        Settings {
            bus: Some(bus.address.clone()),
            initial: InterceptMode::Pass,
            swap: LabelSwap::Off,
            profile: PointerProfile {
                speed_pps: 600,
                curve: 2.5,
                activity_deadzone: 0.1,
            },
            call_timeout: std::time::Duration::from_secs(5),
            // No watchdog in the way: the tap itself must set the mode.
            watchdog: std::time::Duration::from_secs(600),
        },
        tx,
    );
    // A Steam game in front: PASS.
    ready(&fake, 0, 1).await;

    // The user's Guide press: InputPlumber switches to ALL by itself. The fake behaves like
    // InputPlumber there: a Guide sent in ALL comes back as ui_guide.
    fake.modes[0].store(2, Ordering::SeqCst);
    let conn = zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    let proxy = zbus::Proxy::new(
        &conn,
        SERVICE,
        device(0),
        "org.shadowblip.Input.CompositeDevice",
    )
    .await
    .unwrap();
    proxy
        .call::<_, _, ()>(
            "SendEvent",
            &("Gamepad:Button:Guide", zbus::zvariant::Value::Bool(true)),
        )
        .await
        .unwrap();
    let echo = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((echo.event.as_str(), echo.value), ("ui_guide", 1.0));
    assert_eq!(fake.calls.take(), ["Gamepad:Button:Guide true → D-Bus"]);

    // A tap that would loop (the routing is ALL) is not sent at all.
    input.guide_tap(TARGET, InterceptMode::All);
    // The daemon's tap: PASS first, then press and release, both to the pad.
    input.guide_tap(TARGET, InterceptMode::Pass);
    eventually("the Guide tap", async || {
        let calls = fake.calls.log.lock().unwrap().clone();
        (calls.len() == 2).then_some(())
    })
    .await;
    assert_eq!(
        fake.calls.take(),
        [
            "Gamepad:Button:Guide true → pad",
            "Gamepad:Button:Guide false → pad"
        ]
    );
    assert_eq!(fake.mode(0), 1, "the pad is left in PASS, for the game");
    // Nothing came back to the daemon.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(events.try_recv().is_err(), "the tap came back as input");
}

/// In PASS (a game in front) InputPlumber switches the pad to ALL by itself on a Guide press,
/// so that the release reaches us. The watchdog set PASS back in the middle of the press (seen
/// on the dev box): the release went to the game, and a short press became a hold. Now it
/// leaves that ALL alone for GUIDE_SWITCH_GRACE after a Guide press; an ALL without a press,
/// or one that outlasts the grace, still gets PASS back.
#[tokio::test(flavor = "multi_thread")]
async fn watchdog_leaves_inputplumbers_guide_switch_alone() {
    use hinteros_session::input::inputplumber::{
        self, GUIDE_SWITCH_GRACE, PointerProfile, Settings,
    };
    use hinteros_session::input::{InterceptMode, LabelSwap};
    use std::time::{Duration, Instant};

    let Some(bus) = Bus::start() else { return };
    let fake = Fake::start(&bus.address).await;
    let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let _input = inputplumber::spawn(
        Settings {
            bus: Some(bus.address.clone()),
            initial: InterceptMode::Pass,
            swap: LabelSwap::Off,
            profile: PointerProfile {
                speed_pps: 600,
                curve: 2.5,
                activity_deadzone: 0.1,
            },
            call_timeout: Duration::from_secs(5),
            watchdog: Duration::from_millis(100),
        },
        tx,
    );
    ready(&fake, 0, 1).await;

    // ALL without a Guide press: PASS again at the next check.
    fake.modes[0].store(2, Ordering::SeqCst);
    ready(&fake, 0, 1).await;

    // The user's Guide press: InputPlumber switches to ALL and sends ui_guide.
    fake.modes[0].store(2, Ordering::SeqCst);
    let pressed = Instant::now();
    fake.press("ui_guide", 1.0).await;
    let ev = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((ev.event.as_str(), ev.value), ("ui_guide", 1.0));
    // Several checks within the grace: ALL stays, so the release still comes to us.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        fake.mode(0),
        2,
        "the watchdog undid InputPlumber's Guide switch"
    );
    fake.press("ui_guide", 0.0).await;
    let ev = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((ev.event.as_str(), ev.value), ("ui_guide", 0.0));
    // Nobody set the mode afterwards: once the grace is over, PASS for the game again.
    ready(&fake, 0, 1).await;
    assert!(
        pressed.elapsed() >= GUIDE_SWITCH_GRACE,
        "{:?}",
        pressed.elapsed()
    );
}

/// The profile's pointer settings (session API 0.20): a change while the pointer profile is
/// loaded loads it again with the new speed and sticks; a change while it is not waits for the
/// next load. Unloading puts the profile from before ours back.
#[tokio::test(flavor = "multi_thread")]
async fn pointer_settings_reload_the_loaded_profile() {
    use hinteros_session::input::inputplumber::{self, PointerProfile, Settings};
    use hinteros_session::input::{InputControl, InterceptMode, LabelSwap, PointerTuning};

    let Some(bus) = Bus::start() else { return };
    let fake = Fake::start(&bus.address).await;
    let (tx, _events) = tokio::sync::mpsc::unbounded_channel();
    let mut input = inputplumber::spawn(
        Settings {
            bus: Some(bus.address.clone()),
            initial: InterceptMode::All,
            swap: LabelSwap::Off,
            profile: PointerProfile {
                speed_pps: 600,
                curve: 2.5,
                activity_deadzone: 0.1,
            },
            call_timeout: std::time::Duration::from_secs(5),
            watchdog: std::time::Duration::from_secs(600),
        },
        tx,
    );
    ready(&fake, 0, 2).await;
    let loads = |n: usize| {
        let yamls = fake.calls.yamls.clone();
        async move {
            eventually(&format!("{n} profile loads"), async || {
                let y = yamls.lock().unwrap().clone();
                (y.len() >= n).then_some(y)
            })
            .await
        }
    };

    // Not loaded: a change is only remembered.
    let fast = PointerTuning {
        speed: 200,
        swap_sticks: false,
    };
    input.set_pointer_tuning(fast);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(fake.calls.yamls.lock().unwrap().is_empty());
    input.set_pointer(true);
    input.set_mode(InterceptMode::GamepadOnly);
    let y = loads(1).await;
    assert!(y[0].contains("speed_pps: 1200\n"), "{}", y[0]);
    assert_eq!(fake.profile(), "HinterOS pointer");

    // Loaded: the new settings load at once.
    input.set_pointer_tuning(PointerTuning {
        speed: 50,
        swap_sticks: true,
    });
    let y = loads(2).await;
    assert!(y[1].contains("speed_pps: 300\n"), "{}", y[1]);
    assert!(y[1].contains("button: DPadUp"), "{}", y[1]);
    // The same settings again: nothing to do.
    input.set_pointer_tuning(PointerTuning {
        speed: 50,
        swap_sticks: true,
    });
    input.set_pointer(false);
    let y = loads(3).await;
    assert_eq!(y.len(), 3);
    assert!(
        y[2].contains("name: Default"),
        "the profile from before ours"
    );
    assert_eq!(fake.profile(), "Default");
}
