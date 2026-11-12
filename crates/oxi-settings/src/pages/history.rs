//! History page: the saved recordings (SPEC History, R1.3, R3.2, R3.3). The daemon lists them
//! (`HistoryList`, page after page, newest first), transcribes one again (`HistoryRetranscribe`;
//! `HistorySetText` then stores the text) and deletes one (`HistoryDelete`). The audio is read
//! from the folder here, to play and draw (docs/HISTORY-FORMAT.md).

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak as RcWeak};
use std::time::Duration;

use oxi_core::history::{self, DiffKind, Meta, Status, word_diff};
use oxi_core::ipc::{HistoryItem, HistoryOp, Push, Request, Spoken, TestFailure, TestOutcome};
use oxi_core::text::decode_text_file;
use oxi_core::tuning::SettingsSet;
use oxi_core::types::{Language, Millis};
use oxi_platform::capture::{CaptureError, play};
use slint::{ComponentHandle, ModelRc, StyledText, Timer, TimerMode, VecModel, Weak};

use crate::generated::{HistoryRow, HistoryState, Theme, Toast};
use crate::ipc::{Event, Link, Subscription};
use crate::{AppWindow, Ctx, Error, Page, report};

use super::progress::Bars;
use super::tuning::{BARS, clock, peaks, when};

/// Plays mono samples at a rate; dropping what it gives stops the sound. Tests swap in a silent one.
pub type Player = fn(&[f32], u32) -> Result<Box<dyn Any>, CaptureError>;

/// Playback ends this long after the clip: the device's buffer is still sounding.
const PLAY_TAIL: Duration = Duration::from_millis(300);
/// Run ids of this page, far from the Test page's (counting up from 1 on the same link): each page
/// takes only the answers to its own runs.
const FIRST_RUN: u32 = 1 << 31;

const NEEDS_DAEMON: &str = "Start OxiYapper to see the recordings";
const REDO_NEEDS_DAEMON: &str = "Start OxiYapper to transcribe";
const DELETE_NEEDS_DAEMON: &str = "Start OxiYapper to delete a recording";
const RUNNING: &str = "A recording is being transcribed";
const NOT_FOUND: &str = "The recording file is gone";
const HISTORY_ON: &str = "History is on. New dictations are kept.";
const NOTHING_HEARD: &str = "Nothing was heard in this recording.";
const NOT_SAVED: &str = "The new text could not be saved.";
const NOT_DELETED: &str = "The recording could not be deleted.";
const BUSY: &str = "OxiYapper is busy. Transcribe again in a moment.";
const LINK_LOST: &str = "OxiYapper stopped answering, so the transcription ended.";
const ADDED: &str = "Added to replacements.";
const WORD_ADDED: &str = "Added to the dictionary.";
const BAD_WORD: &str = "Select one line to add to the dictionary.";
const EDIT_NEEDS_DAEMON: &str = "Start OxiYapper to change the text";
const BAD_RULE: &str = "Write the heard word on one line, without \"=>\".";
const NOT_TEXT: &str = "The replacements file is not text. Fix it on the Text page.";
const WORDS_NOT_TEXT: &str = "The dictionary file is not text. Fix it on the Text page.";

pub fn bind(window: &AppWindow, ctx: &Ctx) {
    bind_with(window, ctx, play_cpal);
}

fn play_cpal(clip: &[f32], rate: u32) -> Result<Box<dyn Any>, CaptureError> {
    Ok(Box::new(play(clip, rate)?))
}

/// [`bind`] on another speaker.
pub fn bind_with(window: &AppWindow, ctx: &Ctx, player: Player) {
    let s = window.global::<HistoryState>();
    let page = Rc::new_cyclic(|me: &RcWeak<PageState>| PageState {
        _events: ctx.ipc.subscribe({
            let me = me.clone();
            move |event| {
                if let Some(page) = me.upgrade() {
                    page.event(event);
                }
            }
        }),
        me: me.clone(),
        window: window.as_weak(),
        ctx: ctx.clone(),
        player,
        items: RefCell::new(Vec::new()),
        rows: Rc::new(VecModel::default()),
        fresh: Cell::new(false),
        added_since: RefCell::new(Vec::new()),
        loading: Cell::new(false),
        shown_id: RefCell::new(None),
        audible: Cell::new(false),
        playing: RefCell::new(None),
        play_timer: Timer::default(),
        run: RefCell::new(None),
        bar: Bars::new({
            let window = window.as_weak();
            move |(), shown| {
                if let Some(window) = window.upgrade() {
                    window.global::<HistoryState>().set_progress(shown);
                }
            }
        }),
        next_run: Cell::new(FIRST_RUN),
        other: RefCell::new(None),
        restored: Cell::new(false),
        diff_open: Cell::new(false),
        edit: RefCell::new(None),
    });
    s.set_items(ModelRc::from(page.rows.clone()));
    s.set_selected(0);
    s.set_peaks(ModelRc::default());
    s.set_progress(0.0);
    page.refresh(&s);

    let p = Rc::clone(&page);
    s.on_shown(move || p.shown());
    let p = Rc::clone(&page);
    s.on_select(move |i| p.select(i));
    let p = Rc::clone(&page);
    s.on_play(move || p.play());
    let p = Rc::clone(&page);
    s.on_redo(move || p.redo());
    let p = Rc::clone(&page);
    s.on_delete(move || p.delete());
    let p = Rc::clone(&page);
    s.on_turn_on(move || p.turn_on());
    let p = Rc::clone(&page);
    s.on_show_folder(move || p.show_folder());
    let p = Rc::clone(&page);
    s.on_restore(move || p.restore());
    let p = Rc::clone(&page);
    s.on_toggle_diff(move || p.toggle_diff());
    let p = Rc::clone(&page);
    s.on_restyle(move || p.restyle());
    let p = Rc::clone(&page);
    s.on_slice(move |a, b| p.slice(a, b).unwrap_or_default().into());
    let p = Rc::clone(&page);
    s.on_always_fix(move |a, b, wanted| p.always_fix(a, b, &wanted));
    let p = Rc::clone(&page);
    s.on_fix_here(move |a, b, wanted| p.fix_here(a, b, &wanted));
    let p = Rc::clone(&page);
    s.on_add_word(move |a, b| p.add_word(a, b));
}

/// A Transcribe again that is out.
struct Run {
    id: u32,
    entry: String,
    set: SettingsSet,
    language: Language,
    /// The new text, once heard; then `HistorySetText` is out.
    text: Option<String>,
}

struct PageState {
    _events: Subscription,
    me: RcWeak<PageState>,
    window: Weak<AppWindow>,
    ctx: Ctx,
    player: Player,
    /// Newest first.
    items: RefCell<Vec<HistoryItem>>,
    rows: Rc<VecModel<HistoryRow>>,
    /// The next list answer replaces the list instead of joining it (entries the limits removed
    /// go).
    fresh: Cell<bool>,
    /// Entries the daemon announced while a fresh list was out: the answer may have been read
    /// before they were saved, so they survive its clearing.
    added_since: RefCell<Vec<HistoryItem>>,
    loading: Cell<bool>,
    /// The entry whose waveform is drawn.
    shown_id: RefCell<Option<String>>,
    /// Its audio could be read.
    audible: Cell<bool>,
    playing: RefCell<Option<Box<dyn Any>>>,
    play_timer: Timer,
    run: RefCell<Option<Run>>,
    /// The bar of the run: the one progress driver, not a percent of its own.
    bar: Rc<Bars<()>>,
    next_run: Cell<u32>,
    /// The other version of the picked entry's text: the one before the last new text, or the one
    /// a Restore put aside (`Meta::previous`, read from its json when it is picked).
    other: RefCell<Option<String>>,
    /// The text shown is the old version.
    restored: Cell<bool>,
    /// The text area shows what changed from `other`.
    diff_open: Cell<bool>,
    /// An edit that is out: the entry and the whole new text.
    edit: RefCell<Option<(String, String)>>,
}

impl PageState {
    fn state(&self, f: impl FnOnce(&HistoryState<'_>)) {
        if let Some(window) = self.window.upgrade() {
            f(&window.global::<HistoryState>());
        }
    }

    fn toast(&self, text: &str) {
        if let Some(window) = self.window.upgrade() {
            window.global::<Toast>().invoke_show(text.into());
        }
    }

    fn open(&self) -> bool {
        self.window
            .upgrade()
            .is_some_and(|w| w.get_page() == Page::History)
    }

    fn online(&self) -> bool {
        self.ctx.ipc.link() == Link::Online
    }

    fn refresh(&self, s: &HistoryState<'_>) {
        s.set_history_on(self.ctx.config.borrow().config.history.enabled);
        s.set_link_off(self.ctx.ipc.link().off_reason(NEEDS_DAEMON).into());
        s.set_loading(self.loading.get());
        s.set_playing(self.playing.borrow().is_some());
        s.set_play_off(if self.audible.get() { "" } else { NOT_FOUND }.into());
        let run = self.run.borrow();
        let redo_off = if !self.online() {
            self.ctx.ipc.link().off_reason(REDO_NEEDS_DAEMON)
        } else if run.is_some() {
            RUNNING
        } else {
            ""
        };
        s.set_redo_off(redo_off.into());
        s.set_running_id(run.as_ref().map_or("", |r| r.entry.as_str()).into());
    }

    /// The page was opened: list from the newest again.
    fn shown(&self) {
        self.fresh.set(true);
        self.list(None);
        self.state(|s| {
            self.refresh(s);
            self.show_versions(s);
        });
    }

    fn list(&self, older_than: Option<String>) {
        if self.open() && self.online() {
            self.loading.set(true);
            self.ctx.ipc.send(Request::HistoryList { older_than });
        }
    }

    /// A page of the list came (also one the Test page asked for): join it, and ask for the next
    /// while this page is open, so the whole history shows.
    fn listed(&self, items: &[HistoryItem], more: bool) {
        let keep = self.selected_id();
        {
            let mut held = self.items.borrow_mut();
            if self.fresh.replace(false) {
                held.clear();
                held.append(&mut self.added_since.borrow_mut());
            }
            for item in items {
                match held.iter_mut().find(|h| h.id == item.id) {
                    Some(h) => *h = item.clone(),
                    None => held.push(item.clone()),
                }
            }
            // Ids are `YYYY-MM-DD/HHMMSS-mmm`: they sort by time.
            held.sort_by(|a, b| b.id.cmp(&a.id));
        }
        self.loading.set(false);
        match items.last() {
            Some(last) if more => self.list(Some(last.id.clone())),
            _ => {}
        }
        self.show_rows(keep.as_deref(), 0);
    }

    /// The daemon saved a recording: it joins the list (the newest goes on top), and the pick,
    /// the scroll position and what the page is doing stay.
    fn added(&self, item: &HistoryItem) {
        let keep = self.selected_id();
        if self.fresh.get() {
            self.added_since.borrow_mut().push(item.clone());
        }
        {
            let mut held = self.items.borrow_mut();
            match held.iter_mut().find(|h| h.id == item.id) {
                Some(h) => *h = item.clone(),
                None => held.push(item.clone()),
            }
            held.sort_by(|a, b| b.id.cmp(&a.id));
        }
        self.show_rows(keep.as_deref(), 0);
    }

    /// The limits removed recordings: their rows go. A removed pick moves to the row that took
    /// its place.
    fn removed(&self, ids: &[String]) {
        let at = self
            .selected_id()
            .and_then(|id| self.items.borrow().iter().position(|i| i.id == id))
            .unwrap_or(0);
        let keep = self.selected_id().filter(|id| !ids.contains(id));
        self.items.borrow_mut().retain(|i| !ids.contains(&i.id));
        self.added_since
            .borrow_mut()
            .retain(|i| !ids.contains(&i.id));
        self.show_rows(keep.as_deref(), at);
    }

    /// Fills the list, keeping `keep` picked, else the row at `at` (clamped).
    fn show_rows(&self, keep: Option<&str>, at: usize) {
        let folder = self.folder();
        let items = self.items.borrow();
        self.rows
            .set_vec(items.iter().map(|i| row(i, &folder)).collect::<Vec<_>>());
        let at = keep
            .and_then(|id| items.iter().position(|i| i.id == id))
            .unwrap_or(at)
            .min(items.len().saturating_sub(1));
        drop(items);
        self.state(|s| s.set_selected(i32::try_from(at).unwrap_or(0)));
        self.picked();
    }

    fn selected_id(&self) -> Option<String> {
        let window = self.window.upgrade()?;
        let i = usize::try_from(window.global::<HistoryState>().get_selected()).ok()?;
        self.items.borrow().get(i).map(|item| item.id.clone())
    }

    fn select(&self, i: i32) {
        self.state(|s| s.set_selected(i));
        self.picked();
    }

    /// Draws the picked entry's waveform when the pick changed.
    fn picked(&self) {
        let id = self.selected_id();
        if *self.shown_id.borrow() != id {
            self.stop_playing();
            self.load_other(id.as_deref());
            let audio = id.as_deref().and_then(|id| self.read(id));
            self.audible.set(audio.is_some());
            let bars = audio.map_or_else(Vec::new, |(samples, _)| peaks(&samples, BARS));
            self.state(|s| s.set_peaks(ModelRc::from(Rc::new(VecModel::from(bars)))));
            *self.shown_id.borrow_mut() = id;
        }
        self.state(|s| {
            self.refresh(s);
            self.show_versions(s);
        });
    }

    fn folder(&self) -> PathBuf {
        let config = self.ctx.config.borrow();
        history::folder(&config.config.history, &self.ctx.layout.history_dir())
    }

    /// The samples of entry `id` and their rate; `None` when they cannot be read.
    ///
    /// shortcut: reads the whole file (a 30 min recording at 48 kHz is about 350 MB of samples);
    /// stream it when recordings that long are common.
    fn read(&self, id: &str) -> Option<(Vec<f32>, u32)> {
        if !history::is_id(id) {
            return None;
        }
        oxi_platform::wav::read_wav_mono_f32(&wav_path(&self.folder(), id)).ok()
    }

    /// Play, or Stop while it plays.
    fn play(&self) {
        if self.playing.borrow().is_some() {
            self.stop_playing();
        } else if let Some((samples, rate)) = self.selected_id().and_then(|id| self.read(&id)) {
            match (self.player)(&samples, rate) {
                Ok(sound) => {
                    *self.playing.borrow_mut() = Some(sound);
                    let length =
                        Duration::from_millis(samples.len() as u64 * 1000 / u64::from(rate.max(1)));
                    let me = self.me.clone();
                    self.play_timer
                        .start(TimerMode::SingleShot, length + PLAY_TAIL, move || {
                            if let Some(page) = me.upgrade() {
                                page.stop_playing();
                                page.state(|s| page.refresh(s));
                            }
                        });
                }
                Err(e) => self.toast(&e.to_string()),
            }
        } else {
            self.audible.set(false);
            self.toast(NOT_FOUND);
        }
        self.state(|s| self.refresh(s));
    }

    fn stop_playing(&self) {
        self.play_timer.stop();
        self.playing.borrow_mut().take();
    }

    /// Transcribe the picked entry again with the settings as they are now.
    fn redo(&self) {
        if !self.online() || self.run.borrow().is_some() {
            return;
        }
        let Some(entry) = self.selected_id() else {
            return;
        };
        let (set, language) = {
            let config = &self.ctx.config.borrow().config;
            let set = SettingsSet {
                normalize: config.audio.normalize,
                denoise: config.audio.denoise,
                trim: config.audio.trim,
                model: config.model.id.clone(),
            };
            (set, config.model.language)
        };
        let id = self.next_run.get();
        self.next_run.set(id.wrapping_add(1).max(FIRST_RUN));
        self.ctx.ipc.send(Request::HistoryRetranscribe {
            run: id,
            id: entry.clone(),
            set: set.clone(),
            language,
        });
        *self.run.borrow_mut() = Some(Run {
            id,
            entry,
            set,
            language,
            text: None,
        });
        self.bar.clear(&());
        self.state(|s| self.refresh(s));
    }

    /// The daemon answered a run: store a heard text, or say why there is none.
    fn answered(&self, id: u32, outcome: &TestOutcome) {
        let mut slot = self.run.borrow_mut();
        let Some(run) = slot.as_mut().filter(|r| r.id == id && r.text.is_none()) else {
            return;
        };
        let why = match outcome {
            TestOutcome::Heard { text, .. } if !text.trim().is_empty() => {
                run.text = Some(text.clone());
                self.ctx.ipc.send(Request::HistorySetText {
                    id: run.entry.clone(),
                    text: Spoken(text.clone()),
                    set: run.set.clone(),
                    language: run.language,
                });
                self.bar.finish(&());
                return;
            }
            TestOutcome::Heard { .. } => NOTHING_HEARD,
            TestOutcome::Failed { why } => failed(*why),
        };
        *slot = None;
        drop(slot);
        self.bar.fail(&());
        self.toast(why);
        self.state(|s| self.refresh(s));
    }

    /// The daemon stored a text, swapped the versions or deleted an entry.
    fn changed(&self, id: &str, op: HistoryOp, ok: bool) {
        match op {
            HistoryOp::SetText => {
                let run = self
                    .run
                    .borrow_mut()
                    .take_if(|r| r.entry == id && r.text.is_some());
                let Some(run) = run else {
                    return;
                };
                if ok {
                    self.new_text(id, run.text.unwrap_or_default(), |item| {
                        item.model = run.set.model;
                        run.language.as_str().clone_into(&mut item.language);
                    });
                } else {
                    self.toast(NOT_SAVED);
                    self.state(|s| self.refresh(s));
                }
            }
            HistoryOp::Edit => {
                let edit = self.edit.borrow_mut().take_if(|(entry, _)| entry == id);
                match edit {
                    Some((_, text)) if ok => self.new_text(id, text, |_| {}),
                    Some(_) => self.toast(NOT_SAVED),
                    None => {}
                }
            }
            HistoryOp::Restore if ok => {
                let Some(other) = self.other.borrow_mut().take() else {
                    return;
                };
                let swapped = self
                    .items
                    .borrow_mut()
                    .iter_mut()
                    .find(|i| i.id == id)
                    .map(|item| std::mem::replace(&mut item.text, Spoken(other)).0);
                *self.other.borrow_mut() = swapped;
                self.restored.set(!self.restored.get());
                self.diff_open.set(false);
                self.show_rows(Some(id), 0);
            }
            HistoryOp::Restore => self.toast(NOT_SAVED),
            HistoryOp::Delete if ok => {
                let at = self.items.borrow().iter().position(|i| i.id == id);
                if let Some(at) = at {
                    let keep = self.selected_id().filter(|picked| picked != id);
                    self.items.borrow_mut().remove(at);
                    self.show_rows(keep.as_deref(), at);
                }
            }
            HistoryOp::Delete => self.toast(NOT_DELETED),
        }
    }

    /// Entry `id` got `text`: it is shown at once, the old text becomes the other version and the
    /// text area shows what changed. `with` sets what else the entry says about the new text.
    fn new_text(&self, id: &str, text: String, with: impl FnOnce(&mut HistoryItem)) {
        let old = self
            .items
            .borrow_mut()
            .iter_mut()
            .find(|i| i.id == id)
            .map(|item| {
                with(item);
                item.status = Status::Done;
                std::mem::replace(&mut item.text, Spoken(text)).0
            });
        *self.other.borrow_mut() = old.filter(|t| !t.trim().is_empty());
        self.restored.set(false);
        self.diff_open.set(true);
        self.show_rows(Some(id), 0);
    }

    /// The other version of entry `id`, from its json; also resets what the page shows of it.
    fn load_other(&self, id: Option<&str>) {
        let folder = self.folder();
        let kept = id.filter(|id| history::is_id(id)).and_then(|id| {
            let json = fs::read_to_string(entry_path(&folder, id, history::JSON)).ok()?;
            Meta::from_json(&json).ok()?.previous
        });
        // The record says which version is shown, so a re-pick does not forget a Restore.
        self.restored.set(kept.as_ref().is_some_and(|p| p.newer));
        *self.other.borrow_mut() = kept.map(|p| p.text);
        self.diff_open.set(false);
    }

    /// What the page says of the versions: the strip, and the diff while it is open.
    fn show_versions(&self, s: &HistoryState<'_>) {
        let other = self.other.borrow();
        s.set_has_other(other.is_some());
        s.set_restored(self.restored.get());
        let open = self.diff_open.get() && other.is_some();
        s.set_diff_open(open);
        if let (true, Some(other), Some(shown)) = (open, other.as_deref(), self.shown_text()) {
            let colours = self.window.upgrade().map(|w| {
                let theme = w.global::<Theme>();
                (
                    hex(theme.get_text_secondary()),
                    hex(theme.get_success_text()),
                )
            });
            let (removed, added) = colours.unwrap_or_default();
            let markup = diff_markup(other, &shown, &removed, &added);
            s.set_diff(
                StyledText::from_markdown(&markup)
                    .unwrap_or_else(|_| StyledText::from_plain_text(&shown)),
            );
        }
    }

    /// The theme changed: the diff carries colours of its own, so it is made again.
    fn restyle(&self) {
        if self.diff_open.get() {
            self.state(|s| self.show_versions(s));
        }
    }

    fn toggle_diff(&self) {
        self.diff_open.set(!self.diff_open.get());
        self.state(|s| self.show_versions(s));
    }

    /// Swap the shown text with its other version (Restore old, Use new).
    fn restore(&self) {
        if !self.online() {
            self.toast(self.ctx.ipc.link().off_reason(EDIT_NEEDS_DAEMON));
        } else if self.other.borrow().is_some()
            && let Some(id) = self.selected_id()
        {
            self.ctx.ipc.send(Request::HistoryRestore { id });
        }
    }

    /// The picked entry's text as the page shows it (what the selection's offsets count in).
    fn shown_text(&self) -> Option<String> {
        let window = self.window.upgrade()?;
        let i = usize::try_from(window.global::<HistoryState>().get_selected()).ok()?;
        self.items
            .borrow()
            .get(i)
            .map(|item| item.text.0.trim().to_owned())
    }

    /// The shown text between two byte offsets; `None` for a bad range.
    fn slice(&self, a: i32, b: i32) -> Option<String> {
        let (a, b) = (usize::try_from(a).ok()?, usize::try_from(b).ok()?);
        self.shown_text()?.get(a..b).map(str::to_owned)
    }

    /// Opens the folder of the picked recording in the file manager.
    fn show_folder(&self) {
        let Some(id) = self.selected_id().filter(|id| history::is_id(id)) else {
            return;
        };
        let wav = entry_path(&self.folder(), &id, history::WAV);
        let opened = oxi_platform::open::open(wav.parent().unwrap_or(&self.folder()));
        report(&self.window, opened.map_err(Error::from));
    }

    /// Fix here only: the selected words become `wanted` in this recording's text.
    fn fix_here(&self, a: i32, b: i32, wanted: &str) -> bool {
        let (Some(text), Some(id), Some(from)) = (
            self.shown_text(),
            self.selected_id(),
            usize::try_from(a).ok(),
        ) else {
            return false;
        };
        let Some(to) = usize::try_from(b)
            .ok()
            .filter(|to| text.get(from..*to).is_some())
        else {
            return false;
        };
        let new = format!("{}{}{}", &text[..from], wanted.trim(), &text[to..]);
        if new == text {
            return true;
        }
        if !self.online() {
            self.toast(self.ctx.ipc.link().off_reason(EDIT_NEEDS_DAEMON));
            return false;
        }
        self.ctx.ipc.send(Request::HistoryEdit {
            id: id.clone(),
            text: Spoken(new.clone()),
        });
        *self.edit.borrow_mut() = Some((id, new));
        true
    }

    /// Always fix: the selected words become a replacement for every later dictation.
    fn always_fix(&self, a: i32, b: i32, wanted: &str) -> bool {
        self.slice(a, b)
            .is_some_and(|heard| self.add_replacement(&heard, wanted))
    }

    /// Add to dictionary: the selected words become a line of the dictionary.
    fn add_word(&self, a: i32, b: i32) -> bool {
        let word = self.slice(a, b).map(|w| w.trim().to_owned());
        let Some(word) = word.filter(|w| !w.is_empty() && !w.contains(['\n', '\r'])) else {
            self.toast(BAD_WORD);
            return false;
        };
        match append_line(&self.ctx.layout.dictionary_file(), &word) {
            Ok(true) => {
                self.toast(WORD_ADDED);
                true
            }
            Ok(false) => {
                self.toast(WORDS_NOT_TEXT);
                false
            }
            Err(e) => {
                report(&self.window, Err(Error::from(e)));
                false
            }
        }
    }

    fn delete(&self) {
        if !self.online() {
            self.toast(self.ctx.ipc.link().off_reason(DELETE_NEEDS_DAEMON));
            return;
        }
        if let Some(id) = self.selected_id() {
            self.stop_playing();
            self.ctx.ipc.send(Request::HistoryDelete { id });
            self.state(|s| self.refresh(s));
        }
    }

    fn turn_on(&self) {
        // A disabled button still runs its accessible default action.
        if self.ctx.config.borrow().read_only {
            return;
        }
        self.ctx.config.borrow_mut().config.history.enabled = true;
        let saved = self.ctx.save();
        if saved.is_ok() {
            self.toast(HISTORY_ON);
            self.shown();
        }
        report(&self.window, saved);
        self.state(|s| self.refresh(s));
    }

    fn add_replacement(&self, heard: &str, wanted: &str) -> bool {
        let Some(line) = rule_line(heard, wanted) else {
            self.toast(BAD_RULE);
            return false;
        };
        match append_line(&self.ctx.layout.replace_file(), &line) {
            Ok(true) => {
                self.toast(ADDED);
                true
            }
            Ok(false) => {
                self.toast(NOT_TEXT);
                false
            }
            Err(e) => {
                report(&self.window, Err(Error::from(e)));
                false
            }
        }
    }

    fn event(&self, event: &Event) {
        match event {
            Event::Push(Push::History { items, more }) => self.listed(items, *more),
            Event::Push(Push::HistoryAdded { item }) => self.added(item),
            Event::Push(Push::HistoryRemoved { ids }) => self.removed(ids),
            Event::Push(Push::TestProgress {
                id,
                percent,
                est_ms,
            }) => {
                if self.run.borrow().as_ref().is_some_and(|r| r.id == *id) {
                    // The first push carries the estimate: the bar starts there, and until it
                    // does the bar is the indeterminate sweep (the model may still be loading).
                    if !self.bar.live(&()) {
                        self.bar.start((), est_ms.map(|ms| Millis(u64::from(ms))));
                    }
                    self.bar.real(&(), f32::from((*percent).min(100)) / 100.0);
                }
            }
            Event::Push(Push::TestResult { id, outcome }) => self.answered(*id, outcome),
            Event::Push(Push::HistoryResult { id, op, ok }) => self.changed(id, *op, *ok),
            Event::Link(link) => {
                if *link == Link::Online {
                    self.fresh.set(true);
                    self.list(None);
                } else {
                    self.loading.set(false);
                    if self.run.borrow_mut().take().is_some() {
                        self.bar.fail(&());
                        self.toast(LINK_LOST);
                    }
                }
                self.state(|s| self.refresh(s));
            }
            Event::Push(_) => {}
        }
    }
}

/// One line of the list.
fn row(item: &HistoryItem, folder: &Path) -> HistoryRow {
    let text = item.text.0.trim();
    HistoryRow {
        id: item.id.as_str().into(),
        when: when(&item.id).into(),
        duration: clock(item.duration_ms).into(),
        status: status_label(item.status).into(),
        first: text.lines().next().unwrap_or_default().into(),
        text: text.into(),
        path: wav_path(folder, &item.id).display().to_string().into(),
    }
}

/// How an entry that did not finish shows; empty for a finished one.
fn status_label(status: Status) -> &'static str {
    match status {
        Status::Done => "",
        Status::Cancelled => "Cancelled",
        Status::Interrupted => "Interrupted",
        Status::Failed => "Failed",
    }
}

/// `<folder>/<day>/<stem>.wav` of the id `<day>/<stem>`, with the OS's separators.
fn wav_path(folder: &Path, id: &str) -> PathBuf {
    entry_path(folder, id, history::WAV)
}

/// The file with extension `ext` of entry `id`.
fn entry_path(folder: &Path, id: &str, ext: &str) -> PathBuf {
    let mut path = id
        .split('/')
        .fold(folder.to_path_buf(), |p, part| p.join(part));
    path.set_extension(ext);
    path
}

/// `#rrggbb` of an opaque colour, as the styled text's `<font color>` reads it.
fn hex(c: slint::Color) -> String {
    format!("#{:02X}{:02X}{:02X}", c.red(), c.green(), c.blue())
}

/// Markdown for the text area while the changes show: words only in `old` struck through in the
/// `removed` colour, words only in `new` underlined in the `added` colour (so colour is not the
/// only sign; styled text has no background, so no tint). Every character of the text is escaped,
/// so dictated text never reads as markup.
fn diff_markup(old: &str, new: &str, removed: &str, added: &str) -> String {
    let mut out = String::new();
    for part in word_diff(old, new) {
        let core = part.text.trim();
        let lead = &part.text[..part.text.len() - part.text.trim_start().len()];
        let trail = &part.text[lead.len() + core.len()..];
        let (open, close) = match part.kind {
            DiffKind::Same => (String::new(), ""),
            DiffKind::Removed => (format!("<font color=\"{removed}\">~~"), "~~</font>"),
            DiffKind::Added => (format!("<font color=\"{added}\"><u>"), "</u></font>"),
        };
        escape_markdown(lead, &mut out);
        if !core.is_empty() {
            out.push_str(&open);
            escape_markdown(core, &mut out);
            out.push_str(close);
        }
        escape_markdown(trail, &mut out);
    }
    out
}

/// `text` as markdown text: punctuation escaped, a line end a hard break.
fn escape_markdown(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '\r' => {}
            '\n' => out.push_str("\\\n"),
            c if c.is_ascii_punctuation() => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
}

/// Why a run gave no text.
fn failed(why: TestFailure) -> &'static str {
    match why {
        TestFailure::ModelMissing => "The model is not downloaded.",
        TestFailure::ModelBad => "The model failed to load.",
        TestFailure::NoAudio => NOT_FOUND,
        TestFailure::Busy => BUSY,
        TestFailure::Failed => "The transcription failed.",
    }
}

/// `heard => wanted` as one replacements line; `None` when it would not read back as that rule.
fn rule_line(heard: &str, wanted: &str) -> Option<String> {
    let (heard, wanted) = (heard.trim(), wanted.trim());
    let line = format!("{heard} => {wanted}");
    let one_rule = !heard.is_empty() && !heard.contains("=>") && !line.contains(['\n', '\r']);
    one_rule.then_some(line)
}

/// Appends `line` to the replacements file at `path`, a missing file being an empty one, in the
/// file's own line ending. `Ok(false)`: the file is not text and stays as it is. The Text page
/// sees the change on disk and takes it (or flags it against unsaved edits).
fn append_line(path: &Path, line: &str) -> io::Result<bool> {
    let mut text = match fs::read(path) {
        Ok(bytes) => match decode_text_file(&bytes) {
            Ok(text) => text,
            Err(_) => return Ok(false),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    if !text.is_empty() && !text.ends_with('\n') {
        text.push_str(newline);
    }
    text.push_str(line);
    text.push_str(newline);
    oxi_platform::fs::write_atomic(path, text.as_bytes())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::Receiver;

    use oxi_core::ipc::Request;
    use slint::{ComponentHandle, Model};

    use super::*;
    use crate::ipc::{Cmd, Ipc};
    use crate::testing::{has, linked};

    #[expect(clippy::unnecessary_wraps, reason = "the `Player` signature")]
    fn silent(_: &[f32], _: u32) -> Result<Box<dyn Any>, CaptureError> {
        Ok(Box::new(()))
    }

    fn item(id: &str, status: Status, text: &str) -> HistoryItem {
        HistoryItem {
            id: id.into(),
            status,
            duration_ms: 4_500,
            model: String::new(),
            language: String::new(),
            text: Spoken(text.into()),
        }
    }

    /// The window on the History page with the page bound to a silent speaker; what it asked
    /// the daemon for on opening is drained.
    fn page(name: &str) -> (AppWindow, Ipc, Receiver<Cmd>, Ctx) {
        page_with(name, None)
    }

    /// [`page`] on a config file with `config` in it.
    fn page_with(name: &str, config: Option<&str>) -> (AppWindow, Ipc, Receiver<Cmd>, Ctx) {
        let (window, ipc, sent, layout) = linked("history", name, Page::History);
        if let Some(config) = config {
            fs::write(layout.config_file(), config).unwrap();
        }
        let (mut ctx, _) = Ctx::load(layout).unwrap();
        ctx.ipc = ipc.clone();
        bind_with(&window, &ctx, silent);
        // A query builds the page, whose `init` asks for the list.
        has(&window, "History");
        while sent.try_recv().is_ok() {}
        (window, ipc, sent, ctx)
    }

    fn sent_now(sent: &Receiver<Cmd>) -> Vec<Request> {
        sent.try_iter()
            .filter_map(|c| match c {
                Cmd::Send(r) => Some(r),
                _ => None,
            })
            .collect()
    }

    fn rows(window: &AppWindow) -> Vec<HistoryRow> {
        window.global::<HistoryState>().get_items().iter().collect()
    }

    fn listed(ipc: &Ipc, items: Vec<HistoryItem>, more: bool) {
        ipc.deliver(&Event::Push(Push::History { items, more }));
    }

    #[test]
    fn a_new_dictation_joins_an_open_list_and_a_removed_one_leaves_it() {
        let (window, ipc, _sent, _ctx) = page("live");
        let s = window.global::<HistoryState>();
        s.invoke_shown();
        listed(
            &ipc,
            vec![
                item("2026-11-08/120000-000", Status::Done, "old"),
                item("2026-11-08/110000-000", Status::Done, "older"),
            ],
            false,
        );
        s.invoke_select(1);
        // The daemon saved a recording: one more row, on top; the pick stays on its entry.
        let new = item("2026-11-08/134501-123", Status::Done, "fresh");
        ipc.deliver(&Event::Push(Push::HistoryAdded { item: new.clone() }));
        let ids =
            |w: &AppWindow| -> Vec<String> { rows(w).iter().map(|r| r.id.to_string()).collect() };
        assert_eq!(
            ids(&window),
            [
                "2026-11-08/134501-123",
                "2026-11-08/120000-000",
                "2026-11-08/110000-000"
            ]
        );
        assert_eq!(s.get_selected(), 2, "the pick stays on its entry");
        // The same entry again is not a second row.
        ipc.deliver(&Event::Push(Push::HistoryAdded { item: new }));
        assert_eq!(ids(&window).len(), 3);
        // The limits removed the picked one: its row goes, the list stays.
        ipc.deliver(&Event::Push(Push::HistoryRemoved {
            ids: vec!["2026-11-08/110000-000".into()],
        }));
        assert_eq!(
            ids(&window),
            ["2026-11-08/134501-123", "2026-11-08/120000-000"]
        );
        assert!(s.get_selected() < 2);
    }

    #[test]
    fn lists_every_page_newest_first_and_marks_unfinished_ones() {
        let (window, ipc, sent, _ctx) = page("list");
        let s = window.global::<HistoryState>();
        s.invoke_shown();
        assert_eq!(sent_now(&sent), [Request::HistoryList { older_than: None }]);
        assert!(s.get_loading(), "no empty state while the list is out");
        listed(
            &ipc,
            vec![
                item("2026-11-08/134501-123", Status::Done, "hello\nworld"),
                item("2026-11-08/120000-000", Status::Interrupted, ""),
            ],
            true,
        );
        assert_eq!(
            sent_now(&sent),
            [Request::HistoryList {
                older_than: Some("2026-11-08/120000-000".into())
            }],
            "asks for the next page"
        );
        listed(
            &ipc,
            vec![item("2026-11-07/090000-000", Status::Cancelled, "")],
            false,
        );
        assert_eq!(sent_now(&sent), [], "the last page asks nothing");
        let rows = rows(&window);
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "2026-11-08/134501-123",
                "2026-11-08/120000-000",
                "2026-11-07/090000-000"
            ]
        );
        assert_eq!(rows[0].when, "2026-11-08 13:45:01");
        assert_eq!(rows[0].duration, "0:04");
        assert_eq!(rows[0].first, "hello");
        assert_eq!(rows[0].status, "");
        assert_eq!(rows[1].status, "Interrupted");
        assert_eq!(rows[2].status, "Cancelled");
        assert!(rows[0].path.ends_with("134501-123.wav"));
        assert!(!s.get_loading());

        // Opening again lists afresh: an entry the limits removed goes.
        s.invoke_shown();
        listed(
            &ipc,
            vec![item("2026-11-08/134501-123", Status::Done, "hello")],
            false,
        );
        assert_eq!(rows_len(&window), 1);
    }

    /// Review c04 #1: a recording saved after the daemon read the folder for a fresh list, but
    /// announced before that list arrived, stays in the list.
    #[test]
    fn a_recording_announced_before_a_fresh_list_arrives_stays() {
        let (window, ipc, _sent, _ctx) = page("race");
        let s = window.global::<HistoryState>();
        s.invoke_shown();
        let new = item("2026-11-08/134501-123", Status::Done, "fresh");
        ipc.deliver(&Event::Push(Push::HistoryAdded { item: new }));
        listed(
            &ipc,
            vec![item("2026-11-08/120000-000", Status::Done, "old")],
            false,
        );
        let ids: Vec<String> = rows(&window).iter().map(|r| r.id.to_string()).collect();
        assert_eq!(ids, ["2026-11-08/134501-123", "2026-11-08/120000-000"]);
        // The next opening starts clean: only what its own list holds.
        s.invoke_shown();
        listed(
            &ipc,
            vec![item("2026-11-08/120000-000", Status::Done, "old")],
            false,
        );
        assert_eq!(rows_len(&window), 1);
    }

    fn rows_len(window: &AppWindow) -> usize {
        window.global::<HistoryState>().get_items().row_count()
    }

    #[test]
    fn empty_states_say_what_is_missing() {
        let (window, ipc, sent, ctx) = page("empty");
        let s = window.global::<HistoryState>();
        s.invoke_shown();
        listed(&ipc, vec![], false);
        assert!(has(&window, "No recordings yet"));

        ctx.config.borrow_mut().config.history.enabled = false;
        s.invoke_shown();
        listed(&ipc, vec![], false);
        assert!(has(&window, "Saving recordings is off"));
        sent_now(&sent);
        crate::testing::with_role(
            &window,
            "Turn on",
            i_slint_backend_testing::AccessibleRole::Button,
        )
        .invoke_accessible_default_action();
        assert!(ctx.config.borrow().config.history.enabled);
        let saved = fs::read_to_string(ctx.layout.config_file()).unwrap();
        assert!(!saved.contains("enabled = false"), "{saved}");
        assert!(
            sent_now(&sent).contains(&Request::HistoryList { older_than: None }),
            "turning it on lists again"
        );

        ipc.deliver(&Event::Link(Link::Offline));
        assert_eq!(s.get_link_off(), NEEDS_DAEMON);
        assert_eq!(s.get_redo_off(), REDO_NEEDS_DAEMON);
    }

    #[test]
    fn turn_on_does_nothing_on_a_locked_config() {
        let config = "version = 99
[history]
enabled = false
";
        let (window, ipc, sent, ctx) = page_with("locked", Some(config));
        // The shell locks the buttons for a read-only file; the test window opened before it was written.
        window.global::<crate::generated::Shell>().set_locked(true);
        let s = window.global::<HistoryState>();
        s.invoke_shown();
        listed(&ipc, vec![], false);
        assert!(ctx.config.borrow().read_only);
        assert!(has(&window, "Saving recordings is off"));
        sent_now(&sent);
        crate::testing::with_role(
            &window,
            "Turn on",
            i_slint_backend_testing::AccessibleRole::Button,
        )
        .invoke_accessible_default_action();
        assert!(!ctx.config.borrow().config.history.enabled);
        assert_eq!(
            fs::read_to_string(ctx.layout.config_file()).unwrap(),
            config
        );
        assert!(!s.get_history_on());
        assert_eq!(sent_now(&sent), [], "nothing listed again");
        assert_ne!(window.global::<Toast>().get_text(), HISTORY_ON);
        assert!(has(&window, "Saving recordings is off"));
    }

    #[test]
    fn transcribe_again_stores_the_new_text() {
        let (window, ipc, sent, _ctx) = page("redo");
        let s = window.global::<HistoryState>();
        s.invoke_shown();
        listed(
            &ipc,
            vec![
                item("2026-11-08/134501-123", Status::Done, "one"),
                item("2026-11-08/120000-000", Status::Interrupted, ""),
            ],
            false,
        );
        s.invoke_select(1);
        sent_now(&sent);
        s.invoke_redo();
        let run = match sent_now(&sent).as_slice() {
            [Request::HistoryRetranscribe { run, id, .. }] => {
                assert_eq!(id, "2026-11-08/120000-000");
                *run
            }
            other => panic!("{other:?}"),
        };
        assert!(run >= FIRST_RUN, "never one of the Test page's ids");
        assert_eq!(s.get_running_id(), "2026-11-08/120000-000");
        assert_eq!(s.get_redo_off(), RUNNING);
        s.invoke_redo();
        assert_eq!(sent_now(&sent), [], "one run at a time");

        // Nothing known yet: the empty (sweeping) bar. The first push starts it, with its estimate.
        assert!(s.get_progress() <= 0.0);
        ipc.deliver(&Event::Push(Push::TestProgress {
            id: run,
            percent: 0,
            est_ms: Some(5000),
        }));
        ipc.deliver(&Event::Push(Push::TestProgress {
            id: run,
            percent: 40,
            est_ms: None,
        }));
        // The bar shows only what the driver answers, which has not moved before its next frame.
        assert!(s.get_progress() < 0.4, "the page drew the raw percent");
        // A frame later it has moved, ahead of nothing it cannot back up: whisper said 40 %.
        std::thread::sleep(Duration::from_millis(250));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(100));
        assert!(
            (0.02..=0.5).contains(&s.get_progress()),
            "{}",
            s.get_progress()
        );
        // Another page's run is not ours.
        ipc.deliver(&Event::Push(Push::TestResult {
            id: 1,
            outcome: TestOutcome::Heard {
                text: "theirs".into(),
                ms: 1,
            },
        }));
        assert_eq!(sent_now(&sent), []);
        ipc.deliver(&Event::Push(Push::TestResult {
            id: run,
            outcome: TestOutcome::Heard {
                text: "new text".into(),
                ms: 900,
            },
        }));
        match sent_now(&sent).as_slice() {
            [Request::HistorySetText { id, text, .. }] => {
                assert_eq!(id, "2026-11-08/120000-000");
                assert_eq!(text.0, "new text");
            }
            other => panic!("{other:?}"),
        }
        ipc.deliver(&Event::Push(Push::HistoryResult {
            id: "2026-11-08/120000-000".into(),
            op: HistoryOp::SetText,
            ok: true,
        }));
        let rows = rows(&window);
        assert_eq!(rows[1].text, "new text");
        assert_eq!(rows[1].status, "", "a transcribed entry is done");
        assert_eq!(s.get_selected(), 1, "the pick stays");
        assert_eq!(s.get_running_id(), "");
        assert_eq!(s.get_redo_off(), "");
        // The end runs the bar to full and leaves it there: no 100 -> 0 jump.
        std::thread::sleep(Duration::from_millis(200));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(100));
        assert!(
            (s.get_progress() - 1.0).abs() < 1e-6,
            "{}",
            s.get_progress()
        );
    }

    #[test]
    fn a_failed_run_says_why_and_frees_the_button() {
        let (window, ipc, sent, _ctx) = page("redo-failed");
        let s = window.global::<HistoryState>();
        s.invoke_shown();
        listed(
            &ipc,
            vec![item("2026-11-08/120000-000", Status::Failed, "")],
            false,
        );
        s.invoke_redo();
        let Some(Request::HistoryRetranscribe { run, .. }) = sent_now(&sent).pop() else {
            panic!("no run");
        };
        ipc.deliver(&Event::Push(Push::TestResult {
            id: run,
            outcome: TestOutcome::Failed {
                why: TestFailure::ModelMissing,
            },
        }));
        assert_eq!(
            window.global::<Toast>().get_text(),
            "The model is not downloaded."
        );
        assert_eq!(s.get_redo_off(), "");
        assert_eq!(sent_now(&sent), [], "nothing to store");
    }

    #[test]
    fn delete_removes_the_entry_and_picks_the_next() {
        let (window, ipc, sent, _ctx) = page("delete");
        let s = window.global::<HistoryState>();
        s.invoke_shown();
        listed(
            &ipc,
            vec![
                item("2026-11-08/134501-123", Status::Done, "one"),
                item("2026-11-08/120000-000", Status::Done, "two"),
            ],
            false,
        );
        sent_now(&sent);
        s.invoke_delete();
        assert_eq!(
            sent_now(&sent),
            [Request::HistoryDelete {
                id: "2026-11-08/134501-123".into()
            }]
        );
        ipc.deliver(&Event::Push(Push::HistoryResult {
            id: "2026-11-08/134501-123".into(),
            op: HistoryOp::Delete,
            ok: true,
        }));
        let rows = rows(&window);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "2026-11-08/120000-000");
        assert_eq!(s.get_selected(), 0);
        ipc.deliver(&Event::Push(Push::HistoryResult {
            id: "2026-11-08/120000-000".into(),
            op: HistoryOp::Delete,
            ok: false,
        }));
        assert_eq!(window.global::<Toast>().get_text(), NOT_DELETED);
        assert_eq!(rows_len(&window), 1);
    }

    #[test]
    fn play_reads_the_file_and_draws_it() {
        let (window, ipc, _sent, ctx) = page("play");
        let s = window.global::<HistoryState>();
        let id = "2026-11-08/134501-123";
        let folder = history::folder(
            &ctx.config.borrow().config.history,
            &ctx.layout.history_dir(),
        );
        fs::create_dir_all(folder.join("2026-11-08")).unwrap();
        let samples: Vec<f32> = (0..1600).map(|i| (i as f32 / 10.0).sin() * 0.5).collect();
        oxi_platform::wav::write_wav(&wav_path(&folder, id), &samples).unwrap();
        s.invoke_shown();
        listed(
            &ipc,
            vec![
                item(id, Status::Done, "one"),
                item("2026-11-08/120000-000", Status::Done, "gone"),
            ],
            false,
        );
        assert_eq!(s.get_peaks().row_count(), BARS);
        assert_eq!(s.get_play_off(), "");
        s.invoke_play();
        assert!(s.get_playing());
        s.invoke_play();
        assert!(!s.get_playing(), "a second press stops");

        s.invoke_select(1);
        assert_eq!(s.get_peaks().row_count(), 0);
        assert_eq!(s.get_play_off(), NOT_FOUND);
    }

    #[test]
    fn copy_text_says_so_and_is_off_without_a_text() {
        // The testing backend keeps its clipboard in memory: the real one is never touched.
        let (window, ipc, _sent, _ctx) = page("copy");
        window.global::<HistoryState>().invoke_shown();
        listed(
            &ipc,
            vec![item("2026-11-08/134501-123", Status::Done, "one")],
            false,
        );
        crate::testing::with_role(
            &window,
            "Copy text",
            i_slint_backend_testing::AccessibleRole::Button,
        )
        .invoke_accessible_default_action();
        assert_eq!(window.global::<Toast>().get_text(), "Copied.");
        assert!(!has(&window, "Copy path"), "the path button is gone");
    }

    /// The page on one entry with `text`, picked; what it asked for is drained.
    fn picked_page(name: &str, text: &str) -> (AppWindow, Ipc, Receiver<Cmd>, Ctx) {
        let (window, ipc, sent, ctx) = page(name);
        window.global::<HistoryState>().invoke_shown();
        listed(
            &ipc,
            vec![item("2026-11-08/134501-123", Status::Done, text)],
            false,
        );
        sent_now(&sent);
        (window, ipc, sent, ctx)
    }

    fn reply(ipc: &Ipc, op: HistoryOp, ok: bool) {
        ipc.deliver(&Event::Push(Push::HistoryResult {
            id: "2026-11-08/134501-123".into(),
            op,
            ok,
        }));
    }

    #[test]
    fn always_fix_appends_one_rule_for_the_selected_words() {
        let (window, _ipc, sent, ctx) = picked_page("replace", "say oxi yaper now");
        let s = window.global::<HistoryState>();
        let file = ctx.layout.replace_file();
        fs::write(&file, "teh => the").unwrap();
        assert_eq!(s.invoke_slice(4, 13), "oxi yaper");
        assert!(s.invoke_always_fix(4, 13, "OxiYapper".into()));
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "teh => the\noxi yaper => OxiYapper\n"
        );
        assert_eq!(sent_now(&sent), [], "the record stays as it is");
        assert!(!s.invoke_always_fix(3, 4, "x".into()), "a space is no word");
        assert!(!s.invoke_always_fix(0, 99, "x".into()), "out of range");
        assert!(!s.invoke_always_fix(0, 3, "x\ny".into()));
        assert_eq!(window.global::<Toast>().get_text(), BAD_RULE);
    }

    #[test]
    fn add_to_dictionary_appends_one_line_and_refuses_two() {
        let (window, _ipc, _sent, ctx) = picked_page("dictionary", "Zhang Wei\nsaid hi");
        let s = window.global::<HistoryState>();
        let file = ctx.layout.dictionary_file();
        assert!(s.invoke_add_word(0, 5));
        assert_eq!(fs::read_to_string(&file).unwrap(), "Zhang\n");
        assert!(!s.invoke_add_word(6, 15), "a line break is not one entry");
        assert_eq!(window.global::<Toast>().get_text(), BAD_WORD);
        assert_eq!(fs::read_to_string(&file).unwrap(), "Zhang\n");
    }

    #[test]
    fn fix_here_edits_the_record_shows_the_changes_and_restore_swaps_back() {
        let (window, ipc, sent, _ctx) = picked_page("fix-here", "call Anna at noon");
        let s = window.global::<HistoryState>();
        assert!(!s.get_has_other());
        assert!(s.invoke_fix_here(5, 9, "Hanna".into()));
        assert_eq!(
            sent_now(&sent),
            [Request::HistoryEdit {
                id: "2026-11-08/134501-123".into(),
                text: Spoken("call Hanna at noon".into()),
            }]
        );
        assert_eq!(
            rows(&window)[0].text,
            "call Anna at noon",
            "not before the answer"
        );
        reply(&ipc, HistoryOp::Edit, true);
        assert_eq!(rows(&window)[0].text, "call Hanna at noon");
        assert!(s.get_has_other() && s.get_diff_open() && !s.get_restored());
        assert_eq!(window.global::<Toast>().get_text(), "", "no toast");

        s.invoke_restore();
        assert_eq!(
            sent_now(&sent),
            [Request::HistoryRestore {
                id: "2026-11-08/134501-123".into()
            }]
        );
        reply(&ipc, HistoryOp::Restore, true);
        assert_eq!(rows(&window)[0].text, "call Anna at noon");
        assert!(s.get_restored() && !s.get_diff_open() && s.get_has_other());
        s.invoke_restore();
        assert_eq!(sent_now(&sent).len(), 1);
        reply(&ipc, HistoryOp::Restore, true);
        assert_eq!(rows(&window)[0].text, "call Hanna at noon", "Use new");
        assert!(!s.get_restored());

        // The same words change nothing and ask nothing.
        assert!(s.invoke_fix_here(5, 10, "Hanna".into()));
        assert_eq!(sent_now(&sent), []);
        // A failed edit says so and changes nothing.
        assert!(s.invoke_fix_here(0, 4, "ring".into()));
        reply(&ipc, HistoryOp::Edit, false);
        assert_eq!(window.global::<Toast>().get_text(), NOT_SAVED);
        assert_eq!(rows(&window)[0].text, "call Hanna at noon");
    }

    #[test]
    fn a_new_run_shows_its_diff_at_once_and_offline_edits_say_why() {
        let (window, ipc, sent, _ctx) = picked_page("diff", "one two three");
        let s = window.global::<HistoryState>();
        s.invoke_redo();
        let Some(Request::HistoryRetranscribe { run, .. }) = sent_now(&sent).pop() else {
            panic!("no run");
        };
        ipc.deliver(&Event::Push(Push::TestResult {
            id: run,
            outcome: TestOutcome::Heard {
                text: "one 2 three".into(),
                ms: 5,
            },
        }));
        sent_now(&sent);
        reply(&ipc, HistoryOp::SetText, true);
        assert_eq!(rows(&window)[0].text, "one 2 three", "applied at once");
        assert!(s.get_has_other() && s.get_diff_open());
        assert_eq!(
            window.global::<Toast>().get_text(),
            "",
            "no toast over the text"
        );
        s.invoke_toggle_diff();
        assert!(!s.get_diff_open());

        ipc.deliver(&Event::Link(Link::Offline));
        assert!(!s.invoke_fix_here(0, 3, "1".into()));
        assert_eq!(window.global::<Toast>().get_text(), EDIT_NEEDS_DAEMON);
    }

    #[test]
    fn the_other_version_is_read_from_the_entry_json_when_it_is_picked() {
        let (window, ipc, _sent, ctx) = page("kept");
        let s = window.global::<HistoryState>();
        let id = "2026-11-08/134501-123";
        let folder = history::folder(
            &ctx.config.borrow().config.history,
            &ctx.layout.history_dir(),
        );
        fs::create_dir_all(folder.join("2026-11-08")).unwrap();
        let meta = Meta {
            version: Meta::VERSION,
            started: String::new(),
            duration_ms: 1,
            status: Status::Done,
            model: String::new(),
            language: String::new(),
            settings: None,
            previous: Some(history::Previous {
                text: "earlier".into(),
                model: String::new(),
                language: String::new(),
                settings: None,
                newer: true,
            }),
        };
        fs::write(entry_path(&folder, id, "json"), meta.to_json().unwrap()).unwrap();
        s.invoke_shown();
        listed(
            &ipc,
            vec![
                item(id, Status::Done, "now"),
                item("2026-11-08/120000-000", Status::Done, "x"),
            ],
            false,
        );
        assert!(s.get_has_other(), "kept in the record");
        assert!(s.get_restored(), "the record says the old text is shown");
        assert!(!s.get_diff_open(), "the changes open on a click");
        s.invoke_select(1);
        assert!(!s.get_has_other());
        assert!(!s.get_restored());
        s.invoke_select(0);
        assert!(
            s.get_restored(),
            "still the old text after picking it again"
        );
    }

    #[test]
    fn the_changes_are_markdown_with_every_mark_escaped() {
        let marked = |old, new| diff_markup(old, new, "#111111", "#222222");
        assert_eq!(
            marked("a *b* c", "a *B* c"),
            "a <font color=\"#111111\">~~\\*b\\*~~</font> <font color=\"#222222\"><u>\\*B\\*</u></font> c"
        );
        assert_eq!(marked("x\ny", "x\ny"), "x\\\ny");
        assert!(
            StyledText::from_markdown(&marked("1. a_b [c](d) <e> ~f~", "1. a_b [c](d) <e> ~g~"))
                .is_ok()
        );
        assert!(StyledText::from_markdown(&marked("one two", "one three")).is_ok());
        assert_eq!(hex(slint::Color::from_rgb_u8(15, 123, 15)), "#0F7B0F");
    }

    #[test]
    fn paths_and_lines() {
        let p = wav_path(Path::new("h"), "2026-11-08/134501-123");
        assert_eq!(p, Path::new("h").join("2026-11-08").join("134501-123.wav"));
        assert_eq!(rule_line(" a ", ""), Some("a => ".into()));
    }

    /// Draws the page empty, off, with a few recordings (one interrupted, picked) and with 500, at
    /// 720x520, 1280x860 and 1920x1080 in both themes; each frame non-empty and not one colour.
    /// With `OXI_SHOTS_DIR` set they land in `settings-history-<state>-<w>-<theme>.png`. The
    /// renderer platform is per thread and the other tests here use the testing backend: a thread
    /// of its own.
    #[test]
    fn shots_history() {
        std::thread::spawn(draw_states).join().unwrap();
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one renderer set-up, four states, read top to bottom"
    )]
    fn draw_states() {
        use std::time::Duration;

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
        let dir = std::env::var_os("OXI_SHOTS_DIR").map(PathBuf::from);
        let few = vec![
            item(
                "2026-11-08/134501-123",
                Status::Done,
                "Send the report to Anna before lunch, and ask her about the tickets.",
            ),
            item("2026-11-08/120000-000", Status::Interrupted, ""),
            item("2026-11-07/090000-000", Status::Cancelled, ""),
            item("2026-11-06/170512-400", Status::Done, "Short one."),
        ];
        let many: Vec<HistoryItem> = (0..500)
            .map(|i| {
                let id = format!(
                    "2026-11-{:02}/{:06}-000",
                    30 - i / 20,
                    235_959 - i % 20 * 100
                );
                item(
                    &id,
                    if i % 37 == 5 {
                        Status::Interrupted
                    } else {
                        Status::Done
                    },
                    &format!("Dictation number {i}"),
                )
            })
            .collect();
        for state in ["empty", "off", "few", "many", "changes", "selected"] {
            let layout = crate::testing::layout("history", &format!("shot-{state}"));
            if state == "changes" {
                // The first entry was transcribed again: its json keeps the earlier text.
                let day = layout.history_dir().join("2026-11-08");
                fs::create_dir_all(&day).unwrap();
                let kept = "Send the report to Hanna before lunch and ask her about the ticket.";
                let json = format!(
                    "{{\"version\":1,\"started\":\"\",\"duration_ms\":4500,\"status\":\"done\",\"model\":\"\",\"language\":\"\",\"settings\":null,\"previous\":{{\"text\":\"{kept}\",\"model\":\"\",\"language\":\"\",\"settings\":null}}}}"
                );
                fs::write(day.join("134501-123.json"), json).unwrap();
            }
            if state == "off" {
                let off = "version = 2
[history]
enabled = false
";
                fs::write(layout.config_file(), off).unwrap();
            }
            if state == "few" {
                // The picked one has audio, so its waveform shows.
                let day = layout.history_dir().join("2026-11-08");
                fs::create_dir_all(&day).unwrap();
                let speech: Vec<f32> = (0..32_000_u16)
                    .map(|i| (f32::from(i) / 7.0).sin() * (f32::from(i) / 2_500.0).sin().abs())
                    .collect();
                oxi_platform::wav::write_wav(&day.join("120000-000.wav"), &speech).unwrap();
            }
            let (ipc, _sent) = Ipc::fake();
            let window = crate::open_with(layout, ipc.clone()).unwrap();
            let surface = LAST.with_borrow(Clone::clone).unwrap();
            surface.set_size(slint::PhysicalSize::new(1280, 860));
            window.show().unwrap();
            window.set_page(Page::History);
            has(&window, "History");
            let items = match state {
                "few" | "changes" | "selected" => few.clone(),
                "many" => many.clone(),
                _ => Vec::new(),
            };
            listed(&ipc, items, false);
            if state == "few" {
                window.global::<HistoryState>().invoke_select(1);
            }
            if state == "changes" {
                window.global::<HistoryState>().invoke_toggle_diff();
            }
            if state == "selected" {
                // Drag across the first words of the text, as a hand would.
                CLOCK.set(CLOCK.get() + Duration::from_millis(300));
                slint::platform::update_timers_and_animations();
                surface.draw_if_needed(|r| {
                    r.render(
                        &mut vec![PremultipliedRgbaColor::default(); 1280 * 860],
                        1280,
                    );
                });
                let (from, to) = (
                    slint::LogicalPosition::new(655.0, 185.0),
                    slint::LogicalPosition::new(780.0, 185.0),
                );
                let button = slint::platform::PointerEventButton::Left;
                let w = window.window();
                w.dispatch_event(slint::platform::WindowEvent::PointerMoved { position: from });
                w.dispatch_event(slint::platform::WindowEvent::PointerPressed {
                    position: from,
                    button,
                });
                w.dispatch_event(slint::platform::WindowEvent::PointerMoved { position: to });
                w.dispatch_event(slint::platform::WindowEvent::PointerReleased {
                    position: to,
                    button,
                });
            }
            for (w, h) in [(720_u32, 520_u32), (1280, 860), (1920, 1080)] {
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
                        pixels.iter().any(|p| *p != first),
                        "{state} {theme} is flat"
                    );
                    if let Some(dir) = &dir {
                        fs::create_dir_all(dir).unwrap();
                        let name = format!("settings-history-{state}-{w}-{theme}.png");
                        pixmap.save_png(dir.join(name)).unwrap();
                    }
                }
            }
        }
    }
}
