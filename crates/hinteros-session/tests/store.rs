//! `store.list` and `store_changed`: the catalog index file merged with a fake system
//! service's installed apps, apps jobs relayed through the daemon, and the update check's
//! `new_apps`.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hinteros_api::codec::{LineReader, write_message};
use hinteros_api::common::Peer;
use hinteros_api::server::{Connection, Step};
use hinteros_api::{ErrorCode, jsonrpc, system};
use serde_json::{Value, json};
use tokio::net::UnixListener;

mod common;

use common::*;

const CATALOG: &str = r#"{"indexVersion": 4, "entries": [
  {"id": "steam", "name": "Steam", "summary": "Play your Steam library on the TV.", "description": "Valve's store.",
   "category": "games", "homepage": "https://store.steampowered.com", "kind": "app", "sizeMiB": 1000,
   "icon": {"desktopId": "steam", "siteUrl": "https://store.steampowered.com"}, "boxFeatures": ["graphics32"],
   "depends": [], "conflicts": [], "onIso": false,
   "dataPaths": [".local/share/Steam", ".steam"],
   "terms": {"title": "Steam Subscriber Agreement", "version": "2026-09-10",
             "url": "https://store.steampowered.com/subscriber_agreement/", "retrieved": "2026-10-06",
             "file": "terms/steam-2026-09-10.txt"},
   "status": "available", "statusReason": null, "unfree": true, "delivery": "download"},
  {"id": "youtube", "name": "YouTube", "summary": "TV", "description": "d", "category": "video",
   "homepage": "https://www.youtube.com", "kind": "app", "sizeMiB": 1, "icon": {"firefoxProfile": true},
   "depends": [], "conflicts": [], "onIso": true,
   "dataPaths": [".local/share/hinteros/firefox/youtube"], "terms": null, "status": "available",
   "statusReason": null, "unfree": false, "delivery": "download"},
  {"id": "old", "name": "Old", "status": "removed", "statusReason": "Gone."}
]}"#;

/// A fake system service: `apps.list` with the shared installed apps, `apps.install` that
/// starts a job (job_started now, job_finished shortly after, adding the apps), and a cached
/// `update.check` result naming a new app.
fn fake_system(path: &Path, installed: Arc<Mutex<Vec<String>>>) -> tokio::task::JoinHandle<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        let mut conns = tokio::task::JoinSet::new();
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let installed = installed.clone();
            conns.spawn(async move {
                let (r, w) = stream.into_split();
                let w = Arc::new(tokio::sync::Mutex::new(w));
                let mut reader = LineReader::new(r);
                let mut conn = Connection::new(system::VERSION, Peer::new("fake-system", "0"));
                while let Ok(Some(line)) = reader.next_line().await {
                    let reply = match conn.step(&line) {
                        Step::Reply(v) | Step::ReplyAndClose(v) => v,
                        Step::Ignore => continue,
                        Step::Dispatch(req) => {
                            let id = req.id.unwrap();
                            let params = req.params.clone().unwrap_or(json!({}));
                            let outcome = match req.method.as_str() {
                                "subscribe" => Ok(params.clone()),
                                "apps.list" => Ok(json!({
                                    "revision": "6b2c1e0b9f4a7d3c5e8f1a2b4c6d8e0f1a3b5c7d9e1f2a4b6c8d0e2f4a6b8c0d",
                                    "generation": null,
                                    "ids": *installed.lock().unwrap(),
                                    "needs_terms": [],
                                    "box": []
                                })),
                                "apps.install" => {
                                    let ids: Vec<String> = serde_json::from_value(params["ids"].clone()).unwrap();
                                    let (w, installed) = (w.clone(), installed.clone());
                                    tokio::spawn(async move {
                                        let job = "00000000000000aa";
                                        let ev = jsonrpc::notification("job_started", json!({"job_id": job, "kind": "apps_install", "uid": 1000, "subject": {}, "started_at": 1791300000}));
                                        write_message(&mut *w.lock().await, &ev).await.unwrap();
                                        tokio::time::sleep(Duration::from_millis(400)).await;
                                        installed.lock().unwrap().extend(ids);
                                        let ev = jsonrpc::notification("job_finished", json!({
                                            "job_id": job, "kind": "apps_install", "ok": true,
                                            "result": {"generation": 2, "rev": null}, "session_restart_required": false,
                                            "subject": {}, "started_at": 1791300000, "finished_at": 1791300001
                                        }));
                                        write_message(&mut *w.lock().await, &ev).await.unwrap();
                                    });
                                    Ok(json!({"job_id": "00000000000000aa"}))
                                }
                                "update.check" => Ok(json!({"job_id": null, "result": {
                                    "channel": "stable", "current_rev": null, "checked_at": 1791300100,
                                    "available": {"rev": "9a8b7c6d5e4f30211a2b3c4d5e6f708192a3b4c5", "date": 1791300000,
                                        "summary": "Release", "signer": "release@hinteros", "security": null, "commits": 1,
                                        "new_apps": [{"id": "moonlight", "name": "Moonlight"}], "removed_apps": []}
                                }})),
                                m => Err(hinteros_api::server::method_not_found(m)),
                            };
                            jsonrpc::response(&id, outcome)
                        }
                    };
                    write_message(&mut *w.lock().await, &reply).await.unwrap();
                }
            });
        }
    })
}

fn by_id<'a>(list: &'a Value, id: &str) -> &'a Value {
    list["apps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == id)
        .unwrap_or_else(|| panic!("{id} not in {list}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn store_list_merges_catalog_and_install_state() {
    let d = Daemon::start();
    let dir = d.dir.path();
    let mut c = d.client_named("hinteros-launcher").await;

    // No catalog on this system.
    let e = call(&mut c, "store.list", json!({})).await.unwrap_err();
    assert_eq!(e["code"], ErrorCode::Unavailable.code());
    assert_eq!(
        e["data"],
        json!({"service": "catalog", "reason": "missing"})
    );
    // A catalog of a version this daemon does not read: older or newer.
    for version in [3, 5] {
        std::fs::write(
            dir.join("catalog.json"),
            format!(r#"{{"indexVersion": {version}, "entries": []}}"#),
        )
        .unwrap();
        let e = call(&mut c, "store.list", json!({})).await.unwrap_err();
        assert_eq!(e["data"]["reason"], "unsupported_version");
    }

    std::fs::write(dir.join("catalog.json"), CATALOG).unwrap();
    std::fs::create_dir_all(dir.join("terms")).unwrap();
    std::fs::write(
        dir.join("terms/steam-2026-09-10.txt"),
        "STEAM SUBSCRIBER AGREEMENT\n",
    )
    .unwrap();
    // The system service is down: installed apps are unknown.
    let e = call(&mut c, "store.list", json!({})).await.unwrap_err();
    assert_eq!(e["code"], ErrorCode::SystemUnavailable.code());

    let installed = Arc::new(Mutex::new(vec!["youtube".to_string(), "old".to_string()]));
    let task = fake_system(&dir.join("system.sock"), installed.clone());
    let list = eventually("the relay", async || {
        call(&mut c, "store.list", json!({})).await.ok()
    })
    .await;
    assert_eq!(list["index_version"], 4);
    assert_eq!(list["more_after_update"], false);
    let ids: Vec<&str> = list["apps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["old", "steam", "youtube"],
        "by name, tombstone kept while installed"
    );
    assert_eq!(by_id(&list, "youtube")["state"], "installed");
    assert_eq!(by_id(&list, "youtube")["unfree"], false);
    assert_eq!(by_id(&list, "youtube")["delivery"], "download");
    assert_eq!(by_id(&list, "old")["state"], "unavailable");
    assert_eq!(by_id(&list, "old")["status_reason"], "Gone.");
    let steam = by_id(&list, "steam");
    assert_eq!(steam["state"], "available");
    assert_eq!(steam["icon"]["desktop_id"], "steam");
    assert_eq!(steam["unfree"], true);
    assert_eq!(steam["delivery"], "download");
    assert_eq!(steam["box_features"], json!(["graphics32"]));
    let terms_file = steam["terms"]["file"].as_str().unwrap();
    assert_eq!(
        Path::new(terms_file),
        dir.join("terms/steam-2026-09-10.txt")
    );
    assert!(
        std::fs::read_to_string(terms_file)
            .unwrap()
            .contains("AGREEMENT")
    );

    // Install through the relay: installing while the job runs, installed after.
    call(
        &mut c,
        "subscribe",
        json!({"events": ["store_changed", "job_finished"]}),
    )
    .await
    .unwrap();
    let r = call(
        &mut c,
        "system.apps.install",
        json!({"ids": ["steam"], "revision": "6b2c1e0b9f4a7d3c5e8f1a2b4c6d8e0f1a3b5c7d9e1f2a4b6c8d0e2f4a6b8c0d",
               "accepted_terms": {"steam": "2026-09-10"}}),
    )
    .await
    .unwrap();
    assert_eq!(r["job_id"], "00000000000000aa");
    event_named(&mut c, "store_changed").await;
    let list = call(&mut c, "store.list", json!({})).await.unwrap();
    assert_eq!(by_id(&list, "steam")["state"], "installing");
    event_named(&mut c, "job_finished").await;
    event_named(&mut c, "store_changed").await;
    let list = call(&mut c, "store.list", json!({})).await.unwrap();
    assert_eq!(by_id(&list, "steam")["state"], "installed");

    // An update check that finds new apps: more after the next update.
    call(&mut c, "system.update.check", json!({}))
        .await
        .unwrap();
    event_named(&mut c, "store_changed").await;
    let list = call(&mut c, "store.list", json!({})).await.unwrap();
    assert_eq!(list["more_after_update"], true);
    task.abort();
}

/// A catalog app's permissions before install: `apps.set_permission`, then `store.list`
/// shows the choice and the profile's permissions.json keeps it.
#[tokio::test(flavor = "multi_thread")]
async fn set_permission_before_install() {
    let d = Daemon::start_with(|dir| {
        std::fs::write(
            dir.join("session.toml"),
            "[sandbox]\nenable = true\npulse_record_socket = \"pulse/record\"\n",
        )
        .unwrap();
        vec!["--config".into(), dir.join("session.toml").into()]
    });
    let dir = d.dir.path();
    std::fs::write(
        dir.join("catalog.json"),
        r#"{"indexVersion": 4, "entries": [
          {"id": "youtube", "kind": "app", "name": "YouTube", "status": "available",
           "permissions": {"mic": "optional", "gamepad": "required"}},
          {"id": "steam", "kind": "app", "name": "Steam", "status": "available",
           "permissions": {"gamepad": "optional"}}]}"#,
    )
    .unwrap();
    let _task = fake_system(&dir.join("system.sock"), Arc::new(Mutex::new(Vec::new())));
    let mut c = d.client_named("hinteros-launcher").await;
    let list = eventually("the relay", async || {
        call(&mut c, "store.list", json!({})).await.ok()
    })
    .await;
    assert_eq!(
        by_id(&list, "youtube")["permissions"],
        json!([{"name": "gamepad", "required": true, "granted": true},
               {"name": "mic", "required": false, "granted": true}])
    );
    assert_eq!(by_id(&list, "steam")["permissions"], json!([]));

    call(
        &mut c,
        "subscribe",
        json!({"events": ["store_changed", "apps_changed"]}),
    )
    .await
    .unwrap();
    let r = call(
        &mut c,
        "apps.set_permission",
        json!({"id": "youtube", "permission": "mic", "granted": false}),
    )
    .await
    .unwrap();
    assert_eq!(r, json!({}));
    event_named(&mut c, "apps_changed").await;
    event_named(&mut c, "store_changed").await;
    let list = call(&mut c, "store.list", json!({})).await.unwrap();
    assert_eq!(by_id(&list, "youtube")["permissions"][1]["granted"], false);
    assert_eq!(
        std::fs::read_to_string(dir.join("state/hinteros/permissions.json")).unwrap(),
        "{\"version\":1,\"denied\":{\"youtube\":[\"mic\"]}}\n"
    );
    let e = call(
        &mut c,
        "apps.set_permission",
        json!({"id": "youtube", "permission": "gamepad", "granted": false}),
    )
    .await
    .unwrap_err();
    assert_eq!(e["code"], ErrorCode::PermissionRequired.code());
    let e = call(
        &mut c,
        "apps.set_permission",
        json!({"id": "nope", "permission": "mic", "granted": false}),
    )
    .await
    .unwrap_err();
    assert_eq!(e["code"], ErrorCode::AppNotFound.code());
}
