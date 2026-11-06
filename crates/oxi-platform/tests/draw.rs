use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::PathBuf;

use ab_glyph::FontRef;
use oxi_core::audio::Level;
use oxi_core::config::OverlayPosition;
use oxi_core::machine::{Notice, OverlayState, Phase};
use oxi_core::overlay::{CLIP_HOLD, Meter, Rect, Rgba, palette, place};
use oxi_core::types::Millis;
use oxi_platform::draw::{PillCanvas, bgra, bgra_into, pill, tray_icon};
use tiny_skia::{Pixmap, PixmapPaint, Transform};

const FONT: &str = "C:/Windows/Fonts/segoeui.ttf";
const SCALES: [f32; 3] = [1.0, 1.5, 2.0];
const NOTICES: [Notice; 13] = [
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

#[test]
fn tray_icons_differ_in_shape_at_16_px() {
    let shape = |p: Phase| -> Vec<u8> {
        let pm = tray_icon(p, 16).unwrap();
        pm.pixels().iter().map(|c| c.alpha()).collect()
    };
    let (idle, rec, busy) = (
        shape(Phase::Idle),
        shape(Phase::Recording),
        shape(Phase::Transcribing),
    );
    assert_ne!(idle, rec);
    assert_ne!(rec, busy);
    assert_ne!(idle, busy);
    for icon in [&idle, &rec, &busy] {
        assert!(icon.contains(&255), "a solid pixel at 16 px");
        assert_eq!(icon[0], 0, "corner is transparent");
    }
    assert!(tray_icon(Phase::Idle, 0).is_none());
}

#[test]
fn bgra_swaps_red_and_blue() {
    let pm = tray_icon(Phase::Recording, 16).unwrap();
    let out = bgra(&pm);
    let i = (8 * 16 + 8) * 4;
    let c = palette::RECORDING;
    assert_eq!(&out[i..i + 4], &[c.2, c.1, c.0, c.3]);
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

/// With `OXI_SHOTS_DIR` set: every state at every scale, tray icons, and one sheet per scale
/// on light and dark backgrounds for the eye.
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
    for (name, phase) in [
        ("idle", Phase::Idle),
        ("recording", Phase::Recording),
        ("transcribing", Phase::Transcribing),
    ] {
        for size in [16, 20, 24, 32] {
            tray_icon(phase, size)
                .unwrap()
                .save_png(dir.join(format!("tray-{name}-{size}.png")))
                .unwrap();
        }
    }
}

fn paint(grey: u8) -> tiny_skia::Paint<'static> {
    let mut p = tiny_skia::Paint::default();
    p.set_color_rgba8(grey, grey, grey, 255);
    p
}
