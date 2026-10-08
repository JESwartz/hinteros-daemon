//! `hinteros-system pin splash`: the console splash on the session's VT while greetd hands over
//! from the picker to a profile's session (or back). Between the last frame of one compositor and
//! the first of the next the kernel console (fbcon) of greetd's VT is on screen, ~2 s on real
//! hardware; this paints it in the launcher's colours with a centred "Loading <name>…" instead of
//! a black (or stale) text screen.
//!
//! Linux console escape sequences only (console_codes(4)): palette entries 0 (background) and 7/15
//! (the default and bold foreground), reset attributes, clear, cursor home, cursor hidden, then
//! the line at the centre of the tty's size (TIOCGWINSZ). The tty comes from `PAM_TTY` and must be
//! `/dev/ttyN` or `ttyN` (a virtual console); it is opened without following symlinks and checked
//! to be a VT character device. Nothing here ever fails a login: the verb always exits 0.

use std::io::Write;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use tracing::info;

use crate::profiles::ProfilesFile;
use crate::state::{self, State};

/// The launcher's standard theme (hinteros-launcher lib/src/theme/hinter_theme.dart).
pub const BACKGROUND: [u8; 3] = [0x0E, 0x11, 0x18];
pub const TEXT: [u8; 3] = [0xF4, 0xF6, 0xFB];

/// The greeter session's user (its splash says just "HinterOS").
pub const GREETER_USER: &str = "hinteros-greeter";

/// Longest name shown, in characters.
const MAX_NAME: usize = 32;

/// `/dev/ttyN` for a `PAM_TTY` of `/dev/ttyN` or `ttyN` (N = 1..=63); anything else: None.
pub fn tty_path(pam_tty: &str) -> Option<PathBuf> {
    let n = pam_tty
        .strip_prefix("/dev/")
        .unwrap_or(pam_tty)
        .strip_prefix("tty")?;
    if n.is_empty() || n.len() > 2 || !n.bytes().all(|b| b.is_ascii_digit()) || n.starts_with('0') {
        return None;
    }
    let num: u32 = n.parse().ok()?;
    (1..=63)
        .contains(&num)
        .then(|| PathBuf::from(format!("/dev/tty{num}")))
}

/// A name as the console may show it: printable characters only (no controls, no bidi or
/// zero-width format characters), whitespace runs as one space, at most [`MAX_NAME`] characters
/// (then "…").
pub fn sanitize(name: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    for c in name.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if c.is_control() || is_format(c) {
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(c);
    }
    if out.chars().count() > MAX_NAME {
        let mut t: String = out.chars().take(MAX_NAME - 1).collect();
        t.truncate(t.trim_end().len());
        t.push('…');
        return t;
    }
    out
}

/// Unicode format characters a console would show as garbage or that reorder text.
fn is_format(c: char) -> bool {
    matches!(c,
        '\u{00AD}'
        | '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{206F}'
        | '\u{FEFF}'
        | '\u{FFF0}'..='\u{FFFF}'
    )
}

/// The text for `user`: "HinterOS" for the greeter, else "Loading <name>…".
pub fn text_for(name: Option<&str>) -> String {
    match name {
        None => "HinterOS".to_owned(),
        Some(n) => {
            let n = sanitize(n);
            if n.is_empty() {
                "Loading…".to_owned()
            } else {
                format!("Loading {n}…")
            }
        }
    }
}

fn palette(out: &mut Vec<u8>, entry: char, rgb: [u8; 3]) {
    out.extend_from_slice(
        format!("\x1b]P{entry}{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2]).as_bytes(),
    );
}

/// The bytes that paint the splash on a `cols` x `rows` console.
pub fn render(text: &str, cols: u16, rows: u16) -> Vec<u8> {
    let (cols, rows) = if cols == 0 || rows == 0 {
        (80, 25)
    } else {
        (cols, rows)
    };
    // Never wider than the screen.
    let text: String = text.chars().take(cols as usize).collect();
    let width = text.chars().count() as u16;
    let row = rows / 2 + 1;
    let col = (cols - width) / 2 + 1;
    let mut out = Vec::new();
    palette(&mut out, '0', BACKGROUND);
    palette(&mut out, '7', TEXT);
    palette(&mut out, 'F', TEXT);
    // Default colours and attributes, blank screen, cursor home and hidden.
    out.extend_from_slice(b"\x1b[0m\x1b[2J\x1b[H\x1b[?25l");
    out.extend_from_slice(format!("\x1b[{row};{col}H").as_bytes());
    out.extend_from_slice(b"\x1b[1m");
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(b"\x1b[0m");
    out
}

/// The name to show for `user`: None for the greeter ("HinterOS"), "Guest" for the guest, the
/// profile's name, else (a spare in first-boot mode, anything unknown) "HinterOS" too.
pub fn name_for(state_json: &Path, profiles_json: &Path, user: &str) -> Option<String> {
    if user == GREETER_USER {
        return None;
    }
    if user == state::GUEST_USER {
        return Some("Guest".into());
    }
    let state = State::load(state_json).ok().map(|(s, _)| s)?;
    let account = state.accounts().into_iter().find(|a| a.user == user)?;
    let file = ProfilesFile::load(profiles_json).ok()?;
    file.profiles.get(&account.id).map(|m| m.name.clone())
}

/// Open `path` (a VT) for writing, never following a symlink, and check it is one.
fn open_vt(path: &Path) -> std::io::Result<std::fs::File> {
    let f = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc_flags())
        .open(path)?;
    let m = f.metadata()?;
    // Virtual consoles: character devices, major 4, minor 1..=63.
    let rdev = m.rdev();
    let (major, minor) = (
        rustix::fs::major(rdev as rustix::fs::Dev),
        rustix::fs::minor(rdev as rustix::fs::Dev),
    );
    if !m.file_type().is_char_device() || major != 4 || !(1..=63).contains(&minor) {
        return Err(std::io::Error::other("not a virtual console"));
    }
    Ok(f)
}

fn libc_flags() -> i32 {
    (rustix::fs::OFlags::NOCTTY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK)
        .bits() as i32
}

/// Paint the splash on `PAM_TTY` for `user`. Errors are logged, never returned.
pub fn run(state_json: &Path, profiles_json: &Path, user: &str, pam_tty: Option<&str>) {
    let Some(path) = pam_tty.and_then(tty_path) else {
        info!("splash: no virtual console in PAM_TTY");
        return;
    };
    let mut f = match open_vt(&path) {
        Ok(f) => f,
        Err(e) => {
            info!("splash: {}: {e}", path.display());
            return;
        }
    };
    let (cols, rows) = rustix::termios::tcgetwinsize(&f)
        .map(|w| (w.ws_col, w.ws_row))
        .unwrap_or((0, 0));
    let name = name_for(state_json, profiles_json, user);
    let bytes = render(&text_for(name.as_deref()), cols, rows);
    if let Err(e) = f.write_all(&bytes) {
        info!("splash: writing {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_virtual_consoles() {
        assert_eq!(tty_path("/dev/tty1"), Some("/dev/tty1".into()));
        assert_eq!(tty_path("tty7"), Some("/dev/tty7".into()));
        assert_eq!(tty_path("tty63"), Some("/dev/tty63".into()));
        for bad in [
            "",
            "tty",
            "tty0",
            "tty01",
            "tty64",
            "tty100",
            "/dev/pts/0",
            "/dev/ttyS0",
            "/dev/../dev/tty1",
            "/tmp/tty1",
            "tty1 ",
            "/dev/tty1/x",
            ":0",
        ] {
            assert_eq!(tty_path(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn names_are_sanitized_and_truncated() {
        assert_eq!(sanitize("Sam"), "Sam");
        assert_eq!(sanitize("  Sam \t\n Lee  "), "Sam Lee");
        assert_eq!(sanitize("S\x1b[2Jam\x07"), "S[2Jam");
        assert_eq!(sanitize("A\u{202E}B\u{200B}C"), "ABC");
        assert_eq!(sanitize("Zoë"), "Zoë");
        let long = "x".repeat(40);
        let s = sanitize(&long);
        assert_eq!(s.chars().count(), MAX_NAME);
        assert!(s.ends_with('…'));
        assert_eq!(text_for(Some("\x1b\x1b")), "Loading…");
        assert_eq!(text_for(None), "HinterOS");
        assert_eq!(text_for(Some("Sam")), "Loading Sam…");
    }

    #[test]
    fn renders_palette_clear_hidden_cursor_and_centred_text() {
        let out = String::from_utf8(render("Loading Sam…", 120, 33)).unwrap();
        // Palette: background, foreground and bold foreground in the launcher's colours.
        assert!(
            out.starts_with("\x1b]P00E1118\x1b]P7F4F6FB\x1b]PFF4F6FB"),
            "{out:?}"
        );
        let clear = out.find("\x1b[2J").unwrap();
        let hide = out.find("\x1b[?25l").unwrap();
        // "Loading Sam…" is 12 cells: column (120 - 12) / 2 + 1 = 55; row 33 / 2 + 1 = 17.
        let at = out.find("\x1b[17;55H").unwrap();
        let text = out.find("Loading Sam…").unwrap();
        assert!(clear < at && hide < at && at < text, "{out:?}");
        assert!(out.ends_with("Loading Sam…\x1b[0m"));
        // Only escape sequences and the text: no other control characters.
        assert!(
            !out.chars()
                .any(|c| c.is_control() && c != '\x1b' && c != '\u{7}'),
            "{out:?}"
        );
    }

    #[test]
    fn small_or_unknown_sizes() {
        let out = String::from_utf8(render("HinterOS", 0, 0)).unwrap();
        // 80x25 fallback: column 37, row 13.
        assert!(out.contains("\x1b[13;37HHinterOS") || out.contains("\x1b[13;37H\x1b[1mHinterOS"));
        // Narrower than the text: cut to the width, column 1.
        let out = String::from_utf8(render("Loading Samantha…", 7, 3)).unwrap();
        assert!(out.contains("\x1b[2;1H\x1b[1mLoading\x1b[0m"), "{out:?}");
    }

    #[test]
    fn names_from_the_profiles() {
        let d = tempfile::tempdir().unwrap();
        let state = d.path().join("state.json");
        let profiles = d.path().join("profiles.json");
        std::fs::write(
            &state,
            r#"{"schemaVersion":2,"requirements":[],"services":[],"settings":{},
               "profiles":[{"id":"p1","user":"hinteros"},{"id":"p2","user":"hinteros-p2"}]}"#,
        )
        .unwrap();
        std::fs::write(
            &profiles,
            r#"{"version":1,"next":3,"profiles":{"p1":{"name":"James","color":"teal","admin":true,
               "locked":false,"created_at":0,"last_used_at":null,
               "appearance":{"text_scale":1.0,"high_contrast":false,"reduce_motion":false}}}}"#,
        )
        .unwrap();
        assert_eq!(
            name_for(&state, &profiles, "hinteros").as_deref(),
            Some("James")
        );
        assert_eq!(
            name_for(&state, &profiles, "hinteros-guest").as_deref(),
            Some("Guest")
        );
        assert_eq!(name_for(&state, &profiles, "hinteros-greeter"), None);
        // A spare: no profile yet.
        assert_eq!(name_for(&state, &profiles, "hinteros-p2"), None);
        assert_eq!(name_for(&state, &profiles, "root"), None);
    }

    #[test]
    fn refuses_what_is_not_a_vt() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("tty1");
        std::fs::write(&f, "").unwrap();
        assert!(open_vt(&f).is_err());
        let l = d.path().join("link");
        std::os::unix::fs::symlink("/dev/null", &l).unwrap();
        assert!(open_vt(&l).is_err());
        assert!(open_vt(Path::new("/dev/null")).is_err());
    }
}
