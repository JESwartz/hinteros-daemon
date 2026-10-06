//! HinterOS session daemon.
//!
//! Serves the session API (`schema/session-api.json`) to the launcher and owns what happens on
//! the TV: which app gamescope shows ([`focus`]), where controller input goes ([`input`]), and
//! starting and tracking apps ([`apps`], [`reaper`]). [`shell::Shell`] ties these together; it
//! only talks to them through traits, so its behaviour is unit-tested with mocks.

pub mod apps;
pub mod config;
pub mod focus;
pub mod input;
pub mod reaper;
pub mod server;
pub mod shell;

/// An event as broadcast to every connection: name plus encoded params.
pub type Broadcast = (hinteros_api::session::EventName, serde_json::Value);
