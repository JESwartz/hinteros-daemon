//! What gamescope tells a Wayland client about the display: the output's current mode
//! (`wl_output.mode`, flagged current) and the display's identity (`gamescope_control`'s
//! `active_display_info`: connector, make, model, flags), both sent when a client binds.
//! `protocol/gamescope-control.xml` is copied from gamescope 3.16.31 (private protocol,
//! pinned with gamescope like the input method one).

use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{Context, Result};
use hinteros_api::session::Mode;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};

use crate::keys::ime::socket_path;

/// Generated from the protocol XML.
#[allow(missing_docs, clippy::all, unused_imports)]
pub mod protocol {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocol/gamescope-control.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocol/gamescope-control.xml");
}

use protocol::gamescope_control::{self, GamescopeControl};

/// `gamescope_control.display_flag.internal_display`.
const FLAG_INTERNAL: u32 = 0x1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayInfo {
    pub connector: String,
    pub make: String,
    pub model: String,
    pub internal: bool,
}

impl DisplayInfo {
    /// The name gamescope saves this display's mode under (`setup_best_connector`):
    /// "Internal screen", or "<make> <model>" (make as gamescope resolves it from the EDID).
    pub fn description(&self) -> String {
        if self.internal {
            "Internal screen".into()
        } else {
            format!("{} {}", self.make, self.model)
        }
    }

    /// What to show the user.
    pub fn title(&self) -> String {
        self.description().trim().to_owned()
    }
}

#[derive(Debug, Default)]
pub struct Info {
    /// The output's current mode.
    pub mode: Option<Mode>,
    pub display: Option<DisplayInfo>,
}

#[derive(Default)]
struct State {
    info: Info,
}

/// Connect to gamescope's Wayland display and read what it says. Blocking.
pub fn query(display: &str) -> Result<Info> {
    let path = socket_path(display)?;
    let stream = UnixStream::connect(&path)
        .with_context(|| format!("connecting to Wayland display {}", path.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let conn = Connection::from_socket(stream)?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let mut state = State::default();
    let _output: wl_output::WlOutput = globals
        .bind(&qh, 1..=4, ())
        .context("gamescope has no wl_output")?;
    // Only gamescope has this; without it we still know the mode.
    let _control: Option<GamescopeControl> = globals.bind(&qh, 2..=7, ()).ok();
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;
    Ok(state.info)
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Mode {
            flags,
            width,
            height,
            refresh,
        } = event
        {
            let current = match flags {
                WEnum::Value(f) => f.contains(wl_output::Mode::Current),
                WEnum::Unknown(bits) => bits & 1 != 0,
            };
            if current && width > 0 && height > 0 && state.info.mode.is_none() {
                state.info.mode = Some(Mode {
                    width: width as u32,
                    height: height as u32,
                    // mHz to Hz, rounded (59940 is 60).
                    refresh: (refresh.max(0) as u32 + 500) / 1000,
                });
            }
        }
    }
}

impl Dispatch<GamescopeControl, ()> for State {
    fn event(
        state: &mut Self,
        _: &GamescopeControl,
        event: gamescope_control::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let gamescope_control::Event::ActiveDisplayInfo {
            connector_name,
            display_make,
            display_model,
            display_flags,
            ..
        } = event
        {
            let flags = match display_flags {
                WEnum::Value(f) => f.bits(),
                WEnum::Unknown(bits) => bits,
            };
            state.info.display = Some(DisplayInfo {
                connector: connector_name,
                make: display_make,
                model: display_model,
                internal: flags & FLAG_INTERNAL != 0,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_follow_gamescope() {
        let tv = DisplayInfo {
            connector: "HDMI-A-1".into(),
            make: "Sony".into(),
            model: "TV".into(),
            internal: false,
        };
        assert_eq!(tv.description(), "Sony TV");
        let panel = DisplayInfo {
            internal: true,
            ..tv.clone()
        };
        assert_eq!(panel.description(), "Internal screen");
        let unknown = DisplayInfo {
            make: String::new(),
            model: "LG".into(),
            ..tv
        };
        assert_eq!(unknown.description(), " LG");
        assert_eq!(unknown.title(), "LG");
    }
}
