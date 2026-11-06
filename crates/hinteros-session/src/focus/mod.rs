//! Which app gamescope shows.
//!
//! gamescope (in Steam mode, `-e`) picks its focused window from an app id list on its
//! Xwayland root window, `GAMESCOPECTRL_BASELAYER_APPID`: the first id that has a mapped window
//! wins. Window app ids come from a `STEAM_GAME` property, the systemd scope name, or a
//! `reaper SteamLaunch AppId=<id>` ancestor process (see [`crate::reaper`]). gamescope
//! publishes the result on the root as `GAMESCOPE_FOCUSED_APP` and the app ids that own
//! Xwayland windows as `GAMESCOPE_FOCUSABLE_APPS`.
//!
//! [`FocusControl`] is the seam: [`gamescope::GamescopeFocus`] is the real thing, [`NoFocus`]
//! is used when gamescope integration is off, and tests use a recording mock.

pub mod dummies;
pub mod gamescope;

/// The focus-related properties gamescope publishes on its Xwayland root window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GamescopeState {
    /// `GAMESCOPE_FOCUSED_APP`: app id of the window with input focus, if any.
    pub focused_app: Option<u32>,
    /// `GAMESCOPE_FOCUSABLE_APPS`: app ids that own focusable Xwayland windows (native Wayland
    /// windows are not listed).
    pub focusable_apps: Vec<u32>,
    /// `GAMESCOPECTRL_BASELAYER_APPID`: the focus order last written (by us, or by Steam).
    pub baselayer_apps: Vec<u32>,
}

/// What the gamescope watcher reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GamescopeEvent {
    /// Connected (again): gamescope may have restarted, so focus should be re-applied.
    Connected,
    /// One of the focus properties changed.
    State(GamescopeState),
    /// The Steam windows that want attention changed (see [`crate::steam::windows`]): every
    /// mapped Steam dialog or main window, in stacking order.
    SteamWindows(Vec<hinteros_api::session::SteamWindow>),
    /// Lost the connection; the watcher keeps retrying.
    Disconnected,
}

/// Controls which app gamescope shows.
pub trait FocusControl: Send {
    /// App ids that own focusable Xwayland windows.
    fn focusable_apps(&mut self) -> anyhow::Result<Vec<u32>>;
    /// App id of the focused window, if any.
    fn focused_app(&mut self) -> anyhow::Result<Option<u32>>;
    /// Show the launcher.
    fn focus_launcher(&mut self) -> anyhow::Result<()>;
    /// Show the windows of `app_id` as soon as it has one; the launcher until then.
    fn focus_app(&mut self, app_id: u32) -> anyhow::Result<()>;
    /// Show a Steam game's windows as soon as it has one, Steam's own windows (app 769: shader
    /// pre-caching, launch errors) until then, else the launcher
    /// ([`baselayer_for_steam_game`]).
    fn focus_steam_game(&mut self, app_id: u32) -> anyhow::Result<()> {
        self.focus_app(app_id)
    }
    /// Show Steam (Big Picture) with the games it started itself (from its library) in front
    /// ([`baselayer_for_steam_client`]).
    fn focus_steam_client(&mut self, games: &[u32]) -> anyhow::Result<()> {
        let _ = games;
        self.focus_app(hinteros_api::session::STEAM_CLIENT_GAMESCOPE_APP_ID)
    }
    /// Show or hide the on-screen keyboard's overlay window (see [`crate::osk`]).
    fn set_osk_visible(&mut self, visible: bool) -> anyhow::Result<()>;
}

/// The `GAMESCOPECTRL_BASELAYER_APPID` list that shows `target` (`None`: the launcher), with the
/// launcher as the fallback while `target` has no window.
pub fn baselayer_for(target: Option<u32>, launcher_app_id: u32) -> Vec<u32> {
    match target {
        Some(id) if id != launcher_app_id => vec![id, launcher_app_id],
        _ => vec![launcher_app_id],
    }
}

/// The `GAMESCOPECTRL_BASELAYER_APPID` list for a Steam game: the game, Steam's own windows
/// while the game has none (Steam's "Processing Vulkan shaders" pre-launch window, launch
/// errors, cloud sync), then the launcher.
pub fn baselayer_for_steam_game(app_id: u32, launcher_app_id: u32) -> Vec<u32> {
    vec![
        app_id,
        hinteros_api::session::STEAM_CLIENT_GAMESCOPE_APP_ID,
        launcher_app_id,
    ]
}

/// The `GAMESCOPECTRL_BASELAYER_APPID` list for Steam (Big Picture) while it runs games it
/// started itself: those games' windows first (they have their own app ids: the shortcut's,
/// or the one the game's Steam API reports), then Steam's windows, then the launcher.
pub fn baselayer_for_steam_client(games: &[u32], launcher_app_id: u32) -> Vec<u32> {
    games
        .iter()
        .copied()
        .chain([
            hinteros_api::session::STEAM_CLIENT_GAMESCOPE_APP_ID,
            launcher_app_id,
        ])
        .collect()
}

/// Focus control when gamescope integration is off (dev desktops): does nothing.
pub struct NoFocus;

impl FocusControl for NoFocus {
    fn focusable_apps(&mut self) -> anyhow::Result<Vec<u32>> {
        Ok(Vec::new())
    }
    fn focused_app(&mut self) -> anyhow::Result<Option<u32>> {
        Ok(None)
    }
    fn focus_launcher(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn focus_app(&mut self, _app_id: u32) -> anyhow::Result<()> {
        Ok(())
    }
    fn set_osk_visible(&mut self, _visible: bool) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baselayer_puts_the_launcher_last() {
        assert_eq!(baselayer_for(None, 9), vec![9]);
        assert_eq!(baselayer_for(Some(570), 9), vec![570, 9]);
        assert_eq!(baselayer_for(Some(9), 9), vec![9]);
        assert_eq!(baselayer_for_steam_game(570, 9), vec![570, 769, 9]);
        assert_eq!(baselayer_for_steam_client(&[], 9), vec![769, 9]);
        assert_eq!(
            baselayer_for_steam_client(&[5, 480], 9),
            vec![5, 480, 769, 9]
        );
    }
}
