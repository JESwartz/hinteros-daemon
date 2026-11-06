//! Recognising Steam's own windows on gamescope's Xwayland, and which of them want the user's
//! attention.
//!
//! In Steam mode gamescope only considers windows with an app id (`GetPossibleFocusWindows`:
//! app id != 0, or `window_is_steam`), and Steam does not run under a reaper. Current Steam
//! clients set `STEAM_GAME = 769` (gamescope's id for Steam itself) on their `steamwebhelper`
//! (CEF) windows themselves, even in desktop mode (observed with Steam build 1788652215 on a
//! desktop: main window, popup menus); windows of the `steam` process itself (and of older
//! clients) carry nothing and would never be shown. The daemon tags those with
//! `STEAM_GAME = 769` too, so every Steam window is focusable as app 769 without being shown:
//! gamescope shows only what `GAMESCOPECTRL_BASELAYER_APPID` asks for, and `[769, launcher]`
//! shows Steam.
//!
//! What we can observe about a window, and how it is used:
//! - owner process (XRes client id, else `_NET_WM_PID`) named `steam` or `steamwebhelper`, or
//!   `WM_CLASS` exactly `steam`/`steamwebhelper` (not `steam_app_<id>`, which is a Proton
//!   game), or `STEAM_GAME = 769` / `STEAM_BIGPICTURE` already set: a Steam window;
//! - `STEAM_GAME` set to anything else: a game window, never Steam's;
//! - `STEAM_OVERLAY`: Steam's overlay layer, left alone;
//! - override-redirect windows (menus, tooltips) and helper window types are tagged (so they
//!   show over Steam's other windows) but never count as needing attention, nor do unmapped,
//!   input-only or 1x1 windows.

use hinteros_api::session::{STEAM_CLIENT_GAMESCOPE_APP_ID, SteamWindow};

/// Process and `WM_CLASS` names of Steam's client windows.
pub const STEAM_NAMES: &[&str] = &["steam", "steamwebhelper"];

/// `_NET_WM_WINDOW_TYPE`s that are never dialogs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperType {
    Tooltip,
    Notification,
    Menu,
    Dock,
    Desktop,
    Dnd,
}

/// What we read about one top-level window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowInfo {
    pub window: u32,
    pub mapped: bool,
    pub override_redirect: bool,
    /// `InputOutput` class (an `InputOnly` window is never drawn).
    pub input_output: bool,
    pub width: u32,
    pub height: u32,
    /// `WM_CLASS`: instance and class.
    pub wm_class: Vec<String>,
    pub title: String,
    /// Owner processes found for it (XRes, `_NET_WM_PID`) and their names.
    pub owners: Vec<(u32, Option<String>)>,
    /// `STEAM_GAME`, if set.
    pub steam_game: Option<u32>,
    pub steam_overlay: bool,
    pub steam_bigpicture: bool,
    pub helper_type: Option<HelperType>,
    /// Any `_NET_WM_WINDOW_TYPE` is set (of a kind in `helper_type` or not).
    pub has_window_type: bool,
    /// Properties that say what the window is for: an opacity, `GAMESCOPE_EXTERNAL_OVERLAY`.
    pub content_hints: bool,
}

impl WindowInfo {
    fn steam_owner(&self) -> Option<u32> {
        self.owners
            .iter()
            .find(|(_, name)| name.as_deref().is_some_and(|n| STEAM_NAMES.contains(&n)))
            .map(|(pid, _)| *pid)
    }

    /// Whether this is one of Steam's own windows.
    pub fn is_steam(&self) -> bool {
        match self.steam_game {
            Some(STEAM_CLIENT_GAMESCOPE_APP_ID) => return true,
            Some(0) | None => {}
            Some(_) => return false, // a game's window
        }
        self.steam_bigpicture
            || self.steam_owner().is_some()
            || self
                .wm_class
                .iter()
                .any(|c| STEAM_NAMES.iter().any(|n| c.eq_ignore_ascii_case(n)))
    }

    /// Whether the daemon should set `STEAM_GAME = 769` on it.
    pub fn needs_tag(&self) -> bool {
        self.is_steam() && !self.steam_overlay && matches!(self.steam_game, None | Some(0))
    }

    /// Whether it is a Steam window the user should see (a dialog or Steam's main window).
    pub fn wants_attention(&self) -> bool {
        self.is_steam()
            && self.mapped
            && self.input_output
            && !self.override_redirect
            && !self.steam_overlay
            && self.helper_type.is_none()
            && self.width > 1
            && self.height > 1
    }

    pub fn to_api(&self) -> SteamWindow {
        SteamWindow {
            window: self.window,
            title: self.title.clone(),
            pid: self
                .steam_owner()
                .or_else(|| self.owners.first().map(|(pid, _)| *pid))
                .filter(|&p| p != 0),
        }
    }
}

/// The Steam windows among `windows` that want attention, in stacking order.
pub fn attention_candidates(windows: &[WindowInfo]) -> Vec<SteamWindow> {
    windows
        .iter()
        .filter(|w| w.wants_attention())
        .map(WindowInfo::to_api)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialog() -> WindowInfo {
        WindowInfo {
            window: 0x600003,
            mapped: true,
            input_output: true,
            width: 500,
            height: 300,
            wm_class: vec!["steamwebhelper".into(), "steam".into()],
            title: "Steam Guard".into(),
            owners: vec![(2412, Some("steamwebhelper".into()))],
            ..Default::default()
        }
    }

    #[test]
    fn steam_dialogs_want_attention_and_get_tagged() {
        let w = dialog();
        assert!(w.is_steam() && w.needs_tag() && w.wants_attention());
        assert_eq!(
            w.to_api(),
            SteamWindow {
                window: 0x600003,
                title: "Steam Guard".into(),
                pid: Some(2412)
            }
        );
        // Once tagged it is still Steam's, and needs no second tag.
        let tagged = WindowInfo {
            steam_game: Some(769),
            ..w.clone()
        };
        assert!(tagged.is_steam() && !tagged.needs_tag() && tagged.wants_attention());
    }

    #[test]
    fn recognised_by_process_or_class() {
        let by_pid = WindowInfo {
            wm_class: vec![],
            owners: vec![
                (1, Some("gamescope-test".into())),
                (7, Some("steam".into())),
            ],
            ..dialog()
        };
        assert!(by_pid.is_steam());
        assert_eq!(by_pid.to_api().pid, Some(7));
        let by_class = WindowInfo {
            wm_class: vec!["Steam".into(), "Steam".into()],
            owners: vec![(1, Some("xterm".into()))],
            ..dialog()
        };
        assert!(by_class.is_steam());
        assert_eq!(by_class.to_api().pid, Some(1));
        let bigpicture = WindowInfo {
            wm_class: vec![],
            owners: vec![],
            steam_bigpicture: true,
            ..dialog()
        };
        assert!(bigpicture.is_steam());
        assert_eq!(bigpicture.to_api().pid, None);
    }

    #[test]
    fn not_steam() {
        // A Proton game's window: WM_CLASS steam_app_<id>, its own STEAM_GAME.
        let game = WindowInfo {
            wm_class: vec!["steam_app_620".into(), "steam_app_620".into()],
            owners: vec![(99, Some("portal2.exe".into()))],
            steam_game: Some(620),
            ..dialog()
        };
        assert!(!game.is_steam() && !game.needs_tag() && !game.wants_attention());
        // Something Steam started but that a game tagged (e.g. a launcher): not Steam's.
        let tagged_game = WindowInfo {
            steam_game: Some(620),
            ..dialog()
        };
        assert!(!tagged_game.is_steam());
        let launcher = WindowInfo {
            wm_class: vec!["hinteros-launcher".into()],
            owners: vec![(5, Some("hinteros-launch".into()))],
            ..dialog()
        };
        assert!(!launcher.is_steam());
    }

    #[test]
    fn helpers_are_tagged_but_need_no_attention() {
        for w in [
            WindowInfo {
                override_redirect: true,
                ..dialog()
            },
            WindowInfo {
                helper_type: Some(HelperType::Tooltip),
                ..dialog()
            },
            WindowInfo {
                width: 1,
                height: 1,
                ..dialog()
            },
            WindowInfo {
                input_output: false,
                ..dialog()
            },
            WindowInfo {
                mapped: false,
                ..dialog()
            },
        ] {
            assert!(w.needs_tag() && !w.wants_attention(), "{w:?}");
        }
        let overlay = WindowInfo {
            steam_overlay: true,
            ..dialog()
        };
        assert!(!overlay.needs_tag() && !overlay.wants_attention());
        assert_eq!(
            attention_candidates(&[overlay, dialog()]),
            [dialog().to_api()]
        );
    }

    /// Steam's windows as `xprop`/`xwininfo` showed them on a desktop (Steam build 1788652215).
    #[test]
    fn observed_desktop_steam_windows() {
        let webhelper = Some("steamwebhelper".to_string());
        let main = WindowInfo {
            window: 0x300001f,
            mapped: true,
            input_output: true,
            width: 1920,
            height: 1036,
            wm_class: vec!["steamwebhelper".into(), "steam".into()],
            title: "Steam".into(),
            owners: vec![(690_543, webhelper.clone())],
            steam_game: Some(769),
            ..Default::default()
        };
        assert!(main.is_steam() && !main.needs_tag() && main.wants_attention());
        let popup_menu = WindowInfo {
            window: 0x300006f,
            override_redirect: true,
            input_output: true,
            width: 102,
            height: 100,
            helper_type: Some(HelperType::Menu),
            ..main.clone()
        };
        assert!(popup_menu.is_steam() && !popup_menu.wants_attention());
        // Windows of the `steam` process itself: no STEAM_GAME, tagged by us.
        let steam_proc = WindowInfo {
            window: 0x1e00001,
            input_output: true,
            width: 10,
            height: 10,
            wm_class: vec!["steam".into(), "Steam".into()],
            title: "steam".into(),
            owners: vec![(690_130, Some("steam".into()))],
            ..Default::default()
        };
        assert!(steam_proc.is_steam() && steam_proc.needs_tag() && !steam_proc.wants_attention());
        let no_pid = WindowInfo {
            window: 0x1c0000c,
            wm_class: vec!["Steam".into(), "Steam".into()],
            title: "Untitled".into(),
            ..Default::default()
        };
        assert!(no_pid.is_steam() && no_pid.needs_tag());
        let input_only = WindowInfo {
            window: 0x2800001,
            input_output: false,
            mapped: true,
            width: 10,
            height: 10,
            wm_class: vec!["steamwebhelper".into(), "Steamwebhelper".into()],
            owners: vec![(690_543, webhelper)],
            steam_game: Some(769),
            ..Default::default()
        };
        assert!(input_only.is_steam() && !input_only.wants_attention());
    }
}
