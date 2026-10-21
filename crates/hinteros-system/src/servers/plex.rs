//! Plex Media Server: its state from `Preferences.xml` and the claim (link to a Plex account).
//!
//! `Preferences.xml` is one `<Preferences .../>` element whose attributes are the settings.
//! `PlexOnlineToken` non-empty means the server is claimed; the token itself never leaves this
//! module (it is a Plex account credential). The claim posts the code from plex.tv/claim to the
//! server's own `POST /myplex/claim?token=<code>` on loopback (what Plex Web does on an
//! unclaimed server); the server exchanges it with plex.tv and writes the token.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use super::http::{self, Target};

/// What `services.list` and the claim need from `Preferences.xml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prefs {
    pub claimed: bool,
    pub friendly_name: Option<String>,
    pub machine_id: Option<String>,
}

/// A plex.tv/claim code: `claim-` and 20 characters of `[A-Za-z0-9_-]`. (Plex documents only
/// "claim-" plus a short random part and a lifetime of 4 minutes; the 20 characters are what
/// plex.tv hands out today, so a change there shows up as `invalid_params`, not as a bad claim.)
pub fn is_claim_code(s: &str) -> bool {
    s.strip_prefix("claim-").is_some_and(|rest| {
        rest.len() == 20
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|e| *e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let ch = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e => e
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| e.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The attributes of the `<Preferences>` element (`None`: not that file).
pub fn attributes(xml: &str) -> Option<BTreeMap<String, String>> {
    let start = xml.find("<Preferences")?;
    let tail = &xml[start + "<Preferences".len()..];
    let end = tail.find('>')?;
    let mut body = tail[..end].trim_end_matches('/');
    let mut out = BTreeMap::new();
    loop {
        body = body.trim_start();
        let Some(eq) = body.find('=') else { break };
        let name = body[..eq].trim().to_owned();
        let after = body[eq + 1..].trim_start();
        let quote = after.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let close = after[1..].find(quote)?;
        out.insert(name, unescape(&after[1..1 + close]));
        body = &after[close + 2..];
    }
    Some(out)
}

/// Read `Preferences.xml` (`None`: missing or unreadable: Plex has not started yet).
pub fn read(path: &Path) -> Option<Prefs> {
    let text = std::fs::read_to_string(path).ok()?;
    let a = attributes(&text)?;
    let clean = |k: &str| {
        a.get(k)
            .map(|v| {
                v.chars()
                    .filter(|c| !c.is_control())
                    .take(128)
                    .collect::<String>()
            })
            .filter(|v| !v.is_empty())
    };
    Some(Prefs {
        claimed: a
            .get("PlexOnlineToken")
            .is_some_and(|t| !t.trim().is_empty()),
        friendly_name: clean("FriendlyName"),
        machine_id: clean("ProcessedMachineIdentifier").or_else(|| clean("MachineIdentifier")),
    })
}

/// Whether the server answers (`GET /identity`, open without a token).
pub fn reachable(addr: SocketAddr, timeout: Duration) -> bool {
    http::request(Target::Tcp(addr), "GET", "/identity", &[], None, timeout).is_ok_and(|r| r.ok())
}

/// Post the claim code. `Err`: what went wrong, without the code.
pub fn claim(
    addr: SocketAddr,
    code: &str,
    client_id: &str,
    timeout: Duration,
) -> Result<(), String> {
    if !is_claim_code(code) {
        return Err("not a claim code".into());
    }
    let path = format!("/myplex/claim?token={}", http::encode(code));
    let headers = [
        ("X-Plex-Client-Identifier", client_id),
        ("X-Plex-Product", "HinterOS"),
        ("Accept", "application/json"),
    ];
    match http::request(Target::Tcp(addr), "POST", &path, &headers, None, timeout) {
        Ok(r) if r.ok() => Ok(()),
        Ok(r) => Err(format!("Plex refused the code (HTTP {})", r.status)),
        Err(e) => Err(format!("Plex does not answer: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const PREFS_UNCLAIMED: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<Preferences OldestPreviousVersion="1.41.0" MachineIdentifier="a1b2c3d4-e5f6" ProcessedMachineIdentifier="0123456789abcdef0123456789abcdef01234567" AnonymousMachineIdentifier="x" MetricsEpoch="1" AcceptedEULA="1" FriendlyName="Living room &amp; &quot;TV&quot;" PublishServerOnPlexOnlineKey="0" PlexOnlineToken=""/>
"#;

    #[test]
    fn preferences() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("Preferences.xml");
        std::fs::write(&p, PREFS_UNCLAIMED).unwrap();
        let prefs = read(&p).unwrap();
        assert!(!prefs.claimed);
        assert_eq!(prefs.friendly_name.as_deref(), Some("Living room & \"TV\""));
        assert_eq!(
            prefs.machine_id.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        std::fs::write(
            &p,
            PREFS_UNCLAIMED.replace(
                r#"PlexOnlineToken="""#,
                r#"PlexOnlineToken="secret-tok" PlexOnlineUsername='me&#64;x'"#,
            ),
        )
        .unwrap();
        let prefs = read(&p).unwrap();
        assert!(prefs.claimed);
        assert!(!format!("{prefs:?}").contains("secret-tok"));
        assert_eq!(
            attributes(r#"<Preferences a='1' b="&#x41;&bogus;"/>"#).unwrap()["b"],
            "A&bogus;"
        );
        assert_eq!(read(&d.path().join("none")), None);
        assert_eq!(attributes("<Other/>"), None);
        assert_eq!(attributes("<Preferences a=1/>"), None);
        // A fresh install: no name, not claimed.
        let a = attributes("<Preferences/>").unwrap();
        assert!(a.is_empty());
    }

    #[test]
    fn claim_codes() {
        assert!(is_claim_code("claim-AbCdEfGhIjKlMnOpQrSt"));
        assert!(is_claim_code("claim-a1_-a1_-a1_-a1_-a1_-"));
        for bad in [
            "",
            "claim-",
            "claim-AbCdEfGhIjKlMnOpQrS",
            "claim-AbCdEfGhIjKlMnOpQrStU",
            "Claim-AbCdEfGhIjKlMnOpQrSt",
            "claim-AbCdEfGhIjKlMnOpQr&t",
            "claim-AbCdEfGhIjKlMnOpQ t",
            "xclaim-AbCdEfGhIjKlMnOpQrS",
            "claim-ÄbCdEfGhIjKlMnOpQrS",
        ] {
            assert!(!is_claim_code(bad), "{bad:?}");
        }
    }
}
