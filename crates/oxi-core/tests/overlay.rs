use oxi_core::audio::Level;
use oxi_core::config::{Overlay, OverlayPosition as P};
use oxi_core::machine::{Notice, OverlayState, Phase};
use oxi_core::overlay::{
    CLIP_HOLD, Icon, MARGIN_DIP, METER_HZ, Meter, PILL_H_DIP, PILL_W_DIP, Quns, Rect, look,
    palette, place, visible,
};
use oxi_core::strings;
use oxi_core::types::Millis;

const ALL_POSITIONS: [P; 6] = [
    P::BottomRight,
    P::BottomCentre,
    P::BottomLeft,
    P::TopRight,
    P::TopCentre,
    P::TopLeft,
];

const ALL_NOTICES: [Notice; 15] = [
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
fn meter_clipping_on_then_off_after_500_ms() {
    let mut m = Meter::new(4, 0.0);
    assert!(!m.clipping(Millis(0)));
    m.push(level(0.98, 0.5), Millis(100));
    assert!(!m.clipping(Millis(100)));
    m.push(level(1.0, 0.5), Millis(200));
    assert!(m.clipping(Millis(200)));
    assert!(m.clipping(Millis(200) + CLIP_HOLD - Millis(1)));
    assert!(!m.clipping(Millis(200) + CLIP_HOLD));
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
    // Clipping must be visible against the normal meter colour.
    let (a, b) = (palette::RECORDING, palette::CLIP);
    let dist = [(a.0, b.0), (a.1, b.1), (a.2, b.2)]
        .map(|(x, y)| u32::from(x.abs_diff(y)))
        .iter()
        .sum::<u32>();
    assert!(dist >= 300, "meter and clip colours too close: {dist}");
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
