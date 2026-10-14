//! The system relay: `system.*` methods and job events between the launcher and a fake system
//! service (speaking the system API on a temporary socket).

use std::path::Path;

use hinteros_api::codec::{LineReader, write_message};
use hinteros_api::common::Peer;
use hinteros_api::server::{Connection, Step};
use hinteros_api::{ApiVersion, ErrorCode, jsonrpc, system};
use serde_json::{Value, json};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

mod common;

use common::*;

/// A fake system service: answers `subscribe`, `system.info` and `ssh.set` (with
/// `authentication_required`), sends `job_started` after a subscription, and reports every
/// request it saw.
fn fake_system(
    path: &Path,
    version: ApiVersion,
) -> (
    tokio::task::JoinHandle<()>,
    mpsc::UnboundedReceiver<(String, Value)>,
) {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).unwrap();
    let (seen_tx, seen) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        // Connections die with the service (aborting this task drops the set).
        let mut conns = tokio::task::JoinSet::new();
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let seen_tx = seen_tx.clone();
            conns.spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut reader = LineReader::new(r);
                let mut conn = Connection::new(version, Peer::new("fake-system", "0"));
                while let Ok(Some(line)) = reader.next_line().await {
                    match conn.step(&line) {
                        Step::Reply(v) => write_message(&mut w, &v).await.unwrap(),
                        Step::ReplyAndClose(v) => {
                            write_message(&mut w, &v).await.unwrap();
                            return;
                        }
                        Step::Ignore => {}
                        Step::Dispatch(req) => {
                            let id = req.id.unwrap();
                            let params = req.params.clone().unwrap_or(json!({}));
                            let _ = seen_tx.send((req.method.clone(), params.clone()));
                            let outcome = match req.method.as_str() {
                                "subscribe" => Ok(params.clone()),
                                "system.info" => Ok(json!({
                                    "version": "0.1.0", "channel": "stable", "base": "unstable",
                                    "rev": null, "generation": 3, "hostname": "box", "dev": false,
                                    "ssh": {"enabled": false, "host_key_fingerprints": []}
                                })),
                                "rebuild" => {
                                    // Slow, like a call waiting for the admin password.
                                    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                                    Ok(json!({"job_id": null}))
                                }
                                "ssh.set" => Err(hinteros_api::ErrorObject::with_data(
                                    ErrorCode::AuthenticationRequired,
                                    "needs the admin password",
                                    json!({"action": "org.hinteros.system.ssh"}),
                                )),
                                // Installing an update needs the admin PIN (system API 0.5).
                                "update.apply" if params["admin_pin"] == "2468" => {
                                    Ok(json!({"job_id": "3f9a0c2b7e4d1a65"}))
                                }
                                "update.apply" => Err(hinteros_api::ErrorObject::with_data(
                                    ErrorCode::PinRequired,
                                    "a PIN is needed",
                                    json!({"pin": "admin"}),
                                )),
                                m => Err(hinteros_api::server::method_not_found(m)),
                            };
                            write_message(&mut w, &jsonrpc::response(&id, outcome)).await.unwrap();
                            if req.method == "subscribe" {
                                let ev = jsonrpc::notification(
                                    "job_started",
                                    json!({
                                        "job_id": "0123456789abcdef", "kind": "apps_install",
                                        "uid": 1000, "started_at": 1791300000,
                                        "subject": {"owner": "p1", "apps": [{"id": "moonlight", "name": "Moonlight"}]}
                                    }),
                                );
                                write_message(&mut w, &ev).await.unwrap();
                            }
                        }
                    }
                }
            });
        }
    });
    (task, seen)
}

#[tokio::test]
async fn relays_methods_errors_and_job_events() {
    let d = Daemon::start();
    let socket = d.dir.path().join("system.sock");
    let mut c = d.client().await;

    // No service yet: system_unavailable, the rest of the API works.
    let e = call(&mut c, "system.info", json!({})).await.unwrap_err();
    assert_eq!(e["code"], ErrorCode::SystemUnavailable.code());
    assert_eq!(e["data"], json!({"reason": "down"}));
    call(&mut c, "ping", json!({})).await.unwrap();

    let mut launcher = d.client().await;
    call(
        &mut launcher,
        "subscribe",
        json!({"events": ["job_started", "job_finished"]}),
    )
    .await
    .unwrap();

    let (task, mut seen) = fake_system(&socket, system::VERSION);
    let info = eventually("the relay to connect", async || {
        call(&mut c, "system.info", json!({})).await.ok()
    })
    .await;
    assert_eq!(info["generation"], 3);
    // The relay subscribed to the job events, and forwards them.
    let (name, params) = next_event(&mut launcher).await;
    assert_eq!(name, "job_started");
    assert_eq!(params["job_id"], "0123456789abcdef");
    assert_eq!(
        params["subject"]["apps"][0]["name"], "Moonlight",
        "relayed unchanged"
    );

    // Errors come back unchanged (one 3xxx code space); params go through as given.
    let e = call(&mut c, "system.ssh.set", json!({"enabled": true}))
        .await
        .unwrap_err();
    assert_eq!(e["code"], ErrorCode::AuthenticationRequired.code());
    assert_eq!(e["data"], json!({"action": "org.hinteros.system.ssh"}));
    let mut methods = Vec::new();
    while let Ok((m, p)) = seen.try_recv() {
        methods.push((m, p));
    }
    assert!(methods.iter().any(|(m, _)| m == "subscribe"));
    assert!(methods.contains(&("ssh.set".to_string(), json!({"enabled": true}))));
    assert!(methods.iter().any(|(m, _)| m == "system.info"));

    // system.update.apply carries the admin PIN through, and pin_required back unchanged.
    let rev = "9a8b7c6d5e4f30211a2b3c4d5e6f708192a3b4c5";
    let e = call(&mut c, "system.update.apply", json!({"rev": rev}))
        .await
        .unwrap_err();
    assert_eq!(e["code"], ErrorCode::PinRequired.code());
    assert_eq!(e["data"], json!({"pin": "admin"}));
    let with_pin = json!({"rev": rev, "admin_pin": "2468"});
    let r = call(&mut c, "system.update.apply", with_pin.clone())
        .await
        .unwrap();
    assert_eq!(r["job_id"], "3f9a0c2b7e4d1a65");
    let mut applied = Vec::new();
    while let Ok((m, p)) = seen.try_recv() {
        if m == "update.apply" {
            applied.push(p);
        }
    }
    assert_eq!(applied, [json!({"rev": rev}), with_pin]);

    // A slow relayed call does not hold up the connection: the launcher can answer a password
    // prompt (auth.respond) on it while system.ssh.set waits. Responses come by id.
    let started = std::time::Instant::now();
    c.send_line(
        &jsonrpc::request(
            &hinteros_api::jsonrpc::Id::Number(900),
            "system.rebuild",
            json!({}),
        )
        .to_string(),
    )
    .await
    .unwrap();
    c.send_line(
        &jsonrpc::request(&hinteros_api::jsonrpc::Id::Number(901), "ping", json!({})).to_string(),
    )
    .await
    .unwrap();
    let first = c.read_raw().await.unwrap().unwrap();
    assert_eq!(first["id"], 901, "{first}");
    assert!(started.elapsed() < std::time::Duration::from_millis(700));
    let second = c.read_raw().await.unwrap().unwrap();
    assert_eq!(second["id"], 900);
    assert_eq!(second["result"], json!({"job_id": null}));

    // The service restarts speaking another version: version_mismatch.
    task.abort();
    let _ = task.await;
    let (task, _seen) = fake_system(&socket, ApiVersion::new(0, 1));
    let e = eventually("version_mismatch", async || {
        let e = call(&mut c, "system.info", json!({})).await.err()?;
        (e["data"]["reason"] == "version_mismatch").then_some(e)
    })
    .await;
    assert_eq!(e["code"], ErrorCode::SystemUnavailable.code());
    task.abort();
}
