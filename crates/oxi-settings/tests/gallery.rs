//! The design-system gallery drawn by the software renderer the app ships with: light, dark,
//! light at 1.5x, and light again with a purple system accent that must not show anywhere (our
//! components never read the system accent). With `OXI_SHOTS_DIR` set, each sheet lands in
//! `gallery-<name>.png`, plus `gallery-<name>-pNN.png` slices that are readable at 1:1.

// Slint's generated code trips the workspace `unused_qualifications` lint; it is not ours to fix.
#[allow(unused_qualifications)]
mod gallery {
    include!(concat!(env!("OUT_DIR"), "/gallery.rs"));
}

mod common;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use common::purple;
use gallery::{Contrast, GalleryShot, Theme};
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Platform, WindowAdapter, WindowEvent};
use slint::{Color, ComponentHandle, PhysicalSize, PlatformError};
use tiny_skia::{IntRect, IntSize, Pixmap};

const WIDTH: f32 = 1400.0;
/// Slice height of the readable PNG pages, physical pixels.
const SLICE: u32 = 1200;

thread_local! {
    static LAST: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
}

struct Shots;

impl Platform for Shots {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        LAST.with_borrow_mut(|last| *last = Some(window.clone()));
        Ok(window)
    }
}

fn sheet(dark: bool) -> GalleryShot {
    // Once per thread; a second call on the same thread is the same platform.
    let _ = slint::platform::set_platform(Box::new(Shots));
    let shot = GalleryShot::new().unwrap();
    shot.set_dark(dark);
    shot
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // whole positive pixels
fn render(shot: &GalleryShot, scale: f32) -> Pixmap {
    let surface = LAST.with_borrow(Clone::clone).unwrap();
    surface.dispatch_event(WindowEvent::ScaleFactorChanged {
        scale_factor: scale,
    });
    let w = (WIDTH * scale) as u32;
    surface.set_size(PhysicalSize::new(w, 800));
    shot.show().unwrap();
    // Runs the `changed` handlers (the theme) so the sheet has its final height.
    slint::platform::update_timers_and_animations();
    let h = (shot.get_sheet_height() * scale).ceil() as u32;
    assert!(h > 2000, "the sheet is only {h} px tall");
    surface.set_size(PhysicalSize::new(w, h));
    slint::platform::update_timers_and_animations();
    surface.request_redraw();
    let mut buffer = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
    assert!(surface.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, w as usize);
    }));
    shot.hide().unwrap();
    let bytes = buffer
        .iter()
        .flat_map(|p| [p.red, p.green, p.blue, p.alpha])
        .collect();
    Pixmap::from_vec(bytes, IntSize::from_wh(w, h).unwrap()).unwrap()
}

fn save(pixmap: &Pixmap, name: &str) {
    let pixels = pixmap.pixels();
    assert!(
        pixels.iter().any(|p| *p != pixels[0]),
        "{name} is one flat colour"
    );
    let Some(dir) = std::env::var_os("OXI_SHOTS_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    pixmap
        .save_png(dir.join(format!("gallery-{name}.png")))
        .unwrap();
    for (i, top) in (0..pixmap.height()).step_by(SLICE as usize).enumerate() {
        let rect = IntRect::from_xywh(
            0,
            i32::try_from(top).unwrap(),
            pixmap.width(),
            SLICE.min(pixmap.height() - top),
        )
        .unwrap();
        pixmap
            .clone_rect(rect)
            .unwrap()
            .save_png(dir.join(format!("gallery-{name}-p{:02}.png", i + 1)))
            .unwrap();
    }
}

#[test]
fn purple_classifier_catches_the_accent() {
    assert!(purple(0x87, 0x64, 0xB8)); // Windows purple accent
    assert!(!purple(0x0F, 0x76, 0x6E)); // our teal
    assert!(!purple(0x80, 0x80, 0x80)); // grey
    assert!(!purple(0xC4, 0x2B, 0x1C)); // error red
}

#[test]
fn gallery_light() {
    save(&render(&sheet(false), 1.0), "light");
}

#[test]
fn gallery_dark() {
    save(&render(&sheet(true), 1.0), "dark");
}

#[test]
fn gallery_light_scaled() {
    let pixmap = render(&sheet(false), 1.5);
    assert_eq!(pixmap.width(), 2100);
    save(&pixmap, "light-1.5x");
}

#[test]
fn gallery_has_no_system_accent() {
    let shot = sheet(false);
    i_slint_backend_testing::set_system_accent_color(Color::from_rgb_u8(0x87, 0x64, 0xB8));
    for dark in [false, true] {
        shot.set_dark(dark);
        let pixmap = render(&shot, 1.0);
        let leaks = pixmap
            .pixels()
            .iter()
            .map(tiny_skia::PremultipliedColorU8::demultiply)
            .filter(|p| purple(p.red(), p.green(), p.blue()))
            .count();
        save(&pixmap, if dark { "purple-dark" } else { "purple-light" });
        assert_eq!(leaks, 0, "{leaks} purple pixels (dark: {dark})");
    }
}

/// The swatch ratios are computed by `Contrast.ratio` from the tokens; they must match the
/// spec's table (section 1.4, two decimals).
#[test]
fn contrast_matches_spec() {
    let shot = sheet(false);
    for dark in [false, true] {
        shot.set_dark(dark);
        shot.show().unwrap();
        slint::platform::update_timers_and_animations();
        let t = shot.global::<Theme>();
        let c = shot.global::<Contrast>();
        assert_eq!(t.get_dark(), dark);
        let (card, page, control) = (t.get_bg_card(), t.get_bg_page(), t.get_bg_control());
        let (text2, accent, error) = (t.get_text_secondary(), t.get_accent(), t.get_error_text());
        let tint = c.invoke_over(t.get_accent_tint(), card);
        // (pair, fg, bg, light, dark); None: the spec gives no ratio for that theme.
        let rows = [
            (
                "text-primary/card",
                t.get_text_primary(),
                card,
                Some(16.8),
                Some(14.2),
            ),
            ("text-secondary/card", text2, card, Some(6.2), Some(8.2)),
            ("text-secondary/page", text2, page, Some(5.75), None),
            ("accent/control", accent, control, Some(5.47), None),
            ("accent/page", accent, page, Some(4.93), Some(8.74)),
            ("accent/card", accent, card, None, Some(7.59)),
            (
                "on-accent/accent",
                t.get_on_accent(),
                accent,
                Some(5.47),
                Some(9.17),
            ),
            (
                "accent-text/tint",
                t.get_accent_text(),
                tint,
                Some(4.64),
                Some(5.39),
            ),
            ("error/card", error, card, Some(5.47), Some(6.97)),
            (
                "error/fill",
                error,
                t.get_error_fill(),
                Some(4.79),
                Some(6.61),
            ),
            (
                "warning/fill",
                t.get_warning_text(),
                t.get_warning_fill(),
                Some(4.77),
                Some(9.03),
            ),
            (
                "stroke-keycap/control",
                t.get_stroke_keycap(),
                control,
                Some(3.45),
                None,
            ),
        ];
        for (name, fg, bg, light, dark_ratio) in rows {
            let Some(want) = (if dark { dark_ratio } else { light }) else {
                continue;
            };
            let got = c.invoke_ratio(fg, bg);
            assert!(
                (got - want).abs() < 0.06,
                "{name} (dark: {dark}): {got:.2}, spec {want}"
            );
        }
        shot.hide().unwrap();
    }
}

/// R-25: badge text is 12 px, so every badge pair must reach AA (4.5:1) on the card and on a
/// hovered `ClickCard`. The tints are opaque, so the card under them cannot change the ratio;
/// the neutral badge's alpha fill is composited over both.
#[test]
fn badge_pairs_reach_aa_on_card_and_hover() {
    let shot = sheet(false);
    for dark in [false, true] {
        shot.set_dark(dark);
        shot.show().unwrap();
        slint::platform::update_timers_and_animations();
        let t = shot.global::<Theme>();
        let c = shot.global::<Contrast>();
        for (name, fg, fill) in [
            ("success", t.get_success_text(), t.get_success_tint()),
            ("accent", t.get_accent_text(), t.get_accent_badge()),
            ("neutral", t.get_text_secondary(), t.get_bg_inset()),
        ] {
            for (under_name, under) in [
                ("bg-card", t.get_bg_card()),
                ("bg-control-hover", t.get_bg_control_hover()),
            ] {
                let got = c.invoke_ratio(fg, c.invoke_over(fill, under));
                assert!(
                    got >= 4.5,
                    "{name} badge on {under_name} (dark: {dark}): {got:.2}:1"
                );
            }
        }
        shot.hide().unwrap();
    }
}
