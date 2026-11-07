//! Overlay pill and tray icons as premultiplied RGBA pixmaps (`tiny-skia`, `ab_glyph`).
//!
//! Sizes, colours, labels and icons come from [`oxi_core::overlay`]: the pill size is the
//! [`place`](oxi_core::overlay::place) rectangle, and every inner distance is a share of its
//! height, so 100, 150 and 200 % DPI draw the same picture.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use oxi_core::machine::{OverlayState, Phase};
use oxi_core::overlay::palette::{EDGE, SURFACE, TEXT};
use oxi_core::overlay::{
    Icon, Meter, PILL_BAR_GAP as BAR_GAP, PILL_BAR_MAX as BAR_MAX, PILL_BAR_W as BAR_W,
    PILL_EM as EM, PILL_EM_MIN as EM_MIN, PILL_GAP as GAP, PILL_ICON as ICON,
    PILL_METER_MIN as METER_MIN, PILL_PAD as PAD, Rect, Rgba, look, palette,
};
use oxi_core::types::Millis;
use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Mask, Paint, Path, PathBuilder, Pixmap, Stroke, Transform,
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
/// drawn when the state or the size changes; another frame of the same state copies them and
/// adds the newest bars, so it allocates nothing.
pub struct PillCanvas<'f> {
    font: Option<FontRef<'f>>,
    cache: Option<Cache>,
}

struct Cache {
    state: OverlayState,
    /// The pill without its bars.
    base: Pixmap,
    frame: Pixmap,
    bars: Option<Bars>,
}

impl<'f> PillCanvas<'f> {
    /// Without a font the pill shows its icon and meter only.
    #[must_use]
    pub fn new(font: Option<FontRef<'f>>) -> Self {
        Self { font, cache: None }
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
        let sized = |c: &Cache| (c.base.width(), c.base.height()) == size;
        if self
            .cache
            .as_ref()
            .is_none_or(|c| c.state != state || !sized(c))
        {
            let (mut base, frame) = match self.cache.take() {
                Some(c) if sized(&c) => (c.base, c.frame),
                _ => (Pixmap::new(size.0, size.1)?, Pixmap::new(size.0, size.1)?),
            };
            base.fill(Color::TRANSPARENT);
            let bars = surface(&mut base, state, self.font.as_ref());
            self.cache = Some(Cache {
                state,
                base,
                frame,
                bars,
            });
        }
        let c = self.cache.as_mut()?;
        c.frame.data_mut().copy_from_slice(c.base.data());
        if let Some(bars) = &c.bars {
            bars.draw(&mut c.frame, meter, meter.clipping(now));
        }
        Some(&c.frame)
    }
}

/// Everything of the pill for `state` but its bars, into the clear `pm`; the bars when the
/// state has a meter.
fn surface(pm: &mut Pixmap, state: OverlayState, font: Option<&FontRef<'_>>) -> Option<Bars> {
    let look = look(state);
    if state == OverlayState::Hidden {
        return None;
    }
    let (w, h) = (pm.width() as f32, pm.height() as f32);
    fill(
        pm,
        rounded(0.0, 0.0, w, h, h / 2.0).as_ref(),
        SURFACE,
        Transform::identity(),
    );
    stroke(
        pm,
        rounded(0.5, 0.5, w - 1.0, h - 1.0, (h - 1.0) / 2.0).as_ref(),
        EDGE,
        1.0,
        Transform::identity(),
    );

    let (pad, icon, gap) = ((PAD * h).round(), (ICON * h).round(), (GAP * h).round());
    let right = w - pad;
    let mut text = font
        .filter(|_| !look.label.is_empty())
        .map(|f| Label::new(f, look.label, EM * h));
    let x0 = if look.meter {
        if let Some(t) = text.as_mut() {
            t.fit(right - (pad + icon + gap) - gap - METER_MIN * h);
        }
        pad
    } else {
        // No meter: centre the icon and label as one group.
        let group = icon + text.as_ref().map_or(0.0, |t| gap + t.width);
        ((w - group) / 2.0).round().max(pad)
    };
    let cy = h / 2.0;
    draw_icon(
        pm,
        look.icon,
        look.accent,
        x0,
        (cy - icon / 2.0).round(),
        icon,
    );
    let mut x = x0 + icon + gap;
    if let Some(t) = &text {
        t.draw(pm, x, cy, right);
        x += t.width + gap;
    }
    if look.meter {
        Bars::new(look.accent, x.round(), right, cy, h)
    } else {
        None
    }
}

/// The tray icon for `phase` at `size` px (16, 20, 24 or 32). Idle is a grey ring, recording a
/// filled disc, transcribing (and the short paste after it) three dots: shape and colour both
/// change, so the state reads without colour vision. Each shape has a 1 px dark halo, so it
/// stays apart from any taskbar, an accent-coloured one included. `None` for size 0.
#[must_use]
pub fn tray_icon(phase: Phase, size: u32) -> Option<Pixmap> {
    let mut pm = Pixmap::new(size, size)?;
    let n = size as f32;
    let at = Transform::from_scale(n, n);
    // One pixel in the unit square the shapes are drawn in.
    let px = 1.0 / n;
    match phase {
        Phase::Idle => {
            let ring = PathBuilder::from_circle(0.5, 0.5, 0.32);
            stroke(&mut pm, ring.as_ref(), SURFACE, 0.17 + 2.0 * px, at);
            stroke(&mut pm, ring.as_ref(), palette::INFO, 0.17, at);
        }
        Phase::Recording => {
            fill(
                &mut pm,
                PathBuilder::from_circle(0.5, 0.5, 0.44).as_ref(),
                SURFACE,
                at,
            );
            fill(
                &mut pm,
                PathBuilder::from_circle(0.5, 0.5, 0.44 - px).as_ref(),
                palette::RECORDING,
                at,
            );
        }
        Phase::Transcribing | Phase::Pasting => {
            // Every halo first, so a neighbour's halo never covers a dot.
            for (r, c) in [(0.13 + px, SURFACE), (0.13, palette::BUSY)] {
                for cx in [0.19, 0.5, 0.81] {
                    fill(
                        &mut pm,
                        PathBuilder::from_circle(cx, 0.5, r).as_ref(),
                        c,
                        at,
                    );
                }
            }
        }
    }
    Some(pm)
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
    text: &'static str,
    scale: PxScale,
    width: f32,
}

impl<'f> Label<'f> {
    fn new(font: &'f FontRef<'f>, text: &'static str, em: f32) -> Self {
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
    /// Height of the first sprite.
    lo: usize,
    /// Coverage (the alpha) of every bar height from `lo`.
    sprites: Vec<Pixmap>,
}

impl Bars {
    fn new(accent: Rgba, x0: f32, right: f32, cy: f32, h: f32) -> Option<Self> {
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
        let colour = if clipping { palette::CLIP } else { self.accent };
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
#[allow(clippy::many_single_char_names)] // x, y, w, h, r, k: the usual geometry names
fn rounded(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    let k = 0.552_284_8 * r; // cubic control distance of a quarter circle
    let (x1, y1) = (x + w, y + h);
    let mut p = PathBuilder::new();
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
    p.finish()
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
