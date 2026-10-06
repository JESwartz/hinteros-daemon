//! The internal system API (`schema/system-api.json`): spoken between the session daemon and
//! the privileged system service.

use crate::common::{Hello, Ping};
use crate::{ApiInfo, ApiVersion, ErrorCode, MethodInfo};

pub const VERSION: ApiVersion = ApiVersion::new(0, 1);

/// The schema document this module implements.
pub const SCHEMA: &str = include_str!("../../../schema/system-api.json");

pub const DEFAULT_SOCKET: &str = "/run/hinteros/system.sock";

pub static API: ApiInfo = ApiInfo {
    api: "system",
    version: VERSION,
    methods: &[MethodInfo::of::<Hello>(), MethodInfo::of::<Ping>()],
    events: &[],
    errors: ErrorCode::ALL,
};
