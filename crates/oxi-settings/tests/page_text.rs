//! Text page under the testing backend, on a temporary Layout with synthetic files.

mod common;

use std::fs;

use common::layout;
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use oxi_core::config::{ConfigFile, PasteMode};
use oxi_core::dictionary::{BUDGET, build_prompt};
use oxi_core::paths::Layout;
use oxi_settings::{AppWindow, Page};
use slint::ComponentHandle;

fn open(layout: &Layout) -> AppWindow {
    i_slint_backend_testing::init_no_event_loop();
    let window = oxi_settings::open(layout.clone()).unwrap();
    window.show().unwrap();
    window.set_page(Page::Text);
    window
}

fn one(window: &AppWindow, label: &str) -> ElementHandle {
    let mut found = ElementHandle::find_by_accessible_label(window, label);
    let first = found
        .next()
        .unwrap_or_else(|| panic!("no element labelled {label}"));
    assert!(found.next().is_none(), "more than one labelled {label}");
    first
}

/// The element labelled `label` with `role` (a row's text and its control share a label).
fn with_role(window: &AppWindow, label: &str, role: AccessibleRole) -> ElementHandle {
    ElementHandle::find_by_accessible_label(window, label)
        .find(|e| e.accessible_role() == Some(role))
        .unwrap_or_else(|| panic!("no {role:?} labelled {label}"))
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

fn has(window: &AppWindow, label: &str) -> bool {
    ElementHandle::find_by_accessible_label(window, label)
        .next()
        .is_some()
}

fn config(l: &Layout) -> ConfigFile {
    ConfigFile::parse(&fs::read_to_string(l.config_file()).unwrap()).unwrap()
}

#[test]
fn over_budget_dictionary_marks_the_tail() {
    let words: Vec<String> = (1..=300).map(|i| format!("synthword{i}")).collect();
    let l = layout("budget", None);
    fs::write(l.dictionary_file(), words.join("\n")).unwrap();
    let window = open(&l);

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
    one(&window, "Dictionary").set_accessible_value(words[..10].join("\n"));
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
    let window = open(&l);
    assert!(has(&window, "Every line has =>."));
    one(&window, "Replacements").set_accessible_value("alpha => beta\nfoo bar\n\ngamma => delta");
    assert!(
        has(&window, "Ignored, no =>: line 2"),
        "{:?}",
        labels(&window, "Ignored")
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
    let window = open(&l);
    one(&window, "Dictionary").set_accessible_value("synthalpha\nsynthbeta");
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
    let window = open(&l);
    let note = "Saved as UTF-16 or with a BOM: Save rewrites it as UTF-8.";
    assert!(has(&window, note));
    assert_eq!(
        one(&window, "Dictionary").accessible_value().unwrap(),
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
    let window = open(&l);
    assert!(has(
        &window,
        "Not a UTF-8 or UTF-16 text file: fix it in a text editor."
    ));
    let save = one(&window, "Save replace.txt");
    assert_eq!(save.accessible_enabled(), Some(false));
    save.invoke_accessible_default_action();
    assert_eq!(fs::read(l.replace_file()).unwrap(), bytes);
}

#[test]
fn mode_thumb_and_rules_write_the_config() {
    let l = layout("paste", Some("version = 1\n# mine\n"));
    let window = open(&l);
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
    with_role(&window, "Restore clipboard after", AccessibleRole::Slider)
        .set_accessible_value("500");
    assert_eq!(config(&l).config.paste.restore_delay_ms, 500);
    assert!(has(&window, "500 ms"));

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
    let window = open(&l);
    let add = one(&window, "Add rule");
    assert_eq!(add.accessible_enabled(), Some(false));
    assert_eq!(
        one(&window, "Remove mstsc.exe").accessible_enabled(),
        Some(false)
    );
    assert_eq!(fs::read_to_string(l.config_file()).unwrap(), original);
}
