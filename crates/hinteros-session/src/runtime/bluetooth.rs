//! Bluetooth through BlueZ's D-Bus API, as the session user.
//!
//! The daemon registers a pairing agent with the `DisplayYesNo` capability: devices without
//! input or output (controllers, most headsets) pair as "just works" without any prompt, a
//! numeric comparison reaches the launcher as `bluetooth_confirm` and is answered with
//! `bluetooth.confirm`, and a passkey the user must type on the device is shown the same way.
//! The agent only says yes while the user is scanning or pairing: BlueZ would otherwise let any
//! device in range that starts pairing ask it. A device that pairs is trusted so that it
//! reconnects by itself.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use hinteros_api::session::{
    BT_SCAN_TIMEOUT_DEFAULT_S, BT_SCAN_TIMEOUT_MAX_S, BtAddressParams, BtChanged, BtConfirmMode,
    BtConfirmParams, BtConfirmRequest, BtDevice, BtDevices, BtKind, BtScanParams, BtStatus, Empty,
    EventName, Service, valid_bt_address,
};
use hinteros_api::{ErrorCode, ErrorObject};
use tokio::sync::oneshot;
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn};
use zbus::Connection;
use zbus::fdo::ObjectManagerProxy;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

use super::Ctx;
use super::dbus::{self, PropMap, api_error, failed, get, not_found, proxy};

const BLUEZ: &str = "org.bluez";
const SVC: Service = Service::Bluez;
const I_ADAPTER: &str = "org.bluez.Adapter1";
const I_DEVICE: &str = "org.bluez.Device1";
const I_BATTERY: &str = "org.bluez.Battery1";
const I_AGENT_MANAGER: &str = "org.bluez.AgentManager1";
pub const AGENT_PATH: &str = "/org/hinteros/BluetoothAgent";
/// The most `pair_timeout_s` may give `Pair`, so that trusting and connecting afterwards still
/// fit in the 90 s limit of `bluetooth.pair`.
const PAIR_TIMEOUT_MAX_S: u64 = 60;

/// What the methods, the watcher and the agent share.
#[derive(Clone)]
pub struct Shared {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    /// Numeric comparisons waiting for `bluetooth.confirm`.
    confirms: HashMap<String, oneshot::Sender<bool>>,
    /// Addresses of devices `bluetooth.pair` is working on.
    pairing: HashSet<String>,
    /// We started discovery and it has not been stopped.
    scanning: bool,
    /// Bumped on every scan start and stop, so a stale timeout does nothing.
    scan_generation: u64,
    /// The user has scanned or paired, so our agent belongs on BlueZ. Until then the daemon
    /// leaves the default agent alone (a desktop preview has its own).
    agent_wanted: bool,
    /// Registered with the BlueZ that runs now.
    agent_registered: bool,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
        }
    }
}

impl Shared {
    fn with<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        f(&mut self.inner.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Whether the agent may say yes to pairing requests from `address` now.
    fn pairing_allowed(&self, address: &str) -> bool {
        self.with(|i| i.scanning || i.pairing.contains(address))
    }

    /// Let the agent answer for `address` until the guard is dropped.
    fn start_pairing(&self, address: &str) -> PairingGuard {
        self.with(|i| i.pairing.insert(address.to_owned()));
        PairingGuard {
            shared: self.clone(),
            address: address.to_owned(),
            cancel: None,
        }
    }
}

/// Ends the agent's permission for one address when `bluetooth.pair` finishes, and also when
/// its time limit drops the call halfway: otherwise the agent would keep saying yes (and PIN
/// 0000) to that address for the rest of the session.
struct PairingGuard {
    shared: Shared,
    address: String,
    /// The device while its `Pair` call runs, to cancel it if the call is dropped.
    cancel: Option<zbus::Proxy<'static>>,
}

impl Drop for PairingGuard {
    fn drop(&mut self) {
        self.shared.with(|i| i.pairing.remove(&self.address));
        if let Some(dev) = self.cancel.take()
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            rt.spawn(async move {
                let _ = dev.call::<_, _, ()>("CancelPairing", &()).await;
            });
        }
    }
}

fn map(e: zbus::Error) -> ErrorObject {
    bt_error(&e)
}

/// BlueZ's own error names as `device_failed` reasons.
fn bt_error(e: &zbus::Error) -> ErrorObject {
    if let zbus::Error::MethodError(name, desc, _) = e {
        let text = desc.clone().unwrap_or_default();
        let (reason, message) = match name.as_str() {
            "org.bluez.Error.AuthenticationFailed" => {
                ("auth_failed", "the device did not accept the pairing")
            }
            "org.bluez.Error.AuthenticationRejected" => ("rejected", "pairing was rejected"),
            "org.bluez.Error.AuthenticationCanceled" => ("canceled", "pairing was canceled"),
            "org.bluez.Error.AuthenticationTimeout" => ("timeout", "pairing timed out"),
            "org.bluez.Error.ConnectionAttemptFailed" => {
                ("connect_failed", "could not connect to the device")
            }
            "org.bluez.Error.InProgress" => ("in_progress", "another operation is in progress"),
            "org.bluez.Error.NotReady" => ("not_ready", "the Bluetooth adapter is not ready"),
            "org.bluez.Error.NotAvailable" => ("not_available", "the device is not available"),
            "org.bluez.Error.Failed" => ("failed", ""),
            _ => return api_error(SVC, e),
        };
        let message = if message.is_empty() {
            text
        } else {
            message.to_owned()
        };
        let message = if message.is_empty() {
            "Bluetooth operation failed".to_owned()
        } else {
            message
        };
        return failed(reason, message);
    }
    api_error(SVC, e)
}

// ----------------------------------------------------------------------------------- objects

struct Adapter {
    path: String,
    props: PropMap,
}

struct Dev {
    path: String,
    props: PropMap,
    battery: Option<u8>,
}

struct Objects {
    adapter: Option<Adapter>,
    devices: Vec<Dev>,
}

async fn objects(conn: &Connection) -> Result<Objects, ErrorObject> {
    let om = ObjectManagerProxy::builder(conn)
        .destination(BLUEZ)
        .map_err(map)?
        .path("/")
        .map_err(map)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(map)?;
    let all = om
        .get_managed_objects()
        .await
        .map_err(|e| map(zbus::Error::from(e)))?;
    let mut adapters = Vec::new();
    let mut devices = Vec::new();
    for (path, ifaces) in all {
        let mut by_name: HashMap<String, PropMap> = HashMap::new();
        for (name, props) in ifaces {
            by_name.insert(name.to_string(), props);
        }
        if let Some(props) = by_name.remove(I_ADAPTER) {
            adapters.push(Adapter {
                path: path.to_string(),
                props,
            });
        } else if let Some(props) = by_name.remove(I_DEVICE) {
            let battery = by_name
                .get(I_BATTERY)
                .and_then(|b| get::<u8>(b, "Percentage"));
            devices.push(Dev {
                path: path.to_string(),
                props,
                battery,
            });
        }
    }
    adapters.sort_by(|a, b| a.path.cmp(&b.path));
    // The powered adapter, else the first.
    let adapter = {
        let pos = adapters
            .iter()
            .position(|a| get::<bool>(&a.props, "Powered").unwrap_or(false))
            .unwrap_or(0);
        (!adapters.is_empty()).then(|| adapters.swap_remove(pos))
    };
    devices.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Objects { adapter, devices })
}

fn status_of(o: &Objects) -> BtStatus {
    match &o.adapter {
        Some(a) => BtStatus {
            available: true,
            powered: get(&a.props, "Powered").unwrap_or(false),
            scanning: get(&a.props, "Discovering").unwrap_or(false),
        },
        None => BtStatus {
            available: false,
            powered: false,
            scanning: false,
        },
    }
}

/// The kind of device, from BlueZ's icon name, else its class of device, else its GAP
/// appearance.
pub fn kind_of(icon: Option<&str>, class: Option<u32>, appearance: Option<u16>) -> BtKind {
    match icon {
        Some("input-gaming") => return BtKind::Gamepad,
        Some("audio-headset" | "audio-headphones") => return BtKind::Headset,
        Some("audio-speakers") => return BtKind::Speaker,
        Some("audio-card") => return BtKind::Speaker,
        Some("input-keyboard") => return BtKind::Keyboard,
        Some("input-mouse" | "input-tablet") => return BtKind::Mouse,
        Some("phone") => return BtKind::Phone,
        Some("computer") => return BtKind::Computer,
        _ => {}
    }
    if let Some(class) = class {
        let major = (class >> 8) & 0x1f;
        let minor = (class >> 2) & 0x3f;
        match major {
            // Audio/video: headset, hands-free, headphones, portable audio versus speakers.
            0x04 => match minor {
                0x01 | 0x02 | 0x06 => return BtKind::Headset,
                0x05 | 0x07 | 0x0a | 0x0b => return BtKind::Speaker,
                _ => {}
            },
            // Peripheral: keyboard and pointing bits, then joystick and gamepad.
            0x05 => {
                let kind = (class >> 6) & 0x3;
                let sub = (class >> 2) & 0xf;
                if matches!(sub, 0x01 | 0x02) {
                    return BtKind::Gamepad;
                }
                match kind {
                    1 | 3 => return BtKind::Keyboard,
                    2 => return BtKind::Mouse,
                    _ => {}
                }
            }
            0x02 => return BtKind::Phone,
            0x01 => return BtKind::Computer,
            _ => {}
        }
    }
    match appearance {
        Some(0x03c1) => BtKind::Keyboard,
        Some(0x03c2) => BtKind::Mouse,
        Some(0x03c3 | 0x03c4) => BtKind::Gamepad,
        Some(0x0941 | 0x0942) => BtKind::Headset,
        Some(0x0040) => BtKind::Phone,
        Some(0x0080) => BtKind::Computer,
        _ => BtKind::Other,
    }
}

fn device_of(d: &Dev) -> Option<BtDevice> {
    let paired = get::<bool>(&d.props, "Paired").unwrap_or(false)
        || get::<bool>(&d.props, "Bonded").unwrap_or(false);
    let name = get::<String>(&d.props, "Alias")
        .or_else(|| get::<String>(&d.props, "Name"))
        .filter(|n| !n.is_empty());
    let named = get::<String>(&d.props, "Name").is_some();
    // Nearby devices that never told us a name are noise.
    if !paired && !named {
        return None;
    }
    let address = get::<String>(&d.props, "Address")?.to_ascii_uppercase();
    Some(BtDevice {
        name: name.unwrap_or_else(|| address.clone()),
        kind: kind_of(
            get::<String>(&d.props, "Icon").as_deref(),
            get(&d.props, "Class"),
            get(&d.props, "Appearance"),
        ),
        paired,
        connected: get(&d.props, "Connected").unwrap_or(false),
        trusted: get(&d.props, "Trusted").unwrap_or(false),
        battery: d.battery.map(|b| b.min(100)),
        rssi: get::<i16>(&d.props, "RSSI"),
        address,
    })
}

fn devices_of(o: &Objects) -> Vec<BtDevice> {
    let mut v: Vec<BtDevice> = o.devices.iter().filter_map(device_of).collect();
    v.sort_by(|a, b| {
        b.paired
            .cmp(&a.paired)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.address.cmp(&b.address))
    });
    v
}

fn find<'a>(o: &'a Objects, address: &str) -> Result<&'a Dev, ErrorObject> {
    o.devices
        .iter()
        .find(|d| {
            get::<String>(&d.props, "Address").is_some_and(|a| a.eq_ignore_ascii_case(address))
        })
        .ok_or_else(|| not_found(format!("no Bluetooth device {address}")))
}

fn check_address(address: &str) -> Result<(), ErrorObject> {
    if valid_bt_address(address) {
        Ok(())
    } else {
        Err(ErrorObject::invalid_field(
            "address",
            "invalid_address",
            format!("'{address}' is not a Bluetooth address (AA:BB:CC:DD:EE:FF)"),
        ))
    }
}

// ------------------------------------------------------------------------------------ methods

pub async fn status(ctx: &Ctx) -> Result<BtStatus, ErrorObject> {
    let conn = ctx.conn(SVC).await?;
    Ok(status_of(&objects(&conn).await?))
}

pub async fn devices(ctx: &Ctx) -> Result<BtDevices, ErrorObject> {
    let conn = ctx.conn(SVC).await?;
    Ok(BtDevices {
        devices: devices_of(&objects(&conn).await?),
    })
}

pub async fn scan(ctx: &Arc<Ctx>, p: BtScanParams) -> Result<BtStatus, ErrorObject> {
    let conn = ctx.conn(SVC).await?;
    let o = objects(&conn).await?;
    let adapter = o
        .adapter
        .as_ref()
        .ok_or_else(|| failed("no_adapter", "there is no Bluetooth adapter"))?;
    let a = proxy(&conn, BLUEZ, &adapter.path, I_ADAPTER)
        .await
        .map_err(map)?;
    let mut status = status_of(&o);
    if p.enabled {
        ensure_agent(ctx, &conn).await?;
        let secs = p
            .timeout_s
            .unwrap_or(BT_SCAN_TIMEOUT_DEFAULT_S)
            .clamp(1, BT_SCAN_TIMEOUT_MAX_S);
        if !status.powered {
            a.set_property("Powered", true)
                .await
                .map_err(|e| map(zbus::Error::from(e)))?;
            status.powered = true;
        }
        match a.call::<_, _, ()>("StartDiscovery", &()).await {
            Ok(()) => {}
            // We already scan (the same client asked twice).
            Err(zbus::Error::MethodError(n, _, _))
                if n.as_str() == "org.bluez.Error.InProgress" => {}
            Err(e) => return Err(map(e)),
        }
        let generation = ctx.bt.with(|i| {
            i.scanning = true;
            i.scan_generation += 1;
            i.scan_generation
        });
        // Stop by itself: a TV should not scan forever.
        let shared = ctx.bt.clone();
        let path = adapter.path.clone();
        let conn2 = conn.clone();
        tokio::spawn(async move {
            sleep(Duration::from_secs(u64::from(secs))).await;
            if shared.with(|i| i.scan_generation == generation && std::mem::take(&mut i.scanning)) {
                stop_discovery(&conn2, &path).await;
            }
        });
        status.scanning = true;
    } else {
        ctx.bt.with(|i| {
            i.scanning = false;
            i.scan_generation += 1;
        });
        stop_discovery(&conn, &adapter.path).await;
        status.scanning = false;
    }
    Ok(status)
}

async fn stop_discovery(conn: &Connection, adapter: &str) {
    if let Ok(a) = proxy(conn, BLUEZ, adapter, I_ADAPTER).await
        && let Err(e) = a.call::<_, _, ()>("StopDiscovery", &()).await
    {
        debug!("StopDiscovery: {e}");
    }
}

async fn device_proxy(conn: &Connection, d: &Dev) -> Result<zbus::Proxy<'static>, ErrorObject> {
    proxy(conn, BLUEZ, &d.path, I_DEVICE).await.map_err(map)
}

/// The device as it is now.
async fn snapshot(conn: &Connection, address: &str) -> Result<BtDevice, ErrorObject> {
    let o = objects(conn).await?;
    let d = find(&o, address)?;
    device_of(d).ok_or_else(|| not_found(format!("no Bluetooth device {address}")))
}

pub async fn pair(ctx: &Arc<Ctx>, p: BtAddressParams) -> Result<BtDevice, ErrorObject> {
    check_address(&p.address)?;
    let conn = ctx.conn(SVC).await?;
    ensure_agent(ctx, &conn).await?;
    let o = objects(&conn).await?;
    let d = find(&o, &p.address)?;
    let dev = device_proxy(&conn, d).await?;
    let paired = get::<bool>(&d.props, "Paired").unwrap_or(false);
    let mut guard = ctx.bt.start_pairing(&p.address);
    let result = async {
        if !paired {
            // Leave room in the method's time limit for the steps after `Pair`.
            let limit = Duration::from_secs(ctx.bluetooth.pair_timeout_s.min(PAIR_TIMEOUT_MAX_S));
            guard.cancel = Some(dev.clone());
            let outcome = timeout(limit, dev.call::<_, _, ()>("Pair", &())).await;
            guard.cancel = None;
            match outcome {
                Ok(Ok(())) => {}
                // Paired meanwhile (the device started it).
                Ok(Err(zbus::Error::MethodError(n, _, _)))
                    if n.as_str() == "org.bluez.Error.AlreadyExists" => {}
                Ok(Err(e)) => return Err(map(e)),
                Err(_) => {
                    let _ = dev.call::<_, _, ()>("CancelPairing", &()).await;
                    return Err(failed("timeout", "pairing timed out"));
                }
            }
        }
        // Trusted: it may reconnect by itself, which is what a controller must do.
        dev.set_property("Trusted", true)
            .await
            .map_err(|e| map(zbus::Error::from(e)))?;
        // Most controllers connect right after pairing; headsets may need asking.
        if !snapshot(&conn, &p.address).await.is_ok_and(|d| d.connected) {
            match timeout(
                Duration::from_secs(20),
                dev.call::<_, _, ()>("Connect", &()),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => info!(address = %p.address, "paired, but connecting failed: {e}"),
                Err(_) => info!(address = %p.address, "paired, but connecting timed out"),
            }
        }
        snapshot(&conn, &p.address).await
    }
    .await;
    drop(guard);
    if let Err(e) = &result {
        warn!(address = %p.address, code = e.code, "pairing failed: {}", e.message);
    }
    result
}

pub async fn confirm(ctx: &Ctx, p: BtConfirmParams) -> Result<Empty, ErrorObject> {
    check_address(&p.address)?;
    match ctx.bt.with(|i| i.confirms.remove(&p.address)) {
        Some(tx) => {
            let _ = tx.send(p.accept);
            Ok(Empty {})
        }
        None => Err(not_found(format!(
            "no pairing of {} waits for confirmation",
            p.address
        ))),
    }
}

pub async fn connect(ctx: &Ctx, p: BtAddressParams) -> Result<BtDevice, ErrorObject> {
    check_address(&p.address)?;
    let conn = ctx.conn(SVC).await?;
    let o = objects(&conn).await?;
    let dev = device_proxy(&conn, find(&o, &p.address)?).await?;
    match timeout(
        Duration::from_secs(25),
        dev.call::<_, _, ()>("Connect", &()),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(map(e)),
        Err(_) => return Err(failed("timeout", "the device did not answer")),
    }
    snapshot(&conn, &p.address).await
}

pub async fn disconnect(ctx: &Ctx, p: BtAddressParams) -> Result<BtDevice, ErrorObject> {
    check_address(&p.address)?;
    let conn = ctx.conn(SVC).await?;
    let o = objects(&conn).await?;
    let dev = device_proxy(&conn, find(&o, &p.address)?).await?;
    dev.call::<_, _, ()>("Disconnect", &()).await.map_err(map)?;
    snapshot(&conn, &p.address).await
}

pub async fn forget(ctx: &Ctx, p: BtAddressParams) -> Result<Empty, ErrorObject> {
    check_address(&p.address)?;
    let conn = ctx.conn(SVC).await?;
    let o = objects(&conn).await?;
    let d = find(&o, &p.address)?;
    let adapter = o
        .adapter
        .as_ref()
        .ok_or_else(|| failed("no_adapter", "there is no Bluetooth adapter"))?;
    let a = proxy(&conn, BLUEZ, &adapter.path, I_ADAPTER)
        .await
        .map_err(map)?;
    let path = OwnedObjectPath::try_from(d.path.as_str())
        .map_err(|e| failed("bad_path", e.to_string()))?;
    a.call::<_, _, ()>("RemoveDevice", &(path,))
        .await
        .map_err(map)?;
    Ok(Empty {})
}

// -------------------------------------------------------------------------------------- agent

/// The address in a device's object path (`.../dev_AA_BB_CC_DD_EE_FF`).
pub fn address_of_path(path: &str) -> Option<String> {
    let name = path.rsplit('/').next()?.strip_prefix("dev_")?;
    let address = name.replace('_', ":");
    valid_bt_address(&address).then_some(address)
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
}

struct Agent {
    ctx: Arc<Ctx>,
}

impl Agent {
    fn address(device: &ObjectPath<'_>) -> Result<String, AgentError> {
        address_of_path(device.as_str())
            .ok_or_else(|| AgentError::Rejected("unknown device".into()))
    }

    async fn name(&self, device: &ObjectPath<'_>, address: &str) -> String {
        let Ok(conn) = self.ctx.conn(SVC).await else {
            return address.to_owned();
        };
        match dbus::props(&conn, BLUEZ, device.as_str(), I_DEVICE).await {
            Ok(p) => get::<String>(&p, "Alias").unwrap_or_else(|| address.to_owned()),
            Err(_) => address.to_owned(),
        }
    }

    fn allowed(&self, address: &str) -> Result<(), AgentError> {
        if self.ctx.bt.pairing_allowed(address) {
            Ok(())
        } else {
            info!(%address, "refusing a pairing request nobody asked for");
            Err(AgentError::Rejected("not pairing".into()))
        }
    }
}

#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    fn release(&self) {}

    fn cancel(&self) {
        // A request that is still waiting for the user ends: drop the waiters so they reject.
        self.ctx.bt.with(|i| i.confirms.clear());
    }

    async fn request_pin_code(&self, device: ObjectPath<'_>) -> Result<String, AgentError> {
        let address = Self::address(&device)?;
        self.allowed(&address)?;
        // Legacy pairing: the default PIN of devices that have none to show.
        Ok("0000".into())
    }

    async fn display_pin_code(
        &self,
        device: ObjectPath<'_>,
        pincode: String,
    ) -> Result<(), AgentError> {
        let address = Self::address(&device)?;
        self.allowed(&address)?;
        if pincode.len() == 6 && pincode.bytes().all(|b| b.is_ascii_digit()) {
            self.show(&device, address, pincode, BtConfirmMode::Display)
                .await;
        }
        Ok(())
    }

    async fn request_passkey(&self, device: ObjectPath<'_>) -> Result<u32, AgentError> {
        // Typing a passkey into the daemon is not something a TV remote offers.
        let _ = Self::address(&device)?;
        Err(AgentError::Rejected(
            "passkey entry is not supported".into(),
        ))
    }

    async fn display_passkey(
        &self,
        device: ObjectPath<'_>,
        passkey: u32,
        entered: u16,
    ) -> Result<(), AgentError> {
        let address = Self::address(&device)?;
        self.allowed(&address)?;
        if entered == 0 {
            self.show(
                &device,
                address,
                format!("{:06}", passkey % 1_000_000),
                BtConfirmMode::Display,
            )
            .await;
        }
        Ok(())
    }

    async fn request_confirmation(
        &self,
        device: ObjectPath<'_>,
        passkey: u32,
    ) -> Result<(), AgentError> {
        let address = Self::address(&device)?;
        self.allowed(&address)?;
        let (tx, rx) = oneshot::channel();
        self.ctx.bt.with(|i| i.confirms.insert(address.clone(), tx));
        self.show(
            &device,
            address.clone(),
            format!("{:06}", passkey % 1_000_000),
            BtConfirmMode::Confirm,
        )
        .await;
        let answer = timeout(
            Duration::from_secs(self.ctx.bluetooth.confirm_timeout_s),
            rx,
        )
        .await;
        self.ctx.bt.with(|i| i.confirms.remove(&address));
        match answer {
            Ok(Ok(true)) => Ok(()),
            _ => Err(AgentError::Rejected("not confirmed".into())),
        }
    }

    async fn request_authorization(&self, device: ObjectPath<'_>) -> Result<(), AgentError> {
        let address = Self::address(&device)?;
        self.allowed(&address)
    }

    async fn authorize_service(
        &self,
        device: ObjectPath<'_>,
        _uuid: String,
    ) -> Result<(), AgentError> {
        let _ = Self::address(&device)?;
        let conn = self
            .ctx
            .conn(SVC)
            .await
            .map_err(|_| AgentError::Rejected("no bus".into()))?;
        // Services of devices we paired; anything else asks for nothing it has not earned.
        match dbus::props(&conn, BLUEZ, device.as_str(), I_DEVICE).await {
            Ok(p) if get::<bool>(&p, "Paired").unwrap_or(false) => Ok(()),
            _ => Err(AgentError::Rejected("not paired".into())),
        }
    }
}

impl Agent {
    async fn show(
        &self,
        device: &ObjectPath<'_>,
        address: String,
        passkey: String,
        mode: BtConfirmMode,
    ) {
        let name = self.name(device, &address).await;
        self.ctx.emit(
            EventName::BluetoothConfirm,
            &BtConfirmRequest {
                address,
                name,
                passkey,
                mode,
            },
        );
    }
}

/// Put the agent on BlueZ (once per BlueZ run) and keep it there from now on.
async fn ensure_agent(ctx: &Arc<Ctx>, conn: &Connection) -> Result<(), ErrorObject> {
    ctx.bt.with(|i| i.agent_wanted = true);
    if ctx.bt.with(|i| i.agent_registered) {
        return Ok(());
    }
    register_agent(ctx, conn).await?;
    ctx.bt.with(|i| i.agent_registered = true);
    Ok(())
}

/// Serve the agent object and register it with BlueZ as the default agent.
async fn register_agent(ctx: &Arc<Ctx>, conn: &Connection) -> Result<(), ErrorObject> {
    let _ = conn
        .object_server()
        .at(AGENT_PATH, Agent { ctx: ctx.clone() })
        .await
        .map_err(|e| failed("agent", e.to_string()))?;
    let manager = proxy(conn, BLUEZ, "/org/bluez", I_AGENT_MANAGER)
        .await
        .map_err(map)?;
    let path = ObjectPath::try_from(AGENT_PATH).map_err(|e| failed("agent", e.to_string()))?;
    match manager
        .call::<_, _, ()>("RegisterAgent", &(&path, "DisplayYesNo"))
        .await
    {
        Ok(()) => {}
        Err(zbus::Error::MethodError(n, _, _)) if n.as_str() == "org.bluez.Error.AlreadyExists" => {
        }
        Err(e) => return Err(map(e)),
    }
    manager
        .call::<_, _, ()>("RequestDefaultAgent", &(&path,))
        .await
        .map_err(map)?;
    info!("registered the Bluetooth pairing agent");
    Ok(())
}

// ------------------------------------------------------------------------------------ watcher

/// The state as compared for `bluetooth_changed`: signal strength in steps of 10 dBm.
fn coarse(c: &BtChanged) -> BtChanged {
    BtChanged {
        status: c.status.clone(),
        devices: c
            .devices
            .iter()
            .map(|d| BtDevice {
                rssi: d.rssi.map(|r| r / 10 * 10),
                ..d.clone()
            })
            .collect(),
    }
}

async fn changed(conn: &Connection) -> Result<BtChanged, ErrorObject> {
    let o = objects(conn).await?;
    Ok(BtChanged {
        status: status_of(&o),
        devices: devices_of(&o),
    })
}

/// Keep the agent registered and send `bluetooth_changed`.
pub async fn watch(ctx: Arc<Ctx>) {
    loop {
        let Ok((conn, number)) = ctx.watched_conn(SVC).await else {
            sleep(Duration::from_secs(10)).await;
            continue;
        };
        let Ok(mut stream) = dbus::changes(&conn, BLUEZ).await else {
            ctx.conn_lost(number).await;
            sleep(Duration::from_secs(10)).await;
            continue;
        };
        let mut last: Option<BtChanged> = None;
        let mut known_owner: Option<String> = None;
        let mut tick = true;
        while tick {
            // A restarted BlueZ has forgotten our agent, however fast it came back.
            let now_owner = dbus::owner(&conn, BLUEZ).await;
            if now_owner != known_owner {
                ctx.bt.with(|i| i.agent_registered = false);
                known_owner = now_owner;
            }
            match changed(&conn).await {
                Ok(now) => {
                    // BlueZ forgets agents when it restarts: put ours back if the user had it.
                    if ctx.bt.with(|i| i.agent_wanted && !i.agent_registered) {
                        match ensure_agent(&ctx, &conn).await {
                            Ok(()) => {}
                            Err(e) => warn!("Bluetooth pairing agent: {}", e.message),
                        }
                    }
                    let now = coarse(&now);
                    if last.as_ref().is_some_and(|l| *l != now) && ctx.has_clients() {
                        ctx.emit(EventName::BluetoothChanged, &now);
                    }
                    last = Some(now);
                }
                Err(e) => {
                    if e.kind() != Some(ErrorCode::Unavailable) {
                        debug!("Bluetooth state: {}", e.message);
                    }
                    ctx.bt.with(|i| i.agent_registered = false);
                    last = None;
                }
            }
            tick = stream.next().await.is_some();
            dbus::settle(&mut stream, Duration::from_millis(300)).await;
        }
        // The stream ends only when the connection closes.
        ctx.conn_lost(number).await;
        sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(kind_of(Some("input-gaming"), None, None), BtKind::Gamepad);
        assert_eq!(
            kind_of(Some("audio-headphones"), None, None),
            BtKind::Headset
        );
        assert_eq!(
            kind_of(Some("input-keyboard"), None, None),
            BtKind::Keyboard
        );
        // Class of device: audio/video headphones, peripheral gamepad and keyboard.
        assert_eq!(kind_of(None, Some(0x0418), None), BtKind::Headset);
        assert_eq!(kind_of(None, Some(0x0508), None), BtKind::Gamepad);
        assert_eq!(kind_of(None, Some(0x0540), None), BtKind::Keyboard);
        assert_eq!(kind_of(None, Some(0x0580), None), BtKind::Mouse);
        // GAP appearance of a HID gamepad.
        assert_eq!(kind_of(None, None, Some(0x03c4)), BtKind::Gamepad);
        assert_eq!(kind_of(Some("unknown"), None, None), BtKind::Other);
    }

    #[test]
    fn paths_and_addresses() {
        assert_eq!(
            address_of_path("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_0F").as_deref(),
            Some("AA:BB:CC:DD:EE:0F")
        );
        assert_eq!(address_of_path("/org/bluez/hci0"), None);
    }

    #[test]
    fn agent_only_answers_while_pairing_or_scanning() {
        let s = Shared::default();
        assert!(!s.pairing_allowed("AA:BB:CC:DD:EE:01"));
        s.with(|i| i.scanning = true);
        assert!(s.pairing_allowed("AA:BB:CC:DD:EE:01"));
        s.with(|i| i.scanning = false);
        s.with(|i| i.pairing.insert("AA:BB:CC:DD:EE:01".into()));
        assert!(s.pairing_allowed("AA:BB:CC:DD:EE:01"));
    }

    #[tokio::test]
    async fn a_dropped_pair_call_stops_the_agent_answering() {
        let s = Shared::default();
        let guard = s.start_pairing("AA:BB:CC:DD:EE:02");
        let call = async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        };
        // The method's time limit drops the call before it ends.
        assert!(timeout(Duration::from_millis(10), call).await.is_err());
        assert!(!s.pairing_allowed("AA:BB:CC:DD:EE:02"));
    }
}
