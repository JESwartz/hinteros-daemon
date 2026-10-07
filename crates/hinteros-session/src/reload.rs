//! Picking up a changed app list without a restart.
//!
//! The apps come from the generated `session.toml`; installing or removing a Store app switches
//! the system and writes a new one. NixOS switches by replacing symlinks (the config file, a
//! directory like `/etc/static`), so inotify watches every directory along the path's symlink
//! chain, not just the file. Three things make the daemon look again: an inotify event
//! (debounced), the end of any system job (`job_finished`: a switch is done), and a slow
//! periodic check as a safety net. The look is a re-read of the config (the same loading as
//! at startup) and of the catalog index, comparing with what is loaded.
//!
//! What a reload changes: the app list (`apps.list`, what `apps.launch` can start), whether
//! the built-in `steam` app is there (only while `steam.path` names an executable, checked on
//! every look, so Steam installed later shows up within the poll interval at worst) and the
//! icons of apps and catalog entries. Everything else in the file (gamescope, input, paths)
//! needs a restart. Running apps are never touched: a removed app that is running keeps
//! running (and stays in `apps.running`) until it exits. A broken new file is logged and the
//! old list stays.

use std::collections::BTreeSet;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hinteros_api::catalog::Index;
use hinteros_api::session::EventName;
use serde_json::json;
use tokio::io::unix::AsyncFd;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::config::{AppConfig, Cli, Config};
use crate::icons::{IconSpec, Icons, catalog_specs};
use crate::shell::Msg;
use crate::steam::watch::{drain, watch};
use crate::{Audience, Broadcast};

pub struct Options {
    pub cli: Cli,
    /// The catalog index to watch for icons.
    pub index: PathBuf,
    /// What is loaded now.
    pub apps: Vec<AppConfig>,
    /// The Steam binary the shell has now (`None`: Steam is not installed).
    pub steam: Option<PathBuf>,
    pub catalog: Vec<IconSpec>,
    pub icons: Icons,
    pub shell: mpsc::UnboundedSender<Msg>,
    pub events: broadcast::Sender<Broadcast>,
    /// Safety-net interval.
    pub poll: Duration,
    pub debounce: Duration,
}

/// Ask for a reload (something that may have changed the config just finished).
pub type Trigger = mpsc::UnboundedSender<()>;

/// Every directory whose change can change what `path` resolves to: its parent, and for each
/// symlink on the way (the file, or a directory component like `/etc/static`) the symlink's
/// parent and, recursively, where it points.
pub fn watch_dirs(path: &Path) -> BTreeSet<PathBuf> {
    let mut dirs = BTreeSet::new();
    chain(path, &mut dirs, 0);
    dirs.retain(|d| d.is_dir());
    dirs
}

fn chain(path: &Path, dirs: &mut BTreeSet<PathBuf>, depth: usize) {
    if depth > 8 {
        return;
    }
    if let Some(parent) = path.parent() {
        dirs.insert(parent.to_path_buf());
    }
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component);
        let Ok(target) = std::fs::read_link(&prefix) else {
            continue;
        };
        let base = prefix.parent().unwrap_or(Path::new("/")).to_path_buf();
        let resolved = base.join(&target);
        // The rest of the path below the symlink, resolved against the target.
        let rest = path.strip_prefix(&prefix).unwrap_or(Path::new(""));
        chain(&resolved.join(rest), dirs, depth + 1);
        dirs.insert(base);
    }
}

struct State {
    apps: Vec<AppConfig>,
    steam: Option<PathBuf>,
    catalog: Vec<IconSpec>,
    last_error: Option<String>,
}

/// Start the reloader. Drop the returned trigger's last clone to stop it.
pub fn spawn(opts: Options) -> Trigger {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(run(opts, rx));
    tx
}

async fn ready(fd: &Option<AsyncFd<OwnedFd>>) {
    if let Some(fd) = fd
        && let Ok(mut guard) = fd.readable().await
    {
        drain(fd.get_ref());
        guard.clear_ready();
        return;
    }
    std::future::pending::<()>().await;
}

async fn run(opts: Options, mut rx: mpsc::UnboundedReceiver<()>) {
    let mut state = State {
        apps: opts.apps.clone(),
        steam: opts.steam.clone(),
        catalog: opts.catalog.clone(),
        last_error: None,
    };
    let opts = std::sync::Arc::new(opts);
    let mut dirs = BTreeSet::new();
    let mut fd: Option<AsyncFd<OwnedFd>> = None;
    let mut tick = tokio::time::interval(opts.poll.max(Duration::from_millis(50)));
    tick.tick().await;
    loop {
        let wanted: BTreeSet<PathBuf> = Config::config_path(&opts.cli)
            .iter()
            .chain(std::iter::once(&opts.index))
            .flat_map(|p| watch_dirs(p))
            .collect();
        if wanted != dirs {
            debug!(dirs = ?wanted, "watching for config changes");
            fd = watch(&wanted).and_then(|f| AsyncFd::new(f).ok());
            dirs = wanted;
        }
        let mut triggered = false;
        tokio::select! {
            () = ready(&fd) => {}
            t = rx.recv() => match t {
                Some(()) => triggered = true,
                None => return,
            },
            _ = tick.tick() => {}
        }
        // Let a switch finish its burst of changes.
        tokio::time::sleep(opts.debounce).await;
        while rx.try_recv().is_ok() {
            triggered = true;
        }
        if let Some(fd) = &fd {
            drain(fd.get_ref());
        }
        let o = opts.clone();
        match tokio::task::spawn_blocking(move || {
            reload(&o, &mut state, triggered);
            state
        })
        .await
        {
            Ok(s) => state = s,
            Err(e) => {
                warn!("reloading the app list: {e}");
                return;
            }
        }
    }
}

/// One look at the config and the catalog; announces what changed.
fn reload(o: &Options, state: &mut State, rescan: bool) {
    let (mut apps, mut store) = (false, false);
    match Config::load(&o.cli) {
        Ok(config) => {
            state.last_error = None;
            if config.apps != state.apps {
                let before: BTreeSet<&str> = state.apps.iter().map(|a| a.id.as_str()).collect();
                let after: BTreeSet<&str> = config.apps.iter().map(|a| a.id.as_str()).collect();
                info!(
                    added = ?after.difference(&before).collect::<Vec<_>>(),
                    removed = ?before.difference(&after).collect::<Vec<_>>(),
                    "app list reloaded"
                );
                let changed = o.icons.set_apps(&config.apps);
                store |= changed.store;
                state.apps.clone_from(&config.apps);
                // Before the event, so that apps.list (queued behind it) sees the new list.
                let _ = o.shell.send(Msg::SetApps(config.apps));
                apps = true;
            }
            // Steam installed (a Store install, a new `steam.path`) or removed: the built-in
            // `steam` app comes and goes with it.
            let steam = crate::apps::steam_binary(&config.steam.path);
            if steam != state.steam {
                info!(
                    steam = ?steam,
                    configured = %config.steam.path.display(),
                    "Steam {}",
                    if steam.is_some() { "found" } else { "not found" }
                );
                state.steam.clone_from(&steam);
                let _ = o.shell.send(Msg::SetSteam(steam));
                apps = true;
            }
        }
        Err(e) => {
            let text = format!("{e:#}");
            if state.last_error.as_ref() != Some(&text) {
                warn!("not reloading the app list: {text}");
            }
            state.last_error = Some(text);
        }
    }
    if let Ok(index) = Index::load(&o.index) {
        let specs = catalog_specs(&index);
        if specs != state.catalog {
            let changed = o.icons.set_catalog(specs.clone());
            apps |= changed.apps;
            store = true;
            state.catalog = specs;
        }
    }
    if rescan && !apps && !store {
        // Installing an app adds its desktop file, removing it takes it away.
        let changed = o.icons.rescan();
        apps |= changed.apps;
        store |= changed.store;
    }
    for (changed, name) in [
        (apps, EventName::AppsChanged),
        (store, EventName::StoreChanged),
    ] {
        if changed {
            let _ = o.events.send((name, json!({}), Audience::All));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn watches_every_directory_along_the_symlink_chain() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        // Like NixOS: etc/xdg/x.toml -> etc/static/xdg/x.toml, etc/static -> store/gen-1/etc.
        fs::create_dir_all(r.join("store/gen-1/etc/xdg")).unwrap();
        fs::write(r.join("store/gen-1/etc/xdg/x.toml"), "").unwrap();
        fs::create_dir_all(r.join("etc/xdg")).unwrap();
        symlink(r.join("store/gen-1/etc"), r.join("etc/static")).unwrap();
        symlink(r.join("etc/static/xdg/x.toml"), r.join("etc/xdg/x.toml")).unwrap();
        let dirs = watch_dirs(&r.join("etc/xdg/x.toml"));
        assert!(dirs.contains(&r.join("etc/xdg")));
        // Replacing the etc/static symlink shows up in etc.
        assert!(dirs.contains(&r.join("etc")));
        assert!(dirs.contains(&r.join("store/gen-1/etc/xdg")));
        // A path that does not exist yet still watches its parent.
        assert!(watch_dirs(&r.join("etc/xdg/new.toml")).contains(&r.join("etc/xdg")));
        assert!(watch_dirs(&r.join("nowhere/x.toml")).is_empty());
    }
}
