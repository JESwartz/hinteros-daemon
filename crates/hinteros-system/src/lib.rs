//! HinterOS privileged system service.
//!
//! Runs as root and serves the internal system API (`schema/system-api.json`) on
//! `/run/hinteros/system.sock` to any process of a `hinteros` user (the session daemon relays
//! it to the launcher). Read-only methods are open to every connection; mutating ones pass the
//! gate in [`authz`] (pidfd, seat0 owner, polkit, rate limit) and are audited ([`audit`]).
//! Work that changes the system runs as a job in its own unit ([`job`], [`runner`]), which
//! edits a staging copy of `/etc/hinteros`, builds and switches, and only then replaces it.
//! See `hinteros-docs/design/system-api.md`.

pub mod audit;
pub mod authz;
pub mod config;
pub mod datadel;
pub mod flake;
pub mod fsutil;
pub mod generations;
pub mod job;
pub mod jobs;
pub mod nixlog;
pub mod peer;
pub mod runner;
pub mod service;
pub mod state;
pub mod store;
pub mod update;

#[cfg(test)]
mod props;
