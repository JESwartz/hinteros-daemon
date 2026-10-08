//! greetd's IPC (`$GREETD_SOCK`): a 4-byte native-endian length, then JSON. Requests
//! `create_session {username}`, `post_auth_message_response {response}`, `start_session {cmd,
//! env}`, `cancel_session`; responses `success`, `error {error_type, description}`,
//! `auth_message {auth_message_type: visible|secret|info|error, auth_message}`.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    CreateSession { username: String },
    PostAuthMessageResponse { response: Option<String> },
    StartSession { cmd: Vec<String>, env: Vec<String> },
    CancelSession,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMessageType {
    Visible,
    Secret,
    Info,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Success,
    Error {
        error_type: String,
        description: String,
    },
    AuthMessage {
        auth_message_type: AuthMessageType,
        auth_message: String,
    },
}

/// A blocking greetd connection.
pub struct Greetd {
    stream: UnixStream,
}

impl Greetd {
    pub fn connect(path: &Path) -> std::io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(120)))?;
        Ok(Self { stream })
    }

    /// From `$GREETD_SOCK`.
    pub fn from_env() -> std::io::Result<Self> {
        let path = std::env::var_os("GREETD_SOCK").ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "GREETD_SOCK is not set")
        })?;
        Self::connect(Path::new(&path))
    }

    pub fn request(&mut self, r: &Request) -> std::io::Result<Response> {
        // The PIN may be in it: keep the bytes in a wiped buffer.
        let body = Zeroizing::new(serde_json::to_vec(r)?);
        let len = u32::try_from(body.len()).map_err(std::io::Error::other)?;
        self.stream.write_all(&len.to_ne_bytes())?;
        self.stream.write_all(&body)?;
        let mut len = [0u8; 4];
        self.stream.read_exact(&mut len)?;
        let len = u32::from_ne_bytes(len) as usize;
        if len > 1 << 16 {
            return Err(std::io::Error::other("greetd sent an oversized message"));
        }
        let mut buf = vec![0u8; len];
        self.stream.read_exact(&mut buf)?;
        serde_json::from_slice(&buf).map_err(std::io::Error::other)
    }
}

/// `hinteros:locked:<seconds>` from the PAM helper's info message.
pub fn locked_seconds(message: &str) -> Option<u32> {
    message
        .lines()
        .find_map(|l| l.trim().strip_prefix("hinteros:locked:"))
        .and_then(|s| s.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format() {
        let v = serde_json::to_value(Request::CreateSession {
            username: "hinteros".into(),
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"type": "create_session", "username": "hinteros"})
        );
        let v = serde_json::to_value(Request::PostAuthMessageResponse { response: None }).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"type": "post_auth_message_response", "response": null})
        );
        let r: Response = serde_json::from_str(
            r#"{"type":"auth_message","auth_message_type":"secret","auth_message":"Password:"}"#,
        )
        .unwrap();
        assert_eq!(
            r,
            Response::AuthMessage {
                auth_message_type: AuthMessageType::Secret,
                auth_message: "Password:".into()
            }
        );
        assert_eq!(locked_seconds("hinteros:locked:30"), Some(30));
        assert_eq!(locked_seconds("x\nhinteros:locked:4\n"), Some(4));
        assert_eq!(locked_seconds("hinteros:wrong"), None);
    }
}
