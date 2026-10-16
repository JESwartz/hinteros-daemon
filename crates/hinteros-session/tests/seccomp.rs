//! The app sandbox's system call filter (`src/sandbox/seccomp.rs`) in the kernel: bwrap loads
//! it (`--seccomp <fd>`, as the daemon passes it) and runs this program again inside as a probe.
//! Compared with the same probe in bwrap without the filter: a user namespace (nested
//! sandboxes) still works; `AF_PACKET` in that namespace and `personality(ADDR_NO_RANDOMIZE)`
//! fail. The calls that need `unsafe` (keyctl, io_uring, TIOCSTI, i386) are checked by the
//! system repo's VM test `sandbox`; [`hinteros_session::sandbox::seccomp`]'s unit tests check
//! the program for all of them.
//!
//! Skipped when bwrap is not on PATH or cannot run here (no user namespaces).
//! No test harness: the probe must be single-threaded to make a user namespace.

use std::os::fd::AsRawFd;
use std::process::Command;

use hinteros_session::sandbox::seccomp;
use nix::sched::{CloneFlags, unshare};
use nix::sys::personality::{self, Persona};
use nix::sys::socket::{AddressFamily, SockFlag, SockType, socket};

const PROBE: &str = "HINTEROS_SECCOMP_PROBE";

fn result<T>(r: Result<T, nix::Error>) -> String {
    match r {
        Ok(_) => "ok".into(),
        Err(e) => format!("{e:?}"),
    }
}

/// Inside bwrap: one `<check>=<ok|errno>` line per check.
fn probe() {
    let userns = unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNET);
    println!("userns={}", result(userns));
    // CAP_NET_RAW in our own network namespace.
    println!(
        "packet={}",
        result(socket(
            AddressFamily::Packet,
            SockType::Raw,
            SockFlag::SOCK_CLOEXEC,
            None,
        ))
    );
    println!(
        "inet={}",
        result(socket(
            AddressFamily::Inet,
            SockType::Datagram,
            SockFlag::SOCK_CLOEXEC,
            None,
        ))
    );
    let current = personality::get();
    println!("personality_get={}", result(current));
    println!(
        "no_randomize={}",
        result(personality::set(Persona::ADDR_NO_RANDOMIZE))
    );
}

/// Run the probe in bwrap, with the filter when `filtered`; its lines as (check, result).
fn run(bwrap: &str, filtered: bool) -> Option<Vec<(String, String)>> {
    let exe = std::env::current_exe().unwrap();
    let mut cmd = Command::new(bwrap);
    cmd.args(["--dev-bind", "/", "/", "--die-with-parent"]);
    let fd = filtered.then(|| seccomp::pipe().unwrap());
    if let Some(fd) = &fd {
        cmd.args(["--seccomp", &fd.as_raw_fd().to_string()]);
    }
    cmd.arg("--").arg(exe).env(PROBE, "1");
    let out = cmd.output().unwrap();
    drop(fd);
    if !out.status.success() {
        eprintln!(
            "bwrap (filtered: {filtered}) failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        return None;
    }
    Some(
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
}

fn main() {
    if std::env::var_os(PROBE).is_some() {
        probe();
        return;
    }
    let bwrap = "bwrap";
    let Some(open) = run(bwrap, false) else {
        eprintln!("skipped: bwrap is missing or cannot run here");
        return;
    };
    let filtered = run(bwrap, true).expect("bwrap with the filter");
    let get = |r: &[(String, String)], k: &str| {
        r.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("no {k} in {r:?}"))
    };
    println!("without the filter: {open:?}\nwith the filter: {filtered:?}");
    // What works without the filter still works with it.
    for k in ["userns", "inet", "personality_get"] {
        assert_eq!(get(&filtered, k), get(&open, k), "{k}");
    }
    if get(&open, "userns") == "ok" {
        assert_eq!(get(&open, "packet"), "ok");
        assert_eq!(get(&filtered, "userns"), "ok", "nested sandboxes");
    }
    assert_eq!(get(&filtered, "packet"), "EAFNOSUPPORT");
    assert_eq!(get(&open, "no_randomize"), "ok");
    assert_eq!(get(&filtered, "no_randomize"), "EPERM");
    println!("seccomp: ok");
}
