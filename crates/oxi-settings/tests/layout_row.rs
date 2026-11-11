//! The settings `Row` (theme/components/layout.slint): its control column ends at the same x in
//! every row of a card at any page width, and its `info` text is an "i" beside the title.
//!
//! The probe is built by `slint!`, without the debug info `ElementHandle` needs, so it reports
//! its geometry through properties, and the "i" through the hover tooltip it opens.

use std::time::Duration;

use slint::platform::WindowEvent;
use slint::{ComponentHandle, LogicalPosition};

slint::slint! {
    import { Theme, Shell, Card, Group, Row, SliderRow, ToggleRow, Divider, Combo, Switch, Button } from "../ui/theme/index.slint";
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
    assert!(
        h > 1.5 * line && h < 2.5 * line,
        "the long title is {h} px high, not two {line} px lines"
    );
    // The middle of the first of the two lines.
    let first = top + h / 2.0 - line / 2.0;
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
