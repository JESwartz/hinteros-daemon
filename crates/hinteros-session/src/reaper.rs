//! `hinteros-session reaper SteamLaunch AppId=<id> -- <command> [args]`: run a command so that
//! gamescope knows its windows by `<id>`.
//!
//! gamescope (Steam mode) gives a new window the app id of the outermost ancestor process that
//! is named `reaper` and has `SteamLaunch` and `AppId=<id>` before `--` in its command line,
//! which is how Steam's own reaper tags games. This works for X11 and native Wayland clients
//! alike, unlike the `STEAM_GAME` window property (X11 only). We become such a process:
//! rename the thread to `reaper`, become a child subreaper (so the app's orphaned descendants
//! are re-parented to us), run the command in its own process group, forward termination
//! signals to that group, and exit with the command's status once every descendant is gone.
//!
//! The process must not be started below another reaper with an app id: the outermost one wins.

use std::ffi::CString;
use std::os::unix::process::CommandExt;
use std::process::Command;

use rustix::io::Errno;
use rustix::process::{
    Pid, Signal, WaitOptions, getpid, kill_process_group, set_child_subreaper, wait,
};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

/// The subcommand name (also the process name gamescope looks for).
pub const SUBCOMMAND: &str = "reaper";

/// The arguments for running `argv` under the reaper with gamescope app id `app_id`, after the
/// executable path.
pub fn args(app_id: u32, argv: &[String]) -> Vec<String> {
    let mut args = vec![
        SUBCOMMAND.to_string(),
        "SteamLaunch".into(),
        format!("AppId={app_id}"),
        "--".into(),
    ];
    args.extend(argv.iter().cloned());
    args
}

/// Parse what follows `reaper`: the app id (from `AppId=<n>`, needs `SteamLaunch`) and the
/// command after `--`.
pub fn parse(args: &[String]) -> Result<(Option<u32>, Vec<String>), String> {
    let split = args
        .iter()
        .position(|a| a == "--")
        .ok_or("usage: reaper [SteamLaunch AppId=<id>] -- <command> [args...]")?;
    let (before, command) = (&args[..split], &args[split + 1..]);
    if command.is_empty() {
        return Err("reaper: no command after --".into());
    }
    let steam_launch = before.iter().any(|a| a == "SteamLaunch");
    let mut app_id = None;
    for a in before {
        if let Some(id) = a.strip_prefix("AppId=") {
            app_id = Some(
                id.parse::<u32>()
                    .ok()
                    .filter(|&id| id != 0)
                    .ok_or_else(|| format!("reaper: bad {a}"))?,
            );
        } else if a != "SteamLaunch" {
            return Err(format!("reaper: unexpected argument {a}"));
        }
    }
    if app_id.is_some() && !steam_launch {
        return Err(
            "reaper: AppId= needs SteamLaunch before it (gamescope ignores it otherwise)".into(),
        );
    }
    Ok((app_id, command.to_vec()))
}

/// Exit status for a child that ended with `status` (shell convention: 128 + signal).
fn status_code(status: rustix::process::WaitStatus) -> i32 {
    status
        .exit_status()
        .or_else(|| status.terminating_signal().map(|s| 128 + s))
        .unwrap_or(1)
}

/// Run the reaper with the arguments after `reaper`. Returns the exit code.
pub fn run(args: &[String]) -> i32 {
    let command = match parse(args) {
        Ok((_, command)) => command,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    // Best effort: without these the command still runs, gamescope just can't tag it.
    if let Err(e) = rustix::thread::set_name(&CString::new(SUBCOMMAND).expect("no NUL")) {
        eprintln!("reaper: cannot rename process: {e}");
    }
    if let Err(e) = set_child_subreaper(Some(getpid())) {
        eprintln!("reaper: cannot become a subreaper: {e}");
    }

    let child = match Command::new(&command[0])
        .args(&command[1..])
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("reaper: cannot run {}: {e}", command[0]);
            return if e.kind() == std::io::ErrorKind::NotFound {
                127
            } else {
                126
            };
        }
    };
    let child_pid = Pid::from_child(&child);

    match Signals::new([SIGTERM, SIGINT, SIGHUP]) {
        Ok(mut signals) => {
            std::thread::spawn(move || {
                for sig in signals.forever() {
                    let sig = match sig {
                        SIGINT => Signal::INT,
                        SIGHUP => Signal::HUP,
                        _ => Signal::TERM,
                    };
                    let _ = kill_process_group(child_pid, sig);
                }
            });
        }
        Err(e) => eprintln!("reaper: cannot forward signals: {e}"),
    }

    // Reap everything: the command and whatever it leaves behind.
    let mut code = 1;
    loop {
        match wait(WaitOptions::empty()) {
            Ok(Some((pid, status))) if pid == child_pid => code = status_code(status),
            Ok(_) => {}
            Err(Errno::INTR) => {}
            Err(Errno::CHILD) => break,
            Err(e) => {
                eprintln!("reaper: wait failed: {e}");
                break;
            }
        }
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_steam_style_args() {
        assert_eq!(
            parse(&strings(&["SteamLaunch", "AppId=570", "--", "game", "-x"])),
            Ok((Some(570), strings(&["game", "-x"])))
        );
        assert_eq!(parse(&strings(&["--", "a"])), Ok((None, strings(&["a"]))));
        for bad in [
            &["SteamLaunch", "AppId=570", "game"][..],
            &["SteamLaunch", "AppId=570", "--"],
            &["AppId=570", "--", "a"],
            &["SteamLaunch", "AppId=0", "--", "a"],
            &["SteamLaunch", "AppId=x", "--", "a"],
            &["Other", "--", "a"],
        ] {
            assert!(parse(&strings(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn args_round_trip() {
        let a = args(7, &strings(&["firefox", "--kiosk"]));
        assert_eq!(a[0], SUBCOMMAND);
        assert_eq!(
            parse(&a[1..]),
            Ok((Some(7), strings(&["firefox", "--kiosk"])))
        );
    }
}
