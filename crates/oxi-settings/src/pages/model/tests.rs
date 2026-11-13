#[test]
fn download_tooltip_follows_the_link_state() {
    let off = download_off;
    assert_eq!(off(Link::Online), "");
    for link in [Link::Offline, Link::Stuck] {
        assert_eq!(off(link), "Start OxiYapper to download.", "{link:?}");
    }
    assert_eq!(
        off(Link::Mismatch { daemon_proto: 9 }),
        "Restart OxiYapper first"
    );
    assert_eq!(off(Link::Busy), strings::DAEMON_BUSY);
}

use std::sync::mpsc::Receiver;
use std::time::Duration;

use i_slint_backend_testing::{AccessibleRole, ElementHandle, mock_elapsed_time};
use oxi_core::config::ConfigFile;
use oxi_core::machine::{EngineWarning, Phase};
use oxi_core::paths::Layout;
use oxi_core::strings::engine_warning;

use super::*;
use crate::generated::Toast;
use crate::ipc::{Cmd, Ipc};
use crate::testing::{has, linked, spots, with_role};

/// The settings window on a fresh config dir and a fake daemon link, on the Model page.
fn window(name: &str) -> (AppWindow, Ipc, Receiver<Cmd>, Layout) {
    let opened = linked("model", name, Page::Model);
    // Building the page runs its `init`, which asks the daemon.
    assert!(has(&opened.0, "Use my own .bin"));
    opened
}

/// Wait for the path worker: step the mocked clock (which runs the page's poll timer) until
/// `ready`.
fn wait_for(ready: impl Fn() -> bool) {
    for _ in 0..400 {
        mock_elapsed_time(FOLDER_POLL);
        if ready() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("the path check did not answer");
}

fn requests(sent: &Receiver<Cmd>) -> Vec<Request> {
    sent.try_iter()
        .filter_map(|c| match c {
            Cmd::Send(r) => Some(r),
            Cmd::Start | Cmd::Restart => None,
        })
        .collect()
}

fn push(ipc: &Ipc, push: Push) {
    ipc.deliver(&Event::Push(push));
}

fn progress(id: &str, done: u64, total: u64) -> Push {
    Push::DownloadProgress {
        id: id.into(),
        done,
        total,
    }
}

fn done(id: &str, result: DownloadResult) -> Push {
    Push::DownloadDone {
        id: id.into(),
        result,
    }
}

/// Picks the tile: its card shows below the gallery.
fn view(window: &AppWindow, name: &str) {
    radio(window, name).invoke_accessible_default_action();
}

fn click(window: &AppWindow, label: &str) {
    with_role(window, label, AccessibleRole::Button).invoke_accessible_default_action();
}

fn download_large(window: &AppWindow) {
    view(window, "Whisper large-v3");
    click(window, "Download Whisper large-v3");
}

/// The large-v3 progress bar's value, `None` while it has none.
fn bar(window: &AppWindow) -> Option<String> {
    ElementHandle::find_by_accessible_label(window, "Whisper large-v3 download")
        .find(|e| e.accessible_role() == Some(AccessibleRole::ProgressIndicator))
        .and_then(|e| e.accessible_value().map(String::from))
}

/// The large-v3 bar after a few frames of the driver's timer.
fn settled_bar(window: &AppWindow) -> f32 {
    thread::sleep(Duration::from_millis(300));
    mock_elapsed_time(Duration::from_millis(100));
    bar(window).expect("a bar").parse().expect("a number")
}

#[test]
fn download_sends_its_id_and_progress_moves_the_bar() {
    let (window, ipc, sent, _) = window("progress");
    let asked = requests(&sent);
    assert!(asked.contains(&Request::ListDevices) && asked.contains(&Request::Status));

    download_large(&window);
    let want = Request::DownloadModel {
        id: "large-v3".into(),
    };
    assert_eq!(requests(&sent), [want]);
    assert_eq!(bar(&window).as_deref(), Some("0"));

    // The bar shows the progress driver's value: it chases the bytes (a quarter, at most
    // a tenth ahead of it) and never goes back.
    push(&ipc, progress("large-v3", 1_000_000_000, 4_000_000_000));
    let raw = |w: &AppWindow| -> f32 { bar(w).expect("a bar").parse().expect("a number") };
    assert!(raw(&window) < 0.25, "the page drew the raw fraction");
    let first = settled_bar(&window);
    assert!((0.03..=0.36).contains(&first), "{first}");
    assert!(has(&window, "1.0 of 4.0 GB"), "progress beside the bar");
    push(&ipc, progress("large-v3", 3_000_000_000, 4_000_000_000));
    assert!(raw(&window) < 0.75, "the page drew the raw fraction");
    let second = settled_bar(&window);
    assert!(second > first && second <= 0.86, "{first} -> {second}");
    // Another model's progress leaves this bar alone.
    push(&ipc, progress("turbo", 1, 4));
    assert!(bar(&window).is_some());
}

#[test]
fn failed_download_shows_its_text() {
    let (window, ipc, _sent, _) = window("failed");
    view(&window, "Whisper large-v3");
    let message = "connection reset (os error 10054)";
    push(&ipc, progress("large-v3", 5, 10));
    assert!(bar(&window).is_some());
    let failed = DownloadResult::Failed {
        message: message.into(),
    };
    push(&ipc, done("large-v3", failed));
    // The text is the "!" of the card, so the card does not grow.
    let text = strings::download_failed(message);
    assert!(has(&window, &text));
    assert_eq!(bar(&window), None);
    assert!(has(&window, "Whisper large-v3: Not downloaded"));
    // Trying again clears the old error.
    download_large(&window);
    assert!(!has(&window, &text));
}

/// UI rule 1's check: whatever both snapshots have keeps its y and height. Only widths may
/// change: a badge or button with another text is wider or narrower.
fn assert_rows_kept(
    before: Vec<(String, Option<AccessibleRole>, [f32; 4])>,
) -> impl Fn(&AppWindow) {
    move |window| {
        let after = spots(window);
        let mut compared = 0;
        for (label, role, [_, y, _, h]) in &before {
            let same: Vec<_> = after
                .iter()
                .filter(|a| &a.0 == label && &a.1 == role)
                .map(|a| (a.2[1], a.2[3]))
                .collect();
            if same.is_empty() {
                continue;
            }
            compared += 1;
            assert!(
                same.contains(&(*y, *h)),
                "{label} moved: y, h {y}, {h} is now {same:?}"
            );
        }
        assert!(compared > 3, "only {compared} elements compared");
    }
}

/// UI rule 1: a card keeps its height and the cards after it keep their place in every state.
#[test]
fn a_card_keeps_its_place_in_every_state() {
    let (window, ipc, _sent, layout) = window("states");
    // In use and not on disk: the "!" of the card warns, the card keeps its height.
    window.global::<ModelState>().invoke_picked(1);
    assert!(has(&window, NOT_ON_DISK));
    let kept = assert_rows_kept(spots(&window));
    push(&ipc, progress("large-v3", 5, 10));
    assert!(bar(&window).is_some());
    kept(&window);
    let failed = DownloadResult::Failed {
        message: "connection reset".into(),
    };
    push(&ipc, done("large-v3", failed));
    kept(&window);
    fs::create_dir_all(layout.models_dir()).unwrap();
    fs::write(layout.models_dir().join("ggml-large-v3.bin.part"), b"half").unwrap();
    push(&ipc, done("large-v3", DownloadResult::Cancelled));
    assert!(has(&window, "Whisper large-v3: Paused"));
    kept(&window);
}

fn radio(window: &AppWindow, label: &str) -> ElementHandle {
    with_role(window, label, AccessibleRole::RadioButton)
}

const NOT_ON_DISK: &str = "Dictation will not start until this is downloaded.";
const SLOW: &str =
    "Likely slow without a dedicated graphics card. Whisper large-v3 turbo is faster.";

fn gpus(ipc: &Ipc, gpus: &[(&str, bool)]) {
    let gpus = gpus
        .iter()
        .map(|(name, uma)| Device {
            name: (*name).into(),
            description: String::new(),
            uma: *uma,
        })
        .collect();
    push(
        ipc,
        Push::Devices {
            mics: Vec::new(),
            gpus,
        },
    );
}

#[test]
fn heavy_models_warn_without_a_dedicated_gpu_and_still_download() {
    let (window, ipc, sent, _) = window("heavy");
    // No answer from the daemon yet: no warning.
    assert!(!has(&window, SLOW));
    gpus(&ipc, &[("Vulkan0", true)]);
    // Both 3.1 GB models warn, turbo does not.
    assert!(!has(&window, SLOW));
    view(&window, "Whisper large-v3 Russian");
    assert!(has(&window, SLOW));
    let _ = requests(&sent);
    download_large(&window);
    assert!(has(&window, SLOW));
    let want = Request::DownloadModel {
        id: "large-v3".into(),
    };
    assert_eq!(requests(&sent), [want], "the warning blocked the download");
    // The model in use, not on disk, says both.
    window.global::<ModelState>().invoke_picked(2);
    assert!(has(&window, &format!("{NOT_ON_DISK}\n{SLOW}")));
    gpus(&ipc, &[("Vulkan0", false), ("Vulkan1", true)]);
    assert!(!has(&window, SLOW) && !has(&window, &format!("{NOT_ON_DISK}\n{SLOW}")));
    // The CPU picked: slow again.
    window.global::<ModelState>().invoke_gpu_picked(1);
    assert!(has(&window, &format!("{NOT_ON_DISK}\n{SLOW}")));
}

#[test]
fn partial_downloads_are_listed_and_deleted() {
    let (window, ipc, _sent, layout) = window("partials");
    let s = window.global::<ModelState>();
    assert_eq!(s.get_partials_off(), PARTIALS_NONE);
    let dir = layout.models_dir();
    fs::create_dir_all(&dir).unwrap();
    let part = dir.join("ggml-large-v3.bin.part");
    let other = dir.join("old-model.bin.part");
    fs::write(&part, vec![0; 1000]).unwrap();
    fs::write(&other, b"x").unwrap();
    s.invoke_shown();
    view(&window, "Whisper large-v3");
    assert_eq!(s.get_partials_off(), "");
    assert_eq!(s.get_partials(), "0 MB in 2 files");
    assert_eq!(size(3_000_000), "3 MB");
    assert_eq!(size(1_600_000_000), "1.6 GB");
    assert!(has(&window, "1 MB of 3.1 GB · kept 7 more days"));
    // A running download writes its `.part`: the button waits.
    push(&ipc, progress("turbo", 5, 10));
    assert_eq!(s.get_partials_off(), PARTIALS_BUSY);
    s.invoke_delete_partials();
    assert!(part.exists(), "deleted during a download");
    push(&ipc, done("turbo", DownloadResult::Cancelled));
    assert_eq!(s.get_partials_off(), "");
    s.invoke_delete_partials();
    assert!(!part.exists() && !other.exists());
    assert_eq!(s.get_partials_off(), PARTIALS_NONE);
    assert!(has(&window, "Whisper large-v3: Not downloaded"));
    assert_eq!(
        window.global::<Toast>().get_text(),
        "Deleted 0 MB of partial downloads"
    );
}

#[test]
fn kept_days_read_as_words() {
    assert_eq!(kept(7), "kept 7 more days");
    assert_eq!(kept(1), "kept 1 more day");
    assert_eq!(kept(0), "removed at the next start");
}

#[test]
fn download_button_is_not_inside_the_radio() {
    let (window, _ipc, _sent, _) = window("nodes");
    view(&window, "Whisper large-v3");
    let radio = with_role(&window, "Whisper large-v3", AccessibleRole::RadioButton);
    with_role(&window, "Download Whisper large-v3", AccessibleRole::Button);
    let inside = radio
        .query_descendants()
        .match_accessible_role(AccessibleRole::Button)
        .find_all();
    assert!(inside.is_empty(), "the Download button is inside the radio");
}

#[test]
fn cancelled_download_with_a_part_file_shows_paused() {
    let (window, ipc, _sent, layout) = window("paused");
    view(&window, "Whisper large-v3");
    push(&ipc, progress("large-v3", 5, 10));
    fs::create_dir_all(layout.models_dir()).unwrap();
    fs::write(layout.models_dir().join("ggml-large-v3.bin.part"), b"half").unwrap();
    push(&ipc, done("large-v3", DownloadResult::Cancelled));
    // How much is there and how long it stays.
    assert!(has(&window, "1 MB of 3.1 GB · kept 7 more days"));
    with_role(&window, "Resume Whisper large-v3", AccessibleRole::Button);
    // A model with no `.part` stays "Not downloaded".
    view(&window, "Whisper large-v3 turbo");
    assert!(has(&window, "Whisper large-v3 turbo: Not downloaded"));
}

#[test]
fn relative_custom_path_is_refused_and_not_saved() {
    let (window, _ipc, _sent, layout) = window("relative");
    view(&window, "Use my own .bin");
    let field = with_role(&window, "Model file", AccessibleRole::TextInput);
    field.set_accessible_value("mine.bin".to_owned());
    assert!(has(&window, RELATIVE));
    assert!(!layout.config_file().exists(), "a relative path was saved");
}

#[test]
fn an_answer_for_an_older_edit_is_dropped() {
    let (window, _ipc, _sent, layout) = window("stale");
    view(&window, "Use my own .bin");
    let good = layout.config_dir.join("good.bin");
    fs::write(&good, b"synthetic").unwrap();
    let field = with_role(&window, "Model file", AccessibleRole::TextInput);
    field.set_accessible_value(good.to_string_lossy().into_owned());
    field.set_accessible_value("notes.txt".to_owned());
    for _ in 0..20 {
        mock_elapsed_time(FOLDER_POLL);
        thread::sleep(Duration::from_millis(10));
    }
    assert!(has(&window, "The model file must end in .bin"));
    assert!(!layout.config_file().exists(), "the old answer was saved");
}

fn saved_model(layout: &Layout) -> oxi_core::config::Model {
    let text = fs::read_to_string(layout.config_file()).unwrap();
    ConfigFile::parse(&text).unwrap().config.model
}

/// Review d04 #9, arch-plan 9.3: the Model page edits `model.by_language`; a language with
/// its own model moves to the top of the list, Default takes it out.
#[test]
fn by_language_row_gives_a_language_its_model_and_default_takes_it_away() {
    let (window, _ipc, _sent, layout) = window("bylang");
    let s = window.global::<ModelState>();
    let names = || -> Vec<SharedString> { s.get_by_language_names().iter().collect() };
    let offered = || -> Vec<SharedString> { s.get_by_language_models().iter().collect() };
    // d04-fix 1: nothing downloaded and no own file: Default alone is offered.
    assert_eq!(offered(), ["Default"]);
    let entry = models::find("turbo").unwrap();
    on_disk(&layout, entry.file_name, entry.size);
    s.invoke_shown();
    assert_eq!(
        offered(),
        ["Default", entry.display_name],
        "only what is on disk"
    );
    let turbo = 1;
    assert_eq!(names().len(), languages::LANGUAGES.len());
    let ru = names().iter().position(|n| n == "Russian").unwrap();
    s.invoke_by_language_shown(int(ru));
    assert_eq!(s.get_by_language_model(), 0, "no model of its own yet");
    s.invoke_by_language_picked(int(ru), int(turbo));
    let saved = saved_model(&layout).by_language;
    assert_eq!(
        saved.get(&LanguageKey(Language::RU)).map(String::as_str),
        Some("turbo")
    );
    assert_eq!(names()[0], "Russian", "first, still shown with its model");
    assert_eq!(
        (s.get_by_language(), s.get_by_language_model()),
        (0, int(turbo))
    );
    s.invoke_by_language_picked(0, 0);
    assert!(saved_model(&layout).by_language.is_empty());
    assert_eq!(names().len(), languages::LANGUAGES.len());
}

#[test]
fn unload_picker_saves_right_away_minutes_and_never() {
    let (window, _ipc, _sent, layout) = window("unload");
    let s = window.global::<ModelState>();
    let labels: Vec<SharedString> = s.get_unload_options().iter().collect();
    let want = [
        "Right after dictation",
        "After 1 min",
        "After 5 min",
        "After 10 min",
        "After 15 min",
        "After 30 min",
        "After 60 min",
        "Never",
    ];
    assert_eq!(labels, want);
    assert_eq!(s.get_unload(), 2, "the default 5 min");
    s.invoke_unload_picked(0);
    let saved = saved_model(&layout);
    assert!(saved.unload_immediately);
    assert_eq!(saved.idle_unload_minutes, 5, "the minutes are kept");
    s.invoke_unload_picked(5);
    let saved = saved_model(&layout);
    assert_eq!(
        (saved.unload_immediately, saved.idle_unload_minutes),
        (false, 30)
    );
    s.invoke_unload_picked(7);
    let saved = saved_model(&layout);
    assert_eq!(
        (saved.unload_immediately, saved.idle_unload_minutes),
        (false, 0)
    );
}

#[test]
fn unload_rows_keep_an_older_value() {
    let (labels, values) = unload_rows(Some(12));
    assert_eq!(labels[4], "After 12 min");
    assert_eq!(values[3..6], [Some(10), Some(12), Some(15)]);
    let (labels, _) = unload_rows(Some(90));
    assert_eq!(labels[7..], ["After 90 min", "Never"]);
    for current in [None, Some(0), Some(5)] {
        assert_eq!(unload_rows(current).1, UNLOAD, "{current:?}");
    }
}

#[test]
fn cancel_waits_for_the_daemon_to_stop() {
    let (window, ipc, sent, _) = window("cancel");
    view(&window, "Whisper large-v3");
    push(&ipc, progress("large-v3", 5, 10));
    let _ = requests(&sent);
    with_role(&window, "Cancel Whisper large-v3", AccessibleRole::Button)
        .invoke_accessible_default_action();
    let want = Request::CancelDownload {
        id: "large-v3".into(),
    };
    assert_eq!(requests(&sent), [want]);
    assert!(
        bar(&window).is_some(),
        "the bar went before the daemon stopped"
    );
    push(&ipc, done("large-v3", DownloadResult::Cancelled));
    assert_eq!(bar(&window), None);
    assert!(has(&window, "Download Whisper large-v3"));
}

#[test]
fn lost_daemon_ends_the_downloads() {
    let (window, ipc, _sent, _) = window("lost");
    view(&window, "Whisper large-v3");
    push(&ipc, progress("large-v3", 5, 10));
    ipc.deliver(&Event::Link(Link::Offline));
    assert_eq!(bar(&window), None);
    assert!(has(&window, "Whisper large-v3: Not downloaded"));
}

#[test]
fn daemon_gpus_are_listed_by_name_and_picked() {
    let (window, ipc, _sent, layout) = window("gpus");
    let gpu = |name: &str, description: &str, uma| Device {
        name: name.into(),
        description: description.into(),
        uma,
    };
    let gpus = vec![
        gpu("Vulkan0", "NVIDIA GeForce RTX 4070 SUPER", false),
        gpu("Vulkan1", "AMD Radeon(TM) Graphics", true),
    ];
    push(
        &ipc,
        Push::Devices {
            mics: Vec::new(),
            gpus,
        },
    );
    let s = window.global::<ModelState>();
    let rows: Vec<SharedString> = s.get_gpus().iter().collect();
    // The marketing names (UI rule 30); the ggml name stays the config key.
    let want = [
        "Auto",
        "CPU",
        "NVIDIA GeForce RTX 4070 SUPER",
        "AMD Radeon(TM) Graphics",
    ];
    assert_eq!(rows, want);
    s.invoke_gpu_picked(3);
    let text = fs::read_to_string(layout.config_file()).unwrap();
    let saved = ConfigFile::parse(&text).unwrap().config.model.gpu;
    assert_eq!(saved, DeviceChoice::Named("Vulkan1".into()));
}

#[test]
fn status_warning_shows_the_cpu_note() {
    let (window, ipc, _sent, _) = window("warning");
    let note = engine_warning(&EngineWarning::CpuFallback {
        reason: "out of device memory".into(),
    });
    assert!(!has(&window, &note));
    let status = Push::Status {
        phase: Phase::Idle,
        model: "turbo".into(),
        device: DeviceChoice::Auto,
        backend: "cpu".into(),
        loaded: true,
        warnings: vec![note.clone(), "sounds.start: empty, using rise".into()],
    };
    push(&ipc, status);
    // On the Settings sub-tab, beside the GPU; a hidden sub-tab shows nothing.
    assert!(!has(&window, &note));
    with_role(&window, "Settings", AccessibleRole::Tab).invoke_accessible_default_action();
    // The note alone (an exact label): the sound line is the Audio page's.
    assert!(has(&window, &note));
}

#[test]
fn gpu_rows_keep_a_missing_choice() {
    type Case<'a> = (Option<&'a [Device]>, DeviceChoice, &'a [&'a str]);
    let listed = [Device {
        name: "Vulkan0".into(),
        description: String::new(),
        uma: false,
    }];
    let named = |n: &str| DeviceChoice::Named(n.into());
    let table: [Case<'_>; 5] = [
        (None, DeviceChoice::Auto, &["Auto", "CPU"]),
        (None, named("Vulkan1"), &["Auto", "CPU", "Vulkan1"]),
        (
            Some(&listed),
            DeviceChoice::Cpu,
            &["Auto", "CPU", "Vulkan0"],
        ),
        (Some(&listed), named("Vulkan0"), &["Auto", "CPU", "Vulkan0"]),
        (
            Some(&listed),
            named("Vulkan1"),
            &["Auto", "CPU", "Vulkan0", "Not connected: Vulkan1"],
        ),
    ];
    for (listed, current, want) in table {
        let (labels, choices) = gpu_rows(listed, &current);
        assert_eq!(labels, want, "{current:?}");
        assert!(choices.contains(&current), "{current:?}");
    }
}

const RELATIVE: &str = strings::MODEL_PATH_RELATIVE;

#[test]
fn custom_path_problems() {
    let dir = crate::testing::scratch_dir(&format!(
        "oxi-settings-lib-model-{}-custom",
        std::process::id()
    ));
    fs::create_dir_all(dir.join("folder.bin")).unwrap();
    let path = |name: &str| dir.join(name).to_string_lossy().into_owned();
    fs::write(dir.join("mine.BIN"), b"not a model").unwrap();
    let (file, folder, missing) = (path("mine.BIN"), path("folder.bin"), path("gone.bin"));
    for (path, want) in [
        ("", Some("Choose a .bin model file")),
        (file.as_str(), None),
        (missing.as_str(), Some("File not found")),
        (folder.as_str(), Some("This is a folder, not a model file")),
        ("notes.txt", Some("The model file must end in .bin")),
        ("mine.bin", Some(RELATIVE)),
        ("models/mine.bin", Some(RELATIVE)),
    ] {
        assert_eq!(custom_problem(path), want, "{path:?}");
    }
    // The text alone decides these: no disk read on the UI thread.
    assert_eq!(shape_problem(&missing), None);
    assert_eq!(shape_problem("mine.bin"), Some(RELATIVE));
}

#[test]
fn custom_path_answers_come_from_the_worker() {
    let (window, _ipc, _sent, layout) = window("worker");
    view(&window, "Use my own .bin");
    let field = with_role(&window, "Model file", AccessibleRole::TextInput);
    let missing = layout.config_dir.join("gone.bin");
    field.set_accessible_value(missing.to_string_lossy().into_owned());
    wait_for(|| has(&window, "File not found"));
    assert!(!layout.config_file().exists());

    let model = layout.config_dir.join("mine.bin");
    fs::write(&model, b"synthetic").unwrap();
    field.set_accessible_value(format!("\"{}\"", model.display()));
    wait_for(|| layout.config_file().exists());
    let text = fs::read_to_string(layout.config_file()).unwrap();
    let saved = ConfigFile::parse(&text).unwrap().config.model;
    assert_eq!(saved.id, "custom");
    assert_eq!(saved.custom_path, model.to_string_lossy());
    assert!(!has(&window, "File not found"));
    with_role(&window, "Own model file in use", AccessibleRole::Button);
    let tiles = window.global::<ModelState>().get_tiles();
    assert_eq!(tiles.row_data(3).unwrap().hint, IN_USE);
}

/// A model file of the registry's size: "ready" without a hash (the stamp is not there).
fn on_disk(layout: &Layout, file: &str, bytes: u64) -> std::path::PathBuf {
    fs::create_dir_all(layout.models_dir()).unwrap();
    let path = layout.models_dir().join(file);
    crate::testing::fake_model(&path, bytes);
    path
}

fn large() -> &'static ModelEntry {
    MODELS.iter().find(|m| m.id == "large-v3").unwrap()
}

fn deleted(id: &str, result: DeleteResult) -> Push {
    Push::ModelDeleted {
        id: id.into(),
        result,
    }
}

/// Ready: Use and Delete. Delete asks first and names what it frees; Keep sends nothing, the
/// second answer sends the request, the daemon's answer says what was freed.
#[test]
fn delete_asks_first_then_frees_the_space() {
    let (window, ipc, sent, layout) = window("delete");
    let file = on_disk(&layout, large().file_name, large().size);
    window.global::<ModelState>().invoke_shown();
    view(&window, "Whisper large-v3");
    with_role(&window, "Use Whisper large-v3", AccessibleRole::Button);
    let _ = requests(&sent);
    let question = "Delete Whisper large-v3? Frees 3.1 GB.";
    click(&window, "Delete Whisper large-v3");
    assert!(has(&window, question));
    click(&window, "Keep Whisper large-v3");
    assert!(!has(&window, question));
    assert_eq!(requests(&sent), []);
    assert!(file.exists());

    click(&window, "Delete Whisper large-v3");
    click(&window, "Confirm: delete Whisper large-v3");
    let want = Request::DeleteModel {
        id: "large-v3".into(),
    };
    assert_eq!(requests(&sent), [want]);
    fs::remove_file(&file).unwrap();
    push(
        &ipc,
        deleted(
            "large-v3",
            DeleteResult::Done {
                freed: large().size,
            },
        ),
    );
    assert_eq!(
        window.global::<Toast>().get_text(),
        "Deleted Whisper large-v3, freed 3.1 GB"
    );
    assert!(has(&window, "Whisper large-v3: Not downloaded"));
    assert!(!has(&window, question));
    with_role(&window, "Download Whisper large-v3", AccessibleRole::Button);
}

/// Return on the focused button, then the `changed` handlers that move the focus.
fn enter(window: &AppWindow) {
    let text = SharedString::from(slint::platform::Key::Return);
    let w = window.window();
    w.dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.clone() });
    w.dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
    slint::platform::update_timers_and_animations();
}

/// The question's buttons leave with it; the focus stays on the card, not the window top:
/// Keep gives it to Delete, a confirmed Delete to the Download that takes the files' place.
#[test]
fn the_focus_stays_on_the_card_after_the_question() {
    let (window, ipc, sent, layout) = window("delete-focus");
    let file = on_disk(&layout, large().file_name, large().size);
    window.global::<ModelState>().invoke_shown();
    view(&window, "Whisper large-v3");
    // The card's `changed index` closes a question: let it run now, as a frame would.
    slint::platform::update_timers_and_animations();
    let question = "Delete Whisper large-v3? Frees 3.1 GB.";
    click(&window, "Delete Whisper large-v3");
    assert!(has(&window, question));
    enter(&window); // on Keep, which the question focused
    assert!(!has(&window, question));
    enter(&window);
    assert!(has(&window, question), "Keep left the focus nowhere");

    click(&window, "Confirm: delete Whisper large-v3");
    fs::remove_file(&file).unwrap();
    let _ = requests(&sent);
    let freed = DeleteResult::Done {
        freed: large().size,
    };
    push(&ipc, deleted("large-v3", freed));
    assert!(has(&window, "Whisper large-v3: Not downloaded"));
    slint::platform::update_timers_and_animations();
    enter(&window);
    let want = Request::DownloadModel {
        id: "large-v3".into(),
    };
    assert_eq!(requests(&sent), [want], "Delete left the focus nowhere");
}

/// A delete that fails ends the card's wait for the files to go: when they vanish later (Explorer,
/// the folder poll), the Download that appears does not take the focus from where the user is.
#[test]
fn a_failed_delete_does_not_steal_the_focus_later() {
    let (window, ipc, sent, layout) = window("delete-failed-focus");
    let file = on_disk(&layout, large().file_name, large().size);
    window.global::<ModelState>().invoke_shown();
    view(&window, "Whisper large-v3");
    slint::platform::update_timers_and_animations();
    click(&window, "Delete Whisper large-v3");
    click(&window, "Confirm: delete Whisper large-v3");
    let failed = DeleteResult::Failed {
        message: "Access is denied. (os error 5)".into(),
    };
    push(&ipc, deleted("large-v3", failed));
    slint::platform::update_timers_and_animations(); // the focus returns to Delete
    // The user moves on, to Use.
    let w = window.window();
    let back = SharedString::from(slint::platform::Key::Backtab);
    w.dispatch_event(slint::platform::WindowEvent::KeyPressed { text: back.clone() });
    w.dispatch_event(slint::platform::WindowEvent::KeyReleased { text: back });

    fs::remove_file(&file).unwrap();
    // The daemon lists the devices again: the rows are read from the files.
    push(
        &ipc,
        Push::Devices {
            mics: Vec::new(),
            gpus: Vec::new(),
        },
    );
    assert!(has(&window, "Whisper large-v3: Not downloaded"));
    slint::platform::update_timers_and_animations();
    let _ = requests(&sent);
    enter(&window);
    assert_eq!(requests(&sent), [], "the focus was taken to Download");
}

/// A Delete refused at once (the daemon went away while the question was open) ends the wait
/// too, also when the refusal repeats the card's last "!" word for word.
#[test]
fn a_refused_delete_does_not_steal_the_focus_later() {
    let (window, ipc, sent, layout) = window("delete-refused-focus");
    let file = on_disk(&layout, large().file_name, large().size);
    window.global::<ModelState>().invoke_shown();
    view(&window, "Whisper large-v3");
    slint::platform::update_timers_and_animations();
    for _ in 0..2 {
        ipc.deliver(&Event::Link(Link::Online));
        click(&window, "Delete Whisper large-v3");
        ipc.deliver(&Event::Link(Link::Offline));
        click(&window, "Confirm: delete Whisper large-v3");
        slint::platform::update_timers_and_animations();
    }
    ipc.deliver(&Event::Link(Link::Online));
    fs::remove_file(&file).unwrap();
    push(
        &ipc,
        Push::Devices {
            mics: Vec::new(),
            gpus: Vec::new(),
        },
    );
    assert!(has(&window, "Whisper large-v3: Not downloaded"));
    slint::platform::update_timers_and_animations();
    let _ = requests(&sent);
    enter(&window);
    assert_eq!(requests(&sent), [], "the focus was taken to Download");
}

/// Draws the card's states the other shots miss: paused (Resume, Delete), ready and not in use
/// (Use, Delete), and the delete question; each frame non-empty and not one colour. With
/// `OXI_SHOTS_DIR` set they land in `settings-model-card-<state>-<w>-<theme>.png`. The renderer
/// platform is per thread and the other tests here use the testing backend: a thread of its own.
#[test]
fn shots_model_card() {
    thread::spawn(draw_cards).join().unwrap();
}

fn draw_cards() {
    use slint::platform::software_renderer::{
        MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
    };
    use slint::platform::{Platform, WindowAdapter};

    thread_local! {
        static LAST: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
        // The animation clock, stepped by hand past the theme fades.
        static CLOCK: Cell<Duration> = const { Cell::new(Duration::ZERO) };
    }
    struct Shots;
    impl Platform for Shots {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
            LAST.with_borrow_mut(|last| *last = Some(window.clone()));
            Ok(window)
        }
        fn duration_since_start(&self) -> Duration {
            CLOCK.get()
        }
    }

    slint::platform::set_platform(Box::new(Shots)).unwrap();
    let dir = std::env::var_os("OXI_SHOTS_DIR").map(std::path::PathBuf::from);
    for state in ["paused", "ready", "question"] {
        let layout = crate::testing::layout("model", &format!("shot-{state}"));
        let window = crate::open_with(layout.clone(), Ipc::fake().0).unwrap();
        let surface = LAST.with_borrow(Clone::clone).unwrap();
        window.set_page(Page::Model);
        if state == "paused" {
            on_disk(
                &layout,
                &format!("{}{PART_SUFFIX}", large().file_name),
                1_000_000,
            );
        } else {
            on_disk(&layout, large().file_name, large().size);
        }
        surface.set_size(slint::PhysicalSize::new(1280, 860));
        window.show().unwrap();
        window.global::<ModelState>().invoke_shown();
        view(&window, "Whisper large-v3");
        slint::platform::update_timers_and_animations();
        if state == "question" {
            click(&window, "Delete Whisper large-v3");
        }
        for (w, h) in [(1280_u32, 860_u32), (720, 520)] {
            surface.set_size(slint::PhysicalSize::new(w, h));
            for (theme, choice) in [
                ("light", crate::ThemeChoice::Light),
                ("dark", crate::ThemeChoice::Dark),
            ] {
                window.set_theme(choice);
                let mut buffer = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
                // A fade starts when a frame reads the new colour: draw one, then step past it.
                for step in [Duration::ZERO, Duration::from_millis(300)] {
                    CLOCK.set(CLOCK.get() + step);
                    slint::platform::update_timers_and_animations();
                    surface.request_redraw();
                    assert!(surface.draw_if_needed(|r| {
                        r.render(&mut buffer, w as usize);
                    }));
                }
                let bytes = buffer
                    .iter()
                    .flat_map(|p| [p.red, p.green, p.blue, p.alpha])
                    .collect();
                let size = tiny_skia::IntSize::from_wh(w, h).unwrap();
                let pixmap = tiny_skia::Pixmap::from_vec(bytes, size).unwrap();
                let pixels = pixmap.pixels();
                let first = pixels[0];
                assert!(
                    pixels.iter().any(|p| p.alpha() > 0),
                    "{state} {theme} drew nothing"
                );
                assert!(
                    pixels.iter().any(|p| *p != first),
                    "{state} {theme} is flat"
                );
                if let Some(dir) = &dir {
                    let name = format!("settings-model-card-{state}-{w}-{theme}.png");
                    fs::create_dir_all(dir).unwrap();
                    pixmap.save_png(dir.join(name)).unwrap();
                }
            }
        }
    }
}

/// The daemon's refusals stay on the card, as its "!".
#[test]
fn a_refused_delete_says_why() {
    let (window, ipc, _sent, layout) = window("delete-refused");
    on_disk(&layout, large().file_name, large().size);
    window.global::<ModelState>().invoke_shown();
    view(&window, "Whisper large-v3");
    push(&ipc, deleted("large-v3", DeleteResult::InUse));
    assert!(has(&window, DELETE_IN_USE));
    let failed = DeleteResult::Failed {
        message: "Access is denied. (os error 5)".into(),
    };
    push(&ipc, deleted("large-v3", failed));
    assert!(has(&window, "Not deleted: Access is denied. (os error 5)"));
    assert!(has(&window, "Whisper large-v3: Downloaded"));
}

/// In use: no Use, and Delete is off with the reason; another model in use frees it.
#[test]
fn the_model_in_use_is_not_deleted() {
    let (window, _ipc, sent, layout) = window("delete-in-use");
    on_disk(&layout, large().file_name, large().size);
    let s = window.global::<ModelState>();
    s.invoke_shown();
    view(&window, "Whisper large-v3");
    click(&window, "Use Whisper large-v3");
    assert_eq!(saved_model(&layout).id, "large-v3");
    let used = with_role(&window, "Whisper large-v3 in use", AccessibleRole::Button);
    assert_eq!(used.accessible_enabled(), Some(false));
    let delete = with_role(&window, "Delete Whisper large-v3", AccessibleRole::Button);
    assert_eq!(delete.accessible_enabled(), Some(false));
    assert_eq!(
        delete.accessible_description().as_deref(),
        Some("In use. Use another model first to delete this one.")
    );
    // The callback refuses it too.
    let _ = requests(&sent);
    assert!(!s.invoke_delete(1));
    assert_eq!(requests(&sent), []);
    let tiles = s.get_tiles();
    assert_eq!(tiles.row_data(1).unwrap().hint, IN_USE);
    let other = tiles.row_data(0).unwrap();
    assert_eq!(
        (other.hint.as_str(), other.mark),
        ("Not downloaded", MARK_GET)
    );
    assert_ne!(other.caption, other.hint, "the state is not said twice");
}

/// Paused: Resume and Delete; the question names the partial download and its size.
#[test]
fn a_paused_download_can_be_deleted() {
    let (window, _ipc, sent, layout) = window("delete-paused");
    on_disk(
        &layout,
        &format!("{}{PART_SUFFIX}", large().file_name),
        400_000_000,
    );
    window.global::<ModelState>().invoke_shown();
    view(&window, "Whisper large-v3");
    with_role(&window, "Resume Whisper large-v3", AccessibleRole::Button);
    let _ = requests(&sent);
    click(&window, "Delete Whisper large-v3");
    assert!(has(
        &window,
        "Delete the partial download of Whisper large-v3? Frees 400 MB."
    ));
    click(&window, "Confirm: delete Whisper large-v3");
    let want = Request::DeleteModel {
        id: "large-v3".into(),
    };
    assert_eq!(requests(&sent), [want]);
}

/// Not downloaded and downloading offer no Delete; without the daemon Delete is off.
#[test]
fn delete_is_offered_only_for_files_and_needs_the_daemon() {
    let (window, ipc, _sent, layout) = window("delete-offer");
    view(&window, "Whisper large-v3");
    let shown = |window: &AppWindow| has(window, "Delete Whisper large-v3");
    assert!(!shown(&window), "nothing on disk");
    push(&ipc, progress("large-v3", 5, 10));
    assert!(!shown(&window), "downloading");
    on_disk(&layout, large().file_name, large().size);
    push(&ipc, done("large-v3", DownloadResult::Ok));
    assert!(shown(&window));
    ipc.deliver(&Event::Link(Link::Offline));
    let delete = with_role(&window, "Delete Whisper large-v3", AccessibleRole::Button);
    assert_eq!(delete.accessible_enabled(), Some(false));
    assert_eq!(
        delete.accessible_description().as_deref(),
        Some(DELETE_NEEDS_DAEMON)
    );
}

#[test]
fn advanced_choices_write_the_config_and_read_back() {
    let mut a = Advanced::default();
    assert_eq!(adv_modes(&a), [0; 9]);
    adv_apply(&mut a, P_THREADS, 1, 99.0);
    assert_eq!(a.threads, Auto::Set(32), "put in range");
    adv_apply(&mut a, P_FLASH, 2, 0.0);
    adv_apply(&mut a, P_DECODING, 2, 0.0);
    adv_apply(&mut a, P_CANDIDATES, 1, 7.0);
    assert_eq!(
        (a.decoding, a.beam_size, a.best_of),
        (Auto::Set(Decoding::Beam), 7, 1)
    );
    adv_apply(&mut a, P_TEMPERATURE, 1, 0.3);
    adv_apply(&mut a, P_LOGPROB, 1, -9.0);
    assert_eq!(a.logprob_threshold, Auto::Set(-5.0));
    assert_eq!(adv_modes(&a), [1, 2, 2, 1, 0, 0, 1, 0, 0]);
    for p in [P_THREADS, P_FLASH, P_DECODING, P_TEMPERATURE, P_LOGPROB] {
        adv_apply(&mut a, p, 0, 1.0);
    }
    assert_eq!(adv_modes(&a), [0; 9]);
}

/// What `auto` is judged for: the loaded device first, the CPU choice always the processor.
#[test]
fn auto_follows_the_loaded_device() {
    let gpu = Device {
        name: "Vulkan0".into(),
        description: String::new(),
        uma: false,
    };
    let named = DeviceChoice::Named("Vulkan0".into());
    let table = [
        // Loaded: what runs counts, listed or not.
        ("Vulkan0", true, None, DeviceChoice::Auto, true),
        (
            "cpu",
            true,
            Some(&[gpu.clone()][..]),
            DeviceChoice::Auto,
            false,
        ),
        ("cpu", true, None, named.clone(), false),
        // Whatever is loaded, the CPU choice is the processor.
        ("Vulkan0", true, None, DeviceChoice::Cpu, false),
        // Nothing loaded: what the picker would pick among the listed GPUs.
        ("none", false, Some(&[][..]), DeviceChoice::Auto, false),
        (
            "",
            false,
            Some(&[gpu.clone()][..]),
            DeviceChoice::Auto,
            true,
        ),
        // Not listed yet: a guess, GPU unless the CPU is chosen.
        ("", false, None, DeviceChoice::Auto, true),
    ];
    for (backend, loaded, listed, choice, want) in table {
        assert_eq!(
            auto_on_gpu(backend, loaded, listed, &choice),
            want,
            "{backend} {choice:?}"
        );
    }
}

#[test]
fn tiles_say_the_state_once_in_the_mark_and_the_facts_in_the_caption() {
    let row = |badge: &str, ready, downloading, progress| ModelRow {
        badge: badge.into(),
        ready,
        downloading,
        progress,
        ..ModelRow::default()
    };
    let cases = [
        (
            row("Ready", true, false, 0.0),
            false,
            ("Downloaded", MARK_DONE, ""),
        ),
        (row("Ready", true, false, 0.0), true, ("In use", 0, "")),
        (
            row("Not downloaded", false, false, 0.0),
            true,
            ("In use · Not downloaded", 0, ""),
        ),
        (
            row("Not downloaded", false, false, 0.0),
            false,
            ("Not downloaded", MARK_GET, ""),
        ),
        (
            row("Downloading", false, true, 0.454),
            false,
            ("Downloading 45%", 0, "45 %"),
        ),
        (
            row("Paused", false, false, 0.0),
            false,
            ("Paused", MARK_GET, ""),
        ),
    ];
    for (r, in_use, (hint, mark, text)) in cases {
        assert_eq!(
            tile_state(&r, in_use),
            (hint.to_owned(), mark, text.to_owned())
        );
    }
}

#[test]
fn a_dialog_starts_at_the_current_file() {
    let dir = crate::testing::scratch_dir(&format!(
        "oxi-settings-lib-model-{}-start",
        std::process::id()
    ));
    let (models, other) = (dir.join("models"), dir.join("mine"));
    fs::create_dir_all(&models).unwrap();
    fs::create_dir_all(&other).unwrap();
    let file = other.join("x.bin").to_string_lossy().into_owned();
    assert_eq!(start_dir(&file, &models), Some(other.clone()));
    // A folder that is gone: the nearest one above it.
    let gone = other
        .join("gone")
        .join("x.bin")
        .to_string_lossy()
        .into_owned();
    assert_eq!(start_dir(&gone, &models), Some(other));
    // Empty or relative: the models folder, or the nearest one above it.
    assert_eq!(start_dir("", &models), Some(models.clone()));
    assert_eq!(start_dir("mine.bin", &models), Some(models.clone()));
    assert_eq!(start_dir("", &models.join("not-yet")), Some(models));
}

#[test]
fn fractions() {
    assert!((fraction(1, 4) - 0.25).abs() < f32::EPSILON);
    assert!(fraction(1, 0).abs() < f32::EPSILON);
    assert!((fraction(9, 4) - 1.0).abs() < f32::EPSILON);
}
