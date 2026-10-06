//! Runtime settings of the session API (0.7): Wi-Fi, Bluetooth, audio, display and power, all
//! applied without a rebuild by the session daemon through the system's own services
//! (NetworkManager, BlueZ, PipeWire, gamescope, logind). Re-exported from [`crate::session`].

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{Event, Method};

/// Declares a method with its wire name.
macro_rules! method {
    ($(#[$doc:meta])* $name:ident = $wire:literal, $params:ty => $result:ty) => {
        $(#[$doc])*
        pub struct $name;
        impl Method for $name {
            const NAME: &'static str = $wire;
            type Params = $params;
            type Result = $result;
        }
    };
}

/// Params of methods that take none.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoParams {}

/// Result of methods that return nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Empty {}

/// Which service a runtime method needed and did not find (`unavailable` error data).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Service {
    Networkmanager,
    Bluez,
    Audio,
    Display,
    Logind,
}

impl Service {
    pub const ALL: &[Service] = &[
        Service::Networkmanager,
        Service::Bluez,
        Service::Audio,
        Service::Display,
        Service::Logind,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Service::Networkmanager => "networkmanager",
            Service::Bluez => "bluez",
            Service::Audio => "audio",
            Service::Display => "display",
            Service::Logind => "logind",
        }
    }
}

// ------------------------------------------------------------------------------------ network

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Connectivity {
    Unknown,
    None,
    Portal,
    Limited,
    Full,
}

impl Connectivity {
    pub const ALL: &[Connectivity] = &[
        Connectivity::Unknown,
        Connectivity::None,
        Connectivity::Portal,
        Connectivity::Limited,
        Connectivity::Full,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkStatus {
    /// There is a Wi-Fi device.
    pub wifi_available: bool,
    /// Wi-Fi is switched on (NetworkManager's `WirelessEnabled`).
    pub wifi_enabled: bool,
    /// An Ethernet device is connected.
    pub ethernet: bool,
    /// The SSID Wi-Fi is connected to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wifi_ssid: Option<String>,
    /// Its signal strength, 0 to 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wifi_signal: Option<u8>,
    /// The primary connection's IPv4 address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    pub connectivity: Connectivity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WifiSecurity {
    Open,
    Wep,
    /// WPA or WPA2 with a passphrase.
    Wpa,
    /// WPA3 (SAE) only.
    Wpa3,
    /// 802.1X: cannot be joined with a password alone.
    Enterprise,
}

impl WifiSecurity {
    pub const ALL: &[WifiSecurity] = &[
        WifiSecurity::Open,
        WifiSecurity::Wep,
        WifiSecurity::Wpa,
        WifiSecurity::Wpa3,
        WifiSecurity::Enterprise,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WifiNetwork {
    pub ssid: String,
    /// 0 to 100.
    pub signal: u8,
    pub security: WifiSecurity,
    /// The box has a stored connection for it (no password needed to join).
    pub known: bool,
    pub connected: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WifiList {
    /// Strongest first.
    pub networks: Vec<WifiNetwork>,
}

/// `wifi.connect` params. The password is never logged: `Debug` hides it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WifiConnectParams {
    pub ssid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

impl fmt::Debug for WifiConnectParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WifiConnectParams")
            .field("ssid", &self.ssid)
            .field("password", &self.password.as_ref().map(|_| "<hidden>"))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WifiForgetParams {
    pub ssid: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WifiEnableParams {
    pub enabled: bool,
}

method!(NetworkStatusMethod = "network.status", NoParams => NetworkStatus);
method!(WifiScan = "wifi.scan", NoParams => WifiList);
method!(WifiListMethod = "wifi.list", NoParams => WifiList);
method!(WifiConnect = "wifi.connect", WifiConnectParams => NetworkStatus);
method!(WifiDisconnect = "wifi.disconnect", NoParams => NetworkStatus);
method!(WifiForget = "wifi.forget", WifiForgetParams => Empty);
method!(WifiEnable = "wifi.enable", WifiEnableParams => NetworkStatus);

pub struct NetworkChanged;
impl Event for NetworkChanged {
    const NAME: &'static str = "network_changed";
    type Params = NetworkStatus;
}

pub struct WifiListChanged;
impl Event for WifiListChanged {
    const NAME: &'static str = "wifi_list_changed";
    type Params = WifiList;
}

// ---------------------------------------------------------------------------------- bluetooth

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BtKind {
    Gamepad,
    Headset,
    Speaker,
    Keyboard,
    Mouse,
    Phone,
    Computer,
    Other,
}

impl BtKind {
    pub const ALL: &[BtKind] = &[
        BtKind::Gamepad,
        BtKind::Headset,
        BtKind::Speaker,
        BtKind::Keyboard,
        BtKind::Mouse,
        BtKind::Phone,
        BtKind::Computer,
        BtKind::Other,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtDevice {
    /// `AA:BB:CC:DD:EE:FF`, upper case.
    pub address: String,
    pub name: String,
    pub kind: BtKind,
    pub paired: bool,
    pub connected: bool,
    pub trusted: bool,
    /// Battery percentage, if the device reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery: Option<u8>,
    /// Signal strength in dBm while the device is in range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rssi: Option<i16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtStatus {
    /// There is a Bluetooth adapter.
    pub available: bool,
    pub powered: bool,
    pub scanning: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtDevices {
    /// Paired devices first, then by name.
    pub devices: Vec<BtDevice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BtScanParams {
    pub enabled: bool,
    /// Stop scanning by itself after this many seconds [default 30].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u32>,
}

/// Default and largest `bluetooth.scan` timeout, in seconds.
pub const BT_SCAN_TIMEOUT_DEFAULT_S: u32 = 30;
pub const BT_SCAN_TIMEOUT_MAX_S: u32 = 300;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BtAddressParams {
    pub address: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BtConfirmParams {
    pub address: String,
    pub accept: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtChanged {
    pub status: BtStatus,
    pub devices: Vec<BtDevice>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BtConfirmMode {
    /// Compare the passkey with the device's and answer with `bluetooth.confirm`.
    Confirm,
    /// Show the passkey: the user types it on the device. No answer.
    Display,
}

impl BtConfirmMode {
    pub const ALL: &[BtConfirmMode] = &[BtConfirmMode::Confirm, BtConfirmMode::Display];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtConfirmRequest {
    pub address: String,
    pub name: String,
    /// Six digits, with leading zeros.
    pub passkey: String,
    pub mode: BtConfirmMode,
}

method!(BluetoothStatus = "bluetooth.status", NoParams => BtStatus);
method!(BluetoothScan = "bluetooth.scan", BtScanParams => BtStatus);
method!(BluetoothDevices = "bluetooth.devices", NoParams => BtDevices);
method!(BluetoothPair = "bluetooth.pair", BtAddressParams => BtDevice);
method!(BluetoothConfirm = "bluetooth.confirm", BtConfirmParams => Empty);
method!(BluetoothConnect = "bluetooth.connect", BtAddressParams => BtDevice);
method!(BluetoothDisconnect = "bluetooth.disconnect", BtAddressParams => BtDevice);
method!(BluetoothForget = "bluetooth.forget", BtAddressParams => Empty);

pub struct BluetoothChanged;
impl Event for BluetoothChanged {
    const NAME: &'static str = "bluetooth_changed";
    type Params = BtChanged;
}

pub struct BluetoothConfirmEvent;
impl Event for BluetoothConfirmEvent {
    const NAME: &'static str = "bluetooth_confirm";
    type Params = BtConfirmRequest;
}

/// Whether `s` is a Bluetooth address in the API's form (`AA:BB:CC:DD:EE:FF`, upper case).
pub fn valid_bt_address(s: &str) -> bool {
    s.len() == 17
        && s.split(':').count() == 6
        && s.split(':').all(|p| {
            p.len() == 2
                && p.bytes()
                    .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
        })
}

// -------------------------------------------------------------------------------------- audio

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioKind {
    Hdmi,
    Analog,
    Usb,
    Bluetooth,
    Other,
}

impl AudioKind {
    pub const ALL: &[AudioKind] = &[
        AudioKind::Hdmi,
        AudioKind::Analog,
        AudioKind::Usb,
        AudioKind::Bluetooth,
        AudioKind::Other,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioOutput {
    /// The sink's name: stable across restarts, what `audio.set_output` takes.
    pub id: String,
    /// What to show.
    pub name: String,
    pub kind: AudioKind,
    /// The default output (where sound goes now).
    pub default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioState {
    pub outputs: Vec<AudioOutput>,
    /// The default output's volume, 0 to 100.
    pub volume: u8,
    pub muted: bool,
}

/// Largest volume the API sets, in percent.
pub const VOLUME_MAX: u8 = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioSetOutputParams {
    pub id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioVolumeParams {
    /// The new volume; absent: just report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioMuteParams {
    pub muted: bool,
}

method!(AudioOutputs = "audio.outputs", NoParams => AudioState);
method!(AudioSetOutput = "audio.set_output", AudioSetOutputParams => AudioState);
method!(AudioVolume = "audio.volume", AudioVolumeParams => AudioState);
method!(AudioMute = "audio.mute", AudioMuteParams => AudioState);

pub struct AudioChanged;
impl Event for AudioChanged {
    const NAME: &'static str = "audio_changed";
    type Params = AudioState;
}

// ------------------------------------------------------------------------------------ display

/// A display mode as set: size in pixels and refresh rate in Hz (whole numbers: gamescope's
/// mode list is in Hz, 59.94 Hz shows as 60).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayModeInfo {
    pub width: u32,
    pub height: u32,
    pub refresh: u32,
    /// What the display runs now.
    pub current: bool,
    /// The display's own first choice (first in gamescope's list).
    pub preferred: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayModes {
    pub modes: Vec<DisplayModeInfo>,
    /// The display's name (make and model), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// `display.set_mode` can work: gamescope is reachable and configured for it.
    pub can_set: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplaySetResult {
    /// What it ran before: `display.revert` goes back to it.
    pub previous: Mode,
    pub current: Mode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayRevertResult {
    pub current: Mode,
}

method!(DisplayModesMethod = "display.modes", NoParams => DisplayModes);
method!(DisplaySetMode = "display.set_mode", Mode => DisplaySetResult);
method!(DisplayKeep = "display.keep", NoParams => Empty);
method!(DisplayRevert = "display.revert", NoParams => DisplayRevertResult);

// -------------------------------------------------------------------------------------- power

/// logind's answer to `CanSuspend` and friends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerCapability {
    Yes,
    No,
    /// Only after authentication (nobody can answer it on this box).
    Challenge,
    /// Not supported by the hardware or kernel.
    Na,
}

impl PowerCapability {
    pub const ALL: &[PowerCapability] = &[
        PowerCapability::Yes,
        PowerCapability::No,
        PowerCapability::Challenge,
        PowerCapability::Na,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerCapabilities {
    pub suspend: PowerCapability,
    pub reboot: PowerCapability,
    pub poweroff: PowerCapability,
}

method!(PowerCapabilitiesMethod = "power.capabilities", NoParams => PowerCapabilities);
method!(PowerSuspend = "power.suspend", NoParams => Empty);
method!(PowerReboot = "power.reboot", NoParams => Empty);
method!(PowerPoweroff = "power.poweroff", NoParams => Empty);

/// Every runtime method name, for dispatch.
pub const METHODS: &[&str] = &[
    NetworkStatusMethod::NAME,
    WifiScan::NAME,
    WifiListMethod::NAME,
    WifiConnect::NAME,
    WifiDisconnect::NAME,
    WifiForget::NAME,
    WifiEnable::NAME,
    BluetoothStatus::NAME,
    BluetoothScan::NAME,
    BluetoothDevices::NAME,
    BluetoothPair::NAME,
    BluetoothConfirm::NAME,
    BluetoothConnect::NAME,
    BluetoothDisconnect::NAME,
    BluetoothForget::NAME,
    AudioOutputs::NAME,
    AudioSetOutput::NAME,
    AudioVolume::NAME,
    AudioMute::NAME,
    DisplayModesMethod::NAME,
    DisplaySetMode::NAME,
    DisplayKeep::NAME,
    DisplayRevert::NAME,
    PowerCapabilitiesMethod::NAME,
    PowerSuspend::NAME,
    PowerReboot::NAME,
    PowerPoweroff::NAME,
];
