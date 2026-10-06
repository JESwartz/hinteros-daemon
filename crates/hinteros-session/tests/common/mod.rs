//! Shared helpers for the black-box tests: run the real `hinteros-session` binary on a
//! temporary socket and talk to it over the wire, validating against the schema.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use hinteros_api::client::{Client, ClientError};
use hinteros_api::conformance::ApiSchema;
use hinteros_api::session;
use rustix::process::{Pid, Signal, kill_process};
use serde_json::{Value, json};
use tokio::time::timeout;

pub const TIMEOUT: Duration = Duration::from_secs(10);

pub struct Daemon {
    pub child: Child,
    pub socket: PathBuf,
    pub dir: tempfile::TempDir,
}

impl Daemon {
    pub fn start() -> Self {
        Self::start_with(|_| Vec::new())
    }

    /// Start with extra arguments, computed from the temp dir. gamescope and InputPlumber
    /// integration are off unless the arguments turn them on, and no user config is read.
    pub fn start_with(args: impl FnOnce(&Path) -> Vec<std::ffi::OsString>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let extra = args(dir.path());
        // A nested directory also exercises creating the socket's parent.
        let socket = dir.path().join("hinteros/session.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_hinteros-session"))
            .arg("--socket")
            .arg(&socket)
            .args(["--no-gamescope", "--no-inputplumber"])
            .args(extra)
            .env("XDG_CONFIG_HOME", dir.path())
            .env("RUST_LOG", "debug")
            .spawn()
            .expect("start hinteros-session");
        let daemon = Self { child, socket, dir };
        let deadline = Instant::now() + TIMEOUT;
        while std::os::unix::net::UnixStream::connect(&daemon.socket).is_err() {
            assert!(Instant::now() < deadline, "daemon did not start listening");
            std::thread::sleep(Duration::from_millis(20));
        }
        daemon
    }

    pub fn signal(&self, sig: Signal) {
        let pid = Pid::from_child(&self.child);
        kill_process(pid, sig).unwrap();
    }

    pub async fn connect(&self) -> Client {
        Client::connect(&self.socket).await.unwrap()
    }

    /// Connect and complete the handshake.
    pub async fn client(&self) -> Client {
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

pub fn rpc_error(r: Result<Value, ClientError>) -> Value {
    match r {
        Err(ClientError::Rpc(e)) => {
            let v = serde_json::to_value(&e).unwrap();
            ApiSchema::session().check_error(&v).unwrap();
            v
        }
        other => panic!("expected an RPC error, got {other:?}"),
    }
}

pub async fn assert_closed(c: &mut Client) {
    let next = timeout(TIMEOUT, c.read_raw()).await.unwrap().unwrap();
    assert_eq!(next, None, "server should have closed the connection");
}

/// The next event, validated against the schema.
pub async fn next_event(c: &mut Client) -> (String, Value) {
    let (name, params) = timeout(TIMEOUT, c.next_event_raw())
        .await
        .expect("timed out waiting for an event")
        .unwrap();
    ApiSchema::session()
        .check_event(&name, &params)
        .unwrap_or_else(|e| panic!("{name}: {e}"));
    (name, params)
}

/// Call a method; validate the result (or error) against the schema.
pub async fn call(c: &mut Client, method: &str, params: Value) -> Result<Value, Value> {
    let schema = ApiSchema::session();
    schema.check_params(method, &params).ok();
    match c.call_raw(method, params).await {
        Ok(result) => {
            schema
                .check_result(method, &result)
                .unwrap_or_else(|e| panic!("{method}: {e}"));
            Ok(result)
        }
        Err(ClientError::Rpc(e)) => {
            let v = serde_json::to_value(&e).unwrap();
            schema.check_error(&v).unwrap();
            Err(v)
        }
        Err(e) => panic!("{method}: {e:?}"),
    }
}

/// Poll `f` until it returns `Some`, or panic after [`TIMEOUT`].
pub async fn eventually<T>(what: &str, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Write an executable shell script.
pub fn script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}
