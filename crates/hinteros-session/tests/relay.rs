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
                                "ssh.set" => Err(hinteros_api::ErrorObject::with_data(
                                    ErrorCode::AuthenticationRequired,
                                    "needs the admin password",
                                    json!({"action": "org.hinteros.system.ssh"}),
                                )),
                                m => Err(hinteros_api::server::method_not_found(m)),
                            };
                            write_message(&mut w, &jsonrpc::response(&id, outcome)).await.unwrap();
                            if req.method == "subscribe" {
                                let ev = jsonrpc::notification(
                                    "job_started",
                                    json!({"job_id": "0123456789abcdef", "kind": "rebuild", "uid": 1000}),
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
