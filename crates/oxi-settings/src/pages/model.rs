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

use oxi_core::config::{Advanced, Auto, Decoding, LanguageKey};
use oxi_core::device::{Device, DeviceChoice, pick};
use oxi_core::ipc::{DeleteResult, DownloadResult, Push, Request};
use oxi_core::languages::{self, LanguageInfo};
use oxi_core::models::{self, MODELS, ModelEntry, ModelStatus, PartialDownload};
use oxi_core::paste::file::WRITE_FAILED;
use oxi_core::paths::PART_SUFFIX;
use oxi_core::strings::{self, gb};
use oxi_core::tuning::{self, Sampling};
use oxi_core::types::Language;
use oxi_platform::fs::list_partials;
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel, Weak};

use super::progress::{Bars, DOWNLOAD_EST};
use super::toast;
use crate::constants::FOLDER_POLL;
use crate::generated::{GalleryTile, ModelRow, ModelState, TextState};
use crate::ipc::{Event, Link, Subscription};
use crate::{AppWindow, Ctx, Error, Page, report};

/// `[model] id` of "use my own .bin".
const CUSTOM: &str = "custom";
/// The glyph of "use my own .bin" in `ModelState.class-icons`, after the model classes.
const OWN_CLASS: i32 = 4;
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
        backend: RefCell::new(String::new()),
        loaded: Cell::new(false),
        choices: RefCell::new(Vec::new()),
        unloads: RefCell::new(Vec::new()),
        by_language: RefCell::new(Vec::new()),
        // Not a pair the config can hold: the first `push_language` takes the config's.
        slots: Cell::new([Language::AUTO; 2]),
        found: RefCell::new(Vec::new()),
        bars: Bars::new({
            let me = me.clone();
            move |&i, shown| {
                if let Some(page) = me.upgrade() {
                    page.update(i, |r| r.progress = shown);
                }
            }
        }),
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
    let p = Rc::clone(&page);
    s.on_by_language_shown(move |i| p.by_language_shown(i));
    let p = Rc::clone(&page);
    s.on_by_language_picked(move |lang, model| p.by_language_picked(lang, model));
    let p = Rc::clone(&page);
    s.on_adv_changed(move |param, mode, value| p.adv_changed(param, mode, value));
    let p = Rc::clone(&page);
    s.on_adv_prompt_edited(move |text| p.adv_prompt_edited(&text));
    let p = Rc::clone(&page);
    s.on_adv_reset(move || p.adv_reset());

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
    /// The daemon's `Status.backend` (`cpu`, `Vulkan0`, `none`); empty until it answers.
    backend: RefCell<String>,
    /// The daemon's `Status.loaded`: a model is in memory (what `backend` names is running).
    loaded: Cell<bool>,
    /// What each GPU picker row writes.
    choices: RefCell<Vec<DeviceChoice>>,
    /// What each unload picker row writes, as in [`UNLOAD`].
    unloads: RefCell<Vec<Option<u32>>>,
    /// The languages of the by-language row, in its order ([`by_language_order`]).
    by_language: RefCell<Vec<Language>>,
    /// The two language segments as shown, see [`languages::slots`].
    slots: Cell<[Language; 2]>,
    /// The rows of the language popup: what the search matched.
    found: RefCell<Vec<&'static LanguageInfo>>,
    /// The download bars by row: they show what the one progress driver answers, not the bytes
    /// as they come.
    bars: Rc<Bars<usize>>,
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
        self.push_by_language(s, None);
        self.push_advanced(s);
    }

    /// The by-language row (arch-plan 9.3), showing `shown`, else the dictation language when
    /// it is one, else the first language with a model.
    fn push_by_language(&self, s: &ModelState<'_>, shown: Option<Language>) {
        let file = self.ctx.config.borrow();
        let model = &file.config.model;
        let order = by_language_order(&model.by_language);
        let shown = shown
            .or_else(|| (model.language != Language::AUTO).then_some(model.language))
            .unwrap_or(order[0]);
        let at = order.iter().position(|l| *l == shown).unwrap_or(0);
        let names: Vec<SharedString> = order
            .iter()
            .map(|l| l.info().map_or(l.as_str(), |i| i.english).into())
            .collect();
        let ids = self.by_language_models(order[at]);
        let id = model.by_language.get(&LanguageKey(order[at]));
        let model_at = id.and_then(|id| ids.iter().position(|m| m.as_deref() == Some(id)));
        let missing = strings::model_badge(ModelStatus::Missing);
        let labels: Vec<SharedString> = ids
            .iter()
            .map(|id| match id.as_deref() {
                None => "Default".into(),
                Some(CUSTOM) => "Own model file".into(),
                Some(id) if self.downloaded(id) => {
                    models::find(id).map_or(id, |e| e.display_name).into()
                }
                // The language's own model, gone from disk: shown so it can be changed.
                Some(id) => format!(
                    "{}: {missing}",
                    models::find(id).map_or(id, |e| e.display_name)
                )
                .into(),
            })
            .collect();
        drop(file);
        s.set_by_language_names(ModelRc::from(Rc::new(VecModel::from(names))));
        s.set_by_language_models(ModelRc::from(Rc::new(VecModel::from(labels))));
        s.set_by_language(int(at));
        s.set_by_language_model(model_at.map_or(0, int));
        *self.by_language.borrow_mut() = order;
    }

    /// What each row of the by-language model list of `lang` writes: none (Default), the page's
    /// downloaded models, the own file when one is set, and `lang`'s own model when it is none of
    /// these (d04-fix 1: a model not on disk is not offered; the daemon falls back from it).
    fn by_language_models(&self, lang: Language) -> Vec<Option<String>> {
        let file = self.ctx.config.borrow();
        let model = &file.config.model;
        let own = !model.custom_path.trim().is_empty();
        let mut ids: Vec<Option<String>> = std::iter::once(None)
            .chain(
                self.entries
                    .iter()
                    .filter(|e| self.downloaded(e.id))
                    .map(|e| Some(e.id.to_owned())),
            )
            .chain(own.then(|| Some(CUSTOM.to_owned())))
            .collect();
        let current = model.by_language.get(&LanguageKey(lang)).cloned();
        if current.is_some() && !ids.contains(&current) {
            ids.push(current);
        }
        ids
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

    fn by_language_shown(&self, i: i32) {
        let lang = usize::try_from(i)
            .ok()
            .and_then(|i| self.by_language.borrow().get(i).copied());
        if let Some(lang) = lang {
            self.state(|s| self.push_by_language(s, Some(lang)));
        }
    }

    fn by_language_picked(&self, lang: i32, model: i32) {
        let lang = usize::try_from(lang)
            .ok()
            .and_then(|i| self.by_language.borrow().get(i).copied());
        let Some(lang) = lang else { return };
        let id = usize::try_from(model)
            .ok()
            .and_then(|i| self.by_language_models(lang).get(i).cloned());
        let Some(id) = id else { return };
        let mut file = self.ctx.config.borrow_mut();
        let map = &mut file.config.model.by_language;
        match id {
            Some(id) => map.insert(LanguageKey(lang), id),
            None => map.remove(&LanguageKey(lang)),
        };
        drop(file);
        report(&self.window, self.ctx.save());
        // A language that got or lost its model moves in the list: keep it shown.
        self.state(|s| self.push_by_language(s, Some(lang)));
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
        )))));
        s.set_adv_value(ModelRc::from(Rc::new(VecModel::from(values.to_vec()))));
        s.set_adv_prompt(model.advanced.initial_prompt.as_str().into());
    }

    fn adv_changed(&self, param: i32, mode: i32, value: f32) {
        {
            let mut file = self.ctx.config.borrow_mut();
            adv_apply(&mut file.config.model.advanced, param, mode, value);
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

    /// "Reset all to Auto": every parameter, the candidates and the prompt back to the defaults.
    fn adv_reset(&self) {
        self.ctx.config.borrow_mut().config.model.advanced = Advanced::default();
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
                    icon: icon(r.class),
                    mark,
                    mark_text: mark_text.into(),
                    ..GalleryTile::default()
                }
            })
            .collect();
        tiles.push(GalleryTile {
            title: "Use my own .bin".into(),
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
        self.push_partials();
        // The by-language list offers only downloaded models: a download or delete changes it.
        self.state(|s| {
            let shown = usize::try_from(s.get_by_language())
                .ok()
                .and_then(|i| self.by_language.borrow().get(i).copied());
            if shown.is_some() {
                self.push_by_language(s, shown);
            }
        });
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

/// The choice each parameter shows: 0 is Auto; a yes/no one then reads 1 On, 2 Off; decoding 1
/// Greedy, 2 Beam; the numbers 1 Custom.
fn adv_modes(a: &Advanced) -> Vec<i32> {
    let flag = |v: Auto<bool>| match v {
        Auto::Auto => 0,
        Auto::Set(true) => 1,
        Auto::Set(false) => 2,
    };
    let custom = |set: bool| i32::from(set);
    vec![
        custom(matches!(a.threads, Auto::Set(_))),
        flag(a.flash_attn),
        match a.decoding {
            Auto::Auto => 0,
            Auto::Set(Decoding::Greedy) => 1,
            Auto::Set(Decoding::Beam) => 2,
        },
        custom(matches!(a.temperature, Auto::Set(_))),
        flag(a.temperature_fallback),
        custom(matches!(a.no_speech_threshold, Auto::Set(_))),
        custom(matches!(a.logprob_threshold, Auto::Set(_))),
        custom(matches!(a.entropy_threshold, Auto::Set(_))),
        flag(a.suppress_non_speech),
    ]
}

/// Writes what the user chose for `param` (see [`adv_modes`]); numbers are put in range.
fn adv_apply(a: &mut Advanced, param: i32, mode: i32, value: f32) {
    let flag = |mode| match mode {
        1 => Auto::Set(true),
        2 => Auto::Set(false),
        _ => Auto::Auto,
    };
    let number = |range: &std::ops::RangeInclusive<f32>| {
        if mode == 0 {
            Auto::Auto
        } else {
            Auto::Set(value.clamp(*range.start(), *range.end()))
        }
    };
    let whole = |range: &std::ops::RangeInclusive<u8>| {
        // The range is a handful of small numbers, so the cast cannot lose any.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to a u8 range"
        )]
        let n = value
            .round()
            .clamp(f32::from(*range.start()), f32::from(*range.end())) as u8;
        n
    };
    match param {
        P_THREADS => {
            a.threads = if mode == 0 {
                Auto::Auto
            } else {
                Auto::Set(whole(&Advanced::THREADS))
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
                a.beam_size = whole(&Advanced::BEAM_SIZE);
            } else {
                a.best_of = whole(&Advanced::BEST_OF);
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

/// Unload picker labels and what each writes: [`UNLOAD`], with the configured minutes added in
/// order when they are not one of them (an older slider value), so they stay picked.
/// The languages of the by-language row: those with a model first (by code), then the rest in
/// the table's order.
fn by_language_order(map: &std::collections::BTreeMap<LanguageKey, String>) -> Vec<Language> {
    let all = languages::LANGUAGES
        .iter()
        .filter_map(|l| l.code.parse::<Language>().ok());
    let rest: Vec<Language> = all
        .filter(|l| !map.contains_key(&LanguageKey(*l)))
        .collect();
    map.keys().map(|k| k.0).chain(rest).collect()
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
mod tests;
