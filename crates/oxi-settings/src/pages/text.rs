//! Text page: `dictionary.txt` against the prompt budget, `replace.txt` with its ignored lines,
//! and the `[paste]` config (mode, trailing space, Win+V history, restore delay, per-app rules,
//! with a scan for the installed apps: Start menu and desktop shortcuts, Store apps, the registry).

use std::collections::{BTreeMap, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;
use std::{fs, io, thread};

use oxi_core::config::{FileFormat, FileMode, Output, Paste, PasteMode, PasteRule, Text};
use oxi_core::dictionary::{BUDGET, build_prompt, token_count};
use oxi_core::languages::LANGUAGES;
use oxi_core::models::{self, MODELS, ModelStatus};
use oxi_core::paths::Layout;
use oxi_core::strings;
use oxi_core::text::{
    ReplaceRules, decode_text_file, fillers_from_lines, fillers_to_lines, snippets_from_lines,
    snippets_to_lines,
};
use oxi_core::types::Language;
use slint::{
    ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, Timer,
    TimerMode, VecModel, Weak,
};

use crate::debounce::SaveDebounce;
use crate::generated::{FileInfo, FileState, GalleryTile, PasteRuleRow, TextFile, TextState};
use crate::{AppWindow, Ctx, Error, report};

const FILES: [TextFile; 2] = [TextFile::Dictionary, TextFile::Replacements];

/// How often a running scan is asked for its answer.
const SCAN_POLL: Duration = Duration::from_millis(50);
/// How long a rule a duplicate pick points at shows the accent tint (DS-28.1).
const FLASH: Duration = Duration::from_millis(600);
/// Installed apps the search field lists at most; typing more narrows them.
const SUGGESTIONS: usize = 6;

/// An app the scan found; the icon is still pixels, as an `Image` cannot leave the UI thread.
#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "only the Windows scan finds apps")
)]
struct Found {
    name: String,
    exe: String,
    icon: Option<SharedPixelBuffer<Rgba8Pixel>>,
}

/// Runs again after Reset to defaults: an editor with unsaved edits keeps them, the rest reload.
// shortcut: closing the window drops unsaved edits without asking; add a close confirm when the
// shell gets a close hook.
pub fn bind(window: &AppWindow, ctx: &Ctx) {
    let s = window.global::<TextState>();
    s.set_budget(int(BUDGET));
    bind_paste(window, ctx);
    bind_processing(window, ctx);
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

/// Change `[text]` and save at once.
fn apply_text(weak: &Weak<AppWindow>, ctx: &Ctx, edit: impl FnOnce(&mut Text)) {
    edit(&mut ctx.config.borrow_mut().config.text);
    report(weak, ctx.save());
}

/// The Processing tab: `[text]`. Switches save at once; sliders and the two line editors save once
/// they rest, like the restore delay.
fn bind_processing(window: &AppWindow, ctx: &Ctx) {
    let s = window.global::<TextState>();
    let file = ctx.config.borrow();
    let text = &file.config.text;
    s.set_pause_punctuation(text.pause_punctuation);
    s.set_pause_sentence_ms(to_f32(text.pause_sentence_ms));
    s.set_pause_paragraph_ms(to_f32(text.pause_paragraph_ms));
    s.set_filler_filter(text.filler_filter);
    s.set_fillers(fillers_to_lines(&text.fillers).into());
    s.set_voice_commands(text.voice_commands);
    s.set_snippets(snippets_to_lines(&text.snippets).into());
    s.set_confidence_filter(text.confidence_filter);
    s.set_confidence_min(text.confidence_min);
    drop(file);

    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_pause_punctuation_toggled(move |on| apply_text(&w, &c, |t| t.pause_punctuation = on));
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_filler_filter_toggled(move |on| apply_text(&w, &c, |t| t.filler_filter = on));
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_voice_commands_toggled(move |on| apply_text(&w, &c, |t| t.voice_commands = on));
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_confidence_filter_toggled(move |on| apply_text(&w, &c, |t| t.confidence_filter = on));

    let (w, c) = (window.as_weak(), ctx.clone());
    let save = Rc::new(SaveDebounce::new(w, c.clone()));
    let rest = |save: &Rc<SaveDebounce>, c: &Ctx, edit: Box<dyn FnOnce(&mut Text)>| {
        edit(&mut c.config.borrow_mut().config.text);
        save.touch();
    };
    let (c2, s2) = (c.clone(), save.clone());
    s.on_pause_sentence_changed(move |ms| {
        let ms = to_u32(ms).clamp(
            *Text::PAUSE_SENTENCE_MS.start(),
            *Text::PAUSE_SENTENCE_MS.end(),
        );
        rest(&s2, &c2, Box::new(move |t| t.pause_sentence_ms = ms));
    });
    let (c2, s2) = (c.clone(), save.clone());
    s.on_pause_paragraph_changed(move |ms| {
        // 0 is off; anything else is at least the range's start.
        let ms = to_u32(ms);
        let ms = if ms == 0 {
            0
        } else {
            ms.clamp(
                *Text::PAUSE_PARAGRAPH_MS.start(),
                *Text::PAUSE_PARAGRAPH_MS.end(),
            )
        };
        rest(&s2, &c2, Box::new(move |t| t.pause_paragraph_ms = ms));
    });
    let (c2, s2) = (c.clone(), save.clone());
    s.on_confidence_min_changed(move |v| {
        let v = v.clamp(0.05, 0.95);
        rest(&s2, &c2, Box::new(move |t| t.confidence_min = v));
    });
    let (c2, s2) = (c.clone(), save.clone());
    s.on_fillers_edited(move |lines| {
        let fillers = fillers_from_lines(&lines);
        rest(&s2, &c2, Box::new(move |t| t.fillers = fillers));
    });
    s.on_snippets_edited(move |lines| {
        let snippets = snippets_from_lines(&lines);
        rest(&save, &c, Box::new(move |t| t.snippets = snippets));
    });
}

#[allow(
    clippy::cast_precision_loss,
    reason = "a delay in ms is far below 2^24"
)]
fn to_f32(ms: u32) -> f32 {
    ms as f32
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to 0..=10000 first"
)]
fn to_u32(v: f32) -> u32 {
    v.round().clamp(0.0, 10_000.0) as u32
}

fn bind_paste(window: &AppWindow, ctx: &Ctx) {
    let s = window.global::<TextState>();
    let file = ctx.config.borrow();
    let paste = &file.config.paste;
    s.set_paste_mode(i32::from(paste.mode == PasteMode::Type));
    s.set_trailing_space(paste.trailing_space);
    s.set_keep_history(paste.keep_in_clipboard_history);
    s.set_press_enter(paste.press_enter);
    s.set_join_console_lines(paste.join_console_lines);
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
    show_rules(&s, &paste.rules, &file.config.ui.app_names);
    drop(file);
    bind_scan(window, ctx);

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
    s.on_keep_history_toggled(move |on| apply(&w, &c, |p| p.keep_in_clipboard_history = on));
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_press_enter_toggled(move |on| apply(&w, &c, |p| p.press_enter = on));
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_join_console_lines_toggled(move |on| apply(&w, &c, |p| p.join_console_lines = on));
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
    // Arrowing through the Combo picks on every step: save once it rests, like the slider.
    let save = SaveDebounce::new(w.clone(), c.clone());
    s.on_rule_mode_picked(move |i, mode| {
        let mode = match mode {
            1 => Some(PasteMode::Clipboard),
            2 => Some(PasteMode::Type),
            _ => None,
        };
        change(&w, &c, |p| {
            if let Some(rule) = usize::try_from(i).ok().and_then(|i| p.rules.get_mut(i)) {
                rule.mode = mode;
            }
        });
        save.touch();
    });
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_rule_removed(move |i| {
        forget_name(&c, i);
        apply(&w, &c, |p| {
            if let Ok(i) = usize::try_from(i)
                && i < p.rules.len()
            {
                p.rules.remove(i);
            }
        });
    });
    bind_rules(window, ctx);
    bind_profiles(window, ctx);
}

/// The profile editor of a rule and the to-file settings. Profile fields in the order of
/// `TextState::profile_values`: language, model, to file, dictionary, replacements, voice
/// commands, filler words, pause punctuation, join lines.
fn bind_profiles(window: &AppWindow, ctx: &Ctx) {
    let s = window.global::<TextState>();
    s.set_profile_rule(-1);
    s.set_language_names(shared(language_names()));
    show_output(&s, &ctx.config.borrow().config.output);
    // What each row of the model Combo writes, as the open profile listed them.
    let listed: Rc<std::cell::RefCell<Vec<Option<String>>>> = Rc::default();

    let (w, c, shown) = (window.as_weak(), ctx.clone(), listed.clone());
    s.on_profile_opened(move |i| {
        let file = c.config.borrow();
        let Some(rule) = usize::try_from(i)
            .ok()
            .and_then(|i| file.config.paste.rules.get(i))
        else {
            return;
        };
        let dir = c.layout.models_dir();
        let ids = profile_models(&dir, &file.config.model.custom_path, rule.model.as_deref());
        let values = profile_values(rule, &ids);
        let names = ids
            .iter()
            .map(|id| model_name(id.as_deref(), &dir))
            .collect();
        *shown.borrow_mut() = ids;
        with(&w, |s| {
            s.set_model_names(shared(names));
            if let Some(row) = s
                .get_rules()
                .row_data(usize::try_from(i).unwrap_or_default())
            {
                s.set_profile_name(row.name);
                s.set_profile_icon(row.icon);
            }
            s.set_profile_app(rule.app.as_str().into());
            s.set_profile_values(ModelRc::from(Rc::new(VecModel::from(values))));
            s.set_profile_rule(i);
        });
    });
    let (w, c) = (window.as_weak(), ctx.clone());
    // Arrowing through a Combo picks on every step: save once it rests, like the mode Combo.
    let save = SaveDebounce::new(w.clone(), c.clone());
    s.on_profile_picked(move |field, index| {
        let Some(i) = w
            .upgrade()
            .and_then(|win| usize::try_from(win.global::<TextState>().get_profile_rule()).ok())
        else {
            return;
        };
        change(&w, &c, |p| {
            if let Some(rule) = p.rules.get_mut(i) {
                set_profile_field(rule, field, index, &listed.borrow());
            }
        });
        save.touch();
    });

    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_file_mode_picked(move |i| {
        c.config.borrow_mut().config.output.file_mode = match i {
            1 => FileMode::Also,
            2 => FileMode::Only,
            _ => FileMode::Off,
        };
        report(&w, c.save());
    });
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_file_format_picked(move |i| {
        c.config.borrow_mut().config.output.file_format = if i == 1 {
            FileFormat::Txt
        } else {
            FileFormat::Md
        };
        report(&w, c.save());
    });
    // The daemon's to-file warning comes through the Model page, which hears every Status
    // already (one subscriber per page).
    s.set_file_warning(SharedString::new());
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_file_folder_choose(move || {
        let current = c.config.borrow().config.output.file_folder.clone();
        let start = Some(PathBuf::from(&current))
            .filter(|dir| dir.is_dir())
            .unwrap_or_default();
        let Some(path) = pick_folder(&start) else {
            return;
        };
        c.config.borrow_mut().config.output.file_folder = path.to_string_lossy().into_owned();
        with(&w, |s| show_output(&s, &c.config.borrow().config.output));
        report(&w, c.save());
    });
}

/// The paste worker's to-file line among the daemon's warnings; empty when there is none.
pub(super) fn file_warning(warnings: &[String]) -> String {
    warnings
        .iter()
        .find(|w| w.starts_with(oxi_core::paste::file::WRITE_FAILED))
        .cloned()
        .unwrap_or_default()
}

#[cfg(windows)]
fn pick_folder(start: &Path) -> Option<PathBuf> {
    rfd::FileDialog::new().set_directory(start).pick_folder()
}

/// No native dialog on this OS: the folder is set in the file.
#[cfg(not(windows))]
fn pick_folder(_: &Path) -> Option<PathBuf> {
    None
}

fn show_output(s: &TextState<'_>, out: &Output) {
    s.set_file_mode(match out.file_mode {
        FileMode::Off => 0,
        FileMode::Also => 1,
        FileMode::Only => 2,
    });
    s.set_file_format(i32::from(out.file_format == FileFormat::Txt));
    s.set_file_folder(out.file_folder.as_str().into());
    let name = Path::new(&out.file_folder)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    s.set_file_folder_name(name.into());
}

fn shared(names: Vec<String>) -> ModelRc<SharedString> {
    ModelRc::from(Rc::new(VecModel::from(
        names
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    )))
}

/// "Default", "Auto", then every language of the table.
fn language_names() -> Vec<String> {
    ["Default", "Auto"]
        .into_iter()
        .map(String::from)
        .chain(LANGUAGES.iter().map(|l| l.english.to_owned()))
        .collect()
}

/// What each row of a rule's model Combo writes: Default, the downloaded models, the own file
/// when one is set, and the rule's model when it is none of these (shown, so it can be seen and
/// changed; the daemon falls back from it).
fn profile_models(dir: &Path, custom_path: &str, current: Option<&str>) -> Vec<Option<String>> {
    let mut ids: Vec<Option<String>> = std::iter::once(None)
        .chain(
            MODELS
                .iter()
                .filter(|m| downloaded(m, dir))
                .map(|m| Some(m.id.to_owned())),
        )
        .chain((!custom_path.trim().is_empty()).then(|| Some("custom".to_owned())))
        .collect();
    let current = current.map(str::to_owned);
    if current.is_some() && !ids.contains(&current) {
        ids.push(current);
    }
    ids
}

fn downloaded(m: &models::ModelEntry, dir: &Path) -> bool {
    matches!(
        models::status(m, dir),
        ModelStatus::Ready | ModelStatus::NeedsHash
    )
}

/// The Combo row of model `id` (`None` is Default); a registry model not on disk says so.
fn model_name(id: Option<&str>, dir: &Path) -> String {
    let missing = strings::model_badge(ModelStatus::Missing);
    match id {
        None => "Default".to_owned(),
        Some("custom") => "Own model file".to_owned(),
        Some(id) => match models::find(id) {
            Some(m) if downloaded(m, dir) => m.display_name.to_owned(),
            Some(m) => format!("{}: {missing}", m.display_name),
            None => format!("{id}: {missing}"),
        },
    }
}

fn language_at(i: i32) -> Option<Language> {
    match usize::try_from(i).ok()? {
        0 => None,
        1 => Some(Language::AUTO),
        n => LANGUAGES.get(n - 2).and_then(|l| l.code.parse().ok()),
    }
}

fn language_index(lang: Option<Language>) -> i32 {
    let Some(lang) = lang else { return 0 };
    if lang == Language::AUTO {
        return 1;
    }
    LANGUAGES
        .iter()
        .position(|l| l.code == lang.as_str())
        .map_or(0, |p| int(p + 2))
}

/// The model row `i` of `ids` writes; past the list is Default.
fn model_at(i: i32, ids: &[Option<String>]) -> Option<String> {
    usize::try_from(i)
        .ok()
        .and_then(|i| ids.get(i).cloned().flatten())
}

fn model_index(id: Option<&str>, ids: &[Option<String>]) -> i32 {
    ids.iter().position(|m| m.as_deref() == id).map_or(0, int)
}

/// 0 default, 1 on, 2 off.
fn tri(v: Option<bool>) -> i32 {
    v.map_or(0, |on| if on { 1 } else { 2 })
}

fn from_tri(i: i32) -> Option<bool> {
    match i {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

fn profile_values(r: &PasteRule, models: &[Option<String>]) -> Vec<i32> {
    vec![
        language_index(r.language),
        model_index(r.model.as_deref(), models),
        match r.to_file {
            None => 0,
            Some(FileMode::Off) => 1,
            Some(FileMode::Also) => 2,
            Some(FileMode::Only) => 3,
        },
        tri(r.dictionary),
        tri(r.replacements),
        tri(r.voice_commands),
        tri(r.filler_filter),
        tri(r.pause_punctuation),
        tri(r.join_lines),
    ]
}

fn set_profile_field(r: &mut PasteRule, field: i32, i: i32, models: &[Option<String>]) {
    match field {
        0 => r.language = language_at(i),
        1 => r.model = model_at(i, models),
        2 => {
            r.to_file = match i {
                1 => Some(FileMode::Off),
                2 => Some(FileMode::Also),
                3 => Some(FileMode::Only),
                _ => None,
            }
        }
        3 => r.dictionary = from_tri(i),
        4 => r.replacements = from_tri(i),
        5 => r.voice_commands = from_tri(i),
        6 => r.filler_filter = from_tri(i),
        7 => r.pause_punctuation = from_tri(i),
        8 => r.join_lines = from_tri(i),
        _ => {}
    }
}

/// The search field, its list and the apps popup.
fn bind_rules(window: &AppWindow, ctx: &Ctx) {
    let s = window.global::<TextState>();
    let (w, c) = (window.as_weak(), ctx.clone());
    let flash = Rc::new(Timer::default());
    s.on_rule_picked(move |exe| {
        let exe = exe_name(exe.trim());
        if exe.is_empty() {
            return;
        }
        let tile = found_tile(&w, &exe);
        if let Some(tile) = &tile {
            remember_name(&c, tile);
        }
        let mut added = false;
        apply(&w, &c, |p| added = add_rule(p, &exe));
        if added {
            with(&w, |s| s.set_query(SharedString::new()));
        } else {
            // The field's row already said so: the rule itself lights up (DS-28.1).
            let app = c
                .config
                .borrow()
                .config
                .paste
                .rules
                .iter()
                .find_map(|r| r.app.eq_ignore_ascii_case(&exe).then(|| r.app.clone()));
            with(&w, |s| s.set_flash_app(app.unwrap_or_default().into()));
            let w = w.clone();
            flash.start(TimerMode::SingleShot, FLASH, move || {
                with(&w, |s| s.set_flash_app(SharedString::new()));
            });
        }
    });
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_found_toggled(move |exe| {
        if has_rule(&c.config.borrow().config.paste.rules, &exe) {
            forget_exe(&c, &exe);
            apply(&w, &c, |p| {
                p.rules.retain(|r| !r.app.eq_ignore_ascii_case(&exe));
            });
        } else if let Some(tile) = found_tile(&w, &exe) {
            remember_name(&c, &tile);
            apply(&w, &c, |p| {
                add_rule(p, &tile.caption);
            });
        }
    });
    // The filters read the fields' text from the state: the callbacks only say it changed.
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_query_edited(move |_| redraw(&w, &c));
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_popup_search(move |_| redraw(&w, &c));
}

/// The scan's tile of the app with this exe.
fn found_tile(weak: &Weak<AppWindow>, exe: &str) -> Option<GalleryTile> {
    let window = weak.upgrade()?;
    let found = window.global::<TextState>().get_found();
    found.iter().find(|t| t.caption.eq_ignore_ascii_case(exe))
}

fn redraw(weak: &Weak<AppWindow>, ctx: &Ctx) {
    with(weak, |s| {
        let file = ctx.config.borrow();
        show_rules(&s, &file.config.paste.rules, &file.config.ui.app_names);
    });
}

/// The name a found tile's rule keeps after the window closes (UI rule 30).
fn remember_name(ctx: &Ctx, tile: &GalleryTile) {
    let mut file = ctx.config.borrow_mut();
    file.config
        .ui
        .app_names
        .insert(tile.caption.to_lowercase(), tile.title.to_string());
}

/// The saved name of the rule at row `i` goes with the rule.
fn forget_name(ctx: &Ctx, i: i32) {
    let app = usize::try_from(i).ok().and_then(|i| {
        ctx.config
            .borrow()
            .config
            .paste
            .rules
            .get(i)
            .map(|r| r.app.clone())
    });
    if let Some(app) = app {
        forget_exe(ctx, &app);
    }
}

fn forget_exe(ctx: &Ctx, exe: &str) {
    ctx.config
        .borrow_mut()
        .config
        .ui
        .app_names
        .remove(&exe.to_lowercase());
}

/// Scan runs on a worker and a timer picks up its answer, so the window keeps drawing and taking
/// input meanwhile. It starts by itself the first time the Per-app tab shows (the rules want the
/// apps' icons and names), and again from Scan again. Bound again after Reset, which drops a
/// running scan's answer.
fn bind_scan(window: &AppWindow, ctx: &Ctx) {
    let s = window.global::<TextState>();
    s.set_scanning(false);
    let timer = Rc::new(Timer::default());
    let (w, c) = (window.as_weak(), ctx.clone());
    let start = Rc::new(move || {
        let Some(window) = w.upgrade() else { return };
        let s = window.global::<TextState>();
        if s.get_scanning() {
            return;
        }
        s.set_scanning(true);
        let roots = scan_roots();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            // The window closed first: nobody waits for the answer.
            let _ = tx.send(find_apps(roots));
        });
        let (w, c, stop) = (w.clone(), c.clone(), Rc::downgrade(&timer));
        timer.start(TimerMode::Repeated, SCAN_POLL, move || {
            let found = match rx.try_recv() {
                Err(mpsc::TryRecvError::Empty) => return,
                Ok(found) => found,
                // The worker panicked: it found nothing.
                Err(mpsc::TryRecvError::Disconnected) => Vec::new(),
            };
            if let Some(timer) = stop.upgrade() {
                timer.stop();
            }
            with(&w, |s| {
                let tiles: Vec<GalleryTile> = found
                    .into_iter()
                    .map(|f| GalleryTile {
                        picture: f.icon.map(Image::from_rgba8).unwrap_or_default(),
                        title: f.name.into(),
                        caption: f.exe.into(),
                        ..GalleryTile::default()
                    })
                    .collect();
                s.set_found(ModelRc::from(Rc::new(VecModel::from(tiles))));
                let file = c.config.borrow();
                show_rules(&s, &file.config.paste.rules, &file.config.ui.app_names);
                drop(file);
                s.set_scanning(false);
                s.set_scanned(true);
            });
        });
    });
    let again = start.clone();
    s.on_scan(move || again());
    let w = window.as_weak();
    s.on_per_app_shown(move || {
        if w.upgrade()
            .is_some_and(|window| !window.global::<TextState>().get_scanned())
        {
            start();
        }
    });
}

#[cfg(test)]
thread_local! {
    /// Where a scan in a unit test looks: never the real Start menu.
    static TEST_ROOTS: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Where Scan looks; `None`: the Start menu and desktop folders.
#[cfg(not(test))]
fn scan_roots() -> Option<Vec<PathBuf>> {
    None
}

#[cfg(test)]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the signature of the version outside tests"
)]
fn scan_roots() -> Option<Vec<PathBuf>> {
    Some(TEST_ROOTS.with_borrow(Clone::clone))
}

#[cfg(windows)]
fn find_apps(roots: Option<Vec<PathBuf>>) -> Vec<Found> {
    use oxi_platform::os::shortcuts;
    match roots {
        Some(roots) => shortcuts::scan(&roots),
        None => shortcuts::scan_installed(),
    }
    .into_iter()
    .map(|app| Found {
        icon: app
            .icon
            .map(|i| SharedPixelBuffer::clone_from_slice(&i.rgba, i.width, i.height)),
        name: app.name,
        exe: app.exe,
    })
    .collect()
}

/// shortcut: no app scan off Windows yet; the typed exe (bundle id, `WM_CLASS`) is the way there.
#[cfg(not(windows))]
fn find_apps(_: Option<Vec<PathBuf>>) -> Vec<Found> {
    Vec::new()
}

/// The daemon matches the whole process file name, which on Windows ends in `.exe`: a typed
/// "Discord" would never apply, so it becomes "Discord.exe" (people do not know exe names).
fn exe_name(typed: &str) -> String {
    if cfg!(windows) && !typed.is_empty() && !typed.to_lowercase().ends_with(".exe") {
        format!("{typed}.exe")
    } else {
        typed.to_owned()
    }
}

/// The daemon matches rules by app case-insensitively.
fn has_rule(rules: &[PasteRule], app: &str) -> bool {
    rules
        .iter()
        .any(|r| r.app.to_lowercase() == app.to_lowercase())
}

/// A new rule types into `app`. The daemon takes the first rule whose app matches
/// case-insensitively, so a second rule for the same app is refused (false): it would never apply.
fn add_rule(paste: &mut Paste, app: &str) -> bool {
    if has_rule(&paste.rules, app) {
        return false;
    }
    if !app.is_empty() {
        paste.rules.push(PasteRule {
            app: app.to_owned(),
            mode: Some(PasteMode::Type),
            chord: None,
            restore_delay_ms: None,
            ..PasteRule::default()
        });
    }
    true
}

/// Change `[paste]`, show its rules again and save.
fn apply(weak: &Weak<AppWindow>, ctx: &Ctx, edit: impl FnOnce(&mut Paste)) {
    change(weak, ctx, edit);
    report(weak, ctx.save());
}

/// [`apply`] without the save: the caller saves, e.g. through a [`SaveDebounce`].
fn change(weak: &Weak<AppWindow>, ctx: &Ctx, edit: impl FnOnce(&mut Paste)) {
    edit(&mut ctx.config.borrow_mut().config.paste);
    with(weak, |s| {
        let file = ctx.config.borrow();
        show_rules(&s, &file.config.paste.rules, &file.config.ui.app_names);
    });
}

/// Whether `query` (case-insensitive, trimmed) is in the app's name or its exe; an empty one is in all.
fn matches(query: &str, name: &str, exe: &str) -> bool {
    let q = query.trim().to_lowercase();
    q.is_empty() || name.to_lowercase().contains(&q) || exe.to_lowercase().contains(&q)
}

/// Rows of `old` take `new` in place when the two list the same things in the same order (`same`):
/// a new model rebuilds every row, and the Combo being arrowed through, or a grid scrolled to the
/// tile just ticked, would lose its place. `Some`: the model to set instead.
fn sync<T: Clone + PartialEq + 'static>(
    old: &ModelRc<T>,
    new: Vec<T>,
    same: impl Fn(&T, &T) -> bool,
) -> Option<ModelRc<T>> {
    let fits = old.row_count() == new.len()
        && new
            .iter()
            .enumerate()
            .all(|(i, row)| old.row_data(i).is_some_and(|o| same(&o, row)));
    if !fits {
        return Some(ModelRc::from(Rc::new(VecModel::from(new))));
    }
    for (i, row) in new.into_iter().enumerate() {
        if old.row_data(i).as_ref() != Some(&row) {
            old.set_row_data(i, row);
        }
    }
    None
}

/// What the Per-app tab shows, from the config and the last scan: the rule list (each rule named
/// and pictured as the scan found its app, filtered by the search field), the apps ticked in the
/// popup's grid (filtered by its field) and what the search field offers.
fn show_rules(s: &TextState<'_>, rules: &[PasteRule], known: &BTreeMap<String, String>) {
    let found = s.get_found();
    // The default rule's app, then the names saved when a rule was picked, then the last scan.
    let mut names: HashMap<String, SharedString> =
        HashMap::from([("mstsc.exe".to_owned(), "Remote Desktop Connection".into())]);
    names.extend(known.iter().map(|(exe, name)| (exe.clone(), name.into())));
    let mut icons: HashMap<String, Image> = HashMap::new();
    for i in 0..found.row_count() {
        let Some(mut tile) = found.row_data(i) else {
            continue;
        };
        let key = tile.caption.to_lowercase();
        names.insert(key.clone(), tile.title.clone());
        if tile.picture.size().width > 0 {
            icons.insert(key, tile.picture.clone());
        }
        let taken = has_rule(rules, &tile.caption);
        if tile.checked != taken {
            tile.checked = taken;
            found.set_row_data(i, tile);
        }
    }
    let new = rows(rules, &names, &icons, &s.get_query());
    s.set_rule_matches(int(new.iter().filter(|r| !r.hidden).count()));
    if let Some(model) = sync(&s.get_rules(), new, |_, _| true) {
        s.set_rules(model);
    }
    let by_exe = |a: &GalleryTile, b: &GalleryTile| a.caption == b.caption && a.hint == b.hint;
    let grid = grid(&found, &s.get_popup_query());
    if let Some(model) = sync(&s.get_grid(), grid, by_exe) {
        s.set_grid(model);
    }
    let offered = suggestions(&found, rules, &s.get_query());
    if let Some(model) = sync(&s.get_suggestions(), offered, by_exe) {
        s.set_suggestions(model);
    }
}

fn rows(
    rules: &[PasteRule],
    names: &HashMap<String, SharedString>,
    icons: &HashMap<String, Image>,
    query: &str,
) -> Vec<PasteRuleRow> {
    rules
        .iter()
        .map(|r| {
            let key = r.app.to_lowercase();
            let name = names.get(&key).cloned().unwrap_or_default();
            PasteRuleRow {
                hidden: !matches(query, &name, &r.app),
                app: r.app.as_str().into(),
                name,
                mode: match r.mode {
                    None => 0,
                    Some(PasteMode::Clipboard) => 1,
                    Some(PasteMode::Type) => 2,
                },
                icon: icons.get(&key).cloned().unwrap_or_default(),
            }
        })
        .collect()
}

/// The apps popup's tiles: the scan's apps that match `query`.
fn grid(found: &ModelRc<GalleryTile>, query: &str) -> Vec<GalleryTile> {
    found
        .iter()
        .filter(|t| matches(query, &t.title, &t.caption))
        .collect()
}

/// What the search field offers under it: up to [`SUGGESTIONS`] installed apps with no rule yet that
/// match `query`, the one whose exe is the typed text first, then a row for the typed exe unless
/// one of those is it. `hint` "add" adds the typed exe, "dup" says it has a rule already.
fn suggestions(found: &ModelRc<GalleryTile>, rules: &[PasteRule], query: &str) -> Vec<GalleryTile> {
    let typed = query.trim();
    if typed.is_empty() {
        return Vec::new();
    }
    let exe = exe_name(typed);
    let mut apps: Vec<GalleryTile> = found
        .iter()
        .filter(|t| !t.checked && matches(typed, &t.title, &t.caption))
        .collect();
    apps.sort_by_key(|t| !t.caption.eq_ignore_ascii_case(&exe));
    apps.truncate(SUGGESTIONS);
    if !apps
        .first()
        .is_some_and(|t| t.caption.eq_ignore_ascii_case(&exe))
    {
        let dup = has_rule(rules, &exe);
        apps.push(GalleryTile {
            title: exe.clone().into(),
            caption: exe.into(),
            hint: if dup { "dup" } else { "add" }.into(),
            ..GalleryTile::default()
        });
    }
    apps
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

/// Follows an edit of the files from outside: a clean editor takes the new text, a dirty one shows
/// the conflict (both through `refresh`). Nothing happens while a clean editor is up to date.
pub fn poll(window: &AppWindow, layout: &Layout) {
    let s = window.global::<TextState>();
    for file in FILES {
        let current = info(&s, file).base == stamp(&fs::read(path(layout, file)));
        if !current || info(&s, file).dirty {
            refresh(&s, layout, file);
        }
    }
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

#[cfg(test)]
mod filter_tests {
    use slint::{ModelRc, VecModel};

    use super::*;

    fn tile(title: &str, exe: &str, checked: bool) -> GalleryTile {
        GalleryTile {
            title: title.into(),
            caption: exe.into(),
            checked,
            ..GalleryTile::default()
        }
    }

    #[test]
    fn a_query_is_a_trimmed_case_insensitive_part_of_the_name_or_the_exe() {
        assert!(matches("", "Discord", "Discord.exe"));
        assert!(matches("  ", "x", "y"));
        assert!(matches("disc", "Discord", "x.exe"));
        assert!(matches(" X.EXE", "Name", "x.exe"));
        assert!(matches("ÄPP", "Mein äpp", "a.exe"));
        assert!(!matches("zzz", "Discord", "Discord.exe"));
    }

    #[test]
    fn suggestions_skip_apps_with_a_rule_cap_at_six_and_end_with_the_typed_exe() {
        let apps: Vec<GalleryTile> = (0..9)
            .map(|i| tile(&format!("Synth {i}"), &format!("synth{i}.exe"), i == 0))
            .collect();
        let found = ModelRc::from(Rc::new(VecModel::from(apps)));
        let rules = [PasteRule {
            app: "synth0.exe".into(),
            ..PasteRule::default()
        }];
        let offered = suggestions(&found, &rules, "synth");
        // Six apps (synth0 has a rule), then the typed exe.
        assert_eq!(offered.len(), SUGGESTIONS + 1);
        assert!(
            offered[..SUGGESTIONS]
                .iter()
                .all(|t| t.caption != "synth0.exe")
        );
        assert_eq!(offered[SUGGESTIONS].hint, "add");
        // Nothing typed: nothing offered.
        assert_eq!(suggestions(&found, &rules, "  ").len(), 0);
        // The exe typed in full is its app's row, first, and no other row.
        let exact = suggestions(&found, &rules, "synth8.exe");
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].caption, "synth8.exe");
        // A typed app that has a rule is a refused row, not an app.
        let dup = suggestions(&found, &rules, "SYNTH0.EXE");
        assert_eq!((dup.len(), dup[0].hint.as_str()), (1, "dup"));
    }

    #[test]
    fn sync_changes_rows_in_place_and_replaces_the_model_only_when_the_list_changed() {
        let old = ModelRc::from(Rc::new(VecModel::from(vec![1, 2, 3])));
        assert!(sync(&old, vec![1, 9, 3], |a, b| a == b).is_some());
        assert!(sync(&old, vec![1, 2, 3], |a, b| a == b).is_none());
        // Same length, same key: updated in place.
        assert!(sync(&old, vec![5, 6, 7], |_, _| true).is_none());
        assert_eq!(old.iter().collect::<Vec<_>>(), [5, 6, 7]);
        assert!(sync(&old, vec![5, 6], |_, _| true).is_some());
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::fmt::Write;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use i_slint_backend_testing::{AccessibleRole, ElementHandle, mock_elapsed_time};
    use oxi_core::paths::Layout;
    use oxi_platform::os::shortcuts::link_bytes;
    use slint::platform::{Key, WindowEvent};
    use slint::{ComponentHandle, LogicalPosition, LogicalSize, Model, SharedString};

    use super::{SCAN_POLL, TEST_ROOTS, TextState};
    use crate::testing::{assert_nothing_moved, has, one, spots, with_role};
    use crate::{AppWindow, Page};

    /// A folder of synthetic shortcuts: name, target, arguments.
    fn shortcuts(name: &str, links: &[(String, String, String)]) -> PathBuf {
        let dir = crate::testing::layout("text-links", name).data_dir;
        for (title, target, args) in links {
            fs::write(dir.join(format!("{title}.lnk")), link_bytes(target, args)).unwrap();
        }
        dir
    }

    /// `n` synthetic apps with long names.
    fn many_apps(name: &str, n: usize) -> PathBuf {
        let links: Vec<_> = (0..n)
            .map(|i| {
                (
                    format!("Synthetic Application With A Long Name {i:03}"),
                    format!(r"C:\Synth\synthapp{i:03}.exe"),
                    String::new(),
                )
            })
            .collect();
        shortcuts(name, &links)
    }

    fn many_rules(n: usize) -> String {
        let mut text = String::from("version = 2\n");
        for i in 0..n {
            writeln!(
                text,
                "[[paste.rules]]\napp = \"synthrule{i}.exe\"\nmode = \"type\""
            )
            .unwrap();
        }
        text
    }

    /// The Per-app rules tab at 720 x 520, its scan pointed at `root`.
    fn per_app(name: &str, config: Option<&str>, root: PathBuf) -> (AppWindow, Layout) {
        i_slint_backend_testing::init_no_event_loop();
        TEST_ROOTS.set(vec![root]);
        let layout = crate::testing::layout("text", name);
        if let Some(config) = config {
            fs::write(layout.config_file(), config).unwrap();
        }
        let window = crate::open(layout.clone()).unwrap();
        window.show().unwrap();
        window.set_page(Page::Text);
        window.window().set_size(LogicalSize::new(720.0, 520.0));
        mock_elapsed_time(Duration::from_millis(1));
        with_role(&window, "Per-app rules", AccessibleRole::Tab).invoke_accessible_default_action();
        mock_elapsed_time(Duration::from_millis(1));
        (window, layout)
    }

    /// Waits for the running scan's answer, the timer that picks it up driven by hand.
    fn wait_scan(window: &AppWindow) {
        let s = window.global::<TextState>();
        let start = Instant::now();
        while s.get_scanning() {
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "the scan never answered"
            );
            std::thread::sleep(Duration::from_millis(5));
            mock_elapsed_time(SCAN_POLL);
        }
    }

    fn found(window: &AppWindow) -> Vec<(String, String, bool)> {
        window
            .global::<TextState>()
            .get_found()
            .iter()
            .map(|t| (t.title.to_string(), t.caption.to_string(), t.checked))
            .collect()
    }

    fn saved_names(layout: &Layout) -> std::collections::BTreeMap<String, String> {
        let text = fs::read_to_string(layout.config_file()).unwrap_or_default();
        oxi_core::config::ConfigFile::parse(&text)
            .unwrap()
            .config
            .ui
            .app_names
    }

    fn rules(layout: &Layout) -> Vec<String> {
        let text = fs::read_to_string(layout.config_file()).unwrap_or_default();
        oxi_core::config::ConfigFile::parse(&text)
            .unwrap()
            .config
            .paste
            .rules
            .into_iter()
            .map(|r| r.app)
            .collect()
    }

    fn whole_in_view(window: &AppWindow, label: &str) -> bool {
        ElementHandle::find_by_accessible_label(window, label)
            .next()
            .is_some_and(|e| {
                let y = e.absolute_position().y;
                y >= 0.0 && y + e.size().height <= 520.0
            })
    }

    fn page_scrolls(window: &AppWindow) -> bool {
        let at = with_role(window, "Dictionary", AccessibleRole::Tab).absolute_position();
        window
            .window()
            .dispatch_event(WindowEvent::PointerScrolled {
                position: LogicalPosition::new(at.x + 4.0, at.y + 4.0),
                delta_x: 0.0,
                delta_y: -100_000.0,
            });
        mock_elapsed_time(Duration::from_millis(500));
        window.get_scroll_y() != 0.0
    }

    fn press(window: &AppWindow, text: SharedString) {
        window
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        window
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text });
    }

    /// Review d03 #9: "Hold doubtful words back" follows History saving switched after the page
    /// was bound, without reopening the window.
    #[test]
    fn hold_back_follows_history_saving_switched_later() {
        i_slint_backend_testing::init_no_event_loop();
        let layout = crate::testing::layout("text", "hold-follow");
        let config = "version = 2\n[history]\nenabled = false\n";
        fs::write(layout.config_file(), config).unwrap();
        let window = crate::open(layout).unwrap();
        let t = window.global::<TextState>();
        assert!(!t.get_history_on());
        // What the System page's switch (and History's "Turn on") write.
        let system = window.global::<crate::generated::SystemState>();
        system.set_history_on(true);
        assert!(t.get_history_on());
        system.set_history_on(false);
        assert!(!t.get_history_on());
    }

    #[test]
    fn scan_runs_on_a_worker_and_the_ui_thread_stays_free() {
        let (window, layout) = per_app("worker", None, many_apps("worker", 300));
        let s = window.global::<TextState>();
        // Opening the tab started it; the answer waits for the UI thread's timer.
        assert!(s.get_scanning());
        assert_eq!(s.get_found().row_count(), 0);
        // Meanwhile the page takes a typed rule.
        s.invoke_rule_picked("synthtyped.exe".into());
        assert!(rules(&layout).contains(&"synthtyped.exe".to_owned()));
        wait_scan(&window);
        assert_eq!(s.get_found().row_count(), 300);
        assert_eq!(s.get_grid().row_count(), 300);
        assert!(s.get_scanned());
        // A tab that was shown once does not scan again by itself.
        s.invoke_per_app_shown();
        assert!(!s.get_scanning());
    }

    #[test]
    fn a_scan_that_finds_nothing_leaves_the_popup_a_way_to_scan_again() {
        let empty = crate::testing::layout("text-links", "empty").data_dir;
        let (window, _layout) = per_app("empty", None, empty);
        let s = window.global::<TextState>();
        assert!(s.get_scanning());
        wait_scan(&window);
        assert_eq!(found(&window), []);
        assert_eq!(s.get_grid().row_count(), 0);
        // The popup says so, with the way out beside it.
        one(&window, "Scan for apps").invoke_accessible_default_action();
        mock_elapsed_time(Duration::from_millis(1));
        assert!(has(&window, "No apps found"));
        one(&window, "Scan again").invoke_accessible_default_action();
        assert!(s.get_scanning());
        wait_scan(&window);
    }

    #[test]
    fn a_found_app_becomes_a_rule_for_its_exe_under_its_name_and_a_second_pick_removes_it() {
        let links = [
            (
                "Synth Chat".to_owned(),
                r"C:\Users\synth\AppData\Local\SynthChat\Update.exe".to_owned(),
                "--processStart SynthChat.exe".to_owned(),
            ),
            (
                "Synth Editor".to_owned(),
                r"C:\Synth\editor.exe".to_owned(),
                String::new(),
            ),
        ];
        let config = "version = 2\n[[paste.rules]]\napp = \"EDITOR.EXE\"\nmode = \"clipboard\"\n";
        let (window, layout) = per_app("pick", Some(config), shortcuts("pick", &links));
        wait_scan(&window);
        // The editor already has a rule (the daemon matches case-insensitively): ticked.
        assert_eq!(
            found(&window),
            [
                ("Synth Chat".into(), "SynthChat.exe".into(), false),
                ("Synth Editor".into(), "editor.exe".into(), true),
            ]
        );
        let s = window.global::<TextState>();
        assert_eq!(s.get_rules().row_data(0).unwrap().name, "Synth Editor");

        // The grid keeps its model through a tick: it neither rebuilds nor jumps.
        let grid = s.get_grid();
        s.invoke_found_toggled("SynthChat.exe".into());
        assert!(grid == s.get_grid(), "the grid model was replaced");
        assert_eq!(rules(&layout), ["EDITOR.EXE", "SynthChat.exe"]);
        assert!(found(&window)[0].2, "the picked app is ticked now");
        assert!(s.get_grid().row_data(0).unwrap().checked);
        let row = s.get_rules().row_data(1).unwrap();
        assert_eq!((row.name.as_str(), row.mode), ("Synth Chat", 2));
        mock_elapsed_time(Duration::from_millis(1));
        assert!(has(&window, "Remove SynthChat.exe"));
        // UI rule 30: the name is saved with the rule, so a new window has it without a scan.
        assert_eq!(saved_names(&layout)["synthchat.exe"], "Synth Chat");
        // The same tile again takes the rule off and forgets the name.
        s.invoke_found_toggled("SynthChat.exe".into());
        assert_eq!(rules(&layout), ["EDITOR.EXE"]);
        assert!(!found(&window)[0].2);
        assert!(!saved_names(&layout).contains_key("synthchat.exe"));
        // Removing a rule in the list unticks its tile too.
        s.invoke_rule_removed(0);
        assert!(!found(&window)[1].2);
    }

    #[test]
    fn removing_a_rule_deletes_its_name_from_a_file_that_already_has_the_table() {
        let config = concat!(
            "# my note
version = 2
[[paste.rules]]
app = \"a.exe\"
mode = \"clipboard\"

",
            "[ui.app_names]
\"a.exe\" = \"Aaa\"
\"b.exe\" = \"Bbb\"
"
        );
        let (window, layout) = per_app("forget", Some(config), shortcuts("forget", &[]));
        assert!(has(&window, "Remove a.exe"));
        one(&window, "Remove a.exe").invoke_accessible_default_action();
        let text = fs::read_to_string(layout.config_file()).unwrap();
        let names = saved_names(&layout);
        assert!(!names.contains_key("a.exe"), "{text}");
        assert_eq!(names["b.exe"], "Bbb", "{text}");
        assert!(text.contains("# my note"), "{text}");
        assert!(rules(&layout).is_empty(), "{text}");
    }

    #[test]
    fn the_search_field_filters_the_rules_and_offers_apps_and_a_typed_exe() {
        let links = [
            ("Synth Chat", r"C:\S\chat.exe"),
            ("Synth Editor", r"C:\S\editor.exe"),
            ("Other Tool", r"C:\S\tool.exe"),
        ]
        .map(|(n, t)| (n.to_owned(), t.to_owned(), String::new()));
        let config = "version = 2\n[[paste.rules]]\napp = \"editor.exe\"\n[[paste.rules]]\napp = \"zeta.exe\"\n";
        let (window, layout) = per_app("search", Some(config), shortcuts("search", &links));
        wait_scan(&window);
        let s = window.global::<TextState>();
        let hidden = || -> Vec<bool> { s.get_rules().iter().map(|r| r.hidden).collect() };
        assert_eq!(hidden(), [false, false]);
        assert_eq!(s.get_rule_matches(), 2);
        assert_eq!(s.get_suggestions().row_count(), 0);

        // Name and exe, any case; the row keeps its index in the config.
        s.set_query("ZETA".into());
        mock_elapsed_time(Duration::from_millis(1));
        assert_eq!(hidden(), [true, false]);
        assert_eq!(s.get_rule_matches(), 1);
        s.invoke_rule_mode_picked(1, 1);
        mock_elapsed_time(Duration::from_millis(350));
        let text = fs::read_to_string(layout.config_file()).unwrap();
        assert!(text.contains("zeta.exe"), "{text}");
        assert_eq!(
            oxi_core::config::ConfigFile::parse(&text)
                .unwrap()
                .config
                .paste
                .rules[1]
                .mode,
            Some(oxi_core::config::PasteMode::Clipboard)
        );

        // Installed apps without a rule that match, then the typed exe.
        s.set_query("synth".into());
        mock_elapsed_time(Duration::from_millis(1));
        let offered = || -> Vec<(String, String)> {
            s.get_suggestions()
                .iter()
                .map(|t| (t.caption.to_string(), t.hint.to_string()))
                .collect()
        };
        assert_eq!(
            offered(),
            [
                ("chat.exe".into(), String::new()),
                ("synth.exe".into(), "add".into())
            ]
        );
        // An exe that is an installed app is that app's row, first, with no add row.
        s.set_query("tool.exe".into());
        mock_elapsed_time(Duration::from_millis(1));
        assert_eq!(offered(), [("tool.exe".into(), String::new())]);
        // A duplicate says why in its row.
        s.set_query("ZETA.EXE".into());
        mock_elapsed_time(Duration::from_millis(1));
        assert_eq!(offered(), [("ZETA.EXE".into(), "dup".into())]);
        // Picking a row adds the rule, clears the field and shows every rule again.
        s.invoke_rule_picked("tool.exe".into());
        mock_elapsed_time(Duration::from_millis(1));
        assert_eq!(s.get_query(), "");
        assert_eq!(rules(&layout), ["editor.exe", "zeta.exe", "tool.exe"]);
        assert_eq!(s.get_rule_matches(), 3);
        assert_eq!(s.get_rules().row_data(2).unwrap().name, "Other Tool");
        // A duplicate pick adds nothing and lights up the existing rule for 600 ms, no toast.
        s.invoke_rule_picked("ZETA.EXE".into());
        assert_eq!(rules(&layout).len(), 3);
        assert_eq!(s.get_flash_app(), "zeta.exe");
        assert_eq!(window.global::<crate::generated::Toast>().get_text(), "");
        mock_elapsed_time(Duration::from_millis(599));
        assert_eq!(s.get_flash_app(), "zeta.exe");
        mock_elapsed_time(Duration::from_millis(2));
        assert_eq!(s.get_flash_app(), "");
    }

    #[test]
    fn the_popup_filters_its_grid_by_name_and_exe() {
        let links = [
            ("Synth Chat", r"C:\S\chat.exe"),
            ("Synth Editor", r"C:\S\editor.exe"),
            ("Other Tool", r"C:\S\tool.exe"),
        ]
        .map(|(n, t)| (n.to_owned(), t.to_owned(), String::new()));
        let (window, _layout) = per_app("grid", None, shortcuts("grid", &links));
        wait_scan(&window);
        let s = window.global::<TextState>();
        let titles =
            || -> Vec<String> { s.get_grid().iter().map(|t| t.title.to_string()).collect() };
        assert_eq!(titles(), ["Other Tool", "Synth Chat", "Synth Editor"]);
        s.set_popup_query("EDITOR".into());
        mock_elapsed_time(Duration::from_millis(1));
        assert_eq!(titles(), ["Synth Editor"]);
        s.set_popup_query("nothing like it".into());
        mock_elapsed_time(Duration::from_millis(1));
        assert_eq!(titles(), Vec::<String>::new());
        // Found apps are all still there for the other field.
        assert_eq!(s.get_found().row_count(), 3);
    }

    /// The keyboard reaches the field, a letter lands in it, the arrows and Enter take a row.
    #[test]
    fn the_search_field_and_its_list_are_reached_by_keyboard() {
        let links = [(
            "Synth Notes".to_owned(),
            r"C:\Synth\notes.exe".to_owned(),
            String::new(),
        )];
        let (window, layout) = per_app("keys", None, shortcuts("keys", &links));
        wait_scan(&window);
        let s = window.global::<TextState>();
        let field = || one(&window, "New rule app").accessible_value();
        // Tab on until a letter lands in the field, then empty it again.
        for _ in 0..16 {
            press(&window, Key::Tab.into());
            press(&window, "q".into());
            if field().as_deref() == Some("q") {
                break;
            }
        }
        assert_eq!(field().as_deref(), Some("q"), "Tab never reached the field");
        one(&window, "New rule app").set_accessible_value("");
        for letter in ["n", "o", "t"] {
            press(&window, letter.into());
        }
        mock_elapsed_time(Duration::from_millis(1));
        assert_eq!(s.get_query(), "not");
        assert_eq!(
            s.get_suggestions().row_count(),
            2,
            "the app and the typed exe"
        );
        // Down to the second row, Up back to the first, Enter takes it.
        press(&window, Key::DownArrow.into());
        press(&window, Key::UpArrow.into());
        press(&window, Key::Return.into());
        assert_eq!(rules(&layout), ["mstsc.exe", "notes.exe"]);
        assert_eq!(s.get_query(), "");
    }

    /// UI rule 25 with 300 found apps and 150 rules: the bar stays whole and the page does not
    /// scroll at 720 x 520, with no banner, the offline daemon's and a broken config's.
    #[test]
    fn three_hundred_apps_and_150_rules_fit_the_minimum_size() {
        let (window, _layout) = per_app("fit", Some(&many_rules(150)), many_apps("fit", 300));
        wait_scan(&window);
        for (banner, daemon) in [
            ("", ""),
            ("", oxi_core::strings::DAEMON_OFFLINE),
            ("Synthetic config trouble.", ""),
        ] {
            window.set_banner_text(banner.into());
            window.set_daemon_text(daemon.into());
            window.set_daemon_start(!daemon.is_empty());
            mock_elapsed_time(Duration::from_millis(1));
            for label in ["Scan for apps", "New rule app"] {
                assert!(
                    whole_in_view(&window, label),
                    "{label} is cut (banner {banner:?}, daemon {daemon:?})"
                );
            }
            assert!(
                !page_scrolls(&window),
                "the tab scrolls (banner {banner:?}, daemon {daemon:?})"
            );
        }
    }

    /// The rule row is on the control grid: the mode select and Delete sit where every row's do,
    /// Delete right-most, and nothing about the bar moves when a rule is added from the popup
    /// (item 4: picking an app never resizes what is on the page).
    #[test]
    fn the_rule_row_is_on_the_grid_and_adding_a_rule_moves_nothing_above_it() {
        let links = [(
            "Synth Notes".to_owned(),
            r"C:\Synth\notes.exe".to_owned(),
            String::new(),
        )];
        let (window, _layout) = per_app("grid-row", None, shortcuts("grid-row", &links));
        wait_scan(&window);
        let s = window.global::<TextState>();
        let right = |label: &str| {
            let e = one(&window, label);
            (
                e.absolute_position().x + e.size().width,
                e.absolute_position().y,
            )
        };
        let (mode_right, mode_y) = right("Mode for mstsc.exe");
        let (remove_right, remove_y) = right("Remove mstsc.exe");
        assert!(remove_right > mode_right, "Delete is the right-most");
        assert_eq!(mode_y + 16.0, remove_y + 16.0, "same row");
        let height = window.global::<crate::Tokens>().get_control_height();
        assert_eq!(one(&window, "Mode for mstsc.exe").size().height, height);
        assert_eq!(one(&window, "Remove mstsc.exe").size().height, height);
        let before = spots(&window);
        s.invoke_found_toggled("notes.exe".into());
        // The bar, the sub-tabs and the first row stay; a row appears below.
        let after = spots(&window);
        assert!(has(&window, "Remove notes.exe"));
        assert_nothing_moved(&before, &after);
    }

    /// A mode pick changes the row in place: a new model would rebuild the rows and take the
    /// focus from the Combo being arrowed through. A mode changed from outside still shows.
    #[test]
    fn a_mode_pick_keeps_the_rule_rows() {
        let config = "version = 2
[[paste.rules]]
app = \"a.exe\"
mode = \"type\"
";
        let (window, layout) = per_app("in-place", Some(config), shortcuts("in-place", &[]));
        let s = window.global::<TextState>();
        let before = s.get_rules();
        s.invoke_rule_mode_picked(0, 1);
        assert!(before == s.get_rules(), "the rule model was replaced");
        assert_eq!(s.get_rules().row_data(0).unwrap().mode, 1);
        mock_elapsed_time(Duration::from_millis(350));
        let text = fs::read_to_string(layout.config_file()).unwrap();
        assert!(text.contains("mode = \"clipboard\""), "{text}");
        mock_elapsed_time(Duration::from_millis(1));
        let combo = ElementHandle::find_by_accessible_label(&window, "Mode for a.exe")
            .next()
            .unwrap();
        assert_eq!(combo.accessible_value().as_deref(), Some("Clipboard"));
    }

    /// Arrowing a mode Combo picks on every key: the file waits for the picks to rest, then holds
    /// the last one.
    #[test]
    fn mode_picks_in_a_row_save_once_they_rest() {
        let config = "version = 2
[[paste.rules]]
app = \"a.exe\"
mode = \"type\"
";
        let (window, layout) = per_app("arrow-save", Some(config), shortcuts("arrow-save", &[]));
        let s = window.global::<TextState>();
        s.invoke_rule_mode_picked(0, 1);
        s.invoke_rule_mode_picked(0, 0);
        let text = fs::read_to_string(layout.config_file()).unwrap();
        assert!(
            text.contains("mode = \"type\""),
            "saved on an arrow: {text}"
        );
        mock_elapsed_time(Duration::from_millis(350));
        let text = fs::read_to_string(layout.config_file()).unwrap();
        assert!(
            !text.contains("mode = \"type\""),
            "the last pick (default) is saved: {text}"
        );
    }

    /// The Per-app tab drawn by the software renderer: none found (with the default rule and with
    /// none), and 300 with 150 rules, each as the page and with the apps popup open, at the
    /// minimum, two middle and a large size in both themes, into `OXI_SHOTS_DIR` as
    /// `settings-text-found-<state>-<w>-<theme>.png` (`-popup` after the state when open). Nothing
    /// runs without it: the real Start menu is never scanned here, and `tests/shots.rs` cannot point
    /// the scan at synthetic links. The renderer platform is per thread, so it draws on a thread of
    /// its own.
    #[test]
    fn shots_found_apps() {
        let Some(dir) = std::env::var_os("OXI_SHOTS_DIR").map(PathBuf::from) else {
            return;
        };
        let empty = crate::testing::layout("text-links", "shots-empty").data_dir;
        let many = many_apps("shots-many", 300);
        // One target that exists, so a real icon shows next to the shell's stand-in.
        fs::write(
            many.join("Synth Pad.lnk"),
            link_bytes(r"C:\Windows\System32\notepad.exe", ""),
        )
        .unwrap();
        std::thread::spawn(move || draw_found(&dir, empty, many))
            .join()
            .unwrap();
    }

    /// Review d08-fix #4: the Processing tab with "Press Enter after pasting" on in Clipboard mode
    /// (its "!") and off, drawn by the software renderer at 720x520 and 1280x860 in both themes,
    /// into `OXI_SHOTS_DIR` as `settings-text-<enter|words>-<on|off>-<w>-<theme>.png` (Processing,
    /// then Words). Nothing runs without it.
    #[test]
    fn shots_press_enter() {
        let Some(dir) = std::env::var_os("OXI_SHOTS_DIR").map(PathBuf::from) else {
            return;
        };
        std::thread::spawn(move || draw_press_enter(&dir))
            .join()
            .unwrap();
    }

    fn draw_press_enter(dir: &std::path::Path) {
        use std::cell::RefCell;
        use std::rc::Rc;

        use slint::platform::software_renderer::{
            MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
        };
        use slint::platform::{Platform, WindowAdapter};

        use crate::ThemeChoice;

        thread_local! {
            static LAST: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
        }
        struct Shots;
        impl Platform for Shots {
            fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
                let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
                LAST.with_borrow_mut(|last| *last = Some(window.clone()));
                Ok(window)
            }
        }

        fs::create_dir_all(dir).unwrap();
        slint::platform::set_platform(Box::new(Shots)).unwrap();
        for (state, on) in [("on", true), ("off", false)] {
            for (theme, choice) in [("light", ThemeChoice::Light), ("dark", ThemeChoice::Dark)] {
                let layout =
                    crate::testing::layout("text-enter-shots", &format!("{state}-{theme}"));
                let config = format!("version = 2\n[paste]\npress_enter = {on}\n");
                fs::write(layout.config_file(), config).unwrap();
                let window = crate::open(layout).unwrap();
                window.set_page(Page::Text);
                window.set_theme(choice);
                window.show().unwrap();
                let surface = LAST.with_borrow(Clone::clone).unwrap();
                // 4: Processing, 5: Words (`real_window` has no name for the second).
                for (tab, name) in [(4, "enter"), (5, "words")] {
                    window.global::<TextState>().set_tab(tab);
                    for (w, h) in [(720_u32, 520_u32), (1280, 860)] {
                        surface.set_size(slint::PhysicalSize::new(w, h));
                        slint::platform::update_timers_and_animations();
                        surface.request_redraw();
                        let mut buffer = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
                        assert!(surface.draw_if_needed(|r| {
                            r.render(&mut buffer, w as usize);
                        }));
                        let bytes = buffer
                            .iter()
                            .flat_map(|p| [p.red, p.green, p.blue, p.alpha])
                            .collect();
                        tiny_skia::Pixmap::from_vec(
                            bytes,
                            tiny_skia::IntSize::from_wh(w, h).unwrap(),
                        )
                        .unwrap()
                        .save_png(dir.join(format!("settings-text-{name}-{state}-{w}-{theme}.png")))
                        .unwrap();
                    }
                }
                window.hide().unwrap();
            }
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one drawing loop over states, themes and sizes"
    )]
    fn draw_found(dir: &std::path::Path, empty: PathBuf, many: PathBuf) {
        use std::cell::RefCell;
        use std::rc::Rc;

        use slint::platform::software_renderer::{
            MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
        };
        use slint::platform::{Platform, WindowAdapter};

        use crate::ThemeChoice;

        thread_local! {
            static LAST: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
        }
        struct Shots;
        impl Platform for Shots {
            fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
                let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
                LAST.with_borrow_mut(|last| *last = Some(window.clone()));
                Ok(window)
            }
            // The scan's timer runs on this clock; the default one stands still.
            fn duration_since_start(&self) -> Duration {
                static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
                START.get_or_init(Instant::now).elapsed()
            }
        }

        fs::create_dir_all(dir).unwrap();
        slint::platform::set_platform(Box::new(Shots)).unwrap();
        let rules = many_rules(150);
        let no_rules = "version = 2
[paste]
rules = []
";
        for (state, root, config) in [
            ("none", empty.clone(), None),
            ("norules", empty, Some(no_rules)),
            ("300", many, Some(rules.as_str())),
        ] {
            for (theme, choice) in [("light", ThemeChoice::Light), ("dark", ThemeChoice::Dark)] {
                TEST_ROOTS.set(vec![root.clone()]);
                let layout = crate::testing::layout("text-shots", &format!("{state}-{theme}"));
                if let Some(config) = config {
                    fs::write(layout.config_file(), config).unwrap();
                }
                let window = crate::open(layout).unwrap();
                window.set_page(Page::Text);
                window.set_theme(choice);
                window.show().unwrap();
                let surface = LAST.with_borrow(Clone::clone).unwrap();
                surface.set_size(slint::PhysicalSize::new(720, 520));
                slint::platform::update_timers_and_animations();
                with_role(&window, "Per-app rules", AccessibleRole::Tab)
                    .invoke_accessible_default_action();
                let s = window.global::<TextState>();
                let start = Instant::now();
                while s.get_scanning() {
                    assert!(start.elapsed() < Duration::from_secs(60), "no answer");
                    std::thread::sleep(Duration::from_millis(5));
                    slint::platform::update_timers_and_animations();
                }
                for variant in ["", "-suggest", "-popup"] {
                    match variant {
                        "-suggest" => {
                            // The keyboard puts the caret in the field, and a part of a name in it.
                            let key = |text: SharedString| {
                                for event in [
                                    WindowEvent::KeyPressed { text: text.clone() },
                                    WindowEvent::KeyReleased { text },
                                ] {
                                    surface.dispatch_event(event);
                                }
                            };
                            for _ in 0..16 {
                                key(Key::Tab.into());
                                key("q".into());
                                if one(&window, "New rule app").accessible_value().as_deref()
                                    == Some("q")
                                {
                                    break;
                                }
                            }
                            one(&window, "New rule app").set_accessible_value("synth");
                        }
                        "-popup" => {
                            one(&window, "New rule app").set_accessible_value("");
                            one(&window, "Scan for apps").invoke_accessible_default_action();
                        }
                        _ => {}
                    }
                    for (w, h) in [(720_u32, 520_u32), (1000, 700), (1280, 860), (1920, 1080)] {
                        surface.set_size(slint::PhysicalSize::new(w, h));
                        slint::platform::update_timers_and_animations();
                        surface.request_redraw();
                        let mut buffer = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
                        assert!(surface.draw_if_needed(|r| {
                            r.render(&mut buffer, w as usize);
                        }));
                        let bytes = buffer
                            .iter()
                            .flat_map(|p| [p.red, p.green, p.blue, p.alpha])
                            .collect();
                        tiny_skia::Pixmap::from_vec(
                            bytes,
                            tiny_skia::IntSize::from_wh(w, h).unwrap(),
                        )
                        .unwrap()
                        .save_png(dir.join(format!(
                            "settings-text-found-{state}{variant}-{w}-{theme}.png"
                        )))
                        .unwrap();
                    }
                }
                window.hide().unwrap();
            }
        }
    }
    #[test]
    fn a_rules_profile_opens_in_the_card_edits_its_fields_and_saves() {
        let config = "version = 2
[[paste.rules]]
app = \"a.exe\"
mode = \"type\"
voice_commands = true
";
        let (window, layout) = per_app("profile", Some(config), shortcuts("profile", &[]));
        let turbo = oxi_core::models::find("turbo").unwrap();
        fs::create_dir_all(layout.models_dir()).unwrap();
        crate::testing::fake_model(&layout.models_dir().join(turbo.file_name), turbo.size);
        let s = window.global::<TextState>();
        assert_eq!(s.get_profile_rule(), -1);
        s.invoke_profile_opened(0);
        // d04-fix 1: only what is on disk is offered.
        let names: Vec<SharedString> = s.get_model_names().iter().collect();
        assert_eq!(names, ["Default", turbo.display_name]);
        assert_eq!(s.get_profile_rule(), 0);
        assert_eq!(s.get_profile_app(), "a.exe");
        // The rule's own values show, the rest read Default.
        let values: Vec<i32> = s.get_profile_values().iter().collect();
        assert_eq!(values, [0, 0, 0, 0, 0, 1, 0, 0, 0]);
        let ru = super::language_names()
            .iter()
            .position(|n| n == "Russian")
            .unwrap();
        s.invoke_profile_picked(0, i32::try_from(ru).unwrap());
        s.invoke_profile_picked(1, 1);
        s.invoke_profile_picked(2, 3);
        s.invoke_profile_picked(3, 2);
        s.invoke_profile_picked(5, 0);
        s.invoke_profile_picked(8, 1);
        mock_elapsed_time(Duration::from_millis(350));
        let text = fs::read_to_string(layout.config_file()).unwrap();
        let rule = oxi_core::config::ConfigFile::parse(&text)
            .unwrap()
            .config
            .paste
            .rules
            .remove(0);
        assert_eq!(rule.language, Some(oxi_core::types::Language::RU));
        assert_eq!(rule.model.as_deref(), Some("turbo"));
        assert_eq!(rule.to_file, Some(oxi_core::config::FileMode::Only));
        assert_eq!(rule.dictionary, Some(false));
        assert_eq!(rule.voice_commands, None, "Default drops the key");
        assert_eq!(rule.join_lines, Some(true));
        assert_eq!(
            rule.mode,
            Some(oxi_core::config::PasteMode::Type),
            "the mode is untouched"
        );
    }

    #[test]
    fn the_to_file_settings_save_to_output() {
        let (window, layout) = per_app("tofile", None, shortcuts("tofile", &[]));
        let s = window.global::<TextState>();
        assert_eq!((s.get_file_mode(), s.get_file_format()), (0, 0));
        s.invoke_file_mode_picked(2);
        s.invoke_file_format_picked(1);
        let text = fs::read_to_string(layout.config_file()).unwrap();
        let out = oxi_core::config::ConfigFile::parse(&text)
            .unwrap()
            .config
            .output;
        assert_eq!(
            (out.file_mode, out.file_format),
            (
                oxi_core::config::FileMode::Only,
                oxi_core::config::FileFormat::Txt
            )
        );
    }

    /// Review d04 #5: a failed to-file write shows on the file row until the daemon clears it,
    /// and goes with the daemon.
    #[test]
    fn a_failed_file_write_warns_on_the_file_row() {
        use crate::ipc::{Event, Link};
        use oxi_core::ipc::Push;
        let (window, ipc, _sent, _layout) = crate::testing::linked("text", "filewarn", Page::Text);
        let s = window.global::<TextState>();
        let line = format!("{}: entity not found", oxi_core::paste::file::WRITE_FAILED);
        let status = |warnings: Vec<String>| {
            Event::Push(Push::Status {
                phase: oxi_core::machine::Phase::Idle,
                model: "turbo".into(),
                device: oxi_core::device::DeviceChoice::Auto,
                backend: "cpu".into(),
                loaded: true,
                warnings,
            })
        };
        ipc.deliver(&status(vec![
            "sounds.start: empty, using rise".into(),
            line.clone(),
        ]));
        assert_eq!(s.get_file_warning(), line.as_str(), "its own line only");
        let gpu_row = window
            .global::<crate::generated::ModelState>()
            .get_warning();
        assert_eq!(gpu_row, "", "not a model warning");
        ipc.deliver(&status(Vec::new()));
        assert_eq!(s.get_file_warning(), "");
        ipc.deliver(&status(vec![line]));
        ipc.deliver(&Event::Link(Link::Offline));
        assert_eq!(s.get_file_warning(), "");
    }

    #[test]
    fn the_profile_lists_map_both_ways() {
        use super::{
            from_tri, language_at, language_index, language_names, model_at, model_index,
            profile_models, tri,
        };
        for (i, name) in language_names().iter().enumerate() {
            let lang = language_at(i32::try_from(i).unwrap());
            assert_eq!(lang.is_none(), name == "Default");
            assert_eq!(language_index(lang), i32::try_from(i).unwrap());
        }
        // d04-fix 1: nothing downloaded lists Default, the own file and the rule's own model.
        let nowhere = std::path::Path::new("oxi-no-models-here");
        let ids = profile_models(nowhere, "C:/m/mine.bin", Some("tiny"));
        assert_eq!(ids, [None, Some("custom".into()), Some("tiny".into())]);
        for i in 0..ids.len() {
            let i = i32::try_from(i).unwrap();
            assert_eq!(model_index(model_at(i, &ids).as_deref(), &ids), i);
        }
        assert_eq!(model_at(999, &ids), None, "past the list is Default");
        assert_eq!(model_index(Some("nope"), &ids), 0);
        assert_eq!(
            profile_models(nowhere, " ", None),
            [None],
            "no file, no own row"
        );
        assert_eq!(language_at(999), None);
        for v in [None, Some(true), Some(false)] {
            assert_eq!(from_tri(tri(v)), v);
        }
    }
}
