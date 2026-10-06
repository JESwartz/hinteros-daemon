//! Black-box tests: run the real `hinteros-session` binary on a temporary socket, talk to it over
//! the wire and validate every message it sends against `schema/session-api.json`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use hinteros_api::client::{Client, ClientError};
use hinteros_api::common::{Peer, Ping, PingParams};
use hinteros_api::conformance::ApiSchema;
use hinteros_api::session::{self, AppsList, AppsListParams, EventName, HomePressed};
use hinteros_api::{ApiVersion, ErrorCode};
use rustix::process::{Pid, Signal, kill_process};
use serde_json::{Value, json};
use tokio::time::timeout;

const TIMEOUT: Duration = Duration::from_secs(10);

struct Daemon {
    child: Child,
    socket: PathBuf,
    _dir: tempfile::TempDir,
}

impl Daemon {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        // A nested directory also exercises creating the socket's parent.
        let socket = dir.path().join("hinteros/session.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_hinteros-session"))
            .arg("--socket")
            .arg(&socket)
            .env("RUST_LOG", "debug")
            .spawn()
            .expect("start hinteros-session");
        let daemon = Self {
            child,
            socket,
            _dir: dir,
        };
        let deadline = Instant::now() + TIMEOUT;
        while std::os::unix::net::UnixStream::connect(&daemon.socket).is_err() {
            assert!(Instant::now() < deadline, "daemon did not start listening");
            std::thread::sleep(Duration::from_millis(20));
        }
        daemon
    }

    fn signal(&self, sig: Signal) {
        let pid = Pid::from_child(&self.child);
        kill_process(pid, sig).unwrap();
    }

    async fn connect(&self) -> Client {
        Client::connect(&self.socket).await.unwrap()
    }

    /// Connect and complete the handshake.
    async fn client(&self) -> Client {
        let mut c = self.connect().await;
        let schema = ApiSchema::session();
        let params = json!({
            "api_version": session::VERSION,
            "client": { "name": "test", "version": "0" },
        });
        schema.check_params("hello", &params).unwrap();
        let result = c.call_raw("hello", params).await.unwrap();
        schema.check_result("hello", &result).unwrap();
        assert_eq!(result["api_version"], session::VERSION.to_string());
        assert_eq!(result["server"]["name"], "hinteros-session");
        c
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn rpc_error(r: Result<Value, ClientError>) -> Value {
    match r {
        Err(ClientError::Rpc(e)) => {
            let v = serde_json::to_value(&e).unwrap();
            ApiSchema::session().check_error(&v).unwrap();
            v
        }
        other => panic!("expected an RPC error, got {other:?}"),
    }
}

async fn assert_closed(c: &mut Client) {
    let next = timeout(TIMEOUT, c.read_raw()).await.unwrap().unwrap();
    assert_eq!(next, None, "server should have closed the connection");
}

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
        ApiVersion::new(0, 1),
        ApiVersion::new(0, 3),
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
