use std::fmt::Write as _;
use std::path::Path;

use super::{ENTRIES, Entry, Kind, find, fold, place, rank};
use crate::Page;

/// The page files that hold settings, and the page each belongs to (About is System's fourth tab).
/// Home, History and Test hold no settings (ia-map 1: "Not settings").
const FILES: [(&str, Page, Option<i32>); 8] = [
    ("audio", Page::Audio, None),
    ("hotkey", Page::Hotkey, None),
    ("model", Page::Model, None),
    ("overlay", Page::Overlay, None),
    ("paste", Page::Paste, None),
    ("system", Page::System, None),
    ("text", Page::Text, None),
    ("about", Page::System, Some(3)),
];

/// Titles on those pages that are no setting: the confirm line under "Check for updates".
const NOT_SETTINGS: [&str; 1] = ["Update checks tell you when a fix is out."];

/// A `Row`, `ToggleRow`, `SliderRow` or `Group` as written in a page file.
#[derive(Debug)]
struct Written {
    kind: String,
    tab: i32,
    title: String,
    tip: String,
    /// `search-key` when the block sets it.
    key: Option<String>,
    line: usize,
}

struct Frame {
    kind: String,
    tab: Option<i32>,
    props: Vec<String>,
    line: usize,
}

/// The sub-tab a condition names: `X.tab == 2`, `TextState.paste-tab == 1`, `AudioState.view >= 2`.
fn tab_in(text: &str) -> Option<i32> {
    ["tab == ", "view == ", "view >= "].iter().find_map(|cue| {
        let at = text.find(cue)? + cue.len();
        let digits: String = text[at..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()
    })
}

/// The first `@tr("...")` literal of `value`, as written (escapes kept).
fn tr_literal(value: &str) -> Option<String> {
    let start = value.find("@tr(\"")? + 5;
    let mut out = String::new();
    let mut escaped = false;
    for c in value[start..].chars() {
        match c {
            '"' if !escaped => return Some(out),
            '\\' if !escaped => escaped = true,
            _ => escaped = false,
        }
        out.push(c);
    }
    None
}

fn prop<'a>(props: &'a [String], name: &str) -> Option<&'a str> {
    props
        .iter()
        .find_map(|p| p.strip_prefix(name)?.trim_start().strip_prefix(':'))
}

/// Every row and group block of a `.slint` source with the sub-tab it sits on. A small scanner:
/// strings and `//` comments are skipped, `{` opens a block named by the last word before it,
/// `;` ends a property of the innermost block.
fn written(src: &str, fixed_tab: Option<i32>) -> Vec<Written> {
    let mut found = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut stmt = String::new();
    let (mut line, mut in_string, mut escaped) = (1, false, false);
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\n' {
            line += 1;
        }
        if in_string {
            stmt.push(c);
            match c {
                '"' if !escaped => in_string = false,
                '\\' if !escaped => escaped = true,
                _ => escaped = false,
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                stmt.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        line += 1;
                        break;
                    }
                }
                stmt.push(' ');
            }
            '{' => {
                let header = stmt.trim();
                let kind = header
                    .rsplit(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
                    .find(|w| !w.is_empty())
                    .unwrap_or_default()
                    .to_owned();
                let tab = fixed_tab
                    .or_else(|| tab_in(header))
                    .or_else(|| stack.last().and_then(|f| f.tab));
                stack.push(Frame {
                    kind,
                    tab,
                    props: Vec::new(),
                    line,
                });
                stmt.clear();
            }
            ';' => {
                let text = stmt.split_whitespace().collect::<Vec<_>>().join(" ");
                if let Some(frame) = stack.last_mut() {
                    if text.starts_with("shown:") && fixed_tab.is_none() {
                        frame.tab = tab_in(&text).or(frame.tab);
                    }
                    frame.props.push(text);
                }
                stmt.clear();
            }
            '}' => {
                stmt.clear();
                let Some(frame) = stack.pop() else { continue };
                if !matches!(
                    frame.kind.as_str(),
                    "Row" | "ToggleRow" | "SliderRow" | "Group"
                ) {
                    continue;
                }
                let Some(title) = prop(&frame.props, "title").and_then(tr_literal) else {
                    continue;
                };
                found.push(Written {
                    kind: frame.kind,
                    tab: frame.tab.unwrap_or(0),
                    title,
                    tip: prop(&frame.props, "info")
                        .and_then(tr_literal)
                        .unwrap_or_default(),
                    key: prop(&frame.props, "search-key")
                        .map(|v| v.trim().trim_matches('"').to_owned()),
                    line: frame.line,
                });
            }
            _ => stmt.push(c),
        }
    }
    found
}

fn source(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("ui/{name}.slint"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The tab names a page's `SubTabs { items: [...] }` lists, in order.
fn tab_names(src: &str) -> Vec<String> {
    let Some(at) = src.find("SubTabs {") else {
        return Vec::new();
    };
    let items = &src[at..];
    let items = &items[items.find("items: [").expect("SubTabs without items")..];
    let items = &items[..items.find(']').expect("items end")];
    items
        .split("@tr(")
        .skip(1)
        .filter_map(|t| tr_literal(&format!("@tr({t}")))
        .collect()
}

fn rust_line(page: Page, w: &Written) -> String {
    let maker = if w.key.as_deref() == Some("") {
        "inner"
    } else {
        "row"
    };
    // The tip as written: raw, so the `.slint` escapes stay; hashes only around a quote.
    let tip = match (w.tip.is_empty(), w.tip.contains('"')) {
        (true, _) => "\"\"".to_owned(),
        (false, false) => format!("r\"{}\"", w.tip),
        (false, true) => format!("r#\"{}\"#", w.tip),
    };
    format!(
        "    {maker}({page:?}, {}, \"{}\", {tip}, \"\"),",
        w.tab, w.title
    )
}

#[test]
fn every_row_of_the_pages_is_in_the_index() {
    let mut missing = String::new();
    let mut wrong = Vec::new();
    let mut seen: Vec<(Page, i32, String)> = Vec::new();
    for (file, page, fixed_tab) in FILES {
        let src = source(&format!("pages/{file}"));
        let tabs = tab_names(&src);
        for w in written(&src, fixed_tab) {
            if NOT_SETTINGS.contains(&w.title.as_str()) {
                continue;
            }
            // A group named as its tab is the tab's own entry.
            let tab_name = if fixed_tab.is_some() {
                None
            } else {
                tabs.get(usize::try_from(w.tab).unwrap_or(0))
            };
            if w.kind == "Group" && tab_name == Some(&w.title) {
                continue;
            }
            seen.push((page, w.tab, w.title.clone()));
            // One title can sit on two tabs of a page (Typing speed: Paste, and the app profile).
            let Some(entry) = ENTRIES.iter().find(|e| {
                e.kind == Kind::Row && e.page == page && e.tab == w.tab && e.title == w.title
            }) else {
                let _ = writeln!(missing, "{}", rust_line(page, &w));
                continue;
            };
            let key = w.key.clone().unwrap_or_else(|| w.title.clone());
            if entry.tip != w.tip || entry.key != key {
                wrong.push(format!(
                    "{file}.slint:{} \"{}\": the index has key \"{}\" tip \"{}\"; the page says\n{}",
                    w.line,
                    w.title,
                    entry.key,
                    entry.tip,
                    rust_line(page, &w)
                ));
            }
        }
    }
    assert!(
        missing.is_empty() && wrong.is_empty(),
        "settings search index (src/search.rs ENTRIES) is out of date with ui/pages:\nmissing:\n{missing}\nwrong:\n{}",
        wrong.join("\n")
    );
    // No entry for a row that is gone.
    for e in ENTRIES.iter().filter(|e| e.kind == Kind::Row) {
        assert!(
            seen.contains(&(e.page, e.tab, e.title.to_owned())),
            "{:?} tab {} \"{}\" is in the index but on no page (or on another tab)",
            e.page,
            e.tab,
            e.title
        );
    }
}

#[test]
fn a_search_key_flashes_one_row_per_page() {
    for (file, page, fixed_tab) in FILES {
        let mut keys: Vec<String> = written(&source(&format!("pages/{file}")), fixed_tab)
            .into_iter()
            .map(|w| w.key.unwrap_or(w.title))
            .filter(|k| !k.is_empty())
            .collect();
        keys.sort();
        let before = keys.len();
        keys.dedup();
        assert_eq!(
            before,
            keys.len(),
            "{page:?} ({file}.slint) has two rows with one search-key"
        );
    }
}

#[test]
fn tabs_and_pages_are_named_as_the_window_names_them() {
    let app = source("app");
    for (file, page, fixed_tab) in FILES.iter().filter(|f| f.2.is_none()) {
        let _ = fixed_tab;
        let names = tab_names(&source(&format!("pages/{file}")));
        let indexed: Vec<&str> = ENTRIES
            .iter()
            .filter(|e| e.kind == Kind::Tab && e.page == *page)
            .map(|e| e.title)
            .collect();
        assert_eq!(indexed, names, "{file}.slint tabs");
        for (i, e) in ENTRIES
            .iter()
            .filter(|e| e.kind == Kind::Tab && e.page == *page)
            .enumerate()
        {
            assert_eq!(
                usize::try_from(e.tab).ok(),
                Some(i),
                "{:?} tab \"{}\"",
                e.page,
                e.title
            );
        }
    }
    for e in ENTRIES.iter().filter(|e| e.kind == Kind::Page) {
        let name = format!("{:?}", e.page).to_lowercase();
        let rail = format!("page: Page.{name}, label: @tr(\"{}\")", e.title);
        assert!(app.contains(&rail), "app.slint rail has no {rail}");
    }
}

fn titles(query: &str) -> Vec<&'static str> {
    find(query).into_iter().map(|i| ENTRIES[i].title).collect()
}

fn entry(title: &str) -> &'static Entry {
    ENTRIES.iter().find(|e| e.title == title).expect(title)
}

#[test]
fn old_names_find_the_new_places() {
    // "Advanced" was split into Speed and Accuracy (ia-map 3.7, RT R9).
    let advanced = titles("Advanced");
    assert!(
        advanced.contains(&"Speed") && advanced.contains(&"Accuracy"),
        "{advanced:?}"
    );
    for (old, new) in [
        ("Unload after idle", "Free memory after"),
        ("Model by language", "Model for each language"),
        ("Normalise", "Even out loudness"),
        ("doubt level", "Certainty needed"),
        ("snippets", "Phrase shortcuts"),
        ("processing", "Formatting"),
        ("pill", "Show the overlay"),
        ("GPU", "Run the model on"),
        ("graphics card", "Run the model on"),
        ("misheard", "Replacements"),
        ("version", "About"),
        ("save dictations", "Save dictations to History"),
    ] {
        assert!(
            titles(old).contains(&new),
            "\"{old}\" should find \"{new}\": {:?}",
            titles(old)
        );
    }
}

#[test]
fn titles_rank_over_tabs_and_tips_and_case_and_accents_do_not_matter() {
    // "Speed boost" (title) before the Speed tab (a tab name) before rows whose tip says "speed".
    let speed = titles("speed");
    let at = |t: &str| speed.iter().position(|s| *s == t).unwrap_or(usize::MAX);
    assert!(at("Speed boost") < at("Speed"), "{speed:?}");
    assert_eq!(rank(entry("Speed boost"), &["speed"]), Some(0));
    assert_eq!(rank(entry("Speed"), &["speed"]), Some(1));
    assert_eq!(rank(entry("Free memory after"), &["idle"]), Some(1));
    // "fan" is only in the tip of Reduce background noise.
    assert_eq!(rank(entry("Reduce background noise"), &["fan"]), Some(2));
    let fan = titles("background fan");
    assert_eq!(fan.first(), Some(&"Reduce background noise"), "{fan:?}");
    assert_eq!(
        titles("MICROPHONE BOOST").first(),
        Some(&"Microphone boost")
    );
    assert_eq!(
        titles("mïcrophoné boost").first(),
        Some(&"Microphone boost")
    );
    assert_eq!(fold("Ёлка Café {} \\\"x\\\""), "елка cafe  \"x\"");
    assert_eq!(titles("zzqx nothing"), Vec::<&str>::new());
}

#[test]
fn an_empty_query_lists_the_pages_and_tabs() {
    let all = find("");
    assert_ne!(all, Vec::<usize>::new());
    assert!(all.iter().all(|&i| ENTRIES[i].kind != Kind::Row));
}

#[test]
fn a_match_says_where_it_is() {
    assert_eq!(place(entry("Microphone boost")), "Audio > Microphone");
    assert_eq!(place(entry("Accuracy")), "Model");
    assert_eq!(place(entry("Show the overlay")), "Overlay");
    assert_eq!(place(entry("Built with")), "System > About");
}
