//! The session daemon ending its children when it shuts down (SIGTERM, with a grace period
//! before SIGKILL) or dies (SIGKILL: the reapers notice their parent is gone).

use std::path::Path;
use std::time::{Duration, Instant};

use rustix::process::Signal;
use serde_json::json;

mod common;

use common::*;

/// The first child of `pid` named `sleep` (an app's command under its reaper).
fn sleep_under(pid: u32) -> u32 {
    use hinteros_session::procs::{descendants_of, list};
    let table = list();
    descendants_of(&table, pid)
        .into_iter()
        .find(|p| table.iter().any(|t| t.pid == *p && t.comm == "sleep"))
        .expect("the app's command runs under its reaper")
}

async fn launch_sleeper(d: &Daemon, c: &mut hinteros_api::client::Client) -> (u32, u32) {
    let app = call(c, "apps.launch", json!({"id": "sleeper"}))
        .await
        .unwrap();
    let reaper = app["app"]["pid"].as_u64().unwrap() as u32;
    let sleep = eventually("the app's command", async || {
        std::panic::catch_unwind(|| sleep_under(reaper)).ok()
    })
    .await;
    assert!(alive(reaper) && alive(sleep), "{}", d.log());
    (reaper, sleep)
}

fn wait_exit(d: &mut Daemon) {
    let deadline = Instant::now() + TIMEOUT;
    while d.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the daemon did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn sleeper_config(dir: &Path, command: &str, extra: &str) -> Vec<std::ffi::OsString> {
    let config = dir.join("session.toml");
    std::fs::write(
        &config,
        format!("{extra}\n[[apps]]\nid = \"sleeper\"\nname = \"Sleeper\"\ncommand = {command}\n"),
    )
    .unwrap();
    vec!["--config".into(), config.into(), "--no-icon-fetch".into()]
}

#[tokio::test(flavor = "multi_thread")]
async fn sigterm_ends_the_apps_it_started() {
    let mut d = Daemon::start_with(|dir| sleeper_config(dir, r#"["sleep", "60"]"#, ""));
    let mut c = d.client().await;
    let (reaper, sleep) = launch_sleeper(&d, &mut c).await;
    d.signal(Signal::TERM);
    wait_exit(&mut d);
    eventually("the app to be gone", async || {
        (!alive(reaper) && !alive(sleep)).then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sigterm_kills_what_ignores_it_after_the_grace_period() {
    let mut d = Daemon::start_with(|dir| {
        sleeper_config(
            dir,
            r#"["sh", "-c", "trap '' TERM; sleep 60; sleep 60"]"#,
            "[close]\nshutdown_grace_ms = 300",
        )
    });
    let mut c = d.client().await;
    let (reaper, sleep) = launch_sleeper(&d, &mut c).await;
    let started = Instant::now();
    d.signal(Signal::TERM);
    wait_exit(&mut d);
    assert!(started.elapsed() >= Duration::from_millis(300));
    eventually("the app to be gone", async || {
        (!alive(reaper) && !alive(sleep)).then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_killed_daemon_does_not_leave_its_apps_behind() {
    let mut d = Daemon::start_with(|dir| sleeper_config(dir, r#"["sleep", "60"]"#, ""));
    let mut c = d.client().await;
    let (reaper, sleep) = launch_sleeper(&d, &mut c).await;
    // SIGKILL cannot be handled: the reaper notices its parent died.
    d.child.kill().unwrap();
    d.child.wait().unwrap();
    eventually("the app to be gone", async || {
        (!alive(reaper) && !alive(sleep)).then_some(())
    })
    .await;
}
