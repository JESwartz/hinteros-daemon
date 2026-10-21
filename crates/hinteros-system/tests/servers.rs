//! The Server section (system API 0.11, server.md) against the real binary, a fake root and fake
//! services on loopback: a fake Jellyfin (libraries, users, tokens), a fake Plex (the claim), a fake
//! cloudflared `/ready`, a fake tailscaled LocalAPI socket and a fake `tailscale` CLI.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

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
const API_KEY: &str = "0123456789abcdef0123456789abcdef";
const CLAIM: &str = "claim-AbCdEfGhIjKlMnOpQrSt";

const INDEX: &str = r#"{"indexVersion": 3, "apps": [
  {"id": "jellyfin-server", "kind": "service", "type": "service", "name": "Jellyfin server", "status": "available",
   "ports": [{"port": 8096, "proto": "tcp", "from": "lan"}, {"port": 7359, "proto": "udp", "from": "lan"}],
   "setup": "jellyfin", "stateDirs": ["jellyfin"],
   "icon": {"iconUrl": "https://example.org/jellyfin.svg", "sha256": "817c25b47442b192250448fe48cd5f265fe9183bc75aa12030bbe335495255d1", "siteUrl": "https://jellyfin.org"}},
  {"id": "plex-media-server", "kind": "service", "type": "service", "name": "Plex Media Server", "status": "available",
   "ports": [{"port": 32400, "proto": "tcp", "from": "any"}], "setup": "plex", "stateDirs": ["plex"],
   "terms": {"title": "Plex Terms of Service", "version": "2025-10-30", "url": "https://www.plex.tv/", "retrieved": "2026-10-20",
             "file": "terms/plex-media-server-2025-10-30.txt"}},
  {"id": "tailscale", "kind": "service", "type": "service", "name": "Tailscale", "status": "available",
   "ports": [{"port": 41641, "proto": "udp", "from": "any"}], "setup": "tailscale", "stateDirs": ["tailscale"]},
  {"id": "cloudflare-tunnel", "kind": "service", "type": "service", "name": "Cloudflare Tunnel", "status": "available",
   "ports": [], "setup": "cloudflare", "stateDirs": ["hinteros/services/cloudflare-tunnel"]},
  {"id": "steam", "kind": "app", "type": "native", "name": "Steam", "status": "available"}]}"#;

const ROUTE: &str = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
wlan0\t0001A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0\n";

const FIB_TRIE: &str = "Main:\n  +-- 0.0.0.0/0 3 0 5\n     +-- 192.168.1.0/24 2 0 2\n        |-- 192.168.1.42\n           /32 host LOCAL\n";

// ------------------------------------------------------------------------------------- fakes

struct Request {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

fn read_request(s: &mut TcpStream) -> Option<Request> {
    let mut r = BufReader::new(s.try_clone().ok()?);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();
    let mut headers = BTreeMap::new();
    loop {
        let mut h = String::new();
        r.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
        }
    }
    let n: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; n];
    r.read_exact(&mut body).ok()?;
    Some(Request {
        method,
        path,
        headers,
        body,
    })
}

fn respond(s: &mut TcpStream, status: u16, body: &str) {
    let text = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = s.write_all(text.as_bytes());
}

/// A loopback HTTP server answering with `f(request) -> (status, body)`.
fn serve(f: impl Fn(&Request) -> (u16, String) + Send + 'static) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            if let Some(req) = read_request(&mut s) {
                let (status, body) = f(&req);
                respond(&mut s, status, &body);
            }
        }
    });
    addr
}

fn query(path: &str, key: &str) -> Option<String> {
    let q = path.split_once('?')?.1;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| {
            // Percent-decoding of what the daemon encodes.
            let mut out = Vec::new();
            let b = v.as_bytes();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'%' && i + 2 < b.len() + 1 {
                    out.push(u8::from_str_radix(&v[i + 1..i + 3], 16).unwrap());
                    i += 3;
                } else {
                    out.push(b[i]);
                    i += 1;
                }
            }
            String::from_utf8(out).unwrap()
        })
    })
}

#[derive(Default)]
struct Jellyfin {
    /// (item id, name, collection type, path)
    libraries: Vec<(String, String, Option<String>, String)>,
    /// id → (name, password)
    users: BTreeMap<String, (String, String)>,
    tokens: Vec<String>,
    authentications: usize,
    next: u32,
    log: Vec<String>,
}

fn hex_id(n: u32) -> String {
    format!("{n:032x}")
}

fn fake_jellyfin() -> (SocketAddr, Arc<Mutex<Jellyfin>>) {
    let state = Arc::new(Mutex::new(Jellyfin::default()));
    state
        .lock()
        .unwrap()
        .users
        .insert(hex_id(1), ("admin".into(), "adminpw".into()));
    let s = state.clone();
    let addr = serve(move |r| {
        let mut j = s.lock().unwrap();
        j.log.push(format!("{} {}", r.method, r.path));
        let route = r.path.split('?').next().unwrap().to_owned();
        if route == "/System/Info/Public" {
            return (
                200,
                json!({"Id": "f0e1d2c3b4a5968778695a4b3c2d1e0f", "StartupWizardCompleted": true})
                    .to_string(),
            );
        }
        if route == "/Users/AuthenticateByName" {
            let b: Value = serde_json::from_slice(&r.body).unwrap();
            assert!(r.headers["authorization"].contains("DeviceId=\"hinteros-p1\""));
            let found = j
                .users
                .iter()
                .find(|(_, (n, p))| *n == b["Username"] && *p == b["Pw"])
                .map(|(id, _)| id.clone());
            let Some(id) = found else {
                return (401, String::new());
            };
            j.authentications += 1;
            j.next += 1;
            let token = format!("tok{}", j.next);
            j.tokens = vec![token.clone()];
            return (
                200,
                json!({"AccessToken": token, "ServerId": "x", "User": {"Id": id}}).to_string(),
            );
        }
        let auth = r.headers.get("authorization").cloned().unwrap_or_default();
        if route == "/System/Info" {
            let ok = j
                .tokens
                .iter()
                .any(|t| auth == format!("MediaBrowser Token=\"{t}\""));
            return (if ok { 200 } else { 401 }, "{}".into());
        }
        if auth != format!("MediaBrowser Token=\"{API_KEY}\"") {
            return (401, String::new());
        }
        let user_json = |id: &str, name: &str| json!({"Id": id, "Name": name, "Configuration": {"SubtitleMode": "Default"}, "Policy": {"IsAdministrator": false, "AuthenticationProviderId": "a", "PasswordResetProviderId": "b"}});
        match (r.method.as_str(), route.as_str()) {
            ("GET", "/Library/VirtualFolders") => {
                let list: Vec<Value> = j
                    .libraries
                    .iter()
                    .map(|(id, n, t, p)| {
                        let mut v = json!({"Name": n, "Locations": [p], "ItemId": id});
                        if let Some(t) = t {
                            v["CollectionType"] = json!(t);
                        }
                        v
                    })
                    .collect();
                (200, json!(list).to_string())
            }
            ("POST", "/Library/VirtualFolders") => {
                let b: Value = serde_json::from_slice(&r.body).unwrap();
                let path = b["LibraryOptions"]["PathInfos"][0]["Path"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                if !Path::new(&path).is_dir() {
                    return (400, "no such path".into());
                }
                j.next += 1;
                let id = hex_id(1000 + j.next);
                j.libraries.push((
                    id,
                    query(&r.path, "name").unwrap(),
                    query(&r.path, "collectionType"),
                    path,
                ));
                (204, String::new())
            }
            ("DELETE", "/Library/VirtualFolders") => {
                let name = query(&r.path, "name").unwrap();
                let before = j.libraries.len();
                j.libraries.retain(|l| l.1 != name);
                (
                    if j.libraries.len() < before { 204 } else { 404 },
                    String::new(),
                )
            }
            ("GET", "/Users") => {
                let list: Vec<Value> = j
                    .users
                    .iter()
                    .map(|(id, (n, _))| user_json(id, n))
                    .collect();
                (200, json!(list).to_string())
            }
            ("POST", "/Users/New") => {
                let b: Value = serde_json::from_slice(&r.body).unwrap();
                j.next += 1;
                let id = hex_id(2000 + j.next);
                let name = b["Name"].as_str().unwrap().to_owned();
                j.users.insert(
                    id.clone(),
                    (name.clone(), b["Password"].as_str().unwrap().to_owned()),
                );
                (200, user_json(&id, &name).to_string())
            }
            ("POST", "/Users") => {
                let id = query(&r.path, "userId").unwrap();
                let b: Value = serde_json::from_slice(&r.body).unwrap();
                assert!(
                    b["Configuration"].is_object(),
                    "rename without Configuration"
                );
                if let Some(u) = j.users.get_mut(&id) {
                    u.0 = b["Name"].as_str().unwrap().to_owned();
                }
                (204, String::new())
            }
            ("POST", p) if p.ends_with("/Policy") => {
                let b: Value = serde_json::from_slice(&r.body).unwrap();
                assert_eq!(b["IsAdministrator"], false);
                assert_eq!(b["EnableContentDeletion"], false);
                (204, String::new())
            }
            ("GET", p) if p.starts_with("/Users/") => {
                let id = &p["/Users/".len()..];
                match j.users.get(id) {
                    Some((n, _)) => (200, user_json(id, n).to_string()),
                    None => (404, String::new()),
                }
            }
            ("DELETE", p) if p.starts_with("/Users/") => {
                let id = p["/Users/".len()..].to_owned();
                (
                    if j.users.remove(&id).is_some() {
                        204
                    } else {
                        404
                    },
                    String::new(),
                )
            }
            _ => (404, String::new()),
        }
    });
    (addr, state)
}

/// A fake tailscaled LocalAPI on a unix socket, answering status with `backend`.
fn fake_tailscaled(socket: &Path, backend: Arc<Mutex<String>>) {
    let l = std::os::unix::net::UnixListener::bind(socket).unwrap();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            assert!(req.starts_with("GET /localapi/v0/status"), "{req}");
            assert!(req.contains("Host: local-tailscaled.sock\r\n"), "{req}");
            let state = backend.lock().unwrap().clone();
            let body = json!({"BackendState": state, "HaveNodeKey": false, "AuthURL": "",
                "Self": {"HostName": "hinteros", "DNSName": "hinteros.tail1234.ts.net.", "TailscaleIPs": ["100.101.102.103"], "KeyExpiry": "2027-04-18T10:00:00Z"},
                "CurrentTailnet": {"Name": "owner@example.com"}})
            .to_string();
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
}

// ------------------------------------------------------------------------------------- setup

fn root() -> Root {
    let root = Root::create();
    std::fs::write(root.p("etc/hinteros/state.json"), STATE2).unwrap();
    std::fs::write(root.p("var/lib/hinteros/profiles.json"), PROFILES).unwrap();
    hinteros_system::pins::Pins::new(root.p("var/lib/hinteros/pins"))
        .set("p1", ADMIN_PIN)
        .unwrap();
    std::fs::write(root.p("policy/catalog.json"), INDEX).unwrap();
    std::fs::write(
        root.p("policy/terms/plex-media-server-2025-10-30.txt"),
        "terms",
    )
    .unwrap();
    // The starter env, so no profile job runs behind the test's back.
    let env = root.p("store/env-empty");
    std::fs::create_dir_all(env.join("share/hinteros")).unwrap();
    std::fs::write(
        env.join("share/hinteros/apps.json"),
        r#"{"version":1,"ids":[],"apps":[],"box":[]}"#,
    )
    .unwrap();
    std::fs::create_dir_all(root.p("nixprofiles/apps")).unwrap();
    for link in ["nixprofiles/starter", "nixprofiles/apps/hinteros"] {
        std::os::unix::fs::symlink(&env, root.p(link)).unwrap();
    }
    std::fs::create_dir_all(root.p("proc/net")).unwrap();
    std::fs::write(root.p("proc/net/route"), ROUTE).unwrap();
    std::fs::write(root.p("proc/net/fib_trie"), FIB_TRIE).unwrap();
    // /srv/media: two folders, a nested one, a symlink out and a file.
    for d in ["media/Movies/Kids", "media/Music", "outside/secret"] {
        std::fs::create_dir_all(root.p(d)).unwrap();
    }
    std::fs::write(root.p("media/notes.txt"), "x").unwrap();
    std::os::unix::fs::symlink(root.p("outside"), root.p("media/Escape")).unwrap();
    // What the system repo's Jellyfin setup unit leaves.
    let jf = root.p("var/lib/hinteros/services/jellyfin-server");
    std::fs::create_dir_all(&jf).unwrap();
    std::fs::write(jf.join("setup-done"), "").unwrap();
    std::fs::write(jf.join("api-key"), API_KEY).unwrap();
    std::fs::write(jf.join("admin-password"), "adminpw").unwrap();
    // Service data that delete_data removes, and what it must leave.
    for d in [
        "var/lib/jellyfin/data",
        "var/cache/jellyfin/images",
        "var/lib/plex",
    ] {
        std::fs::create_dir_all(root.p(d)).unwrap();
    }
    std::fs::write(root.p("var/lib/jellyfin/data/library.db"), "db").unwrap();
    std::os::unix::fs::symlink(root.p("outside"), root.p("var/lib/tailscale")).unwrap();
    root
}

async fn revision(c: &mut Client) -> String {
    call(c, "services.list", json!({})).await.unwrap()["revision"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn run_job(
    sys: &System,
    c: &mut Client,
    method: &str,
    params: Value,
) -> Vec<(String, Value)> {
    let r = call(c, method, params.clone())
        .await
        .unwrap_or_else(|e| panic!("{method}: {e}"));
    let id = r["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{method}: no job: {r}"))
        .to_owned();
    let events = until_finished(c, &id).await;
    let done = finished(&events).clone();
    assert_eq!(done["ok"], true, "{method}: {done}\n{}", sys.job_log(&id));
    events
}

fn service<'a>(list: &'a Value, id: &str) -> &'a Value {
    list["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == id)
        .unwrap_or_else(|| panic!("no {id} in {list}"))
}

fn check(method: &str, v: &Value) {
    hinteros_api::conformance::ApiSchema::system()
        .check_result(method, v)
        .unwrap_or_else(|e| panic!("{e}"));
}

// ------------------------------------------------------------------------------------- tests

#[tokio::test(flavor = "multi_thread")]
async fn services_jellyfin_and_delete_data() {
    let (jf_addr, jf) = fake_jellyfin();
    let addr = jf_addr.to_string();
    let Some(sys) = System::start(root(), &["--jellyfin-addr", &addr]).await else {
        return;
    };
    let root = &sys.root;
    sys.fakes
        .lock()
        .unwrap()
        .slices
        .insert("jellyfin.service".into(), String::new());
    let mut c = sys.subscriber().await;

    // Everything off: no detail, Plex's terms with the text's path.
    let list = call(&mut c, "services.list", json!({})).await.unwrap();
    check("services.list", &list);
    let ids: Vec<&str> = list["services"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "cloudflare-tunnel",
            "jellyfin-server",
            "plex-media-server",
            "tailscale"
        ]
    );
    assert_eq!(service(&list, "jellyfin-server")["detail"], Value::Null);
    assert_eq!(service(&list, "jellyfin-server")["status"], Value::Null);
    // The catalog's icon (the session daemon adds the resolved path); data to delete.
    let icon = &service(&list, "jellyfin-server")["icon"];
    assert_eq!(icon["icon_url"], "https://example.org/jellyfin.svg");
    assert_eq!(icon["site_url"], "https://jellyfin.org");
    assert_eq!(icon["sha256"].as_str().unwrap().len(), 64);
    assert_eq!(icon.get("path"), None);
    assert_eq!(
        service(&list, "tailscale")["icon"],
        json!({"firefox_profile": false})
    );
    for s in list["services"].as_array().unwrap() {
        assert_eq!(s["has_data"], true, "{s}");
    }
    let terms = &service(&list, "plex-media-server")["terms"];
    assert_eq!(terms["version"], "2025-10-30");
    assert!(Path::new(terms["file"].as_str().unwrap()).is_file());
    let rev = list["revision"].as_str().unwrap().to_owned();

    // Refusals.
    let enable = |ids: Value, pin: &str| json!({"ids": ids, "pin": pin, "revision": rev});
    let e = call(
        &mut c,
        "services.enable",
        enable(json!(["jellyfin-server"]), "1111"),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::PinRequired.code(), "{e}");
    let e = call(
        &mut c,
        "services.enable",
        enable(json!(["steam"]), ADMIN_PIN),
    )
    .await
    .unwrap_err();
    assert_eq!(e["data"]["reason"], "not_a_service", "{e}");
    let e = call(
        &mut c,
        "services.enable",
        enable(json!(["nope"]), ADMIN_PIN),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound.code());
    let e = call(
        &mut c,
        "services.enable",
        enable(json!(["plex-media-server"]), ADMIN_PIN),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::TermsRequired.code(), "{e}");
    let e = call(
        &mut c,
        "services.enable",
        json!({"ids": ["tailscale"], "pin": ADMIN_PIN, "revision": "0".repeat(64)}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::StateConflict.code());
    let e = call(
        &mut c,
        "jellyfin.libraries.add",
        json!({"pin": ADMIN_PIN, "name": "Movies", "kind": "movies", "folder": "Movies"}),
    )
    .await
    .unwrap_err();
    assert_eq!(
        e["data"],
        json!({"service": "jellyfin", "reason": "off"}),
        "{e}"
    );

    // On: state.json, and James's Jellyfin user (not an administrator).
    run_job(
        &sys,
        &mut c,
        "services.enable",
        enable(json!(["jellyfin-server"]), ADMIN_PIN),
    )
    .await;
    assert_eq!(root.etc_state()["services"], json!(["jellyfin-server"]));
    let users_json = root.p("var/lib/hinteros/services/jellyfin-server/users.json");
    let stored: Value =
        serde_json::from_str(&root.read("var/lib/hinteros/services/jellyfin-server/users.json"))
            .unwrap();
    assert_eq!(stored["users"]["p1"]["name"], "James");
    assert_eq!(
        std::fs::metadata(&users_json).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let names: Vec<String> = jf
        .lock()
        .unwrap()
        .users
        .values()
        .map(|u| u.0.clone())
        .collect();
    assert_eq!(names, ["admin", "James"]);
    // Again: nothing to do.
    let rev2 = revision(&mut c).await;
    let r = call(
        &mut c,
        "services.enable",
        json!({"ids": ["jellyfin-server"], "pin": ADMIN_PIN, "revision": rev2}),
    )
    .await
    .unwrap();
    assert_eq!(r["job_id"], Value::Null);

    let list = call(&mut c, "services.list", json!({})).await.unwrap();
    check("services.list", &list);
    let j = service(&list, "jellyfin-server");
    assert_eq!(j["status"], "active");
    assert_eq!(j["detail"]["setup_done"], true);
    assert_eq!(j["detail"]["url"], "http://192.168.1.42:8096");
    assert_eq!(
        j["detail"]["users"],
        json!([{"profile_id": "p1", "jellyfin_user": "James"}])
    );

    // Folders: one level, no symlink, no file, no way out.
    let f = call(&mut c, "media.folders", json!({})).await.unwrap();
    check("media.folders", &f);
    assert_eq!(
        f["folders"],
        json!([{"id": "Movies", "name": "Movies", "has_children": true}, {"id": "Music", "name": "Music", "has_children": false}])
    );
    let f = call(&mut c, "media.folders", json!({"folder_id": "Movies"}))
        .await
        .unwrap();
    assert_eq!(f["folders"][0]["id"], "Movies/Kids");
    let e = call(&mut c, "media.folders", json!({"folder_id": "Escape"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound.code());
    let e = call(&mut c, "media.folders", json!({"folder_id": "../outside"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidParams.code());

    // Libraries.
    let e = call(
        &mut c,
        "jellyfin.libraries.add",
        json!({"pin": ADMIN_PIN, "name": "Secret", "kind": "movies", "folder": "Escape"}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound.code(), "{e}");
    let events = run_job(&sys, &mut c, "jellyfin.libraries.add", json!({"pin": ADMIN_PIN, "name": "Kids & Family", "kind": "shows", "folder": "Movies/Kids"})).await;
    let lib = jf.lock().unwrap().libraries[0].clone();
    assert_eq!(lib.1, "Kids & Family");
    assert_eq!(lib.2.as_deref(), Some("tvshows"));
    assert_eq!(Path::new(&lib.3), root.p("media/Movies/Kids"));
    assert!(
        events
            .iter()
            .any(|(n, p)| n == "job_progress" && p["subject"]["library"]["id"] == lib.0),
        "{events:?}"
    );
    let e = call(
        &mut c,
        "jellyfin.libraries.add",
        json!({"pin": ADMIN_PIN, "name": "kids & family", "kind": "mixed", "folder": "Music"}),
    )
    .await
    .unwrap_err();
    assert_eq!(
        e["data"],
        json!({"field": "name", "reason": "exists"}),
        "{e}"
    );
    run_job(
        &sys,
        &mut c,
        "jellyfin.libraries.add",
        json!({"pin": ADMIN_PIN, "name": "Music", "kind": "mixed", "folder": "Music"}),
    )
    .await;
    assert_eq!(jf.lock().unwrap().libraries[1].2, None);
    let list = call(&mut c, "services.list", json!({})).await.unwrap();
    let libs = &service(&list, "jellyfin-server")["detail"]["libraries"];
    assert_eq!(
        libs[0],
        json!({"id": lib.0, "name": "Kids & Family", "kind": "shows", "folder": "Movies/Kids"})
    );
    assert_eq!(libs[1]["kind"], "mixed");
    run_job(
        &sys,
        &mut c,
        "jellyfin.libraries.remove",
        json!({"pin": ADMIN_PIN, "library_id": lib.0}),
    )
    .await;
    assert_eq!(jf.lock().unwrap().libraries.len(), 1);
    assert!(root.p("media/Movies/Kids").is_dir(), "media stay");
    let e = call(
        &mut c,
        "jellyfin.libraries.remove",
        json!({"pin": ADMIN_PIN, "library_id": lib.0}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound.code());

    // Passwords behind the PIN.
    let e = call(&mut c, "jellyfin.users.reveal", json!({"pin": "1111"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::PinRequired.code());
    let r = call(&mut c, "jellyfin.users.reveal", json!({"pin": ADMIN_PIN}))
        .await
        .unwrap();
    check("jellyfin.users.reveal", &r);
    assert_eq!(r["admin"], json!({"name": "admin", "password": "adminpw"}));
    let pw = stored["users"]["p1"]["password"].as_str().unwrap();
    assert_eq!(
        r["users"],
        json!([{"profile_id": "p1", "profile_name": "James", "name": "James", "password": pw}])
    );

    // The caller's own token, kept and made again when Jellyfin drops it.
    let t = call(&mut c, "jellyfin.client_token", json!({}))
        .await
        .unwrap();
    check("jellyfin.client_token", &t);
    assert_eq!(t["device_id"], "hinteros-p1");
    assert_eq!(t["server_id"], "f0e1d2c3b4a5968778695a4b3c2d1e0f");
    let t2 = call(&mut c, "jellyfin.client_token", json!({}))
        .await
        .unwrap();
    assert_eq!(t, t2);
    assert_eq!(jf.lock().unwrap().authentications, 1);
    jf.lock().unwrap().tokens.clear();
    let t3 = call(&mut c, "jellyfin.client_token", json!({}))
        .await
        .unwrap();
    assert_ne!(t3["access_token"], t["access_token"]);
    assert_eq!(jf.lock().unwrap().authentications, 2);

    // A rename follows (in the background).
    call(
        &mut c,
        "profiles.rename",
        json!({"id": "p1", "name": "Jim 🎮"}),
    )
    .await
    .unwrap();
    eventually("the Jellyfin user renamed", || {
        jf.lock().unwrap().users.values().any(|u| u.0 == "Jim")
    })
    .await;

    // No secret in the audit or the service's log.
    let audit = format!("{:?}", sys.audit());
    for secret in [pw, "adminpw", t3["access_token"].as_str().unwrap()] {
        assert!(
            !audit.contains(secret) && !sys.log().contains(secret),
            "{secret} leaked"
        );
    }

    // Off, with its data: /var/lib/jellyfin, its cache, HinterOS's files; nothing else.
    let rev = revision(&mut c).await;
    let events = run_job(
        &sys,
        &mut c,
        "services.disable",
        json!({"ids": ["jellyfin-server"], "pin": ADMIN_PIN, "revision": rev, "delete_data": true}),
    )
    .await;
    assert_eq!(finished(&events)["subject"]["delete_data"], true);
    assert_eq!(root.etc_state()["services"], json!([]));
    assert!(!root.p("var/lib/jellyfin").exists());
    assert!(!root.p("var/cache/jellyfin").exists());
    assert!(!root.p("var/lib/hinteros/services/jellyfin-server").exists());
    assert!(root.p("var/lib/plex").is_dir() && root.p("outside/secret").is_dir());
    assert!(root.p("media/Movies/Kids").is_dir());
    let rev = revision(&mut c).await;
    let e = call(&mut c, "services.disable", json!({"ids": ["jellyfin-server"], "pin": ADMIN_PIN, "revision": rev, "delete_data": false})).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound.code());
}

#[tokio::test(flavor = "multi_thread")]
async fn plex_cloudflare_tailscale() {
    let prefs_dir = tempfile::tempdir().unwrap();
    let prefs = prefs_dir.path().join("Preferences.xml");
    std::fs::write(&prefs, r#"<Preferences ProcessedMachineIdentifier="abc123" FriendlyName="Box" PlexOnlineToken=""/>"#).unwrap();
    let claims: Arc<Mutex<Vec<String>>> = Arc::default();
    let (p2, c2) = (prefs.clone(), claims.clone());
    let plex = serve(move |r| {
        if r.path == "/identity" {
            return (200, "{}".into());
        }
        if r.method == "POST" && r.path.starts_with("/myplex/claim?token=") {
            assert_eq!(r.headers["x-plex-client-identifier"], "abc123");
            c2.lock().unwrap().push(query(&r.path, "token").unwrap());
            std::fs::write(&p2, r#"<Preferences ProcessedMachineIdentifier="abc123" FriendlyName="Box" PlexOnlineToken="secret-plex-token"/>"#).unwrap();
            return (200, "{}".into());
        }
        (404, String::new())
    });
    let ready = serve(|r| {
        if r.path == "/ready" {
            (200, r#"{"status":200}"#.into())
        } else {
            (404, String::new())
        }
    });
    let (plex_s, ready_s, prefs_s) = (
        plex.to_string(),
        ready.to_string(),
        prefs.display().to_string(),
    );
    let root = root();
    // The tailscale CLI: the sign-in URL, then Running (pretty-printed, as the real one).
    script(
        &root.p("bin/tailscale"),
        r#"echo "$@" >> "$FAKE_ROOT/tailscale.log"
case "$1" in
up) printf '{\n\t"AuthURL": "https://login.tailscale.com/a/1a2b3c",\n\t"BackendState": "NeedsLogin"\n}\n'; sleep 1; printf '{\n  "BackendState": "Running"\n}\n' ;;
logout) ;;
esac"#,
    );
    let backend = Arc::new(Mutex::new("NeedsLogin".to_owned()));
    fake_tailscaled(&root.p("run/tailscaled.sock"), backend.clone());
    let Some(sys) = System::start(
        root,
        &[
            "--plex-addr",
            &plex_s,
            "--cloudflared-metrics",
            &ready_s,
            "--plex-preferences",
            &prefs_s,
        ],
    )
    .await
    else {
        return;
    };
    let root = &sys.root;
    let mut c = sys.subscriber().await;
    let rev = revision(&mut c).await;
    run_job(
        &sys,
        &mut c,
        "services.enable",
        json!({"ids": ["plex-media-server", "cloudflare-tunnel", "tailscale"], "pin": ADMIN_PIN, "revision": rev,
               "accepted_terms": {"plex-media-server": "2025-10-30"}}),
    )
    .await;
    let terms: Value =
        serde_json::from_str(&root.read("var/lib/hinteros/terms-accepted.json")).unwrap();
    assert_eq!(terms["plex-media-server"][0]["version"], "2025-10-30");
    assert_eq!(terms["plex-media-server"][0]["profile"], "p1");

    let list = call(&mut c, "services.list", json!({})).await.unwrap();
    check("services.list", &list);
    assert_eq!(
        service(&list, "plex-media-server")["detail"],
        json!({"claimed": false, "server_name": "Box", "url": "http://192.168.1.42:32400"})
    );
    assert_eq!(
        service(&list, "cloudflare-tunnel")["detail"],
        json!({"token_set": false, "connected": false, "local_ip": "192.168.1.42"})
    );
    let ts = &service(&list, "tailscale")["detail"];
    assert_eq!(ts["state"], "needs_login");
    assert_eq!(ts["ip4"], Value::Null);

    // Plex.
    let e = call(
        &mut c,
        "plex.claim",
        json!({"pin": ADMIN_PIN, "code": "claim-short"}),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), ErrorCode::InvalidParams.code());
    run_job(
        &sys,
        &mut c,
        "plex.claim",
        json!({"pin": ADMIN_PIN, "code": CLAIM}),
    )
    .await;
    assert_eq!(*claims.lock().unwrap(), [CLAIM]);
    let list = call(&mut c, "services.list", json!({})).await.unwrap();
    assert_eq!(
        service(&list, "plex-media-server")["detail"]["claimed"],
        true
    );
    assert!(!list.to_string().contains("secret-plex-token"));
    let r = call(
        &mut c,
        "plex.claim",
        json!({"pin": ADMIN_PIN, "code": CLAIM}),
    )
    .await
    .unwrap();
    assert_eq!(r["job_id"], Value::Null, "linked already");

    // Cloudflare.
    let e = call(
        &mut c,
        "cloudflare.set_token",
        json!({"pin": ADMIN_PIN, "token": "eyJhIjoiYiJ9"}),
    )
    .await
    .unwrap_err();
    assert_eq!(e["data"]["field"], "token");
    let token = "eyJhIjoiMDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWYiLCJ0IjoiNmYxYzhhMmUtM2I0ZC00ZTVmLThhOWItMGMxZDJlM2Y0YTViIiwicyI6IkJ3Y0hCd2NIQndjSEJ3Y0hCd2NIQndjSEJ3Y0hCd2NIQndjSEJ3Y0hCd2M9In0=";
    run_job(
        &sys,
        &mut c,
        "cloudflare.set_token",
        json!({"pin": ADMIN_PIN, "token": token}),
    )
    .await;
    let file = root.p("var/lib/hinteros/services/cloudflare-tunnel/token");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), token);
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        sys.fakes
            .lock()
            .unwrap()
            .units
            .contains(&"restart hinteros-cloudflared.service".to_owned())
    );
    let list = call(&mut c, "services.list", json!({})).await.unwrap();
    assert_eq!(
        service(&list, "cloudflare-tunnel")["detail"]["connected"],
        true
    );
    run_job(
        &sys,
        &mut c,
        "cloudflare.clear_token",
        json!({"pin": ADMIN_PIN}),
    )
    .await;
    assert!(!file.exists());
    assert!(
        sys.fakes
            .lock()
            .unwrap()
            .units
            .contains(&"stop hinteros-cloudflared.service".to_owned())
    );
    let r = call(&mut c, "cloudflare.clear_token", json!({"pin": ADMIN_PIN}))
        .await
        .unwrap();
    assert_eq!(r["job_id"], Value::Null);

    // Tailscale: the sign-in URL comes in job_progress.
    let events = run_job(&sys, &mut c, "tailscale.login", json!({"pin": ADMIN_PIN})).await;
    let url = events.iter().find(|(n, p)| {
        n == "job_progress" && p["stage"] == "signing_in" && p["detail"]["auth_url"].is_string()
    });
    let (_, p) = url.unwrap_or_else(|| panic!("no sign-in URL in {events:?}"));
    assert_eq!(
        p["detail"]["auth_url"],
        "https://login.tailscale.com/a/1a2b3c"
    );
    hinteros_api::conformance::ApiSchema::system()
        .check_event("job_progress", p)
        .unwrap();
    let log = root.read("tailscale.log");
    assert!(log.starts_with("up --reset --json --timeout=600s"), "{log}");
    *backend.lock().unwrap() = "Running".into();
    let list = call(&mut c, "services.list", json!({})).await.unwrap();
    let ts = &service(&list, "tailscale")["detail"];
    assert_eq!(ts["state"], "running");
    assert_eq!(ts["ip4"], "100.101.102.103");
    assert_eq!(ts["dns_name"], "hinteros.tail1234.ts.net");
    let r = call(&mut c, "tailscale.login", json!({"pin": ADMIN_PIN}))
        .await
        .unwrap();
    assert_eq!(r["job_id"], Value::Null, "signed in already");
    run_job(&sys, &mut c, "tailscale.logout", json!({"pin": ADMIN_PIN})).await;
    assert!(root.read("tailscale.log").contains("logout"));

    // Off with data: tailscale's stateDir is a symlink here; the link goes, not its target.
    let rev = revision(&mut c).await;
    run_job(&sys, &mut c, "services.disable", json!({"ids": ["tailscale", "plex-media-server"], "pin": ADMIN_PIN, "revision": rev, "delete_data": true})).await;
    assert!(!root.p("var/lib/tailscale").exists() && root.p("outside/secret").is_dir());
    assert!(!root.p("var/lib/plex").exists());

    // Secrets never in the audit (claim code, token) and the service's log.
    let audit = format!("{:?}", sys.audit());
    for secret in [CLAIM, token, "secret-plex-token"] {
        assert!(!audit.contains(secret), "{secret} in the audit");
        assert!(!sys.log().contains(secret), "{secret} in the log");
    }
    assert!(audit.contains("org.hinteros.system.services"));
}
