//! System page, Recordings folder row: the path field, Browse and the labelled Open button. The
//! rest of the System tests share `page_overlay_system.rs`.

mod common;

use std::fs;
use std::time::Duration;

use common::{config, layout, open, with_role};
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use oxi_settings::{AppWindow, Page};
use slint::ComponentHandle;
use slint::platform::{Key, PointerEventButton, WindowEvent};

fn recordings(l: &oxi_core::paths::Layout) -> AppWindow {
    let window = open(l, Page::System);
    with_role(&window, "Recordings and files", AccessibleRole::Tab)
        .invoke_accessible_default_action();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    window
}

fn field(window: &AppWindow) -> ElementHandle {
    with_role(window, "Recordings folder", AccessibleRole::TextInput)
}

/// Types `text` into the folder field and presses Enter.
fn type_folder(window: &AppWindow, text: &str) {
    let f = field(window);
    f.mock_single_click(PointerEventButton::Left);
    f.set_accessible_value(text.to_owned());
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Return.into(),
    });
    window.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Return.into(),
    });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
}

/// Types `text` into the folder field without Enter, then leaves it with Tab.
fn type_and_tab_away(window: &AppWindow, text: &str) {
    let f = field(window);
    f.mock_single_click(PointerEventButton::Left);
    f.set_accessible_value(text.to_owned());
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Tab.into(),
    });
    window.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Tab.into(),
    });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
}

fn right(e: &ElementHandle) -> f32 {
    e.absolute_position().x + e.size().width
}

#[test]
fn the_field_holds_where_recordings_land_and_enter_saves_a_typed_folder() {
    let l = layout("folder-typed", None);
    let window = recordings(&l);
    assert_eq!(
        field(&window).accessible_value().as_deref(),
        Some(l.history_dir().to_string_lossy().as_ref())
    );
    // Enter on the unchanged default writes nothing: a look does not pin the default folder.
    type_folder(&window, &l.history_dir().to_string_lossy());
    assert!(!l.config_file().exists(), "the unchanged default was saved");

    // In the test's scratch directory, which goes when the test ends, pass or fail.
    let chosen = l.data_dir.join("typed");
    type_folder(&window, &chosen.to_string_lossy());
    assert_eq!(
        config(&l).config.history.folder,
        chosen.to_string_lossy().as_ref()
    );
    let lands = chosen.join(oxi_core::history::OWNED_DIR);
    assert_eq!(
        field(&window).accessible_value().as_deref(),
        Some(lands.to_string_lossy().as_ref())
    );

    // The app's own folder stands for the one around it; empty is the default again.
    type_folder(&window, &lands.to_string_lossy());
    assert_eq!(
        config(&l).config.history.folder,
        chosen.to_string_lossy().as_ref()
    );
    type_folder(&window, "");
    assert_eq!(config(&l).config.history.folder, "");
    assert_eq!(
        field(&window).accessible_value().as_deref(),
        Some(l.history_dir().to_string_lossy().as_ref())
    );
}

#[test]
fn a_relative_folder_is_refused_and_the_field_shows_the_saved_one() {
    let l = layout("folder-relative", None);
    let window = recordings(&l);
    type_folder(&window, "recordings");
    assert!(!l.config_file().exists(), "a relative folder was saved");
    assert_eq!(
        field(&window).accessible_value().as_deref(),
        Some(l.history_dir().to_string_lossy().as_ref())
    );
}

/// Only Enter saves: leaving the field with text in it puts the saved path back.
#[test]
fn leaving_the_field_unsaved_restores_the_saved_path() {
    let l = layout("folder-tab-away", None);
    let window = recordings(&l);
    type_and_tab_away(&window, &l.data_dir.join("elsewhere").to_string_lossy());
    assert_eq!(
        field(&window).accessible_value().as_deref(),
        Some(l.history_dir().to_string_lossy().as_ref())
    );
    assert!(!l.config_file().exists(), "leaving the field saved it");
}

/// Not only Tab: a click on another control, or a sub-tab switch and back, also puts the saved
/// path back, and saves nothing.
#[test]
fn unsaved_folder_text_is_restored_on_any_focus_loss() {
    let l = layout("folder-focus-loss", None);
    let window = recordings(&l);
    let saved = l.history_dir().to_string_lossy().into_owned();
    let typed = l.data_dir.join("elsewhere").to_string_lossy().into_owned();

    let f = field(&window);
    f.mock_single_click(PointerEventButton::Left);
    f.set_accessible_value(typed.clone());
    with_role(&window, "Keep recordings for", AccessibleRole::Spinbox)
        .mock_single_click(PointerEventButton::Left);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    assert_eq!(
        field(&window).accessible_value().as_deref(),
        Some(saved.as_str()),
        "after a click elsewhere"
    );

    let f = field(&window);
    f.mock_single_click(PointerEventButton::Left);
    f.set_accessible_value(typed);
    with_role(&window, "General", AccessibleRole::Tab).invoke_accessible_default_action();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    with_role(&window, "Recordings and files", AccessibleRole::Tab)
        .invoke_accessible_default_action();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    assert_eq!(
        field(&window).accessible_value().as_deref(),
        Some(saved.as_str()),
        "after a sub-tab switch"
    );
    assert!(!l.config_file().exists(), "leaving the field saved it");
}

/// Alt+Tab or Win+V takes the window's focus, not the field's to another control: the typed text
/// stays for when the user comes back.
#[test]
fn typed_folder_text_survives_the_window_losing_focus() {
    let l = layout("folder-deactivate", None);
    let window = recordings(&l);
    let typed = l.data_dir.join("elsewhere").to_string_lossy().into_owned();
    let f = field(&window);
    f.mock_single_click(PointerEventButton::Left);
    f.set_accessible_value(typed.clone());
    window
        .window()
        .dispatch_event(WindowEvent::WindowActiveChanged(false));
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    assert_eq!(field(&window).accessible_value().as_deref(), Some(typed.as_str()));
    window
        .window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    assert_eq!(field(&window).accessible_value().as_deref(), Some(typed.as_str()));
    assert!(!l.config_file().exists(), "deactivating saved the text");
}

/// The switches take no focus on a click, so they put the saved path back themselves.
#[test]
fn a_switch_click_restores_the_saved_path() {
    let l = layout("folder-switch", None);
    let window = recordings(&l);
    let saved = l.history_dir().to_string_lossy().into_owned();
    let typed = l.data_dir.join("elsewhere").to_string_lossy().into_owned();
    for name in ["Save recordings", "Log dictated text"] {
        let f = field(&window);
        f.mock_single_click(PointerEventButton::Left);
        f.set_accessible_value(typed.clone());
        with_role(&window, name, AccessibleRole::Switch).mock_single_click(PointerEventButton::Left);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        assert_eq!(
            field(&window).accessible_value().as_deref(),
            Some(saved.as_str()),
            "after the {name} switch"
        );
    }
}

/// A path that exists but is a file cannot hold the history folder: refused, with the reason shown
/// and the field back on the saved path. The file is in the test's own scratch directory.
#[test]
fn a_typed_file_is_refused_with_a_reason() {
    let l = layout("folder-file", None);
    let file = l.data_dir.join("a.txt");
    fs::write(&file, b"abc").unwrap();
    let window = recordings(&l);
    type_folder(&window, &file.to_string_lossy());
    assert!(!l.config_file().exists(), "a file was saved as the folder");
    assert!(
        window.get_action_error().contains("not a folder"),
        "no reason: {:?}",
        window.get_action_error()
    );
    assert_eq!(
        field(&window).accessible_value().as_deref(),
        Some(l.history_dir().to_string_lossy().as_ref())
    );
}

/// A chosen folder whose drive is gone still shows where recordings would land, in full.
#[test]
fn a_gone_folder_still_shows_its_whole_path() {
    let l = layout("folder-gone", None);
    let gone = l.data_dir.join("no-drive").join("gone");
    fs::write(
        l.config_file(),
        format!(
            "version = 1\n[history]\nfolder = {:?}\n",
            gone.display().to_string()
        ),
    )
    .unwrap();
    let window = recordings(&l);
    let lands = gone.join(oxi_core::history::OWNED_DIR);
    assert_eq!(
        field(&window).accessible_value().as_deref(),
        Some(lands.to_string_lossy().as_ref())
    );
}

/// Items 4 and 7: Open has a label, as Settings and Logs do; it only opens Explorer, so a locked
/// config keeps it, while the field and Browse that change the config are off.
#[test]
fn open_is_labelled_and_stays_live_when_the_config_is_locked() {
    let original = "version = 99\n";
    let l = layout("folder-locked", Some(original));
    let window = recordings(&l);
    let open = with_role(&window, "Open recordings folder", AccessibleRole::Button);
    assert_eq!(open.accessible_enabled(), Some(true));
    // An icon-only button is one control height square; a labelled one is wider.
    assert!(open.size().width > open.size().height, "Open has no label");
    assert_eq!(field(&window).accessible_enabled(), Some(false));
    if let Some(browse) =
        ElementHandle::find_by_accessible_label(&window, "Change recordings folder").next()
    {
        assert_eq!(browse.accessible_enabled(), Some(false));
    }
    type_folder(&window, &std::env::temp_dir().to_string_lossy());
    assert_eq!(fs::read_to_string(l.config_file()).unwrap(), original);
}

/// DS-3.1: the path field fills the control column, so its edges are those of the Save recordings
/// switch's column end and of the Open button under it.
#[test]
fn the_folder_field_fills_the_control_column() {
    let l = layout("folder-column", None);
    let window = recordings(&l);
    for (w, h) in [(720.0_f32, 520.0_f32), (1280.0, 860.0), (1920.0, 1080.0)] {
        window.window().set_size(slint::LogicalSize::new(w, h));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        let f = field(&window);
        let switch = with_role(&window, "Save recordings", AccessibleRole::Switch);
        let open = with_role(&window, "Open recordings folder", AccessibleRole::Button);
        let log = with_role(&window, "Log dictated text", AccessibleRole::Switch);
        for (name, e) in [
            ("save switch", &switch),
            ("open", &open),
            ("log switch", &log),
        ] {
            assert!(
                (right(&f) - right(e)).abs() < 0.5,
                "{w}x{h}: field ends {}, {name} {}",
                right(&f),
                right(e)
            );
        }
        // At least the 240 px column minimum wide, less nothing: the field starts the column.
        assert!(f.size().width >= 240.0 - 0.5, "{w}x{h}: {}", f.size().width);
    }
}
