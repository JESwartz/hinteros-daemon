//! Server-side protocol state shared by both binaries: request parsing, handshake gating and
//! typed dispatch. The binaries own the socket loop and the method implementations.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::path::Path;

use serde_json::{Map, Value};
use tokio::net::UnixListener;

use crate::common::{
    Hello, HelloParams, HelloResult, PING_NONCE_MAX, Peer, PingParams, PingResult,
};
use crate::jsonrpc::{self, Request};
use crate::{ApiVersion, ErrorCode, ErrorObject, Method};

/// What to do with one incoming line.
#[derive(Debug)]
pub enum Step {
    /// Send this message and keep the connection open.
    Reply(Value),
    /// Send this message, then close the connection.
    ReplyAndClose(Value),
    /// Nothing to send (the client sent a notification).
    Ignore,
    /// A request after a successful handshake, for the service to dispatch.
    Dispatch(Request),
}

/// Protocol state of one connection.
pub struct Connection {
    server_version: ApiVersion,
    server: Peer,
    negotiated: Option<(ApiVersion, Peer)>,
}

impl Connection {
    pub fn new(server_version: ApiVersion, server: Peer) -> Self {
        Self {
            server_version,
            server,
            negotiated: None,
        }
    }

    /// Negotiated version and client identity, once the handshake has succeeded.
    pub fn negotiated(&self) -> Option<&(ApiVersion, Peer)> {
        self.negotiated.as_ref()
    }

    pub fn step(&mut self, line: &str) -> Step {
        let req = match jsonrpc::parse_request(line) {
            Ok(req) => req,
            Err(reply) => return Step::Reply(reply),
        };
        let Some(id) = req.id.clone() else {
            return Step::Ignore;
        };
        if req.method == Hello::NAME {
            if self.negotiated.is_some() {
                let err = ErrorObject::new(ErrorCode::AlreadyHandshaken, "handshake already done");
                return Step::Reply(jsonrpc::error_response(Some(id), err));
            }
            let outcome = call::<Hello>(req.params, |p| self.hello(p));
            let close =
                matches!(&outcome, Err(e) if e.kind().is_some_and(ErrorCode::closes_connection));
            let reply = jsonrpc::response(&id, outcome);
            return if close {
                Step::ReplyAndClose(reply)
            } else {
                Step::Reply(reply)
            };
        }
        if self.negotiated.is_none() {
            let err = ErrorObject::new(
                ErrorCode::HandshakeRequired,
                format!("call 'hello' before '{}'", req.method),
            );
            return Step::Reply(jsonrpc::error_response(Some(id), err));
        }
        Step::Dispatch(req)
    }

    fn hello(&mut self, p: HelloParams) -> Result<HelloResult, ErrorObject> {
        let api_version = self
            .server_version
            .negotiate(p.api_version)
            .ok_or_else(|| ErrorObject::version_mismatch(p.api_version, self.server_version))?;
        self.negotiated = Some((api_version, p.client));
        Ok(HelloResult {
            api_version,
            server: self.server.clone(),
        })
    }
}

/// Bind a listening socket at `path` and chmod it to `mode`.
///
/// Creates a missing parent directory with `dir_mode`. A stale socket left by a crashed
/// instance is removed; a live one (something accepts connections) is an `AddrInUse` error,
/// and a non-socket file at `path` is never deleted.
pub fn bind_unix(path: &Path, mode: u32, dir_mode: u32) -> io::Result<UnixListener> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
        && !parent.exists()
    {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(dir_mode)
            .create(parent)?;
    }
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("{} is in use by a running instance", path.display()),
                ));
            }
            fs::remove_file(path)?;
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(listener)
}

/// Decode params (absent means `{}`), run `f`, encode the result.
pub fn call<M: Method>(
    params: Option<Value>,
    f: impl FnOnce(M::Params) -> Result<M::Result, ErrorObject>,
) -> Result<Value, ErrorObject> {
    let params = match params {
        None => Value::Object(Map::new()),
        Some(v @ Value::Object(_)) => v,
        Some(_) => {
            return Err(ErrorObject::new(
                ErrorCode::InvalidParams,
                "params must be an object",
            ));
        }
    };
    let params = serde_json::from_value(params)
        .map_err(|e| ErrorObject::new(ErrorCode::InvalidParams, format!("invalid params: {e}")))?;
    let result = f(params)?;
    serde_json::to_value(result)
        .map_err(|e| ErrorObject::new(ErrorCode::InternalError, format!("encoding result: {e}")))
}

pub fn method_not_found(method: &str) -> ErrorObject {
    ErrorObject::new(
        ErrorCode::MethodNotFound,
        format!("unknown method '{method}'"),
    )
}

/// The `ping` implementation shared by both services.
pub fn ping(p: PingParams) -> Result<PingResult, ErrorObject> {
    if p.nonce
        .as_ref()
        .is_some_and(|n| n.chars().count() > PING_NONCE_MAX)
    {
        return Err(ErrorObject::new(
            ErrorCode::InvalidParams,
            format!("nonce longer than {PING_NONCE_MAX} characters"),
        ));
    }
    Ok(PingResult { nonce: p.nonce })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        Connection::new(ApiVersion::new(0, 1), Peer::new("test", "0"))
    }

    fn reply(step: Step) -> Value {
        match step {
            Step::Reply(v) => v,
            other => panic!("expected Reply, got {other:?}"),
        }
    }

    #[test]
    fn handshake_gates_methods() {
        let mut c = conn();
        let r = reply(c.step(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#));
        assert_eq!(r["error"]["code"], 1001);
        let r = reply(c.step(
            r#"{"jsonrpc":"2.0","id":2,"method":"hello","params":{"api_version":"0.1","client":{"name":"t","version":"1"}}}"#,
        ));
        assert_eq!(r["result"]["api_version"], "0.1");
        assert!(matches!(
            c.step(r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#),
            Step::Dispatch(_)
        ));
        let r = reply(c.step(
            r#"{"jsonrpc":"2.0","id":4,"method":"hello","params":{"api_version":"0.1","client":{"name":"t","version":"1"}}}"#,
        ));
        assert_eq!(r["error"]["code"], 1003);
    }

    #[test]
    fn mismatch_closes() {
        let mut c = conn();
        let step = c.step(
            r#"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"api_version":"1.0","client":{"name":"t","version":"1"}}}"#,
        );
        let Step::ReplyAndClose(r) = step else {
            panic!("expected ReplyAndClose, got {step:?}")
        };
        assert_eq!(r["error"]["code"], 1002);
        assert_eq!(r["error"]["data"]["server_api_version"], "0.1");
        assert!(c.negotiated().is_none());
    }

    #[test]
    fn bad_params() {
        let mut c = conn();
        let r = reply(c.step(r#"{"jsonrpc":"2.0","id":1,"method":"hello","params":[1]}"#));
        assert_eq!(r["error"]["code"], -32602);
    }
}
