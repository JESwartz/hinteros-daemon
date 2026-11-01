//! The automatic on-screen keyboard end to end: a keyboard-profile app (standing in for
//! Firefox) starts the native-messaging host the way Firefox does, feeds it the extension's
//! messages, and the daemon shows and hides the keyboard.

use serde_json::json;

mod common;

use common::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_text_field_in_firefox_shows_the_keyboard() {
    let d = Daemon::start_with(|dir| {
        // Firefox starts the host by path: here a symlink named like the installed one.
        let host = dir.join("hinteros-osk-native-host");
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_hinteros-session"), &host).unwrap();
        // Native messaging: a 32-bit native-endian length (16), then the JSON.
        script(
            &dir.join("firefox"),
            &format!(
                r#"{{ printf '\020\000\000\000{{"focus":"text"}}'; sleep 1
   printf '\020\000\000\000{{"focus":"none"}}'; sleep 5; }} |
  "{host}" "{dir}/hinteros_osk.json" hinteros-osk@hinteros > "{dir}/host.out""#,
                host = host.display(),
                dir = dir.display(),
            ),
        );
        let config = dir.join("session.toml");
        std::fs::write(
            &config,
            format!(
                r#"
                [[apps]]
                id = "youtube"
                name = "YouTube"
                kind = "web"
                command = ["{}"]
                input = "keyboard"
                [osk]
                auto_show = true
                "#,
                dir.join("firefox").display()
            ),
        )
        .unwrap();
        vec!["--config".into(), config.into()]
    });
    let mut c = d.client().await;
    call(&mut c, "subscribe", json!({"events": ["osk_changed"]}))
        .await
        .unwrap();
    call(&mut c, "apps.launch", json!({"id": "youtube"}))
        .await
        .unwrap();
    assert_eq!(
        event_named(&mut c, "osk_changed").await,
        json!({"visible": true})
    );
    assert_eq!(
        event_named(&mut c, "osk_changed").await,
        json!({"visible": false})
    );
    // The host answered both messages.
    let out = std::fs::read(d.dir.path().join("host.out")).unwrap();
    let ok = br#"{"ok":true}"#;
    let mut expected = (ok.len() as u32).to_ne_bytes().to_vec();
    expected.extend_from_slice(ok);
    assert_eq!(out, [expected.clone(), expected].concat());
    assert!(d.log().contains("text field focused → on-screen keyboard"));
}
