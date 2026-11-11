//! Model page: the dictation language, the curated models as a gallery with their status on disk,
//! download, cancel and delete through the daemon (the page itself never touches the network),
//! "use my own .bin", the models folder and its partial downloads, the GPU and the unload.

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::Path;
use std::rc::{Rc, Weak as RcWeak};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::SystemTime;

use oxi_core::device::{Device, DeviceChoice};
use oxi_core::ipc::{DeleteResult, DownloadResult, Push, Request};
use oxi_core::languages::{self, LanguageInfo};
use oxi_core::models::{self, MODELS, ModelEntry, ModelStatus, PartialDownload};
use oxi_core::paths::PART_SUFFIX;
use oxi_core::strings::{self, gb};
use oxi_core::types::Language;
use oxi_platform::fs::list_partials;
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel, Weak};

use crate::constants::FOLDER_POLL;
use crate::generated::{GalleryTile, ModelRow, ModelState, Toast};
use crate::ipc::{Event, Link, Subscription};
use crate::{AppWindow, Ctx, Error, Page, report};

/// `[model] id` of "use my own .bin".
const CUSTOM: &str = "custom";
/// "Unload after idle" choices in picker order: `None` is right after a dictation
/// (`unload_immediately`), `Some(0)` never, the rest minutes.
const UNLOAD: [Option<u32>; 8] = [
    None,
    Some(1),
    Some(5),
    Some(10),
    Some(15),
    Some(30),
    Some(60),
    Some(0),
];
/// Why "Delete partial downloads" is off.
const PARTIALS_NONE: &str = "No partial downloads";
const PARTIALS_BUSY: &str = "Wait until the download stops";
/// Why a model's Delete is off without the daemon, which may hold the file open.
const DELETE_NEEDS_DAEMON: &str = "Start OxiYapper to delete.";
/// A delete the daemon refused: a dictation, a test or a download is using the model.
const DELETE_IN_USE: &str = "In use right now. Try again when it is done.";

pub fn bind(window: &AppWindow, ctx: &Ctx) {
    let s = window.global::<ModelState>();
    let entries: Vec<&'static ModelEntry> = MODELS.iter().filter(|m| m.id != "tiny").collect();
    let dir = ctx.layout.models_dir();
    let now = SystemTime::now();
    let rows: Vec<ModelRow> = entries.iter().map(|e| row(e, &dir, None, now)).collect();
    let (tx, rx) = channel();
    let page = Rc::new_cyclic(|me: &RcWeak<PageState>| PageState {
        me: me.clone(),
        // A rebind (Reset) replaces the callbacks below, which drops this page and with it
        // this subscription.
        _events: ctx.ipc.subscribe({
            let me = me.clone();
            move |event| {
                if let Some(page) = me.upgrade() {
                    page.event(event);
                }
            }
        }),
        window: window.as_weak(),
        ctx: ctx.clone(),
        entries,
        rows: Rc::new(VecModel::from(rows)),
        tiles: Rc::new(VecModel::default()),
        partials: RefCell::new(Vec::new()),
        listed: RefCell::new(None),
        choices: RefCell::new(Vec::new()),
        unloads: RefCell::new(Vec::new()),
        // Not a pair the config can hold: the first `push_language` takes the config's.
        slots: Cell::new([Language::AUTO; 2]),
        found: RefCell::new(Vec::new()),
        checks: Checks {
            latest: Cell::new(0),
            running: Cell::new(0),
            tx,
            rx,
            timer: Timer::default(),
        },
    });
    s.set_rows(ModelRc::from(page.rows.clone()));
    s.set_tiles(ModelRc::from(page.tiles.clone()));
    s.set_can_browse(cfg!(windows));
    s.set_warning(SharedString::new());
    s.set_download_off(download_off(ctx.ipc.link()).into());
    s.set_delete_off(delete_off(ctx.ipc.link()).into());
    page.refresh_rows();
    page.push_config(&s);

    page.language_search("");

    let p = Rc::clone(&page);
    s.on_language_picked(move |i| p.language_picked(i));
    let p = Rc::clone(&page);
    s.on_language_search(move |text| p.language_search(&text));
    let p = Rc::clone(&page);
    s.on_language_chosen(move |i| p.language_chosen(i));
    let p = Rc::clone(&page);
    s.on_shown(move || p.shown());
    let p = Rc::clone(&page);
    s.on_picked(move |i| p.picked(i));
    let p = Rc::clone(&page);
    s.on_download(move |i| p.download(i));
    let p = Rc::clone(&page);
    s.on_cancel(move |i| p.cancel(i));
    let p = Rc::clone(&page);
    s.on_delete(move |i| p.delete(i));
    let p = Rc::clone(&page);
    s.on_custom_edited(move |text| p.custom_edited(&text));
    let p = Rc::clone(&page);
    s.on_browse(move |path| p.browse(&path));
    let p = Rc::clone(&page);
    s.on_open_folder(move || p.open_folder());
    let p = Rc::clone(&page);
    s.on_delete_partials(move || p.delete_partials());
    let p = Rc::clone(&page);
    s.on_gpu_picked(move |i| p.gpu_picked(i));
    let p = Rc::clone(&page);
    s.on_unload_picked(move |i| p.unload_picked(i));

    // Reset to defaults rebinds while the page may be open; its `init` does not run again.
    if window.get_page() == Page::Model {
        page.shown();
    }
}

struct PageState {
    me: RcWeak<Self>,
    _events: Subscription,
    window: Weak<AppWindow>,
    ctx: Ctx,
    /// The registry models the page offers, in row order.
    entries: Vec<&'static ModelEntry>,
    rows: Rc<VecModel<ModelRow>>,
    /// The gallery: a tile per row, then "use my own .bin".
    tiles: Rc<VecModel<GalleryTile>>,
    /// The `.part` files of the models folder as last read.
    partials: RefCell<Vec<PartialDownload>>,
    /// The daemon's GPU list; `None` until it answers `ListDevices`.
    listed: RefCell<Option<Vec<Device>>>,
    /// What each GPU picker row writes.
    choices: RefCell<Vec<DeviceChoice>>,
    /// What each unload picker row writes, as in [`UNLOAD`].
    unloads: RefCell<Vec<Option<u32>>>,
    /// The two language segments as shown, see [`languages::slots`].
    slots: Cell<[Language; 2]>,
    /// The rows of the language popup: what the search matched.
    found: RefCell<Vec<&'static LanguageInfo>>,
    checks: Checks,
}

/// A path's existence check, run on a worker thread: a UNC path to an offline host can block
/// for a long time, and the window must not freeze with it.
struct Checks {
    /// Id of the newest check; an older answer is dropped.
    latest: Cell<u64>,
    /// Workers not answered yet; the poll timer runs while it is above zero.
    running: Cell<usize>,
    tx: Sender<Checked>,
    rx: Receiver<Checked>,
    timer: Timer,
}

struct Checked {
    id: u64,
    path: String,
    problem: Option<&'static str>,
    /// From an edit: a valid path is then saved and picks "use my own .bin".
    save: bool,
}

impl PageState {
    fn state(&self, f: impl FnOnce(&ModelState<'_>)) {
        if let Some(window) = self.window.upgrade() {
            f(&window.global::<ModelState>());
        }
    }

    /// Ask the daemon for its GPUs and warnings; dropped while it is not connected.
    fn ask(&self) {
        self.ctx.ipc.send(Request::ListDevices);
        self.ctx.ipc.send(Request::Status);
    }

    /// The page was opened: read the files again (a model may have been copied in) and ask.
    fn shown(&self) {
        self.refresh_rows();
        self.state(|s| self.push_config(s));
        self.ask();
    }

    /// The model in use: an index in `entries`, `entries.len()` for "use my own .bin".
    fn in_use(&self) -> Option<usize> {
        let file = self.ctx.config.borrow();
        let id = &file.config.model.id;
        if id == CUSTOM {
            Some(self.entries.len())
        } else {
            self.entries.iter().position(|e| e.id == id)
        }
    }

    /// Every config value the page shows; the gallery opens on the model in use.
    fn push_config(&self, s: &ModelState<'_>) {
        let selected = self.in_use();
        s.set_selected(selected.map_or(-1, int));
        s.set_viewed(int(selected.unwrap_or(0)));
        self.push_tiles();
        let file = self.ctx.config.borrow();
        let model = &file.config.model;
        s.set_custom_path(model.custom_path.as_str().into());
        let unload = (!model.unload_immediately).then_some(model.idle_unload_minutes);
        let path = model.custom_path.clone();
        drop(file);
        let (labels, values) = unload_rows(unload);
        s.set_unload_options(ModelRc::from(Rc::new(VecModel::from(labels))));
        s.set_unload(values.iter().position(|v| *v == unload).map_or(0, int));
        *self.unloads.borrow_mut() = values;
        self.check_custom(&path, false);
        self.push_gpus(s);
        self.push_language(s);
    }

    /// The language segments and which of them is on.
    fn push_language(&self, s: &ModelState<'_>) {
        let (current, recent) = {
            let file = self.ctx.config.borrow();
            (
                file.config.model.language,
                file.config.model.recent_languages,
            )
        };
        let slots = languages::slots(self.slots.get(), current, recent);
        self.slots.set(slots);
        let strings = |f: fn(Language) -> SharedString| {
            ModelRc::from(Rc::new(VecModel::from(slots.map(f).to_vec())))
        };
        s.set_language_codes(strings(|l| l.as_str().to_uppercase().into()));
        s.set_language_names(strings(|l| l.info().map_or("", |i| i.english).into()));
        s.set_language(slots.iter().position(|l| *l == current).map_or(2, int));
        s.set_language_row(self.language_row(current));
    }

    fn language_row(&self, current: Language) -> i32 {
        let found = self.found.borrow();
        found
            .iter()
            .position(|i| i.code == current.as_str())
            .map_or(-1, int)
    }

    /// A segment: 0 and 1 the two languages shown, anything else Auto.
    fn language_picked(&self, i: i32) {
        let slot = usize::try_from(i)
            .ok()
            .and_then(|i| self.slots.get().get(i).copied());
        self.use_language(slot.unwrap_or(Language::AUTO));
    }

    fn language_search(&self, query: &str) {
        let found = languages::search(query);
        let column = |f: fn(&LanguageInfo) -> &str| {
            let rows: Vec<SharedString> = found.iter().map(|i| f(i).into()).collect();
            ModelRc::from(Rc::new(VecModel::from(rows)))
        };
        let (rows, native) = (column(|i| i.english), column(|i| i.native));
        *self.found.borrow_mut() = found;
        let current = self.ctx.config.borrow().config.model.language;
        self.state(|s| {
            s.set_language_rows(rows);
            s.set_language_native(native);
            s.set_language_row(self.language_row(current));
        });
    }

    /// A row of the popup.
    fn language_chosen(&self, i: i32) {
        let found = usize::try_from(i)
            .ok()
            .and_then(|i| self.found.borrow().get(i).copied());
        if let Some(lang) = found.and_then(|info| info.code.parse().ok()) {
            self.use_language(lang);
        }
    }

    fn use_language(&self, lang: Language) {
        {
            let mut file = self.ctx.config.borrow_mut();
            let model = &mut file.config.model;
            // One set by hand is not in `recent_languages`: it enters now, as the one just used,
            // so the pick that replaces it does not lose it.
            let before = model.language;
            if before != Language::AUTO && !model.recent_languages.contains(&before) {
                model.use_language(before);
            }
            model.use_language(lang);
        }
        self.state(|s| self.push_language(s));
        report(&self.window, self.ctx.save());
    }

    fn push_gpus(&self, s: &ModelState<'_>) {
        let current = self.ctx.config.borrow().config.model.gpu.clone();
        let (labels, choices) = gpu_rows(self.listed.borrow().as_deref(), &current);
        let index = choices.iter().position(|c| *c == current).unwrap_or(0);
        s.set_gpus(ModelRc::from(Rc::new(VecModel::from(labels))));
        s.set_gpu(int(index));
        *self.choices.borrow_mut() = choices;
    }

    fn update(&self, i: usize, f: impl FnOnce(&mut ModelRow)) {
        if let Some(mut r) = self.rows.row_data(i) {
            f(&mut r);
            self.rows.set_row_data(i, r);
            self.push_tiles();
        }
    }

    /// The gallery from the rows and the model in use; a tile that did not change is left alone,
    /// so a progress push does not rebuild the others.
    fn push_tiles(&self) {
        let in_use = self.in_use();
        let mut tiles: Vec<GalleryTile> = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| GalleryTile {
                title: r.name.clone(),
                caption: tile_caption(&r, in_use == Some(i)).into(),
                ..GalleryTile::default()
            })
            .collect();
        tiles.push(GalleryTile {
            title: "Use my own .bin".into(),
            caption: if in_use == Some(self.entries.len()) {
                IN_USE
            } else {
                "Loaded as is"
            }
            .into(),
            ..GalleryTile::default()
        });
        if self.tiles.row_count() != tiles.len() {
            self.tiles.set_vec(tiles);
            return;
        }
        for (i, tile) in tiles.into_iter().enumerate() {
            if self.tiles.row_data(i).as_ref() != Some(&tile) {
                self.tiles.set_row_data(i, tile);
            }
        }
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.entries.iter().position(|e| e.id == id)
    }

    fn downloading(&self) -> bool {
        self.rows.iter().any(|r| r.downloading)
    }

    /// Whether a `.part` may be written now: a download this page heard of, or a link that does
    /// not say the daemon is gone (Connecting, Busy, Mismatch, Stuck and the rest may still be
    /// writing). Only Online, where the daemon reports its downloads, and Offline, with no
    /// daemon, are sure.
    fn may_be_writing(&self) -> bool {
        self.downloading() || !matches!(self.ctx.ipc.link(), Link::Online | Link::Offline)
    }

    fn stop_downloading(&self) {
        for i in 0..self.rows.row_count() {
            self.update(i, |r| r.downloading = false);
        }
        self.refresh_rows();
    }

    fn picked(&self, i: i32) {
        let Ok(i) = usize::try_from(i) else { return };
        let id = match self.entries.get(i) {
            Some(entry) => entry.id,
            None if i == self.entries.len() => CUSTOM,
            None => return,
        };
        id.clone_into(&mut self.ctx.config.borrow_mut().config.model.id);
        self.state(|s| self.push_config(s));
        report(&self.window, self.ctx.save());
    }

    /// Asks the daemon to delete the model's files (and its `.part`); the card asked first. The
    /// ready model in use is refused here too: its card offers no Delete. True when the request
    /// went out.
    fn delete(&self, i: i32) -> bool {
        let Some((i, entry)) = self.entry(i) else {
            return false;
        };
        let off = delete_off(self.ctx.ipc.link());
        let ready = self.rows.row_data(i).is_some_and(|r| r.ready);
        if !off.is_empty() || (ready && self.in_use() == Some(i)) {
            self.update(i, |r| r.error = off.into());
            return false;
        }
        self.update(i, |r| r.error = SharedString::new());
        self.ctx.ipc.send(Request::DeleteModel {
            id: entry.id.to_owned(),
        });
        true
    }

    fn deleted(&self, id: &str, result: &DeleteResult) {
        let Some(i) = self.index_of(id) else { return };
        let error = match result {
            DeleteResult::Done { freed } => {
                if let Some(window) = self.window.upgrade() {
                    let name = self.entries[i].display_name;
                    let note = format!("Deleted {name}, freed {}", size(*freed));
                    window.global::<Toast>().invoke_show(note.into());
                }
                String::new()
            }
            DeleteResult::InUse => DELETE_IN_USE.to_owned(),
            DeleteResult::Failed { message } => format!("Not deleted: {message}"),
        };
        self.update(i, |r| r.error = error.into());
        self.refresh_rows();
    }

    fn download(&self, i: i32) {
        let Some((i, entry)) = self.entry(i) else {
            return;
        };
        let off = download_off(self.ctx.ipc.link());
        if !off.is_empty() {
            self.update(i, |r| r.error = off.into());
            return;
        }
        self.ctx.ipc.send(Request::DownloadModel {
            id: entry.id.to_owned(),
        });
        self.update(i, |r| {
            r.downloading = true;
            r.progress = 0.0;
            r.progress_text = strings::download_progress(0, entry.size).into();
            r.badge = strings::MODEL_BADGE_DOWNLOADING.into();
            r.error = SharedString::new();
        });
        self.push_partials();
    }

    /// The row stays in "downloading" until the daemon's `DownloadDone` says it stopped.
    fn cancel(&self, i: i32) {
        if let Some((_, entry)) = self.entry(i) {
            self.ctx.ipc.send(Request::CancelDownload {
                id: entry.id.to_owned(),
            });
        }
    }

    fn entry(&self, i: i32) -> Option<(usize, &'static ModelEntry)> {
        let i = usize::try_from(i).ok()?;
        Some((i, *self.entries.get(i)?))
    }

    /// A valid path is saved and picks "use my own .bin"; anything else only shows why not.
    fn custom_edited(&self, text: &str) {
        self.check_custom(clean(text), true);
    }

    /// Show why `path` cannot be a custom model. What the text alone decides shows at once;
    /// the file itself is looked at on a worker and shown (and saved, with `save`) when it
    /// answers.
    fn check_custom(&self, path: &str, save: bool) {
        let id = self.checks.latest.get() + 1;
        self.checks.latest.set(id);
        if let Some(problem) = shape_problem(path) {
            // An empty field is only an error while "use my own .bin" is picked.
            let custom = self.ctx.config.borrow().config.model.id == CUSTOM;
            let note = if custom || !path.is_empty() {
                problem
            } else {
                ""
            };
            self.state(|s| s.set_custom_error(note.into()));
            return;
        }
        self.state(|s| s.set_custom_error(SharedString::new()));
        let (tx, path) = (self.checks.tx.clone(), path.to_owned());
        self.checks.running.set(self.checks.running.get() + 1);
        thread::spawn(move || {
            let problem = custom_problem(&path);
            // The page is gone when the window closed first.
            let _ = tx.send(Checked {
                id,
                path,
                problem,
                save,
            });
        });
        let me = self.me.clone();
        self.checks
            .timer
            .start(TimerMode::Repeated, FOLDER_POLL, move || {
                if let Some(page) = me.upgrade() {
                    page.poll();
                }
            });
    }

    fn poll(&self) {
        while let Ok(answer) = self.checks.rx.try_recv() {
            self.checks
                .running
                .set(self.checks.running.get().saturating_sub(1));
            if answer.id == self.checks.latest.get() {
                self.checked(&answer);
            }
        }
        if self.checks.running.get() == 0 {
            self.checks.timer.stop();
        }
    }

    fn checked(&self, answer: &Checked) {
        if answer.problem.is_none() && answer.save {
            let mut file = self.ctx.config.borrow_mut();
            answer
                .path
                .as_str()
                .clone_into(&mut file.config.model.custom_path);
            CUSTOM.clone_into(&mut file.config.model.id);
            drop(file);
            report(&self.window, self.ctx.save());
            self.state(|s| s.set_selected(int(self.entries.len())));
            self.push_tiles();
        }
        self.state(|s| s.set_custom_error(answer.problem.unwrap_or_default().into()));
    }

    /// The dialog starts at the folder of `path`, the field's text (DS-13.4).
    #[cfg(windows)]
    fn browse(&self, path: &str) {
        let mut dialog = rfd::FileDialog::new().add_filter("Whisper model", &["bin"]);
        if let Some(dir) = start_dir(clean(path), &self.ctx.layout.models_dir()) {
            dialog = dialog.set_directory(dir);
        }
        let picked = dialog.pick_file();
        if let Some(path) = picked {
            let text = path.to_string_lossy().into_owned();
            self.state(|s| s.set_custom_path(text.as_str().into()));
            self.custom_edited(&text);
        }
    }

    /// No native dialog on this OS: `can-browse` hides the button.
    #[cfg(not(windows))]
    #[allow(clippy::unused_self, reason = "same signature as the Windows dialog")]
    fn browse(&self, _path: &str) {}

    /// Opens the models folder, made first when nothing was downloaded yet.
    fn open_folder(&self) {
        let dir = self.ctx.layout.models_dir();
        let opened = fs::create_dir_all(&dir).and_then(|()| oxi_platform::open::open(&dir));
        report(&self.window, opened.map_err(Error::from));
    }

    /// Removes every `.part` of the models folder; off while a download may write one.
    /// shortcut: "downloading" is what this page has heard. The daemon replays every active
    /// download when the link comes Online (even one with no bytes yet), so a reopen is exposed
    /// only until that replay; ask the daemon if that gap ever matters.
    fn delete_partials(&self) {
        if self.may_be_writing() {
            return;
        }
        let dir = self.ctx.layout.models_dir();
        let (mut freed, mut failed) = (0, None);
        for part in list_partials(&dir).unwrap_or_default() {
            match fs::remove_file(dir.join(format!("{}{PART_SUFFIX}", part.file_name))) {
                Ok(()) => freed += part.size,
                Err(e) => failed = Some(e),
            }
        }
        self.refresh_rows();
        if let Some(e) = failed {
            report(&self.window, Err(Error::from(e)));
        } else if let Some(window) = self.window.upgrade() {
            let note = format!("Deleted {} of partial downloads", size(freed));
            window.global::<Toast>().invoke_show(note.into());
        }
    }

    fn gpu_picked(&self, i: i32) {
        let choice = usize::try_from(i)
            .ok()
            .and_then(|i| self.choices.borrow().get(i).cloned());
        if let Some(choice) = choice {
            self.ctx.config.borrow_mut().config.model.gpu = choice;
            report(&self.window, self.ctx.save());
            self.refresh_rows();
        }
    }

    fn unload_picked(&self, i: i32) {
        let Some(choice) = usize::try_from(i)
            .ok()
            .and_then(|i| self.unloads.borrow().get(i).copied())
        else {
            return;
        };
        let mut file = self.ctx.config.borrow_mut();
        let model = &mut file.config.model;
        model.unload_immediately = choice.is_none();
        // "Right after dictation" keeps the minutes, so picking a time later starts from them.
        if let Some(minutes) = choice {
            model.idle_unload_minutes = minutes;
        }
        drop(file);
        report(&self.window, self.ctx.save());
    }

    fn event(&self, event: &Event) {
        if let Event::Link(link) = event {
            self.state(|s| {
                s.set_download_off(download_off(*link).into());
                s.set_delete_off(delete_off(*link).into());
            });
        }
        match event {
            // The daemon replays its active downloads right after this, so a flag left from an
            // older daemon (one that died while the link was Stuck) goes now.
            Event::Link(Link::Online) => {
                self.stop_downloading();
                self.ask();
            }
            // No daemon: nothing downloads or writes.
            Event::Link(Link::Offline) => self.stop_downloading(),
            // Connecting, Busy, Mismatch, Stuck: the daemon may still be writing, so the flags
            // stay and Delete goes off with the link.
            Event::Link(_) => self.push_partials(),
            Event::Push(Push::Devices { gpus, .. }) => {
                *self.listed.borrow_mut() = Some(gpus.clone());
                self.state(|s| self.push_gpus(s));
                self.refresh_rows();
            }
            Event::Push(Push::Status { warnings, .. }) => {
                // The sound lines are the Audio page's (audio::sound_warning).
                let ours: Vec<&str> = warnings
                    .iter()
                    .map(String::as_str)
                    .filter(|l| !l.starts_with("sounds."))
                    .collect();
                self.state(|s| s.set_warning(ours.join("\n").into()));
            }
            Event::Push(Push::DownloadProgress { id, done, total }) => {
                if let Some(i) = self.index_of(id) {
                    self.update(i, |r| {
                        r.downloading = true;
                        r.progress = fraction(*done, *total);
                        r.progress_text = strings::download_progress(*done, *total).into();
                        r.badge = strings::MODEL_BADGE_DOWNLOADING.into();
                    });
                    self.push_partials();
                }
            }
            Event::Push(Push::DownloadDone { id, result }) => {
                if let Some(i) = self.index_of(id) {
                    self.update(i, |r| {
                        r.downloading = false;
                        r.error = match result {
                            DownloadResult::Failed { message } => {
                                strings::download_failed(message).into()
                            }
                            DownloadResult::Ok | DownloadResult::Cancelled => SharedString::new(),
                        };
                    });
                    self.refresh_rows();
                }
            }
            Event::Push(Push::ModelDeleted { id, result }) => self.deleted(id, result),
            Event::Push(_) => {}
        }
    }

    /// Every row's state from the files (a downloading row keeps its own), its weight warning
    /// for the GPU picked, and the partial downloads.
    fn refresh_rows(&self) {
        let dir = self.ctx.layout.models_dir();
        // A folder that cannot be listed shows the rows without their partial details.
        let partials = list_partials(&dir).unwrap_or_default();
        let now = SystemTime::now();
        let gpu = self.ctx.config.borrow().config.model.gpu.clone();
        let listed = self.listed.borrow();
        for (i, entry) in self.entries.iter().enumerate() {
            let part = partials.iter().find(|p| p.file_name == entry.file_name);
            self.update(i, |r| {
                r.heavy = models::too_heavy(entry, listed.as_deref(), &gpu);
                if !r.downloading {
                    let fresh = row(entry, &dir, part, now);
                    r.detail = fresh.detail;
                    r.badge = fresh.badge;
                    r.ready = fresh.ready;
                    r.can_download = fresh.can_download;
                    r.paused = fresh.paused;
                    r.can_delete = fresh.can_delete;
                    r.frees = fresh.frees;
                }
            });
        }
        drop(listed);
        *self.partials.borrow_mut() = partials;
        self.push_partials();
    }

    /// The "Delete partial downloads" button: off with nothing to delete or while a download
    /// writes its `.part`; its tooltip says how much it frees.
    fn push_partials(&self) {
        let partials = self.partials.borrow();
        let off = if partials.is_empty() {
            PARTIALS_NONE
        } else if self.may_be_writing() {
            PARTIALS_BUSY
        } else {
            ""
        };
        let bytes = partials.iter().map(|p| p.size).sum();
        let files = match partials.len() {
            1 => "1 file".to_owned(),
            n => format!("{n} files"),
        };
        let tip = format!("{} in {files}", size(bytes));
        self.state(|s| {
            s.set_partials_off(off.into());
            s.set_partials(tip.into());
        });
    }
}

/// Decimal MB below a gigabyte, so a small `.part` is not "0.0 GB"; `gb` from there.
fn size(bytes: u64) -> String {
    if bytes < 1_000_000_000 {
        format!("{} MB", bytes / 1_000_000)
    } else {
        gb(bytes)
    }
}

/// A row from the files; `part` is the model's `.part` as listed, for the paused caption.
fn row(
    entry: &ModelEntry,
    dir: &Path,
    part: Option<&PartialDownload>,
    now: SystemTime,
) -> ModelRow {
    let status = models::status(entry, dir);
    // What to do about a state the badge only names takes the place of the hint.
    let note = match strings::model_status(status) {
        "" => strings::model_hint(entry.id),
        todo => todo,
    };
    let detail = match part.filter(|_| status == ModelStatus::Partial) {
        Some(part) => format!(
            "{} · {}",
            strings::download_progress(part.size, entry.size),
            kept(models::days_kept(part.remove_at, now))
        ),
        None if note.is_empty() => gb(entry.size),
        None => format!("{} · {note}", gb(entry.size)),
    };
    let on_disk =
        fs::metadata(dir.join(entry.file_name)).map_or(0, |m| m.len()) + part.map_or(0, |p| p.size);
    ModelRow {
        id: entry.id.into(),
        name: entry.display_name.into(),
        detail: detail.into(),
        progress_text: SharedString::new(),
        badge: strings::model_badge(status).into(),
        ready: matches!(status, ModelStatus::Ready | ModelStatus::NeedsHash),
        can_download: matches!(
            status,
            ModelStatus::Missing | ModelStatus::Partial | ModelStatus::Bad
        ),
        paused: status == ModelStatus::Partial,
        downloading: false,
        progress: 0.0,
        error: SharedString::new(),
        heavy: false,
        can_delete: status != ModelStatus::Missing,
        frees: size(on_disk).into(),
    }
}

/// What the model in use's tile says beside its state.
const IN_USE: &str = "In use";

/// A tile's caption: its state, "Downloading 45%" while it downloads, "In use" first for the
/// model in use (alone when it is ready, as nothing is to be done).
fn tile_caption(r: &ModelRow, in_use: bool) -> String {
    let state = if r.downloading {
        format!(
            "{} {:.0}%",
            strings::MODEL_BADGE_DOWNLOADING,
            r.progress * 100.0
        )
    } else {
        r.badge.to_string()
    };
    match (in_use, r.ready && !r.downloading) {
        (true, true) => IN_USE.to_owned(),
        (true, false) => format!("{IN_USE} · {state}"),
        (false, _) => state,
    }
}

/// Where a file dialog for `path` starts: its folder, else the nearest folder above it that is
/// there, else the models folder (or the nearest one above it). `None` only when none exists.
/// shortcut: `is_dir` runs on the UI thread and an offline network path can stall it; the dialog
/// that follows is modal anyway. Move it to the path worker if that ever shows.
#[cfg(any(windows, test))] // only the Windows file dialog uses it
fn start_dir(path: &str, models: &Path) -> Option<std::path::PathBuf> {
    let path = Path::new(path);
    let from_path = path
        .is_absolute()
        .then(|| path.parent())
        .flatten()
        .into_iter()
        .flat_map(Path::ancestors);
    from_path
        .chain(models.ancestors())
        .find(|dir| dir.is_dir())
        .map(Path::to_path_buf)
}

/// How long a paused download stays: "kept 7 more days", or gone at the next start.
fn kept(days: u64) -> String {
    match days {
        0 => "removed at the next start".to_owned(),
        1 => "kept 1 more day".to_owned(),
        n => format!("kept {n} more days"),
    }
}

/// Unload picker labels and what each writes: [`UNLOAD`], with the configured minutes added in
/// order when they are not one of them (an older slider value), so they stay picked.
fn unload_rows(current: Option<u32>) -> (Vec<SharedString>, Vec<Option<u32>>) {
    let mut values = UNLOAD.to_vec();
    if !values.contains(&current) {
        let at = values
            .iter()
            .position(|v| v.is_some_and(|m| m == 0 || current.is_some_and(|c| m > c)))
            .unwrap_or(values.len());
        values.insert(at, current);
    }
    let labels = values
        .iter()
        .map(|v| match v {
            None => SharedString::from("Right after dictation"),
            Some(0) => SharedString::from("Never"),
            Some(m) => format!("After {m} min").into(),
        })
        .collect();
    (labels, values)
}

/// Why Download is off on `link`; empty while it is on.
fn download_off(link: Link) -> &'static str {
    link.off_reason(strings::DOWNLOAD_NEEDS_DAEMON)
}

/// Why a model's Delete is off on `link`; empty while it is on.
fn delete_off(link: Link) -> &'static str {
    link.off_reason(DELETE_NEEDS_DAEMON)
}

fn fraction(done: u64, total: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        reason = "a bar position"
    )]
    let f = (done as f64 / total as f64).clamp(0.0, 1.0) as f32;
    f
}

fn int(i: usize) -> i32 {
    i32::try_from(i).unwrap_or(i32::MAX)
}

/// Explorer's "Copy as path" wraps the path in quotes.
fn clean(text: &str) -> &str {
    text.trim().trim_matches('"')
}

/// Why `path` cannot be a custom model, from the text alone; `None` when it may be.
fn shape_problem(path: &str) -> Option<&'static str> {
    if path.is_empty() {
        return Some(strings::MODEL_PATH_EMPTY);
    }
    let path = Path::new(path);
    if !path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("bin"))
    {
        return Some(strings::MODEL_PATH_NOT_BIN);
    }
    // The daemon resolves a relative path against its own working directory, not ours.
    if !path.is_absolute() {
        return Some(strings::MODEL_PATH_RELATIVE);
    }
    None
}

/// Why `path` cannot be loaded as a custom model; `None` when it can. Reads the disk, which a
/// network path can stall: never on the UI thread.
fn custom_problem(path: &str) -> Option<&'static str> {
    if let Some(problem) = shape_problem(path) {
        return Some(problem);
    }
    match fs::metadata(path) {
        Ok(meta) if meta.is_file() => None,
        Ok(_) => Some(strings::MODEL_PATH_FOLDER),
        Err(_) => Some(strings::MODEL_PATH_MISSING),
    }
}

/// Picker labels and what each writes: Auto, CPU, the listed GPUs by their display name, then the configured GPU
/// when it is not listed, so it stays picked. Before the daemon answers (`listed` is `None`) a
/// configured name is not called missing.
fn gpu_rows(
    listed: Option<&[Device]>,
    current: &DeviceChoice,
) -> (Vec<SharedString>, Vec<DeviceChoice>) {
    let mut rows = vec![
        ("Auto".to_owned(), DeviceChoice::Auto),
        ("CPU".to_owned(), DeviceChoice::Cpu),
    ];
    for d in listed.unwrap_or_default() {
        rows.push((
            d.display_name().to_owned(),
            DeviceChoice::Named(d.name.clone()),
        ));
    }
    if let DeviceChoice::Named(name) = current
        && !rows.iter().any(|(_, c)| c == current)
    {
        let label = if listed.is_some() {
            strings::not_connected(name)
        } else {
            name.clone()
        };
        rows.push((label, current.clone()));
    }
    rows.into_iter()
        .map(|(l, c)| (SharedString::from(l), c))
        .unzip()
}

#[cfg(test)]
mod tests {
    #[test]
    fn download_tooltip_follows_the_link_state() {
        let off = download_off;
        assert_eq!(off(Link::Online), "");
        for link in [Link::Offline, Link::Stuck] {
            assert_eq!(off(link), "Start OxiYapper to download.", "{link:?}");
        }
        assert_eq!(
            off(Link::Mismatch { daemon_proto: 9 }),
            "Restart OxiYapper first"
        );
        assert_eq!(off(Link::Busy), strings::DAEMON_BUSY);
    }

    use std::sync::mpsc::Receiver;
    use std::time::Duration;

    use i_slint_backend_testing::{AccessibleRole, ElementHandle, mock_elapsed_time};
    use oxi_core::config::ConfigFile;
    use oxi_core::machine::{EngineWarning, Phase};
    use oxi_core::paths::Layout;
    use oxi_core::strings::engine_warning;

    use super::*;
    use crate::ipc::{Cmd, Ipc};
    use crate::testing::{has, linked, spots, with_role};

    /// The settings window on a fresh config dir and a fake daemon link, on the Model page.
    fn window(name: &str) -> (AppWindow, Ipc, Receiver<Cmd>, Layout) {
        let opened = linked("model", name, Page::Model);
        // Building the page runs its `init`, which asks the daemon.
        assert!(has(&opened.0, "Use my own .bin"));
        opened
    }

    /// Wait for the path worker: step the mocked clock (which runs the page's poll timer) until
    /// `ready`.
    fn wait_for(ready: impl Fn() -> bool) {
        for _ in 0..400 {
            mock_elapsed_time(FOLDER_POLL);
            if ready() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("the path check did not answer");
    }

    fn requests(sent: &Receiver<Cmd>) -> Vec<Request> {
        sent.try_iter()
            .filter_map(|c| match c {
                Cmd::Send(r) => Some(r),
                Cmd::Start | Cmd::Restart => None,
            })
            .collect()
    }

    fn push(ipc: &Ipc, push: Push) {
        ipc.deliver(&Event::Push(push));
    }

    fn progress(id: &str, done: u64, total: u64) -> Push {
        Push::DownloadProgress {
            id: id.into(),
            done,
            total,
        }
    }

    fn done(id: &str, result: DownloadResult) -> Push {
        Push::DownloadDone {
            id: id.into(),
            result,
        }
    }

    /// Picks the tile: its card shows below the gallery.
    fn view(window: &AppWindow, name: &str) {
        radio(window, name).invoke_accessible_default_action();
    }

    fn click(window: &AppWindow, label: &str) {
        with_role(window, label, AccessibleRole::Button).invoke_accessible_default_action();
    }

    fn download_large(window: &AppWindow) {
        view(window, "Whisper large-v3");
        click(window, "Download Whisper large-v3");
    }

    /// The large-v3 progress bar's value, `None` while it has none.
    fn bar(window: &AppWindow) -> Option<String> {
        ElementHandle::find_by_accessible_label(window, "Whisper large-v3 download")
            .find(|e| e.accessible_role() == Some(AccessibleRole::ProgressIndicator))
            .and_then(|e| e.accessible_value().map(String::from))
    }

    #[test]
    fn download_sends_its_id_and_progress_moves_the_bar() {
        let (window, ipc, sent, _) = window("progress");
        let asked = requests(&sent);
        assert!(asked.contains(&Request::ListDevices) && asked.contains(&Request::Status));

        download_large(&window);
        let want = Request::DownloadModel {
            id: "large-v3".into(),
        };
        assert_eq!(requests(&sent), [want]);
        assert_eq!(bar(&window).as_deref(), Some("0"));

        push(&ipc, progress("large-v3", 1_000_000_000, 4_000_000_000));
        assert_eq!(bar(&window).as_deref(), Some("0.25"));
        assert!(has(&window, "1.0 of 4.0 GB"), "progress beside the bar");
        push(&ipc, progress("large-v3", 3_000_000_000, 4_000_000_000));
        assert_eq!(bar(&window).as_deref(), Some("0.75"));
        // Another model's progress leaves this bar alone.
        push(&ipc, progress("turbo", 1, 4));
        assert_eq!(bar(&window).as_deref(), Some("0.75"));
    }

    #[test]
    fn failed_download_shows_its_text() {
        let (window, ipc, _sent, _) = window("failed");
        view(&window, "Whisper large-v3");
        let message = "connection reset (os error 10054)";
        push(&ipc, progress("large-v3", 5, 10));
        assert!(bar(&window).is_some());
        let failed = DownloadResult::Failed {
            message: message.into(),
        };
        push(&ipc, done("large-v3", failed));
        // The text is the "!" of the card, so the card does not grow.
        let text = strings::download_failed(message);
        assert!(has(&window, &text));
        assert_eq!(bar(&window), None);
        assert!(has(&window, "Whisper large-v3: Not downloaded"));
        // Trying again clears the old error.
        download_large(&window);
        assert!(!has(&window, &text));
    }

    /// UI rule 1's check: whatever both snapshots have keeps its y and height. Only widths may
    /// change: a badge or button with another text is wider or narrower.
    fn assert_rows_kept(
        before: Vec<(String, Option<AccessibleRole>, [f32; 4])>,
    ) -> impl Fn(&AppWindow) {
        move |window| {
            let after = spots(window);
            let mut compared = 0;
            for (label, role, [_, y, _, h]) in &before {
                let same: Vec<_> = after
                    .iter()
                    .filter(|a| &a.0 == label && &a.1 == role)
                    .map(|a| (a.2[1], a.2[3]))
                    .collect();
                if same.is_empty() {
                    continue;
                }
                compared += 1;
                assert!(
                    same.contains(&(*y, *h)),
                    "{label} moved: y, h {y}, {h} is now {same:?}"
                );
            }
            assert!(compared > 3, "only {compared} elements compared");
        }
    }

    /// UI rule 1: a card keeps its height and the cards after it keep their place in every state.
    #[test]
    fn a_card_keeps_its_place_in_every_state() {
        let (window, ipc, _sent, layout) = window("states");
        // In use and not on disk: the "!" of the card warns, the card keeps its height.
        window.global::<ModelState>().invoke_picked(1);
        assert!(has(&window, NOT_ON_DISK));
        let kept = assert_rows_kept(spots(&window));
        push(&ipc, progress("large-v3", 5, 10));
        assert!(bar(&window).is_some());
        kept(&window);
        let failed = DownloadResult::Failed {
            message: "connection reset".into(),
        };
        push(&ipc, done("large-v3", failed));
        kept(&window);
        fs::create_dir_all(layout.models_dir()).unwrap();
        fs::write(layout.models_dir().join("ggml-large-v3.bin.part"), b"half").unwrap();
        push(&ipc, done("large-v3", DownloadResult::Cancelled));
        assert!(has(&window, "Whisper large-v3: Paused"));
        kept(&window);
    }

    fn radio(window: &AppWindow, label: &str) -> ElementHandle {
        with_role(window, label, AccessibleRole::RadioButton)
    }

    const NOT_ON_DISK: &str = "Dictation will not start until this is downloaded.";
    const SLOW: &str =
        "Likely slow without a dedicated graphics card. Whisper large-v3 turbo is faster.";

    fn gpus(ipc: &Ipc, gpus: &[(&str, bool)]) {
        let gpus = gpus
            .iter()
            .map(|(name, uma)| Device {
                name: (*name).into(),
                description: String::new(),
                uma: *uma,
            })
            .collect();
        push(
            ipc,
            Push::Devices {
                mics: Vec::new(),
                gpus,
            },
        );
    }

    #[test]
    fn heavy_models_warn_without_a_dedicated_gpu_and_still_download() {
        let (window, ipc, sent, _) = window("heavy");
        // No answer from the daemon yet: no warning.
        assert!(!has(&window, SLOW));
        gpus(&ipc, &[("Vulkan0", true)]);
        // Both 3.1 GB models warn, turbo does not.
        assert!(!has(&window, SLOW));
        view(&window, "Whisper large-v3 Russian");
        assert!(has(&window, SLOW));
        let _ = requests(&sent);
        download_large(&window);
        assert!(has(&window, SLOW));
        let want = Request::DownloadModel {
            id: "large-v3".into(),
        };
        assert_eq!(requests(&sent), [want], "the warning blocked the download");
        // The model in use, not on disk, says both.
        window.global::<ModelState>().invoke_picked(2);
        assert!(has(&window, &format!("{NOT_ON_DISK}\n{SLOW}")));
        gpus(&ipc, &[("Vulkan0", false), ("Vulkan1", true)]);
        assert!(!has(&window, SLOW) && !has(&window, &format!("{NOT_ON_DISK}\n{SLOW}")));
        // The CPU picked: slow again.
        window.global::<ModelState>().invoke_gpu_picked(1);
        assert!(has(&window, &format!("{NOT_ON_DISK}\n{SLOW}")));
    }

    #[test]
    fn partial_downloads_are_listed_and_deleted() {
        let (window, ipc, _sent, layout) = window("partials");
        let s = window.global::<ModelState>();
        assert_eq!(s.get_partials_off(), PARTIALS_NONE);
        let dir = layout.models_dir();
        fs::create_dir_all(&dir).unwrap();
        let part = dir.join("ggml-large-v3.bin.part");
        let other = dir.join("old-model.bin.part");
        fs::write(&part, vec![0; 1000]).unwrap();
        fs::write(&other, b"x").unwrap();
        s.invoke_shown();
        view(&window, "Whisper large-v3");
        assert_eq!(s.get_partials_off(), "");
        assert_eq!(s.get_partials(), "0 MB in 2 files");
        assert_eq!(size(3_000_000), "3 MB");
        assert_eq!(size(1_600_000_000), "1.6 GB");
        assert!(has(&window, "1 MB of 3.1 GB · kept 7 more days"));
        // A running download writes its `.part`: the button waits.
        push(&ipc, progress("turbo", 5, 10));
        assert_eq!(s.get_partials_off(), PARTIALS_BUSY);
        s.invoke_delete_partials();
        assert!(part.exists(), "deleted during a download");
        push(&ipc, done("turbo", DownloadResult::Cancelled));
        assert_eq!(s.get_partials_off(), "");
        s.invoke_delete_partials();
        assert!(!part.exists() && !other.exists());
        assert_eq!(s.get_partials_off(), PARTIALS_NONE);
        assert!(has(&window, "Whisper large-v3: Not downloaded"));
        assert_eq!(
            window.global::<Toast>().get_text(),
            "Deleted 0 MB of partial downloads"
        );
    }

    #[test]
    fn kept_days_read_as_words() {
        assert_eq!(kept(7), "kept 7 more days");
        assert_eq!(kept(1), "kept 1 more day");
        assert_eq!(kept(0), "removed at the next start");
    }

    #[test]
    fn download_button_is_not_inside_the_radio() {
        let (window, _ipc, _sent, _) = window("nodes");
        view(&window, "Whisper large-v3");
        let radio = with_role(&window, "Whisper large-v3", AccessibleRole::RadioButton);
        with_role(&window, "Download Whisper large-v3", AccessibleRole::Button);
        let inside = radio
            .query_descendants()
            .match_accessible_role(AccessibleRole::Button)
            .find_all();
        assert!(inside.is_empty(), "the Download button is inside the radio");
    }

    #[test]
    fn cancelled_download_with_a_part_file_shows_paused() {
        let (window, ipc, _sent, layout) = window("paused");
        view(&window, "Whisper large-v3");
        push(&ipc, progress("large-v3", 5, 10));
        fs::create_dir_all(layout.models_dir()).unwrap();
        fs::write(layout.models_dir().join("ggml-large-v3.bin.part"), b"half").unwrap();
        push(&ipc, done("large-v3", DownloadResult::Cancelled));
        // How much is there and how long it stays.
        assert!(has(&window, "1 MB of 3.1 GB · kept 7 more days"));
        with_role(&window, "Resume Whisper large-v3", AccessibleRole::Button);
        // A model with no `.part` stays "Not downloaded".
        view(&window, "Whisper large-v3 turbo");
        assert!(has(&window, "Whisper large-v3 turbo: Not downloaded"));
    }

    #[test]
    fn relative_custom_path_is_refused_and_not_saved() {
        let (window, _ipc, _sent, layout) = window("relative");
        view(&window, "Use my own .bin");
        let field = with_role(&window, "Model file", AccessibleRole::TextInput);
        field.set_accessible_value("mine.bin".to_owned());
        assert!(has(&window, RELATIVE));
        assert!(!layout.config_file().exists(), "a relative path was saved");
    }

    #[test]
    fn an_answer_for_an_older_edit_is_dropped() {
        let (window, _ipc, _sent, layout) = window("stale");
        view(&window, "Use my own .bin");
        let good = layout.config_dir.join("good.bin");
        fs::write(&good, b"synthetic").unwrap();
        let field = with_role(&window, "Model file", AccessibleRole::TextInput);
        field.set_accessible_value(good.to_string_lossy().into_owned());
        field.set_accessible_value("notes.txt".to_owned());
        for _ in 0..20 {
            mock_elapsed_time(FOLDER_POLL);
            thread::sleep(Duration::from_millis(10));
        }
        assert!(has(&window, "The model file must end in .bin"));
        assert!(!layout.config_file().exists(), "the old answer was saved");
    }

    fn saved_model(layout: &Layout) -> oxi_core::config::Model {
        let text = fs::read_to_string(layout.config_file()).unwrap();
        ConfigFile::parse(&text).unwrap().config.model
    }

    #[test]
    fn unload_picker_saves_right_away_minutes_and_never() {
        let (window, _ipc, _sent, layout) = window("unload");
        let s = window.global::<ModelState>();
        let labels: Vec<SharedString> = s.get_unload_options().iter().collect();
        let want = [
            "Right after dictation",
            "After 1 min",
            "After 5 min",
            "After 10 min",
            "After 15 min",
            "After 30 min",
            "After 60 min",
            "Never",
        ];
        assert_eq!(labels, want);
        assert_eq!(s.get_unload(), 2, "the default 5 min");
        s.invoke_unload_picked(0);
        let saved = saved_model(&layout);
        assert!(saved.unload_immediately);
        assert_eq!(saved.idle_unload_minutes, 5, "the minutes are kept");
        s.invoke_unload_picked(5);
        let saved = saved_model(&layout);
        assert_eq!(
            (saved.unload_immediately, saved.idle_unload_minutes),
            (false, 30)
        );
        s.invoke_unload_picked(7);
        let saved = saved_model(&layout);
        assert_eq!(
            (saved.unload_immediately, saved.idle_unload_minutes),
            (false, 0)
        );
    }

    #[test]
    fn unload_rows_keep_an_older_value() {
        let (labels, values) = unload_rows(Some(12));
        assert_eq!(labels[4], "After 12 min");
        assert_eq!(values[3..6], [Some(10), Some(12), Some(15)]);
        let (labels, _) = unload_rows(Some(90));
        assert_eq!(labels[7..], ["After 90 min", "Never"]);
        for current in [None, Some(0), Some(5)] {
            assert_eq!(unload_rows(current).1, UNLOAD, "{current:?}");
        }
    }

    #[test]
    fn cancel_waits_for_the_daemon_to_stop() {
        let (window, ipc, sent, _) = window("cancel");
        view(&window, "Whisper large-v3");
        push(&ipc, progress("large-v3", 5, 10));
        let _ = requests(&sent);
        with_role(&window, "Cancel Whisper large-v3", AccessibleRole::Button)
            .invoke_accessible_default_action();
        let want = Request::CancelDownload {
            id: "large-v3".into(),
        };
        assert_eq!(requests(&sent), [want]);
        assert!(
            bar(&window).is_some(),
            "the bar went before the daemon stopped"
        );
        push(&ipc, done("large-v3", DownloadResult::Cancelled));
        assert_eq!(bar(&window), None);
        assert!(has(&window, "Download Whisper large-v3"));
    }

    #[test]
    fn lost_daemon_ends_the_downloads() {
        let (window, ipc, _sent, _) = window("lost");
        view(&window, "Whisper large-v3");
        push(&ipc, progress("large-v3", 5, 10));
        ipc.deliver(&Event::Link(Link::Offline));
        assert_eq!(bar(&window), None);
        assert!(has(&window, "Whisper large-v3: Not downloaded"));
    }

    #[test]
    fn daemon_gpus_are_listed_by_name_and_picked() {
        let (window, ipc, _sent, layout) = window("gpus");
        let gpu = |name: &str, description: &str, uma| Device {
            name: name.into(),
            description: description.into(),
            uma,
        };
        let gpus = vec![
            gpu("Vulkan0", "NVIDIA GeForce RTX 4070 SUPER", false),
            gpu("Vulkan1", "AMD Radeon(TM) Graphics", true),
        ];
        push(
            &ipc,
            Push::Devices {
                mics: Vec::new(),
                gpus,
            },
        );
        let s = window.global::<ModelState>();
        let rows: Vec<SharedString> = s.get_gpus().iter().collect();
        // The marketing names (UI rule 30); the ggml name stays the config key.
        let want = [
            "Auto",
            "CPU",
            "NVIDIA GeForce RTX 4070 SUPER",
            "AMD Radeon(TM) Graphics",
        ];
        assert_eq!(rows, want);
        s.invoke_gpu_picked(3);
        let text = fs::read_to_string(layout.config_file()).unwrap();
        let saved = ConfigFile::parse(&text).unwrap().config.model.gpu;
        assert_eq!(saved, DeviceChoice::Named("Vulkan1".into()));
    }

    #[test]
    fn status_warning_shows_the_cpu_note() {
        let (window, ipc, _sent, _) = window("warning");
        let note = engine_warning(&EngineWarning::CpuFallback {
            reason: "out of device memory".into(),
        });
        assert!(!has(&window, &note));
        let status = Push::Status {
            phase: Phase::Idle,
            model: "turbo".into(),
            device: DeviceChoice::Auto,
            backend: "cpu".into(),
            warnings: vec![note.clone(), "sounds.start: empty, using rise".into()],
        };
        push(&ipc, status);
        // On the Settings sub-tab, beside the GPU; a hidden sub-tab shows nothing.
        assert!(!has(&window, &note));
        with_role(&window, "Settings", AccessibleRole::Tab).invoke_accessible_default_action();
        // The note alone (an exact label): the sound line is the Audio page's.
        assert!(has(&window, &note));
    }

    #[test]
    fn gpu_rows_keep_a_missing_choice() {
        type Case<'a> = (Option<&'a [Device]>, DeviceChoice, &'a [&'a str]);
        let listed = [Device {
            name: "Vulkan0".into(),
            description: String::new(),
            uma: false,
        }];
        let named = |n: &str| DeviceChoice::Named(n.into());
        let table: [Case<'_>; 5] = [
            (None, DeviceChoice::Auto, &["Auto", "CPU"]),
            (None, named("Vulkan1"), &["Auto", "CPU", "Vulkan1"]),
            (
                Some(&listed),
                DeviceChoice::Cpu,
                &["Auto", "CPU", "Vulkan0"],
            ),
            (Some(&listed), named("Vulkan0"), &["Auto", "CPU", "Vulkan0"]),
            (
                Some(&listed),
                named("Vulkan1"),
                &["Auto", "CPU", "Vulkan0", "Not connected: Vulkan1"],
            ),
        ];
        for (listed, current, want) in table {
            let (labels, choices) = gpu_rows(listed, &current);
            assert_eq!(labels, want, "{current:?}");
            assert!(choices.contains(&current), "{current:?}");
        }
    }

    const RELATIVE: &str = strings::MODEL_PATH_RELATIVE;

    #[test]
    fn custom_path_problems() {
        let dir = crate::testing::scratch_dir(&format!(
            "oxi-settings-lib-model-{}-custom",
            std::process::id()
        ));
        fs::create_dir_all(dir.join("folder.bin")).unwrap();
        let path = |name: &str| dir.join(name).to_string_lossy().into_owned();
        fs::write(dir.join("mine.BIN"), b"not a model").unwrap();
        let (file, folder, missing) = (path("mine.BIN"), path("folder.bin"), path("gone.bin"));
        for (path, want) in [
            ("", Some("Choose a .bin model file")),
            (file.as_str(), None),
            (missing.as_str(), Some("File not found")),
            (folder.as_str(), Some("This is a folder, not a model file")),
            ("notes.txt", Some("The model file must end in .bin")),
            ("mine.bin", Some(RELATIVE)),
            ("models/mine.bin", Some(RELATIVE)),
        ] {
            assert_eq!(custom_problem(path), want, "{path:?}");
        }
        // The text alone decides these: no disk read on the UI thread.
        assert_eq!(shape_problem(&missing), None);
        assert_eq!(shape_problem("mine.bin"), Some(RELATIVE));
    }

    #[test]
    fn custom_path_answers_come_from_the_worker() {
        let (window, _ipc, _sent, layout) = window("worker");
        view(&window, "Use my own .bin");
        let field = with_role(&window, "Model file", AccessibleRole::TextInput);
        let missing = layout.config_dir.join("gone.bin");
        field.set_accessible_value(missing.to_string_lossy().into_owned());
        wait_for(|| has(&window, "File not found"));
        assert!(!layout.config_file().exists());

        let model = layout.config_dir.join("mine.bin");
        fs::write(&model, b"synthetic").unwrap();
        field.set_accessible_value(format!("\"{}\"", model.display()));
        wait_for(|| layout.config_file().exists());
        let text = fs::read_to_string(layout.config_file()).unwrap();
        let saved = ConfigFile::parse(&text).unwrap().config.model;
        assert_eq!(saved.id, "custom");
        assert_eq!(saved.custom_path, model.to_string_lossy());
        assert!(!has(&window, "File not found"));
        with_role(&window, "Own model file in use", AccessibleRole::Button);
        let tiles = window.global::<ModelState>().get_tiles();
        assert_eq!(tiles.row_data(3).unwrap().caption, IN_USE);
    }

    /// A model file of the registry's size: "ready" without a hash (the stamp is not there).
    fn on_disk(layout: &Layout, file: &str, bytes: u64) -> std::path::PathBuf {
        fs::create_dir_all(layout.models_dir()).unwrap();
        let path = layout.models_dir().join(file);
        crate::testing::fake_model(&path, bytes);
        path
    }

    fn large() -> &'static ModelEntry {
        MODELS.iter().find(|m| m.id == "large-v3").unwrap()
    }

    fn deleted(id: &str, result: DeleteResult) -> Push {
        Push::ModelDeleted {
            id: id.into(),
            result,
        }
    }

    /// Ready: Use and Delete. Delete asks first and names what it frees; Keep sends nothing, the
    /// second answer sends the request, the daemon's answer says what was freed.
    #[test]
    fn delete_asks_first_then_frees_the_space() {
        let (window, ipc, sent, layout) = window("delete");
        let file = on_disk(&layout, large().file_name, large().size);
        window.global::<ModelState>().invoke_shown();
        view(&window, "Whisper large-v3");
        with_role(&window, "Use Whisper large-v3", AccessibleRole::Button);
        let _ = requests(&sent);
        let question = "Delete Whisper large-v3? Frees 3.1 GB.";
        click(&window, "Delete Whisper large-v3");
        assert!(has(&window, question));
        click(&window, "Keep Whisper large-v3");
        assert!(!has(&window, question));
        assert_eq!(requests(&sent), []);
        assert!(file.exists());

        click(&window, "Delete Whisper large-v3");
        click(&window, "Confirm: delete Whisper large-v3");
        let want = Request::DeleteModel {
            id: "large-v3".into(),
        };
        assert_eq!(requests(&sent), [want]);
        fs::remove_file(&file).unwrap();
        push(
            &ipc,
            deleted(
                "large-v3",
                DeleteResult::Done {
                    freed: large().size,
                },
            ),
        );
        assert_eq!(
            window.global::<Toast>().get_text(),
            "Deleted Whisper large-v3, freed 3.1 GB"
        );
        assert!(has(&window, "Whisper large-v3: Not downloaded"));
        assert!(!has(&window, question));
        with_role(&window, "Download Whisper large-v3", AccessibleRole::Button);
    }

    /// Return on the focused button, then the `changed` handlers that move the focus.
    fn enter(window: &AppWindow) {
        let text = SharedString::from(slint::platform::Key::Return);
        let w = window.window();
        w.dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.clone() });
        w.dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
        slint::platform::update_timers_and_animations();
    }

    /// The question's buttons leave with it; the focus stays on the card, not the window top:
    /// Keep gives it to Delete, a confirmed Delete to the Download that takes the files' place.
    #[test]
    fn the_focus_stays_on_the_card_after_the_question() {
        let (window, ipc, sent, layout) = window("delete-focus");
        let file = on_disk(&layout, large().file_name, large().size);
        window.global::<ModelState>().invoke_shown();
        view(&window, "Whisper large-v3");
        // The card's `changed index` closes a question: let it run now, as a frame would.
        slint::platform::update_timers_and_animations();
        let question = "Delete Whisper large-v3? Frees 3.1 GB.";
        click(&window, "Delete Whisper large-v3");
        assert!(has(&window, question));
        enter(&window); // on Keep, which the question focused
        assert!(!has(&window, question));
        enter(&window);
        assert!(has(&window, question), "Keep left the focus nowhere");

        click(&window, "Confirm: delete Whisper large-v3");
        fs::remove_file(&file).unwrap();
        let _ = requests(&sent);
        let freed = DeleteResult::Done {
            freed: large().size,
        };
        push(&ipc, deleted("large-v3", freed));
        assert!(has(&window, "Whisper large-v3: Not downloaded"));
        slint::platform::update_timers_and_animations();
        enter(&window);
        let want = Request::DownloadModel {
            id: "large-v3".into(),
        };
        assert_eq!(requests(&sent), [want], "Delete left the focus nowhere");
    }

    /// A delete that fails ends the card's wait for the files to go: when they vanish later (Explorer,
    /// the folder poll), the Download that appears does not take the focus from where the user is.
    #[test]
    fn a_failed_delete_does_not_steal_the_focus_later() {
        let (window, ipc, sent, layout) = window("delete-failed-focus");
        let file = on_disk(&layout, large().file_name, large().size);
        window.global::<ModelState>().invoke_shown();
        view(&window, "Whisper large-v3");
        slint::platform::update_timers_and_animations();
        click(&window, "Delete Whisper large-v3");
        click(&window, "Confirm: delete Whisper large-v3");
        let failed = DeleteResult::Failed {
            message: "Access is denied. (os error 5)".into(),
        };
        push(&ipc, deleted("large-v3", failed));
        slint::platform::update_timers_and_animations(); // the focus returns to Delete
        // The user moves on, to Use.
        let w = window.window();
        let back = SharedString::from(slint::platform::Key::Backtab);
        w.dispatch_event(slint::platform::WindowEvent::KeyPressed { text: back.clone() });
        w.dispatch_event(slint::platform::WindowEvent::KeyReleased { text: back });

        fs::remove_file(&file).unwrap();
        // The daemon lists the devices again: the rows are read from the files.
        push(
            &ipc,
            Push::Devices {
                mics: Vec::new(),
                gpus: Vec::new(),
            },
        );
        assert!(has(&window, "Whisper large-v3: Not downloaded"));
        slint::platform::update_timers_and_animations();
        let _ = requests(&sent);
        enter(&window);
        assert_eq!(requests(&sent), [], "the focus was taken to Download");
    }

    /// A Delete refused at once (the daemon went away while the question was open) ends the wait
    /// too, also when the refusal repeats the card's last "!" word for word.
    #[test]
    fn a_refused_delete_does_not_steal_the_focus_later() {
        let (window, ipc, sent, layout) = window("delete-refused-focus");
        let file = on_disk(&layout, large().file_name, large().size);
        window.global::<ModelState>().invoke_shown();
        view(&window, "Whisper large-v3");
        slint::platform::update_timers_and_animations();
        for _ in 0..2 {
            ipc.deliver(&Event::Link(Link::Online));
            click(&window, "Delete Whisper large-v3");
            ipc.deliver(&Event::Link(Link::Offline));
            click(&window, "Confirm: delete Whisper large-v3");
            slint::platform::update_timers_and_animations();
        }
        ipc.deliver(&Event::Link(Link::Online));
        fs::remove_file(&file).unwrap();
        push(
            &ipc,
            Push::Devices {
                mics: Vec::new(),
                gpus: Vec::new(),
            },
        );
        assert!(has(&window, "Whisper large-v3: Not downloaded"));
        slint::platform::update_timers_and_animations();
        let _ = requests(&sent);
        enter(&window);
        assert_eq!(requests(&sent), [], "the focus was taken to Download");
    }

    /// Draws the card's states the other shots miss: paused (Resume, Delete), ready and not in use
    /// (Use, Delete), and the delete question; each frame non-empty and not one colour. With
    /// `OXI_SHOTS_DIR` set they land in `settings-model-card-<state>-<w>-<theme>.png`. The renderer
    /// platform is per thread and the other tests here use the testing backend: a thread of its own.
    #[test]
    fn shots_model_card() {
        thread::spawn(draw_cards).join().unwrap();
    }

    fn draw_cards() {
        use slint::platform::software_renderer::{
            MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
        };
        use slint::platform::{Platform, WindowAdapter};

        thread_local! {
            static LAST: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
            // The animation clock, stepped by hand past the theme fades.
            static CLOCK: Cell<Duration> = const { Cell::new(Duration::ZERO) };
        }
        struct Shots;
        impl Platform for Shots {
            fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
                let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
                LAST.with_borrow_mut(|last| *last = Some(window.clone()));
                Ok(window)
            }
            fn duration_since_start(&self) -> Duration {
                CLOCK.get()
            }
        }

        slint::platform::set_platform(Box::new(Shots)).unwrap();
        let dir = std::env::var_os("OXI_SHOTS_DIR").map(std::path::PathBuf::from);
        for state in ["paused", "ready", "question"] {
            let layout = crate::testing::layout("model", &format!("shot-{state}"));
            let window = crate::open_with(layout.clone(), Ipc::fake().0).unwrap();
            let surface = LAST.with_borrow(Clone::clone).unwrap();
            window.set_page(Page::Model);
            if state == "paused" {
                on_disk(
                    &layout,
                    &format!("{}{PART_SUFFIX}", large().file_name),
                    1_000_000,
                );
            } else {
                on_disk(&layout, large().file_name, large().size);
            }
            surface.set_size(slint::PhysicalSize::new(1280, 860));
            window.show().unwrap();
            window.global::<ModelState>().invoke_shown();
            view(&window, "Whisper large-v3");
            slint::platform::update_timers_and_animations();
            if state == "question" {
                click(&window, "Delete Whisper large-v3");
            }
            for (w, h) in [(1280_u32, 860_u32), (720, 520)] {
                surface.set_size(slint::PhysicalSize::new(w, h));
                for (theme, choice) in [
                    ("light", crate::ThemeChoice::Light),
                    ("dark", crate::ThemeChoice::Dark),
                ] {
                    window.set_theme(choice);
                    let mut buffer = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
                    // A fade starts when a frame reads the new colour: draw one, then step past it.
                    for step in [Duration::ZERO, Duration::from_millis(300)] {
                        CLOCK.set(CLOCK.get() + step);
                        slint::platform::update_timers_and_animations();
                        surface.request_redraw();
                        assert!(surface.draw_if_needed(|r| {
                            r.render(&mut buffer, w as usize);
                        }));
                    }
                    let bytes = buffer
                        .iter()
                        .flat_map(|p| [p.red, p.green, p.blue, p.alpha])
                        .collect();
                    let size = tiny_skia::IntSize::from_wh(w, h).unwrap();
                    let pixmap = tiny_skia::Pixmap::from_vec(bytes, size).unwrap();
                    let pixels = pixmap.pixels();
                    let first = pixels[0];
                    assert!(
                        pixels.iter().any(|p| p.alpha() > 0),
                        "{state} {theme} drew nothing"
                    );
                    assert!(
                        pixels.iter().any(|p| *p != first),
                        "{state} {theme} is flat"
                    );
                    if let Some(dir) = &dir {
                        let name = format!("settings-model-card-{state}-{w}-{theme}.png");
                        fs::create_dir_all(dir).unwrap();
                        pixmap.save_png(dir.join(name)).unwrap();
                    }
                }
            }
        }
    }

    /// The daemon's refusals stay on the card, as its "!".
    #[test]
    fn a_refused_delete_says_why() {
        let (window, ipc, _sent, layout) = window("delete-refused");
        on_disk(&layout, large().file_name, large().size);
        window.global::<ModelState>().invoke_shown();
        view(&window, "Whisper large-v3");
        push(&ipc, deleted("large-v3", DeleteResult::InUse));
        assert!(has(&window, DELETE_IN_USE));
        let failed = DeleteResult::Failed {
            message: "Access is denied. (os error 5)".into(),
        };
        push(&ipc, deleted("large-v3", failed));
        assert!(has(&window, "Not deleted: Access is denied. (os error 5)"));
        assert!(has(&window, "Whisper large-v3: Downloaded"));
    }

    /// In use: no Use, and Delete is off with the reason; another model in use frees it.
    #[test]
    fn the_model_in_use_is_not_deleted() {
        let (window, _ipc, sent, layout) = window("delete-in-use");
        on_disk(&layout, large().file_name, large().size);
        let s = window.global::<ModelState>();
        s.invoke_shown();
        view(&window, "Whisper large-v3");
        click(&window, "Use Whisper large-v3");
        assert_eq!(saved_model(&layout).id, "large-v3");
        let used = with_role(&window, "Whisper large-v3 in use", AccessibleRole::Button);
        assert_eq!(used.accessible_enabled(), Some(false));
        let delete = with_role(&window, "Delete Whisper large-v3", AccessibleRole::Button);
        assert_eq!(delete.accessible_enabled(), Some(false));
        assert_eq!(
            delete.accessible_description().as_deref(),
            Some("In use. Use another model first to delete this one.")
        );
        // The callback refuses it too.
        let _ = requests(&sent);
        assert!(!s.invoke_delete(1));
        assert_eq!(requests(&sent), []);
        assert_eq!(s.get_tiles().row_data(1).unwrap().caption, IN_USE);
        assert_eq!(s.get_tiles().row_data(0).unwrap().caption, "Not downloaded");
    }

    /// Paused: Resume and Delete; the question names the partial download and its size.
    #[test]
    fn a_paused_download_can_be_deleted() {
        let (window, _ipc, sent, layout) = window("delete-paused");
        on_disk(
            &layout,
            &format!("{}{PART_SUFFIX}", large().file_name),
            400_000_000,
        );
        window.global::<ModelState>().invoke_shown();
        view(&window, "Whisper large-v3");
        with_role(&window, "Resume Whisper large-v3", AccessibleRole::Button);
        let _ = requests(&sent);
        click(&window, "Delete Whisper large-v3");
        assert!(has(
            &window,
            "Delete the partial download of Whisper large-v3? Frees 400 MB."
        ));
        click(&window, "Confirm: delete Whisper large-v3");
        let want = Request::DeleteModel {
            id: "large-v3".into(),
        };
        assert_eq!(requests(&sent), [want]);
    }

    /// Not downloaded and downloading offer no Delete; without the daemon Delete is off.
    #[test]
    fn delete_is_offered_only_for_files_and_needs_the_daemon() {
        let (window, ipc, _sent, layout) = window("delete-offer");
        view(&window, "Whisper large-v3");
        let shown = |window: &AppWindow| has(window, "Delete Whisper large-v3");
        assert!(!shown(&window), "nothing on disk");
        push(&ipc, progress("large-v3", 5, 10));
        assert!(!shown(&window), "downloading");
        on_disk(&layout, large().file_name, large().size);
        push(&ipc, done("large-v3", DownloadResult::Ok));
        assert!(shown(&window));
        ipc.deliver(&Event::Link(Link::Offline));
        let delete = with_role(&window, "Delete Whisper large-v3", AccessibleRole::Button);
        assert_eq!(delete.accessible_enabled(), Some(false));
        assert_eq!(
            delete.accessible_description().as_deref(),
            Some(DELETE_NEEDS_DAEMON)
        );
    }

    #[test]
    fn tile_captions_say_the_state() {
        let row = |badge: &str, ready, downloading, progress| ModelRow {
            badge: badge.into(),
            ready,
            downloading,
            progress,
            ..ModelRow::default()
        };
        let cases = [
            (row("Ready", true, false, 0.0), false, "Ready"),
            (row("Ready", true, false, 0.0), true, "In use"),
            (
                row("Not downloaded", false, false, 0.0),
                true,
                "In use · Not downloaded",
            ),
            (
                row("Downloading", false, true, 0.454),
                false,
                "Downloading 45%",
            ),
            (row("Paused", false, false, 0.0), false, "Paused"),
        ];
        for (r, in_use, want) in cases {
            assert_eq!(tile_caption(&r, in_use), want);
        }
    }

    #[test]
    fn a_dialog_starts_at_the_current_file() {
        let dir = crate::testing::scratch_dir(&format!(
            "oxi-settings-lib-model-{}-start",
            std::process::id()
        ));
        let (models, other) = (dir.join("models"), dir.join("mine"));
        fs::create_dir_all(&models).unwrap();
        fs::create_dir_all(&other).unwrap();
        let file = other.join("x.bin").to_string_lossy().into_owned();
        assert_eq!(start_dir(&file, &models), Some(other.clone()));
        // A folder that is gone: the nearest one above it.
        let gone = other
            .join("gone")
            .join("x.bin")
            .to_string_lossy()
            .into_owned();
        assert_eq!(start_dir(&gone, &models), Some(other));
        // Empty or relative: the models folder, or the nearest one above it.
        assert_eq!(start_dir("", &models), Some(models.clone()));
        assert_eq!(start_dir("mine.bin", &models), Some(models.clone()));
        assert_eq!(start_dir("", &models.join("not-yet")), Some(models));
    }

    #[test]
    fn fractions() {
        assert!((fraction(1, 4) - 0.25).abs() < f32::EPSILON);
        assert!(fraction(1, 0).abs() < f32::EPSILON);
        assert!((fraction(9, 4) - 1.0).abs() < f32::EPSILON);
    }
}
