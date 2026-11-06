//! Overlay pill and tray icons as premultiplied RGBA pixmaps (`tiny-skia`, `ab_glyph`).
//!
//! Sizes, colours, labels and icons come from [`oxi_core::overlay`]: the pill size is the
//! [`place`](oxi_core::overlay::place) rectangle, and every inner distance is a share of its
//! height, so 100, 150 and 200 % DPI draw the same picture.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use oxi_core::machine::{OverlayState, Phase};
use oxi_core::overlay::{Icon, Meter, Rect, Rgba, look, palette};
use oxi_core::types::Millis;
use tiny_skia::{
    FillRule, LineCap, LineJoin, Mask, Paint, Path, PathBuilder, Pixmap, Stroke, Transform,
};

// shortcut: surface, text and edge colours belong in `oxi_core::overlay::palette` (t23 owns
// it); move them there when that file is next touched.
/// The pill body: opaque near-black, readable over light and dark desktops alike.
const SURFACE: Rgba = Rgba(0x1E, 0x1F, 0x24, 0xFF);
const TEXT: Rgba = Rgba(0xEE, 0xEF, 0xF2, 0xFF);
/// Hairline rim that keeps the pill's outline visible on a dark desktop.
const EDGE: Rgba = Rgba(0xFF, 0xFF, 0xFF, 0x24);

// Shares of the pill height.
const PAD: f32 = 0.34;
const ICON: f32 = 0.46;
const GAP: f32 = 0.22;
const EM: f32 = 0.33;
/// Narrowest meter next to a label; a longer label shrinks its font instead.
const METER_MIN: f32 = 0.7;
const BAR_MAX: f32 = 0.5;
const BAR_W: f32 = 0.065;
const BAR_GAP: f32 = 0.05;
/// Smallest label font, as a share of [`EM`]: below it the label is cut instead.
const EM_MIN: f32 = 0.85;

/// The overlay pill for `state`, sized `rect.w` x `rect.h` (the [`place`] result).
/// `now` decides the clipping colour of the meter. Without a font the pill shows its icon and
/// meter only. `None` when the rectangle is empty.
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
    let mut pm = Pixmap::new(u32::try_from(rect.w).ok()?, u32::try_from(rect.h).ok()?)?;
    let look = look(state);
    if state == OverlayState::Hidden {
        return Some(pm);
    }
    let (w, h) = (pm.width() as f32, pm.height() as f32);
    fill(
        &mut pm,
        rounded(0.0, 0.0, w, h, h / 2.0).as_ref(),
        SURFACE,
        Transform::identity(),
    );
    stroke(
        &mut pm,
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
        &mut pm,
        look.icon,
        look.accent,
        x0,
        (cy - icon / 2.0).round(),
        icon,
    );
    let mut x = x0 + icon + gap;
    if let Some(t) = &text {
        t.draw(&mut pm, x, cy, right);
        x += t.width + gap;
    }
    if look.meter {
        let colour = if meter.clipping(now) {
            palette::CLIP
        } else {
            look.accent
        };
        bars(&mut pm, meter, colour, x.round(), right, cy, h);
    }
    Some(pm)
}

/// The tray icon for `phase` at `size` px (16, 20, 24 or 32). Idle is a grey ring, recording a
/// filled disc, transcribing (and the short paste after it) three dots: shape and colour both
/// change, so the state reads without colour vision. `None` for size 0.
#[must_use]
pub fn tray_icon(phase: Phase, size: u32) -> Option<Pixmap> {
    let mut pm = Pixmap::new(size, size)?;
    let n = size as f32;
    let at = Transform::from_scale(n, n);
    match phase {
        Phase::Idle => {
            stroke(
                &mut pm,
                PathBuilder::from_circle(0.5, 0.5, 0.33).as_ref(),
                palette::INFO,
                0.15,
                at,
            );
        }
        Phase::Recording => {
            fill(
                &mut pm,
                PathBuilder::from_circle(0.5, 0.5, 0.4).as_ref(),
                palette::RECORDING,
                at,
            );
        }
        Phase::Transcribing | Phase::Pasting => {
            for cx in [0.2, 0.5, 0.8] {
                fill(
                    &mut pm,
                    PathBuilder::from_circle(cx, 0.5, 0.12).as_ref(),
                    palette::BUSY,
                    at,
                );
            }
        }
    }
    Some(pm)
}

/// The pixmap's premultiplied RGBA bytes as premultiplied BGRA, the order of a 32-bit DIB for
/// `UpdateLayeredWindow` and `CreateIconIndirect`.
#[must_use]
pub fn bgra(pixmap: &Pixmap) -> Vec<u8> {
    let mut out = pixmap.data().to_vec();
    for px in out.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    out
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

/// The newest bars that fit in `x0..right`, right-aligned, centred on `cy`; silence is a row
/// of dots. Bar edges sit on whole pixels so the meter stays crisp at any scale.
fn bars(pm: &mut Pixmap, meter: &Meter, colour: Rgba, x0: f32, right: f32, cy: f32, h: f32) {
    let bar = (BAR_W * h).round().max(1.0);
    let pitch = bar + (BAR_GAP * h).round().max(1.0);
    let room = right.round() - x0 + (pitch - bar);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    // a few dozen bars at most; negative room is 0
    let fit = (room / pitch).floor().max(0.0) as usize;
    let tall = (BAR_MAX * h).round();
    let len = meter.bars().len();
    let shown = fit.min(len);
    let mut x = right.round() - bar - (shown.max(1) - 1) as f32 * pitch;
    for v in meter.bars().skip(len - shown) {
        // Odd or even height to match the bar width, so the bar centres on the dot row.
        let len = (bar + v * (tall - bar)).round();
        let y = (cy - len / 2.0).round();
        fill(
            pm,
            rounded(x, y, bar, len, bar / 2.0).as_ref(),
            colour,
            Transform::identity(),
        );
        x += pitch;
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
