//! Validation of JSON values against the schema documents (feature `conformance`, tests only).

use serde_json::{Value, json};

use crate::{session, system};

/// One parsed API document (`schema/<api>-api.json`).
pub struct ApiSchema {
    doc: Value,
}

impl ApiSchema {
    pub fn session() -> Self {
        Self::parse(session::SCHEMA)
    }

    pub fn system() -> Self {
        Self::parse(system::SCHEMA)
    }

    pub fn parse(text: &str) -> Self {
        Self {
            doc: serde_json::from_str(text).expect("schema document is valid JSON"),
        }
    }

    pub fn doc(&self) -> &Value {
        &self.doc
    }

    /// Validate `instance` against the payload schema at JSON pointer `pointer` in the document
    /// (e.g. `/methods/ping/result`). `#/$defs/...` references resolve against the document.
    pub fn check(&self, pointer: &str, instance: &Value) -> Result<(), String> {
        let schema = self
            .doc
            .pointer(pointer)
            .ok_or_else(|| format!("no schema at {pointer}"))?;
        let wrapper = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": self.doc["$defs"],
            "allOf": [schema],
        });
        let validator = jsonschema::validator_for(&wrapper)
            .map_err(|e| format!("schema at {pointer} does not compile: {e}"))?;
        let errors: Vec<String> = validator
            .iter_errors(instance)
            .map(|e| format!("{} at {}", e, e.instance_path()))
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "{pointer}: {instance} is invalid: {}",
                errors.join("; ")
            ))
        }
    }

    pub fn check_params(&self, method: &str, params: &Value) -> Result<(), String> {
        self.check(&format!("/methods/{}/params", escape(method)), params)
    }

    pub fn check_result(&self, method: &str, result: &Value) -> Result<(), String> {
        self.check(&format!("/methods/{}/result", escape(method)), result)
    }

    pub fn check_event(&self, event: &str, params: &Value) -> Result<(), String> {
        self.check(&format!("/events/{}/params", escape(event)), params)
    }

    /// Validate a JSON-RPC error object: its code must be declared, and its `data` must match
    /// that error's data schema (or be absent if it has none).
    pub fn check_error(&self, error: &Value) -> Result<(), String> {
        let code = error["code"]
            .as_i64()
            .ok_or_else(|| format!("error without integer code: {error}"))?;
        let errors = self.doc["errors"].as_object().expect("errors object");
        let (name, decl) = errors
            .iter()
            .find(|(_, d)| d["code"].as_i64() == Some(code))
            .ok_or_else(|| format!("error code {code} is not declared in the schema"))?;
        if !error["message"].is_string() {
            return Err(format!("error without message: {error}"));
        }
        match (decl.get("data"), error.get("data")) {
            (Some(_), Some(data)) => self.check(&format!("/errors/{name}/data"), data),
            (Some(_), None) if decl["data_optional"] == true => Ok(()),
            (Some(_), None) => Err(format!("error {name} requires data")),
            (None, Some(data)) => Err(format!("error {name} declares no data, got {data}")),
            (None, None) => Ok(()),
        }
    }

    /// Validate a whole response or notification sent by the server for `method`.
    pub fn check_server_message(&self, method: &str, message: &Value) -> Result<(), String> {
        if message["jsonrpc"] != "2.0" {
            return Err(format!("not JSON-RPC 2.0: {message}"));
        }
        if let Some(result) = message.get("result") {
            self.check_result(method, result)
        } else if let Some(error) = message.get("error") {
            self.check_error(error)
        } else {
            self.check_event(method, &message["params"])
        }
    }
}

/// Escape a key for use in a JSON pointer.
fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}
