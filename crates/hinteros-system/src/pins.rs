//! Profile PINs (profiles.md "PINs and locking"): argon2id hashes in `pins/<id>` (PHC string,
//! m = 19 MiB, t = 2, p = 1, a fresh salt per set) and the backoff in `pins/<id>.state`
//! (`{failures, locked_until}`), shared by the service and the PAM helper under an `flock` on
//! the state file. The first 3 failures are free, then 1 s, 2 s, 4 s … up to 5 min; a success
//! resets it. A PIN never reaches a log or a file in plaintext.
//!
//! The admin profile's PIN has no wait (DECISIONS J4) but a limit per boot (owner: "After 10
//! failed attempts require that the box needs to be restarted before it can be attempted
//! again"): `pins/<id>.tries` (`{boot_id, failures}`, the kernel's boot id) counts wrong tries
//! from every path; after [`ADMIN_TRIES`] every check is refused, also the right PIN, until the
//! boot id changes. A service restart keeps the count; a right PIN before the limit, a new PIN
//! ([`Pins::set_hash`]) and `reset-pin` ([`Pins::clear`]) reset it.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::fsutil;

/// Free failures before the backoff starts.
pub const FREE_FAILURES: u32 = 3;
/// Longest wait.
pub const MAX_LOCK_S: i64 = 300;
/// Wrong admin-PIN tries a boot.
pub const ADMIN_TRIES: u32 = 10;
/// The kernel's id of this boot (a new one at every boot).
pub const BOOT_ID: &str = "/proc/sys/kernel/random/boot_id";

/// `^[0-9]{4,8}$`.
pub fn is_pin(s: &str) -> bool {
    (4..=8).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

fn argon() -> Argon2<'static> {
    Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(19 * 1024, 2, 1, None).expect("valid argon2 params"),
    )
}

/// The PHC string of `pin` with a fresh salt.
pub fn hash(pin: &str) -> std::io::Result<String> {
    let mut salt = [0u8; 16];
    rustix::rand::getrandom(&mut salt, rustix::rand::GetRandomFlags::empty())?;
    let salt = SaltString::encode_b64(&salt).map_err(std::io::Error::other)?;
    argon()
        .hash_password(pin.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(std::io::Error::other)
}

/// Whether `pin` matches the PHC string.
pub fn matches(phc: &str, pin: &str) -> bool {
    PasswordHash::new(phc)
        .ok()
        .is_some_and(|h| argon().verify_password(pin.as_bytes(), &h).is_ok())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Backoff {
    pub failures: u32,
    /// Unix seconds; 0 when not locked.
    pub locked_until: i64,
}

impl Backoff {
    /// After one more failure at `now`.
    pub fn fail(self, now: i64) -> Self {
        Self::after(self.failures.saturating_add(1), now)
    }

    /// After `failures` failures, the last at `now`.
    pub fn after(failures: u32, now: i64) -> Self {
        let locked_until = if failures > FREE_FAILURES {
            let n = failures - FREE_FAILURES - 1;
            now + (1i64 << n.min(16)).min(MAX_LOCK_S)
        } else {
            0
        };
        Self {
            failures,
            locked_until,
        }
    }

    /// Seconds left of the lock at `now` (0: not locked).
    pub fn remaining(&self, now: i64) -> i64 {
        (self.locked_until - now).max(0)
    }
}

/// The admin PIN's wrong tries in one boot (`pins/<id>.tries`). Another boot id: none yet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminTries {
    pub boot_id: String,
    pub failures: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verify {
    Ok,
    /// Wrong; the lock (if any) that follows, in seconds.
    Wrong {
        locked_for: i64,
    },
    /// Refused without checking: the backoff lock is on.
    Locked {
        seconds: i64,
    },
    /// The admin PIN: refused until the box restarts ([`ADMIN_TRIES`] wrong tries this boot).
    /// `started`: this try was wrong and was the last one.
    RestartRequired {
        started: bool,
    },
    /// No PIN is set.
    NoPin,
}

/// The PIN files of the box.
#[derive(Debug, Clone)]
pub struct Pins {
    dir: PathBuf,
    boot_id: PathBuf,
}

impl Pins {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            boot_id: BOOT_ID.into(),
        }
    }

    /// Tests: read the boot id from `path` instead of [`BOOT_ID`].
    pub fn with_boot_id_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.boot_id = path.into();
        self
    }

    /// This boot's id. Unreadable: a fixed one, so the count never starts over.
    fn boot_id(&self) -> String {
        fs::read_to_string(&self.boot_id)
            .map(|s| s.trim().to_owned())
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".into())
    }

    fn hash_path(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    fn state_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.state"))
    }

    fn tries_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.tries"))
    }

    fn ensure_dir(&self) -> std::io::Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
    }

    pub fn is_set(&self, id: &str) -> bool {
        self.hash_path(id).is_file()
    }

    /// Store a hash (from [`hash`]) and reset the backoff and the admin tries.
    pub fn set_hash(&self, id: &str, phc: &str) -> std::io::Result<()> {
        self.ensure_dir()?;
        fsutil::write_atomic(&self.hash_path(id), phc.as_bytes(), 0o600)?;
        self.with_state(id, |_| Backoff::default())?;
        self.reset_tries(id)
    }

    pub fn set(&self, id: &str, pin: &str) -> std::io::Result<()> {
        let phc = hash(pin)?;
        self.set_hash(id, &phc)
    }

    /// Remove `id`'s PIN and clear its backoff, under the same `flock` the checks take (so a
    /// check running now finishes first, and none sees a half-done reset). Whether a PIN was set.
    pub fn clear(&self, id: &str) -> std::io::Result<bool> {
        let mut removed: std::io::Result<bool> = Ok(false);
        self.with_state(id, |_| {
            removed = match fs::remove_file(self.hash_path(id)) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e),
            };
            Backoff::default()
        })?;
        self.reset_tries(id)?;
        removed
    }

    /// Any PIN file of `id` is there (a removed profile's files that stayed).
    pub fn any(&self, id: &str) -> bool {
        [self.hash_path(id), self.state_path(id), self.tries_path(id)]
            .iter()
            .any(|p| p.symlink_metadata().is_ok())
    }

    pub fn remove(&self, id: &str) {
        let _ = fs::remove_file(self.hash_path(id));
        let _ = fs::remove_file(self.state_path(id));
        let _ = fs::remove_file(self.tries_path(id));
    }

    /// The admin tries of `id` this boot (read under the lock).
    pub fn admin_tries(&self, id: &str) -> u32 {
        let boot = self.boot_id();
        let mut out = 0;
        let _ = self.with_json(&self.tries_path(id), |t: AdminTries| {
            if t.boot_id == boot {
                out = t.failures;
            }
            t
        });
        out
    }

    /// Forget `id`'s admin tries (a new PIN, `reset-pin`).
    fn reset_tries(&self, id: &str) -> std::io::Result<()> {
        if !self.tries_path(id).exists() {
            return Ok(());
        }
        self.with_json(&self.tries_path(id), |_| AdminTries::default())
    }

    /// The backoff of `id` (read under the lock).
    pub fn backoff(&self, id: &str) -> Backoff {
        let mut out = Backoff::default();
        let _ = self.with_state(id, |b| {
            out = b;
            b
        });
        out
    }

    /// The backoff of `id` for display (lists, the public view): read under a shared `flock`,
    /// never creating or writing the file.
    pub fn peek_backoff(&self, id: &str) -> Backoff {
        let Ok(mut file) = fs::File::open(self.state_path(id)) else {
            return Backoff::default();
        };
        if flock(&file, FlockOperation::LockShared).is_err() {
            return Backoff::default();
        }
        let mut text = String::new();
        if file.read_to_string(&mut text).is_err() {
            return Backoff::default();
        }
        fsutil::parse_cache(text.as_bytes()).unwrap_or_default()
    }

    /// Read-modify-write the backoff under an exclusive `flock`.
    fn with_state(&self, id: &str, f: impl FnOnce(Backoff) -> Backoff) -> std::io::Result<()> {
        self.with_json(&self.state_path(id), f)
    }

    /// Read-modify-write a JSON state file (0600) under an exclusive `flock` on it.
    fn with_json<T>(&self, path: &Path, f: impl FnOnce(T) -> T) -> std::io::Result<()>
    where
        T: Serialize + serde::de::DeserializeOwned + Default + PartialEq + Clone,
    {
        self.ensure_dir()?;
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        flock(&file, FlockOperation::LockExclusive)?;
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        // Counters: a file this daemon cannot read starts them again.
        let before: T = fsutil::parse_cache(text.as_bytes()).unwrap_or_default();
        let after = f(before.clone());
        if after != before || text.is_empty() {
            let bytes = fsutil::cache_bytes(&after)?;
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        Ok(())
    }

    /// Forget `id`'s failures and wait (the admin profile, which has no backoff).
    pub fn reset_backoff(&self, id: &str) -> std::io::Result<()> {
        if !self.state_path(id).exists() {
            return Ok(());
        }
        self.with_state(id, |_| Backoff::default())
    }

    /// Check `pin` for profile `id` at `now`, applying the backoff: a lock refuses at once; the
    /// try is counted under the lock before the (slow) hash check, so tries at the same time never
    /// pass a wait; the wait counts from when the check ended, and a right PIN resets it.
    pub fn verify(&self, id: &str, pin: &str, now: i64) -> std::io::Result<Verify> {
        self.verify_with(id, pin, now, true)
    }

    /// [`Pins::verify`]; with `backoff` false (the admin profile, DECISIONS J4) there is no wait
    /// (a stale backoff file never refuses) but the limit of [`ADMIN_TRIES`] wrong tries a boot.
    /// Each try is counted under the lock before the (slow) hash check, so tries that run at the
    /// same time never get past the limit; a right PIN then takes its count back to 0.
    pub fn verify_with(
        &self,
        id: &str,
        pin: &str,
        now: i64,
        backoff: bool,
    ) -> std::io::Result<Verify> {
        let pin = Zeroizing::new(pin.to_owned());
        let phc = match fs::read_to_string(self.hash_path(id)) {
            Ok(p) => Zeroizing::new(p),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Verify::NoPin),
            Err(e) => return Err(e),
        };
        if !backoff {
            if self.backoff(id) != Backoff::default() {
                self.reset_backoff(id)?;
            }
            let boot = self.boot_id();
            let mut taken = None;
            self.with_json(&self.tries_path(id), |t: AdminTries| {
                let failures = if t.boot_id == boot { t.failures } else { 0 };
                if failures >= ADMIN_TRIES {
                    return t;
                }
                taken = Some(failures + 1);
                AdminTries {
                    boot_id: boot.clone(),
                    failures: failures + 1,
                }
            })?;
            let Some(taken) = taken else {
                return Ok(Verify::RestartRequired { started: false });
            };
            if is_pin(&pin) && matches(phc.trim(), &pin) {
                self.with_json(&self.tries_path(id), |t: AdminTries| AdminTries {
                    failures: 0,
                    ..t
                })?;
                return Ok(Verify::Ok);
            }
            return Ok(if taken >= ADMIN_TRIES {
                Verify::RestartRequired { started: true }
            } else {
                Verify::Wrong { locked_for: 0 }
            });
        }
        // The lock is checked and the try counted as a failure in one step under the lock,
        // before the (slow) hash check: tries that run at the same time (the API and PAM) see
        // each other's, so none gets past a wait.
        let mut left = 0;
        self.with_state(id, |b| {
            left = b.remaining(now);
            if left > 0 { b } else { b.fail(now) }
        })?;
        if left > 0 {
            return Ok(Verify::Locked { seconds: left });
        }
        let started = std::time::Instant::now();
        let ok = is_pin(&pin) && matches(phc.trim(), &pin);
        let now = now + hash_seconds(started);
        let mut outcome = Verify::Ok;
        self.with_state(id, |b| {
            if ok {
                Backoff::default()
            } else {
                // Counted already (at least this try, if a right PIN reset it meanwhile); the
                // wait starts when the check ended.
                let after = Backoff::after(b.failures.max(1), now);
                let after = Backoff {
                    locked_until: after.locked_until.max(b.locked_until),
                    ..after
                };
                outcome = Verify::Wrong {
                    locked_for: after.remaining(now),
                };
                after
            }
        })?;
        Ok(outcome)
    }
}

/// Whole seconds the hash check took. Not in unit tests: they pass `now` themselves, and the
/// check takes seconds on a loaded machine.
fn hash_seconds(started: std::time::Instant) -> i64 {
    if cfg!(test) {
        0
    } else {
        started.elapsed().as_secs() as i64
    }
}

/// The PIN of a PAM conversation (pam_exec `expose_authtok`): stdin up to a NUL or newline.
pub fn read_authtok(mut r: impl Read) -> Zeroizing<String> {
    let mut buf = Zeroizing::new(Vec::with_capacity(64));
    let mut byte = [0u8; 1];
    while buf.len() < 512 {
        match r.read(&mut byte) {
            Ok(1) if byte[0] != 0 && byte[0] != b'\n' => buf.push(byte[0]),
            _ => break,
        }
    }
    Zeroizing::new(String::from_utf8_lossy(&buf).into_owned())
}

/// The pins directory of a state dir.
pub fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("pins")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validator() {
        for ok in ["0000", "2468", "12345678"] {
            assert!(is_pin(ok), "{ok}");
        }
        for bad in ["", "123", "123456789", "12a4", " 1234", "１２３４", "-123"] {
            assert!(!is_pin(bad), "{bad}");
        }
    }

    #[test]
    fn argon2id_round_trip() {
        let h = hash("2468").unwrap();
        assert!(h.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"), "{h}");
        assert!(matches(&h, "2468"));
        assert!(!matches(&h, "2469"));
        assert_ne!(hash("2468").unwrap(), h, "a fresh salt each time");
        assert!(!matches("garbage", "2468"));
    }

    #[test]
    fn backoff_doubles_up_to_five_minutes() {
        let mut b = Backoff::default();
        let mut locks = Vec::new();
        for _ in 0..14 {
            b = b.fail(1000);
            locks.push(b.remaining(1000));
        }
        assert_eq!(locks, [0, 0, 0, 1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300]);
    }

    #[test]
    fn verify_applies_and_resets_the_backoff_and_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let pins = Pins::new(d.path().join("pins"));
        assert_eq!(pins.verify("p1", "1234", 0).unwrap(), Verify::NoPin);
        pins.set("p1", "2468").unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&d.path().join("pins")), 0o700);
        assert_eq!(mode(&d.path().join("pins/p1")), 0o600);
        assert!(
            !fs::read_to_string(d.path().join("pins/p1"))
                .unwrap()
                .contains("2468")
        );
        for _ in 0..3 {
            assert_eq!(
                pins.verify("p1", "0000", 100).unwrap(),
                Verify::Wrong { locked_for: 0 }
            );
        }
        assert_eq!(
            pins.verify("p1", "0000", 100).unwrap(),
            Verify::Wrong { locked_for: 1 }
        );
        // Locked: even the right PIN is refused until the wait is over.
        assert_eq!(
            pins.verify("p1", "2468", 100).unwrap(),
            Verify::Locked { seconds: 1 }
        );
        // Another process (a new Pins on the same files) sees the same lock.
        let other = Pins::new(d.path().join("pins"));
        assert_eq!(other.backoff("p1").failures, 4);
        assert_eq!(other.peek_backoff("p1"), other.backoff("p1"));
        // Peeking never creates a state file.
        assert_eq!(other.peek_backoff("p7"), Backoff::default());
        assert!(!d.path().join("pins/p7.state").exists());
        assert_eq!(other.verify("p1", "2468", 101).unwrap(), Verify::Ok);
        assert_eq!(pins.backoff("p1"), Backoff::default());
        pins.remove("p1");
        assert!(!pins.is_set("p1"));
    }

    /// Pins on `dir` whose boot id is in `<dir>/../boot_id`.
    fn booted(dir: &Path, boot: &str) -> Pins {
        let file = dir.parent().unwrap().join("boot_id");
        fs::write(&file, format!("{boot}\n")).unwrap();
        Pins::new(dir).with_boot_id_file(file)
    }

    #[test]
    fn the_admin_pin_has_no_wait_but_ten_tries_a_boot() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("pins");
        let pins = booted(&dir, "boot-a");
        pins.set("p1", "2468").unwrap();
        // A stale lock from before the upgrade (or from the backoff path) never refuses.
        for _ in 0..6 {
            let _ = pins.verify("p1", "0000", 100);
        }
        assert!(pins.backoff("p1").remaining(100) > 0);
        // No wait: wrong is only wrong; a right PIN before the limit resets the count.
        for _ in 0..9 {
            assert_eq!(
                pins.verify_with("p1", "0000", 100, false).unwrap(),
                Verify::Wrong { locked_for: 0 }
            );
        }
        assert_eq!(
            pins.backoff("p1"),
            Backoff::default(),
            "the stale lock is gone"
        );
        assert_eq!(pins.admin_tries("p1"), 9);
        assert_eq!(
            pins.verify_with("p1", "2468", 100, false).unwrap(),
            Verify::Ok
        );
        assert_eq!(pins.admin_tries("p1"), 0);
        // Ten wrong tries: the tenth starts the lock; then even the right PIN is refused.
        for _ in 0..9 {
            assert_eq!(
                pins.verify_with("p1", "0000", 100, false).unwrap(),
                Verify::Wrong { locked_for: 0 }
            );
        }
        assert_eq!(
            pins.verify_with("p1", "0000", 100, false).unwrap(),
            Verify::RestartRequired { started: true }
        );
        for pin in ["2468", "0000", "x"] {
            assert_eq!(
                pins.verify_with("p1", pin, 100, false).unwrap(),
                Verify::RestartRequired { started: false }
            );
        }
        let mode = fs::metadata(dir.join("p1.tries"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        // Another process (a restarted service, the PAM helper) in the same boot: still locked.
        assert_eq!(
            booted(&dir, "boot-a")
                .verify_with("p1", "2468", 100, false)
                .unwrap(),
            Verify::RestartRequired { started: false }
        );
        // The backoff path of other profiles is not touched.
        assert_eq!(pins.admin_tries("p2"), 0);
        // A new boot: ten tries again.
        let next = booted(&dir, "boot-b");
        assert_eq!(next.admin_tries("p1"), 0);
        assert_eq!(
            next.verify_with("p1", "0000", 100, false).unwrap(),
            Verify::Wrong { locked_for: 0 }
        );
        assert_eq!(
            next.verify_with("p1", "2468", 100, false).unwrap(),
            Verify::Ok
        );
        assert_eq!(
            pins.verify_with("p9", "2468", 100, false).unwrap(),
            Verify::NoPin
        );
        pins.reset_backoff("p9").unwrap();
        assert!(!dir.join("p9.state").exists());
    }

    #[test]
    fn a_new_pin_or_reset_pin_ends_the_admin_lock() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("pins");
        let pins = booted(&dir, "boot-a");
        pins.set("p1", "2468").unwrap();
        for _ in 0..ADMIN_TRIES {
            let _ = pins.verify_with("p1", "0000", 0, false);
        }
        assert_eq!(pins.admin_tries("p1"), ADMIN_TRIES);
        // reset-pin (root): no PIN, no count.
        assert!(pins.clear("p1").unwrap());
        assert_eq!(pins.admin_tries("p1"), 0);
        pins.set("p1", "2468").unwrap();
        for _ in 0..ADMIN_TRIES {
            let _ = pins.verify_with("p1", "0000", 0, false);
        }
        // A new PIN (profiles.reset_pin with the admin password).
        pins.set("p1", "1357").unwrap();
        assert_eq!(
            pins.verify_with("p1", "1357", 0, false).unwrap(),
            Verify::Ok
        );
        pins.remove("p1");
        assert!(!dir.join("p1.tries").exists());
    }

    #[test]
    fn admin_tries_at_the_same_time_never_pass_the_limit() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("pins");
        booted(&dir, "boot-a").set("p1", "2468").unwrap();
        let file = d.path().join("boot_id");
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let (dir, file) = (dir.clone(), file.clone());
                std::thread::spawn(move || {
                    Pins::new(dir)
                        .with_boot_id_file(file)
                        .verify_with("p1", "0000", 0, false)
                        .unwrap()
                })
            })
            .collect();
        let outcomes: Vec<Verify> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        let checked = outcomes
            .iter()
            .filter(|o| !matches!(o, Verify::RestartRequired { started: false }))
            .count();
        assert_eq!(checked, ADMIN_TRIES as usize, "{outcomes:?}");
        let started = outcomes
            .iter()
            .filter(|o| **o == Verify::RestartRequired { started: true })
            .count();
        assert_eq!(started, 1, "{outcomes:?}");
    }

    #[test]
    fn backoff_across_processes_is_serialized() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("pins");
        Pins::new(&dir).set("p2", "1357").unwrap();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let dir = dir.clone();
                std::thread::spawn(move || Pins::new(dir).verify("p2", "0000", 50).unwrap())
            })
            .collect();
        let outcomes: Vec<Verify> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        // Each try is counted before its hash check: the free ones and the one that starts the
        // wait are checked, every other try at the same time sees the wait.
        let checked = outcomes
            .iter()
            .filter(|o| matches!(o, Verify::Wrong { .. }))
            .count();
        assert_eq!(checked, FREE_FAILURES as usize + 1, "{outcomes:?}");
        let b = Pins::new(&dir).backoff("p2");
        assert_eq!(b.failures, FREE_FAILURES + 1, "{b:?}");
        assert!(b.remaining(50) >= 1);
    }

    #[test]
    fn authtok_from_pam_exec() {
        assert_eq!(&*read_authtok(&b"2468\0junk"[..]), "2468");
        assert_eq!(&*read_authtok(&b"2468\n"[..]), "2468");
        assert_eq!(&*read_authtok(&b""[..]), "");
    }
}
