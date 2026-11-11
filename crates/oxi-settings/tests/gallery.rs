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

// Probes for the shared controls, built here so the gallery sheet stays the page-task's file.
// Slint's generated code trips the workspace `unused_qualifications` lint; it is not ours to fix.
#[allow(unused_qualifications)]
mod controls {
    slint::slint! {
        import { Palette } from "std-widgets.slint";
        import { Theme, Icons, Button, Group, ActivityHeatmap, PathField, PositionTiles, TilePicker } from "../ui/theme/index.slint";

        export component PathProbe inherits Window {
            in property <string> display;
            out property <color> ink: f.shown-color;
            out property <color> secondary: Theme.text-secondary;
            out property <bool> browse-icon: f.browse-icon.width > 0;
            width: 400px;
            height: 100px;
            f := PathField {
                width: 240px;
                path: "C:/models/a.bin";
                display: root.display;
                browse-text: "Browse";
            }
        }

        // A Group title row with an icon, an "i" and a trailing action: the row is the button's height.
        export component GroupProbe inherits Window {
            width: 340px;
            height: 80px;
            Group {
                x: 0;
                y: 0;
                width: 300px;
                title: "Title";
                icon: Icons.calendar;
                info: "Hint";
                action-text: "Go";
                action-icon: Icons.folder-open-16;
            }
        }

        // The heatmap's own height: 34 px cells, 3 px apart, with and without the legend.
        export component HeatSizeProbe inherits Window {
            in property <bool> legend;
            out property <length> total: h.min-height;
            out property <length> grid: Theme.heat-top + 7 * (34px + 3px);
            out property <length> gap: Theme.space-3;
            width: 400px;
            height: 400px;
            h := ActivityHeatmap {
                levels: [1, 2, 3];
                cell: 34px;
                legend-shown: root.legend;
            }
        }

        export component TilesProbe inherits Window {
            out property <length> next-x: Theme.position-tile-width + Theme.position-tile-gap;
            width: 400px;
            height: 100px;
            init => {
                Palette.color-scheme = ColorScheme.dark;
            }
            PositionTiles {
                x: 0;
                y: 0;
                width: 360px;
                height: 72px;
                names: ["a", "b", "c", "d", "e", "f"];
                selected: 0;
            }
        }

        export component FocusProbe inherits Window {
            out property <bool> ring: b.key-focused;
            width: 200px;
            height: 100px;
            b := Button {
                text: "One";
            }
        }

        // A tile row that opens a PopupWindow when a tile is picked, as About's Slint tile does.
        export component PopupProbe inherits Window {
            out property <bool> ring: t.ring;
            width: 400px;
            height: 100px;
            t := TilePicker {
                tiles: [{ title: "a" }, { title: "b" }];
                selected: 0;
                picked(i) => {
                    pop.show();
                }
            }
            pop := PopupWindow {
                x: 0;
                y: 60px;
                width: 100px;
                height: 30px;
                Rectangle {
                    background: Palette.background;
                }
            }
        }
    }
}

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use common::purple;
use controls::{GroupProbe, HeatSizeProbe};
use gallery::{
    ClickRingProbe, ComboProbe, Contrast, DotProbe, GalleryShot, GalleryTile, HeatProbe, HoverTip,
    IconDangerProbe, InvalidFieldProbe, MeterProbe, PathFieldProbe, PathText, PickProbe,
    SubTabsProbe, Theme, TileGalleryProbe, TipFadeProbe,
};
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Key, Platform, PointerEventButton, WindowAdapter, WindowEvent};
use slint::{
    Color, ComponentHandle, LogicalPosition, ModelRc, PhysicalSize, PlatformError, SharedString,
    VecModel,
};
use tiny_skia::{IntRect, IntSize, Pixmap};

const WIDTH: f32 = 1400.0;
/// Slice height of the readable PNG pages, physical pixels.
const SLICE: u32 = 1200;

thread_local! {
    static LAST: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
    static START: Instant = Instant::now();
    // A test that must dispatch events between time steps owns the clock (`advance`).
    static CLOCK: Cell<Option<Duration>> = const { Cell::new(None) };
}

/// Moves the animation clock on by `ms` and runs the animations; a real-clock tick cannot follow it
/// on this thread, unlike `mock_elapsed_time`, so events may be dispatched between steps.
fn advance(ms: u64) {
    let now = CLOCK.get().unwrap_or_else(|| START.with(Instant::elapsed));
    CLOCK.set(Some(now + Duration::from_millis(ms)));
    slint::platform::update_timers_and_animations();
}

struct Shots;

impl Platform for Shots {
    fn duration_since_start(&self) -> Duration {
        CLOCK.get().unwrap_or_else(|| START.with(Instant::elapsed))
    }

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
    shot.global::<PathText>().on_elide(|path, budget| {
        oxi_core::paths::elide_path(&path, usize::try_from(budget).unwrap_or(0)).into()
    });
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
    assert!(!purple(0xD9, 0x3A, 0x3F)); // our default Red accent
    assert!(!purple(0x0F, 0x76, 0x6E)); // the old teal
    assert!(!purple(0x80, 0x80, 0x80)); // grey
    assert!(!purple(0xC4, 0x2B, 0x1C)); // error red
}

#[test]
fn gallery_light() {
    let shot = sheet(false);
    save(&render(&shot, 1.0), "light");
    // The 60-tile grid is sized at creation and opens on tile 31: that tile's row is whole in the
    // view, which shows `edge` (half a gap) more than the grid's height on each side.
    let t = shot.global::<Theme>();
    let row_height = t.get_tile_height() + t.get_tile_gap();
    #[allow(clippy::cast_precision_loss)] // a row index
    let top = (31 / shot.get_long_grid_columns()) as f32 * row_height;
    let (from, to) = (
        shot.get_long_grid_scrolled(),
        shot.get_long_grid_scrolled() + shot.get_long_grid_height() + t.get_tile_gap(),
    );
    assert!(
        from > 0.0 && from <= top && top + row_height <= to,
        "row at {top}, view {from}..{to}"
    );
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

/// 160 ms ease-out from 0; each step advances the mock clock by that many more milliseconds.
#[test]
fn tooltip_bubble_fades_in() {
    let _ = slint::platform::set_platform(Box::new(Shots));
    let probe = TipFadeProbe::new().unwrap();
    let got = probe.get_bubble_opacity();
    assert!(got.abs() < 0.001, "starts at {got}");
    for (step, want) in [(5, 0.05), (20, 0.25), (60, 0.72), (100, 1.0)] {
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(step));
        let got = probe.get_bubble_opacity();
        assert!((got - want).abs() < 0.03, "+{step} ms: {got}, want {want}");
    }
}

/// DS-8.7: the reason shows on the field body and on the glyph, with the glyph in the field or not.
#[test]
fn invalid_field_exposes_its_tip() {
    let _ = slint::platform::set_platform(Box::new(Shots));
    let probe = InvalidFieldProbe::new().unwrap();
    probe.show().unwrap();
    advance(0);
    let window = probe.window();
    let hover = |x: f32, y: f32| {
        let position = LogicalPosition::new(x, y);
        window.dispatch_event(WindowEvent::PointerMoved { position });
        advance(700);
        probe.global::<HoverTip>().get_text()
    };
    let away = |x: f32, y: f32| {
        let position = LogicalPosition::new(x, y);
        window.dispatch_event(WindowEvent::PointerMoved { position });
        advance(20);
        probe.global::<HoverTip>().get_text()
    };
    // (x, y, what): body of the field, then its glyph (right edge 240, 4 px inset, 24 px wide); the clearable row sits at y 80.
    for (x, y, what) in [
        (30.0, 16.0, "body, glyph in the field"),
        (224.0, 16.0, "the glyph"),
        (30.0, 56.0, "body, glyph in the Row"),
    ] {
        assert_eq!(hover(x, y), "Not a colour", "{what}");
        assert_eq!(
            away(300.0, 90.0),
            "",
            "{what}: closes when the pointer leaves"
        );
    }
    // Body -> clear "x" -> body: the "x" says "Clear", the body shows the reason again.
    assert_eq!(hover(30.0, 96.0), "Not a colour", "clearable body");
    assert_eq!(hover(228.0, 96.0), "Clear", "the clear button");
    assert_eq!(hover(30.0, 96.0), "Not a colour", "back on the body");
    // Click the "x" while its Tip shows: the text empties and the "x" goes away under the pointer.
    // A new invalid value must show its reason on the body again.
    assert_eq!(hover(228.0, 96.0), "Clear", "the clear button again");
    let position = LogicalPosition::new(228.0, 96.0);
    let button = PointerEventButton::Left;
    window.dispatch_event(WindowEvent::PointerPressed { position, button });
    window.dispatch_event(WindowEvent::PointerReleased { position, button });
    advance(0);
    assert_eq!(probe.get_clearable_text(), "", "the click cleared the text");
    probe.set_clearable_text("#12G4".into());
    assert_eq!(
        hover(30.0, 96.0),
        "Not a colour",
        "body after clear and retype"
    );
}

#[test]
fn pressed_danger_fill_differs_from_hover_and_reaches_aa() {
    let shot = sheet(false);
    for dark in [false, true] {
        shot.set_dark(dark);
        shot.show().unwrap();
        slint::platform::update_timers_and_animations();
        let (t, c) = (shot.global::<Theme>(), shot.global::<Contrast>());
        assert_eq!(t.get_dark(), dark);
        assert_ne!(t.get_error_fill_pressed(), t.get_error_fill());
        let got = c.invoke_ratio(t.get_error_text(), t.get_error_fill_pressed());
        assert!(
            got >= 4.5,
            "error-text on pressed fill: {got} (dark: {dark})"
        );
        shot.hide().unwrap();
    }
}

/// The swatch ratios are computed by `Contrast.ratio` from the tokens; they must match the
/// concept C neutrals (DESIGN-SYSTEM section 19) and the default Red accent (section 20), two decimals.
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
        // (pair, fg, bg, light, dark); None: the design system gives no ratio for that theme.
        let rows = [
            (
                "text-primary/card",
                t.get_text_primary(),
                card,
                Some(16.63),
                Some(14.74),
            ),
            ("text-secondary/card", text2, card, Some(6.84), Some(7.72)),
            ("text-secondary/page", text2, page, Some(6.50), Some(8.47)),
            ("accent/control", accent, control, Some(4.50), Some(3.33)),
            ("accent/page", accent, page, Some(4.17), Some(3.99)),
            ("accent/card", accent, card, Some(4.39), Some(3.63)),
            (
                "on-accent/accent",
                t.get_on_accent(),
                accent,
                Some(4.54),
                Some(4.54),
            ),
            (
                "accent-text/tint",
                t.get_accent_text(),
                tint,
                Some(4.59),
                Some(4.72),
            ),
            ("error/card", error, card, Some(5.98), Some(8.12)),
            (
                "error/fill",
                error,
                t.get_error_fill(),
                Some(5.31),
                Some(6.61),
            ),
            (
                "warning/fill",
                t.get_warning_text(),
                t.get_warning_fill(),
                Some(5.59),
                Some(9.03),
            ),
            (
                "stroke-keycap/control",
                t.get_stroke_keycap(),
                control,
                Some(3.64),
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

/// The light theme is concept C "white minimal" (owner's pick, DESIGN-SYSTEM section 19): a
/// reverted token fails here.
#[test]
fn light_theme_is_concept_c() {
    let shot = sheet(false);
    shot.show().unwrap();
    slint::platform::update_timers_and_animations();
    let t = shot.global::<Theme>();
    assert!(!t.get_dark());
    for (name, got, want) in [
        ("bg-page", t.get_bg_page(), 0xFFF7_F5F0),
        ("bg-card", t.get_bg_card(), 0xFFFC_FBF8),
        ("bg-control", t.get_bg_control(), 0xFFFF_FEFB),
        ("bg-control-hover", t.get_bg_control_hover(), 0xFFF8_F6F2),
        (
            "bg-control-pressed",
            t.get_bg_control_pressed(),
            0xFFF1_EEE8,
        ),
        ("bg-card-pressed", t.get_bg_card_pressed(), 0xFFF1_EEE8),
        ("bg-subtle-hover", t.get_bg_subtle_hover(), 0x0A1C_1B19),
        (
            "bg-subtle-selected",
            t.get_bg_subtle_selected(),
            0x0F1C_1B19,
        ),
        ("bg-inset", t.get_bg_inset(), 0x141C_1B19),
        ("stroke-card", t.get_stroke_card(), 0xFFE6_E2DA),
        ("stroke-control", t.get_stroke_control(), 0xFFD3_CEC4),
        (
            "stroke-control-bottom",
            t.get_stroke_control_bottom(),
            0xFFB5_AFA4,
        ),
        ("stroke-keycap", t.get_stroke_keycap(), 0xFF8A_857C),
        ("divider", t.get_divider(), 0xFFEA_E7E0),
        ("text-primary", t.get_text_primary(), 0xFF1C_1B19),
        ("text-secondary", t.get_text_secondary(), 0xFF5C_5850),
        ("text-disabled", t.get_text_disabled(), 0xFFA3_9F97),
        ("focus-ring", t.get_focus_ring(), 0xFFD9_3A3F),
        ("accent", t.get_accent(), 0xFFD9_3A3F),
        ("on-accent", t.get_on_accent(), 0xFFFF_FFFF),
        ("accent-tint", t.get_accent_tint(), 0xFFF8_E4E2),
        ("accent-badge", t.get_accent_badge(), 0xFFF8_E4E2),
        // Deprecated alias of the accent (DS-21.5).
        ("recording", t.get_recording(), 0xFFD9_3A3F),
        ("accent-hover", t.get_accent_hover(), 0xFFC3_3439),
        ("accent-pressed", t.get_accent_pressed(), 0xFFAE_2E32),
        ("accent-text", t.get_accent_text(), 0xFFBF_3337),
        ("error-text", t.get_error_text(), 0xFFB9_291A),
        ("error-fill", t.get_error_fill(), 0xFFFB_EAE6),
        (
            "error-fill-pressed",
            t.get_error_fill_pressed(),
            0xFFFC_F3EF,
        ),
        ("warning-text", t.get_warning_text(), 0xFF8A_5200),
        ("warning-fill", t.get_warning_fill(), 0xFFFA_EFD5),
        ("success-tint", t.get_success_tint(), 0xFFEA_F2E5),
        ("info-fill", t.get_info_fill(), 0xFFF2_F0EB),
    ] {
        assert_eq!(got, Color::from_argb_encoded(want), "{name}");
    }
    shot.hide().unwrap();
}

/// UI rule 19 over the text pairs the pages draw: every ink on every surface, then the pairs
/// that exist only on their own fill. Disabled text is exempt.
#[test]
fn text_pairs_reach_aa_in_both_themes() {
    let shot = sheet(false);
    for dark in [false, true] {
        shot.set_dark(dark);
        shot.show().unwrap();
        slint::platform::update_timers_and_animations();
        let t = shot.global::<Theme>();
        let c = shot.global::<Contrast>();
        assert_eq!(t.get_dark(), dark);
        let surfaces = [
            ("bg-page", t.get_bg_page()),
            ("bg-card", t.get_bg_card()),
            ("bg-control", t.get_bg_control()),
            ("bg-control-hover", t.get_bg_control_hover()),
            ("bg-control-pressed", t.get_bg_control_pressed()),
            ("bg-card-pressed", t.get_bg_card_pressed()),
            ("info-fill", t.get_info_fill()),
        ];
        let inks = [
            ("text-primary", t.get_text_primary()),
            ("text-secondary", t.get_text_secondary()),
            ("accent-text", t.get_accent_text()),
            ("error-text", t.get_error_text()),
            ("warning-text", t.get_warning_text()),
            ("success-text", t.get_success_text()),
        ];
        let mut pairs: Vec<(String, Color, Color)> = Vec::new();
        for (ink, fg) in inks {
            for (surface, bg) in surfaces {
                pairs.push((format!("{ink} on {surface}"), fg, bg));
            }
        }
        // Rail rows and list hovers draw the neutral inks on an alpha fill over a surface.
        for (ink, fg) in &inks[..2] {
            for (fill, overlay) in [
                ("bg-subtle-hover", t.get_bg_subtle_hover()),
                ("bg-subtle-selected", t.get_bg_subtle_selected()),
                ("bg-inset", t.get_bg_inset()),
            ] {
                for (surface, bg) in &surfaces[..4] {
                    let under = c.invoke_over(overlay, *bg);
                    pairs.push((format!("{ink} on {fill} over {surface}"), *fg, under));
                }
            }
        }
        for (name, fg, bg) in [
            ("on-accent on accent", t.get_on_accent(), t.get_accent()),
            (
                "on-accent on hover",
                t.get_on_accent(),
                t.get_accent_hover(),
            ),
            (
                "on-accent on pressed",
                t.get_on_accent(),
                t.get_accent_pressed(),
            ),
            (
                "accent-text on tint",
                t.get_accent_text(),
                t.get_accent_tint(),
            ),
            ("error on fill", t.get_error_text(), t.get_error_fill()),
            (
                "error on pressed",
                t.get_error_text(),
                t.get_error_fill_pressed(),
            ),
            (
                "warning on fill",
                t.get_warning_text(),
                t.get_warning_fill(),
            ),
            (
                "success on tint",
                t.get_success_text(),
                t.get_success_tint(),
            ),
        ] {
            pairs.push((name.to_owned(), fg, bg));
        }
        for (name, fg, bg) in pairs {
            let got = c.invoke_ratio(fg, bg);
            assert!(got >= 4.5, "{name} (dark: {dark}): {got:.2}:1");
        }
        shot.hide().unwrap();
    }
}

/// `count` tiles at 3 columns (560 px wide); tile 2 is disabled.
fn tile_gallery(count: usize) -> TileGalleryProbe {
    tile_gallery_at(count, -1)
}

fn tile_gallery_at(count: usize, selected: i32) -> TileGalleryProbe {
    let _ = slint::platform::set_platform(Box::new(Shots));
    let probe = TileGalleryProbe::new().unwrap();
    probe.set_selected(selected);
    let tiles: Vec<GalleryTile> = (0..count)
        .map(|i| GalleryTile {
            title: format!("Tile {i}").into(),
            disabled: i == 2,
            ..Default::default()
        })
        .collect();
    probe.set_tiles(ModelRc::new(VecModel::from(tiles)));
    probe.show().unwrap();
    // Slint turns a `width` on the Window into constraints; the surface sets the real size.
    LAST.with_borrow(Clone::clone)
        .unwrap()
        .set_size(PhysicalSize::new(560, 360));
    slint::platform::update_timers_and_animations();
    assert_eq!(probe.get_columns(), 3.min(i32::try_from(count).unwrap()));
    probe
}

fn click_at(probe: &impl ComponentHandle, x: f32, y: f32) {
    let position = LogicalPosition::new(x, y);
    let button = PointerEventButton::Left;
    let window = probe.window();
    window.dispatch_event(WindowEvent::PointerPressed { position, button });
    window.dispatch_event(WindowEvent::PointerReleased { position, button });
    slint::platform::update_timers_and_animations();
}

fn press(probe: &impl ComponentHandle, key: impl Into<SharedString>) {
    let text = key.into();
    let window = probe.window();
    window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    window.dispatch_event(WindowEvent::KeyReleased { text });
    slint::platform::update_timers_and_animations();
}

#[test]
fn tile_gallery_keyboard_moves_a_cursor_and_picks() {
    let probe = tile_gallery(60);
    press(&probe, Key::Tab);
    assert_eq!(probe.get_cursor(), 0, "Tab reaches the first tile");
    press(&probe, Key::LeftArrow);
    assert_eq!(probe.get_cursor(), 0, "stops at the first tile");
    press(&probe, Key::RightArrow);
    press(&probe, Key::DownArrow);
    assert_eq!(probe.get_cursor(), 4);
    assert_eq!(probe.get_picks(), 0, "walking picks nothing");
    press(&probe, " ");
    assert_eq!((probe.get_selected(), probe.get_picks()), (4, 1));
    press(&probe, Key::Return);
    assert_eq!(probe.get_picks(), 1, "the picked tile fires once");
    press(&probe, Key::UpArrow);
    press(&probe, Key::RightArrow);
    assert_eq!(probe.get_cursor(), 2);
    press(&probe, Key::Return);
    assert_eq!(probe.get_selected(), 4, "a disabled tile does not pick");
    press(&probe, Key::RightArrow);
    assert_eq!(
        probe.get_cursor(),
        3,
        "a disabled tile does not trap the keys"
    );
}

#[test]
fn tile_gallery_short_last_row_and_edges() {
    let probe = tile_gallery(5);
    press(&probe, Key::Tab);
    press(&probe, Key::RightArrow);
    press(&probe, Key::RightArrow);
    press(&probe, Key::DownArrow);
    assert_eq!(
        probe.get_cursor(),
        4,
        "Down lands on the short row's last tile"
    );
    press(&probe, Key::DownArrow);
    press(&probe, Key::RightArrow);
    assert_eq!(probe.get_cursor(), 4, "stops at the last tile");
}

#[test]
fn tile_gallery_keeps_the_cursor_in_view() {
    let probe = tile_gallery(60);
    press(&probe, Key::Tab);
    assert!(probe.get_scrolled().abs() < 0.5);
    for _ in 0..10 {
        press(&probe, Key::DownArrow);
    }
    let down = probe.get_scrolled();
    assert_eq!(probe.get_cursor(), 30);
    // Row 10 ends at 1232, the box shows 368: the view moves in whole rows.
    assert!((down - 896.0).abs() < 0.5, "scrolled {down}");
    press(&probe, Key::UpArrow);
    assert!(
        (probe.get_scrolled() - down).abs() < 0.5,
        "still in view: no jump"
    );
    probe.set_selected(1);
    slint::platform::update_timers_and_animations();
    assert!(
        probe.get_scrolled().abs() < 0.5,
        "the owner's selection is scrolled to"
    );
}

#[test]
fn tile_gallery_click_picks_the_tile_under_the_pointer() {
    let probe = tile_gallery(5);
    let click = |col: f32, row: f32| {
        // Cells 181 wide with 8 between, rows 112 apart, tiles start at the box's corner.
        let position = LogicalPosition::new(col * 189.0 + 20.0, row * 112.0 + 20.0);
        let button = PointerEventButton::Left;
        let window = probe.window();
        window.dispatch_event(WindowEvent::PointerPressed { position, button });
        window.dispatch_event(WindowEvent::PointerReleased { position, button });
        slint::platform::update_timers_and_animations();
    };
    click(0.0, 1.0);
    assert_eq!(probe.get_selected(), 3);
    click(2.0, 0.0);
    assert_eq!(probe.get_selected(), 3, "tile 2 is disabled");
    click(1.0, 1.0);
    assert_eq!((probe.get_selected(), probe.get_cursor()), (4, 4));
    probe.set_locked(true);
    click(1.0, 0.0);
    assert_eq!(probe.get_selected(), 4, "locked");
}

/// Fewer tiles than fit in a row: each tile takes a share of the row, not a 3-per-row cell.
#[test]
fn tile_gallery_short_gallery_stretches_its_tiles() {
    let probe = tile_gallery(2);
    // Cells 276 wide with 8 between: x 200 is inside tile 0, where 3 columns would put tile 1.
    click_at(&probe, 200.0, 20.0);
    assert_eq!(probe.get_selected(), 0, "tile 0 spans past the 181 px cell");
    click_at(&probe, 540.0, 20.0);
    assert_eq!(probe.get_selected(), 1, "tile 1 reaches the right edge");
}

#[test]
fn tile_gallery_opens_on_the_selected_tile() {
    let probe = tile_gallery_at(60, 31);
    // Row 10 ends at 1232, the box shows 368: rows are kept whole, so 8 rows up, not 7.7.
    let got = probe.get_scrolled();
    assert!((got - 896.0).abs() < 0.5, "scrolled {got}");
}

#[test]
fn path_field_elides_in_the_middle() {
    let _ = slint::platform::set_platform(Box::new(Shots));
    let probe = PathFieldProbe::new().unwrap();
    probe.global::<PathText>().on_elide(|path, budget| {
        oxi_core::paths::elide_path(&path, usize::try_from(budget).unwrap_or(0)).into()
    });
    probe.show().unwrap();
    slint::platform::update_timers_and_animations();
    // Root and file name whole, the middle elided (DS-13.1).
    let shown = probe.get_shown();
    assert!(
        shown.starts_with("C:\\…\\") && shown.ends_with("\\ggml-large-v3-turbo.bin"),
        "{shown}"
    );
    probe.set_path(r"C:\models\a.bin".into());
    slint::platform::update_timers_and_animations();
    assert_eq!(probe.get_shown(), r"C:\models\a.bin");
}

fn sub_tabs(selected: i32) -> SubTabsProbe {
    let _ = slint::platform::set_platform(Box::new(Shots));
    let probe = SubTabsProbe::new().unwrap();
    probe.set_selected(selected);
    probe.show().unwrap();
    LAST.with_borrow(Clone::clone)
        .unwrap()
        .set_size(PhysicalSize::new(480, 300));
    slint::platform::update_timers_and_animations();
    probe
}

#[test]
fn sub_tabs_walk_by_keyboard() {
    let probe = sub_tabs(0);
    assert!(!probe.get_ring(), "no focus yet");
    press(&probe, Key::Tab);
    assert!(probe.get_ring(), "Tab reaches the bar");
    press(&probe, Key::LeftArrow);
    assert_eq!(
        (probe.get_selected(), probe.get_picks()),
        (0, 0),
        "stops at the first tab"
    );
    press(&probe, Key::RightArrow);
    assert_eq!((probe.get_selected(), probe.get_picks()), (1, 1));
    press(&probe, Key::End);
    assert_eq!(probe.get_selected(), 2);
    press(&probe, Key::RightArrow);
    assert_eq!(
        (probe.get_selected(), probe.get_picks()),
        (2, 2),
        "stops at the last tab"
    );
    press(&probe, Key::Home);
    assert_eq!(probe.get_selected(), 0);
    press(&probe, Key::Tab);
    assert!(probe.get_draft_focused(), "Tab leaves for the open pane");
}

#[test]
fn sub_tab_keeps_its_state_while_another_is_open() {
    let probe = sub_tabs(0);
    press(&probe, Key::Tab);
    press(&probe, Key::Tab);
    press(&probe, "h");
    press(&probe, "i");
    assert_eq!(probe.get_draft(), "hi");
    press(&probe, Key::Backtab);
    press(&probe, Key::RightArrow);
    assert_eq!(probe.get_selected(), 1);
    press(&probe, Key::LeftArrow);
    assert_eq!(probe.get_draft(), "hi", "the hidden pane kept its text");
}

#[test]
fn hidden_sub_tab_is_out_of_the_tab_order_and_asks_for_no_room() {
    let probe = sub_tabs(1);
    assert!((probe.get_panes_min() - 200.0).abs() < 0.5);
    press(&probe, Key::Tab);
    press(&probe, Key::Tab);
    assert!(probe.get_other_focused(), "the open pane is next");
    press(&probe, Key::Tab);
    assert!(!probe.get_draft_focused(), "the hidden field is skipped");
    probe.set_selected(0);
    slint::platform::update_timers_and_animations();
    let got = probe.get_panes_min();
    assert!(got < 100.0, "the 200 px hidden pane still counts: {got}");
    let width = probe.get_draft_width();
    assert!(
        (width - 480.0).abs() < 0.5,
        "the open pane fills its host: {width}"
    );
}

/// UI rule 17 for the controls that call `focus()` from a click (its reason is not a pointer
/// click): a click picks and draws no ring, Tab after it does.
#[test]
fn clicks_on_segmented_tiles_and_slider_draw_no_ring() {
    let _ = slint::platform::set_platform(Box::new(Shots));
    let probe = ClickRingProbe::new().unwrap();
    probe.show().unwrap();
    LAST.with_borrow(Clone::clone)
        .unwrap()
        .set_size(PhysicalSize::new(480, 200));
    slint::platform::update_timers_and_animations();
    let click = |x: f32, y: f32| {
        let position = LogicalPosition::new(x, y);
        let button = PointerEventButton::Left;
        probe
            .window()
            .dispatch_event(WindowEvent::PointerPressed { position, button });
        probe
            .window()
            .dispatch_event(WindowEvent::PointerReleased { position, button });
        slint::platform::update_timers_and_animations();
    };
    click(360.0, 10.0);
    assert_eq!(probe.get_picked(), 1, "the click hit the second segment");
    assert!(
        !probe.get_segmented_ring(),
        "a click on a segment drew the ring"
    );
    click(100.0, probe.get_tiles_y() + 10.0);
    assert_eq!(probe.get_picked(), 0, "the click hit the first tile");
    assert!(!probe.get_tiles_ring(), "a click on a tile drew the ring");
    click(240.0, probe.get_slider_y());
    assert!(
        !probe.get_slider_ring(),
        "a click on the slider drew the ring"
    );
    press(&probe, Key::Backtab);
    assert!(
        probe.get_tiles_ring(),
        "Shift+Tab back to the tiles draws it"
    );
}

/// Item 16: a pick cross-fades the check badges with `motion-quick` (no badge popping in at once,
/// which read as a blink), and the first paint shows the selected tile's badge without a fade.
#[test]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // whole positive pixels
fn tile_pick_fades_the_check_badge_in() {
    let _ = slint::platform::set_platform(Box::new(Shots));
    let probe = ClickRingProbe::new().unwrap();
    probe.show().unwrap();
    let surface = LAST.with_borrow(Clone::clone).unwrap();
    surface.set_size(PhysicalSize::new(480, 200));
    slint::platform::update_timers_and_animations();
    let accent = probe.global::<Theme>().get_accent();
    let near = |a: u8, b: u8| a.abs_diff(b) <= 8;
    // Badge-coloured pixels in tile `i`'s top-right corner: two 236 px tiles, 8 px apart.
    let badge = |i: usize| {
        surface.request_redraw();
        let mut buffer = vec![PremultipliedRgbaColor::default(); 480 * 200];
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut buffer, 480);
        });
        let top = probe.get_tiles_y() as usize;
        let right = 236 + i * 244;
        let buffer = &buffer;
        (top..top + 30)
            .flat_map(|y| (right - 40..right).map(move |x| buffer[y * 480 + x]))
            .filter(|p| {
                near(p.red, accent.red())
                    && near(p.green, accent.green())
                    && near(p.blue, accent.blue())
            })
            .count()
    };
    assert!(badge(0) > 100, "the selected tile opened without its badge");
    assert_eq!(badge(1), 0, "an unpicked tile shows a badge");
    let position = LogicalPosition::new(360.0, probe.get_tiles_y() + 10.0);
    let button = PointerEventButton::Left;
    let window = probe.window();
    window.dispatch_event(WindowEvent::PointerPressed { position, button });
    window.dispatch_event(WindowEvent::PointerReleased { position, button });
    slint::platform::update_timers_and_animations();
    assert_eq!(probe.get_picked(), 1, "the click hit the second tile");
    assert_eq!(badge(1), 0, "the badge popped in at once");
    // Moves the animation clock on; a real-clock tick after it would go back in time.
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
    assert!(badge(1) > 100, "the badge never faded in");
    assert_eq!(badge(0), 0, "the old badge never faded out");
}

/// Item 16 (dark): pressing an unselected tile goes hover -> tint; the pressed fill is darker than
/// the card, so showing it made the pick blink dark between two brighter fills.
#[test]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)] // whole positive pixels
fn tile_press_never_dips_below_the_rest_fill_in_dark() {
    use slint::private_unstable_api::re_exports::{ColorScheme, WindowInner};

    let _ = slint::platform::set_platform(Box::new(Shots));
    let probe = ClickRingProbe::new().unwrap();
    probe.show().unwrap();
    let surface = LAST.with_borrow(Clone::clone).unwrap();
    surface.set_size(PhysicalSize::new(480, 200));
    WindowInner::from_pub(probe.window())
        .context()
        .set_color_scheme(ColorScheme::Dark);
    advance(1);
    let t = probe.global::<Theme>();
    assert!(t.get_dark(), "the probe is not dark");
    let light = |c: Color| u32::from(c.red()) + u32::from(c.green()) + u32::from(c.blue());
    let rest = light(t.get_bg_card());
    // Tile 1 (236 px wide, 8 px after tile 0), inside its top edge: no glyph, text or badge.
    let (x, y) = (244 + 30, probe.get_tiles_y() as usize + 3);
    let fill = || {
        surface.request_redraw();
        let mut buffer = vec![PremultipliedRgbaColor::default(); 480 * 200];
        surface.draw_if_needed(|renderer| {
            renderer.render(&mut buffer, 480);
        });
        let p = buffer[y * 480 + x];
        u32::from(p.red) + u32::from(p.green) + u32::from(p.blue)
    };
    assert_eq!(fill(), rest, "the tile does not rest on the card fill");
    let position = LogicalPosition::new(x as f32, y as f32);
    let button = PointerEventButton::Left;
    let window = probe.window();
    let mut lowest = rest;
    let step = |lowest: &mut u32| {
        for _ in 0..10 {
            advance(20);
            *lowest = (*lowest).min(fill());
        }
    };
    window.dispatch_event(WindowEvent::PointerMoved { position });
    step(&mut lowest);
    window.dispatch_event(WindowEvent::PointerPressed { position, button });
    step(&mut lowest);
    window.dispatch_event(WindowEvent::PointerReleased { position, button });
    step(&mut lowest);
    assert_eq!(probe.get_picked(), 1, "the click hit the second tile");
    assert!(
        lowest >= rest,
        "the fill dipped to {lowest}, rest is {rest}"
    );
}

/// UI rule 16: Remove turns red under the pointer, also as an icon-only button.
#[test]
fn destructive_icon_button_turns_red_on_hover() {
    let _ = slint::platform::set_platform(Box::new(Shots));
    let probe = IconDangerProbe::new().unwrap();
    probe.show().unwrap();
    slint::platform::update_timers_and_animations();
    assert!(!probe.get_red(), "red at rest");
    let position = LogicalPosition::new(16.0, 16.0);
    probe
        .window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    slint::platform::update_timers_and_animations();
    assert!(probe.get_red(), "no red under the pointer");
}

#[test]
fn sub_tab_click_picks_without_a_focus_ring() {
    let probe = sub_tabs(1);
    // Inside the first tab's left padding, whatever the font.
    let position = LogicalPosition::new(6.0, 10.0);
    let button = PointerEventButton::Left;
    let window = probe.window();
    window.dispatch_event(WindowEvent::PointerPressed { position, button });
    window.dispatch_event(WindowEvent::PointerReleased { position, button });
    slint::platform::update_timers_and_animations();
    assert_eq!(probe.get_selected(), 0);
    assert!(!probe.get_ring(), "a click draws no ring");
    press(&probe, Key::RightArrow);
    assert_eq!(
        probe.get_selected(),
        1,
        "the click left the keys on the bar"
    );
}

fn shown_path(display: &str) -> controls::PathProbe {
    let _ = slint::platform::set_platform(Box::new(Shots));
    let probe = controls::PathProbe::new().unwrap();
    probe.set_display(display.into());
    probe.show().unwrap();
    slint::platform::update_timers_and_animations();
    probe
}

/// DS-13.3: a shown default reads as a default (secondary ink); a real path stays primary.
#[test]
fn path_field_default_is_secondary_ink() {
    let default = shown_path("Default folder");
    assert_eq!(default.get_ink(), default.get_secondary());
    assert_ne!(default.get_ink(), shown_path("").get_ink());
}

/// DS-7.4: Browse is a Button with the `browse-folder` glyph (DS section 24).
#[test]
fn path_field_browse_has_the_folder_glyph() {
    assert!(shown_path("").get_browse_icon(), "Browse has no glyph");
}

/// DS-20.6: the picked spot is the tint plus the accent mark, never a coloured outline: its stroke
/// is the same as a plain tile's.
#[test]
fn position_tiles_selected_has_no_coloured_outline() {
    let _ = slint::platform::set_platform(Box::new(Shots));
    let probe = controls::TilesProbe::new().unwrap();
    let surface = LAST.with_borrow(Clone::clone).unwrap();
    surface.set_size(PhysicalSize::new(400, 100));
    probe.show().unwrap();
    slint::platform::update_timers_and_animations();
    // Tile 0 is picked, tile 1 is not: the pixel under each one's left edge, at y 10.
    surface.request_redraw();
    let mut buffer = vec![PremultipliedRgbaColor::default(); 400 * 100];
    assert!(surface.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, 400);
    }));
    let at = |x: usize| {
        let p = buffer[10 * 400 + x];
        [p.red, p.green, p.blue]
    };
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a small positive tile pitch in pixels"
    )]
    let next = probe.get_next_x().round() as usize;
    assert_eq!(at(0), at(next), "the picked tile has a stroke of its own");
}

/// A window coming back to the front (Alt+Tab) is not a key press: no ring. Tab still draws it.
#[test]
fn window_activation_draws_no_focus_ring() {
    let _ = slint::platform::set_platform(Box::new(Shots));
    let probe = controls::FocusProbe::new().unwrap();
    probe.show().unwrap();
    slint::platform::update_timers_and_animations();
    probe
        .window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    press(&probe, Key::Tab);
    assert!(probe.get_ring(), "Tab draws no ring");
    probe
        .window()
        .dispatch_event(WindowEvent::WindowActiveChanged(false));
    probe
        .window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    slint::platform::update_timers_and_animations();
    assert!(!probe.get_ring(), "the ring came back with the window");
}

/// Closing a popup with Esc hands the focus back to the opener: a keyboard user keeps the ring.
#[test]
fn popup_closed_by_keyboard_keeps_the_focus_ring() {
    let _ = slint::platform::set_platform(Box::new(Shots));
    let probe = controls::PopupProbe::new().unwrap();
    probe.show().unwrap();
    slint::platform::update_timers_and_animations();
    press(&probe, Key::Tab);
    assert!(probe.get_ring(), "Tab draws no ring");
    press(&probe, Key::RightArrow);
    press(&probe, Key::Escape);
    assert!(probe.get_ring(), "the ring went with the popup");
}

/// Renders `probe`'s window once at `size`; the returned closure reads the pixel at (x, y) as [r, g, b].
fn pixels(probe: &impl ComponentHandle, size: (u32, u32)) -> impl Fn(u32, u32) -> [u8; 3] {
    let surface = LAST.with_borrow(Clone::clone).unwrap();
    surface.set_size(PhysicalSize::new(size.0, size.1));
    probe.show().unwrap();
    slint::platform::update_timers_and_animations();
    surface.request_redraw();
    let mut buffer = vec![PremultipliedRgbaColor::default(); (size.0 * size.1) as usize];
    assert!(surface.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, size.0 as usize);
    }));
    move |x, y| {
        let p = buffer[(y * size.0 + x) as usize];
        [p.red, p.green, p.blue]
    }
}

fn rgb(c: Color) -> [u8; 3] {
    [c.red(), c.green(), c.blue()]
}

/// DS-28.4, 02:8: tiles are 112 x 32, 12 apart across and 8 down; a click on the picked tile fires.
#[test]
fn position_tiles_fire_on_the_picked_tile_and_keep_the_grid() {
    let _ = slint::platform::set_platform(Box::new(Shots));
    let probe = PickProbe::new().unwrap();
    probe.show().unwrap();
    slint::platform::update_timers_and_animations();
    // Tile 0 is the picked one.
    click_at(&probe, 56.0, 16.0);
    assert_eq!((probe.get_count(), probe.get_last()), (1, 0), "picked tile");
    // The 12 px gap and the 8 px row gap are no tile.
    click_at(&probe, 118.0, 16.0);
    click_at(&probe, 56.0, 36.0);
    assert_eq!(probe.get_count(), 1, "a gap fired");
    // Tile 1 starts at x 124, tile 5 at (248, 40) and ends at (360, 72).
    click_at(&probe, 125.0, 1.0);
    assert_eq!((probe.get_count(), probe.get_last()), (2, 1), "tile 1");
    click_at(&probe, 359.0, 71.0);
    assert_eq!((probe.get_count(), probe.get_last()), (3, 5), "tile 5");
}

/// DS-21.3, 02:9: overloaded, the Meter has the 2 px clip line at its top edge and error-coloured
/// bars; at rest it has neither.
#[test]
fn meter_overload_draws_the_clip_line_and_error_bars() {
    let _ = slint::platform::set_platform(Box::new(Shots));
    for overloaded in [false, true] {
        let probe = MeterProbe::new().unwrap();
        probe.set_overloaded(overloaded);
        let at = pixels(&probe, (300, 32));
        let (error, accent) = (rgb(probe.get_error()), rgb(probe.get_accent()));
        let row = |y: u32, colour: [u8; 3]| (0..300).any(|x| at(x, y) == colour);
        assert_eq!(row(0, error) && row(1, error), overloaded, "clip line");
        assert!(!row(2, error), "the line is 2 px");
        assert_eq!(row(16, error), overloaded, "bars in the error colour");
        assert_eq!(row(16, accent), !overloaded, "bars in the accent");
    }
}

/// DS-28.6, 02:10: a live day outlines on hover, shows its tip and reports a click; an empty one
/// and the gap between cells do nothing.
#[test]
fn heatmap_days_hover_and_click() {
    let _ = slint::platform::set_platform(Box::new(Shots));
    let probe = HeatProbe::new().unwrap();
    probe.show().unwrap();
    advance(0);
    let window = probe.window();
    let top = probe.get_top().round();
    let hover = |x: f32, y: f32| {
        window.dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(x, y),
        });
        advance(700);
        (probe.get_hot(), probe.global::<HoverTip>().get_text())
    };
    // Day i of week w, row r: x 32 + 20 w, y top + 20 r.
    let tip_at = || {
        let tip = probe.global::<HoverTip>();
        (tip.get_x(), tip.get_y())
    };
    assert_eq!(hover(40.0, top + 28.0), (1, "one".into()), "day 1");
    let (x1, y1) = tip_at();
    assert_eq!(hover(40.0, top + 48.0), (2, "two".into()), "day 2");
    // The open tip follows the cell when the pointer hops to the next live day.
    assert_eq!(tip_at(), (x1, y1 + 20.0), "tip moves one row down");
    assert_eq!(hover(60.0, top + 8.0), (7, "week two".into()), "day 7");
    assert_eq!(tip_at(), (x1 + 20.0, y1 - 20.0), "tip moves one week right");
    assert_eq!(
        hover(40.0, top + 8.0),
        (-1, SharedString::new()),
        "empty day"
    );
    assert_eq!(hover(49.0, top + 28.0).0, -1, "the gap between cells");
    assert_eq!(hover(10.0, top + 28.0).0, -1, "the day labels");
    hover(40.0, top + 28.0);
    click_at(&probe, 40.0, top + 8.0);
    assert_eq!(probe.get_clicked_day(), -1, "an empty day is inert");
    click_at(&probe, 40.0, top + 28.0);
    assert_eq!(probe.get_clicked_day(), 1, "a live day reports itself");
}

/// DS-28.3, 02:11: the kept-draft dot is the accent, inside the button's top-right corner.
#[test]
fn icon_button_kept_dot_is_the_accent_in_the_corner() {
    let _ = slint::platform::set_platform(Box::new(Shots));
    for kept in [false, true] {
        let probe = DotProbe::new().unwrap();
        probe.set_kept(kept);
        let at = pixels(&probe, (32, 32));
        let accent = rgb(probe.get_accent());
        // The 6 px dot spans x 22..28, y 4..10.
        assert_eq!(at(25, 7) == accent, kept, "dot centre");
        assert_ne!(at(30, 2), accent, "outside the dot");
    }
}

/// DS-1, 02:12: in a Group title row the icon and the "i" share the vertical centre of the trailing
/// action button (the tallest item), not the row's top.
#[test]
fn group_title_icon_is_centred_with_the_action_button() {
    let _ = slint::platform::set_platform(Box::new(Shots));
    let probe = GroupProbe::new().unwrap();
    let at = pixels(&probe, (340, 80));
    let bg = at(339, 0);
    // The rows (first, last) where some pixel in columns `xs` differs from the window.
    let span = |xs: std::ops::Range<u32>| {
        let rows: Vec<u32> = (0..80)
            .filter(|&y| xs.clone().any(|x| at(x, y) != bg))
            .collect();
        (*rows.first().unwrap(), *rows.last().unwrap())
    };
    // The button ends at x 300: its last 4 columns are its fill. The icon is the first 20.
    let (b0, b1) = span(296..300);
    let (i0, i1) = span(0..20);
    assert!(b1 - b0 >= 31, "the button is 32 px high: {b0}..{b1}");
    let (bc, ic) = (b0 + b1, i0 + i1);
    assert!(
        bc.abs_diff(ic) <= 1,
        "icon rows {i0}..{i1}, button rows {b0}..{b1}"
    );
}

/// DS-28.6, 02:12: the legend is one 16 px line under the grid, whatever the cell size.
#[test]
fn heatmap_legend_is_one_caption_line_not_a_cell() {
    let _ = slint::platform::set_platform(Box::new(Shots));
    let probe = HeatSizeProbe::new().unwrap();
    probe.show().unwrap();
    assert_eq!(
        probe.get_total(),
        probe.get_grid(),
        "no legend: the grid only"
    );
    probe.set_legend(true);
    assert_eq!(
        probe.get_total(),
        probe.get_grid() + probe.get_gap() + 16.0,
        "legend: a gap and a 16 px line"
    );
}

fn combo_probe() -> ComboProbe {
    let _ = slint::platform::set_platform(Box::new(Shots));
    let probe = ComboProbe::new().unwrap();
    probe.show().unwrap();
    LAST.with_borrow(Clone::clone)
        .unwrap()
        .set_size(PhysicalSize::new(400, 300));
    slint::platform::update_timers_and_animations();
    // A click opens the list.
    click_at(&probe, 100.0, 16.0);
    assert!(probe.get_open());
    probe
}

/// `committed` is Return, Space or a click on a line: not arrows, Esc or a click outside.
#[test]
fn combo_commits_on_return_space_and_a_line_click_only() {
    let probe = combo_probe();
    press(&probe, Key::DownArrow);
    assert_eq!((probe.get_index(), probe.get_commits()), (1, 0), "arrow");
    press(&probe, Key::Escape);
    assert!(!probe.get_open());
    assert_eq!(probe.get_commits(), 0, "Esc");

    click_at(&probe, 100.0, 16.0);
    press(&probe, Key::DownArrow);
    click_at(&probe, 390.0, 290.0);
    assert!(!probe.get_open());
    assert_eq!(probe.get_commits(), 0, "click outside");

    click_at(&probe, 100.0, 16.0);
    press(&probe, Key::Return);
    assert_eq!(probe.get_commits(), 1, "Return");

    click_at(&probe, 100.0, 16.0);
    press(&probe, " ");
    assert_eq!(probe.get_commits(), 2, "Space");

    click_at(&probe, 100.0, 16.0);
    // Row 0 sits under the field: 32 px field, 4 px gap, 4 px list padding, rows of 32 px.
    click_at(&probe, 100.0, 32.0 + 8.0 + 16.0);
    assert_eq!((probe.get_index(), probe.get_commits()), (0, 3), "a line");
}
