//! Wi-Fi and network status against a fake NetworkManager on a private bus: the same names,
//! interfaces and signals as the real one (read from its D-Bus API), with a scripted network
//! (which password is right, which access points exist). Skipped when `dbus-daemon` is not
//! on PATH, unless HINTEROS_REQUIRE_TEST_TOOLS is set.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use hinteros_api::client::Client;
use serde_json::{Value, json};
use zbus::names::BusName;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value as ZValue};
use zbus::{Connection, ObjectServer};

mod common;

use common::*;

const NM: &str = "org.freedesktop.NetworkManager";
const ROOT: &str = "/org/freedesktop/NetworkManager";
const SETTINGS: &str = "/org/freedesktop/NetworkManager/Settings";
const WIFI: &str = "/org/freedesktop/NetworkManager/Devices/1";
const ETH: &str = "/org/freedesktop/NetworkManager/Devices/2";
const ACTIVE: &str = "/org/freedesktop/NetworkManager/ActiveConnection/1";
const IP4: &str = "/org/freedesktop/NetworkManager/IP4Config/1";
const RIGHT_PASSWORD: &str = "correct horse";

const PSK: u32 = 0x100;
const SAE: u32 = 0x400;
const EAP: u32 = 0x200;

struct Ap {
    ssid: &'static str,
    strength: u8,
    flags: u32,
    wpa: u32,
    rsn: u32,
}

struct Saved {
    path: String,
    ssid: Vec<u8>,
    psk: Option<String>,
}

struct Model {
    wifi_enabled: bool,
    wifi_state: u32,
    eth_state: u32,
    aps: Vec<Ap>,
    active_ap: Option<usize>,
    active_conn: Option<String>,
    saved: Vec<Saved>,
    next: u32,
    last_scan: i64,
    scans: u32,
    /// Refuse scans, like NetworkManager does right after another one.
    refuse_scan: bool,
    /// Take scans and never finish them (LastScan stays).
    hold_scan: bool,
}

type Shared = Arc<Mutex<Model>>;

fn ap_path(i: usize) -> String {
    format!("{ROOT}/AccessPoint/{i}")
}

async fn emit_state(conn: &Connection, new: u32, old: u32, reason: u32) {
    conn.emit_signal(
        None::<BusName<'_>>,
        WIFI,
        "org.freedesktop.NetworkManager.Device",
        "StateChanged",
        &(new, old, reason),
    )
    .await
    .unwrap();
}

async fn changed(conn: &Connection) {
    conn.emit_signal(
        None::<BusName<'_>>,
        ROOT,
        "org.freedesktop.DBus.Properties",
        "PropertiesChanged",
        &(
            "org.freedesktop.NetworkManager",
            HashMap::<String, ZValue<'_>>::new(),
            Vec::<String>::new(),
        ),
    )
    .await
    .unwrap();
}

struct Root(Shared);

#[zbus::interface(name = "org.freedesktop.NetworkManager")]
impl Root {
    #[zbus(property)]
    fn wireless_enabled(&self) -> bool {
        self.0.lock().unwrap().wifi_enabled
    }
    #[zbus(property)]
    fn set_wireless_enabled(&mut self, on: bool) {
        let mut m = self.0.lock().unwrap();
        m.wifi_enabled = on;
        if !on {
            m.wifi_state = 30;
            m.active_ap = None;
            m.active_conn = None;
        }
    }
    #[zbus(property)]
    fn connectivity(&self) -> u32 {
        let m = self.0.lock().unwrap();
        if m.wifi_state == 100 || m.eth_state == 100 {
            4
        } else {
            1
        }
    }
    #[zbus(property)]
    fn primary_connection(&self) -> OwnedObjectPath {
        let m = self.0.lock().unwrap();
        let p = if m.wifi_state == 100 || m.eth_state == 100 {
            ACTIVE
        } else {
            "/"
        };
        OwnedObjectPath::try_from(p).unwrap()
    }

    fn get_devices(&self) -> Vec<OwnedObjectPath> {
        [WIFI, ETH]
            .iter()
            .map(|p| OwnedObjectPath::try_from(*p).unwrap())
            .collect()
    }

    async fn activate_connection(
        &self,
        connection: ObjectPath<'_>,
        _device: ObjectPath<'_>,
        specific_object: ObjectPath<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> OwnedObjectPath {
        let model = self.0.clone();
        let conn = conn.clone();
        let connection = connection.to_string();
        let ap: usize = specific_object.rsplit('/').next().unwrap().parse().unwrap();
        tokio::spawn(async move {
            let (psk, ssid_ok) = {
                let m = model.lock().unwrap();
                let saved = m.saved.iter().find(|s| s.path == connection).unwrap();
                (saved.psk.clone(), saved.ssid == m.aps[ap].ssid.as_bytes())
            };
            let old = model.lock().unwrap().wifi_state;
            emit_state(&conn, 40, old, 0).await;
            let secured =
                model.lock().unwrap().aps[ap].rsn != 0 || model.lock().unwrap().aps[ap].wpa != 0;
            if !ssid_ok || (secured && psk.as_deref() != Some(RIGHT_PASSWORD)) {
                // What NetworkManager does without a secrets agent and a rejected key.
                emit_state(&conn, 60, 40, 0).await;
                model.lock().unwrap().wifi_state = 30;
                emit_state(&conn, 120, 60, 7).await;
                changed(&conn).await;
                return;
            }
            {
                let mut m = model.lock().unwrap();
                m.wifi_state = 100;
                m.active_ap = Some(ap);
                m.active_conn = Some(connection);
            }
            emit_state(&conn, 100, 40, 0).await;
            changed(&conn).await;
        });
        OwnedObjectPath::try_from(ACTIVE).unwrap()
    }
}

struct Device(Shared, bool);

#[zbus::interface(name = "org.freedesktop.NetworkManager.Device")]
impl Device {
    #[zbus(property)]
    fn device_type(&self) -> u32 {
        if self.1 { 2 } else { 1 }
    }
    #[zbus(property)]
    fn state(&self) -> u32 {
        let m = self.0.lock().unwrap();
        if self.1 { m.wifi_state } else { m.eth_state }
    }
    #[zbus(property)]
    fn ip4_config(&self) -> OwnedObjectPath {
        let m = self.0.lock().unwrap();
        let up = if self.1 { m.wifi_state } else { m.eth_state } == 100;
        OwnedObjectPath::try_from(if up { IP4 } else { "/" }).unwrap()
    }
    #[zbus(property)]
    fn active_connection(&self) -> OwnedObjectPath {
        let m = self.0.lock().unwrap();
        let up = self.1 && m.wifi_state == 100;
        OwnedObjectPath::try_from(if up { ACTIVE } else { "/" }).unwrap()
    }
    async fn disconnect(&self, #[zbus(connection)] conn: &Connection) {
        {
            let mut m = self.0.lock().unwrap();
            m.wifi_state = 30;
            m.active_ap = None;
            m.active_conn = None;
        }
        emit_state(conn, 30, 100, 39).await;
        changed(conn).await;
    }
}

struct Wireless(Shared);

#[zbus::interface(name = "org.freedesktop.NetworkManager.Device.Wireless")]
impl Wireless {
    fn get_all_access_points(&self) -> Vec<OwnedObjectPath> {
        let m = self.0.lock().unwrap();
        (0..m.aps.len())
            .map(|i| OwnedObjectPath::try_from(ap_path(i)).unwrap())
            .collect()
    }
    #[zbus(property)]
    fn active_access_point(&self) -> OwnedObjectPath {
        let m = self.0.lock().unwrap();
        OwnedObjectPath::try_from(m.active_ap.map_or("/".to_owned(), ap_path)).unwrap()
    }
    #[zbus(property)]
    fn last_scan(&self) -> i64 {
        self.0.lock().unwrap().last_scan
    }
    async fn request_scan(
        &self,
        _options: HashMap<String, OwnedValue>,
        #[zbus(connection)] conn: &Connection,
    ) -> zbus::fdo::Result<()> {
        let mut m = self.0.lock().unwrap();
        if m.refuse_scan {
            return Err(zbus::fdo::Error::Failed(
                "Scanning not allowed immediately following previous scan".into(),
            ));
        }
        m.scans += 1;
        if m.hold_scan {
            return Ok(());
        }
        m.last_scan += 1000;
        // A new network shows up with the scan.
        if m.scans == 1 && !m.aps.iter().any(|a| a.ssid == "NewNet") {
            m.aps.push(Ap {
                ssid: "NewNet",
                strength: 20,
                flags: 1,
                wpa: 0,
                rsn: PSK,
            });
        }
        let conn = conn.clone();
        tokio::spawn(async move { changed(&conn).await });
        Ok(())
    }
}

struct AccessPoint(Shared, usize);

#[zbus::interface(name = "org.freedesktop.NetworkManager.AccessPoint")]
impl AccessPoint {
    #[zbus(property)]
    fn ssid(&self) -> Vec<u8> {
        self.0.lock().unwrap().aps[self.1].ssid.as_bytes().to_vec()
    }
    #[zbus(property)]
    fn strength(&self) -> u8 {
        self.0.lock().unwrap().aps[self.1].strength
    }
    #[zbus(property)]
    fn flags(&self) -> u32 {
        self.0.lock().unwrap().aps[self.1].flags
    }
    #[zbus(property)]
    fn wpa_flags(&self) -> u32 {
        self.0.lock().unwrap().aps[self.1].wpa
    }
    #[zbus(property)]
    fn rsn_flags(&self) -> u32 {
        self.0.lock().unwrap().aps[self.1].rsn
    }
}

struct ActiveConnection(Shared);

#[zbus::interface(name = "org.freedesktop.NetworkManager.Connection.Active")]
impl ActiveConnection {
    #[zbus(property)]
    fn connection(&self) -> OwnedObjectPath {
        let m = self.0.lock().unwrap();
        OwnedObjectPath::try_from(m.active_conn.clone().unwrap_or("/".into())).unwrap()
    }
    #[zbus(property)]
    fn ip4_config(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(IP4).unwrap()
    }
}

struct Ip4(Shared);

#[zbus::interface(name = "org.freedesktop.NetworkManager.IP4Config")]
impl Ip4 {
    #[zbus(property)]
    fn address_data(&self) -> Vec<HashMap<String, OwnedValue>> {
        let m = self.0.lock().unwrap();
        let address = if m.wifi_state == 100 {
            "192.168.1.20"
        } else {
            "10.0.0.5"
        };
        vec![HashMap::from([
            (
                "address".to_owned(),
                OwnedValue::try_from(ZValue::from(address)).unwrap(),
            ),
            (
                "prefix".to_owned(),
                OwnedValue::try_from(ZValue::from(24u32)).unwrap(),
            ),
        ])]
    }
}

struct Settings(Shared);

#[zbus::interface(name = "org.freedesktop.NetworkManager.Settings")]
impl Settings {
    fn list_connections(&self) -> Vec<OwnedObjectPath> {
        let m = self.0.lock().unwrap();
        m.saved
            .iter()
            .map(|s| OwnedObjectPath::try_from(s.path.clone()).unwrap())
            .collect()
    }
    async fn add_connection(
        &self,
        connection: HashMap<String, HashMap<String, OwnedValue>>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let ssid: Vec<u8> = connection["802-11-wireless"]["ssid"]
            .clone()
            .try_into()
            .unwrap();
        let psk = connection
            .get("802-11-wireless-security")
            .and_then(|s| s.get("psk"))
            .map(|v| String::try_from(v.clone()).unwrap());
        let path = {
            let mut m = self.0.lock().unwrap();
            m.next += 1;
            let path = format!("{SETTINGS}/{}", m.next);
            m.saved.push(Saved {
                path: path.clone(),
                ssid,
                psk,
            });
            path
        };
        server
            .at(path.clone(), SavedConnection(self.0.clone(), path.clone()))
            .await?;
        Ok(OwnedObjectPath::try_from(path).unwrap())
    }
}

struct SavedConnection(Shared, String);

#[zbus::interface(name = "org.freedesktop.NetworkManager.Settings.Connection")]
impl SavedConnection {
    fn get_settings(&self) -> HashMap<String, HashMap<String, OwnedValue>> {
        let m = self.0.lock().unwrap();
        let s = m.saved.iter().find(|s| s.path == self.1).unwrap();
        let v = |x: ZValue<'_>| OwnedValue::try_from(x).unwrap();
        HashMap::from([
            (
                "connection".to_owned(),
                HashMap::from([
                    ("type".to_owned(), v(ZValue::from("802-11-wireless"))),
                    (
                        "id".to_owned(),
                        v(ZValue::from(String::from_utf8_lossy(&s.ssid).into_owned())),
                    ),
                ]),
            ),
            (
                "802-11-wireless".to_owned(),
                HashMap::from([("ssid".to_owned(), v(ZValue::from(s.ssid.clone())))]),
            ),
        ])
    }
    fn update(&self, settings: HashMap<String, HashMap<String, OwnedValue>>) {
        let mut m = self.0.lock().unwrap();
        let psk = settings
            .get("802-11-wireless-security")
            .and_then(|s| s.get("psk"))
            .map(|v| String::try_from(v.clone()).unwrap());
        m.saved.iter_mut().find(|s| s.path == self.1).unwrap().psk = psk;
    }
    async fn delete(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(signal_emitter)] _e: SignalEmitter<'_>,
    ) {
        {
            let mut m = self.0.lock().unwrap();
            m.saved.retain(|s| s.path != self.1);
            if m.active_conn.as_deref() == Some(&self.1) {
                m.wifi_state = 30;
                m.active_ap = None;
                m.active_conn = None;
            }
        }
        let _ = server.remove::<SavedConnection, _>(self.1.as_str()).await;
    }
}

struct Fake {
    conn: Connection,
    model: Shared,
}

impl Fake {
    async fn start(bus: &Bus, connected: bool) -> Self {
        let aps = vec![
            Ap {
                ssid: "Home",
                strength: 72,
                flags: 1,
                wpa: PSK,
                rsn: PSK,
            },
            Ap {
                ssid: "Home",
                strength: 40,
                flags: 1,
                wpa: PSK,
                rsn: PSK,
            },
            Ap {
                ssid: "Neighbour",
                strength: 31,
                flags: 1,
                wpa: 0,
                rsn: SAE,
            },
            Ap {
                ssid: "Cafe",
                strength: 55,
                flags: 0,
                wpa: 0,
                rsn: 0,
            },
            Ap {
                ssid: "Corp",
                strength: 80,
                flags: 1,
                wpa: 0,
                rsn: EAP,
            },
            Ap {
                ssid: "OldWep",
                strength: 10,
                flags: 1,
                wpa: 0,
                rsn: 0,
            },
        ];
        let model = Arc::new(Mutex::new(Model {
            wifi_enabled: true,
            wifi_state: if connected { 100 } else { 30 },
            eth_state: 20,
            active_ap: connected.then_some(0),
            active_conn: connected.then(|| format!("{SETTINGS}/1")),
            saved: vec![Saved {
                path: format!("{SETTINGS}/1"),
                ssid: b"Home".to_vec(),
                psk: Some(RIGHT_PASSWORD.into()),
            }],
            aps,
            next: 1,
            last_scan: 1000,
            scans: 0,
            refuse_scan: false,
            hold_scan: false,
        }));
        let mut b = zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .serve_at(ROOT, Root(model.clone()))
            .unwrap()
            .serve_at(WIFI, Device(model.clone(), true))
            .unwrap()
            .serve_at(WIFI, Wireless(model.clone()))
            .unwrap()
            .serve_at(ETH, Device(model.clone(), false))
            .unwrap()
            .serve_at(ACTIVE, ActiveConnection(model.clone()))
            .unwrap()
            .serve_at(IP4, Ip4(model.clone()))
            .unwrap()
            .serve_at(SETTINGS, Settings(model.clone()))
            .unwrap()
            .serve_at(
                format!("{SETTINGS}/1"),
                SavedConnection(model.clone(), format!("{SETTINGS}/1")),
            )
            .unwrap();
        // NewNet joins the list later, so its object exists from the start.
        for i in 0..8 {
            b = b
                .serve_at(ap_path(i), AccessPoint(model.clone(), i))
                .unwrap_or_else(|_| unreachable!());
        }
        let conn = b.name(NM).unwrap().build().await.unwrap();
        Self { conn, model }
    }
}

async fn start(connected: bool) -> Option<(Bus, Fake, Daemon, Client)> {
    let bus = Bus::start()?;
    let fake = Fake::start(&bus, connected).await;
    let d = Daemon::start_with(|_| runtime_bus(&bus));
    let c = d.client().await;
    Some((bus, fake, d, c))
}

fn net(list: &Value, ssid: &str) -> Value {
    list["networks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["ssid"] == ssid)
        .unwrap_or_else(|| panic!("no {ssid} in {list}"))
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn status_and_list() {
    let Some((_bus, _fake, _d, mut c)) = start(true).await else {
        return;
    };
    let status = call(&mut c, "network.status", json!({})).await.unwrap();
    assert_eq!(
        status,
        json!({
            "wifi_available": true, "wifi_enabled": true, "ethernet": false,
            "wifi_ssid": "Home", "wifi_signal": 72, "address": "192.168.1.20",
            "connectivity": "full"
        })
    );
    let list = call(&mut c, "wifi.list", json!({})).await.unwrap();
    let ssids: Vec<&str> = list["networks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["ssid"].as_str().unwrap())
        .collect();
    // One entry per SSID, strongest first.
    assert_eq!(ssids, ["Corp", "Home", "Cafe", "Neighbour", "OldWep"]);
    assert_eq!(
        net(&list, "Home"),
        json!({"ssid": "Home", "signal": 72, "security": "wpa", "known": true, "connected": true})
    );
    assert_eq!(net(&list, "Neighbour")["security"], "wpa3");
    assert_eq!(net(&list, "Neighbour")["known"], false);
    assert_eq!(net(&list, "Cafe")["security"], "open");
    assert_eq!(net(&list, "Corp")["security"], "enterprise");
    assert_eq!(net(&list, "OldWep")["security"], "wep");
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_open_known_and_new_networks() {
    let Some((_bus, fake, d, mut c)) = start(false).await else {
        return;
    };
    let status = call(&mut c, "network.status", json!({})).await.unwrap();
    assert!(status.get("wifi_ssid").is_none());
    assert_eq!(status["connectivity"], "none");

    // A known network needs no password.
    let status = call(&mut c, "wifi.connect", json!({"ssid": "Home"}))
        .await
        .unwrap();
    assert_eq!(status["wifi_ssid"], "Home");
    assert_eq!(fake.model.lock().unwrap().saved.len(), 1);

    // An open network is created without security; a password given is ignored.
    let status = call(
        &mut c,
        "wifi.connect",
        json!({"ssid": "Cafe", "password": "ignored ignored"}),
    )
    .await;
    // Cafe's access point is open, so the fake accepts any connection to it.
    assert_eq!(status.unwrap()["wifi_ssid"], "Cafe");
    assert_eq!(fake.model.lock().unwrap().saved.len(), 2);
    assert_eq!(fake.model.lock().unwrap().saved[1].psk, None);

    // A new secured network: created system-wide with the password.
    let secret = "correct horse";
    let status = call(
        &mut c,
        "wifi.connect",
        json!({"ssid": "Neighbour", "password": secret}),
    )
    .await
    .unwrap();
    assert_eq!(status["wifi_ssid"], "Neighbour");
    {
        let m = fake.model.lock().unwrap();
        assert_eq!(m.saved.len(), 3);
        assert_eq!(m.saved[2].psk.as_deref(), Some(secret));
    }
    // The password was never logged.
    assert!(!d.log().contains(secret), "password in the daemon's log");
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_password_leaves_nothing_behind() {
    let Some((_bus, fake, d, mut c)) = start(false).await else {
        return;
    };
    let e = call(
        &mut c,
        "wifi.connect",
        json!({"ssid": "Neighbour", "password": "wrong-password-1"}),
    )
    .await
    .unwrap_err();
    assert_eq!(e["code"], 4003, "{e}");
    assert_eq!(
        fake.model.lock().unwrap().saved.len(),
        1,
        "the failed connection was deleted again"
    );
    assert!(!d.log().contains("wrong-password-1"));
    // Retrying with the right one works.
    let status = call(
        &mut c,
        "wifi.connect",
        json!({"ssid": "Neighbour", "password": RIGHT_PASSWORD}),
    )
    .await
    .unwrap();
    assert_eq!(status["wifi_ssid"], "Neighbour");

    // A new password for a stored network replaces the stored one.
    fake.model.lock().unwrap().saved[0].psk = Some("old old old old".into());
    let status = call(
        &mut c,
        "wifi.connect",
        json!({"ssid": "Home", "password": RIGHT_PASSWORD}),
    )
    .await
    .unwrap();
    assert_eq!(status["wifi_ssid"], "Home");
    assert_eq!(
        fake.model.lock().unwrap().saved[0].psk.as_deref(),
        Some(RIGHT_PASSWORD)
    );
    // ... and a wrong one for a stored network is also reported (the old connection stays).
    let e = call(
        &mut c,
        "wifi.connect",
        json!({"ssid": "Home", "password": "different wrong 1"}),
    )
    .await
    .unwrap_err();
    assert_eq!(e["code"], 4003);
    assert_eq!(fake.model.lock().unwrap().saved.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_requests() {
    let Some((_bus, fake, _d, mut c)) = start(false).await else {
        return;
    };
    // A secured network without a password (and no stored connection).
    let e = call(&mut c, "wifi.connect", json!({"ssid": "Neighbour"}))
        .await
        .unwrap_err();
    assert_eq!(e["code"], -32602);
    assert_eq!(
        e["data"],
        json!({"field": "password", "reason": "required"})
    );
    // WPA passwords are 8 to 63 characters.
    let e = call(
        &mut c,
        "wifi.connect",
        json!({"ssid": "Home", "password": "short"}),
    )
    .await
    .unwrap_err();
    assert_eq!(e["data"]["reason"], "invalid_length");
    // Enterprise and WEP cannot be joined with a password.
    for ssid in ["Corp", "OldWep"] {
        let e = call(
            &mut c,
            "wifi.connect",
            json!({"ssid": ssid, "password": "whatever 123"}),
        )
        .await
        .unwrap_err();
        assert_eq!(e["code"], -32602);
        assert_eq!(e["data"]["reason"], "unsupported_security");
    }
    // Not in range.
    let e = call(
        &mut c,
        "wifi.connect",
        json!({"ssid": "Gone", "password": "whatever 123"}),
    )
    .await
    .unwrap_err();
    assert_eq!(e["code"], 3010);
    // Unknown fields and empty names.
    let e = call(
        &mut c,
        "wifi.connect",
        json!({"ssid": "Home", "hidden": true}),
    )
    .await
    .unwrap_err();
    assert_eq!(e["code"], -32602);
    // Nothing was created or changed.
    assert_eq!(fake.model.lock().unwrap().saved.len(), 1);
    assert_eq!(fake.model.lock().unwrap().wifi_state, 30);
}

#[tokio::test(flavor = "multi_thread")]
async fn disconnect_forget_and_switch() {
    let Some((_bus, fake, _d, mut c)) = start(true).await else {
        return;
    };
    let status = call(&mut c, "wifi.disconnect", json!({})).await.unwrap();
    assert!(status.get("wifi_ssid").is_none());
    assert_eq!(status["connectivity"], "none");
    let list = call(&mut c, "wifi.list", json!({})).await.unwrap();
    assert_eq!(net(&list, "Home")["connected"], false);
    assert_eq!(net(&list, "Home")["known"], true);

    // Disconnecting again is not an error.
    call(&mut c, "wifi.disconnect", json!({})).await.unwrap();

    call(&mut c, "wifi.forget", json!({"ssid": "Home"}))
        .await
        .unwrap();
    assert!(fake.model.lock().unwrap().saved.is_empty());
    let list = call(&mut c, "wifi.list", json!({})).await.unwrap();
    assert_eq!(net(&list, "Home")["known"], false);
    // Forgetting what is not stored is fine.
    call(&mut c, "wifi.forget", json!({"ssid": "Home"}))
        .await
        .unwrap();

    let status = call(&mut c, "wifi.enable", json!({"enabled": false}))
        .await
        .unwrap();
    assert_eq!(status["wifi_enabled"], false);
    let e = call(&mut c, "wifi.scan", json!({})).await.unwrap_err();
    assert_eq!(e["code"], 4002);
    assert_eq!(e["data"]["reason"], "wifi_disabled");
    let e = call(&mut c, "wifi.connect", json!({"ssid": "Cafe"}))
        .await
        .unwrap_err();
    assert_eq!(e["data"]["reason"], "wifi_disabled");
    let status = call(&mut c, "wifi.enable", json!({"enabled": true}))
        .await
        .unwrap();
    assert_eq!(status["wifi_enabled"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn scan_and_events() {
    let Some((_bus, fake, _d, mut c)) = start(true).await else {
        return;
    };
    call(
        &mut c,
        "subscribe",
        json!({"events": ["network_changed", "wifi_list_changed"]}),
    )
    .await
    .unwrap();

    // A scan returns the new list (the fake finds NewNet) and tells subscribers.
    let list = call(&mut c, "wifi.scan", json!({})).await.unwrap();
    assert_eq!(net(&list, "NewNet")["signal"], 20);
    assert_eq!(fake.model.lock().unwrap().scans, 1);
    let ev = event_named(&mut c, "wifi_list_changed").await;
    assert!(
        ev["networks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["ssid"] == "NewNet")
    );

    // A scan NetworkManager refuses (too soon after the last one) still answers with the list.
    fake.model.lock().unwrap().refuse_scan = true;
    let list = call(&mut c, "wifi.scan", json!({})).await.unwrap();
    assert!(list["networks"].as_array().unwrap().len() >= 5);

    // The network changing under us is reported: the cable goes in, Wi-Fi drops.
    {
        let mut m = fake.model.lock().unwrap();
        m.eth_state = 100;
        m.wifi_state = 30;
        m.active_ap = None;
        m.active_conn = None;
    }
    changed(&fake.conn).await;
    let status = loop {
        let s = event_named(&mut c, "network_changed").await;
        if s["ethernet"] == true {
            break s;
        }
    };
    assert!(status.get("wifi_ssid").is_none());
    assert_eq!(status["connectivity"], "full");
}

#[tokio::test(flavor = "multi_thread")]
async fn no_networkmanager() {
    let Some(bus) = Bus::start() else { return };
    let d = Daemon::start_with(|_| runtime_bus(&bus));
    let mut c = d.client().await;
    for (method, params) in [
        ("network.status", json!({})),
        ("wifi.list", json!({})),
        ("wifi.scan", json!({})),
        ("wifi.connect", json!({"ssid": "x", "password": "12345678"})),
        ("wifi.disconnect", json!({})),
        ("wifi.forget", json!({"ssid": "x"})),
        ("wifi.enable", json!({"enabled": true})),
    ] {
        let e = call(&mut c, method, params).await.unwrap_err();
        assert_eq!(e["code"], 4001, "{method}: {e}");
        assert_eq!(e["data"]["service"], "networkmanager");
    }
    // It comes up later: the same daemon works then.
    let fake = Fake::start(&bus, true).await;
    let status = call(&mut c, "network.status", json!({})).await.unwrap();
    assert_eq!(status["wifi_ssid"], "Home");
    drop(fake);
}

#[tokio::test(flavor = "multi_thread")]
async fn scan_does_not_hold_up_the_connection() {
    let Some((_bus, fake, _d, mut c)) = start(true).await else {
        return;
    };
    fake.model.lock().unwrap().hold_scan = true;
    // While a scan runs, other calls are answered (and events, the controller input among
    // them, go out): responses come by id.
    c.send_line(r#"{"jsonrpc":"2.0","id":1,"method":"wifi.scan","params":{}}"#)
        .await
        .unwrap();
    c.send_line(r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#)
        .await
        .unwrap();
    let first = c.read_raw().await.unwrap().unwrap();
    assert_eq!(first["id"], 2, "{first}");
    // The scan ends: its answer comes after.
    while fake.model.lock().unwrap().scans == 0 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    fake.model.lock().unwrap().last_scan += 1000;
    let second = c.read_raw().await.unwrap().unwrap();
    assert_eq!(second["id"], 1, "{second}");
    assert!(second["result"]["networks"].is_array(), "{second}");
}
