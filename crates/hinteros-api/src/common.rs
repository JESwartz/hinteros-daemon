//! Types shared by the session and system APIs: the handshake and ping.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{ApiVersion, Method};

/// Name and build version of one side of a connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    pub name: String,
    pub version: String,
}

impl Peer {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
        }
    }
}

/// `hello`'s params. Closed like every params object: unknown fields (here or in `client`) are
/// `invalid_params`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "StrictHello")]
pub struct HelloParams {
    pub api_version: ApiVersion,
    pub client: Peer,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictPeer {
    name: String,
    version: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictHello {
    api_version: ApiVersion,
    client: StrictPeer,
}

impl From<StrictHello> for HelloParams {
    fn from(h: StrictHello) -> Self {
        Self {
            api_version: h.api_version,
            client: Peer {
                name: h.client.name,
                version: h.client.version,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloResult {
    /// The negotiated version used for the rest of the connection.
    pub api_version: ApiVersion,
    pub server: Peer,
}

pub struct Hello;
impl Method for Hello {
    const NAME: &'static str = "hello";
    type Params = HelloParams;
    type Result = HelloResult;
}

pub const PING_NONCE_MAX: usize = 256;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PingParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PingResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
}

pub struct Ping;
impl Method for Ping {
    const NAME: &'static str = "ping";
    type Params = PingParams;
    type Result = PingResult;
}

// ---------------------------------------------------------------------------------------------
// Persisted JSON files

/// Every JSON file the daemons keep has a top-level `version`, and one load policy: unknown
/// fields are ignored. A file with a newer version is not understood: the reader uses defaults
/// (or refuses, when the file must not be lost) and nothing overwrites it, except a cache.
/// `Err` when `bytes`, the contents of `path`, have a version above `known`.
pub fn check_version(path: &Path, bytes: &[u8], known: u32) -> std::io::Result<()> {
    #[derive(Deserialize)]
    struct Header {
        version: u64,
    }
    match serde_json::from_slice::<Header>(bytes) {
        Ok(h) if h.version > u64::from(known) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{} has version {}, newer than the {known} this daemon reads",
                path.display(),
                h.version
            ),
        )),
        _ => Ok(()),
    }
}

/// [`check_version`] on the file at `path`, before writing it (missing or unreadable: fine).
pub fn check_file_version(path: &Path, known: u32) -> std::io::Result<()> {
    match std::fs::read(path) {
        Ok(bytes) => check_version(path, &bytes, known),
        Err(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_newer_version_is_refused() {
        let p = Path::new("/x.json");
        for ok in [
            r#"{"version":1}"#,
            r#"{"version":2,"a":[]}"#,
            r#"{"a":1}"#,
            "not json",
            "[]",
        ] {
            check_version(p, ok.as_bytes(), 2).unwrap();
        }
        let e = check_version(p, br#"{"a":1,"version":3}"#, 2).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
        assert!(e.to_string().contains("/x.json has version 3"), "{e}");
    }

    #[test]
    fn a_missing_file_is_not_newer() {
        let p = std::env::temp_dir().join(format!("hinteros-version-{}.json", std::process::id()));
        check_file_version(&p, 1).unwrap();
        std::fs::write(&p, r#"{"version":2}"#).unwrap();
        let refused = check_file_version(&p, 1).is_err();
        std::fs::remove_file(&p).unwrap();
        assert!(refused);
    }
}
