//! The settings `Row` (theme/components/layout.slint): its control column ends at the same x in
//! every row of a card at any page width, and its `info` text is an "i" beside the title.
//!
//! The probe is built by `slint!`, without the debug info `ElementHandle` needs, so it reports
//! its geometry through properties, and the "i" through the hover tooltip it opens.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Platform, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, Model, ModelRc, PhysicalSize, PlatformError};

slint::slint! {
    import { Theme, Shell, Icons, Card, Group, Row, SliderRow, ToggleRow, Divider, Combo, Switch, Button, StatusChip, IconLabel } from "../ui/theme/index.slint";
    import { HoverTip } from "../ui/theme/components/base.slint";

    export component RowProbe inherits Window {
        out property <length> combo-w: combo.width;
        out property <length> combo-right: combo.absolute-position.x + combo.width;
        out property <length> switch-right: switch.absolute-position.x + switch.width;
        out property <length> button-right: button.absolute-position.x + button.width;
        // The column ends the row: every control must end where the row does.
        out property <length> row-right: gain.absolute-position.x + gain.width;
        out property <length> gain-x: gain.absolute-position.x;
        out property <length> gain-mid: gain.absolute-position.y + gain.height / 2;
        out property <length> title-w: measure.preferred-width;
        out property <string> tip: HoverTip.text;
        out property <length> tip-x: HoverTip.x;
        out property <length> tip-y: HoverTip.y;
        out property <length> long-x: long.absolute-position.x;
        out property <length> long-y: long.absolute-position.y;
        out property <length> long-h: long.height;
        out property <length> long-switch-x: long-switch.absolute-position.x;
        out property <length> device-title-x: device.title-x;
        out property <length> toggle-title-x: toggle.title-x;
        out property <length> gain-title-x: gain.title-x;
        out property <length> button-title-x: recordings.title-x;
        out property <length> line-h: line.preferred-height;
        out property <length> described-mid: described.absolute-position.y + described.height / 2;
        out property <length> described-h: described.height;
        out property <length> described-switch-mid: described-switch.absolute-position.y + described-switch.height / 2;
        out property <length> group-x: group.absolute-position.x;
        out property <length> group-mid: group.absolute-position.y + 10px;
        out property <length> group-title-w: group-measure.preferred-width;
        public function set-page(w: length) {
            Shell.page-width = w;
        }
        measure := Text {
            visible: false;
            text: "Gain";
            font-size: Theme.body-size;
        }
        line := Text {
            visible: false;
            text: "X";
            font-size: Theme.body-size;
        }
        group-measure := Text {
            visible: false;
            text: "Microphone";
            font-size: Theme.body-size;
            font-weight: Theme.weight-strong;
        }
        VerticalLayout {
            alignment: start;
            Card {
                device := Row {
                    title: "Device";
                    fill-controls: true;
                    combo := Combo {
                        model: ["Microphone"];
                    }
                }
                Divider { }
                Row {
                    title: "Noise suppression";
                    switch := Switch { }
                }
                Divider { }
                recordings := Row {
                    title: "Recordings";
                    button := Button {
                        text: "Open folder";
                    }
                }
                Divider { }
                gain := SliderRow {
                    title: "Gain";
                    info: "What Whisper hears.";
                    maximum: 10;
                }
                Divider { }
                toggle := ToggleRow {
                    flat: true;
                    title: "Start with Windows";
                }
                Divider { }
                long := Row {
                    title: "Keep the microphone open between dictations so the first word is never cut";
                    info: "Uses a little more power.";
                    long-switch := Switch { }
                }
                Divider { }
                described := Row {
                    title: "Noise suppression";
                    description: "Cuts steady hiss before Whisper hears it.";
                    described-switch := Switch { }
                }
            }
            group := Group {
                title: "Microphone";
                info: "Group hint.";
                Card {
                    Row {
                        title: "Inside";
                    }
                }
            }
        }
    }
    // The pairs of an icon and its text, drawn: each one's icon region and text region, as window
    // pixels (`x0 x1 y0 y1`), so the test measures the ink it finds there.
    export component PairProbe inherits Window {
        default-font-family: Theme.font-family;
        default-font-size: Theme.body-size;
        out property <[length]> row-icon: [keys.absolute-position.x, keys.absolute-position.x + Theme.icon-size, keys.absolute-position.y, keys.absolute-position.y + keys.height];
        out property <[length]> row-text: [keys.title-x, keys.title-x + 120px, keys.absolute-position.y, keys.absolute-position.y + keys.height];
        out property <[length]> head-icon: [head.absolute-position.x, head.absolute-position.x + Theme.icon-size, head.absolute-position.y, head.absolute-position.y + head.height];
        out property <[length]> head-text: [head.absolute-position.x + Theme.icon-size + Theme.pair-gap, head.absolute-position.x + head.width, head.absolute-position.y, head.absolute-position.y + head.height];
        out property <[length]> chip-dot: [chip.absolute-position.x + chip.width - Theme.space-2 - Theme.status-dot-size, chip.absolute-position.x + chip.width - Theme.space-2, chip.absolute-position.y, chip.absolute-position.y + chip.height];
        out property <[length]> chip-text: [chip.absolute-position.x + Theme.space-2, chip.absolute-position.x + chip.width - Theme.space-2 - Theme.status-dot-size - Theme.pair-gap, chip.absolute-position.y, chip.absolute-position.y + chip.height];
        out property <[length]> button-icon: [pair-button.absolute-position.x + pair-button.icon-x, pair-button.absolute-position.x + pair-button.icon-x + Theme.icon-size-small, pair-button.absolute-position.y, pair-button.absolute-position.y + pair-button.height];
        out property <[length]> button-text: [pair-button.absolute-position.x + pair-button.label-x, pair-button.absolute-position.x + pair-button.width - Theme.space-3, pair-button.absolute-position.y, pair-button.absolute-position.y + pair-button.height];
        VerticalLayout {
            alignment: start;
            // Capital H and E only: flat tops and bottoms, so the ink's middle is the capitals' middle.
            // A circle icon: the same ink above and below its middle.
            Card {
                keys := Row {
                    icon: Icons.error;
                    title: "HEH";
                }
            }
            head := IconLabel {
                height: Theme.control-height;
                icon: Icons.error;
                text: "HEH";
                font-weight: Theme.weight-strong;
            }
            HorizontalLayout {
                alignment: start;
                chip := StatusChip {
                    text: "HEH";
                }
            }
            HorizontalLayout {
                alignment: start;
                pair-button := Button {
                    text: "HEH";
                    icon: Icons.error;
                }
            }
        }
    }
}

/// The probe laid out `width` px wide, its card as wide as the page.
fn probe(width: f32, height: f32) -> RowProbe {
    let probe = RowProbe::new().unwrap();
    probe.invoke_set_page(width);
    probe
        .window()
        .set_size(slint::LogicalSize::new(width, height));
    probe.show().unwrap();
    probe
}

#[test]
fn control_column_ends_at_one_x_at_every_width() {
    i_slint_backend_testing::init_no_event_loop();
    // 560: a narrow page keeps the 240 px column; above ~1030 the column stops at 360.
    for (width, column) in [(560.0, 240.0), (1280.0, 360.0), (1920.0, 360.0)] {
        let p = probe(width, 1080.0);
        let right = p.get_row_right();
        assert!(
            (p.get_combo_w() - column).abs() < 0.5,
            "at {width} px the Combo is {} wide, not the {column} px column",
            p.get_combo_w()
        );
        for (name, end) in [
            ("Combo", p.get_combo_right()),
            ("Switch", p.get_switch_right()),
            ("Button", p.get_button_right()),
        ] {
            assert!(
                (end - right).abs() < 0.5,
                "at {width} px the {name} ends at {end}, the row at {right}"
            );
        }
    }
}

#[test]
fn info_text_is_an_i_beside_the_title() {
    i_slint_backend_testing::init_no_event_loop();
    let p = probe(1920.0, 1080.0);
    // Where the "i" belongs: `pair-gap` (8) after the title's text, centred on the row.
    let at = p.get_gain_x() + p.get_title_w() + 8.0 + 10.0;
    p.window().dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(at, p.get_gain_mid()),
    });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(700));
    assert_eq!(p.get_tip(), "What Whisper hears.", "no i beside the title");
    // The tooltip anchors inside the 20 px "i" it belongs to.
    let x = p.get_tip_x();
    assert!(
        (x - at).abs() <= 10.0,
        "the i is at x {x}, not right after the title (row x {}, title {} wide)",
        p.get_gain_x(),
        p.get_title_w()
    );
}

#[test]
fn info_sits_on_the_first_line_of_a_wrapped_title() {
    i_slint_backend_testing::init_no_event_loop();
    let p = probe(720.0, 520.0);
    let line = p.get_line_h();
    let (top, h) = (p.get_long_y(), p.get_long_h());
    // Two lines in Segoe UI Variable Text; another font (Linux) is wider and may need a third.
    let lines = if cfg!(windows) {
        2.0
    } else {
        (h / line).round()
    };
    assert!(
        h > 1.5 * line && h < (lines + 0.5) * line,
        "the long title is {h} px high, not two {line} px lines"
    );
    // The middle of the first line of the block, which the row centres.
    let first = top + h / 2.0 - lines * line / 2.0 + line / 2.0;
    // Walk the first line rightwards until the "i" opens its tooltip.
    let end = p.get_long_switch_x();
    let mut x = p.get_long_x();
    while x < end && p.get_tip() != "Uses a little more power." {
        p.window().dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(x, first),
        });
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(700));
        x += 4.0;
    }
    assert_eq!(
        p.get_tip(),
        "Uses a little more power.",
        "no i on the title's first line"
    );
    let (long_x, long_tip_y) = (p.get_tip_x(), p.get_tip_y());
    // Calibrate on the one-line Gain row, whose "i" is centred on the row: the tooltip's offset
    // from the middle of the "i" it opens under.
    p.window().dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(0.0, 0.0),
    });
    p.window().dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(p.get_gain_x() + p.get_title_w() + 18.0, p.get_gain_mid()),
    });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(700));
    assert_eq!(p.get_tip(), "What Whisper hears.", "no i beside Gain");
    let below = p.get_tip_y() - p.get_gain_mid();
    let mid = long_tip_y - below;
    assert!(
        (mid - first).abs() <= 2.0,
        "the i is centred at y {mid}, the first line at {first} (row {top}, {h} high)"
    );
    // Left of the switch, with at least the "!" room (icon-gap 16 + icon 20) between.
    assert!(
        long_x + 20.0 <= end - 36.0 + 0.5,
        "the i ({long_x}) runs into the control column ({end})"
    );
}

#[test]
fn titles_in_one_card_share_a_left_edge() {
    i_slint_backend_testing::init_no_event_loop();
    for (width, height) in [(720.0, 520.0), (1920.0, 1080.0)] {
        let p = probe(width, height);
        let edge = p.get_device_title_x();
        for (name, x) in [
            ("Recordings", p.get_button_title_x()),
            ("flat ToggleRow", p.get_toggle_title_x()),
            ("SliderRow", p.get_gain_title_x()),
        ] {
            assert!(
                (x - edge).abs() < 0.5,
                "at {width} px the {name} title starts at {x}, the Row title at {edge}"
            );
        }
    }
}

#[test]
fn i_is_a_pair_gap_after_the_title_text() {
    i_slint_backend_testing::init_no_event_loop();
    for (width, height) in [(720.0, 520.0), (1920.0, 1080.0)] {
        let p = probe(width, height);
        // Walk the Gain row's middle line until the "i" opens its tooltip.
        let end = p.get_gain_x() + width;
        let mut x = p.get_gain_title_x();
        while x < end && p.get_tip() != "What Whisper hears." {
            p.window().dispatch_event(WindowEvent::PointerMoved {
                position: LogicalPosition::new(x, p.get_gain_mid()),
            });
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(700));
            x += 1.0;
        }
        assert_eq!(
            p.get_tip(),
            "What Whisper hears.",
            "no i on the Gain row at {width} px"
        );
        // The tooltip anchors at the middle of the 20 px "i" box: pair-gap 8 + 10 after the title.
        let gap = p.get_tip_x() - (p.get_gain_title_x() + p.get_title_w());
        assert!(
            (gap - 18.0).abs() < 0.5,
            "at {width} px the i is {gap} px after the title, not the 8 px pair gap (18 to its middle)"
        );
    }
}

#[test]
fn a_described_row_centres_its_controls() {
    i_slint_backend_testing::init_no_event_loop();
    for (width, height) in [(720.0, 520.0), (1920.0, 1080.0)] {
        let p = probe(width, height);
        assert!(
            p.get_described_h() > 32.0 + 0.5,
            "the row is {} px high, not taller than a control",
            p.get_described_h()
        );
        let off = p.get_described_switch_mid() - p.get_described_mid();
        assert!(
            off.abs() <= 0.5,
            "at {width} px the Switch is {off} px off the described row's centre"
        );
    }
}

#[test]
fn a_group_info_is_an_i_a_pair_gap_after_the_title() {
    i_slint_backend_testing::init_no_event_loop();
    for (width, height) in [(720.0, 520.0), (1920.0, 1080.0)] {
        let p = probe(width, height);
        let mut x = p.get_group_x();
        while x < p.get_group_x() + width && p.get_tip() != "Group hint." {
            p.window().dispatch_event(WindowEvent::PointerMoved {
                position: LogicalPosition::new(x, p.get_group_mid()),
            });
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(700));
            x += 1.0;
        }
        assert_eq!(
            p.get_tip(),
            "Group hint.",
            "no i on the Group title at {width} px"
        );
        // The tooltip anchors at the middle of the 20 px "i" box: pair-gap 8 + 10 after the title.
        let gap = p.get_tip_x() - (p.get_group_x() + p.get_group_title_w());
        assert!(
            (gap - 18.0).abs() < 0.5,
            "at {width} px the i is {gap} px after the Group title, not the 8 px pair gap (18 to its middle)"
        );
    }
}

thread_local! {
    static SURFACE: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
}

struct Drawn;

impl Platform for Drawn {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        SURFACE.with_borrow_mut(|surface| *surface = Some(window.clone()));
        Ok(window)
    }
}

/// The probe drawn at 1:1 into an RGB buffer.
fn drawn(width: u32, height: u32) -> (PairProbe, Vec<[u8; 3]>) {
    let _ = slint::platform::set_platform(Box::new(Drawn));
    let probe = PairProbe::new().unwrap();
    let surface = SURFACE.with_borrow(Clone::clone).unwrap();
    surface.set_size(PhysicalSize::new(width, height));
    probe.show().unwrap();
    slint::platform::update_timers_and_animations();
    surface.request_redraw();
    let mut buffer = vec![PremultipliedRgbaColor::default(); (width * height) as usize];
    assert!(surface.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, width as usize);
    }));
    (
        probe,
        buffer.iter().map(|p| [p.red, p.green, p.blue]).collect(),
    )
}

/// The vertical centre of the ink in `area` (`x0 x1 y0 y1`, window px), weighted by how much each
/// pixel differs from the area's top-left one, which is background; a half-covered edge row weighs
/// half, so a 1 px shift of the text is a 1 px shift here. The area's edge rows are not read: a
/// control's border is not ink.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)] // whole positive pixels
fn ink_mid(pixels: &[[u8; 3]], width: u32, area: &ModelRc<f32>) -> f32 {
    let at = |i| area.row_data(i).unwrap().round() as usize;
    let (x0, x1, y0, y1) = (at(0), at(1), at(2) + 3, at(3) - 3);
    let px = |x: usize, y: usize| pixels[y * width as usize + x];
    let bg = px(x0, y0);
    let (mut weight, mut moment) = (0.0, 0.0);
    for y in y0..y1 {
        for x in x0..x1 {
            let ink: u32 = px(x, y)
                .iter()
                .zip(bg)
                .map(|(a, b)| u32::from(a.abs_diff(b)))
                .sum();
            weight += ink as f32;
            moment += ink as f32 * (y as f32 + 0.5);
        }
    }
    assert!(weight > 0.0, "no ink in {x0}..{x1} x {y0}..{y1}");
    moment / weight
}

// The 0.4 px bound is tuned for Segoe UI Variable Text, which only Windows has: elsewhere the font
// falls back to another face with other capital proportions, so only a loose bound (still above
// one icon height off) holds and the exact one is Windows-only.
#[cfg(windows)]
const CENTRE_TOLERANCE: f32 = 0.4;
#[cfg(not(windows))]
const CENTRE_TOLERANCE: f32 = 2.0;

#[test]
fn an_icon_and_its_text_share_one_centre_line() {
    let (width, height) = (400, 300);
    let (p, pixels) = drawn(width, height);
    let pairs = [
        ("Row title", p.get_row_text(), p.get_row_icon()),
        ("IconLabel", p.get_head_text(), p.get_head_icon()),
        ("StatusChip", p.get_chip_text(), p.get_chip_dot()),
        ("Button", p.get_button_text(), p.get_button_icon()),
    ];
    for (name, text, icon) in pairs {
        let (text, icon) = (
            ink_mid(&pixels, width, &text),
            ink_mid(&pixels, width, &icon),
        );
        assert!(
            (text - icon).abs() <= CENTRE_TOLERANCE,
            "the {name} capitals are centred at row {text}, its icon at row {icon} (on Windows cap-lift 1 px measures 0.1-0.3 apart, 0 measures 0.7)"
        );
    }
}
