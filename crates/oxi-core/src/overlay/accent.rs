//! The user's accent (DESIGN-SYSTEM sections 20 and 21): one seed colour, everything else
//! derived from it per theme by pure colour maths. The settings window and the overlay both
//! take their colours from here; nothing in `ui/` computes a colour.

use crate::config::{Accent, Rgb};

use super::look::{Rgba, palette};

const WHITE: Rgb = Rgb(0xFF, 0xFF, 0xFF);
const BLACK: Rgb = Rgb(0, 0, 0);

/// A preset swatch: its name (the swatch's tooltip) and seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub name: &'static str,
    pub seed: Rgb,
}

/// The ten presets in swatch order; Red, the brand colour, is the default (DS-20).
pub const PRESETS: [Preset; 10] = [
    Preset::new("Red", Rgb(0xD9, 0x3A, 0x3F)),
    Preset::new("Orange", Rgb(0xF7, 0x6B, 0x15)),
    Preset::new("Amber", Rgb(0xFF, 0xC5, 0x3D)),
    Preset::new("Green", Rgb(0x46, 0xA7, 0x58)),
    Preset::new("Teal", Rgb(0x12, 0xA5, 0x94)),
    Preset::new("Cyan", Rgb(0x00, 0xA2, 0xC7)),
    Preset::new("Blue", Rgb(0x00, 0x90, 0xFF)),
    Preset::new("Iris", Rgb(0x5B, 0x5B, 0xD6)),
    Preset::new("Purple", Rgb(0x8E, 0x4E, 0xC6)),
    Preset::new("Pink", Rgb(0xD6, 0x40, 0x9F)),
];

impl Preset {
    const fn new(name: &'static str, seed: Rgb) -> Self {
        Self { name, seed }
    }
}

/// The seed an `ui.accent` setting stands for.
#[must_use]
pub fn seed(accent: Accent) -> Rgb {
    match accent {
        Accent::Default => PRESETS[0].seed,
        Accent::Hex(rgb) => rgb,
    }
}

/// Which preset has this seed, for the picker's check mark.
#[must_use]
pub fn preset_index(seed: Rgb) -> Option<usize> {
    PRESETS.iter().position(|p| p.seed == seed)
}

/// `#RRGGBB` or `RRGGBB`, any case; nothing else (DS-20.2).
#[must_use]
pub fn parse_seed(text: &str) -> Option<Rgb> {
    let hex = text.trim();
    let hex = hex.strip_prefix('#').unwrap_or(hex);
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some(Rgb(byte(0)?, byte(2)?, byte(4)?))
}

/// `#RRGGBB`, upper case.
#[must_use]
pub fn hex(c: Rgb) -> String {
    format!("#{:02X}{:02X}{:02X}", c.0, c.1, c.2)
}

fn linear(v: u8) -> f64 {
    let s = f64::from(v) / 255.0;
    if s <= 0.039_28 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG 2 contrast ratio, 1.0..=21.0.
#[must_use]
pub fn contrast(a: Rgb, b: Rgb) -> f64 {
    let lum = |c: Rgb| 0.2126 * linear(c.0) + 0.7152 * linear(c.1) + 0.0722 * linear(c.2);
    let (x, y) = (lum(a), lum(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// Per sRGB channel `floor(a + (b - a) * t + 0.5)`.
#[must_use]
pub fn mix(from: Rgb, to: Rgb, t: f64) -> Rgb {
    let channel = |lo: u8, hi: u8| {
        let (lo, hi) = (f64::from(lo), f64::from(hi));
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        // 0..=255 by construction
        let v = (lo + (hi - lo) * t + 0.5).floor() as u8;
        v
    };
    Rgb(
        channel(from.0, to.0),
        channel(from.1, to.1),
        channel(from.2, to.2),
    )
}

fn worst(c: Rgb, set: &[Rgb]) -> f64 {
    set.iter()
        .map(|&s| contrast(c, s))
        .fold(f64::INFINITY, f64::min)
}

/// White or black, whichever has the higher lowest contrast against `set` (white on a tie).
fn toward(set: &[Rgb]) -> Rgb {
    if worst(WHITE, set) >= worst(BLACK, set) {
        WHITE
    } else {
        BLACK
    }
}

/// `seed` moved toward white or black in 4 % steps until it has `need`:1 against every colour of
/// `set` (which never mixes light and dark colours). Ends at worst on white or black itself.
#[must_use]
pub fn lift(seed: Rgb, set: &[Rgb], need: f64) -> Rgb {
    let to = toward(set);
    (0..=25u8)
        .map(|k| mix(seed, to, 0.04 * f64::from(k)))
        .find(|&c| worst(c, set) >= need)
        .unwrap_or(to)
}

const fn grey(v: u8) -> Rgb {
    Rgb(v, v, v)
}

const fn rgb(v: u32) -> Rgb {
    let [_, r, g, b] = v.to_be_bytes();
    Rgb(r, g, b)
}

/// The theme's opaque fills an accent mark sits on (`bg-page`, `bg-card`, `bg-control`,
/// `bg-control-hover`, `bg-control-pressed`, `bg-card-pressed`; settings `tokens.slint`).
const SURFACES_DARK: [Rgb; 6] = [
    grey(0x16),
    grey(0x1F),
    grey(0x26),
    grey(0x2C),
    grey(0x22),
    grey(0x1A),
];
const SURFACES_LIGHT: [Rgb; 6] = [
    rgb(0x00F7_F5F0),
    rgb(0x00FC_FBF8),
    rgb(0x00FF_FEFB),
    rgb(0x00F8_F6F2),
    rgb(0x00F1_EEE8),
    rgb(0x00F1_EEE8),
];

/// The colours one seed gives in one theme (DS-20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccentTheme {
    pub accent: Rgb,
    pub hover: Rgb,
    pub pressed: Rgb,
    pub on_accent: Rgb,
    pub text: Rgb,
    pub tint: Rgb,
    /// Heat scale levels 1 to 4.
    pub heat: [Rgb; 4],
}

/// Every accent colour of `seed` in the dark or the light theme.
#[must_use]
pub fn derive(seed: Rgb, dark: bool) -> AccentTheme {
    let surfaces = if dark {
        &SURFACES_DARK
    } else {
        &SURFACES_LIGHT
    };
    let card = surfaces[1];
    let accent = lift(seed, surfaces, 3.0);
    let tint = mix(card, accent, if dark { 0.16 } else { 0.12 });
    let mut with_tint = surfaces.to_vec();
    with_tint.push(tint);
    let text = lift(seed, &with_tint, 4.5);
    let on_accent = if contrast(WHITE, accent) >= 4.5 {
        WHITE
    } else {
        BLACK
    };
    let away = if on_accent == WHITE { BLACK } else { WHITE };
    // The accent already sits at the `away` end: step toward the ink instead, so both differ.
    let dir = if contrast(mix(accent, away, 0.20), accent) < 1.15
        && contrast(on_accent, mix(accent, on_accent, 0.20)) >= 4.5
    {
        on_accent
    } else {
        away
    };
    AccentTheme {
        accent,
        hover: mix(accent, dir, 0.10),
        pressed: mix(accent, dir, 0.20),
        on_accent,
        text,
        tint,
        heat: [0.25, 0.45, 0.70, 1.00].map(|t| mix(card, accent, t)),
    }
}

fn opaque(c: Rgba) -> Rgb {
    Rgb(c.0, c.1, c.2)
}

fn rgba(c: Rgb) -> Rgba {
    Rgba(c.0, c.1, c.2, 0xFF)
}

/// The accent on the overlay pill (and its progress fill): the seed lifted to 3:1 on the pill's
/// own surface (DS-21).
#[must_use]
pub fn pill_accent(seed: Rgb) -> Rgba {
    rgba(lift(seed, &[opaque(palette::SURFACE)], 3.0))
}

/// The pill's rim while recording: the accent halfway into the surface.
#[must_use]
pub fn pill_rim(pill_accent: Rgba) -> Rgba {
    rgba(mix(opaque(palette::SURFACE), opaque(pill_accent), 0.5))
}

/// A colour of the tray icon drawn so it holds 3:1 on the taskbar under it (DS-21.4).
#[must_use]
pub fn tray_colour(c: Rgba, light_taskbar: bool) -> Rgba {
    let bar = if light_taskbar {
        rgb(0x00EE_EEEE)
    } else {
        rgb(0x0020_2020)
    };
    rgba(lift(opaque(c), &[bar], 3.0))
}

/// A custom hex that sits near the error colour is allowed, with a warning (DS-20.2). A preset
/// never warns (Red is in range by design).
#[must_use]
pub fn close_to_error(seed: Rgb) -> bool {
    if preset_index(seed).is_some() {
        return false;
    }
    let (r, g, b) = (
        f64::from(seed.0) / 255.0,
        f64::from(seed.1) / 255.0,
        f64::from(seed.2) / 255.0,
    );
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let light = f64::midpoint(max, min);
    let delta = max - min;
    if delta == 0.0 {
        return false;
    }
    let sat = delta / (1.0 - (2.0 * light - 1.0).abs());
    let hue = 60.0
        * if r >= g && r >= b {
            ((g - b) / delta).rem_euclid(6.0)
        } else if g >= b {
            (b - r) / delta + 2.0
        } else {
            (r - g) / delta + 4.0
        };
    let red_hue = !(15.0..345.0).contains(&hue);
    let pale = [rgb(0x00FF_99A4), rgb(0x00B9_291A)]
        .iter()
        .any(|&e| contrast(seed, e) < 1.5);
    red_hue && sat >= 0.45 && ((0.25..=0.75).contains(&light) || pale)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(text: &str) -> Rgb {
        parse_seed(text).unwrap()
    }

    #[test]
    fn red_gives_the_design_system_vectors() {
        let d = derive(PRESETS[0].seed, true);
        assert_eq!(d.accent, c("#D93A3F"));
        assert_eq!((d.hover, d.pressed), (c("#C33439"), c("#AE2E32")));
        assert_eq!(
            (d.on_accent, d.text, d.tint),
            (WHITE, c("#E47175"), c("#3D2324"))
        );
        assert_eq!(
            d.heat,
            [c("#4E2627"), c("#732B2D"), c("#A13235"), c("#D93A3F")]
        );
        let l = derive(PRESETS[0].seed, false);
        assert_eq!(
            (l.accent, l.text, l.tint),
            (c("#D93A3F"), c("#BF3337"), c("#F8E4E2"))
        );
        assert_eq!(
            l.heat,
            [c("#F3CBCA"), c("#ECA4A5"), c("#E47477"), c("#D93A3F")]
        );
    }

    #[test]
    fn more_presets_and_edge_seeds_match() {
        let iris = PRESETS[7].seed;
        let d = derive(iris, true);
        assert_eq!(
            (d.accent, d.hover, d.pressed),
            (c("#6868D9"), c("#5E5EC3"), c("#5353AE"))
        );
        assert_eq!((d.text, d.tint), (c("#8F8FE3"), c("#2B2B3D")));
        let l = derive(iris, false);
        assert_eq!(
            (l.accent, l.hover, l.text, l.tint),
            (c("#5B5BD6"), c("#5252C1"), c("#5757CD"), c("#E9E8F4"))
        );
        let amber = PRESETS[2].seed;
        let l = derive(amber, false);
        assert_eq!(
            (l.accent, l.text, l.on_accent),
            (c("#A37E27"), c("#856620"), BLACK)
        );
        let d = derive(amber, true);
        assert_eq!(
            (d.accent, d.hover, d.pressed),
            (c("#FFC53D"), c("#E6B137"), c("#CC9E31"))
        );
        assert_eq!(derive(PRESETS[6].seed, false).accent, c("#008AF5"));
    }

    /// DS-20.1: a custom hex that fails contrast is lifted, not refused.
    #[test]
    fn a_low_contrast_hex_is_lifted_toward_the_theme() {
        let black = derive(BLACK, true);
        assert_eq!((black.accent, black.text), (c("#7A7A7A"), c("#999999")));
        let light = derive(BLACK, false);
        assert_eq!(
            (light.accent, light.hover, light.pressed),
            (BLACK, c("#1A1A1A"), c("#333333"))
        );
        let white = derive(WHITE, true);
        assert_eq!(
            (white.accent, white.hover, white.pressed),
            (WHITE, c("#E6E6E6"), c("#CCCCCC"))
        );
    }

    /// The guarantees of the table hold over a grid of seeds, in both themes.
    #[test]
    fn guarantees_hold_over_a_seed_grid() {
        for r in (0..=255u8).step_by(17) {
            for g in (0..=255u8).step_by(17) {
                for b in (0..=255u8).step_by(17) {
                    let seed = Rgb(r, g, b);
                    for dark in [true, false] {
                        let t = derive(seed, dark);
                        let surfaces = if dark {
                            &SURFACES_DARK
                        } else {
                            &SURFACES_LIGHT
                        };
                        assert!(worst(t.accent, surfaces) >= 3.0 - 1e-9, "{seed:?} {dark}");
                        assert!(worst(t.text, surfaces) >= 4.5 - 1e-9, "{seed:?} {dark}");
                        assert!(contrast(t.text, t.tint) >= 4.5 - 1e-9, "{seed:?} {dark}");
                        assert!(
                            contrast(t.on_accent, t.accent) >= 4.5 - 1e-9,
                            "{seed:?} {dark}"
                        );
                        assert!(
                            contrast(t.on_accent, t.pressed) >= 4.5 - 1e-9,
                            "{seed:?} {dark}"
                        );
                        assert_ne!(t.hover, t.accent, "{seed:?} {dark}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_pill_accent_keeps_3_to_1_on_the_pill() {
        assert_eq!(pill_accent(PRESETS[0].seed), Rgba(0xD9, 0x3A, 0x3F, 0xFF));
        assert_eq!(pill_accent(PRESETS[7].seed), Rgba(0x5B, 0x5B, 0xD6, 0xFF));
        assert_eq!(
            pill_rim(pill_accent(PRESETS[0].seed)),
            Rgba(0x7C, 0x2D, 0x32, 0xFF)
        );
        let dim = pill_accent(BLACK);
        assert!(contrast(opaque(dim), opaque(palette::SURFACE)) >= 3.0);
    }

    #[test]
    fn tray_colours_follow_the_taskbar() {
        let red = Rgba(0xD9, 0x3A, 0x3F, 0xFF);
        assert_eq!(tray_colour(red, true), red);
        assert_eq!(tray_colour(red, false), red);
        let amber = Rgba(0xFF, 0xC5, 0x3D, 0xFF);
        assert_eq!(tray_colour(amber, true), Rgba(0xA3, 0x7E, 0x27, 0xFF));
        assert_eq!(
            tray_colour(palette::ERROR, true),
            Rgba(0xB8, 0x6E, 0x76, 0xFF)
        );
        assert_eq!(tray_colour(palette::ERROR, false), palette::ERROR);
        assert_eq!(tray_colour(palette::OK, true), Rgba(0x2C, 0x97, 0x63, 0xFF));
    }

    #[test]
    fn hex_input_takes_six_digits_with_or_without_a_hash() {
        assert_eq!(parse_seed("#d93a3f"), Some(c("#D93A3F")));
        assert_eq!(parse_seed(" 12AB3f "), Some(c("#12AB3F")));
        for bad in ["12345", "#12345G", "#1234567", "", "#"] {
            assert_eq!(parse_seed(bad), None, "{bad}");
        }
        assert_eq!(hex(c("#0A0B0C")), "#0A0B0C");
        assert_eq!(seed(Accent::Default), c("#D93A3F"));
        assert_eq!(preset_index(c("#5B5BD6")), Some(7));
    }

    #[test]
    fn a_hex_near_the_error_colour_warns_but_presets_and_other_hues_do_not() {
        for near in ["#CC0000", "#FF99A4"] {
            assert!(close_to_error(c(near)), "{near}");
        }
        for far in ["#F76B15", "#D6409F", "#808080", "#0090FF"] {
            assert!(!close_to_error(c(far)), "{far}");
        }
        assert!(!close_to_error(PRESETS[0].seed));
    }
}
