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
    AppNotFound,
    AppLaunchFailed,
    SteamNoWindows,
    KeyboardUnavailable,
    OskUnavailable,
    SystemUnavailable,
    PermissionDenied,
    JobFailed,
    InsufficientResources,
    JobBusy,
    RateLimited,
    StateConflict,
    UpdateUnreachable,
    UpdateVerificationFailed,
    NotFound,
    NeedsAcknowledgement,
    AuthenticationRequired,
    AppConflict,
    TermsRequired,
    /// 0.4: `profiles.create` with 8 profiles.
    ProfileLimit,
    /// 0.4: a PIN is missing or wrong.
    PinRequired,
    Unavailable,
    DeviceFailed,
    WrongPassword,
    NotAuthorized,
    PowerInhibited,
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
        Self::AppNotFound,
        Self::AppLaunchFailed,
        Self::SteamNoWindows,
        Self::KeyboardUnavailable,
        Self::OskUnavailable,
        Self::SystemUnavailable,
        Self::PermissionDenied,
        Self::JobFailed,
        Self::InsufficientResources,
        Self::JobBusy,
        Self::RateLimited,
        Self::StateConflict,
        Self::UpdateUnreachable,
        Self::UpdateVerificationFailed,
        Self::NotFound,
        Self::NeedsAcknowledgement,
        Self::AuthenticationRequired,
        Self::AppConflict,
        Self::TermsRequired,
        Self::ProfileLimit,
        Self::PinRequired,
        Self::Unavailable,
        Self::DeviceFailed,
        Self::WrongPassword,
        Self::NotAuthorized,
        Self::PowerInhibited,
    ];

    /// The JSON-RPC and protocol errors, which every API declares.
    pub const GENERIC: &[ErrorCode] = &[
        Self::ParseError,
        Self::InvalidRequest,
        Self::MethodNotFound,
        Self::InvalidParams,
        Self::InternalError,
        Self::HandshakeRequired,
        Self::VersionMismatch,
        Self::AlreadyHandshaken,
    ];

    /// What the system API declares: the generic errors and the system domain (3xxx), except
    /// the session-side `system_unavailable`.
    pub const SYSTEM: &[ErrorCode] = &[
        Self::ParseError,
        Self::InvalidRequest,
        Self::MethodNotFound,
        Self::InvalidParams,
        Self::InternalError,
        Self::HandshakeRequired,
        Self::VersionMismatch,
        Self::AlreadyHandshaken,
        Self::PermissionDenied,
        Self::JobFailed,
        Self::InsufficientResources,
        Self::JobBusy,
        Self::RateLimited,
        Self::StateConflict,
        Self::UpdateUnreachable,
        Self::UpdateVerificationFailed,
        Self::NotFound,
        Self::NeedsAcknowledgement,
        Self::AuthenticationRequired,
        Self::AppConflict,
        Self::TermsRequired,
        Self::ProfileLimit,
        Self::PinRequired,
        Self::Unavailable,
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
            Self::AppNotFound => 2001,
            Self::AppLaunchFailed => 2002,
            Self::SteamNoWindows => 2003,
            Self::KeyboardUnavailable => 2004,
            Self::OskUnavailable => 2005,
            Self::SystemUnavailable => 3001,
            Self::PermissionDenied => 3002,
            Self::JobFailed => 3003,
            Self::InsufficientResources => 3004,
            Self::JobBusy => 3005,
            Self::RateLimited => 3006,
            Self::StateConflict => 3007,
            Self::UpdateUnreachable => 3008,
            Self::UpdateVerificationFailed => 3009,
            Self::NotFound => 3010,
            Self::NeedsAcknowledgement => 3011,
            Self::AuthenticationRequired => 3012,
            Self::AppConflict => 3013,
            Self::TermsRequired => 3014,
            Self::ProfileLimit => 3015,
            Self::PinRequired => 3016,
            Self::Unavailable => 4001,
            Self::DeviceFailed => 4002,
            Self::WrongPassword => 4003,
            Self::NotAuthorized => 4004,
            Self::PowerInhibited => 4005,
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
            Self::AppNotFound => "app_not_found",
            Self::AppLaunchFailed => "app_launch_failed",
            Self::SteamNoWindows => "steam_no_windows",
            Self::KeyboardUnavailable => "keyboard_unavailable",
            Self::OskUnavailable => "osk_unavailable",
            Self::SystemUnavailable => "system_unavailable",
            Self::PermissionDenied => "permission_denied",
            Self::JobFailed => "job_failed",
            Self::InsufficientResources => "insufficient_resources",
            Self::JobBusy => "job_busy",
            Self::RateLimited => "rate_limited",
            Self::StateConflict => "state_conflict",
            Self::UpdateUnreachable => "update_unreachable",
            Self::UpdateVerificationFailed => "update_verification_failed",
            Self::NotFound => "not_found",
            Self::NeedsAcknowledgement => "needs_acknowledgement",
            Self::AuthenticationRequired => "authentication_required",
            Self::AppConflict => "app_conflict",
            Self::TermsRequired => "terms_required",
            Self::ProfileLimit => "profile_limit",
            Self::PinRequired => "pin_required",
            Self::Unavailable => "unavailable",
            Self::DeviceFailed => "device_failed",
            Self::WrongPassword => "wrong_password",
            Self::NotAuthorized => "not_authorized",
            Self::PowerInhibited => "power_inhibited",
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

    /// An error with a `data` payload.
    pub fn with_data(code: ErrorCode, message: impl Into<String>, data: Value) -> Self {
        Self {
            data: Some(data),
            ..Self::new(code, message)
        }
    }

    /// `invalid_params` naming the offending field and why (system API and relay).
    pub fn invalid_field(field: &str, reason: &str, message: impl Into<String>) -> Self {
        Self::with_data(
            ErrorCode::InvalidParams,
            message,
            serde_json::json!({ "field": field, "reason": reason }),
        )
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
