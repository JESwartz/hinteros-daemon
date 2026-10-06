//! Black-box tests: run the real `hinteros-session` binary on a temporary socket, talk to it over
//! the wire and validate every message it sends against `schema/session-api.json`.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use hinteros_api::common::{Peer, Ping, PingParams};
use hinteros_api::conformance::ApiSchema;
use hinteros_api::session::{self, AppsList, AppsListParams, EventName, HomePressed};
use hinteros_api::{ApiVersion, ErrorCode};
use rustix::process::{Pid, Signal, kill_process};
use serde_json::{Value, json};
use tokio::time::timeout;

mod common;

use common::*;

#[tokio::test]
async fn handshake_ping_apps_and_home_event() {
    let d = Daemon::start();
    let schema = ApiSchema::session();
    let mut c = d.client().await;

    // ping, raw and typed.
    let r = c.call_raw("ping", json!({ "nonce": "abc" })).await.unwrap();
    schema.check_result("ping", &r).unwrap();
    assert_eq!(r, json!({ "nonce": "abc" }));
    let typed = c.call::<Ping>(&PingParams::default()).await.unwrap();
    assert_eq!(typed.nonce, None);
    let long = "x".repeat(300);
    let e = rpc_error(c.call_raw("ping", json!({ "nonce": long })).await);
    assert_eq!(e["code"], ErrorCode::InvalidParams.code());

    // apps.list stub.
    let r = c.call_raw("apps.list", json!({})).await.unwrap();
    schema.check_result("apps.list", &r).unwrap();
    let apps = c.call::<AppsList>(&AppsListParams {}).await.unwrap().apps;
    assert!(apps.iter().any(|a| a.id == "steam"));

    // No events before subscribing.
    let r = c.call_raw(
        "subscribe",
        json!({ "events": ["home_pressed", "home_pressed"] }),
    );
    let r = r.await.unwrap();
    schema.check_result("subscribe", &r).unwrap();
    assert_eq!(r, json!({ "events": ["home_pressed"] }));

    // Home button (SIGUSR1 in M0) reaches the subscriber.
    d.signal(Signal::USR1);
    let (name, params) = timeout(TIMEOUT, c.next_event_raw()).await.unwrap().unwrap();
    assert_eq!(name, "home_pressed");
    schema.check_event(&name, &params).unwrap();

    d.signal(Signal::USR1);
    timeout(TIMEOUT, c.next_event::<HomePressed>())
        .await
        .unwrap()
        .unwrap();

    // After unsubscribing, the next message is the ping reply, not an event.
    let r = c.call_raw("unsubscribe", json!({ "events": [EventName::HomePressed] }));
    assert_eq!(r.await.unwrap(), json!({ "events": [] }));
    d.signal(Signal::USR1);
    tokio::time::sleep(Duration::from_millis(200)).await;
    c.send_line(r#"{"jsonrpc":"2.0","id":"last","method":"ping"}"#)
        .await
        .unwrap();
    let msg = timeout(TIMEOUT, c.read_raw())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(msg["id"], "last");
    schema.check_server_message("ping", &msg).unwrap();
}

#[tokio::test]
async fn version_mismatch_is_rejected_and_closes() {
    let d = Daemon::start();
    for client_version in [
        ApiVersion::new(1, 0),
        ApiVersion::new(0, 2),
        ApiVersion::new(0, 5),
    ] {
        let mut c = d.connect().await;
        let e = rpc_error(
            c.hello(client_version, Peer::new("test", "0"))
                .await
                .map(|_| Value::Null),
        );
        assert_eq!(e["code"], ErrorCode::VersionMismatch.code());
        assert_eq!(e["data"]["client_api_version"], client_version.to_string());
        assert_eq!(
            e["data"]["server_api_version"],
            session::VERSION.to_string()
        );
        assert_closed(&mut c).await;
    }
    // The daemon is still serving.
    d.client().await;
}

#[tokio::test]
async fn handshake_is_required_and_only_once() {
    let d = Daemon::start();
    let mut c = d.connect().await;
    let e = rpc_error(c.call_raw("ping", json!({})).await);
    assert_eq!(e["code"], ErrorCode::HandshakeRequired.code());
    let e = rpc_error(c.call_raw("apps.list", json!({})).await);
    assert_eq!(e["code"], ErrorCode::HandshakeRequired.code());
    // The connection survives and the handshake still works.
    c.hello(session::VERSION, Peer::new("test", "0"))
        .await
        .unwrap();
    let e = rpc_error(
        c.hello(session::VERSION, Peer::new("test", "0"))
            .await
            .map(|_| Value::Null),
    );
    assert_eq!(e["code"], ErrorCode::AlreadyHandshaken.code());
}

#[tokio::test]
async fn malformed_input_gets_jsonrpc_errors() {
    let d = Daemon::start();
    let schema = ApiSchema::session();
    let mut c = d.client().await;
    let cases = [
        ("this is not json", None, ErrorCode::ParseError),
        ("[]", None, ErrorCode::InvalidRequest),
        (
            r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#,
            Some(json!(1)),
            ErrorCode::InvalidRequest,
        ),
        (
            r#"{"jsonrpc":"2.0","id":2,"method":"nope"}"#,
            Some(json!(2)),
            ErrorCode::MethodNotFound,
        ),
        (
            r#"{"jsonrpc":"2.0","id":3,"method":"ping","params":[1]}"#,
            Some(json!(3)),
            ErrorCode::InvalidParams,
        ),
        (
            r#"{"jsonrpc":"2.0","id":4,"method":"subscribe","params":{"events":["x"]}}"#,
            Some(json!(4)),
            ErrorCode::InvalidParams,
        ),
    ];
    for (line, id, code) in cases {
        c.send_line(line).await.unwrap();
        let msg = timeout(TIMEOUT, c.read_raw())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(msg["error"]["code"], code.code(), "{line}");
        assert_eq!(msg["id"], id.unwrap_or(Value::Null), "{line}");
        schema.check_error(&msg["error"]).unwrap();
    }
    // A client notification gets no reply; the next reply belongs to the next request.
    c.send_line(r#"{"jsonrpc":"2.0","method":"ping"}"#)
        .await
        .unwrap();
    c.call::<Ping>(&PingParams::default()).await.unwrap();
}

#[tokio::test]
async fn sigterm_removes_socket() {
    let mut d = Daemon::start();
    d.signal(Signal::TERM);
    let status = d.child.wait().unwrap();
    assert!(status.success(), "{status}");
    assert!(!Path::new(&d.socket).exists());
}

#[tokio::test]
async fn refuses_to_replace_a_live_socket() {
    let d = Daemon::start();
    let out = Command::new(env!("CARGO_BIN_EXE_hinteros-session"))
        .arg("--socket")
        .arg(&d.socket)
        .output()
        .unwrap();
    assert!(!out.status.success());
    // The first instance is unaffected.
    d.client().await;
}

#[tokio::test]
async fn launch_command_track_exit_and_focus() {
    let d = Daemon::start();
    let mut c = d.client().await;
    call(
        &mut c,
        "subscribe",
        json!({ "events": ["home_pressed", "app_started", "app_exited", "focus_changed"] }),
    )
    .await
    .unwrap();

    // The app runs under our reaper, as gamescope needs, and records its parent's name.
    let out = d.dir.path().join("parent");
    let app = call(
        &mut c,
        "apps.launch",
        json!({
            "command": ["sh", "-c", format!("cat /proc/$PPID/comm > {}; exec sleep 30", out.display())],
            "name": "Sleeper",
        }),
    )
    .await
    .unwrap()["app"]
        .clone();
    assert_eq!(app["id"], "cmd-1");
    assert_eq!(app["name"], "Sleeper");
    assert_eq!(app["source"], "command");
    assert_eq!(
        next_event(&mut c).await,
        ("app_started".into(), json!({ "app": app }))
    );
    assert_eq!(
        next_event(&mut c).await,
        ("focus_changed".into(), json!({ "app": "cmd-1" }))
    );
    let parent = eventually("the app to start", async || {
        std::fs::read_to_string(&out).ok().filter(|s| !s.is_empty())
    })
    .await;
    assert_eq!(parent.trim(), "reaper");

    let running = call(&mut c, "apps.running", json!({})).await.unwrap();
    assert_eq!(running, json!({ "apps": [app], "focused": "cmd-1" }));

    // Home: the launcher gets focus, the app keeps running.
    d.signal(Signal::USR1);
    assert_eq!(next_event(&mut c).await.0, "home_pressed");
    assert_eq!(
        next_event(&mut c).await,
        ("focus_changed".into(), json!({ "app": null }))
    );
    let running = call(&mut c, "apps.running", json!({})).await.unwrap();
    assert_eq!(running["focused"], Value::Null);
    assert_eq!(running["apps"][0]["id"], "cmd-1");

    call(&mut c, "apps.focus", json!({ "id": "cmd-1" }))
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut c).await,
        ("focus_changed".into(), json!({ "app": "cmd-1" }))
    );

    // Kill it: app_exited (killed = crashed, 128 + SIGKILL), and the launcher is back.
    let pid = app["pid"].as_i64().unwrap() as i32;
    let reaper = Pid::from_raw(pid).unwrap();
    kill_process(reaper, Signal::TERM).unwrap(); // forwarded to the app's process group
    assert_eq!(
        next_event(&mut c).await,
        (
            "app_exited".into(),
            json!({ "id": "cmd-1", "reason": "crashed", "exit_code": 143 })
        )
    );
    assert_eq!(
        next_event(&mut c).await,
        ("focus_changed".into(), json!({ "app": null }))
    );
    assert_eq!(
        call(&mut c, "apps.running", json!({})).await.unwrap(),
        json!({ "apps": [], "focused": null })
    );

    // A clean exit is "exited".
    call(&mut c, "apps.launch", json!({ "command": ["true"] }))
        .await
        .unwrap();
    assert_eq!(next_event(&mut c).await.0, "app_started");
    assert_eq!(next_event(&mut c).await.0, "focus_changed");
    assert_eq!(
        next_event(&mut c).await,
        (
            "app_exited".into(),
            json!({ "id": "cmd-2", "reason": "exited", "exit_code": 0 })
        )
    );
}

#[tokio::test]
async fn launch_steam_url_with_a_fake_steam() {
    let d = Daemon::start_with(|dir| {
        let steam = dir.join("steam");
        script(
            &steam,
            &format!("echo \"$@\" >> {}/steam.log", dir.display()),
        );
        vec!["--steam-path".into(), steam.into()]
    });
    let mut c = d.client().await;
    call(
        &mut c,
        "subscribe",
        json!({ "events": ["app_started", "app_exited", "focus_changed"] }),
    )
    .await
    .unwrap();
    let r = call(
        &mut c,
        "apps.launch",
        json!({ "url": "steam://rungameid/1000001" }),
    )
    .await
    .unwrap();
    assert_eq!(
        r,
        json!({ "app": { "id": "steam-1000001", "name": "Steam game 1000001", "source": "steam", "gamescope_app_id": 1000001 } })
    );
    assert_eq!(next_event(&mut c).await.0, "app_started");
    assert_eq!(next_event(&mut c).await.0, "focus_changed");
    // Without gamescope the steam command's end stands in for the game's.
    assert_eq!(
        next_event(&mut c).await,
        (
            "app_exited".into(),
            json!({ "id": "steam-1000001", "reason": "exited", "exit_code": 0 })
        )
    );
    let log = std::fs::read_to_string(d.dir.path().join("steam.log")).unwrap();
    assert_eq!(log, "-silent steam://rungameid/1000001\n");
}

#[tokio::test]
async fn configured_apps_and_launch_errors() {
    let d = Daemon::start_with(|dir| {
        let config = dir.join("session.toml");
        std::fs::write(
            &config,
            r#"
            [[apps]]
            id = "shell"
            name = "Shell"
            command = ["sleep", "30"]
            [[apps]]
            id = "broken"
            name = "Broken"
            command = ["/nonexistent/program"]
            "#,
        )
        .unwrap();
        vec!["--config".into(), config.into()]
    });
    let mut c = d.client().await;
    let list = call(&mut c, "apps.list", json!({})).await.unwrap();
    assert_eq!(
        list,
        json!({ "apps": [
            { "id": "steam", "name": "Steam", "kind": "native" },
            { "id": "shell", "name": "Shell", "kind": "native" },
            { "id": "broken", "name": "Broken", "kind": "native" },
        ] })
    );

    let app = call(&mut c, "apps.launch", json!({ "id": "shell" }))
        .await
        .unwrap()["app"]
        .clone();
    assert_eq!(app["id"], "shell");
    // Launching a running app focuses it instead of starting a second one.
    let again = call(&mut c, "apps.launch", json!({ "id": "shell" }))
        .await
        .unwrap()["app"]
        .clone();
    assert_eq!(again, app);

    let code = |r: Result<Value, Value>| r.unwrap_err()["code"].clone();
    assert_eq!(
        code(call(&mut c, "apps.launch", json!({ "id": "nope" })).await),
        ErrorCode::AppNotFound.code()
    );
    assert_eq!(
        code(call(&mut c, "apps.launch", json!({ "id": "broken" })).await),
        ErrorCode::AppLaunchFailed.code()
    );
    assert_eq!(
        code(
            call(
                &mut c,
                "apps.launch",
                json!({ "command": ["no-such-program-hinteros"] })
            )
            .await
        ),
        ErrorCode::AppLaunchFailed.code()
    );
    assert_eq!(
        code(
            call(
                &mut c,
                "apps.launch",
                json!({ "id": "shell", "url": "steam://rungameid/1" })
            )
            .await
        ),
        ErrorCode::InvalidParams.code()
    );
    assert_eq!(
        code(call(&mut c, "apps.focus", json!({ "id": "cmd-1" })).await),
        ErrorCode::AppNotFound.code()
    );
    call(&mut c, "apps.focus", json!({ "id": null }))
        .await
        .unwrap();
    assert_eq!(
        call(&mut c, "apps.running", json!({})).await.unwrap()["focused"],
        Value::Null
    );
}

#[test]
fn reaper_subcommand() {
    let dir = tempfile::tempdir().unwrap();
    let late = dir.path().join("late");
    let out = Command::new(env!("CARGO_BIN_EXE_hinteros-session"))
        .args(["reaper", "SteamLaunch", "AppId=77", "--", "sh", "-c"])
        .arg(format!(
            "cat /proc/$PPID/comm; tr '\\0' ' ' < /proc/$PPID/cmdline; echo; \
             (sleep 0.3; echo done > {}) & exit 5",
            late.display()
        ))
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let mut lines = stdout.lines();
    // What gamescope looks for: a process named "reaper" with SteamLaunch AppId=<id> before --.
    assert_eq!(lines.next(), Some("reaper"));
    assert!(
        lines
            .next()
            .unwrap()
            .contains(" reaper SteamLaunch AppId=77 -- sh -c "),
        "{stdout}"
    );
    // The command's status, after waiting for the orphan it left behind.
    assert_eq!(out.status.code(), Some(5));
    assert_eq!(std::fs::read_to_string(&late).unwrap(), "done\n");

    let out = Command::new(env!("CARGO_BIN_EXE_hinteros-session"))
        .args(["reaper", "--", "/nonexistent"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127));
    let out = Command::new(env!("CARGO_BIN_EXE_hinteros-session"))
        .args(["reaper", "AppId=3", "--", "true"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[tokio::test]
async fn close_apps_politely_then_by_force() {
    let d = Daemon::start_with(|dir| {
        let config = dir.join("session.toml");
        std::fs::write(&config, "[close]\ntimeout_ms = 300\n").unwrap();
        vec!["--config".into(), config.into()]
    });
    let mut c = d.client().await;
    call(&mut c, "subscribe", json!({ "events": ["app_exited"] }))
        .await
        .unwrap();

    // Quits on SIGTERM.
    call(&mut c, "apps.launch", json!({ "command": ["sleep", "30"] }))
        .await
        .unwrap();
    assert_eq!(
        call(&mut c, "apps.close", json!({ "id": "cmd-1" })).await,
        Ok(json!({}))
    );
    assert_eq!(
        next_event(&mut c).await,
        (
            "app_exited".into(),
            json!({ "id": "cmd-1", "reason": "closed", "exit_code": 143 })
        )
    );

    // Ignores SIGTERM (and so does its child): killed after the timeout, tree and all.
    let pids = d.dir.path().join("pids");
    call(
        &mut c,
        "apps.launch",
        json!({ "command": ["sh", "-c", format!("trap '' TERM; sleep 30 & echo $$ $! > {}; wait; wait", pids.display())] }),
    )
    .await
    .unwrap();
    let pids: Vec<u32> = eventually("the app to start", async || {
        let s = std::fs::read_to_string(&pids).ok()?;
        let v: Vec<u32> = s
            .split_whitespace()
            .filter_map(|p| p.parse().ok())
            .collect();
        (v.len() == 2).then_some(v)
    })
    .await;
    let started = std::time::Instant::now();
    call(&mut c, "apps.close", json!({ "id": "cmd-2" }))
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut c).await,
        (
            "app_exited".into(),
            json!({ "id": "cmd-2", "reason": "closed", "exit_code": 137 })
        )
    );
    assert!(started.elapsed() >= Duration::from_millis(300));
    for pid in pids {
        assert!(!alive(pid), "{pid} survived");
    }

    let e = call(&mut c, "apps.close", json!({ "id": "cmd-2" }))
        .await
        .unwrap_err();
    assert_eq!(e["code"], ErrorCode::AppNotFound.code());
}

#[tokio::test]
async fn games_list_reads_the_steam_library() {
    let d = Daemon::start_with(|_| {
        vec![
            "--steam-dir".into(),
            steam_fixture().join(".local/share/Steam").into(),
        ]
    });
    let mut c = d.client().await;
    let games = call(&mut c, "games.list", json!({})).await.unwrap();
    let names: Vec<&str> = games["games"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Baldur's Gate 3", "Dota 2", "Hades", "Portal 2"]);
    let portal = &games["games"][3];
    assert_eq!(portal["appid"], 620);
    assert_eq!(portal["install_state"], "installed");
    assert_eq!(portal["last_played"], 1_786_300_589);
    let hero = portal["artwork"]["hero"].as_str().unwrap();
    assert!(Path::new(hero).is_file(), "{hero}");
    assert_eq!(games["games"][0]["artwork"]["hero"], Value::Null);

    // A launched game is named from the library.
    let steam = d.dir.path().join("steam");
    script(&steam, "exit 0");
    drop(c);
    let d2 = Daemon::start_with(|_| {
        vec![
            "--steam-dir".into(),
            steam_fixture().join(".local/share/Steam").into(),
            "--steam-path".into(),
            steam.into(),
        ]
    });
    let mut c = d2.client().await;
    let r = call(
        &mut c,
        "apps.launch",
        json!({ "url": "steam://rungameid/1145360" }),
    )
    .await
    .unwrap();
    assert_eq!(r["app"]["name"], "Hades");
}

#[tokio::test]
async fn games_changed_when_a_game_is_installed() {
    let d = Daemon::start_with(|dir| {
        copy_tree(&steam_fixture(), &dir.join("home"));
        let config = dir.join("session.toml");
        std::fs::write(
            &config,
            format!(
                "[steam]\ndir = \"{}\"\nlibrary_debounce_ms = 50\nlibrary_poll_s = 0\n",
                dir.join("home/.local/share/Steam").display()
            ),
        )
        .unwrap();
        vec!["--config".into(), config.into()]
    });
    let mut c = d.client().await;
    call(&mut c, "subscribe", json!({ "events": ["games_changed"] }))
        .await
        .unwrap();
    let before = call(&mut c, "games.list", json!({})).await.unwrap();
    // Give the watcher time to take its first snapshot.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Steam finishes installing a game: writes the manifest under a temp name, renames it.
    let apps = d.dir.path().join("home/.local/share/Steam/steamapps");
    let tmp = apps.join("appmanifest_400.acf.1.tmp");
    std::fs::write(
        &tmp,
        "\"AppState\"\n{\n\t\"appid\"\t\t\"400\"\n\t\"name\"\t\t\"Portal\"\n\t\"StateFlags\"\t\t\"4\"\n}\n",
    )
    .unwrap();
    std::fs::rename(&tmp, apps.join("appmanifest_400.acf")).unwrap();
    assert_eq!(
        next_event(&mut c).await,
        ("games_changed".into(), json!({}))
    );
    let after = call(&mut c, "games.list", json!({})).await.unwrap();
    assert_eq!(
        after["games"].as_array().unwrap().len(),
        before["games"].as_array().unwrap().len() + 1
    );
    assert!(
        after["games"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["name"] == "Portal")
    );
}

#[tokio::test]
async fn steam_entry_opens_big_picture() {
    let d = Daemon::start_with(|dir| {
        let steam = dir.join("steam");
        script(
            &steam,
            &format!("echo \"$@\" >> {}/steam.log", dir.display()),
        );
        vec!["--steam-path".into(), steam.into()]
    });
    let mut c = d.client().await;
    call(
        &mut c,
        "subscribe",
        json!({ "events": ["app_started", "app_exited", "focus_changed", "steam_attention"] }),
    )
    .await
    .unwrap();
    let apps = call(&mut c, "apps.list", json!({})).await.unwrap();
    assert_eq!(apps["apps"][0]["id"], "steam");
    let r = call(&mut c, "apps.launch", json!({ "id": "steam" }))
        .await
        .unwrap();
    assert_eq!(
        r,
        json!({ "app": { "id": "steam", "name": "Steam", "source": "steam_client", "gamescope_app_id": 769 } })
    );
    assert_eq!(next_event(&mut c).await.0, "app_started");
    assert_eq!(
        next_event(&mut c).await,
        ("focus_changed".into(), json!({ "app": "steam" }))
    );
    // Without gamescope the steam command's end stands in for Big Picture's.
    assert_eq!(event_named(&mut c, "app_exited").await["id"], "steam");
    let log = std::fs::read_to_string(d.dir.path().join("steam.log")).unwrap();
    // "-gamepadui" if no Steam client runs on this machine, else the URL for the running one.
    assert!(
        log == "-gamepadui\n" || log == "steam://open/bigpicture\n",
        "{log:?}"
    );

    // Nothing to show without gamescope.
    assert_eq!(
        call(&mut c, "steam.attention", json!({})).await.unwrap(),
        json!({ "windows": [] })
    );
    let e = call(&mut c, "steam.focus", json!({})).await.unwrap_err();
    assert_eq!(e["code"], ErrorCode::SteamNoWindows.code());
}

#[tokio::test]
async fn osk_and_keys_without_a_keyboard() {
    let d = Daemon::start();
    let mut c = d.client().await;
    call(&mut c, "subscribe", json!({ "events": ["osk_visibility"] }))
        .await
        .unwrap();
    // No virtual keyboard (--no-uinput) and no gamescope input method.
    for (method, params) in [
        ("keys.type", json!({ "text": "hello" })),
        ("keys.key", json!({ "key": "f5" })),
        ("keys.action", json!({ "action": "enter" })),
    ] {
        let e = call(&mut c, method, params).await.unwrap_err();
        assert_eq!(e["code"], ErrorCode::KeyboardUnavailable.code(), "{method}");
    }
    let e = call(&mut c, "keys.key", json!({ "key": "nope" }))
        .await
        .unwrap_err();
    assert_eq!(e["code"], ErrorCode::InvalidParams.code());

    // The on-screen keyboard state works without gamescope (nothing to show it on).
    assert_eq!(
        call(&mut c, "osk.show", json!({})).await.unwrap(),
        json!({ "visible": true })
    );
    assert_eq!(
        next_event(&mut c).await,
        ("osk_visibility".into(), json!({ "visible": true }))
    );
    assert_eq!(
        call(&mut c, "osk.hide", json!({})).await.unwrap(),
        json!({ "visible": false })
    );
    assert_eq!(
        next_event(&mut c).await,
        ("osk_visibility".into(), json!({ "visible": false }))
    );

    // Disabled in the config: osk_unavailable.
    let d = Daemon::start_with(|dir| {
        let config = dir.join("session.toml");
        std::fs::write(&config, "[osk]\nenable = false\n").unwrap();
        vec!["--config".into(), config.into()]
    });
    let mut c = d.client().await;
    let e = call(&mut c, "osk.show", json!({})).await.unwrap_err();
    assert_eq!(e["code"], ErrorCode::OskUnavailable.code());
}

#[tokio::test]
async fn keys_go_to_the_fake_keyboard() {
    let d = Daemon::start_with(|dir| vec!["--fake-keyboard".into(), dir.join("keys.log").into()]);
    let log = d.dir.path().join("keys.log");
    let mut c = d.client().await;
    let r = call(&mut c, "keys.type", json!({ "text": "a:" }))
        .await
        .unwrap();
    assert_eq!(r, json!({ "untypeable": [] }));
    call(
        &mut c,
        "keys.key",
        json!({ "key": "l", "modifiers": ["ctrl"] }),
    )
    .await
    .unwrap();
    call(&mut c, "keys.action", json!({ "action": "backspace" }))
        .await
        .unwrap();
    let expected = "down 30\nup 30\ndown 42\ndown 39\nup 39\nup 42\n\
                    down 29\ndown 38\nup 38\nup 29\ndown 14\nup 14\n";
    eventually("the key events", async || {
        (std::fs::read_to_string(&log).ok()? == expected).then_some(())
    })
    .await;
}
