//! Run the real `hinteros-system` binary (unprivileged, on a temporary socket) and check the
//! handshake and `ping` against `schema/system-api.json`.

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use hinteros_api::client::{Client, ClientError};
use hinteros_api::common::Peer;
use hinteros_api::conformance::ApiSchema;
use hinteros_api::{ApiVersion, ErrorCode, system};
use serde_json::json;

#[tokio::test]
async fn handshake_and_ping() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("system.sock");
    let mut child = Command::new(env!("CARGO_BIN_EXE_hinteros-system"))
        .arg("--socket")
        .arg(&socket)
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
    // SO_PEERCRED: the caller (this test process) is logged.
    let me = std::process::id();
    assert!(log.contains(&format!("pid=Some({me})")), "log: {log}");
}
