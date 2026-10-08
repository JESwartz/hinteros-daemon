//! `sudo hinteros-system reset-pin [<profile>]`: the way back in for a forgotten PIN (profiles.md;
//! owner: "the user should have to go into the terminal and use the root account to remove the PIN
//! and let the user set a new one"). Root only. Without an argument it lists the profiles; with a
//! profile id (`p2`) or its exact name it removes that profile's PIN and clears its wrong-PIN
//! backoff (under the backoff file's `flock`, like every check), records it in the audit log
//! (never a PIN), and republishes the picker's public view.
//!
//! No service notification is needed: hinteros-system reads `pins/` and `profiles.json` on every
//! request (`profiles.list`, every PIN check), and the PAM helper on every login. A profile
//! without a PIN is entered from the picker without one (`pin unlocked`, even when locked), and
//! its session's launcher shows the unskippable "Set your PIN" page (`pin_set` false), where
//! `profiles.set_pin` needs no current PIN.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use crate::audit::Audit;
use crate::config::Paths;
use crate::pins::Pins;
use crate::profiles::ProfilesFile;
use crate::public;
use crate::state::State;

/// One line of the listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub admin: bool,
    pub locked: bool,
    pub pin_set: bool,
}

/// What a reset did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    pub id: String,
    pub name: String,
    /// A PIN was set before (false: there was none; the backoff was still cleared).
    pub had_pin: bool,
}

fn load(paths: &Paths) -> Result<(State, ProfilesFile), String> {
    let (state, _) = State::load(&paths.state_json())
        .map_err(|e| format!("cannot read {}: {e}", paths.state_json().display()))?;
    if !state.is_schema2() {
        return Err("this box has no profiles or PINs yet (state.json schema 1)".into());
    }
    let file = ProfilesFile::load(&paths.profiles_json())
        .map_err(|e| format!("cannot read {}: {e}", paths.profiles_json().display()))?;
    Ok((state, file))
}

/// The profiles, in state.json order.
pub fn list(paths: &Paths) -> Result<Vec<Entry>, String> {
    let (state, file) = load(paths)?;
    let pins = Pins::new(paths.pins_dir());
    Ok(file
        .visible(&state)
        .into_iter()
        .map(|(a, m)| Entry {
            pin_set: pins.is_set(&a.id),
            id: a.id,
            name: m.name.clone(),
            admin: m.admin,
            locked: m.locked,
        })
        .collect())
}

/// Remove the PIN of the profile `which` (an id, or exactly its name).
pub fn reset(
    paths: &Paths,
    which: &str,
    audit: &Audit,
    sudo_user: Option<&str>,
    now: i64,
) -> Result<Done, String> {
    let (state, file) = load(paths)?;
    let visible = file.visible(&state);
    let by_id = visible.iter().find(|(a, _)| a.id == which);
    let found = match by_id {
        Some(f) => f,
        None => {
            let named: Vec<_> = visible.iter().filter(|(_, m)| m.name == which).collect();
            match named.as_slice() {
                [one] => *one,
                [] => return Err(format!("no profile with the id or name {which:?}")),
                _ => return Err(format!("several profiles are called {which:?}; use the id")),
            }
        }
    };
    let (account, meta) = found;
    let had_pin = Pins::new(paths.pins_dir())
        .clear(&account.id)
        .map_err(|e| format!("removing the PIN: {e}"))?;
    audit.pin_reset_by_root(&account.id, had_pin, sudo_user);
    public::publish(paths, &state, &file, now);
    Ok(Done {
        id: account.id.clone(),
        name: meta.name.clone(),
        had_pin,
    })
}

fn print_list(out: &mut impl Write, entries: &[Entry]) {
    let _ = writeln!(out, "Profiles on this box:");
    for e in entries {
        let mut notes = Vec::new();
        if e.admin {
            notes.push("admin");
        }
        if e.locked {
            notes.push("locked");
        }
        notes.push(if e.pin_set { "PIN set" } else { "no PIN" });
        let _ = writeln!(out, "  {:<5} {}  ({})", e.id, e.name, notes.join(", "));
    }
    let _ = writeln!(
        out,
        "\nTo remove a profile's PIN: sudo hinteros-system reset-pin <id or name>"
    );
}

pub fn main(paths: &Paths, profile: Option<&str>, journal_socket: &Path) -> ExitCode {
    let mut out = std::io::stdout().lock();
    if !rustix::process::geteuid().is_root() {
        eprintln!(
            "hinteros-system reset-pin: must be run as root (sudo hinteros-system reset-pin {})",
            profile.unwrap_or("<profile>")
        );
        return ExitCode::from(1);
    }
    let Some(which) = profile else {
        return match list(paths) {
            Ok(entries) => {
                print_list(&mut out, &entries);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("hinteros-system reset-pin: {e}");
                ExitCode::from(1)
            }
        };
    };
    let sudo_user = std::env::var("SUDO_USER").ok();
    let audit = Audit::new(journal_socket.to_owned());
    match reset(
        paths,
        which,
        &audit,
        sudo_user.as_deref(),
        crate::job::now(),
    ) {
        Ok(d) => {
            let _ = if d.had_pin {
                writeln!(out, "Removed the PIN of {} ({}).", d.name, d.id)
            } else {
                writeln!(
                    out,
                    "{} ({}) had no PIN; its wrong-PIN wait was cleared.",
                    d.name, d.id
                )
            };
            let _ = writeln!(
                out,
                "{} can now be opened from the profile picker without a PIN, and will be asked \
                 to set a new PIN when it starts.",
                d.name
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("hinteros-system reset-pin: {e}");
            if let Ok(entries) = list(paths) {
                print_list(&mut std::io::stderr(), &entries);
            }
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_text() {
        let mut out = Vec::new();
        print_list(
            &mut out,
            &[
                Entry {
                    id: "p1".into(),
                    name: "James".into(),
                    admin: true,
                    locked: true,
                    pin_set: true,
                },
                Entry {
                    id: "p2".into(),
                    name: "Sam".into(),
                    admin: false,
                    locked: false,
                    pin_set: false,
                },
            ],
        );
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("  p1    James  (admin, locked, PIN set)"), "{s}");
        assert!(s.contains("  p2    Sam  (no PIN)"), "{s}");
    }
}
