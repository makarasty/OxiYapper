//! Overlay and System pages under the testing backend, on a temporary Layout. The Preview
//! request is checked by a unit test in `pages::overlay`: `open` takes no fake daemon link yet.

mod common;

use std::fs;
use std::time::Duration;

use common::layout;
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use oxi_core::config::{ConfigFile, OverlayPosition, Theme};
use oxi_core::paths::Layout;
use oxi_settings::{AppWindow, Page, Shell, ThemeChoice};
use slint::{Color, ComponentHandle};

fn open(layout: &Layout, page: Page) -> AppWindow {
    i_slint_backend_testing::init_no_event_loop();
    let window = oxi_settings::open(layout.clone()).unwrap();
    window.show().unwrap();
    window.set_page(page);
    window
}

fn with_role(window: &AppWindow, label: &str, role: AccessibleRole) -> ElementHandle {
    ElementHandle::find_by_accessible_label(window, label)
        .find(|e| e.accessible_role() == Some(role))
        .unwrap_or_else(|| panic!("no {role:?} labelled {label}"))
}

fn card(window: &AppWindow, label: &str) -> ElementHandle {
    with_role(window, label, AccessibleRole::RadioButton)
}

fn switch(window: &AppWindow, label: &str) -> ElementHandle {
    with_role(window, label, AccessibleRole::Switch)
}

fn has(window: &AppWindow, label: &str) -> bool {
    ElementHandle::find_by_accessible_label(window, label)
        .next()
        .is_some()
}

fn text(l: &Layout) -> String {
    fs::read_to_string(l.config_file()).unwrap_or_default()
}

fn config(l: &Layout) -> ConfigFile {
    ConfigFile::parse(&text(l)).unwrap()
}

#[test]
fn position_click_writes_the_kebab_value() {
    let l = layout("position", Some("version = 1\n# mine\n"));
    let window = open(&l, Page::Overlay);
    assert_eq!(
        card(&window, "Bottom right").accessible_checked(),
        Some(true)
    );
    assert_eq!(card(&window, "Top left").accessible_checked(), Some(false));

    for (label, kebab, position) in [
        ("Top left", "top-left", OverlayPosition::TopLeft),
        (
            "Bottom centre",
            "bottom-centre",
            OverlayPosition::BottomCentre,
        ),
        ("Top right", "top-right", OverlayPosition::TopRight),
    ] {
        card(&window, label).invoke_accessible_default_action();
        assert!(
            text(&l).contains(&format!("position = \"{kebab}\"")),
            "{}",
            text(&l)
        );
        assert_eq!(config(&l).config.overlay.position, position);
        assert_eq!(card(&window, label).accessible_checked(), Some(true));
    }
    // One card is checked across both rows.
    assert_eq!(
        card(&window, "Bottom right").accessible_checked(),
        Some(false)
    );
    assert_eq!(
        card(&window, "Bottom centre").accessible_checked(),
        Some(false)
    );
    assert!(text(&l).contains("# mine"));
}

#[test]
fn overlay_switches_write_the_config() {
    let l = layout("overlay-switches", None);
    let window = open(&l, Page::Overlay);
    switch(&window, "Show the overlay").invoke_accessible_default_action();
    switch(&window, "Hide over fullscreen apps").invoke_accessible_default_action();
    let overlay = config(&l).config.overlay;
    assert!(!overlay.enabled);
    assert!(overlay.hide_over_fullscreen);
    assert!(has(&window, "Preview"));
}

#[test]
fn update_check_off_writes_false_only_after_the_confirm() {
    let l = layout("update-off", None);
    let window = open(&l, Page::System);
    let note = "Turn off update checks";
    assert!(!has(&window, note));

    switch(&window, "Check for updates").invoke_accessible_default_action();
    assert!(has(&window, note));
    assert_eq!(
        switch(&window, "Check for updates").accessible_checked(),
        Some(true)
    );
    assert!(!l.config_file().exists(), "written before the confirm");

    // Keep checking: the note goes, nothing is written.
    with_role(&window, "Keep checking", AccessibleRole::Button).invoke_accessible_default_action();
    assert!(!has(&window, note));
    assert!(!l.config_file().exists());

    switch(&window, "Check for updates").invoke_accessible_default_action();
    with_role(&window, note, AccessibleRole::Button).invoke_accessible_default_action();
    assert!(!config(&l).config.system.update_check);
    assert_eq!(
        switch(&window, "Check for updates").accessible_checked(),
        Some(false)
    );
    assert!(!has(&window, note));

    // Back on needs no confirm.
    switch(&window, "Check for updates").invoke_accessible_default_action();
    assert!(config(&l).config.system.update_check);
    assert!(!has(&window, note));
}

#[test]
fn log_text_warns_while_on_and_autostart_writes() {
    let warning = "Dictated text is written to the log files while this is on. Turn it off when you are done.";
    let l = layout("system-switches", None);
    let window = open(&l, Page::System);
    assert!(!has(&window, warning));
    switch(&window, "Log dictated text").invoke_accessible_default_action();
    assert!(config(&l).config.debug.log_text);
    assert!(has(&window, warning));
    switch(&window, "Log dictated text").invoke_accessible_default_action();
    assert!(!config(&l).config.debug.log_text);
    assert!(!has(&window, warning));

    switch(&window, "Start at sign-in").invoke_accessible_default_action();
    assert!(!config(&l).config.system.autostart);
}

#[test]
fn log_text_on_in_the_file_shows_the_warning() {
    let l = layout(
        "log-text-file",
        Some("version = 1\n[debug]\nlog_text = true\n"),
    );
    let window = open(&l, Page::System);
    assert_eq!(
        switch(&window, "Log dictated text").accessible_checked(),
        Some(true)
    );
    assert!(has(
        &window,
        "Dictated text is written to the log files while this is on. Turn it off when you are done."
    ));
}

#[test]
fn dark_thumb_sets_the_scheme() {
    let l = layout("dark", None);
    let window = open(&l, Page::System);
    let dark_error = Color::from_rgb_u8(0xff, 0x8a, 0x80);
    assert_ne!(window.global::<Shell>().get_error_text(), dark_error);
    card(&window, "Dark").invoke_accessible_default_action();
    assert_eq!(window.get_theme(), ThemeChoice::Dark);
    assert_eq!(card(&window, "Dark").accessible_checked(), Some(true));
    assert_eq!(config(&l).config.ui.theme, Theme::Dark);
    // `changed theme` handlers run from the event loop's timer pass.
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    // The error colour follows the style's scheme: its dark value means the scheme flipped.
    assert_eq!(window.global::<Shell>().get_error_text(), dark_error);
}

#[test]
fn locked_config_keeps_both_pages_unwritten() {
    let original = "version = 99\n";
    let l = layout("locked", Some(original));
    let window = open(&l, Page::Overlay);
    card(&window, "Top left").invoke_accessible_default_action();
    assert_eq!(card(&window, "Top left").accessible_checked(), Some(false));
    window.set_page(Page::System);
    assert_eq!(
        switch(&window, "Check for updates").accessible_enabled(),
        Some(false)
    );
    assert_eq!(fs::read_to_string(l.config_file()).unwrap(), original);
}
