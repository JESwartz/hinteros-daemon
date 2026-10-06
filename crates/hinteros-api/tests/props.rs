//! Property tests (proptest) for the request parser and the NDJSON reader, which every byte
//! from a client goes through, standing in for fuzzing in `cargo test` / `nix flake check`.

use hinteros_api::codec::{LineReader, ReadError};
use hinteros_api::common::Peer;
use hinteros_api::server::{Connection, Step};
use hinteros_api::{jsonrpc, system};
use proptest::prelude::*;

fn request_like() -> impl Strategy<Value = String> {
    prop_oneof![
        ".{0,200}",
        (
            prop_oneof![Just("2.0".to_string()), ".{0,4}"],
            prop_oneof![
                Just("1".to_string()),
                Just("\"a\"".to_string()),
                Just("null".to_string()),
                "-?[0-9]{1,20}(\\.[0-9])?"
            ],
            prop_oneof![
                Just("hello".to_string()),
                Just("ping".to_string()),
                Just("rebuild".to_string()),
                "[a-z.]{0,20}"
            ],
            prop_oneof![
                Just("{}".to_string()),
                Just("[]".to_string()),
                Just("null".to_string()),
                Just(r#"{"api_version":"0.2","client":{"name":"x","version":"1"}}"#.to_string()),
                ".{0,40}"
            ],
        )
            .prop_map(|(v, id, m, p)| format!(
                r#"{{"jsonrpc":"{v}","id":{id},"method":"{m}","params":{p}}}"#
            )),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, ..ProptestConfig::default() })]

    #[test]
    fn parse_request_never_panics(line in request_like()) {
        match jsonrpc::parse_request(&line) {
            Ok(r) => prop_assert!(r.method.len() <= line.len()),
            Err(reply) => prop_assert!(reply["error"]["code"].is_i64()),
        }
    }

    #[test]
    fn connection_step_never_panics(lines in prop::collection::vec(request_like(), 1..8)) {
        let mut conn = Connection::new(system::VERSION, Peer::new("t", "0"));
        for line in &lines {
            match conn.step(line) {
                Step::Dispatch(req) => prop_assert!(conn.negotiated().is_some() && req.id.is_some()),
                Step::Reply(v) | Step::ReplyAndClose(v) => prop_assert_eq!(&v["jsonrpc"], "2.0"),
                Step::Ignore => {}
            }
        }
    }

    #[test]
    fn line_reader_respects_its_limit(data in prop::collection::vec(any::<u8>(), 0..2000), limit in 1usize..300) {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let mut r = LineReader::with_limit(&data[..], limit);
            for _ in 0..2100 {
                match r.next_line().await {
                    Ok(Some(line)) => assert!(line.len() <= limit),
                    Ok(None) => break,
                    Err(ReadError::TooLong | ReadError::NotUtf8) => break,
                    Err(ReadError::Io(e)) => panic!("{e}"),
                }
            }
        });
    }
}
