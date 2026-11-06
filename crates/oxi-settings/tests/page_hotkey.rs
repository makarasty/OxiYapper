//! Hotkey page under the testing backend, on a temporary Layout. The capture tests against the
//! fake daemon link live in `src/pages/hotkey.rs`: `open` makes its own offline link.

mod common;

use std::fs;

use common::layout;
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use oxi_core::config::ConfigFile;
use oxi_core::machine::Mode;
use oxi_core::paths::Layout;
use oxi_settings::{AppWindow, Page};
use slint::ComponentHandle;

fn open(layout: &Layout) -> AppWindow {
    i_slint_backend_testing::init_no_event_loop();
    let window = oxi_settings::open(layout.clone()).unwrap();
    window.show().unwrap();
    window.set_page(Page::Hotkey);
    window
}

fn one(window: &AppWindow, label: &str) -> ElementHandle {
    let mut found = ElementHandle::find_by_accessible_label(window, label);
    let first = found
        .next()
        .unwrap_or_else(|| panic!("no element labelled {label}"));
    assert!(found.next().is_none(), "more than one labelled {label}");
    first
}

fn has(window: &AppWindow, label: &str) -> bool {
    ElementHandle::find_by_accessible_label(window, label)
        .next()
        .is_some()
}

fn click(window: &AppWindow, label: &str) {
    one(window, label).invoke_accessible_default_action();
}

fn config(l: &Layout) -> ConfigFile {
    ConfigFile::parse(&fs::read_to_string(l.config_file()).unwrap()).unwrap()
}

#[test]
fn clicked_keys_apply_as_the_new_chord() {
    let l = layout("apply", Some("version = 1\n[hotkey]\nchord = [\"F13\"]\n"));
    let window = open(&l);
    assert!(has(&window, "F13"));
    assert!(!has(&window, "LCtrl"), "the keyboard is open before Change");

    click(&window, "Change hotkey");
    click(&window, "LCtrl");
    click(&window, "LAlt");
    click(&window, "LWin");
    // A second click takes a key out again.
    click(&window, "LAlt");
    assert_eq!(one(&window, "LCtrl").accessible_checked(), Some(true));
    assert_eq!(one(&window, "LAlt").accessible_checked(), Some(false));
    assert!(has(&window, "New hotkey: Ctrl + Win"));

    click(&window, "Apply hotkey");
    assert_eq!(config(&l).config.hotkey.chord, ["Ctrl", "Win"]);
    assert!(has(&window, "Ctrl + Win"));
    assert!(
        !has(&window, "LCtrl"),
        "the keyboard stays open after Apply"
    );
}

#[test]
fn both_sides_of_a_modifier_are_one_key() {
    let l = layout("sides", None);
    let window = open(&l);
    click(&window, "Change hotkey");
    click(&window, "RCtrl");
    click(&window, "LCtrl");
    click(&window, "F12");
    click(&window, "Apply hotkey");
    assert_eq!(config(&l).config.hotkey.chord, ["Ctrl", "F12"]);
}

#[test]
fn esc_is_refused_with_a_message() {
    let l = layout("esc", None);
    let window = open(&l);
    click(&window, "Change hotkey");
    click(&window, "LCtrl");
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
    assert!(has(&window, "No keys picked"));
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
    assert!(!has(&window, "LCtrl"));
    assert!(has(&window, "Ctrl + Win"));
    assert!(!l.config_file().exists());

    // Opened again, the board starts with nothing picked.
    click(&window, "Change hotkey");
    assert_eq!(one(&window, "F12").accessible_checked(), Some(false));
}

#[test]
fn mode_thumb_writes_toggle() {
    let l = layout("mode", Some("version = 1\n# mine\n"));
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

#[test]
fn locked_config_keeps_change_off() {
    let original = "version = 99\n";
    let l = layout("locked", Some(original));
    let window = open(&l);
    let change = one(&window, "Change hotkey");
    assert_eq!(change.accessible_enabled(), Some(false));
    change.invoke_accessible_default_action();
    assert!(!has(&window, "LCtrl"));
    assert_eq!(fs::read_to_string(l.config_file()).unwrap(), original);
}
