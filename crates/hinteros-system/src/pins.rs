//! Profile PINs (profiles.md "PINs and locking"): argon2id hashes in `pins/<id>` (PHC string,
//! m = 19 MiB, t = 2, p = 1, a fresh salt per set) and the backoff in `pins/<id>.state`
//! (`{failures, locked_until}`), shared by the service and the PAM helper under an `flock` on
//! the state file. The first 3 failures are free, then 1 s, 2 s, 4 s … up to 5 min; a success
//! resets it. A PIN never reaches a log or a file in plaintext.

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
        let failures = self.failures.saturating_add(1);
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
    /// No PIN is set.
    NoPin,
}

/// The PIN files of the box.
#[derive(Debug, Clone)]
pub struct Pins {
    dir: PathBuf,
}

impl Pins {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn hash_path(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    fn state_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.state"))
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

    /// Store a hash (from [`hash`]) and reset the backoff.
    pub fn set_hash(&self, id: &str, phc: &str) -> std::io::Result<()> {
        self.ensure_dir()?;
        fsutil::write_atomic(&self.hash_path(id), phc.as_bytes(), 0o600)?;
        self.with_state(id, |_| Backoff::default())?;
        Ok(())
    }

    pub fn set(&self, id: &str, pin: &str) -> std::io::Result<()> {
        let phc = hash(pin)?;
        self.set_hash(id, &phc)
    }

    pub fn remove(&self, id: &str) {
        let _ = fs::remove_file(self.hash_path(id));
        let _ = fs::remove_file(self.state_path(id));
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

    /// Read-modify-write the backoff under an exclusive `flock`.
    fn with_state(&self, id: &str, f: impl FnOnce(Backoff) -> Backoff) -> std::io::Result<()> {
        self.ensure_dir()?;
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.state_path(id))?;
        flock(&file, FlockOperation::LockExclusive)?;
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        let before: Backoff = serde_json::from_str(&text).unwrap_or_default();
        let after = f(before);
        if after != before || text.is_empty() {
            let bytes = serde_json::to_vec(&after)?;
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        Ok(())
    }

    /// Check `pin` for profile `id` at `now`, applying the backoff: a lock refuses at once; the
    /// outcome is recorded under the lock, counted from when the (slow) hash check ended.
    pub fn verify(&self, id: &str, pin: &str, now: i64) -> std::io::Result<Verify> {
        let pin = Zeroizing::new(pin.to_owned());
        let phc = match fs::read_to_string(self.hash_path(id)) {
            Ok(p) => Zeroizing::new(p),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Verify::NoPin),
            Err(e) => return Err(e),
        };
        let left = self.backoff(id).remaining(now);
        if left > 0 {
            return Ok(Verify::Locked { seconds: left });
        }
        let started = std::time::Instant::now();
        let ok = is_pin(&pin) && matches(phc.trim(), &pin);
        let now = now + started.elapsed().as_secs() as i64;
        let mut outcome = Verify::Ok;
        self.with_state(id, |b| {
            if ok {
                Backoff::default()
            } else {
                let after = b.fail(now);
                outcome = Verify::Wrong {
                    locked_for: after.remaining(now),
                };
                after
            }
        })?;
        Ok(outcome)
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
        assert_eq!(other.verify("p1", "2468", 101).unwrap(), Verify::Ok);
        assert_eq!(pins.backoff("p1"), Backoff::default());
        pins.remove("p1");
        assert!(!pins.is_set("p1"));
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
        for t in threads {
            t.join().unwrap();
        }
        // Every failure counted once, whatever the interleaving (some saw the lock).
        let b = Pins::new(&dir).backoff("p2");
        assert!((4..=8).contains(&b.failures), "{b:?}");
        assert!(b.remaining(50) >= 1);
    }

    #[test]
    fn authtok_from_pam_exec() {
        assert_eq!(&*read_authtok(&b"2468\0junk"[..]), "2468");
        assert_eq!(&*read_authtok(&b"2468\n"[..]), "2468");
        assert_eq!(&*read_authtok(&b""[..]), "");
    }
}
