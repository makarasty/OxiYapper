//! Overlay pill, tray icons and the app icon as premultiplied RGBA pixmaps (`tiny-skia`,
//! `ab_glyph`). The app's mark is drawn here once: tray, exe icon and settings logo use it.
//!
//! Sizes, colours, labels and icons come from [`oxi_core::overlay`]: the pill size is the
//! [`place`](oxi_core::overlay::place) rectangle, already in the monitor's pixels, and every
//! inner distance is a share of its height, so 100, 150 and 200 % DPI draw the same picture at
//! full resolution. Pill edges are area-antialiased fills: a 1 px stroke goes through
//! tiny-skia's hairline path, which steps visibly on a curve at 100 %.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use oxi_core::machine::{OverlayState, Phase};
use oxi_core::overlay::palette::{EDGE, SURFACE, TEXT};
use oxi_core::overlay::{
    CARD_RADIUS, Icon, Meter, PILL_BAR_GAP as BAR_GAP, PILL_BAR_MAX as BAR_MAX,
    PILL_BAR_W as BAR_W, PILL_EM as EM, PILL_EM_MIN as EM_MIN, PILL_GAP as GAP, PILL_H_DIP,
    PILL_ICON as ICON, PILL_METER_MIN as METER_MIN, PILL_PAD as PAD, PILL_TRACK, PILL_TRACK_BOTTOM,
    RecClock, Rect, Rgba, TOO_LOUD, label_variants, label_x, look_with, palette, pill_accent,
    pill_label, pill_rim, progress_span, tray_colour,
};
use oxi_core::tray::TrayBadge;
use oxi_core::types::Millis;
use tiny_skia::{
    BlendMode, Color, FillRule, LineCap, LineJoin, Mask, Paint, Path, PathBuilder, PathSegment,
    Pixmap, Stroke, Transform,
};

/// The overlay pill for `state`, sized `rect.w` x `rect.h` (the [`place`] result).
/// `now` decides the clipping colour of the meter. Without a font the pill shows its icon and
/// meter only. `None` when the rectangle is empty. The one-off form of [`PillCanvas`].
///
/// [`place`]: oxi_core::overlay::place
#[must_use]
pub fn pill(
    state: OverlayState,
    meter: &Meter,
    now: Millis,
    rect: Rect,
    font: Option<&FontRef<'_>>,
) -> Option<Pixmap> {
    let mut canvas = PillCanvas::new(font.cloned());
    canvas.draw(state, meter, now, rect)?;
    canvas.cache.map(|c| c.frame)
}

/// The pill redrawn in place for the 30 Hz meter. The surface, icon, label and bar shapes are
/// drawn when the state, the size or the label text changes; another frame copies them and adds
/// the newest bars or the progress fill, so a recording frame allocates nothing.
pub struct PillCanvas<'f> {
    font: Option<FontRef<'f>>,
    cache: Option<Cache>,
    /// Whisper's percent of the transcription; `None` shows the live indicator instead.
    progress: Option<u8>,
    clock: RecClock,
    /// The label of the frame being drawn, reused.
    text: String,
    /// The user's accent on the pill ([`pill_accent`]): live work and the wave.
    accent: Rgba,
}

struct Cache {
    state: OverlayState,
    label: String,
    /// The pill without its bars and progress fill.
    base: Pixmap,
    frame: Pixmap,
    bars: Option<Bars>,
    track: Option<Track>,
}

/// The 1 px rim of the coloured tray shapes on a light taskbar (a dark red; white on a dark one).
const TRAY_RIM: Rgba = Rgba(0x8F, 0x1D, 0x22, 0xFF);
/// The record dot of the exe icon and its files: fixed, a file cannot follow a setting (DS-20.8).
const ICON_DOT: Rgba = Rgba(0xE5, 0x48, 0x4D, 0xFF);
/// The record dot of the in-app mark and tray recording state before a user accent: Red (DS-20).
const DEFAULT_DOT: Rgba = Rgba(0xD9, 0x3A, 0x3F, 0xFF);

impl<'f> PillCanvas<'f> {
    /// Without a font the pill shows its icon and meter only.
    #[must_use]
    pub fn new(font: Option<FontRef<'f>>) -> Self {
        Self {
            font,
            cache: None,
            progress: None,
            clock: RecClock::default(),
            text: String::new(),
            accent: pill_accent(oxi_core::overlay::PRESETS[0].seed),
        }
    }

    /// The accent of the next frames: the seed's [`pill_accent`]. A change redraws the pill.
    pub fn set_accent(&mut self, accent: Rgba) {
        if self.accent != accent {
            self.accent = accent;
            self.cache = None;
        }
    }

    /// Whisper's percent of the transcription for the next frames ("Transcribing 42%" and a
    /// filled track); `None` while it is unknown, which shows a sweeping indicator.
    pub fn set_progress(&mut self, percent: Option<u8>) {
        self.progress = percent;
    }

    /// The pill went away without a frame drawn for it: the next recording's clock starts
    /// at zero.
    pub fn hide(&mut self) {
        self.clock = RecClock::default();
    }

    /// The frame of [`pill`] for these arguments; `None` when the rectangle is empty.
    pub fn draw(
        &mut self,
        state: OverlayState,
        meter: &Meter,
        now: Millis,
        rect: Rect,
    ) -> Option<&Pixmap> {
        let size = (u32::try_from(rect.w).ok()?, u32::try_from(rect.h).ok()?);
        self.clock.see(state, now);
        pill_label(&mut self.text, state, self.progress, self.clock, now);
        // Overloaded: the label says why the wave changed (DS-21.3); a change is a new cache.
        if matches!(state, OverlayState::Recording { .. }) && meter.clipping(now) {
            self.text.clear();
            self.text.push_str(TOO_LOUD);
        }
        let sized = |c: &Cache| (c.base.width(), c.base.height()) == size;
        if self
            .cache
            .as_ref()
            .is_none_or(|c| c.state != state || !sized(c) || c.label != self.text)
        {
            let (mut base, frame, mut label) = match self.cache.take() {
                Some(c) if sized(&c) => (c.base, c.frame, c.label),
                old => (
                    Pixmap::new(size.0, size.1)?,
                    Pixmap::new(size.0, size.1)?,
                    old.map(|c| c.label).unwrap_or_default(),
                ),
            };
            base.fill(Color::TRANSPARENT);
            let (bars, track) = surface(
                &mut base,
                state,
                self.font.as_ref(),
                &self.text,
                self.accent,
            );
            label.clone_from(&self.text);
            self.cache = Some(Cache {
                state,
                label,
                base,
                frame,
                bars,
                track,
            });
        }
        let c = self.cache.as_mut()?;
        c.frame.data_mut().copy_from_slice(c.base.data());
        if let Some(bars) = &c.bars {
            bars.draw(&mut c.frame, meter, meter.clipping(now));
        }
        if let Some(track) = &c.track {
            track.draw(&mut c.frame, progress_span(self.progress, now));
        }
        Some(&c.frame)
    }
}

/// Everything of the pill for `state` but its bars and progress fill, into the clear `pm`, with
/// `label` as its text; the bars when the state has a meter, the progress track while loading
/// or transcribing.
fn surface(
    pm: &mut Pixmap,
    state: OverlayState,
    font: Option<&FontRef<'_>>,
    label: &str,
    accent: Rgba,
) -> (Option<Bars>, Option<Track>) {
    let look = look_with(state, accent);
    if state == OverlayState::Hidden {
        return (None, None);
    }
    let (w, h) = (pm.width() as f32, pm.height() as f32);
    // Pixels per DIP; under the monitor's scale only when a tiny work area shrank the pill.
    let dip = h / PILL_H_DIP as f32;
    let (radius, rim) = (CARD_RADIUS as f32 * dip, dip.round().max(1.0));
    fill(
        pm,
        rounded(0.0, 0.0, w, h, radius).as_ref(),
        SURFACE,
        Transform::identity(),
    );
    // The rim as the area between two outlines: an even-odd fill antialiases by area.
    let mut edge = PathBuilder::new();
    push_rounded(&mut edge, 0.0, 0.0, w, h, radius);
    push_rounded(
        &mut edge,
        rim,
        rim,
        w - 2.0 * rim,
        h - 2.0 * rim,
        radius - rim,
    );
    // A recording pill's rim is the accent halfway into the surface (DS-21.2).
    let rim_colour = if look.meter && look.accent == accent {
        pill_rim(accent)
    } else {
        EDGE
    };
    fill_eo(pm, edge.finish().as_ref(), rim_colour);

    let (pad, icon, gap) = ((PAD * h).round(), (ICON * h).round(), (GAP * h).round());
    let right = w - pad;
    let track = matches!(
        state,
        OverlayState::LoadingModel | OverlayState::Transcribing
    )
    .then(|| Track::new(pm, look.accent, pad, right, h));
    let mut text = font
        .filter(|_| !label.is_empty())
        .map(|f| Label::new(f, label, EM * h));
    // The icon starts at the left padding in every state, so it never jumps.
    let x0 = pad;
    let text_x = x0 + icon + gap;
    // The wave box starts after the widest text the label can become (the overload warning, the
    // clock), so a swap of the label never moves it (DS-21.3, DS-25).
    let slot = match font {
        Some(f) if look.meter => {
            let room = right - text_x - gap - METER_MIN * h;
            label_variants(state)
                .iter()
                .map(|s| Label::new(f, s, EM * h).width)
                .fold(0.0, f32::max)
                .min(room)
        }
        _ => 0.0,
    };
    if look.meter
        && let Some(t) = text.as_mut()
    {
        t.fit(slot);
    }
    let cy = h / 2.0;
    draw_icon(
        pm,
        look.icon,
        look.accent,
        x0,
        (cy - icon / 2.0).round(),
        icon,
    );
    let mut x = text_x;
    if let Some(t) = &text {
        // Beside a meter the label stays left; alone it is centred on the pill.
        let at = if look.meter {
            text_x
        } else {
            label_x(w, text_x, right, t.width)
        };
        t.draw(pm, at, cy, right);
        x = if look.meter {
            text_x + slot + gap
        } else {
            at + t.width + gap
        };
    }
    let bars = if look.meter {
        Bars::new(look.accent, x.round(), right, cy, h, dip)
    } else {
        None
    };
    (bars, track)
}

/// The progress track under the label: a faint rail drawn with the pill, its filled part added
/// per frame.
struct Track {
    x0: f32,
    len: f32,
    y: f32,
    thick: f32,
    accent: Rgba,
}

impl Track {
    /// The rail from `x0` to `x1` near the bottom of a pill `h` high, drawn into `pm`.
    fn new(pm: &mut Pixmap, accent: Rgba, x0: f32, x1: f32, h: f32) -> Self {
        let thick = (PILL_TRACK * h).round().max(1.0);
        let t = Self {
            x0,
            len: x1 - x0,
            y: h - (PILL_TRACK_BOTTOM * h).round() - thick,
            thick,
            accent,
        };
        t.bar(pm, (0.0, 1.0), EDGE);
        t
    }

    /// Fills the shares `a..b` of the track.
    fn draw(&self, pm: &mut Pixmap, span: (f32, f32)) {
        self.bar(pm, span, self.accent);
    }

    fn bar(&self, pm: &mut Pixmap, (from, to): (f32, f32), colour: Rgba) {
        let width = (to - from) * self.len;
        if width > 0.0 {
            let x = self.x0 + from * self.len;
            let path = rounded(x, self.y, width, self.thick, self.thick / 2.0);
            fill(pm, path.as_ref(), colour, Transform::identity());
        }
    }
}

/// The tray icon for `phase` at `size` px (16, 20, 24 or 32), drawn per size, never scaled.
/// Every state is the app mark, an "o" ring with a record dot at its top right, so the icon
/// reads as this app in any state: idle is the ring and dot, recording the ring filled red
/// around a white hole (a record button), transcribing (and the short paste after it) the ring
/// filled solid with three knocked-out dots. Shape changes with the state and only recording
/// has colour, so it reads on a light, dark or accent taskbar and without colour vision.
/// `light_taskbar` picks near-black over white glyphs. `badge` adds a dot in a knocked-out
/// ring. `None` for size 0.
#[must_use]
pub fn tray_icon(phase: Phase, size: u32, light_taskbar: bool, badge: TrayBadge) -> Option<Pixmap> {
    let mut pm = Pixmap::new(size, size)?;
    let n = size as f32;
    let k = n / 16.0;
    let m = Mark::tray(n);
    let ink = if light_taskbar {
        palette::INK
    } else {
        palette::WHITE
    };
    let id = Transform::identity();
    match phase {
        Phase::Idle => {
            fill_eo(&mut pm, m.ring().as_ref(), ink);
            fill(&mut pm, m.dot().as_ref(), ink, id);
        }
        Phase::Recording => {
            let red = tray_colour(DEFAULT_DOT, light_taskbar);
            fill(&mut pm, m.disc(0.0).as_ref(), red, id);
            fill(&mut pm, m.hole().as_ref(), palette::WHITE, id);
            // A 1 px rim on every taskbar: red alone is under 3:1 against accent taskbars
            // (#8764B8, #005FB8), and a dark or white edge holds on any colour.
            let rim = tray_rim(light_taskbar);
            stroke(&mut pm, m.disc(0.5).as_ref(), rim, 1.0, id);
            fill(&mut pm, m.dot().as_ref(), red, id);
            // The dot gets the same rim, outside its edge: inside it would leave a 1 px red
            // core at 16 px. It fits the pixmap at every tray size.
            let dot_rim = PathBuilder::from_circle(m.dot_x, m.dot_y, m.dot_r + 0.5);
            stroke(&mut pm, dot_rim.as_ref(), rim, 1.0, id);
        }
        Phase::Transcribing | Phase::Pasting => {
            fill(&mut pm, m.disc(0.0).as_ref(), ink, id);
            for dot in m.typing_dots(n) {
                knock(&mut pm, dot.as_ref());
            }
            fill(&mut pm, m.dot().as_ref(), ink, id);
        }
    }
    // A dot `d` units wide in the right corner, `bottom` or top, cut free of the mark by a
    // 1-unit ring, with an optional 1 px `rim` just inside its edge. A whole-pixel radius keeps
    // the ring clear of partial pixels at 20 px.
    let mut dot = |d: f32, bottom: bool, c: Rgba, rim: Option<Rgba>| {
        let r = (d * k / 2.0).round();
        let (cx, cy) = (n - r, if bottom { n - r } else { r });
        let ring = k.round().max(1.0);
        knock(&mut pm, PathBuilder::from_circle(cx, cy, r + ring).as_ref());
        fill(&mut pm, PathBuilder::from_circle(cx, cy, r).as_ref(), c, id);
        if let Some(rim) = rim {
            stroke(
                &mut pm,
                PathBuilder::from_circle(cx, cy, r - 0.5).as_ref(),
                rim,
                1.0,
                id,
            );
        }
    };
    match badge {
        TrayBadge::None => {}
        // The record dot's rim: orange alone is under 2:1 on accent taskbars (#005FB8,
        // #8764B8). Inside the edge, so the clear ring around the badge stays.
        TrayBadge::Error => dot(
            6.0,
            true,
            tray_colour(palette::ERROR, light_taskbar),
            Some(tray_rim(light_taskbar)),
        ),
        TrayBadge::Update => dot(5.0, false, palette::UPDATE, None),
    }
    Some(pm)
}

/// The 1 px rim of the coloured tray shapes (record dot and disc, error badge): dark on a light
/// taskbar, white on a dark or accent one.
fn tray_rim(light_taskbar: bool) -> Rgba {
    if light_taskbar {
        TRAY_RIM
    } else {
        palette::WHITE
    }
}

/// The app icon at `size` px: the mark (ink ring, red record dot) on a paper rounded-square
/// tile with a hairline edge, so it holds on light and dark backgrounds. `None` for size 0.
#[must_use]
pub fn logo(size: u32) -> Option<Pixmap> {
    logo_with(size, ICON_DOT)
}

/// [`logo`] with the record dot in `dot`: the window icon follows the accent (DS-20.8).
#[must_use]
pub fn logo_with(size: u32, dot: Rgba) -> Option<Pixmap> {
    let mut pm = Pixmap::new(size, size)?;
    let id = Transform::identity();
    let logo = Logo::new(size);
    fill(&mut pm, logo.tile.as_ref(), palette::PAPER, id);
    stroke(
        &mut pm,
        logo.edge.as_ref(),
        palette::PAPER_EDGE,
        logo.edge_width,
        id,
    );
    fill_eo(&mut pm, logo.mark.ring().as_ref(), palette::MARK);
    fill(&mut pm, logo.mark.dot().as_ref(), dot, id);
    Some(pm)
}

/// [`logo`] at 256 px as SVG for the settings window: the same paths, so the two never drift.
#[must_use]
pub fn logo_svg() -> String {
    logo_svg_with(DEFAULT_DOT)
}

/// [`logo_svg`] with the record dot in `dot`.
#[must_use]
pub fn logo_svg_with(dot: Rgba) -> String {
    use std::fmt::Write;
    const SIZE: u32 = 256;
    let logo = Logo::new(SIZE);
    let hex = |c: Rgba| format!("#{:02X}{:02X}{:02X}", c.0, c.1, c.2);
    let mut svg = format!(
        "<svg width=\"{SIZE}\" height=\"{SIZE}\" viewBox=\"0 0 {SIZE} {SIZE}\" \
         xmlns=\"http://www.w3.org/2000/svg\">\n\
         <!-- Generated from oxi-platform draw::logo by the regen_icons test; do not edit. -->\n"
    );
    let mut path = |p: Option<&Path>, attrs: &str| {
        let Some(p) = p else { return };
        svg.push_str("<path d=\"");
        for seg in p.segments() {
            let _ = match seg {
                PathSegment::MoveTo(a) => write!(svg, "M{} {}", a.x, a.y),
                PathSegment::LineTo(a) => write!(svg, "L{} {}", a.x, a.y),
                PathSegment::QuadTo(a, b) => write!(svg, "Q{} {} {} {}", a.x, a.y, b.x, b.y),
                PathSegment::CubicTo(a, b, c) => {
                    write!(svg, "C{} {} {} {} {} {}", a.x, a.y, b.x, b.y, c.x, c.y)
                }
                PathSegment::Close => write!(svg, "Z"),
            };
        }
        let _ = writeln!(svg, "\" {attrs}/>");
    };
    path(
        logo.tile.as_ref(),
        &format!("fill=\"{}\"", hex(palette::PAPER)),
    );
    path(
        logo.edge.as_ref(),
        &format!(
            "fill=\"none\" stroke=\"{}\" stroke-width=\"{}\"",
            hex(palette::PAPER_EDGE),
            logo.edge_width
        ),
    );
    path(
        logo.mark.ring().as_ref(),
        &format!("fill=\"{}\" fill-rule=\"evenodd\"", hex(palette::MARK)),
    );
    path(logo.mark.dot().as_ref(), &format!("fill=\"{}\"", hex(dot)));
    svg.push_str("</svg>\n");
    svg
}

/// The paths of [`logo`] at one size.
struct Logo {
    tile: Option<Path>,
    edge: Option<Path>,
    edge_width: f32,
    mark: Mark,
}

impl Logo {
    fn new(size: u32) -> Self {
        let n = size as f32;
        let corner = (0.22 * n).round();
        // The brand sheet's 4-unit hairline at 256, never under a pixel.
        let w = (n / 64.0).max(1.0);
        Self {
            tile: rounded(0.0, 0.0, n, n, corner),
            edge: rounded(w / 2.0, w / 2.0, n - w, n - w, corner - w / 2.0),
            edge_width: w,
            mark: Mark::new(n / 256.0, 0.0, n),
        }
    }
}

/// The app mark, designed on the 256 grid of the app icon (brand concept C): an "o" ring centred
/// at (116, 140), 80 out and 40 in, and the record dot at (196, 60), radius 24. The tray crops it
/// to the square 28..228. The ring's centre sits on a whole pixel and radii snap to half pixels,
/// so both sides of the ring cover the same pixels and the shapes stay crisp from 16 to 256 px.
struct Mark {
    cx: f32,
    cy: f32,
    outer: f32,
    inner: f32,
    dot_x: f32,
    dot_y: f32,
    dot_r: f32,
}

impl Mark {
    /// The mark at `scale` pixels per grid unit, with the grid point (`crop`, `crop`) at the
    /// pixmap's origin, inside a `side`-pixel square.
    fn new(scale: f32, crop: f32, side: f32) -> Self {
        let half = |v: f32| (v * 2.0).round() / 2.0;
        let at = |u: f32| half((u - crop) * scale);
        let outer = half(80.0 * scale);
        Self {
            cx: ((116.0 - crop) * scale).round().clamp(outer, side - outer),
            cy: ((140.0 - crop) * scale).round().min(side - outer),
            outer,
            inner: half(40.0 * scale),
            dot_x: at(196.0),
            dot_y: at(60.0),
            dot_r: half(24.0 * scale).max(1.0),
        }
    }

    /// The tray's crop: the square 28..228 of the grid. At the four tray sizes the radii are
    /// picked by hand within a quarter pixel of the grid's, so no ring edge grazes a pixel
    /// corner: a grazed corner renders on one side of the ring and not the other.
    fn tray(side: f32) -> Self {
        let mut m = Self::new(side / 200.0, 28.0, side);
        // Each pair: (outer, inner) in pixels; the grid gives 0.4 and 0.2 of the side.
        #[allow(clippy::float_cmp)] // side is one of the whole tray sizes
        let radii = [
            (16.0, 6.5, 3.0),
            (20.0, 8.0, 3.75),
            (24.0, 9.75, 4.5),
            (32.0, 13.0, 6.75),
        ]
        .into_iter()
        .find(|r| r.0 == side);
        if let Some((_, outer, inner)) = radii {
            (m.outer, m.inner) = (outer, inner);
        }
        m
    }

    /// The ring as two circles, for an even-odd fill.
    fn ring(&self) -> Option<Path> {
        let mut p = PathBuilder::new();
        p.push_circle(self.cx, self.cy, self.outer);
        p.push_circle(self.cx, self.cy, self.inner);
        p.finish()
    }

    /// The ring's outer edge as a disc, moved in by `inset` (half a line width for a stroke).
    fn disc(&self, inset: f32) -> Option<Path> {
        PathBuilder::from_circle(self.cx, self.cy, self.outer - inset)
    }

    /// The ring's hole.
    fn hole(&self) -> Option<Path> {
        PathBuilder::from_circle(self.cx, self.cy, self.inner)
    }

    fn dot(&self) -> Option<Path> {
        PathBuilder::from_circle(self.dot_x, self.dot_y, self.dot_r)
    }

    /// Three "typing" dots across the disc's middle, each `side / 8` wide with as much between.
    fn typing_dots(&self, side: f32) -> [Option<Path>; 3] {
        let d = (side / 8.0).round().max(2.0);
        let left = (self.cx - 2.5 * d).round();
        let y = (self.cy - d / 2.0).round();
        [0.0, 2.0, 4.0].map(|i| {
            let x = left + i * d;
            if d >= 3.0 {
                PathBuilder::from_circle(x + d / 2.0, y + d / 2.0, d / 2.0)
            } else {
                tiny_skia::Rect::from_xywh(x, y, d, d).map(PathBuilder::from_rect)
            }
        })
    }
}

/// The pixmap's premultiplied RGBA bytes as premultiplied BGRA, the order of a 32-bit DIB for
/// `UpdateLayeredWindow` and `CreateIconIndirect`.
#[must_use]
pub fn bgra(pixmap: &Pixmap) -> Vec<u8> {
    let mut out = vec![0; pixmap.data().len()];
    bgra_into(pixmap, &mut out);
    out
}

/// [`bgra`] into `out`, which must hold exactly the pixmap's bytes (a reused DIB).
pub fn bgra_into(pixmap: &Pixmap, out: &mut [u8]) {
    out.copy_from_slice(pixmap.data());
    for px in out.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
}

/// One line of text laid out with kerning, baseline at the cap-height centre.
struct Label<'f> {
    font: &'f FontRef<'f>,
    text: &'f str,
    scale: PxScale,
    width: f32,
}

impl<'f> Label<'f> {
    fn new(font: &'f FontRef<'f>, text: &'f str, em: f32) -> Self {
        let mut label = Self {
            font,
            text,
            scale: PxScale::from(1.0),
            width: 0.0,
        };
        label.set_em(em);
        label
    }

    fn set_em(&mut self, em: f32) {
        // `PxScale` is the ascent-to-descent height, not the em size.
        let upem = self.font.units_per_em().unwrap_or(1.0);
        self.scale = PxScale::from(em * self.font.height_unscaled() / upem);
        let sf = self.font.as_scaled(self.scale);
        let mut prev = None;
        self.width = self
            .text
            .chars()
            .map(|c| {
                let id = sf.glyph_id(c);
                let k = prev.map_or(0.0, |p| sf.kern(p, id));
                prev = Some(id);
                k + sf.h_advance(id)
            })
            .sum();
    }

    /// Shrinks the font so the label fits `room`, down to [`EM_MIN`] of its size.
    fn fit(&mut self, room: f32) {
        if self.width > room && self.width > 0.0 {
            let em = self.em() * (room / self.width).max(EM_MIN);
            self.set_em(em);
        }
    }

    fn em(&self) -> f32 {
        self.scale.y * self.font.units_per_em().unwrap_or(1.0) / self.font.height_unscaled()
    }

    /// Draws from `x`, cut at `right`, vertically centred on `cy`.
    fn draw(&self, pm: &mut Pixmap, x: f32, cy: f32, right: f32) {
        let sf = self.font.as_scaled(self.scale);
        let cap = sf
            .outline_glyph(sf.scaled_glyph('H'))
            .map_or(sf.ascent() * 0.7, |g| -g.px_bounds().min.y);
        let baseline = (cy + cap / 2.0).round();
        let (width, height) = (pm.width(), pm.height());
        let Some(mut mask) = Mask::new(width, height) else {
            return;
        };
        let data = mask.data_mut();
        let mut pen = x.round();
        let mut prev = None;
        for ch in self.text.chars() {
            let id = sf.glyph_id(ch);
            pen += prev.map_or(0.0, |p| sf.kern(p, id));
            prev = Some(id);
            let glyph = id.with_scale_and_position(self.scale, point(pen, baseline));
            pen += sf.h_advance(id);
            let Some(outlined) = sf.outline_glyph(glyph) else {
                continue;
            };
            let origin = outlined.px_bounds().min;
            outlined.draw(|gx, gy, cover| {
                let (px, py) = (origin.x + gx as f32, origin.y + gy as f32);
                if px < 0.0 || py < 0.0 || px >= right.min(width as f32) || py >= height as f32 {
                    return;
                }
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                // in range: checked above; coverage is 0..=1
                let (at, add) = (
                    py as usize * width as usize + px as usize,
                    (cover * 255.0).round() as u8,
                );
                data[at] = data[at].saturating_add(add);
            });
        }
        let mut paint = paint(TEXT);
        paint.anti_alias = false;
        if let Some(all) = tiny_skia::Rect::from_xywh(0.0, 0.0, width as f32, height as f32) {
            pm.fill_rect(all, &paint, Transform::identity(), Some(&mask));
        }
    }
}

/// The meter: the newest bars that fit in `x0..right`, right-aligned, centred on `cy`; silence
/// is a row of dots. Bar edges sit on whole pixels so the meter stays crisp at any scale. Each
/// bar height is rasterised once; a frame only blends its coverage in the bar colour.
struct Bars {
    x0: f32,
    right: f32,
    cy: f32,
    bar: f32,
    pitch: f32,
    tall: f32,
    accent: Rgba,
    /// Thickness of the clip line, 2 DIP.
    line: f32,
    /// Height of the first sprite.
    lo: usize,
    /// Coverage (the alpha) of every bar height from `lo`.
    sprites: Vec<Pixmap>,
}

impl Bars {
    fn new(accent: Rgba, x0: f32, right: f32, cy: f32, h: f32, dip: f32) -> Option<Self> {
        let bar = (BAR_W * h).round().max(1.0);
        let pitch = bar + (BAR_GAP * h).round().max(1.0);
        let tall = (BAR_MAX * h).round();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        // whole pixels of a pill at most a few hundred high, at least 1
        let (bar_px, lo, hi) = (
            bar as u32,
            bar.min(tall).max(1.0) as usize,
            bar.max(tall) as usize,
        );
        let sprite = |len: usize| {
            let mut pm = Pixmap::new(bar_px, u32::try_from(len).ok()?)?;
            let path = rounded(0.0, 0.0, bar, len as f32, bar / 2.0);
            fill(&mut pm, path.as_ref(), TEXT, Transform::identity());
            Some(pm)
        };
        Some(Self {
            x0,
            right: right.round(),
            cy,
            bar,
            pitch,
            tall,
            accent,
            line: (2.0 * dip).round().max(1.0),
            lo,
            sprites: (lo..=hi).map(sprite).collect::<Option<_>>()?,
        })
    }

    fn draw(&self, pm: &mut Pixmap, meter: &Meter, clipping: bool) {
        let (bar, pitch) = (self.bar, self.pitch);
        let room = self.right - self.x0 + (pitch - bar);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        // a few dozen bars at most; negative room is 0
        let fit = (room / pitch).floor().max(0.0) as usize;
        let len = meter.bars().len();
        let shown = fit.min(len);
        let mut x = self.right - bar - (shown.max(1) - 1) as f32 * pitch;
        let colour = if clipping {
            palette::ERROR
        } else {
            self.accent
        };
        if clipping {
            // The mark of overload (DS-21.3): a line across the whole wave box, at its top edge.
            let top = (self.cy - self.tall / 2.0).round();
            let path = rounded(self.x0, top, self.right - self.x0, self.line, 0.0);
            fill(pm, path.as_ref(), palette::ERROR, Transform::identity());
        }
        for v in meter.bars().skip(len - shown) {
            // Odd or even height to match the bar width, so the bar centres on the dot row.
            let h = (bar + v * (self.tall - bar)).round();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            // whole pixels inside the pill; a height of 0 matches no sprite
            if let Some(s) = self.sprites.get((h as usize).wrapping_sub(self.lo)) {
                blend(pm, s, colour, x as i32, (self.cy - h / 2.0).round() as i32);
            }
            x += pitch;
        }
    }
}

/// Paints the opaque `colour` through the alpha of `cover`, its top-left at (`x`, `y`),
/// clipped to `dst`, with tiny-skia's own rounding (`lerp` by coverage, `div255` as
/// `(v + 255) >> 8`), so the bar lands exactly as a path fill of it would.
fn blend(dst: &mut Pixmap, cover: &Pixmap, colour: Rgba, x: i32, y: i32) {
    let (dw, dh) = (i64::from(dst.width()), i64::from(dst.height()));
    let row = cover.width() as usize * 4;
    let src = [colour.0, colour.1, colour.2, colour.3].map(u16::from);
    let out = dst.data_mut();
    for (sy, line) in (0i64..).zip(cover.data().chunks_exact(row)) {
        let dy = i64::from(y) + sy;
        if !(0..dh).contains(&dy) {
            continue;
        }
        for (sx, px) in (0i64..).zip(line.as_chunks::<4>().0) {
            let (dx, c) = (i64::from(x) + sx, u16::from(px[3]));
            if !(0..dw).contains(&dx) || c == 0 {
                continue;
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            // inside `dst`: checked above
            let at = ((dy * dw + dx) * 4) as usize;
            for (d, s) in out[at..at + 4].iter_mut().zip(src) {
                #[allow(clippy::cast_possible_truncation)] // (255 * 255 + 255) >> 8 is 255
                let v = ((u16::from(*d) * (255 - c) + s * c + 255) >> 8) as u8;
                *d = v;
            }
        }
    }
}

/// `kind` in the `s`-pixel box at (`x`, `y`): accent shapes, cut-outs in the surface colour.
#[allow(clippy::too_many_lines)] // one short path per icon, read top to bottom
fn draw_icon(pm: &mut Pixmap, kind: Icon, accent: Rgba, x: f32, y: f32, s: f32) {
    let at = Transform::from_row(s, 0.0, 0.0, s, x, y);
    let line = 0.1;
    match kind {
        Icon::None => {}
        Icon::Mic => {
            fill(
                pm,
                rounded(0.33, 0.0, 0.34, 0.58, 0.17).as_ref(),
                accent,
                at,
            );
            let mut p = PathBuilder::new();
            p.move_to(0.15, 0.4);
            p.cubic_to(0.15, 0.6, 0.31, 0.76, 0.5, 0.76);
            p.cubic_to(0.69, 0.76, 0.85, 0.6, 0.85, 0.4);
            p.move_to(0.5, 0.76);
            p.line_to(0.5, 0.95);
            p.move_to(0.32, 0.95);
            p.line_to(0.68, 0.95);
            stroke(pm, p.finish().as_ref(), accent, line, at);
        }
        Icon::Lock => {
            let mut p = PathBuilder::new();
            p.move_to(0.29, 0.5);
            p.line_to(0.29, 0.31);
            p.cubic_to(0.29, 0.19, 0.38, 0.1, 0.5, 0.1);
            p.cubic_to(0.62, 0.1, 0.71, 0.19, 0.71, 0.31);
            p.line_to(0.71, 0.5);
            stroke(pm, p.finish().as_ref(), accent, line, at);
            fill(
                pm,
                rounded(0.14, 0.44, 0.72, 0.52, 0.12).as_ref(),
                accent,
                at,
            );
            fill(
                pm,
                PathBuilder::from_circle(0.5, 0.69, 0.075).as_ref(),
                SURFACE,
                at,
            );
        }
        Icon::Hourglass => {
            // Glass outline with sand settled at the bottom.
            let mut p = PathBuilder::new();
            p.move_to(0.27, 0.1);
            p.line_to(0.73, 0.1);
            p.line_to(0.55, 0.5);
            p.line_to(0.73, 0.9);
            p.line_to(0.27, 0.9);
            p.line_to(0.45, 0.5);
            p.close();
            p.move_to(0.17, 0.05);
            p.line_to(0.83, 0.05);
            p.move_to(0.17, 0.95);
            p.line_to(0.83, 0.95);
            stroke(pm, p.finish().as_ref(), accent, line, at);
            let mut sand = PathBuilder::new();
            sand.move_to(0.5, 0.6);
            sand.line_to(0.68, 0.88);
            sand.line_to(0.32, 0.88);
            sand.close();
            fill(pm, sand.finish().as_ref(), accent, at);
        }
        Icon::Paste => {
            stroke(
                pm,
                rounded(0.17, 0.13, 0.66, 0.82, 0.12).as_ref(),
                accent,
                line,
                at,
            );
            fill(
                pm,
                rounded(0.33, 0.02, 0.34, 0.2, 0.07).as_ref(),
                accent,
                at,
            );
            let mut p = PathBuilder::new();
            for ly in [0.5, 0.7] {
                p.move_to(0.36, ly);
                p.line_to(0.64, ly);
            }
            stroke(pm, p.finish().as_ref(), accent, line, at);
        }
        Icon::Info => {
            fill(
                pm,
                PathBuilder::from_circle(0.5, 0.5, 0.48).as_ref(),
                accent,
                at,
            );
            fill(
                pm,
                PathBuilder::from_circle(0.5, 0.29, 0.075).as_ref(),
                SURFACE,
                at,
            );
            fill(
                pm,
                rounded(0.43, 0.42, 0.14, 0.34, 0.07).as_ref(),
                SURFACE,
                at,
            );
        }
        Icon::Warning => {
            let mut p = PathBuilder::new();
            p.move_to(0.5, 0.1);
            p.line_to(0.93, 0.86);
            p.line_to(0.07, 0.86);
            p.close();
            let p = p.finish();
            fill(pm, p.as_ref(), accent, at);
            stroke(pm, p.as_ref(), accent, 0.14, at);
            fill(
                pm,
                rounded(0.44, 0.33, 0.12, 0.3, 0.06).as_ref(),
                SURFACE,
                at,
            );
            fill(
                pm,
                PathBuilder::from_circle(0.5, 0.74, 0.07).as_ref(),
                SURFACE,
                at,
            );
        }
    }
}

/// A rounded rectangle as one path, so a translucent fill never doubles at the joins.
#[allow(clippy::many_single_char_names)] // x, y, w, h, r: the usual geometry names
fn rounded(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let mut p = PathBuilder::new();
    push_rounded(&mut p, x, y, w, h, r);
    p.finish()
}

/// [`rounded`] as one more contour of `p`.
#[allow(clippy::many_single_char_names)] // x, y, w, h, r, k: the usual geometry names
fn push_rounded(p: &mut PathBuilder, x: f32, y: f32, w: f32, h: f32, r: f32) {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    let k = 0.552_284_8 * r; // cubic control distance of a quarter circle
    let (x1, y1) = (x + w, y + h);
    p.move_to(x + r, y);
    p.line_to(x1 - r, y);
    p.cubic_to(x1 - r + k, y, x1, y + r - k, x1, y + r);
    p.line_to(x1, y1 - r);
    p.cubic_to(x1, y1 - r + k, x1 - r + k, y1, x1 - r, y1);
    p.line_to(x + r, y1);
    p.cubic_to(x + r - k, y1, x, y1 - r + k, x, y1 - r);
    p.line_to(x, y + r);
    p.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    p.close();
}

fn paint(c: Rgba) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(c.0, c.1, c.2, c.3);
    p.anti_alias = true;
    p
}

fn fill(pm: &mut Pixmap, path: Option<&Path>, c: Rgba, at: Transform) {
    if let Some(path) = path {
        pm.fill_path(path, &paint(c), FillRule::Winding, at, None);
    }
}

/// [`fill`] with the even-odd rule: a path of nested contours draws as a ring.
fn fill_eo(pm: &mut Pixmap, path: Option<&Path>, c: Rgba) {
    if let Some(path) = path {
        pm.fill_path(
            path,
            &paint(c),
            FillRule::EvenOdd,
            Transform::identity(),
            None,
        );
    }
}

/// Clears `path` out of `pm`: a hole the taskbar shows through.
fn knock(pm: &mut Pixmap, path: Option<&Path>) {
    if let Some(path) = path {
        let mut p = paint(palette::WHITE);
        p.blend_mode = BlendMode::DestinationOut;
        pm.fill_path(path, &p, FillRule::Winding, Transform::identity(), None);
    }
}

fn stroke(pm: &mut Pixmap, path: Option<&Path>, c: Rgba, width: f32, at: Transform) {
    if let Some(path) = path {
        let s = Stroke {
            width,
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            ..Stroke::default()
        };
        pm.stroke_path(path, &paint(c), &s, at, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxi_core::config::OverlayPosition;
    use oxi_core::overlay::place;

    /// The wave box starts at one x whatever the label is (DS-21.3 "nothing moves").
    #[test]
    fn the_wave_box_does_not_move_when_the_label_swaps() {
        let Ok(bytes) = std::fs::read("C:/Windows/Fonts/segoeui.ttf") else {
            eprintln!("skip: no font");
            return;
        };
        let font = FontRef::try_from_slice(&bytes).unwrap();
        let screen = Rect {
            x: 0,
            y: 0,
            w: 10_000,
            h: 10_000,
        };
        let state = OverlayState::Recording {
            locked: true,
            ending_soon: false,
        };
        for scale in [1.0, 1.5, 2.0] {
            let r = place(OverlayPosition::TopLeft, screen, scale);
            let starts: Vec<u32> = ["Recording", "1:05", TOO_LOUD]
                .into_iter()
                .map(|label| {
                    let mut pm = Pixmap::new(r.w.cast_unsigned(), r.h.cast_unsigned()).unwrap();
                    let (bars, _) = surface(&mut pm, state, Some(&font), label, Rgba(1, 2, 3, 255));
                    bars.unwrap().x0.to_bits()
                })
                .collect();
            assert!(starts.windows(2).all(|w| w[0] == w[1]), "{starts:?}");
        }
    }
}
