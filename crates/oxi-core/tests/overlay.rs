use oxi_core::audio::Level;
use oxi_core::config::{Overlay, OverlayPosition as P};
use oxi_core::machine::{Notice, OverlayState, Phase};
use oxi_core::overlay::{
    CARD_RADIUS, Icon, LOCK_HINT, MARGIN_DIP, METER_HZ, Meter, OVERLOAD_HOLD, PILL_EM, PILL_GAP,
    PILL_H_DIP, PILL_ICON, PILL_PAD, PILL_W_DIP, Quns, RecClock, Rect, SWEEP, TOO_LOUD,
    label_variants, label_x, look, palette, pill_label, place, progress_span, visible,
};
use oxi_core::strings;
use oxi_core::types::Millis;

#[path = "common/notices.rs"]
mod notices;
use notices::ALL_NOTICES;

const ALL_POSITIONS: [P; 6] = [
    P::BottomRight,
    P::BottomCentre,
    P::BottomLeft,
    P::TopRight,
    P::TopCentre,
    P::TopLeft,
];

#[test]
fn place_all_positions_all_scales_on_second_monitor() {
    // A monitor right of and above the primary one: x is 1920, y is negative.
    let work = Rect {
        x: 1920,
        y: -200,
        w: 2560,
        h: 1360,
    };
    for scale in [1.0_f32, 1.5, 2.0] {
        #[allow(clippy::cast_possible_truncation)] // small test sizes
        let dip = |v: i32| (v as f32 * scale).round() as i32;
        let (w, h, m) = (dip(PILL_W_DIP), dip(PILL_H_DIP), dip(MARGIN_DIP));
        let (left, right) = (work.x + m, work.x + work.w - w - m);
        let centre = work.x + (work.w - w) / 2;
        let (top, bottom) = (work.y + m, work.y + work.h - h - m);
        let expect = [
            (P::BottomRight, right, bottom),
            (P::BottomCentre, centre, bottom),
            (P::BottomLeft, left, bottom),
            (P::TopRight, right, top),
            (P::TopCentre, centre, top),
            (P::TopLeft, left, top),
        ];
        for (pos, x, y) in expect {
            assert_eq!(
                place(pos, work, scale),
                Rect { x, y, w, h },
                "{pos:?} at {scale}"
            );
        }
    }
}

#[test]
fn place_stays_inside_even_a_tiny_work_area() {
    for work in [
        Rect {
            x: -100,
            y: 50,
            w: 100,
            h: 30,
        },
        Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        },
    ] {
        for pos in ALL_POSITIONS {
            let r = place(pos, work, 2.0);
            assert!(r.x >= work.x && r.y >= work.y, "{pos:?} {work:?} {r:?}");
            assert!(r.x + r.w <= work.x + work.w && r.y + r.h <= work.y + work.h);
        }
    }
}

#[test]
fn place_treats_a_bad_scale_as_one() {
    let work = Rect {
        x: 0,
        y: 0,
        w: 1000,
        h: 800,
    };
    for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
        assert_eq!(place(P::TopLeft, work, bad), place(P::TopLeft, work, 1.0));
    }
}

#[test]
fn fullscreen_table() {
    use Quns::*;
    let overlay = |enabled, hide_over_fullscreen| Overlay {
        enabled,
        hide_over_fullscreen,
        ..Overlay::default()
    };
    let states = [
        (NotPresent, false),
        (Busy, true),
        (D3dFullScreen, true),
        (PresentationMode, true),
        (AcceptsNotifications, false),
        (QuietTime, false),
        (App, false),
    ];
    for (state, fullscreen) in states {
        assert_eq!(
            visible(&overlay(true, true), state),
            !fullscreen,
            "{state:?} hiding"
        );
        assert!(
            visible(&overlay(true, false), state),
            "{state:?} not hiding"
        );
        assert!(
            !visible(&overlay(false, false), state),
            "{state:?} disabled"
        );
    }
}

fn level(peak: f32, rms: f32) -> Level {
    Level { peak, rms }
}

#[test]
fn meter_silence_is_empty_and_full_scale_is_full() {
    let mut m = Meter::new(4, 0.0);
    let step = 1000 / METER_HZ;
    for i in 0..4 {
        m.push(level(0.0, 0.0), Millis(i * step));
    }
    assert_eq!(m.bars().collect::<Vec<_>>(), [0.0; 4]);
    for i in 4..8 {
        m.push(level(1.0, 1.0), Millis(i * step));
    }
    assert_eq!(m.bars().collect::<Vec<_>>(), [1.0; 4]);
}

#[test]
fn meter_maps_dbfs_and_applies_sensitivity() {
    let mut m = Meter::new(1, 0.0);
    m.push(level(0.5, 0.0316), Millis(0)); // -30 dBFS
    let v = m.bars().next().unwrap();
    assert!((v - 0.5).abs() < 0.01, "{v}");
    let mut loud = Meter::new(1, 15.0);
    loud.push(level(0.5, 0.0316), Millis(0));
    let v = loud.bars().next().unwrap();
    assert!((v - 0.75).abs() < 0.01, "{v}");
}

#[test]
fn meter_adds_a_bar_per_period_from_the_loudest_block() {
    let mut m = Meter::new(2, 0.0);
    m.push(level(0.0, 0.0), Millis(0)); // first block: a bar at once
    m.push(level(1.0, 1.0), Millis(10)); // too early for a bar, kept as pending
    m.push(level(0.0, 0.0), Millis(20));
    assert_eq!(m.bars().collect::<Vec<_>>(), [0.0, 0.0]);
    m.push(level(0.0, 0.0), Millis(40));
    assert_eq!(m.bars().collect::<Vec<_>>(), [0.0, 1.0]);
}

#[test]
fn meter_keeps_30_hz_when_polled_every_32_ms() {
    let mut m = Meter::new(64, 0.0);
    for i in 0..32 {
        m.push(level(0.0, 1.0), Millis(i * 32));
    }
    let bars = m.bars().filter(|b| *b > 0.5).count();
    assert!((29..=31).contains(&bars), "{bars} bars in 1 s");
}

#[test]
fn meter_clipping_is_overload_not_one_hot_sample() {
    let mut m = Meter::new(4, 0.0);
    assert!(!m.clipping(Millis(0)));
    m.push(level(0.88, 0.5), Millis(100));
    assert!(!m.clipping(Millis(100)));
    m.push(level(1.0, 0.5), Millis(200));
    assert!(!m.clipping(Millis(200)), "one hot frame is not overload");
    m.push(level(1.0, 0.5), Millis(233));
    m.push(level(1.0, 0.5), Millis(266));
    assert!(m.clipping(Millis(266)));
    assert!(m.clipping(Millis(266) + OVERLOAD_HOLD - Millis(1)));
    assert!(!m.clipping(Millis(266) + OVERLOAD_HOLD));
}

#[test]
fn every_notice_has_text() {
    for n in ALL_NOTICES {
        assert!(!strings::notice(n).is_empty(), "{n:?}");
    }
    assert_eq!(
        strings::notice(Notice::PressPaste),
        "Text is on the clipboard, press Ctrl+V"
    );
    assert_eq!(
        strings::notice(Notice::AdminWindow),
        "Admin window: press Ctrl+V"
    );
    for n in [Notice::ConfigInvalid, Notice::ConfigAdjusted] {
        assert!(
            strings::notice(n).contains(oxi_core::paths::CONFIG_FILE),
            "{n:?}"
        );
    }
}

#[test]
fn every_state_has_a_look_and_every_phase_a_tooltip() {
    let mut states = vec![
        OverlayState::Recording {
            locked: false,
            ending_soon: false,
        },
        OverlayState::Recording {
            locked: true,
            ending_soon: false,
        },
        OverlayState::Recording {
            locked: false,
            ending_soon: true,
        },
        OverlayState::LoadingModel,
        OverlayState::Transcribing,
        OverlayState::Pasting,
    ];
    states.extend(ALL_NOTICES.map(OverlayState::Text));
    for s in states {
        let l = look(s);
        assert!(!l.label.is_empty(), "{s:?}");
        assert_ne!(l.accent, palette::NONE, "{s:?}");
        assert_ne!(l.icon, Icon::None, "{s:?}");
        assert!(
            l.label.chars().count() <= strings::OVERLAY_LABEL_MAX,
            "{s:?}: {:?}",
            l.label
        );
        assert_eq!(
            l.meter,
            matches!(s, OverlayState::Recording { .. }),
            "{s:?}"
        );
    }
    let locked_ending = look(OverlayState::Recording {
        locked: true,
        ending_soon: true,
    });
    assert_eq!(locked_ending.icon, Icon::Lock);
    assert_eq!(locked_ending.label, "Ending soon");
    let hidden = look(OverlayState::Hidden);
    assert_eq!(
        (hidden.label, hidden.icon, hidden.meter),
        ("", Icon::None, false)
    );
    for p in [
        Phase::Idle,
        Phase::Recording,
        Phase::Transcribing,
        Phase::Pasting,
    ] {
        assert!(strings::tray_tooltip(p).starts_with(oxi_core::APP_NAME));
    }
    assert_eq!(
        [
            strings::MENU_SETTINGS,
            strings::MENU_COPY_LAST,
            strings::MENU_QUIT
        ],
        ["Settings", "Copy last text", "Quit"]
    );
}

/// Every status label of the pill that draw.rs centres (the states without a meter), plus
/// Cyrillic ones of the same kind (the overlay is English today; the check is the layout's, which
/// sees only a width). Widths are in em, narrow enough that centring applies.
fn labels() -> Vec<(String, f32)> {
    let em_share = |ch: char| if ch.is_ascii() { 0.4 } else { 0.45 };
    let mut texts: Vec<String> = ALL_NOTICES
        .iter()
        .map(|n| strings::notice_short(*n).to_string())
        .collect();
    texts.extend(["Loading model", "Transcribing", "Pasting"].map(String::from));
    texts.extend(["Транскрипция", "Вставка", "Нет модели"].map(String::from));
    texts
        .into_iter()
        .map(|t| {
            let w = t.chars().map(em_share).sum::<f32>();
            (t, w)
        })
        .collect()
}

#[test]
fn status_label_is_centred_in_the_pill_within_one_pixel() {
    for scale in [1.0_f32, 1.5, 2.0] {
        let r = place(
            P::TopLeft,
            Rect {
                x: 0,
                y: 0,
                w: 10_000,
                h: 10_000,
            },
            scale,
        );
        let (w, h) = (r.w as f32, r.h as f32);
        let (pad, icon, gap) = (
            (PILL_PAD * h).round(),
            (PILL_ICON * h).round(),
            (PILL_GAP * h).round(),
        );
        let (min_x, right) = (pad + icon + gap, w - pad);
        let mut centred = 0;
        for (text, em_widths) in labels() {
            let text_w = em_widths * PILL_EM * h;
            let x = label_x(w, min_x, right, text_w);
            if text_w <= w - 2.0 * min_x {
                centred += 1;
                let centre = x + text_w / 2.0;
                assert!(
                    (centre - w / 2.0).abs() <= 1.0,
                    "{text} at {scale}: centre {centre}"
                );
            } else {
                // Too wide to centre without touching the icon: starts right after it.
                assert!((x - min_x).abs() <= 1.0, "{text} at {scale}: x {x}");
            }
        }
        // The six status labels (three of them Cyrillic) all fit, so they are all asserted.
        assert!(centred >= 6, "only {centred} labels centred at {scale}");
    }
}

/// DS-2.2: the pill's corner is the settings window's `radius-card`, one value for both.
#[test]
fn pill_radius_is_the_card_radius_token() {
    let tokens = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../oxi-settings/ui/theme/tokens.slint"),
    )
    .unwrap();
    let line = tokens
        .lines()
        .find(|l| l.contains("property <length> radius-card:"))
        .expect("radius-card token");
    let px: i32 = line
        .split(':')
        .nth(1)
        .and_then(|v| v.trim().trim_end_matches(';').strip_suffix("px"))
        .and_then(|v| v.parse().ok())
        .expect("radius-card in px");
    assert_eq!(px, CARD_RADIUS);
}

#[test]
fn a_known_percent_fills_the_track_up_to_it() {
    for now in [Millis(0), Millis(777)] {
        assert_eq!(progress_span(Some(0), now), (0.0, 0.0));
        assert_eq!(progress_span(Some(42), now), (0.0, 0.42));
        assert_eq!(progress_span(Some(100), now), (0.0, 1.0));
        assert_eq!(progress_span(Some(250), now), (0.0, 1.0), "clamped");
    }
}

#[test]
fn an_unknown_percent_sweeps_a_segment_left_to_right() {
    let mut last_head = -1.0;
    for ms in (0..SWEEP.0).step_by(50) {
        let (a, b) = progress_span(None, Millis(ms));
        assert!(
            (0.0..=1.0).contains(&a) && (0.0..=1.0).contains(&b) && a <= b,
            "{ms}: {a} {b}"
        );
        assert!(b - a <= 0.3 + 1e-6, "{ms}: segment {a}..{b}");
        assert!(b >= last_head, "{ms}: moves right only");
        last_head = b;
    }
    // Enters from the left, leaves at the right, then starts again.
    assert_eq!(progress_span(None, Millis(0)), (0.0, 0.0));
    let (a, b) = progress_span(None, Millis(SWEEP.0 - 1));
    assert!(a > 0.99 && (b - 1.0).abs() < f32::EPSILON, "{a} {b}");
    assert_eq!(progress_span(None, SWEEP), progress_span(None, Millis(0)));
    let mid = progress_span(None, Millis(SWEEP.0 / 2));
    assert!(mid.1 - mid.0 > 0.29, "a full segment mid-sweep: {mid:?}");
}

fn label(state: OverlayState, percent: Option<u8>, clock: RecClock, now: u64) -> String {
    let mut out = String::from("stale");
    pill_label(&mut out, state, percent, clock, Millis(now));
    out
}

#[test]
fn transcribing_shows_the_percent_when_known() {
    let c = RecClock::default();
    assert_eq!(
        label(OverlayState::Transcribing, None, c, 0),
        "Transcribing"
    );
    assert_eq!(
        label(OverlayState::Transcribing, Some(42), c, 0),
        "Transcribing 42%"
    );
    assert_eq!(
        label(OverlayState::Transcribing, Some(100), c, 0),
        "Transcribing 100%"
    );
    // Only transcription has a percent.
    assert_eq!(
        label(OverlayState::LoadingModel, Some(42), c, 0),
        strings::overlay_label(OverlayState::LoadingModel)
    );
}

#[test]
fn a_locked_recording_says_how_to_stop_then_shows_its_length() {
    let rec = |locked, ending_soon| OverlayState::Recording {
        locked,
        ending_soon,
    };
    let mut c = RecClock::default();
    c.see(rec(false, false), Millis(1_000));
    assert_eq!(label(rec(false, false), None, c, 9_000), "Recording");
    // Locked at 5 s, 4 s into the recording.
    c.see(rec(true, false), Millis(5_000));
    c.see(rec(true, false), Millis(5_033));
    let hint = strings::overlay_label(rec(true, false));
    assert_eq!(label(rec(true, false), None, c, 5_000), hint);
    assert_eq!(
        label(rec(true, false), None, c, 5_000 + LOCK_HINT.0 - 1),
        hint
    );
    assert_eq!(
        label(rec(true, false), None, c, 5_000 + LOCK_HINT.0),
        "0:07"
    );
    assert_eq!(label(rec(true, false), None, c, 66_000), "1:05");
    assert_eq!(label(rec(true, false), None, c, 1_000 + 600_000), "10:00");
    // The warning before the cap keeps its own words.
    assert_eq!(
        label(rec(true, true), None, c, 66_000),
        strings::overlay_label(rec(true, true))
    );
    // A new recording starts its own clock.
    c.see(OverlayState::Transcribing, Millis(70_000));
    c.see(rec(false, false), Millis(80_000));
    c.see(rec(true, false), Millis(81_000));
    assert_eq!(
        label(rec(true, false), None, c, 81_000 + LOCK_HINT.0),
        "0:04"
    );
    // A hidden pill ends it too: a minute later the next one starts at zero.
    c.see(OverlayState::Hidden, Millis(90_000));
    c.see(rec(true, false), Millis(150_000));
    assert_eq!(
        label(rec(true, false), None, c, 150_000 + LOCK_HINT.0),
        "0:03"
    );
}

/// The wave box is placed after the widest text the label can become, so it never moves.
#[test]
fn the_label_slot_covers_the_warning_and_the_clock() {
    let rec = OverlayState::Recording {
        locked: false,
        ending_soon: false,
    };
    let v = label_variants(rec);
    assert!(v.contains(&TOO_LOUD) && v.contains(&strings::overlay_label(rec)));
    let mut clock = RecClock::default();
    let locked = OverlayState::Recording {
        locked: true,
        ending_soon: false,
    };
    clock.see(locked, Millis(0));
    let mut out = String::new();
    pill_label(
        &mut out,
        locked,
        None,
        clock,
        Millis(LOCK_HINT.0 + 59 * 60_000),
    );
    assert!(out.len() <= v[5].len(), "{out} fits the clock slot");
}

/// Every recording state gets the same set, so the wave box is the same in all of them.
#[test]
fn the_label_slot_is_the_same_in_every_recording_state() {
    let set = |locked, ending_soon| {
        let mut v = label_variants(OverlayState::Recording {
            locked,
            ending_soon,
        });
        v.sort_unstable();
        v
    };
    let first = set(false, false);
    for (l, e) in [(true, false), (true, true), (false, true)] {
        assert_eq!(set(l, e), first);
    }
    for s in [
        "Recording",
        "Press to stop",
        "Ending soon",
        TOO_LOUD,
        "00:00",
    ] {
        assert!(first.contains(&s), "{s}");
    }
}
