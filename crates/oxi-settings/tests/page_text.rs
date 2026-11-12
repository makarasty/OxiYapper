//! Text page under the testing backend, on a temporary Layout with synthetic files.

mod common;

use std::fmt::Write as _;
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

/// Opens the sub-tab `label` (Dictionary is open at first).
fn sub_tab(window: &AppWindow, label: &str) {
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
        Some("One word or name per line")
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

    window.set_page(Page::About);
    // The query builds the About page, so the Text page is really left.
    assert!(has(&window, "Version"));
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
    let slider = with_role(&window, "Restore your copy after", AccessibleRole::Spinbox);
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
    with_role(&window, "Restore your copy after", AccessibleRole::Spinbox)
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
            with_role(&window, "Restore your copy after", AccessibleRole::Slider),
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

    with_role(&window, "Trailing space", AccessibleRole::Switch).invoke_accessible_default_action();
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
    sub_tab(&window, "Processing");
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
    sub_tab(&window, "Paste");
    with_role(&window, "Type", AccessibleRole::RadioButton).invoke_accessible_default_action();
    assert_eq!(config(&l).config.paste.mode, PasteMode::Type);
    sub_tab(&window, "Processing");
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
    let at = with_role(window, "Dictionary", AccessibleRole::Tab).absolute_position();
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
    let window = common::open_again(&layout("rule-names-default", None), Page::Text);
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
/// the offline daemon's and a broken config's.
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
            ("Paste", "Restore your copy after"),
            ("Per-app rules", "Scan for apps"),
            ("Processing", "Doubt level"),
            ("Words", "Snippets"),
        ] {
            sub_tab(&window, tab);
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
            assert!(
                whole_in_view(&window, last),
                "{last} is cut on {tab} (banner {banner:?}, daemon {daemon:?})"
            );
            if tab == "Words" {
                // The label also names the row above the editor: the editor itself must be whole.
                let e = editor(&window, "Snippets");
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
                "TextPage::card",
            )
            .next()
            .expect("the rules card");
            (card.absolute_position(), card.size())
        };
        let list = card();
        one(&window, "Profile of synthapp0.exe").invoke_accessible_default_action();
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        assert!(has(&window, "Close the profile"), "the profile is open");
        assert_eq!(card(), list, "the card moved or changed size at {w}x{h}");
        if h > 520.0 {
            let bottom = i_slint_backend_testing::ElementHandle::find_by_accessible_label(
                &window,
                "Pause punctuation",
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
