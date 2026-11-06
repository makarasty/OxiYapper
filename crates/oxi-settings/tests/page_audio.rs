//! Audio page under the testing backend, on a temporary config directory. No test here opens a
//! microphone: the meter opens one on its 30 Hz timer, which only `OXI_MIC_TEST=1` runs.

mod common;

use std::fs;
use std::time::Duration;

use common::layout;
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use oxi_core::config::{Config, ConfigFile};
use oxi_core::paths::Layout;
use oxi_core::types::Language;
use oxi_settings::{AppWindow, Page};
use slint::ComponentHandle;
use slint::platform::{Key, PointerEventButton, WindowEvent};

fn open(layout: &Layout) -> AppWindow {
    i_slint_backend_testing::init_no_event_loop();
    let window = oxi_settings::open(layout.clone()).unwrap();
    window.show().unwrap();
    window.set_page(Page::Audio);
    window
}

/// The element labelled `label` with `role` (a row's label text shares the control's label).
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

fn saved(layout: &Layout) -> Config {
    let text = fs::read_to_string(layout.config_file()).unwrap();
    ConfigFile::parse(&text).unwrap().config
}

#[test]
fn every_control_writes_its_key() {
    let l = layout("controls", Some("version = 1\n# mine\n"));
    let window = open(&l);
    let defaults = Config::default();

    let slider = |label: &str, value: &str| {
        with_role(&window, label, AccessibleRole::Slider).set_accessible_value(value);
    };
    slider("Gain", "-6");
    assert!((saved(&l).audio.gain_db - -6.0).abs() < f64::EPSILON);
    slider("Meter sensitivity", "12");
    assert!((saved(&l).audio.meter_sensitivity_db - 12.0).abs() < f32::EPSILON);
    slider("Tail after release", "300");
    assert_eq!(saved(&l).audio.tail_ms, 300);
    slider("Max length", "15");
    assert_eq!(saved(&l).audio.max_seconds, 900);

    for (label, get) in [
        (
            "Normalise level",
            (|c: &Config| c.audio.normalize) as fn(&Config) -> bool,
        ),
        ("Trim silence", |c: &Config| c.audio.trim),
        ("Noise suppression", |c: &Config| c.audio.denoise),
    ] {
        with_role(&window, label, AccessibleRole::Switch).invoke_accessible_default_action();
        assert_eq!(get(&saved(&l)), !get(&defaults), "{label}");
    }

    for (label, lang) in [
        ("EN", Language::En),
        ("Auto", Language::Auto),
        ("RU", Language::Ru),
    ] {
        with_role(&window, label, AccessibleRole::Button).invoke_accessible_default_action();
        assert_eq!(saved(&l).model.language, lang, "{label}");
    }

    let text = fs::read_to_string(l.config_file()).unwrap();
    assert!(text.contains("# mine"), "comment lost: {text}");
}

#[test]
fn denoise_shows_the_whisper_note() {
    let l = layout("denoise", None);
    let window = open(&l);
    let note = "Noise suppression can make Whisper worse.";
    assert!(!has(&window, note));
    with_role(&window, "Noise suppression", AccessibleRole::Switch)
        .invoke_accessible_default_action();
    assert!(has(&window, note));
}

#[test]
fn config_values_show_on_the_page() {
    let l = layout(
        "values",
        Some("version = 1\n[audio]\ngain_db = 4.0\ntail_ms = 250\n[model]\nlanguage = \"en\"\n"),
    );
    let window = open(&l);
    let gain = with_role(&window, "Gain", AccessibleRole::Slider);
    assert_eq!(gain.accessible_value().as_deref(), Some("4"));
    assert!(has(&window, "250 ms"));
    assert!(has(&window, "10 min"));
}

/// A configured microphone that is not plugged in stays picked; choosing the first row writes
/// the system default (an empty name).
#[test]
fn missing_device_stays_picked_until_default_is_chosen() {
    let l = layout(
        "device",
        Some("version = 1\n[audio]\ndevice = \"Test Mic 9\"\n"),
    );
    let window = open(&l);
    let picker = with_role(&window, "Device", AccessibleRole::Combobox);
    assert_eq!(picker.accessible_value().as_deref(), Some("Test Mic 9"));

    picker.mock_single_click(PointerEventButton::Left);
    let rows = 1 + oxi_platform::capture::list_input_devices().map_or(0, |d| d.len());
    for _ in 0..rows {
        window.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::UpArrow.into(),
        });
    }
    assert_eq!(saved(&l).audio.device, "");
    assert_eq!(picker.accessible_value().as_deref(), Some("System default"));
}

#[test]
fn locked_config_disables_the_controls() {
    let original = "version = 99\n[audio]\ngain_db = 3.0\n";
    let l = layout("locked", Some(original));
    let window = open(&l);
    for (label, role) in [
        ("Gain", AccessibleRole::Slider),
        ("Trim silence", AccessibleRole::Switch),
        ("Device", AccessibleRole::Combobox),
        ("EN", AccessibleRole::Button),
    ] {
        let e = with_role(&window, label, role);
        assert_eq!(e.accessible_enabled(), Some(false), "{label}");
    }
    assert_eq!(fs::read_to_string(l.config_file()).unwrap(), original);
}

/// Real microphone: the meter opens it on the Audio page and closes it when the page is left.
/// `live` outlives the page, so coming back before the next frame shows whether it closed.
#[test]
fn mic_meter_closes_when_the_page_is_left() {
    if std::env::var_os("OXI_MIC_TEST").is_none_or(|v| v != "1") {
        return;
    }
    let l = layout("mic", None);
    let window = open(&l);
    let live = || {
        ElementHandle::find_by_accessible_label(&window, "Level")
            .any(|e| e.accessible_role() == Some(AccessibleRole::ProgressIndicator))
    };
    assert!(!live());
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(100));
    assert!(live(), "the meter did not open the microphone");
    window.set_page(Page::About);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(100));
    window.set_page(Page::Audio);
    assert!(
        !live(),
        "the microphone stayed open after the page was left"
    );
}
