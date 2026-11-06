//! Text page: `dictionary.txt` against the prompt budget, `replace.txt` with its ignored lines,
//! and the `[paste]` config (mode, trailing space, restore delay, per-app rules).

use std::io;
use std::path::Path;
use std::rc::Rc;

use oxi_core::config::{Paste, PasteMode, PasteRule};
use oxi_core::dictionary::{BUDGET, build_prompt, token_count};
use oxi_core::text::{ReplaceRules, decode_text_file};
use slint::{ComponentHandle, ModelRc, VecModel, Weak};

use crate::generated::{FileState, PasteRuleRow, TextState};
use crate::{AppWindow, Ctx, Error, report};

pub fn bind(window: &AppWindow, ctx: &Ctx) {
    let s = window.global::<TextState>();
    s.set_budget(int(BUDGET));
    bind_paste(window, ctx);

    let (text, state) = load(&ctx.layout.dictionary_file());
    s.set_dictionary(text.clone().into());
    s.set_dictionary_state(state);
    show_dictionary(&s, &text);
    let (text, state) = load(&ctx.layout.replace_file());
    s.set_replacements(text.clone().into());
    s.set_replacements_state(state);
    show_replacements(&s, &text);

    let weak = window.as_weak();
    s.on_dictionary_edited(move |text| with(&weak, |s| show_dictionary(&s, &text)));
    let weak = window.as_weak();
    s.on_replacements_edited(move |text| with(&weak, |s| show_replacements(&s, &text)));

    let weak = window.as_weak();
    let path = ctx.layout.dictionary_file();
    s.on_save_dictionary(move || {
        with(&weak, |s| {
            if save(&weak, &path, &s.get_dictionary(), s.get_dictionary_state()) {
                s.set_dictionary_state(FileState::Ok);
            }
        });
    });
    let weak = window.as_weak();
    let path = ctx.layout.replace_file();
    s.on_save_replacements(move || {
        with(&weak, |s| {
            if save(
                &weak,
                &path,
                &s.get_replacements(),
                s.get_replacements_state(),
            ) {
                s.set_replacements_state(FileState::Ok);
            }
        });
    });
    let weak = window.as_weak();
    let path = ctx.layout.dictionary_file();
    s.on_open_dictionary(move || open(&weak, &path));
    let weak = window.as_weak();
    let path = ctx.layout.replace_file();
    s.on_open_replacements(move || open(&weak, &path));
}

fn bind_paste(window: &AppWindow, ctx: &Ctx) {
    let s = window.global::<TextState>();
    let file = ctx.config.borrow();
    let paste = &file.config.paste;
    s.set_paste_mode(i32::from(paste.mode == PasteMode::Type));
    s.set_trailing_space(paste.trailing_space);
    #[allow(
        clippy::cast_precision_loss,
        reason = "a delay in ms is far below 2^24"
    )]
    let ms = paste.restore_delay_ms as f32;
    s.set_restore_delay_ms(ms);
    s.set_rules(rows(&paste.rules));
    drop(file);

    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_paste_mode_picked(move |i| {
        let mode = if i == 1 {
            PasteMode::Type
        } else {
            PasteMode::Clipboard
        };
        apply(&w, &c, |p| p.mode = mode);
    });
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_trailing_space_toggled(move |on| apply(&w, &c, |p| p.trailing_space = on));
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_restore_delay_changed(move |ms| {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to the slider's 0..=1000 first"
        )]
        let ms = ms.round().clamp(0.0, 1000.0) as u32;
        apply(&w, &c, |p| p.restore_delay_ms = ms);
    });
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_rule_mode_picked(move |i, mode| {
        let mode = match mode {
            1 => Some(PasteMode::Clipboard),
            2 => Some(PasteMode::Type),
            _ => None,
        };
        apply(&w, &c, |p| {
            if let Some(rule) = usize::try_from(i).ok().and_then(|i| p.rules.get_mut(i)) {
                rule.mode = mode;
            }
        });
    });
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_rule_removed(move |i| {
        apply(&w, &c, |p| {
            if let Ok(i) = usize::try_from(i)
                && i < p.rules.len()
            {
                p.rules.remove(i);
            }
        });
    });
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_rule_added(move |app| apply(&w, &c, |p| add_rule(p, app.trim())));
}

/// A new rule types into `app`. The daemon takes the first rule whose app matches
/// case-insensitively, so a second rule for the same app is not added: it would never apply.
fn add_rule(paste: &mut Paste, app: &str) {
    let taken = paste
        .rules
        .iter()
        .any(|r| r.app.to_lowercase() == app.to_lowercase());
    if !app.is_empty() && !taken {
        paste.rules.push(PasteRule {
            app: app.to_owned(),
            mode: Some(PasteMode::Type),
            chord: None,
            restore_delay_ms: None,
        });
    }
}

/// Change `[paste]`, show its rules again and save.
fn apply(weak: &Weak<AppWindow>, ctx: &Ctx, change: impl FnOnce(&mut Paste)) {
    change(&mut ctx.config.borrow_mut().config.paste);
    with(weak, |s| {
        s.set_rules(rows(&ctx.config.borrow().config.paste.rules));
    });
    report(weak, ctx.save());
}

fn rows(rules: &[PasteRule]) -> ModelRc<PasteRuleRow> {
    let rows: Vec<PasteRuleRow> = rules
        .iter()
        .map(|r| PasteRuleRow {
            app: r.app.as_str().into(),
            mode: match r.mode {
                None => 0,
                Some(PasteMode::Clipboard) => 1,
                Some(PasteMode::Type) => 2,
            },
        })
        .collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

fn show_dictionary(s: &TextState<'_>, text: &str) {
    let lines: Vec<&str> = text.lines().collect();
    let prompt = build_prompt(lines.iter().copied());
    let all: Vec<&str> = lines
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    s.set_tokens(int(token_count(&all.join(", "))));
    s.set_dropped_from(prompt.dropped.first().map_or(0, |&i| int(i + 1)));
}

fn show_replacements(s: &TextState<'_>, text: &str) {
    let bad: Vec<String> = ReplaceRules::parse(text)
        .bad_lines
        .iter()
        .map(ToString::to_string)
        .collect();
    s.set_bad_lines(bad.join(", ").into());
}

/// The file's text and how it was read; a missing file is an empty one.
fn load(path: &Path) -> (String, FileState) {
    match std::fs::read(path) {
        Ok(bytes) => match decode_text_file(&bytes) {
            Ok(text) if text.as_bytes() == bytes => (text, FileState::Ok),
            Ok(text) => (text, FileState::Converted),
            Err(_) => (String::new(), FileState::Unreadable),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => (String::new(), FileState::Ok),
        Err(_) => (String::new(), FileState::Unreadable),
    }
}

/// Write `text` over `path`; true when it was written. A file that could not be read is never
/// written: the editor holds none of it, so saving would wipe it.
fn save(weak: &Weak<AppWindow>, path: &Path, text: &str, state: FileState) -> bool {
    if state == FileState::Unreadable {
        return false;
    }
    let result = oxi_platform::fs::write_atomic(path, text.as_bytes());
    let written = result.is_ok();
    report(weak, result.map_err(Error::from));
    written
}

fn open(weak: &Weak<AppWindow>, path: &Path) {
    report(weak, oxi_platform::open::open(path).map_err(Error::from));
}

fn with(weak: &Weak<AppWindow>, f: impl FnOnce(TextState<'_>)) {
    if let Some(window) = weak.upgrade() {
        f(window.global::<TextState>());
    }
}

fn int(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}
