//! Hotkey page under the testing backend, on a temporary Layout. The capture tests against the
//! fake daemon link live in `src/pages/hotkey.rs`: `open` there makes its own offline link; here the link is a fake that stays online.

mod common;

use std::fs;

use common::{config, has, layout, one, open_linked, with_role};
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use oxi_core::config::OtherKey;
use oxi_core::machine::Mode;
use oxi_core::paths::Layout;
use oxi_settings::ipc::{Event, Link};
use oxi_settings::{AppWindow, Ipc, Page};
use slint::ComponentHandle;
use slint::platform::WindowEvent;

fn open(layout: &Layout) -> AppWindow {
    // Apply needs the daemon link; the receiver is dropped and sends are ignored.
    open_linked(layout, Page::Hotkey, Ipc::fake().0)
}

fn click(window: &AppWindow, label: &str) {
    one(window, label).invoke_accessible_default_action();
}

#[test]
fn clicked_keys_apply_as_the_new_chord() {
    let l = layout("apply", Some("version = 2\n[hotkey]\nchord = [\"F13\"]\n"));
    let window = open(&l);
    assert!(has(&window, "F13"));
    assert!(
        !has(&window, "Left Ctrl"),
        "the keyboard is open before Change"
    );

    click(&window, "Change hotkey");
    click(&window, "Left Ctrl");
    click(&window, "Left Alt");
    click(&window, "Left Win");
    // A second click takes a key out again.
    click(&window, "Left Alt");
    assert_eq!(one(&window, "Left Ctrl").accessible_checked(), Some(true));
    assert_eq!(one(&window, "Left Alt").accessible_checked(), Some(false));
    assert!(has(&window, "New hotkey: Ctrl + Win"));

    click(&window, "Apply hotkey");
    assert_eq!(config(&l).config.hotkey.chord, ["Ctrl", "Win"]);
    assert!(has(&window, "Ctrl + Win"));
    assert!(
        !has(&window, "Left Ctrl"),
        "the keyboard stays open after Apply"
    );
}

#[test]
fn both_sides_of_a_modifier_are_one_key() {
    let l = layout("sides", None);
    let window = open(&l);
    click(&window, "Change hotkey");
    click(&window, "Right Ctrl");
    click(&window, "Left Ctrl");
    click(&window, "F12");
    click(&window, "Apply hotkey");
    assert_eq!(config(&l).config.hotkey.chord, ["Ctrl", "F12"]);
}

#[test]
fn esc_is_refused_with_a_message() {
    let l = layout("esc", None);
    let window = open(&l);
    click(&window, "Change hotkey");
    click(&window, "Left Ctrl");
    click(&window, "Esc");
    let message = "Esc cancels a recording and cannot be part of the hotkey";
    assert!(has(&window, message));
    click(&window, "Apply hotkey");
    assert!(!l.config_file().exists(), "an Esc chord was saved");
    assert!(has(&window, message));

    // Taking Esc out clears the message.
    click(&window, "Esc");
    assert!(!has(&window, message));
    assert!(has(&window, "New hotkey: Ctrl"));
}

#[test]
fn empty_chord_is_refused() {
    let l = layout("empty", None);
    let window = open(&l);
    click(&window, "Change hotkey");
    assert!(has(&window, "No keys yet"));
    click(&window, "Apply hotkey");
    assert!(has(&window, "the hotkey has no keys"));
    assert!(!l.config_file().exists());
}

#[test]
fn cancel_keeps_the_saved_chord() {
    let l = layout("cancel", None);
    let window = open(&l);
    click(&window, "Change hotkey");
    click(&window, "F12");
    click(&window, "Cancel hotkey change");
    assert!(!has(&window, "Left Ctrl"));
    assert!(has(&window, "Ctrl + Win"));
    assert!(!l.config_file().exists());

    // Opened again, the board starts with nothing picked.
    click(&window, "Change hotkey");
    assert_eq!(one(&window, "F12").accessible_checked(), Some(false));
}

#[test]
fn mode_thumb_writes_toggle() {
    let l = layout("mode", Some("version = 2\n# mine\n"));
    let window = open(&l);
    let card = |label: &str| {
        ElementHandle::find_by_accessible_label(&window, label)
            .find(|e| e.accessible_role() == Some(AccessibleRole::RadioButton))
            .unwrap_or_else(|| panic!("no card {label}"))
    };
    assert_eq!(card("Push to talk").accessible_checked(), Some(true));
    card("Toggle").invoke_accessible_default_action();
    assert_eq!(card("Toggle").accessible_checked(), Some(true));
    assert_eq!(config(&l).config.hotkey.mode, Mode::Toggle);
    let text = fs::read_to_string(l.config_file()).unwrap();
    assert!(
        text.contains("mode = \"toggle\"") && text.contains("# mine"),
        "{text}"
    );
}

/// The Keys row has no caption: the mode tiles and the Esc row already say it.
#[test]
fn the_keys_row_has_no_caption() {
    let plain = layout(
        "keys-caption",
        Some(
            "version = 2
",
        ),
    );
    let esc = layout(
        "keys-caption-esc",
        Some(
            "version = 2
[hotkey]
esc_cancels = true
",
        ),
    );
    for window in [open(&plain), common::open_again(&esc, Page::Hotkey)] {
        assert!(has(&window, "Change hotkey"), "the page is shown");
        assert!(!has(&window, "Hold these keys to talk."));
        assert!(!has(
            &window,
            "Hold these keys to talk. Esc cancels a recording."
        ));
    }
}

/// R2.1: "Another key while holding" and "Esc cancels a recording" start from the file and
/// write it back.
#[test]
fn other_key_and_esc_round_trip() {
    let fresh = open(&layout("recording-default", None));
    let esc = |w: &AppWindow| with_role(w, "Esc cancels a recording", AccessibleRole::Switch);
    let other = |w: &AppWindow| with_role(w, "Another key while holding", AccessibleRole::Combobox);
    assert_eq!(esc(&fresh).accessible_checked(), Some(false));
    assert_eq!(other(&fresh).accessible_value().as_deref(), Some("Auto"));
    // The Auto hint is the "i" after the title (Row.info), not text on the page.
    assert!(has(
        &fresh,
        "The key always reaches the app. Auto cancels only if you have not spoken yet."
    ));

    let l = layout(
        "recording",
        Some("version = 2\n# mine\n[hotkey]\nesc_cancels = true\nother_key = \"always\"\n"),
    );
    let window = common::open_again(&l, Page::Hotkey);
    assert_eq!(esc(&window).accessible_checked(), Some(true));
    assert_eq!(
        other(&window).accessible_value().as_deref(),
        Some("Always cancel")
    );

    esc(&window).invoke_accessible_default_action();
    assert!(!config(&l).config.hotkey.esc_cancels);

    other(&window).invoke_accessible_expand_action();
    with_role(&window, "Never cancel", AccessibleRole::ListItem).invoke_accessible_default_action();
    assert_eq!(
        other(&window).accessible_value().as_deref(),
        Some("Never cancel")
    );
    // The pick saves once the Combo rests.
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(350));
    let file = config(&l);
    assert_eq!(file.config.hotkey.other_key, OtherKey::Never);
    assert!(!file.config.hotkey.esc_cancels);
    let text = fs::read_to_string(l.config_file()).unwrap();
    assert!(
        text.contains("other_key = \"never\"") && text.contains("# mine"),
        "{text}"
    );
}

#[test]
fn locked_config_keeps_change_off() {
    let original = "version = 99\n";
    let l = layout("locked", Some(original));
    let window = open(&l);
    let change = one(&window, "Change hotkey");
    assert_eq!(change.accessible_enabled(), Some(false));
    change.invoke_accessible_default_action();
    assert!(!has(&window, "Left Ctrl"));
    assert_eq!(fs::read_to_string(l.config_file()).unwrap(), original);
}

#[test]
fn offline_keeps_the_help_line_and_leaves_the_reason_to_the_banner() {
    let l = layout("offline-help", None);
    let (ipc, _rx) = Ipc::fake();
    let window = open_linked(&l, Page::Hotkey, ipc.clone());
    ipc.deliver(&Event::Link(Link::Offline));
    click(&window, "Change hotkey");
    assert!(has(
        &window,
        "Click keys or press them. Left and right modifiers both count."
    ));
    // The third mention of "not running" is gone: the banner and the rail chip say it.
    assert!(!has(
        &window,
        "Not connected to OxiYapper: pressed keys are not seen. Start OxiYapper to pick a hotkey."
    ));
    assert_eq!(
        one(&window, "Apply hotkey").accessible_enabled(),
        Some(false)
    );
}

/// Apply's tooltip names why it is off on each link; it must not claim the daemon is not
/// running when it is the pipe that is the problem.
#[test]
fn apply_tooltip_follows_the_link() {
    let l = layout("apply-tip", None);
    let (ipc, _rx) = Ipc::fake();
    let window = open_linked(&l, Page::Hotkey, ipc.clone());
    click(&window, "Change hotkey");
    let tip = || {
        one(&window, "Apply hotkey")
            .accessible_description()
            .map(|d| d.to_string())
    };
    // Online and nothing wrong with the chord: on, so nothing to explain.
    click(&window, "Left Ctrl");
    assert_eq!(tip().as_deref(), Some(""));
    for (link, text) in [
        (
            Link::Offline,
            "Pressed keys are seen only while OxiYapper runs",
        ),
        (
            Link::Mismatch { daemon_proto: 9 },
            "Restart OxiYapper first",
        ),
        (Link::Busy, "OxiYapper is running but not answering"),
        (
            Link::Untrusted,
            "The OxiYapper pipe is held by an untrusted program. Not connecting.",
        ),
        (
            Link::Foreign,
            "The OxiYapper pipe is held by another account. Not connecting.",
        ),
    ] {
        ipc.deliver(&Event::Link(link));
        assert_eq!(tip().as_deref(), Some(text), "{link:?}");
    }
    ipc.deliver(&Event::Link(Link::Online));
    assert_eq!(tip().as_deref(), Some(""));
}

/// The owner's chord plus one: 15 keys.
const LONG: &str = "version = 2\n[hotkey]\nchord = [\"Shift\", \"W\", \"E\", \"R\", \"I\", \"O\", \"BracketLeft\", \"A\", \"S\", \"D\", \"F\", \"J\", \"K\", \"Quote\", \"Z\"]\n";
const LONG_TEXT: &str =
    "Shift + W + E + R + I + O + Bracket Left + A + S + D + F + J + K + Quote + Z";
const SIZES: [(f32, f32); 3] = [(720.0, 520.0), (1000.0, 700.0), (1280.0, 860.0)];

/// `[left, top, right, bottom]` of an element.
fn rect(e: &ElementHandle) -> [f32; 4] {
    let (at, size) = (e.absolute_position(), e.size());
    [at.x, at.y, at.x + size.width, at.y + size.height]
}

fn resized(window: &AppWindow, (w, h): (f32, f32)) {
    window.window().set_size(slint::LogicalSize::new(w, h));
    // Runs the `changed` handlers that follow the window width.
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(1));
}

fn chips(window: &AppWindow, id: &str) -> Vec<ElementHandle> {
    ElementHandle::find_by_element_id(window, &format!("KeycapRow::{id}")).collect()
}

#[test]
fn a_long_chord_stays_inside_the_keys_row_at_every_size() {
    let l = layout("long", Some(LONG));
    let window = open(&l);
    let short = layout(
        "long-short",
        Some("version = 2\n[hotkey]\nchord = [\"F13\"]\n"),
    );
    let single = common::open_again(&short, Page::Hotkey);
    let tile = |win: &AppWindow| {
        ElementHandle::find_by_accessible_label(win, "Push to talk")
            .find(|e| e.accessible_role() == Some(AccessibleRole::RadioButton))
            .map(|e| rect(&e))
    };
    for size in SIZES {
        resized(&window, size);
        resized(&single, size);
        let row = rect(&one(&window, LONG_TEXT));
        let button = rect(&one(&window, "Change hotkey"));
        assert!(
            row[2] <= button[0],
            "{size:?}: row {row:?}, button {button:?}"
        );
        let (shown, more) = (chips(&window, "chip"), chips(&window, "more"));
        assert!(!shown.is_empty(), "{size:?}: no key shown");
        assert_eq!(more.len(), 1, "{size:?}: no +N cap");
        for chip in shown.iter().chain(&more) {
            let r = rect(chip);
            assert!(
                r[0] >= row[0] && r[2] <= row[2] && r[1] >= row[1] && r[3] <= row[3],
                "{size:?}: chip {r:?} leaves the row {row:?}"
            );
        }
        // The chord's length moves nothing.
        assert_eq!(
            button,
            rect(&one(&single, "Change hotkey")),
            "{size:?}: the row changed with the chord"
        );
        assert_eq!(tile(&window), tile(&single), "{size:?}: the next row moved");
    }
}

#[test]
fn a_short_chord_is_not_folded_and_reads_as_a_person_says_it() {
    let l = layout(
        "short",
        Some("version = 2\n[hotkey]\nchord = [\"Ctrl\", \"BracketLeft\", \"Home\"]\n"),
    );
    let window = open(&l);
    assert!(has(&window, "Ctrl + Bracket Left + Home"));
    assert_eq!(chips(&window, "chip").len(), 3);
    assert!(chips(&window, "more").is_empty());
}

#[test]
fn twin_keys_of_a_saved_chord_read_apart() {
    let saved = |keys: &str| {
        format!(
            "version = 2
[hotkey]
chord = [{keys}]
"
        )
    };
    let digit = layout("twin-digit", Some(&saved(r#""1", "Num1""#)));
    assert!(has(&open(&digit), "1 + Numpad 1"));
    let enter = layout("twin-enter", Some(&saved(r#""Enter", "NumEnter""#)));
    assert!(has(
        &common::open_again(&enter, Page::Hotkey),
        "Enter + Numpad Enter"
    ));
}

fn press(window: &AppWindow, text: slint::SharedString) {
    let w = window.window();
    w.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    w.dispatch_event(WindowEvent::KeyReleased { text });
    // The tip is set in a `changed` handler.
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(1));
}

fn tab(window: &AppWindow) {
    press(window, slint::platform::Key::Tab.into());
}

#[test]
fn the_folded_keys_are_told_on_keyboard_focus() {
    let window = open(&layout("fold-focus", Some(LONG)));
    resized(&window, SIZES[0]);
    let copies = || ElementHandle::find_by_accessible_label(&window, LONG_TEXT).count();
    assert_eq!(copies(), 1);
    // The rail, then the "+N" cap.
    tab(&window);
    tab(&window);
    assert_eq!(copies(), 2, "focus on the +N cap shows the whole chord");
    tab(&window);
    assert_eq!(copies(), 1, "leaving it closes the tip");
}

#[test]
fn leaving_the_page_closes_the_tip() {
    let window = open(&layout("fold-leave", Some(LONG)));
    resized(&window, SIZES[0]);
    tab(&window);
    tab(&window);
    assert!(has(&window, LONG_TEXT));
    window.set_page(Page::Audio);
    resized(&window, SIZES[0]);
    assert!(!has(&window, LONG_TEXT), "the tip outlived its page");
}

#[test]
fn a_chord_that_is_not_folded_is_no_tab_stop() {
    let window = open(&layout("fold-none", None));
    // The rail, then Change hotkey at once.
    tab(&window);
    tab(&window);
    press(&window, " ".into());
    assert!(has(&window, "Left Ctrl"), "Change hotkey had no focus");
}

/// Esc plus 12 keys: refused, and the row folds, so the tooltip must name the chips on screen.
#[test]
fn a_refused_chord_tells_the_keys_shown() {
    let window = open(&layout("refused-tip", None));
    click(&window, "Change hotkey");
    let keys = [
        "Esc", "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
    ];
    for key in keys {
        click(&window, key);
    }
    assert!(has(&window, &format!("New hotkey: {}", keys.join(" + "))));
    assert!(!has(&window, "No keys yet"));
}

/// UI rule 25: Apply and Cancel are in view with the keyboard open at the small window sizes and
/// the page does not scroll.
#[test]
fn the_open_keyboard_fits_the_small_sizes_without_scrolling() {
    let l = layout("open-fit", None);
    let window = open(&l);
    click(&window, "Change hotkey");
    for (w, h) in [(720.0, 520.0), (1000.0, 700.0), (1280.0, 860.0)] {
        resized(&window, (w, h));
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(1));
        let apply = rect(&one(&window, "Apply hotkey"));
        let last = rect(&one(&window, "Cancel hotkey change"));
        assert!(
            apply[3] <= h && last[3] <= h,
            "{w}x{h}: Apply {apply:?}, Cancel {last:?}"
        );
        assert_eq!(window.get_scroll_y(), 0.0, "{w}x{h} scrolled");
    }
}

#[test]
fn opening_the_keyboard_keeps_the_content_edges_at_every_size() {
    let l = layout("edges", Some(LONG));
    let window = open(&l);
    let many = [
        "Left Ctrl",
        "F1",
        "F2",
        "F3",
        "F4",
        "F5",
        "F6",
        "F7",
        "F8",
        "F9",
        "F10",
        "F11",
        "F12",
        "Q",
        "W",
        "E",
    ];
    let pending = "New hotkey: Ctrl + F1 + F2 + F3 + F4 + F5 + F6 + F7 + F8 + F9 + F10 + F11 + F12 + Q + W + E";
    for size in SIZES {
        resized(&window, size);
        let closed = rect(&one(&window, "Change hotkey"));
        click(&window, "Change hotkey");
        resized(&window, size);
        assert_eq!(
            closed,
            rect(&one(&window, "Change hotkey")),
            "{size:?}: the Keys row moved"
        );
        for key in many {
            click(&window, key);
        }
        resized(&window, size);
        let row = rect(&one(&window, pending));
        let apply = rect(&one(&window, "Apply hotkey"));
        assert!(
            row[2] <= apply[2],
            "{size:?}: pending row {row:?}, Apply {apply:?}"
        );
        for chip in chips(&window, "chip") {
            let r = rect(&chip);
            assert!(r[2] <= apply[2], "{size:?}: chip {r:?} past the card");
        }
        click(&window, "Cancel hotkey change");
    }
}
