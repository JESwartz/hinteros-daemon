//! Display modes under gamescope.
//!
//! What gamescope (3.16.31, read from its source) offers, and what this uses:
//!
//! * **Listing modes**: with `GAMESCOPE_PATCHED_EDID_FILE=<path>` set, gamescope writes the
//!   connector's modes, one `WxH@R` per line in its own preference order, to `<path>.modes`
//!   every time it picks a connector. The first line is the display's preferred mode.
//! * **The current mode**: gamescope's Wayland `wl_output` (all are read, the display's is picked by
//!   connector name, else make/model, else not gamescope's own virtual one; see `control::pick_output`)
//!   reports the output size and refresh (`wl_output.mode`, current flag), and its private
//!   `gamescope_control` protocol names the display (`active_display_info`: make, model,
//!   internal or external). Both arrive when a client binds ([`control`]).
//! * **Changing it**: there is no request for it in `gamescope_control` or `gamescope_private`
//!   (only convars like `backend_set_dirty`). But when gamescope picks the mode for an
//!   *external* display and no `-W/-H/-r` was given, it looks the display up by name
//!   ("<make> <model>") in the file `GAMESCOPE_MODE_SAVE_FILE` (`<name>:WxH@R` lines), and any
//!   client can make it choose again by setting the `GAMESCOPE_DISPLAY_MODE_NUDGE` property on
//!   its Xwayland root window (`steamcompmgr.cpp` then calls `DirtyState(true)`, which
//!   re-runs `setup_best_connector`, picks the saved mode and does a modeset). So
//!   `display.set_mode` writes the display's line in the save file and nudges gamescope. That
//!   is also the persistence: the next start reads the same file, so the mode survives
//!   restarts of gamescope and the session without any rebuild.
//! * **Not changing it**: if gamescope runs with `-W`, `-H` or `-r`, those win and a nudge
//!   changes nothing; `display.set_mode` notices (the mode does not change), puts the file
//!   back and answers `device_failed {reason: not_applied}`. Internal panels ignore the save
//!   file too, so `can_set` is false for them.
//!
//! A kept (or reverted) mode change restarts the on-screen keyboard, whose overlay window gamescope
//! sizes only when it maps it. On a failed switch the log says what gamescope showed.
//!
//! The 15-second keep-or-revert flow: `display.set_mode` remembers the mode it replaced,
//! `display.keep` forgets it, `display.revert` goes back to it. The daemon reverts by itself
//! when neither comes within `[display] revert_after_ms` (the launcher may have crashed), and
//! puts the save file back at its next start when it stopped before either (a crash, a power
//! cut): only a kept mode outlives the session.

pub mod control;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hinteros_api::ErrorObject;
use hinteros_api::session::{
    DisplayModeInfo, DisplayModes, DisplayRevertResult, DisplaySetResult, Empty, Mode, Service,
};
use tokio::time::{Instant, sleep, timeout};
use tracing::{debug, info, warn};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode};
use x11rb::wrapper::ConnectionExt as _;

use super::Ctx;
use super::dbus::{failed, not_found, unavailable};
use control::DisplayInfo;

const SVC: Service = Service::Display;

/// What is shared between calls.
#[derive(Default)]
pub struct Shared {
    /// One mode change at a time.
    op: tokio::sync::Mutex<()>,
    /// The mode the last un-kept `display.set_mode` replaced.
    pending: Mutex<Option<Mode>>,
    /// The display that change is on (its description, the key in gamescope's save file) and
    /// the mode it got.
    pending_display: Mutex<Option<(String, Mode)>>,
    /// The display of the last change `display.keep` kept, until the server takes it
    /// ([`take_kept`]).
    kept: Mutex<Option<(String, Mode)>>,
    /// Counts the mode changes: the daemon's own revert deadline belongs to the latest.
    changes: AtomicU64,
}

impl Shared {
    fn pending(&self) -> std::sync::MutexGuard<'_, Option<Mode>> {
        lock(&self.pending)
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn modes_path(ctx: &Ctx) -> Option<PathBuf> {
    ctx.display.modes_file.clone().or_else(|| {
        env_path("GAMESCOPE_PATCHED_EDID_FILE").map(|mut p| {
            let mut name = p.file_name().unwrap_or_default().to_os_string();
            name.push(".modes");
            p.set_file_name(name);
            p
        })
    })
}

fn save_path(ctx: &Ctx) -> Option<PathBuf> {
    ctx.display
        .save_file
        .clone()
        .or_else(|| env_path("GAMESCOPE_MODE_SAVE_FILE"))
}

/// `WxH@R` lines, as gamescope writes them.
pub fn parse_modes(text: &str) -> Vec<Mode> {
    text.lines()
        .filter_map(|line| {
            let (size, refresh) = line.trim().split_once('@')?;
            let (w, h) = size.split_once('x')?;
            let mode = Mode {
                width: w.parse().ok()?,
                height: h.parse().ok()?,
                refresh: refresh.parse().ok()?,
            };
            // gamescope's own sanity limits.
            (mode.width > 0
                && mode.height > 0
                && mode.refresh > 0
                && mode.width <= 16384
                && mode.height <= 16384
                && mode.refresh <= 1000)
                .then_some(mode)
        })
        .collect()
}

fn mode_text(m: Mode) -> String {
    format!("{}x{}@{}", m.width, m.height, m.refresh)
}

/// The save file with `description`'s line set to `mode` (added if missing). Other lines, and a
/// trailing field on this one (gamescope's broadcast RGB mode), are kept.
pub fn upsert_saved_mode(content: &str, description: &str, mode: Mode) -> String {
    let mut out = String::new();
    let mut done = false;
    for line in content.lines() {
        match line.split_once(':') {
            Some((d, rest)) if !done && d == description => {
                // `WxH@R[ extra]`
                let extra = rest.split_once(' ').map(|(_, e)| e).unwrap_or("");
                out.push_str(&format!("{description}:{}", mode_text(mode)));
                if !extra.is_empty() {
                    out.push(' ');
                    out.push_str(extra);
                }
                done = true;
            }
            _ => out.push_str(line),
        }
        out.push('\n');
    }
    if !done {
        out.push_str(&format!("{description}:{}\n", mode_text(mode)));
    }
    out
}

fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".hinteros-tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// What gamescope says now, if it can be reached.
async fn query(ctx: &Ctx) -> Option<control::Info> {
    let display = ctx.wayland_display.clone();
    let result = timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || control::query(&display)),
    )
    .await;
    match result {
        Ok(Ok(Ok(info))) => Some(info),
        Ok(Ok(Err(e))) => {
            debug!("gamescope's Wayland display: {e:#}");
            None
        }
        _ => None,
    }
}

fn need_gamescope(ctx: &Ctx) -> Result<&str, ErrorObject> {
    ctx.x_display.as_deref().ok_or_else(|| {
        unavailable(
            SVC,
            "gamescope integration is off ([gamescope] enable = false)",
        )
    })
}

pub async fn modes(ctx: &Ctx) -> Result<DisplayModes, ErrorObject> {
    need_gamescope(ctx)?;
    let info = query(ctx).await;
    let listed = modes_path(ctx)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| parse_modes(&t))
        .unwrap_or_default();
    let current = info.as_ref().and_then(|i| i.mode);
    if info.is_none() && listed.is_empty() {
        return Err(unavailable(
            SVC,
            format!(
                "gamescope's Wayland display {} is not reachable and it shares no mode list \
                 (GAMESCOPE_PATCHED_EDID_FILE)",
                ctx.wayland_display
            ),
        ));
    }
    let mut modes: Vec<DisplayModeInfo> = Vec::new();
    for (i, m) in listed.iter().enumerate() {
        if modes
            .iter()
            .any(|x| (x.width, x.height, x.refresh) == (m.width, m.height, m.refresh))
        {
            continue;
        }
        modes.push(DisplayModeInfo {
            width: m.width,
            height: m.height,
            refresh: m.refresh,
            current: current == Some(*m),
            preferred: i == 0,
        });
    }
    // A mode in use that the list lacks (set with -W/-H/-r) is still the current one.
    if let Some(c) = current
        && !modes.iter().any(|m| m.current)
    {
        modes.insert(
            0,
            DisplayModeInfo {
                width: c.width,
                height: c.height,
                refresh: c.refresh,
                current: true,
                preferred: false,
            },
        );
    }
    let display = info.as_ref().and_then(|i| i.display.as_ref());
    let external = display.is_some_and(|d| !d.internal);
    Ok(DisplayModes {
        modes,
        display: display.map(DisplayInfo::title).filter(|t| !t.is_empty()),
        can_set: external && save_path(ctx).is_some() && !listed.is_empty(),
    })
}

/// Set `GAMESCOPE_DISPLAY_MODE_NUDGE` on gamescope's Xwayland root: it picks the display's
/// mode again.
fn nudge(display: &str) -> Result<(), ErrorObject> {
    let display = &crate::config::resolve_display(display);
    let (conn, screen) = x11rb::connect(Some(display)).map_err(|e| {
        unavailable(
            SVC,
            format!("cannot reach gamescope's Xwayland {display}: {e}"),
        )
    })?;
    let root = conn.setup().roots[screen].root;
    let bad =
        |e: &dyn std::fmt::Display| failed("nudge_failed", format!("cannot nudge gamescope: {e}"));
    let atom = conn
        .intern_atom(false, b"GAMESCOPE_DISPLAY_MODE_NUDGE")
        .map_err(|e| bad(&e))?
        .reply()
        .map_err(|e| bad(&e))?
        .atom;
    conn.change_property32(PropMode::REPLACE, root, atom, AtomEnum::CARDINAL, &[1])
        .map_err(|e| bad(&e))?;
    // Round trip: the property is set when this returns.
    conn.get_input_focus()
        .map_err(|e| bad(&e))?
        .reply()
        .map_err(|e| bad(&e))?;
    Ok(())
}

/// Make gamescope run `target` on the display described by `info`; the file is put back when
/// it does not.
async fn apply(ctx: &Ctx, info: &control::Info, target: Mode) -> Result<(), ErrorObject> {
    let x = need_gamescope(ctx)?;
    let display = info.display.as_ref().ok_or_else(|| {
        unavailable(
            SVC,
            "gamescope did not say which display this is (gamescope_control)",
        )
    })?;
    if display.internal {
        return Err(unavailable(
            SVC,
            "the internal display's mode cannot be changed",
        ));
    }
    let save = save_path(ctx).ok_or_else(|| {
        unavailable(
            SVC,
            "GAMESCOPE_MODE_SAVE_FILE is not set, so gamescope would not remember a mode",
        )
    })?;
    let description = display.description();
    if description.contains(':') || description.contains('\n') || description.len() > 255 {
        return Err(failed(
            "unsupported_display",
            format!("gamescope cannot save a mode for the display '{description}'"),
        ));
    }
    let before = std::fs::read_to_string(&save).unwrap_or_default();
    write_atomically(&save, &upsert_saved_mode(&before, &description, target)).map_err(|e| {
        failed(
            "save_failed",
            format!("cannot write {}: {e}", save.display()),
        )
    })?;
    info!(display = %description, mode = %mode_text(target), "asking gamescope for a display mode");
    let x = x.to_owned();
    tokio::task::spawn_blocking(move || nudge(&x))
        .await
        .map_err(|e| failed("nudge_failed", e.to_string()))??;

    let deadline = Instant::now() + Duration::from_millis(ctx.display.apply_timeout_ms);
    let last;
    loop {
        sleep(Duration::from_millis(150)).await;
        let seen = query(ctx).await;
        if seen.as_ref().and_then(|i| i.mode) == Some(target) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            last = seen;
            break;
        }
    }
    let saw = match last {
        Some(i) => i.describe_outputs(),
        None => "gamescope's Wayland display not reachable".into(),
    };
    warn!(
        "gamescope did not switch: expected {}, saw {saw}; restoring the mode file",
        mode_text(target)
    );
    if let Err(e) = write_atomically(&save, &before) {
        warn!("cannot restore {}: {e}", save.display());
    }
    Err(failed(
        "not_applied",
        format!(
            "gamescope did not switch to {} (is it started with -W/-H/-r?)",
            mode_text(target)
        ),
    ))
}

pub async fn set_mode(ctx: &Arc<Ctx>, target: Mode) -> Result<DisplaySetResult, ErrorObject> {
    need_gamescope(ctx)?;
    let _one = ctx.display_state.op.lock().await;
    let info = query(ctx).await.ok_or_else(|| {
        unavailable(
            SVC,
            format!(
                "gamescope's Wayland display {} is not reachable",
                ctx.wayland_display
            ),
        )
    })?;
    let listed = modes_path(ctx)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| parse_modes(&t))
        .unwrap_or_default();
    if !listed.contains(&target) {
        return Err(not_found(format!(
            "the display has no {} mode",
            mode_text(target)
        )));
    }
    // Without the mode in use there is nothing to compare with (a change could not be seen)
    // and nothing to revert to.
    let previous = info.mode.ok_or_else(|| {
        unavailable(
            SVC,
            format!(
                "gamescope reports no current mode ({})",
                info.describe_outputs()
            ),
        )
    })?;
    let mut revert_to = previous;
    if previous != target {
        let first = ctx.display_state.pending().is_none();
        if first {
            remember_confirmed(ctx);
        }
        if let Err(e) = apply(ctx, &info, target).await {
            if first {
                forget_confirmed(ctx);
            }
            return Err(e);
        }
        // A second change before the first was kept still reverts to the mode before both.
        revert_to = *ctx.display_state.pending().get_or_insert(previous);
        *lock(&ctx.display_state.pending_display) =
            info.display.as_ref().map(|d| (d.description(), target));
        let change = ctx.display_state.changes.fetch_add(1, Ordering::SeqCst) + 1;
        revert_unless_kept(ctx, change);
    }
    Ok(DisplaySetResult {
        previous: revert_to,
        current: target,
    })
}

/// Where the save file as it was before an unconfirmed change is kept, next to it.
fn confirmed_path(save: &Path) -> PathBuf {
    let mut p = save.as_os_str().to_os_string();
    p.push(".confirmed");
    PathBuf::from(p)
}

/// Keep the save file as it is now (the last confirmed modes), until the change about to be
/// made is kept or reverted. If the daemon stops first (a crash, a power cut), it puts this
/// back at its next start ([`restore_confirmed`]): an unconfirmed mode must not become the mode
/// of every later start.
fn remember_confirmed(ctx: &Ctx) {
    let Some(save) = save_path(ctx) else {
        return;
    };
    let before = std::fs::read_to_string(&save).unwrap_or_default();
    if let Err(e) = write_atomically(&confirmed_path(&save), &before) {
        warn!("cannot keep a copy of {}: {e}", save.display());
    }
}

/// The change was kept or reverted (or never made).
fn forget_confirmed(ctx: &Ctx) {
    if let Some(save) = save_path(ctx) {
        let path = confirmed_path(&save);
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            warn!("cannot remove {}: {e}", path.display());
        }
    }
}

/// At start: a change that was neither kept nor reverted when the daemon last stopped is
/// undone in the save file, and gamescope is asked to pick its mode again (if it runs yet and
/// is known). An empty copy means there was no save file: it is removed again.
pub fn restore_confirmed(ctx: &Ctx) {
    let Some(save) = save_path(ctx) else {
        return;
    };
    let path = confirmed_path(&save);
    let Ok(before) = std::fs::read_to_string(&path) else {
        return;
    };
    warn!(
        "a display mode change was not kept before the last stop; restoring {}",
        save.display()
    );
    let restored = if before.is_empty() {
        std::fs::remove_file(&save).or_else(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(e),
        })
    } else {
        write_atomically(&save, &before)
    };
    if let Err(e) = restored {
        warn!("cannot restore {}: {e}", save.display());
        return;
    }
    if let Err(e) = std::fs::remove_file(&path) {
        warn!("cannot remove {}: {e}", path.display());
    }
    let Some(x) = ctx.x_display.clone() else {
        return;
    };
    tokio::task::spawn_blocking(move || {
        if let Err(e) = nudge(&x) {
            debug!("nudging gamescope after restoring the mode: {}", e.message);
        }
    });
}

/// The daemon's own deadline for a change ([`DisplayConfig::revert_after_ms`]): if neither
/// `display.keep` nor another change comes in time, the change is reverted. The launcher's
/// dialog does the same after 15 s; this covers a launcher that crashed or restarted meanwhile.
///
/// [`DisplayConfig::revert_after_ms`]: crate::config::DisplayConfig::revert_after_ms
fn revert_unless_kept(ctx: &Arc<Ctx>, change: u64) {
    let after = Duration::from_millis(ctx.display.revert_after_ms);
    if after.is_zero() {
        return;
    }
    let ctx = ctx.clone();
    tokio::spawn(async move {
        sleep(after).await;
        match revert_change(&ctx, Some(change)).await {
            Ok(r) => warn!(
                "display mode not kept within {after:?}; back to {}",
                mode_text(r.current)
            ),
            Err(e)
                if e.data
                    .as_ref()
                    .is_some_and(|d| d["reason"] == "nothing_to_revert") => {}
            Err(e) => warn!(
                "reverting the display mode that was not kept: {}",
                e.message
            ),
        }
    });
}

/// The on-screen keyboard is an overlay that gamescope sizes when it maps the window, so it
/// is started again to pick up the new screen size.
fn restart_osk(ctx: &Ctx) {
    if crate::osk::restart(&ctx.osk_pid) {
        info!("restarting the on-screen keyboard for the new display mode");
    }
}

pub async fn keep(ctx: &Ctx) -> Result<Empty, ErrorObject> {
    let changed = ctx.display_state.pending().take().is_some();
    let display = lock(&ctx.display_state.pending_display).take();
    if changed {
        *lock(&ctx.display_state.kept) = display;
        forget_confirmed(ctx);
        restart_osk(ctx);
    }
    Ok(Empty {})
}

/// The display whose mode the last `display.keep` kept (its save file key) and that mode, once:
/// the server saves it for every profile.
pub fn take_kept(shared: &Shared) -> Option<(String, Mode)> {
    lock(&shared.kept).take()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub async fn revert(ctx: &Ctx) -> Result<DisplayRevertResult, ErrorObject> {
    revert_change(ctx, None).await
}

/// Revert the pending change; with `only`, only if that is still the latest change.
async fn revert_change(ctx: &Ctx, only: Option<u64>) -> Result<DisplayRevertResult, ErrorObject> {
    let _one = ctx.display_state.op.lock().await;
    let nothing = || failed("nothing_to_revert", "no display change to revert");
    if only.is_some_and(|c| c != ctx.display_state.changes.load(Ordering::SeqCst)) {
        return Err(nothing());
    }
    // Forgotten only once the old mode is back: a failed revert can be tried again.
    let previous = ctx.display_state.pending().ok_or_else(nothing)?;
    let info = query(ctx).await.ok_or_else(|| {
        unavailable(
            SVC,
            format!(
                "gamescope's Wayland display {} is not reachable",
                ctx.wayland_display
            ),
        )
    })?;
    if info.mode != Some(previous) {
        apply(ctx, &info, previous).await?;
        restart_osk(ctx);
    }
    ctx.display_state.pending().take();
    lock(&ctx.display_state.pending_display).take();
    forget_confirmed(ctx);
    Ok(DisplayRevertResult { current: previous })
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: Mode = Mode {
        width: 1920,
        height: 1080,
        refresh: 60,
    };

    #[test]
    fn parses_gamescopes_mode_list() {
        let list = parse_modes("3840x2160@60\n1920x1080@120\nnot a mode\n0x0@0\n1280x720@60");
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].width, 3840);
        assert_eq!(list[1].refresh, 120);
        assert!(parse_modes("").is_empty());
        // Gamescope's own limits.
        assert!(parse_modes("20000x1080@60\n1920x1080@5000").is_empty());
    }

    #[test]
    fn save_file_gets_one_line_per_display() {
        let out = upsert_saved_mode("", "Sony TV", M);
        assert_eq!(out, "Sony TV:1920x1080@60\n");
        // Another display's line stays; this one is replaced in place, extra field kept.
        let start = "Other 1:1280x720@50 2\nSony TV:3840x2160@30 1\nLast:1x1@1\n";
        let out = upsert_saved_mode(start, "Sony TV", M);
        assert_eq!(
            out,
            "Other 1:1280x720@50 2\nSony TV:1920x1080@60 1\nLast:1x1@1\n"
        );
        // Appended when missing, with a missing final newline repaired.
        let out = upsert_saved_mode("Other:1x1@1", "Sony TV", M);
        assert_eq!(out, "Other:1x1@1\nSony TV:1920x1080@60\n");
        // The first of duplicate lines is the one gamescope reads, and the one changed.
        let out = upsert_saved_mode("A:1x1@1\nA:2x2@2\n", "A", M);
        assert_eq!(out, "A:1920x1080@60\nA:2x2@2\n");
    }
}
