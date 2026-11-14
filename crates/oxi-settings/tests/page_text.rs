//! Text page under the testing backend, on a temporary Layout with synthetic files.

mod common;

use std::fmt::Write as _;
use std::fs;
use std::time::Duration;

use common::{config, has, layout, one, open, open_again, with_role};
use i_slint_backend_testing::AccessibleRole;
use oxi_core::config::{FileFormat, FileMode, PasteMode};
use oxi_core::dictionary::build_prompt;
use oxi_core::paths::Layout;
use oxi_settings::{AppWindow, Page, TextState};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, Model};

/// The Text page in a window tall enough for all of it: the element search finds only what is in
/// view.
fn open_tall(l: &Layout) -> AppWindow {
    let window = open(l, Page::Text);
    window
        .window()
        .set_size(slint::LogicalSize::new(1280.0, 2400.0));
    window
}

/// Opens the sub-tab `label` (Dictionary is open at first). Paste, Write to file and Per-app
/// rules are the Paste page's (ia-map 3.2): it opens that page on them, and the Text page for the
/// others.
fn sub_tab(window: &AppWindow, label: &str) {
    let paste = ["Paste", "Write to file", "Per-app rules"]
        .iter()
        .position(|t| *t == label);
    if let Some(tab) = paste {
        window.set_page(Page::Paste);
        window
            .global::<TextState>()
            .set_paste_tab(i32::try_from(tab).unwrap());
        return;
    }
    if window.get_page() != Page::Text {
        window.set_page(Page::Text);
    }
    with_role(window, label, AccessibleRole::Tab).invoke_accessible_default_action();
}

/// The editor labelled `label`; the sub-tab above it shares the label.
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
    // "{p} % full" (item 34): over the budget it is past 100.
    let full = || -> usize {
        let count = labels(&window, "")
            .into_iter()
            .find(|l| l.ends_with(" % full"))
            .unwrap();
        count.split(' ').next().unwrap().parse().unwrap()
    };
    assert!(full() > 100, "{}", full());

    // Down to the first ten words: everything fits, the mark goes.
    editor(&window, "Dictionary").set_accessible_value(words[..10].join("\n"));
    assert_eq!(labels(&window, "Over the limit"), Vec::<String>::new());
    let small = full();
    assert!(small > 0 && small <= 100, "{small}");
}

#[test]
fn replacement_line_without_arrow_reports_its_line() {
    let l = layout("replace", None);
    let window = open_tall(&l);
    sub_tab(&window, "Replacements");
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
fn utf16_file_with_a_bad_line_says_both_the_worse_first() {
    let l = layout("utf16-bad", None);
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend("synth line".encode_utf16().flat_map(u16::to_le_bytes));
    fs::write(l.replace_file(), bytes).unwrap();
    let window = open_tall(&l);
    sub_tab(&window, "Replacements");
    assert!(
        has(
            &window,
            "Line 1 has no =>: ignored\nSaved as UTF-16 or with a BOM: Save rewrites it as UTF-8."
        ),
        "{:?}",
        labels(&window, "Line")
    );
}

#[test]
fn unreadable_file_is_never_overwritten() {
    let l = layout("unreadable", None);
    let bytes = [b's', 0xFF, b'x'];
    fs::write(l.replace_file(), bytes).unwrap();
    let window = open_tall(&l);
    sub_tab(&window, "Replacements");
    assert!(has(
        &window,
        "replace.txt is not UTF-8 or UTF-16 text: fix it in a text editor."
    ));
    // The empty editor says it too, so it does not pass for an empty file.
    assert_eq!(
        editor(&window, "Replacements")
            .accessible_placeholder_text()
            .as_deref(),
        Some("replace.txt is not UTF-8 or UTF-16 text: fix it in a text editor.")
    );
    let save = one(&window, "Save replace.txt");
    assert_eq!(save.accessible_enabled(), Some(false));
    save.invoke_accessible_default_action();
    assert_eq!(fs::read(l.replace_file()).unwrap(), bytes);
}

/// UI rule 24: at the window maximum the editor takes the room under the sub-tabs, no empty band
/// under its card; an empty editor shows the file's format.
#[test]
fn editor_fills_the_maximum_window_and_hints_the_format() {
    let window = open(&layout("editor-max", None), Page::Text);
    window
        .window()
        .set_size(slint::LogicalSize::new(1280.0, 860.0));
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    let dictionary = editor(&window, "Dictionary");
    assert_eq!(
        dictionary.accessible_placeholder_text().as_deref(),
        Some("Names and rare words Whisper should know, one per line")
    );
    let save = one(&window, "Save dictionary.txt");
    let bottom = save.absolute_position().y + save.size().height;
    assert!(bottom > 860.0 - 80.0, "the card ends at {bottom} of 860");
    assert!(dictionary.size().height > 360.0, "{:?}", dictionary.size());
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
    sub_tab(&window, "Replacements");
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

    sub_tab(&window, "Replacements");
    one(&window, "Reload replace.txt").invoke_accessible_default_action();
    assert_eq!(
        editor(&window, "Replacements").accessible_value().unwrap(),
        "a => b"
    );
    assert!(!has(
        &window,
        "replace.txt is not UTF-8 or UTF-16 text: fix it in a text editor."
    ));

    window.set_page(Page::Overlay);
    // The query builds the Overlay page, so the Text page is really left.
    assert!(has(&window, "Show the overlay"));
    window.set_page(Page::Text);
    sub_tab(&window, "Dictionary");
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
    let l = layout("reset", Some("version = 2\n[audio]\ngain_db = \n"));
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
    sub_tab(&window, "Replacements");
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
    window.invoke_files_check();
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
    window.invoke_files_check();
    assert_eq!(value(), "synthtwo");
    assert!(!has(&window, "Unsaved changes"));

    editor(&window, "Dictionary").set_accessible_value("synthmine");
    fs::write(l.dictionary_file(), "synththree").unwrap();
    window.invoke_files_check();
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
    sub_tab(&window, "Paste");
    let slider = with_role(
        &window,
        "Give your copy back after",
        AccessibleRole::Spinbox,
    );
    slider.set_accessible_value("400");
    slider.set_accessible_value("500");
    assert_eq!(slider.accessible_value().as_deref(), Some("500"));
    assert!(!l.config_file().exists(), "saved during the drag");
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(350));
    assert_eq!(config(&l).config.paste.restore_delay_ms, 500);
}

#[test]
fn duplicate_rule_is_refused_without_a_toast() {
    let l = layout("duplicate", None);
    let window = open_tall(&l);
    sub_tab(&window, "Per-app rules");
    one(&window, "New rule app").set_accessible_value("synth.exe");
    one(&window, "Add rule").invoke_accessible_default_action();
    one(&window, "New rule app").set_accessible_value("SYNTH.EXE");
    one(&window, "Add rule").invoke_accessible_default_action();
    // The row of the field says so; the rule flashes instead of a toast saying it again.
    assert!(!has(&window, "already has a rule: change its mode"));
    assert_eq!(config(&l).config.paste.rules.len(), 2);
}

#[test]
fn closing_within_the_debounce_still_saves_the_delay() {
    let l = layout("delay-close", None);
    let window = open_tall(&l);
    sub_tab(&window, "Paste");
    with_role(
        &window,
        "Give your copy back after",
        AccessibleRole::Spinbox,
    )
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
    let l = layout("paste", Some("version = 2\n# mine\n"));
    let window = open_tall(&l);
    sub_tab(&window, "Paste");
    with_role(&window, "Paste mode", AccessibleRole::RadioGroup);
    let card = |label: &str| with_role(&window, label, AccessibleRole::RadioButton);
    assert_eq!(card("Clipboard").accessible_checked(), Some(true));
    card("Type").invoke_accessible_default_action();
    assert_eq!(card("Type").accessible_checked(), Some(true));
    // Per-app Clipboard rules and "Copy last text" still use both rows in Type mode: they stay live.
    let clipboard_rows = || {
        [
            with_role(
                &window,
                "Keep the last dictation in Win+V",
                AccessibleRole::Switch,
            ),
            with_role(&window, "Give your copy back after", AccessibleRole::Slider),
        ]
        .map(|e| e.accessible_enabled())
    };
    assert_eq!(clipboard_rows(), [Some(true), Some(true)]);
    card("Clipboard").invoke_accessible_default_action();
    assert_eq!(clipboard_rows(), [Some(true), Some(true)]);
    card("Type").invoke_accessible_default_action();
    let file = config(&l);
    assert_eq!(file.config.paste.mode, PasteMode::Type);
    assert!(
        fs::read_to_string(l.config_file())
            .unwrap()
            .contains("# mine")
    );

    with_role(
        &window,
        "Add a space after the text",
        AccessibleRole::Switch,
    )
    .invoke_accessible_default_action();
    assert!(!config(&l).config.paste.trailing_space);

    // The default rule is mstsc.exe; add one, refuse its case-insensitive twin, remove the first.
    sub_tab(&window, "Per-app rules");
    assert!(has(&window, "Remove mstsc.exe"));
    one(&window, "New rule app").set_accessible_value("synth.exe");
    one(&window, "Add rule").invoke_accessible_default_action();
    one(&window, "New rule app").set_accessible_value("SYNTH.EXE");
    one(&window, "Add rule").invoke_accessible_default_action();
    // Typed without ".exe" it is the same app (the daemon matches the whole file name); only
    // Windows adds the ".exe", so elsewhere "Synth" is another app.
    if cfg!(windows) {
        one(&window, "New rule app").set_accessible_value("Synth");
        one(&window, "Add rule").invoke_accessible_default_action();
    }
    // A refused duplicate leaves its text in the field, which filters the list: clear it.
    one(&window, "New rule app").set_accessible_value("");
    one(&window, "Remove mstsc.exe").invoke_accessible_default_action();
    let rules = config(&l).config.paste.rules;
    assert_eq!(rules.len(), 1, "{rules:?}");
    assert_eq!(rules[0].app, "synth.exe");
    assert_eq!(rules[0].mode, Some(PasteMode::Type));
    assert!(!has(&window, "Remove mstsc.exe"));
    assert!(has(&window, "Remove synth.exe"));
}

/// The rule count shows at 0 too, so removing the last rule does not narrow the tab and move its
/// underline; a screen reader hears the count.
#[test]
fn rule_count_keeps_the_tab_width_at_zero() {
    let l = layout("rule-count", None);
    let window = open_tall(&l);
    sub_tab(&window, "Per-app rules");
    let tab = || with_role(&window, "Per-app rules", AccessibleRole::Tab);
    assert_eq!(tab().accessible_description().as_deref(), Some("1"));
    let wide = tab().size().width;
    one(&window, "Remove mstsc.exe").invoke_accessible_default_action();
    assert_eq!(config(&l).config.paste.rules, []);
    assert_eq!(tab().accessible_description().as_deref(), Some("0"));
    // Item 39: the empty list says so where the rules were.
    assert!(has(&window, "No app has a rule of its own"));
    assert!(
        (tab().size().width - wide).abs() < 0.5,
        "{wide} -> {}",
        tab().size().width
    );
}

#[test]
fn locked_config_keeps_paste_controls_off() {
    let original = "version = 99\n";
    let l = layout("locked", Some(original));
    let window = open_tall(&l);
    sub_tab(&window, "Per-app rules");
    // The field and the button that opens the apps popup are off, so no rule can be added.
    assert_eq!(
        one(&window, "New rule app").accessible_enabled(),
        Some(false)
    );
    assert_eq!(
        one(&window, "Scan for apps").accessible_enabled(),
        Some(false)
    );
    assert_eq!(
        one(&window, "Remove mstsc.exe").accessible_enabled(),
        Some(false)
    );
    assert_eq!(fs::read_to_string(l.config_file()).unwrap(), original);
}

/// Owner 2026-11-12: off by default; on with Clipboard mode the row's "!" says the Enter needs
/// the app's own read (UI rule 2), and it goes away in Type mode.
#[test]
fn press_enter_is_off_until_switched_on_and_warns_in_clipboard_mode() {
    const WARN: &str = "With Clipboard mode, Enter only when the app confirms it took the text";
    let l = layout("press-enter", None);
    let window = open_tall(&l);
    // Owner item 14: the row sits with pasting, on Paste > Paste.
    sub_tab(&window, "Paste");
    assert!(!config(&l).config.paste.press_enter);
    assert_eq!(
        labels(&window, WARN),
        Vec::<String>::new(),
        "off: no warning"
    );
    with_role(&window, "Press Enter after pasting", AccessibleRole::Switch)
        .invoke_accessible_default_action();
    assert!(config(&l).config.paste.press_enter);
    let shown = labels(&window, WARN);
    assert_eq!(shown.len(), 1, "on, Clipboard mode: one warning");
    // Review d08-fix #4: option A keeps the text from Win+V and monitors until the app took it,
    // so the "!" no longer blames them; it says what fails closed.
    assert!(
        shown[0].contains("No confirmation, no Enter.") && !shown[0].contains("Win+V"),
        "{shown:?}"
    );
    with_role(&window, "Type", AccessibleRole::RadioButton).invoke_accessible_default_action();
    assert_eq!(config(&l).config.paste.mode, PasteMode::Type);
    assert_eq!(
        labels(&window, WARN),
        Vec::<String>::new(),
        "Type mode: no warning"
    );
}

#[test]
fn sub_tabs_show_one_part_of_the_page() {
    let l = layout("tabs", None);
    let window = open_tall(&l);
    let tab = |label| with_role(&window, label, AccessibleRole::Tab);
    let shown = || {
        [
            "Save dictionary.txt",
            "Save replace.txt",
            "Paste mode",
            "New rule app",
        ]
        .map(|label| has(&window, label))
    };
    assert_eq!(tab("Dictionary").accessible_item_selected(), Some(true));
    assert_eq!(shown(), [true, false, false, false]);
    sub_tab(&window, "Replacements");
    assert_eq!(tab("Replacements").accessible_item_selected(), Some(true));
    assert_eq!(shown(), [false, true, false, false]);
    sub_tab(&window, "Paste");
    assert_eq!(shown(), [false, false, true, false]);
    sub_tab(&window, "Per-app rules");
    assert_eq!(shown(), [false, false, false, true]);
}

#[test]
fn win_v_history_is_on_by_default_and_saved_off() {
    let l = layout("win-v", None);
    let window = open_tall(&l);
    sub_tab(&window, "Paste");
    let switch = || {
        with_role(
            &window,
            "Keep the last dictation in Win+V",
            AccessibleRole::Switch,
        )
    };
    assert_eq!(switch().accessible_checked(), Some(true));
    switch().invoke_accessible_default_action();
    assert_eq!(switch().accessible_checked(), Some(false));
    assert!(!config(&l).config.paste.keep_in_clipboard_history);
}

/// e08, owner 2026-11-12: lines are joined in a cmd.exe console by default; the switch sits over
/// the per-app rules that override it, and saves off.
#[test]
fn join_lines_in_command_prompts_is_on_by_default_and_saved_off() {
    let l = layout("join-lines", None);
    let window = open_tall(&l);
    sub_tab(&window, "Per-app rules");
    let switch = || {
        with_role(
            &window,
            "Join lines in command prompts",
            AccessibleRole::Switch,
        )
    };
    assert_eq!(switch().accessible_checked(), Some(true));
    switch().invoke_accessible_default_action();
    assert_eq!(switch().accessible_checked(), Some(false));
    assert!(!config(&l).config.paste.join_console_lines);
}

/// e12: the Paste tab links to Per-app rules with a button that switches the tab.
#[test]
fn paste_tab_links_to_the_per_app_rules() {
    let l = layout("paste-link", None);
    let window = open_tall(&l);
    sub_tab(&window, "Paste");
    let state = window.global::<TextState>();
    assert_eq!(state.get_paste_tab(), 0);
    with_role(&window, "Per-app rules", AccessibleRole::Button).invoke_accessible_default_action();
    assert_eq!(state.get_paste_tab(), 2);
    let tab = with_role(&window, "Per-app rules", AccessibleRole::Tab);
    assert_eq!(tab.accessible_item_selected(), Some(true));
    assert!(has(&window, "New rule app"), "the rules list is in view");
}

/// Owner item 15 (ia-map 3.2): writing to a file has a tab of its own, Paste > Write to file. Its
/// Folder and File type rows are live while writing is Off (choosing them writes nothing, so
/// nothing is grey without a reason); with writing on and no folder the Folder row says so.
#[test]
fn write_to_file_tab_keeps_its_rows_live_and_says_when_no_folder_is_chosen() {
    const NO_FOLDER: &str = "No folder chosen: nothing is written";
    let l = layout("folder-grey", None);
    let window = open_tall(&l);
    sub_tab(&window, "Write to file");
    let state = window.global::<TextState>();
    assert_eq!(state.get_file_mode(), 0);
    assert_eq!(one(&window, "File format").accessible_enabled(), Some(true));
    assert!(has(&window, "Choose folder"));
    assert!(!has(&window, NO_FOLDER), "Off: nothing to warn about");
    // The Combo's own pick reaches the state and the file.
    let combo = with_role(
        &window,
        "Write each dictation to a file",
        AccessibleRole::Combobox,
    );
    combo.mock_single_click(PointerEventButton::Left);
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::DownArrow.into(),
    });
    assert_eq!(state.get_file_mode(), 1);
    assert_eq!(config(&l).config.output.file_mode, FileMode::Also);
    assert!(
        has(&window, NO_FOLDER),
        "on with no folder: the Folder row's \"!\""
    );
    state.invoke_file_format_picked(1);
    let out = config(&l).config.output;
    assert_eq!(out.file_mode, FileMode::Also);
    assert_eq!(out.file_format, FileFormat::Txt);
}

/// c03 R4: writing on in an app rule only, no folder: the Folder row warns too.
#[test]
fn an_app_rule_that_writes_without_a_folder_warns_on_the_folder_row() {
    let l = layout(
        "folder-rule",
        Some("version = 2\n[[paste.rules]]\napp = \"synthapp.exe\"\nto_file = \"only\"\n"),
    );
    let window = open_tall(&l);
    sub_tab(&window, "Write to file");
    assert!(has(&window, "No folder chosen: nothing is written"));
}

/// e12-fix, DS-15.1: the Paste mode tiles carry their captions, at the tile's own height (no
/// pin): each caption ends inside its tile, above the tile's 12 px padding.
#[test]
fn paste_tiles_have_captions_inside_the_tile() {
    let l = layout("paste-tiles", None);
    let window = open_min(&l);
    sub_tab(&window, "Paste");
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    for (title, caption) in [
        ("Clipboard", "Pastes with Ctrl+V"),
        ("Type", "Types key by key, for games"),
    ] {
        let tile = with_role(&window, title, AccessibleRole::RadioButton);
        let text = one(&window, caption);
        let bottom =
            |e: &i_slint_backend_testing::ElementHandle| e.absolute_position().y + e.size().height;
        assert!(
            bottom(&text) <= bottom(&tile) - 12.0 + 0.5,
            "{caption} spills: caption {} tile {}",
            bottom(&text),
            bottom(&tile)
        );
    }
}

/// A config with `n` synthetic per-app rules.
fn many_rules(n: usize) -> String {
    let mut text = String::from("version = 2\n");
    for i in 0..n {
        let _ = write!(
            text,
            "[[paste.rules]]\napp = \"synthapp{i}.exe\"\nmode = \"type\"\n"
        );
    }
    text
}

/// The Text page at the minimum window size, the first tick laid out.
fn open_min(l: &Layout) -> AppWindow {
    let window = open(l, Page::Text);
    window
        .window()
        .set_size(slint::LogicalSize::new(720.0, 520.0));
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    window
}

/// Whether the element labelled `label` is whole inside the 520 px tall window.
fn whole_in_view(window: &AppWindow, label: &str) -> bool {
    i_slint_backend_testing::ElementHandle::find_by_accessible_label(window, label)
        .next()
        .is_some_and(|e| {
            let y = e.absolute_position().y;
            y >= 0.0 && y + e.size().height <= 520.0
        })
}

/// Whether the page itself scrolls: a wheel over the sub-tab bar moves it only if its content is
/// taller than the room it has (the list and the editors keep the wheel over themselves).
fn page_scrolls(window: &AppWindow) -> bool {
    // The first sub-tab of the open page: Dictionary on Text, Write to file on Paste (the rail
    // has an item named Paste too).
    let first = if window.get_page() == Page::Paste {
        "Write to file"
    } else {
        "Dictionary"
    };
    let at = with_role(window, first, AccessibleRole::Tab).absolute_position();
    window
        .window()
        .dispatch_event(WindowEvent::PointerScrolled {
            position: slint::LogicalPosition::new(at.x + 4.0, at.y + 4.0),
            delta_x: 0.0,
            delta_y: -100_000.0,
        });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(500));
    window.get_scroll_y() != 0.0
}

/// UI rule 30: a rule keeps its app's name between windows, and the default rule has one.
#[test]
fn rule_rows_are_named_without_a_scan() {
    let original = "version = 2
[ui.app_names]
\"synthapp.exe\" = \"Synth App\"
[[paste.rules]]
app = \"SynthApp.exe\"
";
    let window = open(&layout("rule-names", Some(original)), Page::Text);
    sub_tab(&window, "Per-app rules");
    assert!(has(&window, "Synth App"));
    // The name is the title; the exe stands under it as its caption (DS-28.1), never as the title.
    assert!(has(&window, "SynthApp.exe"), "the exe is the caption");
    assert!(has(&window, "Remove SynthApp.exe"));
    let window = open_again(&layout("rule-names-default", None), Page::Text);
    sub_tab(&window, "Per-app rules");
    assert!(has(&window, "Remote Desktop Connection"));
}

/// UI rules 1 and 28: a long app name keeps its rule row one line tall (cut, whole on hover).
#[test]
fn long_app_name_keeps_its_rule_row_one_line() {
    let long = "Synth Application With A Very Long Name That Goes On And On Past The Column";
    let original = format!(
        "version = 2\n[ui.app_names]\n\"synthlong.exe\" = \"{long}\"\n\
         [[paste.rules]]\napp = \"synthlong.exe\"\n[[paste.rules]]\napp = \"synthshort.exe\"\n"
    );
    let window = open_min(&layout("rule-long-name", Some(&original)));
    sub_tab(&window, "Per-app rules");
    let height = |label: &str| one(&window, label).size().height;
    let (long_h, short_h) = (height(long), height("synthshort.exe"));
    assert!(
        (long_h - short_h).abs() < 0.5,
        "long {long_h} vs short {short_h}"
    );
}

/// UI rule 25, worst case: a 500-word dictionary, a long replace.txt and 150 rules, and every
/// sub-tab has its last control in view at 720 x 520 and the page cannot scroll, with no banner,
/// the offline daemon's and a broken config's (Paste > Paste with no banner only).
#[test]
#[cfg_attr(
    not(windows),
    ignore = "the fit is measured in Segoe UI Variable Text, which only Windows has"
)]
fn every_tab_fits_the_minimum_size_with_long_lists() {
    let l = layout("fit", Some(&many_rules(150)));
    let words: Vec<String> = (0..500).map(|i| format!("synthword{i}")).collect();
    fs::write(l.dictionary_file(), words.join("\n")).unwrap();
    let lines: Vec<String> = (0..300).map(|i| format!("synth{i} => s{i}")).collect();
    fs::write(l.replace_file(), lines.join("\n")).unwrap();
    let window = open_min(&l);
    // Nothing, the offline daemon's banner, and a broken config's: each takes room off the page.
    for (banner, daemon) in [
        ("", ""),
        ("", oxi_core::strings::DAEMON_OFFLINE),
        ("Synthetic config trouble.", ""),
    ] {
        window.set_banner_text(banner.into());
        window.set_daemon_text(daemon.into());
        window.set_daemon_start(!daemon.is_empty());
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        for (tab, last) in [
            ("Dictionary", "Save dictionary.txt"),
            ("Replacements", "Save replace.txt"),
            ("Paste", "Give your copy back after"),
            ("Write to file", "File type"),
            ("Per-app rules", "Scan for apps"),
            ("Per-app rules", "Join lines in command prompts"),
            ("Formatting", "Certainty needed"),
            ("Voice commands", "Delete the last sentence"),
            ("Words", "Phrase shortcuts"),
        ] {
            sub_tab(&window, tab);
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
            // Paste > Paste has five rows (ia-map 3.2: 412 of 444 px, 32 left): a 72 px banner
            // pushes it past the window, and the page scrolls (ia-map RT R12, UI rule 1 (a)).
            // Formatting has seven rows since the two style switches (ia-map 3.4: 429 of 444 px):
            // the same.
            if (tab == "Paste" || tab == "Formatting") && !(banner.is_empty() && daemon.is_empty())
            {
                continue;
            }
            assert!(
                whole_in_view(&window, last),
                "{last} is cut on {tab} (banner {banner:?}, daemon {daemon:?})"
            );
            if tab == "Words" {
                // The label also names the row above the editor: the editor itself must be whole.
                let e = editor(&window, "Phrase shortcuts");
                let bottom = e.absolute_position().y + e.size().height;
                assert!(
                    bottom <= 520.0,
                    "the snippets editor is cut: bottom {bottom}"
                );
            }
            assert!(
                !page_scrolls(&window),
                "{tab} scrolls at 720 x 520 (banner {banner:?}, daemon {daemon:?})"
            );
        }
    }
}

/// Review d04 #8, UI rule 1: opening a rule's profile keeps the card's size, at the least window
/// and a large one, and the large one shows every field of the profile.
#[test]
fn opening_a_profile_keeps_the_card_size() {
    let l = layout("profile-size", Some(&many_rules(2)));
    let window = open_min(&l);
    sub_tab(&window, "Per-app rules");
    for (w, h) in [(720.0, 520.0), (1280.0, 860.0)] {
        window.window().set_size(slint::LogicalSize::new(w, h));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        let card = || {
            let card = i_slint_backend_testing::ElementHandle::find_by_element_id(
                &window,
                "PastePage::card",
            )
            .next()
            .expect("the rules card");
            (card.absolute_position(), card.size())
        };
        let list = card();
        one(&window, "Settings for synthapp0.exe").invoke_accessible_default_action();
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        assert!(has(&window, "Close the profile"), "the profile is open");
        assert_eq!(card(), list, "the card moved or changed size at {w}x{h}");
        if h > 520.0 {
            let bottom = i_slint_backend_testing::ElementHandle::find_by_accessible_label(
                &window,
                "Join lines",
            )
            .map(|e| e.absolute_position().y + e.size().height)
            .fold(0.0_f32, f32::max);
            assert!(bottom <= list.0.y + list.1.height, "the last field is cut");
        }
        one(&window, "Close the profile").invoke_accessible_default_action();
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        assert_eq!(card(), list, "closing moved the card at {w}x{h}");
    }
}

/// UI rule 26: the wheel over the rule list moves the list, never the page.
#[test]
fn a_long_rule_list_scrolls_by_itself() {
    let l = layout("rules-scroll", Some(&many_rules(150)));
    let window = open_min(&l);
    sub_tab(&window, "Per-app rules");
    let at = one(&window, "Remove synthapp0.exe").absolute_position();
    assert!(!has(&window, "Remove synthapp149.exe"));
    window
        .window()
        .dispatch_event(WindowEvent::PointerScrolled {
            position: slint::LogicalPosition::new(at.x, at.y),
            delta_x: 0.0,
            delta_y: -100_000.0,
        });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(500));
    assert!(
        has(&window, "Remove synthapp149.exe"),
        "the list did not scroll"
    );
    assert_eq!(window.get_scroll_y(), 0.0, "the page scrolled");
    assert!(whole_in_view(&window, "New rule app"));
    // Scan sits beside the field; the scan itself is tested in `pages::text`, which can point it at
    // synthetic shortcuts (this binary would read the real Start menu).
    assert!(whole_in_view(&window, "Scan for apps"));
}

/// The worst-case list still lets the keyboard reach the field, and Enter adds the typed exe.
#[test]
fn add_is_reached_by_keyboard_past_a_long_rule_list() {
    let l = layout("rules-keys", Some(&many_rules(150)));
    let window = open_min(&l);
    sub_tab(&window, "Per-app rules");
    let press = |text: slint::SharedString| {
        window
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        window
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text });
    };
    let field = || one(&window, "New rule app").accessible_value();
    // Tab on; the first letter that lands in the field shows the focus is there.
    for _ in 0..12 {
        press(Key::Tab.into());
        press("q".into());
        if field().as_deref() == Some("q") {
            break;
        }
    }
    assert_eq!(
        field().as_deref(),
        Some("q"),
        "Tab never reached the app field"
    );
    for c in ".exe".chars() {
        press(c.to_string().into());
    }
    // The list under the field offers the typed exe; Enter takes its first row.
    press(Key::Return.into());
    let rules = config(&l).config.paste.rules;
    assert_eq!(rules.len(), 151);
    assert_eq!(rules[150].app, "q.exe");
}

/// In a window too short for the tab (200 px) the page scrolls as a whole and its padding gives the scrollbar
/// gutter back; the apps popup still takes the window's width less 2 x 32 px, not the page's.
#[test]
fn apps_popup_is_window_wide_when_the_page_scrolls() {
    let window = open(&layout("popup-width", None), Page::Text);
    window
        .window()
        .set_size(slint::LogicalSize::new(720.0, 200.0));
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    sub_tab(&window, "Per-app rules");
    assert!(page_scrolls(&window), "the page must scroll at 720x200");
    one(&window, "Scan for apps").invoke_accessible_default_action();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    // Positions inside the popup are its own: it is 720 - 2 x 32 = 656 wide (668 with the old
    // derived width), and its padding (16 px) sits right of the close button.
    let close = one(&window, "Close");
    let right = close.absolute_position().x + close.size().width;
    assert!((right - 640.0).abs() < 0.5, "close button ends at {right}");
}

/// The Filler words editor is greyed while "Remove filler words" is Off, as Processing's
/// dependents are, and live again when it is turned on.
#[test]
fn the_filler_editor_follows_the_filter() {
    let window = open_tall(&layout("filler-grey", None));
    sub_tab(&window, "Words");
    let state = window.global::<TextState>();
    state.set_filler_filter(false);
    assert_eq!(
        editor(&window, "Filler words").accessible_enabled(),
        Some(false)
    );
    state.set_filler_filter(true);
    assert_eq!(
        editor(&window, "Filler words").accessible_enabled(),
        Some(true)
    );
}

/// Owner item 38: a modal popup dims the window behind it, the dim fades with the popup's motion
/// and is gone when the popup closes (Esc here; a click outside shares the `is-open` path).
#[test]
fn apps_popup_dims_the_window_while_open() {
    let window = open_tall(&layout("scrim", None));
    sub_tab(&window, "Per-app rules");
    let shell = window.global::<oxi_settings::Shell>();
    assert_eq!(shell.get_modals(), 0, "dim before any popup");
    one(&window, "Scan for apps").invoke_accessible_default_action();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(250));
    assert_eq!(shell.get_modals(), 1, "the popup is open but no dim");
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Escape.into(),
    });
    window.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Escape.into(),
    });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(250));
    assert_eq!(
        shell.get_modals(),
        0,
        "the dim stays after the popup closed"
    );
}

/// RT R2, ia-owner: a rule that follows the Paste tab's mode names that mode in its Combo, and
/// follows a change of it.
#[test]
fn a_rule_mode_names_the_paste_mode_it_follows() {
    let l = layout(
        "rule-default",
        Some(
            "version = 2
[[paste.rules]]
app = \"synthapp.exe\"
",
        ),
    );
    let window = open_tall(&l);
    sub_tab(&window, "Per-app rules");
    let mode = || {
        with_role(&window, "Mode for synthapp.exe", AccessibleRole::Combobox)
            .accessible_value()
            .map(|v| v.to_string())
    };
    assert_eq!(mode().as_deref(), Some("Default (Clipboard)"));
    window.global::<TextState>().invoke_paste_mode_picked(1);
    window.global::<TextState>().set_paste_mode(1);
    assert_eq!(mode().as_deref(), Some("Default (Type)"));
}

/// d01 review: the Folder row's "!" goes with the last rule that wrote to a file, at once (no
/// reload), whether the rule is removed in the list or unticked in the scan popup.
#[test]
fn the_folder_warning_goes_with_the_last_rule_that_writes() {
    const NO_FOLDER: &str = "No folder chosen: nothing is written";
    let l = layout(
        "folder-rule-gone",
        Some(
            "version = 2
[[paste.rules]]
app = \"synthapp.exe\"
to_file = \"only\"
",
        ),
    );
    let window = open_tall(&l);
    sub_tab(&window, "Write to file");
    assert!(has(&window, NO_FOLDER));
    sub_tab(&window, "Per-app rules");
    one(&window, "Remove synthapp.exe").invoke_accessible_default_action();
    sub_tab(&window, "Write to file");
    assert!(!has(&window, NO_FOLDER), "no rule writes: no \"!\"");
}

/// ia-map S41, SPEC:39: the bar shows the share, the editor's "i" the count of word pieces.
#[test]
fn the_dictionary_tip_says_the_word_piece_count() {
    let l = layout("token-tip", None);
    fs::write(
        l.dictionary_file(),
        "synthone
synthtwo
",
    )
    .unwrap();
    let window = open_tall(&l);
    let s = window.global::<TextState>();
    assert!(s.get_tokens() > 0);
    let tip = format!("Now {} of {} word pieces.", s.get_tokens(), s.get_budget());
    let found = labels(
        &window,
        "Names and rare words Whisper should know, one per line. A",
    );
    assert!(found.iter().any(|l| l.ends_with(&tip)), "{found:?}");
}

/// d01 review: "Certainty needed" follows the config after a drag (a two-way binding): a value
/// changed in the file outside reaches the slider on the next look at the files.
#[test]
fn certainty_needed_follows_the_file_after_a_drag() {
    let l = layout(
        "certainty",
        Some(
            "version = 2
[text]
confidence_filter = true
",
        ),
    );
    let window = open_tall(&l);
    sub_tab(&window, "Formatting");
    let slider = || with_role(&window, "Certainty needed", AccessibleRole::Slider);
    let value = || -> f32 { slider().accessible_value().unwrap().parse().unwrap() };
    assert!((value() - 35.0).abs() < 0.5, "{}", value());
    slider().invoke_accessible_increment_action();
    assert!((value() - 40.0).abs() < 0.5, "{}", value());
    // The drag's save rests first, so the edit below is the file's last word.
    i_slint_backend_testing::mock_elapsed_time(Duration::from_secs(5));
    assert!((config(&l).config.text.confidence_min - 0.40).abs() < 0.001);
    fs::write(
        l.config_file(),
        "version = 2
[text]
confidence_filter = true
confidence_min = 0.6
",
    )
    .unwrap();
    window.invoke_files_check();
    assert!((value() - 60.0).abs() < 0.5, "{}", value());
}

#[test]
fn voice_phrases_are_saved_and_a_phrase_of_another_command_is_refused() {
    let l = layout("voice", None);
    let window = open_tall(&l);
    let s = window.global::<TextState>();
    let err = || -> Vec<String> {
        Model::iter(&s.get_voice_errors())
            .map(|e| e.to_string())
            .collect()
    };
    assert_eq!(s.get_voice_line_phrases(), "новая строка, new line");
    // Several phrases for one command; a repeat inside the field is left out.
    s.invoke_voice_phrases_edited(0, "enter, Next line, enter!".into());
    assert_eq!(err(), ["", "", ""]);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(350));
    let voice = config(&l).config.text.voice;
    assert_eq!(voice.new_line.phrases, ["enter", "Next line"]);
    // "new paragraph" belongs to New paragraph: the field says why, nothing is saved.
    s.set_voice_line_phrases("enter, New  Paragraph".into()); // what typing does
    s.invoke_voice_phrases_edited(0, "enter, New  Paragraph".into());
    assert!(err()[0].contains("New paragraph"));
    assert_eq!(s.get_voice_line_phrases(), "enter, New  Paragraph");
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(350));
    assert_eq!(
        config(&l).config.text.voice.new_line.phrases,
        voice.new_line.phrases
    );
    // Editing another field does not drop the refused text: it stays, with its error, unsaved.
    s.invoke_voice_phrases_edited(2, "delete that".into());
    assert!(err()[0].contains("New paragraph"), "{:?}", err());
    assert_eq!(s.get_voice_line_phrases(), "enter, New  Paragraph");
    // Removing the clash from the other command lets the kept text through.
    s.invoke_voice_phrases_edited(1, "new para".into());
    assert_eq!(err(), ["", "", ""]);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(350));
    let saved = config(&l).config.text.voice;
    assert_eq!(saved.new_line.phrases, ["enter", "New  Paragraph"]);
    assert_eq!(saved.new_paragraph.phrases, ["new para"]);
    // A fixed edit clears it; a switch is saved at once; Reset brings the phrases back.
    s.invoke_voice_phrases_edited(0, "enter".into());
    assert_eq!(err(), ["", "", ""]);
    s.invoke_voice_toggled(2, false);
    assert!(!config(&l).config.text.voice.delete_last.on);
    s.invoke_voice_reset();
    assert_eq!(s.get_voice_line_phrases(), "новая строка, new line");
    let voice = config(&l).config.text.voice;
    assert_eq!(voice.new_line.phrases, ["новая строка", "new line"]);
    assert!(!voice.delete_last.on, "Reset keeps the switches");
}

/// Six edited synthetic dictations, each with the word "Zorblat" doubted and the full stop taken
/// away by hand, in the history of `l`.
fn write_doubted_history(l: &Layout) {
    use oxi_core::history::{Meta, Previous, Status, Word};
    let day = l.history_dir().join("2026-11-13");
    fs::create_dir_all(&day).unwrap();
    for i in 0..6 {
        let stem = format!("1000{i:02}-000");
        let meta = Meta {
            original: Some(Previous {
                text: "Synthetic note.".into(),
                model: "turbo".into(),
                language: "en".into(),
                settings: None,
                newer: false,
                edited: false,
            }),
            words: vec![Word {
                w: " Zorblat".into(),
                p: 0.2,
                t0: None,
                t1: None,
            }],
            ..Meta::basic(
                &format!("2026-11-13/{stem}"),
                1000,
                Status::Done,
                "turbo",
                "en",
            )
        };
        fs::write(day.join(format!("{stem}.json")), meta.to_json().unwrap()).unwrap();
        fs::write(day.join(format!("{stem}.txt")), "Synthetic note").unwrap();
    }
}

/// Waits (mock time) for the history read of the open Text page to come back.
fn wait_for_suggestions(s: &TextState<'_>) {
    for _ in 0..200 {
        if !s.get_period_tip().is_empty() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(60));
    }
}

/// Hold item 41, candidates 6 and 7: edits that took the final full stop away and words the model
/// doubted, in a synthetic history, make a suggestion on the "No full stop at the end" row and a
/// word in the Dictionary's list. Nothing is written before Apply or Add.
#[test]
fn suggestions_from_the_history_change_nothing_until_apply_or_add() {
    let l = layout("learn", Some("version = 2\n"));
    write_doubted_history(&l);
    let window = open(&l, Page::Text);
    let s = window.global::<TextState>();
    // The read runs on its own thread; a timer takes the answer.
    for _ in 0..200 {
        if !s.get_period_tip().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(60));
    }
    assert_eq!(
        s.get_period_tip().as_str(),
        "You took away the full stop at the end in 6 of your last 6 edits. Turn this on?"
    );
    assert_eq!(s.get_doubted_count(), 1);
    assert!(
        !config(&l).config.text.no_final_period,
        "nothing changes unasked"
    );
    assert!(!l.dictionary_file().exists(), "nothing added unasked");

    s.invoke_suggestion(0, true);
    assert!(config(&l).config.text.no_final_period);
    assert!(s.get_no_final_period());
    assert_eq!(
        s.get_period_tip().as_str(),
        "",
        "applied: nothing left to suggest"
    );

    s.invoke_doubt_added(0);
    assert_eq!(fs::read_to_string(l.dictionary_file()).unwrap(), "Zorblat");
    assert_eq!(s.get_doubted_count(), 0, "in the dictionary now");
}

/// Review fix: "Not this one" and "Not now" stay after the window is opened again, the list says
/// what a word costs of the word-piece limit before Add, and a dictionary at the limit says so.
#[test]
fn dismissed_suggestions_stay_dismissed_and_the_list_shows_the_cost() {
    use oxi_core::history::{Meta, Status, Word};
    let l = layout(
        "learn-dismiss",
        Some(
            "version = 2
",
        ),
    );
    write_doubted_history(&l);
    // A second doubted word that is not dismissed: once it is listed again after the reopen, the
    // history read has answered, so what is missing then was kept dismissed (not read late).
    let day = l.history_dir().join("2026-11-12");
    fs::create_dir_all(&day).unwrap();
    for i in 0..3 {
        let stem = format!("1000{i:02}-000");
        let meta = Meta {
            words: vec![Word {
                w: " Quaxle".into(),
                p: 0.2,
                t0: None,
                t1: None,
            }],
            ..Meta::basic(
                &format!("2026-11-12/{stem}"),
                1000,
                Status::Done,
                "turbo",
                "en",
            )
        };
        fs::write(day.join(format!("{stem}.json")), meta.to_json().unwrap()).unwrap();
        fs::write(day.join(format!("{stem}.txt")), "Quaxle").unwrap();
    }
    let word = |s: &TextState<'_>, i| s.get_doubted().row_data(i).unwrap().word.to_string();
    {
        let window = open(&l, Page::Text);
        let s = window.global::<TextState>();
        wait_for_suggestions(&s);
        assert_eq!(s.get_doubted_count(), 2);
        let row = s.get_doubted().row_data(0).unwrap();
        assert!(
            row.cost.starts_with("Adds ") && row.cost.contains("of 223") && !row.over,
            "{}",
            row.cost
        );
        let zorblat = (0..2).find(|&i| word(&s, i) == "Zorblat").unwrap();
        s.invoke_suggestion(0, false);
        s.invoke_doubt_skipped(i32::try_from(zorblat).unwrap());
        assert_eq!(s.get_period_tip().as_str(), "");
        assert_eq!(s.get_doubted_count(), 1);
    }
    let window = open_again(&l, Page::Text);
    let s = window.global::<TextState>();
    for _ in 0..200 {
        if s.get_doubted_count() > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(60));
    }
    assert_eq!(s.get_doubted_count(), 1, "the read answered");
    assert_eq!(word(&s, 0), "Quaxle", "Not this one came back");
    assert_eq!(s.get_period_tip().as_str(), "", "Not now came back");
    assert!(
        fs::read_to_string(l.data_dir.join("learn-dismissed.txt"))
            .unwrap()
            .lines()
            .all(|k| k.len() == 16 && k.chars().all(|c| c.is_ascii_hexdigit())),
        "only hashed keys are kept"
    );
}

#[test]
fn a_full_dictionary_says_a_doubted_word_is_over_the_limit() {
    let l = layout("learn-full", Some("version = 2\n"));
    write_doubted_history(&l);
    fs::write(
        l.dictionary_file(),
        "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu\n".repeat(30),
    )
    .unwrap();
    let window = open(&l, Page::Text);
    let s = window.global::<TextState>();
    wait_for_suggestions(&s);
    let row = s.get_doubted().row_data(0).unwrap();
    assert!(
        row.over && row.cost.contains("over the limit"),
        "{}",
        row.cost
    );
}

/// ia-map 3.8 (RT R8): the unsure-word rows, grey while History saving is off, say where that
/// switch is, and the place they name exists.
#[test]
fn the_unsure_word_rows_name_where_history_saving_is() {
    let slint = include_str!("../ui/pages/text.slint");
    let group = &slint[slint
        .find("title: @tr(\"Unsure words\")")
        .expect("the group")..];
    let group = &group[..group.find("title: @tr(\"Certainty needed\")").unwrap() + 600];
    let named = group
        .matches("Turn on System > History and files > Save dictations to History")
        .count();
    assert_eq!(named, 2, "both rows name the switch: {named}");
    let window = open(&layout("unsure-off-tip", None), Page::System);
    with_role(&window, "History and files", AccessibleRole::Tab).invoke_accessible_default_action();
    with_role(
        &window,
        "Save dictations to History",
        AccessibleRole::Switch,
    );
}
