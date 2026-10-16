//! The reaper's `Cgroup=` (apps without `lan`, `src/reaper.rs`) against the real binary: it
//! moves into the cgroup before it starts the command and says so on its stdin pipe; if it
//! cannot, it starts nothing.

use std::io::Read;
use std::process::{Command, Stdio};

fn reaper(cgroup: &std::path::Path, out: &std::path::Path) -> (std::process::Output, Vec<u8>) {
    let (mut read, write) = std::io::pipe().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hinteros-session"));
    cmd.args(["reaper", "SteamLaunch", "AppId=7"])
        .arg(format!("Cgroup={}", cgroup.display()))
        .args(["--", "sh", "-c"])
        // The command gets no stdin (not the daemon's pipe).
        .arg(format!(
            "echo ran > {0}; if read -r x; then echo stdin >> {0}; fi",
            out.display()
        ))
        .stdin(write)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn().unwrap();
    drop(cmd);
    let output = child.wait_with_output().unwrap();
    let mut said = Vec::new();
    read.read_to_end(&mut said).unwrap();
    (output, said)
}

#[test]
fn enters_the_cgroup_before_the_command_runs() {
    let t = tempfile::tempdir().unwrap();
    let out = t.path().join("out");
    let (output, said) = reaper(t.path(), &out);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(said, b"1");
    let pid: u32 = std::fs::read_to_string(t.path().join("cgroup.procs"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(pid > 0);
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "ran\n");
}

#[test]
fn starts_nothing_when_it_cannot_enter_the_cgroup() {
    let t = tempfile::tempdir().unwrap();
    let out = t.path().join("out");
    let (output, said) = reaper(&t.path().join("gone"), &out);
    assert_eq!(output.status.code(), Some(125), "{output:?}");
    assert!(said.is_empty(), "no ack: {said:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot enter cgroup"),
        "{output:?}"
    );
    assert!(!out.exists(), "the command ran");
}
