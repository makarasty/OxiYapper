//! Text page: `dictionary.txt` against the prompt budget, `replace.txt` with its ignored lines,
//! and the `[paste]` config (mode, trailing space, Win+V history, restore delay, per-app rules,
//! with a scan for the apps behind Start menu and desktop shortcuts).

use std::collections::{BTreeMap, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;
use std::{fs, io, thread};

use oxi_core::config::{Paste, PasteMode, PasteRule};
use oxi_core::dictionary::{BUDGET, build_prompt, token_count};
use oxi_core::paths::Layout;
use oxi_core::strings;
use oxi_core::text::{ReplaceRules, decode_text_file};
use slint::{
    ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, Timer,
    TimerMode, VecModel, Weak,
};

use crate::debounce::SaveDebounce;
use crate::generated::{
    FileInfo, FileState, GalleryTile, PasteRuleRow, TextFile, TextState, Toast,
};
use crate::{AppWindow, Ctx, Error, report};

const FILES: [TextFile; 2] = [TextFile::Dictionary, TextFile::Replacements];

/// How often a running scan is asked for its answer.
const SCAN_POLL: Duration = Duration::from_millis(50);

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
    s.set_keep_history(paste.keep_in_clipboard_history);
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
        forget_name(&c, i);
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
        let app = exe_name(app.trim());
        let mut added = false;
        apply(&w, &c, |p| added = add_rule(p, &app));
        if !added && let Some(window) = w.upgrade() {
            let message = format!("{app} already has a rule: change its mode in the list below.");
            window.global::<Toast>().invoke_show(message.into());
        }
    });
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_found_picked(move |i| {
        let tile = w.upgrade().and_then(|window| {
            let found = window.global::<TextState>().get_found();
            usize::try_from(i).ok().and_then(|i| found.row_data(i))
        });
        // A tile with a rule is disabled, so this adds one.
        if let Some(tile) = tile {
            remember_name(&c, &tile);
            apply(&w, &c, |p| {
                add_rule(p, &tile.caption);
            });
        }
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
    let mut file = ctx.config.borrow_mut();
    let app = usize::try_from(i)
        .ok()
        .and_then(|i| file.config.paste.rules.get(i))
        .map(|r| r.app.to_lowercase());
    if let Some(app) = app {
        file.config.ui.app_names.remove(&app);
    }
}

/// Scan runs on a worker and a timer picks up its answer, so the window keeps drawing and taking
/// input meanwhile. Bound again after Reset, which drops a running scan's answer.
fn bind_scan(window: &AppWindow, ctx: &Ctx) {
    let s = window.global::<TextState>();
    s.set_scanning(false);
    let timer = Rc::new(Timer::default());
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_scan(move || {
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
    shortcuts::scan(&roots.unwrap_or_else(shortcuts::roots))
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
        });
    }
    true
}

/// Change `[paste]`, show its rules again and save.
fn apply(weak: &Weak<AppWindow>, ctx: &Ctx, change: impl FnOnce(&mut Paste)) {
    change(&mut ctx.config.borrow_mut().config.paste);
    with(weak, |s| {
        let file = ctx.config.borrow();
        show_rules(&s, &file.config.paste.rules, &file.config.ui.app_names);
    });
    report(weak, ctx.save());
}

/// The rule list, each rule named as the scan found its app, and the found apps with a rule
/// disabled.
fn show_rules(s: &TextState<'_>, rules: &[PasteRule], known: &BTreeMap<String, String>) {
    let found = s.get_found();
    // The default rule's app, then the names saved when a rule was picked, then the last scan.
    let mut names: HashMap<String, SharedString> =
        HashMap::from([("mstsc.exe".to_owned(), "Remote Desktop Connection".into())]);
    names.extend(known.iter().map(|(exe, name)| (exe.clone(), name.into())));
    for i in 0..found.row_count() {
        let Some(mut tile) = found.row_data(i) else {
            continue;
        };
        names.insert(tile.caption.to_lowercase(), tile.title.clone());
        let taken = has_rule(rules, &tile.caption);
        if tile.disabled != taken {
            tile.disabled = taken;
            tile.hint = if taken {
                "Has a rule: change its mode in the list below.".into()
            } else {
                SharedString::new()
            };
            found.set_row_data(i, tile);
        }
    }
    s.set_rules(rows(rules, &names));
}

fn rows(rules: &[PasteRule], names: &HashMap<String, SharedString>) -> ModelRc<PasteRuleRow> {
    let rows: Vec<PasteRuleRow> = rules
        .iter()
        .map(|r| PasteRuleRow {
            app: r.app.as_str().into(),
            name: names
                .get(&r.app.to_lowercase())
                .cloned()
                .unwrap_or_default(),
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
    use crate::testing::{has, one, with_role};
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
        let mut text = String::from("version = 1\n");
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
            .map(|t| (t.title.to_string(), t.caption.to_string(), t.disabled))
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

    #[test]
    fn scan_runs_on_a_worker_and_the_ui_thread_stays_free() {
        let (window, layout) = per_app("worker", None, many_apps("worker", 300));
        let s = window.global::<TextState>();
        one(&window, "Scan for apps").invoke_accessible_default_action();
        // The click came back before any answer: only the UI thread's timer takes it in.
        assert!(s.get_scanning());
        assert_eq!(s.get_found().row_count(), 0);
        assert_eq!(
            one(&window, "Scan for apps").accessible_enabled(),
            Some(false)
        );
        // A second click while it runs starts nothing.
        one(&window, "Scan for apps").invoke_accessible_default_action();
        // Meanwhile the page takes a typed rule.
        one(&window, "New rule app").set_accessible_value("synthtyped.exe");
        one(&window, "Add rule").invoke_accessible_default_action();
        assert!(rules(&layout).contains(&"synthtyped.exe".to_owned()));
        wait_scan(&window);
        assert_eq!(s.get_found().row_count(), 300);
        assert!(s.get_scanned());
        assert_eq!(
            one(&window, "Scan for apps").accessible_enabled(),
            Some(true)
        );
    }

    #[test]
    fn a_scan_that_finds_nothing_says_so_and_keeps_the_typed_path() {
        let empty = crate::testing::layout("text-links", "empty").data_dir;
        let (window, _layout) = per_app("empty", None, empty);
        one(&window, "Scan for apps").invoke_accessible_default_action();
        assert!(has(&window, "Looking for apps"));
        wait_scan(&window);
        assert_eq!(found(&window), []);
        assert!(has(
            &window,
            "No app shortcuts in the Start menu or on the desktop: type the app's .exe name above."
        ));
        assert!(whole_in_view(&window, "Add rule"));
        // Said in a line, not a gallery-high empty card that pushes the rules down.
        let gallery = ElementHandle::find_by_accessible_label(&window, "Found apps")
            .next()
            .unwrap();
        let tile = window.global::<crate::Tokens>().get_tile_height();
        assert!(gallery.size().height < tile, "{} px", gallery.size().height);
    }

    #[test]
    fn a_found_app_becomes_a_rule_for_its_exe_under_its_name() {
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
        let config = "version = 1\n[[paste.rules]]\napp = \"EDITOR.EXE\"\nmode = \"clipboard\"\n";
        let (window, layout) = per_app("pick", Some(config), shortcuts("pick", &links));
        one(&window, "Scan for apps").invoke_accessible_default_action();
        wait_scan(&window);
        // The editor already has a rule (the daemon matches case-insensitively).
        assert_eq!(
            found(&window),
            [
                ("Synth Chat".into(), "SynthChat.exe".into(), false),
                ("Synth Editor".into(), "editor.exe".into(), true),
            ]
        );
        let s = window.global::<TextState>();
        assert_eq!(s.get_rules().row_data(0).unwrap().name, "Synth Editor");

        with_role(&window, "Synth Chat", AccessibleRole::RadioButton)
            .invoke_accessible_default_action();
        assert_eq!(rules(&layout), ["EDITOR.EXE", "SynthChat.exe"]);
        assert!(found(&window)[0].2, "the picked app is disabled now");
        let row = s.get_rules().row_data(1).unwrap();
        assert_eq!((row.name.as_str(), row.mode), ("Synth Chat", 2));
        assert!(has(&window, "Remove SynthChat.exe"));
        // UI rule 30: the name is saved with the rule, so a new window has it without a scan.
        let saved = saved_names(&layout);
        assert_eq!(saved["synthchat.exe"], "Synth Chat");
        // Removing the rule frees the tile again and forgets the name.
        one(&window, "Remove SynthChat.exe").invoke_accessible_default_action();
        assert!(!found(&window)[0].2);
        let saved = saved_names(&layout);
        assert!(!saved.contains_key("synthchat.exe"));
    }

    #[test]
    fn removing_a_rule_deletes_its_name_from_a_file_that_already_has_the_table() {
        let config = concat!(
            "# my note
version = 1
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
    fn scan_and_a_found_app_are_reached_by_keyboard() {
        let links = [(
            "Synth Notes".to_owned(),
            r"C:\Synth\notes.exe".to_owned(),
            String::new(),
        )];
        let (window, layout) = per_app("keys", None, shortcuts("keys", &links));
        let s = window.global::<TextState>();
        let field = || one(&window, "New rule app").accessible_value();
        // Tab on until a letter lands in the app field, then empty it again.
        let to_field = || {
            for _ in 0..16 {
                press(&window, Key::Tab.into());
                press(&window, "q".into());
                if field().as_deref() == Some("q") {
                    break;
                }
            }
            assert_eq!(field().as_deref(), Some("q"), "Tab never reached the field");
            one(&window, "New rule app").set_accessible_value("");
        };
        to_field();
        // Scan is the stop before the field.
        press(&window, Key::Backtab.into());
        press(&window, " ".into());
        assert!(s.get_scanning(), "Space on Scan did not start a scan");
        wait_scan(&window);
        // Past the empty field (Add is off) lie the found apps; Enter adds the one under the cursor.
        to_field();
        press(&window, Key::Tab.into());
        press(&window, Key::Return.into());
        assert_eq!(rules(&layout), ["mstsc.exe", "notes.exe"]);
    }

    /// UI rule 25 with 300 found apps and 150 rules: Scan and Add stay whole and the page does not
    /// scroll at 720 x 520, with no banner, the offline daemon's and a broken config's.
    #[test]
    fn three_hundred_apps_and_150_rules_fit_the_minimum_size() {
        let (window, _layout) = per_app("fit", Some(&many_rules(150)), many_apps("fit", 300));
        one(&window, "Scan for apps").invoke_accessible_default_action();
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
            for label in ["Scan for apps", "Add rule", "Found apps"] {
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

    /// With the gallery shown and both banners (the daemon's and a config's) up at 720 x 520, the
    /// rule list keeps at least a row; the page scrolls for the rest, the list never goes.
    #[test]
    fn the_rule_list_keeps_a_row_with_the_gallery_and_two_banners() {
        let (window, _layout) = per_app("floor", Some(&many_rules(150)), many_apps("floor", 300));
        one(&window, "Scan for apps").invoke_accessible_default_action();
        wait_scan(&window);
        window.set_banner_text("Synthetic config trouble. ".repeat(12).into());
        window.set_daemon_text(oxi_core::strings::DAEMON_OFFLINE.into());
        window.set_daemon_start(true);
        mock_elapsed_time(Duration::from_millis(1));
        // The room is short: the page scrolls (its fallback), to the bottom, where the list is.
        assert!(page_scrolls(&window), "the page does not scroll");
        let list = ElementHandle::find_by_element_id(&window, "TextPage::rules")
            .next()
            .expect("the rule list");
        assert!(list.size().height >= 56.0, "{:?}", list.size());
        assert!(has(&window, "Remove synthrule0.exe"));
    }

    /// The found apps drawn by the software renderer: none found, and 300 with 150 rules, at the
    /// minimum, a middle and the largest size in both themes, into `OXI_SHOTS_DIR` as
    /// `settings-text-found-<state>-<w>-<theme>.png`. Nothing runs without it: the real Start
    /// menu is never scanned here, and `tests/shots.rs` cannot point the scan at synthetic links.
    /// The renderer platform is per thread, so it draws on a thread of its own.
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
        for (state, root, config) in [("none", empty, None), ("300", many, Some(rules.as_str()))] {
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
                one(&window, "Scan for apps").invoke_accessible_default_action();
                let s = window.global::<TextState>();
                let start = Instant::now();
                while s.get_scanning() {
                    assert!(start.elapsed() < Duration::from_secs(60), "no answer");
                    std::thread::sleep(Duration::from_millis(5));
                    slint::platform::update_timers_and_animations();
                }
                for (w, h) in [(720_u32, 520_u32), (1000, 700), (1280, 860)] {
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
                    tiny_skia::Pixmap::from_vec(bytes, tiny_skia::IntSize::from_wh(w, h).unwrap())
                        .unwrap()
                        .save_png(dir.join(format!("settings-text-found-{state}-{w}-{theme}.png")))
                        .unwrap();
                }
                window.hide().unwrap();
            }
        }
    }
}
