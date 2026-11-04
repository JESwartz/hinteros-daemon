//! InputPlumber over D-Bus (zbus): makes it manage all pads, sets every composite device's
//! intercept mode and device profile (the right-stick pointer, [`super::pointer`]), presses
//! mouse buttons, and forwards `InputEvent` signals.
//!
//! InputPlumber forgets `ManageAllDevices`, the modes and loaded profiles when it restarts, so
//! all are re-applied whenever its bus name gets a new owner, and applied to every composite
//! device that appears later (one per pad). If there is no bus or no InputPlumber, this logs
//! and waits; the rest of the daemon works without it. A bus connection that fails is made
//! again (see [`spawn`]); a single bad message is skipped.
//!
//! The intercept mode is also what keeps controller input from sandboxed apps while HinterOS
//! UI is in front (a PIN, a password): apps with the `gamepad` permission get `/dev/input`,
//! where only the virtual pads are readable (InputPlumber grabs and hides the physical ones),
//! and in `All` InputPlumber sends a pad's events only to D-Bus, i.e. to us. Each event goes
//! either to the virtual pad or to D-Bus, never both, so nothing the launcher gets from a pad
//! reaches an app. A new pad starts in `None` (all to the virtual pad), so its worker sets
//! the mode before anything else.
//!
//! Input is taken only from InputPlumber itself: any process on the system bus can send an
//! `InputEvent` signal (a media server's account, say), and a forged one would act like a pad
//! press in the launcher. The match rule names InputPlumber as the sender (the bus enforces
//! it), and each signal's sender is checked against the unique name that owns InputPlumber's
//! bus name now (`NameOwnerChanged`).
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
//! - setting InputPlumber up (`ManageAllDevices`) runs in its own task too, and is retried
//!   (250 ms, doubling up to 5 s) until it works: after a restart its bus name appears before
//!   its Manager object is exported, so the first try can fail with `UnknownObject`. Once set
//!   up, the same task checks every [`Settings::watchdog`] that `ManageAllDevices` is still on.

use std::collections::{BTreeSet, HashMap, HashSet};
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
use zbus::names::{BusName, OwnedUniqueName};
use zbus::proxy::CacheProperties;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, Value};
use zbus::{Connection, MatchRule, MessageStream, Proxy};

use super::pointer::{self, MouseButton, PointerTuning};
use super::{
    GUIDE_CAPABILITY, GUIDE_TAP_HOLD, InputControl, InterceptMode, LabelSwap, RawInput,
    swap_xbox_label, xbox_layout,
};

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

/// In `Pass`, InputPlumber switches the pad to `All` by itself on a Guide press, so that the
/// rest of the press reaches us. For this long after a Guide press the watchdog leaves that
/// `All` alone: setting `Pass` back mid-press sent the release to the game, and the daemon took
/// a short press for a hold. The session's answer (Home, the Guide tap for Steam) sets the mode.
pub const GUIDE_SWITCH_GRACE: Duration = Duration::from_secs(2);

/// Where input should go: the intercept mode, whether the pointer profile is loaded, and the
/// profile's pointer settings it is made with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Routing {
    pub mode: InterceptMode,
    pub pointer: bool,
    pub tuning: PointerTuning,
}

/// The pointer profile settings (`[pointer]`); the profile's own settings ([`PointerTuning`])
/// come with the routing.
#[derive(Debug, Clone)]
pub struct PointerProfile {
    pub speed_pps: u32,
    pub curve: f64,
    pub activity_deadzone: f64,
}

impl PointerProfile {
    /// The profile YAML ([`pointer::profile_yaml`]).
    pub fn yaml(&self, tuning: PointerTuning) -> String {
        pointer::profile_yaml(self.speed_pps, self.curve, self.activity_deadzone, tuning)
    }
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
    GuideTap {
        source: String,
        mode: InterceptMode,
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

    fn set_pointer_tuning(&mut self, tuning: PointerTuning) {
        self.routing.send_if_modified(|r| {
            let changed = r.tuning != tuning;
            r.tuning = tuning;
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

    fn guide_tap(&mut self, source: &str, mode: InterceptMode) {
        let _ = self.commands.send(Command::GuideTap {
            source: source.into(),
            mode,
        });
    }
}

/// The first wait before the integration connects again after it failed; doubles up to
/// [`RECONNECT_MAX`].
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// Start the InputPlumber task. If the integration fails (the bus connection broke), it
/// connects again (1 s, doubling up to 30 s) and sets every pad up from scratch: without it
/// nothing sets the intercept modes that keep the pads from apps while HinterOS UI is in front.
pub fn spawn(settings: Settings, events: mpsc::UnboundedSender<RawInput>) -> InputPlumber {
    let (tx, rx) = watch::channel(Routing {
        mode: settings.initial,
        pointer: false,
        tuning: PointerTuning::default(),
    });
    let (ctx, mut crx) = mpsc::unbounded_channel();
    let settings = Arc::new(settings);
    tokio::spawn(async move {
        let mut retry = RECONNECT_MIN;
        let mut held = Held::default();
        let mut failures = 0u32;
        loop {
            let started = std::time::Instant::now();
            let result = run(settings.clone(), rx.clone(), &mut crx, &events, &mut held).await;
            let Err(e) = result else {
                // Nobody takes input any more: the daemon is stopping.
                return;
            };
            // The connection is gone, and with it what the pads held.
            if !Held::send(held.release(|_| false), &events) || events.is_closed() {
                return;
            }
            if started.elapsed() >= RECONNECT_MAX {
                // It ran for a while: a new failure starts the backoff over.
                retry = RECONNECT_MIN;
                failures = 0;
            }
            if failures == 0 {
                warn!("InputPlumber integration failed: {e:#}; connecting again");
            } else {
                debug!(failures, "InputPlumber integration failed: {e:#}");
            }
            failures += 1;
            tokio::time::sleep(retry).await;
            retry = (retry * 2).min(RECONNECT_MAX);
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
    GuideTap { mode: InterceptMode },
}

/// When a pad's last Guide press arrived (set by the signal loop, read by its worker).
type GuideAt = Arc<std::sync::Mutex<Option<std::time::Instant>>>;

/// What each D-Bus target holds down (pressed, not released yet). When its pad goes away
/// (unplugged, a flat battery, InputPlumber gone) nothing releases it, and the session would
/// repeat a held direction until another pad pressed one; so the task lets go of it.
#[derive(Debug, Default)]
struct Held(HashMap<String, BTreeSet<String>>);

impl Held {
    fn note(&mut self, input: &RawInput) {
        if input.pressed() {
            self.0
                .entry(input.source.clone())
                .or_default()
                .insert(input.event.clone());
        } else if let Some(events) = self.0.get_mut(&input.source) {
            events.remove(&input.event);
            if events.is_empty() {
                self.0.remove(&input.source);
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Releases for everything held by targets that `keep` does not keep, which are forgotten.
    fn release(&mut self, keep: impl Fn(&str) -> bool) -> Vec<RawInput> {
        let gone: Vec<String> = self.0.keys().filter(|s| !keep(s)).cloned().collect();
        let mut out = Vec::new();
        for source in gone {
            for event in self.0.remove(&source).unwrap_or_default() {
                out.push(RawInput {
                    source: source.clone(),
                    event,
                    value: 0.0,
                });
            }
        }
        out.sort_by(|a, b| (&a.source, &a.event).cmp(&(&b.source, &b.event)));
        out
    }

    /// Send the releases; `false` when nobody takes input any more.
    fn send(releases: Vec<RawInput>, events: &mpsc::UnboundedSender<RawInput>) -> bool {
        releases.into_iter().all(|r| {
            debug!(source = %r.source, event = %r.event, "released: its pad is gone");
            events.send(r).is_ok()
        })
    }
}

/// One composite device (pad): its worker and what the signal loop needs to know.
struct Device {
    /// Its `ui_action`/`ui_context` labels get swapped (set by the worker once classified).
    swap: Arc<AtomicBool>,
    guide_at: GuideAt,
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

/// Connect and run until the connection fails (`Err`, connect again) or nobody takes input any
/// more (`Ok`). A bad single message is skipped, not a failure.
async fn run(
    settings: Arc<Settings>,
    routing: watch::Receiver<Routing>,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    events: &mpsc::UnboundedSender<RawInput>,
    held: &mut Held,
) -> Result<()> {
    let conn = connect(settings.bus.as_deref(), settings.call_timeout)
        .await
        .context("no D-Bus connection for InputPlumber; controller input is not managed")?;
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
    // Only InputPlumber's own signals: anyone on the system bus may send an `InputEvent`, and
    // a forged one would drive the launcher like a pad. The bus delivers only signals sent by
    // the name's owner (`sender=`); each is checked against that owner's unique name too.
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(SERVICE)?
        .interface(DBUS_DEVICE_IFACE)?
        .member("InputEvent")?
        .path_namespace(ROOT)?
        .build();
    let mut inputs = MessageStream::for_match_rule(rule, &conn, Some(256)).await?;

    let mut task = Task {
        conn,
        settings,
        routing,
        devices: HashMap::new(),
        targets: HashMap::new(),
    };
    // InputPlumber's presence (a new generation every time it appears) for the manager task,
    // which sets it up (retrying until that works) and reports the pads to add.
    let (presence, presence_rx) = watch::channel(None::<u64>);
    let (ready_tx, mut ready) = mpsc::unbounded_channel();
    let _manager = AbortOnDrop(tokio::spawn(manage(
        task.conn.clone(),
        task.settings.clone(),
        presence_rx,
        ready_tx,
    )));
    let mut generation = 0;
    let service = BusName::try_from(SERVICE)?;
    // InputPlumber's unique name: the only sender whose input is taken.
    let mut owner: Option<OwnedUniqueName> = None;
    match dbus.get_name_owner(service).await {
        Ok(name) => {
            owner = Some(name);
            generation += 1;
            presence.send_replace(Some(generation));
        }
        Err(zbus::fdo::Error::NameHasNoOwner(_)) => {
            info!("waiting for InputPlumber ({SERVICE}) to appear on D-Bus")
        }
        Err(e) => warn!("asking D-Bus for InputPlumber: {e}; waiting for it to appear"),
    }

    loop {
        tokio::select! {
            // Input first: nothing else may hold it up.
            biased;
            Some(msg) = inputs.next() => {
                let msg = match msg {
                    Ok(msg) => msg,
                    // The connection is broken: connect again.
                    Err(e @ zbus::Error::InputOutput(_)) => return Err(e.into()),
                    Err(e) => {
                        debug!("ignoring a bad message: {e}");
                        continue;
                    }
                };
                let sender = msg.header().sender().map(|s| s.as_str().to_owned());
                if sender.is_none() || sender.as_deref() != owner.as_ref().map(|o| o.as_str()) {
                    debug!(?sender, owner = ?owner.as_ref().map(|o| o.as_str()), "ignoring InputEvent not from InputPlumber");
                    continue;
                }
                let source = msg.header().path().map(ToString::to_string).unwrap_or_default();
                match msg.body().deserialize::<(String, f64)>() {
                    Ok((event, value)) => {
                        debug!(%source, %event, value, "input");
                        let event = if task.swaps(&source).await {
                            swap_xbox_label(&event).to_string()
                        } else {
                            event
                        };
                        if event == "ui_guide" && value > 0.5 {
                            task.guide_pressed(&source).await;
                        }
                        let input = RawInput { source, event, value };
                        held.note(&input);
                        if events.send(input).is_err() {
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
                Command::GuideTap { source, mode } => task.guide_tap(&source, mode).await,
            },
            Some(change) = owner_changes.next() => {
                let args = match change.args() {
                    Ok(args) => args,
                    Err(e) => {
                        warn!("ignoring a bad NameOwnerChanged signal: {e}");
                        continue;
                    }
                };
                task.devices.clear();
                task.targets.clear();
                // Its pads went with it (a new owner starts from scratch).
                if !Held::send(held.release(|_| false), events) {
                    return Ok(());
                }
                owner = args.new_owner().as_ref().map(|n| n.to_owned().into());
                if owner.is_some() {
                    info!("InputPlumber appeared");
                    generation += 1;
                    presence.send_replace(Some(generation));
                } else {
                    warn!("InputPlumber went away; waiting for it to come back");
                    presence.send_replace(None);
                }
            }
            Some((set_up, paths)) = ready.recv() => {
                // Set up (again): every pad it has gets a worker (one we have keeps its own).
                if set_up == generation {
                    for path in paths {
                        if !task.devices.contains_key(&path) {
                            task.add(path);
                        }
                    }
                    info!(
                        controllers = task.devices.len(),
                        routing = ?*task.routing.borrow(),
                        "InputPlumber ready"
                    );
                }
            }
            Some(signal) = added.next() => {
                let args = match signal.args() {
                    Ok(args) => args,
                    Err(e) => {
                        debug!("ignoring a bad InterfacesAdded signal: {e}");
                        continue;
                    }
                };
                if args.interfaces_and_properties().keys().any(|i| i.as_str() == COMPOSITE_IFACE) {
                    let path: OwnedObjectPath = args.object_path().clone().into();
                    info!(device = %path, "new controller");
                    task.add(path);
                }
            }
            Some(signal) = removed.next() => {
                let args = match signal.args() {
                    Ok(args) => args,
                    Err(e) => {
                        debug!("ignoring a bad InterfacesRemoved signal: {e}");
                        continue;
                    }
                };
                if args.interfaces().iter().any(|i| i.as_str() == COMPOSITE_IFACE) {
                    info!(device = %args.object_path(), "controller gone");
                    task.devices.remove(&OwnedObjectPath::from(args.object_path().clone()));
                    task.targets.clear();
                    if !held.is_empty() {
                        let claimed = task.claimed_targets().await;
                        if !Held::send(held.release(|s| claimed.contains(s)), events) {
                            return Ok(());
                        }
                    }
                }
            }
            else => anyhow::bail!("the D-Bus connection closed"),
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

/// Aborts the task when dropped (the manager task ends with the signal loop).
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The first retry of a failed setup; doubles up to [`SETUP_RETRY_MAX`].
const SETUP_RETRY_MIN: Duration = Duration::from_millis(250);
const SETUP_RETRY_MAX: Duration = Duration::from_secs(5);

/// Manage all pads, and list the composite devices.
async fn set_up(conn: &Connection) -> Result<Vec<OwnedObjectPath>> {
    let manager = Proxy::new(conn, SERVICE, MANAGER_PATH, MANAGER_IFACE).await?;
    manager
        .set_property("ManageAllDevices", true)
        .await
        .context(
            "setting ManageAllDevices (is the user in the 'inputplumber' group, with polkit running?)",
        )?;
    let objects = ObjectManagerProxy::builder(conn)
        .destination(SERVICE)?
        .path(ROOT)?
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    Ok(objects
        .get_managed_objects()
        .await?
        .into_iter()
        .filter(|(_, interfaces)| interfaces.keys().any(|i| i.as_str() == COMPOSITE_IFACE))
        .map(|(path, _)| path)
        .collect())
}

/// Whether InputPlumber still manages all pads.
async fn manages_all(conn: &Connection) -> Result<bool> {
    let manager = Proxy::new(conn, SERVICE, MANAGER_PATH, MANAGER_IFACE).await?;
    Ok(manager.get_property::<bool>("ManageAllDevices").await?)
}

/// (Re)initialize InputPlumber whenever it appears: `ManageAllDevices`, then the pads it has
/// go to the signal loop (`ready`, with the presence generation they belong to). A failed setup
/// is retried (250 ms, doubling up to 5 s) for as long as InputPlumber is there: its bus name
/// can appear before its Manager object does. Once set up, every [`Settings::watchdog`] it
/// checks that `ManageAllDevices` is still on and sets up again if not.
async fn manage(
    conn: Connection,
    settings: Arc<Settings>,
    mut presence: watch::Receiver<Option<u64>>,
    ready: mpsc::UnboundedSender<(u64, Vec<OwnedObjectPath>)>,
) {
    loop {
        let current = *presence.borrow_and_update();
        let generation = match current {
            Some(g) => g,
            None => {
                if presence.changed().await.is_err() {
                    return;
                }
                continue;
            }
        };
        let mut set = false;
        let mut retry = SETUP_RETRY_MIN;
        let mut failures = 0u32;
        loop {
            let wait = if set {
                match manages_all(&conn).await {
                    Ok(true) => settings.watchdog,
                    Ok(false) => {
                        warn!("InputPlumber no longer manages all pads; setting it up again");
                        set = false;
                        continue;
                    }
                    Err(e) => {
                        debug!("checking InputPlumber's ManageAllDevices: {e:#}");
                        settings.watchdog
                    }
                }
            } else {
                match set_up(&conn).await {
                    Ok(paths) => {
                        if failures > 0 {
                            info!(
                                attempts = failures + 1,
                                "InputPlumber set up after retrying"
                            );
                        }
                        set = true;
                        retry = SETUP_RETRY_MIN;
                        failures = 0;
                        if ready.send((generation, paths)).is_err() {
                            return;
                        }
                        settings.watchdog
                    }
                    Err(e) => {
                        if failures == 0 {
                            warn!("setting up InputPlumber: {e:#}; retrying");
                        } else {
                            debug!(failures, "setting up InputPlumber: {e:#}");
                        }
                        failures += 1;
                        let wait = retry;
                        retry = (retry * 2).min(SETUP_RETRY_MAX);
                        wait
                    }
                }
            };
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                changed = presence.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    // Gone, or a new InputPlumber: start over.
                    break;
                }
            }
        }
    }
}

impl Task {
    /// A composite device appeared: start its worker (which classifies it and applies the
    /// routing).
    fn add(&mut self, path: OwnedObjectPath) {
        let swap = Arc::new(AtomicBool::new(false));
        let guide_at = GuideAt::default();
        let (ops, rx) = mpsc::unbounded_channel();
        let worker = Worker {
            conn: self.conn.clone(),
            path: path.clone(),
            settings: self.settings.clone(),
            routing: self.routing.clone(),
            swap: swap.clone(),
            guide_at: guide_at.clone(),
            pointer: false,
            loaded: None,
            previous_profile: None,
            dirty: true,
            answering: true,
        };
        let worker = tokio::spawn(worker.run(rx));
        self.devices.insert(
            path,
            Device {
                swap,
                guide_at,
                ops,
                worker,
            },
        );
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

    /// The D-Bus targets the pads claim now (read from each pad; the cache learns them too).
    /// Only when a pad went away while something was held, so not per event.
    async fn claimed_targets(&mut self) -> HashSet<String> {
        let mut claimed = HashSet::new();
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
                        self.targets.insert(t.clone(), Some(path.clone()));
                        claimed.insert(t);
                    }
                }
                Err(e) => debug!(device = %path, "reading {DBUS_DEVICES_PROPERTY}: {e}"),
            }
        }
        claimed
    }

    /// A Guide press came from the D-Bus target `source` (all pads when it is unknown).
    async fn guide_pressed(&mut self, source: &str) {
        let now = std::time::Instant::now();
        let path = self.device_of(source).await;
        for (p, d) in &self.devices {
            if path.as_ref().is_none_or(|path| path == p) {
                *d.guide_at.lock().unwrap() = Some(now);
            }
        }
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
        self.send(source, Op::Button { button, pressed }).await;
    }

    /// Tap Guide on the virtual pad of the pad behind `source` (the first pad if unknown),
    /// after putting it in `mode`: queued to that pad's worker.
    async fn guide_tap(&mut self, source: &str, mode: InterceptMode) {
        self.send(source, Op::GuideTap { mode }).await;
    }

    async fn send(&mut self, source: &str, op: Op) {
        let path = match self.device_of(source).await {
            Some(p) => p,
            None => match self.devices.keys().next() {
                Some(p) => p.clone(),
                None => return,
            },
        };
        if let Some(device) = self.devices.get(&path) {
            let _ = device.ops.send(op);
        }
    }
}

/// One pad's worker: sets its mode at once, classifies it, applies the routing (profile, then
/// mode), sends its mouse buttons, and watches that InputPlumber still does what we asked.
struct Worker {
    conn: Connection,
    path: OwnedObjectPath,
    settings: Arc<Settings>,
    routing: watch::Receiver<Routing>,
    swap: Arc<AtomicBool>,
    /// The pad's last Guide press ([`GUIDE_SWITCH_GRACE`]).
    guide_at: GuideAt,
    /// Our pointer profile is loaded (as far as we know).
    pointer: bool,
    /// The settings our loaded pointer profile was made with (`None`: not loaded by this worker,
    /// or not known).
    loaded: Option<PointerTuning>,
    /// The profile that was loaded before ours (YAML), to put back.
    previous_profile: Option<String>,
    /// Something we asked for may not have happened: re-apply at the next check.
    dirty: bool,
    /// The pad answered the last call (to log changes once).
    answering: bool,
}

impl Worker {
    async fn run(mut self, mut ops: mpsc::UnboundedReceiver<Op>) {
        self.set_mode_first().await;
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
                    Op::GuideTap { mode } => self.guide_tap(mode).await,
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

    /// A new pad (hotplug, or InputPlumber restarted) starts in InputPlumber's default mode,
    /// `None`: all its input goes to its virtual pad, which every app with the `gamepad`
    /// permission can read, also while the launcher asks for a PIN. So the wanted mode is set
    /// before the pad is classified (several polkit-checked reads). [`Self::apply`] then sets
    /// the profile and the mode again, in their order. A failure here is left to `apply`.
    async fn set_mode_first(&mut self) {
        let mode = self.routing.borrow().mode;
        let result = async {
            let proxy = device_proxy(&self.conn, &self.path).await?;
            proxy.set_property("InterceptMode", mode as u32).await?;
            zbus::Result::Ok(())
        }
        .await;
        if let Err(e) = result {
            debug!(device = %self.path, ?mode, "setting InterceptMode before classifying: {e}");
        }
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
        self.loaded = None;
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
        // Ours with other settings (or settings we do not know) is loaded again.
        let reload = routing.pointer && self.pointer && self.loaded != Some(routing.tuning);
        if routing.pointer != self.pointer || reload {
            let result: zbus::Result<()> = async {
                if routing.pointer {
                    // Not ours: the profile to put back afterwards.
                    if !self.pointer && self.previous_profile.is_none() {
                        self.previous_profile =
                            proxy.call::<_, _, String>("GetProfileYaml", &()).await.ok();
                    }
                    let yaml = self.settings.profile.yaml(routing.tuning);
                    proxy
                        .call::<_, _, ()>("LoadProfileFromYaml", &(yaml.as_str(),))
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
                    info!(device = %self.path, pointer = routing.pointer, speed = routing.tuning.speed, swap_sticks = routing.tuning.swap_sticks, "pointer profile");
                    self.pointer = routing.pointer;
                    self.loaded = routing.pointer.then_some(routing.tuning);
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
                    self.loaded = None;
                    self.dirty = true;
                }
                let guide_switch = want.mode == InterceptMode::Pass
                    && mode == InterceptMode::All as u32
                    && self
                        .guide_at
                        .lock()
                        .unwrap()
                        .is_some_and(|at| at.elapsed() < GUIDE_SWITCH_GRACE);
                if guide_switch {
                    // Anything else waits for the next check too (`dirty` stays).
                    debug!(device = %self.path, "InputPlumber's switch to All on a Guide press; left alone");
                    return;
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

impl Worker {
    /// Tap Guide on the virtual pad, for Steam (see [`InputControl::guide_tap`]): set the
    /// intercept mode (InputPlumber switched the pad to `All` on the user's Guide press), then
    /// press and release Guide with `SendEvent`. Both go through the composite device's one
    /// command queue in the order we await them, so the mode is in place before the press.
    /// If the mode cannot be set, or is one in which the tap would come back to us, no tap.
    /// The session asks with the routing's mode; if the routing has another mode by now (focus
    /// moved to the launcher meanwhile), the tap is for nobody, and setting `mode` would leave
    /// the pad in it until the next watchdog check: no tap either.
    async fn guide_tap(&mut self, mode: InterceptMode) {
        if !mode.reaches_pad() {
            warn!(device = %self.path, ?mode, "no Guide tap: it would come back as ui_guide");
            return;
        }
        let routed = self.routing.borrow().mode;
        if routed != mode {
            info!(device = %self.path, ?mode, ?routed, "no Guide tap: the routing changed since the press");
            return;
        }
        let proxy = match device_proxy(&self.conn, &self.path).await {
            Ok(p) => p,
            Err(e) => {
                warn!(device = %self.path, "Guide tap: {e}");
                self.dirty = true;
                return;
            }
        };
        if let Err(e) = proxy.set_property("InterceptMode", mode as u32).await {
            warn!(device = %self.path, ?mode, "Guide tap: setting InterceptMode: {e}; no tap");
            self.dirty = true;
            return;
        }
        let send = async |pressed: bool| {
            proxy
                .call::<_, _, ()>("SendEvent", &(GUIDE_CAPABILITY, Value::Bool(pressed)))
                .await
        };
        let pressed = send(true).await;
        tokio::time::sleep(GUIDE_TAP_HOLD).await;
        // Released even if the press failed: it may have arrived (a timeout).
        let released = send(false).await;
        match (pressed, released) {
            (Ok(()), Ok(())) => info!(device = %self.path, ?mode, "Guide tap sent to Steam"),
            (p, r) => {
                warn!(device = %self.path, press = ?p.err(), release = ?r.err(), "Guide tap failed");
                self.dirty = true;
            }
        }
        // The routing changed during the tap: the pad is in `mode`; put it in the routing's now.
        if self.routing.borrow().mode != mode {
            self.apply().await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn input(source: &str, event: &str, value: f64) -> RawInput {
        RawInput {
            source: source.into(),
            event: event.into(),
            value,
        }
    }

    #[test]
    fn held_inputs_are_let_go_of_when_their_pad_goes() {
        let mut held = Held::default();
        held.note(&input("a", "ui_down", 1.0));
        held.note(&input("a", "ui_accept", 1.0));
        held.note(&input("a", "ui_accept", 0.0));
        held.note(&input("b", "ui_up", 1.0));
        // A release with no press is nothing to remember.
        held.note(&input("c", "ui_left", 0.0));
        // Pad "b" is still there: only "a" is let go of, once.
        assert_eq!(held.release(|s| s == "b"), [input("a", "ui_down", 0.0)]);
        assert!(held.release(|s| s == "b").is_empty());
        assert!(!held.is_empty());
        // Everything (InputPlumber gone).
        assert_eq!(held.release(|_| false), [input("b", "ui_up", 0.0)]);
        assert!(held.is_empty());
    }
}
