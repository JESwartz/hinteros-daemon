//! Runtime settings: what Settings changes without a rebuild (Wi-Fi, Bluetooth, audio, display,
//! power), served as `network.*`, `wifi.*`, `bluetooth.*`, `audio.*`, `display.*` and
//! `power.*`.
//!
//! The daemon runs as the session user and talks to the system's own services, which authorise
//! it with the polkit rules from the system repo (`runtime.nix`): NetworkManager and BlueZ and
//! logind over D-Bus, PipeWire through `pactl`, gamescope through its Xwayland and Wayland
//! sockets and its mode save file. Each feature degrades on its own: when its service is
//! missing the method fails with `unavailable {service}` and everything else keeps working.
//! Tests run the real daemon against fake services on a private bus (`tests/runtime_*.rs`).

pub mod audio;
pub mod bluetooth;
pub mod dbus;
pub mod display;
pub mod network;
pub mod power;

use std::future::Future;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};

use std::time::Duration;

use hinteros_api::server::{decode_params, encode_result};
use hinteros_api::session::{self, EventName, Service};
use hinteros_api::{ErrorObject, Method};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{OnceCell, broadcast};
use tracing::{debug, info};
use zbus::Connection;

use crate::config::{AudioConfig, BluetoothConfig, Config, DisplayConfig};
use crate::{Audience, Broadcast};

/// What the features share.
pub struct Ctx {
    pub enabled: bool,
    pub bus: Option<String>,
    pub audio: AudioConfig,
    pub display: DisplayConfig,
    pub bluetooth: BluetoothConfig,
    /// gamescope's Xwayland (`:0`), when gamescope integration is on.
    pub x_display: Option<String>,
    /// gamescope's Wayland display, for `gamescope_control`.
    pub wayland_display: String,
    events: broadcast::Sender<Broadcast>,
    conn: OnceCell<Connection>,
    pub bt: bluetooth::Shared,
    pub display_state: display::Shared,
    /// Pid of the on-screen keyboard the daemon runs (0: none), restarted after a mode change.
    pub osk_pid: Arc<AtomicU32>,
    /// The audio state the clients last heard.
    pub audio_last: Mutex<Option<hinteros_api::session::AudioState>>,
}

impl Ctx {
    /// The D-Bus connection, made on first use (and again after a failure).
    pub async fn conn(&self, service: Service) -> Result<Connection, ErrorObject> {
        if !self.enabled {
            return Err(dbus::unavailable(
                service,
                "runtime settings are off ([runtime] enable = false)",
            ));
        }
        self.conn
            .get_or_try_init(|| dbus::connect(self.bus.as_deref()))
            .await
            .cloned()
            .map_err(|e| dbus::unavailable(service, format!("no D-Bus connection ({e})")))
    }

    /// Send an event to the connected clients.
    pub fn emit<T: Serialize>(&self, name: EventName, params: &T) {
        if let Ok(params) = serde_json::to_value(params) {
            let _ = self.events.send((name, params, Audience::All));
        }
    }

    /// Whether any client is connected (there is nobody to tell otherwise).
    pub fn has_clients(&self) -> bool {
        self.events.receiver_count() > 0
    }
}

/// Handle to the runtime settings; cheap to clone.
#[derive(Clone)]
pub struct Runtime {
    ctx: Arc<Ctx>,
}

/// The service a runtime method needs, for the error when the runtime is switched off.
fn service_of(method: &str) -> Service {
    match method.split('.').next() {
        Some("network" | "wifi") => Service::Networkmanager,
        Some("bluetooth") => Service::Bluez,
        Some("audio") => Service::Audio,
        Some("display") => Service::Display,
        _ => Service::Logind,
    }
}

/// How long a method may take before it fails: a service that hangs must not hang the launcher.
fn time_limit(method: &str) -> Duration {
    Duration::from_secs(match method {
        "wifi.connect" => 60,
        "bluetooth.pair" => 90,
        "display.set_mode" | "display.revert" => 25,
        _ => 30,
    })
}

/// Decode the params, run `f`, encode the result.
async fn run<M, F, Fut>(params: Option<Value>, f: F) -> Result<Value, ErrorObject>
where
    M: Method,
    F: FnOnce(M::Params) -> Fut,
    Fut: Future<Output = Result<M::Result, ErrorObject>>,
{
    encode_result::<M>(f(decode_params::<M>(params)?).await?)
}

impl Runtime {
    /// Start the watchers that send `network_changed` and friends.
    pub fn spawn(
        config: &Config,
        events: broadcast::Sender<Broadcast>,
        osk_pid: Arc<AtomicU32>,
    ) -> Self {
        let ctx = Arc::new(Ctx {
            enabled: config.runtime.enable,
            bus: config.runtime.bus.clone(),
            audio: config.audio.clone(),
            display: config.display.clone(),
            bluetooth: config.bluetooth.clone(),
            x_display: config
                .gamescope
                .enable
                .then(|| config.gamescope_display_setting()),
            wayland_display: config.wayland_display(),
            events,
            conn: OnceCell::new(),
            bt: bluetooth::Shared::default(),
            display_state: display::Shared::default(),
            osk_pid,
            audio_last: Mutex::new(None),
        });
        if ctx.enabled {
            tokio::spawn(network::watch(ctx.clone()));
            tokio::spawn(bluetooth::watch(ctx.clone()));
            tokio::spawn(audio::watch(ctx.clone()));
            tokio::spawn(audio::apply_shared(ctx.clone()));
            info!("runtime settings on");
        } else {
            info!("runtime settings off");
        }
        Self { ctx }
    }

    pub fn handles(method: &str) -> bool {
        session::METHODS.contains(&method)
    }

    /// The methods the server answers from a task of their own: each may wait seconds for
    /// NetworkManager, BlueZ, PipeWire, gamescope or logind (a mode switch, a hung service up to
    /// its `time_limit`), and on the connection it would hold up the events, the controller
    /// input included. Not the volume and mute: they are set in the order they come.
    pub fn waits(method: &str) -> bool {
        Self::handles(method) && !matches!(method, "audio.volume" | "audio.mute")
    }

    /// The display whose mode the last `display.keep` kept and that mode, once (see
    /// [`display::take_kept`]).
    pub fn take_kept_display(&self) -> Option<(String, session::Mode)> {
        display::take_kept(&self.ctx.display_state)
    }

    /// Run one runtime method.
    pub async fn call(&self, method: &str, params: Option<Value>) -> Result<Value, ErrorObject> {
        let ctx = &*self.ctx;
        if !ctx.enabled {
            return Err(dbus::unavailable(
                service_of(method),
                "runtime settings are off ([runtime] enable = false)",
            ));
        }
        debug!(method, "runtime call");
        match tokio::time::timeout(time_limit(method), self.dispatch(method, params)).await {
            Ok(outcome) => outcome,
            Err(_) => Err(dbus::failed(
                "timeout",
                format!(
                    "{} did not answer in time",
                    dbus::service_name(service_of(method))
                ),
            )),
        }
    }

    async fn dispatch(&self, method: &str, params: Option<Value>) -> Result<Value, ErrorObject> {
        use session::*;
        let ctx = &self.ctx;
        match method {
            NetworkStatusMethod::NAME => {
                run::<NetworkStatusMethod, _, _>(params, |_| network::status(ctx)).await
            }
            WifiScan::NAME => run::<WifiScan, _, _>(params, |_| network::scan(ctx)).await,
            WifiListMethod::NAME => {
                run::<WifiListMethod, _, _>(params, |_| network::list(ctx)).await
            }
            WifiConnect::NAME => {
                run::<WifiConnect, _, _>(params, |p| network::connect(ctx, p)).await
            }
            WifiDisconnect::NAME => {
                run::<WifiDisconnect, _, _>(params, |_| network::disconnect(ctx)).await
            }
            WifiForget::NAME => run::<WifiForget, _, _>(params, |p| network::forget(ctx, p)).await,
            WifiEnable::NAME => run::<WifiEnable, _, _>(params, |p| network::enable(ctx, p)).await,
            BluetoothStatus::NAME => {
                run::<BluetoothStatus, _, _>(params, |_| bluetooth::status(ctx)).await
            }
            BluetoothScan::NAME => {
                run::<BluetoothScan, _, _>(params, |p| bluetooth::scan(ctx, p)).await
            }
            BluetoothDevices::NAME => {
                run::<BluetoothDevices, _, _>(params, |_| bluetooth::devices(ctx)).await
            }
            BluetoothPair::NAME => {
                run::<BluetoothPair, _, _>(params, |p| bluetooth::pair(ctx, p)).await
            }
            BluetoothConfirm::NAME => {
                run::<BluetoothConfirm, _, _>(params, |p| bluetooth::confirm(ctx, p)).await
            }
            BluetoothConnect::NAME => {
                run::<BluetoothConnect, _, _>(params, |p| bluetooth::connect(ctx, p)).await
            }
            BluetoothDisconnect::NAME => {
                run::<BluetoothDisconnect, _, _>(params, |p| bluetooth::disconnect(ctx, p)).await
            }
            BluetoothForget::NAME => {
                run::<BluetoothForget, _, _>(params, |p| bluetooth::forget(ctx, p)).await
            }
            AudioOutputs::NAME => run::<AudioOutputs, _, _>(params, |_| audio::outputs(ctx)).await,
            AudioSetOutput::NAME => {
                run::<AudioSetOutput, _, _>(params, |p| audio::set_output(ctx, p)).await
            }
            AudioVolume::NAME => run::<AudioVolume, _, _>(params, |p| audio::volume(ctx, p)).await,
            AudioMute::NAME => run::<AudioMute, _, _>(params, |p| audio::mute(ctx, p)).await,
            DisplayModesMethod::NAME => {
                run::<DisplayModesMethod, _, _>(params, |_| display::modes(ctx)).await
            }
            DisplaySetMode::NAME => {
                run::<DisplaySetMode, _, _>(params, |p| display::set_mode(ctx, p)).await
            }
            DisplayKeep::NAME => run::<DisplayKeep, _, _>(params, |_| display::keep(ctx)).await,
            DisplayRevert::NAME => {
                run::<DisplayRevert, _, _>(params, |_| display::revert(ctx)).await
            }
            PowerCapabilitiesMethod::NAME => {
                run::<PowerCapabilitiesMethod, _, _>(params, |_| power::capabilities(ctx)).await
            }
            PowerSuspend::NAME => run::<PowerSuspend, _, _>(params, |_| power::suspend(ctx)).await,
            PowerReboot::NAME => run::<PowerReboot, _, _>(params, |_| power::reboot(ctx)).await,
            PowerPoweroff::NAME => {
                run::<PowerPoweroff, _, _>(params, |_| power::poweroff(ctx)).await
            }
            other => Err(hinteros_api::server::method_not_found(other)),
        }
    }
}
