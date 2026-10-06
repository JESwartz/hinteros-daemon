//! Error codes (the `errors` section of the schema) and the JSON-RPC error object.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ApiVersion;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    ParseError,
    InvalidRequest,
    MethodNotFound,
    InvalidParams,
    InternalError,
    HandshakeRequired,
    VersionMismatch,
    AlreadyHandshaken,
}

impl ErrorCode {
    pub const ALL: &[ErrorCode] = &[
        Self::ParseError,
        Self::InvalidRequest,
        Self::MethodNotFound,
        Self::InvalidParams,
        Self::InternalError,
        Self::HandshakeRequired,
        Self::VersionMismatch,
        Self::AlreadyHandshaken,
    ];

    pub const fn code(self) -> i64 {
        match self {
            Self::ParseError => -32700,
            Self::InvalidRequest => -32600,
            Self::MethodNotFound => -32601,
            Self::InvalidParams => -32602,
            Self::InternalError => -32603,
            Self::HandshakeRequired => 1001,
            Self::VersionMismatch => 1002,
            Self::AlreadyHandshaken => 1003,
        }
    }

    /// The error's key in the schema's `errors` object.
    pub const fn name(self) -> &'static str {
        match self {
            Self::ParseError => "parse_error",
            Self::InvalidRequest => "invalid_request",
            Self::MethodNotFound => "method_not_found",
            Self::InvalidParams => "invalid_params",
            Self::InternalError => "internal_error",
            Self::HandshakeRequired => "handshake_required",
            Self::VersionMismatch => "version_mismatch",
            Self::AlreadyHandshaken => "already_handshaken",
        }
    }

    /// Whether the server closes the connection after sending this error.
    pub const fn closes_connection(self) -> bool {
        matches!(self, Self::VersionMismatch)
    }

    pub fn from_code(code: i64) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.code() == code)
    }
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{message} (code {code})")]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorObject {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.code(),
            message: message.into(),
            data: None,
        }
    }

    pub fn kind(&self) -> Option<ErrorCode> {
        ErrorCode::from_code(self.code)
    }

    pub fn version_mismatch(client: ApiVersion, server: ApiVersion) -> Self {
        Self {
            data: Some(serde_json::json!({
                "client_api_version": client,
                "server_api_version": server,
            })),
            ..Self::new(
                ErrorCode::VersionMismatch,
                format!("API version mismatch: client {client}, server {server}"),
            )
        }
    }
}
