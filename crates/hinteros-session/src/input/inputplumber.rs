//! InputPlumber over D-Bus (zbus): makes it manage all pads, sets every composite device's
//! intercept mode and device profile (the right-stick pointer, [`super::pointer`]), presses
//! mouse buttons, and forwards `InputEvent` signals.
//!
//! InputPlumber forgets `ManageAllDevices`, the modes and loaded profiles when it restarts, so
//! all are re-applied whenever its bus name gets a new owner, and applied to every composite
//! device that appears later (one per pad). If there is no bus or no InputPlumber, this logs
//! and waits; the rest of the daemon works without it.
//!
//! Every InputPlumber method and property read goes through polkit, so nothing here may cost a
//! D-Bus round trip per input event: the signal loop forwards events at once, and the mapping
//! from a D-Bus target to its pad is looked up once per target and cached (also when the
//! lookup finds nothing). Proxies never cache properties (a caching proxy fetches *all*
//! properties, each one a polkit check, on first use).

use std::collections::HashMap;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};
use zbus::fdo::{DBusProxy, ObjectManagerProxy};
use zbus::message::Type as MessageType;
use zbus::names::BusName;
use zbus::proxy::CacheProperties;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, Value};
use zbus::{Connection, MatchRule, MessageStream, Proxy};

use super::pointer::{self, MouseButton};
use super::{InputControl, InterceptMode, LabelSwap, RawInput, swap_xbox_label, xbox_layout};

pub const SERVICE: &str = "org.shadowblip.InputPlumber";
/// Where InputPlumber's ObjectManager lives; all its objects are below.
pub const ROOT: &str = "/org/shadowblip/InputPlumber";
pub const MANAGER_PATH: &str = "/org/shadowblip/InputPlumber/Manager";
pub const MANAGER_IFACE: &str = "org.shadowblip.InputManager";
pub const COMPOSITE_IFACE: &str = "org.shadowblip.Input.CompositeDevice";
pub const DBUS_DEVICE_IFACE: &str = "org.shadowblip.Input.DBusDevice";
/// The composite device property listing its D-Bus targets. zbus derives the name from
/// InputPlumber's `fn dbus_devices` (`src/dbus/interface/composite_device.rs:575`), so it is
/// `DbusDevices`, not `DBusDevices`.
pub const DBUS_DEVICES_PROPERTY: &str = "DbusDevices";

/// Where input should go: the intercept mode, and whether the pointer profile is loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Routing {
    pub mode: InterceptMode,
    pub pointer: bool,
}

/// The pointer profile settings.
#[derive(Debug, Clone)]
pub struct PointerProfile {
    /// The profile YAML ([`pointer::profile_yaml`]).
    pub yaml: String,
}

enum Command {
    Button {
        source: String,
        button: MouseButton,
        pressed: bool,
    },
}

/// [`InputControl`] backed by the InputPlumber task: the latest routing wins; mouse buttons
/// are sent in order.
pub struct InputPlumber {
    routing: watch::Sender<Routing>,
    commands: mpsc::UnboundedSender<Command>,
}

impl InputControl for InputPlumber {
    fn set_mode(&mut self, mode: InterceptMode) {
        self.routing.send_if_modified(|r| {
            let changed = r.mode != mode;
            r.mode = mode;
            changed
        });
    }

    fn set_pointer(&mut self, on: bool) {
        self.routing.send_if_modified(|r| {
            let changed = r.pointer != on;
            r.pointer = on;
            changed
        });
    }

    fn mouse_button(&mut self, source: &str, button: MouseButton, pressed: bool) {
        let _ = self.commands.send(Command::Button {
            source: source.into(),
            button,
            pressed,
        });
    }
}

/// Start the InputPlumber task. `bus` is a D-Bus address, `None` for the system bus.
pub fn spawn(
    bus: Option<String>,
    initial: InterceptMode,
    swap: LabelSwap,
    profile: PointerProfile,
    events: mpsc::UnboundedSender<RawInput>,
) -> InputPlumber {
    let (tx, rx) = watch::channel(Routing {
        mode: initial,
        pointer: false,
    });
    let (ctx, crx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        if let Err(e) = run(bus, rx, crx, swap, profile, events).await {
            warn!("InputPlumber integration stopped: {e:#}");
        }
    });
    InputPlumber {
        routing: tx,
        commands: ctx,
    }
}

async fn connect(bus: Option<&str>) -> zbus::Result<Connection> {
    match bus {
        None => Connection::system().await,
        Some(address) => zbus::connection::Builder::address(address)?.build().await,
    }
}

/// What we know about one composite device (pad).
#[derive(Debug, Default)]
struct Device {
    /// Its `ui_action`/`ui_context` labels get swapped.
    swap: bool,
    /// Our pointer profile is loaded.
    pointer: bool,
    /// The profile that was loaded before ours (YAML), to put back.
    previous_profile: Option<String>,
}

type Devices = HashMap<OwnedObjectPath, Device>;

/// D-Bus target path -> the composite device it belongs to (`None`: nobody claimed it).
type Targets = HashMap<String, Option<OwnedObjectPath>>;

struct Task {
    conn: Connection,
    swap: LabelSwap,
    profile: PointerProfile,
    devices: Devices,
    targets: Targets,
}

async fn run(
    bus: Option<String>,
    mut routing: watch::Receiver<Routing>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    swap: LabelSwap,
    profile: PointerProfile,
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
        .cache_properties(CacheProperties::No)
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

    let mut task = Task {
        conn,
        swap,
        profile,
        devices: HashMap::new(),
        targets: HashMap::new(),
    };
    let service = BusName::try_from(SERVICE)?;
    if dbus.name_has_owner(service).await? {
        let r = *routing.borrow_and_update();
        task.setup(&objects, r).await;
    } else {
        info!("waiting for InputPlumber ({SERVICE}) to appear on D-Bus");
    }

    loop {
        tokio::select! {
            // Input first: nothing else may hold it up.
            biased;
            Some(msg) = inputs.next() => {
                let msg = msg?;
                let source = msg.header().path().map(ToString::to_string).unwrap_or_default();
                match msg.body().deserialize::<(String, f64)>() {
                    Ok((event, value)) => {
                        debug!(%source, %event, value, "input");
                        let event = if task.swaps(&source).await {
                            swap_xbox_label(&event).to_string()
                        } else {
                            event
                        };
                        if events.send(RawInput { source, event, value }).is_err() {
                            return Ok(());
                        }
                    }
                    Err(e) => debug!("ignoring malformed InputEvent: {e}"),
                }
            }
            Some(command) = commands.recv() => match command {
                Command::Button { source, button, pressed } => {
                    task.button(&source, button, pressed).await;
                }
            },
            Some(change) = owner_changes.next() => {
                task.devices.clear();
                task.targets.clear();
                if change.args()?.new_owner().is_some() {
                    info!("InputPlumber appeared");
                    let r = *routing.borrow_and_update();
                    task.setup(&objects, r).await;
                } else {
                    warn!("InputPlumber went away; waiting for it to come back");
                }
            }
            Some(signal) = added.next() => {
                let args = signal.args()?;
                if args.interfaces_and_properties().keys().any(|i| i.as_str() == COMPOSITE_IFACE) {
                    let path: OwnedObjectPath = args.object_path().clone().into();
                    info!(device = %path, "new controller");
                    let r = *routing.borrow();
                    task.add(path, r).await;
                }
            }
            Some(signal) = removed.next() => {
                let args = signal.args()?;
                if args.interfaces().iter().any(|i| i.as_str() == COMPOSITE_IFACE) {
                    info!(device = %args.object_path(), "controller gone");
                    task.devices.remove(&OwnedObjectPath::from(args.object_path().clone()));
                    task.targets.clear();
                }
            }
            changed = routing.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let r = *routing.borrow_and_update();
                debug!(routing = ?r, devices = task.devices.len(), "routing");
                let paths: Vec<OwnedObjectPath> = task.devices.keys().cloned().collect();
                for path in paths {
                    task.apply(&path, r).await;
                }
            }
        }
    }
}

/// A composite device proxy that never caches properties.
async fn device_proxy<'a>(conn: &Connection, path: &ObjectPath<'_>) -> zbus::Result<Proxy<'a>> {
    zbus::proxy::Builder::new(conn)
        .destination(SERVICE)?
        .path(path.to_owned())?
        .interface(COMPOSITE_IFACE)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

impl Task {
    /// (Re)initialize after InputPlumber (re)started: manage all pads, find the composite
    /// devices and apply the routing to them. Errors are logged; new devices still arrive via
    /// InterfacesAdded.
    async fn setup(&mut self, objects: &ObjectManagerProxy<'_>, routing: Routing) {
        let result: Result<Vec<OwnedObjectPath>> = async {
            let manager = Proxy::new(&self.conn, SERVICE, MANAGER_PATH, MANAGER_IFACE).await?;
            manager
                .set_property("ManageAllDevices", true)
                .await
                .context("setting ManageAllDevices (is the user in the 'inputplumber' group, with polkit running?)")?;
            Ok(objects
                .get_managed_objects()
                .await?
                .into_iter()
                .filter(|(_, interfaces)| interfaces.keys().any(|i| i.as_str() == COMPOSITE_IFACE))
                .map(|(path, _)| path)
                .collect())
        }
        .await;
        match result {
            Ok(paths) => {
                for path in paths {
                    self.add(path, routing).await;
                }
                info!(
                    controllers = self.devices.len(),
                    ?routing,
                    "InputPlumber ready"
                );
            }
            Err(e) => warn!("setting up InputPlumber: {e:#}"),
        }
    }

    /// A composite device appeared: classify it and apply the routing.
    async fn add(&mut self, path: OwnedObjectPath, routing: Routing) {
        let device = self.classify(&path).await;
        self.devices.insert(path.clone(), device);
        self.targets.clear();
        self.apply(&path, routing).await;
    }

    /// Load the wanted profile, then set the mode (a mouse mapping must be in place before
    /// `GAMEPAD_ONLY` lets it through, and gone before a game gets the pad).
    async fn apply(&mut self, path: &OwnedObjectPath, routing: Routing) {
        let Some(device) = self.devices.get_mut(path) else {
            return;
        };
        let proxy = match device_proxy(&self.conn, path).await {
            Ok(p) => p,
            Err(e) => {
                warn!(device = %path, "InputPlumber device: {e}");
                return;
            }
        };
        if routing.pointer != device.pointer {
            let result: zbus::Result<()> = async {
                if routing.pointer {
                    if device.previous_profile.is_none() {
                        device.previous_profile =
                            proxy.call::<_, _, String>("GetProfileYaml", &()).await.ok();
                    }
                    proxy
                        .call::<_, _, ()>("LoadProfileFromYaml", &(self.profile.yaml.as_str(),))
                        .await
                } else {
                    let previous = device
                        .previous_profile
                        .take()
                        .unwrap_or_else(pointer::default_profile_yaml);
                    proxy
                        .call::<_, _, ()>("LoadProfileFromYaml", &(previous.as_str(),))
                        .await
                }
            }
            .await;
            match result {
                Ok(()) => {
                    info!(device = %path, pointer = routing.pointer, "pointer profile");
                    device.pointer = routing.pointer;
                }
                Err(e) => warn!(device = %path, "loading the device profile: {e}"),
            }
        }
        if let Err(e) = proxy
            .set_property("InterceptMode", routing.mode as u32)
            .await
        {
            warn!(device = %path, mode = ?routing.mode, "setting InterceptMode: {e}");
        }
    }

    /// Decide (and log) the Xbox label swap for a composite device, and whether our pointer
    /// profile is still loaded on it (the daemon restarted while it was).
    async fn classify(&self, path: &ObjectPath<'_>) -> Device {
        let (mut name, mut sources, mut profile) =
            (String::new(), Vec::<String>::new(), String::new());
        if let Ok(p) = device_proxy(&self.conn, path).await {
            name = p.get_property("Name").await.unwrap_or_default();
            sources = p
                .get_property("SourceDevicePaths")
                .await
                .unwrap_or_default();
            profile = p.get_property("ProfileName").await.unwrap_or_default();
        }
        let drivers: Vec<String> = sources.iter().filter_map(|s| source_driver(s)).collect();
        let (swapped, why) = match self.swap {
            LabelSwap::Off => (false, "disabled"),
            LabelSwap::On => (true, "forced"),
            LabelSwap::Auto => match xbox_layout(&name, &drivers) {
                Some(why) => (true, why),
                None => (false, "not an Xbox layout"),
            },
        };
        if swapped {
            info!(%why, ?drivers, "Xbox layout: swapping ui_action/ui_context labels for {name}");
        } else {
            info!(%why, ?drivers, "not swapping ui_action/ui_context labels for {name}");
        }
        Device {
            swap: swapped,
            // Ours from before a restart: restore the default when the pointer is off.
            pointer: profile == pointer::PROFILE_NAME,
            previous_profile: None,
        }
    }

    /// The composite device a D-Bus target belongs to, through the `DbusDevices` property.
    /// Looked up once per target (also when nobody claims it); the cache is cleared when pads
    /// come or go. A target nobody claims belongs to the only pad when there is exactly one.
    async fn device_of(&mut self, source: &str) -> Option<OwnedObjectPath> {
        if !self.targets.contains_key(source) {
            for path in self.devices.keys() {
                let Ok(proxy) = device_proxy(&self.conn, path).await else {
                    continue;
                };
                match proxy
                    .get_property::<Vec<String>>(DBUS_DEVICES_PROPERTY)
                    .await
                {
                    Ok(paths) => {
                        for t in paths {
                            self.targets.insert(t, Some(path.clone()));
                        }
                    }
                    Err(e) => debug!(device = %path, "reading {DBUS_DEVICES_PROPERTY}: {e}"),
                }
            }
            if !self.targets.contains_key(source) {
                let only = (self.devices.len() == 1)
                    .then(|| self.devices.keys().next().cloned())
                    .flatten();
                debug!(%source, device = ?only, "D-Bus target claimed by no pad");
                self.targets.insert(source.to_string(), only);
            }
        }
        self.targets.get(source).cloned().flatten()
    }

    /// Whether events from the D-Bus target `source` get their labels swapped.
    async fn swaps(&mut self, source: &str) -> bool {
        if !self.devices.values().any(|d| d.swap) {
            return false;
        }
        match self.device_of(source).await {
            Some(path) => self.devices.get(&path).is_some_and(|d| d.swap),
            None => false,
        }
    }

    /// Press or release a mouse button on the pad behind `source` (the first pad if unknown).
    async fn button(&mut self, source: &str, button: MouseButton, pressed: bool) {
        let path = match self.device_of(source).await {
            Some(p) => p,
            None => match self.devices.keys().next() {
                Some(p) => p.clone(),
                None => return,
            },
        };
        let result: zbus::Result<()> = async {
            let proxy = device_proxy(&self.conn, &path).await?;
            proxy
                .call::<_, _, ()>("SendEvent", &(button.capability(), Value::Bool(pressed)))
                .await
        }
        .await;
        match result {
            Ok(()) => debug!(device = %path, ?button, pressed, "mouse button"),
            Err(e) => warn!(device = %path, ?button, pressed, "mouse button: {e}"),
        }
    }
}

/// The kernel driver bound to an input (`/dev/input/eventN`) or hidraw source device.
fn source_driver(dev: &str) -> Option<String> {
    let path = std::path::Path::new(dev);
    let node = path.file_name()?.to_str()?;
    let link = if dev.starts_with("/dev/input/") {
        format!("/sys/class/input/{node}/device/device/driver")
    } else if dev.starts_with("/dev/hidraw") {
        format!("/sys/class/hidraw/{node}/device/driver")
    } else {
        return None;
    };
    let target = std::fs::read_link(link).ok()?;
    Some(target.file_name()?.to_str()?.to_string())
}
