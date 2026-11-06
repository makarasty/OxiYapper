//! Model page under the testing backend, on a temporary config directory with no daemon. The
//! model files are a few synthetic bytes; the download tests with a fake daemon link live next
//! to the page (`pages::model::tests`), which can bind it to `Ipc::fake()`.

mod common;

use std::fs;

use common::layout;
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use oxi_core::config::{ConfigFile, Model};
use oxi_core::paths::Layout;
use oxi_settings::{AppWindow, Page};
use slint::ComponentHandle;

fn open(layout: &Layout) -> AppWindow {
    i_slint_backend_testing::init_no_event_loop();
    open_again(layout)
}

/// [`open`] for a second window in the same test: the platform is set once per test.
fn open_again(layout: &Layout) -> AppWindow {
    let window = oxi_settings::open(layout.clone()).unwrap();
    window.show().unwrap();
    window.set_page(Page::Model);
    window
}

fn with_role(window: &AppWindow, label: &str, role: AccessibleRole) -> ElementHandle {
    ElementHandle::find_by_accessible_label(window, label)
        .find(|e| e.accessible_role() == Some(role))
        .unwrap_or_else(|| panic!("no {role:?} labelled {label}"))
}

fn has(window: &AppWindow, label: &str) -> bool {
    ElementHandle::find_by_accessible_label(window, label)
        .next()
        .is_some()
}

fn saved(layout: &Layout) -> Model {
    let text = fs::read_to_string(layout.config_file()).unwrap();
    ConfigFile::parse(&text).unwrap().config.model
}

fn radio(window: &AppWindow, label: &str) -> ElementHandle {
    with_role(window, label, AccessibleRole::RadioButton)
}

#[test]
fn rows_show_size_and_status_from_the_files() {
    let l = layout("rows", None);
    fs::create_dir_all(l.models_dir()).unwrap();
    // A cut-off download: the right name, far from the right size.
    fs::write(l.models_dir().join("ggml-large-v3.bin"), b"synthetic").unwrap();
    let window = open(&l);
    assert!(has(&window, "1.6 GB · Not downloaded"));
    assert!(has(
        &window,
        "3.1 GB · Damaged or incomplete, download it again"
    ));
    for name in ["Whisper large-v3 turbo", "Whisper large-v3"] {
        with_role(&window, &format!("Download {name}"), AccessibleRole::Button);
    }
    // The test model is not offered.
    assert!(!has(&window, "Whisper tiny (tests and CI)"));
}

#[test]
fn picking_a_row_writes_model_id() {
    let l = layout("pick", Some("version = 1\n# mine\n"));
    let window = open(&l);
    assert_eq!(
        radio(&window, "Whisper large-v3 turbo").accessible_checked(),
        Some(true)
    );
    for (label, id) in [
        ("Whisper large-v3", "large-v3"),
        ("Whisper large-v3 Russian (podlodka v1.2, f16)", "podlodka"),
        ("Use my own .bin", "custom"),
        ("Whisper large-v3 turbo", "turbo"),
    ] {
        radio(&window, label).invoke_accessible_default_action();
        assert_eq!(saved(&l).id, id, "{label}");
        assert_eq!(radio(&window, label).accessible_checked(), Some(true));
    }
    let text = fs::read_to_string(l.config_file()).unwrap();
    assert!(text.contains("# mine"), "comment lost: {text}");
}

#[test]
fn missing_custom_path_shows_an_error() {
    let l = layout(
        "custom",
        Some("version = 1\n[model]\nid = \"custom\"\ncustom_path = \"C:/nowhere/gone.bin\"\n"),
    );
    let window = open(&l);
    assert!(has(&window, "File not found"));

    let field = with_role(&window, "Model file", AccessibleRole::TextInput);
    let notes = l.config_dir.join("notes.txt");
    fs::write(&notes, b"text").unwrap();
    field.set_accessible_value(notes.to_string_lossy().into_owned());
    assert!(has(&window, "The model file must end in .bin"));
    assert_eq!(saved(&l).custom_path, "C:/nowhere/gone.bin");

    let model = l.config_dir.join("mine.bin");
    fs::write(&model, b"synthetic").unwrap();
    let model = model.to_string_lossy().into_owned();
    field.set_accessible_value(model.clone());
    assert!(!has(&window, "The model file must end in .bin"));
    assert!(!has(&window, "File not found"));
    let saved = saved(&l);
    assert_eq!((saved.id.as_str(), saved.custom_path), ("custom", model));
}

#[test]
fn typing_a_good_path_picks_the_custom_model() {
    let l = layout("custom-pick", None);
    let window = open(&l);
    assert!(!has(&window, "Choose a .bin model file"));
    radio(&window, "Use my own .bin").invoke_accessible_default_action();
    assert!(has(&window, "Choose a .bin model file"));
    radio(&window, "Whisper large-v3").invoke_accessible_default_action();
    assert!(!has(&window, "Choose a .bin model file"));

    let model = l.config_dir.join("mine.bin");
    fs::write(&model, b"synthetic").unwrap();
    with_role(&window, "Model file", AccessibleRole::TextInput)
        .set_accessible_value(format!("\"{}\"", model.display()));
    assert_eq!(saved(&l).id, "custom");
    assert_eq!(saved(&l).custom_path, model.to_string_lossy());
    assert_eq!(
        radio(&window, "Use my own .bin").accessible_checked(),
        Some(true)
    );
}

#[test]
fn gpu_picker_shows_the_configured_choice() {
    i_slint_backend_testing::init_no_event_loop();
    for (config, shown) in [
        ("gpu = \"cpu\"", "CPU"),
        ("gpu = \"auto\"", "Auto"),
        // No daemon to list the GPUs: the name is shown, not called missing.
        ("gpu = \"Vulkan7\"", "Vulkan7"),
    ] {
        let l = layout("gpu", Some(&format!("version = 1\n[model]\n{config}\n")));
        let window = open_again(&l);
        let picker = with_role(&window, "GPU", AccessibleRole::Combobox);
        assert_eq!(
            picker.accessible_value().as_deref(),
            Some(shown),
            "{config}"
        );
    }
}

#[test]
fn idle_unload_slider_writes_minutes() {
    let l = layout("idle", None);
    let window = open(&l);
    assert!(has(&window, "5 min"));
    let slider = with_role(&window, "Unload after idle", AccessibleRole::Slider);
    slider.set_accessible_value("12");
    assert_eq!(saved(&l).idle_unload_minutes, 12);
    slider.set_accessible_value("0");
    assert_eq!(saved(&l).idle_unload_minutes, 0);
    assert!(has(&window, "Never"));
}

#[test]
fn download_without_the_daemon_says_to_start_it() {
    let l = layout("offline", None);
    let window = open(&l);
    with_role(&window, "Download Whisper large-v3", AccessibleRole::Button)
        .invoke_accessible_default_action();
    assert!(has(&window, "Start OxiYapper to download models"));
    assert!(
        !ElementHandle::find_by_accessible_label(&window, "Whisper large-v3 download")
            .any(|e| e.accessible_role() == Some(AccessibleRole::ProgressIndicator))
    );
}

#[test]
fn locked_config_disables_the_controls() {
    let original = "version = 99\n[model]\nid = \"large-v3\"\n";
    let l = layout("locked", Some(original));
    let window = open(&l);
    for (label, role) in [
        ("Whisper large-v3 turbo", AccessibleRole::RadioButton),
        ("Use my own .bin", AccessibleRole::RadioButton),
        ("Model file", AccessibleRole::TextInput),
        ("GPU", AccessibleRole::Combobox),
        ("Unload after idle", AccessibleRole::Slider),
    ] {
        let e = with_role(&window, label, role);
        assert_eq!(e.accessible_enabled(), Some(false), "{label}");
    }
    radio(&window, "Whisper large-v3 turbo").invoke_accessible_default_action();
    assert_eq!(fs::read_to_string(l.config_file()).unwrap(), original);
}
