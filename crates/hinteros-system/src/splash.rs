//! `hinteros-system pin splash`: the splash on the session's VT while greetd hands over from the
//! picker to a profile's session (or back). Between the last frame of one compositor and the
//! first of the next, the kernel's framebuffer console of greetd's VT is on screen (~2 s on real
//! hardware). This paints the launcher's "Loading Sam…" page there instead ([`crate::splashdraw`]:
//! the launcher's background, avatar and headline in its own typeface, anti-aliased), so the
//! picker's loading page, the splash and the session's loading page read as one screen.
//!
//! How it stays on screen through the handover:
//! - The helper (run by pam_exec as root, `session optional`) validates `PAM_TTY` (a virtual
//!   console, opened without following symlinks), resolves the name and colour, clears the
//!   console's text in the background colour (what fbcon shows if it ever repaints) and checks
//!   that `/dev/fb0` is a 32-bit truecolour framebuffer and the font loads. Then it starts the
//!   painter (`hinteros-system splash-paint`) as a transient systemd service
//!   (`hinteros-splash-<pid>.service`: outside every session scope, so a closing session cannot
//!   take it along; without the bus, a direct child with stdio on /dev/null, every other
//!   descriptor close-on-exec, in its own session) and exits 0 at once: PAM never waits for the
//!   painting.
//! - The painter switches the VT to `KD_GRAPHICS` (fbcon stops drawing on it) and writes the
//!   picture to `/dev/fb0` every 80 ms, while the VT is the foreground one. On the first login
//!   after the picker the greeter's compositor may still own the display when the splash starts;
//!   the kernel shows the framebuffer again once it lets go, and logind puts the VT back to text
//!   mode when the greeter's session ends: the painter repaints and takes graphics mode again.
//! - With a profile, the launcher's loading spinner turns below the line (only its square is
//!   written each frame). Once a session controller holds the VT again after it was free
//!   (logind's `VT_PROCESS` mode: the next compositor took control; fbcon stays off and the
//!   picture stays until that compositor's first frame) only the spinner is painted, so the
//!   wait for that first frame does not look like a hang. It stops when a newer splash replaces
//!   it (`/run/hinteros/splash-painter`), or after 5 s. If it set graphics mode and no session holds
//!   the VT when it stops, it writes the line as console text and puts the VT back in text mode:
//!   a console is never left blank.
//!
//! Without a usable framebuffer (or font) the fallback is console text: the line centred in the
//! console's own font (Linux escape sequences, console_codes(4)). Nothing here ever fails a
//! login: the verb always exits 0, and the painter only logs.

use std::io::Write;
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tracing::info;

use crate::profiles::ProfilesFile;
use crate::splashdraw::{self, Avatar, BACKGROUND, PixelFormat, Scene, TEXT, Typeface};
use crate::state::{self, State};
use crate::vtio;

/// The greeter session's user (its splash says just "HinterOS").
pub const GREETER_USER: &str = "hinteros-greeter";

/// The framebuffer of the console (DRM's fbdev emulation on the box: `i915drmfb`).
pub const FB_PATH: &str = "/dev/fb0";

/// The painter's claim: the pid of the newest one (an older one stops when it changes).
pub const PAINTER_FILE: &str = "splash-painter";

/// Longest name shown, in characters.
const MAX_NAME: usize = 32;
/// Longest font file read.
const MAX_FONT: u64 = 16 << 20;
/// How long the painter keeps the picture up, and how often it repaints.
const PAINT_FOR: Duration = Duration::from_secs(5);
const PAINT_EVERY: Duration = Duration::from_millis(80);

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

/// A name as the splash may show it: printable characters only (no controls, no bidi or
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

/// Unicode format characters that would show as garbage or reorder text.
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

/// The text for a name: "HinterOS" for none (the greeter), else "Loading <name>…".
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

/// Who is being loaded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Identity {
    pub name: String,
    /// The profile's colour (profiles.rs `COLORS`); None for the guest or unknown.
    pub color: Option<String>,
    pub guest: bool,
}

impl Identity {
    fn guest() -> Identity {
        Identity {
            name: "Guest".into(),
            color: None,
            guest: true,
        }
    }
}

/// Who `user` is: the guest, a profile (name and colour), else None ("HinterOS": a spare in
/// first-boot mode, anything unknown). The greeter shows the target of a switch it is about to
/// enter without any UI (the public view's live `next` with `fast`), so the TV says
/// "Loading Sam…" all the way; otherwise "HinterOS".
pub fn identity_for(
    state_json: &Path,
    profiles_json: &Path,
    public_json: &Path,
    user: &str,
    now: i64,
) -> Option<Identity> {
    if user == GREETER_USER {
        return switch_target(public_json, now);
    }
    if user == state::GUEST_USER {
        return Some(Identity::guest());
    }
    let state = State::load(state_json).ok().map(|(s, _)| s)?;
    let account = state.accounts().into_iter().find(|a| a.user == user)?;
    let file = ProfilesFile::load(profiles_json).ok()?;
    file.profiles.get(&account.id).map(|m| Identity {
        name: m.name.clone(),
        color: Some(m.color.clone()),
        guest: false,
    })
}

/// A live fast switch target from the public view.
fn switch_target(public_json: &Path, now: i64) -> Option<Identity> {
    let v: crate::public::PublicView =
        serde_json::from_slice(&std::fs::read(public_json).ok()?).ok()?;
    let next = v.next.filter(|n| n.fast && n.expires > now)?;
    if next.target == "guest" {
        return Some(Identity::guest());
    }
    v.profiles
        .into_iter()
        .find(|p| p.id == next.target)
        .map(|p| Identity {
            name: p.name,
            color: Some(p.color),
            guest: false,
        })
}

/// What the framebuffer splash shows for `who` with `face`: the line, and the avatar for a
/// profile or the guest. A name the font cannot draw: "Loading…" (and a plain circle).
pub fn scene_for(face: &Typeface<'_>, who: Option<&Identity>) -> Scene {
    let Some(who) = who else {
        return Scene {
            text: text_for(None),
            avatar: None,
        };
    };
    let name = sanitize(&who.name);
    let drawable = face.covers(&name);
    let text = if drawable {
        text_for(Some(&name))
    } else {
        text_for(Some(""))
    };
    // The launcher's initial: the name's first character, upper case; "H" without a name.
    let initial: String = match name.chars().next() {
        Some(c) => c.to_uppercase().collect(),
        None => "H".into(),
    };
    let initial = face.covers(&initial).then_some(initial);
    let avatar = if who.guest {
        Avatar::guest(initial)
    } else {
        Avatar::profile(initial, who.color.as_deref())
    };
    Scene {
        text,
        avatar: Some(avatar),
    }
}

fn palette(out: &mut Vec<u8>, entry: char, rgb: splashdraw::Rgb) {
    out.extend_from_slice(
        format!("\x1b]P{entry}{:02X}{:02X}{:02X}", rgb.0, rgb.1, rgb.2).as_bytes(),
    );
}

/// Palette in the launcher's colours, default attributes, blank screen, cursor home and hidden.
fn prelude(out: &mut Vec<u8>) {
    palette(out, '0', BACKGROUND);
    palette(out, '7', TEXT);
    palette(out, 'F', TEXT);
    out.extend_from_slice(b"\x1b[0m\x1b[2J\x1b[H\x1b[?25l");
}

/// The console text fallback: `text` in bold, centred on a `cols` x `rows` console (an unknown
/// size, 0, counts as 80x25), cut to the width.
pub fn console_text(text: &str, cols: u16, rows: u16) -> Vec<u8> {
    let (cols, rows) = if cols == 0 || rows == 0 {
        (80usize, 25usize)
    } else {
        (cols as usize, rows as usize)
    };
    let text: String = text.chars().take(cols).collect();
    let width = text.chars().count();
    let mut out = Vec::new();
    prelude(&mut out);
    out.extend_from_slice(
        format!(
            "\x1b[{};{}H\x1b[1m{text}\x1b[0m",
            rows.div_ceil(2),
            (cols - width) / 2 + 1
        )
        .as_bytes(),
    );
    out
}

/// Only the prelude: the console cleared in the background colour.
fn console_blank() -> Vec<u8> {
    let mut out = Vec::new();
    prelude(&mut out);
    out
}

fn nofollow() -> i32 {
    (rustix::fs::OFlags::NOCTTY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK)
        .bits() as i32
}

/// Open `path` (a VT) for writing, never following a symlink, and check it is one; with its
/// number.
fn open_vt(path: &Path) -> std::io::Result<(std::fs::File, u16)> {
    let f = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(nofollow())
        .open(path)?;
    let m = f.metadata()?;
    // Virtual consoles: character devices, major 4, minor 1..=63.
    let rdev = m.rdev() as rustix::fs::Dev;
    let (major, minor) = (rustix::fs::major(rdev), rustix::fs::minor(rdev));
    if !m.file_type().is_char_device() || major != 4 || !(1..=63).contains(&minor) {
        return Err(std::io::Error::other("not a virtual console"));
    }
    Ok((f, minor as u16))
}

/// The framebuffer, opened without following a symlink, if the splash can draw on it.
pub struct Fb {
    file: std::fs::File,
    pub var: vtio::FbVar,
    pub fix: vtio::FbFix,
    pub format: PixelFormat,
}

impl Fb {
    pub fn open(path: &Path) -> std::io::Result<Fb> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(nofollow())
            .open(path)?;
        let m = file.metadata()?;
        // Framebuffers: character devices, major 29.
        if !m.file_type().is_char_device() || rustix::fs::major(m.rdev() as rustix::fs::Dev) != 29 {
            return Err(std::io::Error::other("not a framebuffer"));
        }
        let (var, fix) = vtio::fb_info(&file)?;
        let format = PixelFormat::from_fb(&var, &fix).ok_or_else(|| {
            std::io::Error::other(format!(
                "{} bpp, visual {}: not 32-bit truecolour",
                var.bits_per_pixel, fix.visual
            ))
        })?;
        if var.xres == 0 || var.yres == 0 || frame_place(&var, &fix).is_none() {
            return Err(std::io::Error::other("unusable geometry"));
        }
        Ok(Fb {
            file,
            var,
            fix,
            format,
        })
    }

    fn describe(&self) -> String {
        let f = self.format;
        let alpha = f.alpha.map(|a| format!(" a{a}")).unwrap_or_default();
        format!(
            "{}x{}, 32 bpp r{} g{} b{}{alpha}",
            self.var.xres, self.var.yres, f.red, f.green, f.blue
        )
    }
}

/// Where the visible screen starts in the framebuffer's memory and how long it is (bytes), if
/// it fits in it.
pub fn frame_place(var: &vtio::FbVar, fix: &vtio::FbFix) -> Option<(u64, usize)> {
    let stride = fix.line_length as usize;
    if stride < (var.xoffset as usize + var.xres as usize) * 4 {
        return None;
    }
    let offset = var.yoffset as u64 * stride as u64;
    let len = stride.checked_mul(var.yres as usize)?;
    (fix.smem_len == 0 || offset + len as u64 <= fix.smem_len as u64).then_some((offset, len))
}

/// The font file, read whole (bounded).
fn read_font(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_FONT)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn journal(message: &str, fields: &[(&str, &str)]) {
    crate::audit::journal_note(
        Path::new("/run/systemd/journal/socket"),
        "hinteros-splash",
        message,
        fields,
    );
}

/// Paint the splash on `PAM_TTY` for `user` (the framebuffer with `font`, else console text).
/// Errors are logged, never returned.
pub fn run(
    paths: &crate::config::Paths,
    user: &str,
    pam_tty: Option<&str>,
    font: Option<&Path>,
    bus: Option<&str>,
    now: i64,
) {
    let Some(path) = pam_tty.and_then(tty_path) else {
        info!("splash: no virtual console in PAM_TTY");
        return;
    };
    let (mut tty, _) = match open_vt(&path) {
        Ok(f) => f,
        Err(e) => {
            info!("splash: {}: {e}", path.display());
            return;
        }
    };
    let who = identity_for(
        &paths.state_json(),
        &paths.profiles_json(),
        &paths.public_json(),
        user,
        now,
    );
    let text = text_for(who.as_ref().map(|w| w.name.as_str()));
    // The framebuffer if it can be drawn on; why not, else.
    let fb = match font {
        None => Err("no font given".to_owned()),
        Some(font) => read_font(font)
            .map_err(|e| format!("font {}: {e}", font.display()))
            .and_then(|b| {
                Typeface::parse(&b)
                    .map(|_| ())
                    .ok_or_else(|| format!("font {}: not a font", font.display()))
            })
            .and_then(|()| Fb::open(Path::new(FB_PATH)).map_err(|e| format!("{FB_PATH}: {e}")))
            .and_then(|fb| {
                let args = painter_args(&path, font, &paths.run_dir, who.as_ref());
                start_painter(bus, &args)
                    .map(|how| format!("{}, painter: {how}", fb.describe()))
                    .map_err(|e| format!("painter: {e}"))
            }),
    };
    match fb {
        Ok(desc) => {
            // What fbcon shows if it repaints before the painter takes the VT.
            let _ = tty.write_all(&console_blank());
            journal(
                &format!("splash on {}: {text} (framebuffer {desc})", path.display()),
                &[
                    ("SPLASH", &text),
                    ("USER", user),
                    ("SPLASH_MODE", "framebuffer"),
                ],
            );
        }
        Err(why) => {
            let (cols, rows) = rustix::termios::tcgetwinsize(&tty)
                .map(|w| (w.ws_col, w.ws_row))
                .unwrap_or((0, 0));
            if let Err(e) = tty.write_all(&console_text(&text, cols, rows)) {
                info!("splash: writing {}: {e}", path.display());
                return;
            }
            // pam_exec drops the helper's stderr: say what is on screen in the journal (the VM
            // tests read it there).
            journal(
                &format!(
                    "splash on {}: {text} (console text, {cols}x{rows} cells; {why})",
                    path.display()
                ),
                &[
                    ("SPLASH", &text),
                    ("USER", user),
                    ("SPLASH_MODE", "console"),
                ],
            );
        }
    }
}

/// The painter's command line after the program.
pub fn painter_args(
    tty: &Path,
    font: &Path,
    run_dir: &Path,
    who: Option<&Identity>,
) -> Vec<String> {
    let mut args = vec![
        "splash-paint".to_owned(),
        format!("--tty={}", tty.display()),
        format!("--font={}", font.display()),
        format!("--run-dir={}", run_dir.display()),
    ];
    if let Some(w) = who {
        // `--name=` so that a name starting with '-' is still the value.
        args.push(format!("--name={}", sanitize(&w.name)));
        if let Some(c) = w.color.as_deref().filter(|c| crate::profiles::is_color(c)) {
            args.push(format!("--color={c}"));
        }
        if w.guest {
            args.push("--guest".to_owned());
        }
    }
    args
}

/// Start the painter and return at once (see the module docs): a transient systemd service, so
/// that it runs outside every session's scope (pam_exec may run this helper in a process that
/// already belongs to the session being closed, and systemd kills a closing scope's processes);
/// without the bus, a direct child in its own session. Says which.
fn start_painter(bus: Option<&str>, args: &[String]) -> std::io::Result<String> {
    let exe = std::fs::read_link("/proc/self/exe")?;
    let unit = format!("hinteros-splash-{}.service", std::process::id());
    match start_unit(bus, &unit, &exe, args) {
        Ok(()) => Ok(unit),
        Err(e) => {
            info!("splash: no transient unit ({e}); spawning the painter");
            vtio::cloexec_inherited();
            Command::new(&exe)
                .args(args)
                .env_clear()
                .current_dir("/")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            // Not waited for: when this helper exits the painter is reparented to init.
            Ok("a detached process".to_owned())
        }
    }
}

/// `StartTransientUnit` for the painter, waiting at most 2 s for systemd to accept it.
fn start_unit(bus: Option<&str>, unit: &str, exe: &Path, args: &[String]) -> anyhow::Result<()> {
    use zbus::zvariant::Value;
    let mut argv = vec![exe.to_string_lossy().into_owned()];
    argv.extend(args.iter().cloned());
    let exec = vec![(exe.to_string_lossy().into_owned(), argv, false)];
    let props: Vec<(&str, Value<'_>)> = vec![
        ("Description", Value::from("HinterOS handover splash")),
        ("ExecStart", Value::from(exec)),
        ("CollectMode", Value::from("inactive-or-failed")),
        // A hard bound on top of the painter's own 5 s.
        ("RuntimeMaxUSec", Value::from(15_000_000u64)),
        ("StandardOutput", Value::from("null")),
    ];
    let aux: Vec<(&str, Vec<(&str, Value<'_>)>)> = Vec::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), async {
            let conn = match bus {
                Some(a) => zbus::connection::Builder::address(a)?.build().await?,
                None => zbus::Connection::system().await?,
            };
            conn.call_method(
                Some("org.freedesktop.systemd1"),
                "/org/freedesktop/systemd1",
                Some("org.freedesktop.systemd1.Manager"),
                "StartTransientUnit",
                &(unit, "fail", props, aux),
            )
            .await?;
            anyhow::Ok(())
        })
        .await?
    })
}

/// The painter's arguments (`hinteros-system splash-paint`).
#[derive(Debug, Clone, clap::Args)]
pub struct PaintArgs {
    /// The VT (`/dev/ttyN`)
    #[arg(long)]
    pub tty: PathBuf,
    /// The typeface (TTF/OTF)
    #[arg(long)]
    pub font: PathBuf,
    /// The profile's name (none: "HinterOS")
    #[arg(long)]
    pub name: Option<String>,
    /// The profile's colour
    #[arg(long)]
    pub color: Option<String>,
    /// The guest
    #[arg(long)]
    pub guest: bool,
    #[arg(long, default_value = "/run/hinteros")]
    pub run_dir: PathBuf,
}

/// Why the painter stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// A session controller holds the VT again.
    Session,
    /// A newer splash took over.
    Replaced,
    Timeout,
}

/// `hinteros-system splash-paint`: paint until the next compositor holds the VT, a newer
/// splash replaces this one, or [`PAINT_FOR`] passed. Logs, never fails.
pub fn paint_main(a: &PaintArgs) {
    // Its own session when spawned directly (a systemd service already is one): nothing of
    // greetd's or PAM's process group or terminal.
    let _ = rustix::process::setsid();
    let started = Instant::now();
    if let Err(e) = paint(a, started) {
        journal(
            &format!("splash painter on {}: {e}", a.tty.display()),
            &[("SPLASH_MODE", "framebuffer")],
        );
    }
}

fn paint(a: &PaintArgs, started: Instant) -> std::io::Result<()> {
    let tty_ok = a
        .tty
        .to_str()
        .and_then(tty_path)
        .is_some_and(|p| p == a.tty);
    if !tty_ok {
        return Err(std::io::Error::other("not a virtual console"));
    }
    let (mut tty, vt) = open_vt(&a.tty)?;
    let fb = Fb::open(Path::new(FB_PATH))?;
    let font = read_font(&a.font)?;
    let face = Typeface::parse(&font).ok_or_else(|| std::io::Error::other("not a font"))?;
    let who = a.name.as_ref().map(|n| Identity {
        name: n.clone(),
        color: a.color.clone(),
        guest: a.guest,
    });
    let scene = scene_for(&face, who.as_ref());
    let canvas = splashdraw::render(&face, &scene, fb.var.xres, fb.var.yres);
    let (offset, len) =
        frame_place(&fb.var, &fb.fix).ok_or_else(|| std::io::Error::other("unusable geometry"))?;
    let frame = splashdraw::to_fb_bytes(
        &canvas,
        fb.format,
        fb.fix.line_length as usize,
        fb.var.xoffset as usize,
    );
    debug_assert_eq!(frame.len(), len);
    // The spinner turns every frame: only its square is written again.
    let lay = splashdraw::scene_layout(&face, &scene, fb.var.xres, fb.var.yres);
    let stride = fb.fix.line_length as u64;
    let xoffset = fb.var.xoffset as u64;
    let spin = |turn: f32| -> bool {
        let Some((left, top, square)) = splashdraw::render_spinner(&lay, turn) else {
            return true;
        };
        splashdraw::to_fb_rows(&square, fb.format)
            .iter()
            .enumerate()
            .all(|(i, row)| {
                let at = offset + (top as u64 + i as u64) * stride + (left as u64 + xoffset) * 4;
                fb.file.write_all_at(row, at).is_ok()
            })
    };

    // The newest painter owns the screen.
    let claim = a.run_dir.join(PAINTER_FILE);
    let me = std::process::id().to_string();
    let claimed = crate::fsutil::write_atomic(&claim, me.as_bytes(), 0o644).is_ok();
    let replaced = || claimed && std::fs::read_to_string(&claim).is_ok_and(|s| s != me);

    let (mut frames, mut released, mut set_graphics) = (0u32, false, false);
    let (mut session, mut turn) = (false, 0.0f32);
    let stop = loop {
        if replaced() {
            break Stop::Replaced;
        }
        let controlled = vtio::vt_controlled(&tty).unwrap_or(false);
        if !controlled {
            released = true;
        } else if released {
            // The next compositor took control: the picture stays until its first frame,
            // which takes a moment more. Keep the spinner turning meanwhile (only its square:
            // nothing else changes, and once that frame is up the framebuffer is not shown).
            session = true;
        }
        turn += splashdraw::SPINNER_STEP;
        if session {
            if spin(turn) {
                frames += 1;
            }
        } else if vtio::active_vt(&tty).ok() == Some(vt) {
            if !controlled && vtio::kd_mode(&tty).ok() != Some(vtio::KD_GRAPHICS) {
                set_graphics |= vtio::set_kd_mode(&tty, vtio::KD_GRAPHICS).is_ok();
            }
            if fb.file.write_all_at(&frame, offset).is_ok() && spin(turn) {
                frames += 1;
            }
        }
        if started.elapsed() >= PAINT_FOR {
            break if session {
                Stop::Session
            } else {
                Stop::Timeout
            };
        }
        std::thread::sleep(PAINT_EVERY);
    };
    // Never leave a VT nobody holds blank in graphics mode: the line as console text.
    let mut restored = false;
    if stop != Stop::Replaced
        && set_graphics
        && !vtio::vt_controlled(&tty).unwrap_or(true)
        && vtio::kd_mode(&tty).ok() == Some(vtio::KD_GRAPHICS)
    {
        let (cols, rows) = rustix::termios::tcgetwinsize(&tty)
            .map(|w| (w.ws_col, w.ws_row))
            .unwrap_or((0, 0));
        let _ = tty.write_all(&console_text(&scene.text, cols, rows));
        restored = vtio::set_kd_mode(&tty, vtio::KD_TEXT).is_ok();
    }
    if claimed && !replaced() {
        let _ = std::fs::remove_file(&claim);
    }
    let reason = match stop {
        Stop::Session => "the session holds the console",
        Stop::Replaced => "a newer splash took over",
        Stop::Timeout => "time is up",
    };
    journal(
        &format!(
            "splash painter on {}: {} on the framebuffer ({}), {frames} frames in {} ms; {reason}{}",
            a.tty.display(),
            scene.text,
            fb.describe(),
            started.elapsed().as_millis(),
            if restored {
                "; console back in text mode"
            } else {
                ""
            }
        ),
        &[
            ("SPLASH_MODE", "framebuffer"),
            ("SPLASH_FRAMES", &frames.to_string()),
        ],
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    const FONT: &[u8] = include_bytes!("../fixtures/AtkinsonHyperlegible-Bold.otf");

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
    fn scenes() {
        let face = Typeface::parse(FONT).unwrap();
        let sam = Identity {
            name: "sam".into(),
            color: Some("orange".into()),
            guest: false,
        };
        let s = scene_for(&face, Some(&sam));
        assert_eq!(s.text, "Loading sam…");
        let a = s.avatar.unwrap();
        assert_eq!(a.initial.as_deref(), Some("S"));
        assert_eq!(Some(a.fill), splashdraw::profile_color("orange"));
        let g = scene_for(&face, Some(&Identity::guest()));
        assert_eq!(g.text, "Loading Guest…");
        assert!(g.avatar.unwrap().ring.is_some());
        assert_eq!(
            scene_for(&face, None),
            Scene {
                text: "HinterOS".into(),
                avatar: None
            }
        );
        // A name the typeface cannot draw: no tofu, just "Loading…" and a plain circle.
        let yamada = Identity {
            name: "山田".into(),
            color: Some("teal".into()),
            guest: false,
        };
        let y = scene_for(&face, Some(&yamada));
        assert_eq!(y.text, "Loading…");
        assert_eq!(y.avatar.unwrap().initial, None);
        // Control characters never reach the picture.
        let evil = Identity {
            name: "\u{202E}Sam\x1b[2J".into(),
            color: None,
            guest: false,
        };
        assert_eq!(scene_for(&face, Some(&evil)).text, "Loading Sam[2J…");
    }

    #[test]
    fn console_fallback_is_normal_text_centred() {
        let out = String::from_utf8(console_text("Loading Sam…", 120, 33)).unwrap();
        assert!(
            out.starts_with("\x1b]P00E1118\x1b]P7F4F6FB\x1b]PFF4F6FB"),
            "{out:?}"
        );
        let clear = out.find("\x1b[2J").unwrap();
        let hide = out.find("\x1b[?25l").unwrap();
        // 12 characters: column (120 - 12) / 2 + 1 = 55, row 17 of 33.
        let at = out.find("\x1b[17;55H\x1b[1mLoading Sam…\x1b[0m").unwrap();
        assert!(clear < at && hide < at, "{out:?}");
        // No block letters any more, no other control characters.
        assert!(!out.chars().any(|c| "█▀▄".contains(c)));
        assert!(
            !out.chars().any(|c| c.is_control() && c != '\x1b'),
            "{out:?}"
        );
        // Unknown size: 80x25; too narrow: cut, column 1.
        assert!(
            String::from_utf8(console_text("HinterOS", 0, 0))
                .unwrap()
                .contains("\x1b[13;37H")
        );
        let narrow = String::from_utf8(console_text("Loading Samantha…", 7, 3)).unwrap();
        assert!(
            narrow.contains("\x1b[2;1H\x1b[1mLoading\x1b[0m"),
            "{narrow:?}"
        );
    }

    #[test]
    fn painter_command_line() {
        let sam = Identity {
            name: "-Sam \x1b[2J".into(),
            color: Some("orange; rm".into()),
            guest: false,
        };
        let a = painter_args(
            Path::new("/dev/tty1"),
            Path::new("/f.otf"),
            Path::new("/run/hinteros"),
            Some(&sam),
        );
        assert_eq!(
            a,
            [
                "splash-paint",
                "--tty=/dev/tty1",
                "--font=/f.otf",
                "--run-dir=/run/hinteros",
                "--name=-Sam [2J",
            ]
        );
        // What the painter parses back.
        let cli = crate::config::Cli::try_parse_from(
            std::iter::once("hinteros-system".to_owned()).chain(a),
        )
        .unwrap();
        let Some(crate::config::Command::SplashPaint(p)) = cli.command else {
            panic!("not splash-paint");
        };
        assert_eq!(p.name.as_deref(), Some("-Sam [2J"));
        assert_eq!((p.color, p.guest), (None, false));
        let g = painter_args(
            Path::new("/dev/tty1"),
            Path::new("/f.otf"),
            Path::new("/run/hinteros"),
            Some(&Identity::guest()),
        );
        assert_eq!(g[4..], ["--name=Guest", "--guest"]);
        let t = painter_args(
            Path::new("/dev/tty1"),
            Path::new("/f.otf"),
            Path::new("/r"),
            None,
        );
        assert_eq!(t.len(), 4);
    }

    #[test]
    fn frame_placement() {
        let var = vtio::FbVar {
            xres: 1920,
            yres: 1080,
            yoffset: 1080,
            ..Default::default()
        };
        let fix = vtio::FbFix {
            line_length: 7680,
            smem_len: 7680 * 2160,
            ..Default::default()
        };
        assert_eq!(frame_place(&var, &fix), Some((7680 * 1080, 7680 * 1080)));
        // Past the end of the memory, or a stride too short for the width.
        let small = vtio::FbFix {
            smem_len: 7680 * 2000,
            ..fix
        };
        assert_eq!(frame_place(&var, &small), None);
        let narrow = vtio::FbFix {
            line_length: 4000,
            ..fix
        };
        assert_eq!(frame_place(&var, &narrow), None);
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
        let who = |user: &str, now: i64| identity_for(&state, &profiles, &public, user, now);
        let name = |user: &str, now: i64| who(user, now).map(|i| i.name);
        let james = who("hinteros", 0).unwrap();
        assert_eq!(
            (james.name.as_str(), james.color.as_deref(), james.guest),
            ("James", Some("teal"), false)
        );
        assert_eq!(who("hinteros-guest", 0), Some(Identity::guest()));
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
        assert_eq!(who("hinteros-greeter", 50), Some(james));
        assert_eq!(name("hinteros-greeter", 150), None, "expired");
        std::fs::write(&public, view("guest", true)).unwrap();
        assert_eq!(who("hinteros-greeter", 50), Some(Identity::guest()));
        std::fs::write(&public, view("p1", false)).unwrap();
        assert_eq!(name("hinteros-greeter", 50), None, "the picker will ask");
    }

    #[test]
    fn refuses_what_is_not_a_vt_or_a_framebuffer() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("tty1");
        std::fs::write(&f, "").unwrap();
        assert!(open_vt(&f).is_err());
        assert!(Fb::open(&f).is_err());
        let l = d.path().join("link");
        std::os::unix::fs::symlink("/dev/null", &l).unwrap();
        assert!(open_vt(&l).is_err());
        assert!(Fb::open(&l).is_err());
        assert!(open_vt(Path::new("/dev/null")).is_err());
        assert!(Fb::open(Path::new("/dev/null")).is_err());
    }

    #[test]
    fn the_painter_refuses_a_bad_tty_without_touching_anything() {
        let d = tempfile::tempdir().unwrap();
        let a = PaintArgs {
            tty: "/dev/pts/0".into(),
            font: d.path().join("none.otf"),
            name: None,
            color: None,
            guest: false,
            run_dir: d.path().to_owned(),
        };
        assert!(paint(&a, Instant::now()).is_err());
        assert!(!d.path().join(PAINTER_FILE).exists());
    }
}
