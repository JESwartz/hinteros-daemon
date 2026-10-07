//! The polkit authentication agent against a fake polkit authority on a private bus and a fake
//! `polkit-agent-helper-1` (a shell script speaking the helper's line protocol). The fake plays
//! polkit: it records the registration and calls the agent's BeginAuthentication and
//! CancelAuthentication the way polkitd does. Never the machine's polkit.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hinteros_api::ErrorCode;
use serde_json::json;
use tokio::time::timeout;
use zbus::message::Header;
use zbus::zvariant::{OwnedValue, Value as ZValue};

mod common;

use common::*;

const AGENT_PATH: &str = "/org/hinteros/PolkitAgent";
const SSH: &str = "org.hinteros.system.ssh";

#[derive(Debug, Clone)]
struct Registration {
    sender: String,
    kind: String,
    pid: Option<u32>,
    pidfd: bool,
    uid: Option<i32>,
    path: String,
}

#[derive(Default)]
struct Fake {
    registrations: Vec<Registration>,
}

struct Authority(Arc<Mutex<Fake>>);

#[zbus::interface(name = "org.freedesktop.PolicyKit1.Authority")]
impl Authority {
    fn register_authentication_agent(
        &self,
        #[zbus(header)] header: Header<'_>,
        subject: (String, HashMap<String, OwnedValue>),
        _locale: String,
        object_path: String,
    ) {
        self.0.lock().unwrap().registrations.push(Registration {
            sender: header.sender().unwrap().to_string(),
            kind: subject.0,
            pid: subject.1.get("pid").and_then(|v| u32::try_from(v).ok()),
            pidfd: subject.1.contains_key("pidfd"),
            uid: subject.1.get("uid").and_then(|v| i32::try_from(v).ok()),
            path: object_path,
        });
    }
}

async fn polkit(bus: &Bus, fake: Arc<Mutex<Fake>>) -> zbus::Connection {
    let conn = zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    conn.object_server()
        .at("/org/freedesktop/PolicyKit1/Authority", Authority(fake))
        .await
        .unwrap();
    conn.request_name("org.freedesktop.PolicyKit1")
        .await
        .unwrap();
    conn
}

fn me() -> u32 {
    rustix::process::getuid().as_raw()
}

/// What polkit would offer for auth_admin: root and (if it has a name) the test user.
fn identities() -> Vec<(&'static str, HashMap<&'static str, ZValue<'static>>)> {
    vec![
        ("unix-user", HashMap::from([("uid", ZValue::U32(0))])),
        ("unix-user", HashMap::from([("uid", ZValue::U32(me()))])),
    ]
}

/// Call BeginAuthentication on the agent, as polkit does (from `conn`), in the background.
fn begin(
    conn: &zbus::Connection,
    agent: &str,
    cookie: &str,
) -> tokio::task::JoinHandle<Result<(), String>> {
    let (conn, agent, cookie) = (conn.clone(), agent.to_owned(), cookie.to_owned());
    tokio::spawn(async move {
        let details: HashMap<&str, &str> = HashMap::new();
        conn.call_method(
            Some(agent.as_str()),
            AGENT_PATH,
            Some("org.freedesktop.PolicyKit1.AuthenticationAgent"),
            "BeginAuthentication",
            &(
                SSH,
                "Authentication is required to turn remote login (SSH) on or off",
                "",
                details,
                cookie.as_str(),
                identities(),
            ),
        )
        .await
        .map(|_| ())
        .map_err(|e| match e {
            zbus::Error::MethodError(name, _, _) => name.to_string(),
            other => other.to_string(),
        })
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_prompts_the_launcher_and_runs_the_helper() {
    let Some(bus) = Bus::start() else {
        return;
    };
    let fake = Arc::new(Mutex::new(Fake::default()));
    let mut polkit_conn = polkit(&bus, fake.clone()).await;
    let address = bus.address.clone();
    let d = Daemon::start_env(
        |dir| {
            let log = dir.join("helper.log");
            script(
                &dir.join("fake-helper"),
                &format!(
                    r#"user="$1"
read cookie
echo "$user $cookie" >> {log}
printf 'PAM_TEXT_INFO Checking\\tnow\n'
printf 'PAM_PROMPT_ECHO_OFF Password: \n'
read pw
if [ "$pw" = "s3cret" ]; then echo SUCCESS; else echo 'PAM_ERROR_MSG Authentication failure'; echo FAILURE; fi"#,
                    log = log.display()
                ),
            );
            std::fs::write(
                dir.join("session.toml"),
                "[auth]\nprompt_timeout_s = 2\nmax_attempts = 3\n",
            )
            .unwrap();
            vec!["--config".into(), dir.join("session.toml").into()]
        },
        |dir| {
            vec![
                ("HINTEROS_AUTH_BUS".into(), address.into()),
                (
                    "HINTEROS_POLKIT_AGENT_HELPER".into(),
                    dir.join("fake-helper").into(),
                ),
            ]
        },
    );

    // Registered for this process: a unix-process subject with the daemon's pid and pidfd.
    let reg = eventually("the agent to register", async || {
        fake.lock().unwrap().registrations.first().cloned()
    })
    .await;
    assert_eq!(reg.kind, "unix-process");
    assert_eq!(reg.pid, Some(d.child.id()));
    assert!(reg.pidfd, "the subject carries the daemon's pidfd");
    assert_eq!(reg.uid, Some(i32::try_from(me()).unwrap()));
    assert_eq!(reg.path, AGENT_PATH);
    let agent = reg.sender.clone();

    let mut launcher = d.client_named("hinteros-launcher").await;
    let mut other = d.client().await;
    let events = json!({"events": ["authentication_requested", "authentication_finished"]});
    call(&mut launcher, "subscribe", events.clone())
        .await
        .unwrap();
    call(&mut other, "subscribe", events).await.unwrap();

    // 1. A wrong password, then the right one.
    let cookie = "1-6f1c0e2a9b7d4c58-1-0a9f3e1d2c4b5a69";
    let task = begin(&polkit_conn, &agent, cookie);
    let req = event_named(&mut launcher, "authentication_requested").await;
    assert_eq!(req["cookie"], cookie);
    assert_eq!(req["action"], SSH);
    assert!(req["message"].as_str().unwrap().contains("SSH"));
    let first = &req["identities"][0];
    let named_me = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(me()))
        .unwrap()
        .is_some();
    if named_me && me() != 0 {
        assert_eq!(first["uid"], me(), "a non-root identity first");
    }
    let user = first["name"].as_str().unwrap().to_owned();

    // Only the launcher may answer.
    let e = call(
        &mut other,
        "auth.respond",
        json!({"cookie": cookie, "password": "s3cret"}),
    )
    .await
    .unwrap_err();
    assert_eq!(e["code"], ErrorCode::NotAuthorized.code());
    let e = call(&mut other, "auth.cancel", json!({"cookie": cookie}))
        .await
        .unwrap_err();
    assert_eq!(e["code"], ErrorCode::NotAuthorized.code());
    // A password with a line break never reaches the helper.
    let e = call(
        &mut launcher,
        "auth.respond",
        json!({"cookie": cookie, "password": "a\nb"}),
    )
    .await
    .unwrap_err();
    assert_eq!(e["code"], ErrorCode::InvalidParams.code());

    let r = call(
        &mut launcher,
        "auth.respond",
        json!({"cookie": cookie, "password": "wrongpw-1"}),
    )
    .await
    .unwrap();
    assert_eq!(r, json!({"ok": false, "attempts_left": 2}));
    assert!(
        !task.is_finished(),
        "the request stays open after a wrong password"
    );
    let r = call(
        &mut launcher,
        "auth.respond",
        json!({"cookie": cookie, "password": "s3cret"}),
    )
    .await
    .unwrap();
    assert_eq!(r, json!({"ok": true, "attempts_left": 1}));
    assert_eq!(timeout(TIMEOUT, task).await.unwrap().unwrap(), Ok(()));
    let fin = event_named(&mut launcher, "authentication_finished").await;
    assert_eq!(fin, json!({"cookie": cookie, "outcome": "authorized"}));
    let helper_log = std::fs::read_to_string(d.dir.path().join("helper.log")).unwrap();
    assert_eq!(
        helper_log,
        format!("{user} {cookie}\n{user} {cookie}\n"),
        "the helper ran for the chosen identity with the cookie, once per answer"
    );
    let e = call(
        &mut launcher,
        "auth.respond",
        json!({"cookie": cookie, "password": "s3cret"}),
    )
    .await
    .unwrap_err();
    assert_eq!(e["code"], ErrorCode::NotFound.code(), "the request is over");

    // 2. The launcher cancels: polkit sees the dismissal.
    let cookie = "1-6f1c0e2a9b7d4c58-2-1b2c3d4e5f607182";
    let task = begin(&polkit_conn, &agent, cookie);
    event_named(&mut launcher, "authentication_requested").await;
    call(&mut launcher, "auth.cancel", json!({"cookie": cookie}))
        .await
        .unwrap();
    assert_eq!(
        timeout(TIMEOUT, task).await.unwrap().unwrap().unwrap_err(),
        "org.freedesktop.PolicyKit1.Error.Cancelled"
    );
    let fin = event_named(&mut launcher, "authentication_finished").await;
    assert_eq!(fin["outcome"], "cancelled");

    // 3. polkit cancels (CancelAuthentication).
    let cookie = "1-6f1c0e2a9b7d4c58-3-2b2c3d4e5f607182";
    let task = begin(&polkit_conn, &agent, cookie);
    event_named(&mut launcher, "authentication_requested").await;
    polkit_conn
        .call_method(
            Some(agent.as_str()),
            AGENT_PATH,
            Some("org.freedesktop.PolicyKit1.AuthenticationAgent"),
            "CancelAuthentication",
            &(cookie,),
        )
        .await
        .unwrap();
    assert_eq!(
        timeout(TIMEOUT, task).await.unwrap().unwrap().unwrap_err(),
        "org.freedesktop.PolicyKit1.Error.Cancelled"
    );
    assert_eq!(
        event_named(&mut launcher, "authentication_finished").await["outcome"],
        "cancelled"
    );

    // 4. Three wrong passwords: the request fails.
    let cookie = "1-6f1c0e2a9b7d4c58-4-3b2c3d4e5f607182";
    let task = begin(&polkit_conn, &agent, cookie);
    event_named(&mut launcher, "authentication_requested").await;
    for left in [2, 1, 0] {
        let r = call(
            &mut launcher,
            "auth.respond",
            json!({"cookie": cookie, "password": format!("wrongpw-{left}")}),
        )
        .await
        .unwrap();
        assert_eq!(r, json!({"ok": false, "attempts_left": left}));
    }
    assert_eq!(
        timeout(TIMEOUT, task).await.unwrap().unwrap().unwrap_err(),
        "org.freedesktop.PolicyKit1.Error.Failed"
    );
    assert_eq!(
        event_named(&mut launcher, "authentication_finished").await["outcome"],
        "failed"
    );

    // 5. Nobody answers: cancelled after prompt_timeout_s.
    let cookie = "1-6f1c0e2a9b7d4c58-5-4b2c3d4e5f607182";
    let started = Instant::now();
    let task = begin(&polkit_conn, &agent, cookie);
    event_named(&mut launcher, "authentication_requested").await;
    assert_eq!(
        timeout(TIMEOUT, task).await.unwrap().unwrap().unwrap_err(),
        "org.freedesktop.PolicyKit1.Error.Cancelled"
    );
    assert!(started.elapsed() >= Duration::from_secs(2));
    assert_eq!(
        event_named(&mut launcher, "authentication_finished").await["outcome"],
        "cancelled"
    );

    // 6. Someone other than polkit calls the agent: refused, nobody is prompted.
    let intruder = zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    let r = timeout(TIMEOUT, begin(&intruder, &agent, "9-a-9-b"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        r.unwrap_err(),
        "org.freedesktop.PolicyKit1.Error.NotAuthorized"
    );

    // Only the launcher heard any of it.
    call(&mut other, "ping", json!({})).await.unwrap();
    assert!(
        timeout(Duration::from_millis(300), other.next_event_raw())
            .await
            .is_err(),
        "authentication events go only to the launcher"
    );
    assert!(
        timeout(Duration::from_millis(300), launcher.next_event_raw())
            .await
            .is_err(),
        "no prompt for the intruder"
    );

    // 7. polkit restarts: the agent registers again.
    polkit_conn
        .release_name("org.freedesktop.PolicyKit1")
        .await
        .unwrap();
    drop(polkit_conn);
    polkit_conn = polkit(&bus, fake.clone()).await;
    eventually("the agent to register again", async || {
        (fake.lock().unwrap().registrations.len() >= 2).then_some(())
    })
    .await;
    drop(polkit_conn);

    // Passwords are never logged.
    let log = d.log();
    assert!(log.contains("polkit asks for authentication"), "{log}");
    for secret in ["s3cret", "wrongpw"] {
        assert!(!log.contains(secret), "{secret} in the daemon log");
    }
}
