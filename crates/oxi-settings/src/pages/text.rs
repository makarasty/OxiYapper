//! Text page: `dictionary.txt` against the prompt budget, `replace.txt` with its ignored lines,
//! and the `[paste]` config (mode, trailing space, restore delay, per-app rules).

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::{fs, io};

use oxi_core::config::{Paste, PasteMode, PasteRule};
use oxi_core::dictionary::{BUDGET, build_prompt, token_count};
use oxi_core::paths::Layout;
use oxi_core::strings;
use oxi_core::text::{ReplaceRules, decode_text_file};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

use crate::debounce::SaveDebounce;
use crate::generated::{FileInfo, FileState, PasteRuleRow, TextFile, TextState};
use crate::{AppWindow, Ctx, Error, report};

const FILES: [TextFile; 2] = [TextFile::Dictionary, TextFile::Replacements];

/// Runs again after Reset to defaults: an editor with unsaved edits keeps them, the rest reload.
// shortcut: closing the window drops unsaved edits without asking; add a close confirm when the
// shell gets a close hook.
pub fn bind(window: &AppWindow, ctx: &Ctx) {
    let s = window.global::<TextState>();
    s.set_budget(int(BUDGET));
    bind_paste(window, ctx);
    for file in FILES {
        refresh(&s, &ctx.layout, file);
    }

    let (w, l) = (window.as_weak(), ctx.layout.clone());
    s.on_shown(move || {
        with(&w, |s| {
            for file in FILES {
                refresh(&s, &l, file);
            }
        });
    });
    let w = window.as_weak();
    s.on_edited(move |file, text| {
        with(&w, |s| {
            show(&s, file, &text);
            let mut info = info(&s, file);
            info.dirty = true;
            set_info(&s, file, info);
        });
    });
    let (w, l) = (window.as_weak(), ctx.layout.clone());
    s.on_save(move |file| save(&w, &l, file, false));
    let (w, l) = (window.as_weak(), ctx.layout.clone());
    s.on_overwrite(move |file| save(&w, &l, file, true));
    let (w, l) = (window.as_weak(), ctx.layout.clone());
    s.on_reload(move |file| with(&w, |s| reload(&s, &l, file)));
    let (w, l) = (window.as_weak(), ctx.layout.clone());
    s.on_open(move |file| {
        report(
            &w,
            oxi_platform::open::open(&path(&l, file)).map_err(Error::from),
        );
    });
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
    #[allow(
        clippy::cast_precision_loss,
        reason = "a delay in ms is far below 2^24"
    )]
    s.set_default_restore_delay_ms(Paste::default().restore_delay_ms as f32);
    s.set_rules(rows(&paste.rules));
    s.set_duplicate_app(SharedString::new());
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
    let save = SaveDebounce::new(w, c.clone());
    s.on_restore_delay_changed(move |ms| {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to the slider's 0..=1000 first"
        )]
        let ms = ms.round().clamp(0.0, 1000.0) as u32;
        c.config.borrow_mut().config.paste.restore_delay_ms = ms;
        save.touch();
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
        with(&w, |s| s.set_duplicate_app(SharedString::new()));
        apply(&w, &c, |p| {
            if let Ok(i) = usize::try_from(i)
                && i < p.rules.len()
            {
                p.rules.remove(i);
            }
        });
    });
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_rule_added(move |app| {
        let app = app.trim();
        let mut added = false;
        apply(&w, &c, |p| added = add_rule(p, app));
        let duplicate = if added { "" } else { app };
        with(&w, |s| s.set_duplicate_app(duplicate.into()));
    });
}

/// A new rule types into `app`. The daemon takes the first rule whose app matches
/// case-insensitively, so a second rule for the same app is refused (false): it would never apply.
fn add_rule(paste: &mut Paste, app: &str) -> bool {
    let taken = paste
        .rules
        .iter()
        .any(|r| r.app.to_lowercase() == app.to_lowercase());
    if taken {
        return false;
    }
    if !app.is_empty() {
        paste.rules.push(PasteRule {
            app: app.to_owned(),
            mode: Some(PasteMode::Type),
            chord: None,
            restore_delay_ms: None,
        });
    }
    true
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
    s.set_bad_lines(strings::replace_bad_lines(&ReplaceRules::parse(text).bad_lines).into());
}

/// Read the file into the editor unless it has unsaved edits; then only flag a change on disk.
fn refresh(s: &TextState<'_>, layout: &Layout, file: TextFile) {
    let mut info = info(s, file);
    if info.dirty {
        flag_disk(&mut info, &fs::read(path(layout, file)));
        set_info(s, file, info);
    } else {
        reload(s, layout, file);
    }
}

/// A dirty editor against the disk: `conflict` when the file is no longer what the editor was
/// loaded from, and `error` naming the read error when the file cannot be read now.
fn flag_disk(info: &mut FileInfo, read: &io::Result<Vec<u8>>) {
    info.conflict = stamp(read) != info.base;
    info.error = match read {
        Err(e) if e.kind() != io::ErrorKind::NotFound => e.kind().to_string().into(),
        _ => SharedString::new(),
    };
}

/// Read the file into the editor, dropping any edits.
fn reload(s: &TextState<'_>, layout: &Layout, file: TextFile) {
    let (text, info) = load(&path(layout, file));
    match file {
        TextFile::Dictionary => s.set_dictionary(text.as_str().into()),
        TextFile::Replacements => s.set_replacements(text.as_str().into()),
    }
    set_info(s, file, info);
    show(s, file, &text);
}

/// Write the editor over the file. Without `force`, only while the disk still holds what the
/// editor was loaded from: a clean editor then takes the new file, a dirty one shows the conflict.
fn save(weak: &Weak<AppWindow>, layout: &Layout, file: TextFile, force: bool) {
    let Some(window) = weak.upgrade() else { return };
    let s = window.global::<TextState>();
    let path = path(layout, file);
    let mut info = info(&s, file);
    if !force {
        // The editor holds nothing of a file it could not read: saving would wipe that file.
        if matches!(info.state, FileState::NotText | FileState::Unreadable) {
            return;
        }
        let read = fs::read(&path);
        if stamp(&read) != info.base {
            if info.dirty {
                flag_disk(&mut info, &read);
                set_info(&s, file, info);
            } else {
                reload(&s, layout, file);
            }
            return;
        }
    }
    let text = match file {
        TextFile::Dictionary => s.get_dictionary(),
        TextFile::Replacements => s.get_replacements(),
    };
    let result = oxi_platform::fs::write_atomic(&path, text.as_bytes());
    if result.is_ok() {
        let info = FileInfo {
            state: FileState::Ok,
            base: stamp(&Ok(text.as_bytes().to_vec())),
            ..FileInfo::default()
        };
        set_info(&s, file, info);
    }
    report(weak, result.map_err(Error::from));
}

/// The file's text and how it was read; a missing file is an empty one.
fn load(path: &Path) -> (String, FileInfo) {
    let read = fs::read(path);
    let base = stamp(&read);
    let (text, state, error) = match read {
        Ok(bytes) => match decode_text_file(&bytes) {
            Ok(text) if text.as_bytes() == bytes => (text, FileState::Ok, String::new()),
            Ok(text) => (text, FileState::Converted, String::new()),
            Err(_) => (String::new(), FileState::NotText, String::new()),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            (String::new(), FileState::Ok, String::new())
        }
        Err(e) => (String::new(), FileState::Unreadable, e.kind().to_string()),
    };
    let info = FileInfo {
        state,
        error: error.into(),
        base,
        ..FileInfo::default()
    };
    (text, info)
}

/// What a read found, to tell whether the file changed since: "missing", the error kind, or
/// the length and a hash of the bytes.
fn stamp(read: &io::Result<Vec<u8>>) -> SharedString {
    match read {
        Ok(bytes) => {
            let mut hasher = DefaultHasher::new();
            bytes.hash(&mut hasher);
            format!("{}:{:016x}", bytes.len(), hasher.finish()).into()
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => "missing".into(),
        Err(e) => format!("error:{}", e.kind()).into(),
    }
}

fn path(layout: &Layout, file: TextFile) -> PathBuf {
    match file {
        TextFile::Dictionary => layout.dictionary_file(),
        TextFile::Replacements => layout.replace_file(),
    }
}

fn info(s: &TextState<'_>, file: TextFile) -> FileInfo {
    match file {
        TextFile::Dictionary => s.get_dictionary_info(),
        TextFile::Replacements => s.get_replacements_info(),
    }
}

fn set_info(s: &TextState<'_>, file: TextFile, info: FileInfo) {
    match file {
        TextFile::Dictionary => s.set_dictionary_info(info),
        TextFile::Replacements => s.set_replacements_info(info),
    }
}

fn show(s: &TextState<'_>, file: TextFile, text: &str) {
    match file {
        TextFile::Dictionary => show_dictionary(s, text),
        TextFile::Replacements => show_replacements(s, text),
    }
}

fn with(weak: &Weak<AppWindow>, f: impl FnOnce(TextState<'_>)) {
    if let Some(window) = weak.upgrade() {
        f(window.global::<TextState>());
    }
}

fn int(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}
