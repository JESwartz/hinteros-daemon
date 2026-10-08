//! Property tests (proptest) standing in for fuzzing: every parser that sees untrusted input
//! must never panic, and accepted input must keep its invariants. They run in `cargo test`, so
//! in `nix flake check`.

use hinteros_api::system::{self, Base, Severity};
use proptest::prelude::*;
use serde_json::{Value, json};

use crate::service::Request;
use crate::state::{Profile, SshState, State, StateSettings};
use crate::{flake, nixlog, state, update};

/// Arbitrary JSON values, a few levels deep.
fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        any::<f64>().prop_map(|f| json!(f)),
        ".{0,40}".prop_map(Value::String),
        "[0-9a-f]{16,64}".prop_map(Value::String),
    ];
    leaf.prop_recursive(3, 32, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::btree_map(
                prop_oneof![
                    Just("enabled".to_string()),
                    Just("revision".to_string()),
                    Just("changes".to_string()),
                    Just("rev".to_string()),
                    Just("generation".to_string()),
                    Just("channel".to_string()),
                    Just("base".to_string()),
                    Just("job_id".to_string()),
                    Just("acknowledge_downgrade".to_string()),
                    "[a-z_]{1,12}",
                ],
                inner,
                0..6
            )
            .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn request_params_never_panic(m in prop::sample::select(system::MUTATING), p in json_value()) {
        let _ = Request::decode(m, Some(p));
    }

    #[test]
    fn invalid_params_carry_well_formed_data(m in prop::sample::select(system::MUTATING), p in json_value()) {
        if let Err(e) = Request::decode(m, Some(p)) {
            prop_assert!(e.code == -32602 || e.code == -32601);
            if let Some(d) = &e.data {
                prop_assert!(d["field"].as_str().is_some_and(|f| f.chars().count() <= 200));
                prop_assert!(d["reason"].as_str().is_some_and(|r| !r.is_empty()));
            }
        }
    }

    #[test]
    fn state_parse_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..400)) {
        let _ = State::parse(&bytes);
    }

    #[test]
    fn state_json_values_never_panic(v in json_value()) {
        let _ = State::parse(v.to_string().as_bytes());
        let _ = state::check_changes(&v);
    }

    #[test]
    fn accepted_states_round_trip(
        enable in any::<bool>(),
        stable in any::<bool>(),
        apps in prop::collection::vec("[a-z0-9][a-z0-9-]{0,31}", 0..5),
        host in prop::option::of("[a-z][a-z0-9-]{0,10}"),
        users in prop::collection::vec(("[a-z0-9-]{1,8}", "[^\\p{Cc}]{1,20}", "[a-z_][a-z0-9_-]{0,10}"), 0..3),
    ) {
        let s = State {
            schema_version: 1,
            base: if stable { Base::Stable } else { Base::Unstable },
            state_version: host.clone().map(|_| "26.11".to_string()),
            settings: StateSettings { hostname: host, ssh: SshState { enable }, admin: None, guest: None },
            disk: None,
            apps: Some(apps),
            requirements: None,
            services: None,
            profiles: users.into_iter().map(|(id, name, user)| Profile { id, name: Some(name), user }).collect(),
        };
        prop_assume!(s.validate().is_ok());
        let bytes = s.to_canonical_json();
        let back = State::parse(&bytes).unwrap();
        prop_assert_eq!(&back, &s);
        prop_assert_eq!(back.to_canonical_json(), bytes);
    }

    #[test]
    fn git_parsers_never_panic(s in ".{0,300}") {
        let c = update::Commit::parse(&s);
        let _ = c.subject();
        let _ = update::trailers(&s);
        let _ = update::security_severity(&s);
        let _ = update::signer_from_output(&s);
    }

    #[test]
    fn security_trailer_is_found(
        subject in "[A-Za-z][A-Za-z0-9 ]{0,30}",
        body in "[A-Za-z ]{1,40}",
        level in prop::sample::select(vec!["low", "medium", "high", "critical"]),
    ) {
        let msg = format!("{subject}\n\n{body}\n\nSecurity: {level}\nSigned-off-by: A <a@x>\n");
        let expected = match level {
            "low" => Severity::Low,
            "medium" => Severity::Medium,
            "high" => Severity::High,
            _ => Severity::Critical,
        };
        prop_assert_eq!(update::security_severity(&msg), Some(expected));
    }

    #[test]
    fn nix_log_never_panics(lines in prop::collection::vec(prop_oneof![
        ".{0,80}",
        (0u64..5, prop::sample::select(vec!["start", "stop", "result", "msg"]), 99u64..110, prop::collection::vec(any::<u64>(), 0..5))
            .prop_map(|(id, action, ty, fields)| format!(
                "@nix {}",
                json!({"action": action, "id": id, "type": ty, "fields": fields, "level": 0, "msg": "error: x"})
            )),
    ], 0..40)) {
        let mut p = nixlog::Progress::default();
        for l in &lines {
            p.line(l);
        }
        let _ = p.detail();
        if let Some(pc) = p.percent() {
            prop_assert!(pc <= 100);
        }
    }

    #[test]
    fn flake_parsers_never_panic(s in ".{0,200}", v in json_value()) {
        let _ = flake::parse_lock(&s);
        let _ = flake::parse_lock(&v.to_string());
        let _ = flake::retarget_flake_nix(&s, "\"a\"", "b");
        let _ = flake::is_branch(&s);
    }
}
