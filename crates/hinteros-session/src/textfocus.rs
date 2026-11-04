//! Showing the on-screen keyboard when a text field gets focus in a Firefox kiosk app.
//!
//! The chain (hinteros-docs/spikes/05-on-screen-keyboard.md, "auto-summon"):
//!
//! 1. A WebExtension in the kiosk Firefox (`firefox-extension/` in this repo) watches
//!    `focusin`/`focusout` and tells its background script whether a text field, a password
//!    field or nothing editable has focus.
//! 2. The background script talks to the native-messaging host [`HOST_NAME`] over
//!    `runtime.connectNative`: Firefox starts `hinteros-osk-native-host` (this binary under a
//!    second name, or `hinteros-session osk-native-host`) and exchanges length-prefixed JSON
//!    with it on stdin/stdout ([`read_frame`], [`write_frame`], [`run_host`]).
//! 3. The host forwards each `{"focus": "text"|"password"|"none"}` as one JSON line to the
//!    session daemon's internal socket `osk.sock` (next to `session.sock`; `$`[`SOCKET_ENV`],
//!    which the daemon sets for the apps it starts). That socket is not part of the session API:
//!    it takes nothing but these lines, from processes of the session user, and never answers.
//! 4. The daemon ([`serve`]) passes them to the shell as [`crate::shell::Msg::TextFocus`] with
//!    the host's pid. The shell shows the keyboard when the focused app has the keyboard input
//!    profile and the host runs inside that app's process tree, and hides a keyboard it showed
//!    this way shortly after the field loses focus (a short delay, so moving from one field to
//!    the next does not flicker). A closed connection counts as `none`.
//!
//! Content scripts cannot see Firefox's own toolbar fields (the address bar, find in page),
//! so Firefox's autoConfig (privileged chrome code, hinteros `nixos/modules/firefox-kiosk.nix`)
//! reports those itself: it connects to the same socket straight from Firefox's main process
//! and writes the same lines (`client` `hinteros-firefox-chrome`). Every connection is its own
//! reporter with its own state in the shell; a field has focus while any reporter says so.
//!
//! Password fields show the keyboard too: the keyboard masks its own preview.
//!
//! Showing on a click: focus alone shows the keyboard only with `[osk] auto_show` (arrow-key
//! navigation and pages focusing a field on load must not pop it up); a click into a field
//! with the pointer does. Reporters add `"clicked": true` to a report sent because of a
//! trusted primary-button click that landed on the focused field: `{"focus": "text",
//! "clicked": true}`. Such a report is sent even if the focus kind did not change (clicking
//! into the field that already has focus). Older daemons ignore the field; reports without
//! it mean `false`. The shell also counts focus that arrives right after its own left click
//! (RT) as clicked ([`crate::shell::OSK_CLICK_WINDOW`]).

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::shell::Msg;

/// The client name the host uses in its messages (and in the daemon's logs).
pub const FIREFOX_CLIENT_NAME: &str = "hinteros-firefox";
/// The internal socket, relative to `$XDG_RUNTIME_DIR` (next to the session socket).
pub const SOCKET_IN_RUNTIME_DIR: &str = "hinteros/osk.sock";
/// Its file name.
pub const SOCKET_NAME: &str = "osk.sock";
/// Where the host finds the socket (set by the daemon for the apps it starts).
pub const SOCKET_ENV: &str = "HINTEROS_OSK_SOCKET";
/// `hinteros-session osk-native-host`.
pub const HOST_SUBCOMMAND: &str = "osk-native-host";
/// The binary name Firefox starts (a symlink to `hinteros-session`).
pub const HOST_BIN: &str = "hinteros-osk-native-host";
/// The native-messaging host name (`runtime.connectNative`, the host manifest's `name`).
pub const HOST_NAME: &str = "hinteros_osk";

/// Largest message the host accepts from Firefox (they are a few bytes).
pub const MAX_FRAME: usize = 64 * 1024;

/// What has keyboard focus in the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldFocus {
    /// A text field (input, textarea, contenteditable).
    Text,
    /// A password field.
    Password,
    /// Nothing editable.
    None,
}

impl FieldFocus {
    pub fn editable(self) -> bool {
        self != Self::None
    }
}

/// One message from the extension, and one line on the internal socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusMessage {
    pub focus: FieldFocus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// Sent because the user clicked into the field (see the module docs).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clicked: bool,
}

impl FocusMessage {
    /// Parse a message from the extension or a line from the socket. Unknown fields are
    /// ignored (the extension may grow), an unknown `focus` value is an error.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(bytes).map_err(|e| format!("bad message: {e}"))
    }
}

/// Read one native-messaging frame: a 32-bit length in native byte order, then that many
/// bytes of UTF-8 JSON. `Ok(None)` at a clean end of input (Firefox closed the port).
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < len.len() {
        match r.read(&mut len[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "input ended inside a message length",
                ));
            }
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let len = u32::from_ne_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message of {len} bytes (at most {MAX_FRAME})"),
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

/// Write one native-messaging frame.
pub fn write_frame(w: &mut impl Write, msg: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(msg)?;
    let len = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "message too long"))?;
    w.write_all(&len.to_ne_bytes())?;
    w.write_all(&bytes)?;
    w.flush()
}

/// Where the internal socket is: `$HINTEROS_OSK_SOCKET`, else `$XDG_RUNTIME_DIR/hinteros/osk.sock`.
pub fn socket_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(SOCKET_ENV).filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|p| !p.is_empty())
        .map(|d| PathBuf::from(d).join(SOCKET_IN_RUNTIME_DIR))
}

/// The internal socket next to the session socket.
pub fn socket_beside(session_socket: &Path) -> PathBuf {
    session_socket.with_file_name(SOCKET_NAME)
}

/// The host's work: frames from `input` (Firefox) go to the daemon as lines on the stream
/// `connect` opens (again after a failure); each gets a frame on `output` saying whether it
/// was passed on (`{"ok": true}`, or `{"ok": false, "error": ...}`). Returns at the end of
/// `input`; an unreadable frame ends it with an error (the stream is out of step).
pub fn host_loop(
    input: &mut impl Read,
    output: &mut impl Write,
    mut connect: impl FnMut() -> io::Result<UnixStream>,
) -> io::Result<()> {
    let mut daemon: Option<UnixStream> = None;
    while let Some(frame) = read_frame(input)? {
        let reply = match FocusMessage::parse(&frame) {
            Err(e) => json!({"ok": false, "error": e}),
            Ok(mut msg) => {
                msg.client.get_or_insert_with(|| FIREFOX_CLIENT_NAME.into());
                let mut line = serde_json::to_vec(&msg)?;
                line.push(b'\n');
                match forward(&mut daemon, &mut connect, &line) {
                    Ok(()) => json!({"ok": true}),
                    Err(e) => json!({"ok": false, "error": format!("session daemon: {e}")}),
                }
            }
        };
        write_frame(output, &reply)?;
    }
    Ok(())
}

/// Write `line` to the daemon, connecting (once more after a broken connection) as needed.
fn forward(
    daemon: &mut Option<UnixStream>,
    connect: &mut impl FnMut() -> io::Result<UnixStream>,
    line: &[u8],
) -> io::Result<()> {
    for attempt in 0..2 {
        if daemon.is_none() {
            *daemon = Some(connect()?);
        }
        let stream = daemon.as_mut().expect("connected above");
        match stream.write_all(line) {
            Ok(()) => return Ok(()),
            Err(e) if attempt == 0 => {
                debug!("the session daemon connection broke ({e}); reconnecting");
                *daemon = None;
            }
            Err(e) => {
                *daemon = None;
                return Err(e);
            }
        }
    }
    unreachable!("the second attempt returns")
}

/// `hinteros-session osk-native-host [<manifest path> <extension id>]`, started by Firefox.
/// Talks native messaging on stdin/stdout; logs go to stderr (Firefox's browser console).
pub fn run_host(args: &[String]) -> i32 {
    let Some(path) = socket_path() else {
        eprintln!("{HOST_BIN}: neither {SOCKET_ENV} nor XDG_RUNTIME_DIR is set");
        return 1;
    };
    if let Some(extension) = args.get(1) {
        eprintln!(
            "{HOST_BIN}: serving {extension}, forwarding to {}",
            path.display()
        );
    }
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    match host_loop(&mut stdin, &mut stdout, || UnixStream::connect(&path)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{HOST_BIN}: {e}");
            1
        }
    }
}

/// Serve the internal socket: every connection's lines become [`Msg::TextFocus`] with the
/// peer's pid. Only the session user may connect.
pub async fn serve(listener: UnixListener, shell: mpsc::UnboundedSender<Msg>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let shell = shell.clone();
                tokio::spawn(async move { connection(stream, shell).await });
            }
            Err(e) => {
                warn!("osk.sock: accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// Lines a reporter may send per second before the daemon reads its connection more slowly
/// (nothing is dropped; the reporter waits). Focus changes come at the pace of a person.
const LINES_PER_SECOND: u32 = 50;

async fn connection(stream: tokio::net::UnixStream, shell: mpsc::UnboundedSender<Msg>) {
    let Ok(cred) = stream.peer_cred() else { return };
    if cred.uid() != rustix::process::geteuid().as_raw() {
        warn!(
            uid = cred.uid(),
            "osk.sock: rejecting a connection from another user"
        );
        return;
    }
    let pid = cred.pid().and_then(|p| u32::try_from(p).ok());
    // Each connection keeps its own focus state in the shell.
    static NEXT_REPORTER: AtomicU64 = AtomicU64::new(1);
    let reporter = NEXT_REPORTER.fetch_add(1, Ordering::Relaxed);
    debug!(
        ?pid,
        reporter, "osk.sock: text field focus reporter connected"
    );
    let mut reader = hinteros_api::codec::LineReader::with_limit(stream, MAX_FRAME);
    let mut window = (tokio::time::Instant::now(), 0u32);
    let mut bad = 0u32;
    loop {
        // A reporter that floods the socket is read at LINES_PER_SECOND.
        if window.1 >= LINES_PER_SECOND {
            tokio::time::sleep_until(window.0 + std::time::Duration::from_secs(1)).await;
        }
        let now = tokio::time::Instant::now();
        if now >= window.0 + std::time::Duration::from_secs(1) {
            window = (now, 0);
        }
        window.1 += 1;
        match reader.next_line().await {
            Ok(Some(line)) => match FocusMessage::parse(line.as_bytes()) {
                Ok(msg) => {
                    debug!(?pid, reporter, client = ?msg.client, focus = ?msg.focus, clicked = msg.clicked, "text field focus");
                    if shell
                        .send(Msg::TextFocus {
                            pid,
                            reporter,
                            focus: msg.focus,
                            clicked: msg.clicked,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                // The first bad line of a connection at info, the rest at debug.
                Err(e) if bad == 0 => {
                    bad += 1;
                    info!(?pid, "osk.sock: {e}");
                }
                Err(e) => {
                    bad += 1;
                    debug!(?pid, bad, "osk.sock: {e}");
                }
            },
            Ok(None) => break,
            Err(e) => {
                info!(?pid, "osk.sock: {e}");
                break;
            }
        }
    }
    // Firefox (or the extension) went away: whatever had focus no longer does.
    let _ = shell.send(Msg::TextFocusClosed { reporter });
    debug!(?pid, "osk.sock: reporter disconnected");
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Cursor};
    use std::os::unix::net::UnixListener as StdListener;

    use super::*;

    fn frame(bytes: &[u8]) -> Vec<u8> {
        let mut v = (bytes.len() as u32).to_ne_bytes().to_vec();
        v.extend_from_slice(bytes);
        v
    }

    fn frames(mut bytes: &[u8]) -> Vec<Value> {
        let mut out = vec![];
        while let Some(f) = read_frame(&mut bytes).unwrap() {
            out.push(serde_json::from_slice(&f).unwrap());
        }
        out
    }

    #[test]
    fn native_messaging_framing() {
        let mut buf = vec![];
        write_frame(&mut buf, &json!({"ok": true})).unwrap();
        assert_eq!(&buf[..4], &11u32.to_ne_bytes());
        assert_eq!(&buf[4..], br#"{"ok":true}"#);
        assert_eq!(frames(&buf), [json!({"ok": true})]);

        // Two frames back to back, then a clean end.
        let mut input = frame(br#"{"focus":"text"}"#);
        input.extend(frame(br#"{"focus":"none"}"#));
        let mut r = Cursor::new(input);
        assert_eq!(read_frame(&mut r).unwrap().unwrap(), br#"{"focus":"text"}"#);
        assert_eq!(read_frame(&mut r).unwrap().unwrap(), br#"{"focus":"none"}"#);
        assert_eq!(read_frame(&mut r).unwrap(), None);

        // Truncated: inside the length, inside the body.
        let e = read_frame(&mut &[1u8, 0][..]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
        let mut short = frame(b"{\"focus\":\"text\"}");
        short.truncate(10);
        assert_eq!(
            read_frame(&mut &short[..]).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        // Too long to be one of ours.
        let huge = ((MAX_FRAME + 1) as u32).to_ne_bytes();
        assert_eq!(
            read_frame(&mut &huge[..]).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn focus_messages() {
        assert_eq!(
            FocusMessage::parse(br#"{"focus":"password","frame":3}"#).unwrap(),
            FocusMessage {
                focus: FieldFocus::Password,
                client: None,
                clicked: false,
            }
        );
        assert_eq!(
            FocusMessage::parse(br#"{"focus":"text","clicked":true}"#).unwrap(),
            FocusMessage {
                focus: FieldFocus::Text,
                client: None,
                clicked: true,
            }
        );
        assert!(FocusMessage::parse(br#"{"focus":"button"}"#).is_err());
        assert!(FocusMessage::parse(br#"{}"#).is_err());
        assert!(FocusMessage::parse(b"nope").is_err());
        assert!(FieldFocus::Text.editable() && !FieldFocus::None.editable());
    }

    #[test]
    fn host_forwards_to_the_daemon_and_reconnects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("osk.sock");
        let listener = StdListener::bind(&path).unwrap();

        let mut input = frame(br#"{"focus":"text"}"#);
        input.extend(frame(br#"{"focus":"bogus"}"#));
        input.extend(frame(br#"{"focus":"text","clicked":true}"#));
        input.extend(frame(br#"{"focus":"none"}"#));
        let mut output = vec![];
        let connects = std::cell::Cell::new(0);
        host_loop(&mut &input[..], &mut output, || {
            connects.set(connects.get() + 1);
            UnixStream::connect(&path)
        })
        .unwrap();
        assert_eq!(connects.get(), 1, "one connection for every message");
        assert_eq!(
            frames(&output),
            [
                json!({"ok": true}),
                json!({"ok": false, "error": frames_error("bogus")}),
                json!({"ok": true}),
                json!({"ok": true}),
            ]
        );
        let (conn, _) = listener.accept().unwrap();
        let lines: Vec<String> = BufReader::new(conn).lines().map(Result::unwrap).collect();
        assert_eq!(
            lines,
            [
                r#"{"focus":"text","client":"hinteros-firefox"}"#,
                r#"{"focus":"text","client":"hinteros-firefox","clicked":true}"#,
                r#"{"focus":"none","client":"hinteros-firefox"}"#,
            ]
        );

        // The daemon is gone: an error per message, and the host keeps going.
        drop(listener);
        std::fs::remove_file(&path).unwrap();
        let input = frame(br#"{"focus":"text"}"#);
        let mut output = vec![];
        host_loop(&mut &input[..], &mut output, || UnixStream::connect(&path)).unwrap();
        let reply = &frames(&output)[0];
        assert_eq!(reply["ok"], false);
        assert!(
            reply["error"]
                .as_str()
                .unwrap()
                .starts_with("session daemon:")
        );

        // The daemon restarted (the connection broke): the host connects again and the
        // message still arrives.
        let listener = StdListener::bind(&path).unwrap();
        let kept = std::cell::RefCell::new(None);
        let mut connects = 0;
        let mut input = frame(br#"{"focus":"text"}"#);
        input.extend(frame(br#"{"focus":"none"}"#));
        let mut output = vec![];
        host_loop(&mut &input[..], &mut output, || {
            connects += 1;
            let s = UnixStream::connect(&path)?;
            let (server_end, _) = listener.accept()?;
            if connects == 1 {
                drop(server_end); // the old daemon goes away
            } else {
                *kept.borrow_mut() = Some(server_end);
            }
            Ok(s)
        })
        .unwrap();
        assert_eq!(connects, 2);
        assert_eq!(frames(&output), [json!({"ok": true}), json!({"ok": true})]);
        let conn = kept.borrow_mut().take().unwrap();
        let lines: Vec<String> = BufReader::new(conn).lines().map(Result::unwrap).collect();
        assert_eq!(lines.len(), 2);
    }

    fn frames_error(value: &str) -> String {
        FocusMessage::parse(format!(r#"{{"focus":"{value}"}}"#).as_bytes()).unwrap_err()
    }

    #[test]
    fn socket_next_to_the_session_socket() {
        assert_eq!(
            socket_beside(Path::new("/run/user/1000/hinteros/session.sock")),
            Path::new("/run/user/1000/hinteros/osk.sock")
        );
    }

    async fn next_focus(rx: &mut mpsc::UnboundedReceiver<Msg>) -> (Option<u32>, u64, FieldFocus) {
        match rx.recv().await.unwrap() {
            Msg::TextFocus {
                pid,
                reporter,
                focus,
                clicked: false,
            } => (pid, reporter, focus),
            other => panic!("{other:?}"),
        }
    }

    async fn assert_closed(rx: &mut mpsc::UnboundedReceiver<Msg>, reporter: u64) {
        match rx.recv().await.unwrap() {
            Msg::TextFocusClosed { reporter: r } => assert_eq!(r, reporter),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn daemon_side_reports_focus_and_a_closed_connection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("osk.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let server = tokio::spawn(serve(listener, tx));
        let me = Some(std::process::id());

        let mut c = UnixStream::connect(&path).unwrap();
        c.write_all(b"{\"focus\":\"password\"}\nnot json\n{\"focus\":\"text\",\"client\":\"x\"}\n")
            .unwrap();
        let (pid, a, focus) = next_focus(&mut rx).await;
        assert_eq!((pid, focus), (me, FieldFocus::Password));
        assert_eq!(next_focus(&mut rx).await, (me, a, FieldFocus::Text));

        // A second reporter (Firefox's address bar next to its extension) gets its own id,
        // and closing it clears only its own state.
        let mut d = UnixStream::connect(&path).unwrap();
        d.write_all(b"{\"focus\":\"text\",\"client\":\"hinteros-firefox-chrome\"}\n")
            .unwrap();
        let (pid, b, focus) = next_focus(&mut rx).await;
        assert_ne!(a, b);
        assert_eq!((pid, focus), (me, FieldFocus::Text));
        // A click into the field it already had: the same state, marked clicked.
        d.write_all(b"{\"focus\":\"text\",\"clicked\":true}\n")
            .unwrap();
        match rx.recv().await.unwrap() {
            Msg::TextFocus {
                reporter,
                focus: FieldFocus::Text,
                clicked: true,
                ..
            } => assert_eq!(reporter, b),
            other => panic!("{other:?}"),
        }
        drop(d);
        assert_closed(&mut rx, b).await;

        drop(c);
        assert_closed(&mut rx, a).await;
        server.abort();
    }
}
