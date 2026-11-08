//! Overlay and System pages under the testing backend, on a temporary Layout. The Preview
//! request is checked by a unit test in `pages::overlay`: `open` takes no fake daemon link yet.

mod common;

use std::fs;
use std::time::Duration;

use common::{config, has, layout, open, with_role};
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use oxi_core::config::{OverlayPosition, Theme};
use oxi_core::overlay::{Rgba, palette};
use oxi_core::paths::Layout;
use oxi_settings::{AppWindow, Page, Shell, ThemeChoice};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{Color, ComponentHandle};

fn card(window: &AppWindow, label: &str) -> ElementHandle {
    with_role(window, label, AccessibleRole::RadioButton)
}

fn switch(window: &AppWindow, label: &str) -> ElementHandle {
    with_role(window, label, AccessibleRole::Switch)
}

fn text(l: &Layout) -> String {
    fs::read_to_string(l.config_file()).unwrap_or_default()
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

const POSITION_CARDS: [&str; 6] = [
    "Top left",
    "Top centre",
    "Top right",
    "Bottom left",
    "Bottom centre",
    "Bottom right",
];

#[test]
fn positions_are_one_group_with_one_checked_and_arrows_cross_rows() {
    let l = layout("position-keys", None);
    let window = open(&l, Page::Overlay);
    // The query yields an element once per descendant it matched through: count distinct spots.
    let mut groups: Vec<(Option<String>, i32, i32)> =
        i_slint_backend_testing::ElementQuery::from_root(&window)
            .match_descendants()
            .find_all()
            .into_iter()
            .filter(|e| e.accessible_role() == Some(AccessibleRole::RadioGroup))
            .map(|e| {
                let p = e.absolute_position();
                #[allow(clippy::cast_possible_truncation, reason = "logical px on one screen")]
                (
                    e.accessible_label().map(|l| l.to_string()),
                    p.x as i32,
                    p.y as i32,
                )
            })
            .collect();
    groups.dedup();
    assert_eq!(groups.len(), 1, "{groups:?}");
    assert_eq!(groups[0].0.as_deref(), Some("Position"));
    let checked = || {
        POSITION_CARDS
            .into_iter()
            .filter(|c| card(&window, c).accessible_checked() == Some(true))
            .collect::<Vec<_>>()
    };
    assert_eq!(checked(), ["Bottom right"]);

    let key = |key: Key| {
        window
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
        window
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
    };
    card(&window, "Top right").mock_single_click(PointerEventButton::Left);
    assert_eq!(checked(), ["Top right"]);
    key(Key::DownArrow);
    assert_eq!(checked(), ["Bottom right"]);
    assert_eq!(
        config(&l).config.overlay.position,
        OverlayPosition::BottomRight
    );
    key(Key::DownArrow);
    assert_eq!(checked(), ["Bottom right"], "stops at the last row");
    key(Key::LeftArrow);
    key(Key::UpArrow);
    assert_eq!(checked(), ["Top centre"]);
    key(Key::RightArrow);
    key(Key::RightArrow);
    assert_eq!(checked(), ["Top right"], "Right stops at the row edge");
}

#[test]
fn position_svgs_use_the_overlay_palette() {
    let hex = |Rgba(r, g, b, _): Rgba| format!("#{r:02x}{g:02x}{b:02x}");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/img");
    for spot in [
        "top-left",
        "top-centre",
        "top-right",
        "bottom-left",
        "bottom-centre",
        "bottom-right",
    ] {
        let svg = fs::read_to_string(dir.join(format!("overlay-{spot}.svg"))).unwrap();
        for colour in [hex(palette::SURFACE), hex(palette::RECORDING)] {
            assert!(svg.contains(&colour), "overlay-{spot}.svg lacks {colour}");
        }
    }
}

#[test]
fn preview_is_off_without_the_daemon() {
    let l = layout("preview-offline", None);
    let window = open(&l, Page::Overlay);
    let preview = with_role(&window, "Preview", AccessibleRole::Button);
    assert_eq!(preview.accessible_enabled(), Some(false));
    assert_eq!(
        preview.accessible_description().as_deref(),
        Some(oxi_core::strings::PREVIEW_OFFLINE)
    );
}

#[test]
fn autostart_says_it_takes_effect_later() {
    let l = layout("autostart-note", None);
    let window = open(&l, Page::System);
    assert!(has(
        &window,
        "Takes effect in a later version: OxiYapper does not add itself to sign-in yet."
    ));
}
