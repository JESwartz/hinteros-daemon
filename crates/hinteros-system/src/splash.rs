//! `hinteros-system pin splash`: the console splash on the session's VT while greetd hands over
//! from the picker to a profile's session (or back). Between the last frame of one compositor and
//! the first of the next the kernel console (fbcon) of greetd's VT is on screen, ~2 s on real
//! hardware; this paints it in the launcher's colours with a centred "Loading <name>…" instead of
//! a black (or stale) text screen, in block letters large enough to read from a couch
//! ([`crate::bigtext`]).
//!
//! Linux console escape sequences only (console_codes(4)): palette entries 0 (background) and 7/15
//! (the default and bold foreground), reset attributes, clear, cursor home, cursor hidden, then
//! the lines centred in the tty's size (TIOCGWINSZ, read when the splash runs: the console font is
//! set in the initrd, so the size is final by then). The tty comes from `PAM_TTY` and must be
//! `/dev/ttyN` or `ttyN` (a virtual console); it is opened without following symlinks and checked
//! to be a VT character device. Nothing here ever fails a login: the verb always exits 0.

use std::io::Write;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use tracing::info;

use crate::bigtext;
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

/// Palette, default attributes, blank screen, cursor home and hidden.
fn prelude(out: &mut Vec<u8>) {
    palette(out, '0', BACKGROUND);
    palette(out, '7', TEXT);
    palette(out, 'F', TEXT);
    out.extend_from_slice(b"\x1b[0m\x1b[2J\x1b[H\x1b[?25l");
}

/// The lines the splash may show for `name` (see [`text_for`]), in order of preference at a
/// given size: one line, else "Loading" above the name.
fn layouts(name: Option<&str>) -> Vec<Vec<String>> {
    let text = text_for(name);
    let mut out = vec![vec![text.clone()]];
    if let Some(rest) = text.strip_prefix("Loading ") {
        out.push(vec!["Loading".to_owned(), rest.to_owned()]);
    }
    out
}

/// How the splash is drawn on a console of a given size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Block-letter scale (1-3); 0: the console font's own text (nothing larger fits).
    pub scale: usize,
    pub lines: Vec<String>,
    /// Top row and each line's first column, 1-based (console cursor addresses).
    pub top: usize,
    pub cols: Vec<usize>,
}

/// The largest block-letter layout of the splash for `name` that fits a `cols` x `rows`
/// console with a margin, centred; else the plain line in the middle. An unknown size (0)
/// counts as 80x25.
pub fn layout(name: Option<&str>, cols: u16, rows: u16) -> Layout {
    let (cols, rows) = if cols == 0 || rows == 0 {
        (80usize, 25usize)
    } else {
        (cols as usize, rows as usize)
    };
    for scale in (1..=3).rev() {
        for lines in layouts(name) {
            if !lines.iter().all(|l| bigtext::drawable(l)) {
                continue;
            }
            let gap = scale;
            let h = lines.len() * bigtext::height(scale) + (lines.len() - 1) * gap;
            let widest = lines.iter().map(|l| bigtext::width(l, scale)).max();
            if widest.is_some_and(|w| w + 4 <= cols) && h + 2 <= rows {
                let top = (rows - h) / 2 + 1;
                let cols = lines
                    .iter()
                    .map(|l| (cols - bigtext::width(l, scale)) / 2 + 1)
                    .collect();
                return Layout {
                    scale,
                    lines,
                    top,
                    cols,
                };
            }
        }
    }
    // Never wider than the screen.
    let text: String = text_for(name).chars().take(cols).collect();
    let width = text.chars().count();
    Layout {
        scale: 0,
        cols: vec![(cols - width) / 2 + 1],
        top: rows / 2 + 1,
        lines: vec![text],
    }
}

/// The bytes that paint the splash for `name` (None: "HinterOS") on a `cols` x `rows` console.
pub fn render(name: Option<&str>, cols: u16, rows: u16) -> Vec<u8> {
    let l = layout(name, cols, rows);
    let mut out = Vec::new();
    prelude(&mut out);
    if l.scale == 0 {
        out.extend_from_slice(format!("\x1b[{};{}H", l.top, l.cols[0]).as_bytes());
        out.extend_from_slice(b"\x1b[1m");
        out.extend_from_slice(l.lines[0].as_bytes());
        out.extend_from_slice(b"\x1b[0m");
        return out;
    }
    let mut row = l.top;
    for (line, col) in l.lines.iter().zip(&l.cols) {
        for cells in bigtext::rows(line, l.scale) {
            if !cells.is_empty() {
                out.extend_from_slice(format!("\x1b[{row};{col}H").as_bytes());
                out.extend_from_slice(cells.as_bytes());
            }
            row += 1;
        }
        row += l.scale;
    }
    out
}

/// The name to show for `user`: "Guest" for the guest, the profile's name, else None
/// ("HinterOS": a spare in first-boot mode, anything unknown). The greeter shows the target of
/// a switch it is about to enter without any UI (the public view's live `next` with `fast`),
/// so the TV says "Loading Sam…" all the way; otherwise "HinterOS".
pub fn name_for(
    state_json: &Path,
    profiles_json: &Path,
    public_json: &Path,
    user: &str,
    now: i64,
) -> Option<String> {
    if user == GREETER_USER {
        return switch_target(public_json, now);
    }
    if user == state::GUEST_USER {
        return Some("Guest".into());
    }
    let state = State::load(state_json).ok().map(|(s, _)| s)?;
    let account = state.accounts().into_iter().find(|a| a.user == user)?;
    let file = ProfilesFile::load(profiles_json).ok()?;
    file.profiles.get(&account.id).map(|m| m.name.clone())
}

/// The name of a live fast switch target from the public view.
fn switch_target(public_json: &Path, now: i64) -> Option<String> {
    let v: crate::public::PublicView =
        serde_json::from_slice(&std::fs::read(public_json).ok()?).ok()?;
    let next = v.next.filter(|n| n.fast && n.expires > now)?;
    if next.target == "guest" {
        return Some("Guest".into());
    }
    v.profiles
        .into_iter()
        .find(|p| p.id == next.target)
        .map(|p| p.name)
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
pub fn run(paths: &crate::config::Paths, user: &str, pam_tty: Option<&str>, now: i64) {
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
    let name = name_for(
        &paths.state_json(),
        &paths.profiles_json(),
        &paths.public_json(),
        user,
        now,
    );
    let bytes = render(name.as_deref(), cols, rows);
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
    fn renders_palette_clear_hidden_cursor_and_centred_block_letters() {
        // Terminus 16x32 (console.font ter-v32b, set in the initrd) on a 1920x1080 TV: 120x33.
        let out = String::from_utf8(render(Some("Sam"), 120, 33)).unwrap();
        // Palette: background, foreground and bold foreground in the launcher's colours.
        assert!(
            out.starts_with("\x1b]P00E1118\x1b]P7F4F6FB\x1b]PFF4F6FB"),
            "{out:?}"
        );
        let clear = out.find("\x1b[2J").unwrap();
        let hide = out.find("\x1b[?25l").unwrap();
        // "Loading" over "Sam…" at scale 2: 82 and 46 cells wide, 8 rows each and a 2-row gap
        // (18 rows): top row (33 - 18) / 2 + 1 = 8, columns (120 - 82) / 2 + 1 = 20 and
        // (120 - 46) / 2 + 1 = 38.
        let l = layout(Some("Sam"), 120, 33);
        assert_eq!(l.scale, 2);
        assert_eq!(l.lines, ["Loading", "Sam…"]);
        assert_eq!((l.top, l.cols.clone()), (8, vec![20, 38]));
        let first = out.find("\x1b[8;20H").unwrap();
        assert!(clear < first && hide < first, "{out:?}");
        assert!(out.contains("\x1b[18;38H"), "{out:?}");
        // Only escape sequences, block characters and spaces: no other control characters.
        assert!(
            !out.chars()
                .any(|c| c.is_control() && c != '\x1b' && c != '\u{7}'),
            "{out:?}"
        );
        assert!(
            out.chars().any(|c| "█▀▄".contains(c)),
            "drawn in block characters"
        );
    }

    #[test]
    fn centred_for_the_console_sizes_of_a_tv() {
        // (cols, rows) → the brand's scale, top row, column; every layout inside the screen
        // with its margin, the same distance from the left and the right edge (±1), and from
        // the top and the bottom.
        let sizes = [
            (120, 33, 2, 13, 14), // ter-v32b at 1920x1080 (the box)
            (240, 67, 3, 28, 50), // ter-v32b at 3840x2160, or 8x16 at 1920x1080
            (80, 25, 1, 11, 17),  // unknown size
            (160, 45, 3, 17, 10), // 8x16 at 1280x720
        ];
        for (cols, rows, scale, top, col) in sizes {
            let l = layout(None, cols, rows);
            assert_eq!(l.lines, ["HinterOS"]);
            assert_eq!(
                (l.scale, l.top, l.cols[0]),
                (scale, top, col),
                "{cols}x{rows}"
            );
        }
        for (cols, rows, ..) in sizes {
            for name in [
                None,
                Some("Sam"),
                Some("James"),
                Some("Guest"),
                Some("Alexandria"),
            ] {
                let l = layout(name, cols, rows);
                assert!(l.scale >= 1, "{name:?} {cols}x{rows}");
                let h = l.lines.len() * bigtext::height(l.scale) + (l.lines.len() - 1) * l.scale;
                let (above, below) = (l.top - 1, rows as usize - (l.top - 1 + h));
                assert!(above.abs_diff(below) <= 1, "{name:?} {cols}x{rows}: {l:?}");
                for (line, c) in l.lines.iter().zip(&l.cols) {
                    let w = bigtext::width(line, l.scale);
                    let (left, right) = (c - 1, cols as usize - (c - 1 + w));
                    assert!(left >= 2 && right >= 2, "{name:?} {cols}x{rows}: {l:?}");
                    assert!(left.abs_diff(right) <= 1, "{name:?} {cols}x{rows}: {l:?}");
                }
            }
        }
    }

    #[test]
    fn small_or_unknown_sizes() {
        // 80x25 fallback: block letters at scale 1.
        assert_eq!(layout(None, 0, 0), layout(None, 80, 25));
        // Too small for any block letters: the plain line, cut to the width, column 1.
        let out = String::from_utf8(render(Some("Samantha"), 7, 3)).unwrap();
        assert!(out.contains("\x1b[2;1H\x1b[1mLoading\x1b[0m"), "{out:?}");
        // A name the block font cannot draw: the plain line, centred.
        let l = layout(Some("山田"), 120, 33);
        assert_eq!(
            (l.scale, l.lines.clone()),
            (0, vec!["Loading 山田…".to_owned()])
        );
        assert_eq!((l.top, l.cols[0]), (17, 55));
        // A long name: still big, on two lines, or plain if even that does not fit.
        let l = layout(Some(&"W".repeat(32)), 120, 33);
        assert_eq!(l.scale, 0);
        let l = layout(Some("Christopher"), 120, 33);
        assert_eq!((l.scale, l.lines.len()), (1, 2));
    }

    #[test]
    fn names_from_the_profiles() {
        let d = tempfile::tempdir().unwrap();
        let state = d.path().join("state.json");
        let profiles = d.path().join("profiles.json");
        let public = d.path().join("profiles-public.json");
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
        let name = |user: &str, now: i64| name_for(&state, &profiles, &public, user, now);
        assert_eq!(name("hinteros", 0).as_deref(), Some("James"));
        assert_eq!(name("hinteros-guest", 0).as_deref(), Some("Guest"));
        // The greeter without a switch it enters by itself: "HinterOS".
        assert_eq!(name("hinteros-greeter", 0), None);
        // A spare: no profile yet.
        assert_eq!(name("hinteros-p2", 0), None);
        assert_eq!(name("root", 0), None);
        // A fast switch to James or the guest: the greeter already says "Loading James…".
        let view = |target: &str, fast: bool| {
            format!(
                r#"{{"version":1,"profiles":[{{"id":"p1","user":"hinteros","name":"James","color":"teal",
                   "locked":true,"admin":true,"locked_until":null}}],
                   "guest":{{"enabled":true,"user":"hinteros-guest"}},"first_boot":false,"spare":null,
                   "last_used":"p1","next":{{"target":"{target}","fast":{fast},"expires":100}},
                   "appearance":{{"text_scale":1.0,"high_contrast":false,"reduce_motion":false}}}}"#
            )
        };
        std::fs::write(&public, view("p1", true)).unwrap();
        assert_eq!(name("hinteros-greeter", 50).as_deref(), Some("James"));
        assert_eq!(name("hinteros-greeter", 150), None, "expired");
        std::fs::write(&public, view("guest", true)).unwrap();
        assert_eq!(name("hinteros-greeter", 50).as_deref(), Some("Guest"));
        std::fs::write(&public, view("p1", false)).unwrap();
        assert_eq!(name("hinteros-greeter", 50), None, "the picker will ask");
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
