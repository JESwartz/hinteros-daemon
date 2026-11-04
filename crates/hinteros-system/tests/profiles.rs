//! Profiles (profiles.md) against the real binary and a fake root: the list,
//! create and remove jobs, which PIN each method checks, the backoff, the switch hint, the
//! public view, first-boot mode, the greeter socket and the PAM helper's verbs.

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

use hinteros_api::ErrorCode;
use hinteros_api::client::Client;
use serde_json::{Value, json};

mod common;

use common::*;

const STATE2: &str = r#"{
  "schemaVersion": 2,
  "base": "unstable",
  "stateVersion": "26.11",
  "settings": { "ssh": { "enable": false }, "admin": { "user": "admin" } },
  "requirements": [],
  "services": [],
  "profiles": [{ "id": "p1" }, { "id": "p2" }]
}
"#;

const PROFILES: &str = r#"{"version": 1, "next": 3, "profiles": {
  "p1": {"name": "James", "color": "teal", "admin": true, "locked": false, "created_at": 0, "last_used_at": null,
         "appearance": {"text_scale": 1.0, "high_contrast": false, "reduce_motion": false}}}}"#;

/// The admin profile's PIN in these tests.
const ADMIN_PIN: &str = "2468";

/// A well-formed revision for `update.apply`.
const UPDATE_REV: &str = "9a8b7c6d5e4f30211a2b3c4d5e6f708192a3b4c5";

fn root_with(profiles: Option<&str>) -> Root {
    let root = Root::create();
    std::fs::write(root.p("etc/hinteros/state.json"), STATE2).unwrap();
    root.rm("var/lib/hinteros/profiles.json");
    if let Some(p) = profiles {
        std::fs::write(root.p("var/lib/hinteros/profiles.json"), p).unwrap();
        hinteros_system::pins::Pins::new(root.p("var/lib/hinteros/pins"))
            .set("p1", ADMIN_PIN)
            .unwrap();
    }
    // A starter env every new profile gets.
    let env = root.p("store/env-youtube");
    std::fs::create_dir_all(env.join("share/hinteros")).unwrap();
    std::fs::write(
        env.join("share/hinteros/apps.json"),
        r#"{"version":1,"ids":["youtube"],"apps":[],"box":[]}"#,
    )
    .unwrap();
    std::fs::create_dir_all(root.p("nixprofiles")).unwrap();
    std::os::unix::fs::symlink(&env, root.p("nixprofiles/starter-1-link")).unwrap();
    std::os::unix::fs::symlink("starter-1-link", root.p("nixprofiles/starter")).unwrap();
    root
}

async fn revision(c: &mut Client) -> String {
    call(c, "settings.get", json!({})).await.unwrap()["revision"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn job(sys: &System, c: &mut Client, method: &str, params: Value) -> Value {
    let r = call(c, method, params.clone())
        .await
        .unwrap_or_else(|e| panic!("{method}: {e}"));
    let id = r["job_id"].as_str().unwrap().to_owned();
    let events = until_finished(c, &id).await;
    let done = finished(&events).clone();
    assert_eq!(done["ok"], true, "{done}\n{}", sys.job_log(&id));
    r
}

/// Wait until no job runs and the profiles and the guest have their starter envs.
async fn settled(sys: &System) {
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        let mut c = sys.client().await;
        let jobs = call(&mut c, "jobs.list", json!({})).await.unwrap();
        let running = jobs["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|j| j["state"] == "running");
        if !running && sys.root.p("run/apps/hinteros-guest").is_symlink() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{jobs}\n{}",
            sys.log()
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Whether `text` shows `pin` as a token of its own (in a JSON field, a log line, a PHC
/// field), not as part of a longer run of letters and digits: audit entries carry pids,
/// timestamps and hashes, and a 4-digit PIN turns up inside those by chance.
fn shows_pin(text: &str, pin: &str) -> bool {
    let bytes = text.as_bytes();
    text.match_indices(pin).any(|(at, _)| {
        let before = at.checked_sub(1).map(|i| bytes[i]);
        let after = bytes.get(at + pin.len()).copied();
        !before.is_some_and(|b| b.is_ascii_alphanumeric())
            && !after.is_some_and(|b| b.is_ascii_alphanumeric())
    })
}

#[test]
fn a_pin_is_a_whole_token() {
    assert!(shows_pin(r#"{"pin":"1357"}"#, "1357"));
    assert!(shows_pin("pin 1357", "1357"));
    assert!(shows_pin("1357", "1357"));
    assert!(!shows_pin("1791135790", "1357"), "a timestamp");
    assert!(!shows_pin("pid=213579", "1357"), "a pid");
    assert!(
        !shows_pin("$argon2id$v=19$m=19456$aB1357cd", "1357"),
        "a hash"
    );
    assert!(!shows_pin(r#"{"pin":"***"}"#, "1357"));
}

fn public(root: &Root) -> Value {
    serde_json::from_str(&root.read("run/profiles-public.json")).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn profiles_lifecycle_and_pins() {
    let Some(sys) = System::start(root_with(Some(PROFILES)), &[]).await else {
        return;
    };
    let root = &sys.root;
    settled(&sys).await;
    let mut c = sys.subscriber().await;

    // The list: p1 is the caller and the admin; p2 is a spare (not listed).
    let l = call(&mut c, "profiles.list", json!({})).await.unwrap();
    assert_eq!(l["profiles"].as_array().unwrap().len(), 1);
    assert_eq!(l["profiles"][0]["id"], "p1");
    assert_eq!(l["profiles"][0]["pin_set"], true);
    assert_eq!(l["profiles"][0]["active"], true);
    assert_eq!(
        l["caller"],
        json!({"id": "p1", "user": "hinteros-p1", "guest": false, "spare": false})
    );
    assert_eq!(l["first_boot"], false);
    assert_eq!(l["max"], 8);
    // The public view exists from the start; the switch hint does not.
    let v = public(root);
    assert_eq!(v["profiles"][0]["name"], "James");
    assert_eq!(v["spare"], "hinteros-p2");
    assert_eq!(v["first_boot"], false);
    assert_eq!(
        std::fs::metadata(root.p("run/profiles-public.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644
    );

    // Create Sam: a rebuild adds the account; then metadata, PIN hash, starter env.
    let rev = revision(&mut c).await;
    let r = job(
        &sys,
        &mut c,
        "profiles.create",
        json!({"name": "  Sam ", "color": "orange", "pin": "1357", "locked": true, "revision": rev}),
    )
    .await;
    assert_eq!(r["id"], "p3", "the counter skips the existing p2");
    let got = call(&mut c, "jobs.get", json!({"job_id": r["job_id"]}))
        .await
        .unwrap();
    assert_eq!(
        got["job"]["subject"],
        json!({"profile": {"id": "p3", "name": "Sam"}})
    );
    let state = root.etc_state();
    assert_eq!(state["profiles"][2], json!({"id": "p3"}));
    let pf: Value = serde_json::from_str(&root.read("var/lib/hinteros/profiles.json")).unwrap();
    assert_eq!(pf["profiles"]["p3"]["name"], "Sam");
    assert_eq!(pf["profiles"]["p3"]["locked"], true);
    assert_eq!(pf["next"], 4);
    let hash = root.read("var/lib/hinteros/pins/p3");
    assert!(hash.starts_with("$argon2id$") && !shows_pin(&hash, "1357"));
    assert_eq!(
        std::fs::canonicalize(root.p("nixprofiles/apps/hinteros-p3")).unwrap(),
        root.p("store/env-youtube")
    );
    assert!(public(root)["profiles"][1]["locked"] == true);

    // Which PIN each method checks.
    let e = call(
        &mut c,
        "profiles.rename",
        json!({"id": "p3", "name": "Sammy"}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::PinRequired.code());
    assert_eq!(e["data"], json!({"pin": "admin"}));
    let e = call(
        &mut c,
        "profiles.rename",
        json!({"id": "p3", "name": "Sammy", "pin": "1357"}),
    )
    .await
    .unwrap_err();
    assert_eq!(
        e["data"]["pin"], "admin",
        "Sam's own PIN is not the admin PIN"
    );
    call(
        &mut c,
        "profiles.rename",
        json!({"id": "p3", "name": "Sammy", "pin": ADMIN_PIN}),
    )
    .await
    .unwrap();
    call(
        &mut c,
        "profiles.rename",
        json!({"id": "p1", "name": "Jim"}),
    )
    .await
    .unwrap();
    let e = call(
        &mut c,
        "profiles.set_pin",
        json!({"pin": "8642", "current_pin": "0000"}),
    )
    .await
    .unwrap_err();
    assert_eq!(e["data"]["pin"], "self");
    let e = call(&mut c, "profiles.set_pin", json!({"pin": "8642"}))
        .await
        .unwrap_err();
    assert_eq!(e["data"], json!({"pin": "self"}));
    call(
        &mut c,
        "profiles.set_locked",
        json!({"locked": true, "pin": ADMIN_PIN}),
    )
    .await
    .unwrap();
    call(
        &mut c,
        "profiles.set_appearance",
        json!({"color": "blue", "text_scale": 1.5}),
    )
    .await
    .unwrap();
    let v = public(root);
    assert_eq!(v["appearance"]["text_scale"], 1.5);
    assert_eq!(v["profiles"][0]["name"], "Jim");
    assert_eq!(v["profiles"][0]["locked"], true);
    // A bad format never reaches the PIN check.
    let e = call(&mut c, "profiles.unlock", json!({"pin": "12"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidParams.code());

    // The admin profile's PIN has no backoff: wrong is only wrong, however often.
    for _ in 0..6 {
        let e = call(&mut c, "profiles.unlock", json!({"pin": "0000"}))
            .await
            .unwrap_err();
        assert_eq!(
            e["data"],
            json!({"pin": "self"}),
            "no wait for the admin PIN"
        );
    }
    assert!(public(root)["profiles"][0]["locked_until"].is_null());
    call(&mut c, "profiles.unlock", json!({"pin": ADMIN_PIN}))
        .await
        .unwrap();

    // The switch hint: 0600, mirrored in the public view. Sam is locked: the switch needs
    // Sam's PIN first (nothing is written without it), then carries a token for Sam.
    let e = call(&mut c, "profiles.switch", json!({"id": "p3"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::PinRequired.code());
    assert_eq!(e["data"], json!({"pin": "target"}));
    assert!(!root.p("run/switch.json").exists());
    let e = call(
        &mut c,
        "profiles.switch",
        json!({"id": "p3", "pin": ADMIN_PIN}),
    )
    .await
    .unwrap_err();
    assert_eq!(
        e["data"],
        json!({"pin": "target"}),
        "the admin PIN is not Sam's"
    );
    call(
        &mut c,
        "profiles.switch",
        json!({"id": "p3", "pin": "1357"}),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::metadata(root.p("run/switch.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let hint: Value = serde_json::from_str(&root.read("run/switch.json")).unwrap();
    assert_eq!(hint["token"]["user"], "hinteros-p3");
    assert_eq!(hint["from"], "p1");
    let v = public(root);
    assert_eq!(v["next"]["target"], "p3");
    assert_eq!(v["next"]["fast"], true, "Sam's PIN was given: no UI");
    // The same caller renews its live token without the PIN (a switch still closing apps).
    call(&mut c, "profiles.switch", json!({"id": "p3"}))
        .await
        .unwrap();
    let renewed: Value = serde_json::from_str(&root.read("run/switch.json")).unwrap();
    assert_eq!(renewed["token"]["user"], "hinteros-p3");
    assert!(renewed["token"]["expires"].as_i64() >= hint["token"]["expires"].as_i64());
    // Cancelled: hint and token are gone; asking again needs the PIN again.
    call(&mut c, "profiles.switch_cancel", json!({}))
        .await
        .unwrap();
    assert!(!root.p("run/switch.json").exists());
    assert!(public(root)["next"].is_null());
    let e = call(&mut c, "profiles.switch", json!({"id": "p3"}))
        .await
        .unwrap_err();
    assert_eq!(e["data"], json!({"pin": "target"}));
    // Sam is not the admin: Sam's PIN keeps its backoff (3 free failures, then a wait).
    for _ in 0..3 {
        let e = call(
            &mut c,
            "profiles.switch",
            json!({"id": "p3", "pin": "0000"}),
        )
        .await
        .unwrap_err();
        assert_eq!(e["data"], json!({"pin": "target"}));
    }
    let e = call(
        &mut c,
        "profiles.switch",
        json!({"id": "p3", "pin": "0000"}),
    )
    .await
    .unwrap_err();
    assert_eq!(e["data"]["retry_after_ms"], 1000);
    std::thread::sleep(std::time::Duration::from_millis(2100));
    call(
        &mut c,
        "profiles.switch",
        json!({"id": "p3", "pin": "1357"}),
    )
    .await
    .unwrap();
    // For the own id a resume token.
    call(&mut c, "profiles.switch", json!({"id": "p1"}))
        .await
        .unwrap();
    let hint: Value = serde_json::from_str(&root.read("run/switch.json")).unwrap();
    assert_eq!(hint["token"]["user"], "hinteros-p1");
    assert_eq!(public(root)["next"]["fast"], true);
    let e = call(&mut c, "profiles.switch", json!({"id": "p2"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound.code(), "a spare is no target");

    // Shared display mode and audio output.
    call(
        &mut c,
        "display.save_mode",
        json!({"display": "LG TV", "width": 3840, "height": 2160, "refresh": 60}),
    )
    .await
    .unwrap();
    assert_eq!(root.read("run/gamescope-modes"), "LG TV:3840x2160@60\n");
    call(
        &mut c,
        "audio.save_output",
        json!({"node": "alsa_output.hdmi"}),
    )
    .await
    .unwrap();
    assert_eq!(root.read("run/audio-output"), "alsa_output.hdmi\n");

    // Removal: never the admin profile; others with the admin PIN; the home goes after the
    // switch, without following symlinks.
    let rev = revision(&mut c).await;
    let e = call(
        &mut c,
        "profiles.remove",
        json!({"id": "p1", "confirm_id": "p1", "pin": ADMIN_PIN, "revision": rev}),
    )
    .await
    .unwrap_err();
    assert_eq!(e["data"]["reason"], "admin");
    let e = call(
        &mut c,
        "profiles.remove",
        json!({"id": "p3", "confirm_id": "p1", "pin": ADMIN_PIN, "revision": rev}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidParams.code());
    // Sam's home, with a planted link out of it.
    let home = root.p("homes/sam");
    std::fs::create_dir_all(home.join(".local/share/Steam")).unwrap();
    std::fs::write(home.join(".local/share/Steam/save"), "x").unwrap();
    std::fs::create_dir_all(root.p("outside")).unwrap();
    std::fs::write(root.p("outside/keep"), "keep").unwrap();
    std::os::unix::fs::symlink(root.p("outside"), home.join("link")).unwrap();
    let mut passwd = root.read("passwd");
    passwd.push_str(&format!(
        "hinteros-p3:x:{}:100::{}:/bin/sh\n",
        me(),
        home.display()
    ));
    std::fs::write(root.p("passwd"), passwd).unwrap();
    job(
        &sys,
        &mut c,
        "profiles.remove",
        json!({"id": "p3", "confirm_id": "p3", "pin": ADMIN_PIN, "revision": rev}),
    )
    .await;
    assert!(!home.exists(), "the home is gone");
    assert_eq!(
        root.read("outside/keep"),
        "keep",
        "the link was not followed"
    );
    assert_eq!(root.etc_state()["profiles"].as_array().unwrap().len(), 2);
    assert!(!root.p("var/lib/hinteros/pins/p3").exists());
    assert!(!root.p("nixprofiles/apps/hinteros-p3").exists());
    assert!(!root.p("nixprofiles/apps/hinteros-p3-1-link").exists());
    let pf: Value = serde_json::from_str(&root.read("var/lib/hinteros/profiles.json")).unwrap();
    assert!(pf["profiles"].get("p3").is_none());
    // Gone from what the pickers show: the session's list and the greeter's public view.
    let l = call(&mut c, "profiles.list", json!({})).await.unwrap();
    assert!(
        l["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["id"] != "p3"),
        "{l}"
    );
    let v = public(root);
    assert!(
        v["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["id"] != "p3"),
        "{v}"
    );
    // The next profile is p4: names are never reused.
    let rev = revision(&mut c).await;
    let r = job(
        &sys,
        &mut c,
        "profiles.create",
        json!({"name": "Kim", "pin": "1111", "locked": false, "revision": rev}),
    )
    .await;
    assert_eq!(r["id"], "p4");

    // The starter set: the admin PIN, only apps the admin profile has; the guest, which has
    // installed nothing, follows it.
    std::fs::write(
        root.p("policy/catalog.json"),
        r#"{"indexVersion": 4, "entries": [
          {"id": "youtube", "kind": "app", "name": "YouTube", "status": "available"},
          {"id": "moonlight", "kind": "app", "name": "Moonlight", "status": "available"}]}"#,
    )
    .unwrap();
    let rev = call(&mut c, "apps.list", json!({})).await.unwrap()["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    job(
        &sys,
        &mut c,
        "apps.install",
        json!({"ids": ["moonlight"], "revision": rev}),
    )
    .await;
    let e = call(
        &mut c,
        "starter_set.set",
        json!({"ids": ["moonlight", "steam"], "pin": ADMIN_PIN}),
    )
    .await
    .unwrap_err();
    assert_eq!(e["data"]["reason"], "not_in_admin_profile");
    let e = call(
        &mut c,
        "starter_set.set",
        json!({"ids": ["moonlight"], "pin": "1111"}),
    )
    .await
    .unwrap_err();
    assert_eq!(e["data"]["pin"], "admin");
    assert!(root.p("run/apps/hinteros-guest").is_symlink());
    job(
        &sys,
        &mut c,
        "starter_set.set",
        json!({"ids": ["moonlight", "youtube"], "pin": ADMIN_PIN}),
    )
    .await;
    let s = call(&mut c, "starter_set.get", json!({})).await.unwrap();
    assert_eq!(s["ids"], json!(["moonlight", "youtube"]));
    assert_eq!(
        std::fs::canonicalize(root.p("nixprofiles/starter")).unwrap(),
        root.p("store/env-moonlight+youtube")
    );
    assert_eq!(
        std::fs::canonicalize(root.p("run/apps/hinteros-guest")).unwrap(),
        root.p("store/env-moonlight+youtube")
    );
    // Existing profiles are never changed by it.
    assert_eq!(
        std::fs::canonicalize(root.p("nixprofiles/apps/hinteros-p4")).unwrap(),
        root.p("store/env-youtube")
    );

    // No PIN ever reaches the audit log.
    let audit = sys.audit();
    for entry in &audit {
        for v in entry.values() {
            assert!(
                ![ADMIN_PIN, "1357", "8642"].iter().any(|p| shows_pin(v, p)),
                "{entry:?}"
            );
        }
    }
    assert!(
        audit
            .iter()
            .any(|e| e.get("HINTEROS_PIN_CHECK").map(String::as_str) == Some("admin"))
    );
    // Quoted or as a field value: a bare "1357" can turn up inside a timestamp or pid.
    let log = sys.log();
    assert!(!log.contains("\"1357\"") && !log.contains("=1357"), "{log}");

    // Guest off with the admin PIN (a rebuild).
    let e = call(
        &mut c,
        "guest.set",
        json!({"enabled": false, "admin_pin": "0001"}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::PinRequired.code());
    job(
        &sys,
        &mut c,
        "guest.set",
        json!({"enabled": false, "admin_pin": ADMIN_PIN}),
    )
    .await;
    assert_eq!(
        root.etc_state()["settings"]["guest"],
        json!({"enable": false})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn first_boot_and_the_greeter_socket() {
    // No profiles.json: first-boot mode; the caller (p1) is a spare.
    let Some(sys) = System::start(root_with(None), &[]).await else {
        return;
    };
    let root = &sys.root;
    let mut c = sys.client().await;
    let l = call(&mut c, "profiles.list", json!({})).await.unwrap();
    assert_eq!(l["first_boot"], true);
    assert_eq!(l["caller"]["spare"], true);
    assert_eq!(public(root)["first_boot"], true);
    assert_eq!(public(root)["spare"], "hinteros-p1");
    // A spare may not install apps.
    let e = call(
        &mut c,
        "apps.install",
        json!({"ids": ["youtube"], "revision": "0".repeat(64)}),
    )
    .await
    .unwrap_err();
    assert_eq!(e["data"]["reason"], "profile");
    let r = call(
        &mut c,
        "profiles.setup_admin",
        json!({"name": "James", "pin": ADMIN_PIN, "locked": false}),
    )
    .await
    .unwrap();
    assert_eq!(r, json!({"id": "p1"}));
    let l = call(&mut c, "profiles.list", json!({})).await.unwrap();
    assert_eq!(l["first_boot"], false);
    assert_eq!(l["profiles"][0]["admin"], true);
    // Only once.
    let e = call(
        &mut c,
        "profiles.setup_admin",
        json!({"name": "Again", "pin": "1111", "locked": false}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::PermissionDenied.code());
    // The new admin profile gets the starter env (a short job when the box is idle).
    eventually("the starter env for p1", || {
        std::fs::canonicalize(root.p("nixprofiles/apps/hinteros-p1")).ok()
            == Some(root.p("store/env-youtube"))
    })
    .await;

    settled(&sys).await;
    // The greeter socket serves only its few methods.
    let mut g = sys.greeter_client().await;
    let e = call(&mut g, "rebuild", json!({})).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::MethodNotFound.code());
    let e = call(&mut g, "profiles.list", json!({})).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::MethodNotFound.code());
    call(
        &mut g,
        "subscribe",
        json!({"events": ["job_started", "job_progress", "job_finished"]}),
    )
    .await
    .unwrap();
    // settings.get is there for profiles.create's revision.
    let rev = revision(&mut g).await;
    let r = call(
        &mut g,
        "profiles.create",
        json!({"name": "Sam", "pin": "1357", "locked": true, "revision": rev}),
    )
    .await
    .unwrap();
    let id = r["job_id"].as_str().unwrap().to_owned();
    let events = until_finished(&mut g, &id).await;
    assert_eq!(finished(&events)["ok"], true, "{}", sys.job_log(&id));
    // From the greeter: a one-time token lets the greeter straight in.
    let hint: Value = serde_json::from_str(&root.read("run/switch.json")).unwrap();
    assert_eq!(hint["token"]["user"], "hinteros-p3");
    assert_eq!(public(root)["next"]["fast"], true);
}

/// A `profiles.json` this binary cannot read (damaged, or newer after a rollback) fails closed:
/// not first-boot mode, no spare, no admin PIN accepted, and the file is never overwritten.
#[tokio::test(flavor = "multi_thread")]
async fn an_unreadable_profiles_file_fails_closed() {
    let newer = PROFILES.replace(r#""version": 1"#, r#""version": 2"#);
    for bad in [newer.as_str(), r#"{"version": 1, "next""#] {
        let Some(sys) = System::start(root_with(Some(bad)), &[]).await else {
            return;
        };
        let root = &sys.root;
        let mut c = sys.client().await;
        let l = call(&mut c, "profiles.list", json!({})).await.unwrap();
        assert_eq!(l["first_boot"], false, "{bad}");
        assert_eq!(l["profiles"], json!([]));
        assert_eq!(public(root)["first_boot"], false);
        assert_eq!(public(root)["spare"], Value::Null);
        let e = call(
            &mut c,
            "profiles.setup_admin",
            json!({"name": "Kid", "pin": "1111", "locked": false}),
        )
        .await
        .unwrap_err();
        assert_eq!(code(&e), ErrorCode::PermissionDenied.code(), "{e}");
        for (method, params) in [
            ("rollback", json!({"admin_pin": ADMIN_PIN})),
            ("time_zone.set", json!({"zone": "UTC"})),
        ] {
            let e = call(&mut c, method, params).await.unwrap_err();
            assert_eq!(code(&e), ErrorCode::PinRequired.code(), "{method}: {e}");
        }
        assert!(!pam_ok(root, "unlocked", "hinteros-p1"));
        assert_eq!(root.read("var/lib/hinteros/profiles.json"), bad);
    }
}

/// `hinteros-system pin <verb>` as pam_exec runs it: `PAM_USER`, the PIN on stdin, status lines
/// on stdout.
#[test]
fn pam_helper_verbs() {
    let root = root_with(Some(PROFILES));
    let pin = |verb: &str, user: &str, stdin: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_hinteros-system"))
            .args(["pin", verb])
            .arg("--state-dir")
            .arg(root.p("var/lib/hinteros"))
            .arg("--run-dir")
            .arg(root.p("run"))
            .arg("--config-dir")
            .arg(root.p("etc/hinteros"))
            .arg("--passwd-file")
            .arg(root.p("passwd"))
            .arg("--group-file")
            .arg(root.p("group"))
            .env_clear()
            .env("PAM_USER", user)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    // p1 is unlocked; p2 is a spare outside first-boot mode; root and the admin account never.
    assert!(pin("unlocked", "hinteros-p1", "").0);
    assert!(!pin("unlocked", "hinteros-p2", "").0);
    assert!(!pin("unlocked", "root", "").0);
    assert!(!pin("unlocked", "admin", "").0);
    assert!(pin("unlocked", "hinteros-guest", "").0);
    // verify: the PIN from pam_exec (NUL-terminated).
    assert_eq!(
        pin("verify", "hinteros-p1", "2468\0"),
        (true, String::new())
    );
    assert_eq!(
        pin("verify", "hinteros-p1", "0000\0"),
        (false, "hinteros:wrong\n".into())
    );
    // p1 is the admin profile: no backoff, wrong stays just wrong (DECISIONS J4).
    for _ in 0..5 {
        assert_eq!(
            pin("verify", "hinteros-p1", "0000\0"),
            (false, "hinteros:wrong\n".into())
        );
    }
    assert_eq!(
        pin("verify", "hinteros-p1", "2468\0"),
        (true, String::new())
    );
    // end-others with nobody else running: succeeds, touches last_used_at.
    assert!(pin("end-others", "hinteros-p1", "").0);
    let pf: Value = serde_json::from_str(&root.read("var/lib/hinteros/profiles.json")).unwrap();
    assert!(pf["profiles"]["p1"]["last_used_at"].is_i64());
}

/// Run the PAM helper as pam_exec would (exit status only).
fn pam_ok(root: &Root, verb: &str, user: &str) -> bool {
    Command::new(env!("CARGO_BIN_EXE_hinteros-system"))
        .args(["pin", verb])
        .arg("--state-dir")
        .arg(root.p("var/lib/hinteros"))
        .arg("--run-dir")
        .arg(root.p("run"))
        .arg("--config-dir")
        .arg(root.p("etc/hinteros"))
        .arg("--passwd-file")
        .arg(root.p("passwd"))
        .arg("--group-file")
        .arg(root.p("group"))
        .env_clear()
        .env("PAM_USER", user)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// A forgotten PIN: `sudo hinteros-system reset-pin <profile>` (the CLI's code path, as root
/// would run it) removes the PIN and the wait while the service runs; the picker then lets the
/// profile in without a PIN (even though it is locked), its session sees `pin_set: false`, and
/// `set_pin` without the current PIN sets the new one.
#[tokio::test(flavor = "multi_thread")]
async fn reset_pin_from_the_terminal() {
    let Some(sys) = System::start(root_with(Some(PROFILES)), &[]).await else {
        return;
    };
    let root = &sys.root;
    settled(&sys).await;
    let mut c = sys.client().await;
    let pins = hinteros_system::pins::Pins::new(root.p("var/lib/hinteros/pins"));

    // p1 locked with its PIN, then wrong PINs: the admin profile has no wait (DECISIONS J4).
    call(
        &mut c,
        "profiles.set_locked",
        json!({"locked": true, "pin": ADMIN_PIN}),
    )
    .await
    .unwrap();
    for _ in 0..4 {
        let e = call(
            &mut c,
            "profiles.set_locked",
            json!({"locked": true, "pin": "0000"}),
        )
        .await
        .unwrap_err();
        assert_eq!(code(&e), ErrorCode::PinRequired.code(), "{e}");
    }
    assert_eq!(pins.backoff("p1").failures, 0);
    assert!(!pam_ok(root, "unlocked", "hinteros-p1"));

    // Not root: the binary refuses (and changes nothing).
    if !rustix::process::geteuid().is_root() {
        let out = Command::new(env!("CARGO_BIN_EXE_hinteros-system"))
            .args(["reset-pin", "p1", "--state-dir"])
            .arg(root.p("var/lib/hinteros"))
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("must be run as root"));
        assert!(pins.is_set("p1"));
    }

    // The reset, by name; unknown names are refused.
    let paths = hinteros_system::pam::paths(
        root.p("var/lib/hinteros"),
        root.p("run"),
        root.p("etc/hinteros"),
        hinteros_system::users::Users {
            passwd: Some(root.p("passwd")),
            group: Some(root.p("group")),
        },
    );
    let audit = hinteros_system::audit::Audit::new(root.p("journal.sock"));
    assert!(hinteros_system::resetpin::reset(&paths, "Nobody", &audit, None, 0).is_err());
    let listed = hinteros_system::resetpin::list(&paths).unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].pin_set && listed[0].locked && listed[0].admin);
    let done = hinteros_system::resetpin::reset(&paths, "James", &audit, Some("admin"), 0).unwrap();
    assert_eq!((done.id.as_str(), done.had_pin), ("p1", true));
    assert!(!pins.is_set("p1"));
    assert_eq!(
        pins.backoff("p1"),
        hinteros_system::pins::Backoff::default()
    );
    // The public view no longer shows a wait; the audit names the profile, never a PIN.
    assert!(public(root)["profiles"][0]["locked_until"].is_null());
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        let found = sys
            .journal
            .lock()
            .unwrap()
            .iter()
            .any(|m| m.get("HINTEROS_METHOD").map(String::as_str) == Some("reset-pin"));
        if found {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "no audit entry");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    for m in sys.journal.lock().unwrap().iter() {
        assert!(!m.values().any(|v| shows_pin(v, ADMIN_PIN)), "{m:?}");
    }

    // The picker: enterable without a PIN although still locked.
    assert!(pam_ok(root, "unlocked", "hinteros-p1"));
    // The session: "Set your PIN" (pin_set false); set_pin needs no current PIN.
    let l = call(&mut c, "profiles.list", json!({})).await.unwrap();
    assert_eq!(l["profiles"][0]["pin_set"], false);
    assert_eq!(l["profiles"][0]["locked"], true);
    call(&mut c, "profiles.set_pin", json!({"pin": "8642"}))
        .await
        .unwrap();
    let l = call(&mut c, "profiles.list", json!({})).await.unwrap();
    assert_eq!(l["profiles"][0]["pin_set"], true);
    // From now on the new PIN is the one, and the picker asks for it again.
    assert!(!pam_ok(root, "unlocked", "hinteros-p1"));
    call(
        &mut c,
        "profiles.set_locked",
        json!({"locked": true, "pin": "8642"}),
    )
    .await
    .unwrap();
}

/// The admin PIN has no wait but ten wrong tries a boot (owner): the API and the PAM helper
/// share the count; the tenth refuses every later check, the right PIN's too, with
/// `reboot_required` (audited once); other profiles keep their backoff; `reset-pin` ends it.
#[tokio::test(flavor = "multi_thread")]
async fn the_admin_pin_locks_until_a_reboot() {
    let Some(sys) = System::start(root_with(Some(PROFILES)), &[]).await else {
        return;
    };
    let root = &sys.root;
    settled(&sys).await;
    let mut c = sys.client().await;
    let pins = hinteros_system::pins::Pins::new(root.p("var/lib/hinteros/pins"));
    let wrong = || json!({"admin_pin": "1111"});
    for _ in 0..8 {
        let e = call(&mut c, "rollback", wrong()).await.unwrap_err();
        assert_eq!(e["data"], json!({"pin": "admin"}), "{e}");
    }
    // The ninth from the greeter (the PAM helper), the tenth from the API.
    let pam_verify = |pin: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_hinteros-system"))
            .args(["pin", "verify", "--state-dir"])
            .arg(root.p("var/lib/hinteros"))
            .arg("--run-dir")
            .arg(root.p("run"))
            .arg("--config-dir")
            .arg(root.p("etc/hinteros"))
            .arg("--passwd-file")
            .arg(root.p("passwd"))
            .arg("--group-file")
            .arg(root.p("group"))
            .arg("--journal-socket")
            .arg(root.p("no-journal.sock"))
            .env_clear()
            .env("PAM_USER", "hinteros-p1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(format!("{pin}\0").as_bytes()).unwrap();
        drop(stdin);
        String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).into_owned()
    };
    assert_eq!(pam_verify("0000"), "hinteros:wrong\n");
    assert_eq!(pins.admin_tries("p1"), 9);
    let e = call(&mut c, "rollback", wrong()).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::PinRequired.code(), "{e}");
    assert_eq!(e["data"], json!({"pin": "admin", "reboot_required": true}));
    // Now even the right PIN, on every path and for every admin check.
    let e = call(&mut c, "rollback", json!({"admin_pin": ADMIN_PIN}))
        .await
        .unwrap_err();
    assert_eq!(e["data"], json!({"pin": "admin", "reboot_required": true}));
    let e = call(
        &mut c,
        "profiles.set_locked",
        json!({"locked": true, "pin": ADMIN_PIN}),
    )
    .await
    .unwrap_err();
    assert_eq!(e["data"], json!({"pin": "self", "reboot_required": true}));
    assert_eq!(pam_verify(ADMIN_PIN), "hinteros:reboot\n");
    // Audited once, when it started; never a PIN.
    let deadline = std::time::Instant::now() + TIMEOUT;
    let started = loop {
        let n = sys
            .audit()
            .iter()
            .filter(|e| {
                e.get("HINTEROS_PIN_OUTCOME").map(String::as_str) == Some("lockout_started")
            })
            .count();
        if n > 0 || std::time::Instant::now() > deadline {
            break n;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(started, 1);
    for entry in &sys.audit() {
        assert!(
            !entry.values().any(|v| shows_pin(v, ADMIN_PIN)),
            "{entry:?}"
        );
    }
    // reset-pin (root) removes the PIN and the count: in without a PIN, then a new one.
    let paths = hinteros_system::pam::paths(
        root.p("var/lib/hinteros"),
        root.p("run"),
        root.p("etc/hinteros"),
        hinteros_system::users::Users {
            passwd: Some(root.p("passwd")),
            group: Some(root.p("group")),
        },
    );
    let audit = hinteros_system::audit::Audit::new(root.p("journal.sock"));
    hinteros_system::resetpin::reset(&paths, "p1", &audit, None, 0).unwrap();
    assert_eq!(pins.admin_tries("p1"), 0);
    call(&mut c, "profiles.set_pin", json!({"pin": "8642"}))
        .await
        .unwrap();
    call(
        &mut c,
        "profiles.set_locked",
        json!({"locked": true, "pin": "8642"}),
    )
    .await
    .unwrap();
}

/// Rollback, channel and base switches, hardware re-detect and installing a system update
/// (update.apply) need the admin profile's PIN
/// Missing or wrong is `pin_required {pin: admin}` before anything else
/// happens; right, the method goes on as before. Without an admin profile (first-boot mode)
/// they are refused like every admin action; an admin profile without a PIN yet passes for
/// itself.
#[tokio::test(flavor = "multi_thread")]
async fn box_wide_actions_need_the_admin_pin() {
    let Some(sys) = System::start(root_with(Some(PROFILES)), &[]).await else {
        return;
    };
    let root = &sys.root;
    settled(&sys).await;
    let mut c = sys.subscriber().await;
    let guarded = [
        ("rollback", json!({})),
        ("channel.set", json!({"channel": "beta"})),
        // Already on unstable: the PIN is still checked first.
        ("base.set", json!({"base": "unstable"})),
        ("hardware.redetect", json!({})),
        // Installing a system update, also before the revision is looked at.
        ("update.apply", json!({"rev": UPDATE_REV})),
    ];
    for (method, params) in &guarded {
        let e = call(&mut c, method, params.clone()).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::PinRequired.code(), "{method}: {e}");
        assert_eq!(e["data"], json!({"pin": "admin"}), "{method}");
        let mut wrong = params.clone();
        wrong["admin_pin"] = json!("1111");
        let e = call(&mut c, method, wrong).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::PinRequired.code(), "{method}: {e}");
        assert_eq!(
            e["data"],
            json!({"pin": "admin"}),
            "{method}: no backoff for the admin PIN"
        );
        let mut bad = params.clone();
        bad["admin_pin"] = json!("12");
        let e = call(&mut c, method, bad).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidParams.code(), "{method}");
        assert_eq!(e["data"]["field"], "admin_pin");
    }
    // The right PIN: each goes on as before.
    let with = |p: &Value| {
        let mut p = p.clone();
        p["admin_pin"] = json!(ADMIN_PIN);
        p
    };
    let e = call(&mut c, "rollback", with(&guarded[0].1))
        .await
        .unwrap_err();
    assert_eq!(
        code(&e),
        ErrorCode::NotFound.code(),
        "only one generation: {e}"
    );
    let e = call(&mut c, "channel.set", with(&guarded[1].1))
        .await
        .unwrap_err();
    assert_ne!(code(&e), ErrorCode::PinRequired.code(), "{e}");
    let r = call(&mut c, "base.set", with(&guarded[2].1)).await.unwrap();
    assert_eq!(r["job_id"], Value::Null);
    job(&sys, &mut c, "hardware.redetect", with(&guarded[3].1)).await;
    assert!(root.read("facter.log").contains("-o"));
    // Past the PIN, update.apply looks at the revision: nothing was checked, so it is refused.
    let e = call(&mut c, "update.apply", with(&guarded[4].1))
        .await
        .unwrap_err();
    assert_ne!(code(&e), ErrorCode::PinRequired.code(), "{e}");
    assert_ne!(code(&e), ErrorCode::InvalidParams.code(), "{e}");
    // update.check stays open: no PIN asked.
    if let Err(e) = call(&mut c, "update.check", json!({})).await {
        assert_ne!(code(&e), ErrorCode::PinRequired.code(), "{e}");
    }
    // The audit names the PIN that was checked, never the PIN.
    let audit = sys.audit();
    assert!(audit.iter().any(|e| {
        e.get("HINTEROS_METHOD").map(String::as_str) == Some("rollback")
            && e.get("HINTEROS_PIN_CHECK").map(String::as_str) == Some("admin")
    }));
    for entry in &audit {
        assert!(
            !entry.values().any(|v| shows_pin(v, ADMIN_PIN)),
            "{entry:?}"
        );
    }

    // An admin profile without a PIN yet passes without one (the caller is the admin profile).
    std::fs::remove_file(root.p("var/lib/hinteros/pins/p1")).unwrap();
    let r = call(&mut c, "base.set", json!({"base": "unstable"}))
        .await
        .unwrap();
    assert_eq!(r["job_id"], Value::Null);
}

#[tokio::test(flavor = "multi_thread")]
async fn box_wide_actions_without_an_admin_profile() {
    // First-boot mode: no admin profile, so no admin PIN can be given.
    let Some(sys) = System::start(root_with(None), &[]).await else {
        return;
    };
    let mut c = sys.client().await;
    for (method, params) in [
        ("rollback", json!({"admin_pin": ADMIN_PIN})),
        (
            "base.set",
            json!({"base": "unstable", "admin_pin": ADMIN_PIN}),
        ),
        (
            "update.apply",
            json!({"rev": UPDATE_REV, "admin_pin": ADMIN_PIN}),
        ),
    ] {
        let e = call(&mut c, method, params).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::PinRequired.code(), "{method}: {e}");
        assert_eq!(e["data"]["pin"], "admin");
    }
}

/// While the admin profile has no PIN (after `reset-pin`), only the admin profile itself passes
/// the admin-PIN checks; another profile gets `pin_required {pin: admin}`.
#[tokio::test(flavor = "multi_thread")]
async fn an_admin_profile_without_a_pin_passes_only_for_itself() {
    // p2 is the admin profile, without a PIN; the caller is p1, an ordinary profile.
    let profiles = r#"{"version": 1, "next": 3, "profiles": {
      "p1": {"name": "Sam", "color": "teal"},
      "p2": {"name": "James", "color": "blue", "admin": true}}}"#;
    let Some(sys) = System::start(root_with(Some(profiles)), &[]).await else {
        return;
    };
    settled(&sys).await;
    let mut c = sys.client().await;
    for (method, params) in [
        ("base.set", json!({"base": "unstable"})),
        ("profiles.rename", json!({"id": "p2", "name": "X"})),
        (
            "profiles.reset_pin",
            json!({"id": "p1", "new_pin": "1111", "admin_pin": "0000"}),
        ),
    ] {
        let e = call(&mut c, method, params).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::PinRequired.code(), "{method}: {e}");
        assert_eq!(e["data"], json!({"pin": "admin"}), "{method}");
    }
}

/// The test's uid as the guest (p1 moves to another uid), with a home under the root.
fn become_the_guest(root: &Root) -> std::path::PathBuf {
    let home = root.p("guesthomes/hinteros-guest");
    std::fs::create_dir_all(&home).unwrap();
    let passwd = root
        .read("passwd")
        .lines()
        .map(|l| {
            if l.starts_with("hinteros-p1:") {
                format!(
                    "hinteros-p1:x:20001:100::{}:/bin/sh",
                    root.p("home").display()
                )
            } else if l.starts_with("hinteros-guest:") {
                format!("hinteros-guest:x:{}:100::{}:/bin/sh", me(), home.display())
            } else {
                l.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(root.p("passwd"), passwd + "\n").unwrap();
    home
}

/// End guest session: the guest ends its own session and the job wipes it now:
/// its home (never following a link out of it), its entries in the shared temp directories,
/// its app env (back to the starter env), its terms, usage entry and a switch hint into it.
#[tokio::test(flavor = "multi_thread")]
async fn end_guest_session_wipes_now() {
    let root = root_with(Some(PROFILES));
    let home = become_the_guest(&root);
    std::fs::create_dir_all(home.join(".local/share/Steam/config")).unwrap();
    std::fs::write(home.join(".local/share/Steam/config/loginusers.vdf"), "x").unwrap();
    std::fs::create_dir_all(home.join(".mozilla/firefox")).unwrap();
    std::fs::write(home.join(".mozilla/firefox/cookies.sqlite"), "x").unwrap();
    std::fs::create_dir_all(root.p("outside")).unwrap();
    std::fs::write(root.p("outside/keep"), "keep").unwrap();
    std::os::unix::fs::symlink(root.p("outside"), home.join("link-out")).unwrap();
    std::fs::write(root.p("tmp/guest-file"), "x").unwrap();
    std::fs::write(
        root.p("var/lib/hinteros/terms-accepted.json"),
        r#"{"version": 1, "accepted": {"steam": [{"version": "1", "accepted_at": "2026-10-01T00:00:00Z", "by": "hinteros-guest"},
                      {"version": "1", "accepted_at": "2026-10-01T00:00:00Z", "by": "hinteros-p1", "profile": "p1"}]}}"#,
    )
    .unwrap();
    std::fs::write(
        root.p("var/lib/hinteros/usage.json"),
        r#"{"version": 1, "data": {"guest": {"total_bytes": 5, "steam_games_bytes": 0, "at": 1}}}"#,
    )
    .unwrap();
    // This is a new boot for the service: the boot clean-up must not run before the test.
    std::fs::write(root.p("run/guest-boot-clean"), "").unwrap();
    let me_s = me().to_string();
    let Some(sys) = System::start(root, &["--guest-uid", &me_s]).await else {
        return;
    };
    let root = &sys.root;
    settled(&sys).await;
    let mut c = sys.subscriber().await;
    let l = call(&mut c, "profiles.list", json!({})).await.unwrap();
    assert_eq!(l["caller"]["guest"], true, "{l}");
    // The guest installed something: its link points at another env.
    let own = root.p("store/env-guest-own");
    std::fs::create_dir_all(own.join("share/hinteros")).unwrap();
    std::fs::write(
        own.join("share/hinteros/apps.json"),
        r#"{"version":1,"ids":["moonlight","youtube"],"apps":[],"box":[]}"#,
    )
    .unwrap();
    std::fs::remove_file(root.p("run/apps/hinteros-guest")).unwrap();
    std::os::unix::fs::symlink(&own, root.p("run/apps/hinteros-guest")).unwrap();
    std::fs::write(
        root.p("run/switch.json"),
        format!(
            r#"{{"target": "guest", "from": "p1", "expires": {}, "token": null}}"#,
            i64::MAX / 2
        ),
    )
    .unwrap();

    let r = job(&sys, &mut c, "guest.end", json!({})).await;
    assert!(r["job_id"].is_string());
    let j = call(&mut c, "jobs.get", json!({"job_id": r["job_id"]}))
        .await
        .unwrap();
    assert_eq!(j["job"]["subject"], json!({"owner": "guest"}));
    assert!(home.is_dir(), "the home itself stays");
    assert_eq!(
        std::fs::read_dir(&home).unwrap().count(),
        0,
        "the home is empty"
    );
    assert_eq!(
        root.read("outside/keep"),
        "keep",
        "the link was not followed"
    );
    assert!(!root.p("tmp/guest-file").exists());
    assert_eq!(
        std::fs::canonicalize(root.p("run/apps/hinteros-guest")).unwrap(),
        root.p("store/env-youtube"),
        "the starter env again"
    );
    let terms: Value =
        serde_json::from_str(&root.read("var/lib/hinteros/terms-accepted.json")).unwrap();
    assert_eq!(terms["accepted"]["steam"].as_array().unwrap().len(), 1);
    assert_eq!(terms["accepted"]["steam"][0]["by"], "hinteros-p1");
    let usage: Value = serde_json::from_str(&root.read("var/lib/hinteros/usage.json")).unwrap();
    assert!(usage["data"].get("guest").is_none());
    assert!(!root.p("run/switch.json").exists());
    assert!(!root.p("run/guest-ending").exists());
    assert_eq!(public(root)["guest"]["enabled"], true, "the guest is back");
    assert!(pam_ok(root, "unlocked", "hinteros-guest"));
    // While a guest_end job runs, the picker hides the guest and PAM refuses it.
    let id = "0123456789abcdef";
    std::fs::create_dir_all(root.p(&format!("run/jobs/{id}"))).unwrap();
    std::fs::write(root.p("run/guest-ending"), id).unwrap();
    assert!(!pam_ok(root, "unlocked", "hinteros-guest"));
    std::fs::write(root.p(&format!("run/jobs/{id}/status.json")), "{}").unwrap();
    assert!(
        pam_ok(root, "unlocked", "hinteros-guest"),
        "a finished job's marker is stale"
    );
}

/// guest.end only for the guest, and only for exactly the guest account.
#[tokio::test(flavor = "multi_thread")]
async fn end_guest_session_refusals() {
    // A profile (p1) may not.
    let Some(sys) = System::start(root_with(Some(PROFILES)), &[]).await else {
        return;
    };
    settled(&sys).await;
    let mut c = sys.client().await;
    let e = call(&mut c, "guest.end", json!({})).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::PermissionDenied.code(), "{e}");
    assert_eq!(e["data"]["reason"], "profile");
    drop(c);
    drop(sys);

    // The guest, but its uid is not the box's guest uid (20000 here): nothing is touched.
    let root = root_with(Some(PROFILES));
    let home = become_the_guest(&root);
    std::fs::write(home.join("keep"), "x").unwrap();
    std::fs::write(root.p("tmp/keep"), "x").unwrap();
    let Some(sys) = System::start(root, &[]).await else {
        return;
    };
    let mut c = sys.client().await;
    let e = call(&mut c, "guest.end", json!({})).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::InternalError.code(), "{e}");
    assert!(
        e["message"].as_str().unwrap().contains("expected 20000"),
        "{e}"
    );
    assert!(home.join("keep").exists());
    assert!(sys.root.p("tmp/keep").exists());
    let jobs = call(&mut c, "jobs.list", json!({})).await.unwrap();
    assert!(
        jobs["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|j| j["kind"] != "guest_end"),
        "{jobs}"
    );
}
