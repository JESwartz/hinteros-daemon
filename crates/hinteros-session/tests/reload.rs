//! The session daemon picking up a new app list without a restart, Store icons for apps that
//! are not installed, and ending its children when it shuts down.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use hinteros_api::codec::{LineReader, write_message};
use hinteros_api::common::Peer;
use hinteros_api::server::{Connection, Step};
use hinteros_api::{jsonrpc, system};
use serde_json::{Value, json};
use tokio::net::UnixListener;

mod common;

use common::*;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nstore-icon";

/// A local HTTP server (the daemon runs with `--icon-allow-http`): `/logo.png`, and a page
/// whose icon is `/logo.png`. Records the paths requested.
struct Site {
    base: String,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Site {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { return };
                let mut buf = [0u8; 2048];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                log.lock().unwrap().push(path.clone());
                let (status, ctype, body): (&str, &str, &[u8]) = match path.as_str() {
                    "/logo.png" => {
                        // Slow enough for a client to subscribe first.
                        std::thread::sleep(Duration::from_millis(600));
                        ("200 OK", "image/png", PNG)
                    }
                    "/page" => (
                        "200 OK",
                        "text/html",
                        br#"<html><link rel="icon" href="/logo.png"></html>"#,
                    ),
                    _ => ("404 Not Found", "text/plain", b"no"),
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(body);
            }
        });
        Self { base, requests }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn requested(&self, path: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|p| *p == path)
            .count()
    }
}

/// A fake system service that only answers `apps.list` with no installed apps.
fn fake_system(path: &Path) -> tokio::task::JoinHandle<()> {
    fake_system_with(path, |_| None)
}

/// Like [`fake_system`], with more answers from `more` (by system method name).
fn fake_system_with(path: &Path, more: fn(&str) -> Option<Value>) -> tokio::task::JoinHandle<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        let mut conns = tokio::task::JoinSet::new();
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            conns.spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut reader = LineReader::new(r);
                let mut conn = Connection::new(system::VERSION, Peer::new("fake-system", "0"));
                while let Ok(Some(line)) = reader.next_line().await {
                    let reply = match conn.step(&line) {
                        Step::Reply(v) | Step::ReplyAndClose(v) => v,
                        Step::Ignore => continue,
                        Step::Dispatch(req) => {
                            let outcome = match req.method.as_str() {
                                m if more(m).is_some() => Ok(more(m).unwrap()),
                                "subscribe" => Ok(req.params.clone().unwrap_or(json!({}))),
                                "apps.list" => Ok(json!({
                                    "revision": "6b2c1e0b9f4a7d3c5e8f1a2b4c6d8e0f1a3b5c7d9e1f2a4b6c8d0e2f4a6b8c0d",
                                    "generation": null, "ids": [], "needs_terms": [], "box": []
                                })),
                                m => Err(hinteros_api::server::method_not_found(m)),
                            };
                            jsonrpc::response(&req.id.unwrap(), outcome)
                        }
                    };
                    write_message(&mut w, &reply).await.unwrap();
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

fn ids(list: &Value) -> Vec<String> {
    list["apps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap().to_string())
        .collect()
}

fn entry(id: &str, icon: Value) -> Value {
    json!({"id": id, "name": id, "summary": "s", "description": "d", "category": "video",
           "homepage": "https://example.org", "kind": "native", "sizeMiB": 1, "icon": icon,
           "requires": [], "depends": [], "conflicts": [], "installer": true, "onIso": false,
           "installerDefault": false, "dataPaths": [], "terms": null, "status": "available",
           "statusReason": null, "buildsOnDevice": false, "free": true, "redistributable": true})
}

#[tokio::test(flavor = "multi_thread")]
async fn store_entries_carry_resolved_icons() {
    let site = Site::start();
    let d = Daemon::start_with(|dir| {
        // An installed app's desktop file and themed icon.
        let share = dir.join("share");
        let icons = share.join("icons/hicolor/256x256/apps");
        std::fs::create_dir_all(&icons).unwrap();
        std::fs::create_dir_all(share.join("applications")).unwrap();
        std::fs::write(icons.join("org.example.Desk.png"), PNG).unwrap();
        std::fs::write(
            share.join("applications/org.example.Desk.desktop"),
            "[Desktop Entry]\nName=Desk\nIcon=org.example.Desk\n",
        )
        .unwrap();
        let catalog = json!({"indexVersion": 2, "apps": [
            entry("desk", json!({"desktopId": "org.example.Desk"})),
            entry("direct", json!({"iconUrl": site.url("/logo.png")})),
            entry("found", json!({"siteUrl": site.url("/page")})),
            entry("broken", json!({"iconUrl": site.url("/missing.png")})),
            entry("plain", json!({})),
            entry("tls", json!({"iconUrl": "http://127.0.0.1:1/never.png"})),
        ]});
        std::fs::write(dir.join("catalog.json"), catalog.to_string()).unwrap();
        vec!["--icon-allow-http".into()]
    });
    let dir = d.dir.path();
    let task = fake_system(&dir.join("system.sock"));
    let mut c = d.client().await;
    call(&mut c, "subscribe", json!({"events": ["store_changed"]}))
        .await
        .unwrap();

    // The desktop-file icon is there at once; the downloads arrive with store_changed.
    event_named(&mut c, "store_changed").await;
    let list = eventually("the relay", async || {
        call(&mut c, "store.list", json!({})).await.ok()
    })
    .await;
    let cache = dir.join(".cache/hinteros/icons");
    assert_eq!(
        by_id(&list, "desk")["icon"]["path"],
        json!(dir.join("share/icons/hicolor/256x256/apps/org.example.Desk.png"))
    );
    let list = eventually("downloaded icons", async || {
        let list = call(&mut c, "store.list", json!({})).await.ok()?;
        (by_id(&list, "direct")["icon"].get("path").is_some()
            && by_id(&list, "found")["icon"].get("path").is_some())
        .then_some(list)
    })
    .await;
    assert_eq!(
        by_id(&list, "direct")["icon"]["path"],
        json!(cache.join("direct.png"))
    );
    assert_eq!(
        by_id(&list, "found")["icon"]["path"],
        json!(cache.join("found.png"))
    );
    assert_eq!(std::fs::read(cache.join("direct.png")).unwrap(), PNG);
    for id in ["broken", "plain", "tls"] {
        assert!(by_id(&list, id)["icon"].get("path").is_none(), "{id}");
    }
    assert!(site.requested("/missing.png") >= 1);
    // The URLs are still reported as the catalog has them.
    assert_eq!(
        by_id(&list, "direct")["icon"]["icon_url"],
        json!(site.url("/logo.png"))
    );
    task.abort();
}

/// Replace `path` (a symlink, as NixOS does) with one pointing at a new file.
fn switch_config(dir: &Path, generation: u32, apps: &str) {
    let target = dir.join(format!("gen-{generation}.toml"));
    std::fs::write(&target, apps).unwrap();
    let tmp = dir.join("session.toml.new");
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(&target, &tmp).unwrap();
    std::fs::rename(&tmp, dir.join("session.toml")).unwrap();
}

const ONE: &str = r#"
[[apps]]
id = "one"
name = "One"
command = ["sleep", "30"]
"#;

const TWO: &str = r#"
[[apps]]
id = "two"
name = "Two"
command = ["sleep", "30"]
desktop_id = "org.example.Two"
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_switched_config_reloads_the_app_list() {
    let d = Daemon::start_with(|dir| {
        let share = dir.join("share");
        let icons = share.join("icons/hicolor/256x256/apps");
        std::fs::create_dir_all(&icons).unwrap();
        std::fs::create_dir_all(share.join("applications")).unwrap();
        std::fs::write(icons.join("org.example.Two.png"), PNG).unwrap();
        std::fs::write(
            share.join("applications/org.example.Two.desktop"),
            "[Desktop Entry]\nName=Two\nIcon=org.example.Two\n",
        )
        .unwrap();
        switch_config(dir, 1, ONE);
        vec![
            "--config".into(),
            dir.join("session.toml").into(),
            "--no-icon-fetch".into(),
        ]
    });
    let dir = d.dir.path();
    let mut c = d.client().await;
    call(&mut c, "subscribe", json!({"events": ["apps_changed"]}))
        .await
        .unwrap();
    let list = call(&mut c, "apps.list", json!({})).await.unwrap();
    assert_eq!(ids(&list), ["steam", "one"]);
    let one = call(&mut c, "apps.launch", json!({"id": "one"}))
        .await
        .unwrap();
    let one_pid = one["app"]["pid"].as_u64().unwrap() as u32;

    // A switch: the symlink now points at a config without "one" and with "two".
    switch_config(dir, 2, TWO);
    event_named(&mut c, "apps_changed").await;
    let list = call(&mut c, "apps.list", json!({})).await.unwrap();
    assert_eq!(ids(&list), ["steam", "two"]);
    assert_eq!(
        by_id(&list, "two")["icon"],
        json!(dir.join("share/icons/hicolor/256x256/apps/org.example.Two.png"))
    );
    // The removed app is still running, and goes away when it exits; it cannot be started again.
    assert!(alive(one_pid));
    let running = call(&mut c, "apps.running", json!({})).await.unwrap();
    assert_eq!(running["apps"][0]["id"], "one");
    let e = call(&mut c, "apps.launch", json!({"id": "one"}))
        .await
        .unwrap_err();
    assert_eq!(e["code"], hinteros_api::ErrorCode::AppNotFound.code());
    call(&mut c, "apps.launch", json!({"id": "two"}))
        .await
        .unwrap();

    // A broken file changes nothing (and is not announced); the next good one is picked up.
    std::fs::write(dir.join("gen-3.toml"), "[[apps]\nid = ").unwrap();
    let tmp = dir.join("session.toml.new");
    std::os::unix::fs::symlink(dir.join("gen-3.toml"), &tmp).unwrap();
    std::fs::rename(&tmp, dir.join("session.toml")).unwrap();
    eventually("the warning", async || {
        d.log().contains("not reloading the app list").then_some(())
    })
    .await;
    let list = call(&mut c, "apps.list", json!({})).await.unwrap();
    assert_eq!(ids(&list), ["steam", "two"]);
    switch_config(dir, 4, &format!("{ONE}{TWO}"));
    event_named(&mut c, "apps_changed").await;
    let list = call(&mut c, "apps.list", json!({})).await.unwrap();
    assert_eq!(ids(&list), ["steam", "one", "two"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_steam_app_appears_when_steam_is_installed() {
    let d = Daemon::start_env(
        |dir| {
            // Like a box without Steam whose next generation (a Store install) has it.
            switch_config(dir, 1, &format!("[steam]\npath = \"steam\"\n{ONE}"));
            vec![
                "--config".into(),
                dir.join("session.toml").into(),
                "--no-icon-fetch".into(),
            ]
        },
        // No stub: `steam` is looked up on a PATH that does not have it.
        |dir| {
            vec![
                ("HINTEROS_STEAM".into(), "".into()),
                ("PATH".into(), dir.join("bin").into()),
            ]
        },
    );
    let dir = d.dir.path();
    let mut c = d.client().await;
    call(&mut c, "subscribe", json!({"events": ["apps_changed"]}))
        .await
        .unwrap();
    let list = call(&mut c, "apps.list", json!({})).await.unwrap();
    assert_eq!(ids(&list), ["one"]);
    let e = call(&mut c, "apps.launch", json!({"id": "steam"}))
        .await
        .unwrap_err();
    assert_eq!(e["code"], hinteros_api::ErrorCode::AppNotFound.code());
    assert!(
        e["message"]
            .as_str()
            .unwrap()
            .contains("Steam is not installed"),
        "{e}"
    );

    // The new generation points steam.path at an installed Steam.
    std::fs::create_dir_all(dir.join("store/steam/bin")).unwrap();
    script(&dir.join("store/steam/bin/steam"), "exit 0");
    let path = dir.join("store/steam/bin/steam");
    switch_config(
        dir,
        2,
        &format!("[steam]\npath = \"{}\"\n{ONE}", path.display()),
    );
    event_named(&mut c, "apps_changed").await;
    let list = call(&mut c, "apps.list", json!({})).await.unwrap();
    assert_eq!(ids(&list), ["steam", "one"]);
    assert!(d.log().contains("Steam found"));
}

/// An app env as `appEnv.build` makes it: a manifest, and the apps' desktop files and icons.
fn app_env(dir: &Path, name: &str, apps: Value, steam: bool) -> std::path::PathBuf {
    let env = dir.join("store").join(name);
    let share = env.join("share");
    std::fs::create_dir_all(share.join("hinteros")).unwrap();
    std::fs::create_dir_all(share.join("applications")).unwrap();
    std::fs::create_dir_all(share.join("icons/hicolor/256x256/apps")).unwrap();
    std::fs::create_dir_all(env.join("bin")).unwrap();
    for a in apps.as_array().unwrap() {
        if let Some(d) = a.get("desktop_id").and_then(Value::as_str) {
            std::fs::write(
                share.join(format!("icons/hicolor/256x256/apps/{d}.png")),
                PNG,
            )
            .unwrap();
            std::fs::write(
                share.join(format!("applications/{d}.desktop")),
                format!("[Desktop Entry]\nName={d}\nIcon={d}\n"),
            )
            .unwrap();
        }
    }
    let steam = if steam {
        script(&env.join("bin/steam"), "exit 0");
        json!({"path": env.join("bin/steam")})
    } else {
        Value::Null
    };
    let ids: Vec<Value> = apps
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].clone())
        .collect();
    std::fs::write(
        share.join("hinteros/apps.json"),
        json!({"version": 1, "system_rev": null, "ids": ids, "apps": apps, "steam": steam, "box": []})
            .to_string(),
    )
    .unwrap();
    env
}

/// Point the Nix profile `dir/profiles/apps` at `env` as generation `n` (as nix-env does).
fn set_generation(dir: &Path, n: u32, env: &Path) {
    let link = dir.join(format!("profiles/apps-{n}-link"));
    std::os::unix::fs::symlink(env, &link).unwrap();
    let tmp = dir.join("profiles/apps.new");
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(format!("apps-{n}-link"), &tmp).unwrap();
    std::fs::rename(&tmp, dir.join("profiles/apps")).unwrap();
}

/// Profile mode (`[apps] profile`): the apps, Steam and icons come from the profile's app env,
/// and switching the Nix profile's generation reloads them.
#[tokio::test(flavor = "multi_thread")]
async fn a_switched_profile_env_reloads_the_app_list() {
    let d = Daemon::start_env(
        |dir| {
            let one = app_env(
                dir,
                "env-one",
                json!([{"id": "one", "name": "One", "command": ["sleep", "30"]}]),
                false,
            );
            std::fs::create_dir_all(dir.join("profiles")).unwrap();
            std::fs::create_dir_all(dir.join("run")).unwrap();
            set_generation(dir, 1, &one);
            // /run/hinteros/apps/<user> → the Nix profile.
            std::os::unix::fs::symlink(dir.join("profiles/apps"), dir.join("run/tester")).unwrap();
            std::fs::write(
                dir.join("session.toml"),
                format!("[apps]\nprofile = \"{}/run/%u\"\n", dir.display()),
            )
            .unwrap();
            vec![
                "--config".into(),
                dir.join("session.toml").into(),
                "--no-icon-fetch".into(),
            ]
        },
        |_| vec![("USER".into(), "tester".into())],
    );
    let dir = d.dir.path();
    let task = fake_system(&dir.join("system.sock"));
    let mut c = d.client().await;
    call(&mut c, "subscribe", json!({"events": ["apps_changed"]}))
        .await
        .unwrap();
    // No Steam in this env (HINTEROS_STEAM is ignored in profile mode), no stand-in apps.
    let list = call(&mut c, "apps.list", json!({})).await.unwrap();
    assert_eq!(ids(&list), ["one"], "{}", d.log());

    let two = app_env(
        dir,
        "env-two",
        json!([{"id": "two", "name": "Two", "command": ["sleep", "30"], "desktop_id": "org.example.Two"}]),
        true,
    );
    set_generation(dir, 2, &two);
    let list = eventually("the new env", async || {
        let list = call(&mut c, "apps.list", json!({})).await.ok()?;
        (ids(&list) == ["steam", "two"] && by_id(&list, "two").get("icon").is_some())
            .then_some(list)
    })
    .await;
    assert_eq!(
        by_id(&list, "two")["icon"],
        json!(dir.join("run/tester/share/icons/hicolor/256x256/apps/org.example.Two.png"))
    );
    // Gone again: back to an env without apps.
    let none = app_env(dir, "env-none", json!([]), false);
    set_generation(dir, 3, &none);
    eventually("the empty env", async || {
        let list = call(&mut c, "apps.list", json!({})).await.ok()?;
        ids(&list).is_empty().then_some(())
    })
    .await;
    task.abort();
}

/// Starter-set apps with vendor terms: needs_terms on the entry, and apps.launch refuses them
/// until the profile accepts.
#[tokio::test(flavor = "multi_thread")]
async fn unaccepted_terms_block_the_launch() {
    let d = Daemon::start_env(
        |dir| {
            let env = app_env(
                dir,
                "env-plex",
                json!([{"id": "plex", "name": "Plex", "command": ["sleep", "30"]}]),
                false,
            );
            let mut e = entry("plex", json!({}));
            e["terms"] = json!({"title": "Plex Terms", "version": "2025-10-30",
                "url": "https://www.plex.tv/x", "retrieved": "2026-10-06",
                "file": "terms/plex-2025-10-30.txt"});
            std::fs::write(
                dir.join("catalog.json"),
                json!({"indexVersion": 2, "apps": [e]}).to_string(),
            )
            .unwrap();
            vec![
                "--apps-profile".into(),
                env.into(),
                "--no-icon-fetch".into(),
            ]
        },
        |_| Vec::new(),
    );
    let dir = d.dir.path();
    let task = fake_system_with(&dir.join("system.sock"), |m| {
        (m == "apps.list").then(|| {
            json!({"revision": "6b2c1e0b9f4a7d3c5e8f1a2b4c6d8e0f1a3b5c7d9e1f2a4b6c8d0e2f4a6b8c0d",
                   "generation": 1, "ids": ["plex"], "needs_terms": ["plex"], "box": []})
        })
    });
    let mut c = d.client().await;
    let list = eventually("needs_terms", async || {
        let list = call(&mut c, "apps.list", json!({})).await.ok()?;
        by_id(&list, "plex")
            .get("needs_terms")
            .is_some()
            .then_some(list)
    })
    .await;
    let t = &by_id(&list, "plex")["needs_terms"];
    assert_eq!(t["version"], "2025-10-30");
    assert_eq!(t["file"], json!(dir.join("terms/plex-2025-10-30.txt")));
    let e = call(&mut c, "apps.launch", json!({"id": "plex"}))
        .await
        .unwrap_err();
    assert_eq!(e["code"], 3014);
    assert_eq!(e["data"], json!({"id": "plex", "version": "2025-10-30"}));
    task.abort();
}
