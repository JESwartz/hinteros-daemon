//! `Client::call_secret` (requests that carry a PIN) puts the same request on the wire as
//! `Client::call`.

use std::io::{BufRead, BufReader, Write};

use hinteros_api::client::Client;
use hinteros_api::system::{self, ProfilesCreateParams};
use serde_json::{Value, json};

#[test]
fn a_secret_call_is_an_ordinary_request() {
    let dir = std::env::temp_dir().join(format!("hinteros-client-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("s.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        let mut requests = Vec::new();
        for _ in 0..2 {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut request: Value = serde_json::from_str(&line).unwrap();
            let id = request["id"].take();
            let reply = json!({"jsonrpc": "2.0", "id": id,
                "result": {"job_id": "3f9a0c2b7e4d1a65", "id": "p2"}});
            writeln!(writer, "{reply}").unwrap();
            requests.push(request);
        }
        requests
    });
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut c = Client::connect(&socket).await.unwrap();
        let params = ProfilesCreateParams {
            name: "Sam".into(),
            color: None,
            pin: "1357".into(),
            locked: true,
            revision: "0".repeat(64),
        };
        let plain = c.call::<system::ProfilesCreate>(&params).await.unwrap();
        let secret = c
            .call_secret::<system::ProfilesCreate>(&params)
            .await
            .unwrap();
        assert_eq!(plain, secret);
        assert_eq!(secret.id, "p2");
    });
    let requests = server.join().unwrap();
    assert_eq!(requests[0], requests[1]);
    assert_eq!(requests[1]["method"], "profiles.create");
    assert_eq!(requests[1]["params"]["pin"], "1357");
    std::fs::remove_dir_all(&dir).unwrap();
}
