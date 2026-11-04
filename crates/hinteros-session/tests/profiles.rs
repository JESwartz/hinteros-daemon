//! The session's side of profiles: `profile.current`, the profile switch
//! against a fake system service (no silent force-close: a TERM-ignoring app produces
//! `switch_blocked` and lives until `switch_force_close`; `switch_cancel` keeps the session),
//! `session.unlock`, `profiles_changed` from the system's event, and the sleep lock (only
//! logind's own PrepareForSleep counts).

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hinteros_api::codec::{LineReader, write_message};
use hinteros_api::common::Peer;
use hinteros_api::server::{Connection, Step};
use hinteros_api::{ErrorObject, jsonrpc, system};
use serde_json::{Value, json};
use tokio::net::UnixListener;

mod common;

use common::*;

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

const LIST: &str = r#"{"profiles": [
  {"id": "p1", "name": "James", "color": "teal", "user": "hinteros-p1", "admin": true, "locked": true,
   "locked_until": null, "active": true, "last_used_at": null, "usage": null, "pin_set": true},
  {"id": "p3", "name": "Sam", "color": "orange", "user": "hinteros-p3", "admin": false, "locked": false,
   "locked_until": null, "active": false, "last_used_at": null, "usage": null, "pin_set": true}],
  "guest": {"enabled": true, "usage": null}, "max": 8, "first_boot": false,
  "caller": {"id": "p1", "user": "hinteros-p1", "guest": false, "spare": false}}"#;

/// A fake system service answering the profile methods; records calls; after `profiles.rename`
/// it sends `profiles_changed`.
fn fake_system(path: &Path, calls: Calls) -> tokio::task::JoinHandle<()> {
    fake_system_with(path, calls, LIST)
}

/// The fake with another `profiles.list` answer (the caller as the guest).
fn fake_system_with(path: &Path, calls: Calls, list: &'static str) -> tokio::task::JoinHandle<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        let mut conns = tokio::task::JoinSet::new();
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let calls = calls.clone();
            conns.spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut reader = LineReader::new(r);
                let mut conn = Connection::new(system::VERSION, Peer::new("fake-system", "0"));
                while let Ok(Some(line)) = reader.next_line().await {
                    let reply = match conn.step(&line) {
                        Step::Reply(v) | Step::ReplyAndClose(v) => v,
                        Step::Ignore => continue,
                        Step::Dispatch(req) => {
                            let params = req.params.clone().unwrap_or(json!({}));
                            calls
                                .lock()
                                .unwrap()
                                .push((req.method.clone(), params.clone()));
                            let outcome: Result<Value, ErrorObject> = match req.method.as_str() {
                                "subscribe" => Ok(params.clone()),
                                "profiles.list" => Ok(serde_json::from_str(list).unwrap()),
                                "guest.end" => Ok(json!({"job_id": "3f9a0c2b7e4d1a65"})),
                                "profiles.switch" if params["pin"] == "0000" => {
                                    Err(ErrorObject::with_data(
                                        hinteros_api::ErrorCode::PinRequired,
                                        "that PIN didn't match",
                                        json!({"pin": "target"}),
                                    ))
                                }
                                "profiles.switch" | "profiles.switch_cancel" => Ok(json!({})),
                                "profiles.unlock" if params["pin"] == "2468" => Ok(json!({})),
                                "profiles.unlock" => Err(ErrorObject::with_data(
                                    hinteros_api::ErrorCode::PinRequired,
                                    "that PIN didn't match",
                                    json!({"pin": "self"}),
                                )),
                                "profiles.rename" => Ok(json!({})),
                                m => Err(hinteros_api::server::method_not_found(m)),
                            };
                            let renamed = req.method == "profiles.rename";
                            let reply = jsonrpc::response(&req.id.unwrap(), outcome);
                            write_message(&mut w, &reply).await.unwrap();
                            if renamed {
                                let ev = jsonrpc::notification("profiles_changed", json!({}));
                                write_message(&mut w, &ev).await.unwrap();
                            }
                            continue;
                        }
                    };
                    write_message(&mut w, &reply).await.unwrap();
                }
            });
        }
    })
}

fn daemon() -> Daemon {
    Daemon::start_with(|dir| {
        std::fs::write(
            dir.join("session.toml"),
            "[switch]\nwait_s = 1\nsteam_wait_s = 1\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("proc")).unwrap();
        vec![
            "--config".into(),
            dir.join("session.toml").into(),
            "--test-no-session-end".into(),
            "--test-proc-dir".into(),
            dir.join("proc").into(),
            "--no-runtime".into(),
        ]
    })
}

async fn launch(c: &mut Client, cmd: &str) -> (String, u32) {
    let r = call(
        c,
        "apps.launch",
        json!({"command": ["sh", "-c", cmd], "name": "Stubborn"}),
    )
    .await
    .unwrap();
    (
        r["app"]["id"].as_str().unwrap().to_owned(),
        r["app"]["pid"].as_u64().unwrap() as u32,
    )
}

use hinteros_api::client::Client;

#[tokio::test(flavor = "multi_thread")]
async fn switch_closes_apps_and_never_kills_silently() {
    let d = daemon();
    let calls: Calls = Arc::default();
    let task = fake_system(&d.dir.path().join("system.sock"), calls.clone());
    let mut c = d.client().await;
    call(
        &mut c,
        "subscribe",
        json!({"events": ["switch_progress", "switch_blocked", "app_exited", "profiles_changed"]}),
    )
    .await
    .unwrap();
    let cur = eventually("the relay", async || {
        call(&mut c, "profile.current", json!({})).await.ok()
    })
    .await;
    assert_eq!(
        cur,
        json!({"id": "p1", "name": "James", "color": "teal", "admin": true, "locked": true,
               "guest": false, "setup_required": false, "pin_set": true, "screen_locked": false})
    );
    let e = call(&mut c, "session.switch_profile", json!({"id": "p1"}))
        .await
        .unwrap_err();
    assert_eq!(e["data"]["reason"], "same");

    // A locked target: a wrong PIN comes back as the system's error and nothing starts.
    let e = call(
        &mut c,
        "session.switch_profile",
        json!({"id": "p3", "pin": "0000"}),
    )
    .await
    .unwrap_err();
    assert_eq!(
        (e["code"].clone(), e["data"].clone()),
        (json!(3016), json!({"pin": "target"}))
    );
    let e = call(&mut c, "session.switch_wait", json!({}))
        .await
        .unwrap_err();
    assert_eq!(
        e["data"]["reason"], "no_switch",
        "no switch runs after a wrong PIN"
    );

    // One app quits on SIGTERM, one ignores it.
    let (_polite, _) = launch(&mut c, "exec sleep 60").await;
    let (stubborn, pid) = launch(&mut c, "trap '' TERM; while true; do sleep 1; done").await;
    call(&mut c, "session.switch_profile", json!({"id": "p3"}))
        .await
        .unwrap();
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|(m, p)| m == "profiles.switch" && p["id"] == "p3" && p["pin"].is_null())
    );
    assert_eq!(
        event_named(&mut c, "switch_progress").await["stage"],
        "closing_apps"
    );
    let blocked = event_named(&mut c, "switch_blocked").await;
    assert_eq!(blocked["app"]["id"], json!(stubborn));
    assert!(blocked["waited_s"].as_u64().unwrap() >= 1);
    // Nothing is killed while the user decides.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(alive(pid), "killed without asking");
    call(&mut c, "session.switch_wait", json!({}))
        .await
        .unwrap();
    assert_eq!(
        event_named(&mut c, "switch_blocked").await["app"]["id"],
        json!(stubborn)
    );
    call(
        &mut c,
        "session.switch_force_close",
        json!({"app": stubborn}),
    )
    .await
    .unwrap();
    let p = event_named(&mut c, "switch_progress").await;
    assert_eq!(p["stage"], "ending", "{p}");
    eventually("the stubborn app gone", async || {
        (!alive(pid)).then_some(())
    })
    .await;

    // A second switch, cancelled: the app stays, the session stays.
    let shell_pid = d.dir.path().join("stubborn.pid");
    let (stubborn, pid) = launch(
        &mut c,
        &format!(
            "echo $$ > {}; trap '' TERM; while true; do sleep 1; done",
            shell_pid.display()
        ),
    )
    .await;
    call(&mut c, "session.switch_profile", json!({"id": "guest"}))
        .await
        .unwrap();
    assert_eq!(
        event_named(&mut c, "switch_blocked").await["app"]["id"],
        json!(stubborn)
    );
    call(&mut c, "session.switch_cancel", json!({}))
        .await
        .unwrap();
    assert_eq!(
        event_named(&mut c, "switch_progress").await["stage"],
        "cancelled"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(alive(pid));
    // The hint (and a token) the switch wrote is dropped.
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|(m, _)| m == "profiles.switch_cancel")
    );
    let e = call(&mut c, "session.switch_wait", json!({}))
        .await
        .unwrap_err();
    assert_eq!(e["data"]["reason"], "no_switch");

    // The sleep lock's PIN goes through the system service.
    let e = call(&mut c, "session.unlock", json!({"pin": "0000"}))
        .await
        .unwrap_err();
    assert_eq!(e["code"], 3016);
    call(&mut c, "session.unlock", json!({"pin": "2468"}))
        .await
        .unwrap();
    assert_eq!(
        call(&mut c, "session.lock_state", json!({})).await.unwrap(),
        json!({"locked": false})
    );

    // profiles_changed: the system's empty event becomes the list.
    call(
        &mut c,
        "system.profiles.rename",
        json!({"id": "p1", "name": "Jim"}),
    )
    .await
    .unwrap();
    let ev = event_named(&mut c, "profiles_changed").await;
    assert_eq!(ev["profiles"][1]["id"], "p3");
    // It ignores SIGTERM: kill it so nothing outlives the test.
    let _ = stubborn;
    if let Some(p) = std::fs::read_to_string(&shell_pid)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process(p, rustix::process::Signal::KILL);
    }
    task.abort();
}

const GUEST_LIST: &str = r#"{"profiles": [
  {"id": "p1", "name": "James", "color": "teal", "user": "hinteros-p1", "admin": true, "locked": true,
   "locked_until": null, "active": false, "last_used_at": null, "usage": null, "pin_set": true}],
  "guest": {"enabled": true, "usage": null}, "max": 8, "first_boot": false,
  "caller": {"id": null, "user": "hinteros-guest", "guest": true, "spare": false}}"#;

/// End guest session: the apps close as on a switch, then the daemon asks the system service
/// to end the session and wipe the guest now (system.guest.end).
#[tokio::test(flavor = "multi_thread")]
async fn end_guest_session_asks_for_the_wipe() {
    let d = daemon();
    let calls: Calls = Arc::default();
    let task = fake_system_with(&d.dir.path().join("system.sock"), calls.clone(), GUEST_LIST);
    let mut c = d.client().await;
    call(&mut c, "subscribe", json!({"events": ["switch_progress"]}))
        .await
        .unwrap();
    let cur = eventually("the relay", async || {
        call(&mut c, "profile.current", json!({})).await.ok()
    })
    .await;
    assert_eq!(cur["guest"], true);
    let (_, pid) = launch(&mut c, "sleep 60").await;
    call(&mut c, "session.end_guest", json!({})).await.unwrap();
    assert_eq!(
        event_named(&mut c, "switch_progress").await["stage"],
        "closing_apps"
    );
    assert_eq!(
        event_named(&mut c, "switch_progress").await["stage"],
        "ending"
    );
    eventually("guest.end called", async || {
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|(m, p)| m == "guest.end" && *p == json!({}))
            .then_some(())
    })
    .await;
    assert!(!alive(pid), "the apps were closed first");
    // Never a switch hint: the session goes to the picker.
    assert!(
        !calls
            .lock()
            .unwrap()
            .iter()
            .any(|(m, _)| m == "profiles.switch")
    );
    task.abort();
}

/// logind's Inhibit: hands out a delay lock (here /dev/null) and counts the calls.
struct Login1(Arc<Mutex<u32>>);

#[zbus::interface(name = "org.freedesktop.login1.Manager")]
impl Login1 {
    fn inhibit(&self, _what: &str, _who: &str, _why: &str, _mode: &str) -> zbus::zvariant::OwnedFd {
        *self.0.lock().unwrap() += 1;
        let f = std::fs::File::open("/dev/null").unwrap();
        std::os::fd::OwnedFd::from(f).into()
    }
}

/// PrepareForSleep(true) locks a locked profile only when logind sends it: the same signal from
/// another bus client, or from logind on another path, changes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn only_logind_puts_the_profile_to_sleep() {
    let Some(bus) = Bus::start() else { return };
    let inhibits = Arc::new(Mutex::new(0));
    let logind = zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .serve_at("/org/freedesktop/login1", Login1(inhibits.clone()))
        .unwrap()
        .name("org.freedesktop.login1")
        .unwrap()
        .build()
        .await
        .unwrap();
    let d = Daemon::start_with(|dir| {
        std::fs::create_dir_all(dir.join("proc")).unwrap();
        let mut args: Vec<std::ffi::OsString> = vec![
            "--test-no-session-end".into(),
            "--test-proc-dir".into(),
            dir.join("proc").into(),
        ];
        args.extend(runtime_bus(&bus));
        args
    });
    let task = fake_system(&d.dir.path().join("system.sock"), Calls::default());
    let mut c = d.client().await;
    eventually("the relay", async || {
        call(&mut c, "profile.current", json!({})).await.ok()
    })
    .await;
    // The watch is up once it holds its delay lock.
    eventually("the inhibitor", async || {
        (*inhibits.lock().unwrap() > 0).then_some(())
    })
    .await;

    let sleep = async |conn: &zbus::Connection, path: &str| {
        conn.emit_signal(
            None::<zbus::names::BusName<'_>>,
            path,
            "org.freedesktop.login1.Manager",
            "PrepareForSleep",
            &(true,),
        )
        .await
        .unwrap();
    };
    let stranger = zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    sleep(&stranger, "/org/freedesktop/login1").await;
    sleep(&logind, "/org/freedesktop/login1/other").await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(
        call(&mut c, "session.lock_state", json!({})).await.unwrap(),
        json!({"locked": false})
    );

    sleep(&logind, "/org/freedesktop/login1").await;
    eventually("the sleep lock", async || {
        let s = call(&mut c, "session.lock_state", json!({})).await.ok()?;
        (s == json!({"locked": true})).then_some(())
    })
    .await;
    task.abort();
}
