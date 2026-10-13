//! Per-profile apps (system API 0.4, apps.md) against the real binary, a fake root and fake
//! `nix`/`nix-env`/`nix-store`: install, remove and rollback in the caller's own Nix profile,
//! the box stage, the registry, the final apps stage of system jobs, and the v1 → v2
//! migration.

use std::time::{Duration, Instant};

use hinteros_api::ErrorCode;
use hinteros_api::client::Client;
use serde_json::{Value, json};

mod common;

use common::*;

const STATE2: &str = r#"{
  "schemaVersion": 2,
  "base": "unstable",
  "settings": { "ssh": { "enable": false } },
  "requirements": [],
  "services": [],
  "profiles": [{ "id": "p1", "user": "hinteros" }, { "id": "p2", "user": "hinteros-p2" }]
}
"#;

/// p1 (the test user) is a profile; p2 is a spare.
const PROFILES: &str = r#"{"version": 1, "next": 3, "profiles": {
  "p1": {"name": "Dev", "color": "teal", "admin": true, "locked": false, "created_at": 0, "last_used_at": null,
         "appearance": {"text_scale": 1.0, "high_contrast": false, "reduce_motion": false}}}}"#;

const CATALOG3: &str = r#"{"indexVersion": 3, "apps": [
  {"id": "steam", "kind": "app", "type": "native", "name": "Steam", "status": "available", "sizeMiB": 1000,
   "box": ["graphics32", "steam-devices"], "dataPaths": [".local/share/Steam"],
   "terms": {"title": "Steam Subscriber Agreement", "version": "2026-09-10", "url": "https://store.steampowered.com/x",
             "retrieved": "2026-10-06", "file": "terms/steam-2026-09-10.txt"}},
  {"id": "moonlight", "kind": "app", "type": "native", "name": "Moonlight", "status": "available", "sizeMiB": 150,
   "dataPaths": [".config/Moonlight Game Streaming Project"]},
  {"id": "youtube", "kind": "app", "type": "web", "name": "YouTube", "status": "available", "sizeMiB": 1},
  {"id": "jellyfin-server", "kind": "service", "type": "service", "name": "Jellyfin server", "status": "available", "sizeMiB": 300},
  {"id": "old", "kind": "app", "name": "Old", "status": "removed", "statusReason": "Gone."}
]}"#;

/// Profile p1's (the test user's) app profile.
fn p1_link(root: &Root) -> std::path::PathBuf {
    root.p("nixprofiles/apps/hinteros")
}

fn env_of(root: &Root, link: &std::path::Path) -> Option<String> {
    let target = std::fs::canonicalize(link).ok()?;
    let name = target.file_name()?.to_string_lossy().into_owned();
    let _ = root;
    Some(name)
}

async fn apps_list(c: &mut Client) -> Value {
    call(c, "apps.list", json!({})).await.unwrap()
}

async fn run_job(sys: &System, c: &mut Client, method: &str, params: Value) -> Value {
    let r = call(c, method, params.clone())
        .await
        .unwrap_or_else(|e| panic!("{method} {params}: {e}"));
    let id = r["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{method}: no job: {r}"))
        .to_owned();
    let events = until_finished(c, &id).await;
    let done = finished(&events).clone();
    assert_eq!(
        done["ok"],
        true,
        "{method}: {done}\n{}\n{}",
        sys.job_log(&id),
        sys.log()
    );
    json!({ "finished": done, "stages": events.iter()
        .filter(|(n, _)| n == "job_progress")
        .map(|(_, p)| p["stage"].clone())
        .collect::<Vec<_>>() })
}

#[tokio::test(flavor = "multi_thread")]
async fn profile_apps_install_remove_rollback() {
    let root = Root::create();
    std::fs::write(root.p("etc/hinteros/state.json"), STATE2).unwrap();
    std::fs::write(root.p("var/lib/hinteros/profiles.json"), PROFILES).unwrap();
    std::fs::write(root.p("policy/catalog.json"), CATALOG3).unwrap();
    let Some(sys) = System::start(root, &[]).await else {
        return;
    };
    let root = &sys.root;
    let mut c = sys.subscriber().await;

    // The session link is published at start, before any app exists.
    assert_eq!(
        std::fs::read_link(root.p("run/apps/hinteros")).unwrap(),
        p1_link(root)
    );
    assert!(root.p("run/apps/hinteros-p2").is_symlink());
    let l = apps_list(&mut c).await;
    assert_eq!(l["generation"], Value::Null);
    assert_eq!(l["ids"], json!([]));
    let rev0 = l["revision"].as_str().unwrap().to_owned();
    // schema 2: settings.get has requirements, no apps.
    let s = call(&mut c, "settings.get", json!({})).await.unwrap();
    assert_eq!(s["settings"]["requirements"], json!([]));
    assert!(s["settings"].get("apps").is_none());

    // Refusals.
    for (ids, want) in [
        (json!(["jellyfin-server"]), ErrorCode::NotFound),
        (json!(["old"]), ErrorCode::InvalidParams),
        (json!(["nope"]), ErrorCode::NotFound),
    ] {
        let e = call(
            &mut c,
            "apps.install",
            json!({"ids": ids, "revision": rev0}),
        )
        .await
        .unwrap_err();
        assert_eq!(code(&e), want.code(), "{ids} {e}");
    }
    let e = call(
        &mut c,
        "apps.install",
        json!({"ids": ["moonlight"], "revision": "0".repeat(64)}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::StateConflict.code());

    // A free app: an env, a new generation of p1's Nix profile, no system rebuild.
    let switches = root.read("switch.log");
    let r = run_job(
        &sys,
        &mut c,
        "apps.install",
        json!({"ids": ["moonlight"], "revision": rev0}),
    )
    .await;
    assert!(!r["stages"].as_array().unwrap().contains(&json!("box")));
    assert_eq!(root.read("switch.log"), switches, "no system switch");
    assert_eq!(env_of(root, &p1_link(root)).unwrap(), "env-moonlight");
    assert!(root.read("nix.log").contains("eval --impure --json --expr"));
    assert!(root.read("last-envs.json").contains("moonlight"));
    let l = apps_list(&mut c).await;
    assert_eq!(l["ids"], json!(["moonlight"]));
    assert_eq!(l["generation"], 1);
    let rev1 = l["revision"].as_str().unwrap().to_owned();
    assert_ne!(rev1, rev0);
    // p2 is untouched.
    assert!(!root.p("nixprofiles/apps/hinteros-p2").exists());
    let reg: Value =
        serde_json::from_str(&root.read("var/lib/hinteros/apps/registry.json")).unwrap();
    assert_eq!(reg["owners"]["p1"]["ids"], json!(["moonlight"]));

    // Steam: terms first; then the box stage turns its features on before the env.
    let e = call(
        &mut c,
        "apps.install",
        json!({"ids": ["steam"], "revision": rev1}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::TermsRequired.code());
    let r = run_job(
        &sys,
        &mut c,
        "apps.install",
        json!({"ids": ["steam"], "revision": rev1, "accepted_terms": {"steam": "2026-09-10"}}),
    )
    .await;
    assert!(
        r["stages"].as_array().unwrap().contains(&json!("box")),
        "{r}"
    );
    assert_eq!(
        root.etc_state()["requirements"],
        json!(["graphics32", "steam-devices"])
    );
    assert_ne!(root.read("switch.log"), switches, "the box stage switched");
    assert_eq!(env_of(root, &p1_link(root)).unwrap(), "env-moonlight+steam");
    let terms: Value =
        serde_json::from_str(&root.read("var/lib/hinteros/terms-accepted.json")).unwrap();
    assert_eq!(terms["steam"][0]["profile"], "p1");
    assert_eq!(terms["steam"][0]["by"], "hinteros");
    let l = apps_list(&mut c).await;
    assert_eq!(l["ids"], json!(["moonlight", "steam"]));
    assert_eq!(l["needs_terms"], json!([]));
    assert_eq!(l["box"], json!(["graphics32", "steam-devices"]));
    let rev2 = l["revision"].as_str().unwrap().to_owned();

    // Remove with data: only the caller's home, features stay on.
    std::fs::create_dir_all(root.p("home/.config/Moonlight Game Streaming Project")).unwrap();
    std::fs::write(
        root.p("home/.config/Moonlight Game Streaming Project/x.conf"),
        "x",
    )
    .unwrap();
    let switches = root.read("switch.log");
    run_job(
        &sys,
        &mut c,
        "apps.remove",
        json!({"ids": ["moonlight"], "revision": rev2, "delete_data": true}),
    )
    .await;
    assert!(
        !root
            .p("home/.config/Moonlight Game Streaming Project")
            .exists()
    );
    assert_eq!(env_of(root, &p1_link(root)).unwrap(), "env-steam");
    assert_eq!(
        root.etc_state()["requirements"],
        json!(["graphics32", "steam-devices"])
    );
    assert_eq!(root.read("switch.log"), switches);

    // Generations and rollback.
    let g = call(&mut c, "apps.generations", json!({})).await.unwrap();
    let gens = g["generations"].as_array().unwrap();
    assert_eq!(gens.len(), 3);
    assert_eq!(gens[0]["generation"], 3);
    assert_eq!(gens[0]["current"], true);
    assert_eq!(gens[2]["ids"], json!(["moonlight"]));
    let e = call(&mut c, "apps.rollback", json!({"generation": 3}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound.code());
    run_job(&sys, &mut c, "apps.rollback", json!({"generation": 1})).await;
    let l = apps_list(&mut c).await;
    assert_eq!(l["ids"], json!(["moonlight"]));
    assert_eq!(l["generation"], 1);

    // Nobody needs Steam's features now; they go at the next job that rebuilds anyway.
    assert_eq!(
        root.etc_state()["requirements"],
        json!(["graphics32", "steam-devices"])
    );
    run_job(&sys, &mut c, "rebuild", json!({})).await;
    assert_eq!(root.etc_state()["requirements"], json!([]));
    assert_eq!(root.etc_state()["schemaVersion"], 2);

    // Terms the profile has not accepted (a new terms version): needs_terms, then accept.
    run_job(&sys, &mut c, "apps.rollback", json!({"generation": 3})).await;
    std::fs::write(
        root.p("policy/catalog.json"),
        CATALOG3.replace("2026-09-10", "2027-01-01"),
    )
    .unwrap();
    let l = apps_list(&mut c).await;
    assert_eq!(l["needs_terms"], json!(["steam"]));
    let e = call(
        &mut c,
        "apps.accept_terms",
        json!({"id": "steam", "version": "2026-09-10"}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidParams.code());
    let e = call(
        &mut c,
        "apps.accept_terms",
        json!({"id": "youtube", "version": "1"}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound.code());
    call(
        &mut c,
        "apps.accept_terms",
        json!({"id": "steam", "version": "2027-01-01"}),
    )
    .await
    .unwrap();
    assert_eq!(apps_list(&mut c).await["needs_terms"], json!([]));

    // Another user that is not on screen: the seat check; a non-profile user: refused.
    sys.fakes.lock().unwrap().seat_user = Some(4242);
    let e = call(&mut c, "apps.rollback", json!({"generation": 1}))
        .await
        .unwrap_err();
    assert_eq!(e["data"]["reason"], "seat");
}

/// The registry is rebuilt from the env manifests at start (after a crash or a manual nix-env).
#[tokio::test(flavor = "multi_thread")]
async fn registry_recovers_from_the_manifests() {
    let root = Root::create();
    std::fs::write(root.p("etc/hinteros/state.json"), STATE2).unwrap();
    std::fs::write(root.p("var/lib/hinteros/profiles.json"), PROFILES).unwrap();
    std::fs::write(root.p("policy/catalog.json"), CATALOG3).unwrap();
    // p2 got an env behind the service's back; the registry claims something else.
    let env = root.p("store/env-youtube");
    std::fs::create_dir_all(env.join("share/hinteros")).unwrap();
    std::fs::write(
        env.join("share/hinteros/apps.json"),
        r#"{"version":1,"ids":["youtube"],"apps":[],"box":[]}"#,
    )
    .unwrap();
    std::fs::create_dir_all(root.p("nixprofiles/apps")).unwrap();
    std::os::unix::fs::symlink(&env, root.p("nixprofiles/apps/hinteros-p2-1-link")).unwrap();
    std::os::unix::fs::symlink("hinteros-p2-1-link", root.p("nixprofiles/apps/hinteros-p2"))
        .unwrap();
    std::fs::create_dir_all(root.p("var/lib/hinteros/apps")).unwrap();
    std::fs::write(
        root.p("var/lib/hinteros/apps/registry.json"),
        r#"{"version":1,"owners":{"p2":{"ids":["steam"],"box":["graphics32"]}}}"#,
    )
    .unwrap();
    let Some(sys) = System::start(root, &[]).await else {
        return;
    };
    let reg: Value =
        serde_json::from_str(&sys.root.read("var/lib/hinteros/apps/registry.json")).unwrap();
    assert_eq!(
        reg["owners"]["p2"],
        json!({"user": "hinteros-p2", "generation": 1, "ids": ["youtube"], "box": []})
    );
}

/// After a system switch every env is rebuilt in one evaluation; one that fails keeps its
/// generation and is reported, and the job still succeeds. Tombstoned ids are dropped.
#[tokio::test(flavor = "multi_thread")]
async fn apps_stage_after_a_switch() {
    let root = Root::create();
    std::fs::write(root.p("etc/hinteros/state.json"), STATE2).unwrap();
    std::fs::write(root.p("var/lib/hinteros/profiles.json"), PROFILES).unwrap();
    std::fs::write(root.p("policy/catalog.json"), CATALOG3).unwrap();
    let Some(sys) = System::start(root, &[]).await else {
        return;
    };
    let root = &sys.root;
    let mut c = sys.subscriber().await;
    let rev = apps_list(&mut c).await["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    run_job(
        &sys,
        &mut c,
        "apps.install",
        json!({"ids": ["moonlight", "youtube"], "revision": rev}),
    )
    .await;
    // The starter set has Steam, whose env will fail to rebuild.
    let steam_env = root.p("store/old-steam");
    std::fs::create_dir_all(steam_env.join("share/hinteros")).unwrap();
    std::fs::write(
        steam_env.join("share/hinteros/apps.json"),
        r#"{"version":1,"ids":["steam"],"apps":[],"box":["graphics32","steam-devices"]}"#,
    )
    .unwrap();
    std::os::unix::fs::symlink(&steam_env, root.p("nixprofiles/starter-1-link")).unwrap();
    std::os::unix::fs::symlink("starter-1-link", root.p("nixprofiles/starter")).unwrap();
    root.touch("env-fail-steam");
    // The new catalog removes youtube.
    std::fs::write(
        root.p("policy/catalog.json"),
        CATALOG3.replace(
            r#"{"id": "youtube", "kind": "app", "type": "web", "name": "YouTube", "status": "available", "sizeMiB": 1},"#,
            r#"{"id": "youtube", "kind": "app", "name": "YouTube", "status": "removed", "statusReason": "Gone."},"#,
        ),
    )
    .unwrap();
    let r = run_job(
        &sys,
        &mut c,
        "base.set",
        json!({"base": "stable", "acknowledge_downgrade": true}),
    )
    .await;
    assert!(
        r["stages"].as_array().unwrap().contains(&json!("apps")),
        "{r}"
    );
    let result = &r["finished"]["result"];
    assert_eq!(result["apps_not_updated"][0]["owner"], "starter", "{r}");
    assert_eq!(result["apps_not_updated"][0]["ids"], json!(["steam"]));
    assert_eq!(result["apps_not_updated"].as_array().unwrap().len(), 1);
    // p1 lost the tombstone; the starter kept its old generation.
    assert_eq!(env_of(root, &p1_link(root)).unwrap(), "env-moonlight");
    assert_eq!(
        env_of(root, &root.p("nixprofiles/starter")).unwrap(),
        "old-steam"
    );
    // The features the starter's Steam needs were kept on (the starter still has it).
    assert_eq!(
        root.etc_state()["requirements"],
        json!(["graphics32", "steam-devices"])
    );
}

const STATE1: &str = r#"{
  "schemaVersion": 1,
  "base": "unstable",
  "settings": { "ssh": { "enable": false } },
  "apps": ["youtube", "steam", "old"],
  "profiles": [{ "id": "p1", "name": "Dev", "user": "hinteros" }]
}
"#;

async fn wait_for_job(sys: &System, kind: &str, not: Option<&Value>) -> Value {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let mut c = sys.client().await;
        let jobs = call(&mut c, "jobs.list", json!({})).await.unwrap();
        if let Some(j) = jobs["jobs"].as_array().unwrap().iter().find(|j| {
            j["kind"] == kind
                && j["state"] != "running"
                && not.is_none_or(|n| n["job_id"] != j["job_id"])
        }) {
            return j.clone();
        }
        assert!(
            Instant::now() < deadline,
            "no finished {kind} job: {jobs}\n{}",
            sys.log()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// v1 → v2: at start a schema-1 box on a generation that reads schema 2 migrates by itself. A
/// failure leaves schema 1; the next boot retries, and what was done is not done again.
#[tokio::test(flavor = "multi_thread")]
async fn migration_is_resumable_and_idempotent() {
    let root = Root::create();
    std::fs::write(root.p("etc/hinteros/state.json"), STATE1).unwrap();
    std::fs::write(root.p("policy/catalog.json"), CATALOG3).unwrap();
    std::fs::write(
        root.p("policy/release.json"),
        r#"{"version":"0.2.0-test","dev":true,"stateSchema":2}"#,
    )
    .unwrap();
    std::fs::write(
        root.p("var/lib/hinteros/terms-accepted.json"),
        r#"{"steam":[{"version":"2026-09-10","accepted_at":"2026-01-01T00:00:00Z","by":"installer"}]}"#,
    )
    .unwrap();
    root.touch("switch-fail");
    // The switch waits until this is gone: the job is running meanwhile.
    root.touch("switch-hold");
    let Some(sys) = System::start(root, &[]).await else {
        return;
    };
    // While the job runs, the public view already follows the new profiles.json: no longer
    // first boot, p1 listed (the greeter must not offer first-boot setup meanwhile).
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let public: Value =
            serde_json::from_str(&sys.root.read("run/profiles-public.json")).unwrap_or_default();
        if public["first_boot"] == false && public["profiles"][0]["id"] == "p1" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "public view during the migration: {public}\n{}",
            sys.log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut c = sys.client().await;
    let jobs = call(&mut c, "jobs.list", json!({})).await.unwrap();
    assert!(
        jobs["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|j| j["kind"] == "migrate_apps" && j["state"] == "running"),
        "{jobs}"
    );
    drop(c);
    sys.root.rm("switch-hold");
    let first = wait_for_job(&sys, "migrate_apps", None).await;
    assert_eq!(first["state"], "failed", "{first}");
    let root = &sys.root;
    // Still schema 1, but p1 already has its env and profiles.json exists.
    assert_eq!(root.etc_state()["schemaVersion"], 1);
    assert_eq!(env_of(root, &p1_link(root)).unwrap(), "env-steam+youtube");
    let profiles: Value =
        serde_json::from_str(&root.read("var/lib/hinteros/profiles.json")).unwrap();
    assert_eq!(profiles["profiles"]["p1"]["admin"], true);
    assert_eq!(profiles["profiles"]["p1"]["locked"], false);
    assert_eq!(profiles["profiles"]["p1"]["name"], "Dev");
    // Schema 1: the per-profile methods are unavailable.
    let mut c = sys.client().await;
    let e = call(&mut c, "apps.list", json!({})).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Unavailable.code());
    assert_eq!(
        e["data"],
        json!({"service": "profiles", "reason": "schema1"})
    );
    // No PINs before the migration: the launcher must not ask for one yet.
    let l = call(&mut c, "profiles.list", json!({})).await.unwrap();
    assert_eq!(l["profiles"][0]["pin_set"], true, "{l}");
    drop(c);

    // "Next boot": a new service, the marker gone, the switch works now.
    let root_dir = {
        let mut sys = sys;
        let _ = sys.child.kill();
        let _ = sys.child.wait();
        std::mem::replace(&mut sys.root, Root::create())
    };
    root_dir.rm("run/migrate-attempted");
    root_dir.rm("journal.sock");
    root_dir.rm("switch-fail");
    let Some(sys) = System::start(root_dir, &[]).await else {
        return;
    };
    let job = wait_for_job(&sys, "migrate_apps", Some(&first)).await;
    assert_eq!(job["state"], "succeeded", "{job}\n{}", sys.log());
    let root = &sys.root;
    let state = root.etc_state();
    assert_eq!(state["schemaVersion"], 2);
    assert!(state.get("apps").is_none());
    assert_eq!(
        state["requirements"],
        json!(["graphics32", "steam-devices"])
    );
    assert_eq!(state["services"], json!([]));
    assert_eq!(state["profiles"], json!([{"id": "p1", "user": "hinteros"}]));
    // Now p1 has no PIN yet: "Set your PIN".
    let mut c = sys.client().await;
    let l = call(&mut c, "profiles.list", json!({})).await.unwrap();
    assert_eq!(l["profiles"][0]["pin_set"], false, "{l}");
    drop(c);
    // p1's env was set once only; the starter set is the box's apps.
    let sets = root
        .read("nix-env.log")
        .lines()
        .filter(|l| l.contains("apps/hinteros --set"))
        .count();
    assert_eq!(sets, 1, "{}", root.read("nix-env.log"));
    assert_eq!(
        env_of(root, &root.p("nixprofiles/starter")).unwrap(),
        "env-steam+youtube"
    );
    let starter: Value = serde_json::from_str(&root.read("var/lib/hinteros/starter.json")).unwrap();
    assert_eq!(starter["ids"], json!(["steam", "youtube"]));
    // The installer's acceptance of Steam's terms counts for p1.
    let mut c = sys.client().await;
    let l = apps_list(&mut c).await;
    assert_eq!(l["ids"], json!(["steam", "youtube"]));
    assert_eq!(l["needs_terms"], json!([]));
    assert_eq!(
        std::fs::read_link(root.p("run/apps/hinteros")).unwrap(),
        p1_link(root)
    );
}

/// A system rollback to a generation from before Steam's box features (apps.md open question
/// 2): the job switches back, rebuilds the envs, then turns the features the envs still need
/// back on in a `box` stage (a new generation). generations.list says so beforehand.
#[tokio::test(flavor = "multi_thread")]
async fn rollback_readds_needed_box_features() {
    let root = Root::create();
    std::fs::write(root.p("etc/hinteros/state.json"), STATE2).unwrap();
    // Generation 1 already had schema 2, without box features.
    std::fs::write(root.p("store/gen1/hinteros/state.json"), STATE2).unwrap();
    std::fs::write(root.p("var/lib/hinteros/profiles.json"), PROFILES).unwrap();
    std::fs::write(root.p("policy/catalog.json"), CATALOG3).unwrap();
    let Some(sys) = System::start(root, &[]).await else {
        return;
    };
    let root = &sys.root;
    let mut c = sys.subscriber().await;
    let rev = apps_list(&mut c).await["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    run_job(
        &sys,
        &mut c,
        "apps.install",
        json!({"ids": ["steam"], "revision": rev, "accepted_terms": {"steam": "2026-09-10"}}),
    )
    .await;
    assert_eq!(
        root.etc_state()["requirements"],
        json!(["graphics32", "steam-devices"])
    );
    // Generation 2 has the features; a rollback to 1 would re-add them.
    let gens = call(&mut c, "generations.list", json!({})).await.unwrap();
    let generation = |n: u64| {
        gens["generations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["generation"] == n)
            .unwrap()
            .clone()
    };
    assert_eq!(
        generation(1)["readds_features"],
        json!(["graphics32", "steam-devices"])
    );
    assert!(generation(2).get("readds_features").is_none(), "{gens}");

    let r = run_job(&sys, &mut c, "rollback", json!({"generation": 1})).await;
    let stages = r["stages"].as_array().unwrap();
    let apps_at = stages.iter().position(|s| s == "apps");
    let box_at = stages.iter().rposition(|s| s == "box");
    assert!(
        apps_at.is_some() && box_at.is_some() && apps_at < box_at,
        "the box stage follows the apps stage: {r}"
    );
    let result = &r["finished"]["result"];
    assert_eq!(
        result["features_readded"],
        json!(["graphics32", "steam-devices"]),
        "{r}"
    );
    // The restored configuration, with the features back on, as a new generation.
    assert_eq!(
        root.etc_state()["requirements"],
        json!(["graphics32", "steam-devices"])
    );
    assert_eq!(result["generation"], 3, "{r}");
    assert_eq!(env_of(root, &p1_link(root)).unwrap(), "env-steam");
}

/// A rollback to a generation that already has every needed feature builds nothing more.
#[tokio::test(flavor = "multi_thread")]
async fn rollback_without_missing_features_builds_once() {
    let root = Root::create();
    std::fs::write(root.p("etc/hinteros/state.json"), STATE2).unwrap();
    std::fs::write(root.p("store/gen1/hinteros/state.json"), STATE2).unwrap();
    std::fs::write(root.p("var/lib/hinteros/profiles.json"), PROFILES).unwrap();
    std::fs::write(root.p("policy/catalog.json"), CATALOG3).unwrap();
    let Some(sys) = System::start(root, &[]).await else {
        return;
    };
    let mut c = sys.subscriber().await;
    run_job(&sys, &mut c, "rebuild", json!({})).await;
    let r = run_job(&sys, &mut c, "rollback", json!({"generation": 1})).await;
    assert!(
        !r["stages"].as_array().unwrap().contains(&json!("box")),
        "{r}"
    );
    assert!(r["finished"]["result"].get("features_readded").is_none());
    assert_eq!(r["finished"]["result"]["generation"], 1);
}
