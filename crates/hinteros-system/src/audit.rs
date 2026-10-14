//! Structured audit log in the system journal (journald's native protocol), mirrored to the
//! tracing log. Every mutating request and every denial is recorded with `HINTEROS_*` fields;
//! denials are capped at 20 per minute and uid, then summarised.

use std::collections::HashMap;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::{info, warn};

/// Decisions recorded in `HINTEROS_DECISION`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    DeniedSeat,
    DeniedPolkit,
    DeniedPeer,
    AuthRequired,
    Invalid,
    RateLimited,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::DeniedSeat => "denied_seat",
            Self::DeniedPolkit => "denied_polkit",
            Self::DeniedPeer => "denied_peer",
            Self::AuthRequired => "auth_required",
            Self::Invalid => "invalid",
            Self::RateLimited => "rate_limited",
        }
    }

    fn is_denial(self) -> bool {
        self != Self::Allowed
    }
}

const DENIALS_PER_MINUTE: u32 = 20;

pub struct Audit {
    socket: PathBuf,
    denials: Mutex<HashMap<u32, (Instant, u32)>>,
}

impl Audit {
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            denials: Mutex::new(HashMap::new()),
        }
    }

    /// Log a request decision. `fields` are extra `KEY=value` pairs (`HINTEROS_` is added).
    #[allow(clippy::too_many_arguments)]
    pub fn request(
        &self,
        uid: u32,
        pid: Option<u32>,
        exe: &str,
        method: &str,
        action: Option<&str>,
        decision: Decision,
        job_id: Option<&str>,
        params: &str,
    ) {
        if decision.is_denial() {
            match self.denial_budget(uid) {
                Budget::Log => {}
                Budget::Suppress => return,
                Budget::Summary(n) => self.send(
                    &format!("hinteros-system: suppressed {n} denied requests of uid {uid}"),
                    &[("UID", uid.to_string()), ("DECISION", "suppressed".into())],
                ),
            }
        }
        let message = format!(
            "{method} by uid {uid} ({exe}): {}{}",
            decision.as_str(),
            job_id.map(|j| format!(", job {j}")).unwrap_or_default()
        );
        let mut fields = vec![
            ("UID", uid.to_string()),
            ("PID", pid.map(|p| p.to_string()).unwrap_or_default()),
            ("EXE", exe.to_owned()),
            ("METHOD", method.to_owned()),
            ("ACTION", action.unwrap_or("").to_owned()),
            ("DECISION", decision.as_str().to_owned()),
            ("PARAMS", params.chars().take(500).collect()),
        ];
        if let Some(j) = job_id {
            fields.push(("JOB_ID", j.to_owned()));
        }
        self.send(&message, &fields);
    }

    /// Log a finished job with the system before and after.
    pub fn job(&self, job_id: &str, kind: &str, uid: u32, outcome: &str, extra: &[(&str, String)]) {
        let message = format!("job {job_id} ({kind}, uid {uid}): {outcome}");
        let mut fields = vec![
            ("JOB_ID", job_id.to_owned()),
            ("JOB_KIND", kind.to_owned()),
            ("UID", uid.to_string()),
            ("OUTCOME", outcome.to_owned()),
        ];
        fields.extend(extra.iter().map(|(k, v)| (*k, v.clone())));
        self.send(&message, &fields);
    }

    fn denial_budget(&self, uid: u32) -> Budget {
        let mut map = self.denials.lock().unwrap();
        let now = Instant::now();
        let entry = map.entry(uid).or_insert((now, 0));
        if now.duration_since(entry.0) >= Duration::from_secs(60) {
            let suppressed = entry.1.saturating_sub(DENIALS_PER_MINUTE);
            *entry = (now, 1);
            return if suppressed > 0 {
                Budget::Summary(suppressed)
            } else {
                Budget::Log
            };
        }
        entry.1 += 1;
        if entry.1 <= DENIALS_PER_MINUTE {
            Budget::Log
        } else {
            Budget::Suppress
        }
    }

    /// A vendor terms acceptance (`apps.install` with `accepted_terms`): app, terms version, who
    /// (uid, user, profile) and the job that installs it.
    pub fn terms_accepted(
        &self,
        uid: u32,
        user: &str,
        profile: Option<&str>,
        id: &str,
        version: &str,
        job_id: &str,
    ) {
        let message = format!("terms of {id} version {version} accepted by {user} (uid {uid})");
        self.send(
            &message,
            &[
                ("UID", uid.to_string()),
                ("USER", user.to_owned()),
                ("PROFILE", profile.unwrap_or("").to_owned()),
                ("TERMS_ID", id.to_owned()),
                ("TERMS_VERSION", version.to_owned()),
                ("JOB_ID", job_id.to_owned()),
            ],
        );
    }

    /// Which PIN a method checked, and the outcome (never the PIN).
    pub fn pin_check(&self, uid: u32, method: &str, which: &str, outcome: &str) {
        self.send(
            &format!("{method} by uid {uid}: {which} PIN {outcome}"),
            &[
                ("UID", uid.to_string()),
                ("METHOD", method.to_owned()),
                ("PIN_CHECK", which.to_owned()),
                ("PIN_OUTCOME", outcome.to_owned()),
            ],
        );
    }

    /// The admin profile's PIN is refused until the box restarts: its last wrong try this boot,
    /// from `method` (a system.sock method, or `greeter` for the PAM helper) by `uid` (when
    /// known). Never a PIN.
    pub fn admin_pin_lockout(&self, id: &str, method: &str, uid: Option<u32>) {
        self.send(
            &format!(
                "{method}{}: {} wrong admin PINs; the admin PIN is refused until the box restarts",
                uid.map(|u| format!(" by uid {u}")).unwrap_or_default(),
                crate::pins::ADMIN_TRIES
            ),
            &[
                ("UID", uid.map(|u| u.to_string()).unwrap_or_default()),
                ("METHOD", method.to_owned()),
                ("PROFILE", id.to_owned()),
                ("PIN_CHECK", "admin".into()),
                ("PIN_OUTCOME", "lockout_started".into()),
            ],
        );
    }

    /// `hinteros-system reset-pin` (root, from a terminal): which profile's PIN was removed.
    pub fn pin_reset_by_root(&self, id: &str, had_pin: bool, sudo_user: Option<&str>) {
        self.send(
            &format!(
                "reset-pin by root{}: the PIN of profile {id} {}",
                sudo_user
                    .map(|u| format!(" (sudo by {u})"))
                    .unwrap_or_default(),
                if had_pin {
                    "was removed"
                } else {
                    "was not set"
                }
            ),
            &[
                ("UID", "0".into()),
                ("METHOD", "reset-pin".into()),
                ("PROFILE", id.to_owned()),
                ("SUDO_USER", sudo_user.unwrap_or("").to_owned()),
                (
                    "PIN_OUTCOME",
                    if had_pin { "removed" } else { "not_set" }.into(),
                ),
            ],
        );
    }

    fn send(&self, message: &str, fields: &[(&str, String)]) {
        info!(target: "audit", "{message}");
        let mut datagram = Vec::new();
        field(&mut datagram, "MESSAGE", message);
        field(&mut datagram, "PRIORITY", "5");
        field(&mut datagram, "SYSLOG_IDENTIFIER", "hinteros-system");
        field(&mut datagram, "HINTEROS_AUDIT", "1");
        for (k, v) in fields {
            field(&mut datagram, &format!("HINTEROS_{k}"), v);
        }
        if !self.socket.exists() {
            return;
        }
        if let Err(e) = send_datagram(&self.socket, &datagram) {
            warn!("audit: cannot write to the journal: {e}");
        }
    }
}

/// A plain journal entry (not an audit record) from a helper whose stderr goes nowhere (the PAM
/// helper under pam_exec): `MESSAGE`, `SYSLOG_IDENTIFIER` and `HINTEROS_<key>` fields. Best effort.
pub fn journal_note(
    socket: &std::path::Path,
    identifier: &str,
    message: &str,
    fields: &[(&str, &str)],
) {
    let mut datagram = Vec::new();
    field(&mut datagram, "MESSAGE", message);
    field(&mut datagram, "PRIORITY", "6");
    field(&mut datagram, "SYSLOG_IDENTIFIER", identifier);
    for (k, v) in fields {
        field(&mut datagram, &format!("HINTEROS_{k}"), v);
    }
    if socket.exists() {
        let _ = send_datagram(socket, &datagram);
    }
}

/// Send without ever blocking the service for long: a full journal queue gets a few short retries,
/// then the entry is reported as lost (it is still in the service's own log above).
fn send_datagram(socket: &std::path::Path, datagram: &[u8]) -> std::io::Result<()> {
    let s = UnixDatagram::unbound()?;
    s.set_nonblocking(true)?;
    let mut tries = 0;
    loop {
        match s.send_to(datagram, socket) {
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && tries < 20 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return Err(e),
        }
    }
}

enum Budget {
    Log,
    Suppress,
    Summary(u32),
}

/// Append one field in journald's native format (the binary form if the value has a newline).
fn field(out: &mut Vec<u8>, key: &str, value: &str) {
    out.extend_from_slice(key.as_bytes());
    if value.contains('\n') {
        out.push(b'\n');
        out.extend_from_slice(&(value.len() as u64).to_le_bytes());
        out.extend_from_slice(value.as_bytes());
    } else {
        out.push(b'=');
        out.extend_from_slice(value.as_bytes());
    }
    out.push(b'\n');
}

/// Parse a native journal datagram (tests and the integration tests' fake journal).
pub fn parse_datagram(mut data: &[u8]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    while !data.is_empty() {
        let Some(nl) = data.iter().position(|&b| b == b'\n') else {
            break;
        };
        let line = &data[..nl];
        if let Some(eq) = line.iter().position(|&b| b == b'=') {
            out.insert(
                String::from_utf8_lossy(&line[..eq]).into_owned(),
                String::from_utf8_lossy(&line[eq + 1..]).into_owned(),
            );
            data = &data[nl + 1..];
        } else {
            let key = String::from_utf8_lossy(line).into_owned();
            let rest = &data[nl + 1..];
            let Some((len, rest)) = rest.split_first_chunk::<8>() else {
                break;
            };
            let len = (u64::from_le_bytes(*len) as usize).min(rest.len());
            out.insert(key, String::from_utf8_lossy(&rest[..len]).into_owned());
            data = rest.get(len + 1..).unwrap_or_default();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_format_round_trips() {
        let mut d = Vec::new();
        field(&mut d, "MESSAGE", "hi");
        field(&mut d, "HINTEROS_PARAMS", "a\nb");
        field(&mut d, "X", "");
        let m = parse_datagram(&d);
        assert_eq!(m["MESSAGE"], "hi");
        assert_eq!(m["HINTEROS_PARAMS"], "a\nb");
        assert_eq!(m["X"], "");
    }

    #[test]
    fn writes_to_the_socket_and_caps_denials() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal");
        let sock = UnixDatagram::bind(&path).unwrap();
        sock.set_nonblocking(true).unwrap();
        let audit = Audit::new(path);
        audit.request(
            1000,
            Some(5),
            "/bin/x",
            "rebuild",
            Some("org.hinteros.system.rebuild"),
            Decision::Allowed,
            Some("0123456789abcdef"),
            "{}",
        );
        let mut buf = vec![0u8; 65536];
        let n = sock.recv(&mut buf).unwrap();
        let m = parse_datagram(&buf[..n]);
        assert_eq!(m["HINTEROS_AUDIT"], "1");
        assert_eq!(m["HINTEROS_UID"], "1000");
        assert_eq!(m["HINTEROS_DECISION"], "allowed");
        assert_eq!(m["HINTEROS_JOB_ID"], "0123456789abcdef");
        // Read as we go: the datagram queue may be as short as 10 (build sandboxes).
        let mut count = 0;
        for _ in 0..30 {
            audit.request(
                1000,
                None,
                "?",
                "rebuild",
                None,
                Decision::DeniedSeat,
                None,
                "",
            );
            while sock.recv(&mut buf).is_ok() {
                count += 1;
            }
        }
        // A full queue never blocks the caller.
        let start = std::time::Instant::now();
        for _ in 0..30 {
            audit.request(
                1001,
                None,
                "?",
                "rebuild",
                None,
                Decision::Allowed,
                None,
                "",
            );
        }
        assert!(start.elapsed() < Duration::from_secs(10));
        assert_eq!(count, DENIALS_PER_MINUTE);
    }
}
