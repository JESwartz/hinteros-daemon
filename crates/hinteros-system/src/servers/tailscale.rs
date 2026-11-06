//! Tailscale: the state from tailscaled's LocalAPI (`GET /localapi/v0/status` on its unix
//! socket; root has full access) and what the login job reads from `tailscale up --json`.

use std::path::Path;
use std::time::Duration;

use hinteros_api::system::{TailscaleDetail, TailscaleState};
use serde_json::Value;

use super::http::{self, Target};

/// The `Host` tailscaled's LocalAPI wants (any other, or a `Referer`/`Origin`, is refused).
pub const LOCALAPI_HOST: &str = "local-tailscaled.sock";

/// Longest sign-in URL passed on.
pub const MAX_AUTH_URL: usize = 512;

/// The status object (`ipnstate.Status`) without the peer list.
pub fn status(socket: &Path, timeout: Duration) -> Result<Value, String> {
    let r = http::request(
        Target::Unix(socket, LOCALAPI_HOST),
        "GET",
        "/localapi/v0/status?peers=false",
        &[],
        None,
        timeout,
    )
    .map_err(|e| format!("tailscaled does not answer: {e}"))?;
    if !r.ok() {
        return Err(format!("tailscaled answered HTTP {}", r.status));
    }
    serde_json::from_slice(&r.body).map_err(|e| format!("tailscaled status: {e}"))
}

pub fn backend_state(status: &Value) -> &str {
    status["BackendState"].as_str().unwrap_or("NoState")
}

/// tailscaled's `BackendState` as the API shows it.
pub fn state_of(backend: &str) -> TailscaleState {
    match backend {
        "Running" => TailscaleState::Running,
        "Starting" => TailscaleState::Starting,
        "Stopped" => TailscaleState::Stopped,
        "NeedsMachineAuth" => TailscaleState::NeedsApproval,
        // NoState, NeedsLogin, InUseOtherUser and anything newer: sign in.
        _ => TailscaleState::NeedsLogin,
    }
}

fn text(v: &Value, max: usize) -> Option<String> {
    v.as_str()
        .map(|s| {
            s.chars()
                .filter(|c| !c.is_control())
                .take(max)
                .collect::<String>()
        })
        .filter(|s| !s.is_empty())
}

/// `services.list`'s detail from a status object (`None`: tailscaled does not answer).
pub fn detail(status: Option<&Value>) -> TailscaleDetail {
    let Some(s) = status else {
        return TailscaleDetail {
            state: TailscaleState::Unavailable,
            name: None,
            ip4: None,
            dns_name: None,
            tailnet: None,
            key_expiry: None,
        };
    };
    let me = &s["Self"];
    let signed_in = !matches!(state_of(backend_state(s)), TailscaleState::NeedsLogin);
    let ip4 = me["TailscaleIPs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find_map(|a| a.parse::<std::net::Ipv4Addr>().ok())
        .map(|a| a.to_string());
    TailscaleDetail {
        state: state_of(backend_state(s)),
        name: signed_in.then(|| text(&me["HostName"], 64)).flatten(),
        ip4: signed_in.then_some(ip4).flatten(),
        dns_name: signed_in
            .then(|| text(&me["DNSName"], 253).map(|d| d.trim_end_matches('.').to_owned()))
            .flatten(),
        tailnet: signed_in
            .then(|| text(&s["CurrentTailnet"]["Name"], 128))
            .flatten(),
        key_expiry: signed_in
            .then(|| me["KeyExpiry"].as_str().and_then(rfc3339_seconds))
            .flatten(),
    }
}

/// `2027-04-18T10:00:00Z`, `2027-04-18T10:00:00.123456789-05:00` → Unix seconds.
pub fn rfc3339_seconds(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let t = s.get(r)?;
        t.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| t.parse().ok())?
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if b[13] != b':' || b[16] != b':' || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let mut rest = &s[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        let n = frac.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 {
            return None;
        }
        rest = &frac[n..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        o if o.len() == 6 && (o.starts_with('+') || o.starts_with('-')) && &o[3..4] == ":" => {
            let oh: i64 = o[1..3].parse().ok()?;
            let om: i64 = o[4..6].parse().ok()?;
            let v = oh * 3600 + om * 60;
            if o.starts_with('-') { -v } else { v }
        }
        _ => return None,
    };
    // Howard Hinnant's days_from_civil.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + se - offset)
}

/// A sign-in URL from `tailscale up --json`: `https://`, printable ASCII, at most
/// [`MAX_AUTH_URL`] bytes (it is shown as a QR code; nothing else is passed on).
pub fn auth_url(v: &Value) -> Option<String> {
    let u = v["AuthURL"].as_str()?;
    (u.starts_with("https://")
        && u.len() <= MAX_AUTH_URL
        && u.bytes().all(|b| b.is_ascii_graphic()))
    .then(|| u.to_owned())
}

/// The arguments of the login: a reset to the defaults (no exit node, no routes, no Tailscale
/// SSH; the owner's choice), JSON output, a deadline; `--force-reauth` when an old
/// key needs a new sign-in.
pub fn login_args(status: Option<&Value>, timeout: Duration) -> Vec<String> {
    let mut args = vec![
        "up".to_owned(),
        "--reset".to_owned(),
        "--json".to_owned(),
        format!("--timeout={}s", timeout.as_secs()),
    ];
    if let Some(s) = status
        && s["HaveNodeKey"].as_bool() == Some(true)
        && backend_state(s) == "NeedsLogin"
    {
        args.push("--force-reauth".to_owned());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub fn running() -> Value {
        json!({"Version":"1.104.1","TUN":true,"BackendState":"Running","HaveNodeKey":true,"AuthURL":"",
          "TailscaleIPs":["100.101.102.103","fd7a:115c:a1e0::1234:5678"],
          "Self":{"ID":"n1","PublicKey":"nodekey:ab12","HostName":"hinteros","DNSName":"hinteros.tail1234.ts.net.","OS":"linux","UserID":1,
                  "TailscaleIPs":["fd7a:115c:a1e0::1234:5678","100.101.102.103"],"Online":true,"KeyExpiry":"2027-04-18T10:00:00Z"},
          "Health":[],"MagicDNSSuffix":"tail1234.ts.net",
          "CurrentTailnet":{"Name":"owner@example.com","MagicDNSSuffix":"tail1234.ts.net","MagicDNSEnabled":true}})
    }

    #[test]
    fn details() {
        let d = detail(Some(&running()));
        assert_eq!(d.state, TailscaleState::Running);
        assert_eq!(d.name.as_deref(), Some("hinteros"));
        assert_eq!(d.ip4.as_deref(), Some("100.101.102.103"));
        assert_eq!(d.dns_name.as_deref(), Some("hinteros.tail1234.ts.net"));
        assert_eq!(d.tailnet.as_deref(), Some("owner@example.com"));
        assert_eq!(d.key_expiry, Some(1_808_042_400));
        let login = json!({"BackendState":"NeedsLogin","AuthURL":"","Self":{"HostName":"hinteros","TailscaleIPs":[]},"CurrentTailnet":null});
        let d = detail(Some(&login));
        assert_eq!(d.state, TailscaleState::NeedsLogin);
        assert_eq!((d.name, d.ip4, d.key_expiry), (None, None, None));
        // Tagged nodes have no key expiry.
        let mut tagged = running();
        tagged["Self"].as_object_mut().unwrap().remove("KeyExpiry");
        assert_eq!(detail(Some(&tagged)).key_expiry, None);
        assert_eq!(detail(None).state, TailscaleState::Unavailable);
        for (b, s) in [
            ("NeedsMachineAuth", TailscaleState::NeedsApproval),
            ("Stopped", TailscaleState::Stopped),
            ("Starting", TailscaleState::Starting),
            ("NoState", TailscaleState::NeedsLogin),
            ("InUseOtherUser", TailscaleState::NeedsLogin),
        ] {
            assert_eq!(state_of(b), s);
        }
    }

    #[test]
    fn times() {
        assert_eq!(rfc3339_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_seconds("2027-04-18T10:00:00Z"), Some(1_808_042_400));
        assert_eq!(
            rfc3339_seconds("2027-04-18T05:00:00.123456789-05:00"),
            Some(1_808_042_400)
        );
        for bad in [
            "",
            "2027-04-18",
            "2027-04-18T10:00:00",
            "2027-13-18T10:00:00Z",
            "2027-04-18T10:00:00.Z",
            "2027-04-18 10:00:00Z",
        ] {
            assert_eq!(rfc3339_seconds(bad), None, "{bad}");
        }
    }

    #[test]
    fn login() {
        assert_eq!(
            auth_url(&json!({"AuthURL":"https://login.tailscale.com/a/1a2b3c"})).as_deref(),
            Some("https://login.tailscale.com/a/1a2b3c")
        );
        for bad in ["http://x", "https://x y", "javascript:x", ""] {
            assert_eq!(auth_url(&json!({ "AuthURL": bad })), None);
        }
        assert_eq!(
            auth_url(&json!({"AuthURL": format!("https://{}", "a".repeat(600))})),
            None
        );
        let t = Duration::from_secs(600);
        assert_eq!(
            login_args(None, t),
            ["up", "--reset", "--json", "--timeout=600s"]
        );
        let expired = json!({"BackendState":"NeedsLogin","HaveNodeKey":true});
        assert_eq!(
            login_args(Some(&expired), t).last().unwrap(),
            "--force-reauth"
        );
        assert!(!login_args(Some(&running()), t).contains(&"--force-reauth".to_owned()));
    }
}
