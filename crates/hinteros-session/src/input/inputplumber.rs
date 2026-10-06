//! InputPlumber over D-Bus (zbus): makes it manage all pads, sets every composite device's
//! intercept mode, and forwards `InputEvent` signals.
//!
//! InputPlumber forgets `ManageAllDevices` and the modes when it restarts, so both are
//! re-applied whenever its bus name gets a new owner, and the mode is applied to every
//! composite device that appears later (one per pad). If there is no bus or no InputPlumber,
//! this logs and waits; the rest of the daemon works without it.

use std::collections::HashSet;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};
use zbus::fdo::{DBusProxy, ObjectManagerProxy};
use zbus::message::Type as MessageType;
use zbus::names::BusName;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};
use zbus::{Connection, MatchRule, MessageStream, Proxy};

use super::{InputControl, InterceptMode, RawInput};

pub const SERVICE: &str = "org.shadowblip.InputPlumber";
/// Where InputPlumber's ObjectManager lives; all its objects are below.
pub const ROOT: &str = "/org/shadowblip/InputPlumber";
pub const MANAGER_PATH: &str = "/org/shadowblip/InputPlumber/Manager";
pub const MANAGER_IFACE: &str = "org.shadowblip.InputManager";
pub const COMPOSITE_IFACE: &str = "org.shadowblip.Input.CompositeDevice";
pub const DBUS_DEVICE_IFACE: &str = "org.shadowblip.Input.DBusDevice";

/// [`InputControl`] backed by the InputPlumber task: the latest mode wins.
pub struct InputPlumber {
    mode: watch::Sender<InterceptMode>,
}

impl InputControl for InputPlumber {
    fn set_mode(&mut self, mode: InterceptMode) {
        self.mode
            .send_if_modified(|m| std::mem::replace(m, mode) != mode);
    }
}

/// Start the InputPlumber task. `bus` is a D-Bus address, `None` for the system bus.
pub fn spawn(
    bus: Option<String>,
    initial: InterceptMode,
    events: mpsc::UnboundedSender<RawInput>,
) -> InputPlumber {
    let (tx, rx) = watch::channel(initial);
    tokio::spawn(async move {
        if let Err(e) = run(bus, rx, events).await {
            warn!("InputPlumber integration stopped: {e:#}");
        }
    });
    InputPlumber { mode: tx }
}

async fn connect(bus: Option<&str>) -> zbus::Result<Connection> {
    match bus {
        None => Connection::system().await,
        Some(address) => zbus::connection::Builder::address(address)?.build().await,
    }
}

async fn run(
    bus: Option<String>,
    mut mode: watch::Receiver<InterceptMode>,
    events: mpsc::UnboundedSender<RawInput>,
) -> Result<()> {
    let conn = match connect(bus.as_deref()).await {
        Ok(conn) => conn,
        Err(e) => {
            warn!("no D-Bus connection for InputPlumber ({e}); controller input is not managed");
            return Ok(());
        }
    };
    let dbus = DBusProxy::new(&conn).await?;
    let mut owner_changes = dbus
        .receive_name_owner_changed_with_args(&[(0, SERVICE)])
        .await?;
    let objects = ObjectManagerProxy::builder(&conn)
        .destination(SERVICE)?
        .path(ROOT)?
        .build()
        .await?;
    let mut added = objects.receive_interfaces_added().await?;
    let mut removed = objects.receive_interfaces_removed().await?;
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .interface(DBUS_DEVICE_IFACE)?
        .member("InputEvent")?
        .path_namespace(ROOT)?
        .build();
    let mut inputs = MessageStream::for_match_rule(rule, &conn, Some(256)).await?;

    let mut devices = HashSet::new();
    let service = BusName::try_from(SERVICE)?;
    if dbus.name_has_owner(service).await? {
        let m = *mode.borrow_and_update();
        setup(&conn, &objects, &mut devices, m).await;
    } else {
        info!("waiting for InputPlumber ({SERVICE}) to appear on D-Bus");
    }

    loop {
        tokio::select! {
            Some(change) = owner_changes.next() => {
                devices.clear();
                if change.args()?.new_owner().is_some() {
                    info!("InputPlumber appeared");
                    let m = *mode.borrow_and_update();
                    setup(&conn, &objects, &mut devices, m).await;
                } else {
                    warn!("InputPlumber went away; waiting for it to come back");
                }
            }
            Some(signal) = added.next() => {
                let args = signal.args()?;
                if args.interfaces_and_properties().keys().any(|i| i.as_str() == COMPOSITE_IFACE) {
                    let path: OwnedObjectPath = args.object_path().clone().into();
                    info!(device = %path, "new controller");
                    let m = *mode.borrow();
                    apply(&conn, &path, m).await;
                    devices.insert(path);
                }
            }
            Some(signal) = removed.next() => {
                let args = signal.args()?;
                if args.interfaces().iter().any(|i| i.as_str() == COMPOSITE_IFACE) {
                    info!(device = %args.object_path(), "controller gone");
                    devices.remove(&OwnedObjectPath::from(args.object_path().clone()));
                }
            }
            Some(msg) = inputs.next() => {
                let msg = msg?;
                let source = msg.header().path().map(ToString::to_string).unwrap_or_default();
                match msg.body().deserialize::<(String, f64)>() {
                    Ok((event, value)) => {
                        debug!(%source, %event, value, "input");
                        if events.send(RawInput { source, event, value }).is_err() {
                            return Ok(());
                        }
                    }
                    Err(e) => debug!("ignoring malformed InputEvent: {e}"),
                }
            }
            changed = mode.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let m = *mode.borrow_and_update();
                debug!(mode = ?m, devices = devices.len(), "intercept mode");
                for path in &devices {
                    apply(&conn, path, m).await;
                }
            }
        }
    }
}

/// (Re)initialize after InputPlumber (re)started: manage all pads, find the composite devices
/// and apply `mode` to them. Errors are logged; new devices still arrive via InterfacesAdded.
async fn setup(
    conn: &Connection,
    objects: &ObjectManagerProxy<'_>,
    devices: &mut HashSet<OwnedObjectPath>,
    mode: InterceptMode,
) {
    let result: Result<()> = async {
        let manager = Proxy::new(conn, SERVICE, MANAGER_PATH, MANAGER_IFACE).await?;
        manager
            .set_property("ManageAllDevices", true)
            .await
            .context("setting ManageAllDevices (is the user in the 'inputplumber' group, with polkit running?)")?;
        for (path, interfaces) in objects.get_managed_objects().await? {
            if interfaces.keys().any(|i| i.as_str() == COMPOSITE_IFACE) {
                apply(conn, &path, mode).await;
                devices.insert(path);
            }
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => info!(controllers = devices.len(), mode = ?mode, "InputPlumber ready"),
        Err(e) => warn!("setting up InputPlumber: {e:#}"),
    }
}

async fn apply(conn: &Connection, path: &ObjectPath<'_>, mode: InterceptMode) {
    let result: zbus::Result<()> = async {
        let device = Proxy::new(conn, SERVICE, path.to_owned(), COMPOSITE_IFACE).await?;
        device.set_property("InterceptMode", mode as u32).await?;
        Ok(())
    }
    .await;
    if let Err(e) = result {
        warn!(device = %path, ?mode, "setting InterceptMode: {e}");
    }
}
