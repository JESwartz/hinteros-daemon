//! Time zone (system API 0.8, time-zone.md) against the real binary and a fake root: the zone
//! and list from tzdata fixtures, `time_zone.set` (state.json, the localtime link, the event),
//! its errors, the admin PIN and first-boot mode, a job that runs across a change, and a
//! rollback that restores another zone.

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
  "profiles": [{ "id": "p1", "user": "hinteros" }]
}
"#;

const PROFILES: &str = r#"{"version": 1, "next": 2, "profiles": {
  "p1": {"name": "James", "color": "teal", "admin": true, "locked": false, "created_at": 0, "last_used_at": null,
         "appearance": {"text_scale": 1.0, "high_contrast": false, "reduce_motion": false}}}}"#;

const ADMIN_PIN: &str = "2468";

/// A schema-2 root; with an admin profile (PIN [`ADMIN_PIN`]) unless `first_boot`.
fn root2(first_boot: bool) -> Root {
    let root = Root::create();
    std::fs::write(root.p("etc/hinteros/state.json"), STATE2).unwrap();
    if !first_boot {
        std::fs::write(root.p("var/lib/hinteros/profiles.json"), PROFILES).unwrap();
        hinteros_system::pins::Pins::new(root.p("var/lib/hinteros/pins"))
            .set("p1", ADMIN_PIN)
            .unwrap();
    }
    // A starter env, so the service's own env jobs have something to install.
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

/// A client subscribed to `time_zone_changed` only.
async fn zone_watcher(sys: &System) -> Client {
    let mut c = sys.client().await;
    call(
        &mut c,
        "subscribe",
        json!({"events": ["time_zone_changed"]}),
    )
    .await
    .unwrap();
    c
}

async fn zone_event(c: &mut Client) -> Value {
    let (name, params) = next_event(c).await;
    assert_eq!(name, "time_zone_changed");
    params
}

fn localtime(root: &Root) -> std::path::PathBuf {
    std::fs::read_link(root.p("etc/localtime")).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn get_list_set_and_a_job_across_it() {
    let Some(sys) = System::start(Root::create(), &[]).await else {
        return;
    };
    let root = &sys.root;
    let mut c = sys.subscriber().await;
    let mut w = zone_watcher(&sys).await;

    // Never set: UTC.
    let r = call(&mut c, "time_zone.get", json!({})).await.unwrap();
    assert_eq!(
        r,
        json!({"zone": "UTC", "offset_seconds": 0, "standard_offset_seconds": 0, "offset_valid_until": null})
    );
    // The list: the tab's zones that have a file, plus UTC, sorted.
    let r = call(&mut c, "time_zone.list", json!({})).await.unwrap();
    let zones: Vec<&str> = r["zones"]
        .as_array()
        .unwrap()
        .iter()
        .map(|z| z["zone"].as_str().unwrap())
        .collect();
    assert_eq!(zones.len(), 8, "{zones:?}");
    assert!(zones.is_sorted());
    assert!(zones.contains(&"UTC") && zones.contains(&"America/Chicago"));
    assert!(!zones.contains(&"Europe/Paris"), "no file, not offered");
    let kolkata = r["zones"]
        .as_array()
        .unwrap()
        .iter()
        .find(|z| z["zone"] == "Asia/Kolkata")
        .unwrap();
    assert_eq!(
        *kolkata,
        json!({"zone": "Asia/Kolkata", "offset_seconds": 19800, "standard_offset_seconds": 19800})
    );

    // Bad names and zones tzdata does not have.
    for (zone, reason) in [
        ("../../etc/shadow", "bad_format"),
        ("/etc/passwd", "bad_format"),
        ("America/../UTC", "bad_format"),
        ("Mars/Olympus_Mons", "unknown"),
        ("Europe/Paris", "unknown"),
        ("America", "unknown"),
    ] {
        let e = call(&mut c, "time_zone.set", json!({"zone": zone}))
            .await
            .unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidParams.code(), "{zone}: {e}");
        assert_eq!(
            e["data"],
            json!({"field": "zone", "reason": reason}),
            "{zone}"
        );
    }
    let e = call(&mut c, "time_zone.set", json!({"zone": "UTC", "x": 1}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidParams.code());
    assert!(!root.p("etc/localtime").exists(), "nothing changed");
    assert!(root.etc_state()["settings"].get("timeZone").is_none());

    // A change while a job builds: no job_busy, at once, and the job keeps it.
    root.touch("build-hold");
    let r = call(&mut c, "ssh.set", json!({"enabled": true}))
        .await
        .unwrap();
    let job = r["job_id"].as_str().unwrap().to_owned();
    eventually("the build to start", || {
        root.read("nix.log").contains("build")
    })
    .await;
    let r = call(&mut c, "time_zone.set", json!({"zone": "America/Chicago"}))
        .await
        .unwrap();
    assert_eq!(r["zone"], "America/Chicago");
    assert!(
        [-5 * 3600, -6 * 3600].contains(&r["offset_seconds"].as_i64().unwrap()),
        "{r}"
    );
    assert_eq!(r["standard_offset_seconds"], -6 * 3600);
    assert!(r["offset_valid_until"].as_i64().unwrap() > 1_790_000_000);
    assert_eq!(localtime(root), zoneinfo_fixtures().join("America/Chicago"));
    assert_eq!(root.etc_state()["settings"]["timeZone"], "America/Chicago");
    assert_eq!(zone_event(&mut w).await, r);
    assert_eq!(call(&mut c, "time_zone.get", json!({})).await.unwrap(), r);
    root.rm("build-hold");
    let done = finished(&until_finished(&mut c, &job).await).clone();
    assert_eq!(done["ok"], true, "{}", sys.job_log(&job));
    let state = root.etc_state();
    assert_eq!(state["settings"]["ssh"]["enable"], true);
    assert_eq!(
        state["settings"]["timeZone"], "America/Chicago",
        "the job kept the zone set while it ran"
    );
    assert_eq!(localtime(root), zoneinfo_fixtures().join("America/Chicago"));

    // The same zone again: no change, no event; then another one.
    let again = call(&mut c, "time_zone.set", json!({"zone": "America/Chicago"}))
        .await
        .unwrap();
    assert_eq!(again["zone"], "America/Chicago");
    let r = call(&mut c, "time_zone.set", json!({"zone": "Asia/Tokyo"}))
        .await
        .unwrap();
    assert_eq!(
        r,
        json!({"zone": "Asia/Tokyo", "offset_seconds": 32400, "standard_offset_seconds": 32400, "offset_valid_until": null})
    );
    assert_eq!(
        zone_event(&mut w).await["zone"],
        "Asia/Tokyo",
        "not Chicago twice"
    );
    assert_eq!(localtime(root), zoneinfo_fixtures().join("Asia/Tokyo"));

    // The audit names the method.
    let audit = sys.audit();
    assert!(
        audit
            .iter()
            .any(|m| m.get("HINTEROS_METHOD").map(String::as_str) == Some("time_zone.set")),
        "{audit:?}"
    );

    // A rollback to generation 1 restores its state.json (no zone): UTC, and the event.
    let r = call(&mut c, "rollback", json!({"generation": 1}))
        .await
        .unwrap();
    let id = r["job_id"].as_str().unwrap().to_owned();
    let done = finished(&until_finished(&mut c, &id).await).clone();
    assert_eq!(done["ok"], true, "{}", sys.job_log(&id));
    assert!(root.etc_state()["settings"].get("timeZone").is_none());
    assert_eq!(localtime(root), zoneinfo_fixtures().join("UTC"));
    let ev = zone_event(&mut w).await;
    assert_eq!(ev["zone"], "UTC");
    assert_eq!(call(&mut c, "time_zone.get", json!({})).await.unwrap(), ev);
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_pin_except_in_first_boot() {
    // An admin profile: the admin PIN every time.
    let Some(sys) = System::start(root2(false), &[]).await else {
        return;
    };
    let mut c = sys.client().await;
    let e = call(&mut c, "time_zone.set", json!({"zone": "Europe/London"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::PinRequired.code(), "{e}");
    assert_eq!(e["data"]["pin"], "admin");
    let e = call(
        &mut c,
        "time_zone.set",
        json!({"zone": "Europe/London", "admin_pin": "1111"}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::PinRequired.code(), "{e}");
    let e = call(
        &mut c,
        "time_zone.set",
        json!({"zone": "Europe/London", "admin_pin": "12"}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidParams.code(), "{e}");
    assert!(!sys.root.p("etc/localtime").exists());
    let r = call(
        &mut c,
        "time_zone.set",
        json!({"zone": "Europe/London", "admin_pin": ADMIN_PIN}),
    )
    .await
    .unwrap();
    assert_eq!(r["zone"], "Europe/London");
    assert_eq!(
        sys.root.etc_state()["settings"]["timeZone"],
        "Europe/London"
    );
    assert_eq!(
        localtime(&sys.root),
        zoneinfo_fixtures().join("Europe/London")
    );
    // The PIN never reaches the audit log.
    let audit = format!("{:?}", sys.audit());
    assert!(!audit.contains(&format!("\"{ADMIN_PIN}\"")), "{audit}");
    drop(sys);

    // First boot (no admin profile yet): no PIN.
    let Some(sys) = System::start(root2(true), &[]).await else {
        return;
    };
    let mut c = sys.client().await;
    let r = call(
        &mut c,
        "time_zone.set",
        json!({"zone": "Australia/Lord_Howe"}),
    )
    .await
    .unwrap();
    assert_eq!(r["zone"], "Australia/Lord_Howe");
    assert_eq!(r["standard_offset_seconds"], 10 * 3600 + 1800);
    assert_eq!(
        sys.root.etc_state()["settings"]["timeZone"],
        "Australia/Lord_Howe"
    );
    // UTC is always there.
    let r = call(&mut c, "time_zone.set", json!({"zone": "UTC"}))
        .await
        .unwrap();
    assert_eq!(r["zone"], "UTC");
    assert_eq!(localtime(&sys.root), zoneinfo_fixtures().join("UTC"));
}

/// A zone in state.json that tzdata does not have reads as UTC.
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_zone_in_state_reads_as_utc() {
    let root = Root::create();
    let state = root.read("etc/hinteros/state.json").replacen(
        "\"settings\": {",
        "\"settings\": { \"timeZone\": \"Europe/Paris\",",
        1,
    );
    assert!(state.contains("timeZone"), "{state}");
    std::fs::write(root.p("etc/hinteros/state.json"), state).unwrap();
    let Some(sys) = System::start(root, &[]).await else {
        return;
    };
    let mut c = sys.client().await;
    let r = call(&mut c, "time_zone.get", json!({})).await.unwrap();
    assert_eq!(r["zone"], "UTC");
}
