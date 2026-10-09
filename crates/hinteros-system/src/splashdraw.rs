//! The framebuffer splash's picture ([`crate::splash`]): the launcher's "Loading Sam…" page
//! (hinteros-launcher `ProfileLoadingPage`) without its spinner, drawn anti-aliased in software.
//!
//! At 1920x1080 it is the launcher's page pixel for pixel in position and size: a column centred
//! on the screen of the profile's avatar (a 200 px circle in the profile's colour with its
//! initial, bold, 0.48 of the circle), 32 px, the headline (Atkinson Hyperlegible Bold 56 px,
//! line height 1.25), 32 px and the spinner's 56 px (left empty). Other sizes scale with the
//! screen (the smaller of height / 1080 and width / 1920). "HinterOS" (the greeter) is the
//! headline alone, centred. A line wider than the screen less a 64 px margin each side is drawn
//! smaller until it fits.
//!
//! The text is rasterised from the font's outlines with coverage-based anti-aliasing
//! (`ab_glyph`); circles get one pixel of anti-aliased edge. Colours blend in sRGB, as Flutter
//! draws them.

use ab_glyph::{Font, FontRef, GlyphId, PxScale, ScaleFont, point};

use crate::vtio::{FbFix, FbVar};

/// The launcher's standard theme (hinteros-launcher lib/src/theme/hinter_theme.dart).
pub const BACKGROUND: Rgb = Rgb(0x0E, 0x11, 0x18);
pub const TEXT: Rgb = Rgb(0xF4, 0xF6, 0xFB);
/// `surfaceHigh`: the guest's avatar.
pub const SURFACE_HIGH: Rgb = Rgb(0x2A, 0x31, 0x47);
/// `accent`: the loading spinner.
pub const ACCENT: Rgb = Rgb(0x7A, 0xA2, 0xFF);

/// The launcher's page at 1080 px of height (logical pixels at device pixel ratio 1).
const AVATAR: f32 = 200.0;
const GAP: f32 = 32.0;
const HEADLINE: f32 = 56.0;
const LINE_HEIGHT: f32 = 1.25;
const SPINNER: f32 = 56.0;
/// The spinner's stroke (`CircularProgressIndicator(strokeWidth: 6)`).
const SPINNER_STROKE: f32 = 6.0;
/// How much of the circle the spinner's arc covers, and how far it turns per painted frame.
const SPINNER_SWEEP: f32 = std::f32::consts::PI * 1.5;
pub const SPINNER_STEP: f32 = std::f32::consts::PI / 8.0;
const MARGIN: f32 = 64.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// Flutter's `Color.computeLuminance` (relative luminance of the sRGB colour).
    fn luminance(self) -> f32 {
        let lin = |c: u8| {
            let c = c as f32 / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(self.0) + 0.7152 * lin(self.1) + 0.0722 * lin(self.2)
    }

    fn blend(self, over: Rgb, alpha: f32) -> Rgb {
        let a = alpha.clamp(0.0, 1.0);
        let mix = |b: u8, f: u8| (b as f32 + (f as f32 - b as f32) * a).round() as u8;
        Rgb(
            mix(self.0, over.0),
            mix(self.1, over.1),
            mix(self.2, over.2),
        )
    }
}

/// A profile colour (profiles.rs `COLORS`) as the launcher draws it (`profileColor`).
pub fn profile_color(name: &str) -> Option<Rgb> {
    Some(match name {
        "teal" => Rgb(0x2B, 0xB3, 0xA3),
        "blue" => Rgb(0x4C, 0x8D, 0xF6),
        "purple" => Rgb(0x9B, 0x6B, 0xF2),
        "pink" => Rgb(0xE8, 0x6F, 0xB3),
        "red" => Rgb(0xE5, 0x53, 0x4B),
        "orange" => Rgb(0xF0, 0x8C, 0x3A),
        "yellow" => Rgb(0xE8, 0xC5, 0x47),
        "green" => Rgb(0x5D, 0xBB, 0x63),
        _ => return None,
    })
}

/// The initial's colour on a profile colour (the launcher's `_onColor`).
fn ink_on(fill: Rgb) -> Rgb {
    if fill.luminance() > 0.35 {
        Rgb(0x10, 0x14, 0x18)
    } else {
        Rgb(0xFF, 0xFF, 0xFF)
    }
}

/// The avatar above the text.
#[derive(Debug, Clone, PartialEq)]
pub struct Avatar {
    /// The (upper-case) initial; None: a plain circle (the font has no glyph for it).
    pub initial: Option<String>,
    pub fill: Rgb,
    pub ink: Rgb,
    /// The guest's ring (no colour of its own).
    pub ring: Option<Rgb>,
}

impl Avatar {
    /// A profile's avatar (an unknown colour: the launcher's no-colour look).
    pub fn profile(initial: Option<String>, color: Option<&str>) -> Avatar {
        match color.and_then(profile_color) {
            Some(fill) => Avatar {
                initial,
                fill,
                ink: ink_on(fill),
                ring: None,
            },
            None => Avatar {
                initial,
                fill: SURFACE_HIGH,
                ink: TEXT,
                ring: None,
            },
        }
    }

    pub fn guest(initial: Option<String>) -> Avatar {
        Avatar {
            initial,
            fill: SURFACE_HIGH,
            ink: TEXT,
            ring: Some(TEXT),
        }
    }
}

/// What the splash shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Scene {
    pub text: String,
    pub avatar: Option<Avatar>,
}

/// A line's size at a font size (px per em).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextMetrics {
    pub width: f32,
    /// Above and below the baseline (both positive).
    pub ascent: f32,
    pub descent: f32,
}

/// Where everything goes on a `w` x `h` screen.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// Scale relative to the launcher's 1080p page.
    pub unit: f32,
    /// Font size of the line (px per em).
    pub text_px: f32,
    /// The line's box: left edge, top, width, height; and its baseline.
    pub text_left: f32,
    pub line_top: f32,
    pub text_width: f32,
    pub line_height: f32,
    pub baseline: f32,
    /// Avatar centre and radius.
    pub avatar: Option<(f32, f32, f32)>,
    /// The spinner below the line (with an avatar, as on the launcher's loading page): its
    /// square's left and top edge and side, in whole pixels, and the stroke width.
    pub spinner: Option<(u32, u32, u32, f32)>,
}

/// Lay out a line measured by `measure` (its metrics at a font size), with or without an avatar.
pub fn layout(w: u32, h: u32, avatar: bool, measure: impl Fn(f32) -> TextMetrics) -> Layout {
    let (wf, hf) = (w as f32, h as f32);
    let unit = (hf / 1080.0).min(wf / 1920.0).max(0.25);
    let mut text_px = HEADLINE * unit;
    let mut m = measure(text_px);
    let room = wf - 2.0 * MARGIN * unit;
    if m.width > room && m.width > 0.0 {
        text_px = (text_px * room / m.width).max(8.0);
        m = measure(text_px);
    }
    let line_height = LINE_HEIGHT * text_px;
    let column = if avatar {
        (AVATAR + GAP + GAP + SPINNER) * unit + line_height
    } else {
        line_height
    };
    let top = (hf - column) / 2.0;
    let line_top = if avatar {
        top + (AVATAR + GAP) * unit
    } else {
        top
    };
    // Flutter's proportional leading: the line box's extra height splits like ascent:descent.
    let em = (m.ascent + m.descent).max(f32::EPSILON);
    let baseline = line_top + line_height * m.ascent / em;
    Layout {
        unit,
        text_px,
        text_left: (wf - m.width) / 2.0,
        line_top,
        text_width: m.width,
        line_height,
        baseline,
        avatar: avatar.then(|| (wf / 2.0, top + AVATAR * unit / 2.0, AVATAR * unit / 2.0)),
        spinner: avatar
            .then(|| {
                let side = (SPINNER * unit).round().max(4.0);
                let left = ((wf - side) / 2.0).round();
                let top = (line_top + line_height + GAP * unit).round();
                (left >= 0.0 && top + side <= hf).then_some((
                    left as u32,
                    top as u32,
                    side as u32,
                    SPINNER_STROKE * unit,
                ))
            })
            .flatten(),
    }
}

/// An RGB picture.
#[derive(Debug, Clone)]
pub struct Canvas {
    pub w: u32,
    pub h: u32,
    pub px: Vec<Rgb>,
}

impl Canvas {
    pub fn new(w: u32, h: u32, bg: Rgb) -> Canvas {
        Canvas {
            w,
            h,
            px: vec![bg; w as usize * h as usize],
        }
    }

    pub fn get(&self, x: u32, y: u32) -> Rgb {
        self.px[y as usize * self.w as usize + x as usize]
    }

    fn blend(&mut self, x: i64, y: i64, c: Rgb, alpha: f32) {
        if alpha <= 0.0 || x < 0 || y < 0 || x >= self.w as i64 || y >= self.h as i64 {
            return;
        }
        let i = y as usize * self.w as usize + x as usize;
        self.px[i] = self.px[i].blend(c, alpha);
    }

    /// An arc of the circle of radius `r` (the stroke's middle) around (`cx`, `cy`), `width`
    /// wide, from angle `start` clockwise (y down) over `sweep` radians, butt ends.
    fn arc(&mut self, (cx, cy): (f32, f32), r: f32, width: f32, start: f32, sweep: f32, c: Rgb) {
        use std::f32::consts::TAU;
        let reach = r + width / 2.0 + 1.0;
        let (x0, x1) = ((cx - reach).floor() as i64, (cx + reach).ceil() as i64);
        let (y0, y1) = ((cy - reach).floor() as i64, (cy + reach).ceil() as i64);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                let d = (dx * dx + dy * dy).sqrt();
                let radial = (width / 2.0 - (d - r).abs() + 0.5).clamp(0.0, 1.0);
                if radial <= 0.0 {
                    continue;
                }
                let a = (dy.atan2(dx) - start).rem_euclid(TAU);
                // Anti-aliased ends: a pixel's share of the angle, at its distance.
                let px_angle = 1.0 / d.max(1.0);
                let inside = ((sweep - a) / px_angle + 0.5).clamp(0.0, 1.0)
                    * ((a + px_angle / 2.0) / px_angle).clamp(0.0, 1.0);
                self.blend(x, y, c, radial * inside);
            }
        }
    }

    /// A disc of radius `r` in `fill`, with a ring of width `ring.1` inside its edge.
    fn disc(&mut self, cx: f32, cy: f32, r: f32, fill: Rgb, ring: Option<(Rgb, f32)>) {
        let x0 = (cx - r - 1.0).floor() as i64;
        let x1 = (cx + r + 1.0).ceil() as i64;
        let y0 = (cy - r - 1.0).floor() as i64;
        let y1 = (cy + r + 1.0).ceil() as i64;
        for y in y0..=y1 {
            for x in x0..=x1 {
                let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                let outer = (r - d + 0.5).clamp(0.0, 1.0);
                self.blend(x, y, fill, outer);
                if let Some((c, width)) = ring {
                    let inner = (r - width - d + 0.5).clamp(0.0, 1.0);
                    self.blend(x, y, c, outer - inner);
                }
            }
        }
    }
}

/// The font the splash draws with.
pub struct Typeface<'a> {
    font: FontRef<'a>,
}

impl<'a> Typeface<'a> {
    pub fn parse(bytes: &'a [u8]) -> Option<Typeface<'a>> {
        let font = FontRef::try_from_slice(bytes).ok()?;
        font.units_per_em()?;
        Some(Typeface { font })
    }

    /// Whether every character of `text` (but spaces) has a glyph.
    pub fn covers(&self, text: &str) -> bool {
        text.chars()
            .all(|c| c == ' ' || self.font.glyph_id(c) != GlyphId(0))
    }

    /// `ab_glyph`'s scale (ascent - descent in px) for a size in px per em.
    fn scale(&self, px: f32) -> PxScale {
        let upem = self.font.units_per_em().unwrap_or(1000.0);
        PxScale::from(px * self.font.height_unscaled() / upem)
    }

    pub fn measure(&self, text: &str, px: f32) -> TextMetrics {
        let f = self.font.as_scaled(self.scale(px));
        let mut width = 0.0;
        let mut prev: Option<GlyphId> = None;
        for c in text.chars() {
            let id = f.glyph_id(c);
            if let Some(p) = prev {
                width += f.kern(p, id);
            }
            width += f.h_advance(id);
            prev = Some(id);
        }
        TextMetrics {
            width,
            ascent: f.ascent(),
            descent: -f.descent(),
        }
    }

    /// Draw `text` at `left` on `baseline` in `color`.
    fn draw(&self, canvas: &mut Canvas, text: &str, px: f32, left: f32, baseline: f32, color: Rgb) {
        let scale = self.scale(px);
        let f = self.font.as_scaled(scale);
        let mut x = left;
        let mut prev: Option<GlyphId> = None;
        for c in text.chars() {
            let id = f.glyph_id(c);
            if let Some(p) = prev {
                x += f.kern(p, id);
            }
            let glyph = id.with_scale_and_position(scale, point(x, baseline));
            if let Some(o) = self.font.outline_glyph(glyph) {
                let b = o.px_bounds();
                o.draw(|gx, gy, cov| {
                    canvas.blend(
                        b.min.x as i64 + gx as i64,
                        b.min.y as i64 + gy as i64,
                        color,
                        cov,
                    );
                });
            }
            x += f.h_advance(id);
            prev = Some(id);
        }
    }
}

/// Paint `scene` on a `w` x `h` canvas.
pub fn render(face: &Typeface<'_>, scene: &Scene, w: u32, h: u32) -> Canvas {
    let mut canvas = Canvas::new(w, h, BACKGROUND);
    let l = scene_layout(face, scene, w, h);
    if let (Some(a), Some((cx, cy, r))) = (&scene.avatar, l.avatar) {
        let ring = a.ring.map(|c| (c, (2.0 * r) * 0.03 + l.unit));
        canvas.disc(cx, cy, r, a.fill, ring);
        if let Some(initial) = &a.initial {
            // The launcher's initial: bold, 0.48 of the circle, line height 1, centred.
            let px = 2.0 * r * 0.48;
            let m = face.measure(initial, px);
            let em = (m.ascent + m.descent).max(f32::EPSILON);
            let baseline = cy - px / 2.0 + px * m.ascent / em;
            face.draw(
                &mut canvas,
                initial,
                px,
                cx - m.width / 2.0,
                baseline,
                a.ink,
            );
        }
    }
    face.draw(
        &mut canvas,
        &scene.text,
        l.text_px,
        l.text_left,
        l.baseline,
        TEXT,
    );
    if let Some((left, top, spinner)) = render_spinner(&l, 0.0) {
        for y in 0..spinner.h {
            for x in 0..spinner.w {
                let i = (top + y) as usize * canvas.w as usize + (left + x) as usize;
                canvas.px[i] = spinner.get(x, y);
            }
        }
    }
    canvas
}

/// The layout [`render`] uses for `scene` on a `w` x `h` screen.
pub fn scene_layout(face: &Typeface<'_>, scene: &Scene, w: u32, h: u32) -> Layout {
    layout(w, h, scene.avatar.is_some(), |px| {
        face.measure(&scene.text, px)
    })
}

/// The spinner's square of `layout` (see [`Layout::spinner`]) with the arc turned `turn`
/// radians: what a painter writes over the full picture each frame. `None` without a spinner.
pub fn render_spinner(layout: &Layout, turn: f32) -> Option<(u32, u32, Canvas)> {
    let (left, top, side, stroke) = layout.spinner?;
    let mut canvas = Canvas::new(side, side, BACKGROUND);
    let mid = side as f32 / 2.0;
    canvas.arc(
        (mid, mid),
        mid - stroke / 2.0,
        stroke,
        turn - std::f32::consts::FRAC_PI_2,
        SPINNER_SWEEP,
        ACCENT,
    );
    Some((left, top, canvas))
}

/// A 32-bit truecolour framebuffer's pixel layout: the bit offsets of red, green, blue and, if
/// it has one, alpha (each 8 bits wide).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelFormat {
    pub red: u32,
    pub green: u32,
    pub blue: u32,
    pub alpha: Option<u32>,
}

const FB_TYPE_PACKED_PIXELS: u32 = 0;
const FB_VISUAL_TRUECOLOR: u32 = 2;

impl PixelFormat {
    /// XRGB8888 (what DRM's fbdev emulation offers).
    pub const XRGB: PixelFormat = PixelFormat {
        red: 16,
        green: 8,
        blue: 0,
        alpha: None,
    };

    /// The framebuffer's format if the splash can draw on it: 32 bits per pixel, packed
    /// truecolour, 8-bit channels at distinct byte offsets. Anything else: None.
    pub fn from_fb(var: &FbVar, fix: &FbFix) -> Option<PixelFormat> {
        if var.bits_per_pixel != 32
            || fix.type_ != FB_TYPE_PACKED_PIXELS
            || fix.visual != FB_VISUAL_TRUECOLOR
            || var.grayscale != 0
            || var.nonstd != 0
        {
            return None;
        }
        let chan = |b: &crate::vtio::FbBitfield| {
            (b.length == 8 && b.offset.is_multiple_of(8) && b.offset < 32 && b.msb_right == 0)
                .then_some(b.offset)
        };
        let (red, green, blue) = (chan(&var.red)?, chan(&var.green)?, chan(&var.blue)?);
        let alpha = match var.transp.length {
            0 => None,
            _ => Some(chan(&var.transp)?),
        };
        let mut offsets = vec![red, green, blue];
        offsets.extend(alpha);
        offsets.sort_unstable();
        offsets.dedup();
        (offsets.len() == 3 + alpha.is_some() as usize).then_some(PixelFormat {
            red,
            green,
            blue,
            alpha,
        })
    }

    /// One pixel as the framebuffer stores it (native byte order), alpha opaque.
    pub fn pack(&self, c: Rgb) -> [u8; 4] {
        let mut v =
            (c.0 as u32) << self.red | (c.1 as u32) << self.green | (c.2 as u32) << self.blue;
        if let Some(a) = self.alpha {
            v |= 0xFF << a;
        }
        v.to_ne_bytes()
    }
}

/// The framebuffer's visible rows (`stride` bytes each) with `canvas` at column `xoffset`; the
/// rest of each row (panning, padding) in the background colour.
pub fn to_fb_bytes(canvas: &Canvas, fmt: PixelFormat, stride: usize, xoffset: usize) -> Vec<u8> {
    let bg = fmt.pack(BACKGROUND);
    let mut out = Vec::with_capacity(stride * canvas.h as usize);
    for y in 0..canvas.h {
        let row_start = out.len();
        for _ in 0..xoffset {
            out.extend_from_slice(&bg);
        }
        for x in 0..canvas.w {
            out.extend_from_slice(&fmt.pack(canvas.get(x, y)));
        }
        while out.len() < row_start + stride {
            out.extend_from_slice(&bg);
        }
        out.truncate(row_start + stride);
    }
    out
}

/// `canvas` as framebuffer rows of `canvas.w` pixels each (no stride padding), for writing a
/// rectangle into a framebuffer row by row.
pub fn to_fb_rows(canvas: &Canvas, fmt: PixelFormat) -> Vec<Vec<u8>> {
    (0..canvas.h)
        .map(|y| {
            (0..canvas.w)
                .flat_map(|x| fmt.pack(canvas.get(x, y)))
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The launcher's font (assets/fonts, OFL-1.1; the box uses nixpkgs' identical file).
    const FONT: &[u8] = include_bytes!("../fixtures/AtkinsonHyperlegible-Bold.otf");

    /// A monospaced stand-in: 0.5 em per character, ascent 0.8 em, descent 0.2 em.
    fn mono(text: &str) -> impl Fn(f32) -> TextMetrics + '_ {
        move |px| TextMetrics {
            width: 0.5 * px * text.chars().count() as f32,
            ascent: 0.8 * px,
            descent: 0.2 * px,
        }
    }

    #[test]
    fn the_spinner_sits_below_the_line_and_turns() {
        let l = layout(1920, 1080, true, mono("Loading Sam…"));
        let (left, top, side, stroke) = l.spinner.unwrap();
        assert_eq!(side, 56);
        assert_eq!(stroke, 6.0);
        assert_eq!(left, (1920 - 56) / 2);
        assert!(top as f32 >= l.line_top + l.line_height, "below the line");
        assert!(
            layout(1920, 1080, false, mono("HinterOS"))
                .spinner
                .is_none()
        );

        // At turn 0 the arc starts at the top and runs clockwise for three quarters: the top,
        // right and bottom of the ring are drawn, the upper-left quarter is not.
        let (_, _, sq) = render_spinner(&l, 0.0).unwrap();
        let ring = |x: f32, y: f32| sq.get(x as u32, y as u32);
        let (mid, r) = (28.0, 28.0 - 3.0);
        assert_eq!(ring(mid + 2.0, mid - r), ACCENT, "top");
        assert_eq!(ring(mid + r, mid), ACCENT, "right");
        assert_eq!(ring(mid, mid + r), ACCENT, "bottom");
        let d = r / 2f32.sqrt();
        assert_eq!(ring(mid - d, mid - d), BACKGROUND, "upper left");
        assert_eq!(ring(mid, mid), BACKGROUND, "centre");
        // A quarter turn later the gap is in the upper right.
        let (_, _, sq) = render_spinner(&l, std::f32::consts::FRAC_PI_2).unwrap();
        assert_eq!(sq.get((mid + d) as u32, (mid - d) as u32), BACKGROUND);
        assert_eq!(sq.get((mid - d) as u32, (mid - d) as u32), ACCENT);
    }

    #[test]
    fn the_launchers_page_at_1080p_scaled_at_720p() {
        // 1920x1080: unit 1, 56 px text; column 200 + 32 + 70 + 32 + 56 = 390 tall, from 345.
        let l = layout(1920, 1080, true, mono("Loading Sam…"));
        assert_eq!((l.unit, l.text_px, l.line_height), (1.0, 56.0, 70.0));
        assert_eq!(l.avatar, Some((960.0, 445.0, 100.0)));
        assert_eq!(l.line_top, 345.0 + 232.0);
        assert_eq!(l.baseline, 577.0 + 56.0);
        // Centred: as far from the left as from the right.
        assert!((l.text_left - (1920.0 - l.text_left - l.text_width)).abs() < 0.01);
        // 1280x720: everything two thirds.
        let s = layout(1280, 720, true, mono("Loading Sam…"));
        assert!((s.unit - 2.0 / 3.0).abs() < 1e-6);
        assert!((s.text_px - 56.0 * 2.0 / 3.0).abs() < 1e-4);
        let (cx, cy, r) = s.avatar.unwrap();
        assert!((cx - 640.0).abs() < 1e-4 && (r - 200.0 / 3.0).abs() < 1e-4);
        assert!((cy - 445.0 * 2.0 / 3.0).abs() < 1e-3, "{cy}");
        assert!((s.text_left - (1280.0 - s.text_left - s.text_width)).abs() < 0.01);
        // Text between 4 and 6 % of the screen's height at both sizes, and at 4K.
        for (w, h) in [(1920, 1080), (1280, 720), (3840, 2160)] {
            let l = layout(w, h, false, mono("HinterOS"));
            let share = l.text_px / h as f32;
            assert!((0.04..0.06).contains(&share), "{w}x{h}: {share}");
            // Without an avatar the line is centred vertically.
            let below = h as f32 - (l.line_top + l.line_height);
            assert!((l.line_top - below).abs() < 0.01, "{w}x{h}: {l:?}");
        }
        // A narrow screen: the smaller ratio wins (1080x1920 portrait).
        assert!((layout(1080, 1920, false, mono("x")).unit - 0.5625).abs() < 1e-6);
    }

    #[test]
    fn a_long_line_shrinks_to_fit() {
        // 80 characters of half an em: 1493 px at 37 px, on a 1280 px screen.
        let text = &"W".repeat(80);
        let l = layout(1280, 720, true, mono(text));
        assert!(l.text_px < 56.0 * 2.0 / 3.0);
        assert!(l.text_left >= 64.0 * 2.0 / 3.0 - 0.01, "{l:?}");
        assert!((l.text_left - (1280.0 - l.text_left - l.text_width)).abs() < 0.01);
    }

    #[test]
    fn layout_with_the_real_font() {
        let face = Typeface::parse(FONT).unwrap();
        assert!(face.covers("Loading Zoë…"));
        assert!(!face.covers("山田"));
        for (w, h) in [(1920, 1080), (1280, 720)] {
            let l = layout(w, h, true, |px| face.measure("Loading Sam…", px));
            assert!(l.text_width > 6.0 * l.text_px, "{l:?}");
            let right = w as f32 - l.text_left - l.text_width;
            assert!((l.text_left - right).abs() < 0.01, "{w}x{h}: {l:?}");
            assert!(l.baseline > l.line_top && l.baseline < l.line_top + l.line_height);
        }
    }

    #[test]
    fn pixel_formats() {
        let xrgb = PixelFormat::XRGB;
        let c = Rgb(0x11, 0x22, 0x33);
        // Little endian XRGB8888: B, G, R, X.
        assert_eq!(u32::from_ne_bytes(xrgb.pack(c)), 0x0011_2233);
        let argb = PixelFormat {
            alpha: Some(24),
            ..xrgb
        };
        assert_eq!(u32::from_ne_bytes(argb.pack(c)), 0xFF11_2233);
        let xbgr = PixelFormat {
            red: 0,
            green: 8,
            blue: 16,
            alpha: None,
        };
        assert_eq!(u32::from_ne_bytes(xbgr.pack(c)), 0x0033_2211);

        let bf = |offset, length| crate::vtio::FbBitfield {
            offset,
            length,
            msb_right: 0,
        };
        let mut var = FbVar {
            bits_per_pixel: 32,
            red: bf(16, 8),
            green: bf(8, 8),
            blue: bf(0, 8),
            ..Default::default()
        };
        let fix = FbFix {
            visual: FB_VISUAL_TRUECOLOR,
            ..Default::default()
        };
        assert_eq!(PixelFormat::from_fb(&var, &fix), Some(xrgb));
        var.transp = bf(24, 8);
        assert_eq!(PixelFormat::from_fb(&var, &fix), Some(argb));
        // Not drawable: 16 bpp, a palette, overlapping or odd channels.
        let mut v16 = var;
        v16.bits_per_pixel = 16;
        assert_eq!(PixelFormat::from_fb(&v16, &fix), None);
        let pseudo = FbFix { visual: 3, ..fix };
        assert_eq!(PixelFormat::from_fb(&var, &pseudo), None);
        let mut same = var;
        same.green = bf(16, 8);
        assert_eq!(PixelFormat::from_fb(&same, &fix), None);
        let mut ten = var;
        ten.red = bf(20, 10);
        assert_eq!(PixelFormat::from_fb(&ten, &fix), None);
    }

    #[test]
    fn frame_bytes_follow_the_stride() {
        let mut c = Canvas::new(2, 2, Rgb(1, 2, 3));
        c.px[3] = Rgb(9, 9, 9);
        let out = to_fb_bytes(&c, PixelFormat::XRGB, 16, 1);
        assert_eq!(out.len(), 32);
        let px = |i: usize| u32::from_ne_bytes(out[i * 4..i * 4 + 4].try_into().unwrap());
        let bg = u32::from_ne_bytes(PixelFormat::XRGB.pack(BACKGROUND));
        assert_eq!([px(0), px(1), px(2), px(3)], [bg, 0x010203, 0x010203, bg]);
        assert_eq!(px(6), 0x090909);
    }

    #[test]
    fn renders_anti_aliased_text_and_avatar() {
        let face = Typeface::parse(FONT).unwrap();
        let scene = Scene {
            text: "Loading Sam…".into(),
            avatar: Some(Avatar::profile(Some("S".into()), Some("orange"))),
        };
        let (w, h) = (640, 360);
        let c = render(&face, &scene, w, h);
        let l = layout(w, h, true, |px| face.measure(&scene.text, px));
        // In the text's box: pure text colour, pure background and many shades between.
        let (mut full, mut between) = (0, 0);
        for y in l.line_top as u32..(l.line_top + l.line_height) as u32 {
            for x in l.text_left as u32..(l.text_left + l.text_width) as u32 {
                let p = c.get(x, y);
                if p == TEXT {
                    full += 1;
                } else if p != BACKGROUND {
                    between += 1;
                    // On the line from background to text colour.
                    assert!(p.0 >= BACKGROUND.0 && p.0 <= TEXT.0, "{p:?}");
                }
            }
        }
        assert!(full > 50 && between > 50, "full {full}, between {between}");
        let shades: std::collections::BTreeSet<u8> = c.px.iter().map(|p| p.0).collect();
        assert!(shades.len() > 20, "not 1-bit: {} shades", shades.len());
        // Nothing outside the text and the avatar: the corners are background.
        assert_eq!(c.get(0, 0), BACKGROUND);
        assert_eq!(c.get(w - 1, h - 1), BACKGROUND);
        // The avatar: orange around its centre's left, the dark ink of the initial on orange
        // somewhere, an anti-aliased rim.
        let (cx, cy, r) = l.avatar.unwrap();
        let orange = profile_color("orange").unwrap();
        assert_eq!(c.get((cx - r * 0.8) as u32, cy as u32), orange);
        let ink = ink_on(orange);
        assert_eq!(ink, Rgb(0x10, 0x14, 0x18), "orange is light: dark initial");
        let disc = |p: &Rgb| *p == ink;
        assert!(c.px.iter().any(disc));
        let rim = c.get((cx + r) as u32, cy as u32);
        assert!(rim != orange && rim != BACKGROUND, "{rim:?}");
    }

    #[test]
    fn the_guest_and_unknown_colours() {
        let g = Avatar::guest(Some("G".into()));
        assert_eq!((g.fill, g.ink, g.ring), (SURFACE_HIGH, TEXT, Some(TEXT)));
        let p = Avatar::profile(None, Some("no-such-colour"));
        assert_eq!((p.fill, p.ring), (SURFACE_HIGH, None));
        // Dark profile colours get a white initial.
        assert_eq!(ink_on(profile_color("blue").unwrap()), Rgb(255, 255, 255));
        for c in crate::profiles::COLORS {
            assert!(profile_color(c).is_some(), "{c}");
        }
        let face = Typeface::parse(FONT).unwrap();
        let c = render(
            &face,
            &Scene {
                text: "Loading Guest…".into(),
                avatar: Some(g),
            },
            320,
            180,
        );
        assert!(c.px.contains(&SURFACE_HIGH));
    }
}
