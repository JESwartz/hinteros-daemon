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
use hinteros_api::session::{
    AppKind, AppSource, ControllerButton, EventName, ExitReason, InstallState, Key, KeyAction,
    Modifier, NavAction, NavPhase,
};
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
                if m.name == "hello" {
                    // Examples must speak the schema's own version (the "major mismatch" one
                    // is the client being wrong, and the server answering with ours).
                    let version = schema.doc()["version"].as_str().unwrap();
                    let mismatch = ex.get("error").is_some();
                    let cases = [
                        (!mismatch).then(|| ("params", params["api_version"].clone())),
                        ex.get("result")
                            .map(|r| ("result", r["api_version"].clone())),
                        ex.get("error")
                            .map(|e| ("error", e["data"]["server_api_version"].clone())),
                    ];
                    for (what, v) in cases.into_iter().flatten() {
                        assert_eq!(
                            v, version,
                            "{label}: {what} api_version is not the schema's"
                        );
                    }
                    if let Some(e) = ex.get("error") {
                        let message = e["message"].as_str().unwrap();
                        assert!(
                            message.ends_with(&format!("server {version}")),
                            "{label}: message {message:?} names another server version"
                        );
                    }
                }
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
                        ErrorCode::GENERIC.contains(&c)
                            && !matches!(
                                c,
                                ErrorCode::VersionMismatch | ErrorCode::AlreadyHandshaken
                            )
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
    assert_eq!(schema_enum(&schema, "AppSource"), rust_enum(AppSource::ALL));
    assert_eq!(
        schema_enum(&schema, "ExitReason"),
        rust_enum(ExitReason::ALL)
    );
    assert_eq!(schema_enum(&schema, "NavAction"), rust_enum(NavAction::ALL));
    assert_eq!(schema_enum(&schema, "NavPhase"), rust_enum(NavPhase::ALL));
    assert_eq!(
        schema_enum(&schema, "InstallState"),
        rust_enum(InstallState::ALL)
    );
    assert_eq!(schema_enum(&schema, "Key"), rust_enum(Key::ALL));
    assert_eq!(schema_enum(&schema, "Modifier"), rust_enum(Modifier::ALL));
    assert_eq!(schema_enum(&schema, "KeyAction"), rust_enum(KeyAction::ALL));
    for b in ControllerButton::ALL {
        assert_eq!(rust_enum(&[*b]).pop_first().unwrap(), b.name());
        assert_eq!(ControllerButton::from_name(b.name()), Some(*b));
    }
    for k in Key::ALL {
        assert_eq!(rust_enum(&[*k]).pop_first().unwrap(), k.name());
        assert_eq!(Key::from_name(k.name()), Some(*k));
    }
    for m in Modifier::ALL {
        assert_eq!(
            Modifier::from_name(&rust_enum(&[*m]).pop_first().unwrap()),
            Some(*m)
        );
    }
    for e in EventName::ALL {
        assert_eq!(rust_enum(&[*e]).pop_first().unwrap(), e.wire_name());
    }
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
        // apps.launch: exactly one target, well-formed.
        ("/methods/apps.launch/params", r#"{}"#),
        (
            "/methods/apps.launch/params",
            r#"{"id":"a","url":"steam://rungameid/1"}"#,
        ),
        ("/methods/apps.launch/params", r#"{"command":[]}"#),
        (
            "/methods/apps.launch/params",
            r#"{"url":"steam://rungameid/0"}"#,
        ),
        ("/methods/apps.launch/params", r#"{"url":"http://x"}"#),
        ("/methods/apps.focus/params", r#"{}"#),
        (
            "/events/input_nav/params",
            r#"{"action":"jump","phase":"press"}"#,
        ),
        ("/events/app_exited/params", r#"{"id":"x"}"#),
        ("/events/focus_changed/params", r#"{}"#),
        ("/methods/apps.close/params", r#"{}"#),
        ("/methods/apps.close/params", r#"{"id":null}"#),
        ("/events/steam_attention/params", r#"{}"#),
        (
            "/events/steam_attention/params",
            r#"{"windows":[{"window":0,"title":"x"}]}"#,
        ),
        (
            "/methods/games.list/result",
            r#"{"games":[{"steam_app_id":1,"name":"x","install_state":"installed","size_bytes":0,"last_played":null}]}"#,
        ),
        (
            "/methods/games.list/result",
            r#"{"games":[{"steam_app_id":1,"name":"x","install_state":"gone","size_bytes":0,"last_played":null,"artwork":{"portrait":null,"header":null,"hero":null,"logo":null,"icon":null}}]}"#,
        ),
        (
            "/methods/games.list/result",
            r#"{"games":[{"steam_app_id":1,"name":"x","install_state":"installed","size_bytes":0,"last_played":null,"artwork":{"portrait":null}}]}"#,
        ),
        ("/methods/games.list/params", r#"{"x":1}"#),
        ("/methods/keys.type/params", r#"{"text":""}"#),
        ("/methods/keys.type/params", r#"{}"#),
        ("/methods/keys.key/params", r#"{"key":"A"}"#),
        (
            "/methods/keys.key/params",
            r#"{"key":"a","modifiers":["hyper"]}"#,
        ),
        (
            "/methods/keys.key/params",
            r#"{"key":"a","modifiers":["ctrl","ctrl"]}"#,
        ),
        ("/methods/keys.action/params", r#"{"action":"jump"}"#),
        ("/methods/osk.show/result", r#"{}"#),
        ("/events/osk_changed/params", r#"{"visible":"yes"}"#),
        // Permissions.
        (
            "/methods/apps.list/result",
            r#"{"apps":[{"id":"a","name":"x","kind":"native"}]}"#,
        ),
        (
            "/methods/apps.list/result",
            r#"{"apps":[{"id":"a","name":"x","kind":"native","permissions":[{"name":"mic","required":false}]}]}"#,
        ),
        (
            "/methods/apps.list/result",
            r#"{"apps":[{"id":"a","name":"x","kind":"native","permissions":[{"name":"Mic","required":false,"granted":true}]}]}"#,
        ),
        (
            "/methods/apps.set_permission/params",
            r#"{"id":"a","permission":"mic"}"#,
        ),
        (
            "/methods/apps.set_permission/params",
            r#"{"id":"a","permission":"../x","granted":false}"#,
        ),
        (
            "/methods/apps.set_permission/params",
            r#"{"id":"a","permission":"mic","granted":"no"}"#,
        ),
        (
            "/methods/apps.set_permission/params",
            r#"{"id":"a","permission":"mic","granted":false,"profile":"x"}"#,
        ),
        ("/methods/apps.set_permission/result", r#"{"ok":true}"#),
        ("/methods/apps.displays.set/params", r#"{"count":0}"#),
        ("/methods/apps.displays.set/params", r#"{"count":9}"#),
        ("/methods/apps.displays.set/params", r#"{}"#),
        (
            "/methods/apps.displays.get/result",
            r#"{"count":4,"min":1,"max":8}"#,
        ),
        (
            "/methods/apps.displays.get/result",
            r#"{"count":4,"active":0,"min":1,"max":8}"#,
        ),
        ("/methods/input.settings.set/params", r#"{}"#),
        (
            "/methods/input.settings.set/params",
            r#"{"settings":{"general":{"accept":"A"}}}"#,
        ),
        (
            "/methods/input.settings.set/params",
            r#"{"settings":{"keyboard":{"back":"alt++left"}}}"#,
        ),
        (
            "/methods/input.settings.set/params",
            r#"{"settings":{"pointer":{"speed":110}}}"#,
        ),
        (
            "/methods/input.settings.set/params",
            r#"{"settings":{"pointer":{"speed":500}}}"#,
        ),
        (
            "/methods/input.settings.set/params",
            r#"{"settings":{"apps":{"Bad Id":{}}}}"#,
        ),
        (
            "/methods/input.settings.set/params",
            r#"{"settings":{"mouse":{}}}"#,
        ),
        ("/methods/input.capture/params", r#"{"timeout":5}"#),
        ("/events/input_captured/params", r#"{}"#),
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

#[test]
fn launch_params_validation() {
    use hinteros_api::session::{AppsLaunchParams, LaunchRequest};
    let parse = |v: Value| {
        serde_json::from_value::<AppsLaunchParams>(v)
            .unwrap()
            .request()
    };
    assert_eq!(
        parse(serde_json::json!({"url": "steam://rungameid/570"})),
        Ok(LaunchRequest::Steam(570))
    );
    assert_eq!(
        parse(serde_json::json!({"id": "jellyfin-desktop"})),
        Ok(LaunchRequest::Id("jellyfin-desktop".into()))
    );
    assert!(matches!(
        parse(serde_json::json!({"command": ["sh", "-c", "true"], "name": "Shell"})),
        Ok(LaunchRequest::Command { argv, name: Some(n) }) if argv.len() == 3 && n == "Shell"
    ));
    for bad in [
        serde_json::json!({}),
        serde_json::json!({"command": []}),
        serde_json::json!({"id": "a", "command": ["x"]}),
        serde_json::json!({"url": "steam://rungameid/0"}),
        serde_json::json!({"url": "steam://rungameid/99999999999"}),
        serde_json::json!({"url": "steam://store/570"}),
    ] {
        assert!(parse(bad.clone()).is_err(), "{bad} accepted");
    }
    // Every schema example agrees with the Rust check.
    let schema = ApiSchema::session();
    for ex in schema.doc()["methods"]["apps.launch"]["examples"]
        .as_array()
        .unwrap()
    {
        assert!(parse(ex["params"].clone()).is_ok(), "{}", ex["name"]);
    }
}

#[test]
fn nullable_fields_are_required_in_rust_too() {
    use hinteros_api::session::{AppsFocusParams, FocusChangedParams};
    assert!(serde_json::from_str::<AppsFocusParams>("{}").is_err());
    assert_eq!(
        serde_json::from_str::<AppsFocusParams>(r#"{"id":null}"#)
            .unwrap()
            .id,
        None
    );
    assert!(serde_json::from_str::<FocusChangedParams>("{}").is_err());
    let back = serde_json::to_value(FocusChangedParams { app: None }).unwrap();
    assert_eq!(back, serde_json::json!({"app": null}));
}

#[test]
fn game_nullables_are_required_in_rust_too() {
    use hinteros_api::session::{Game, GameArtwork};
    let artwork = r#"{"portrait":null,"header":null,"hero":null,"logo":null,"icon":null}"#;
    assert!(serde_json::from_str::<GameArtwork>(r#"{"portrait":null}"#).is_err());
    assert_eq!(
        serde_json::from_str::<GameArtwork>(artwork).unwrap(),
        GameArtwork::default()
    );
    let game = |last: &str| {
        format!(
            r#"{{"steam_app_id":1,"name":"x","install_state":"installed","size_bytes":0,{last}"artwork":{artwork}}}"#
        )
    };
    assert!(serde_json::from_str::<Game>(&game("")).is_err());
    let g = serde_json::from_str::<Game>(&game(r#""last_played":null,"#)).unwrap();
    assert_eq!(g.last_played, None);
    assert_eq!(
        serde_json::to_value(&g).unwrap()["last_played"],
        Value::Null
    );
}

#[test]
fn system_enums_match() {
    use hinteros_api::system::{
        Base, Channel, CleanupFrequency, EventName, JobKind, JobState, NodeKind, Severity, Stage,
    };
    let schema = ApiSchema::system();
    for s in [ApiSchema::system(), ApiSchema::session()] {
        assert_eq!(schema_enum(&s, "NodeKind"), rust_enum(NodeKind::ALL));
        assert_eq!(
            schema_enum(&s, "CleanupFrequency"),
            rust_enum(CleanupFrequency::ALL)
        );
        assert_eq!(schema_enum(&s, "Stage"), rust_enum(Stage::ALL));
    }
    assert_eq!(schema_enum(&schema, "EventName"), rust_enum(EventName::ALL));
    assert_eq!(schema_enum(&schema, "Channel"), rust_enum(Channel::ALL));
    assert_eq!(schema_enum(&schema, "Base"), rust_enum(Base::ALL));
    assert_eq!(schema_enum(&schema, "JobKind"), rust_enum(JobKind::ALL));
    assert_eq!(schema_enum(&schema, "JobState"), rust_enum(JobState::ALL));
    assert_eq!(schema_enum(&schema, "Stage"), rust_enum(Stage::ALL));
    assert_eq!(schema_enum(&schema, "Severity"), rust_enum(Severity::ALL));
    for e in EventName::ALL {
        assert_eq!(rust_enum(&[*e]).pop_first().unwrap(), e.wire_name());
    }
    for c in Channel::ALL {
        assert_eq!(rust_enum(&[*c]).pop_first().unwrap(), c.name());
    }
    let events: BTreeSet<String> = system::API.events.iter().map(|e| e.name.into()).collect();
    assert_eq!(events, rust_enum(EventName::ALL));
}

#[test]
fn system_schema_rejects_what_it_should() {
    let schema = ApiSchema::system();
    let bad = [
        // Closed params everywhere: a forged uid, a path, a URL.
        ("/methods/rebuild/params", r#"{"uid":0}"#),
        (
            "/methods/ssh.set/params",
            r#"{"enabled":true,"user":"root"}"#,
        ),
        ("/methods/ssh.set/params", r#"{"enabled":"yes"}"#),
        (
            "/methods/settings.set/params",
            r#"{"revision":"x","changes":{}}"#,
        ),
        (
            "/methods/settings.set/params",
            r#"{"revision":"6b2c1e0b9f4a7d3c5e8f1a2b4c6d8e0f1a3b5c7d9e1f2a4b6c8d0e2f4a6b8c0d","changes":{"substituters":["http://evil"]}}"#,
        ),
        (
            "/methods/channel.set/params",
            r#"{"channel":"https://evil/repo.git"}"#,
        ),
        (
            "/methods/channel.set/params",
            r#"{"channel":"beta","url":"https://x"}"#,
        ),
        // The confirmed revision, not an acknowledgement of whatever the tip is.
        (
            "/methods/channel.set/params",
            r#"{"channel":"stable","acknowledge_downgrade":true}"#,
        ),
        (
            "/methods/channel.set/params",
            r#"{"channel":"stable","rev":"HEAD"}"#,
        ),
        (
            "/methods/channel.check/params",
            r#"{"channel":"beta","url":"https://x"}"#,
        ),
        ("/methods/channel.check/params", r#"{}"#),
        ("/methods/base.set/params", r#"{"base":"23.11"}"#),
        ("/methods/update.apply/params", r#"{"rev":"HEAD"}"#),
        (
            "/methods/update.apply/params",
            r#"{"rev":"../../etc/passwd"}"#,
        ),
        ("/methods/rollback/params", r#"{"generation":0}"#),
        ("/methods/jobs.get/params", r#"{"job_id":"../x"}"#),
        (
            "/methods/jobs.cancel/params",
            r#"{"job_id":"ABCDEF0123456789"}"#,
        ),
        (
            "/events/job_progress/params",
            r#"{"job_id":"3f9a0c2b7e4d1a65","stage":"x"}"#,
        ),
        (
            "/events/job_progress/params",
            r#"{"job_id":"3f9a0c2b7e4d1a65","stage":"downloading","percent":101}"#,
        ),
        // A build's percent stays below 100 and is a whole number.
        (
            "/events/job_progress/params",
            r#"{"job_id":"3f9a0c2b7e4d1a65","kind":"rebuild","stage":"building","detail":{"building":{"name":"x","percent":100}}}"#,
        ),
        (
            "/events/job_progress/params",
            r#"{"job_id":"3f9a0c2b7e4d1a65","kind":"rebuild","stage":"building","detail":{"building":{"name":"x","percent":-1}}}"#,
        ),
        (
            "/events/job_progress/params",
            r#"{"job_id":"3f9a0c2b7e4d1a65","kind":"rebuild","stage":"building","detail":{"building":{"name":"x","percent":"20"}}}"#,
        ),
    ];
    for (pointer, instance) in bad {
        let v: Value = serde_json::from_str(instance).unwrap();
        assert!(
            schema.check(pointer, &v).is_err(),
            "{pointer} accepted {instance}"
        );
    }
    // invalid_params may carry {field, reason} or nothing.
    schema
        .check_error(&serde_json::json!({"code": -32602, "message": "m"}))
        .unwrap();
    schema
        .check_error(&serde_json::json!({"code": -32602, "message": "m", "data": {"field": "changes.x", "reason": "unknown_field"}}))
        .unwrap();
    assert!(
        schema
            .check_error(&serde_json::json!({"code": 3002, "message": "m", "data": {"action": "org.hinteros.system.ssh", "reason": "mood"}}))
            .is_err()
    );
    // system_unavailable is session-side only.
    assert!(
        schema
            .check_error(
                &serde_json::json!({"code": 3001, "message": "m", "data": {"reason": "down"}})
            )
            .is_err()
    );
}

/// The Server section's inputs are closed and strict in both
/// documents, and its secrets stay out of debug output.
#[test]
fn server_section() {
    use hinteros_api::system::{
        CloudflareSetTokenParams, JellyfinAccount, JellyfinProfileAccount,
        JellyfinUsersRevealResult, LibraryKind, LibraryKindShown, PlexClaimParams, Setup,
        TailscaleState, UnitState,
    };
    let rev = "6b2c1e0b9f4a7d3c5e8f1a2b4c6d8e0f1a3b5c7d9e1f2a4b6c8d0e2f4a6b8c0d";
    for (schema, prefix) in [(ApiSchema::system(), ""), (ApiSchema::session(), "system.")] {
        assert_eq!(schema_enum(&schema, "UnitState"), rust_enum(UnitState::ALL));
        assert_eq!(schema_enum(&schema, "Setup"), rust_enum(Setup::ALL));
        assert_eq!(
            schema_enum(&schema, "LibraryKind"),
            rust_enum(LibraryKind::ALL)
        );
        assert_eq!(
            schema_enum(&schema, "LibraryKindShown"),
            rust_enum(LibraryKindShown::ALL)
        );
        assert_eq!(
            schema_enum(&schema, "TailscaleState"),
            rust_enum(TailscaleState::ALL)
        );
        let bad = [
            (
                "services.enable",
                format!(r#"{{"ids":["a"],"admin_pin":"1234","revision":"{rev}","uid":0}}"#),
            ),
            (
                "services.enable",
                format!(r#"{{"ids":["a","a"],"admin_pin":"1234","revision":"{rev}"}}"#),
            ),
            (
                "services.enable",
                format!(r#"{{"ids":["../x"],"admin_pin":"1234","revision":"{rev}"}}"#),
            ),
            (
                "services.enable",
                format!(r#"{{"ids":["a"],"pin":"1234","revision":"{rev}"}}"#),
            ),
            (
                "services.disable",
                format!(r#"{{"ids":["a"],"admin_pin":"1234","revision":"{rev}"}}"#),
            ),
            ("media.folders", r#"{"folder_id":"../etc"}"#.into()),
            ("media.folders", r#"{"folder_id":"/srv/media"}"#.into()),
            ("media.folders", r#"{"folder_id":"a/.hidden"}"#.into()),
            ("media.folders", r#"{"path":"/"}"#.into()),
            (
                "jellyfin.libraries.add",
                r#"{"admin_pin":"1234","name":"M","kind":"books","folder":"M"}"#.into(),
            ),
            (
                "jellyfin.libraries.add",
                r#"{"admin_pin":"1234","name":"","kind":"movies","folder":"M"}"#.into(),
            ),
            (
                "jellyfin.libraries.add",
                r#"{"admin_pin":"1234","name":"M","kind":"movies","folder":"M/../.."}"#.into(),
            ),
            (
                "jellyfin.libraries.remove",
                r#"{"admin_pin":"1234","library_id":"F137A2DD21BBC1B99AA5C0F6BF02A805"}"#.into(),
            ),
            ("jellyfin.users.reveal", r#"{"pin":"1234"}"#.into()),
            (
                "plex.claim",
                r#"{"admin_pin":"1234","code":"claim-abc"}"#.into(),
            ),
            (
                "plex.claim",
                r#"{"admin_pin":"1234","code":"AbCdEfGhIjKlMnOpQrStUv"}"#.into(),
            ),
            (
                "tailscale.login",
                r#"{"admin_pin":"1234","exit_node":"x"}"#.into(),
            ),
            (
                "tailscale.login",
                r#"{"admin_pin":"1234","authkey":"tskey-x"}"#.into(),
            ),
            (
                "cloudflare.set_token",
                r#"{"admin_pin":"1234","token":"not base64!"}"#.into(),
            ),
            (
                "cloudflare.set_token",
                format!(r#"{{"admin_pin":"1234","token":"{}"}}"#, "A".repeat(600)),
            ),
            (
                "cloudflare.clear_token",
                r#"{"admin_pin":"1234","token":"x"}"#.into(),
            ),
        ];
        for (m, p) in bad {
            let v: Value = serde_json::from_str(&p).unwrap();
            assert!(
                schema.check_params(&format!("{prefix}{m}"), &v).is_err(),
                "{prefix}{m} accepted {p}"
            );
        }
        // A Server section job failure says why; job_progress carries the sign-in URL.
        schema
            .check_error(&serde_json::json!({"code": 3003, "message": "m", "data": {"stage": "signing_in", "log_hint": "x", "reason": "timeout"}}))
            .unwrap();
        schema
            .check_error(&serde_json::json!({"code": 3003, "message": "m", "data": {"stage": "building", "log_hint": "x", "reason": "cancelled"}}))
            .unwrap();
        assert!(
            schema
                .check_error(&serde_json::json!({"code": 3003, "message": "m", "data": {"stage": "configuring", "log_hint": "x", "reason": "mood"}}))
                .is_err()
        );
        schema
            .check_error(&serde_json::json!({"code": 4001, "message": "m", "data": {"service": "jellyfin", "reason": "not_ready"}}))
            .unwrap();
        let progress = serde_json::json!({"job_id": "3f9a0c2b7e4d1a65", "kind": "tailscale_login", "stage": "signing_in", "started_at": 1, "detail": {"auth_url": "https://login.tailscale.com/a/1a2b3c"}});
        schema.check_event("job_progress", &progress).unwrap();
        let mut bad_url = progress.clone();
        bad_url["detail"]["auth_url"] = "javascript:alert(1)".into();
        assert!(schema.check_event("job_progress", &bad_url).is_err());
    }
    let p: PlexClaimParams =
        serde_json::from_str(r#"{"admin_pin":"1234","code":"claim-AbCdEfGhIjKlMnOpQrSt"}"#)
            .unwrap();
    assert!(!format!("{p:?}").contains("claim-") && !format!("{p:?}").contains("1234"));
    let t: CloudflareSetTokenParams =
        serde_json::from_str(r#"{"admin_pin":"1234","token":"c2VjcmV0"}"#).unwrap();
    assert!(!format!("{t:?}").contains("c2VjcmV0"));
    let r = JellyfinUsersRevealResult {
        admin: Some(JellyfinAccount {
            name: "admin".into(),
            password: "s3cret-a".into(),
        }),
        users: vec![JellyfinProfileAccount {
            profile_id: "p1".into(),
            profile_name: "A".into(),
            name: "A".into(),
            password: "s3cret-b".into(),
        }],
    };
    assert!(!format!("{r:?}").contains("s3cret"));
    for m in system::MUTATING {
        if m.starts_with("services.")
            || m.starts_with("jellyfin.")
            || m.starts_with("plex.")
            || m.starts_with("tailscale.")
            || m.starts_with("cloudflare.")
        {
            assert_eq!(
                system::polkit_action(m),
                Some("org.hinteros.system.services"),
                "{m}"
            );
        }
    }
    assert!(
        !system::MUTATING.contains(&"services.list")
            && !system::MUTATING.contains(&"media.folders")
    );
}

/// `Building.percent`, in both documents, relayed unchanged.
#[test]
fn building_percent() {
    let ok = r#"{"job_id":"3f9a0c2b7e4d1a65","kind":"rebuild","stage":"building","started_at":1,"last_activity_at":1,"detail":{"building":{"name":"x","phase":"build","percent":20}}}"#;
    let bad = r#"{"job_id":"3f9a0c2b7e4d1a65","kind":"rebuild","stage":"building","started_at":1,"last_activity_at":1,"detail":{"building":{"name":"x","percent":99.5}}}"#;
    for schema in [ApiSchema::system(), ApiSchema::session()] {
        let v: Value = serde_json::from_str(ok).unwrap();
        schema.check("/events/job_progress/params", &v).unwrap();
        let v: Value = serde_json::from_str(bad).unwrap();
        assert!(schema.check("/events/job_progress/params", &v).is_err());
    }
    let p: system::JobProgressParams = serde_json::from_str(ok).unwrap();
    assert_eq!(p.detail.unwrap().building.unwrap().percent, Some(20));
}

/// The schema's ZoneName and `is_zone_name` agree; nothing path-like gets through.
#[test]
fn zone_names() {
    let schema = ApiSchema::system();
    let check = |zone: &str| {
        schema
            .check(
                "/methods/time_zone.set/params",
                &serde_json::json!({ "zone": zone }),
            )
            .is_ok()
    };
    for good in [
        "UTC",
        "America/Chicago",
        "America/Argentina/ComodRivadavia",
        "Etc/GMT+5",
        "Etc/GMT-14",
        "America/Port-au-Prince",
        "Asia/Ust-Nera",
    ] {
        assert!(system::is_zone_name(good), "{good}");
        assert!(check(good), "schema refuses {good}");
    }
    let long = "A".repeat(65);
    for bad in [
        "",
        "/etc/passwd",
        "../etc/shadow",
        "America/../../etc",
        "America/./Chicago",
        "..",
        ".",
        "America/",
        "/UTC",
        "America//Chicago",
        "America/Chi cago",
        "America/chicago",
        "posixrules",
        "right/UTC",
        "posix/Europe/London",
        "zone1970.tab",
        "Etc/GMT.5",
        "America\\Chicago",
        "Europe/Zürich",
        "UTC\n",
        long.as_str(),
    ] {
        assert!(!system::is_zone_name(bad), "{bad:?}");
        assert!(!check(bad), "schema accepts {bad:?}");
    }
    use hinteros_api::system::TimeZoneSetParams;
    assert!(serde_json::from_str::<TimeZoneSetParams>(r#"{"zone":"UTC","path":"/"}"#).is_err());
}

#[test]
fn system_rust_params_are_closed() {
    use hinteros_api::system::{AdminPinParams, NoParams, SshSetParams, UpdateApplyParams};
    assert!(serde_json::from_str::<NoParams>(r#"{"uid":0}"#).is_err());
    assert!(serde_json::from_str::<SshSetParams>(r#"{"enabled":true,"x":1}"#).is_err());
    assert!(serde_json::from_str::<UpdateApplyParams>(r#"{"rev":"a","path":"/"}"#).is_err());
    // No alias: the admin PIN is only ever `admin_pin`.
    assert!(serde_json::from_str::<AdminPinParams>(r#"{"pin":"1234"}"#).is_err());
    assert_eq!(
        serde_json::from_str::<AdminPinParams>("{}").unwrap(),
        AdminPinParams::default()
    );
    for action in system::POLKIT_ACTIONS {
        assert!(action.starts_with("org.hinteros.system."));
    }
    for m in system::MUTATING {
        assert!(
            system::polkit_action(m).is_some(),
            "{m} has no polkit action"
        );
    }
    // The session relay waits longer for these (the admin password). The launcher's
    // `DaemonClient._interactive` (lib/src/api/client.dart) lists the same methods: change both.
    let waits: Vec<&str> = system::API
        .methods
        .iter()
        .map(|m| m.name)
        .filter(|m| system::may_interact(m))
        .collect();
    assert_eq!(waits, ["ssh.set", "profiles.reset_pin"]);
}

/// Inline every `#/$defs/...` reference, so schemas from two documents can be compared. A
/// recursive definition (a tree node) stays a reference where it refers to itself.
fn resolve(doc: &Value, v: &Value) -> Value {
    resolve_in(doc, v, &mut Vec::new())
}

fn resolve_in(doc: &Value, v: &Value, open: &mut Vec<String>) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                if k == "$ref" {
                    let name = val.as_str().unwrap().strip_prefix("#/$defs/").unwrap();
                    if open.iter().any(|n| n == name) {
                        out.insert(k.clone(), val.clone());
                        continue;
                    }
                    open.push(name.to_owned());
                    let target = resolve_in(doc, &doc["$defs"][name], open);
                    open.pop();
                    for (tk, tv) in target.as_object().cloned().unwrap_or_default() {
                        out.insert(tk, tv);
                    }
                } else {
                    out.insert(k.clone(), resolve_in(doc, val, open));
                }
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| resolve_in(doc, x, open)).collect()),
        other => other.clone(),
    }
}

#[test]
fn relay_matches_the_system_api() {
    let session = ApiSchema::session();
    let system = ApiSchema::system();
    let (s, y) = (session.doc(), system.doc());
    let relayed: BTreeSet<&str> = session::RELAY.iter().map(|(s, _)| *s).collect();
    let marked: BTreeSet<&str> = s["methods"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, m)| m["relay"] == true)
        .map(|(k, _)| k.as_str())
        .collect();
    assert_eq!(relayed, marked, "relay table vs schema relay flags");
    // Every system method except the core ones and the session daemon's own (`lan_filter.*`,
    // the switch hint and the end of the guest session) is relayed.
    let system_methods: BTreeSet<&str> = y["methods"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .filter(|m| !["hello", "ping", "subscribe", "unsubscribe"].contains(m))
        .filter(|m| !m.starts_with("lan_filter."))
        .filter(|m| !["profiles.switch", "profiles.switch_cancel", "guest.end"].contains(m))
        .collect();
    let targets: BTreeSet<&str> = session::RELAY.iter().map(|(_, t)| *t).collect();
    assert_eq!(system_methods, targets);
    for (sm, ym) in session::RELAY {
        for part in ["params", "result"] {
            assert_eq!(
                resolve(s, &s["methods"][sm][part]),
                resolve(y, &y["methods"][ym][part]),
                "{sm} {part} differs from the system API's {ym}"
            );
        }
        let s_errors: BTreeSet<&str> = s["methods"][sm]["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e.as_str().unwrap())
            .collect();
        assert!(s_errors.contains("system_unavailable"), "{sm}");
        for e in y["methods"][ym]["errors"].as_array().unwrap() {
            assert!(s_errors.contains(e.as_str().unwrap()), "{sm} lacks {e}");
        }
    }
    for ev in [
        "job_started",
        "job_progress",
        "job_finished",
        "time_zone_changed",
    ] {
        assert_eq!(
            resolve(s, &s["events"][ev]["params"]),
            resolve(y, &y["events"][ev]["params"]),
            "event {ev}"
        );
    }
    // Errors are one code space: system errors are forwarded unchanged.
    for (name, decl) in y["errors"].as_object().unwrap() {
        assert_eq!(
            resolve(s, &s["errors"][name]["data"]),
            resolve(y, &decl["data"]),
            "error {name} data"
        );
        assert_eq!(s["errors"][name]["code"], decl["code"], "error {name}");
    }
}

#[test]
fn runtime_enums_match() {
    use hinteros_api::session::{
        AudioKind, BtConfirmMode, BtKind, Connectivity, PowerCapability, Service, WifiSecurity,
    };
    let schema = ApiSchema::session();
    assert_eq!(
        schema_enum(&schema, "Connectivity"),
        rust_enum(Connectivity::ALL)
    );
    assert_eq!(
        schema_enum(&schema, "WifiSecurity"),
        rust_enum(WifiSecurity::ALL)
    );
    assert_eq!(schema_enum(&schema, "BtKind"), rust_enum(BtKind::ALL));
    assert_eq!(schema_enum(&schema, "AudioKind"), rust_enum(AudioKind::ALL));
    assert_eq!(
        schema_enum(&schema, "PowerCapability"),
        rust_enum(PowerCapability::ALL)
    );
    let confirm: Vec<String> = schema.doc()["events"]["bluetooth_confirm"]["params"]["properties"]
        ["mode"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        confirm.into_iter().collect::<BTreeSet<_>>(),
        rust_enum(BtConfirmMode::ALL)
    );
    let services: Vec<String> =
        schema.doc()["errors"]["unavailable"]["data"]["properties"]["service"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
    assert_eq!(
        services.into_iter().collect::<BTreeSet<_>>(),
        rust_enum(Service::ALL)
    );
    for s in Service::ALL {
        assert_eq!(rust_enum(&[*s]).pop_first().unwrap(), s.name());
    }
    // Every runtime method is dispatchable and in the API, and nothing else is.
    let methods: BTreeSet<&str> = session::METHODS.iter().copied().collect();
    for m in &methods {
        assert!(session::API.methods.iter().any(|i| i.name == *m), "{m}");
    }
    for (prefix, n) in [
        ("network.", 1),
        ("wifi.", 6),
        ("bluetooth.", 8),
        ("audio.", 4),
        ("display.", 4),
        ("power.", 4),
    ] {
        assert_eq!(
            methods.iter().filter(|m| m.starts_with(prefix)).count(),
            n,
            "{prefix}"
        );
    }
}

#[test]
fn runtime_schema_rejects_what_it_should() {
    let schema = ApiSchema::session();
    let bad = [
        ("/methods/wifi.connect/params", r#"{}"#),
        ("/methods/wifi.connect/params", r#"{"ssid":""}"#),
        (
            "/methods/wifi.connect/params",
            r#"{"ssid":"a","password":"x","hidden":true}"#,
        ),
        ("/methods/wifi.enable/params", r#"{"enabled":"on"}"#),
        (
            "/methods/bluetooth.pair/params",
            r#"{"address":"aa:bb:cc:dd:ee:ff"}"#,
        ),
        ("/methods/bluetooth.pair/params", r#"{"address":"AA:BB"}"#),
        (
            "/methods/bluetooth.confirm/params",
            r#"{"address":"AA:BB:CC:DD:EE:FF"}"#,
        ),
        (
            "/methods/bluetooth.scan/params",
            r#"{"enabled":true,"timeout_s":0}"#,
        ),
        ("/methods/audio.volume/params", r#"{"percent":101}"#),
        ("/methods/audio.volume/params", r#"{"percent":-1}"#),
        ("/methods/audio.mute/params", r#"{}"#),
        ("/methods/display.set_mode/params", r#"{"width":1920}"#),
        (
            "/methods/display.set_mode/params",
            r#"{"width":0,"height":1080,"refresh":60}"#,
        ),
        ("/methods/power.reboot/params", r#"{"force":true}"#),
        (
            "/events/bluetooth_confirm/params",
            r#"{"address":"AA:BB:CC:DD:EE:FF","name":"x","passkey":"12345","mode":"confirm"}"#,
        ),
        (
            "/methods/network.status/result",
            r#"{"wifi_available":true,"wifi_enabled":true,"ethernet":false,"connectivity":"great"}"#,
        ),
    ];
    for (pointer, instance) in bad {
        let v: Value = serde_json::from_str(instance).unwrap();
        assert!(
            schema.check(pointer, &v).is_err(),
            "{pointer} accepted {instance}"
        );
    }
    for code in [4001, 4002, 4003, 4004, 4005] {
        assert!(
            schema
                .check_error(&serde_json::json!({"code": code, "message": "m", "data": {"x": 1}}))
                .is_err(),
            "{code} accepts stray data"
        );
    }
    schema
        .check_error(&serde_json::json!({"code": 4001, "message": "m", "data": {"service": "bluez", "reason": "r"}}))
        .unwrap();
    assert!(
        schema
            .check_error(&serde_json::json!({"code": 4001, "message": "m"}))
            .is_err()
    );
}

#[test]
fn wifi_password_is_not_in_debug_output() {
    use hinteros_api::session::WifiConnectParams;
    let p = WifiConnectParams {
        ssid: "Home".into(),
        password: Some("hunter2hunter2".into()),
    };
    let shown = format!("{p:?}");
    assert!(
        shown.contains("Home") && !shown.contains("hunter2"),
        "{shown}"
    );
}

#[test]
fn bluetooth_addresses() {
    use hinteros_api::session::valid_bt_address;
    assert!(valid_bt_address("AA:BB:CC:DD:EE:0F"));
    for bad in [
        "aa:bb:cc:dd:ee:0f",
        "AA:BB:CC:DD:EE",
        "AA-BB-CC-DD-EE-0F",
        "AA:BB:CC:DD:EE:GG",
        "",
    ] {
        assert!(!valid_bt_address(bad), "{bad}");
    }
}

#[test]
fn agent_and_store_enums_match() {
    use hinteros_api::session::{AuthOutcome, CatalogStatus, StoreState};
    let schema = ApiSchema::session();
    assert_eq!(
        schema_enum(&schema, "AuthOutcome"),
        rust_enum(AuthOutcome::ALL)
    );
    assert_eq!(
        schema_enum(&schema, "StoreState"),
        rust_enum(StoreState::ALL)
    );
    assert_eq!(
        schema_enum(&schema, "CatalogStatus"),
        rust_enum(CatalogStatus::ALL)
    );
    // The system API's JobKind is relayed unchanged.
    use hinteros_api::system::JobKind;
    assert_eq!(schema_enum(&schema, "JobKind"), rust_enum(JobKind::ALL));
}

#[test]
fn store_and_agent_schema_rejects_what_it_should() {
    let system = ApiSchema::system();
    let rev = "6b2c1e0b9f4a7d3c5e8f1a2b4c6d8e0f1a3b5c7d9e1f2a4b6c8d0e2f4a6b8c0d";
    let bad = [
        format!(r#"{{"ids":[],"revision":"{rev}"}}"#),
        format!(r#"{{"ids":["../x"],"revision":"{rev}"}}"#),
        format!(r#"{{"ids":["/nix/store/abc-steam"],"revision":"{rev}"}}"#),
        format!(r#"{{"ids":["Steam"],"revision":"{rev}"}}"#),
        format!(r#"{{"ids":["a","b","c","d","e","f","g","h","i"],"revision":"{rev}"}}"#),
        r#"{"ids":["steam"],"revision":"x"}"#.to_string(),
        format!(r#"{{"ids":["steam"],"revision":"{rev}","accepted_terms":{{"../x":"1"}}}}"#),
        format!(r#"{{"ids":["steam"],"revision":"{rev}","accepted_terms":{{"steam":"1 2"}}}}"#),
        format!(r#"{{"ids":["steam"],"revision":"{rev}","path":"/"}}"#),
    ];
    for instance in &bad {
        let v: Value = serde_json::from_str(instance).unwrap();
        assert!(
            system.check("/methods/apps.install/params", &v).is_err(),
            "apps.install accepted {instance}"
        );
    }
    for instance in [
        format!(r#"{{"ids":["steam"],"revision":"{rev}"}}"#),
        format!(r#"{{"ids":["steam"],"revision":"{rev}","delete_data":"yes"}}"#),
        format!(r#"{{"ids":["steam"],"revision":"{rev}","delete_data":true,"home":"/root"}}"#),
    ] {
        let v: Value = serde_json::from_str(&instance).unwrap();
        assert!(
            system.check("/methods/apps.remove/params", &v).is_err(),
            "apps.remove accepted {instance}"
        );
    }
    use hinteros_api::system::{AppsInstallParams, AppsRemoveParams};
    assert!(
        serde_json::from_str::<AppsInstallParams>(&format!(
            r#"{{"ids":["a"],"revision":"{rev}","uid":0}}"#
        ))
        .is_err()
    );
    assert!(
        serde_json::from_str::<AppsRemoveParams>(&format!(r#"{{"ids":["a"],"revision":"{rev}"}}"#))
            .is_err(),
        "delete_data is required"
    );

    let session = ApiSchema::session();
    for (pointer, instance) in [
        ("/methods/auth.respond/params", r#"{"cookie":"a"}"#),
        (
            "/methods/auth.respond/params",
            r#"{"cookie":"a b","password":"x"}"#,
        ),
        (
            "/methods/auth.respond/params",
            r#"{"cookie":"a","password":"x","user":"root"}"#,
        ),
        ("/methods/auth.cancel/params", r#"{"cookie":""}"#),
        ("/methods/session.restart/params", r#"{"now":true}"#),
        (
            "/events/authentication_finished/params",
            r#"{"cookie":"a","outcome":"maybe"}"#,
        ),
    ] {
        let v: Value = serde_json::from_str(instance).unwrap();
        assert!(
            session.check(pointer, &v).is_err(),
            "{pointer} accepted {instance}"
        );
    }
    let long = "x".repeat(512);
    assert!(
        session
            .check(
                "/methods/auth.respond/params",
                &serde_json::json!({"cookie": "a", "password": long})
            )
            .is_err()
    );
    assert!(hinteros_api::session::is_auth_cookie(
        "3-6f1c0e2a9b7d4c58-1-0a9f3e1d2c4b5a69"
    ));
    for bad in ["", "a b", "a\n", &"x".repeat(129), "../x/"] {
        assert!(!hinteros_api::session::is_auth_cookie(bad), "{bad:?}");
    }
}

#[test]
fn auth_password_is_not_in_debug_output() {
    use hinteros_api::session::AuthRespondParams;
    let p = AuthRespondParams {
        cookie: "1-a-1-b".into(),
        password: "hunter2hunter2".into(),
    };
    let shown = format!("{p:?}");
    assert!(
        shown.contains("1-a-1-b") && !shown.contains("hunter2"),
        "{shown}"
    );
}
