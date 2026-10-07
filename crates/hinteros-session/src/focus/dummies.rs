//! Invisible override-redirect windows that cover an app.
//!
//! GTK3 apps create a 1x1 override-redirect GLX dummy window with nothing on it: no
//! `WM_CLASS`, no name, no window type. gamescope (with `--force-windows-fullscreen`) resizes
//! such a window to the whole output and paints it above the app's real window: a black screen.
//!
//! The watcher unmaps such a window, but only when everything says it is that dummy and
//! nothing else:
//! - it is mapped, override-redirect and as big as the root window;
//! - it has no `WM_CLASS`, title, `_NET_WM_WINDOW_TYPE` or content hints (an opacity, an
//!   overlay marker), and is not Steam's or the on-screen keyboard's;
//! - it belongs (XRes client or `_NET_WM_PID`) to an app the daemon launched, found by the
//!   reaper above the process, and that app also has a normal mapped top-level window.

use crate::osk;
use crate::steam::windows::WindowInfo;

/// What the dummy is blamed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dummy {
    pub window: u32,
    /// The gamescope app id of the app it belongs to.
    pub app_id: u32,
    /// A name for the log: the owning process.
    pub owner: String,
}

/// An override-redirect window that has nothing that says what it is (the shape of a GTK GL
/// dummy), whatever its size and owner.
pub fn is_blank_override_redirect(w: &WindowInfo) -> bool {
    w.mapped
        && w.override_redirect
        && w.input_output
        && w.wm_class.is_empty()
        && w.title.is_empty()
        && !w.has_window_type
        && !w.content_hints
        && w.helper_type.is_none()
        && w.steam_game.is_none()
        && !w.steam_overlay
        && !w.steam_bigpicture
}

/// A normal window the user can see: what the dummy hides.
fn is_toplevel(w: &WindowInfo) -> bool {
    w.mapped && w.input_output && !w.override_redirect && w.width > 1 && w.height > 1
}

/// The GL dummies among `windows`. `root` is the root window's size; `app_of` tells the
/// gamescope app id of the app a window belongs to (`None`: not one of ours).
pub fn find(
    windows: &[WindowInfo],
    root: (u32, u32),
    osk_pid: u32,
    mut app_of: impl FnMut(&WindowInfo) -> Option<u32>,
) -> Vec<Dummy> {
    let mut found = Vec::new();
    for w in windows {
        if !is_blank_override_redirect(w)
            || w.width < root.0
            || w.height < root.1
            || w.is_steam()
            || osk::is_osk_window(w, osk_pid)
        {
            continue;
        }
        let Some(app_id) = app_of(w) else { continue };
        if app_id == hinteros_api::session::STEAM_CLIENT_GAMESCOPE_APP_ID {
            continue;
        }
        let has_toplevel = windows
            .iter()
            .any(|t| t.window != w.window && is_toplevel(t) && app_of(t) == Some(app_id));
        if !has_toplevel {
            continue;
        }
        let owner = w
            .owners
            .iter()
            .find_map(|(pid, comm)| comm.clone().map(|c| format!("{c} (pid {pid})")))
            .unwrap_or_else(|| format!("app {app_id}"));
        found.push(Dummy {
            window: w.window,
            app_id,
            owner,
        });
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steam::windows::HelperType;

    const ROOT: (u32, u32) = (1280, 800);

    fn dummy(id: u32) -> WindowInfo {
        WindowInfo {
            window: id,
            mapped: true,
            override_redirect: true,
            input_output: true,
            width: 1280,
            height: 800,
            owners: vec![(50, Some("gtkapp".into()))],
            ..WindowInfo::default()
        }
    }

    fn toplevel(id: u32) -> WindowInfo {
        WindowInfo {
            window: id,
            mapped: true,
            input_output: true,
            width: 1280,
            height: 800,
            wm_class: vec!["gtkapp".into()],
            title: "GTK app".into(),
            owners: vec![(50, Some("gtkapp".into()))],
            ..WindowInfo::default()
        }
    }

    /// Every window belongs to app 7, except those owned by pid 99 (not ours).
    fn app(w: &WindowInfo) -> Option<u32> {
        w.owners.iter().all(|(p, _)| *p != 99).then_some(7)
    }

    fn found(windows: &[WindowInfo]) -> Vec<u32> {
        find(windows, ROOT, 0, app)
            .into_iter()
            .map(|d| d.window)
            .collect()
    }

    #[test]
    fn the_gl_dummy_next_to_its_window_is_found() {
        let found = find(&[toplevel(1), dummy(2)], ROOT, 0, app);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].window, found[0].app_id), (2, 7));
        assert_eq!(found[0].owner, "gtkapp (pid 50)");
    }

    #[test]
    fn needs_the_whole_picture() {
        // No real window of the app: nothing to uncover.
        assert!(found(&[dummy(2)]).is_empty());
        // Still 1x1 (not resized yet), or unmapped.
        let mut small = dummy(2);
        small.width = 1;
        small.height = 1;
        assert!(found(&[toplevel(1), small]).is_empty());
        let mut unmapped = dummy(2);
        unmapped.mapped = false;
        assert!(found(&[toplevel(1), unmapped]).is_empty());
        // The real window is not mapped or is itself an override-redirect one.
        let mut hidden = toplevel(1);
        hidden.mapped = false;
        assert!(found(&[hidden, dummy(2)]).is_empty());
        let mut popup = toplevel(1);
        popup.override_redirect = true;
        assert!(found(&[popup, dummy(2)]).is_empty());
        // Only one dummy per rescan is not assumed.
        assert_eq!(found(&[toplevel(1), dummy(2), dummy(3)]), [2, 3]);
    }

    #[test]
    fn leaves_everything_that_says_what_it_is() {
        type Tweak = (&'static str, fn(&mut WindowInfo));
        let tweaks: [Tweak; 8] = [
            ("class", |w| w.wm_class = vec!["menu".into()]),
            ("title", |w| w.title = "Popup".into()),
            ("type", |w| w.has_window_type = true),
            ("helper type", |w| w.helper_type = Some(HelperType::Menu)),
            ("hints", |w| w.content_hints = true),
            ("steam game", |w| w.steam_game = Some(7)),
            ("steam overlay", |w| w.steam_overlay = true),
            ("big picture", |w| w.steam_bigpicture = true),
        ];
        for (what, tweak) in tweaks {
            let mut w = dummy(2);
            tweak(&mut w);
            assert!(found(&[toplevel(1), w]).is_empty(), "{what}");
        }
    }

    #[test]
    fn leaves_steam_the_osk_and_other_apps() {
        // Steam's window (by owner process) with a blank override-redirect window.
        let mut steam = dummy(2);
        steam.owners = vec![(60, Some("steamwebhelper".into()))];
        assert!(found(&[toplevel(1), steam]).is_empty());
        // Steam's gamescope app id.
        let all_steam = |_: &WindowInfo| Some(769);
        assert!(find(&[toplevel(1), dummy(2)], ROOT, 0, all_steam).is_empty());
        // The on-screen keyboard.
        let mut osk = dummy(2);
        osk.owners = vec![(70, Some("hinteros-osk".into()))];
        assert!(found(&[toplevel(1), osk]).is_empty());
        // A process that is not under one of our reapers.
        let mut other = dummy(2);
        other.owners = vec![(99, Some("stranger".into()))];
        assert!(found(&[toplevel(1), other]).is_empty());
        // A window of another app does not count as the app's real window.
        let mut elsewhere = toplevel(1);
        elsewhere.owners = vec![(99, None)];
        assert!(found(&[elsewhere, dummy(2)]).is_empty());
    }
}
