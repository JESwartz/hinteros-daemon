//! polkit's agent helper: the root side that checks a password with PAM and tells polkit.
//!
//! Protocol (polkit 127, `src/polkitagent/polkitagenthelper-pam.c` and
//! `polkitagentsession.c`): the agent starts the setuid `polkit-agent-helper-1 <user>` (or,
//! when `/run/polkit/agent-helper.socket` exists, connects to that socket-activated helper and
//! first writes `<user>\n`), then writes `<cookie>\n`. The helper runs PAM and writes one line
//! per conversation message, `g_strescape`d: `PAM_PROMPT_ECHO_OFF <text>` and
//! `PAM_PROMPT_ECHO_ON <text>` (answer with one line), `PAM_ERROR_MSG <text>`,
//! `PAM_TEXT_INFO <text>`, and finally `SUCCESS` (it has already called polkit's
//! `AuthenticationAgentResponse2` for the cookie) or `FAILURE`.
//!
//! The password is written once, to the first prompt, from a buffer that is wiped afterwards;
//! a second prompt (PAM asking again, or for something else) ends the attempt as a failure.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tracing::{debug, warn};
use zeroize::Zeroizing;

/// Longest line read from the helper.
const MAX_LINE: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum HelperError {
    #[error("cannot run polkit's agent helper {0}: {1}")]
    Start(String, std::io::Error),
    #[error("polkit's agent helper did not finish in time")]
    Timeout,
    #[error("polkit's agent helper: {0}")]
    Protocol(String),
}

/// Authenticate `user` for `cookie` with `password`. `Ok(true)`: the helper reported
/// `SUCCESS`; `Ok(false)`: `FAILURE` (wrong password, locked account, ...).
pub async fn authenticate(
    helper: &Path,
    socket: &Path,
    user: &str,
    cookie: &str,
    password: &str,
    limit: Duration,
) -> Result<bool, HelperError> {
    match tokio::time::timeout(limit, run(helper, socket, user, cookie, password)).await {
        Ok(outcome) => outcome,
        Err(_) => Err(HelperError::Timeout),
    }
}

async fn run(
    helper: &Path,
    socket: &Path,
    user: &str,
    cookie: &str,
    password: &str,
) -> Result<bool, HelperError> {
    if socket.exists() {
        match tokio::net::UnixStream::connect(socket).await {
            Ok(stream) => {
                debug!(socket = %socket.display(), "using polkit's helper socket");
                let (r, mut w) = stream.into_split();
                w.write_all(format!("{user}\n").as_bytes())
                    .await
                    .map_err(|e| HelperError::Protocol(e.to_string()))?;
                return converse(r, w, cookie, password).await;
            }
            Err(e) => warn!(
                socket = %socket.display(),
                "cannot connect to polkit's helper socket ({e}); trying the setuid helper"
            ),
        }
    }
    let mut child = tokio::process::Command::new(helper)
        .arg(user)
        .env_clear()
        .env("PATH", "/run/wrappers/bin:/run/current-system/sw/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| HelperError::Start(helper.display().to_string(), e))?;
    let stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    let outcome = converse(stdout, stdin, cookie, password).await;
    // The helper exits by itself after its verdict; don't leave it behind either way.
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    let _ = child.start_kill();
    let _ = child.wait().await;
    outcome
}

async fn converse(
    reader: impl AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin,
    cookie: &str,
    password: &str,
) -> Result<bool, HelperError> {
    let io = |e: std::io::Error| HelperError::Protocol(e.to_string());
    writer
        .write_all(format!("{cookie}\n").as_bytes())
        .await
        .map_err(io)?;
    writer.flush().await.map_err(io)?;
    let mut lines = BufReader::new(reader);
    let mut answered = false;
    loop {
        let mut raw = Vec::new();
        let n = read_line(&mut lines, &mut raw).await.map_err(io)?;
        if n == 0 {
            return Err(HelperError::Protocol("ended without a verdict".into()));
        }
        let line = unescape(String::from_utf8_lossy(&raw).trim_end_matches('\n'));
        if line.starts_with("PAM_PROMPT_ECHO_OFF ") || line.starts_with("PAM_PROMPT_ECHO_ON ") {
            if answered {
                debug!("the helper asked a second time; giving up on this attempt");
                return Ok(false);
            }
            let mut answer = Zeroizing::new(Vec::with_capacity(password.len() + 1));
            answer.extend_from_slice(password.as_bytes());
            answer.push(b'\n');
            writer.write_all(&answer).await.map_err(io)?;
            writer.flush().await.map_err(io)?;
            answered = true;
        } else if let Some(text) = line.strip_prefix("PAM_ERROR_MSG ") {
            debug!(message = text, "PAM error message");
        } else if let Some(text) = line.strip_prefix("PAM_TEXT_INFO ") {
            debug!(message = text, "PAM info");
        } else if line.starts_with("SUCCESS") {
            return Ok(true);
        } else if line.starts_with("FAILURE") {
            return Ok(false);
        } else {
            return Err(HelperError::Protocol(format!(
                "unexpected line {:?}",
                line.chars().take(80).collect::<String>()
            )));
        }
    }
}

/// One line, at most [`MAX_LINE`] bytes (longer lines are cut).
async fn read_line(
    r: &mut (impl AsyncBufReadExt + Unpin),
    out: &mut Vec<u8>,
) -> std::io::Result<usize> {
    let mut total = 0;
    loop {
        let buf = r.fill_buf().await?;
        if buf.is_empty() {
            return Ok(total);
        }
        let (take, done) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (buf.len(), false),
        };
        let room = MAX_LINE.saturating_sub(out.len());
        out.extend_from_slice(&buf[..take.min(room)]);
        r.consume(take);
        total += take;
        if done {
            return Ok(total);
        }
    }
}

/// GLib's `g_strcompress`: the inverse of the helper's `g_strescape`.
pub fn unescape(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' || i + 1 == b.len() {
            out.push(b[i]);
            i += 1;
            continue;
        }
        i += 1;
        match b[i] {
            b'0'..=b'7' => {
                let mut v: u32 = 0;
                let mut n = 0;
                while n < 3 && i < b.len() && (b'0'..=b'7').contains(&b[i]) {
                    v = v * 8 + u32::from(b[i] - b'0');
                    i += 1;
                    n += 1;
                }
                out.push(u8::try_from(v & 0xff).unwrap_or(b'?'));
                continue;
            }
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(11),
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_like_glib() {
        assert_eq!(unescape(r"Password: "), "Password: ");
        assert_eq!(unescape(r#"a\nb\t\"c\"\\"#), "a\nb\t\"c\"\\");
        assert_eq!(unescape(r"caf\303\251"), "café");
        assert_eq!(unescape(r"x\"), "x\\");
    }

    /// The socket-activated helper: user, then cookie, then the same conversation.
    #[tokio::test]
    async fn socket_helper_protocol() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("agent-helper.socket");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let mut verdicts = Vec::new();
            for _ in 0..2 {
                let (s, _) = listener.accept().await.unwrap();
                let (r, mut w) = s.into_split();
                let mut r = BufReader::new(r);
                let mut user = String::new();
                r.read_line(&mut user).await.unwrap();
                let mut cookie = String::new();
                r.read_line(&mut cookie).await.unwrap();
                w.write_all(b"PAM_TEXT_INFO hello\\tthere\nPAM_PROMPT_ECHO_OFF Password: \n")
                    .await
                    .unwrap();
                let mut pw = String::new();
                r.read_line(&mut pw).await.unwrap();
                let ok = pw == "s3cret\n";
                w.write_all(if ok { b"SUCCESS\n" } else { b"FAILURE\n" })
                    .await
                    .unwrap();
                verdicts.push((user, cookie, ok));
            }
            verdicts
        });
        let missing = d.path().join("no-helper");
        let t = Duration::from_secs(5);
        assert!(
            authenticate(&missing, &path, "admin", "1-a-1-b", "s3cret", t)
                .await
                .unwrap()
        );
        assert!(
            !authenticate(&missing, &path, "admin", "1-a-1-b", "nope", t)
                .await
                .unwrap()
        );
        let v = server.await.unwrap();
        assert_eq!(v[0], ("admin\n".into(), "1-a-1-b\n".into(), true));
        assert!(!v[1].2);
        // No socket and no helper: an error, not a verdict.
        assert!(matches!(
            authenticate(
                &missing,
                &d.path().join("nope.socket"),
                "admin",
                "c",
                "p",
                t
            )
            .await,
            Err(HelperError::Start(..))
        ));
    }
}
