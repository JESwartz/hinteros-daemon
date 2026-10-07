//! HinterOS socket API.
//!
//! The API is described by the language-neutral documents in `schema/` (the source of truth).
//! This crate holds the matching Rust types; `tests/schema.rs` checks them against the schema
//! and the daemon integration tests validate live traffic against it.
//!
//! Protocol: JSON-RPC 2.0, one JSON object per line (NDJSON) over a Unix stream socket. The first
//! request on a connection must be `hello` (version handshake). See `hinteros-docs/design/api.md`.

pub mod catalog;
pub mod client;
pub mod codec;
pub mod common;
#[cfg(feature = "conformance")]
pub mod conformance;
pub mod error;
pub mod jsonrpc;
pub mod runtime;
pub mod server;
pub mod session;
pub mod system;
pub mod version;

pub use error::{ErrorCode, ErrorObject};
pub use version::ApiVersion;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// A request/response method of an API.
pub trait Method {
    const NAME: &'static str;
    type Params: Serialize + DeserializeOwned + Send;
    type Result: Serialize + DeserializeOwned + Send;
}

/// A server-pushed event (a JSON-RPC notification from server to client).
pub trait Event {
    const NAME: &'static str;
    type Params: Serialize + DeserializeOwned + Send;
}

/// Static description of one API as implemented in Rust, compared against the schema in tests.
pub struct ApiInfo {
    pub api: &'static str,
    pub version: ApiVersion,
    pub methods: &'static [MethodInfo],
    pub events: &'static [EventInfo],
    pub errors: &'static [ErrorCode],
}

type Roundtrip = fn(Value) -> serde_json::Result<Value>;

pub struct MethodInfo {
    pub name: &'static str,
    /// Deserialize a params value into the Rust type and serialize it back.
    pub roundtrip_params: Roundtrip,
    /// Same for the result type.
    pub roundtrip_result: Roundtrip,
}

pub struct EventInfo {
    pub name: &'static str,
    pub roundtrip_params: Roundtrip,
}

fn roundtrip<T: Serialize + DeserializeOwned>(value: Value) -> serde_json::Result<Value> {
    serde_json::to_value(serde_json::from_value::<T>(value)?)
}

impl MethodInfo {
    pub const fn of<M: Method>() -> Self {
        Self {
            name: M::NAME,
            roundtrip_params: roundtrip::<M::Params>,
            roundtrip_result: roundtrip::<M::Result>,
        }
    }
}

impl EventInfo {
    pub const fn of<E: Event>() -> Self {
        Self {
            name: E::NAME,
            roundtrip_params: roundtrip::<E::Params>,
        }
    }
}
