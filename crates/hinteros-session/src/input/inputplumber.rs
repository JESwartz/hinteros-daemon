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
//!
//! **A hung InputPlumber call must never hold up input.** InputPlumber 0.81's
//! `CompositeDevice.SendEvent` panics in its D-Bus task (`blocking_send` inside the tokio
//! runtime, `src/dbus/interface/composite_device.rs:284` → `client.rs:463`) and never replies;
//! when the click waited for that reply in the signal loop, no event (Guide included) reached
//! the daemon again. So:
//! - every call has a timeout ([`Settings::call_timeout`], the connection's method timeout);
//! - the signal loop makes no method calls except the cached `DbusDevices` lookups: each pad
//!   has its own worker task that loads profiles, sets modes and sends mouse buttons, in order;
//! - each worker is a watchdog: every [`Settings::watchdog`] it reads the pad's
//!   `InterceptMode` (and `ProfileName`) and re-applies everything when they are not what we
//!   asked for, when a call failed or timed out, or when the pad stops answering and later
//!   answers again. A pad object that vanishes, or InputPlumber restarting, sets up from
//!   scratch (as before).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
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

/// How the InputPlumber task runs.
#[derive(Debug, Clone)]
pub struct Settings {
    /// A D-Bus address, `None` for the system bus.
    pub bus: Option<String>,
    pub initial: InterceptMode,
    pub swap: LabelSwap,
    pub profile: PointerProfile,
    /// The longest any InputPlumber method call or property access may take.
    pub call_timeout: Duration,
    /// How often each pad's worker checks that its mode and profile are still ours.
    pub watchdog: Duration,
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

/// Start the InputPlumber task.
pub fn spawn(settings: Settings, events: mpsc::UnboundedSender<RawInput>) -> InputPlumber {
    let (tx, rx) = watch::channel(Routing {
        mode: settings.initial,
        pointer: false,
    });
    let (ctx, crx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        if let Err(e) = run(settings, rx, crx, events).await {
            warn!("InputPlumber integration stopped: {e:#}");
        }
    });
    InputPlumber {
        routing: tx,
        commands: ctx,
    }
}

async fn connect(bus: Option<&str>, timeout: Duration) -> zbus::Result<Connection> {
    let builder = match bus {
        None => zbus::connection::Builder::system()?,
        Some(address) => zbus::connection::Builder::address(address)?,
    };
    builder.method_timeout(timeout).build().await
}

/// A pad's operations, done in order by its worker.
#[derive(Debug)]
enum Op {
    Button { button: MouseButton, pressed: bool },
}

/// One composite device (pad): its worker and what the signal loop needs to know.
struct Device {
    /// Its `ui_action`/`ui_context` labels get swapped (set by the worker once classified).
    swap: Arc<AtomicBool>,
    ops: mpsc::UnboundedSender<Op>,
    worker: JoinHandle<()>,
}

impl Drop for Device {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

type Devices = HashMap<OwnedObjectPath, Device>;

/// D-Bus target path -> the composite device it belongs to (`None`: nobody claimed it).
type Targets = HashMap<String, Option<OwnedObjectPath>>;

struct Task {
    conn: Connection,
    settings: Arc<Settings>,
    routing: watch::Receiver<Routing>,
    devices: Devices,
    targets: Targets,
}

async fn run(
    settings: Settings,
    routing: watch::Receiver<Routing>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<RawInput>,
) -> Result<()> {
    let conn = match connect(settings.bus.as_deref(), settings.call_timeout).await {
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
        settings: Arc::new(settings),
        routing,
        devices: HashMap::new(),
        targets: HashMap::new(),
    };
    let service = BusName::try_from(SERVICE)?;
    match dbus.name_has_owner(service).await {
        Ok(true) => task.setup(&objects).await,
        Ok(false) => info!("waiting for InputPlumber ({SERVICE}) to appear on D-Bus"),
        Err(e) => warn!("asking D-Bus for InputPlumber: {e}; waiting for it to appear"),
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
                    task.setup(&objects).await;
                } else {
                    warn!("InputPlumber went away; waiting for it to come back");
                }
            }
            Some(signal) = added.next() => {
                let args = signal.args()?;
                if args.interfaces_and_properties().keys().any(|i| i.as_str() == COMPOSITE_IFACE) {
                    let path: OwnedObjectPath = args.object_path().clone().into();
                    info!(device = %path, "new controller");
                    task.add(path);
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
            else => return Ok(()),
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
    /// (Re)initialize after InputPlumber (re)started: manage all pads and start a worker for
    /// every composite device. Errors are logged; new devices still arrive via
    /// InterfacesAdded.
    async fn setup(&mut self, objects: &ObjectManagerProxy<'_>) {
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
                    self.add(path);
                }
                info!(
                    controllers = self.devices.len(),
                    routing = ?*self.routing.borrow(),
                    "InputPlumber ready"
                );
            }
            Err(e) => warn!("setting up InputPlumber: {e:#}"),
        }
    }

    /// A composite device appeared: start its worker (which classifies it and applies the
    /// routing).
    fn add(&mut self, path: OwnedObjectPath) {
        let swap = Arc::new(AtomicBool::new(false));
        let (ops, rx) = mpsc::unbounded_channel();
        let worker = Worker {
            conn: self.conn.clone(),
            path: path.clone(),
            settings: self.settings.clone(),
            routing: self.routing.clone(),
            swap: swap.clone(),
            pointer: false,
            previous_profile: None,
            dirty: true,
            answering: true,
        };
        let worker = tokio::spawn(worker.run(rx));
        self.devices.insert(path, Device { swap, ops, worker });
        self.targets.clear();
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
        if !self
            .devices
            .values()
            .any(|d| d.swap.load(Ordering::Relaxed))
        {
            return false;
        }
        match self.device_of(source).await {
            Some(path) => self
                .devices
                .get(&path)
                .is_some_and(|d| d.swap.load(Ordering::Relaxed)),
            None => false,
        }
    }

    /// Press or release a mouse button on the pad behind `source` (the first pad if unknown):
    /// queued to that pad's worker.
    async fn button(&mut self, source: &str, button: MouseButton, pressed: bool) {
        let path = match self.device_of(source).await {
            Some(p) => p,
            None => match self.devices.keys().next() {
                Some(p) => p.clone(),
                None => return,
            },
        };
        if let Some(device) = self.devices.get(&path) {
            let _ = device.ops.send(Op::Button { button, pressed });
        }
    }
}

/// One pad's worker: classifies it, applies the routing (profile, then mode), sends its mouse
/// buttons, and watches that InputPlumber still does what we asked.
struct Worker {
    conn: Connection,
    path: OwnedObjectPath,
    settings: Arc<Settings>,
    routing: watch::Receiver<Routing>,
    swap: Arc<AtomicBool>,
    /// Our pointer profile is loaded (as far as we know).
    pointer: bool,
    /// The profile that was loaded before ours (YAML), to put back.
    previous_profile: Option<String>,
    /// Something we asked for may not have happened: re-apply at the next check.
    dirty: bool,
    /// The pad answered the last call (to log changes once).
    answering: bool,
}

impl Worker {
    async fn run(mut self, mut ops: mpsc::UnboundedReceiver<Op>) {
        self.classify().await;
        self.apply().await;
        let mut watchdog = tokio::time::interval(self.settings.watchdog);
        watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        watchdog.reset();
        loop {
            tokio::select! {
                // Buttons first: a click is the user waiting.
                biased;
                Some(op) = ops.recv() => match op {
                    Op::Button { button, pressed } => self.button(button, pressed).await,
                },
                changed = self.routing.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    self.apply().await;
                }
                _ = watchdog.tick() => self.check().await,
            }
        }
    }

    fn answered(&mut self, ok: bool) {
        if ok && !self.answering {
            info!(device = %self.path, "InputPlumber answers again; re-applying");
            self.dirty = true;
        } else if !ok && self.answering {
            warn!(
                device = %self.path,
                "InputPlumber stopped answering for this pad; input events still come through, \
                 re-checking every {:?}",
                self.settings.watchdog
            );
        }
        self.answering = ok;
    }

    /// Decide (and log) the Xbox label swap, and whether our pointer profile is still loaded
    /// (the daemon restarted while it was).
    async fn classify(&mut self) {
        let (mut name, mut sources, mut profile) =
            (String::new(), Vec::<String>::new(), String::new());
        if let Ok(p) = device_proxy(&self.conn, &self.path).await {
            name = p.get_property("Name").await.unwrap_or_default();
            sources = p
                .get_property("SourceDevicePaths")
                .await
                .unwrap_or_default();
            profile = p.get_property("ProfileName").await.unwrap_or_default();
        }
        let drivers: Vec<String> = sources.iter().filter_map(|s| source_driver(s)).collect();
        let (swapped, why) = match self.settings.swap {
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
        self.swap.store(swapped, Ordering::Relaxed);
        // Ours from before a restart: restore the default when the pointer is off.
        self.pointer = profile == pointer::PROFILE_NAME;
    }

    /// Load the wanted profile, then set the mode (a mouse mapping must be in place before
    /// `GAMEPAD_ONLY` lets it through, and gone before a game gets the pad). Anything that
    /// fails is retried by the watchdog.
    async fn apply(&mut self) {
        let routing = *self.routing.borrow_and_update();
        debug!(device = %self.path, ?routing, "routing");
        let proxy = match device_proxy(&self.conn, &self.path).await {
            Ok(p) => p,
            Err(e) => {
                warn!(device = %self.path, "InputPlumber device: {e}");
                self.dirty = true;
                return;
            }
        };
        let mut ok = true;
        if routing.pointer != self.pointer {
            let result: zbus::Result<()> = async {
                if routing.pointer {
                    if self.previous_profile.is_none() {
                        self.previous_profile =
                            proxy.call::<_, _, String>("GetProfileYaml", &()).await.ok();
                    }
                    proxy
                        .call::<_, _, ()>(
                            "LoadProfileFromYaml",
                            &(self.settings.profile.yaml.as_str(),),
                        )
                        .await
                } else {
                    let previous = self
                        .previous_profile
                        .clone()
                        .unwrap_or_else(pointer::default_profile_yaml);
                    proxy
                        .call::<_, _, ()>("LoadProfileFromYaml", &(previous.as_str(),))
                        .await
                }
            }
            .await;
            match result {
                Ok(()) => {
                    info!(device = %self.path, pointer = routing.pointer, "pointer profile");
                    self.pointer = routing.pointer;
                    if !routing.pointer {
                        self.previous_profile = None;
                    }
                }
                Err(e) => {
                    warn!(device = %self.path, "loading the device profile: {e}");
                    ok = false;
                }
            }
        }
        if let Err(e) = proxy
            .set_property("InterceptMode", routing.mode as u32)
            .await
        {
            warn!(device = %self.path, mode = ?routing.mode, "setting InterceptMode: {e}");
            ok = false;
        }
        self.dirty = !ok;
        self.answered(ok);
    }

    /// The watchdog: is the pad still in our mode, with our profile? Re-apply if not, or if
    /// something failed before.
    async fn check(&mut self) {
        let want = *self.routing.borrow();
        let state: zbus::Result<(u32, String)> = async {
            let proxy = device_proxy(&self.conn, &self.path).await?;
            let mode = proxy.get_property::<u32>("InterceptMode").await?;
            let profile = proxy.get_property::<String>("ProfileName").await?;
            Ok((mode, profile))
        }
        .await;
        match state {
            Ok((mode, profile)) => {
                self.answered(true);
                let ours = profile == pointer::PROFILE_NAME;
                if ours != self.pointer {
                    // Somebody else loaded a profile (or InputPlumber dropped ours).
                    warn!(device = %self.path, %profile, want_pointer = want.pointer, "unexpected device profile; re-applying");
                    self.pointer = ours;
                    self.dirty = true;
                }
                if mode != want.mode as u32 {
                    warn!(device = %self.path, mode, want = ?want.mode, "unexpected InterceptMode; re-applying");
                    self.dirty = true;
                }
                if self.dirty {
                    self.apply().await;
                }
            }
            Err(e) => {
                debug!(device = %self.path, "watchdog: {e}");
                self.answered(false);
                self.dirty = true;
            }
        }
    }

    async fn button(&mut self, button: MouseButton, pressed: bool) {
        let result: zbus::Result<()> = async {
            let proxy = device_proxy(&self.conn, &self.path).await?;
            proxy
                .call::<_, _, ()>("SendEvent", &(button.capability(), Value::Bool(pressed)))
                .await
        }
        .await;
        match result {
            Ok(()) => debug!(device = %self.path, ?button, pressed, "mouse button"),
            Err(e) => {
                // InputPlumber 0.81 never answers SendEvent (see the module docs; HinterOS
                // patches it). Make sure the pad is still as we left it.
                warn!(device = %self.path, ?button, pressed, "mouse button: {e}");
                self.dirty = true;
            }
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
