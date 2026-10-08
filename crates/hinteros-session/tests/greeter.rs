//! Greeter mode (session API 0.12) against a fake greetd IPC socket: the picker's list, logging
//! in with and without a PIN, the PIN backoff message, and `hinteros-session greet`'s fast path.

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

mod common;

use common::*;

type Log = Arc<Mutex<Vec<Value>>>;

/// A fake greetd: `hinteros` enters without a PIN, `hinteros-p3` needs 1357 (a wrong one gives
/// the helper's lock message, then an auth error).
fn fake_greetd(path: &Path) -> Log {
    let listener = UnixListener::bind(path).unwrap();
    let log: Log = Arc::default();
    let l = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { return };
            let l = l.clone();
            std::thread::spawn(move || {
                loop {
                    let mut len = [0u8; 4];
                    if s.read_exact(&mut len).is_err() {
                        return;
                    }
                    let mut buf = vec![0u8; u32::from_ne_bytes(len) as usize];
                    s.read_exact(&mut buf).unwrap();
                    let req: Value = serde_json::from_slice(&buf).unwrap();
                    l.lock().unwrap().push(req.clone());
                    let resp = match req["type"].as_str().unwrap() {
                        "create_session" => {
                            if req["username"] == "hinteros-p3" {
                                json!({"type": "auth_message", "auth_message_type": "secret", "auth_message": "Password:"})
                            } else {
                                json!({"type": "success"})
                            }
                        }
                        "post_auth_message_response" => match req["response"].as_str() {
                            Some("1357") => json!({"type": "success"}),
                            Some(_) => {
                                json!({"type": "auth_message", "auth_message_type": "info", "auth_message": "hinteros:locked:30"})
                            }
                            None => {
                                json!({"type": "error", "error_type": "auth_error", "description": "pam"})
                            }
                        },
                        "start_session" => json!({"type": "success"}),
                        "cancel_session" => json!({"type": "success"}),
                        _ => json!({"type": "error", "error_type": "error", "description": "?"}),
                    };
                    let body = serde_json::to_vec(&resp).unwrap();
                    s.write_all(&(body.len() as u32).to_ne_bytes()).unwrap();
                    s.write_all(&body).unwrap();
                }
            });
        }
    });
    log
}

fn public(dir: &Path, next: Value) {
    std::fs::write(
        dir.join("public.json"),
        json!({"version": 1, "profiles": [
            {"id": "p1", "user": "hinteros", "name": "James", "color": "teal", "locked": false, "admin": true, "locked_until": null},
            {"id": "p3", "user": "hinteros-p3", "name": "Sam", "color": "orange", "locked": true, "admin": false, "locked_until": null}],
          "guest": {"enabled": true, "user": "hinteros-guest"}, "first_boot": false, "spare": null,
          "last_used": "p3", "next": next,
          "appearance": {"text_scale": 1.25, "high_contrast": false, "reduce_motion": true}})
        .to_string(),
    )
    .unwrap();
}

fn config(dir: &Path) {
    std::fs::write(
        dir.join("session.toml"),
        format!(
            "[greeter]\npublic = \"{d}/public.json\"\nsocket = \"{d}/greeter.sock\"\nsession_command = [\"/run/current-system/hinteros-console/session\"]\n",
            d = dir.display()
        ),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn picker_logs_in_through_greetd() {
    let gd = tempfile::tempdir().unwrap();
    let sock = gd.path().join("greetd.sock");
    let log = fake_greetd(&sock);
    let d = Daemon::start_env(
        |dir| {
            config(dir);
            public(dir, Value::Null);
            vec![
                "--greeter".into(),
                "--config".into(),
                dir.join("session.toml").into(),
                "--no-runtime".into(),
            ]
        },
        |_| vec![("GREETD_SOCK".into(), sock.clone().into())],
    );
    let mut c = d.client_named("hinteros-launcher").await;
    call(
        &mut c,
        "subscribe",
        json!({"events": ["greeter_prompt", "greeter_result"]}),
    )
    .await
    .unwrap();
    let l = call(&mut c, "greeter.list", json!({})).await.unwrap();
    assert_eq!(l["focus"], "p3", "the last used one");
    assert_eq!(l["profiles"][1]["locked"], true);
    assert_eq!(l["appearance"]["text_scale"], 1.25);
    assert_eq!(l["guest"], true);
    // No apps in the picker, not even the stand-ins.
    let apps = call(&mut c, "apps.list", json!({})).await.unwrap();
    assert_eq!(apps["apps"], json!([]));

    // Unlocked: straight in, with the configured session command.
    call(&mut c, "greeter.login", json!({"id": "p1"}))
        .await
        .unwrap();
    assert_eq!(
        event_named(&mut c, "greeter_result").await,
        json!({"ok": true})
    );
    assert!(
        log.lock()
            .unwrap()
            .iter()
            .any(|r| r["type"] == "start_session"
                && r["cmd"] == json!(["/run/current-system/hinteros-console/session"]))
    );

    // Locked: the PIN pad, a wrong PIN with the lock message, then the right one.
    call(&mut c, "greeter.login", json!({"id": "p3"}))
        .await
        .unwrap();
    assert_eq!(
        event_named(&mut c, "greeter_prompt").await,
        json!({"kind": "pin"})
    );
    call(&mut c, "greeter.pin", json!({"pin": "0000"}))
        .await
        .unwrap();
    assert_eq!(
        event_named(&mut c, "greeter_result").await,
        json!({"ok": false, "locked_seconds": 30})
    );
    call(&mut c, "greeter.login", json!({"id": "p3"}))
        .await
        .unwrap();
    event_named(&mut c, "greeter_prompt").await;
    call(&mut c, "greeter.pin", json!({"pin": "1357"}))
        .await
        .unwrap();
    assert_eq!(
        event_named(&mut c, "greeter_result").await,
        json!({"ok": true})
    );

    // Only the launcher may.
    let mut other = d.client_named("someone").await;
    let e = call(&mut other, "greeter.list", json!({}))
        .await
        .unwrap_err();
    assert_eq!(e["code"], 4004);
    let e = call(&mut c, "greeter.login", json!({"id": "p9"}))
        .await
        .unwrap_err();
    assert_eq!(e["code"], 3010);
}

#[test]
fn greet_takes_the_fast_path_or_runs_the_picker() {
    let gd = tempfile::tempdir().unwrap();
    let sock = gd.path().join("greetd.sock");
    let log = fake_greetd(&sock);
    let dir = gd.path();
    config(dir);
    let greet = |marker: &str| {
        std::process::Command::new(env!("CARGO_BIN_EXE_hinteros-session"))
            .arg("greet")
            .arg("--config")
            .arg(dir.join("session.toml"))
            .arg("--")
            .args(["sh", "-c", &format!("echo picker > {marker}")])
            .env("GREETD_SOCK", &sock)
            .status()
            .unwrap()
    };
    let far = 4_000_000_000i64;
    // A fast hint to unlocked p1: greetd starts it; the picker never runs.
    public(dir, json!({"target": "p1", "fast": true, "expires": far}));
    assert!(greet(&dir.join("m1").display().to_string()).success());
    assert!(!dir.join("m1").exists());
    assert!(
        log.lock()
            .unwrap()
            .iter()
            .any(|r| r["type"] == "create_session" && r["username"] == "hinteros")
    );
    // A locked target without a token: the picker.
    public(dir, json!({"target": "p3", "fast": false, "expires": far}));
    assert!(greet(&dir.join("m2").display().to_string()).success());
    assert_eq!(std::fs::read_to_string(dir.join("m2")).unwrap(), "picker\n");
    // A hint that claims fast but greetd asks for the PIN anyway: cancelled, the picker.
    public(dir, json!({"target": "p3", "fast": true, "expires": far}));
    assert!(greet(&dir.join("m3").display().to_string()).success());
    assert!(dir.join("m3").exists());
    assert!(
        log.lock()
            .unwrap()
            .iter()
            .any(|r| r["type"] == "cancel_session")
    );
}
