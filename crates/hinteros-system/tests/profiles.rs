//! Profiles (system API 0.4, profiles.md) against the real binary and a fake root: the list,
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
  "settings": { "ssh": { "enable": false }, "admin": { "user": "admin" } },
  "requirements": [],
  "services": [],
  "profiles": [{ "id": "p1", "user": "hinteros" }, { "id": "p2", "user": "hinteros-p2" }]
}
"#;

const PROFILES: &str = r#"{"version": 1, "next": 3, "profiles": {
  "p1": {"name": "James", "color": "teal", "admin": true, "locked": false, "created_at": 0, "last_used_at": null,
         "appearance": {"text_scale": 1.0, "high_contrast": false, "reduce_motion": false}}}}"#;

/// The admin profile's PIN in these tests.
const ADMIN_PIN: &str = "2468";

fn root_with(profiles: Option<&str>) -> Root {
    let root = Root::create();
    std::fs::write(root.p("etc/hinteros/state.json"), STATE2).unwrap();
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
        json!({"id": "p1", "user": "hinteros", "guest": false, "spare": false})
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
    let state = root.etc_state();
    assert_eq!(
        state["profiles"][2],
        json!({"id": "p3", "user": "hinteros-p3"})
    );
    let pf: Value = serde_json::from_str(&root.read("var/lib/hinteros/profiles.json")).unwrap();
    assert_eq!(pf["profiles"]["p3"]["name"], "Sam");
    assert_eq!(pf["profiles"]["p3"]["locked"], true);
    assert_eq!(pf["next"], 4);
    let hash = root.read("var/lib/hinteros/pins/p3");
    assert!(hash.starts_with("$argon2id$") && !hash.contains("1357"));
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
    assert_eq!(hint["token"]["user"], "hinteros");
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
        r#"{"indexVersion": 3, "apps": [
          {"id": "youtube", "kind": "app", "type": "web", "name": "YouTube", "status": "available"},
          {"id": "moonlight", "kind": "app", "type": "native", "name": "Moonlight", "status": "available"}]}"#,
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
                !v.contains(ADMIN_PIN) && !v.contains("1357") && !v.contains("8642"),
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
    assert_eq!(public(root)["spare"], "hinteros");
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
        std::fs::canonicalize(root.p("nixprofiles/apps/hinteros")).ok()
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
    assert!(pin("unlocked", "hinteros", "").0);
    assert!(!pin("unlocked", "hinteros-p2", "").0);
    assert!(!pin("unlocked", "root", "").0);
    assert!(!pin("unlocked", "admin", "").0);
    assert!(pin("unlocked", "hinteros-guest", "").0);
    // verify: the PIN from pam_exec (NUL-terminated).
    assert_eq!(pin("verify", "hinteros", "2468\0"), (true, String::new()));
    assert_eq!(
        pin("verify", "hinteros", "0000\0"),
        (false, "hinteros:wrong\n".into())
    );
    // p1 is the admin profile: no backoff, wrong stays just wrong (DECISIONS J4).
    for _ in 0..5 {
        assert_eq!(
            pin("verify", "hinteros", "0000\0"),
            (false, "hinteros:wrong\n".into())
        );
    }
    assert_eq!(pin("verify", "hinteros", "2468\0"), (true, String::new()));
    // end-others with nobody else running: succeeds, touches last_used_at.
    assert!(pin("end-others", "hinteros", "").0);
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
    assert!(!pam_ok(root, "unlocked", "hinteros"));

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
        assert!(!m.values().any(|v| v.contains(ADMIN_PIN)), "{m:?}");
    }

    // The picker: enterable without a PIN although still locked.
    assert!(pam_ok(root, "unlocked", "hinteros"));
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
    assert!(!pam_ok(root, "unlocked", "hinteros"));
    call(
        &mut c,
        "profiles.set_locked",
        json!({"locked": true, "pin": "8642"}),
    )
    .await
    .unwrap();
}
