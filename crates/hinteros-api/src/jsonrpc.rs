//! JSON-RPC 2.0 envelopes.
//!
//! Requests carry an `id` (integer or string). Messages without `id` are notifications: the
//! server sends events that way, and ignores notifications from clients. Batches are rejected.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{ErrorCode, ErrorObject};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Number(i64),
    String(String),
}

/// A request from a client. `id == None` means it is a notification.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: Option<Id>,
    pub method: String,
    pub params: Option<Value>,
}

/// Parse one line into a request, or the error response to send instead.
pub fn parse_request(line: &str) -> Result<Request, Value> {
    let value: Value = serde_json::from_str(line).map_err(|e| {
        error_response(
            None,
            ErrorObject::new(ErrorCode::ParseError, format!("parse error: {e}")),
        )
    })?;
    let invalid = |id: Option<Id>, msg: &str| {
        error_response(id, ErrorObject::new(ErrorCode::InvalidRequest, msg))
    };
    let mut obj = match value {
        Value::Object(obj) => obj,
        Value::Array(_) => return Err(invalid(None, "batches are not supported")),
        _ => return Err(invalid(None, "a request must be a JSON object")),
    };
    let id = match obj.remove("id") {
        None => None,
        Some(v) => match serde_json::from_value::<Id>(v) {
            Ok(id) => Some(id),
            Err(_) => return Err(invalid(None, "id must be an integer or a string")),
        },
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(invalid(id, "jsonrpc must be \"2.0\""));
    }
    let Some(Value::String(method)) = obj.remove("method") else {
        return Err(invalid(id, "method must be a string"));
    };
    Ok(Request {
        id,
        method,
        params: obj.remove("params"),
    })
}

pub fn request(id: &Id, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

pub fn result_response(id: &Id, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// `id` is `None` (serialized as `null`) when the request's id could not be determined.
pub fn error_response(id: Option<Id>, error: ErrorObject) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}

pub fn response(id: &Id, outcome: Result<Value, ErrorObject>) -> Value {
    match outcome {
        Ok(result) => result_response(id, result),
        Err(error) => error_response(Some(id.clone()), error),
    }
}

pub fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

/// A message received by a client.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    Response {
        id: Option<Id>,
        outcome: Result<Value, ErrorObject>,
    },
    Notification {
        method: String,
        params: Value,
    },
}

/// Parse a message sent by a server.
pub fn parse_server_message(value: Value) -> Result<ServerMessage, String> {
    let Value::Object(mut obj) = value else {
        return Err("message is not an object".into());
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err("jsonrpc must be \"2.0\"".into());
    }
    if let Some(Value::String(method)) = obj.remove("method") {
        let params = obj
            .remove("params")
            .unwrap_or_else(|| Value::Object(Map::new()));
        return Ok(ServerMessage::Notification { method, params });
    }
    let id = match obj.remove("id") {
        None | Some(Value::Null) => None,
        Some(v) => Some(serde_json::from_value(v).map_err(|e| format!("bad id: {e}"))?),
    };
    let outcome = match (obj.remove("result"), obj.remove("error")) {
        (Some(result), None) => Ok(result),
        (None, Some(error)) => {
            Err(serde_json::from_value(error).map_err(|e| format!("bad error object: {e}"))?)
        }
        _ => return Err("response needs exactly one of result and error".into()),
    };
    Ok(ServerMessage::Response { id, outcome })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(v: &Value) -> i64 {
        v["error"]["code"].as_i64().unwrap()
    }

    #[test]
    fn parses_requests() {
        let r = parse_request(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).unwrap();
        assert_eq!(r.id, Some(Id::Number(1)));
        assert_eq!(r.method, "ping");
        assert_eq!(r.params, None);
        let n = parse_request(r#"{"jsonrpc":"2.0","method":"x","params":{}}"#).unwrap();
        assert_eq!(n.id, None);
    }

    #[test]
    fn rejects_bad_requests() {
        assert_eq!(code(&parse_request("{").unwrap_err()), -32700);
        assert_eq!(code(&parse_request("[]").unwrap_err()), -32600);
        assert_eq!(
            code(&parse_request(r#"{"id":1,"method":"x"}"#).unwrap_err()),
            -32600
        );
        assert_eq!(
            code(&parse_request(r#"{"jsonrpc":"2.0","id":1.5,"method":"x"}"#).unwrap_err()),
            -32600
        );
        let e = parse_request(r#"{"jsonrpc":"2.0","id":"a","method":3}"#).unwrap_err();
        assert_eq!(e["id"], "a");
    }
}
