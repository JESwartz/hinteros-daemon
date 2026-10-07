//! HinterOS session daemon binary. See the library docs (`lib.rs`) and the README.
//!
//! `hinteros-session reaper SteamLaunch AppId=<id> -- <command>` runs a command so that
//! gamescope tags its windows with `<id>` (see `reaper.rs`).
//!
//! `hinteros-session osk-native-host` (also: this binary started as `hinteros-osk-native-host`)
//! is the Firefox native-messaging host that reports text field focus (see `textfocus.rs`).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use hinteros_api::server as api_server;
use hinteros_api::session;
use hinteros_session::apps::{ProcessSpawner, which};
use hinteros_session::config::{Cli, Config};
use hinteros_session::focus::gamescope::{GamescopeFocus, WatchOptions, spawn_watcher};
use hinteros_session::focus::{FocusControl, NoFocus};
use hinteros_session::icons::{self, Icons};
use hinteros_session::input::{InputControl, InterceptMode, NoInput, devpad, inputplumber};
use hinteros_session::keys::ime::GamescopeIme;
use hinteros_session::keys::uinput::UinputKeyboard;
use hinteros_session::keys::worker::{
    FileSink, ImeConnect, ImeSink, KeySink, ThreadInjector, Timing, Worker,
};
use hinteros_session::keys::{KeyInjector, NoKeys};
use hinteros_session::procs::{Children, Procs};
use hinteros_session::shell::{Msg, Shell, ShellConfig};
use hinteros_session::steam;
use hinteros_session::{Audience, Broadcast, osk, reaper, server, shell, textfocus};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, warn};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(reaper::SUBCOMMAND) {
        let code = reaper::run(&args[2..]);
        return ExitCode::from(u8::try_from(code).unwrap_or(1));
    }
    // Firefox starts the host by path with its own arguments (manifest path, extension id).
    let invoked_as = args
        .first()
        .and_then(|a| Path::new(a).file_name())
        .and_then(|n| n.to_str());
    if invoked_as == Some(textfocus::HOST_BIN) {
        return ExitCode::from(u8::try_from(textfocus::run_host(&args[1..])).unwrap_or(1));
    }
    if args.get(1).map(String::as_str) == Some(textfocus::HOST_SUBCOMMAND) {
        return ExitCode::from(u8::try_from(textfocus::run_host(&args[2..])).unwrap_or(1));
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    let cli = Cli::parse();
    let result = Config::load(&cli).and_then(|config| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(serve(cli, config))
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn serve(cli: Cli, config: Config) -> Result<()> {
    let socket = cli.socket.clone();
    // Before binding: a client may signal us as soon as the socket accepts.
    let mut usr1 = signal(SignalKind::user_defined1())?;
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let path = match socket {
        Some(p) => p,
        None => {
            let dir = std::env::var_os("XDG_RUNTIME_DIR")
                .context("XDG_RUNTIME_DIR is not set; pass --socket")?;
            PathBuf::from(dir).join(session::SOCKET_IN_RUNTIME_DIR)
        }
    };
    let listener = api_server::bind_unix(&path, 0o600, 0o700)
        .with_context(|| format!("binding {}", path.display()))?;
    info!(socket = %path.display(), api_version = %session::VERSION, "listening");

    let (events, _) = broadcast::channel::<Broadcast>(256);
    let (shell_tx, shell_rx) = mpsc::unbounded_channel::<Msg>();
    let exe = std::env::current_exe().context("finding our own executable (for the reaper)")?;
    let env = config.child_env();

    // Text field focus from Firefox's native-messaging host (not part of the session API).
    let osk_socket = textfocus::socket_beside(&path);
    let osk_listener = if config.osk.enable && config.osk.auto_show {
        match api_server::bind_unix(&osk_socket, 0o600, 0o700) {
            Ok(l) => Some(l),
            Err(e) => {
                warn!(socket = %osk_socket.display(), "no automatic on-screen keyboard: {e}");
                None
            }
        }
    } else {
        None
    };
    let mut env = env;
    // For the native-messaging host, which Firefox (an app we start) starts.
    env.push((
        textfocus::SOCKET_ENV.into(),
        osk_socket.to_string_lossy().into_owned(),
    ));

    let (exit_tx, exit_rx) = mpsc::unbounded_channel();
    forward(exit_rx, shell_tx.clone(), Msg::ChildExit);
    let children = Children::default();
    let spawner =
        ProcessSpawner::new(exe.clone(), env.clone(), exit_tx).with_children(children.clone());

    let osk_pid = Arc::new(AtomicU32::new(0));
    let focus: Box<dyn FocusControl> = if config.gamescope.enable {
        let dpy = config.gamescope_display();
        info!(display = %dpy, launcher_app_id = config.launcher.app_id, "gamescope focus control on");
        let (tx, rx) = mpsc::unbounded_channel();
        forward(rx, shell_tx.clone(), Msg::Gamescope);
        let opts = WatchOptions {
            tag_steam_windows: config.steam.tag_windows,
            osk_pid: osk_pid.clone(),
        };
        spawn_watcher(dpy.clone(), Duration::from_secs(1), opts, tx);
        Box::new(GamescopeFocus::new(dpy, config.launcher.app_id))
    } else {
        info!("gamescope focus control off");
        Box::new(NoFocus)
    };

    if config.input.dev_gamepad {
        if config.inputplumber.enable {
            warn!(
                "input.dev_gamepad is ignored while InputPlumber is enabled (it manages the \
                 pads); use --no-inputplumber on a desktop"
            );
        } else {
            let (tx, rx) = mpsc::unbounded_channel();
            forward(rx, shell_tx.clone(), Msg::Input);
            devpad::spawn(tx);
        }
    }

    let input: Box<dyn InputControl> = if config.inputplumber.enable {
        let (tx, rx) = mpsc::unbounded_channel();
        forward(rx, shell_tx.clone(), Msg::Input);
        Box::new(inputplumber::spawn(
            config.inputplumber.bus.clone(),
            InterceptMode::All,
            config.input.xbox_label_swap,
            tx,
        ))
    } else {
        info!("InputPlumber integration off");
        Box::new(NoInput)
    };

    let icon_store = hinteros_session::icons::IconStore::default();
    let catalog = hinteros_api::catalog::Index::load(&config.store.index)
        .map(|i| icons::catalog_specs(&i))
        .unwrap_or_default();
    let icons = {
        let events = events.clone();
        Icons::start(
            &config.apps,
            catalog.clone(),
            &config.icons,
            &icon_store,
            move |changed| {
                for (yes, name) in [
                    (changed.apps, session::EventName::AppsChanged),
                    (changed.store, session::EventName::StoreChanged),
                ] {
                    if yes {
                        let params = serde_json::json!({});
                        let _ = events.send((name, params, Audience::All));
                    }
                }
            },
        )
    };
    let mut shell_config = ShellConfig::from(&config);
    shell_config.icons = icon_store.clone();
    shell_config.icon_refresh = icons.refresher();
    let steam = shell_config.steam_path.clone();
    match &steam {
        Some(p) => info!(steam = %p.display(), "Steam found"),
        None => info!(
            configured = %config.steam.path.display(),
            "Steam not found: no built-in steam app until it is installed"
        ),
    }
    let shell = Shell::new(
        shell_config,
        focus,
        input,
        key_injector(&config),
        Box::new(spawner),
        Box::new(Procs),
        events.clone(),
    );
    tokio::spawn(shell::run(shell, shell_rx));
    if let Some(listener) = osk_listener {
        info!(socket = %osk_socket.display(), "listening for text field focus (Firefox extension)");
        tokio::spawn(textfocus::serve(listener, shell_tx.clone()));
    }

    // New apps without a restart: watch the config (and the catalog, for its icons), and look
    // again whenever a system job ends (a switch writes a new session.toml).
    let reload = hinteros_session::reload::spawn(hinteros_session::reload::Options {
        cli: cli.clone(),
        index: config.store.index.clone(),
        apps: config.apps.clone(),
        steam,
        catalog,
        icons: icons.clone(),
        shell: shell_tx.clone(),
        events: events.clone(),
        poll: Duration::from_secs(30),
        debounce: Duration::from_millis(300),
    });
    {
        let mut rx = events.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok((session::EventName::JobFinished, ..))
                    | Err(broadcast::error::RecvError::Lagged(_)) => {
                        if reload.send(()).is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    let library_events = events.clone();
    steam::watch::spawn(
        config.steam.dir.clone(),
        Duration::from_millis(config.steam.library_debounce_ms),
        Duration::from_secs(config.steam.library_poll_s),
        move |_| {
            let params = serde_json::json!({});
            let _ = library_events.send((session::EventName::GamesChanged, params, Audience::All));
            true
        },
    );

    if let Some(argv) = config.launcher.command.clone() {
        tokio::spawn(supervise_launcher(
            exe.clone(),
            argv,
            config.launcher.app_id,
            env.clone(),
            children.clone(),
        ));
    }
    if config.steam.autostart {
        start_steam(&config.steam.path, &env);
    }
    if config.osk.enable && !config.osk.command.is_empty() {
        // The OSK connects to the same socket.
        let mut osk_env = env.clone();
        osk_env.push((
            "HINTEROS_SESSION_SOCKET".into(),
            path.to_string_lossy().into_owned(),
        ));
        tokio::spawn(osk::supervise(
            config.osk.command.clone(),
            osk_env,
            osk_pid.clone(),
            children.clone(),
        ));
    }

    let relay = if config.system.enable {
        info!(socket = %config.system.socket.display(), "relaying system.* to the system service");
        hinteros_session::system::Relay::spawn(config.system.socket.clone(), events.clone())
    } else {
        info!("system relay off");
        hinteros_session::system::Relay::disabled()
    };

    let runtime =
        hinteros_session::runtime::Runtime::spawn(&config, events.clone(), osk_pid.clone());
    let services = server::Services {
        relay,
        runtime,
        auth: hinteros_session::auth::Auth::spawn(config.auth.clone(), events.clone()),
        store: hinteros_session::store::Store::spawn(
            config.store.index.clone(),
            icon_store.clone(),
            events.clone(),
        ),
        restarter: hinteros_session::restart::Restarter::new(
            config.session.clone(),
            config.runtime.bus.clone(),
        ),
    };

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let rx = events.subscribe();
                    let shell = shell_tx.clone();
                    let services = services.clone();
                    tokio::spawn(async move {
                        if let Err(e) = server::handle(stream, rx, shell, services).await {
                            debug!("connection ended: {e:#}");
                        }
                    });
                }
                Err(e) => warn!("accept failed: {e}"),
            },
            _ = usr1.recv() => {
                info!("SIGUSR1: Home pressed");
                let _ = shell_tx.send(Msg::Home);
            }
            _ = term.recv() => break,
            _ = int.recv() => break,
        }
    }
    info!("shutting down");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&osk_socket);
    // The apps, the launcher and the on-screen keyboard are ours: they must not outlive us.
    children
        .shutdown(Duration::from_millis(config.close.shutdown_grace_ms))
        .await;
    Ok(())
}

/// Pass everything from `rx` on to the shell, wrapped in a [`Msg`].
fn forward<T: Send + 'static>(
    mut rx: mpsc::UnboundedReceiver<T>,
    shell: mpsc::UnboundedSender<Msg>,
    wrap: fn(T) -> Msg,
) {
    tokio::spawn(async move {
        while let Some(item) = rx.recv().await {
            if shell.send(wrap(item)).is_err() {
                break;
            }
        }
    });
}

/// The virtual keyboard and gamescope's input method, as far as they are available.
fn key_injector(config: &Config) -> Box<dyn KeyInjector> {
    let keys: Option<Box<dyn KeySink>> = if let Some(path) = &config.keys.fake {
        match FileSink::new(path.clone()) {
            Ok(sink) => {
                info!(file = %path.display(), "fake keyboard: key events go to a file");
                Some(Box::new(sink))
            }
            Err(e) => {
                warn!(file = %path.display(), "fake keyboard: {e}");
                None
            }
        }
    } else if config.keys.uinput {
        match UinputKeyboard::new() {
            Ok(kbd) => {
                info!("virtual keyboard ready");
                Some(Box::new(kbd))
            }
            Err(e) => {
                warn!(
                    "cannot create the virtual keyboard through /dev/uinput ({e}); the session \
                     user needs the 'uinput' group (and the uinput module). Keyboard input \
                     profiles fall back to the gamepad and keys.key is unavailable"
                );
                None
            }
        }
    } else {
        info!("virtual keyboard off");
        None
    };
    let ime: Option<ImeConnect> = if config.keys.gamescope_ime && config.gamescope.enable {
        let display = config.wayland_display();
        Some(Box::new(move || {
            Ok(Box::new(GamescopeIme::connect(&display)?) as Box<dyn ImeSink>)
        }))
    } else {
        None
    };
    if keys.is_none() && ime.is_none() {
        warn!("no way to type into apps: keys.* and keyboard input profiles are unavailable");
        return Box::new(NoKeys);
    }
    let timing = Timing {
        key_delay: Duration::from_millis(config.keys.key_delay_ms),
        ime_delay: Duration::from_millis(config.keys.ime_delay_ms),
    };
    Box::new(ThreadInjector::spawn(Worker::new(keys, ime, timing)))
}

/// Keep the launcher running under the reaper with its app id, restarting it when it exits.
async fn supervise_launcher(
    exe: PathBuf,
    argv: Vec<String>,
    app_id: u32,
    env: Vec<(String, String)>,
    children: Children,
) {
    while !children.is_closing() {
        let mut cmd = tokio::process::Command::new(&exe);
        cmd.args(reaper::args(app_id, &argv))
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::null());
        match cmd.spawn() {
            Ok(mut child) => {
                let pid = child.id().unwrap_or_default();
                if !children.add(pid) {
                    let _ = child.start_kill();
                    return;
                }
                info!(pid = child.id(), ?argv, "launcher started");
                match child.wait().await {
                    Ok(status) => warn!(%status, "launcher exited; restarting"),
                    Err(e) => warn!("waiting for the launcher: {e}"),
                }
                children.remove(pid);
            }
            Err(e) => error!(?argv, "cannot start the launcher: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// `steam -silent`, so that later steam://rungameid launches go to a running client. Not under
/// the reaper (games must get Steam's own app ids), not tracked as an app.
fn start_steam(path: &Path, env: &[(String, String)]) {
    let program = match which(&path.to_string_lossy()) {
        Ok(p) => p,
        Err(e) => {
            warn!("not starting Steam: {e}");
            return;
        }
    };
    let mut cmd = tokio::process::Command::new(program);
    cmd.arg("-silent")
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(std::process::Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => {
            info!(pid = child.id(), "started steam -silent");
            tokio::spawn(async move {
                if let Ok(status) = child.wait().await {
                    info!(%status, "steam exited");
                }
            });
        }
        Err(e) => warn!("cannot start Steam: {e}"),
    }
}
