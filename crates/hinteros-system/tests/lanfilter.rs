//! The local network filter (`lanfilter.rs`) against the real binary: the
//! caller's cgroup is checked, kept in `<run-dir>/lan-filter.json`, and the helper unit
//! (`hinteros-lan-filter.service`, the real `lan-filter-apply` with a fake `nft` that records
//! its input) is restarted; its failure is the call's; `lan_filter.clear`; no filter without
//! `--lan-filter`.

use std::os::unix::fs::PermissionsExt;

use serde_json::{Value, json};

mod common;

use common::*;

/// A root with a cgroup tree for the caller and a fake nft writing its input to `nft.in`
/// (`exit 1` when `nft.fail` exists).
fn lan_root() -> (Root, String) {
    let root = Root::create();
    let me = me();
    let rel = format!("user.slice/user-{me}.slice/user@{me}.service/app.slice/s.service/nolan");
    std::fs::create_dir_all(root.p("cgroup").join(&rel)).unwrap();
    std::fs::write(root.p("resolv.conf"), "nameserver 192.168.1.1\n").unwrap();
    let nft = root.p("bin/fake-nft");
    std::fs::create_dir_all(root.p("bin")).unwrap();
    std::fs::write(
        &nft,
        format!(
            "#!/bin/sh\ncat > {0}/nft.in\ntest ! -e {0}/nft.fail\n",
            root.path().display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&nft, std::fs::Permissions::from_mode(0o755)).unwrap();
    (root, format!("/{rel}"))
}

async fn start(root: Root, on: bool) -> Option<System> {
    let cgroup = root.p("cgroup").display().to_string();
    let extra: Vec<&str> = if on {
        vec!["--lan-filter", "--cgroup-root", &cgroup]
    } else {
        vec!["--cgroup-root", &cgroup]
    };
    let sys = System::start(root, &extra).await?;
    let r = |p: &str| sys.root.p(p).display().to_string();
    sys.fakes.lock().unwrap().lan_cmd = Some((
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_hinteros-system")),
        vec![
            "lan-filter-apply".into(),
            "--run-dir".into(),
            r("run"),
            "--cgroup-root".into(),
            r("cgroup"),
            "--resolv-conf".into(),
            r("resolv.conf"),
            "--proc-dir".into(),
            r("no-proc"),
            "--nft".into(),
            r("bin/fake-nft"),
        ],
    ));
    Some(sys)
}

fn state(sys: &System) -> Value {
    serde_json::from_str(&sys.root.read("run/lan-filter.json")).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn set_checks_the_cgroup_and_loads_the_table() {
    let (root, cgroup) = lan_root();
    let Some(sys) = start(root, true).await else {
        return;
    };
    let mut c = sys.client().await;
    let me = me();

    call(&mut c, "lan_filter.set", json!({ "cgroup": cgroup }))
        .await
        .unwrap();
    assert_eq!(
        state(&sys),
        json!({"version": 1, "cgroups": {me.to_string(): &cgroup[1..]}})
    );
    let rules = sys.root.read("nft.in");
    assert!(
        rules.contains(&format!(
            "socket cgroupv2 level 6 \"{}\" jump nolan",
            &cgroup[1..]
        )),
        "{rules}"
    );
    assert!(rules.contains("elements = { 192.168.1.1 }"), "{rules}");
    assert_eq!(
        sys.fakes.lock().unwrap().units,
        ["hinteros-lan-filter.service"]
    );

    // Not the caller's, not a cgroup, not there: refused before anything changes.
    for (bad, reason) in [
        (
            "/user.slice/user-0.slice/user@0.service/app.slice/s.service/nolan".to_string(),
            "not_owned",
        ),
        (format!("{cgroup}/../../x"), "bad_format"),
        (format!("{cgroup}/gone"), "not_found"),
        ("/system.slice/hinteros-system.service".into(), "bad_format"),
    ] {
        let e = call(&mut c, "lan_filter.set", json!({ "cgroup": bad }))
            .await
            .unwrap_err();
        assert_eq!(code(&e), -32602, "{bad}");
        assert_eq!(e["data"]["field"], "cgroup");
        assert_eq!(e["data"]["reason"], reason, "{bad}");
    }
    let e = call(
        &mut c,
        "lan_filter.set",
        json!({ "cgroup": cgroup, "uid": 0 }),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&e), -32602);
    assert_eq!(sys.fakes.lock().unwrap().units.len(), 1);

    // nft fails: so does the call; the state keeps the cgroup (the table is what it was).
    sys.root.touch("nft.fail");
    let e = call(&mut c, "lan_filter.set", json!({ "cgroup": cgroup }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), -32603, "{e}");
    std::fs::remove_file(sys.root.p("nft.fail")).unwrap();

    // Cleared: the table goes; clearing again changes nothing.
    call(&mut c, "lan_filter.clear", json!({})).await.unwrap();
    assert_eq!(state(&sys), json!({"version": 1, "cgroups": {}}));
    assert_eq!(
        sys.root.read("nft.in"),
        "table inet hinteros_lan\ndelete table inet hinteros_lan\n"
    );
    let n = sys.fakes.lock().unwrap().units.len();
    call(&mut c, "lan_filter.clear", json!({})).await.unwrap();
    assert_eq!(sys.fakes.lock().unwrap().units.len(), n, "nothing to clear");

    // Audited, without an action.
    let audit = sys.audit();
    let lan: Vec<_> = audit
        .iter()
        .filter(|m| {
            m.get("HINTEROS_METHOD")
                .is_some_and(|x| x.starts_with("lan_filter."))
        })
        .collect();
    assert!(
        lan.iter().any(|m| m["HINTEROS_DECISION"] == "allowed"),
        "{audit:?}"
    );
    assert!(
        lan.iter().any(|m| m["HINTEROS_DECISION"] == "invalid"),
        "{audit:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gone_cgroup_is_dropped_and_no_filter_without_the_flag() {
    let (root, cgroup) = lan_root();
    // Another user's entry whose cgroup is gone (a session that ended without clear).
    std::fs::write(
        root.p("run/lan-filter.json"),
        r#"{"version":1,"cgroups":{"4242":"user.slice/user-4242.slice/user@4242.service/x/nolan"}}"#,
    )
    .unwrap();
    let Some(sys) = start(root, true).await else {
        return;
    };
    let mut c = sys.client().await;
    call(&mut c, "lan_filter.set", json!({ "cgroup": cgroup }))
        .await
        .unwrap();
    let s = state(&sys);
    assert_eq!(s["cgroups"].as_object().unwrap().len(), 1, "{s}");
    assert!(!sys.root.read("nft.in").contains("4242"));
    drop(c);
    drop(sys);

    let (root, cgroup) = lan_root();
    let Some(sys) = start(root, false).await else {
        return;
    };
    let mut c = sys.client().await;
    let e = call(&mut c, "lan_filter.set", json!({ "cgroup": cgroup }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), -32603, "{e}");
    assert!(sys.fakes.lock().unwrap().units.is_empty());
}
