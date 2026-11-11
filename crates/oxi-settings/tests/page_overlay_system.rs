//! Overlay and System pages under the testing backend, on a temporary Layout. The Preview
//! request is checked by a unit test in `pages::overlay`: `open` takes no fake daemon link yet.

mod common;

use std::fs;
use std::time::Duration;

use common::{assert_nothing_moved, config, has, layout, open, with_role};
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use oxi_core::config::{OverlayPosition, Theme};
use oxi_core::paths::Layout;
use oxi_settings::{AppWindow, Page, ThemeChoice, Tokens};
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
    let l = layout("position", Some("version = 2\n# mine\n"));
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
    assert!(!l.config_file().exists(), "written before the confirm");

    // Keep checking: the note goes, nothing is written.
    with_role(&window, "Keep checking", AccessibleRole::Button).invoke_accessible_default_action();
    assert!(!has(&window, note));
    assert_eq!(
        switch(&window, "Check for updates").accessible_checked(),
        Some(true)
    );
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
    let window = open_recordings(&l);
    assert!(!has(&window, warning));
    switch(&window, "Log dictated text").invoke_accessible_default_action();
    assert!(config(&l).config.debug.log_text);
    assert!(has(&window, warning));
    switch(&window, "Log dictated text").invoke_accessible_default_action();
    assert!(!config(&l).config.debug.log_text);
    assert!(!has(&window, warning));

    with_role(&window, "General", AccessibleRole::Tab).invoke_accessible_default_action();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    switch(&window, "Start at sign-in").invoke_accessible_default_action();
    assert!(!config(&l).config.system.autostart);
}

/// UI rule 25: each sub-tab fits the minimum window; at 1000 x 700 too.
#[test]
fn system_sub_tabs_fit_the_small_windows_without_scrolling() {
    let l = layout("system-fit", None);
    let window = open(&l, Page::System);
    for (w, h) in [(720.0_f32, 520.0_f32), (1000.0, 700.0)] {
        window.window().set_size(slint::LogicalSize::new(w, h));
        for (tab, last) in [
            ("General", "Check for updates"),
            ("Recordings and files", "Log dictated text"),
        ] {
            with_role(&window, tab, AccessibleRole::Tab).invoke_accessible_default_action();
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
            let e = switch(&window, last);
            let mut bottom = e.absolute_position().y + e.size().height;
            // The last row's lowest text: the card must not be cut.
            if let Some(text) = ElementHandle::find_by_accessible_label(
                &window,
                "Once a day OxiYapper asks GitHub for the latest version. Nothing installs on its own.",
            )
            .next()
            {
                bottom = bottom.max(text.absolute_position().y + text.size().height);
            }
            assert!(bottom <= h, "{tab} at {w}x{h}: {last} ends at {bottom}");
            assert_eq!(window.get_scroll_y(), 0.0, "{tab} at {w}x{h} scrolled");
        }
    }
}

/// The "!" of a switch row sits just left of the switch it warns about, not alone at the start of
/// the control column (UI rule 9). The folder path fills the column (DS-3.1): `page_system.rs`.
#[test]
fn the_warning_and_the_folder_name_sit_beside_their_controls() {
    let warning = "Dictated text is written to the log files while this is on. Turn it off when you are done.";
    let l = layout(
        "warning-beside",
        Some(
            "version = 2
[debug]
log_text = true
",
        ),
    );
    let window = open_recordings(&l);
    let gap = window.global::<Tokens>().get_control_gap() + 1.0;
    for (w, h) in [(720.0_f32, 520.0_f32), (1280.0, 860.0)] {
        window.window().set_size(slint::LogicalSize::new(w, h));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        let bang = with_role(&window, warning, AccessibleRole::Text);
        let toggle = switch(&window, "Log dictated text");
        let end = bang.absolute_position().x + bang.size().width;
        let apart = toggle.absolute_position().x - end;
        assert!(
            (0.0..=gap).contains(&apart),
            "{w}: ! ends {end}, switch at {apart} from it"
        );
    }
}

#[test]
fn log_text_on_in_the_file_shows_the_warning() {
    let l = layout(
        "log-text-file",
        Some("version = 2\n[debug]\nlog_text = true\n"),
    );
    let window = open_recordings(&l);
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
    let dark_error = Color::from_rgb_u8(0xFF, 0x99, 0xA4);
    assert_ne!(window.global::<Tokens>().get_error_text(), dark_error);
    card(&window, "Dark").invoke_accessible_default_action();
    assert_eq!(window.get_theme(), ThemeChoice::Dark);
    assert_eq!(card(&window, "Dark").accessible_checked(), Some(true));
    assert_eq!(config(&l).config.ui.theme, Theme::Dark);
    // `changed theme` handlers run from the event loop's timer pass.
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    // The error colour follows the style's scheme: its dark value means the scheme flipped.
    assert_eq!(window.global::<Tokens>().get_error_text(), dark_error);
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

/// Read-only config: hovering a tile says why it is off (rule 31), and a click still writes nothing.
#[test]
fn a_locked_tile_says_why_on_hover() {
    let original = "version = 99
";
    let l = layout("locked-tip", Some(original));
    let window = open(&l, Page::Overlay);
    let shown = |window: &AppWindow| {
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(700));
        let tip = window.global::<oxi_settings::Shell>().get_locked_tip();
        ElementHandle::find_by_accessible_label(window, &tip).count()
    };
    assert_eq!(shown(&window), 0, "up before the pointer came");
    let tile = card(&window, "Top left");
    let (at, size) = (tile.absolute_position(), tile.size());
    window.window().dispatch_event(WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(at.x + size.width / 2.0, at.y + size.height / 2.0),
    });
    assert!(shown(&window) > 0, "no tooltip on a locked tile");
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

/// UI rule 25: at 720 x 520 the one card fits whole, with no banner, the offline daemon's and a
/// broken config's, and the six tiles stay one row inside the window.
#[test]
fn the_page_fits_the_minimum_size_with_every_banner() {
    let l = layout("position-fit", None);
    let window = open(&l, Page::Overlay);
    window
        .window()
        .set_size(slint::LogicalSize::new(720.0, 520.0));
    let whole = |e: &ElementHandle| {
        let (p, s) = (e.absolute_position(), e.size());
        p.x >= 0.0 && p.y >= 0.0 && p.x + s.width <= 720.0 && p.y + s.height <= 520.0
    };
    for (banner, daemon) in [
        ("", ""),
        ("", oxi_core::strings::DAEMON_OFFLINE),
        ("Synthetic config trouble.", ""),
    ] {
        window.set_banner_text(banner.into());
        window.set_daemon_text(daemon.into());
        window.set_daemon_start(!daemon.is_empty());
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        let preview = with_role(&window, "Preview", AccessibleRole::Button);
        assert!(whole(&preview), "Preview is cut (banner {banner:?})");
        let row = card(&window, POSITION_CARDS[0]).absolute_position().y;
        for label in POSITION_CARDS {
            let tile = card(&window, label);
            assert!(whole(&tile), "{label} is cut (banner {banner:?})");
            assert!(
                (tile.absolute_position().y - row).abs() < 0.5,
                "{label} left the row"
            );
        }
        let at = switch(&window, "Show the overlay").absolute_position();
        window
            .window()
            .dispatch_event(WindowEvent::PointerScrolled {
                position: slint::LogicalPosition::new(at.x + 4.0, at.y + 4.0),
                delta_x: 0.0,
                delta_y: -100_000.0,
            });
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(500));
        assert_eq!(window.get_scroll_y(), 0.0, "scrolls (banner {banner:?})");
    }
}

/// Tab alone reaches the tiles; an arrow then moves the pick. No click anywhere.
#[test]
fn tab_reaches_the_position_tiles_and_arrows_pick() {
    let l = layout("position-tab", None);
    let window = open(&l, Page::Overlay);
    let press = |key: Key| {
        window
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
        window
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
    };
    let reached = (0..30).any(|_| {
        press(Key::Tab);
        press(Key::LeftArrow);
        card(&window, "Bottom centre").accessible_checked() == Some(true)
    });
    assert!(reached, "Tab never reached the position tiles");
    assert_eq!(
        config(&l).config.overlay.position,
        OverlayPosition::BottomCentre
    );
}

/// The picked spot's name is the Position row's description and follows the pick.
#[test]
fn the_position_row_names_the_picked_spot() {
    let l = layout("position-name", None);
    let window = open(&l, Page::Overlay);
    // The tiles carry the names too: look for the visible text, not the radio labels.
    let shown = |name: &str| {
        ElementHandle::find_by_accessible_label(&window, name)
            .any(|e| e.accessible_role() == Some(AccessibleRole::Text))
    };
    assert!(shown("Bottom right"));
    card(&window, "Top centre").invoke_accessible_default_action();
    assert!(shown("Top centre"));
    assert!(!shown("Bottom right"));
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
    // The reason is the button's tooltip; no second sentence beside it (the banner has one).
    assert!(!has(&window, "Start OxiYapper to preview."));
    assert!(!has(&window, oxi_core::strings::PREVIEW_OFFLINE));
}

#[test]
fn app_language_is_its_own_picker_and_says_it_is_not_the_dictation_language() {
    let l = layout("app-language", None);
    let window = open(&l, Page::System);
    let picker = with_role(&window, "App language", AccessibleRole::Combobox);
    assert_eq!(picker.accessible_value().as_deref(), Some("English"));
    assert!(has(
        &window,
        "Menus and labels of this window. It does not change the language you dictate in."
    ));
    assert!(!l.config_file().exists(), "showing it writes nothing");
}

#[test]
fn autostart_no_longer_says_it_takes_effect_later() {
    let l = layout("autostart-note", None);
    let window = open(&l, Page::System);
    assert!(has(&window, "Start at sign-in"));
    assert!(!has(&window, "Takes effect in a later version."));
}

const FOLDER_MISSING: &str =
    "This folder is gone. Nothing is saved until it is back or you choose another one.";
const FOLDER_READ_ONLY: &str =
    "This folder cannot be written to. Nothing is saved: choose another one.";
const FOLDER_IS_A_FILE: &str = "This is a file, not a folder. Nothing is saved: choose a folder.";
const FOLDER_REFUSED: &str = "The \"OxiYapper history\" folder here holds other files. Nothing is saved: choose another one.";

fn history_config(folder: &std::path::Path) -> String {
    format!(
        "version = 2\n[history]\nfolder = {:?}\n",
        folder.display().to_string()
    )
}

/// The Recordings folder field's text, which holds the whole path where recordings land.
fn folder_field(window: &AppWindow) -> ElementHandle {
    with_role(window, "Recordings folder", AccessibleRole::TextInput)
}

/// Runs the page's poll until the folder check has answered, then says whether `label` is shown.
fn after_the_check(window: &AppWindow, label: &str) -> bool {
    for _ in 0..100 {
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(30));
        if has(window, label) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

/// The Recordings and files sub-tab of the System page.
fn show_recordings(window: &AppWindow) {
    with_role(window, "Recordings and files", AccessibleRole::Tab)
        .invoke_accessible_default_action();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
}

fn open_recordings(l: &Layout) -> AppWindow {
    let window = open(l, Page::System);
    show_recordings(&window);
    window
}

fn recordings_again(l: &Layout) -> AppWindow {
    let window = common::open_again(l, Page::System);
    show_recordings(&window);
    window
}

#[test]
fn saving_is_on_by_default_and_the_switch_writes_it() {
    let l = layout("history-switch", None);
    let window = open_recordings(&l);
    let on = switch(&window, "Save recordings");
    assert_eq!(on.accessible_checked(), Some(true));
    assert!(!l.config_file().exists(), "showing it writes nothing");
    on.invoke_accessible_default_action();
    assert!(!config(&l).config.history.enabled);
    on.invoke_accessible_default_action();
    assert!(config(&l).config.history.enabled);
}

#[test]
fn retention_fields_show_the_defaults_and_write_what_is_typed() {
    let l = layout("history-limits", None);
    let window = open_recordings(&l);
    let days = with_role(&window, "Keep recordings for", AccessibleRole::Spinbox);
    let size = with_role(&window, "Recordings size limit", AccessibleRole::Spinbox);
    assert_eq!(days.accessible_value().as_deref(), Some("30"));
    assert_eq!(size.accessible_value().as_deref(), Some("2"));

    days.set_accessible_value("14");
    assert_eq!(config(&l).config.history.max_days, 14);
    size.set_accessible_value("5");
    assert_eq!(config(&l).config.history.max_bytes, 5 << 30);
    // Out of range is clamped by the field; 0 is "no limit" in the config.
    days.set_accessible_value("5000");
    assert_eq!(config(&l).config.history.max_days, 999);
    days.set_accessible_value("0");
    assert_eq!(config(&l).config.history.max_days, 0);
}

#[test]
fn a_size_limit_under_a_gigabyte_is_not_shown_as_no_limit_and_stays_unwritten() {
    let original = "version = 2\n[history]\nmax_bytes = 5\n";
    let l = layout("history-small-cap", Some(original));
    let window = open_recordings(&l);
    let size = with_role(&window, "Recordings size limit", AccessibleRole::Spinbox);
    assert_eq!(size.accessible_value().as_deref(), Some("1"));
    assert_eq!(fs::read_to_string(l.config_file()).unwrap(), original);
    // The daemon trims to 5 bytes, not 1 GB: the row says so.
    let note = "The settings file limits recordings to 5 bytes; the field shows it rounded up. Older recordings are removed past 5 bytes.";
    assert!(has(&window, note));
    size.set_accessible_value("3");
    assert_eq!(config(&l).config.history.max_bytes, 3 << 30);
    assert!(!has(&window, note), "a typed whole GB is what it shows");
}

#[test]
fn a_stored_limit_above_the_field_maximum_is_never_lowered_by_a_step() {
    let original = "version = 2\n[history]\nmax_days = 1500\nmax_bytes = 2147483648000\n";
    let l = layout("history-over-max", Some(original));
    let window = open_recordings(&l);
    let days = with_role(&window, "Keep recordings for", AccessibleRole::Spinbox);
    let size = with_role(&window, "Recordings size limit", AccessibleRole::Spinbox);
    assert_eq!(days.accessible_value().as_deref(), Some("1500"));
    assert_eq!(size.accessible_value().as_deref(), Some("2000"));
    days.invoke_accessible_increment_action();
    size.invoke_accessible_increment_action();
    days.set_accessible_value("5000");
    assert_eq!(fs::read_to_string(l.config_file()).unwrap(), original);
    // Down is the person's choice and is saved.
    days.invoke_accessible_decrement_action();
    assert_eq!(config(&l).config.history.max_days, 1499);
}

#[test]
fn the_limits_say_they_act_on_the_folder_shown_even_while_off() {
    let l = layout("history-honest", None);
    let window = open_recordings(&l);
    assert!(has(
        &window,
        "Keeps audio and text in the folder below up to these limits. Older ones are deleted, even while off."
    ));
}

#[test]
fn the_default_folder_is_named_and_warns_of_nothing() {
    let l = layout("history-default", None);
    let window = open_recordings(&l);
    assert_eq!(
        folder_field(&window).accessible_value().as_deref(),
        Some(l.history_dir().to_string_lossy().as_ref())
    );
    assert!(!has(&window, FOLDER_MISSING) && !has(&window, FOLDER_REFUSED));
}

#[test]
fn a_folder_whose_drive_is_gone_is_the_warning_of_the_row() {
    let l = layout("history-gone", None);
    // Its parent is missing too, as with an unplugged drive or an offline share.
    let gone = l.data_dir.join("no-drive").join("gone");
    fs::write(l.config_file(), history_config(&gone)).unwrap();
    let window = open_recordings(&l);
    assert!(after_the_check(&window, FOLDER_MISSING));
    // Where recordings would land, in full, behind the cut path.
    let path = gone.join(oxi_core::history::OWNED_DIR);
    assert_eq!(
        folder_field(&window).accessible_value().as_deref(),
        Some(path.to_string_lossy().as_ref())
    );
}

/// A stored folder that is a file is the row's "!", with the reason.
#[test]
fn a_folder_that_is_a_file_is_the_warning_of_the_row() {
    let l = layout("history-is-file", None);
    let file = l.data_dir.join("a.txt");
    fs::write(&file, b"abc").unwrap();
    fs::write(l.config_file(), history_config(&file)).unwrap();
    let window = open_recordings(&l);
    assert!(after_the_check(&window, FOLDER_IS_A_FILE));
}

#[test]
fn a_deleted_folder_on_a_present_drive_is_not_a_warning() {
    let l = layout("history-deleted", None);
    // The daemon creates it on the first save.
    fs::write(l.config_file(), history_config(&l.data_dir.join("deleted"))).unwrap();
    let window = open_recordings(&l);
    assert!(!after_the_check(&window, FOLDER_MISSING));
    assert!(!has(&window, FOLDER_READ_ONLY));
}

#[test]
fn a_foreign_folder_of_our_name_is_refused_and_ours_is_not() {
    let l = layout("history-refused", None);
    let chosen = l.data_dir.join("chosen");
    let own = chosen.join(oxi_core::history::OWNED_DIR);
    fs::create_dir_all(&own).unwrap();
    fs::write(own.join("somebody-else.txt"), "x").unwrap();
    fs::write(l.config_file(), history_config(&chosen)).unwrap();
    let window = open_recordings(&l);
    assert!(after_the_check(&window, FOLDER_REFUSED));

    // The same folder with the app's marker is fine, on the next visit to the page.
    fs::write(own.join(oxi_core::history::MARKER), "").unwrap();
    window.set_page(Page::Overlay);
    // The query builds the page; without it the System page would never have gone away.
    assert!(has(&window, "Preview"));
    window.set_page(Page::System);
    let cleared = (0..100).any(|_| {
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(30));
        std::thread::sleep(Duration::from_millis(5));
        !has(&window, FOLDER_REFUSED)
    });
    assert!(cleared, "the warning stayed");
}

#[test]
fn the_folder_warning_and_a_long_path_move_nothing() {
    i_slint_backend_testing::init_no_event_loop();
    let shown = |name: &str, folder: Option<String>| {
        let l = layout(name, None);
        if let Some(folder) = &folder {
            let path = l.data_dir.join("no-drive").join(folder);
            fs::write(l.config_file(), history_config(&path)).unwrap();
        }
        let window = recordings_again(&l);
        // The minimum width, tall enough to hold the page: the query skips what is scrolled away.
        window
            .window()
            .set_size(slint::LogicalSize::new(720.0, 900.0));
        if folder.is_some() {
            assert!(after_the_check(&window, FOLDER_MISSING));
        }
        common::spots(&window)
    };
    let plain = shown("history-y-plain", None);
    let long = "a very long folder name with spaces ".repeat(8);
    assert_nothing_moved(&plain, &shown("history-y-long", Some(long)));
}

#[test]
fn a_read_only_config_locks_the_recording_controls() {
    let original = "version = 99\n";
    let l = layout("history-locked", Some(original));
    let window = open_recordings(&l);
    assert_eq!(
        switch(&window, "Save recordings").accessible_enabled(),
        Some(false)
    );
    assert_eq!(
        with_role(&window, "Keep recordings for", AccessibleRole::Spinbox).accessible_enabled(),
        Some(false)
    );
    // The folder button shows only where a folder dialog exists (can-browse).
    if cfg!(windows) {
        assert_eq!(
            with_role(&window, "Change recordings folder", AccessibleRole::Button)
                .accessible_enabled(),
            Some(false)
        );
    }
    assert_eq!(fs::read_to_string(l.config_file()).unwrap(), original);
}

/// UI rule 9: the App language title starts where every other System row title does (it had the
/// page's only row icon).
#[test]
fn system_row_titles_start_at_one_x() {
    let window = open(&layout("system-title-x", None), Page::System);
    window
        .window()
        .set_size(slint::LogicalSize::new(1280.0, 860.0));
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    let x = |label: &str| {
        with_role(&window, label, AccessibleRole::Text)
            .absolute_position()
            .x
    };
    let (language, sign_in) = (x("App language"), x("Start at sign-in"));
    assert!((language - sign_in).abs() < 0.5, "{language} vs {sign_in}");
}
