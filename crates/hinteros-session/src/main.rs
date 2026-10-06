//! HinterOS session daemon binary. See the library docs (`lib.rs`) and the README.
//!
//! `hinteros-session reaper SteamLaunch AppId=<id> -- <command>` runs a command so that
//! gamescope tags its windows with `<id>` (see `reaper.rs`).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use hinteros_api::server as api_server;
use hinteros_api::session;
use hinteros_session::apps::{ProcessSpawner, which};
use hinteros_session::config::{Cli, Config};
use hinteros_session::focus::gamescope::{GamescopeFocus, WatchOptions, spawn_watcher};
use hinteros_session::focus::{FocusControl, NoFocus};
use hinteros_session::input::{InputControl, InterceptMode, NoInput, inputplumber};
use hinteros_session::procs::Procs;
use hinteros_session::shell::{Msg, Shell, ShellConfig};
use hinteros_session::steam;
use hinteros_session::{Broadcast, reaper, server, shell};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, warn};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(reaper::SUBCOMMAND) {
        let code = reaper::run(&args[2..]);
        return ExitCode::from(u8::try_from(code).unwrap_or(1));
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
            .block_on(serve(cli.socket, config))
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn serve(socket: Option<PathBuf>, config: Config) -> Result<()> {
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

    let (exit_tx, exit_rx) = mpsc::unbounded_channel();
    forward(exit_rx, shell_tx.clone(), Msg::ChildExit);
    let spawner = ProcessSpawner::new(exe.clone(), env.clone(), exit_tx);

    let focus: Box<dyn FocusControl> = if config.gamescope.enable {
        let dpy = config.gamescope_display();
        info!(display = %dpy, launcher_app_id = config.launcher.app_id, "gamescope focus control on");
        let (tx, rx) = mpsc::unbounded_channel();
        forward(rx, shell_tx.clone(), Msg::Gamescope);
        let opts = WatchOptions {
            tag_steam_windows: config.steam.tag_windows,
        };
        spawn_watcher(dpy.clone(), Duration::from_secs(1), opts, tx);
        Box::new(GamescopeFocus::new(dpy, config.launcher.app_id))
    } else {
        info!("gamescope focus control off");
        Box::new(NoFocus)
    };

    let input: Box<dyn InputControl> = if config.inputplumber.enable {
        let (tx, rx) = mpsc::unbounded_channel();
        forward(rx, shell_tx.clone(), Msg::Input);
        Box::new(inputplumber::spawn(
            config.inputplumber.bus.clone(),
            InterceptMode::All,
            tx,
        ))
    } else {
        info!("InputPlumber integration off");
        Box::new(NoInput)
    };

    let shell = Shell::new(
        ShellConfig::from(&config),
        focus,
        input,
        Box::new(spawner),
        Box::new(Procs),
        events.clone(),
    );
    tokio::spawn(shell::run(shell, shell_rx));

    let library_events = events.clone();
    steam::watch::spawn(
        config.steam.dir.clone(),
        Duration::from_millis(config.steam.library_debounce_ms),
        Duration::from_secs(config.steam.library_poll_s),
        move |_| {
            let params = serde_json::json!({});
            let _ = library_events.send((session::EventName::GamesChanged, params));
            true
        },
    );

    if let Some(argv) = config.launcher.command.clone() {
        tokio::spawn(supervise_launcher(
            exe.clone(),
            argv,
            config.launcher.app_id,
            env.clone(),
        ));
    }
    if config.steam.autostart {
        start_steam(&config.steam.path, &env);
    }

    let mut usr1 = signal(SignalKind::user_defined1())?;
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let rx = events.subscribe();
                    let shell = shell_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = server::handle(stream, rx, shell).await {
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

/// Keep the launcher running under the reaper with its app id, restarting it when it exits.
async fn supervise_launcher(
    exe: PathBuf,
    argv: Vec<String>,
    app_id: u32,
    env: Vec<(String, String)>,
) {
    loop {
        let mut cmd = tokio::process::Command::new(&exe);
        cmd.args(reaper::args(app_id, &argv))
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::null());
        match cmd.spawn() {
            Ok(mut child) => {
                info!(pid = child.id(), ?argv, "launcher started");
                match child.wait().await {
                    Ok(status) => warn!(%status, "launcher exited; restarting"),
                    Err(e) => warn!("waiting for the launcher: {e}"),
                }
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
