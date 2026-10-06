//! Bluetooth against a fake BlueZ on a private bus: adapter, devices with their properties,
//! discovery, the agent manager, and pairing that calls the daemon's agent the way BlueZ does
//! (numeric comparison, passkey display). Skipped when `dbus-daemon` is not on PATH, unless
//! HINTEROS_REQUIRE_TEST_TOOLS is set.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hinteros_api::client::Client;
use serde_json::{Value, json};
use zbus::names::BusName;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, Value as ZValue};
use zbus::{Connection, ObjectServer};

mod common;

use common::*;

const ADAPTER: &str = "/org/bluez/hci0";

fn dev_path(address: &str) -> String {
    format!("{ADAPTER}/dev_{}", address.replace(':', "_"))
}

#[derive(Clone)]
struct Dev {
    address: &'static str,
    name: Option<&'static str>,
    icon: Option<&'static str>,
    class: u32,
    paired: bool,
    trusted: bool,
    connected: bool,
    rssi: Option<i16>,
    /// What Pair() does: "just_works", "confirm", "passkey", "refused".
    pairing: &'static str,
    battery: Option<u8>,
    /// Connects by itself after pairing (controllers do).
    auto_connect: bool,
}

#[derive(Default)]
struct Model {
    powered: bool,
    discovering: bool,
    devices: Vec<Dev>,
    discoveries_started: u32,
    discoveries_stopped: u32,
    pair_calls: Vec<String>,
    connect_calls: Vec<String>,
    removed: Vec<String>,
    /// The agent as registered: sender, path, capability, and whether it is the default.
    agent: Option<(String, String, String, bool)>,
    /// What the agent answered during the last pairing.
    agent_answer: Option<String>,
}

type Shared = Arc<Mutex<Model>>;

async fn poke(conn: &Connection) {
    conn.emit_signal(
        None::<BusName<'_>>,
        ADAPTER,
        "org.freedesktop.DBus.Properties",
        "PropertiesChanged",
        &(
            "org.bluez.Adapter1",
            HashMap::<String, ZValue<'_>>::new(),
            Vec::<String>::new(),
        ),
    )
    .await
    .unwrap();
}

fn find<'a>(m: &'a mut Model, address: &str) -> &'a mut Dev {
    m.devices.iter_mut().find(|d| d.address == address).unwrap()
}

struct AgentManager(Shared);

#[zbus::interface(name = "org.bluez.AgentManager1")]
impl AgentManager {
    fn register_agent(
        &self,
        agent: ObjectPath<'_>,
        capability: String,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        let sender = header.sender().unwrap().to_string();
        self.0.lock().unwrap().agent = Some((sender, agent.to_string(), capability, false));
    }
    fn request_default_agent(&self, agent: ObjectPath<'_>) {
        let mut m = self.0.lock().unwrap();
        let a = m.agent.as_mut().unwrap();
        assert_eq!(a.1, agent.as_str());
        a.3 = true;
    }
}

struct Adapter(Shared);

#[zbus::interface(name = "org.bluez.Adapter1")]
impl Adapter {
    #[zbus(property)]
    fn address(&self) -> String {
        "00:11:22:33:44:55".into()
    }
    #[zbus(property)]
    fn powered(&self) -> bool {
        self.0.lock().unwrap().powered
    }
    #[zbus(property)]
    fn set_powered(&mut self, on: bool) {
        self.0.lock().unwrap().powered = on;
    }
    #[zbus(property)]
    fn discovering(&self) -> bool {
        self.0.lock().unwrap().discovering
    }

    async fn start_discovery(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> Result<(), BluezError> {
        let new = {
            let mut m = self.0.lock().unwrap();
            if m.discovering {
                return Err(BluezError::InProgress(
                    "Operation already in progress".into(),
                ));
            }
            m.discovering = true;
            m.discoveries_started += 1;
            // Scanning finds a headset (it has a name by now).
            let found = Dev {
                address: "AA:BB:CC:DD:EE:02",
                name: Some("Headphones"),
                icon: Some("audio-headphones"),
                class: 0x240418,
                paired: false,
                trusted: false,
                connected: false,
                rssi: Some(-62),
                pairing: "just_works",
                battery: None,
                auto_connect: false,
            };
            if m.devices.iter().any(|d| d.address == found.address) {
                None
            } else {
                m.devices.push(found.clone());
                Some(found)
            }
        };
        if let Some(d) = new {
            server
                .at(dev_path(d.address), Device(self.0.clone(), d.address))
                .await?;
        }
        poke(conn).await;
        Ok(())
    }

    async fn stop_discovery(&self, #[zbus(connection)] conn: &Connection) -> zbus::fdo::Result<()> {
        {
            let mut m = self.0.lock().unwrap();
            if !m.discovering {
                return Err(zbus::fdo::Error::Failed("No discovery started".into()));
            }
            m.discovering = false;
            m.discoveries_stopped += 1;
        }
        poke(conn).await;
        Ok(())
    }

    async fn remove_device(
        &self,
        device: ObjectPath<'_>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> zbus::fdo::Result<()> {
        let address = device
            .rsplit('/')
            .next()
            .unwrap()
            .trim_start_matches("dev_")
            .replace('_', ":");
        server.remove::<Device, _>(device.as_str()).await?;
        let _ = server.remove::<Battery, _>(device.as_str()).await;
        let mut m = self.0.lock().unwrap();
        m.devices.retain(|d| d.address != address);
        m.removed.push(address);
        Ok(())
    }
}

struct Device(Shared, &'static str);

impl Device {
    fn get<T>(&self, f: impl FnOnce(&mut Dev) -> T) -> T {
        let mut m = self.0.lock().unwrap();
        f(find(&mut m, self.1))
    }
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum BluezError {
    #[zbus(error)]
    ZBus(zbus::Error),
    AuthenticationFailed(String),
    AuthenticationRejected(String),
    Failed(String),
    InProgress(String),
}

#[zbus::interface(name = "org.bluez.Device1")]
impl Device {
    #[zbus(property)]
    fn address(&self) -> String {
        self.1.into()
    }
    #[zbus(property)]
    fn name(&self) -> zbus::fdo::Result<String> {
        self.get(|d| d.name.map(String::from))
            .ok_or(zbus::fdo::Error::UnknownProperty("Name".into()))
    }
    #[zbus(property)]
    fn alias(&self) -> String {
        self.get(|d| d.name.unwrap_or(d.address).to_owned())
    }
    #[zbus(property)]
    fn icon(&self) -> zbus::fdo::Result<String> {
        self.get(|d| d.icon.map(String::from))
            .ok_or(zbus::fdo::Error::UnknownProperty("Icon".into()))
    }
    #[zbus(property)]
    fn class(&self) -> u32 {
        self.get(|d| d.class)
    }
    #[zbus(property)]
    fn paired(&self) -> bool {
        self.get(|d| d.paired)
    }
    #[zbus(property)]
    fn trusted(&self) -> bool {
        self.get(|d| d.trusted)
    }
    #[zbus(property)]
    fn set_trusted(&mut self, on: bool) {
        self.get(|d| d.trusted = on);
    }
    #[zbus(property)]
    fn connected(&self) -> bool {
        self.get(|d| d.connected)
    }
    #[zbus(property, name = "RSSI")]
    fn rssi(&self) -> zbus::fdo::Result<i16> {
        self.get(|d| d.rssi)
            .ok_or(zbus::fdo::Error::UnknownProperty("RSSI".into()))
    }

    async fn pair(&self, #[zbus(connection)] conn: &Connection) -> Result<(), BluezError> {
        let (mode, agent) = {
            let mut m = self.0.lock().unwrap();
            m.pair_calls.push(self.1.into());
            (find(&mut m, self.1).pairing, m.agent.clone())
        };
        let path = ObjectPath::try_from(dev_path(self.1)).unwrap();
        let ask = |method: &'static str, body: (ObjectPath<'static>, u32)| {
            let agent = agent.clone();
            let conn = conn.clone();
            async move {
                let (sender, apath, _, _) = agent.expect("no agent registered");
                let p = zbus::Proxy::new(&conn, sender, apath, "org.bluez.Agent1")
                    .await
                    .unwrap();
                p.call::<_, _, ()>(method, &body).await
            }
        };
        let outcome = match mode {
            "just_works" => Ok(()),
            "confirm" => ask("RequestConfirmation", (path.into_owned(), 12345))
                .await
                .map_err(|e| e.to_string()),
            "passkey" => {
                // DisplayPasskey takes (device, passkey, entered u16); send it by hand.
                let (sender, apath, _, _) = agent.clone().expect("no agent registered");
                let p = zbus::Proxy::new(conn, sender, apath, "org.bluez.Agent1")
                    .await
                    .unwrap();
                p.call::<_, _, ()>("DisplayPasskey", &(path.into_owned(), 654321u32, 0u16))
                    .await
                    .map_err(|e| e.to_string())
            }
            "legacy" => {
                let (sender, apath, _, _) = agent.clone().expect("no agent registered");
                let p = zbus::Proxy::new(conn, sender, apath, "org.bluez.Agent1")
                    .await
                    .unwrap();
                match p
                    .call::<_, _, String>("RequestPinCode", &(path.into_owned(),))
                    .await
                {
                    Ok(pin) if pin == "0000" => Ok(()),
                    Ok(pin) => Err(format!("unexpected pin {pin}")),
                    Err(e) => Err(e.to_string()),
                }
            }
            _ => {
                return Err(BluezError::AuthenticationFailed(
                    "Authentication Failed".into(),
                ));
            }
        };
        self.0.lock().unwrap().agent_answer = Some(format!("{outcome:?}"));
        if outcome.is_err() {
            return Err(BluezError::AuthenticationRejected(
                "Authentication Rejected".into(),
            ));
        }
        self.get(|d| {
            d.paired = true;
            if d.auto_connect {
                d.connected = true;
            }
        });
        poke(conn).await;
        Ok(())
    }

    async fn connect(&self, #[zbus(connection)] conn: &Connection) -> Result<(), BluezError> {
        {
            let mut m = self.0.lock().unwrap();
            m.connect_calls.push(self.1.into());
            let d = find(&mut m, self.1);
            if !d.paired {
                return Err(BluezError::Failed("Not paired".into()));
            }
            d.connected = true;
        }
        poke(conn).await;
        Ok(())
    }

    async fn disconnect(&self, #[zbus(connection)] conn: &Connection) {
        self.get(|d| d.connected = false);
        poke(conn).await;
    }

    fn cancel_pairing(&self) {}
}

struct Battery(Shared, &'static str);

#[zbus::interface(name = "org.bluez.Battery1")]
impl Battery {
    #[zbus(property)]
    fn percentage(&self) -> u8 {
        let m = self.0.lock().unwrap();
        m.devices
            .iter()
            .find(|d| d.address == self.1)
            .and_then(|d| d.battery)
            .unwrap_or(0)
    }
}

fn pad() -> Dev {
    Dev {
        address: "AA:BB:CC:DD:EE:01",
        name: Some("Xbox Wireless Controller"),
        icon: Some("input-gaming"),
        class: 0x2508,
        paired: false,
        trusted: false,
        connected: false,
        rssi: Some(-50),
        pairing: "just_works",
        battery: None,
        auto_connect: true,
    }
}

struct Fake {
    conn: Connection,
    model: Shared,
}

impl Fake {
    async fn start(bus: &Bus, powered: bool, devices: Vec<Dev>) -> Self {
        let model = Arc::new(Mutex::new(Model {
            powered,
            devices: devices.clone(),
            ..Model::default()
        }));
        let mut b = zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .serve_at("/", zbus::fdo::ObjectManager)
            .unwrap()
            .serve_at("/org/bluez", AgentManager(model.clone()))
            .unwrap()
            .serve_at(ADAPTER, Adapter(model.clone()))
            .unwrap();
        for d in &devices {
            b = b
                .serve_at(dev_path(d.address), Device(model.clone(), d.address))
                .unwrap();
            if d.battery.is_some() {
                b = b
                    .serve_at(dev_path(d.address), Battery(model.clone(), d.address))
                    .unwrap();
            }
        }
        let conn = b.name("org.bluez").unwrap().build().await.unwrap();
        Self { conn, model }
    }
}

fn devices() -> Vec<Dev> {
    vec![
        Dev {
            paired: true,
            trusted: true,
            connected: true,
            battery: Some(80),
            pairing: "just_works",
            ..pad()
        },
        Dev {
            address: "AA:BB:CC:DD:EE:03",
            name: Some("Phone"),
            icon: Some("phone"),
            class: 0x200204,
            paired: false,
            trusted: false,
            connected: false,
            rssi: Some(-70),
            pairing: "confirm",
            battery: None,
            auto_connect: false,
        },
        Dev {
            address: "AA:BB:CC:DD:EE:04",
            name: Some("Keyboard"),
            icon: Some("input-keyboard"),
            class: 0x2540,
            paired: false,
            trusted: false,
            connected: false,
            rssi: Some(-40),
            pairing: "passkey",
            battery: None,
            auto_connect: false,
        },
        Dev {
            address: "AA:BB:CC:DD:EE:05",
            name: None,
            icon: None,
            class: 0,
            paired: false,
            trusted: false,
            connected: false,
            rssi: Some(-90),
            pairing: "refused",
            battery: None,
            auto_connect: false,
        },
        Dev {
            address: "AA:BB:CC:DD:EE:06",
            name: Some("Old Remote"),
            icon: None,
            class: 0,
            paired: false,
            trusted: false,
            connected: false,
            rssi: None,
            pairing: "refused",
            battery: None,
            auto_connect: false,
        },
        Dev {
            address: "AA:BB:CC:DD:EE:07",
            name: Some("Legacy Headset"),
            icon: Some("audio-headset"),
            class: 0x240404,
            paired: false,
            trusted: false,
            connected: false,
            rssi: Some(-55),
            pairing: "legacy",
            battery: None,
            auto_connect: false,
        },
    ]
}

async fn start(powered: bool, devs: Vec<Dev>) -> Option<(Bus, Fake, Daemon, Client)> {
    let bus = Bus::start()?;
    let fake = Fake::start(&bus, powered, devs).await;
    let d = Daemon::start_with(|_| runtime_bus(&bus));
    let mut c = d.client().await;
    // The daemon has seen BlueZ (it answers).
    call(&mut c, "bluetooth.status", json!({})).await.unwrap();
    Some((bus, fake, d, c))
}

fn dev(list: &Value, address: &str) -> Value {
    list["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["address"] == address)
        .unwrap_or_else(|| panic!("no {address} in {list}"))
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn status_devices_and_the_agent() {
    let Some((_bus, fake, _d, mut c)) = start(true, devices()).await else {
        return;
    };
    assert_eq!(
        call(&mut c, "bluetooth.status", json!({})).await.unwrap(),
        json!({"available": true, "powered": true, "scanning": false})
    );
    let list = call(&mut c, "bluetooth.devices", json!({})).await.unwrap();
    let names: Vec<&str> = list["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    // Paired first, then by name; the nameless stranger is left out.
    assert_eq!(
        names,
        [
            "Xbox Wireless Controller",
            "Keyboard",
            "Legacy Headset",
            "Old Remote",
            "Phone"
        ]
    );
    assert_eq!(
        dev(&list, "AA:BB:CC:DD:EE:01"),
        json!({"address": "AA:BB:CC:DD:EE:01", "name": "Xbox Wireless Controller", "kind": "gamepad",
               "paired": true, "connected": true, "trusted": true, "battery": 80, "rssi": -50})
    );
    assert_eq!(dev(&list, "AA:BB:CC:DD:EE:04")["kind"], "keyboard");
    assert_eq!(dev(&list, "AA:BB:CC:DD:EE:07")["kind"], "headset");
    assert_eq!(dev(&list, "AA:BB:CC:DD:EE:03")["kind"], "phone");
    // No icon, class 0: other; no RSSI reported: left out.
    let old = dev(&list, "AA:BB:CC:DD:EE:06");
    assert_eq!(old["kind"], "other");
    assert!(old.get("rssi").is_none() && old.get("battery").is_none());

    // Until the user scans or pairs, the daemon leaves BlueZ's agents alone.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(fake.model.lock().unwrap().agent.is_none());
    call(&mut c, "bluetooth.scan", json!({"enabled": true}))
        .await
        .unwrap();
    call(&mut c, "bluetooth.scan", json!({"enabled": false}))
        .await
        .unwrap();
    // Then its agent is DisplayYesNo (so that numeric comparisons reach the launcher), the default.
    let agent = fake.model.lock().unwrap().agent.clone().unwrap();
    assert_eq!(agent.1, "/org/hinteros/BluetoothAgent");
    assert_eq!(agent.2, "DisplayYesNo");
    assert!(agent.3, "requested as the default agent");
}

#[tokio::test(flavor = "multi_thread")]
async fn scanning_with_a_timeout() {
    let Some((_bus, fake, _d, mut c)) = start(false, devices()).await else {
        return;
    };
    call(
        &mut c,
        "subscribe",
        json!({"events": ["bluetooth_changed"]}),
    )
    .await
    .unwrap();

    // Starting a scan powers the adapter on.
    let status = call(
        &mut c,
        "bluetooth.scan",
        json!({"enabled": true, "timeout_s": 1}),
    )
    .await
    .unwrap();
    assert_eq!(
        status,
        json!({"available": true, "powered": true, "scanning": true})
    );
    assert!(fake.model.lock().unwrap().powered);
    // The headset it finds is announced.
    let ev = loop {
        let ev = event_named(&mut c, "bluetooth_changed").await;
        if ev["devices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["name"] == "Headphones")
        {
            break ev;
        }
    };
    assert_eq!(ev["status"]["scanning"], true);
    let headphones = dev(&json!({"devices": ev["devices"]}), "AA:BB:CC:DD:EE:02");
    assert_eq!(headphones["kind"], "headset");
    assert_eq!(headphones["paired"], false);

    // After the timeout the daemon stops discovery by itself, and says so.
    eventually("discovery to stop", async || {
        (fake.model.lock().unwrap().discoveries_stopped == 1).then_some(())
    })
    .await;
    loop {
        let ev = event_named(&mut c, "bluetooth_changed").await;
        if ev["status"]["scanning"] == false {
            break;
        }
    }

    // Scanning twice in a row is not an error, and an explicit stop ends it at once.
    call(
        &mut c,
        "bluetooth.scan",
        json!({"enabled": true, "timeout_s": 60}),
    )
    .await
    .unwrap();
    call(&mut c, "bluetooth.scan", json!({"enabled": true}))
        .await
        .unwrap();
    let status = call(&mut c, "bluetooth.scan", json!({"enabled": false}))
        .await
        .unwrap();
    assert_eq!(status["scanning"], false);
    assert_eq!(fake.model.lock().unwrap().discoveries_started, 2);
    assert!(!fake.model.lock().unwrap().discovering);
    // Stopping when nothing runs is fine too.
    call(&mut c, "bluetooth.scan", json!({"enabled": false}))
        .await
        .unwrap();
    // A timeout of 0 and one that is too long are refused by the schema's limits in Rust too.
    let e = call(
        &mut c,
        "bluetooth.scan",
        json!({"enabled": true, "timeout_s": 0}),
    )
    .await;
    assert!(e.is_ok(), "clamped, not refused: {e:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_a_controller_trusts_it_and_a_headset_gets_connected() {
    let mut devs = devices();
    devs.push(Dev {
        address: "AA:BB:CC:DD:EE:02",
        name: Some("Headphones"),
        icon: Some("audio-headphones"),
        class: 0x240418,
        paired: false,
        trusted: false,
        connected: false,
        rssi: Some(-62),
        pairing: "just_works",
        battery: None,
        auto_connect: false,
    });
    // A second controller that has not paired yet.
    devs.push(Dev {
        address: "AA:BB:CC:DD:EE:08",
        ..pad()
    });
    let Some((_bus, fake, _d, mut c)) = start(true, devs).await else {
        return;
    };

    let r = call(
        &mut c,
        "bluetooth.pair",
        json!({"address": "AA:BB:CC:DD:EE:08"}),
    )
    .await
    .unwrap();
    assert_eq!(
        (
            r["paired"].clone(),
            r["trusted"].clone(),
            r["connected"].clone()
        ),
        (json!(true), json!(true), json!(true))
    );
    assert_eq!(r["kind"], "gamepad");
    // The pad connected by itself: no Connect() call.
    assert!(fake.model.lock().unwrap().connect_calls.is_empty());

    let r = call(
        &mut c,
        "bluetooth.pair",
        json!({"address": "AA:BB:CC:DD:EE:02"}),
    )
    .await
    .unwrap();
    assert_eq!(
        (
            r["paired"].clone(),
            r["trusted"].clone(),
            r["connected"].clone()
        ),
        (json!(true), json!(true), json!(true))
    );
    assert_eq!(
        fake.model.lock().unwrap().connect_calls,
        ["AA:BB:CC:DD:EE:02"]
    );

    // Pairing an already paired device trusts and connects it without pairing again.
    let before = fake.model.lock().unwrap().pair_calls.len();
    {
        let mut m = fake.model.lock().unwrap();
        let d = find(&mut m, "AA:BB:CC:DD:EE:08");
        d.trusted = false;
        d.connected = false;
    }
    let r = call(
        &mut c,
        "bluetooth.pair",
        json!({"address": "AA:BB:CC:DD:EE:08"}),
    )
    .await
    .unwrap();
    assert_eq!(
        (r["trusted"].clone(), r["connected"].clone()),
        (json!(true), json!(true))
    );
    assert_eq!(fake.model.lock().unwrap().pair_calls.len(), before);

    // A device that refuses.
    let e = call(
        &mut c,
        "bluetooth.pair",
        json!({"address": "AA:BB:CC:DD:EE:06"}),
    )
    .await
    .unwrap_err();
    assert_eq!(e["code"], 4002, "{e}");
    assert_eq!(e["data"]["reason"], "auth_failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn numeric_comparison_goes_to_the_launcher() {
    let Some((_bus, fake, _d, mut c)) = start(true, devices()).await else {
        return;
    };
    call(
        &mut c,
        "subscribe",
        json!({"events": ["bluetooth_confirm"]}),
    )
    .await
    .unwrap();
    let mut pairing = {
        let mut p = _d.client().await;
        tokio::spawn(async move {
            call(
                &mut p,
                "bluetooth.pair",
                json!({"address": "AA:BB:CC:DD:EE:03"}),
            )
            .await
        })
    };
    let ask = event_named(&mut c, "bluetooth_confirm").await;
    assert_eq!(
        ask,
        json!({"address": "AA:BB:CC:DD:EE:03", "name": "Phone", "passkey": "012345", "mode": "confirm"})
    );
    // Nothing else is waiting.
    let e = call(
        &mut c,
        "bluetooth.confirm",
        json!({"address": "AA:BB:CC:DD:EE:04", "accept": true}),
    )
    .await
    .unwrap_err();
    assert_eq!(e["code"], 3010);
    call(
        &mut c,
        "bluetooth.confirm",
        json!({"address": "AA:BB:CC:DD:EE:03", "accept": true}),
    )
    .await
    .unwrap();
    let r = (&mut pairing).await.unwrap().unwrap();
    assert_eq!(r["paired"], true);
    assert_eq!(r["trusted"], true);
    assert!(
        fake.model
            .lock()
            .unwrap()
            .agent_answer
            .as_deref()
            .unwrap()
            .starts_with("Ok")
    );

    // Rejecting fails the pairing.
    {
        let mut m = fake.model.lock().unwrap();
        let d = find(&mut m, "AA:BB:CC:DD:EE:03");
        d.paired = false;
        d.trusted = false;
    }
    let mut pairing = {
        let mut p = _d.client().await;
        tokio::spawn(async move {
            call(
                &mut p,
                "bluetooth.pair",
                json!({"address": "AA:BB:CC:DD:EE:03"}),
            )
            .await
        })
    };
    event_named(&mut c, "bluetooth_confirm").await;
    call(
        &mut c,
        "bluetooth.confirm",
        json!({"address": "AA:BB:CC:DD:EE:03", "accept": false}),
    )
    .await
    .unwrap();
    let e = (&mut pairing).await.unwrap().unwrap_err();
    assert_eq!(e["code"], 4002);
    assert_eq!(e["data"]["reason"], "rejected");
}

#[tokio::test(flavor = "multi_thread")]
async fn passkeys_are_shown_and_legacy_pins_answered() {
    let Some((_bus, _fake, d, mut c)) = start(true, devices()).await else {
        return;
    };
    call(
        &mut c,
        "subscribe",
        json!({"events": ["bluetooth_confirm"]}),
    )
    .await
    .unwrap();
    let mut p = d.client().await;
    let pairing = tokio::spawn(async move {
        call(
            &mut p,
            "bluetooth.pair",
            json!({"address": "AA:BB:CC:DD:EE:04"}),
        )
        .await
    });
    let ask = event_named(&mut c, "bluetooth_confirm").await;
    assert_eq!(
        ask,
        json!({"address": "AA:BB:CC:DD:EE:04", "name": "Keyboard", "passkey": "654321", "mode": "display"})
    );
    // Nothing to answer: the user types it on the keyboard.
    assert_eq!(pairing.await.unwrap().unwrap()["paired"], true);

    // A legacy device asking for a PIN gets 0000.
    let r = call(
        &mut c,
        "bluetooth.pair",
        json!({"address": "AA:BB:CC:DD:EE:07"}),
    )
    .await
    .unwrap();
    assert_eq!(r["paired"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agent_refuses_requests_nobody_asked_for() {
    let Some((bus, fake, _d, mut c)) = start(true, devices()).await else {
        return;
    };
    // The user looked for devices once; the agent stays on BlueZ afterwards.
    call(&mut c, "bluetooth.scan", json!({"enabled": true}))
        .await
        .unwrap();
    call(&mut c, "bluetooth.scan", json!({"enabled": false}))
        .await
        .unwrap();
    // A device in range starts pairing by itself: BlueZ asks the agent while the user is
    // neither scanning nor pairing.
    let agent = fake.model.lock().unwrap().agent.clone().unwrap();
    let conn = fake.conn.clone();
    let path = ObjectPath::try_from(dev_path("AA:BB:CC:DD:EE:03")).unwrap();
    let p = zbus::Proxy::new(&conn, agent.0.clone(), agent.1.clone(), "org.bluez.Agent1")
        .await
        .unwrap();
    let r = p
        .call::<_, _, ()>("RequestConfirmation", &(path.clone(), 111111u32))
        .await;
    assert!(r.is_err(), "unsolicited confirmation was accepted");
    let r = p
        .call::<_, _, ()>("RequestAuthorization", &(path.clone(),))
        .await;
    assert!(r.is_err());
    let r = p
        .call::<_, _, String>("RequestPinCode", &(path.clone(),))
        .await;
    assert!(r.is_err());
    // While scanning, the user is looking for the device: authorization is fine.
    let mut c2 = _d.client().await;
    call(
        &mut c2,
        "bluetooth.scan",
        json!({"enabled": true, "timeout_s": 30}),
    )
    .await
    .unwrap();
    p.call::<_, _, ()>("RequestAuthorization", &(path,))
        .await
        .unwrap();
    drop(bus);
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_disconnect_forget_and_errors() {
    let Some((_bus, fake, _d, mut c)) = start(true, devices()).await else {
        return;
    };
    let r = call(
        &mut c,
        "bluetooth.disconnect",
        json!({"address": "AA:BB:CC:DD:EE:01"}),
    )
    .await
    .unwrap();
    assert_eq!(r["connected"], false);
    assert_eq!(r["paired"], true);
    let r = call(
        &mut c,
        "bluetooth.connect",
        json!({"address": "AA:BB:CC:DD:EE:01"}),
    )
    .await
    .unwrap();
    assert_eq!(r["connected"], true);

    // Not paired: BlueZ refuses.
    let e = call(
        &mut c,
        "bluetooth.connect",
        json!({"address": "AA:BB:CC:DD:EE:03"}),
    )
    .await
    .unwrap_err();
    assert_eq!(e["code"], 4002);

    call(
        &mut c,
        "bluetooth.forget",
        json!({"address": "AA:BB:CC:DD:EE:01"}),
    )
    .await
    .unwrap();
    assert_eq!(fake.model.lock().unwrap().removed, ["AA:BB:CC:DD:EE:01"]);
    let list = call(&mut c, "bluetooth.devices", json!({})).await.unwrap();
    assert!(
        list["devices"]
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["address"] != "AA:BB:CC:DD:EE:01")
    );

    // Unknown and malformed addresses.
    for method in [
        "bluetooth.pair",
        "bluetooth.connect",
        "bluetooth.disconnect",
        "bluetooth.forget",
    ] {
        let e = call(&mut c, method, json!({"address": "AA:BB:CC:DD:EE:99"}))
            .await
            .unwrap_err();
        assert_eq!(e["code"], 3010, "{method}: {e}");
        let e = call(&mut c, method, json!({"address": "not an address"}))
            .await
            .unwrap_err();
        assert_eq!(e["code"], -32602, "{method}: {e}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn changes_are_pushed() {
    let Some((_bus, fake, _d, mut c)) = start(true, devices()).await else {
        return;
    };
    call(
        &mut c,
        "subscribe",
        json!({"events": ["bluetooth_changed"]}),
    )
    .await
    .unwrap();
    // The pad drops out on its own (battery flat).
    fake.model.lock().unwrap().devices[0].connected = false;
    poke(&fake.conn).await;
    let ev = loop {
        let ev = event_named(&mut c, "bluetooth_changed").await;
        if dev(&json!({"devices": ev["devices"]}), "AA:BB:CC:DD:EE:01")["connected"] == false {
            break ev;
        }
    };
    assert_eq!(
        ev["status"],
        json!({"available": true, "powered": true, "scanning": false})
    );
    // No change, no event: signal noise alone does not wake the launcher.
    poke(&fake.conn).await;
    let quiet = tokio::time::timeout(Duration::from_millis(900), c.next_event_raw()).await;
    assert!(quiet.is_err(), "event without a change: {quiet:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_bluez_or_an_adapter() {
    let Some(bus) = Bus::start() else { return };
    let d = Daemon::start_with(|_| runtime_bus(&bus));
    let mut c = d.client().await;
    for (method, params) in [
        ("bluetooth.status", json!({})),
        ("bluetooth.devices", json!({})),
        ("bluetooth.scan", json!({"enabled": true})),
        ("bluetooth.pair", json!({"address": "AA:BB:CC:DD:EE:01"})),
        (
            "bluetooth.confirm",
            json!({"address": "AA:BB:CC:DD:EE:01", "accept": true}),
        ),
        ("bluetooth.connect", json!({"address": "AA:BB:CC:DD:EE:01"})),
        (
            "bluetooth.disconnect",
            json!({"address": "AA:BB:CC:DD:EE:01"}),
        ),
        ("bluetooth.forget", json!({"address": "AA:BB:CC:DD:EE:01"})),
    ] {
        let e = call(&mut c, method, params).await.unwrap_err();
        // confirm needs no D-Bus: nothing waits.
        let expect = if method == "bluetooth.confirm" {
            3010
        } else {
            4001
        };
        assert_eq!(e["code"], expect, "{method}: {e}");
        if expect == 4001 {
            assert_eq!(e["data"]["service"], "bluez");
        }
    }

    // BlueZ without an adapter (a box without Bluetooth hardware).
    let conn = zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .serve_at("/", zbus::fdo::ObjectManager)
        .unwrap()
        .serve_at(
            "/org/bluez",
            AgentManager(Arc::new(Mutex::new(Model::default()))),
        )
        .unwrap()
        .name("org.bluez")
        .unwrap()
        .build()
        .await
        .unwrap();
    let status = call(&mut c, "bluetooth.status", json!({})).await.unwrap();
    assert_eq!(
        status,
        json!({"available": false, "powered": false, "scanning": false})
    );
    let e = call(&mut c, "bluetooth.scan", json!({"enabled": true}))
        .await
        .unwrap_err();
    assert_eq!(e["code"], 4002);
    assert_eq!(e["data"]["reason"], "no_adapter");
    drop(conn);
    let _ = OwnedObjectPath::try_from("/");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agent_comes_back_when_bluez_restarts() {
    let Some((bus, fake, _d, mut c)) = start(true, devices()).await else {
        return;
    };
    call(&mut c, "bluetooth.scan", json!({"enabled": false}))
        .await
        .unwrap();
    call(
        &mut c,
        "bluetooth.pair",
        json!({"address": "AA:BB:CC:DD:EE:01"}),
    )
    .await
    .unwrap();
    assert!(fake.model.lock().unwrap().agent.is_some());
    // BlueZ restarts and has forgotten its agents.
    drop(fake);
    let e = call(&mut c, "bluetooth.status", json!({}))
        .await
        .unwrap_err();
    assert_eq!(e["code"], 4001);
    let fake = Fake::start(&bus, true, devices()).await;
    eventually("the agent to be registered again", async || {
        fake.model.lock().unwrap().agent.clone().filter(|a| a.3)
    })
    .await;
    assert_eq!(
        call(&mut c, "bluetooth.status", json!({})).await.unwrap()["available"],
        true
    );
}
