//! The daemon's virtual keyboard: a uinput device (evdev crate) with every key in
//! [`super::all_codes`]. Created once and held for the whole session (compositors pick up the
//! hotplugged device, spike 5). Needs write access to `/dev/uinput` (the `uinput` group).
//! InputPlumber ignores virtual devices that are not on its allow-list, so it does not grab this
//! one (spike 4).

use std::io;

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode};

use super::worker::KeySink;

pub const DEVICE_NAME: &str = "HinterOS virtual keyboard";

pub struct UinputKeyboard {
    device: VirtualDevice,
}

impl UinputKeyboard {
    /// Create the device. Fails (typically `PermissionDenied` or `NotFound`) without access to
    /// `/dev/uinput`.
    pub fn new() -> io::Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in super::all_codes() {
            keys.insert(KeyCode::new(code));
        }
        // pid.codes test vendor/product, bus "virtual": an honest identity, like the spike's.
        let device = VirtualDevice::builder()?
            .name(DEVICE_NAME)
            .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x1209, 0x4801, 1))
            .with_keys(&keys)?
            .build()?;
        Ok(Self { device })
    }

    /// The `/dev/input/event*` nodes of the device (for tests).
    pub fn dev_nodes(&mut self) -> io::Result<Vec<std::path::PathBuf>> {
        self.device.enumerate_dev_nodes_blocking()?.collect()
    }
}

impl KeySink for UinputKeyboard {
    fn key(&mut self, code: u16, down: bool) -> io::Result<()> {
        // emit() appends the SYN_REPORT.
        self.device
            .emit(&[InputEvent::new(EventType::KEY.0, code, i32::from(down))])
    }
}

#[cfg(test)]
mod tests {
    //! Against the real /dev/uinput, only with `HINTEROS_TEST_UINPUT=1` (a VM or a machine where
    //! a stray virtual keyboard is fine): the device is created and, if its event node is
    //! readable, grabbed (so no other client sees the keys) and read back.

    use std::time::Duration;

    use super::*;

    #[test]
    fn creates_device_and_sends_keys() {
        if std::env::var_os("HINTEROS_TEST_UINPUT").is_none() {
            eprintln!("HINTEROS_TEST_UINPUT not set, skipping");
            return;
        }
        let mut kbd = match UinputKeyboard::new() {
            Ok(k) => k,
            Err(e) => {
                eprintln!("cannot create a uinput device ({e}), skipping");
                return;
            }
        };
        let nodes = kbd.dev_nodes().unwrap();
        assert!(!nodes.is_empty(), "no event node for the virtual keyboard");
        let mut dev = None;
        for _ in 0..50 {
            if let Ok(d) = evdev::Device::open(&nodes[0]) {
                dev = Some(d);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let Some(mut dev) = dev else {
            eprintln!(
                "{} is not readable, skipping the read-back",
                nodes[0].display()
            );
            return;
        };
        assert_eq!(dev.name(), Some(DEVICE_NAME));
        assert!(dev.supported_keys().unwrap().contains(KeyCode::KEY_A));
        dev.grab().unwrap();
        kbd.key(30, true).unwrap();
        kbd.key(30, false).unwrap();
        let mut seen = Vec::new();
        while seen.len() < 2 {
            for ev in dev.fetch_events().unwrap() {
                if ev.event_type() == EventType::KEY {
                    seen.push((ev.code(), ev.value()));
                }
            }
        }
        assert_eq!(seen, [(30, 1), (30, 0)]);
    }
}
