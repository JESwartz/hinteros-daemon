//! Power methods against a fake logind on a private bus: capabilities, the calls logind gets
//! (never interactive), logind's refusals, and the daemon without logind. Skipped when
//! `dbus-daemon` is not on PATH, unless HINTEROS_REQUIRE_TEST_TOOLS is set.

use std::sync::{Arc, Mutex};

use serde_json::json;

mod common;

use common::*;

/// logind's two refusals, with their real D-Bus error names.
#[derive(Debug)]
enum Error {
    BlockedByInhibitor(String),
    Denied(String),
}

impl Error {
    fn parts(&self) -> (&'static str, &str) {
        match self {
            Error::BlockedByInhibitor(t) => ("org.freedesktop.login1.BlockedByInhibitor", t),
            Error::Denied(t) => ("org.freedesktop.DBus.Error.AccessDenied", t),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.parts().0, self.parts().1)
    }
}

impl std::error::Error for Error {}

impl zbus::DBusError for Error {
    fn create_reply(&self, call: &zbus::message::Header<'_>) -> zbus::Result<zbus::Message> {
        let (name, text) = self.parts();
        zbus::Message::error(call, name)?.build(&(text,))
    }
    fn name(&self) -> zbus::names::ErrorName<'_> {
        zbus::names::ErrorName::from_static_str_unchecked(self.parts().0)
    }
    fn description(&self) -> Option<&str> {
        Some(self.parts().1)
    }
}

#[derive(Default)]
struct Model {
    calls: Vec<String>,
    /// What every action answers with.
    refuse: Option<Refusal>,
}

#[derive(Clone)]
enum Refusal {
    Inhibited,
    Denied,
}

struct Manager(Arc<Mutex<Model>>);

impl Manager {
    fn act(&self, what: &str, interactive: bool) -> Result<(), Error> {
        let mut m = self.0.lock().unwrap();
        m.calls.push(format!("{what} interactive={interactive}"));
        match &m.refuse {
            None => Ok(()),
            Some(Refusal::Inhibited) => Err(Error::BlockedByInhibitor(
                r#"Operation inhibited by "hinteros-system" (PID 812 "hinteros-system", user root), reason is "switching to a new system"."#.into(),
            )),
            Some(Refusal::Denied) => Err(Error::Denied(
                "Interactive authentication required.".into(),
            )),
        }
    }
}

#[zbus::interface(name = "org.freedesktop.login1.Manager")]
impl Manager {
    fn can_suspend(&self) -> String {
        "yes".into()
    }
    fn can_reboot(&self) -> String {
        "challenge".into()
    }
    fn can_power_off(&self) -> String {
        "na".into()
    }
    fn suspend(&self, interactive: bool) -> Result<(), Error> {
        self.act("Suspend", interactive)
    }
    fn reboot(&self, interactive: bool) -> Result<(), Error> {
        self.act("Reboot", interactive)
    }
    fn power_off(&self, interactive: bool) -> Result<(), Error> {
        self.act("PowerOff", interactive)
    }
}

async fn fake_logind(bus: &Bus) -> (zbus::Connection, Arc<Mutex<Model>>) {
    let model = Arc::new(Mutex::new(Model::default()));
    let conn = zbus::connection::Builder::address(bus.address.as_str())
        .unwrap()
        .serve_at("/org/freedesktop/login1", Manager(model.clone()))
        .unwrap()
        .name("org.freedesktop.login1")
        .unwrap()
        .build()
        .await
        .unwrap();
    (conn, model)
}

#[tokio::test]
async fn capabilities_and_actions() {
    let Some(bus) = Bus::start() else { return };
    let (_logind, model) = fake_logind(&bus).await;
    let d = Daemon::start_with(|_| runtime_bus(&bus));
    let mut c = d.client().await;

    let caps = call(&mut c, "power.capabilities", json!({})).await.unwrap();
    assert_eq!(
        caps,
        json!({"suspend": "yes", "reboot": "challenge", "poweroff": "na"})
    );

    for (method, what) in [
        ("power.suspend", "Suspend"),
        ("power.reboot", "Reboot"),
        ("power.poweroff", "PowerOff"),
    ] {
        assert_eq!(call(&mut c, method, json!({})).await.unwrap(), json!({}));
        let calls = model.lock().unwrap().calls.clone();
        // Never interactive: nobody could answer a polkit prompt.
        assert_eq!(calls.last().unwrap(), &format!("{what} interactive=false"));
    }
    assert_eq!(model.lock().unwrap().calls.len(), 3);

    // Params are closed.
    let e = call(&mut c, "power.reboot", json!({"force": true}))
        .await
        .unwrap_err();
    assert_eq!(e["code"], -32602);
    assert_eq!(model.lock().unwrap().calls.len(), 3);
}

#[tokio::test]
async fn logind_refusals_come_through() {
    let Some(bus) = Bus::start() else { return };
    let (_logind, model) = fake_logind(&bus).await;
    let d = Daemon::start_with(|_| runtime_bus(&bus));
    let mut c = d.client().await;

    model.lock().unwrap().refuse = Some(Refusal::Inhibited);
    let e = call(&mut c, "power.reboot", json!({})).await.unwrap_err();
    assert_eq!(e["code"], 4005, "{e}");
    assert_eq!(e["data"]["who"], "hinteros-system");
    assert_eq!(e["data"]["why"], "switching to a new system");
    assert!(
        e["message"]
            .as_str()
            .unwrap()
            .contains("Operation inhibited"),
        "logind's own text: {e}"
    );

    model.lock().unwrap().refuse = Some(Refusal::Denied);
    let e = call(&mut c, "power.suspend", json!({})).await.unwrap_err();
    assert_eq!(e["code"], 4004, "{e}");
    assert!(
        e["message"]
            .as_str()
            .unwrap()
            .contains("Interactive authentication")
    );

    // It still works once logind allows it again.
    model.lock().unwrap().refuse = None;
    call(&mut c, "power.poweroff", json!({})).await.unwrap();
}

#[tokio::test]
async fn without_logind_every_power_method_is_unavailable() {
    // A bus with nothing on it.
    let Some(bus) = Bus::start() else { return };
    let d = Daemon::start_with(|_| runtime_bus(&bus));
    let mut c = d.client().await;
    for method in [
        "power.capabilities",
        "power.suspend",
        "power.reboot",
        "power.poweroff",
    ] {
        let e = call(&mut c, method, json!({})).await.unwrap_err();
        assert_eq!(e["code"], 4001, "{method}: {e}");
        assert_eq!(e["data"]["service"], "logind");
    }
    // The rest of the API is unaffected.
    call(&mut c, "apps.list", json!({})).await.unwrap();
    // No bus at all (the default for tests): the same answer, no crash.
    let d = Daemon::start();
    let mut c = d.client().await;
    let e = call(&mut c, "power.reboot", json!({})).await.unwrap_err();
    assert_eq!(e["code"], 4001);
    assert_eq!(e["data"]["service"], "logind");
}

#[tokio::test]
async fn runtime_can_be_switched_off() {
    let Some(bus) = Bus::start() else { return };
    let (_logind, model) = fake_logind(&bus).await;
    let d = Daemon::start_with(|_| {
        let mut args = runtime_bus(&bus);
        args.push("--no-runtime".into());
        args
    });
    let mut c = d.client().await;
    let e = call(&mut c, "power.reboot", json!({})).await.unwrap_err();
    assert_eq!(e["code"], 4001);
    assert!(model.lock().unwrap().calls.is_empty());
}
