//! System page: `[system]` (autostart, update check), `[history]` and `debug.log_text`. The theme
//! picker on the same page belongs to the shell. `ui.language` is the app language, not the
//! dictation one. Autostart is written to the config only; registering it with the OS is M10.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, TryRecvError};
use std::{fs, thread};

use oxi_core::config::{AppLanguage, Config};
use oxi_core::history;
use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, Weak};

use crate::constants::FOLDER_POLL;
use crate::generated::SystemState;
use crate::{AppWindow, Ctx, Error, report};

/// The languages of the app's own text: config value and the name in that language. A new
/// language is one row here plus the translation of the `@tr` strings.
const APP_LANGUAGES: [(AppLanguage, &str); 1] = [(AppLanguage::En, "English")];
/// The size limit's field counts in these.
const GIB: u64 = 1 << 30;
const FOLDER_MISSING: &str =
    "This folder is gone. Nothing is saved until it is back or you choose another one.";
const FOLDER_READ_ONLY: &str =
    "This folder cannot be written to. Nothing is saved: choose another one.";
const FOLDER_REFUSED: &str = "The \"OxiYapper history\" folder here holds other files. Nothing is saved: choose another one.";

pub fn bind(window: &AppWindow, ctx: &Ctx) {
    let s = window.global::<SystemState>();
    let file = ctx.config.borrow();
    s.set_autostart(file.config.system.autostart);
    s.set_update_check(file.config.system.update_check);
    s.set_log_text(file.config.debug.log_text);
    s.set_app_languages(ModelRc::from(
        APP_LANGUAGES.map(|(_, name)| SharedString::from(name)),
    ));
    s.set_app_language(language_row(file.config.ui.language));
    s.set_history_on(file.config.history.enabled);
    #[allow(clippy::cast_precision_loss)] // whole numbers far below 2^24 in any sane config
    {
        s.set_history_days(file.config.history.max_days as f32);
        // Up, so a limit under 1 GB is not shown as "No limit"; the note says what it really is.
        s.set_history_gb(file.config.history.max_bytes.div_ceil(GIB) as f32);
    }
    s.set_history_limit_note(limit_note(file.config.history.max_bytes).into());
    s.set_can_browse(cfg!(windows));
    drop(file);

    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_autostart_toggled(move |on| apply(&w, &c, |cfg| cfg.system.autostart = on));
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_update_check_toggled(move |on| apply(&w, &c, |cfg| cfg.system.update_check = on));
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_log_text_toggled(move |on| apply(&w, &c, |cfg| cfg.debug.log_text = on));
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_app_language_picked(move |row| {
        if let Some((language, _)) = usize::try_from(row).ok().and_then(|r| APP_LANGUAGES.get(r)) {
            apply(&w, &c, |cfg| cfg.ui.language = *language);
        }
    });
    let (w, dir) = (window.as_weak(), ctx.layout.config_dir.clone());
    s.on_open_settings_folder(move || open_folder(&w, &dir));
    let (w, dir) = (window.as_weak(), ctx.layout.logs_dir.clone());
    s.on_open_logs_folder(move || open_folder(&w, &dir));

    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_history_toggled(move |on| apply(&w, &c, |cfg| cfg.history.enabled = on));
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_history_days_changed(move |days| apply(&w, &c, |cfg| cfg.history.max_days = whole(days)));
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_history_gb_changed(move |gb| {
        apply(&w, &c, |cfg| {
            cfg.history.max_bytes = u64::from(whole(gb)) * GIB;
        });
        if let Some(window) = w.upgrade() {
            window
                .global::<SystemState>()
                .set_history_limit_note(SharedString::new());
        }
    });

    let check = Rc::new(Timer::default());
    show_folder(&window.as_weak(), ctx, &check);
    let (w, c, t) = (window.as_weak(), ctx.clone(), check.clone());
    s.on_history_check(move || show_folder(&w, &c, &t));
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_history_browse(move || browse(&w, &c, &check));
}

/// The "!" of a size limit the GB field can only show rounded up: the daemon trims to the stored
/// bytes, so the person must see them. Empty for whole GB (and 0, no limit).
fn limit_note(bytes: u64) -> String {
    if bytes.is_multiple_of(GIB) {
        return String::new();
    }
    #[expect(clippy::cast_precision_loss, reason = "one decimal digit is shown")]
    let size = match bytes {
        b if b < 1 << 20 => format!("{b} bytes"),
        b if b < GIB => format!("{:.1} MB", b as f64 / f64::from(1 << 20)),
        b => format!("{:.2} GB", b as f64 / GIB as f64),
    };
    format!(
        "The settings file limits recordings to {size}; the field shows it rounded up. Older recordings are removed past {size}."
    )
}

/// A value of a `0..999` field as the whole number it stands for.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a field holds a whole number, 0..999 or a larger stored limit"
)]
fn whole(value: f32) -> u32 {
    value.round().max(0.0) as u32
}

#[cfg(windows)]
fn browse(weak: &Weak<AppWindow>, ctx: &Ctx, check: &Rc<Timer>) {
    if let Some(path) = rfd::FileDialog::new().pick_folder() {
        match picked_folder(&path) {
            Ok(folder) => {
                apply(weak, ctx, |cfg| cfg.history.folder = folder);
                show_folder(weak, ctx, check);
            }
            Err(e) => report(weak, Err(e)),
        }
    }
}

/// The config value for a picked folder. The app's own [`history::OWNED_DIR`] (what Open shows)
/// stands for the folder around it: the daemon appends that name itself. A name that is not valid
/// text would be stored as U+FFFD and point elsewhere, so it is refused.
#[cfg(windows)]
fn picked_folder(path: &Path) -> Result<String, Error> {
    let own = path
        .file_name()
        .is_some_and(|n| n.eq_ignore_ascii_case(history::OWNED_DIR));
    let chosen = if own {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    chosen.to_str().map(str::to_owned).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "That folder's name is not valid text. Choose another one.",
        )
        .into()
    })
}

/// No native dialog on this OS: `can-browse` hides the button.
#[cfg(not(windows))]
fn browse(_: &Weak<AppWindow>, _: &Ctx, _: &Rc<Timer>) {}

/// Fills the folder row. The check touches the disk and a UNC path to an offline host can block,
/// so it runs off the UI thread; a newer check replaces the timer and drops the older answer.
/// shortcut: a folder that vanishes while the page stays open shows on the next visit; upgrade
/// when the daemon reports its save failures to this window.
fn show_folder(weak: &Weak<AppWindow>, ctx: &Ctx, check: &Rc<Timer>) {
    let Some(window) = weak.upgrade() else {
        return;
    };
    let s = window.global::<SystemState>();
    let cfg = ctx.config.borrow().config.history.clone();
    let chosen = PathBuf::from(cfg.folder.trim());
    let path = history::folder(&cfg, &ctx.layout.history_dir());
    s.set_history_path(path.to_string_lossy().as_ref().into());
    if !chosen.is_absolute() {
        check.stop();
        s.set_history_folder(SharedString::new());
        s.set_history_problem(SharedString::new());
        return;
    }
    s.set_history_folder(chosen.to_string_lossy().as_ref().into());
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        // The page is gone when the window closed first.
        let _ = tx.send(folder_problem(&chosen));
    });
    let (w, stop) = (weak.clone(), Rc::downgrade(check));
    check.start(TimerMode::Repeated, FOLDER_POLL, move || {
        let problem = match rx.try_recv() {
            Err(TryRecvError::Empty) => return,
            answer => answer.unwrap_or_default(),
        };
        if let Some(check) = stop.upgrade() {
            check.stop();
        }
        if let Some(window) = w.upgrade() {
            window
                .global::<SystemState>()
                .set_history_problem(problem.into());
        }
    });
}

/// Why the daemon would not save into `chosen`, empty when it would. A missing folder is fine
/// while the path to it can be reached (the daemon's `claim` creates it); it is gone only with its
/// drive or share. Then the folder, or what holds it, must take a file, and its
/// [`history::OWNED_DIR`] must be absent, empty or marked as the app's.
fn folder_problem(chosen: &Path) -> &'static str {
    if !chosen.is_dir() {
        return match chosen.parent() {
            Some(parent) if parent.is_dir() => writable(parent),
            _ => FOLDER_MISSING,
        };
    }
    let own = chosen.join(history::OWNED_DIR);
    if fs::symlink_metadata(own.join(history::MARKER)).is_ok_and(|m| m.is_file()) {
        return writable(&own);
    }
    match fs::read_dir(&own) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => writable(chosen),
        Ok(mut files) => {
            if files.next().is_none() {
                writable(&own)
            } else {
                FOLDER_REFUSED
            }
        }
        Err(_) => FOLDER_REFUSED,
    }
}

/// Empty when a file can be made in `dir` (and is removed again), else [`FOLDER_READ_ONLY`].
fn writable(dir: &Path) -> &'static str {
    let probe = dir.join(format!(".oxiyapper-write-test-{}", std::process::id()));
    if fs::write(&probe, b"").is_err() {
        return FOLDER_READ_ONLY;
    }
    let _ = fs::remove_file(probe);
    ""
}

/// The picker row of `language`.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)] // a handful of rows
fn language_row(language: AppLanguage) -> i32 {
    APP_LANGUAGES
        .iter()
        .position(|(l, _)| *l == language)
        .unwrap_or(0) as i32
}

fn open_folder(weak: &Weak<AppWindow>, dir: &Path) {
    report(weak, oxi_platform::open::open(dir).map_err(Error::from));
}

fn apply(weak: &Weak<AppWindow>, ctx: &Ctx, change: impl FnOnce(&mut Config)) {
    change(&mut ctx.config.borrow_mut().config);
    report(weak, ctx.save());
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    #[test]
    fn the_owned_folder_stands_for_its_parent_and_bad_names_are_refused() {
        let root = std::env::temp_dir().join(format!("oxi-picked-{}", std::process::id()));
        let own = root.join(history::OWNED_DIR);
        fs::create_dir_all(&own).unwrap();
        assert_eq!(picked_folder(&own).unwrap(), root.to_str().unwrap());
        assert_eq!(picked_folder(&root).unwrap(), root.to_str().unwrap());
        let lone = OsString::from_wide(&[u16::from(b'a'), 0xD800]);
        assert!(picked_folder(&root.join(lone)).is_err());
        fs::remove_dir_all(&root).unwrap();
    }
}
