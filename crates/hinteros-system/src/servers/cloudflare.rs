//! Cloudflare Tunnel: the tunnel token and whether cloudflared is connected.
//!
//! The token is what the Zero Trust dashboard shows for a remotely managed tunnel. cloudflared
//! (`cmd/cloudflared/tunnel/subcommands.go` `ParseToken`, `connection.TunnelToken`) reads it as
//! standard, padded base64 of a JSON object `{"a": <account tag>, "s": <secret, base64>,
//! "t": <tunnel UUID>, "e": <endpoint, optional>}`. It is checked strictly here before it is
//! written: exactly these keys (`e` optional; cloudflared knows only `fed`), each once, the account
//! tag 32 hex digits, the tunnel id a UUID, the secret base64 of 32 to 256 bytes, the whole at most
//! [`MAX_TUNNEL_TOKEN_BYTES`]. The token never reaches a log.

use std::net::SocketAddr;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hinteros_api::system::MAX_TUNNEL_TOKEN_BYTES;
use serde::Deserialize;

use super::http::{self, Target};

/// The unit that runs cloudflared (system repo; only while the token file exists).
pub const UNIT: &str = "hinteros-cloudflared.service";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Token {
    a: String,
    s: String,
    t: String,
    #[serde(default)]
    e: Option<String>,
}

fn is_hex(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(p, n)| is_hex(p, n))
}

/// Check a tunnel token. `Err`: the reason (`invalid_params` `reason`), never the token.
pub fn check_token(token: &str) -> Result<(), &'static str> {
    if token.is_empty() || token.len() > MAX_TUNNEL_TOKEN_BYTES {
        return Err("bad_length");
    }
    let json = STANDARD.decode(token).map_err(|_| "not_base64")?;
    // Duplicate keys are an error for a derived struct; unknown keys too.
    let t: Token = serde_json::from_slice(&json).map_err(|_| "not_a_token")?;
    if !is_hex(&t.a, 32) {
        return Err("bad_account");
    }
    if !is_uuid(&t.t) {
        return Err("bad_tunnel_id");
    }
    let secret = STANDARD.decode(&t.s).map_err(|_| "bad_secret")?;
    if !(32..=256).contains(&secret.len()) {
        return Err("bad_secret");
    }
    if let Some(e) = &t.e
        && (!(1..=16).contains(&e.len()) || !e.bytes().all(|b| b.is_ascii_lowercase()))
    {
        return Err("bad_endpoint");
    }
    Ok(())
}

/// cloudflared has a connection to Cloudflare (`GET /ready` on its metrics port is 200).
pub fn connected(metrics: SocketAddr, timeout: Duration) -> bool {
    http::request(Target::Tcp(metrics), "GET", "/ready", &[], None, timeout).is_ok_and(|r| r.ok())
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn token(json: &str) -> String {
        STANDARD.encode(json)
    }

    fn unpadded() -> String {
        let mut json = String::from(
            r#"{"a":"0123456789abcdef0123456789abcdef","t":"6f1c8a2e-3b4d-4e5f-8a9b-0c1d2e3f4a5b","s":"x"}"#,
        );
        while json.len() % 3 == 0 {
            json.push(' ');
        }
        let t = token(&json);
        assert!(t.ends_with('='));
        t.trim_end_matches('=').to_owned()
    }

    pub fn good() -> String {
        token(&format!(
            r#"{{"a":"0123456789abcdef0123456789abcdef","t":"6f1c8a2e-3b4d-4e5f-8a9b-0c1d2e3f4a5b","s":"{}"}}"#,
            STANDARD.encode([7u8; 32])
        ))
    }

    #[test]
    fn tokens() {
        assert_eq!(check_token(&good()), Ok(()));
        let s = STANDARD.encode([7u8; 32]);
        let fed = token(&format!(
            r#"{{"a":"0123456789abcdef0123456789abcdef","t":"6f1c8a2e-3b4d-4e5f-8a9b-0c1d2e3f4a5b","s":"{s}","e":"fed"}}"#
        ));
        assert_eq!(check_token(&fed), Ok(()));
        let cases = [
            (String::new(), "bad_length"),
            ("A".repeat(513), "bad_length"),
            ("not base64!".into(), "not_base64"),
            // Unpadded base64 is refused, as cloudflared refuses it.
            (unpadded(), "not_base64"),
            (token("[]"), "not_a_token"),
            (
                token(&format!(
                    r#"{{"a":"0123456789abcdef0123456789abcdef","t":"6f1c8a2e-3b4d-4e5f-8a9b-0c1d2e3f4a5b","s":"{s}","x":1}}"#
                )),
                "not_a_token",
            ),
            (
                token(&format!(
                    r#"{{"a":"0123456789abcdef0123456789abcdef","a":"0123456789abcdef0123456789abcdef","t":"6f1c8a2e-3b4d-4e5f-8a9b-0c1d2e3f4a5b","s":"{s}"}}"#
                )),
                "not_a_token",
            ),
            (
                token(&format!(
                    r#"{{"a":"0123456789abcdef","t":"6f1c8a2e-3b4d-4e5f-8a9b-0c1d2e3f4a5b","s":"{s}"}}"#
                )),
                "bad_account",
            ),
            (
                token(&format!(
                    r#"{{"a":"0123456789abcdef0123456789abcdef","t":"6f1c8a2e3b4d4e5f8a9b0c1d2e3f4a5b","s":"{s}"}}"#
                )),
                "bad_tunnel_id",
            ),
            (
                token(
                    r#"{"a":"0123456789abcdef0123456789abcdef","t":"6f1c8a2e-3b4d-4e5f-8a9b-0c1d2e3f4a5b","s":"c2hvcnQ="}"#,
                ),
                "bad_secret",
            ),
            (
                token(&format!(
                    r#"{{"a":"0123456789abcdef0123456789abcdef","t":"6f1c8a2e-3b4d-4e5f-8a9b-0c1d2e3f4a5b","s":"{s}","e":"FED;x"}}"#
                )),
                "bad_endpoint",
            ),
            (
                token(&format!(
                    r#"{{"a":"0123456789abcdef0123456789abcdef","t":"6f1c8a2e-3b4d-4e5f-8a9b-0c1d2e3f4a5b","s":"{s}","e":7}}"#
                )),
                "not_a_token",
            ),
        ];
        for (t, why) in cases {
            assert_eq!(check_token(&t), Err(why), "{t}");
        }
        // Whitespace or a newline (a sloppy paste) is not trimmed here: the launcher sends what
        // was typed, and the file gets exactly the checked text.
        assert_eq!(check_token(&format!("{}\n", good())), Err("not_base64"));
    }
}
