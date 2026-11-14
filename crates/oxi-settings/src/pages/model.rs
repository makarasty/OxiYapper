//! Model page: the curated models as a gallery with their status on disk, download, cancel and
//! delete through the daemon (the page itself never touches the network), "use my own model
//! file", the models folder and its unfinished downloads; the dictation language and the model of
//! each language; the GPU, the unload and the engine parameters.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::rc::{Rc, Weak as RcWeak};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::SystemTime;

use oxi_core::config::{Advanced, Auto, Decoding, LanguageKey, MemoryMode, PromptSource};
use oxi_core::device::{Device, DeviceChoice, pick};
use oxi_core::ipc::{DeleteResult, DownloadResult, Push, Request};
use oxi_core::languages::{self, LanguageInfo};
use oxi_core::models::{self, Adapter, MODELS, Machine, ModelEntry, ModelStatus, PartialDownload};
use oxi_core::paste::file::WRITE_FAILED;
use oxi_core::paths::PART_SUFFIX;
use oxi_core::stats::{self, Day};
use oxi_core::strings::{self, gb};
use oxi_core::suggest::{self, Advice};
use oxi_core::tuning::{self, Sampling, preset_mode, preset_value};
use oxi_core::types::Language;
use oxi_platform::fs::{list_partials, models_size};
use oxi_platform::{stats_store, sysinfo};
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel, Weak};

use super::progress::{Bars, DOWNLOAD_EST};
use super::recent;
use super::toast;
use crate::constants::FOLDER_POLL;
use crate::generated::{GalleryTile, LanguageModel, ModelRow, ModelState, TextState};
use crate::ipc::{Event, Link, Subscription};
use crate::{AppWindow, Ctx, Error, Page, report};

/// `[model] id` of "use my own .bin".
const CUSTOM: &str = "custom";
/// The glyph of "use my own .bin" in `ModelState.class-icons`, after the model classes.
const OWN_CLASS: i32 = 4;
/// "Free memory after" choices in picker order: `None` is right after a dictation
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
/// Why "Delete unfinished downloads" is off.
const PARTIALS_NONE: &str = "No unfinished downloads";
/// The last item of a language row's Combo: opens Models.
const DOWNLOAD_ANOTHER: &str = "Download another model…";
/// The "!" of a language row whose model is not on disk (src/pipeline.rs: no fallback).
const NOT_DOWNLOADED: &str =
    "Not downloaded: dictation in this language fails until you download it or pick another";
const PARTIALS_BUSY: &str = "Wait until the download stops";
/// Why a model's Delete is off without the daemon, which may hold the file open.
const DELETE_NEEDS_DAEMON: &str = "Start OxiYapper to delete.";
/// A delete the daemon refused: a dictation, a test or a download is using the model.
const DELETE_IN_USE: &str = "In use right now. Try again when it is done.";

/// The Languages tab's callbacks: the dictation language and the language -> model rows.
fn bind_languages(s: &ModelState<'_>, page: &Rc<PageState>) {
    let p = Rc::clone(page);
    s.on_language_picked(move |i| p.language_picked(i));
    let p = Rc::clone(page);
    s.on_language_search(move |text| p.language_search(&text));
    let p = Rc::clone(page);
    s.on_language_chosen(move |i| p.language_chosen(i));
    let p = Rc::clone(page);
    s.on_lang_model_picked(move |row, item| p.lang_model_picked(row, item));
    let p = Rc::clone(page);
    s.on_lang_removed(move |row| p.lang_removed(row));
    let p = Rc::clone(page);
    s.on_add_search(move |text| p.add_search(&text));
    let p = Rc::clone(page);
    s.on_add_chosen(move |i| p.add_chosen(i));
}

pub fn bind(window: &AppWindow, ctx: &Ctx) {
    let s = window.global::<ModelState>();
    let entries: Vec<&'static ModelEntry> = MODELS.iter().filter(|m| m.id != "tiny").collect();
    let dir = ctx.layout.models_dir();
    let now = SystemTime::now();
    let rows: Vec<ModelRow> = entries.iter().map(|e| row(e, &dir, None, now)).collect();
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
        seen: RefCell::default(),
        listed: RefCell::new(None),
        backend: RefCell::new(String::new()),
        loaded: Cell::new(false),
        choices: RefCell::new(Vec::new()),
        unloads: RefCell::new(Vec::new()),
        lang_rows: RefCell::new(Vec::new()),
        added: RefCell::new(Vec::new()),
        // Not a pair the config can hold: the first `push_language` takes the config's.
        slots: Cell::new([Language::AUTO; 2]),
        found: RefCell::new(Vec::new()),
        custom: Cell::new(0),
        bars: Bars::new({
            let me = me.clone();
            move |&i, shown| {
                if let Some(page) = me.upgrade() {
                    page.update(i, |r| r.progress = shown);
                }
            }
        }),
        checks: Checks::new(),
        measuring: Measuring::new(),
        days: RefCell::new(Vec::new()),
        dismissed: RefCell::new(recent::dismissed(&ctx.layout.data_dir)),
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

    bind_languages(&s, &page);
    let p = Rc::clone(&page);
    s.on_shown(move || p.shown());
    let p = Rc::clone(&page);
    s.on_measured(move || p.measured());
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
    let p = Rc::clone(&page);
    s.on_load_at_start_toggled(move |on| p.load_at_start_toggled(on));
    let p = Rc::clone(&page);
    s.on_memory_suggestion(move |apply| p.memory_suggestion(apply));
    let p = Rc::clone(&page);
    s.on_memory_recheck(move || p.memory_recheck());
    let p = Rc::clone(&page);
    s.on_adv_changed(move |param, mode, value| p.adv_changed(param, mode, value));
    let p = Rc::clone(&page);
    s.on_adv_prompt_edited(move |text| p.adv_prompt_edited(&text));
    let p = Rc::clone(&page);
    s.on_adv_prompt_source_picked(move |i| p.adv_prompt_source_picked(i));
    let p = Rc::clone(&page);
    s.on_adv_reset(move |group| p.adv_reset(group));

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
    /// The machine and the models folder as the measuring worker last read them.
    seen: RefCell<Seen>,
    /// The daemon's GPU list; `None` until it answers `ListDevices`.
    listed: RefCell<Option<Vec<Device>>>,
    /// The daemon's `Status.backend` (`cpu`, `Vulkan0`, `none`); empty until it answers.
    backend: RefCell<String>,
    /// The daemon's `Status.loaded`: a model is in memory (what `backend` names is running).
    loaded: Cell<bool>,
    /// What each GPU picker row writes.
    choices: RefCell<Vec<DeviceChoice>>,
    /// What each unload picker row writes, as in [`UNLOAD`].
    unloads: RefCell<Vec<Option<u32>>>,
    /// Model > Languages, row by row: the language and what each item of its Combo writes
    /// (`None` is the last item, "Download another model", which opens Models instead).
    lang_rows: RefCell<Vec<(Language, Vec<Option<String>>)>>,
    /// The rows of the "Add language" popup: what its search matched.
    added: RefCell<Vec<&'static LanguageInfo>>,
    /// The two language segments as shown, see [`languages::slots`].
    slots: Cell<[Language; 2]>,
    /// The rows of the language popup: what the search matched.
    found: RefCell<Vec<&'static LanguageInfo>>,
    /// The Advanced numbers whose Custom the person picked (bit per parameter): Custom stays
    /// picked while the number equals a preset's, and clears with any other choice.
    custom: Cell<u16>,
    /// The download bars by row: they show what the one progress driver answers, not the bytes
    /// as they come.
    bars: Rc<Bars<usize>>,
    checks: Checks,
    measuring: Measuring,
    /// The Home stats rows, read when the page opens (a small file) or on "Check again", for the
    /// memory suggestion (hold item 41, candidate 10).
    days: RefCell<Vec<Day>>,
    /// Keys of the suggestions given "Not now"; kept in the data folder (`recent::DISMISSED_FILE`).
    dismissed: RefCell<HashSet<String>>,
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

impl Checks {
    fn new() -> Self {
        let (tx, rx) = channel();
        Self {
            latest: Cell::new(0),
            running: Cell::new(0),
            tx,
            rx,
            timer: Timer::default(),
        }
    }
}

/// The models folder measure, on a worker: one at a time, so a folder on a share that went
/// offline holds one stuck thread, not one per file change. The worker's answer wakes the page
/// through the event loop (`ModelState.measured`), with no timer.
struct Measuring {
    /// A worker is out.
    busy: Cell<bool>,
    /// A file change came while it was out: measure again when it answers.
    again: Cell<bool>,
    tx: Sender<Measured>,
    rx: Receiver<Measured>,
}

impl Measuring {
    fn new() -> Self {
        let (tx, rx) = channel();
        Self {
            busy: Cell::new(false),
            again: Cell::new(false),
            tx,
            rx,
        }
    }
}

/// What the measuring worker read last.
#[derive(Default)]
struct Seen {
    /// The graphics adapters, `None` until the first answer (read once per page).
    adapters: Option<Vec<Adapter>>,
    ram: Option<u64>,
    /// Free space of the models drive.
    free: Option<u64>,
    /// The finished files of the models folder (bytes, `.bin` count) or why it cannot be read;
    /// `None` until the first answer.
    folder: Option<Result<(u64, usize), String>>,
}

struct Measured {
    folder: Result<(u64, usize), String>,
    free: Option<u64>,
    /// The system memory and the adapters, when they were asked for (the first measure only).
    hardware: Option<(Option<u64>, Vec<Adapter>)>,
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
        self.read_days();
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
        s.set_load_at_start(model.preload_at_start);
        drop(file);
        self.push_memory_tip(s);
        let (labels, values) = unload_rows(unload);
        s.set_unload_options(ModelRc::from(Rc::new(VecModel::from(labels))));
        s.set_unload(values.iter().position(|v| *v == unload).map_or(0, int));
        *self.unloads.borrow_mut() = values;
        self.check_custom(&path, false);
        self.push_gpus(s);
        self.push_language(s);
        self.push_lang_rows(s);
        self.push_advanced(s);
    }

    /// Model > Languages (ia-map 3.5): the first row's model, a row per language with a model of
    /// its own sorted by English name, the one row that runs now, and on each gallery model the
    /// languages that use it (named by its Delete question).
    fn push_lang_rows(&self, s: &ModelState<'_>) {
        let file = self.ctx.config.borrow();
        let model = &file.config.model;
        let mut langs: Vec<(Language, String)> = model
            .by_language
            .iter()
            .map(|(k, id)| (k.0, id.clone()))
            .collect();
        langs.sort_by_key(|(l, _)| english(*l));
        let current = model.language;
        let own = !model.custom_path.trim().is_empty();
        let default_active = current == Language::AUTO || !langs.iter().any(|(l, _)| *l == current);
        let id = model.id.clone();
        drop(file);
        let default_model = self.model_label(&id);
        let mut rows = Vec::with_capacity(langs.len());
        let mut writes = Vec::with_capacity(langs.len());
        for (lang, id) in &langs {
            let mut ids: Vec<Option<String>> = self
                .entries
                .iter()
                .filter(|e| self.downloaded(e.id))
                .map(|e| Some(e.id.to_owned()))
                .collect();
            if own {
                ids.push(Some(CUSTOM.to_owned()));
            }
            if !ids.iter().any(|i| i.as_deref() == Some(id.as_str())) {
                ids.push(Some(id.clone()));
            }
            let at = ids
                .iter()
                .position(|i| i.as_deref() == Some(id.as_str()))
                .unwrap_or(0);
            let mut labels: Vec<SharedString> = ids
                .iter()
                .flatten()
                .map(|id| self.model_label(id).into())
                .collect();
            labels.push(DOWNLOAD_ANOTHER.into());
            ids.push(None);
            let only = models::facts(id).only;
            let warning = if id != CUSTOM && !self.downloaded(id) {
                NOT_DOWNLOADED.to_owned()
            } else if let Some(only) = only.filter(|o| o != lang) {
                format!("This model hears only {}", english(only))
            } else {
                String::new()
            };
            let native = lang
                .info()
                .filter(|i| i.native != i.english)
                .map_or("", |i| i.native);
            rows.push(LanguageModel {
                name: english(*lang).into(),
                native: native.into(),
                models: ModelRc::from(Rc::new(VecModel::from(labels))),
                model: int(at),
                warning: warning.into(),
                active: *lang == current,
            });
            writes.push((*lang, ids));
        }
        s.set_lang_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
        s.set_default_model(default_model.into());
        s.set_default_active(default_active);
        *self.lang_rows.borrow_mut() = writes;
        for (i, entry) in self.entries.iter().enumerate() {
            let used: Vec<&str> = langs
                .iter()
                .filter(|(_, id)| id == entry.id)
                .map(|(l, _)| english(*l))
                .collect();
            let line = if used.is_empty() {
                String::new()
            } else {
                format!("Used for {}.", used.join(", "))
            };
            if self.rows.row_data(i).is_some_and(|r| r.used_for != line) {
                self.update(i, |r| r.used_for = line.into());
            }
        }
    }

    /// A model's name in the language rows: the registry's name, with its one language when it
    /// hears only that, and "Not downloaded" when it is gone; the own file; an unknown id as it is.
    fn model_label(&self, id: &str) -> String {
        if id == CUSTOM {
            return "Own model file".to_owned();
        }
        let Some(entry) = models::find(id) else {
            return id.to_owned();
        };
        let name = match models::facts(id).only {
            Some(only) => format!("{} · {} only", entry.display_name, english(only)),
            None => entry.display_name.to_owned(),
        };
        if self.downloaded(id) {
            name
        } else {
            format!("{name}: {}", strings::model_badge(ModelStatus::Missing))
        }
    }

    /// Registry model `id` is on disk, as its row last read the files.
    fn downloaded(&self, id: &str) -> bool {
        self.entries
            .iter()
            .position(|e| e.id == id)
            .and_then(|i| self.rows.row_data(i))
            .is_some_and(|r| r.ready)
    }

    /// The Text page's file row: this page hears every `Status` already (one subscriber per
    /// page), so it passes the to-file line on.
    fn file_warning(&self, line: &str) {
        if let Some(window) = self.window.upgrade() {
            window.global::<TextState>().set_file_warning(line.into());
        }
    }

    /// A language row's Combo: its model, or the last item, which opens Models to get another.
    fn lang_model_picked(&self, row: i32, item: i32) {
        let pick = usize::try_from(row)
            .ok()
            .and_then(|r| self.lang_rows.borrow().get(r).cloned())
            .and_then(|(lang, ids)| {
                let id = usize::try_from(item)
                    .ok()
                    .and_then(|i| ids.get(i).cloned())?;
                Some((lang, id))
            });
        match pick {
            Some((lang, Some(id))) => {
                self.ctx
                    .config
                    .borrow_mut()
                    .config
                    .model
                    .by_language
                    .insert(LanguageKey(lang), id);
                report(&self.window, self.ctx.save());
            }
            Some((_, None)) => self.state(|s| s.set_tab(0)),
            None => {}
        }
        // Also after "Download another model": the Combo goes back to the row's model.
        self.state(|s| self.push_lang_rows(s));
    }

    fn lang_removed(&self, row: i32) {
        let lang = usize::try_from(row)
            .ok()
            .and_then(|r| self.lang_rows.borrow().get(r).map(|(l, _)| *l));
        let Some(lang) = lang else { return };
        self.ctx
            .config
            .borrow_mut()
            .config
            .model
            .by_language
            .remove(&LanguageKey(lang));
        report(&self.window, self.ctx.save());
        self.state(|s| self.push_lang_rows(s));
    }

    /// The "Add language" popup's search: as "More" (name, code, alias), without Auto and the
    /// languages that have a row already.
    fn add_search(&self, query: &str) {
        let taken = self.ctx.config.borrow().config.model.by_language.clone();
        let found: Vec<&'static LanguageInfo> = languages::search(query)
            .into_iter()
            .filter(|i| {
                i.code
                    .parse::<Language>()
                    .is_ok_and(|l| l != Language::AUTO && !taken.contains_key(&LanguageKey(l)))
            })
            .collect();
        let rows: Vec<SharedString> = found.iter().map(|i| i.english.into()).collect();
        let notes: Vec<SharedString> = found.iter().map(|i| note(i).into()).collect();
        *self.added.borrow_mut() = found;
        self.state(|s| {
            s.set_add_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
            s.set_add_native(ModelRc::from(Rc::new(VecModel::from(notes))));
        });
    }

    /// A row of the "Add language" popup: the language gets a row with the first row's model.
    fn add_chosen(&self, i: i32) {
        let lang = usize::try_from(i)
            .ok()
            .and_then(|i| self.added.borrow().get(i).copied())
            .and_then(|info| info.code.parse::<Language>().ok());
        let Some(lang) = lang else { return };
        {
            let mut file = self.ctx.config.borrow_mut();
            let model = &mut file.config.model;
            let id = model.id.clone();
            model.by_language.insert(LanguageKey(lang), id);
        }
        report(&self.window, self.ctx.save());
        self.state(|s| {
            self.push_lang_rows(s);
            let row = self.lang_rows.borrow().iter().position(|(l, _)| *l == lang);
            s.set_lang_new(row.map_or(-1, int));
        });
    }

    /// The Advanced tab from the config: each parameter's choice, and the number beside it (what
    /// `auto` resolves to while it is auto, so turning it to a value starts from there).
    fn push_advanced(&self, s: &ModelState<'_>) {
        let file = self.ctx.config.borrow();
        let model = &file.config.model;
        let on_gpu = auto_on_gpu(
            &self.backend.borrow(),
            self.loaded.get(),
            self.listed.borrow().as_deref(),
            &model.gpu,
        );
        s.set_adv_gpu(on_gpu);
        let logical = thread::available_parallelism().map_or(1, usize::from);
        let r = tuning::resolve(&model.advanced, on_gpu, logical);
        let candidates = match r.sampling {
            Sampling::Greedy { best_of } => best_of,
            Sampling::Beam { beam_size } => beam_size,
        };
        let values = [
            f32::from(r.threads),
            0.0,
            f32::from(candidates),
            r.temperature,
            0.0,
            r.no_speech_threshold,
            r.logprob_threshold,
            r.entropy_threshold,
            0.0,
        ];
        s.set_adv_mode(ModelRc::from(Rc::new(VecModel::from(adv_modes(
            &model.advanced,
            logical,
            self.custom.get(),
        )))));
        s.set_adv_value(ModelRc::from(Rc::new(VecModel::from(values.to_vec()))));
        s.set_adv_prompt(model.advanced.initial_prompt.as_str().into());
        s.set_adv_prompt_source(prompt_source_index(model.advanced.prompt_source()));
    }

    fn adv_changed(&self, param: i32, mode: i32, value: f32) {
        let logical = thread::available_parallelism().map_or(1, usize::from);
        if let Some(bit) = u32::try_from(param).ok().filter(|b| *b < 9) {
            // Custom of a number with presets is the choice after them.
            let custom = preset_count(param).is_some_and(|n| mode == int(n + 1));
            let bits = self.custom.get() & !(1 << bit);
            self.custom.set(bits | (u16::from(custom) << bit));
        }
        {
            let mut file = self.ctx.config.borrow_mut();
            adv_apply(&mut file.config.model.advanced, param, mode, value, logical);
        }
        report(&self.window, self.ctx.save());
        self.state(|s| self.push_advanced(s));
    }

    fn adv_prompt_edited(&self, text: &str) {
        {
            let mut file = self.ctx.config.borrow_mut();
            let adv = &mut file.config.model.advanced;
            if adv.initial_prompt == text {
                return;
            }
            text.clone_into(&mut adv.initial_prompt);
        }
        // The field's own copy: a later push of the same text (Reset all) must still change it.
        self.state(|s| s.set_adv_prompt(text.into()));
        report(&self.window, self.ctx.save());
    }

    /// The prompt select: 0 the dictionary, 1 none, 2 the person's own text (kept while it is not
    /// the one in use).
    fn adv_prompt_source_picked(&self, i: i32) {
        let source = match i {
            1 => PromptSource::None,
            2 => PromptSource::Custom,
            _ => PromptSource::Dictionary,
        };
        self.ctx
            .config
            .borrow_mut()
            .config
            .model
            .advanced
            .prompt_source = Some(source);
        report(&self.window, self.ctx.save());
        self.state(|s| s.set_adv_prompt_source(prompt_source_index(source)));
    }

    /// "Reset to Auto" of one tab's tuning group (ia-map 3.2): 0 Speed (threads, flash
    /// attention), 1 Accuracy (every other parameter shown). The hint is the person's own text
    /// and stays; the device and the unload are in neither group.
    fn adv_reset(&self, group: i32) {
        // The Custom bits of the group's parameters: 0 and 1 Speed, 2 to 8 Accuracy.
        let bits: u16 = if group == 0 { 0b11 } else { 0b1_1111_1100 };
        self.custom.set(self.custom.get() & !bits);
        {
            let d = Advanced::default();
            let mut file = self.ctx.config.borrow_mut();
            let a = &mut file.config.model.advanced;
            if group == 0 {
                a.threads = d.threads;
                a.flash_attn = d.flash_attn;
            } else {
                a.decoding = d.decoding;
                a.best_of = d.best_of;
                a.beam_size = d.beam_size;
                a.temperature = d.temperature;
                a.temperature_fallback = d.temperature_fallback;
                a.no_speech_threshold = d.no_speech_threshold;
                a.logprob_threshold = d.logprob_threshold;
                a.entropy_threshold = d.entropy_threshold;
                a.suppress_non_speech = d.suppress_non_speech;
            }
        }
        report(&self.window, self.ctx.save());
        self.state(|s| self.push_advanced(s));
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
        let notes: Vec<SharedString> = found.iter().map(|i| note(i).into()).collect();
        let (rows, native) = (
            column(|i| i.english),
            ModelRc::from(Rc::new(VecModel::from(notes))),
        );
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
        self.state(|s| {
            self.push_language(s);
            // "Active now" follows the dictation language.
            self.push_lang_rows(s);
        });
        // The recommendation follows the language.
        self.judge();
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
        let mut icons = Vec::new();
        self.state(|s| icons = s.get_class_icons().iter().collect());
        let icon = |class: i32| {
            usize::try_from(class)
                .ok()
                .and_then(|i| icons.get(i).cloned())
                .unwrap_or_default()
        };
        let mut tiles: Vec<GalleryTile> = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let (hint, mark, mark_text) = tile_state(&r, in_use == Some(i));
                GalleryTile {
                    title: r.name.clone(),
                    caption: r.facts.clone(),
                    hint: hint.into(),
                    badge: if r.recommended {
                        RECOMMENDED.into()
                    } else {
                        SharedString::new()
                    },
                    icon: icon(r.class),
                    mark,
                    mark_text: mark_text.into(),
                    ..GalleryTile::default()
                }
            })
            .collect();
        tiles.push(GalleryTile {
            title: "Use my own model file".into(),
            caption: "Loaded as is".into(),
            hint: if in_use == Some(self.entries.len()) {
                IN_USE.into()
            } else {
                SharedString::new()
            },
            icon: icon(OWN_CLASS),
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
            self.bars.clear(&i);
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
                    toast(&window, &note);
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
        self.wait();
    }

    /// Measures the models folder and the free space of its drive on a worker (a network folder
    /// can stall), and the first time the system memory and graphics adapters; the answer updates
    /// the Files rows, the disk warnings and the recommendation. Runs on every file change the
    /// page hears of (open, download, delete), never on a timer of its own.
    fn measure(&self) {
        let m = &self.measuring;
        if m.busy.replace(true) {
            m.again.set(true);
            return;
        }
        let hardware = self.seen.borrow().adapters.is_none();
        let (tx, dir, window) = (
            m.tx.clone(),
            self.ctx.layout.models_dir(),
            self.window.clone(),
        );
        thread::spawn(move || {
            let measured = Measured {
                folder: models_size(&dir).map_err(|e| e.to_string()),
                free: sysinfo::free_space(&dir),
                hardware: hardware.then(|| (sysinfo::ram(), sysinfo::adapters())),
            };
            // A failed send: the page is gone (closed or rebound).
            if tx.send(measured).is_ok() {
                // Err: the window closed, or no event loop runs (tests call `invoke_measured`).
                let _ =
                    window.upgrade_in_event_loop(|w| w.global::<ModelState>().invoke_measured());
            }
        });
    }

    /// The measuring worker answered: apply it, and measure again if a file changed meanwhile.
    fn measured(&self) {
        let m = &self.measuring;
        while let Ok(answer) = m.rx.try_recv() {
            m.busy.set(false);
            self.apply_measure(answer);
        }
        if !m.busy.get() && m.again.replace(false) {
            self.measure();
        }
    }

    /// Polls the path checks' answers while any is out; the timer stops with the last answer.
    fn wait(&self) {
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

    fn apply_measure(&self, m: Measured) {
        {
            let mut seen = self.seen.borrow_mut();
            if let Some((ram, adapters)) = m.hardware {
                seen.ram = ram;
                seen.adapters = Some(adapters);
            }
            seen.free = m.free;
            seen.folder = Some(m.folder);
        }
        self.judge();
        self.push_partials();
    }

    /// Each row's recommendation, disk warning and "i" from the machine, the device "Run the model on"
    /// picks and the dictation language (`models::recommend`).
    fn judge(&self) {
        let (gpu, language) = {
            let file = self.ctx.config.borrow();
            (file.config.model.gpu.clone(), file.config.model.language)
        };
        let machine = {
            let seen = self.seen.borrow();
            Machine {
                ram: seen.ram,
                vram: seen.adapters.as_deref().and_then(|adapters| {
                    models::graphics_memory(self.listed.borrow().as_deref(), &gpu, adapters)
                }),
                free_disk: seen.free,
            }
        };
        let ready: Vec<bool> = self.rows.iter().map(|r| r.ready).collect();
        let parts: Vec<u64> = {
            let partials = self.partials.borrow();
            self.entries
                .iter()
                .map(|e| {
                    partials
                        .iter()
                        .find(|p| p.file_name == e.file_name)
                        .map_or(0, |p| p.size)
                })
                .collect()
        };
        // What each model has on disk: the whole file, its paused `.part`, or nothing.
        let pick = models::recommend(&machine, language, |id| {
            self.index_of(id).map_or(0, |i| {
                if ready[i] {
                    self.entries[i].size
                } else {
                    parts[i]
                }
            })
        });
        for (i, entry) in self.entries.iter().enumerate() {
            let recommended = pick.filter(|p| p.id == entry.id);
            let disk = models::disk_short(entry, parts[i], machine.free_disk)
                .map(|need| disk_note(need, machine.free_disk.unwrap_or(0)))
                .unwrap_or_default();
            let info = card_info(entry, recommended.map(|p| p.gpu));
            self.update(i, |r| {
                r.recommended = recommended.is_some();
                r.disk_off = disk.into();
                r.info = info.into();
            });
        }
        let picked = pick.and_then(|p| self.index_of(p.id));
        self.state(|s| s.set_recommended(picked.map_or(-1, int)));
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
            let note = format!("Deleted {} of unfinished downloads", size(freed));
            toast(&window, &note);
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
        self.state(|s| self.push_memory_tip(s));
    }

    fn load_at_start_toggled(&self, on: bool) {
        self.ctx.config.borrow_mut().config.model.preload_at_start = on;
        report(&self.window, self.ctx.save());
        self.state(|s| self.push_memory_tip(s));
    }

    /// The Home stats rows; none when the file is missing or unreadable (then no suggestion).
    fn read_days(&self) {
        let path = self.ctx.layout.data_dir.join(stats::FILE);
        *self.days.borrow_mut() = stats_store::read(&path).map(|s| s.days).unwrap_or_default();
    }

    /// What the daily counts suggest for the memory settings as they are now, unless dismissed.
    fn memory_advice(&self) -> Option<Advice<MemoryMode>> {
        let now = self.ctx.config.borrow().config.model.memory();
        suggest::memory(&self.days.borrow(), now)
            .filter(|a| !self.dismissed.borrow().contains(&memory_key(a.value)))
    }

    fn push_memory_tip(&self, s: &ModelState<'_>) {
        s.set_memory_tip(
            self.memory_advice()
                .map(memory_tip)
                .unwrap_or_default()
                .into(),
        );
    }

    /// The memory suggestion's Apply (both rows change) or Not now (kept in the data folder).
    fn memory_suggestion(&self, apply: bool) {
        let Some(advice) = self.memory_advice() else {
            return;
        };
        if apply {
            self.ctx
                .config
                .borrow_mut()
                .config
                .model
                .set_memory(advice.value);
            report(&self.window, self.ctx.save());
            self.state(|s| self.push_config(s));
            return;
        }
        let key = memory_key(advice.value);
        recent::add_dismissed(&self.ctx.layout.data_dir, &key);
        self.dismissed.borrow_mut().insert(key);
        self.state(|s| self.push_memory_tip(s));
    }

    fn memory_recheck(&self) {
        self.read_days();
        self.state(|s| self.push_memory_tip(s));
    }

    fn event(&self, event: &Event) {
        if let Event::Link(link) = event {
            self.state(|s| {
                s.set_download_off(download_off(*link).into());
                s.set_delete_off(delete_off(*link).into());
            });
            // The daemon's warnings are its own: with it gone they are stale.
            if *link != Link::Online {
                self.file_warning("");
            }
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
                self.state(|s| {
                    self.push_gpus(s);
                    self.push_advanced(s);
                });
                self.refresh_rows();
            }
            Event::Push(Push::Status {
                warnings,
                backend,
                loaded,
                ..
            }) => {
                // What Auto picks follows the device that is loaded (DS-11.4 h).
                let was_loaded = self.loaded.replace(*loaded);
                if *self.backend.borrow() != *backend || was_loaded != *loaded {
                    backend.clone_into(&mut self.backend.borrow_mut());
                    self.state(|s| self.push_advanced(s));
                    self.push_runs_on();
                }
                // The sound lines are the Audio page's (audio::sound_warning), the to-file line the
                // Text page's file row.
                self.file_warning(&super::text::file_warning(warnings));
                let ours: Vec<&str> = warnings
                    .iter()
                    .map(String::as_str)
                    .filter(|l| !l.starts_with("sounds.") && !l.starts_with(WRITE_FAILED))
                    .collect();
                self.state(|s| s.set_warning(ours.join("\n").into()));
            }
            Event::Push(Push::DownloadProgress { id, done, total }) => {
                if let Some(i) = self.index_of(id) {
                    // Real from the first byte (DS-26.4); the estimate stays flat meanwhile.
                    if !self.bars.live(&i) {
                        self.bars.start(i, Some(DOWNLOAD_EST));
                    }
                    self.bars.real(&i, fraction(*done, *total));
                    self.update(i, |r| {
                        r.downloading = true;
                        r.progress_text = strings::download_progress(*done, *total).into();
                        r.badge = strings::MODEL_BADGE_DOWNLOADING.into();
                    });
                    self.push_partials();
                }
            }
            Event::Push(Push::DownloadDone { id, result }) => {
                if let Some(i) = self.index_of(id) {
                    self.bars.clear(&i);
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
        self.push_runs_on();
        self.judge();
        self.push_partials();
        self.measure();
        // The language rows offer only downloaded models: a download or delete changes them.
        self.state(|s| self.push_lang_rows(s));
    }

    /// The "Delete unfinished downloads" button: off with nothing to delete or while a download
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
        let shown = match partials.len() {
            0 => "None".to_owned(),
            1 => format!("{} in 1 file", size(bytes)),
            n => format!("{} in {n} files", size(bytes)),
        };
        let (folder, error) = {
            let seen = self.seen.borrow();
            (
                folder_size(seen.folder.as_ref(), seen.free),
                match &seen.folder {
                    Some(Err(e)) => format!("Can't read the models folder: {e}"),
                    _ => String::new(),
                },
            )
        };
        self.state(|s| {
            s.set_partials_off(off.into());
            s.set_partials(shown.into());
            s.set_folder_size(folder.into());
            s.set_folder_error(error.into());
        });
    }

    /// The card's "runs on" from "Run the model on", the daemon's GPU list and the device it loaded on.
    fn push_runs_on(&self) {
        let gpu = self.ctx.config.borrow().config.model.gpu.clone();
        let text = runs_on(
            self.listed.borrow().as_deref(),
            &gpu,
            &self.backend.borrow(),
            self.loaded.get(),
        );
        self.state(|s| s.set_runs_on(text.into()));
    }
}

/// Whether an `auto` setting is judged for a GPU: the loaded device when the daemon says one
/// (`backend`, while `loaded`), else what the picker would pick among the listed GPUs (all but the CPU choice
/// while they are not listed yet). The CPU choice is the processor whatever is loaded.
fn auto_on_gpu(
    backend: &str,
    loaded: bool,
    listed: Option<&[Device]>,
    choice: &DeviceChoice,
) -> bool {
    if *choice == DeviceChoice::Cpu {
        return false;
    }
    match (loaded, listed) {
        (false, Some(gpus)) => pick(gpus, choice).index.is_some(),
        (false, None) => true,
        (true, _) => backend != "cpu",
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
    // What to do about a state the badge only names.
    let note = strings::model_status(status);
    let f = models::facts(entry.id);
    let facts = format!("{} · {} · {}", gb(entry.size), f.speed, f.languages);
    let detail = match part.filter(|_| status == ModelStatus::Partial) {
        Some(part) => format!(
            "{} · {}",
            strings::download_progress(part.size, entry.size),
            kept(models::days_kept(part.remove_at, now))
        ),
        None => note.to_owned(),
    };
    let on_disk =
        fs::metadata(dir.join(entry.file_name)).map_or(0, |m| m.len()) + part.map_or(0, |p| p.size);
    ModelRow {
        id: entry.id.into(),
        name: entry.display_name.into(),
        detail: detail.into(),
        facts: facts.into(),
        class: i32::try_from(f.class.index()).unwrap_or(OWN_CLASS),
        progress_text: SharedString::new(),
        // "Ready" said little (hold 35): a usable model is "Downloaded", checked or not.
        badge: match status {
            ModelStatus::Ready => DOWNLOADED,
            other => strings::model_badge(other),
        }
        .into(),
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
        info: card_info(entry, None).into(),
        disk_off: SharedString::new(),
        recommended: false,
        used_for: SharedString::new(),
    }
}

/// The models folder's line (the Files row, the Models tab header): what its finished files take
/// and the drive's free space; "Measuring" until the worker answers, a short error if it failed.
fn folder_size(folder: Option<&Result<(u64, usize), String>>, free: Option<u64>) -> String {
    let free = free
        .map(|f| format!(" · {} free", size(f)))
        .unwrap_or_default();
    let folder = match folder {
        None => return FOLDER_MEASURING.to_owned(),
        Some(Err(_)) => return FOLDER_UNREADABLE.to_owned(),
        Some(Ok(folder)) => *folder,
    };
    match folder {
        (0, _) => format!("Empty{free}"),
        (bytes, 1) => format!("{} in 1 model{free}", size(bytes)),
        (bytes, n) => format!("{} in {n} models{free}", size(bytes)),
    }
}

/// Why Download is off on a drive too full for the model: the card's "!" and the button's tip.
fn disk_note(need: u64, free: u64) -> String {
    format!(
        "Not enough disk space: needs {}, this drive has {} free.",
        size(need),
        size(free)
    )
}

/// The card's "i": what the model is for and the memory it needs, and why it is recommended
/// (`Some(true)` judged for a graphics card, `Some(false)` for the processor).
fn card_info(entry: &ModelEntry, recommended: Option<bool>) -> String {
    let f = models::facts(entry.id);
    let why = match recommended {
        Some(true) => "\nRecommended for this PC: it fits your graphics card's memory.",
        Some(false) => "\nRecommended for this PC: light enough for the processor.",
        None => "",
    };
    format!(
        "{}. Needs about {} of memory to run.{why}",
        f.note,
        size(f.memory)
    )
}

/// Where the models run, for the card: with a model loaded, the device the daemon says it loaded
/// on (`backend`: "cpu" after a GPU failure too, as `auto_on_gpu`); else the device "Run the model on" picks
/// among the listed GPUs; empty until the daemon lists them.
fn runs_on(
    listed: Option<&[Device]>,
    choice: &DeviceChoice,
    backend: &str,
    loaded: bool,
) -> String {
    if loaded && backend == "cpu" {
        return PROCESSOR.to_owned();
    }
    let Some(gpus) = listed else {
        return String::new();
    };
    let on = gpus.iter().position(|d| loaded && d.name == backend);
    match on.or(pick(gpus, choice).index) {
        Some(i) => format!("runs on {}", gpus[i].display_name()),
        None => PROCESSOR.to_owned(),
    }
}

/// The card's "runs on" on the CPU.
const PROCESSOR: &str = "runs on the processor";

/// The parameters of the Advanced tab, the order of `ModelState.adv-mode` and `adv-value`.
const P_THREADS: i32 = 0;
const P_FLASH: i32 = 1;
const P_DECODING: i32 = 2;
const P_TEMPERATURE: i32 = 3;
const P_FALLBACK: i32 = 4;
const P_NO_SPEECH: i32 = 5;
const P_LOGPROB: i32 = 6;
const P_ENTROPY: i32 = 7;
const P_SUPPRESS: i32 = 8;
/// The number beside Decoding; it has a value but no choice of its own.
const P_CANDIDATES: i32 = 9;

/// The named values between Auto and Custom of a number's choice (the choices of the page in
/// order: Auto, these, Custom); `None` for a parameter that is not a number or has only Auto.
fn presets(param: i32, logical: usize) -> Option<[f32; 2]> {
    match param {
        P_THREADS => Some(tuning::thread_presets(logical)),
        P_TEMPERATURE => Some(tuning::TEMPERATURE_PRESETS),
        P_NO_SPEECH => Some(tuning::NO_SPEECH_PRESETS),
        P_LOGPROB => Some(tuning::LOGPROB_PRESETS),
        P_ENTROPY => Some(tuning::ENTROPY_PRESETS),
        _ => None,
    }
}

/// How many presets `param` has, so Custom is the choice after them.
fn preset_count(param: i32) -> Option<usize> {
    presets(param, 1).map(|p| p.len())
}

/// The choice each parameter shows: 0 is Auto; a yes/no one then reads 1 On, 2 Off; decoding 1
/// Fast (greedy), 2 Accurate (beam); a number its presets, then Custom (see [`presets`]).
/// `custom` has a bit per parameter whose Custom the person picked.
fn adv_modes(a: &Advanced, logical: usize, custom: u16) -> Vec<i32> {
    let flag = |v: Auto<bool>| match v {
        Auto::Auto => 0,
        Auto::Set(true) => 1,
        Auto::Set(false) => 2,
    };
    let number = |param: i32, v: Auto<f32>| {
        let presets = presets(param, logical).unwrap_or_default();
        int(preset_mode(v, &presets, custom >> param & 1 == 1))
    };
    let threads = match a.threads {
        Auto::Auto => Auto::Auto,
        Auto::Set(n) => Auto::Set(f32::from(n)),
    };
    vec![
        number(P_THREADS, threads),
        flag(a.flash_attn),
        match a.decoding {
            Auto::Auto => 0,
            Auto::Set(Decoding::Greedy) => 1,
            Auto::Set(Decoding::Beam) => 2,
        },
        number(P_TEMPERATURE, a.temperature),
        flag(a.temperature_fallback),
        number(P_NO_SPEECH, a.no_speech_threshold),
        number(P_LOGPROB, a.logprob_threshold),
        number(P_ENTROPY, a.entropy_threshold),
        flag(a.suppress_non_speech),
    ]
}

/// The select of the prompt: 0 the dictionary, 1 none, 2 the person's own text.
fn prompt_source_index(source: PromptSource) -> i32 {
    match source {
        PromptSource::Dictionary => 0,
        PromptSource::None => 1,
        PromptSource::Custom => 2,
    }
}

/// Writes what the user chose for `param` (see [`adv_modes`]); numbers are put in range.
fn adv_apply(a: &mut Advanced, param: i32, mode: i32, value: f32, logical: usize) {
    let flag = |mode| match mode {
        1 => Auto::Set(true),
        2 => Auto::Set(false),
        _ => Auto::Auto,
    };
    let number = |range: &std::ops::RangeInclusive<f32>| {
        let presets = presets(param, logical).unwrap_or_default();
        match preset_value(usize::try_from(mode).unwrap_or(0), &presets, value) {
            Auto::Auto => Auto::Auto,
            Auto::Set(v) => Auto::Set(v.clamp(*range.start(), *range.end())),
        }
    };
    let whole = |v: f32, range: &std::ops::RangeInclusive<u8>| {
        // The range is a handful of small numbers, so the cast cannot lose any.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to a u8 range"
        )]
        let n = v
            .round()
            .clamp(f32::from(*range.start()), f32::from(*range.end())) as u8;
        n
    };
    match param {
        P_THREADS => {
            let range = Advanced::THREADS;
            let (low, high) = (f32::from(*range.start()), f32::from(*range.end()));
            a.threads = match number(&(low..=high)) {
                Auto::Auto => Auto::Auto,
                Auto::Set(n) => Auto::Set(whole(n, &range)),
            };
        }
        P_FLASH => a.flash_attn = flag(mode),
        P_DECODING => {
            a.decoding = match mode {
                1 => Auto::Set(Decoding::Greedy),
                2 => Auto::Set(Decoding::Beam),
                _ => Auto::Auto,
            };
        }
        // The number beside Decoding: the candidates of the mode it is on (Auto is greedy).
        P_CANDIDATES => {
            if a.decoding == Auto::Set(Decoding::Beam) {
                a.beam_size = whole(value, &Advanced::BEAM_SIZE);
            } else {
                a.best_of = whole(value, &Advanced::BEST_OF);
            }
        }
        P_TEMPERATURE => a.temperature = number(&Advanced::TEMPERATURE),
        P_FALLBACK => a.temperature_fallback = flag(mode),
        P_NO_SPEECH => a.no_speech_threshold = number(&Advanced::NO_SPEECH),
        P_LOGPROB => a.logprob_threshold = number(&Advanced::LOGPROB),
        P_ENTROPY => a.entropy_threshold = number(&Advanced::ENTROPY),
        P_SUPPRESS => a.suppress_non_speech = flag(mode),
        _ => {}
    }
}

/// What the model in use's tile says: its tooltip.
const IN_USE: &str = "In use";
/// A ready model that is not the one in use.
const DOWNLOADED: &str = "Downloaded";
/// The recommendation badge's word, on the tile.
const RECOMMENDED: &str = "Recommended";
/// The models folder's size until the worker measured it.
const FOLDER_MEASURING: &str = "Measuring";
/// The models folder line when the folder cannot be read; the row's "!" says why.
const FOLDER_UNREADABLE: &str = "Can't read the folder";
/// The marks of `GalleryTile.mark`.
const MARK_DONE: i32 = 1;
const MARK_GET: i32 = 2;

/// A tile's state, said once by the mark in its top-right slot (DS-28.2): the tooltip, the mark
/// and the percent while it downloads. The caption keeps the facts alone. The model in use has
/// the check badge in place of a mark.
fn tile_state(r: &ModelRow, in_use: bool) -> (String, i32, String) {
    let (state, percent) = if r.downloading {
        let percent = (r.progress * 100.0).round();
        (
            format!("{} {percent:.0}%", strings::MODEL_BADGE_DOWNLOADING),
            format!("{percent:.0} %"),
        )
    } else if r.ready {
        (DOWNLOADED.to_owned(), String::new())
    } else {
        (r.badge.to_string(), String::new())
    };
    let mark = match (in_use, r.downloading, r.ready) {
        (true, ..) | (_, true, _) => 0,
        (_, _, true) => MARK_DONE,
        _ => MARK_GET,
    };
    let hint = match (in_use, r.ready && !r.downloading) {
        (true, true) => IN_USE.to_owned(),
        (true, false) => format!("{IN_USE} · {state}"),
        _ => state,
    };
    (hint, mark, percent)
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

/// The key "Not now" keeps for a memory suggestion of `value`.
fn memory_key(value: MemoryMode) -> String {
    oxi_core::learn::dismiss_key("memory", &format!("{value:?}"))
}

/// The memory suggestion's words: what the counts showed and what Apply sets.
fn memory_tip(a: Advice<MemoryMode>) -> String {
    match a.value {
        MemoryMode::AlwaysReady => format!(
            "On your last {} days of use you dictated about {} times a day. Load the model at start and never free it?",
            a.of, a.hits
        ),
        _ => format!(
            "On your last {} days of use you dictated fewer than 3 times a day. Free the memory right after each dictation?",
            a.of
        ),
    }
}

/// Unload picker labels and what each writes: [`UNLOAD`], with the configured minutes added in
/// order when they are not one of them (an older slider value), so they stay picked.
/// A language's English name, the code for one the table does not know.
fn english(l: Language) -> &'static str {
    l.info().map_or(l.as_str(), |i| i.english)
}

/// The muted note beside a language in a search popup: the code in front of the native name, why
/// a search for "ua" or "jp" matched; the code alone where "EN · English" would say the name twice.
fn note(i: &LanguageInfo) -> String {
    let code = i.code.to_uppercase();
    if i.native == i.english {
        code
    } else {
        format!("{code} · {}", i.native)
    }
}

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

/// The first two rows of the "Run the model on" picker.
const DEVICE_AUTO: &str = "Auto (best available)";
const DEVICE_CPU: &str = "Processor (CPU)";

/// A listed GPU as a person calls it: the name its driver gives, then what it is (unless the
/// name already says "Graphics").
fn graphics_card(name: &str) -> String {
    if name.to_lowercase().contains("graphics") {
        name.to_owned()
    } else {
        format!("{name} (graphics card)")
    }
}

/// Picker labels and what each writes: Auto, the processor, the listed graphics cards by their display name, then the configured one
/// when it is not listed, so it stays picked. Before the daemon answers (`listed` is `None`) a
/// configured name is not called missing.
fn gpu_rows(
    listed: Option<&[Device]>,
    current: &DeviceChoice,
) -> (Vec<SharedString>, Vec<DeviceChoice>) {
    let mut rows = vec![
        (DEVICE_AUTO.to_owned(), DeviceChoice::Auto),
        (DEVICE_CPU.to_owned(), DeviceChoice::Cpu),
    ];
    for d in listed.unwrap_or_default() {
        rows.push((
            graphics_card(d.display_name()),
            DeviceChoice::Named(d.name.clone()),
        ));
    }
    if let DeviceChoice::Named(name) = current
        && !rows.iter().any(|(_, c)| c == current)
    {
        let label = if listed.is_some() {
            strings::not_connected(name)
        } else {
            graphics_card(name)
        };
        rows.push((label, current.clone()));
    }
    rows.into_iter()
        .map(|(l, c)| (SharedString::from(l), c))
        .unzip()
}

#[cfg(test)]
mod tests;
