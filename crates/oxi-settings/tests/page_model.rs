//! Model page under the testing backend, on a temporary config directory with no daemon. The
//! model files are a few synthetic bytes; the download tests with a fake daemon link live next
//! to the page (`pages::model::tests`), which can bind it to `Ipc::fake()`.

mod common;

use std::fs;
use std::thread;
use std::time::Duration;

use common::{config, has, layout, open, open_again, open_linked, with_role};
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use oxi_core::config::{ConfigFile, Model};
use oxi_core::ipc::Push;
use oxi_core::models::MODELS;
use oxi_core::paths::Layout;
use oxi_core::types::Language;
use oxi_settings::ipc::{Event, Link};
use oxi_settings::{AppWindow, Ipc, Page, Toast};
use slint::ComponentHandle;
use slint::platform::{Key, PointerEventButton, WindowEvent};

/// Wait for the path worker: step the mocked clock (which runs the page's poll timer) until
/// `ready`.
fn wait_for(ready: impl Fn() -> bool) {
    for _ in 0..400 {
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(30));
        if ready() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("the path check did not answer");
}

fn path_saved(layout: &Layout, path: &str) -> bool {
    fs::read_to_string(layout.config_file())
        .ok()
        .and_then(|t| ConfigFile::parse(&t).ok())
        .is_some_and(|f| f.config.model.custom_path == path)
}

fn saved(layout: &Layout) -> Model {
    config(layout).config.model
}

fn radio(window: &AppWindow, label: &str) -> ElementHandle {
    with_role(window, label, AccessibleRole::RadioButton)
}

fn sub_tab(window: &AppWindow, label: &str) {
    with_role(window, label, AccessibleRole::Tab).invoke_accessible_default_action();
}

/// Picks a gallery tile: its card shows below the gallery. It writes no config.
fn view(window: &AppWindow, name: &str) {
    radio(window, name).invoke_accessible_default_action();
}

fn button(window: &AppWindow, label: &str) -> ElementHandle {
    with_role(window, label, AccessibleRole::Button)
}

#[test]
fn rows_show_size_and_status_from_the_files() {
    let l = layout("rows", None);
    fs::create_dir_all(l.models_dir()).unwrap();
    // A cut-off download: the right name, far from the right size.
    fs::write(l.models_dir().join("ggml-large-v3.bin"), b"synthetic").unwrap();
    let window = open(&l, Page::Model);
    // The gallery opens on the model in use.
    assert!(has(&window, "1.6 GB · fast, good for most"));
    assert!(has(&window, "Whisper large-v3 turbo: Not downloaded"));
    // The badge says "Damaged"; the caption only says what to do.
    view(&window, "Whisper large-v3");
    assert!(has(&window, "3.1 GB · download it again"));
    assert!(has(&window, "Whisper large-v3: Damaged"));
    for name in ["Whisper large-v3 turbo", "Whisper large-v3"] {
        view(&window, name);
        button(&window, &format!("Download {name}"));
    }
    // A damaged file can go too.
    assert!(
        !has(&window, "Delete Whisper large-v3 turbo"),
        "nothing on disk"
    );
    view(&window, "Whisper large-v3");
    button(&window, "Delete Whisper large-v3");
    // The test model is not offered.
    assert!(!has(&window, "Whisper tiny (tests and CI)"));
}

#[test]
fn use_writes_model_id_and_viewing_does_not() {
    let l = layout("pick", Some("version = 1\n# mine\n"));
    // Every model on disk at its registry size (sparse): ready, so each card offers Use.
    fs::create_dir_all(l.models_dir()).unwrap();
    for m in MODELS.iter().filter(|m| m.id != "tiny") {
        common::fake_model(&l.models_dir().join(m.file_name), m.size);
    }
    let window = open(&l, Page::Model);
    assert_eq!(
        radio(&window, "Whisper large-v3 turbo").accessible_checked(),
        Some(true)
    );
    view(&window, "Whisper large-v3");
    let before = fs::read_to_string(l.config_file()).unwrap();
    view(&window, "Whisper large-v3 Russian");
    assert_eq!(
        fs::read_to_string(l.config_file()).unwrap(),
        before,
        "viewing a model saved"
    );
    for (label, id) in [
        ("Whisper large-v3", "large-v3"),
        ("Whisper large-v3 Russian", "podlodka"),
        ("Whisper large-v3 turbo", "turbo"),
    ] {
        view(&window, label);
        button(&window, &format!("Use {label}")).invoke_accessible_default_action();
        assert_eq!(saved(&l).id, id, "{label}");
        let used = button(&window, &format!("{label} in use"));
        assert_eq!(used.accessible_enabled(), Some(false));
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
    let window = open(&l, Page::Model);
    wait_for(|| has(&window, "File not found"));

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
    wait_for(|| path_saved(&l, &model));
    assert!(!has(&window, "The model file must end in .bin"));
    assert!(!has(&window, "File not found"));
    let saved = saved(&l);
    assert_eq!((saved.id.as_str(), saved.custom_path), ("custom", model));
}

#[test]
fn relative_custom_path_is_refused() {
    let l = layout("relative", None);
    let window = open(&l, Page::Model);
    view(&window, "Use my own .bin");
    with_role(&window, "Model file", AccessibleRole::TextInput)
        .set_accessible_value("mine.bin".to_owned());
    assert!(has(
        &window,
        "Use the full path to the file, from the drive letter"
    ));
    assert!(!l.config_file().exists(), "a relative path was saved");
}

#[test]
fn typing_a_good_path_picks_the_custom_model() {
    let l = layout("custom-pick", None);
    let window = open(&l, Page::Model);
    // Viewed, not in use: an empty path is no error yet, and Use waits for a file.
    view(&window, "Use my own .bin");
    assert!(!has(&window, "Choose a .bin model file"));
    let use_it = button(&window, "Use the model file");
    assert_eq!(use_it.accessible_enabled(), Some(false));
    assert_eq!(
        use_it.accessible_description().as_deref(),
        Some("Choose a .bin model file")
    );

    let model = l.config_dir.join("mine.bin");
    fs::write(&model, b"synthetic").unwrap();
    with_role(&window, "Model file", AccessibleRole::TextInput)
        .set_accessible_value(format!("\"{}\"", model.display()));
    wait_for(|| path_saved(&l, &model.to_string_lossy()));
    assert_eq!(saved(&l).id, "custom");
    assert_eq!(saved(&l).custom_path, model.to_string_lossy());
    button(&window, "Own model file in use");
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
        let window = open_again(&l, Page::Model);
        sub_tab(&window, "Settings");
        let picker = with_role(&window, "GPU", AccessibleRole::Combobox);
        assert_eq!(
            picker.accessible_value().as_deref(),
            Some(shown),
            "{config}"
        );
    }
}

#[test]
fn unload_picker_shows_the_configured_choice() {
    i_slint_backend_testing::init_no_event_loop();
    for (config, shown) in [
        ("", "After 5 min"),
        // A file from before "right after dictation": 0 still means never.
        ("idle_unload_minutes = 0", "Never"),
        (
            "idle_unload_minutes = 0
unload_immediately = true",
            "Right after dictation",
        ),
        // An older slider value that is not one of the choices stays picked.
        ("idle_unload_minutes = 12", "After 12 min"),
    ] {
        let l = layout(
            "unload",
            Some(&format!(
                "version = 1
[model]
{config}
"
            )),
        );
        let window = open_again(&l, Page::Model);
        sub_tab(&window, "Settings");
        let picker = with_role(&window, "Unload after idle", AccessibleRole::Combobox);
        assert_eq!(
            picker.accessible_value().as_deref(),
            Some(shown),
            "{config}"
        );
    }
}

#[test]
fn sub_tabs_show_one_part_of_the_page() {
    let l = layout("tabs", None);
    let window = open(&l, Page::Model);
    let tab = |label| with_role(&window, label, AccessibleRole::Tab);
    assert_eq!(tab("Models").accessible_item_selected(), Some(true));
    assert!(has(&window, "Use my own .bin") && !has(&window, "GPU"));
    sub_tab(&window, "Settings");
    assert_eq!(tab("Settings").accessible_item_selected(), Some(true));
    assert!(has(&window, "GPU") && has(&window, "Unload after idle"));
    assert!(!has(&window, "Use my own .bin"));
    sub_tab(&window, "Advanced");
    assert!(has(&window, "Processor threads") && !has(&window, "GPU"));
}

#[test]
fn download_without_the_daemon_says_to_start_it() {
    let l = layout("offline", None);
    let window = open(&l, Page::Model);
    view(&window, "Whisper large-v3");
    let download = with_role(&window, "Download Whisper large-v3", AccessibleRole::Button);
    assert_eq!(download.accessible_enabled(), Some(false));
    assert_eq!(
        download.accessible_description().as_deref(),
        Some(oxi_core::strings::DOWNLOAD_NEEDS_DAEMON)
    );
    download.invoke_accessible_default_action();
    assert!(
        !ElementHandle::find_by_accessible_label(&window, "Whisper large-v3 download")
            .any(|e| e.accessible_role() == Some(AccessibleRole::ProgressIndicator))
    );
}

#[test]
fn a_selected_model_not_on_disk_warns() {
    let warning = "Dictation will not start until this is downloaded.";
    let l = layout("not-on-disk", None);
    let window = open(&l, Page::Model);
    assert!(has(&window, warning));
    // Only the card of the model in use warns.
    view(&window, "Whisper large-v3");
    assert!(!has(&window, warning));
}

#[test]
fn locked_config_disables_the_controls() {
    let original = "version = 99\n[model]\nid = \"large-v3\"\n";
    let l = layout("locked", Some(original));
    let window = open(&l, Page::Model);
    let off = |label: &str, role| {
        let e = with_role(&window, label, role);
        assert_eq!(e.accessible_enabled(), Some(false), "{label}");
    };
    off("Russian", AccessibleRole::RadioButton);
    off("More languages", AccessibleRole::Button);
    more_languages(&window);
    assert!(!popup_open(&window), "a locked config opened the popup");
    // Viewing writes nothing, so the gallery stays on; what writes the config is off.
    view(&window, "Use my own .bin");
    off("Model file", AccessibleRole::TextInput);
    off("Use the model file", AccessibleRole::Button);
    view(&window, "Whisper large-v3 turbo");
    assert!(has(&window, "Whisper large-v3 turbo: Not downloaded"));
    // The sub-tabs only navigate: they stay on.
    sub_tab(&window, "Settings");
    off("GPU", AccessibleRole::Combobox);
    off("Unload after idle", AccessibleRole::Combobox);
    assert_eq!(fs::read_to_string(l.config_file()).unwrap(), original);
}

fn segment(window: &AppWindow, label: &str) -> ElementHandle {
    with_role(window, label, AccessibleRole::RadioButton)
}

fn more_languages(window: &AppWindow) {
    button(window, "More languages").invoke_accessible_default_action();
}

fn search(window: &AppWindow, text: &str) {
    with_role(window, "Search languages", AccessibleRole::TextInput)
        .set_accessible_value(text.to_owned());
}

fn pick_row(window: &AppWindow, label: &str) {
    with_role(window, label, AccessibleRole::ListItem).invoke_accessible_default_action();
}

fn press(window: &AppWindow, key: impl Into<slint::SharedString>) {
    let text = key.into();
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text });
}

fn popup_open(window: &AppWindow) -> bool {
    has(window, "Search languages")
}

#[test]
fn language_is_on_the_first_tab_and_auto_keeps_working() {
    let l = layout("language", None);
    let window = open(&l, Page::Model);
    assert_eq!(segment(&window, "Russian").accessible_checked(), Some(true));
    // "More" is a button with a glyph, not a fourth segment of typed dots (DS-17.1).
    button(&window, "More languages");
    assert!(!has(&window, "…") && !has(&window, "..."));

    segment(&window, "Auto").invoke_accessible_default_action();
    let model = saved(&l);
    assert_eq!(model.language, Language::AUTO);
    // Auto is not a language to remember.
    assert_eq!(model.recent_languages, [Language::RU, Language::EN]);
    assert_eq!(segment(&window, "Auto").accessible_checked(), Some(true));
    assert_eq!(
        segment(&window, "Russian").accessible_checked(),
        Some(false)
    );

    segment(&window, "English").invoke_accessible_default_action();
    assert_eq!(saved(&l).language, Language::EN);
}

#[test]
fn a_rare_language_goes_to_the_model_and_the_last_used() {
    let l = layout("language-rare", None);
    let window = open(&l, Page::Model);
    let haw: Language = "haw".parse().unwrap();
    assert!(!popup_open(&window));
    more_languages(&window);
    assert!(popup_open(&window));
    search(&window, "hawaiian");
    pick_row(&window, "Hawaiian");

    assert!(!popup_open(&window), "the popup stayed open");
    let model = saved(&l);
    assert_eq!(model.language, haw);
    assert_eq!(model.recent_languages, [haw, Language::RU]);
    assert_eq!(
        segment(&window, "Hawaiian").accessible_checked(),
        Some(true)
    );
    assert!(!has(&window, "English"), "the oldest one left the segments");
}

#[test]
fn the_popup_searches_english_and_native_names() {
    let l = layout("language-search", None);
    let window = open(&l, Page::Model);
    more_languages(&window);
    let rows = |window: &AppWindow| {
        ElementHandle::find_by_element_type_name(window, "Rectangle")
            .filter(|e| e.accessible_role() == Some(AccessibleRole::ListItem))
            .filter_map(|e| e.accessible_label().map(|s| s.to_string()))
            .collect::<Vec<_>>()
    };
    search(&window, "Chinese");
    assert_eq!(rows(&window), ["Chinese"]);
    search(&window, "中文");
    assert_eq!(rows(&window), ["Chinese"]);
    search(&window, "русский");
    assert_eq!(rows(&window), ["Russian"]);
    assert!(!has(&window, "No language matches"));
    search(&window, "klingon");
    assert_eq!(rows(&window), Vec::<String>::new());
    assert!(has(&window, "No language matches"));
    // Reopening starts from the whole list, not from the last search.
    press(&window, Key::Escape);
    assert!(!popup_open(&window));
    more_languages(&window);
    assert!(!rows(&window).is_empty() && !has(&window, "No language matches"));
}

#[test]
fn the_last_used_languages_rotate_and_the_segments_hold_still() {
    let l = layout("language-rotate", None);
    let window = open(&l, Page::Model);
    let at = |label: &str| segment(&window, label).absolute_position().x;
    let (ru, en) = (at("Russian"), at("English"));
    let zh: Language = "zh".parse().unwrap();

    segment(&window, "English").invoke_accessible_default_action();
    assert_eq!(saved(&l).recent_languages, [Language::EN, Language::RU]);
    assert_eq!((at("Russian"), at("English")), (ru, en), "a segment jumped");

    more_languages(&window);
    search(&window, "chinese");
    pick_row(&window, "Chinese");
    let model = saved(&l);
    assert_eq!(model.recent_languages, [zh, Language::EN]);
    assert!(!has(&window, "Russian"), "the least recent one stays");

    // Coming back from the popup, a language already used moves to the front of the list.
    more_languages(&window);
    search(&window, "english");
    pick_row(&window, "English");
    let model = saved(&l);
    assert_eq!(model.language, Language::EN);
    assert_eq!(model.recent_languages, [Language::EN, zh]);
}

/// How many elements say `label`: a segment and, while its tooltip is up, the tooltip too.
fn copies(window: &AppWindow, label: &str) -> usize {
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    ElementHandle::find_by_accessible_label(window, label).count()
}

/// Tab until the focus is on the language segments: the tooltip of the lit one is up then.
fn tab_to_the_segments(window: &AppWindow, lit: &str) {
    for _ in 0..12 {
        if copies(window, lit) == 2 {
            return;
        }
        press(window, Key::Tab);
    }
    panic!("Tab never reached the language segments");
}

#[test]
fn the_popup_works_from_the_keyboard() {
    let l = layout("language-keys", None);
    let window = open(&l, Page::Model);
    tab_to_the_segments(&window, "Russian");
    // The "more" button is the next Tab stop: it opens the list, and getting there saves nothing.
    press(&window, Key::Tab);
    assert!(!popup_open(&window));
    assert!(!l.config_file().exists(), "Tab onto the more button saved");
    press(&window, " ");
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(350));
    assert!(popup_open(&window));
    assert!(!l.config_file().exists(), "opening the list saved");
    // Esc closes it and the focus is back on the more button, where Enter opens it again.
    press(&window, Key::Escape);
    assert!(!popup_open(&window));
    assert!(!l.config_file().exists(), "closing the list saved");
    assert_eq!(segment(&window, "Russian").accessible_checked(), Some(true));
    press(&window, Key::Return);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(350));
    assert!(popup_open(&window));
    // The focus is in the search field, so the letters narrow the list and Enter takes the match.
    for letter in ["h", "a", "w", "a", "i"] {
        press(&window, letter);
    }
    press(&window, Key::Return);
    assert!(!popup_open(&window));
    assert_eq!(saved(&l).language.as_str(), "haw");
}

#[test]
fn the_arrows_stop_at_auto() {
    let l = layout("language-arrows", None);
    let window = open(&l, Page::Model);
    tab_to_the_segments(&window, "Russian");
    press(&window, Key::RightArrow);
    assert_eq!(saved(&l).language, Language::EN);
    press(&window, Key::RightArrow);
    assert_eq!(saved(&l).language, Language::AUTO);
    // "More languages" is not a segment: the arrow stops at Auto and opens nothing.
    press(&window, Key::RightArrow);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(350));
    assert!(!popup_open(&window), "an arrow opened the list");
    assert_eq!(saved(&l).language, Language::AUTO);
    assert_eq!(saved(&l).recent_languages, [Language::EN, Language::RU]);
    press(&window, Key::LeftArrow);
    assert_eq!(saved(&l).language, Language::EN);
}

#[test]
fn typing_reaches_the_search_when_an_accessibility_action_opened_it() {
    let l = layout("language-a11y-open", None);
    let window = open(&l, Page::Model);
    // Nothing is focused: no click, no Tab. The first tick lays the page out, as the event loop does.
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    more_languages(&window);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(350));
    assert!(popup_open(&window));
    for letter in ["h", "a", "w", "a", "i"] {
        press(&window, letter);
    }
    press(&window, Key::Return);
    assert!(!popup_open(&window));
    assert_eq!(saved(&l).language.as_str(), "haw");
}

#[test]
fn a_language_in_a_segment_stays_there_when_the_one_in_use_leaves() {
    // zh was set by hand: it is shown first, and not yet one of the last used.
    let l = layout(
        "language-stay",
        Some("version = 1\n[model]\nlanguage = \"zh\"\nrecent_languages = [\"ru\", \"en\"]\n"),
    );
    let window = open(&l, Page::Model);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    let at = |label: &str| segment(&window, label).absolute_position().x;
    let (zh, ru) = (at("Chinese"), at("Russian"));
    assert!(zh < ru);

    segment(&window, "Russian").mock_single_click(PointerEventButton::Left);
    let model = saved(&l);
    assert_eq!(model.language, Language::RU);
    // The language that was in use enters the last used, and it is not lost.
    assert_eq!(
        model.recent_languages,
        [Language::RU, "zh".parse().unwrap()]
    );
    assert_eq!((at("Chinese"), at("Russian")), (zh, ru), "a segment jumped");
    assert!(!has(&window, "English"));
    // The second click lands on Russian again, not on what jumped into its place.
    segment(&window, "Chinese").mock_single_click(PointerEventButton::Left);
    assert_eq!(saved(&l).language.as_str(), "zh");
    segment(&window, "Russian").mock_single_click(PointerEventButton::Left);
    assert_eq!(saved(&l).language, Language::RU);
    assert_eq!((at("Chinese"), at("Russian")), (zh, ru), "a segment jumped");
    // Auto replaces nothing.
    segment(&window, "Auto").invoke_accessible_default_action();
    assert_eq!(saved(&l).language, Language::AUTO);
    assert_eq!((at("Chinese"), at("Russian")), (zh, ru), "a segment jumped");
}

#[test]
fn a_language_code_says_its_name_in_a_tooltip() {
    i_slint_backend_testing::init_no_event_loop();
    for (code, name) in [("haw", "Hawaiian"), ("jw", "Javanese")] {
        let l = layout(
            &format!("language-tip-{code}"),
            Some(&format!(
                "version = 1\n[model]\nlanguage = \"{code}\"\nrecent_languages = [\"{code}\", \"ru\"]\n"
            )),
        );
        let window = open_again(&l, Page::Model);
        assert_eq!(
            copies(&window, name),
            1,
            "the tooltip is up before anything"
        );
        // On the keyboard focus.
        tab_to_the_segments(&window, name);
        press(&window, Key::Tab);
        assert_eq!(copies(&window, name), 1, "leaving the segments closes it");

        // On hover, after the delay.
        let window = open_again(&l, Page::Model);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        let seg = segment(&window, name);
        let (at, size) = (seg.absolute_position(), seg.size());
        window.window().dispatch_event(WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(
                at.x + size.width / 2.0,
                at.y + size.height / 2.0,
            ),
        });
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(700));
        assert_eq!(copies(&window, name), 2, "{name} on hover");
    }
}

/// UI rule 25: Models, with the Language row on top, needs no scrollbar at the minimum size.
#[test]
fn the_models_tab_does_not_scroll_at_the_minimum_size() {
    let l = layout("language-fit", None);
    let window = open(&l, Page::Model);
    window
        .window()
        .set_size(slint::LogicalSize::new(720.0, 520.0));
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    window
        .window()
        .dispatch_event(WindowEvent::PointerScrolled {
            position: slint::LogicalPosition::new(500.0, 300.0),
            delta_x: 0.0,
            delta_y: -400.0,
        });
    assert!(has(&window, "Use my own .bin"), "the last card is there");
    assert_eq!(window.get_scroll_y(), 0.0, "Models scrolls at 720 x 520");
}

#[test]
fn a_language_set_by_hand_is_shown() {
    let l = layout(
        "language-hand",
        Some("version = 1\n[model]\nlanguage = \"zh\"\n"),
    );
    let window = open(&l, Page::Model);
    assert_eq!(segment(&window, "Chinese").accessible_checked(), Some(true));
    assert_eq!(
        segment(&window, "Russian").accessible_checked(),
        Some(false)
    );
}

/// A `.part` of `bytes`, sparse so a test can use megabytes.
fn part_of(l: &Layout, bytes: u64) -> std::path::PathBuf {
    fs::create_dir_all(l.models_dir()).unwrap();
    let part = l.models_dir().join("ggml-large-v3.bin.part");
    common::fake_model(&part, bytes);
    part
}

fn delete_button(window: &AppWindow) -> ElementHandle {
    with_role(window, "Delete partial downloads", AccessibleRole::Button)
}

#[test]
fn delete_partials_waits_while_the_daemon_may_be_writing() {
    let l = layout("delete-gate", None);
    let part = part_of(&l, 3_000_000);
    let (ipc, _sent) = Ipc::fake();
    let window = open_linked(&l, Page::Model, ipc.clone());
    sub_tab(&window, "Settings");
    let link = |link| ipc.deliver(&Event::Link(link));
    assert_eq!(delete_button(&window).accessible_enabled(), Some(true));
    // A daemon that does not answer (or speaks another protocol) may still be writing.
    for busy in [
        Link::Connecting,
        Link::Busy,
        Link::Mismatch { daemon_proto: 1 },
        Link::Stuck,
    ] {
        link(busy);
        assert_eq!(
            delete_button(&window).accessible_enabled(),
            Some(false),
            "{busy:?}"
        );
        delete_button(&window).invoke_accessible_default_action();
        assert!(part.exists(), "deleted on {busy:?}");
    }
    // A download heard of before the link dropped is still running behind Busy.
    link(Link::Online);
    ipc.deliver(&Event::Push(Push::DownloadProgress {
        id: "large-v3".into(),
        done: 1,
        total: 2,
    }));
    link(Link::Busy);
    link(Link::Online);
    link(Link::Offline);
    // No daemon: nothing writes.
    assert_eq!(delete_button(&window).accessible_enabled(), Some(true));
    delete_button(&window).invoke_accessible_default_action();
    assert!(!part.exists());
}

#[test]
fn a_small_part_is_sized_in_mb() {
    let l = layout("part-size", None);
    let part = part_of(&l, 3_000_000);
    let (ipc, _sent) = Ipc::fake();
    let window = open_linked(&l, Page::Model, ipc);
    sub_tab(&window, "Settings");
    delete_button(&window).invoke_accessible_default_action();
    assert!(!part.exists());
    assert_eq!(
        window.global::<Toast>().get_text(),
        "Deleted 3 MB of partial downloads"
    );
}

/// UI rules 1, 2: starting a download moves neither the badge nor the button of the card.
#[test]
fn a_state_change_moves_no_column_of_the_card() {
    let l = layout("card-columns", None);
    let (ipc, _sent) = Ipc::fake();
    let window = open_linked(&l, Page::Model, ipc.clone());
    window
        .window()
        .set_size(slint::LogicalSize::new(720.0, 520.0));
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    let badge = |window: &AppWindow| {
        let e = i_slint_backend_testing::ElementQuery::from_root(window)
            .find_all()
            .into_iter()
            .find(|e| {
                e.accessible_label()
                    .is_some_and(|t| t.starts_with("Whisper large-v3 turbo: "))
            })
            .expect("the badge");
        (
            e.absolute_position().x + e.size().width,
            e.accessible_label().unwrap(),
        )
    };
    let rect = |e: &ElementHandle| (e.absolute_position().x, e.size().width);
    let (idle_badge, idle_label) = badge(&window);
    let download = rect(&with_role(
        &window,
        "Download Whisper large-v3 turbo",
        AccessibleRole::Button,
    ));
    ipc.deliver(&Event::Push(Push::DownloadProgress {
        id: "turbo".into(),
        done: 1,
        total: 2,
    }));
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    let (busy_badge, busy_label) = badge(&window);
    assert_ne!(idle_label, busy_label, "the state did not change");
    assert!(
        (idle_badge - busy_badge).abs() < 0.5,
        "badge {idle_badge} -> {busy_badge}"
    );
    let cancel = rect(&with_role(
        &window,
        "Cancel Whisper large-v3 turbo",
        AccessibleRole::Button,
    ));
    assert!(
        (download.0 - cancel.0).abs() < 0.5 && (download.1 - cancel.1).abs() < 0.5,
        "button {download:?} -> {cancel:?}"
    );
}

/// The gallery holds every model and the own file, as tall as its rows: two at the least
/// window, one where four tiles fit side by side; the page does not scroll.
#[test]
fn the_gallery_is_as_tall_as_its_rows() {
    let l = layout("gallery-rows", None);
    let window = open(&l, Page::Model);
    let tiles = |window: &AppWindow| {
        let mut ys: Vec<f32> = [
            "Whisper large-v3 turbo",
            "Whisper large-v3",
            "Whisper large-v3 Russian",
            "Use my own .bin",
        ]
        .iter()
        .map(|n| radio(window, n).absolute_position().y)
        .collect();
        ys.dedup();
        ys.len()
    };
    for (w, h, rows) in [(720.0, 520.0, 2), (1280.0, 860.0, 1)] {
        window.window().set_size(slint::LogicalSize::new(w, h));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        assert_eq!(tiles(&window), rows, "{w} x {h}");
        assert_eq!(window.get_scroll_y(), 0.0);
    }
}

/// UI rule 16: a button that cancels turns red on hover, which only the danger kind does.
#[test]
fn cancel_on_a_downloading_card_is_a_danger_button() {
    let slint = include_str!("../ui/pages/model.slint");
    let at = slint
        .find("text: @tr(\"Cancel\")")
        .expect("the Cancel button");
    let block = &slint[at..slint[at..].find("clicked").map_or(slint.len(), |n| at + n)];
    assert!(block.contains("ButtonKind.danger"), "{block}");
}

/// UI rule 9: row titles on one page start at one x: the language row, the card of the model
/// under the gallery's check and the own file's (an icon each), the Settings rows and the
/// Advanced rows. An Advanced row has no control, so
/// its description takes the whole width instead of wrapping beside an empty column.
#[test]
fn row_titles_start_at_one_x_and_text_rows_use_the_width() {
    let window = open(&layout("title-x", None), Page::Model);
    window
        .window()
        .set_size(slint::LogicalSize::new(720.0, 520.0));
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    let x = |label: &str| {
        with_role(&window, label, AccessibleRole::Text)
            .absolute_position()
            .x
    };
    let language = x("Dictation language");
    for (tab, title) in [
        (None, "Whisper large-v3 turbo: Not downloaded"),
        (Some("Use my own .bin"), "Use my own .bin"),
        (Some("Settings"), "GPU"),
        (Some("Advanced"), "Processor threads"),
    ] {
        match tab {
            Some("Use my own .bin") => view(&window, "Use my own .bin"),
            Some(tab) => sub_tab(&window, tab),
            None => {}
        }
        let got = x(title);
        assert!(
            (got - language).abs() < 0.5,
            "{title} at {got}, language at {language}"
        );
    }
    let line = with_role(
        &window,
        "4 with a GPU; without one, half the processor's threads, at most 8.",
        AccessibleRole::Text,
    );
    assert!(line.size().height < 24.0, "wraps: {:?}", line.size());
}
