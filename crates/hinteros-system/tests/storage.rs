//! Storage (storage.md) against the real binary and a fake root: measuring
//! (a job; the result kept on disk), who may read which tree (a profile's own; another's only
//! with the admin PIN; never a name in anything else), both clean-ups (keep N, never the running,
//! booted or current generation, the admin PIN every time) and the automatic clean-up setting.

use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;

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

/// p1 is the admin profile; p2 an ordinary one.
const PROFILES: &str = r#"{"version": 1, "next": 3, "profiles": {
  "p1": {"name": "James", "color": "teal", "admin": true, "locked": false, "created_at": 0, "last_used_at": null,
         "appearance": {"text_scale": 1.0, "high_contrast": false, "reduce_motion": false}},
  "p2": {"name": "Kid", "color": "blue", "admin": false, "locked": false, "created_at": 0, "last_used_at": null,
         "appearance": {"text_scale": 1.0, "high_contrast": false, "reduce_motion": false}}}}"#;

const ADMIN_PIN: &str = "2468";

/// Names in p1's home that p2 must never see without the admin PIN.
const SECRET_DIR: &str = "SecretFolderXyz";
const SECRET_GAME: &str = "Secret Game Qwv";

fn root_with_profiles() -> Root {
    let root = Root::create();
    std::fs::write(root.p("etc/hinteros/state.json"), STATE2).unwrap();
    std::fs::write(root.p("var/lib/hinteros/profiles.json"), PROFILES).unwrap();
    hinteros_system::pins::Pins::new(root.p("var/lib/hinteros/pins"))
        .set("p1", ADMIN_PIN)
        .unwrap();
    let env = root.p("store/env-youtube");
    std::fs::create_dir_all(env.join("share/hinteros")).unwrap();
    std::fs::write(
        env.join("share/hinteros/apps.json"),
        r#"{"version":1,"ids":["youtube"],"apps":[],"box":[]}"#,
    )
    .unwrap();
    std::fs::create_dir_all(root.p("nixprofiles")).unwrap();
    symlink(&env, root.p("nixprofiles/starter-1-link")).unwrap();
    symlink("starter-1-link", root.p("nixprofiles/starter")).unwrap();
    root
}

fn write(path: &Path, bytes: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, vec![3u8; bytes]).unwrap();
}

/// Wait until no job runs and every owner has its starter env.
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

/// Run a job method to its end; returns the events of the job.
async fn job(sys: &System, c: &mut Client, method: &str, params: Value) -> Vec<(String, Value)> {
    let r = call(c, method, params)
        .await
        .unwrap_or_else(|e| panic!("{method}: {e}"));
    let id = r["job_id"].as_str().unwrap().to_owned();
    let events = until_finished(c, &id).await;
    let done = finished(&events).clone();
    assert_eq!(done["ok"], true, "{done}\n{}", sys.job_log(&id));
    events
}

/// The test's uid is p2 (an ordinary profile); p1 (the admin) and the guest have homes with
/// names in them.
fn as_p2(root: &Root) {
    std::fs::write(
        root.p("passwd"),
        format!(
            "hinteros-p1:x:20001:100::{}:/bin/sh\nhinteros-p2:x:{}:100::{}:/bin/sh\n\
             hinteros-guest:x:20000:100::{}:/bin/sh\n",
            root.p("homes/hinteros-p1").display(),
            me(),
            root.p("home").display(),
            root.p("homes/hinteros-guest").display(),
        ),
    )
    .unwrap();
    // p1: a big secret folder and a Steam game.
    let p1 = root.p("homes/hinteros-p1");
    write(&p1.join(SECRET_DIR).join("movie"), 3 << 20);
    let steam = p1.join(".local/share/Steam/steamapps");
    write(&steam.join("common/SG/data.pak"), 2 << 20);
    std::fs::write(
        steam.join("appmanifest_4242.acf"),
        format!("\"AppState\"\n{{\n\t\"appid\"\t\"4242\"\n\t\"name\"\t\"{SECRET_GAME}\"\n\t\"installdir\"\t\"SG\"\n}}\n"),
    )
    .unwrap();
    // p2 (the caller): its own folder.
    write(&root.p("home/MyMusic/song"), 2 << 20);
    // The guest.
    write(&root.p("homes/hinteros-guest/Downloads/x"), 1 << 20);
    // /srv.
    write(&root.p("srv/media/Movies/SecretFilm.mkv"), 2 << 20);
}

/// NAR sizes for the fake store: the system generation and the starter env; one dead path.
fn fake_store(root: &Root) {
    let gen1 = root.p("store/gen1").display().to_string();
    let env = root.p("store/env-youtube").display().to_string();
    let dead = root.p("store/dead-thing").display().to_string();
    std::fs::write(
        root.p("path-sizes"),
        format!("{gen1}\t5000000\n{env}\t700000\n{dead}\t900000\n"),
    )
    .unwrap();
    std::fs::write(root.p("live-paths"), format!("{gen1}\n{env}\n")).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn measuring_and_who_sees_what() {
    let root = root_with_profiles();
    as_p2(&root);
    fake_store(&root);
    let Some(sys) = System::start(root, &[]).await else {
        return;
    };
    settled(&sys).await;
    let mut c = sys.subscriber().await;

    // Nothing measured yet.
    let r = call(&mut c, "storage.get", json!({})).await.unwrap();
    assert_eq!(r, json!({"measurement": null}));
    let e = call(&mut c, "storage.tree", json!({"id": "p2"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound.code(), "{e}");

    // Measure (a job with progress).
    let events = job(&sys, &mut c, "storage.measure", json!({})).await;
    let stages: Vec<&str> = events
        .iter()
        .filter(|(n, _)| n == "job_progress")
        .map(|(_, p)| p["stage"].as_str().unwrap())
        .collect();
    assert!(stages.contains(&"measuring"), "{stages:?}");
    let done = finished(&events);
    assert_eq!(done["kind"], "storage_measure");
    assert!(
        done["result"]["measured_at"].as_i64().unwrap() > 0,
        "{done}"
    );
    let stored = std::fs::metadata(sys.root.p("var/lib/hinteros/storage.json")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(stored.permissions().mode() & 0o777, 0o600);

    // The top level: open to everyone, every profile's total, no names.
    let r = call(&mut c, "storage.get", json!({})).await.unwrap();
    let m = &r["measurement"];
    let text = r.to_string();
    assert!(
        !text.contains(SECRET_DIR) && !text.contains(SECRET_GAME),
        "{text}"
    );
    assert!(
        !text.contains("MyMusic"),
        "own names are not at the top level either"
    );
    let top = m["root"]["children"].as_array().unwrap();
    let kinds: Vec<&str> = top.iter().map(|n| n["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds.first(), Some(&"system"), "{kinds:?}");
    assert_eq!(kinds.last(), Some(&"free"), "{kinds:?}");
    let p1 = top.iter().find(|n| n["id"] == "p1").unwrap();
    assert!(p1["bytes"].as_u64().unwrap() >= 5 << 20, "{p1}");
    assert_eq!(p1["children"], json!([]));
    assert!(top.iter().any(|n| n["kind"] == "guest"));
    // Media: a total, after the homes.
    let media = top.iter().find(|n| n["kind"] == "media").unwrap();
    assert!(media["bytes"].as_u64().unwrap() >= 2 << 20, "{media}");
    assert_eq!(media["children"], json!([]));
    assert!(!text.contains("SecretFilm"), "{text}");
    assert_eq!(
        top.iter().find(|n| n["kind"] == "system").unwrap()["bytes"],
        5_000_000
    );
    assert_eq!(m["garbage_bytes"], 900_000);
    assert_eq!(m["estimates"]["system"].as_array().unwrap().len(), 10);
    let sum: u64 = top.iter().map(|n| n["bytes"].as_u64().unwrap()).sum();
    assert_eq!(sum, m["root"]["bytes"].as_u64().unwrap());

    // Own tree: no PIN.
    let r = call(&mut c, "storage.tree", json!({"id": "p2"}))
        .await
        .unwrap();
    assert_eq!(r["root"]["kind"], "profile");
    assert!(r.to_string().contains("MyMusic"), "{r}");

    // Another profile's tree: the admin PIN (missing, wrong, malformed; then right).
    let e = call(&mut c, "storage.tree", json!({"id": "p1"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::PinRequired.code(), "{e}");
    assert_eq!(e["data"], json!({"pin": "admin"}));
    assert!(!e.to_string().contains(SECRET_DIR));
    let e = call(
        &mut c,
        "storage.tree",
        json!({"id": "p1", "admin_pin": "1111"}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::PinRequired.code(), "{e}");
    let e = call(
        &mut c,
        "storage.tree",
        json!({"id": "p1", "admin_pin": "12"}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidParams.code(), "{e}");
    assert_eq!(e["data"]["field"], "admin_pin");
    let e = call(&mut c, "storage.tree", json!({"id": "guest"}))
        .await
        .unwrap_err();
    assert_eq!(
        code(&e),
        ErrorCode::PinRequired.code(),
        "the guest's tree too: {e}"
    );
    let e = call(&mut c, "storage.tree", json!({"id": "../p1"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidParams.code(), "{e}");
    assert_eq!(e["data"]["field"], "id");
    let e = call(
        &mut c,
        "storage.tree",
        json!({"id": "p7", "admin_pin": ADMIN_PIN}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound.code(), "{e}");

    let r = call(
        &mut c,
        "storage.tree",
        json!({"id": "p1", "admin_pin": ADMIN_PIN}),
    )
    .await
    .unwrap();
    let text = r.to_string();
    assert!(text.contains(SECRET_DIR), "{text}");
    let steam = r["root"]["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["kind"] == "steam")
        .unwrap_or_else(|| panic!("{r}"));
    assert_eq!(steam["children"][0]["name"], SECRET_GAME);
    assert_eq!(steam["children"][0]["app_id"], 4242);
    let r = call(
        &mut c,
        "storage.tree",
        json!({"id": "guest", "admin_pin": ADMIN_PIN}),
    )
    .await
    .unwrap();
    assert_eq!(r["root"]["kind"], "guest");

    // Nothing a non-admin can read names p1's files: events, job results, the logs of the
    // service and the job, the audit.
    let jobs = call(&mut c, "jobs.list", json!({}))
        .await
        .unwrap()
        .to_string();
    let id = done["job_id"].as_str().unwrap();
    let mut readable = format!("{events:?}{jobs}{}{}", sys.log(), sys.job_log(id));
    for entry in sys.audit() {
        readable.push_str(&format!("{entry:?}"));
    }
    for secret in [SECRET_DIR, SECRET_GAME, ADMIN_PIN] {
        assert!(!readable.contains(secret), "{secret} leaked");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_measure_while_another_job_runs() {
    let root = root_with_profiles();
    fake_store(&root);
    let Some(sys) = System::start(root, &[]).await else {
        return;
    };
    settled(&sys).await;
    let mut c = sys.subscriber().await;
    sys.root.touch("build-hold");
    let r = call(&mut c, "rebuild", json!({})).await.unwrap();
    let e = call(&mut c, "storage.measure", json!({}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::JobBusy.code(), "{e}");
    sys.root.rm("build-hold");
    until_finished(&mut c, r["job_id"].as_str().unwrap()).await;
    // A measure that is running is the answer to the next request (no second job).
    sys.root.touch("store-hold");
    let a = call(&mut c, "storage.measure", json!({})).await.unwrap();
    let b = call(&mut c, "storage.measure", json!({})).await.unwrap();
    assert_eq!(a, b);
    sys.root.rm("store-hold");
    let events = until_finished(&mut c, a["job_id"].as_str().unwrap()).await;
    assert_eq!(finished(&events)["ok"], true);
}

/// System generations 1 to 5: 5 is the profile's (running) one, 2 the booted one.
fn five_generations(root: &Root) {
    for n in 2..=5 {
        let out = root.p(&format!("store/gen{n}"));
        let ok = Command::new(root.p("mkgen"))
            .arg(&out)
            .arg(root.p("etc/hinteros"))
            .env("FAKE_ROOT", root.path())
            .status()
            .unwrap()
            .success();
        assert!(ok);
        symlink(&out, root.p(&format!("profiles/system-{n}-link"))).unwrap();
    }
    std::fs::write(root.p("counter"), "5").unwrap();
    std::fs::remove_file(root.p("profiles/system")).unwrap();
    symlink("system-5-link", root.p("profiles/system")).unwrap();
    for (link, n) in [("current", 5), ("booted", 2)] {
        std::fs::remove_file(root.p(link)).unwrap();
        symlink(root.p(&format!("store/gen{n}")), root.p(link)).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn clean_up_the_system() {
    let root = root_with_profiles();
    five_generations(&root);
    let Some(sys) = System::start(root, &[]).await else {
        return;
    };
    settled(&sys).await;
    let mut c = sys.subscriber().await;

    // The admin PIN every time, keep 1..10.
    let e = call(&mut c, "storage.cleanup_system", json!({"keep": 1}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::PinRequired.code(), "{e}");
    assert_eq!(e["data"], json!({"pin": "admin"}));
    let e = call(
        &mut c,
        "storage.cleanup_system",
        json!({"keep": 1, "admin_pin": "9999"}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::PinRequired.code(), "{e}");
    for keep in [0, 11] {
        let e = call(
            &mut c,
            "storage.cleanup_system",
            json!({"keep": keep, "admin_pin": ADMIN_PIN}),
        )
        .await
        .unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidParams.code(), "{e}");
        assert_eq!(e["data"]["field"], "keep");
    }
    assert!(
        !sys.root
            .read("nix-env.log")
            .contains("--delete-generations")
    );

    // keep 1: 5 (running) and 2 (booted) stay whatever keep says; 4, 3 and 1 go.
    let events = job(
        &sys,
        &mut c,
        "storage.cleanup_system",
        json!({"keep": 1, "admin_pin": ADMIN_PIN}),
    )
    .await;
    let done = finished(&events);
    assert_eq!(done["kind"], "cleanup_system");
    assert_eq!(done["subject"], json!({"keep": 1}));
    assert_eq!(done["result"]["generations_removed"], 3, "{done}");
    assert!(done["result"]["freed_bytes"].is_u64());
    let stages: Vec<&str> = events
        .iter()
        .filter(|(n, _)| n == "job_progress")
        .map(|(_, p)| p["stage"].as_str().unwrap())
        .collect();
    assert!(
        stages.contains(&"cleaning") && stages.contains(&"collecting"),
        "{stages:?}"
    );
    let env_log = sys.root.read("nix-env.log");
    assert!(env_log.contains("--delete-generations 4 3 1"), "{env_log}");
    for n in [2, 5] {
        assert!(
            sys.root
                .p(&format!("profiles/system-{n}-link"))
                .is_symlink()
        );
    }
    for n in [1, 3, 4] {
        assert!(!sys.root.p(&format!("profiles/system-{n}-link")).exists());
    }
    // The boot menu was rewritten from the default generation, then garbage collected.
    let switches = sys.root.read("switch.log");
    assert!(
        switches.contains(&format!("{} boot", sys.root.p("store/gen5").display())),
        "{switches}"
    );
    assert!(sys.root.read("nix-store.log").lines().any(|l| l == "--gc"));

    // Again with keep 1: nothing to remove, garbage still collected.
    let events = job(
        &sys,
        &mut c,
        "storage.cleanup_system",
        json!({"keep": 1, "admin_pin": ADMIN_PIN}),
    )
    .await;
    assert_eq!(finished(&events)["result"]["generations_removed"], 0);
    let gcs = sys
        .root
        .read("nix-store.log")
        .lines()
        .filter(|l| *l == "--gc")
        .count();
    assert_eq!(gcs, 2);
    // The audit names the PIN check, never the PIN.
    let audit = sys.audit();
    assert!(audit.iter().any(|e| {
        e.get("HINTEROS_METHOD").map(String::as_str) == Some("storage.cleanup_system")
            && e.get("HINTEROS_PIN_CHECK").map(String::as_str) == Some("admin")
    }));
    assert!(!format!("{audit:?}").contains(&format!("\"{ADMIN_PIN}\"")));
}

#[tokio::test(flavor = "multi_thread")]
async fn clean_up_profiles() {
    let root = root_with_profiles();
    let Some(sys) = System::start(root, &[]).await else {
        return;
    };
    settled(&sys).await;
    let mut c = sys.subscriber().await;
    // p1's apps: generations 1 to 4, the current one 2.
    let apps = sys.root.p("nixprofiles/apps");
    let env = sys.root.p("store/env-youtube");
    for n in 2..=4 {
        let link = apps.join(format!("hinteros-p1-{n}-link"));
        if !link.is_symlink() {
            symlink(&env, link).unwrap();
        }
    }
    std::fs::remove_file(apps.join("hinteros-p1")).unwrap();
    symlink("hinteros-p1-2-link", apps.join("hinteros-p1")).unwrap();

    let e = call(&mut c, "storage.cleanup_profiles", json!({"keep": 2}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::PinRequired.code(), "{e}");
    // keep 2: 4 and 3 are the newest, 2 is current; 1 goes. Other owners have one each.
    let events = job(
        &sys,
        &mut c,
        "storage.cleanup_profiles",
        json!({"keep": 2, "admin_pin": ADMIN_PIN}),
    )
    .await;
    let done = finished(&events);
    assert_eq!(done["kind"], "cleanup_profiles");
    assert_eq!(done["subject"], json!({"keep": 2}));
    assert_eq!(done["result"]["generations_removed"], 1, "{done}");
    let log = sys.root.read("nix-env.log");
    assert!(
        log.contains(&format!(
            "-p {} --delete-generations 1",
            apps.join("hinteros-p1").display()
        )),
        "{log}"
    );
    assert!(!apps.join("hinteros-p1-1-link").exists());
    for n in 2..=4 {
        assert!(apps.join(format!("hinteros-p1-{n}-link")).is_symlink());
    }
    // keep 1: the current (2) stays although 4 is the newest; 3 goes.
    let events = job(
        &sys,
        &mut c,
        "storage.cleanup_profiles",
        json!({"keep": 1, "admin_pin": ADMIN_PIN}),
    )
    .await;
    assert_eq!(finished(&events)["result"]["generations_removed"], 1);
    assert!(
        apps.join("hinteros-p1-2-link").is_symlink()
            && apps.join("hinteros-p1-4-link").is_symlink()
    );
    assert!(!apps.join("hinteros-p1-3-link").exists());
    // The system's generations are not touched, and no boot menu is written.
    assert!(sys.root.p("profiles/system-1-link").is_symlink());
    assert!(!sys.root.read("switch.log").contains(" boot"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_automatic_clean_up_setting() {
    let root = root_with_profiles();
    let Some(sys) = System::start(root, &[]).await else {
        return;
    };
    settled(&sys).await;
    let mut c = sys.subscriber().await;

    // Absent: today's policy.
    let r = call(&mut c, "cleanup.get", json!({})).await.unwrap();
    assert_eq!(
        r,
        json!({"enabled": true, "frequency": "weekly", "keep_system": 5, "keep_profiles": 5})
    );
    let set =
        json!({"enabled": false, "frequency": "monthly", "keep_system": 3, "keep_profiles": 2});
    let with_pin = |mut v: Value| {
        v["admin_pin"] = json!(ADMIN_PIN);
        v
    };
    let e = call(&mut c, "cleanup.set", set.clone()).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::PinRequired.code(), "{e}");
    for (field, bad) in [("keep_system", 0), ("keep_profiles", 11)] {
        let mut p = with_pin(set.clone());
        p[field] = json!(bad);
        let e = call(&mut c, "cleanup.set", p).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidParams.code(), "{e}");
        assert_eq!(e["data"]["field"], field);
    }
    let mut p = with_pin(set.clone());
    p["frequency"] = json!("hourly");
    let e = call(&mut c, "cleanup.set", p).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidParams.code(), "{e}");
    // The defaults while absent: nothing to do.
    let r = call(
        &mut c,
        "cleanup.set",
        with_pin(
            json!({"enabled": true, "frequency": "weekly", "keep_system": 5, "keep_profiles": 5}),
        ),
    )
    .await
    .unwrap();
    assert_eq!(r["job_id"], Value::Null);
    assert!(sys.root.etc_state()["settings"].get("cleanup").is_none());

    // A change: a rebuild that writes settings.cleanup.
    let events = job(&sys, &mut c, "cleanup.set", with_pin(set.clone())).await;
    assert_eq!(finished(&events)["kind"], "cleanup_set");
    assert_eq!(
        finished(&events)["subject"],
        json!({"enabled": false, "frequency": "monthly", "keep_system": 3, "keep_profiles": 2})
    );
    assert_eq!(
        sys.root.etc_state()["settings"]["cleanup"],
        json!({"enable": false, "frequency": "monthly", "keepSystem": 3, "keepProfiles": 2})
    );
    let r = call(&mut c, "cleanup.get", json!({})).await.unwrap();
    assert_eq!(r, set);
    // The same again: nothing to do.
    let r = call(&mut c, "cleanup.set", with_pin(set)).await.unwrap();
    assert_eq!(r["job_id"], Value::Null);
}
