//! The seat: Ctrl + Super + D from the keyboard reader to the active session's daemon
//! (`keyboard_home`), and the freezing of profiles whose session is not the active one
//! (`--freeze-inactive`). The real binary against the fake logind and systemd of
//! `common/mod.rs`.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use hinteros_api::client::Client;
use hinteros_api::common::Peer;
use hinteros_api::system;
use serde_json::json;
use tokio::time::timeout;

mod common;

use common::*;

/// A client subscribed to `keyboard_home`.
async fn home_subscriber(c: &mut Client) {
    let r = call(c, "subscribe", json!({"events": ["keyboard_home"]}))
        .await
        .unwrap();
    assert_eq!(r["events"], json!(["keyboard_home"]));
}

async fn no_event(c: &mut Client) {
    assert!(
        timeout(Duration::from_millis(500), c.next_event_raw())
            .await
            .is_err(),
        "an event came"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn keyboard_home_goes_to_the_seat_owner_only() {
    let Some(sys) = System::start(Root::create(), &[]).await else {
        return;
    };
    let socket = sys.root.p("run/keyboard.sock");
    let mode = std::fs::metadata(&socket).unwrap();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777,
        0o660
    );
    let mut owner = sys.client().await;
    home_subscriber(&mut owner).await;
    // The greeter's socket never gets it, even when subscribed.
    let mut greeter = Client::connect(&sys.root.p("run/greeter.sock"))
        .await
        .unwrap();
    greeter
        .hello(system::VERSION, Peer::new("greeter", "0"))
        .await
        .unwrap();
    home_subscriber(&mut greeter).await;

    let mut reader = UnixStream::connect(&socket).unwrap();
    reader.write_all(b"home\n").unwrap();
    let (name, params) = next_event(&mut owner).await;
    assert_eq!((name.as_str(), params), ("keyboard_home", json!({})));
    no_event(&mut greeter).await;

    // Someone else owns seat0 (another profile, the admin on a text console): not us.
    sys.fakes.lock().unwrap().seat_user = Some(me() + 1);
    reader.write_all(b"home\n").unwrap();
    no_event(&mut owner).await;
    // Nobody (a text console without a session).
    sys.fakes.lock().unwrap().seat_user = None;
    reader.write_all(b"home\n").unwrap();
    no_event(&mut owner).await;

    // Ours again; one event per line.
    sys.fakes.lock().unwrap().seat_user = Some(me());
    reader.write_all(b"home\nhome\n").unwrap();
    next_event(&mut owner).await;
    next_event(&mut owner).await;
    no_event(&mut owner).await;

    // Anything else closes the reader's connection.
    reader.write_all(b"type secret\n").unwrap();
    let mut buf = [0u8; 1];
    reader
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    assert_eq!(std::io::Read::read(&mut reader, &mut buf).unwrap(), 0);
    no_event(&mut owner).await;
    assert!(sys.log().contains("keyboard socket: unexpected input"));
}

fn slice_state(sys: &System, uid: u32) -> String {
    sys.fakes.lock().unwrap().slices[&format!("user-{uid}.slice")].clone()
}

/// logind's state of `uid` (`None`: unknown to logind) and, if given, its slice's `FreezerState`.
fn set_user(sys: &System, uid: u32, state: Option<&str>, slice: Option<&str>) {
    let mut f = sys.fakes.lock().unwrap();
    match state {
        Some(s) => f.user_states.insert(uid, s.into()),
        None => f.user_states.remove(&uid),
    };
    if let Some(s) = slice {
        f.slices.insert(format!("user-{uid}.slice"), s.into());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn inactive_profiles_are_frozen_and_thawed() {
    let Some(mut sys) = System::start(Root::create(), &["--freeze-inactive"]).await else {
        return;
    };
    let (p1, p2, guest) = (me(), 20002, 20000);
    // p1 in front; p2 logging in (online, never active yet, its user manager starting): a login
    // waits for both, so p2 is not frozen.
    set_user(&sys, p1, Some("active"), Some("running"));
    set_user(&sys, p2, Some("online"), Some("running"));
    set_user(&sys, guest, None, None);
    sys.fakes
        .lock()
        .unwrap()
        .slices
        .insert(format!("user@{p2}.service"), "running".into());
    sys.fakes
        .lock()
        .unwrap()
        .active_states
        .insert(format!("user@{p2}.service"), "activating".into());
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    assert_eq!(slice_state(&sys, p2), "running");
    // In front once, then another VT: frozen. (Its user manager still starting: not yet.)
    set_user(&sys, p2, Some("active"), None);
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    set_user(&sys, p2, Some("online"), None);
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert_eq!(slice_state(&sys, p2), "running");
    sys.fakes
        .lock()
        .unwrap()
        .active_states
        .insert(format!("user@{p2}.service"), "active".into());
    eventually("p2 frozen", || slice_state(&sys, p2) == "frozen").await;
    assert_eq!(slice_state(&sys, p1), "running");

    // Ctrl+Alt+F2: nobody's session is active. p1 is frozen too.
    set_user(&sys, p1, Some("online"), None);
    eventually("p1 frozen", || slice_state(&sys, p1) == "frozen").await;

    // Back to p1: thawed. p2 is ending (end-others): thawed so it can stop.
    set_user(&sys, p1, Some("active"), None);
    set_user(&sys, p2, Some("closing"), None);
    eventually("p1 and p2 thawed", || {
        slice_state(&sys, p1) == "running" && slice_state(&sys, p2) == "running"
    })
    .await;

    // p2 logs in again and is in front, then not.
    set_user(&sys, p2, Some("active"), None);
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    // Frozen by user.slice during suspend: made ours, so the resume leaves it frozen.
    set_user(&sys, p2, Some("online"), Some("frozen-by-parent"));
    eventually("p2 frozen by us", || slice_state(&sys, p2) == "frozen").await;
    // Never the guest while it has no session, never a slice that is not loaded.
    let calls = sys.fakes.lock().unwrap().freezer_calls.clone();
    assert!(
        !calls
            .iter()
            .any(|(_, u)| u == &format!("user-{guest}.slice")),
        "{calls:?}"
    );

    // The service stops: everyone thawed (a stopped service must not leave a slice frozen).
    set_user(&sys, guest, Some("online"), Some("frozen"));
    let pid = rustix::process::Pid::from_raw(sys.child.id() as i32).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
    let status = sys.child.wait().unwrap();
    assert!(status.success(), "{status:?}");
    for uid in [p1, p2, guest] {
        assert_eq!(slice_state(&sys, uid), "running", "uid {uid}");
    }
    let log = sys.log();
    assert!(
        log.contains("frozen: its session is not the active one"),
        "{log}"
    );
}
