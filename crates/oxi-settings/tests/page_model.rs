//! Model page under the testing backend, on a temporary config directory with no daemon. The
//! model files are a few synthetic bytes; the download tests with a fake daemon link live next
//! to the page (`pages::model::tests`), which can bind it to `Ipc::fake()`.

mod common;

use std::fs;
use std::thread;
use std::time::Duration;

use common::{config, has, layout, open, open_again, open_linked, with_role};
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use oxi_core::config::{Auto, ConfigFile, Model, PromptSource};
use oxi_core::device::Device;
use oxi_core::ipc::Push;
use oxi_core::models::MODELS;
use oxi_core::paths::Layout;
use oxi_core::types::Language;
use oxi_settings::ipc::{Event, Link};
use oxi_settings::{AppWindow, Ipc, ModelState, Page, Toast};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, Model as _};

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

/// Picks a gallery tile: its card shows below the gallery. It writes no config. The tick first
/// lets the window's height reach the gallery, which shows the rows it has room for.
fn view(window: &AppWindow, name: &str) {
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    radio(window, name).invoke_accessible_default_action();
}

fn button(window: &AppWindow, label: &str) -> ElementHandle {
    with_role(window, label, AccessibleRole::Button)
}

/// Some element's label holds `part`: the card's state line joins the state, its note and where
/// the model runs.
fn shows(window: &AppWindow, part: &str) -> bool {
    i_slint_backend_testing::ElementQuery::from_root(window)
        .find_all()
        .into_iter()
        .any(|e| e.accessible_label().is_some_and(|l| l.contains(part)))
}

#[test]
fn rows_show_size_and_status_from_the_files() {
    let l = layout("rows", None);
    fs::create_dir_all(l.models_dir()).unwrap();
    // A cut-off download: the right name, far from the right size.
    fs::write(l.models_dir().join("ggml-large-v3.bin"), b"synthetic").unwrap();
    let window = open(&l, Page::Model);
    // The gallery opens on the model in use.
    assert!(has(&window, "1.6 GB · Fast · 99 languages"));
    assert!(shows(&window, "Whisper large-v3 turbo: Not downloaded"));
    // The badge says "Damaged"; the caption only says what to do.
    view(&window, "Whisper large-v3");
    assert!(shows(&window, "Damaged · download it again"));
    assert!(shows(&window, "Whisper large-v3: Damaged"));
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
    let l = layout("pick", Some("version = 2\n# mine\n"));
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

/// An absolute path that is not there: a drive path is relative off Windows.
const GONE: &str = if cfg!(windows) {
    "C:/nowhere/gone.bin"
} else {
    "/nowhere/gone.bin"
};

#[test]
fn missing_custom_path_shows_an_error() {
    let l = layout(
        "custom",
        Some(&format!(
            "version = 2\n[model]\nid = \"custom\"\ncustom_path = \"{GONE}\"\n"
        )),
    );
    let window = open(&l, Page::Model);
    wait_for(|| has(&window, "File not found"));

    let field = with_role(&window, "Model file", AccessibleRole::TextInput);
    let notes = l.config_dir.join("notes.txt");
    fs::write(&notes, b"text").unwrap();
    field.set_accessible_value(notes.to_string_lossy().into_owned());
    assert!(has(&window, "The model file must end in .bin"));
    assert_eq!(saved(&l).custom_path, GONE);

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
    view(&window, "Use my own model file");
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
    view(&window, "Use my own model file");
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
        ("gpu = \"cpu\"", "Processor (CPU)"),
        ("gpu = \"auto\"", "Auto (best available)"),
        // No daemon to list the GPUs: the name is shown, not called missing.
        ("gpu = \"Vulkan7\"", "Vulkan7 (graphics card)"),
    ] {
        let l = layout("gpu", Some(&format!("version = 2\n[model]\n{config}\n")));
        let window = open_again(&l, Page::Model);
        sub_tab(&window, "Speed");
        let picker = with_role(&window, "Run the model on", AccessibleRole::Combobox);
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
                "version = 2
[model]
{config}
"
            )),
        );
        let window = open_again(&l, Page::Model);
        sub_tab(&window, "Speed");
        let picker = with_role(&window, "Free memory after", AccessibleRole::Combobox);
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
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    assert!(has(&window, "Use my own model file") && !has(&window, "Run the model on"));
    sub_tab(&window, "Languages");
    assert_eq!(tab("Languages").accessible_item_selected(), Some(true));
    assert!(has(&window, "Dictation language") && has(&window, "All other languages and Auto"));
    assert!(!has(&window, "Use my own model file"));
    sub_tab(&window, "Speed");
    assert_eq!(tab("Speed").accessible_item_selected(), Some(true));
    assert!(has(&window, "Run the model on") && has(&window, "Free memory after"));
    assert!(has(&window, "Processor use") && !has(&window, "Careful mode"));
    sub_tab(&window, "Accuracy");
    assert!(has(&window, "Careful mode") && !has(&window, "Run the model on"));
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
    sub_tab(&window, "Languages");
    let off = |label: &str, role| {
        let e = with_role(&window, label, role);
        assert_eq!(e.accessible_enabled(), Some(false), "{label}");
    };
    off("Russian", AccessibleRole::RadioButton);
    off("More languages", AccessibleRole::Button);
    more_languages(&window);
    assert!(!popup_open(&window), "a locked config opened the popup");
    off("Add language", AccessibleRole::Button);
    // Viewing writes nothing, so the gallery stays on; what writes the config is off.
    sub_tab(&window, "Models");
    view(&window, "Use my own model file");
    off("Model file", AccessibleRole::TextInput);
    off("Use the model file", AccessibleRole::Button);
    view(&window, "Whisper large-v3 turbo");
    assert!(shows(&window, "Whisper large-v3 turbo: Not downloaded"));
    // The sub-tabs only navigate: they stay on.
    sub_tab(&window, "Speed");
    off("Run the model on", AccessibleRole::Combobox);
    off("Free memory after", AccessibleRole::Combobox);
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

/// Item 01: the search finds a language by its code and by what people type for it. "ua" is a
/// country code, Ukrainian is "uk": it is the first row, ahead of Lithuanian, which only contains it.
#[test]
fn the_language_search_finds_by_code_and_by_alias() {
    let l = layout("search-code", None);
    let window = open(&l, Page::Model);
    sub_tab(&window, "Languages");
    for (typed, first) in [("ua", "Ukrainian"), ("uk", "Ukrainian"), ("JP", "Japanese")] {
        more_languages(&window);
        search(&window, typed);
        let rows: Vec<_> = i_slint_backend_testing::ElementQuery::from_root(&window)
            .match_accessible_role(AccessibleRole::ListItem)
            .find_all();
        assert_eq!(
            rows[0].accessible_label().as_deref(),
            Some(first),
            "{typed}"
        );
        // The code and the native name sit in the muted note of the row, once.
        let model = window.global::<ModelState>();
        assert!(
            model
                .get_language_native()
                .row_data(0)
                .unwrap()
                .contains(" · ")
        );
        pick_row(&window, first);
        assert!(!popup_open(&window));
    }
    assert_eq!(saved(&l).language, "ja".parse::<Language>().unwrap());
}

#[test]
fn language_is_on_the_first_tab_and_auto_keeps_working() {
    // A new config detects the language; this one was saved with Russian and keeps it.
    let fresh = layout("language-new", None);
    let new_window = open(&fresh, Page::Model);
    sub_tab(&new_window, "Languages");
    assert_eq!(
        segment(&new_window, "Auto").accessible_checked(),
        Some(true)
    );
    drop(new_window);
    let l = layout("language", Some(RUSSIAN));
    let window = open_again(&l, Page::Model);
    sub_tab(&window, "Languages");
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
    sub_tab(&window, "Languages");
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
    sub_tab(&window, "Languages");
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
    sub_tab(&window, "Languages");
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
    let l = layout("language-keys", Some(RUSSIAN));
    let window = open(&l, Page::Model);
    sub_tab(&window, "Languages");
    tab_to_the_segments(&window, "Russian");
    // The "more" button is the next Tab stop: it opens the list, and getting there saves nothing.
    press(&window, Key::Tab);
    assert!(!popup_open(&window));
    assert_eq!(
        fs::read_to_string(l.config_file()).unwrap(),
        RUSSIAN,
        "Tab onto the more button saved"
    );
    press(&window, " ");
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(350));
    assert!(popup_open(&window));
    assert_eq!(
        fs::read_to_string(l.config_file()).unwrap(),
        RUSSIAN,
        "opening the list saved"
    );
    // Esc closes it and the focus is back on the more button, where Enter opens it again.
    press(&window, Key::Escape);
    assert!(!popup_open(&window));
    assert_eq!(
        fs::read_to_string(l.config_file()).unwrap(),
        RUSSIAN,
        "closing the list saved"
    );
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
    let l = layout("language-arrows", Some(RUSSIAN));
    let window = open(&l, Page::Model);
    sub_tab(&window, "Languages");
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
    sub_tab(&window, "Languages");
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
        Some("version = 2\n[model]\nlanguage = \"zh\"\nrecent_languages = [\"ru\", \"en\"]\n"),
    );
    let window = open(&l, Page::Model);
    sub_tab(&window, "Languages");
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
                "version = 2\n[model]\nlanguage = \"{code}\"\nrecent_languages = [\"{code}\", \"ru\"]\n"
            )),
        );
        let window = open_again(&l, Page::Model);
        sub_tab(&window, "Languages");
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
        sub_tab(&window, "Languages");
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

/// UI rules 25, 26: Models never scrolls the page, from the least window up, with either card
/// below the gallery; the gallery takes the room there is (at 720 x 860 all four rows show).
#[test]
fn the_models_tab_never_scrolls_and_the_gallery_takes_the_room() {
    let l = layout("language-fit", None);
    let window = open(&l, Page::Model);
    let own = i32::try_from(MODELS.len() - 1).unwrap();
    for viewed in [0, own] {
        window.global::<ModelState>().set_viewed(viewed);
        for h in (520..=1000).step_by(20) {
            #[expect(clippy::cast_precision_loss, reason = "a window height")]
            let size = slint::LogicalSize::new(720.0, h as f32);
            window.window().set_size(size);
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
            window
                .window()
                .dispatch_event(WindowEvent::PointerScrolled {
                    position: slint::LogicalPosition::new(500.0, 300.0),
                    delta_x: 0.0,
                    delta_y: -400.0,
                });
            assert_eq!(window.get_scroll_y(), 0.0, "card {viewed} at 720 x {h}");
        }
    }
    // From the top: a grid scrolled in a lower window stays scrolled when it grows (gallery.slint).
    window.global::<ModelState>().set_viewed(0);
    window
        .window()
        .set_size(slint::LogicalSize::new(720.0, 860.0));
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    assert!(has(&window, "Use my own model file"), "the last tile shows");
    let last = radio(&window, "Use my own model file")
        .absolute_position()
        .y;
    let first = radio(&window, "Whisper large-v3 turbo")
        .absolute_position()
        .y;
    assert!(last - first > 300.0, "four rows: {first} -> {last}");
}

#[test]
fn a_language_set_by_hand_is_shown() {
    let l = layout(
        "language-hand",
        Some("version = 2\n[model]\nlanguage = \"zh\"\n"),
    );
    let window = open(&l, Page::Model);
    sub_tab(&window, "Languages");
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
    with_role(
        window,
        "Delete unfinished downloads",
        AccessibleRole::Button,
    )
}

#[test]
fn delete_partials_waits_while_the_daemon_may_be_writing() {
    let l = layout("delete-gate", None);
    let part = part_of(&l, 3_000_000);
    let (ipc, _sent) = Ipc::fake();
    let window = open_linked(&l, Page::Model, ipc.clone());
    sub_tab(&window, "Models");
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
    sub_tab(&window, "Models");
    delete_button(&window).invoke_accessible_default_action();
    assert!(!part.exists());
    assert_eq!(
        window.global::<Toast>().get_text(),
        "Deleted 3 MB of unfinished downloads"
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
    // The column is sized for Segoe UI Variable Text; another OS falls back to a wider font, in
    // which a label can widen its button past the column.
    assert!(
        (download.0 - cancel.0).abs() < 0.5
            && (!cfg!(windows) || (download.1 - cancel.1).abs() < 0.5),
        "button {download:?} -> {cancel:?}"
    );
}

/// The gallery holds every offered model and the own file: four to a row in three rows at
/// 1280 x 860, all shown; three to a row at the least window, two rows shown and the rest
/// scrolled inside the grid. The page does not scroll either way (UI rules 25, 26).
#[test]
fn the_gallery_shows_the_rows_that_fit_and_scrolls_the_rest_inside() {
    let l = layout("gallery-rows", None);
    let window = open(&l, Page::Model);
    let offered: Vec<&str> = MODELS
        .iter()
        .filter(|m| m.id != "tiny")
        .map(|m| m.display_name)
        .chain(["Use my own model file"])
        .collect();
    assert_eq!(offered.len(), 10);
    let rows = |window: &AppWindow, names: &[&str]| {
        let mut ys: Vec<f32> = names
            .iter()
            .map(|n| radio(window, n).absolute_position().y)
            .collect();
        ys.sort_by(f32::total_cmp);
        ys.dedup();
        ys.len()
    };
    window
        .window()
        .set_size(slint::LogicalSize::new(1280.0, 860.0));
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    assert_eq!(rows(&window, &offered), 3, "1280 x 860");
    assert_eq!(window.get_scroll_y(), 0.0);
    window
        .window()
        .set_size(slint::LogicalSize::new(720.0, 520.0));
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    assert_eq!(
        rows(&window, &offered[..6]),
        2,
        "three to a row at 720 x 520"
    );
    assert_eq!(window.get_scroll_y(), 0.0);
}

/// Wait for the models folder measure. No event loop runs here, so the test wakes the page as
/// the worker's `upgrade_in_event_loop` would.
fn measured(window: &AppWindow) {
    let state = window.global::<ModelState>();
    wait_for(|| {
        state.invoke_measured();
        state.get_folder_size() != "Measuring"
    });
}

/// A tile shows the "Recommended" badge only with a machine to judge: no GPU list from the daemon
/// (no daemon here) means no badge rather than a guess.
#[test]
fn no_badge_without_the_daemon_gpu_list() {
    let l = layout("no-badge", None);
    let window = open(&l, Page::Model);
    let tiles = window.global::<ModelState>().get_tiles();
    measured(&window);
    assert!(tiles.iter().all(|t| t.badge.is_empty()));
}

/// Hold 24: with only an integrated GPU listed the model runs on shared memory, so a light model
/// is recommended by the system memory (any Windows machine has the 2.6 GB turbo compact needs),
/// and the card's "i" says why.
#[test]
fn an_integrated_gpu_gets_a_light_model_recommended() {
    let l = layout("badge-igpu", None);
    let (ipc, _sent) = Ipc::fake();
    let window = open_linked(&l, Page::Model, ipc.clone());
    ipc.deliver(&Event::Push(Push::Devices {
        mics: vec![],
        gpus: vec![Device {
            name: "Vulkan0".into(),
            description: "Integrated".into(),
            uma: true,
        }],
    }));
    let state = window.global::<ModelState>();
    measured(&window);
    let marked: Vec<String> = state
        .get_tiles()
        .iter()
        .filter(|t| t.badge == "Recommended")
        .map(|t| t.title.to_string())
        .collect();
    if cfg!(windows) {
        assert_eq!(marked, ["Whisper turbo compact"]);
        // A badge of its own, said with the tile; the caption keeps the facts.
        assert!(shows(&window, "Recommended"));
        assert!(
            state
                .get_tiles()
                .iter()
                .all(|t| !t.caption.contains("Recommended"))
        );
        view(&window, "Whisper turbo compact");
        assert!(shows(&window, "light enough for the processor"));
        assert!(shows(&window, "runs on Integrated"));
    } else {
        assert!(marked.is_empty(), "no hardware read on this OS: {marked:?}");
    }
}

/// Hold 42: the Files rows say what the models folder and the partial downloads take.
#[test]
fn files_rows_say_the_sizes() {
    let l = layout("files-sizes", None);
    let _part = part_of(&l, 3_000_000);
    let turbo = MODELS.iter().find(|m| m.id == "turbo").unwrap();
    common::fake_model(&l.models_dir().join(turbo.file_name), turbo.size);
    let window = open(&l, Page::Model);
    sub_tab(&window, "Models");
    let state = window.global::<ModelState>();
    measured(&window);
    let folder = state.get_folder_size();
    assert!(folder.starts_with("1.6 GB in 1 model"), "{folder}");
    assert!(has(&window, "3 MB in 1 file"));
    assert!(shows(&window, "1.6 GB in 1 model"));
    assert_eq!(state.get_folder_error(), "");
    // Hold 42 "on both tabs": the Models tab header says it too.
    sub_tab(&window, "Models");
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    assert!(shows(&window, "1.6 GB in 1 model"));
}

/// Review b04: a models folder that cannot be read (here the path is a file) says so, with the
/// reason on the row's "!", instead of "Measuring" for ever.
#[test]
fn an_unreadable_models_folder_shows_an_error() {
    let l = layout("files-unreadable", None);
    fs::create_dir_all(l.models_dir().parent().unwrap()).unwrap();
    fs::write(l.models_dir(), b"not a folder").unwrap();
    let window = open(&l, Page::Model);
    sub_tab(&window, "Models");
    let state = window.global::<ModelState>();
    measured(&window);
    assert_eq!(state.get_folder_size(), "Can't read the folder");
    let error = state.get_folder_error();
    assert!(
        error.starts_with("Can't read the models folder: "),
        "{error}"
    );
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
    sub_tab(&window, "Languages");
    let language = x("Dictation language");
    sub_tab(&window, "Models");
    for (tab, title) in [
        (None, "Whisper large-v3 turbo: Not downloaded"),
        (Some("Use my own model file"), "Use my own model file"),
        (Some("Accuracy"), "Careful mode"),
        (Some("Speed"), "Run the model on"),
        (Some("Speed"), "Processor use"),
    ] {
        match tab {
            // Past the two rows shown at 720: picked as the arrows and Enter would.
            Some("Use my own model file") => window
                .global::<ModelState>()
                .set_viewed(i32::try_from(MODELS.len() - 1).unwrap()),
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
        "How many processor threads transcription may use. Auto: 4 now, a graphics card does the heavy work. Max: all of them, the fastest, other programs may slow down. Half: half of them.",
        AccessibleRole::Text,
    );
    assert!(line.size().height < 24.0, "wraps: {:?}", line.size());
}

/// The prompt is a select (your dictionary, none, custom); Custom swaps it for a text field of the
/// same place and size, and the text is kept while another choice is on. The field is typed in, so
/// its binding to the config ends; "Reset to Auto" keeps it (the hint is the person's own text).
#[test]
fn the_prompt_select_swaps_for_a_field_of_the_same_size_and_keeps_the_text() {
    let l = layout("prompt-reset", None);
    let window = open(&l, Page::Model);
    window
        .window()
        .set_size(slint::LogicalSize::new(720.0, 520.0));
    sub_tab(&window, "Accuracy");
    // Slint runs `changed` handlers on the next tick, as the running app does between two clicks.
    let tick = || i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    tick();
    let state = window.global::<ModelState>();
    let select = with_role(&window, "Hint for the model", AccessibleRole::Combobox);
    assert_eq!(
        select.accessible_value().as_deref(),
        Some("Your dictionary (auto)")
    );
    let place = (select.absolute_position(), select.size());
    state.invoke_adv_prompt_source_picked(2);
    tick();
    assert_eq!(saved(&l).advanced.prompt_source, Some(PromptSource::Custom));
    assert!(
        ElementHandle::find_by_accessible_label(&window, "Hint for the model")
            .all(|e| e.accessible_role() != Some(AccessibleRole::Combobox)),
        "the select is gone"
    );
    let field = with_role(&window, "Hint for the model", AccessibleRole::TextInput);
    // The field and the arrow back fill the select's place.
    let back = button(&window, "Choose from the list");
    let (x, y) = (field.absolute_position(), back.absolute_position());
    assert!((x.x - place.0.x).abs() < 0.5 && (x.y - place.0.y).abs() < 0.5);
    assert!(
        (y.x + back.size().width - (place.0.x + place.1.width)).abs() < 0.5,
        "the arrow ends where the select did"
    );
    assert!((field.size().height - place.1.height).abs() < 0.5);
    field.set_accessible_value("Notes.".to_owned());
    // Enter or leaving the field commits it.
    state.invoke_adv_prompt_edited("Notes.".into());
    assert_eq!(saved(&l).advanced.initial_prompt, "Notes.");
    // Back to the list: the text is kept for the next Custom.
    back.invoke_accessible_default_action();
    tick();
    assert_eq!(
        saved(&l).advanced.prompt_source,
        Some(PromptSource::Dictionary)
    );
    assert_eq!(saved(&l).advanced.initial_prompt, "Notes.");
    // A real pick from the open list: the select sets its own index, which ends its binding.
    with_role(&window, "Hint for the model", AccessibleRole::Combobox)
        .invoke_accessible_expand_action();
    pick_row(&window, "None");
    tick();
    assert_eq!(saved(&l).advanced.prompt_source, Some(PromptSource::None));
    assert_eq!(
        with_role(&window, "Hint for the model", AccessibleRole::Combobox)
            .accessible_value()
            .as_deref(),
        Some("None")
    );
    // ia-map 3.2, RT R19: Reset to Auto keeps the hint, the person's own choice and text.
    button(&window, "Reset to Auto").invoke_accessible_default_action();
    tick();
    assert_eq!(saved(&l).advanced.prompt_source, Some(PromptSource::None));
    assert_eq!(
        with_role(&window, "Hint for the model", AccessibleRole::Combobox)
            .accessible_value()
            .as_deref(),
        Some("None")
    );
    // A source pushed from outside still reaches the select whose pick ended its binding.
    state.invoke_adv_prompt_source_picked(0);
    tick();
    assert_eq!(
        with_role(&window, "Hint for the model", AccessibleRole::Combobox)
            .accessible_value()
            .as_deref(),
        Some("Your dictionary (auto)")
    );
    state.invoke_adv_prompt_source_picked(2);
    tick();
    let field = with_role(&window, "Hint for the model", AccessibleRole::TextInput);
    assert_eq!(
        field.accessible_value().as_deref(),
        Some("Notes."),
        "the text is kept"
    );
    button(&window, "Reset to Auto").invoke_accessible_default_action();
    tick();
    assert_eq!(saved(&l).advanced.initial_prompt, "Notes.");
    assert_eq!(saved(&l).advanced.prompt_source, Some(PromptSource::Custom));
}

/// A config from before the choice: a typed prompt opens as Custom with its text in the field.
#[test]
fn an_old_config_with_a_prompt_opens_on_custom() {
    let l = layout(
        "prompt-old",
        Some(
            "version = 2
[model.advanced]
initial_prompt = \"Notes.\"
",
        ),
    );
    let window = open(&l, Page::Model);
    sub_tab(&window, "Accuracy");
    let field = with_role(&window, "Hint for the model", AccessibleRole::TextInput);
    assert_eq!(field.accessible_value().as_deref(), Some("Notes."));
}

/// Max and Half of the threads are choices of their own, and the number beside them shows what
/// they give; Custom opens the number for typing, and an old number loads as Custom.
#[test]
fn the_threads_choices_name_max_and_half_and_old_numbers_load() {
    let logical = thread::available_parallelism().map_or(1, usize::from);
    let l = layout(
        "threads",
        Some(
            "version = 2
[model.advanced]
threads = 3
",
        ),
    );
    let window = open(&l, Page::Model);
    sub_tab(&window, "Speed");
    let shown = |window: &AppWindow| {
        with_role(window, "Processor use", AccessibleRole::Combobox)
            .accessible_value()
            .map(|v| v.to_string())
    };
    // 3 is not Max or Half of every machine, unless the machine is that small.
    let state = window.global::<ModelState>();
    let tick = || i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    tick();
    if logical != 3 && logical / 2 != 3 {
        assert_eq!(shown(&window).as_deref(), Some("Custom"));
    }
    state.invoke_adv_changed(0, 1, 3.0);
    tick();
    assert_eq!(shown(&window).as_deref(), Some("Max"));
    assert_eq!(
        saved(&l).advanced.threads,
        Auto::Set(u8::try_from(logical.clamp(1, 32)).unwrap())
    );
    state.invoke_adv_changed(0, 2, 3.0);
    tick();
    assert_eq!(shown(&window).as_deref(), Some("Half"));
    // Custom stays Custom even when the number it starts from is Half's.
    state.invoke_adv_changed(0, 3, state.get_adv_value().row_data(0).unwrap());
    tick();
    assert_eq!(shown(&window).as_deref(), Some("Custom"));
    state.invoke_adv_changed(0, 0, 3.0);
    tick();
    assert_eq!(shown(&window).as_deref(), Some("Auto"));
    assert_eq!(saved(&l).advanced.threads, Auto::Auto);
}

/// Every engine parameter has its Auto row, each with its own glyph (DS-24.1: one meaning, one
/// icon). The audio window is not one: the engine never reads it, so no row offers it.
#[test]
fn advanced_has_a_row_per_parameter_and_none_shares_a_glyph() {
    let window = open(&layout("adv-rows", None), Page::Model);
    sub_tab(&window, "Accuracy");
    for title in ["Word variety", "Second try when a phrase repeats"] {
        assert!(has(&window, title), "{title}");
    }
    assert!(!has(&window, "Audio window"), "a choice the engine ignores");
    let slint = include_str!("../ui/pages/model.slint");
    // Speed and Accuracy (ia-map 3.2), the former Advanced tab.
    let advanced = &slint[slint.find("ModelState.tab == 2").expect("the Speed tab")..];
    let icons: Vec<&str> = advanced
        .split("icon: Icons.")
        .skip(1)
        .filter_map(|t| t.split(';').next())
        .collect();
    assert!(icons.len() >= 12, "{icons:?}");
    let mut seen = std::collections::HashSet::new();
    for icon in icons {
        assert!(seen.insert(icon), "{icon} is on two Advanced controls");
    }
}

/// DS-23.5, DS-7.3: a card's get slot (Resume, Cancel) and Delete slot keep their place while a
/// download starts and stops, and both end inside the control column.
#[test]
fn a_paused_download_moves_no_button_when_it_resumes_or_cancels() {
    let l = layout("card-slots", None);
    let turbo = MODELS.iter().find(|m| m.id == "turbo").unwrap();
    fs::create_dir_all(l.models_dir()).unwrap();
    common::fake_model(
        &l.models_dir().join(format!("{}.part", turbo.file_name)),
        3_000_000,
    );
    let (ipc, _sent) = Ipc::fake();
    let window = open_linked(&l, Page::Model, ipc.clone());
    window
        .window()
        .set_size(slint::LogicalSize::new(720.0, 520.0));
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    let name = "Whisper large-v3 turbo";
    let x = |label: String| {
        let e = button(&window, &label);
        (
            e.absolute_position().x,
            e.absolute_position().x + e.size().width,
        )
    };
    let resume = x(format!("Resume {name}"));
    let delete = x(format!("Delete {name}"));
    assert!(delete.0 >= resume.1, "Delete is the right-most");
    ipc.deliver(&Event::Push(Push::DownloadProgress {
        id: "turbo".into(),
        done: 1,
        total: 2,
    }));
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    let cancel = x(format!("Cancel {name}"));
    assert!(
        (cancel.0 - resume.0).abs() < 0.5,
        "Resume {resume:?} -> Cancel {cancel:?}"
    );
    assert!(
        !has(&window, &format!("Delete {name}")),
        "no Delete in a running download"
    );
}

/// Every Advanced choice leaves its row the room for the number beside it at the least window
/// (the 240 column: choice + 8 + the 88 side slot). The Combo's default width is the whole
/// column, which pushed the number out of the row.
#[test]
fn the_advanced_choices_leave_room_for_the_number_at_the_least_window() {
    let window = open(&layout("adv-fit", None), Page::Model);
    window
        .window()
        .set_size(slint::LogicalSize::new(720.0, 520.0));
    sub_tab(&window, "Accuracy");
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    let combos = i_slint_backend_testing::ElementQuery::from_root(&window)
        .match_accessible_role(AccessibleRole::Combobox)
        .find_all();
    assert!(combos.len() >= 7, "{} choices", combos.len());
    // The hint select is the whole column: it is swapped for a text field of that size.
    for c in combos
        .into_iter()
        .filter(|c| c.accessible_label().as_deref() != Some("Hint for the model"))
    {
        let w = c.size().width;
        assert!(
            w <= 240.0 - 8.0 - 88.0 + 0.5,
            "{:?} is {w} wide",
            c.accessible_label()
        );
    }
}

/// DS-24.1 on the three rows that once shared a meaning with another page's icon: each keeps its
/// own glyph file, and no two Advanced rows draw the same file.
#[test]
fn the_advanced_rows_draw_distinct_glyph_files() {
    let slint = include_str!("../ui/pages/model.slint");
    let icons = include_str!("../ui/theme/icons.slint");
    let icon_of = |title: &str| {
        let at = slint
            .find(&format!("title: @tr(\"{title}\")"))
            .expect(title);
        let before = &slint[..at];
        let name = before.rsplit("icon: Icons.").next().unwrap();
        name.split(';').next().unwrap().to_owned()
    };
    for (title, want) in [
        ("Processor use", "threads"),
        ("Speed boost", "flash-attention"),
        ("Second try", "retry-doubtful"),
    ] {
        assert_eq!(icon_of(title), want, "{title}");
    }
    let file_of = |name: &str| {
        let line = icons
            .lines()
            .find(|l| l.contains(&format!("property <image> {name}:")))
            .unwrap_or_else(|| panic!("no icon {name}"));
        line.split("icons/")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
            .to_owned()
    };
    let advanced = &slint[slint.find("ModelState.tab == 2").unwrap()..];
    let mut seen = std::collections::HashSet::new();
    for name in advanced
        .split("icon: Icons.")
        .skip(1)
        .filter_map(|t| t.split(';').next())
    {
        let file = file_of(name);
        assert!(
            seen.insert(file.clone()),
            "{file} is on two Advanced controls"
        );
    }
}

/// A config saved while Russian was the default language.
const RUSSIAN: &str = "version = 2
[model]
language = \"ru\"
";

const NOT_ON_DISK: &str = "Dictation will not start until this is downloaded.";
const SLOW: &str =
    "Likely slow without a dedicated graphics card. Whisper large-v3 turbo is faster.";

/// UI rule 2: the selected model that is not on disk is shown once (its facts are on the tile)
/// and its warning is the row's "!" with the words as its tooltip and label, not a line under it.
#[test]
fn a_model_not_on_disk_is_named_once_and_warns_in_its_slot() {
    let window = open(&layout("once", None), Page::Model);
    window
        .window()
        .set_size(slint::LogicalSize::new(720.0, 520.0));
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    assert_eq!(
        copies(&window, "1.6 GB · Fast · 99 languages"),
        1,
        "the model's facts are on its tile and again on the card"
    );
    assert_eq!(copies(&window, NOT_ON_DISK), 1, "the warning is a line too");
    let slot = with_role(&window, NOT_ON_DISK, AccessibleRole::Text);
    assert!(
        slot.size().width < 24.0,
        "the warning is a {} px line, not the slot",
        slot.size().width
    );
    // Hold 24: the "!" sits beside the action it warns about, not floating mid-row.
    let download = with_role(
        &window,
        "Download Whisper large-v3 turbo",
        AccessibleRole::Button,
    );
    let gap = download.absolute_position().x - (slot.absolute_position().x + slot.size().width);
    assert!(
        (0.0..=8.5).contains(&gap),
        "the \"!\" is {gap} px from Download"
    );
}

/// DS-25, UI rule 1: the card of a heavy (slow) model in use keeps its height in every state at
/// the least window: the warnings are in the "!" slot, the status note on its one line.
#[test]
fn the_card_keeps_its_height_in_every_state() {
    let l = layout(
        "card-height",
        Some("version = 2\n[model]\nid = \"large-v3\"\n"),
    );
    let (ipc, _sent) = Ipc::fake();
    let window = open_linked(&l, Page::Model, ipc.clone());
    window
        .window()
        .set_size(slint::LogicalSize::new(720.0, 520.0));
    // Only an integrated GPU: the 3.1 GB model is likely slow.
    ipc.deliver(&Event::Push(Push::Devices {
        mics: vec![],
        gpus: vec![Device {
            name: "Vulkan0".into(),
            description: String::new(),
            uma: true,
        }],
    }));
    let entry = MODELS.iter().find(|m| m.id == "large-v3").unwrap();
    fs::create_dir_all(l.models_dir()).unwrap();
    let file = l.models_dir().join(entry.file_name);
    let card = |badge: &str| {
        window.global::<ModelState>().invoke_shown();
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        assert!(
            shows(&window, &format!("Whisper large-v3: {badge}")),
            "{badge}"
        );
        ElementHandle::find_by_element_type_name(&window, "ModelCard")
            .next()
            .expect("the card")
            .size()
            .height
    };
    let missing = card("Not downloaded");
    assert!(
        has(&window, &format!("{NOT_ON_DISK}\n{SLOW}")),
        "both warnings"
    );
    fs::write(&file, b"synthetic").unwrap();
    let damaged = card("Damaged");
    common::fake_model(&file, entry.size);
    let downloaded = card("Downloaded · checked before first use");
    oxi_core::models::write_stamp(&file).unwrap();
    let ready = card("Downloaded · runs on Vulkan0");
    assert!(has(&window, SLOW), "Ready lost the slow warning");
    for (state, height) in [
        ("Damaged", damaged),
        ("Downloaded", downloaded),
        ("Ready", ready),
    ] {
        assert!(
            (height - missing).abs() < 0.5,
            "{state}: {height} px, Not downloaded: {missing} px"
        );
    }
}

/// e11: a downloaded (unchecked) or damaged model's card says its status and what to do, not the
/// tile's facts again.
#[test]
fn a_downloaded_or_damaged_card_does_not_repeat_the_tile() {
    let l = layout("card-note", None);
    fs::create_dir_all(l.models_dir()).unwrap();
    let turbo = MODELS.iter().find(|m| m.id == "turbo").unwrap();
    common::fake_model(&l.models_dir().join(turbo.file_name), turbo.size);
    fs::write(l.models_dir().join("ggml-large-v3.bin"), b"synthetic").unwrap();
    let window = open(&l, Page::Model);
    for (name, badge, note) in [
        (
            "Whisper large-v3 turbo",
            "Downloaded",
            "checked before first use",
        ),
        ("Whisper large-v3", "Damaged", "download it again"),
    ] {
        view(&window, name);
        assert!(
            shows(&window, &format!("{name}: {badge} · {note}")),
            "{name}"
        );
        let lines: Vec<_> = i_slint_backend_testing::ElementQuery::from_root(&window)
            .find_all()
            .into_iter()
            .filter_map(|e| e.accessible_label())
            .filter(|t| t.contains(note))
            .collect();
        assert_eq!(lines.len(), 1, "{name}: {lines:?}");
        assert!(
            !lines[0].contains("99 languages"),
            "{name}: the tile's facts again"
        );
    }
}

/// e11: the chevron beside the language segments says what it opens, and an Auto value is a
/// shown value, not a greyed input.
#[test]
fn the_language_chevron_has_a_word_and_auto_is_not_a_disabled_box() {
    let window = open(&layout("e11-words", None), Page::Model);
    sub_tab(&window, "Languages");
    window
        .window()
        .set_size(slint::LogicalSize::new(720.0, 520.0));
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    assert!(
        button(&window, "More languages").size().width > 48.0,
        "a bare chevron"
    );
    sub_tab(&window, "Speed");
    assert!(has(&window, "4 now"), "Auto's value is a line of its own");
    assert!(
        !i_slint_backend_testing::ElementQuery::from_root(&window)
            .find_all()
            .iter()
            .any(|e| e.accessible_role() == Some(AccessibleRole::Spinbox)),
        "a greyed number box beside Auto"
    );
}

/// The page sources, for the checks of what the Slint text says (the glyphs are not in the
/// accessibility tree).
fn pages() -> Vec<(String, Vec<String>)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/pages");
    let mut all: Vec<_> = fs::read_dir(dir)
        .expect("ui/pages")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "slint"))
        .map(|p| {
            let text = fs::read_to_string(&p).expect("page source");
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                text.lines().map(str::to_owned).collect(),
            )
        })
        .collect();
    all.sort();
    all
}

/// DS-7.8 and DS-24.1: the labelled buttons of a meaning in the table carry its glyph on every
/// page: Delete the trash, Browse and Choose folder the folder dialog's, Open file the
/// open-in-another-app arrow. Flow words (Save, Cancel, Use) carry none.
#[test]
fn labelled_buttons_follow_the_icon_table() {
    let table = [
        ("text: @tr(\"Delete\")", Some("Icons.delete")),
        ("text: @tr(\"Browse\")", Some("Icons.browse-folder")),
        ("@tr(\"Choose folder\")", Some("Icons.browse-folder")),
        ("text: @tr(\"Open file\")", Some("Icons.open-external")),
        ("text: @tr(\"Save\")", None),
        ("text: @tr(\"Cancel\")", None),
        ("text: root.in-use ? @tr(\"In use\")", None),
    ];
    let mut seen = 0;
    for (page, lines) in pages() {
        for (n, line) in lines.iter().enumerate() {
            for (needle, glyph) in table {
                if !line.contains(needle) {
                    continue;
                }
                seen += 1;
                // The button's own properties: the lines around its text up to the next brace.
                let around = lines[n.saturating_sub(2)..(n + 4).min(lines.len())].join("\n");
                let icon = around.lines().find(|l| l.trim_start().starts_with("icon:"));
                match glyph {
                    Some(g) => assert!(
                        icon.is_some_and(|l| l.contains(g)),
                        "{page}:{}: {needle} wants {g}, has {icon:?}",
                        n + 1
                    ),
                    None => assert!(icon.is_none(), "{page}:{}: {needle} has {icon:?}", n + 1),
                }
            }
        }
    }
    assert!(
        seen >= 8,
        "only {seen} buttons matched: the table went stale"
    );
}

/// DS-7.9: a row never leads with a glyph of the table: no button in it can repeat it.
#[test]
fn rows_do_not_lead_with_an_action_glyph() {
    let action = [
        "delete",
        "folder-open",
        "copy",
        "download",
        "retry",
        "edit",
        "add",
        "browse-folder",
        "open-external",
        "scan",
        "revert",
        "close",
    ];
    for (page, lines) in pages() {
        for (n, pair) in lines.windows(2).enumerate() {
            if !pair[0].trim_end().ends_with("Row {") {
                continue;
            }
            let icon = pair[1].trim();
            for name in action {
                for suffix in ["", "-16"] {
                    let glyph = format!("icon: Icons.{name}{suffix};");
                    assert!(icon != glyph, "{page}:{}: a row leads with {glyph}", n + 2);
                }
            }
        }
    }
}

/// UI rule 35 (DS-11.1): Advanced says what Auto means in the "i" of its first group, not in a
/// line of its own.
#[test]
fn the_advanced_tab_has_no_loose_caption() {
    let slint = include_str!("../ui/pages/model.slint");
    let tab = &slint[slint.find("ModelState.tab == 2").expect("the Advanced tab")..];
    assert!(!tab.contains("Caption {"), "a loose Caption on Advanced");
    assert!(
        tab.contains("info: @tr(\"For experts. Auto means"),
        "no \"i\""
    );
}

/// The file each `Icons.` constant draws, from `theme/icons.slint`.
fn icon_file(name: &str) -> String {
    include_str!("../ui/theme/icons.slint")
        .lines()
        .find(|l| l.contains(&format!("property <image> {name}:")))
        .unwrap_or_else(|| panic!("no icon {name}"))
        .split("icons/")
        .nth(1)
        .and_then(|t| t.split('"').next())
        .expect("an icon file")
        .to_owned()
}

/// DS-24.1, DS-7.9: on Model > Settings no two rows lead with the same glyph file, and the Files
/// rows lead with no glyph of a model class or an action (the review found the brain of "most
/// accurate" on Models folder and the idle-unload timer on Partial downloads).
#[test]
fn settings_rows_lead_with_distinct_glyphs() {
    let slint = include_str!("../ui/pages/model.slint");
    // Models (with the Files rows), Languages and Speed (ia-map 3.2).
    let tab = &slint[slint.find("ModelState.tab == 0").expect("the Models tab")
        ..slint.find("ModelState.tab == 3").expect("the Accuracy tab")];
    let lines: Vec<&str> = tab.lines().collect();
    let leads: Vec<&str> = lines
        .windows(2)
        .filter(|p| p[0].trim_end().ends_with("Row {"))
        .filter_map(|p| p[1].trim().strip_prefix("icon: Icons.")?.strip_suffix(';'))
        .collect();
    assert!(leads.len() >= 5, "{leads:?}");
    let mut seen = std::collections::HashMap::new();
    for name in &leads {
        let file = icon_file(name);
        if let Some(other) = seen.insert(file.clone(), *name) {
            panic!("{other} and {name} both draw {file}");
        }
    }
    let taken: Vec<String> = [
        "model-accurate",
        "model-fast",
        "model-small",
        "model-own",
        "timer",
        "browse-folder",
        "browse-file",
        "folder-open",
        "delete",
        "download",
    ]
    .iter()
    .map(|n| icon_file(n))
    .collect();
    for name in ["models-folder", "partial-downloads"] {
        assert!(leads.contains(&name), "no row leads with {name}");
        assert!(!taken.contains(&icon_file(name)), "{name} reuses a glyph");
    }
}

// Slint's generated code trips the workspace `unused_qualifications` lint; it is not ours to fix.
#[allow(unused_qualifications)]
mod browse_probe {
    slint::slint! {
        import { Icons, PathField } from "../ui/theme/index.slint";
        export component BrowseProbe inherits Window {
            in property <bool> file;
            out property <image> glyph: f.browse-icon;
            out property <image> file-glyph: Icons.browse-file;
            out property <image> folder-glyph: Icons.browse-folder;
            width: 400px;
            height: 100px;
            f := PathField {
                path: "C:/models/a.bin";
                browse-text: "Browse";
                picks-file: root.file;
            }
        }
    }
}

/// DS-24: Browse for the own model picks a `.bin` file, so it shows `browse-file`; a folder
/// picker keeps `browse-folder`.
#[test]
fn a_file_picking_path_field_shows_the_file_glyph() {
    i_slint_backend_testing::init_no_event_loop();
    let probe = browse_probe::BrowseProbe::new().unwrap();
    assert_ne!(probe.get_file_glyph(), probe.get_folder_glyph());
    probe.set_file(true);
    assert!(
        probe.get_glyph() == probe.get_file_glyph(),
        "a file picker shows browse-folder"
    );
    probe.set_file(false);
    assert!(
        probe.get_glyph() == probe.get_folder_glyph(),
        "a folder picker lost browse-folder"
    );
    let slint = include_str!("../ui/pages/model.slint");
    let field = &slint[slint.find("PathField {").expect("the own model field")..];
    let field = &field[..field.find("browse(path)").expect("its Browse")];
    assert!(
        field.contains("picks-file: true;"),
        "the .bin Browse shows browse-folder"
    );
}

/// ia-map 3.5: the row "Add language" makes takes the caret on its Combo, once (its scroll
/// into view is `shots::shot_model_language_added`, which lays out in the real fonts).
#[test]
fn an_added_language_row_takes_the_caret() {
    let l = layout("lang-added", None);
    let window = open(&l, Page::Model);
    sub_tab(&window, "Languages");
    let s = window.global::<ModelState>();
    s.invoke_add_search("yoruba".into());
    assert_eq!(s.get_add_rows().row_count(), 1);
    s.invoke_add_chosen(0);
    assert_eq!(s.get_lang_new(), 0, "Yoruba is the only row");
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    with_role(&window, "Model for Yoruba", AccessibleRole::Combobox);
    assert_eq!(s.get_lang_new(), -1, "the new row took the caret once");
    // Yoruba's Combo lists its model, then "Download another model", which opens Models: Down on
    // the focused Combo picks it.
    assert_eq!(s.get_tab(), 1);
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::DownArrow.into(),
    });
    window.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::DownArrow.into(),
    });
    assert_eq!(s.get_tab(), 0, "the caret is on Yoruba's Combo");
}

/// ia-map 3.5, ia-owner: the first row shows the default model as text and a soft "Change"
/// that opens Models; it has no Delete.
#[test]
fn the_default_language_row_changes_the_model_on_models() {
    let l = layout("lang-change", None);
    let window = open(&l, Page::Model);
    sub_tab(&window, "Languages");
    let change = with_role(&window, "Change the default model", AccessibleRole::Button);
    assert!(!has(&window, "Remove All other languages and Auto"));
    change.invoke_accessible_default_action();
    assert_eq!(window.global::<ModelState>().get_tab(), 0);
}

/// Hold item 41, candidate 10: the Home daily counts suggest the memory settings on "Free memory
/// after" (ia-map 3.4); nothing changes until Apply, which sets both rows. "Not now" survives a
/// reopen. "Load the model at start" is its own switch.
#[test]
fn daily_counts_suggest_the_memory_settings_only_on_apply() {
    use oxi_core::stats::{Date, Day, Stats, Totals};
    i_slint_backend_testing::init_no_event_loop();
    let l = layout("memory", Some("version = 2\n"));
    let stats = Stats {
        days: (1..=10)
            .map(|d| Day {
                date: Date::from_ymd(2026, 11, d).unwrap(),
                n: Totals {
                    dictations: 40,
                    ..Totals::default()
                },
            })
            .collect(),
        ..Stats::default()
    };
    fs::create_dir_all(&l.data_dir).unwrap();
    fs::write(
        l.data_dir.join(oxi_core::stats::FILE),
        stats.to_json().unwrap(),
    )
    .unwrap();
    {
        let window = open_again(&l, Page::Model);
        sub_tab(&window, "Speed");
        let s = window.global::<ModelState>();
        assert_eq!(
            s.get_memory_tip().as_str(),
            "On your last 10 days of use you dictated about 40 times a day. Load the model at start and never free it?"
        );
        assert!(!saved(&l).preload_at_start, "nothing changes unasked");
        s.invoke_memory_suggestion(true);
        let m = saved(&l);
        assert!(m.preload_at_start && m.idle_unload_minutes == 0 && !m.unload_immediately);
        assert_eq!(s.get_memory_tip().as_str(), "", "applied: gone");
        assert!(s.get_load_at_start(), "the switch shows it");
        let picker = with_role(&window, "Free memory after", AccessibleRole::Combobox);
        assert_eq!(picker.accessible_value().as_deref(), Some("Never"));

        // The switch alone: off makes it a hand-set mix, so the suggestion is back; Not now.
        with_role(&window, "Load the model at start", AccessibleRole::Switch)
            .invoke_accessible_default_action();
        assert!(!saved(&l).preload_at_start);
        assert_ne!(s.get_memory_tip().as_str(), "");
        s.invoke_memory_suggestion(false);
        assert_eq!(s.get_memory_tip().as_str(), "");
    }
    let window = open_again(&l, Page::Model);
    sub_tab(&window, "Speed");
    let s = window.global::<ModelState>();
    assert_eq!(
        s.get_memory_tip().as_str(),
        "",
        "Not now came back after a reopen"
    );
    assert!(!saved(&l).preload_at_start);
}

/// The daemon lists one integrated GPU: this PC's memory judges the models (Windows only; no
/// hardware is read elsewhere).
fn list_an_integrated_gpu(ipc: &Ipc) {
    ipc.deliver(&Event::Push(Push::Devices {
        mics: vec![],
        gpus: vec![Device {
            name: "Vulkan0".into(),
            description: "Integrated".into(),
            uma: true,
        }],
    }));
}

/// Hold 24: the recommended tile wears its "Recommended" badge in every state of its model (not on
/// disk, on disk, in use), and no other tile has one.
#[test]
fn the_recommended_tile_keeps_its_badge_in_every_state() {
    if !cfg!(windows) {
        return;
    }
    let l = layout("badge-states", None);
    let (ipc, _sent) = Ipc::fake();
    let window = open_linked(&l, Page::Model, ipc.clone());
    list_an_integrated_gpu(&ipc);
    let state = window.global::<ModelState>();
    measured(&window);
    let badged = || -> Vec<String> {
        state
            .get_tiles()
            .iter()
            .filter(|t| t.badge == "Recommended")
            .map(|t| t.title.to_string())
            .collect()
    };
    // The Badge itself is drawn: an element labelled exactly "Recommended" (the InfoTip's longer
    // "...Recommended for this PC..." text must not count).
    let drawn = || {
        assert!(
            i_slint_backend_testing::ElementQuery::from_root(&window)
                .find_all()
                .into_iter()
                .any(|e| e.accessible_label().is_some_and(|l| l == "Recommended")),
            "the Recommended badge is not drawn"
        );
    };
    assert_eq!(badged(), ["Whisper turbo compact"], "not on disk");
    drawn();

    let entry = MODELS.iter().find(|m| m.id == "turbo-compact").unwrap();
    fs::create_dir_all(l.models_dir()).unwrap();
    common::fake_model(&l.models_dir().join(entry.file_name), entry.size);
    list_an_integrated_gpu(&ipc);
    assert_eq!(badged(), ["Whisper turbo compact"], "on disk");
    drawn();

    view(&window, "Whisper turbo compact");
    button(&window, "Use Whisper turbo compact").invoke_accessible_default_action();
    assert_eq!(saved(&l).id, "turbo-compact");
    list_an_integrated_gpu(&ipc);
    assert_eq!(badged(), ["Whisper turbo compact"], "in use");
    drawn();
}

/// ia-map 3.5 (RT R5): a model Combo item of a one-language model carries its language, a pick
/// that does not match the row's language shows the "!" with the reason, and the last item
/// downloads another model.
#[test]
fn a_language_row_names_single_language_models_and_warns_on_a_mismatch() {
    let l = layout(
        "lang-only",
        Some(
            "version = 2
[model]
id = \"podlodka\"
[model.by_language]
en = \"podlodka\"
ru = \"podlodka\"
",
        ),
    );
    let russian = MODELS.iter().find(|m| m.id == "podlodka").unwrap();
    fs::create_dir_all(l.models_dir()).unwrap();
    common::fake_model(&l.models_dir().join(russian.file_name), russian.size);
    let window = open(&l, Page::Model);
    let state = window.global::<ModelState>();
    state.invoke_shown();
    let rows: Vec<_> = state.get_lang_rows().iter().collect();
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["English", "Russian"]);
    let items: Vec<String> = rows[0].models.iter().map(|m| m.to_string()).collect();
    assert!(
        items.contains(&"Whisper large-v3 Russian · Russian only".to_owned()),
        "{items:?}"
    );
    assert_eq!(
        items.last().map(String::as_str),
        Some("Download another model…")
    );
    assert_eq!(
        rows[0].warning, "This model hears only Russian",
        "a Russian-only model on the English row"
    );
    assert_eq!(
        rows[1].warning, "",
        "the same model on the Russian row is fine"
    );
}

/// ia-map 3.5 (RT "model deleted while rows use it"): Delete's question names the languages that
/// run the model, and says nothing of the kind for a model no row uses.
#[test]
fn the_delete_question_names_the_languages_that_use_the_model() {
    let l = layout(
        "delete-names",
        Some(
            "version = 2
[model.by_language]
en = \"large-v3\"
ru = \"large-v3\"
",
        ),
    );
    let (ipc, _sent) = Ipc::fake();
    let window = open_linked(&l, Page::Model, ipc.clone());
    ipc.deliver(&Event::Link(Link::Online));
    for id in ["large-v3", "small"] {
        let entry = MODELS.iter().find(|m| m.id == id).unwrap();
        fs::create_dir_all(l.models_dir()).unwrap();
        common::fake_model(&l.models_dir().join(entry.file_name), entry.size);
    }
    window.global::<ModelState>().invoke_shown();
    view(&window, "Whisper large-v3");
    button(&window, "Delete Whisper large-v3").invoke_accessible_default_action();
    assert!(
        shows(&window, "Used for English, Russian."),
        "names the rows"
    );
    view(&window, "Whisper small");
    button(&window, "Delete Whisper small").invoke_accessible_default_action();
    assert!(!shows(&window, "Used for"), "no row uses Whisper small");
}

/// ia-map 3.5 (c03 R9): the Languages "i" says an app rule's own model beats the language rows.
#[test]
fn the_languages_tip_says_an_app_rules_model_overrides_the_rows() {
    let window = open(&layout("lang-tip", None), Page::Model);
    sub_tab(&window, "Languages");
    assert!(shows(
        &window,
        "An app rule with its own model uses that model"
    ));
}
