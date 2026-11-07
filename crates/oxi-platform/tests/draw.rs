use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::PathBuf;

use ab_glyph::FontRef;
use oxi_core::audio::Level;
use oxi_core::config::OverlayPosition;
use oxi_core::machine::{Notice, OverlayState, Phase};
use oxi_core::overlay::{CLIP_HOLD, Meter, Rect, Rgba, palette, place};
use oxi_core::tray::TrayBadge;
use oxi_core::types::Millis;
use oxi_platform::draw::{PillCanvas, bgra, bgra_into, logo, logo_svg, pill, tray_icon};
use tiny_skia::{Pixmap, PixmapPaint, Transform};

const FONT: &str = "C:/Windows/Fonts/segoeui.ttf";
const SCALES: [f32; 3] = [1.0, 1.5, 2.0];
const NOTICES: [Notice; 15] = [
    Notice::Cancelled,
    Notice::NothingHeard,
    Notice::NoModel,
    Notice::ModelBad,
    Notice::DeviceLost,
    Notice::CaptureFailed,
    Notice::TranscriptionFailed,
    Notice::PressPaste,
    Notice::AdminWindow,
    Notice::RestoreSkipped,
    Notice::PasteFailed,
    Notice::MicFallback,
    Notice::ConfigInvalid,
    Notice::ConfigAdjusted,
    Notice::MicSilence,
];

fn font_bytes() -> Option<Vec<u8>> {
    std::fs::read(FONT).ok()
}

fn rect(scale: f32) -> Rect {
    let screen = Rect {
        x: 0,
        y: 0,
        w: 10_000,
        h: 10_000,
    };
    place(OverlayPosition::TopLeft, screen, scale)
}

/// Every visible state with a file-name slug.
fn states() -> Vec<(String, OverlayState)> {
    let mut v = vec![];
    for (locked, ending_soon, name) in [
        (false, false, "recording"),
        (true, false, "recording-locked"),
        (false, true, "recording-ending"),
        (true, true, "recording-locked-ending"),
    ] {
        v.push((
            name.to_owned(),
            OverlayState::Recording {
                locked,
                ending_soon,
            },
        ));
    }
    v.push(("loading-model".into(), OverlayState::LoadingModel));
    v.push(("transcribing".into(), OverlayState::Transcribing));
    v.push(("pasting".into(), OverlayState::Pasting));
    for n in NOTICES {
        let slug = format!("{n:?}")
            .chars()
            .enumerate()
            .flat_map(|(i, c)| {
                let dash = (i > 0 && c.is_uppercase()).then_some('-');
                dash.into_iter().chain(c.to_lowercase())
            })
            .collect();
        v.push((slug, OverlayState::Text(n)));
    }
    v
}

/// A meter after 2 s of speech-like levels; `peak` is the peak of every block.
fn speech(peak: f32) -> (Meter, Millis) {
    let mut m = Meter::new(40, 0.0);
    let mut t = 0;
    for i in 0..60u16 {
        let rms = 0.02 + 0.3 * (f32::from(i) * 0.45).sin().abs();
        m.push(Level { peak, rms }, Millis(t));
        t += 34;
    }
    (m, Millis(t))
}

fn alpha(pm: &Pixmap, x: u32, y: u32) -> u8 {
    pm.pixel(x, y).unwrap().alpha()
}

fn count(pm: &Pixmap, c: Rgba) -> usize {
    pm.pixels()
        .iter()
        .filter(|p| (p.red(), p.green(), p.blue(), p.alpha()) == (c.0, c.1, c.2, c.3))
        .count()
}

#[test]
fn corners_are_transparent_and_centre_is_opaque_with_and_without_font() {
    let bytes = font_bytes();
    let font = bytes
        .as_deref()
        .map(|b| FontRef::try_from_slice(b).unwrap());
    let (meter, now) = speech(0.5);
    for scale in SCALES {
        for (name, state) in states() {
            for f in [font.as_ref(), None] {
                let pm = pill(state, &meter, now, rect(scale), f).unwrap();
                let (w, h) = (pm.width(), pm.height());
                assert_eq!(
                    (i64::from(w), i64::from(h)),
                    (rect(scale).w.into(), rect(scale).h.into())
                );
                for (x, y) in [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)] {
                    assert_eq!(alpha(&pm, x, y), 0, "{name} @{scale} corner {x},{y}");
                }
                assert_eq!(alpha(&pm, w / 2, h / 2), 255, "{name} @{scale} centre");
            }
        }
    }
}

#[test]
fn hidden_is_fully_transparent() {
    let (meter, now) = speech(0.5);
    let pm = pill(OverlayState::Hidden, &meter, now, rect(1.0), None).unwrap();
    assert!(pm.pixels().iter().all(|p| p.alpha() == 0));
}

#[test]
fn clipping_turns_the_bars_red_until_the_hold_ends() {
    let rec = OverlayState::Recording {
        locked: false,
        ending_soon: false,
    };
    let (calm, now) = speech(0.5);
    let (clipped, _) = speech(1.0);
    let normal = pill(rec, &calm, now, rect(1.0), None).unwrap();
    let red = pill(rec, &clipped, now, rect(1.0), None).unwrap();
    let after = pill(rec, &clipped, now + CLIP_HOLD, rect(1.0), None).unwrap();

    assert_eq!(count(&normal, palette::CLIP), 0);
    assert!(count(&red, palette::CLIP) > 50, "bars are not red");
    assert!(count(&red, palette::RECORDING) < count(&normal, palette::RECORDING));
    assert_eq!(count(&after, palette::CLIP), 0);
}

#[test]
fn loud_bars_are_taller_than_silence() {
    let rec = OverlayState::Recording {
        locked: false,
        ending_soon: false,
    };
    let quiet = Meter::new(40, 0.0);
    let (loud, now) = speech(0.5);
    let q = pill(rec, &quiet, now, rect(1.0), None).unwrap();
    let l = pill(rec, &loud, now, rect(1.0), None).unwrap();
    assert!(count(&l, palette::RECORDING) > 2 * count(&q, palette::RECORDING));
}

#[test]
fn label_is_drawn_when_the_font_is_there() {
    let Some(bytes) = font_bytes() else {
        eprintln!("skip: no {FONT}");
        return;
    };
    let font = FontRef::try_from_slice(&bytes).unwrap();
    let (meter, now) = speech(0.5);
    for (name, state) in states() {
        let with = pill(state, &meter, now, rect(1.0), Some(&font)).unwrap();
        let without = pill(state, &meter, now, rect(1.0), None).unwrap();
        assert_ne!(with.data(), without.data(), "{name}: no label");
    }
}

const TRAY_SIZES: [u32; 4] = [16, 20, 24, 32];
/// The sizes in `assets/oxi.ico`.
const ICO_SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

fn alphas(pm: &Pixmap) -> Vec<u8> {
    pm.pixels().iter().map(|c| c.alpha()).collect()
}

/// A pixel with a hue: any channel more than 40 apart from another.
fn coloured(c: tiny_skia::PremultipliedColorU8) -> bool {
    let c = c.demultiply();
    let (lo, hi) = [c.red(), c.green(), c.blue()]
        .into_iter()
        .fold((255, 0), |(lo, hi), v| (lo.min(v), hi.max(v)));
    c.alpha() > 0 && hi - lo > 40
}

#[test]
fn tray_states_differ_in_shape_at_every_size_and_taskbar() {
    for light in [false, true] {
        for size in TRAY_SIZES {
            let shape = |p: Phase| alphas(&tray_icon(p, size, light, TrayBadge::None).unwrap());
            let (idle, rec, busy) = (
                shape(Phase::Idle),
                shape(Phase::Recording),
                shape(Phase::Transcribing),
            );
            assert_ne!(idle, rec, "{size} light={light}");
            assert_ne!(rec, busy, "{size} light={light}");
            assert_ne!(idle, busy, "{size} light={light}");
            for icon in [&idle, &rec, &busy] {
                assert!(icon.contains(&255), "a solid pixel at {size} px");
                assert_eq!(icon[0], 0, "corner is transparent");
            }
        }
    }
    assert!(tray_icon(Phase::Idle, 0, false, TrayBadge::None).is_none());
}

#[test]
fn only_recording_and_badges_have_colour() {
    for light in [false, true] {
        for size in TRAY_SIZES {
            let hues = |p: Phase, b: TrayBadge| {
                let pm = tray_icon(p, size, light, b).unwrap();
                pm.pixels().iter().filter(|&&c| coloured(c)).count()
            };
            assert_eq!(hues(Phase::Idle, TrayBadge::None), 0);
            assert_eq!(hues(Phase::Transcribing, TrayBadge::None), 0);
            assert!(hues(Phase::Recording, TrayBadge::None) > 20);
            assert!(hues(Phase::Idle, TrayBadge::Error) > 4);
            assert!(hues(Phase::Idle, TrayBadge::Update) > 4);
        }
    }
}

#[test]
fn glyphs_are_dark_on_a_light_taskbar_and_white_on_a_dark_one() {
    for (light, want) in [(true, palette::INK), (false, palette::WHITE)] {
        for phase in [Phase::Idle, Phase::Transcribing] {
            let pm = tray_icon(phase, 16, light, TrayBadge::None).unwrap();
            let solid: Vec<_> = pm.pixels().iter().filter(|c| c.alpha() == 255).collect();
            assert_ne!(solid.len(), 0);
            for c in solid {
                assert_eq!((c.red(), c.green(), c.blue()), (want.0, want.1, want.2));
            }
        }
    }
}

#[test]
fn the_error_badge_sits_bottom_right_in_a_clear_ring() {
    for size in TRAY_SIZES {
        let pm = tray_icon(Phase::Transcribing, size, false, TrayBadge::Error).unwrap();
        let red = |x: u32, y: u32| {
            let c = pm.pixel(x, y).unwrap();
            (c.red(), c.green(), c.blue(), c.alpha())
                == (palette::ERROR.0, palette::ERROR.1, palette::ERROR.2, 255)
        };
        let half = size / 2;
        let reds: Vec<_> = (0..size)
            .flat_map(|y| (0..size).map(move |x| (x, y)))
            .filter(|&(x, y)| red(x, y))
            .collect();
        assert!(!reds.is_empty(), "{size}");
        assert!(reds.iter().all(|&(x, y)| x >= half && y >= half), "{size}");
        // Left of the badge, on its middle row, the filled bubble is cut away before the red.
        let (_, y) = reds[reds.len() / 2];
        let x = reds.iter().filter(|r| r.1 == y).map(|r| r.0).min().unwrap();
        let gap = (x - 3..x).map(|x| pm.pixel(x, y).unwrap().alpha()).min();
        assert!(gap < Some(64), "{size}: no ring, {gap:?}");
    }
}

#[test]
fn bgra_swaps_red_and_blue() {
    let pm = tray_icon(Phase::Recording, 16, false, TrayBadge::None).unwrap();
    let out = bgra(&pm);
    // Inside the bubble, left of the bars.
    let i = (7 * 16 + 2) * 4;
    let c = palette::RECORDING;
    assert_eq!(&out[i..i + 4], &[c.2, c.1, c.0, c.3]);
}

#[test]
fn the_logo_is_a_white_mark_on_a_teal_tile_with_clear_corners() {
    for size in ICO_SIZES {
        let pm = logo(size).unwrap();
        assert_eq!(pm.pixel(0, 0).unwrap().alpha(), 0, "{size}");
        let white = pm
            .pixels()
            .iter()
            .filter(|c| (c.red(), c.green(), c.blue(), c.alpha()) == (255, 255, 255, 255))
            .count();
        let teal = pm
            .pixels()
            .iter()
            .filter(|&&c| coloured(c) && c.alpha() == 255)
            .count();
        let area = (size * size) as usize;
        assert!(
            white > area / 6 && teal > area / 3,
            "{size}: {white} {teal}"
        );
    }
    assert!(logo(0).is_none());
}

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The app icon file: one PNG per size (Windows Vista and later read PNG entries).
fn ico() -> Vec<u8> {
    let pngs: Vec<Vec<u8>> = ICO_SIZES
        .iter()
        .map(|&s| logo(s).unwrap().encode_png().unwrap())
        .collect();
    let count = u16::try_from(pngs.len()).unwrap();
    let mut out = [0u16, 1, count].map(u16::to_le_bytes).concat();
    let mut offset = 6 + 16 * pngs.len();
    for (&size, png) in ICO_SIZES.iter().zip(&pngs) {
        let side = u8::try_from(size).unwrap_or(0); // 0 stands for 256
        out.extend([side, side, 0, 0]);
        out.extend([1u16, 32].map(u16::to_le_bytes).concat()); // planes, bits per pixel
        out.extend(u32::try_from(png.len()).unwrap().to_le_bytes());
        out.extend(u32::try_from(offset).unwrap().to_le_bytes());
        offset += png.len();
    }
    out.extend(pngs.concat());
    out
}

/// The images of an icon file of PNG entries, in file order.
fn ico_images(file: &[u8]) -> Vec<Pixmap> {
    let word = |at: usize| usize::from(u16::from_le_bytes([file[at], file[at + 1]]));
    let dword = |at: usize| u32::from_le_bytes(file[at..at + 4].try_into().unwrap()) as usize;
    assert_eq!((word(0), word(2)), (0, 1), "an icon file");
    (0..word(4))
        .map(|i| {
            let entry = 6 + 16 * i;
            let (len, at) = (dword(entry + 8), dword(entry + 12));
            Pixmap::decode_png(&file[at..at + len]).unwrap()
        })
        .collect()
}

/// The committed `assets/oxi.ico` and `logo.svg` are what `logo` draws now; after a change of
/// the mark, run `regen_icons`.
#[test]
fn committed_icons_match_the_drawing() {
    let committed = std::fs::read(repo().join("assets/oxi.ico")).unwrap();
    let images = ico_images(&committed);
    assert_eq!(images.len(), ICO_SIZES.len());
    for (img, size) in images.iter().zip(ICO_SIZES) {
        // Through PNG and back, as the committed one went.
        let drawn = Pixmap::decode_png(&logo(size).unwrap().encode_png().unwrap()).unwrap();
        assert_eq!(
            img.data(),
            drawn.data(),
            "{size} px differs: run regen_icons"
        );
    }
    let svg = std::fs::read_to_string(repo().join("crates/oxi-settings/ui/theme/logo.svg"))
        .unwrap()
        .replace("\r\n", "\n");
    assert_eq!(svg, logo_svg(), "logo.svg differs: run regen_icons");
}

/// Writes `assets/oxi.ico` and the settings `logo.svg` from `logo`:
/// `cargo test -p oxi-platform --test draw regen_icons -- --ignored`.
#[test]
#[ignore = "writes into the source tree; run by hand after changing the mark"]
fn regen_icons() {
    std::fs::write(repo().join("assets/oxi.ico"), ico()).unwrap();
    std::fs::write(
        repo().join("crates/oxi-settings/ui/theme/logo.svg"),
        logo_svg(),
    )
    .unwrap();
}

/// `pm` scaled up `n` times, nearest neighbour, for looking at single pixels.
fn magnified(pm: &Pixmap, n: u32) -> Pixmap {
    let mut out = Pixmap::new(pm.width() * n, pm.height() * n).unwrap();
    let paint = PixmapPaint {
        quality: tiny_skia::FilterQuality::Nearest,
        ..PixmapPaint::default()
    };
    out.draw_pixmap(
        0,
        0,
        pm.as_ref(),
        &paint,
        Transform::from_scale(n as f32, n as f32),
        None,
    );
    out
}

fn rgb(hex: u32) -> tiny_skia::Paint<'static> {
    let mut paint = tiny_skia::Paint::default();
    let [_, red, green, blue] = hex.to_be_bytes();
    paint.set_color_rgba8(red, green, blue, 255);
    paint
}

/// Every tray state at 16 / 20 / 24 / 32 px on white, light grey, dark, blue accent and purple
/// accent taskbars (rows: states; column groups: taskbars), and the app icon at every size on
/// light and dark, both magnified 2x.
fn icon_sheets(dir: &std::path::Path) {
    const CELL: i32 = 40;
    let taskbars = [
        (0xFF_FFFF, true),
        (0xF3_F3F3, true),
        (0x20_2020, false),
        (0x00_5FB8, false),
        (0x87_64B8, false),
    ];
    let states = [
        (Phase::Idle, TrayBadge::None),
        (Phase::Recording, TrayBadge::None),
        (Phase::Transcribing, TrayBadge::None),
        (Phase::Idle, TrayBadge::Error),
        (Phase::Recording, TrayBadge::Error),
        (Phase::Transcribing, TrayBadge::Error),
        (Phase::Idle, TrayBadge::Update),
    ];
    let group = CELL * 4;
    let mut sheet = Pixmap::new(
        u32::try_from(group * 5).unwrap(),
        u32::try_from(CELL * 7).unwrap(),
    )
    .unwrap();
    for (t, (bg, light)) in (0i32..).zip(taskbars) {
        let area = tiny_skia::Rect::from_xywh(
            (t * group) as f32,
            0.0,
            group as f32,
            sheet.height() as f32,
        )
        .unwrap();
        sheet.fill_rect(area, &rgb(bg), Transform::identity(), None);
        for (row, (phase, badge)) in (0i32..).zip(states) {
            for (col, size) in (0i32..).zip(TRAY_SIZES) {
                let icon = tray_icon(phase, size, light, badge).unwrap();
                let pad = (CELL - i32::try_from(size).unwrap()) / 2;
                sheet.draw_pixmap(
                    t * group + col * CELL + pad,
                    row * CELL + pad,
                    icon.as_ref(),
                    &PixmapPaint::default(),
                    Transform::identity(),
                    None,
                );
            }
        }
    }
    magnified(&sheet, 2)
        .save_png(dir.join("tray-sheet-2x.png"))
        .unwrap();

    let small = &ICO_SIZES[..7];
    let width: u32 = small.iter().map(|s| s + 16).sum::<u32>() + 16;
    let mut logos = Pixmap::new(width, 2 * 80).unwrap();
    for (row, bg) in [(0u32, 0xFF_FFFF), (1, 0x20_2020)] {
        let area = tiny_skia::Rect::from_xywh(0.0, (row * 80) as f32, width as f32, 80.0).unwrap();
        logos.fill_rect(area, &rgb(bg), Transform::identity(), None);
        let mut x = 16;
        for &size in small {
            logos.draw_pixmap(
                i32::try_from(x).unwrap(),
                i32::try_from(row * 80 + (80 - size) / 2).unwrap(),
                logo(size).unwrap().as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
            x += size + 16;
        }
    }
    magnified(&logos, 2)
        .save_png(dir.join("logo-sheet-2x.png"))
        .unwrap();
    logo(256)
        .unwrap()
        .save_png(dir.join("logo-256.png"))
        .unwrap();
}

#[test]
fn a_reused_canvas_draws_what_a_fresh_pill_does() {
    let bytes = font_bytes();
    let font = bytes
        .as_deref()
        .map(|b| FontRef::try_from_slice(b).unwrap());
    let mut canvas = PillCanvas::new(font.clone());
    let (meter, now) = speech(0.5);
    for scale in [1.0, 2.0, 2.0, 1.0] {
        for (name, s) in states() {
            let fresh = pill(s, &meter, now, rect(scale), font.as_ref()).unwrap();
            let reused = canvas.draw(s, &meter, now, rect(scale)).unwrap();
            assert_eq!(reused.data(), fresh.data(), "{name} @{scale}");
        }
    }
}

/// Counts the allocations of the current thread only, so parallel tests do not add to it.
struct Counting;

thread_local! {
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: every call goes to the system allocator unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCS.try_with(|n| n.set(n.get() + 1));
        // SAFETY: the caller's contract, passed on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as above.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// The overlay's 30 Hz tick: new levels, a frame, its BGRA copy into the window's buffer.
#[test]
fn a_recording_frame_allocates_nothing_once_its_state_is_drawn() {
    let bytes = font_bytes();
    let font = bytes
        .as_deref()
        .map(|b| FontRef::try_from_slice(b).unwrap());
    let mut canvas = PillCanvas::new(font);
    let (mut meter, mut now) = speech(0.5);
    let rec = OverlayState::Recording {
        locked: false,
        ending_soon: false,
    };
    let r = rect(1.5);
    let mut dib = vec![0; canvas.draw(rec, &meter, now, r).unwrap().data().len()];
    let before = ALLOCS.with(Cell::get);
    for i in 0..90u16 {
        // Every 30th block clips, so both bar colours are drawn.
        let peak = if i % 30 == 0 { 1.0 } else { 0.4 };
        let rms = f32::from(i % 7) * 0.05;
        meter.push(Level { peak, rms }, now);
        now = now + Millis(33);
        bgra_into(canvas.draw(rec, &meter, now, r).unwrap(), &mut dib);
    }
    assert_eq!(ALLOCS.with(Cell::get) - before, 0);
}

/// With `OXI_SHOTS_DIR` set: every state at every scale, one sheet per scale on light and dark
/// backgrounds for the eye, and the tray and app icon sheets.
#[test]
fn shots() {
    let Some(dir) = std::env::var_os("OXI_SHOTS_DIR").map(PathBuf::from) else {
        return;
    };
    std::fs::create_dir_all(&dir).unwrap();
    let bytes = font_bytes();
    let font = bytes
        .as_deref()
        .map(|b| FontRef::try_from_slice(b).unwrap());
    let (meter, now) = speech(0.5);
    let (clipped, _) = speech(1.0);
    let rec = OverlayState::Recording {
        locked: false,
        ending_soon: false,
    };
    for scale in SCALES {
        let r = rect(scale);
        let mut shots: Vec<(String, Pixmap)> = states()
            .into_iter()
            .map(|(name, s)| (name, pill(s, &meter, now, r, font.as_ref()).unwrap()))
            .collect();
        shots.push((
            "recording-clipping".into(),
            pill(rec, &clipped, now, r, font.as_ref()).unwrap(),
        ));
        shots.push((
            "recording-nofont".into(),
            pill(rec, &meter, now, r, None).unwrap(),
        ));
        for (name, pm) in &shots {
            pm.save_png(dir.join(format!("overlay-{name}-{scale}.png")))
                .unwrap();
        }
        let gap = r.h / 4;
        let (cw, ch) = (r.w + 2 * gap, r.h + gap);
        let rows = i32::try_from(shots.len()).unwrap();
        let mut sheet = Pixmap::new(
            u32::try_from(2 * cw).unwrap(),
            u32::try_from(rows * ch + gap).unwrap(),
        )
        .unwrap();
        sheet.fill_rect(
            tiny_skia::Rect::from_xywh(0.0, 0.0, cw as f32, sheet.height() as f32).unwrap(),
            &paint(0xF3),
            Transform::identity(),
            None,
        );
        sheet.fill_rect(
            tiny_skia::Rect::from_xywh(cw as f32, 0.0, cw as f32, sheet.height() as f32).unwrap(),
            &paint(0x20),
            Transform::identity(),
            None,
        );
        for (i, (_, pm)) in shots.iter().enumerate() {
            let y = gap + i32::try_from(i).unwrap() * ch;
            for x in [gap, cw + gap] {
                sheet.draw_pixmap(
                    x,
                    y,
                    pm.as_ref(),
                    &PixmapPaint::default(),
                    Transform::identity(),
                    None,
                );
            }
        }
        sheet
            .save_png(dir.join(format!("overlay-sheet-{scale}.png")))
            .unwrap();
    }
    icon_sheets(&dir);
}

fn paint(grey: u8) -> tiny_skia::Paint<'static> {
    let mut p = tiny_skia::Paint::default();
    p.set_color_rgba8(grey, grey, grey, 255);
    p
}
