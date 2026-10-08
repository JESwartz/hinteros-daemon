//! Conformance with the system repo's `state.nix`: every state.json the system repo ships or
//! the installer writes must parse and round-trip through the typed `State` without losing a
//! field. Fixtures are copies from the system repo (`devices/*/state.json`,
//! `tests/vm/fixtures/*/state.json`) plus the installer's real output.

use std::path::PathBuf;

use hinteros_system::state::State;
use serde_json::Value;

fn fixture(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/state")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[test]
fn every_valid_fixture_round_trips() {
    for name in [
        "installer-real-box.json",
        "device-devbox.json",
        "vm-system-a.json",
        "vm-system-b.json",
    ] {
        let bytes = fixture(name);
        let original: Value = serde_json::from_slice(&bytes).unwrap();
        let state = State::parse(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        let out = state.to_canonical_json();
        // Nothing dropped, nothing added: same JSON value as the file.
        let written: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(written, original, "{name}: fields lost or invented");
        // Canonical form is a fixed point.
        let again = State::parse(&out).unwrap();
        assert_eq!(again, state, "{name}");
        assert_eq!(again.to_canonical_json(), out, "{name}: not stable");
    }
}

#[test]
fn installer_file_keeps_its_fields() {
    let s = State::parse(&fixture("installer-real-box.json")).unwrap();
    assert_eq!(s.state_version.as_deref(), Some("26.11"));
    assert_eq!(s.disk.as_ref().unwrap().layout, "gpt-esp-ext4");
    assert_eq!(s.settings.hostname.as_deref(), Some("hinteros"));
    assert_eq!(s.settings.admin.as_ref().unwrap().user, "james");
    assert!(s.settings.ssh.enable);
    assert_eq!(s.apps().len(), 5);
}

/// The VM test's config-sync snapshot is a marker file for the sync test, not a device state:
/// state.nix would refuse it, so must we.
#[test]
fn config_sync_marker_is_not_a_state() {
    let e = State::parse(&fixture("vm-config-sync.json")).unwrap_err();
    assert!(e.to_string().contains("marker"), "{e}");
    assert!(e.to_string().contains("out of sync"), "{e}");
}
