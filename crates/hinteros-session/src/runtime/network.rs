//! Wi-Fi and network status through NetworkManager's D-Bus API, as the session user.
//!
//! polkit (the system repo's `runtime.nix`) lets the active local session user scan, switch
//! Wi-Fi, activate connections and create *system* connections, so a password is entered once
//! per box and the connection exists before login. The daemon never logs passwords: they only
//! ever go into the D-Bus call that creates or updates the connection.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use hinteros_api::session::{
    Connectivity, Empty, EventName, NetworkStatus, Service, WifiConnectParams, WifiEnableParams,
    WifiForgetParams, WifiList, WifiNetwork, WifiSecurity,
};
use hinteros_api::{ErrorCode, ErrorObject};
use tokio::time::{Instant, sleep, timeout};
use tracing::{debug, info, warn};
use zbus::Connection;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::Ctx;
use super::dbus::{self, PropMap, Settings, api_error, failed, get, not_found, props, proxy};

const NM: &str = "org.freedesktop.NetworkManager";
const ROOT: &str = "/org/freedesktop/NetworkManager";
const SETTINGS: &str = "/org/freedesktop/NetworkManager/Settings";
const I_NM: &str = "org.freedesktop.NetworkManager";
const I_DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const I_WIRELESS: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const I_AP: &str = "org.freedesktop.NetworkManager.AccessPoint";
const I_SETTINGS: &str = "org.freedesktop.NetworkManager.Settings";
const I_CONNECTION: &str = "org.freedesktop.NetworkManager.Settings.Connection";
const I_ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const I_IP4: &str = "org.freedesktop.NetworkManager.IP4Config";

const SVC: Service = Service::Networkmanager;

/// How long `wifi.connect` waits for the connection to come up or fail.
const ACTIVATE_TIMEOUT: Duration = Duration::from_secs(45);
/// How long `wifi.scan` waits for the scan to finish.
const SCAN_TIMEOUT: Duration = Duration::from_secs(10);

// NMDeviceType
const DEVICE_ETHERNET: u32 = 1;
const DEVICE_WIFI: u32 = 2;
// NMDeviceState
const STATE_DISCONNECTED: u32 = 30;
const STATE_NEED_AUTH: u32 = 60;
const STATE_ACTIVATED: u32 = 100;
const STATE_FAILED: u32 = 120;
// NMDeviceStateReason
const REASON_NO_SECRETS: u32 = 7;
const REASON_SSID_NOT_FOUND: u32 = 53;
// NM80211ApSecurityFlags
const SEC_KEY_PSK: u32 = 0x100;
const SEC_KEY_8021X: u32 = 0x200;
const SEC_KEY_SAE: u32 = 0x400;
const AP_FLAG_PRIVACY: u32 = 0x1;

fn map(e: zbus::Error) -> ErrorObject {
    api_error(SVC, &e)
}

#[derive(Debug, Clone)]
struct Device {
    path: String,
    kind: u32,
    state: u32,
    props: PropMap,
}

#[derive(Debug, Clone)]
struct Ap {
    path: String,
    ssid: Vec<u8>,
    strength: u8,
    security: WifiSecurity,
}

#[derive(Debug, Clone)]
struct Saved {
    path: String,
    ssid: Vec<u8>,
}

async fn devices(conn: &Connection) -> Result<Vec<Device>, ErrorObject> {
    let nm = proxy(conn, NM, ROOT, I_NM).await.map_err(map)?;
    let paths: Vec<OwnedObjectPath> = nm.call("GetDevices", &()).await.map_err(map)?;
    let mut out = Vec::new();
    for path in paths {
        let Ok(p) = props(conn, NM, path.as_str(), I_DEVICE).await else {
            continue; // the device went away meanwhile
        };
        out.push(Device {
            path: path.to_string(),
            kind: get(&p, "DeviceType").unwrap_or(0),
            state: get(&p, "State").unwrap_or(0),
            props: p,
        });
    }
    Ok(out)
}

fn wifi_device(devices: &[Device]) -> Option<&Device> {
    // The one that is connected, else the first.
    devices
        .iter()
        .filter(|d| d.kind == DEVICE_WIFI)
        .find(|d| d.state == STATE_ACTIVATED)
        .or_else(|| devices.iter().find(|d| d.kind == DEVICE_WIFI))
}

fn security(flags: u32, wpa: u32, rsn: u32) -> WifiSecurity {
    let keys = wpa | rsn;
    let psk = keys & SEC_KEY_PSK != 0;
    let sae = keys & SEC_KEY_SAE != 0;
    let eap = keys & SEC_KEY_8021X != 0;
    if psk {
        WifiSecurity::Wpa
    } else if sae {
        WifiSecurity::Wpa3
    } else if eap || keys != 0 {
        // 802.1X, OWE, Suite B: nothing a password alone joins.
        WifiSecurity::Enterprise
    } else if flags & AP_FLAG_PRIVACY != 0 {
        WifiSecurity::Wep
    } else {
        WifiSecurity::Open
    }
}

async fn access_points(conn: &Connection, device: &Device) -> Result<Vec<Ap>, ErrorObject> {
    let wifi = proxy(conn, NM, &device.path, I_WIRELESS)
        .await
        .map_err(map)?;
    let paths: Vec<OwnedObjectPath> = wifi.call("GetAllAccessPoints", &()).await.map_err(map)?;
    let mut out = Vec::new();
    for path in paths {
        let Ok(p) = props(conn, NM, path.as_str(), I_AP).await else {
            continue;
        };
        let ssid: Vec<u8> = get(&p, "Ssid").unwrap_or_default();
        if ssid.is_empty() {
            continue; // hidden
        }
        out.push(Ap {
            path: path.to_string(),
            ssid,
            strength: get::<u8>(&p, "Strength").unwrap_or(0).min(100),
            security: security(
                get(&p, "Flags").unwrap_or(0),
                get(&p, "WpaFlags").unwrap_or(0),
                get(&p, "RsnFlags").unwrap_or(0),
            ),
        });
    }
    Ok(out)
}

/// The stored Wi-Fi connections.
async fn saved(conn: &Connection) -> Result<Vec<Saved>, ErrorObject> {
    let settings = proxy(conn, NM, SETTINGS, I_SETTINGS).await.map_err(map)?;
    let paths: Vec<OwnedObjectPath> = settings.call("ListConnections", &()).await.map_err(map)?;
    let mut out = Vec::new();
    for path in paths {
        let Ok(c) = proxy(conn, NM, path.as_str(), I_CONNECTION).await else {
            continue;
        };
        let Ok(all) = c
            .call::<_, _, HashMap<String, PropMap>>("GetSettings", &())
            .await
        else {
            continue;
        };
        let kind: Option<String> = all.get("connection").and_then(|m| get(m, "type"));
        if kind.as_deref() != Some("802-11-wireless") {
            continue;
        }
        if let Some(ssid) = all
            .get("802-11-wireless")
            .and_then(|m| get::<Vec<u8>>(m, "ssid"))
        {
            out.push(Saved {
                path: path.to_string(),
                ssid,
            });
        }
    }
    Ok(out)
}

async fn ipv4(conn: &Connection, config: &str) -> Option<String> {
    if config.is_empty() || config == "/" {
        return None;
    }
    let p = props(conn, NM, config, I_IP4).await.ok()?;
    let data: Vec<HashMap<String, OwnedValue>> = get(&p, "AddressData")?;
    data.first()
        .and_then(|a| a.get("address"))
        .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
}

fn path_of(p: &PropMap, key: &str) -> String {
    get::<OwnedObjectPath>(p, key)
        .map(|p| p.to_string())
        .unwrap_or_default()
}

fn is_none_path(p: &str) -> bool {
    p.is_empty() || p == "/"
}

async fn status_from(conn: &Connection, devices: &[Device]) -> Result<NetworkStatus, ErrorObject> {
    let nm = props(conn, NM, ROOT, I_NM).await.map_err(map)?;
    let wifi = wifi_device(devices);
    let mut status = NetworkStatus {
        wifi_available: wifi.is_some(),
        wifi_enabled: get(&nm, "WirelessEnabled").unwrap_or(false),
        ethernet: devices
            .iter()
            .any(|d| d.kind == DEVICE_ETHERNET && d.state == STATE_ACTIVATED),
        wifi_ssid: None,
        wifi_signal: None,
        address: None,
        connectivity: match get::<u32>(&nm, "Connectivity").unwrap_or(0) {
            1 => Connectivity::None,
            2 => Connectivity::Portal,
            3 => Connectivity::Limited,
            4 => Connectivity::Full,
            _ => Connectivity::Unknown,
        },
    };
    if let Some(d) = wifi.filter(|d| d.state == STATE_ACTIVATED) {
        let w = props(conn, NM, &d.path, I_WIRELESS).await.map_err(map)?;
        let active = path_of(&w, "ActiveAccessPoint");
        if !is_none_path(&active)
            && let Ok(ap) = props(conn, NM, &active, I_AP).await
        {
            status.wifi_ssid = get::<Vec<u8>>(&ap, "Ssid")
                .map(|s| String::from_utf8_lossy(&s).into_owned())
                .filter(|s| !s.is_empty());
            status.wifi_signal = get::<u8>(&ap, "Strength").map(|s| s.min(100));
        }
    }
    // The primary connection's address, else any connected device's.
    let mut config = String::new();
    let primary = path_of(&nm, "PrimaryConnection");
    if !is_none_path(&primary)
        && let Ok(a) = props(conn, NM, &primary, I_ACTIVE).await
    {
        config = path_of(&a, "Ip4Config");
    }
    if is_none_path(&config) {
        for d in devices.iter().filter(|d| d.state == STATE_ACTIVATED) {
            let c = path_of(&d.props, "Ip4Config");
            if !is_none_path(&c) {
                config = c;
                break;
            }
        }
    }
    status.address = ipv4(conn, &config).await;
    Ok(status)
}

async fn list_from(conn: &Connection, devices: &[Device]) -> Result<WifiList, ErrorObject> {
    let Some(device) = wifi_device(devices) else {
        return Ok(WifiList::default());
    };
    let aps = access_points(conn, device).await?;
    let saved = saved(conn).await?;
    let w = props(conn, NM, &device.path, I_WIRELESS)
        .await
        .map_err(map)?;
    let active = path_of(&w, "ActiveAccessPoint");
    let active_ssid = if device.state == STATE_ACTIVATED && !is_none_path(&active) {
        aps.iter()
            .find(|a| a.path == active)
            .map(|a| a.ssid.clone())
    } else {
        None
    };
    // One entry per SSID: the strongest access point.
    let mut best: HashMap<Vec<u8>, &Ap> = HashMap::new();
    for ap in &aps {
        match best.get(&ap.ssid) {
            Some(b) if b.strength >= ap.strength => {}
            _ => {
                best.insert(ap.ssid.clone(), ap);
            }
        }
    }
    let mut networks: Vec<WifiNetwork> = best
        .into_values()
        .map(|ap| WifiNetwork {
            ssid: String::from_utf8_lossy(&ap.ssid).into_owned(),
            signal: ap.strength,
            security: ap.security,
            known: saved.iter().any(|s| s.ssid == ap.ssid),
            connected: active_ssid.as_deref() == Some(ap.ssid.as_slice()),
        })
        .collect();
    networks.sort_by(|a, b| b.signal.cmp(&a.signal).then_with(|| a.ssid.cmp(&b.ssid)));
    Ok(WifiList { networks })
}

async fn snapshot(conn: &Connection) -> Result<(NetworkStatus, WifiList), ErrorObject> {
    let devices = devices(conn).await?;
    Ok((
        status_from(conn, &devices).await?,
        list_from(conn, &devices).await?,
    ))
}

pub async fn status(ctx: &Ctx) -> Result<NetworkStatus, ErrorObject> {
    let conn = ctx.conn(SVC).await?;
    let devices = devices(&conn).await?;
    status_from(&conn, &devices).await
}

pub async fn list(ctx: &Ctx) -> Result<WifiList, ErrorObject> {
    let conn = ctx.conn(SVC).await?;
    let devices = devices(&conn).await?;
    list_from(&conn, &devices).await
}

fn wifi_off() -> ErrorObject {
    failed("wifi_disabled", "Wi-Fi is turned off")
}

fn no_wifi() -> ErrorObject {
    failed("no_wifi_device", "there is no Wi-Fi device")
}

pub async fn scan(ctx: &Ctx) -> Result<WifiList, ErrorObject> {
    let conn = ctx.conn(SVC).await?;
    let devices = devices(&conn).await?;
    let device = wifi_device(&devices).ok_or_else(no_wifi)?;
    let nm = props(&conn, NM, ROOT, I_NM).await.map_err(map)?;
    if !get(&nm, "WirelessEnabled").unwrap_or(false) {
        return Err(wifi_off());
    }
    let before: i64 = get(
        &props(&conn, NM, &device.path, I_WIRELESS)
            .await
            .map_err(map)?,
        "LastScan",
    )
    .unwrap_or(0);
    let wifi = proxy(&conn, NM, &device.path, I_WIRELESS)
        .await
        .map_err(map)?;
    let options: HashMap<&str, Value<'_>> = HashMap::new();
    if let Err(e) = wifi.call::<_, _, ()>("RequestScan", &(options,)).await {
        let e = api_error(SVC, &e);
        // NetworkManager refuses a scan right after another ("not allowed"); the list it
        // has is current enough then.
        if matches!(
            e.kind(),
            Some(ErrorCode::NotAuthorized | ErrorCode::Unavailable)
        ) {
            return Err(e);
        }
        debug!("RequestScan: {}", e.message);
    } else {
        let deadline = Instant::now() + SCAN_TIMEOUT;
        while Instant::now() < deadline {
            let now: i64 = get(
                &props(&conn, NM, &device.path, I_WIRELESS)
                    .await
                    .map_err(map)?,
                "LastScan",
            )
            .unwrap_or(0);
            if now > before {
                break;
            }
            sleep(Duration::from_millis(250)).await;
        }
    }
    let list = list_from(&conn, &devices).await?;
    ctx.emit(EventName::WifiListChanged, &list);
    Ok(list)
}

pub async fn enable(ctx: &Ctx, p: WifiEnableParams) -> Result<NetworkStatus, ErrorObject> {
    let conn = ctx.conn(SVC).await?;
    let nm = proxy(&conn, NM, ROOT, I_NM).await.map_err(map)?;
    nm.set_property("WirelessEnabled", p.enabled)
        .await
        .map_err(|e| api_error(SVC, &zbus::Error::from(e)))?;
    // The device follows a moment later.
    for _ in 0..12 {
        let devices = devices(&conn).await?;
        let status = status_from(&conn, &devices).await?;
        let settled = if p.enabled {
            status.wifi_enabled
        } else {
            !status.wifi_enabled && wifi_device(&devices).is_none_or(|d| d.state != STATE_ACTIVATED)
        };
        if settled {
            return Ok(status);
        }
        sleep(Duration::from_millis(250)).await;
    }
    status(ctx).await
}

pub async fn disconnect(ctx: &Ctx) -> Result<NetworkStatus, ErrorObject> {
    let conn = ctx.conn(SVC).await?;
    let devices = devices(&conn).await?;
    if let Some(d) =
        wifi_device(&devices).filter(|d| d.state > STATE_DISCONNECTED && d.state < STATE_FAILED)
    {
        let p = proxy(&conn, NM, &d.path, I_DEVICE).await.map_err(map)?;
        // Disconnect() also stops NetworkManager from rejoining by itself.
        p.call::<_, _, ()>("Disconnect", &()).await.map_err(map)?;
    }
    let devices = self::devices(&conn).await?;
    status_from(&conn, &devices).await
}

pub async fn forget(ctx: &Ctx, p: WifiForgetParams) -> Result<Empty, ErrorObject> {
    let conn = ctx.conn(SVC).await?;
    for s in saved(&conn)
        .await?
        .into_iter()
        .filter(|s| s.ssid == p.ssid.as_bytes())
    {
        let c = proxy(&conn, NM, &s.path, I_CONNECTION).await.map_err(map)?;
        c.call::<_, _, ()>("Delete", &()).await.map_err(map)?;
        info!(ssid = %p.ssid, "forgot a Wi-Fi connection");
    }
    Ok(Empty {})
}

fn invalid(field: &str, reason: &str, message: impl Into<String>) -> ErrorObject {
    ErrorObject::invalid_field(field, reason, message)
}

/// A random version 4 UUID.
fn uuid() -> String {
    let mut b = [0u8; 16];
    // Falls back to the clock if the kernel's random source is unavailable.
    if rustix::rand::getrandom(&mut b, rustix::rand::GetRandomFlags::empty()).is_err() {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        b = n.to_le_bytes();
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = |r: std::ops::Range<usize>| b[r].iter().map(|x| format!("{x:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        h(0..4),
        h(4..6),
        h(6..8),
        h(8..10),
        h(10..16)
    )
}

/// The settings of a new system connection.
fn new_settings<'a>(
    ssid: &'a str,
    security: WifiSecurity,
    password: Option<&'a str>,
    uuid: &'a str,
) -> Settings<'a> {
    let mut s: Settings<'a> = HashMap::new();
    s.insert(
        "connection",
        HashMap::from([
            ("id", Value::from(ssid)),
            ("type", Value::from("802-11-wireless")),
            ("uuid", Value::from(uuid)),
            ("autoconnect", Value::from(true)),
        ]),
    );
    s.insert(
        "802-11-wireless",
        HashMap::from([
            ("ssid", Value::from(ssid.as_bytes().to_vec())),
            ("mode", Value::from("infrastructure")),
        ]),
    );
    if let Some(pw) = password.filter(|_| security != WifiSecurity::Open) {
        let key_mgmt = if security == WifiSecurity::Wpa3 {
            "sae"
        } else {
            "wpa-psk"
        };
        s.insert(
            "802-11-wireless-security",
            HashMap::from([
                ("key-mgmt", Value::from(key_mgmt)),
                ("psk", Value::from(pw)),
            ]),
        );
    }
    s.insert("ipv4", HashMap::from([("method", Value::from("auto"))]));
    s.insert("ipv6", HashMap::from([("method", Value::from("auto"))]));
    s
}

/// A WPA passphrase is 8 to 63 characters, or the 64 hex digits of the raw key.
fn check_password(security: WifiSecurity, password: &str) -> Result<(), ErrorObject> {
    match security {
        WifiSecurity::Wpa => {
            let n = password.chars().count();
            let hex = n == 64 && password.bytes().all(|b| b.is_ascii_hexdigit());
            if (8..=63).contains(&n) || hex {
                Ok(())
            } else {
                Err(invalid(
                    "password",
                    "invalid_length",
                    "a WPA password has 8 to 63 characters",
                ))
            }
        }
        _ => Ok(()),
    }
}

/// The settings path of the connection a device runs.
async fn active_connection(conn: &Connection, device: &PropMap) -> Option<String> {
    let active = path_of(device, "ActiveConnection");
    if is_none_path(&active) {
        return None;
    }
    let a = props(conn, NM, &active, I_ACTIVE).await.ok()?;
    Some(path_of(&a, "Connection"))
}

/// Wait for the device to be activated, or fail. `supplied`: the caller gave a password, so
/// a request for secrets means it was wrong.
async fn wait_activated(
    conn: &Connection,
    device: &str,
    target: &str,
    mut states: zbus::proxy::SignalStream<'_>,
    supplied: bool,
) -> Result<(), ErrorObject> {
    let mut needed_auth = false;
    let deadline = Instant::now() + ACTIVATE_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let msg = match timeout(left.min(Duration::from_secs(2)), states.next()).await {
            Ok(Some(m)) => m,
            Ok(None) => {
                return Err(failed(
                    "activation_failed",
                    "lost the connection to NetworkManager",
                ));
            }
            Err(_) => {
                // No signal for a while: look at the state itself.
                let p = props(conn, NM, device, I_DEVICE).await.map_err(map)?;
                if get::<u32>(&p, "State") == Some(STATE_ACTIVATED)
                    && active_connection(conn, &p).await.as_deref() == Some(target)
                {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(failed("timeout", "the network did not answer in time"));
                }
                continue;
            }
        };
        let Ok((new, _old, reason)) = msg.body().deserialize::<(u32, u32, u32)>() else {
            continue;
        };
        debug!(new, reason, "Wi-Fi device state");
        match new {
            STATE_ACTIVATED => return Ok(()),
            STATE_NEED_AUTH => needed_auth = true,
            STATE_FAILED | STATE_DISCONNECTED => {
                if reason == REASON_NO_SECRETS || (needed_auth && supplied) {
                    return Err(ErrorObject::new(
                        ErrorCode::WrongPassword,
                        "the network did not accept the password",
                    ));
                }
                if new == STATE_DISCONNECTED && !needed_auth {
                    // NetworkManager passes through "disconnected" while it switches
                    // connections; only a failure ends the wait.
                    continue;
                }
                return Err(match reason {
                    REASON_SSID_NOT_FOUND => failed("not_in_range", "the network is not in range"),
                    5 | 6 => failed("no_address", "the network gave no address"),
                    _ => failed(
                        "activation_failed",
                        format!("could not connect (reason {reason})"),
                    ),
                });
            }
            _ => {}
        }
    }
}

pub async fn connect(ctx: &Ctx, p: WifiConnectParams) -> Result<NetworkStatus, ErrorObject> {
    let conn = ctx.conn(SVC).await?;
    let devices = devices(&conn).await?;
    let device = wifi_device(&devices).ok_or_else(no_wifi)?;
    let nm = props(&conn, NM, ROOT, I_NM).await.map_err(map)?;
    if !get(&nm, "WirelessEnabled").unwrap_or(false) {
        return Err(wifi_off());
    }
    let ap = access_points(&conn, device)
        .await?
        .into_iter()
        .filter(|a| a.ssid == p.ssid.as_bytes())
        .max_by_key(|a| a.strength)
        .ok_or_else(|| not_found(format!("no network named {} in range", p.ssid)))?;
    if matches!(ap.security, WifiSecurity::Enterprise | WifiSecurity::Wep) {
        return Err(invalid(
            "ssid",
            "unsupported_security",
            format!(
                "{} uses a security type that cannot be joined with a password",
                p.ssid
            ),
        ));
    }
    let password = p
        .password
        .as_deref()
        .filter(|_| ap.security != WifiSecurity::Open);
    if let Some(pw) = password {
        check_password(ap.security, pw)?;
    }
    let existing = saved(&conn)
        .await?
        .into_iter()
        .find(|s| s.ssid == p.ssid.as_bytes());

    let settings = proxy(&conn, NM, SETTINGS, I_SETTINGS).await.map_err(map)?;
    let (target, created) = match (existing, password) {
        (Some(s), Some(pw)) => {
            // A new password for a stored network replaces the stored one.
            let c = proxy(&conn, NM, &s.path, I_CONNECTION).await.map_err(map)?;
            let current: HashMap<String, PropMap> =
                c.call("GetSettings", &()).await.map_err(map)?;
            let mut update: Settings<'_> = HashMap::new();
            for (section, values) in &current {
                if section == "802-11-wireless-security" {
                    continue;
                }
                let entry = update.entry(section.as_str()).or_default();
                for (k, v) in values {
                    if let Ok(v) = v.try_clone() {
                        entry.insert(k.as_str(), Value::from(v));
                    }
                }
            }
            let key_mgmt = if ap.security == WifiSecurity::Wpa3 {
                "sae"
            } else {
                "wpa-psk"
            };
            update.insert(
                "802-11-wireless-security",
                HashMap::from([
                    ("key-mgmt", Value::from(key_mgmt)),
                    ("psk", Value::from(pw)),
                ]),
            );
            c.call::<_, _, ()>("Update", &(update,))
                .await
                .map_err(map)?;
            (s.path, false)
        }
        (Some(s), None) => (s.path, false),
        (None, pw) => {
            if pw.is_none() && ap.security != WifiSecurity::Open {
                return Err(invalid(
                    "password",
                    "required",
                    format!("{} needs a password", p.ssid),
                ));
            }
            let id = uuid();
            let s = new_settings(&p.ssid, ap.security, pw, &id);
            let path: OwnedObjectPath = settings.call("AddConnection", &(s,)).await.map_err(map)?;
            (path.to_string(), true)
        }
    };

    let dev = proxy(&conn, NM, &device.path, I_DEVICE)
        .await
        .map_err(map)?;
    let states = dev.receive_signal("StateChanged").await.map_err(map)?;
    let nmp = proxy(&conn, NM, ROOT, I_NM).await.map_err(map)?;
    let target_path = OwnedObjectPath::try_from(target.as_str())
        .map_err(|e| failed("bad_path", e.to_string()))?;
    let dev_path = OwnedObjectPath::try_from(device.path.as_str())
        .map_err(|e| failed("bad_path", e.to_string()))?;
    let ap_path = OwnedObjectPath::try_from(ap.path.as_str())
        .map_err(|e| failed("bad_path", e.to_string()))?;
    let result = match nmp
        .call::<_, _, OwnedObjectPath>("ActivateConnection", &(&target_path, &dev_path, &ap_path))
        .await
    {
        Ok(_) => wait_activated(&conn, &device.path, &target, states, password.is_some()).await,
        Err(e) => Err(map(e)),
    };
    if let Err(e) = &result {
        warn!(ssid = %p.ssid, code = e.code, "Wi-Fi connect failed: {}", e.message);
        if created && let Ok(c) = proxy(&conn, NM, &target, I_CONNECTION).await {
            // Do not leave a connection with a wrong password behind.
            let _ = c.call::<_, _, ()>("Delete", &()).await;
        }
    }
    result?;
    info!(ssid = %p.ssid, "joined a Wi-Fi network");
    let devices = self::devices(&conn).await?;
    status_from(&conn, &devices).await
}

/// The list as compared for `wifi_list_changed`: signal in steps of 5.
fn coarse(list: &WifiList) -> WifiList {
    WifiList {
        networks: list
            .networks
            .iter()
            .map(|n| WifiNetwork {
                signal: n.signal / 5 * 5,
                ..n.clone()
            })
            .collect(),
    }
}

/// Send `network_changed` and `wifi_list_changed` when NetworkManager reports changes.
pub async fn watch(ctx: Arc<Ctx>) {
    loop {
        let Ok(conn) = ctx.conn(SVC).await else {
            sleep(Duration::from_secs(10)).await;
            continue;
        };
        let Ok(mut stream) = dbus::changes(&conn, NM).await else {
            sleep(Duration::from_secs(10)).await;
            continue;
        };
        let mut last: Option<(NetworkStatus, WifiList)> =
            snapshot(&conn).await.ok().map(|(s, l)| (s, coarse(&l)));
        while stream.next().await.is_some() {
            dbus::settle(&mut stream, Duration::from_millis(400)).await;
            if !ctx.has_clients() {
                last = None;
                continue;
            }
            match snapshot(&conn).await {
                Ok((status, list)) => {
                    let list = coarse(&list);
                    let (old_status, old_list) = match &last {
                        Some((s, l)) => (Some(s), Some(l)),
                        None => (None, None),
                    };
                    if old_status != Some(&status) {
                        ctx.emit(EventName::NetworkChanged, &status);
                    }
                    if old_list != Some(&list) {
                        ctx.emit(EventName::WifiListChanged, &list);
                    }
                    last = Some((status, list));
                }
                Err(_) => last = None,
            }
        }
        sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_from_flags() {
        assert_eq!(security(0, 0, 0), WifiSecurity::Open);
        assert_eq!(security(1, 0, 0), WifiSecurity::Wep);
        assert_eq!(security(1, SEC_KEY_PSK, SEC_KEY_PSK), WifiSecurity::Wpa);
        assert_eq!(security(1, 0, SEC_KEY_PSK | SEC_KEY_SAE), WifiSecurity::Wpa);
        assert_eq!(security(1, 0, SEC_KEY_SAE), WifiSecurity::Wpa3);
        assert_eq!(security(1, 0, SEC_KEY_8021X), WifiSecurity::Enterprise);
        // OWE and the like are not joinable with a password.
        assert_eq!(security(1, 0, 0x800), WifiSecurity::Enterprise);
    }

    #[test]
    fn passwords() {
        assert!(check_password(WifiSecurity::Wpa, "12345678").is_ok());
        assert!(check_password(WifiSecurity::Wpa, "1234567").is_err());
        assert!(check_password(WifiSecurity::Wpa, &"a".repeat(64)).is_ok());
        assert!(check_password(WifiSecurity::Wpa, &"g".repeat(64)).is_err());
        assert!(check_password(WifiSecurity::Wpa3, "x").is_ok());
    }

    #[test]
    fn uuids_look_right() {
        let a = uuid();
        assert_eq!(a.len(), 36);
        assert_eq!(a.as_bytes()[14], b'4');
        assert_ne!(a, uuid());
    }

    #[test]
    fn settings_do_not_carry_a_password_for_open_networks() {
        let s = new_settings("Cafe", WifiSecurity::Open, Some("ignored"), "u");
        assert!(!s.contains_key("802-11-wireless-security"));
        let s = new_settings("Home", WifiSecurity::Wpa, Some("hunter2hunter2"), "u");
        assert!(s.contains_key("802-11-wireless-security"));
    }
}
