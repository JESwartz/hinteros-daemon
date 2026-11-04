//! Forward compatibility: an older client reads a newer server of its major, and a server's
//! requests stay closed. Generic over every example in the schema documents, so a new response
//! type with `deny_unknown_fields` fails here.

use std::fmt::Debug;
use std::io::{BufRead, BufReader, Write};

use hinteros_api::client::Client;
use hinteros_api::conformance::ApiSchema;
use hinteros_api::runtime::{
    AudioKind, BtConfirmMode, BtKind, Connectivity, PowerCapability, WifiSecurity,
};
use hinteros_api::session::{
    AppKind, AppSource, AuthOutcome, CatalogStatus, ExitReason, InstallState, NavAction, NavPhase,
    StoreState, SwitchStage,
};
use hinteros_api::system::{
    self, Channel, JobKind, JobState, LibraryKindShown, NodeKind, Severity, Stage, TailscaleState,
    UnitState,
};
use hinteros_api::{ApiInfo, ErrorCode, ErrorObject, Method, server, session};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const NEW_VALUE: &str = "added_by_a_later_minor";

fn apis() -> [(ApiSchema, &'static ApiInfo); 2] {
    [
        (ApiSchema::session(), &session::API),
        (ApiSchema::system(), &system::API),
    ]
}

/// `v` with a new field in every object.
fn grown(v: &Value) -> Value {
    match v {
        Value::Object(o) => {
            let mut out: serde_json::Map<String, Value> =
                o.iter().map(|(k, v)| (k.clone(), grown(v))).collect();
            out.insert(NEW_VALUE.into(), json!(NEW_VALUE));
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(grown).collect()),
        other => other.clone(),
    }
}

#[test]
fn results_and_events_take_unknown_fields() {
    for (schema, info) in apis() {
        for m in info.methods {
            // Its maps have typed values: a new key with a new value is a closed `ControllerButton`.
            if m.name.starts_with("input.settings.") {
                continue;
            }
            for ex in schema.doc()["methods"][m.name]["examples"]
                .as_array()
                .unwrap()
            {
                if let Some(result) = ex.get("result") {
                    (m.roundtrip_result)(grown(result))
                        .unwrap_or_else(|e| panic!("{} {}: {e}", info.api, m.name));
                }
            }
        }
        for ev in info.events {
            for ex in schema.doc()["events"][ev.name]["examples"]
                .as_array()
                .unwrap()
            {
                (ev.roundtrip_params)(grown(&ex["params"]))
                    .unwrap_or_else(|e| panic!("{} {}: {e}", info.api, ev.name));
            }
        }
    }
}

fn reads_new_value<T: DeserializeOwned + PartialEq + Debug>(want: T) {
    assert_eq!(serde_json::from_value::<T>(json!(NEW_VALUE)).unwrap(), want);
}

#[test]
fn enums_a_client_only_reads_take_unknown_values() {
    reads_new_value(JobKind::Unknown);
    reads_new_value(JobState::Unknown);
    reads_new_value(Stage::Unknown);
    reads_new_value(Severity::Unknown);
    reads_new_value(NodeKind::Unknown);
    reads_new_value(TailscaleState::Unknown);
    reads_new_value(UnitState::Unknown);
    reads_new_value(LibraryKindShown::Other);
    reads_new_value(AppKind::Unknown);
    reads_new_value(AppSource::Unknown);
    reads_new_value(ExitReason::Unknown);
    reads_new_value(NavAction::Unknown);
    reads_new_value(NavPhase::Unknown);
    reads_new_value(InstallState::Unknown);
    reads_new_value(AuthOutcome::Unknown);
    reads_new_value(SwitchStage::Unknown);
    reads_new_value(StoreState::Unknown);
    reads_new_value(CatalogStatus::Unknown);
    reads_new_value(Connectivity::Unknown);
    reads_new_value(WifiSecurity::Unknown);
    reads_new_value(BtKind::Unknown);
    reads_new_value(BtConfirmMode::Unknown);
    reads_new_value(AudioKind::Unknown);
    reads_new_value(PowerCapability::Unknown);
    // The catch-all is not part of the API.
    assert!(!JobKind::ALL.contains(&JobKind::Unknown));
    assert!(!Stage::ALL.contains(&Stage::Unknown));
}

fn job_from_a_newer_server() -> Value {
    json!({
        "job_id": "3f9a0c2b7e4d1a65", "kind": NEW_VALUE, "uid": 1000, "state": NEW_VALUE,
        "stage": NEW_VALUE, "created_at": 1, "finished_at": 2,
        "error": {"code": 9999, "message": "new"},
        "result": {"shape": NEW_VALUE}, "session_restart_required": false,
        "subject": {"new": 1}, "started_at": 1, "percent": null, "detail": null,
        "last_activity_at": null, "new": true,
    })
}

#[test]
fn a_job_of_a_later_minor_reads() {
    let r: system::JobsGetResult =
        serde_json::from_value(json!({ "job": job_from_a_newer_server() })).unwrap();
    assert_eq!(r.job.kind, JobKind::Unknown);
    assert_eq!(r.job.state, JobState::Unknown);
    assert_eq!(r.job.stage, Some(Stage::Unknown));
    assert_eq!(r.job.result, None);
    let e = r.job.error.unwrap();
    assert_eq!((e.code, e.kind()), (9999, None));
}

#[test]
fn a_service_of_a_later_minor_reads() {
    let s: system::ServiceInfo = serde_json::from_value(json!({
        "id": "x", "name": "X", "available": true, "enabled": true, "status": NEW_VALUE,
        "setup": NEW_VALUE, "terms": null, "detail": {"shape": NEW_VALUE},
    }))
    .unwrap();
    assert_eq!((s.setup, s.detail), (None, None));
    assert_eq!(s.status, Some(UnitState::Unknown));
}

/// The whole path: `Client::call` on what a newer server writes.
#[test]
fn the_client_reads_a_newer_server() {
    let dir = std::env::temp_dir().join(format!("hinteros-forward-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("s.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        for result in [
            Ok(json!({ "job": job_from_a_newer_server() })),
            Err(json!({"code": 9999, "message": "new", "data": {"new": 1}})),
        ] {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let id = serde_json::from_str::<Value>(&line).unwrap()["id"].clone();
            let mut reply = json!({"jsonrpc": "2.0", "id": id, "new": 1});
            match result {
                Ok(r) => reply["result"] = r,
                Err(e) => reply["error"] = e,
            }
            writeln!(writer, "{reply}").unwrap();
        }
    });
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut c = Client::connect(&socket).await.unwrap();
        let params = system::JobIdParams {
            job_id: "3f9a0c2b7e4d1a65".into(),
        };
        let r = c.call::<system::JobsGet>(&params).await.unwrap();
        assert_eq!(r.job.kind, JobKind::Unknown);
        let e = c.call_raw("anything", json!({})).await.unwrap_err();
        let hinteros_api::client::ClientError::Rpc(e) = e else {
            panic!("{e:?}")
        };
        assert_eq!(e.kind(), None::<ErrorCode>);
    });
    server.join().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

fn refused<M: Method>(params: Value) {
    let e: ErrorObject = server::decode_params::<M>(Some(params.clone()))
        .err()
        .unwrap_or_else(|| panic!("{} took {params}", M::NAME));
    assert_eq!(e.kind(), Some(ErrorCode::InvalidParams));
}

#[test]
fn requests_stay_closed_where_it_protects_something() {
    refused::<system::Rebuild>(json!({"uid": 0}));
    refused::<system::SshSet>(json!({"enabled": true, "path": "/etc"}));
    refused::<system::ProfilesSwitch>(json!({"id": "p1", "uid": 0}));
    refused::<session::SessionUnlock>(json!({"pin": "1234", "x": 1}));
    refused::<session::GreeterLogin>(json!({"id": "p1", "x": 1}));
    refused::<session::SessionSwitchProfile>(json!({"id": "p1", "x": 1}));
    // A closed enum takes no unknown value, here or in a result: no `Unknown` reaches a server.
    refused::<system::ChannelSet>(json!({"channel": NEW_VALUE}));
    refused::<system::BaseSet>(json!({"base": NEW_VALUE}));
    refused::<session::KeysKey>(json!({"key": NEW_VALUE}));
    assert!(serde_json::from_value::<Channel>(json!(NEW_VALUE)).is_err());
    // Session requests with nothing to protect are open.
    let p = server::decode_params::<session::SessionSwitchCancel>(Some(json!({"new": 1})));
    assert!(p.is_ok());
}
