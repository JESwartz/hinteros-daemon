//! `session.restart` against a fake logind (on the runtime bus) and a fake user systemd (on
//! another private bus): only a greetd session of this user on seat0 is terminated, then the
//! daemon's own unit is restarted. Never the machine's logind or user manager.

use std::sync::{Arc, Mutex};

use hinteros_api::ErrorCode;
use serde_json::json;
use zbus::zvariant::OwnedObjectPath;

mod common;

use common::*;

const SESSION_PATH: &str = "/org/freedesktop/login1/session/_32";

#[derive(Default)]
struct Fake {
    service: String,
    terminated: u32,
    restarted: Vec<(String, String)>,
}

type Shared = Arc<Mutex<Fake>>;

fn me() -> u32 {
    rustix::process::getuid().as_raw()
}

struct User;

#[zbus::interface(name = "org.freedesktop.login1.User")]
impl User {
    #[zbus(property)]
    fn display(&self) -> (String, OwnedObjectPath) {
        ("2".into(), OwnedObjectPath::try_from(SESSION_PATH).unwrap())
    }
}

struct Session(Shared);

#[zbus::interface(name = "org.freedesktop.login1.Session")]
impl Session {
    #[zbus(property)]
    fn user(&self) -> (u32, OwnedObjectPath) {
        (
            me(),
            OwnedObjectPath::try_from(format!("/org/freedesktop/login1/user/_{}", me())).unwrap(),
        )
    }
    #[zbus(property)]
    fn seat(&self) -> (String, OwnedObjectPath) {
        (
            "seat0".into(),
            OwnedObjectPath::try_from("/org/freedesktop/login1/seat/seat0").unwrap(),
        )
    }
    #[zbus(property)]
    fn service(&self) -> String {
        self.0.lock().unwrap().service.clone()
    }
    fn terminate(&self) {
        self.0.lock().unwrap().terminated += 1;
    }
}

struct Systemd(Shared);

#[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
impl Systemd {
    fn restart_unit(&self, name: String, mode: String) -> OwnedObjectPath {
        self.0.lock().unwrap().restarted.push((name, mode));
        OwnedObjectPath::try_from("/org/freedesktop/systemd1/job/7").unwrap()
    }
}

async fn serve(bus: &Bus, name: &str, f: impl AsyncFnOnce(&zbus::Connection)) -> zbus::Connection {
    let conn = zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    f(&conn).await;
    conn.request_name(name).await.unwrap();
    conn
}

#[tokio::test(flavor = "multi_thread")]
async fn terminates_only_a_greetd_session_then_restarts_the_daemon() {
    let (Some(system_bus), Some(user_bus)) = (Bus::start(), Bus::start()) else {
        return;
    };
    let fake: Shared = Arc::new(Mutex::new(Fake {
        service: "gdm-password".into(),
        ..Default::default()
    }));
    let f = fake.clone();
    let _logind = serve(&system_bus, "org.freedesktop.login1", async |c| {
        let s = c.object_server();
        s.at(format!("/org/freedesktop/login1/user/_{}", me()), User)
            .await
            .unwrap();
        s.at(SESSION_PATH, Session(f)).await.unwrap();
    })
    .await;
    let f = fake.clone();
    let _systemd = serve(&user_bus, "org.freedesktop.systemd1", async |c| {
        c.object_server()
            .at("/org/freedesktop/systemd1", Systemd(f))
            .await
            .unwrap();
    })
    .await;

    let (sys_addr, user_addr) = (system_bus.address.clone(), user_bus.address.clone());
    let d = Daemon::start_env(
        |dir| {
            std::fs::write(
                dir.join("session.toml"),
                "[session]\nunit = \"hinteros-session.service\"\n",
            )
            .unwrap();
            vec![
                "--config".into(),
                dir.join("session.toml").into(),
                "--runtime-bus".into(),
                sys_addr.into(),
            ]
        },
        |_| vec![("HINTEROS_SESSION_USER_BUS".into(), user_addr.into())],
    );
    let mut c = d.client_named("hinteros-launcher").await;

    // A desktop session (not greetd's) is never ended.
    let e = call(&mut c, "session.restart", json!({}))
        .await
        .unwrap_err();
    assert_eq!(e["code"], ErrorCode::Unavailable.code());
    assert_eq!(
        e["data"],
        json!({"service": "logind", "reason": "not_greetd"})
    );
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(fake.lock().unwrap().terminated, 0);

    // greetd's session: answered first, then terminated, then the daemon's unit restarted.
    fake.lock().unwrap().service = "greetd-greeter".into();
    let r = call(&mut c, "session.restart", json!({})).await.unwrap();
    assert_eq!(r, json!({}));
    eventually("the restart", async || {
        let f = fake.lock().unwrap();
        (f.terminated == 1 && !f.restarted.is_empty()).then(|| f.restarted.clone())
    })
    .await
    .iter()
    .for_each(|(unit, mode)| {
        assert_eq!(unit, "hinteros-session.service");
        assert_eq!(mode, "replace");
    });
}

#[tokio::test]
async fn without_logind_it_is_unavailable() {
    let d = Daemon::start();
    let mut c = d.client().await;
    let e = call(&mut c, "session.restart", json!({}))
        .await
        .unwrap_err();
    assert_eq!(e["code"], ErrorCode::Unavailable.code());
    assert_eq!(e["data"]["service"], "logind");
}
