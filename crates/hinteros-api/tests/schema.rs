//! Conformance of the Rust API types with the language-neutral schema in `schema/`.
//!
//! The schema is the source of truth. These tests check that:
//! - each document is valid against the meta-schema (`api-doc.schema.json`);
//! - Rust and the schema declare the same version, methods, events and error codes;
//! - every example in the schema validates against its own payload schema, and survives a
//!   round trip through the Rust type unchanged (so Rust neither drops nor renames fields);
//! - schema enums and Rust enums have the same values.
//!
//! The daemon integration tests additionally validate live traffic against the schema.

use std::collections::{BTreeMap, BTreeSet};

use hinteros_api::conformance::ApiSchema;
use hinteros_api::session::{AppKind, EventName};
use hinteros_api::{ApiInfo, ApiVersion, ErrorCode, session, system};
use serde_json::Value;

const META_SCHEMA: &str = include_str!("../../../schema/api-doc.schema.json");

fn apis() -> [(ApiSchema, &'static ApiInfo); 2] {
    [
        (ApiSchema::session(), &session::API),
        (ApiSchema::system(), &system::API),
    ]
}

fn keys(v: &Value) -> BTreeSet<String> {
    v.as_object().unwrap().keys().cloned().collect()
}

#[test]
fn documents_match_meta_schema() {
    let meta: Value = serde_json::from_str(META_SCHEMA).unwrap();
    let validator = jsonschema::validator_for(&meta).expect("meta-schema compiles");
    for (schema, info) in apis() {
        let errors: Vec<String> = validator
            .iter_errors(schema.doc())
            .map(|e| format!("{e} at {}", e.instance_path()))
            .collect();
        assert!(errors.is_empty(), "{} API document: {errors:#?}", info.api);
    }
}

#[test]
fn same_surface() {
    for (schema, info) in apis() {
        let doc = schema.doc();
        assert_eq!(doc["api"], info.api);
        assert_eq!(
            doc["version"],
            info.version.to_string(),
            "{} version",
            info.api
        );

        let rust_methods: BTreeSet<String> = info.methods.iter().map(|m| m.name.into()).collect();
        assert_eq!(keys(&doc["methods"]), rust_methods, "{} methods", info.api);

        let rust_events: BTreeSet<String> = info.events.iter().map(|e| e.name.into()).collect();
        assert_eq!(keys(&doc["events"]), rust_events, "{} events", info.api);

        let schema_errors: BTreeMap<String, i64> = doc["errors"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v["code"].as_i64().unwrap()))
            .collect();
        let rust_errors: BTreeMap<String, i64> = info
            .errors
            .iter()
            .map(|e| (e.name().into(), e.code()))
            .collect();
        assert_eq!(schema_errors, rust_errors, "{} errors", info.api);
        for e in info.errors {
            let closes = doc["errors"][e.name()]["closes_connection"] == true;
            assert_eq!(
                closes,
                e.closes_connection(),
                "{} closes_connection",
                e.name()
            );
        }
    }
}

#[test]
fn handshake_and_since() {
    for (schema, info) in apis() {
        let doc = schema.doc();
        let handshake: Vec<&String> = doc["methods"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, m)| m["handshake"] == true)
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            handshake,
            ["hello"],
            "{}: exactly 'hello' is the handshake",
            info.api
        );

        let sections = ["methods", "events", "errors"];
        for section in sections {
            for (name, item) in doc[section].as_object().unwrap() {
                if let Some(since) = item["since"].as_str() {
                    let since: ApiVersion = since.parse().unwrap();
                    assert!(
                        since <= info.version,
                        "{section}.{name}: since {since} is in the future"
                    );
                }
            }
        }
    }
}

#[test]
fn method_examples_conform_and_round_trip() {
    for (schema, info) in apis() {
        for m in info.methods {
            let decl = &schema.doc()["methods"][m.name];
            let allowed_errors: BTreeSet<i64> = decl["errors"]
                .as_array()
                .unwrap()
                .iter()
                .map(|name| {
                    let name = name.as_str().unwrap();
                    schema.doc()["errors"][name]["code"]
                        .as_i64()
                        .unwrap_or_else(|| panic!("{}: unknown error {name}", m.name))
                })
                .collect();
            for ex in decl["examples"].as_array().unwrap() {
                let label = format!("{} {} example {:?}", info.api, m.name, ex["name"]);
                let params = &ex["params"];
                schema
                    .check_params(m.name, params)
                    .unwrap_or_else(|e| panic!("{label}: {e}"));
                let back = (m.roundtrip_params)(params.clone())
                    .unwrap_or_else(|e| panic!("{label}: Rust params type rejects it: {e}"));
                assert_eq!(&back, params, "{label}: params changed in Rust round trip");

                if let Some(result) = ex.get("result") {
                    schema
                        .check_result(m.name, result)
                        .unwrap_or_else(|e| panic!("{label}: {e}"));
                    let back = (m.roundtrip_result)(result.clone())
                        .unwrap_or_else(|e| panic!("{label}: Rust result type rejects it: {e}"));
                    assert_eq!(&back, result, "{label}: result changed in Rust round trip");
                }
                if let Some(error) = ex.get("error") {
                    schema
                        .check_error(error)
                        .unwrap_or_else(|e| panic!("{label}: {e}"));
                    let code = error["code"].as_i64().unwrap();
                    let generic = ErrorCode::from_code(code).is_some_and(|c| {
                        !matches!(c, ErrorCode::VersionMismatch | ErrorCode::AlreadyHandshaken)
                    });
                    assert!(
                        generic || allowed_errors.contains(&code),
                        "{label}: error {code} is not listed in the method's errors"
                    );
                }
            }
        }
    }
}

#[test]
fn event_examples_conform_and_round_trip() {
    for (schema, info) in apis() {
        for ev in info.events {
            for ex in schema.doc()["events"][ev.name]["examples"]
                .as_array()
                .unwrap()
            {
                let label = format!("{} event {} example {:?}", info.api, ev.name, ex["name"]);
                let params = &ex["params"];
                schema
                    .check_event(ev.name, params)
                    .unwrap_or_else(|e| panic!("{label}: {e}"));
                let back = (ev.roundtrip_params)(params.clone())
                    .unwrap_or_else(|e| panic!("{label}: Rust type rejects it: {e}"));
                assert_eq!(&back, params, "{label}: changed in Rust round trip");
            }
        }
    }
}

fn schema_enum(schema: &ApiSchema, def: &str) -> BTreeSet<String> {
    schema.doc()["$defs"][def]["enum"]
        .as_array()
        .unwrap_or_else(|| panic!("$defs.{def} has no enum"))
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

fn rust_enum<T: serde::Serialize>(values: &[T]) -> BTreeSet<String> {
    values
        .iter()
        .map(|v| {
            serde_json::to_value(v)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

#[test]
fn enums_match() {
    let schema = ApiSchema::session();
    assert_eq!(schema_enum(&schema, "EventName"), rust_enum(EventName::ALL));
    assert_eq!(schema_enum(&schema, "AppKind"), rust_enum(AppKind::ALL));
    // Every event is subscribable and vice versa.
    let events: BTreeSet<String> = session::API.events.iter().map(|e| e.name.into()).collect();
    assert_eq!(events, rust_enum(EventName::ALL));
}

#[test]
fn schema_rejects_what_it_should() {
    // Guards against a schema so loose that the checks above prove nothing.
    let schema = ApiSchema::session();
    let bad = [
        ("/methods/hello/params", r#"{"api_version":"0.1"}"#),
        (
            "/methods/hello/params",
            r#"{"api_version":"v1","client":{"name":"a","version":"1"}}"#,
        ),
        ("/methods/ping/result", r#"{"nonce":"x","extra":1}"#),
        (
            "/methods/apps.list/result",
            r#"{"apps":[{"id":"Bad Id","name":"x","kind":"native"}]}"#,
        ),
        ("/methods/subscribe/params", r#"{"events":["nope"]}"#),
    ];
    for (pointer, instance) in bad {
        let v: Value = serde_json::from_str(instance).unwrap();
        assert!(
            schema.check(pointer, &v).is_err(),
            "{pointer} accepted {instance}"
        );
    }
    assert!(
        schema
            .check_error(&serde_json::json!({"code": 1002, "message": "m"}))
            .is_err()
    );
    assert!(
        schema
            .check_error(&serde_json::json!({"code": 7, "message": "m"}))
            .is_err()
    );
}
