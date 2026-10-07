//! Types shared by the session and system APIs: the handshake and ping.

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
