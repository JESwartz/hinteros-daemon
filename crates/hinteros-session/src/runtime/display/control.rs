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

/// One `wl_output` global and what it said about itself.
#[derive(Debug, Default, Clone)]
pub struct OutputInfo {
    /// `wl_output.name` (version 4), e.g. the connector.
    pub name: String,
    pub description: String,
    /// `wl_output.geometry` make and model.
    pub make: String,
    pub model: String,
    /// The current mode.
    pub mode: Option<Mode>,
}

impl OutputInfo {
    /// For logs: `name "make model" WxH@R`.
    pub fn describe(&self) -> String {
        let mode = self.mode.map_or("no mode".to_string(), |m| {
            format!("{}x{}@{}", m.width, m.height, m.refresh)
        });
        format!("{:?} ({} {}) {mode}", self.name, self.make, self.model)
    }
}

#[derive(Debug, Default)]
pub struct Info {
    /// The mode of the output that is the display (see [`pick_output`]).
    pub mode: Option<Mode>,
    pub display: Option<DisplayInfo>,
    /// Every `wl_output` gamescope advertised.
    pub outputs: Vec<OutputInfo>,
}

impl Info {
    /// "name (make model) WxH@R, ..." for every output, for logs.
    pub fn describe_outputs(&self) -> String {
        if self.outputs.is_empty() {
            return "no wl_output".into();
        }
        self.outputs
            .iter()
            .map(OutputInfo::describe)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Which output is the display. gamescope may advertise several `wl_output`s (a second
/// Xwayland, `--xwayland-count`, gets one of its own), so: the one named like the display's
/// connector (`gamescope_control` `active_display_info`), else the one whose make/model are the
/// display's, else the one that is not obviously gamescope's own virtual output, else the first.
pub fn pick_output<'a>(
    outputs: &'a [OutputInfo],
    display: Option<&DisplayInfo>,
) -> Option<&'a OutputInfo> {
    let eq = |a: &str, b: &str| !b.is_empty() && a.eq_ignore_ascii_case(b);
    if let Some(d) = display {
        if let Some(o) = outputs.iter().find(|o| eq(&o.name, &d.connector)) {
            return Some(o);
        }
        if let Some(o) = outputs
            .iter()
            .find(|o| eq(&o.make, &d.make) && eq(&o.model, &d.model))
        {
            return Some(o);
        }
    }
    outputs
        .iter()
        .find(|o| !o.make.to_ascii_lowercase().contains("gamescope"))
        .or_else(|| outputs.first())
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
    let advertised: Vec<(u32, u32)> = globals.contents().with_list(|list| {
        list.iter()
            .filter(|g| g.interface == "wl_output")
            .map(|g| (g.name, g.version))
            .collect()
    });
    if advertised.is_empty() {
        anyhow::bail!("gamescope has no wl_output");
    }
    // Keep the proxies alive until the events are in.
    let _outputs: Vec<wl_output::WlOutput> = advertised
        .iter()
        .enumerate()
        .map(|(i, &(name, version))| {
            state.info.outputs.push(OutputInfo::default());
            globals.registry().bind(name, version.min(4), &qh, i)
        })
        .collect();
    // Only gamescope has this; without it we still know the mode.
    let _control: Option<GamescopeControl> = globals.bind(&qh, 2..=7, ()).ok();
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;
    state.info.mode =
        pick_output(&state.info.outputs, state.info.display.as_ref()).and_then(|o| o.mode);
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

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(out) = state.info.outputs.get_mut(*index) else {
            return;
        };
        match event {
            wl_output::Event::Geometry { make, model, .. } => {
                out.make = make;
                out.model = model;
            }
            wl_output::Event::Name { name } => out.name = name,
            wl_output::Event::Description { description } => out.description = description,
            wl_output::Event::Mode {
                flags,
                width,
                height,
                refresh,
            } => {
                let current = match flags {
                    WEnum::Value(f) => f.contains(wl_output::Mode::Current),
                    WEnum::Unknown(bits) => bits & 1 != 0,
                };
                if current && width > 0 && height > 0 && out.mode.is_none() {
                    out.mode = Some(Mode {
                        width: width as u32,
                        height: height as u32,
                        // mHz to Hz, rounded (59940 is 60).
                        refresh: (refresh.max(0) as u32 + 500) / 1000,
                    });
                }
            }
            _ => {}
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

#[cfg(test)]
mod pick_tests {
    use super::*;

    fn out(name: &str, make: &str, model: &str) -> OutputInfo {
        OutputInfo {
            name: name.into(),
            make: make.into(),
            model: model.into(),
            ..Default::default()
        }
    }

    fn tv() -> DisplayInfo {
        DisplayInfo {
            connector: "HDMI-A-1".into(),
            make: "Sony".into(),
            model: "TV".into(),
            internal: false,
        }
    }

    #[test]
    fn picks_the_display_output() {
        let virt = out("xwayland-1", "gamescope", "Virtual");
        let real = out("HDMI-A-1", "Sony", "TV");
        let outs = [virt.clone(), real.clone()];
        // By connector name, then by make and model, then by not being gamescope's own.
        assert_eq!(pick_output(&outs, Some(&tv())).unwrap().name, "HDMI-A-1");
        let nameless = [virt.clone(), out("", "Sony", "TV")];
        assert_eq!(pick_output(&nameless, Some(&tv())).unwrap().make, "Sony");
        let unknown = [virt.clone(), out("", "X", "Y")];
        assert_eq!(pick_output(&unknown, None).unwrap().make, "X");
        // Only gamescope's own, or a lone output: that one.
        assert_eq!(pick_output(&[virt], None).unwrap().make, "gamescope");
        assert!(pick_output(&[], None).is_none());
    }
}
