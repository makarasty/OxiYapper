//! Text page under the testing backend, on a temporary Layout with synthetic files.

mod common;

use std::fs;
use std::time::Duration;

use common::{config, has, layout, one, open, with_role};
use i_slint_backend_testing::AccessibleRole;
use oxi_core::config::PasteMode;
use oxi_core::dictionary::{BUDGET, build_prompt};
use oxi_core::paths::Layout;
use oxi_settings::{AppWindow, Page};
use slint::ComponentHandle;
use slint::platform::{Key, WindowEvent};

/// The Text page in a window tall enough for all of it: the element search finds only what is in
/// view.
fn open_tall(l: &Layout) -> AppWindow {
    let window = open(l, Page::Text);
    window
        .window()
        .set_size(slint::LogicalSize::new(1280.0, 2400.0));
    window
}

/// The editor labelled `label`; the group title above it shares the label.
fn editor(window: &AppWindow, label: &str) -> i_slint_backend_testing::ElementHandle {
    with_role(window, label, AccessibleRole::TextInput)
}

/// Labels of every element on screen that start with `prefix`.
fn labels(window: &AppWindow, prefix: &str) -> Vec<String> {
    let mut found: Vec<String> = i_slint_backend_testing::ElementQuery::from_root(window)
        .match_descendants()
        .find_all()
        .iter()
        .filter_map(|e| e.accessible_label().map(|l| l.to_string()))
        .filter(|l| l.starts_with(prefix))
        .collect();
    found.dedup();
    found
}

#[test]
fn over_budget_dictionary_marks_the_tail() {
    let words: Vec<String> = (1..=300).map(|i| format!("synthword{i}")).collect();
    let l = layout("budget", None);
    fs::write(l.dictionary_file(), words.join("\n")).unwrap();
    let window = open_tall(&l);

    let first_dropped = build_prompt(words.iter().map(String::as_str)).dropped[0] + 1;
    assert!(first_dropped > 1 && first_dropped < 300, "{first_dropped}");
    let over = format!("Over the limit: line {first_dropped} and after are not sent.");
    assert!(has(&window, &over), "{:?}", labels(&window, "Over"));
    let count = labels(&window, "")
        .into_iter()
        .find(|l| l.ends_with(&format!(" / {BUDGET} tokens")))
        .unwrap();
    let tokens: usize = count.split(' ').next().unwrap().parse().unwrap();
    assert!(tokens > BUDGET, "{count}");

    // Down to the first ten words: everything fits, the mark goes.
    editor(&window, "Dictionary").set_accessible_value(words[..10].join("\n"));
    assert_eq!(labels(&window, "Over the limit"), Vec::<String>::new());
    let count = labels(&window, "")
        .into_iter()
        .find(|l| l.ends_with(&format!(" / {BUDGET} tokens")))
        .unwrap();
    let small: usize = count.split(' ').next().unwrap().parse().unwrap();
    assert!(small > 0 && small <= BUDGET, "{count}");
}

#[test]
fn replacement_line_without_arrow_reports_its_line() {
    let l = layout("replace", None);
    let window = open_tall(&l);
    assert!(has(&window, "All lines valid"));
    editor(&window, "Replacements")
        .set_accessible_value("alpha => beta\nfoo bar\n\ngamma => delta");
    assert!(
        has(&window, "Line 2 has no =>: ignored"),
        "{:?}",
        labels(&window, "Line")
    );

    one(&window, "Save replace.txt").invoke_accessible_default_action();
    assert_eq!(
        fs::read_to_string(l.replace_file()).unwrap(),
        "alpha => beta\nfoo bar\n\ngamma => delta"
    );
}

#[test]
fn dictionary_is_saved_into_the_layout() {
    let l = layout("save-dict", None);
    let window = open_tall(&l);
    editor(&window, "Dictionary").set_accessible_value("synthalpha\nsynthbeta");
    assert!(!l.dictionary_file().exists(), "saved before Save");
    one(&window, "Save dictionary.txt").invoke_accessible_default_action();
    assert_eq!(
        fs::read_to_string(l.dictionary_file()).unwrap(),
        "synthalpha\nsynthbeta"
    );
}

#[test]
fn utf16_file_is_rewritten_as_utf8_with_a_note_until_saved() {
    let l = layout("utf16", None);
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(
        "synthalpha\nsynthbeta"
            .encode_utf16()
            .flat_map(u16::to_le_bytes),
    );
    fs::write(l.dictionary_file(), bytes).unwrap();
    let window = open_tall(&l);
    let note = "Saved as UTF-16 or with a BOM: Save rewrites it as UTF-8.";
    assert!(has(&window, note));
    assert_eq!(
        editor(&window, "Dictionary").accessible_value().unwrap(),
        "synthalpha\nsynthbeta"
    );

    one(&window, "Save dictionary.txt").invoke_accessible_default_action();
    assert_eq!(
        fs::read(l.dictionary_file()).unwrap(),
        b"synthalpha\nsynthbeta"
    );
    assert!(!has(&window, note));
}

#[test]
fn unreadable_file_is_never_overwritten() {
    let l = layout("unreadable", None);
    let bytes = [b's', 0xFF, b'x'];
    fs::write(l.replace_file(), bytes).unwrap();
    let window = open_tall(&l);
    assert!(has(
        &window,
        "replace.txt is not UTF-8 or UTF-16 text: fix it in a text editor."
    ));
    let save = one(&window, "Save replace.txt");
    assert_eq!(save.accessible_enabled(), Some(false));
    save.invoke_accessible_default_action();
    assert_eq!(fs::read(l.replace_file()).unwrap(), bytes);
}

#[test]
fn save_keeps_a_file_changed_on_disk_until_overwrite() {
    let l = layout("conflict", None);
    fs::write(l.dictionary_file(), "synthalpha").unwrap();
    let window = open_tall(&l);
    editor(&window, "Dictionary").set_accessible_value("synthbeta");
    assert!(has(&window, "Unsaved changes"));
    // Notepad saves its own edit meanwhile.
    fs::write(l.dictionary_file(), "synthgamma").unwrap();

    one(&window, "Save dictionary.txt").invoke_accessible_default_action();
    assert_eq!(
        fs::read_to_string(l.dictionary_file()).unwrap(),
        "synthgamma"
    );
    assert_eq!(
        editor(&window, "Dictionary").accessible_value().unwrap(),
        "synthbeta"
    );
    let note = "dictionary.txt changed on disk since it was loaded.";
    assert!(has(&window, note));

    one(&window, "Overwrite dictionary.txt").invoke_accessible_default_action();
    assert_eq!(
        fs::read_to_string(l.dictionary_file()).unwrap(),
        "synthbeta"
    );
    assert!(!has(&window, note));
    assert!(!has(&window, "Unsaved changes"));
}

#[test]
fn clean_editor_takes_a_file_changed_on_disk_instead_of_saving() {
    let l = layout("clean-reload", None);
    // Save stays on for a clean editor only when it would rewrite a converted file.
    fs::write(l.replace_file(), b"\xEF\xBB\xBFa => b").unwrap();
    let window = open_tall(&l);
    fs::write(l.replace_file(), "c => d").unwrap();
    one(&window, "Save replace.txt").invoke_accessible_default_action();
    assert_eq!(fs::read_to_string(l.replace_file()).unwrap(), "c => d");
    assert_eq!(
        editor(&window, "Replacements").accessible_value().unwrap(),
        "c => d"
    );
}

#[test]
fn fixed_file_is_read_again_when_the_page_is_shown_or_on_reload() {
    let l = layout("fixed", None);
    fs::write(l.dictionary_file(), [b's', 0xFF]).unwrap();
    fs::write(l.replace_file(), [b's', 0xFF]).unwrap();
    let window = open_tall(&l);
    assert_eq!(
        one(&window, "Save dictionary.txt").accessible_enabled(),
        Some(false)
    );
    fs::write(l.dictionary_file(), "synthalpha").unwrap();
    fs::write(l.replace_file(), "a => b").unwrap();

    one(&window, "Reload replace.txt").invoke_accessible_default_action();
    assert_eq!(
        editor(&window, "Replacements").accessible_value().unwrap(),
        "a => b"
    );
    assert!(!has(
        &window,
        "replace.txt is not UTF-8 or UTF-16 text: fix it in a text editor."
    ));

    window.set_page(Page::About);
    // The query builds the About page, so the Text page is really left.
    assert!(has(&window, "Version"));
    window.set_page(Page::Text);
    assert_eq!(
        editor(&window, "Dictionary").accessible_value().unwrap(),
        "synthalpha"
    );
    // Clean, so Save is off until an edit; readable again, so an edit turns it on.
    let save = || one(&window, "Save dictionary.txt").accessible_enabled();
    assert_eq!(save(), Some(false));
    editor(&window, "Dictionary").set_accessible_value("synthbeta");
    assert_eq!(save(), Some(true));
}

#[test]
fn reset_keeps_unsaved_text_and_reloads_clean_files() {
    let l = layout("reset", Some("version = 1\n[audio]\ngain_db = \n"));
    fs::write(l.dictionary_file(), "synthalpha").unwrap();
    fs::write(l.replace_file(), "a => b").unwrap();
    let window = open_tall(&l);
    editor(&window, "Dictionary").set_accessible_value("synthbeta");
    fs::write(l.replace_file(), "c => d").unwrap();

    one(&window, "Reset to defaults").invoke_accessible_default_action();
    assert_eq!(
        editor(&window, "Dictionary").accessible_value().unwrap(),
        "synthbeta"
    );
    assert!(has(&window, "Unsaved changes"));
    assert_eq!(
        editor(&window, "Replacements").accessible_value().unwrap(),
        "c => d"
    );
}

#[test]
fn read_error_names_its_kind_and_keeps_save_off() {
    let l = layout("read-error", None);
    // A directory in the file's place fails the read with an io error, not a decode error.
    fs::create_dir(l.dictionary_file()).unwrap();
    let window = open_tall(&l);
    let notes = labels(&window, "Could not read dictionary.txt: ");
    assert_eq!(notes.len(), 1, "{:?}", labels(&window, ""));
    assert!(!has(
        &window,
        "dictionary.txt is not UTF-8 or UTF-16 text: fix it in a text editor."
    ));
    let save = one(&window, "Save dictionary.txt");
    assert_eq!(save.accessible_enabled(), Some(false));
    save.invoke_accessible_default_action();
    assert!(l.dictionary_file().is_dir());
}

#[test]
fn ctrl_s_saves_the_focused_file_and_only_when_dirty() {
    let l = layout("ctrl-s", None);
    let window = open_tall(&l);
    let key = |down: bool, text: slint::SharedString| {
        window.window().dispatch_event(if down {
            WindowEvent::KeyPressed { text }
        } else {
            WindowEvent::KeyReleased { text }
        });
    };
    let chord = |letter: &str| {
        key(true, Key::Control.into());
        key(true, letter.into());
        key(false, letter.into());
        key(false, Key::Control.into());
    };
    let ctrl_s = || chord("s");
    // Tab to the editor; the first character that lands in it shows the focus is there.
    let dictionary = editor(&window, "Dictionary");
    for _ in 0..10 {
        key(true, Key::Tab.into());
        key(false, Key::Tab.into());
        key(true, "x".into());
        key(false, "x".into());
        if dictionary.accessible_value().as_deref() == Some("x") {
            break;
        }
    }
    assert_eq!(dictionary.accessible_value().as_deref(), Some("x"));
    assert!(!l.dictionary_file().exists());
    ctrl_s();
    assert_eq!(fs::read_to_string(l.dictionary_file()).unwrap(), "x");
    assert!(!has(&window, "Unsaved changes"));

    // Clean now: Ctrl+S writes nothing over a change from outside.
    fs::write(l.dictionary_file(), "synthother").unwrap();
    ctrl_s();
    assert_eq!(
        fs::read_to_string(l.dictionary_file()).unwrap(),
        "synthother"
    );

    // The poll brings the clean editor up to date; Ctrl+S then saves what is typed next.
    i_slint_backend_testing::mock_elapsed_time(oxi_settings::FILE_POLL);
    assert_eq!(dictionary.accessible_value().as_deref(), Some("synthother"));
    // A Ukrainian layout sends "і" for the S key, a Russian one "ы".
    for letter in ["s", "і", "ы"] {
        key(true, "q".into());
        key(false, "q".into());
        assert!(has(&window, "Unsaved changes"), "{letter}");
        chord(letter);
        let saved = fs::read_to_string(l.dictionary_file()).unwrap();
        assert!(saved.contains('q'), "Ctrl+{letter} did not save: {saved}");
        assert!(!has(&window, "Unsaved changes"), "{letter}");
    }
}

#[test]
fn a_clean_editor_follows_the_file_and_a_dirty_one_keeps_its_text() {
    let l = layout("follow", None);
    fs::write(l.dictionary_file(), "synthone").unwrap();
    let window = open_tall(&l);
    let value = || editor(&window, "Dictionary").accessible_value().unwrap();
    assert_eq!(value(), "synthone");

    fs::write(l.dictionary_file(), "synthtwo").unwrap();
    i_slint_backend_testing::mock_elapsed_time(oxi_settings::FILE_POLL);
    assert_eq!(value(), "synthtwo");
    assert!(!has(&window, "Unsaved changes"));

    editor(&window, "Dictionary").set_accessible_value("synthmine");
    fs::write(l.dictionary_file(), "synththree").unwrap();
    i_slint_backend_testing::mock_elapsed_time(oxi_settings::FILE_POLL);
    assert_eq!(value(), "synthmine");
    assert!(has(
        &window,
        "dictionary.txt changed on disk since it was loaded."
    ));
}

#[test]
fn restore_delay_is_saved_once_the_slider_rests() {
    let l = layout("delay", None);
    let window = open_tall(&l);
    let slider = with_role(&window, "Restore clipboard after", AccessibleRole::Spinbox);
    slider.set_accessible_value("400");
    slider.set_accessible_value("500");
    assert_eq!(slider.accessible_value().as_deref(), Some("500"));
    assert!(!l.config_file().exists(), "saved during the drag");
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(350));
    assert_eq!(config(&l).config.paste.restore_delay_ms, 500);
}

#[test]
fn duplicate_rule_shows_a_note() {
    let l = layout("duplicate", None);
    let window = open_tall(&l);
    one(&window, "New rule app").set_accessible_value("synth.exe");
    one(&window, "Add rule").invoke_accessible_default_action();
    one(&window, "New rule app").set_accessible_value("SYNTH.EXE");
    one(&window, "Add rule").invoke_accessible_default_action();
    let note = "SYNTH.EXE already has a rule: change its mode in the list above.";
    assert!(has(&window, note), "{:?}", labels(&window, "SYNTH"));
    assert_eq!(config(&l).config.paste.rules.len(), 2);

    one(&window, "New rule app").set_accessible_value("other.exe");
    one(&window, "Add rule").invoke_accessible_default_action();
    assert!(!has(&window, note));

    // Removing the original rule takes the note about it away too.
    one(&window, "New rule app").set_accessible_value("SYNTH.EXE");
    one(&window, "Add rule").invoke_accessible_default_action();
    assert!(has(&window, note));
    one(&window, "Remove synth.exe").invoke_accessible_default_action();
    assert!(!has(&window, note));
}

#[test]
fn closing_within_the_debounce_still_saves_the_delay() {
    let l = layout("delay-close", None);
    let window = open_tall(&l);
    with_role(&window, "Restore clipboard after", AccessibleRole::Spinbox)
        .set_accessible_value("700");
    assert!(!l.config_file().exists(), "saved during the drag");
    window.hide().unwrap();
    drop(window);
    assert_eq!(config(&l).config.paste.restore_delay_ms, 700);
}

#[test]
fn dirty_editor_names_the_read_error_of_its_file() {
    let l = layout("dirty-unreadable", None);
    fs::write(l.dictionary_file(), "synthalpha").unwrap();
    let window = open_tall(&l);
    editor(&window, "Dictionary").set_accessible_value("synthbeta");
    fs::remove_file(l.dictionary_file()).unwrap();
    fs::create_dir(l.dictionary_file()).unwrap();

    one(&window, "Save dictionary.txt").invoke_accessible_default_action();
    assert_eq!(
        labels(&window, "Could not read dictionary.txt on disk: ").len(),
        1,
        "{:?}",
        labels(&window, "dictionary.txt")
    );
    assert!(!has(
        &window,
        "dictionary.txt changed on disk since it was loaded."
    ));
    assert!(has(&window, "Overwrite dictionary.txt"));
    assert!(l.dictionary_file().is_dir());
    assert_eq!(
        editor(&window, "Dictionary").accessible_value().unwrap(),
        "synthbeta"
    );
}

#[test]
fn mode_thumb_and_rules_write_the_config() {
    let l = layout("paste", Some("version = 1\n# mine\n"));
    let window = open_tall(&l);
    with_role(&window, "Paste mode", AccessibleRole::RadioGroup);
    let card = |label: &str| with_role(&window, label, AccessibleRole::RadioButton);
    assert_eq!(card("Clipboard").accessible_checked(), Some(true));
    card("Type").invoke_accessible_default_action();
    assert_eq!(card("Type").accessible_checked(), Some(true));
    let file = config(&l);
    assert_eq!(file.config.paste.mode, PasteMode::Type);
    assert!(
        fs::read_to_string(l.config_file())
            .unwrap()
            .contains("# mine")
    );

    with_role(&window, "Trailing space", AccessibleRole::Switch).invoke_accessible_default_action();
    assert!(!config(&l).config.paste.trailing_space);

    // The default rule is mstsc.exe; add one, refuse its case-insensitive twin, remove the first.
    assert!(has(&window, "Remove mstsc.exe"));
    one(&window, "New rule app").set_accessible_value("synth.exe");
    one(&window, "Add rule").invoke_accessible_default_action();
    one(&window, "New rule app").set_accessible_value("SYNTH.EXE");
    one(&window, "Add rule").invoke_accessible_default_action();
    one(&window, "Remove mstsc.exe").invoke_accessible_default_action();
    let rules = config(&l).config.paste.rules;
    assert_eq!(rules.len(), 1, "{rules:?}");
    assert_eq!(rules[0].app, "synth.exe");
    assert_eq!(rules[0].mode, Some(PasteMode::Type));
    assert!(!has(&window, "Remove mstsc.exe"));
    assert!(has(&window, "Remove synth.exe"));
}

#[test]
fn locked_config_keeps_paste_controls_off() {
    let original = "version = 99\n";
    let l = layout("locked", Some(original));
    let window = open_tall(&l);
    let add = one(&window, "Add rule");
    assert_eq!(add.accessible_enabled(), Some(false));
    assert_eq!(
        one(&window, "Remove mstsc.exe").accessible_enabled(),
        Some(false)
    );
    assert_eq!(fs::read_to_string(l.config_file()).unwrap(), original);
}
