//! Audio page under the testing backend, on a temporary config directory. No test here opens a
//! microphone: the meter opens one on its 30 Hz timer, which only `OXI_MIC_TEST=1` runs.

mod common;

use std::fs;
use std::time::Duration;

use common::{assert_nothing_moved, config, has, layout, open, open_linked, spots, with_role};
use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery};
use oxi_core::config::Config;
use oxi_core::device::DeviceChoice;
use oxi_core::ipc::{Push, Request};
use oxi_core::machine::Phase;
use oxi_core::paths::Layout;
use oxi_settings::ipc::{Cmd, Event};
use oxi_settings::{AppWindow, Ipc, Page};
use slint::ComponentHandle;
use slint::platform::{Key, PointerEventButton, WindowEvent};

fn saved(layout: &Layout) -> Config {
    config(layout).config
}

/// Opens a sub-tab of the page.
fn tab(window: &AppWindow, label: &str) {
    with_role(window, label, AccessibleRole::Tab).invoke_accessible_default_action();
}

fn checked(window: &AppWindow, label: &str) -> Option<bool> {
    with_role(window, label, AccessibleRole::RadioButton).accessible_checked()
}

/// What the daemon says about the sounds in its `Status`.
fn status(ipc: &Ipc, warnings: &[&str]) {
    ipc.deliver(&Event::Push(Push::Status {
        phase: Phase::Idle,
        model: String::new(),
        device: DeviceChoice::default(),
        backend: String::new(),
        warnings: warnings.iter().map(|w| (*w).to_owned()).collect(),
    }));
}

#[test]
fn every_control_writes_its_key() {
    let l = layout("controls", Some("version = 1\n# mine\n"));
    let window = open(&l, Page::Audio);
    // Tall enough for the whole page: the search skips what is scrolled away.
    window
        .window()
        .set_size(slint::LogicalSize::new(1120.0, 1600.0));
    let defaults = Config::default();

    // The NumberField beside each slider: typing writes the same key as dragging.
    let slider = |label: &str, value: &str| {
        with_role(&window, label, AccessibleRole::Spinbox).set_accessible_value(value);
    };
    slider("Gain", "-6");
    assert!((saved(&l).audio.gain_db - -6.0).abs() < f64::EPSILON);
    slider("Meter sensitivity", "12");
    assert!((saved(&l).audio.meter_sensitivity_db - 12.0).abs() < f32::EPSILON);
    tab(&window, "Recording");
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

    let text = fs::read_to_string(l.config_file()).unwrap();
    assert!(text.contains("# mine"), "comment lost: {text}");
}

#[test]
fn denoise_shows_the_whisper_note() {
    let l = layout("denoise", None);
    let window = open(&l, Page::Audio);
    tab(&window, "Recording");
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
    let window = open(&l, Page::Audio);
    let gain = with_role(&window, "Gain", AccessibleRole::Slider);
    assert_eq!(gain.accessible_value().as_deref(), Some("4"));
    let field = |label: &str| with_role(&window, label, AccessibleRole::Spinbox).accessible_value();
    assert_eq!(field("Gain").as_deref(), Some("+4"));
    tab(&window, "Recording");
    assert_eq!(field("Tail after release").as_deref(), Some("250"));
    assert_eq!(field("Max length").as_deref(), Some("10"));
}

/// UI rule 28: a device name too long for the Combo stands in full in a tooltip; a short one has
/// none.
#[test]
fn long_device_name_stands_in_full_on_hover() {
    let long = "Microphone Array (Synth Smart Sound Technology for Digital Microphones)";
    let l = layout(
        "device-long",
        Some(&format!("version = 1\n[audio]\ndevice = \"{long}\"\n")),
    );
    let window = open(&l, Page::Audio);
    window
        .window()
        .set_size(slint::LogicalSize::new(720.0, 520.0));
    let full = format!("Not connected: {long}");
    let picker = with_role(&window, "Device", AccessibleRole::Combobox);
    assert_eq!(picker.accessible_value().as_deref(), Some(full.as_str()));
    let (at, size) = (picker.absolute_position(), picker.size());
    window.window().dispatch_event(WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(at.x + size.width / 2.0, at.y + size.height / 2.0),
    });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(700));
    let tips = ElementHandle::find_by_accessible_label(&window, &full)
        .filter(|e| e.accessible_role() != Some(AccessibleRole::Combobox))
        .count();
    assert!(tips > 0, "no full-name tooltip");
}

/// A configured microphone that is not plugged in stays picked; choosing the first row writes
/// the system default (an empty name).
#[test]
fn missing_device_stays_picked_until_default_is_chosen() {
    let l = layout(
        "device",
        Some("version = 1\n[audio]\ndevice = \"Test Mic 9\"\n"),
    );
    let window = open(&l, Page::Audio);
    let picker = with_role(&window, "Device", AccessibleRole::Combobox);
    assert_eq!(
        picker.accessible_value().as_deref(),
        Some("Not connected: Test Mic 9")
    );

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
    let window = open(&l, Page::Audio);
    // Tall enough for the whole page under the read-only banner: the search skips what is scrolled away.
    window
        .window()
        .set_size(slint::LogicalSize::new(1120.0, 1400.0));
    let off = |label: &str, role| {
        let e = with_role(&window, label, role);
        assert_eq!(e.accessible_enabled(), Some(false), "{label}");
    };
    off("Gain", AccessibleRole::Slider);
    off("Device", AccessibleRole::Combobox);
    // Hovering a locked control says why (UI rule 31): the Device combo and the Gain slider.
    let tip = window.global::<oxi_settings::Shell>().get_locked_tip();
    for (label, role) in [
        ("Device", AccessibleRole::Combobox),
        ("Gain", AccessibleRole::Slider),
    ] {
        let e = with_role(&window, label, role);
        let (at, size) = (e.absolute_position(), e.size());
        window.window().dispatch_event(WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(
                at.x + size.width / 2.0,
                at.y + size.height / 2.0,
            ),
        });
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(700));
        assert!(has(&window, &tip), "no tooltip on the locked {label}");
        window.window().dispatch_event(WindowEvent::PointerExited);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
        assert!(!has(&window, &tip), "the {label} tooltip stayed");
    }
    tab(&window, "Recording");
    off("Trim silence", AccessibleRole::Switch);
    // The sub-tabs still work; only the settings are locked.
    tab(&window, "Start sound");
    off("Pop", AccessibleRole::RadioButton);
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
    let window = open(&l, Page::Audio);
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

/// At the minimum width the control columns of all rows start at one x: before, the column
/// followed the wrapping of the text on its left, so equal sliders sat 3 to 11 px apart; and a
/// slider row asked for a wider column (280 px) than the device row (240 px), 30 px apart.
#[test]
fn control_columns_start_at_one_x_at_the_minimum_width() {
    let l = layout("columns", None);
    let window = open(&l, Page::Audio);
    window
        .window()
        .set_size(slint::LogicalSize::new(720.0, 1600.0));
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(50));
    let x = |label: &str, role| with_role(&window, label, role).absolute_position().x;
    let gain = x("Gain", AccessibleRole::Slider);
    let same = |label: &str, role| {
        let at = x(label, role);
        assert!((at - gain).abs() < 0.5, "{label}: {at}, Gain {gain}");
    };
    same("Device", AccessibleRole::Combobox);
    same("Meter sensitivity", AccessibleRole::Slider);
    tab(&window, "Recording");
    same("Tail after release", AccessibleRole::Slider);
    same("Max length", AccessibleRole::Slider);
    tab(&window, "Stop sound");
    same("Volume", AccessibleRole::Slider);
}

/// The level track lies exactly under the Device select at the three window sizes: before, the
/// select kept its 240 px at the right of the wider column and the track started left of it.
#[test]
fn level_track_has_the_edges_of_the_device_select() {
    let l = layout("level-edges", None);
    let window = open(&l, Page::Audio);
    for (width, height) in [(720.0, 520.0), (1000.0, 700.0), (1280.0, 860.0)] {
        window
            .window()
            .set_size(slint::LogicalSize::new(width, height));
        // Runs the shell's page-width handler; the 30 Hz meter timer stays asleep.
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        let device = with_role(&window, "Device", AccessibleRole::Combobox);
        let level = ElementQuery::from_root(&window)
            .match_accessible_role(AccessibleRole::ProgressIndicator)
            .find_first()
            .expect("no level meter");
        let edges = |e: &ElementHandle| {
            let x = e.absolute_position().x;
            (x, x + e.size().width)
        };
        let (select, track) = (edges(&device), edges(&level));
        assert!(
            (select.0 - track.0).abs() < 0.5 && (select.1 - track.1).abs() < 0.5,
            "{width}: select {select:?}, track {track:?}"
        );
    }
}

/// Preview plays the picked tile, so it has its own row, not the own-file row's (Choose file).
#[test]
fn preview_is_not_in_the_own_file_row() {
    let window = open(&layout("preview-row", None), Page::Audio);
    tab(&window, "Start sound");
    let y = |label: &str| {
        let e = with_role(&window, label, AccessibleRole::Button);
        e.absolute_position().y + e.size().height / 2.0
    };
    let (choose, preview) = (y("Choose file"), y("Preview"));
    assert!(
        preview > choose + 20.0,
        "Choose file at {choose}, Preview at {preview}"
    );
}

/// Picking a built-in tile writes that sound's own key, plays it, and each sub-tab shows its own.
#[test]
fn picking_a_built_in_saves_it_and_plays_it() {
    let l = layout("builtin", None);
    let (ipc, sent) = Ipc::fake();
    let window = open_linked(&l, Page::Audio, ipc);
    tab(&window, "Start sound");
    assert_eq!(checked(&window, "Rise"), Some(true), "the default");
    with_role(&window, "Pop", AccessibleRole::RadioButton).invoke_accessible_default_action();
    assert_eq!(saved(&l).sounds.start, "pop");
    assert_eq!(saved(&l).sounds.stop, "fall", "the other sound stays");
    assert_eq!(checked(&window, "Pop"), Some(true));
    let previews: Vec<Cmd> = sent.try_iter().collect();
    assert!(
        previews.contains(&Cmd::Send(Request::PreviewSound {
            choice: "pop".into(),
            volume_percent: 50,
        })),
        "{previews:?}"
    );
    tab(&window, "Stop sound");
    assert_eq!(checked(&window, "Fall"), Some(true));
    with_role(&window, "Tick", AccessibleRole::RadioButton).invoke_accessible_default_action();
    assert_eq!(saved(&l).sounds.stop, "tick");
    assert_eq!(saved(&l).sounds.start, "pop");
}

/// An own file from the config stands on its tile with its name; the whole path is the tooltip.
#[test]
fn own_file_shows_on_its_tile() {
    let l = layout(
        "own",
        Some("version = 1\n[sounds]\nstart = 'C:\\Sounds\\my start.wav'\n"),
    );
    let window = open(&l, Page::Audio);
    tab(&window, "Start sound");
    assert_eq!(checked(&window, "Own file"), Some(true));
    let tile = with_role(&window, "Own file", AccessibleRole::RadioButton);
    assert_eq!(
        tile.accessible_description().as_deref(),
        Some(r"C:\Sounds\my start.wav")
    );
    tab(&window, "Stop sound");
    assert_eq!(checked(&window, "Own file"), Some(false));
}

/// The volume is a percent on the page and a fraction in the file.
#[test]
fn volume_slider_writes_the_fraction() {
    let l = layout("volume", None);
    let window = open(&l, Page::Audio);
    tab(&window, "Start sound");
    with_role(&window, "Volume", AccessibleRole::Spinbox).set_accessible_value("55");
    assert!((saved(&l).sounds.volume - 0.55).abs() < 1e-9);
    with_role(&window, "Volume", AccessibleRole::Spinbox).set_accessible_value("400");
    assert!((saved(&l).sounds.volume - 1.0).abs() < 1e-9, "clamped");
}

/// A sound the daemon cannot use is the "!" of the Own file row, moves nothing, and leaves when
/// the daemon stops saying so.
#[test]
fn a_bad_own_file_is_the_warning_of_the_row() {
    let l = layout(
        "bad-file",
        Some("version = 1\n[sounds]\nstart = 'C:\\Sounds\\bad.wav'\n"),
    );
    let (ipc, _sent) = Ipc::fake();
    let window = open_linked(&l, Page::Audio, ipc.clone());
    tab(&window, "Start sound");
    window
        .window()
        .set_size(slint::LogicalSize::new(1280.0, 860.0));
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    let before = spots(&window);
    let text = "Start sound: not a WAV file, using rise";
    assert!(!has(&window, text));
    status(&ipc, &["sounds.start: not a WAV file, using rise"]);
    assert!(has(&window, text), "the \"!\" holds the reason");
    assert_nothing_moved(&before, &spots(&window));
    status(&ipc, &[]);
    assert!(!has(&window, text));
    // A preview that failed says so when no setting has a line of its own.
    status(&ipc, &["sounds.preview: cannot read the file (not found)"]);
    assert!(has(&window, "Preview: cannot read the file (not found)"));
}

/// UI rule 26: under banners the page scrolls, and the sound gallery then shows every tile, so
/// the wheel never has a scroll inside a scroll to pick; with no banner the page does not scroll.
#[test]
fn sound_tabs_never_scroll_inside_a_scroll() {
    let l = layout("sound-fit", None);
    let window = open(&l, Page::Audio);
    window
        .window()
        .set_size(slint::LogicalSize::new(720.0, 520.0));
    let settle = || i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(500));
    let wheel = |at: slint::LogicalPosition| {
        window
            .window()
            .dispatch_event(WindowEvent::PointerScrolled {
                position: at,
                delta_x: 0.0,
                delta_y: -100_000.0,
            });
        settle();
    };
    for (daemon, error) in [
        ("", ""),
        (oxi_core::strings::DAEMON_OFFLINE, ""),
        (oxi_core::strings::DAEMON_OFFLINE, "Synthetic action error."),
    ] {
        for sound in ["Start sound", "Stop sound"] {
            window.set_daemon_text(daemon.into());
            window.set_daemon_start(!daemon.is_empty());
            window.set_action_error(error.into());
            // A page change puts the page back at the top.
            window.set_page(Page::Home);
            settle();
            window.set_page(Page::Audio);
            settle();
            tab(&window, sound);
            settle();
            let at = |label: &str, role| {
                let e = with_role(&window, label, role).absolute_position();
                slint::LogicalPosition::new(e.x + 4.0, e.y + 4.0)
            };
            let (tile, bar) = (
                at("Rise", AccessibleRole::RadioButton),
                at(sound, AccessibleRole::Tab),
            );
            assert!(tile.y < 520.0, "the first tile is below the window");
            // The wheel over a tile: the gallery takes it if it scrolls, else the page does.
            wheel(tile);
            let by_tile = window.get_scroll_y() != 0.0;
            // Over the sub-tabs only the page can take it.
            wheel(bar);
            let by_bar = window.get_scroll_y() != 0.0;
            let what = format!("{sound}, daemon {daemon:?}, error {error:?}");
            assert!(
                !by_bar || by_tile,
                "the page and the gallery both scroll: {what}"
            );
            if daemon.is_empty() && error.is_empty() {
                assert!(!by_bar, "the page scrolls with no banner: {what}");
            }
        }
    }
}
