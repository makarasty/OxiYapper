//! Settings shell under the testing backend, on a temporary config directory.

mod common;

use std::fs;

use common::layout;
use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery};
use oxi_core::config::{ConfigFile, Theme};
use oxi_core::paths::Layout;
use oxi_settings::{AppWindow, Page, Shell, ThemeChoice};
use slint::ComponentHandle;
use slint::platform::{Key, PointerEventButton, WindowEvent};

fn open(layout: &Layout) -> AppWindow {
    i_slint_backend_testing::init_no_event_loop();
    let window = oxi_settings::open(layout.clone()).unwrap();
    window.show().unwrap();
    window
}

fn one(window: &AppWindow, label: &str) -> ElementHandle {
    let mut found = ElementHandle::find_by_accessible_label(window, label);
    let first = found
        .next()
        .unwrap_or_else(|| panic!("no element labelled {label}"));
    assert!(
        found.next().is_none(),
        "more than one element labelled {label}"
    );
    first
}

/// The element labelled `label` with `role` (a rail tab and a thumbnail can share a label).
fn with_role(window: &AppWindow, label: &str, role: AccessibleRole) -> ElementHandle {
    ElementHandle::find_by_accessible_label(window, label)
        .find(|e| e.accessible_role() == Some(role))
        .unwrap_or_else(|| panic!("no {role:?} labelled {label}"))
}

fn tab(window: &AppWindow, label: &str) -> ElementHandle {
    with_role(window, label, AccessibleRole::Tab)
}

fn card(window: &AppWindow, label: &str) -> ElementHandle {
    with_role(window, label, AccessibleRole::RadioButton)
}

fn has(window: &AppWindow, label: &str) -> bool {
    ElementHandle::find_by_accessible_label(window, label)
        .next()
        .is_some()
}

#[test]
fn rail_switches_pages() {
    let l = layout("rail", None);
    let window = open(&l);
    assert_eq!(window.get_page(), Page::Hotkey);
    assert!(!has(&window, "Version"));
    tab(&window, "About").invoke_accessible_default_action();
    assert_eq!(window.get_page(), Page::About);
    assert!(has(&window, "Version"));
    assert_eq!(tab(&window, "About").accessible_item_selected(), Some(true));
    tab(&window, "System").invoke_accessible_default_action();
    assert_eq!(window.get_page(), Page::System);
    assert!(!has(&window, "Version"));
}

#[test]
fn about_shows_name_version_and_licence() {
    let l = layout("about", None);
    let window = open(&l);
    window.set_page(Page::About);
    for text in [
        oxi_core::APP_NAME,
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_LICENSE"),
    ] {
        assert!(has(&window, text), "About lacks {text}");
    }
}

#[test]
fn theme_pick_writes_ui_theme() {
    let l = layout("theme", Some("version = 1\n# mine\n"));
    let window = open(&l);
    window.set_page(Page::System);
    with_role(&window, "Theme", AccessibleRole::RadioGroup);
    let dark = card(&window, "Dark");
    assert_eq!(dark.accessible_checked(), Some(false));
    dark.invoke_accessible_default_action();
    assert_eq!(window.get_theme(), ThemeChoice::Dark);
    assert_eq!(card(&window, "Dark").accessible_checked(), Some(true));
    let text = fs::read_to_string(l.config_file()).unwrap();
    assert!(text.contains("# mine"), "comment lost: {text}");
    assert_eq!(
        ConfigFile::parse(&text).unwrap().config.ui.theme,
        Theme::Dark
    );
}

#[test]
fn newer_config_is_read_only() {
    let original = "version = 99\n[ui]\ntheme = \"light\"\n";
    let l = layout("newer", Some(original));
    let window = open(&l);
    assert!(window.global::<Shell>().get_locked());
    assert!(window.get_banner_text().contains("newer version"));
    assert!(!has(&window, "Reset to defaults"));
    assert_eq!(window.get_theme(), ThemeChoice::Light);

    window.set_page(Page::System);
    let dark = card(&window, "Dark");
    assert_eq!(dark.accessible_enabled(), Some(false));
    dark.invoke_accessible_default_action();
    assert_eq!(window.get_theme(), ThemeChoice::Light);
    assert_eq!(fs::read_to_string(l.config_file()).unwrap(), original);
}

#[test]
fn broken_config_shows_its_line_and_resets_keeping_a_backup() {
    let original = "version = 1\n[audio]\ngain_db = \n";
    let l = layout("broken", Some(original));
    let window = open(&l);
    let banner = window.get_banner_text();
    assert!(banner.contains("line 3"), "banner: {banner}");
    assert!(window.global::<Shell>().get_locked());
    assert!(has(&window, "Open file"));

    one(&window, "Reset to defaults").invoke_accessible_default_action();
    let backup = l.config_dir.join("config.toml.bak");
    assert_eq!(fs::read_to_string(backup).unwrap(), original);
    let text = fs::read_to_string(l.config_file()).unwrap();
    assert!(!ConfigFile::parse(&text).unwrap().read_only);
    assert_eq!(window.get_banner_text(), "");
    assert!(!window.global::<Shell>().get_locked());
    assert!(!has(&window, "Reset to defaults"));
}

#[test]
fn thumb_picker_takes_a_click_then_arrow_keys() {
    let l = layout("keys", None);
    let window = open(&l);
    window.set_page(Page::System);
    card(&window, "Light").mock_single_click(PointerEventButton::Left);
    assert_eq!(window.get_theme(), ThemeChoice::Light);
    let key = |key: Key| {
        window
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
        window
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
    };
    key(Key::LeftArrow);
    assert_eq!(window.get_theme(), ThemeChoice::Dark);
    assert_eq!(card(&window, "Dark").accessible_checked(), Some(true));
    key(Key::RightArrow);
    key(Key::RightArrow);
    assert_eq!(
        window.get_theme(),
        ThemeChoice::Light,
        "stops at the last card"
    );
    let text = fs::read_to_string(l.config_file()).unwrap();
    assert_eq!(
        ConfigFile::parse(&text).unwrap().config.ui.theme,
        Theme::Light
    );
}

/// Labels of the banners on screen, sorted, each once (a component instance and its root
/// element both match the role).
fn banners(window: &AppWindow) -> Vec<String> {
    let mut labels: Vec<String> = ElementQuery::from_root(window)
        .match_descendants()
        .match_accessible_role(AccessibleRole::Banner)
        .find_all()
        .iter()
        .filter_map(|e| e.accessible_label().map(|l| l.to_string()))
        .collect();
    labels.sort();
    labels.dedup();
    labels
}

#[test]
fn failed_reset_keeps_the_parse_banner_and_its_buttons() {
    let original = "version = 1
[audio]
gain_db = 
";
    let l = layout("reset-fails", Some(original));
    // A directory where the backup goes makes the copy fail.
    fs::create_dir(l.config_dir.join("config.toml.bak")).unwrap();
    let window = open(&l);
    one(&window, "Reset to defaults").invoke_accessible_default_action();

    assert!(window.global::<Shell>().get_locked());
    assert!(window.get_banner_text().contains("line 3"));
    assert!(has(&window, "Open file"));
    assert!(has(&window, "Reset to defaults"));
    assert_eq!(banners(&window).len(), 2, "parse banner plus the failure");
    assert_eq!(fs::read_to_string(l.config_file()).unwrap(), original);
}

#[test]
fn failed_open_file_keeps_the_parse_banner_and_its_buttons() {
    let l = layout(
        "open-fails",
        Some(
            "version = 1
[audio]
gain_db = 
",
        ),
    );
    let window = open(&l);
    // Gone before "Open file", so the shell has nothing to open.
    fs::remove_file(l.config_file()).unwrap();
    one(&window, "Open file").invoke_accessible_default_action();

    assert!(window.get_banner_text().contains("line 3"));
    assert!(has(&window, "Reset to defaults"));
    assert_eq!(banners(&window).len(), 2, "parse banner plus the failure");
}

#[test]
fn a_successful_save_clears_the_save_error() {
    let l = layout("save-recovers", None);
    let window = open(&l);
    window.set_page(Page::System);
    // A directory in the file's place makes the atomic rename fail.
    fs::create_dir(l.config_file()).unwrap();
    card(&window, "Dark").invoke_accessible_default_action();
    assert_eq!(banners(&window).len(), 1, "the save error shows");

    fs::remove_dir(l.config_file()).unwrap();
    card(&window, "Light").invoke_accessible_default_action();
    assert_eq!(banners(&window), Vec::<String>::new());
    let text = fs::read_to_string(l.config_file()).unwrap();
    assert_eq!(
        ConfigFile::parse(&text).unwrap().config.ui.theme,
        Theme::Light
    );
}
