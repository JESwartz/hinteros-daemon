//! Black-box tests: the real `hinteros-system` binary against a temporary root, a private bus
//! with fake logind/polkit/systemd, fake nix tools and a real signed git upstream (see
//! `common/mod.rs`). Every response, error and event is validated against
//! `schema/system-api.json`.

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use hinteros_api::client::{Client, ClientError};
use hinteros_api::common::Peer;
use hinteros_api::conformance::ApiSchema;
use hinteros_api::{ApiVersion, ErrorCode, system};
use serde_json::{Value, json};
use tokio::time::timeout;

mod common;

use common::*;

#[tokio::test]
async fn handshake_and_ping() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("system.sock");
    let mut child = Command::new(env!("CARGO_BIN_EXE_hinteros-system"))
        .arg("--socket")
        .arg(&socket)
        .arg("--run-dir")
        .arg(dir.path())
        .arg("--state-dir")
        .arg(dir.path())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        assert!(Instant::now() < deadline, "service did not start listening");
        std::thread::sleep(Duration::from_millis(20));
    }
    let mode = std::fs::metadata(&socket).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o660);

    let schema = ApiSchema::system();
    let mut c = Client::connect(&socket).await.unwrap();
    // Before hello: handshake_required.
    match c.call_raw("system.info", json!({})).await {
        Err(ClientError::Rpc(e)) => assert_eq!(e.kind(), Some(ErrorCode::HandshakeRequired)),
        other => panic!("{other:?}"),
    }
    let hello = c
        .hello(system::VERSION, Peer::new("test", "0"))
        .await
        .unwrap();
    schema
        .check_result("hello", &serde_json::to_value(&hello).unwrap())
        .unwrap();
    let r = c.call_raw("ping", json!({ "nonce": "n" })).await.unwrap();
    schema.check_result("ping", &r).unwrap();
    assert_eq!(r["nonce"], "n");

    let mut c = Client::connect(&socket).await.unwrap();
    match c.hello(ApiVersion::new(9, 0), Peer::new("test", "0")).await {
        Err(ClientError::Rpc(e)) => assert_eq!(e.kind(), Some(ErrorCode::VersionMismatch)),
        other => panic!("expected version mismatch, got {other:?}"),
    }

    child.kill().unwrap();
    let out = child.wait_with_output().unwrap();
    let log = String::from_utf8_lossy(&out.stderr);
    // SO_PEERCRED (and a pidfd): the caller (this test process) is logged.
    let me = std::process::id();
    assert!(log.contains(&format!("pid=Some({me})")), "log: {log}");
    assert!(log.contains("pidfd=true"), "log: {log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_methods_and_denials() {
    let Some(sys) = System::start(Root::create(), &[]).await else {
        return;
    };
    let mut c = sys.client().await;

    let info = call(&mut c, "system.info", json!({})).await.unwrap();
    assert_eq!(info["version"], "0.1.0-test");
    assert_eq!(info["dev"], true);
    assert_eq!(info["base"], "unstable");
    assert_eq!(info["generation"], 1);
    assert_eq!(info["channel"], Value::Null, "no lock, no channel");
    assert_eq!(info["ssh"]["enabled"], false);
    assert_eq!(
        info["ssh"]["host_key_fingerprints"],
        json!(["SHA256:2CixtE95MNbZHanQGv6hP2D3qfguu9QZfjca2OaupKI"])
    );

    let settings = call(&mut c, "settings.get", json!({})).await.unwrap();
    assert_eq!(
        settings["settings"],
        json!({"base": "unstable", "ssh": {"enabled": false}})
    );
    let gens = call(&mut c, "generations.list", json!({})).await.unwrap();
    assert_eq!(gens["generations"][0]["generation"], 1);
    assert_eq!(gens["generations"][0]["current"], true);
    assert_eq!(gens["generations"][0]["booted"], true);
    assert_eq!(
        call(&mut c, "jobs.list", json!({})).await.unwrap(),
        json!({"jobs": []})
    );
    let e = call(&mut c, "jobs.get", json!({"job_id": "0123456789abcdef"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound.code());
    // Read-only methods never ask polkit.
    assert!(sys.fakes.lock().unwrap().polkit_calls.is_empty());

    // Hostile input: closed params, no paths, no URLs, no forged identity.
    let hostile = [
        ("rebuild", json!({"uid": 0})),
        ("ssh.set", json!({"enabled": true, "user": "root"})),
        ("ssh.set", json!({"enabled": "yes"})),
        (
            "settings.set",
            json!({"revision": "0".repeat(64), "changes": {"substituters": ["http://evil"]}}),
        ),
        (
            "settings.set",
            json!({"revision": "../../etc/shadow", "changes": {}}),
        ),
        (
            "channel.set",
            json!({"channel": "https://evil.example/hinteros.git"}),
        ),
        (
            "channel.set",
            json!({"channel": "beta", "url": "https://evil.example"}),
        ),
        ("update.apply", json!({"rev": "../../../etc/passwd"})),
        ("rollback", json!({"generation": "1; rm -rf /"})),
        ("jobs.cancel", json!({"job_id": "/etc/passwd"})),
        ("system.info", json!({"path": "/"})),
    ];
    for (m, p) in hostile {
        let e = call(&mut c, m, p.clone()).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidParams.code(), "{m} {p}");
    }
    let e = call(
        &mut c,
        "settings.set",
        json!({"revision": "0".repeat(64), "changes": {"substituters": []}}),
    )
    .await
    .unwrap_err();
    assert_eq!(
        e["data"],
        json!({"field": "changes.substituters", "reason": "unknown_field"})
    );
    let e = call(&mut c, "rebuild", json!({"uid": 0}))
        .await
        .unwrap_err();
    assert_eq!(e["data"]["field"], "uid");

    // Seat check: someone else owns seat0.
    sys.fakes.lock().unwrap().seat_user = Some(me() + 1);
    let e = call(&mut c, "rebuild", json!({})).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::PermissionDenied.code());
    assert_eq!(
        e["data"],
        json!({"action": "org.hinteros.system.rebuild", "reason": "seat"})
    );
    sys.fakes.lock().unwrap().seat_user = None;
    let e = call(&mut c, "rebuild", json!({})).await.unwrap_err();
    assert_eq!(e["data"]["reason"], "seat");
    assert!(
        sys.fakes.lock().unwrap().polkit_calls.is_empty(),
        "seat first"
    );
    sys.fakes.lock().unwrap().seat_user = Some(me());

    // polkit says no.
    sys.fakes
        .lock()
        .unwrap()
        .answers
        .insert("org.hinteros.system.rebuild".into(), Answer::No);
    let e = call(&mut c, "rebuild", json!({})).await.unwrap_err();
    assert_eq!(
        e["data"],
        json!({"action": "org.hinteros.system.rebuild", "reason": "polkit"})
    );
    // SSH needs the admin password (auth_admin): a challenge, refused distinctly.
    sys.fakes
        .lock()
        .unwrap()
        .answers
        .insert("org.hinteros.system.ssh".into(), Answer::Challenge);
    let e = call(&mut c, "ssh.set", json!({"enabled": true}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::AuthenticationRequired.code());
    assert_eq!(e["data"], json!({"action": "org.hinteros.system.ssh"}));
    {
        let f = sys.fakes.lock().unwrap();
        let (action, pid, pidfd) = f.polkit_calls.last().unwrap().clone();
        assert_eq!(action, "org.hinteros.system.ssh");
        assert_eq!(pid, Some(std::process::id()), "the subject is the caller");
        assert!(pidfd, "the subject carries the caller's pidfd");
        assert!(f.units.is_empty(), "no job started");
    }
    // Nothing changed on disk.
    assert_eq!(sys.root.read("etc/hinteros/state.json"), STATE);

    // Every mutating request and denial is audited.
    let audit = sys.audit();
    let decisions: Vec<(&str, &str)> = audit
        .iter()
        .filter(|m| m.get("HINTEROS_AUDIT").map(String::as_str) == Some("1"))
        .map(|m| {
            (
                m["HINTEROS_METHOD"].as_str(),
                m["HINTEROS_DECISION"].as_str(),
            )
        })
        .collect();
    for expected in [
        ("rebuild", "invalid"),
        ("rebuild", "denied_seat"),
        ("rebuild", "denied_polkit"),
        ("ssh.set", "auth_required"),
    ] {
        assert!(
            decisions.contains(&expected),
            "{expected:?} in {decisions:?}"
        );
    }
    let one = audit
        .iter()
        .find(|m| m.get("HINTEROS_DECISION").map(String::as_str) == Some("auth_required"))
        .unwrap();
    assert_eq!(one["HINTEROS_UID"], me().to_string());
    assert_eq!(one["HINTEROS_PID"], std::process::id().to_string());
    assert_eq!(one["HINTEROS_ACTION"], "org.hinteros.system.ssh");
    assert!(!one["HINTEROS_EXE"].is_empty());
    assert_eq!(one["SYSLOG_IDENTIFIER"], "hinteros-system");
}

#[tokio::test(flavor = "multi_thread")]
async fn limits() {
    let Some(sys) = System::start(
        Root::create(),
        &[
            "--rate-burst",
            "2",
            "--rate-per-minute",
            "1",
            "--max-connections-per-uid",
            "3",
        ],
    )
    .await
    else {
        return;
    };
    sys.fakes.lock().unwrap().seat_user = Some(me() + 1);
    let mut c = sys.client().await;
    for _ in 0..2 {
        let e = call(&mut c, "rebuild", json!({})).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::PermissionDenied.code());
    }
    let e = call(&mut c, "rebuild", json!({})).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::RateLimited.code());
    assert!(e["data"]["retry_after_ms"].as_u64().unwrap() > 1000);
    // Read-only methods are not rate limited.
    call(&mut c, "system.info", json!({})).await.unwrap();

    // Messages over 64 KiB: parse error, connection closed.
    let mut big = sys.client().await;
    let line = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"ping","params":{{"nonce":"{}"}}}}"#,
        "x".repeat(70_000)
    );
    big.send_line(&line).await.unwrap();
    let reply = timeout(TIMEOUT, big.read_raw())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(reply["error"]["code"], ErrorCode::ParseError.code());
    assert_eq!(
        timeout(TIMEOUT, big.read_raw()).await.unwrap().unwrap(),
        None
    );
    // Not UTF-8 either.
    let mut raw = tokio::net::UnixStream::connect(&sys.socket).await.unwrap();
    tokio::io::AsyncWriteExt::write_all(&mut raw, b"\xff\xfe\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    timeout(
        TIMEOUT,
        tokio::io::AsyncReadExt::read_to_end(&mut raw, &mut buf),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(String::from_utf8_lossy(&buf).contains("-32700"));
    // Let the closed connections give their slots back.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Connections per uid: `c` is open; two more fit, the next is closed at once.
    let _c2 = sys.client().await;
    let _c3 = sys.client().await;
    let mut c4 = Client::connect(&sys.socket).await.unwrap();
    let r = timeout(TIMEOUT, c4.hello(system::VERSION, Peer::new("t", "0")))
        .await
        .unwrap();
    assert!(r.is_err(), "fourth connection must be refused");
    drop(_c3);
    // The slot is freed when a connection closes.
    let mut ok = false;
    for _ in 0..50 {
        let mut c5 = Client::connect(&sys.socket).await.unwrap();
        if c5.hello(system::VERSION, Peer::new("t", "0")).await.is_ok() {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ok);

    // No hello within 5 s: closed.
    let mut idle = Client::connect(&sys.socket).await.unwrap();
    drop(c);
    let started = Instant::now();
    let end = timeout(Duration::from_secs(10), idle.read_raw())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(end, None);
    assert!(started.elapsed() >= Duration::from_secs(4));
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_ssh_rebuild_rollback() {
    let Some(sys) = System::start(Root::create(), &[]).await else {
        return;
    };
    let root = &sys.root;
    let mut c = sys.subscriber().await;

    // ssh.set: a job that edits staging, builds, switches, then replaces /etc/hinteros.
    let r = call(&mut c, "ssh.set", json!({"enabled": true}))
        .await
        .unwrap();
    let id = r["job_id"].as_str().unwrap().to_owned();
    let events = until_finished(&mut c, &id).await;
    assert_eq!(events[0].0, "job_started");
    assert_eq!(
        events[0].1,
        json!({"job_id": id, "kind": "ssh", "uid": me()})
    );
    let stages: Vec<&str> = events
        .iter()
        .filter(|(n, _)| n == "job_progress")
        .map(|(_, p)| p["stage"].as_str().unwrap())
        .collect();
    for s in ["preparing", "evaluating", "activating", "finishing"] {
        assert!(stages.contains(&s), "{s} in {stages:?}");
    }
    let done = finished(&events);
    assert_eq!(done["ok"], true, "{done} / {}", sys.job_log(&id));
    assert_eq!(done["result"], json!({"generation": 2, "rev": null}));
    assert_eq!(done["session_restart_required"], false);
    assert_eq!(root.etc_state()["settings"]["ssh"]["enable"], true);
    // Everything else in state.json survives, written canonically by the service.
    assert_eq!(root.etc_state()["apps"], json!(["steam"]));
    assert_eq!(root.etc_state()["profiles"][0]["user"], "hinteros");
    assert_eq!(root.current(), root.p("store/gen2"));
    assert!(
        root.read("switch.log")
            .contains(&format!("{} switch", root.p("store/gen2").display()))
    );
    assert!(
        !root.p("var/lib/hinteros/staging").exists(),
        "staging cleaned up"
    );
    let nix_log = root.read("nix.log");
    assert!(nix_log.contains("--log-format internal-json"), "{nix_log}");
    assert!(nix_log.contains("#nixosConfigurations.hinteros.config.system.build.toplevel"));
    // The generation's snapshot is the new config.
    assert_eq!(
        root.read("store/gen2/hinteros/state.json"),
        root.read("etc/hinteros/state.json")
    );
    {
        let f = sys.fakes.lock().unwrap();
        assert_eq!(f.units, [format!("hinteros-job@{id}.service")]);
        assert!(f.inhibits.contains(&"delay".to_string()));
        assert!(f.inhibits.contains(&"block".to_string()));
    }
    let job = call(&mut c, "jobs.get", json!({"job_id": id}))
        .await
        .unwrap();
    assert_eq!(job["job"]["state"], "succeeded");
    let audit = sys.audit();
    assert!(audit.iter().any(|m| m.get("HINTEROS_JOB_ID") == Some(&id)
        && m.get("HINTEROS_DECISION").map(String::as_str) == Some("allowed")));
    assert!(audit.iter().any(|m| m.get("HINTEROS_JOB_ID") == Some(&id)
        && m.get("HINTEROS_OUTCOME").map(String::as_str) == Some("succeeded")
        && m.get("HINTEROS_GENERATION_AFTER").map(String::as_str) == Some("2")));

    // Already on: nothing to do.
    let r = call(&mut c, "ssh.set", json!({"enabled": true}))
        .await
        .unwrap();
    assert_eq!(r, json!({"job_id": null}));
    let info = call(&mut c, "system.info", json!({})).await.unwrap();
    assert_eq!(info["ssh"]["enabled"], true);
    assert_eq!(info["generation"], 2);

    // settings.set: revision checks and a closed change set.
    let s = call(&mut c, "settings.get", json!({})).await.unwrap();
    let rev = s["revision"].as_str().unwrap().to_owned();
    let e = call(
        &mut c,
        "settings.set",
        json!({"revision": "0".repeat(64), "changes": {}}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::StateConflict.code());
    assert_eq!(e["data"], json!({"reason": "revision"}));
    let r = call(
        &mut c,
        "settings.set",
        json!({"revision": rev, "changes": {}}),
    )
    .await
    .unwrap();
    assert_eq!(r, json!({"job_id": null}));

    // base.set: unstable → stable is a downgrade.
    let e = call(&mut c, "base.set", json!({"base": "stable"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NeedsAcknowledgement.code());
    let r = call(
        &mut c,
        "base.set",
        json!({"base": "stable", "acknowledge_downgrade": true}),
    )
    .await
    .unwrap();
    let id = r["job_id"].as_str().unwrap().to_owned();
    assert_eq!(finished(&until_finished(&mut c, &id).await)["ok"], true);
    assert_eq!(root.etc_state()["base"], "stable");
    assert_eq!(
        call(&mut c, "base.set", json!({"base": "stable"}))
            .await
            .unwrap()["job_id"],
        Value::Null
    );

    // One job at a time, and cancel before activation leaves the box unchanged.
    let before = root.read("etc/hinteros/state.json");
    root.touch("build-hold");
    let r = call(&mut c, "rebuild", json!({})).await.unwrap();
    let id = r["job_id"].as_str().unwrap().to_owned();
    let e = call(&mut c, "ssh.set", json!({"enabled": false}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::JobBusy.code());
    assert_eq!(e["data"]["job_id"], id.as_str());
    let e = call(&mut c, "rebuild", json!({})).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::JobBusy.code());
    let list = call(&mut c, "jobs.list", json!({})).await.unwrap();
    assert_eq!(list["jobs"][0]["state"], "running");
    // Wait until it is building, then cancel.
    let job_dir = root.p(&format!("run/jobs/{id}"));
    eventually("the build to start", || {
        root.read("nix.log").lines().count() >= 3
    })
    .await;
    let r = call(&mut c, "jobs.cancel", json!({"job_id": id}))
        .await
        .unwrap();
    assert_eq!(r["job_id"], id.as_str());
    let events = until_finished(&mut c, &id).await;
    let done = finished(&events);
    assert_eq!(done["ok"], false);
    assert_eq!(done["error"]["code"], ErrorCode::JobFailed.code());
    assert_eq!(done["error"]["message"], "cancelled");
    let job = call(&mut c, "jobs.get", json!({"job_id": id}))
        .await
        .unwrap();
    assert_eq!(job["job"]["state"], "cancelled");
    assert_eq!(root.read("etc/hinteros/state.json"), before);
    assert_eq!(root.current(), root.p("store/gen3"));
    assert!(!root.p("var/lib/hinteros/staging").exists());
    assert!(job_dir.join("status.json").exists());
    root.rm("build-hold");
    let e = call(&mut c, "jobs.cancel", json!({"job_id": id}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::JobBusy.code(), "already finished");

    // A failed build: job_failed with the nix error as the hint; nothing changed.
    root.touch("build-fail");
    let r = call(&mut c, "rebuild", json!({})).await.unwrap();
    let id = r["job_id"].as_str().unwrap().to_owned();
    let done = finished(&until_finished(&mut c, &id).await).clone();
    root.rm("build-fail");
    assert_eq!(done["ok"], false);
    assert_eq!(
        done["error"]["data"]["stage"], "downloading",
        "where the fake build fails"
    );
    assert!(
        done["error"]["data"]["log_hint"]
            .as_str()
            .unwrap()
            .contains("builder for fake.drv failed"),
        "{done}"
    );
    assert_eq!(root.read("etc/hinteros/state.json"), before);

    // A failed switch: the profile goes back to the old generation, /etc/hinteros unchanged.
    root.touch("switch-fail");
    let r = call(&mut c, "ssh.set", json!({"enabled": false}))
        .await
        .unwrap();
    let id = r["job_id"].as_str().unwrap().to_owned();
    let done = finished(&until_finished(&mut c, &id).await).clone();
    root.rm("switch-fail");
    assert_eq!(done["ok"], false, "{}", sys.job_log(&id));
    assert_eq!(done["error"]["data"]["stage"], "activating");
    assert_eq!(root.etc_state()["settings"]["ssh"]["enable"], true);
    let profile = std::fs::read_link(root.p("profiles/system")).unwrap();
    assert_eq!(
        std::fs::canonicalize(root.p("profiles/system")).unwrap(),
        root.p("store/gen3"),
        "{profile:?}"
    );
    assert!(
        root.read("switch.log")
            .contains(&format!("{} boot", root.p("store/gen3").display()))
    );

    // A new session daemon package: session_restart_required.
    std::fs::write(root.p("session-pkg"), "b").unwrap();
    let r = call(&mut c, "rebuild", json!({})).await.unwrap();
    let id = r["job_id"].as_str().unwrap().to_owned();
    let done = finished(&until_finished(&mut c, &id).await).clone();
    assert_eq!(done["ok"], true, "{}", sys.job_log(&id));
    assert_eq!(done["session_restart_required"], true);

    // Hardware re-detection runs nixos-facter into staging and applies it.
    let r = call(&mut c, "hardware.redetect", json!({})).await.unwrap();
    let id = r["job_id"].as_str().unwrap().to_owned();
    assert_eq!(finished(&until_finished(&mut c, &id).await)["ok"], true);
    assert!(root.read("etc/hinteros/facter.json").contains("fake"));

    // Rollback to generation 1: switch, then /etc/hinteros from gen1's snapshot (facter stays).
    let gens = call(&mut c, "generations.list", json!({})).await.unwrap();
    let current = gens["generations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["current"] == true)
        .unwrap()["generation"]
        .as_u64()
        .unwrap();
    let e = call(&mut c, "rollback", json!({"generation": 99}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound.code());
    let e = call(&mut c, "rollback", json!({"generation": current}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound.code(), "not the current one");
    let r = call(&mut c, "rollback", json!({"generation": 1}))
        .await
        .unwrap();
    let id = r["job_id"].as_str().unwrap().to_owned();
    let done = finished(&until_finished(&mut c, &id).await).clone();
    assert_eq!(done["ok"], true, "{}", sys.job_log(&id));
    assert_eq!(done["result"]["generation"], 1);
    assert_eq!(root.current(), root.p("store/gen1"));
    assert_eq!(root.read("etc/hinteros/state.json"), STATE);
    assert!(root.p("etc/hinteros/facter.json").exists());
    let info = call(&mut c, "system.info", json!({})).await.unwrap();
    assert_eq!(info["generation"], 1);
    assert_eq!(info["ssh"]["enabled"], false);

    // Finished jobs are listed (the running one and the last ten).
    let list = call(&mut c, "jobs.list", json!({})).await.unwrap();
    assert!(list["jobs"].as_array().unwrap().len() >= 8);
}

#[tokio::test(flavor = "multi_thread")]
async fn updates_are_verified() {
    let root = Root::create();
    let Some(up) = Upstream::create(&root) else {
        return;
    };
    let c1 = up.commit("Initial", Some("release"));
    up.push(&c1, "stable");
    root.set_lock("stable", &c1);
    let c2 = up.commit(
        "Release 0.2\n\nFixes a hole.\n\nSecurity: high",
        Some("release"),
    );
    let c3 = up.commit("Release 0.2.1", Some("release"));
    up.push(&c3, "stable");
    let Some(sys) = System::start(root, &[]).await else {
        return;
    };
    let root = &sys.root;
    let mut c = sys.subscriber().await;
    let info = call(&mut c, "system.info", json!({})).await.unwrap();
    assert_eq!(info["channel"], "stable");
    assert_eq!(info["rev"], c1.as_str());

    // Nothing checked yet: apply is refused.
    let e = call(&mut c, "update.apply", json!({"rev": c3}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::StateConflict.code());
    assert_eq!(e["data"], json!({"reason": "update_rev"}));

    let r = call(&mut c, "update.check", json!({})).await.unwrap();
    assert_eq!(r["result"], Value::Null);
    let id = r["job_id"].as_str().unwrap().to_owned();
    let events = until_finished(&mut c, &id).await;
    let done = finished(&events);
    assert_eq!(done["ok"], true, "{done} {}", sys.job_log(&id));
    let result = &done["result"];
    assert_eq!(result["channel"], "stable");
    assert_eq!(result["current_rev"], c1.as_str());
    let available = &result["available"];
    assert_eq!(available["rev"], c3.as_str());
    assert_eq!(available["summary"], "Release 0.2.1");
    assert_eq!(available["signer"], "release@hinteros");
    assert_eq!(available["security"], "high", "from c2's trailer");
    assert_eq!(available["commits"], 2);
    let stages: Vec<&str> = events
        .iter()
        .filter(|(n, _)| n == "job_progress")
        .map(|(_, p)| p["stage"].as_str().unwrap())
        .collect();
    assert!(
        stages.contains(&"fetching") && stages.contains(&"verifying"),
        "{stages:?}"
    );

    // Within 60 s: the cached result, no job.
    let r = call(&mut c, "update.check", json!({})).await.unwrap();
    assert_eq!(r["job_id"], Value::Null);
    assert_eq!(r["result"], *result);

    // Only the verified revision can be applied.
    let e = call(&mut c, "update.apply", json!({"rev": c2}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::StateConflict.code());
    let r = call(&mut c, "update.apply", json!({"rev": c3}))
        .await
        .unwrap();
    let id = r["job_id"].as_str().unwrap().to_owned();
    let done = finished(&until_finished(&mut c, &id).await).clone();
    assert_eq!(done["ok"], true, "{done} {}", sys.job_log(&id));
    assert_eq!(done["result"]["rev"], c3.as_str());
    let lock = root.read("etc/hinteros/flake.lock");
    assert!(lock.contains(&c3), "{lock}");
    assert!(root.read("nix.log").contains(&format!(
        "--override-input hinteros {}&rev={c3}",
        root.input_url("stable")
    )));
    let info = call(&mut c, "system.info", json!({})).await.unwrap();
    assert_eq!(info["rev"], c3.as_str());
    assert_eq!(info["channel"], "stable");

    // An unsigned tip.
    let c4 = up.commit("Sneaky", None);
    up.push(&c4, "stable");
    let failure = check_fails(&mut c).await;
    assert_eq!(code(&failure), ErrorCode::UpdateVerificationFailed.code());
    assert_eq!(failure["data"], json!({"reason": "unsigned"}));

    // Signed, but not by an allowed signer.
    let c5 = up.commit("Rogue", Some("rogue"));
    up.push(&c5, "stable");
    let failure = check_fails(&mut c).await;
    assert_eq!(failure["data"], json!({"reason": "unknown_signer"}));

    // A properly signed tip that rewinds the branch (not a descendant of c3).
    up.checkout(&c1);
    let old = up.commit("Old branch", Some("release"));
    up.push(&old, "stable");
    let failure = check_fails(&mut c).await;
    assert_eq!(failure["data"], json!({"reason": "not_fast_forward"}));

    // The server is gone.
    std::fs::rename(root.p("upstream.git"), root.p("upstream.moved")).unwrap();
    let failure = check_fails(&mut c).await;
    assert_eq!(code(&failure), ErrorCode::UpdateUnreachable.code());
    assert_eq!(failure["data"], json!({"host": "localhost"}));
    std::fs::rename(root.p("upstream.moved"), root.p("upstream.git")).unwrap();
    // Nothing of that changed the box.
    assert!(root.read("etc/hinteros/flake.lock").contains(&c3));

    // Channel switch to beta: a signed descendant of the installed revision.
    up.checkout(&c3);
    let b1 = up.commit("Beta 0.3", Some("release"));
    up.push(&b1, "beta");
    let r = call(&mut c, "channel.set", json!({"channel": "beta"}))
        .await
        .unwrap();
    let id = r["job_id"].as_str().unwrap().to_owned();
    let done = finished(&until_finished(&mut c, &id).await).clone();
    assert_eq!(done["ok"], true, "{done} {}", sys.job_log(&id));
    let flake = root.read("etc/hinteros/flake.nix");
    assert!(
        flake.contains(&format!("\"{}\"", root.input_url("beta"))),
        "{flake}"
    );
    assert!(!flake.contains("ref=stable"));
    let info = call(&mut c, "system.info", json!({})).await.unwrap();
    assert_eq!(info["channel"], "beta");
    assert_eq!(info["rev"], b1.as_str());
    assert_eq!(
        call(&mut c, "channel.set", json!({"channel": "beta"}))
            .await
            .unwrap()["job_id"],
        Value::Null
    );

    // Back to stable is a downgrade: refused without acknowledgement.
    let e = call(&mut c, "channel.set", json!({"channel": "stable"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NeedsAcknowledgement.code());
    let r = call(
        &mut c,
        "channel.set",
        json!({"channel": "stable", "acknowledge_downgrade": true}),
    )
    .await
    .unwrap();
    let id = r["job_id"].as_str().unwrap().to_owned();
    let done = finished(&until_finished(&mut c, &id).await).clone();
    assert_eq!(done["ok"], true, "{done} {}", sys.job_log(&id));
    assert_eq!(
        call(&mut c, "system.info", json!({})).await.unwrap()["rev"],
        old.as_str()
    );
    assert!(root.read("etc/hinteros/flake.nix").contains("ref=stable"));
}

/// Run an update check that must fail; returns `job_finished.error`.
async fn check_fails(c: &mut Client) -> Value {
    let r = call(c, "update.check", json!({})).await.unwrap();
    let id = r["job_id"].as_str().expect("a new check job").to_owned();
    let done = finished(&until_finished(c, &id).await).clone();
    assert_eq!(done["ok"], false, "{done}");
    done["error"].clone()
}
