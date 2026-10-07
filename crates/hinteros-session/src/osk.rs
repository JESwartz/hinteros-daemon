//! The on-screen keyboard (OSK): a separate client process (`hinteros-osk`, built in the
//! launcher repo) that the daemon starts, shows over the focused app and feeds with
//! `input_nav`. See hinteros-docs/spikes/05-on-screen-keyboard.md.
//!
//! Showing it: the OSK's X window on gamescope's Xwayland gets `GAMESCOPE_EXTERNAL_OVERLAY = 1`,
//! which makes gamescope paint it on top of everything, at native size, without ever giving
//! it focus (keys keep going to the app). It stays mapped and resident; gamescope only paints
//! it while `_NET_WM_WINDOW_OPACITY` is non-zero, so show/hide is a property write. The shell
//! publishes the wanted state as `HINTEROS_OSK_VISIBLE` on the root window
//! ([`crate::focus::FocusControl::set_osk_visible`]) and the gamescope watcher, which sees
//! every window appear, applies it to the OSK's windows (also to a restarted OSK's new ones).
//!
//! Which windows are the OSK's: `WM_CLASS` `hinteros-osk` (instance or class, any case), an
//! owner process named `hinteros-osk` (also `.hinteros-osk-wrapped`, a Nix wrapper), or owned
//! by the process the daemon started.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tracing::{error, info, warn};

use crate::apps::which;
use crate::procs::Children;
use crate::steam::windows::WindowInfo;

pub const OSK_NAME: &str = "hinteros-osk";

/// Root window property with the wanted visibility (CARDINAL 0/1), written by the shell.
pub const VISIBLE_ATOM: &str = "HINTEROS_OSK_VISIBLE";

/// `_NET_WM_WINDOW_OPACITY` values.
pub const OPAQUE: u32 = 0xffff_ffff;
pub const TRANSPARENT: u32 = 0;

/// Whether a top-level window belongs to the OSK. `osk_pid` is the process the daemon started
/// (0: none).
pub fn is_osk_window(info: &WindowInfo, osk_pid: u32) -> bool {
    let named = |s: &str| {
        let s = s.trim_start_matches('.');
        s.len() >= OSK_NAME.len() && s[..OSK_NAME.len()].eq_ignore_ascii_case(OSK_NAME)
    };
    info.wm_class
        .iter()
        .any(|c| c.eq_ignore_ascii_case(OSK_NAME))
        || info.owners.iter().any(|(pid, comm)| {
            (osk_pid != 0 && *pid == osk_pid) || comm.as_deref().is_some_and(named)
        })
}

/// Keep the OSK running, restarting it when it exits; its pid goes to `pid` (0 while it is
/// not running). Not under the reaper: it must not become a focusable app. Does nothing if the
/// program cannot be found (dev desktops, tests).
pub async fn supervise(
    argv: Vec<String>,
    env: Vec<(String, String)>,
    pid: Arc<AtomicU32>,
    children: Children,
) {
    let Some(program) = argv.first() else { return };
    let program: PathBuf = match which(program) {
        Ok(p) => p,
        Err(e) => {
            info!("not starting the on-screen keyboard: {e}");
            return;
        }
    };
    while !children.is_closing() {
        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&argv[1..])
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        match cmd.spawn() {
            Ok(mut child) => {
                let id = child.id().unwrap_or_default();
                if !children.add(id) {
                    let _ = child.start_kill();
                    return;
                }
                pid.store(id, Ordering::SeqCst);
                info!(pid = id, ?argv, "on-screen keyboard started");
                match child.wait().await {
                    Ok(status) => warn!(%status, "on-screen keyboard exited; restarting"),
                    Err(e) => warn!("waiting for the on-screen keyboard: {e}"),
                }
                children.remove(id);
                pid.store(0, Ordering::SeqCst);
            }
            Err(e) => error!(?argv, "cannot start the on-screen keyboard: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(class: &[&str], owners: &[(u32, Option<&str>)]) -> WindowInfo {
        WindowInfo {
            window: 1,
            wm_class: class.iter().map(|s| s.to_string()).collect(),
            owners: owners
                .iter()
                .map(|(p, c)| (*p, c.map(String::from)))
                .collect(),
            ..WindowInfo::default()
        }
    }

    #[test]
    fn recognises_the_osk() {
        assert!(is_osk_window(
            &window(&["hinteros-osk", "Hinteros-osk"], &[]),
            0
        ));
        assert!(is_osk_window(&window(&["x", "HINTEROS-OSK"], &[]), 0));
        assert!(is_osk_window(&window(&[], &[(5, Some("hinteros-osk"))]), 0));
        assert!(is_osk_window(
            &window(&[], &[(5, Some(".hinteros-osk-w"))]),
            0
        ));
        assert!(is_osk_window(&window(&["flutter"], &[(42, Some("x"))]), 42));
        assert!(!is_osk_window(
            &window(&["firefox"], &[(5, Some("firefox"))]),
            42
        ));
        assert!(!is_osk_window(
            &window(&["firefox"], &[(5, Some("hinteros"))]),
            0
        ));
        assert!(!is_osk_window(&window(&[], &[]), 0));
    }
}
