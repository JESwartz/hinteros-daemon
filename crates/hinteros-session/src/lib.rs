//! HinterOS session daemon.
//!
//! Serves the session API (`schema/session-api.json`) to the launcher and owns what happens on
//! the TV: which app gamescope shows ([`focus`]), where controller input goes ([`input`]), and
//! starting and tracking apps ([`apps`], [`reaper`]), typing into apps ([`keys`]) and the
//! on-screen keyboard ([`osk`]). [`shell::Shell`] ties these together; it only talks to them
//! through traits, so its behaviour is unit-tested with mocks.

pub mod apps;
pub mod config;
pub mod focus;
pub mod input;
pub mod keys;
pub mod osk;
pub mod procs;
pub mod reaper;
pub mod server;
pub mod shell;
pub mod steam;

/// Which connections an event is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    /// Every subscribed connection.
    All,
    /// Connections of the on-screen keyboard (hello client name `hinteros-osk`).
    Osk,
    /// Every other connection (the launcher).
    Launcher,
}

/// An event as broadcast to the connections: name, encoded params and audience.
pub type Broadcast = (
    hinteros_api::session::EventName,
    serde_json::Value,
    Audience,
);
