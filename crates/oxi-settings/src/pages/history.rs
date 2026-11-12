//! History page: the saved recordings (SPEC History, R1.3, R3.2, R3.3). The daemon lists them
//! (`HistoryList`, page after page, newest first), transcribes one again (`HistoryRetranscribe`;
//! `HistorySetText` then stores the text) and deletes one (`HistoryDelete`). The audio is read
//! from the folder here, to play and draw (docs/HISTORY-FORMAT.md).

use std::cell::{Cell, RefCell};
use std::fs;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak as RcWeak};
use std::sync::Arc;
use std::time::Duration;

use oxi_core::audio::{SAMPLE_RATE, voiced_runs};
use oxi_core::dictionary::{Suggestion, suggest};
use oxi_core::history::{
    self, DiffKind, Filter, Meta, Searched, Status, Word, align, export, word_diff,
};
use oxi_core::ipc::{
    FileFailure, FileResult, HistoryItem, HistoryOp, Push, Request, Spoken, TestFailure,
    TestOutcome, Words, WordsResult,
};
use oxi_core::stats::Date;
use oxi_core::text::{HeldWhy, decode_text_file};
use oxi_core::tuning::SettingsSet;
use oxi_core::types::{Language, Millis};
use oxi_platform::capture::{CaptureError, Playback};
use slint::{ComponentHandle, ModelRc, StyledText, Timer, TimerMode, VecModel, Weak};

use crate::generated::{HistoryRow, HistoryState, SystemState, Theme, Toast, WordSpan};
use crate::ipc::{Event, Link, Subscription};
use crate::{AppWindow, Ctx, Error, Page, report};

use super::progress::Bars;
use super::tuning::{BARS, clock, peaks, when};

/// The samples of a recording and their rate.
type Clip = (Arc<[f32]>, u32);

/// What to play of a recording: the part `from..to` (`to` `None`: to its end), `speed` times as
/// fast, again from `from` while `looping`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Plan {
    pub from: Duration,
    pub to: Option<Duration>,
    pub speed: f32,
    pub looping: bool,
}

/// A sound being played: where it is in the recording's own time, and whether it is over.
pub trait Voice {
    fn position(&self) -> Duration;
    fn finished(&self) -> bool;
}

impl Voice for Playback {
    fn position(&self) -> Duration {
        Playback::position(self)
    }

    fn finished(&self) -> bool {
        Playback::finished(self)
    }
}

/// Plays mono samples at a rate by a plan, jumping over the skips (clip time); dropping what it
/// gives stops the sound. Tests swap in a silent one.
pub type Player =
    fn(&Arc<[f32]>, u32, &Plan, &[Range<Duration>]) -> Result<Box<dyn Voice>, CaptureError>;

/// The page's two bars: Transcribe again, and the word times being made.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bar {
    Run,
    Times,
    /// A file being transcribed.
    File,
}

/// The speeds of the player, in the order of its control.
const SPEEDS: [f32; 5] = [0.75, 1.0, 1.25, 1.5, 2.0];
/// The speed the control starts at: 1x.
const NORMAL_SPEED: usize = 1;
/// A click on a word starts the sound this long before it: the times are not exact, and a word
/// that starts mid-syllable sounds wrong.
const LEAD_IN: Duration = Duration::from_millis(150);
/// A played selection ends this long after its last word, for the same reason.
const LEAD_OUT: Duration = Duration::from_millis(150);
/// The highlight stays on a word this long after it ended, over a pause; after that, no word.
const HOLD: u64 = 700;
/// How often the highlight follows the sound; only while one plays.
const TICK: Duration = Duration::from_millis(50);
/// Run ids of this page, far from the Test page's (counting up from 1 on the same link): each page
/// takes only the answers to its own runs.
const FIRST_RUN: u32 = 1 << 31;

/// A search rests this long after the last key before it is sent, so typing makes one request.
const SEARCH_REST: Duration = Duration::from_millis(300);

const FILE_NEEDS_DAEMON: &str = "Start OxiYapper to transcribe a file";
const FILE_BUSY: &str = "OxiYapper is busy. Try the file again in a moment.";
const FILE_NOT_SAVED: &str = "The text of the file could not be saved.";
const NO_TIMES: &str = "This recording has no word times yet";
const TIMING: &str = "Word times are being made";
const EXPORT_NOT_SAVED: &str = "The file could not be saved.";
const EXPORT_NO_TIMES: &str = "This recording has no word times, so no subtitles.";
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
    #[cfg(test)]
    if let Some(player) = TEST_PLAYER.get() {
        return bind_with(window, ctx, player);
    }
    bind_with(window, ctx, play_cpal);
}

#[cfg(test)]
thread_local! {
    /// The speaker the window of a test gets from [`bind`] instead of the sound card.
    static TEST_PLAYER: Cell<Option<Player>> = const { Cell::new(None) };
}

fn play_cpal(
    clip: &Arc<[f32]>,
    rate: u32,
    plan: &Plan,
    skips: &[Range<Duration>],
) -> Result<Box<dyn Voice>, CaptureError> {
    Ok(Box::new(Playback::start(
        Arc::clone(clip),
        rate,
        plan.from,
        plan.to,
        plan.speed,
        plan.looping,
        skips,
    )?))
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
        audio: RefCell::new(None),
        skips: RefCell::new(Vec::new()),
        playing: RefCell::new(None),
        plan: Cell::new(None),
        tick: Timer::default(),
        speed: Cell::new(NORMAL_SPEED),
        looping: Cell::new(false),
        heard: RefCell::new(Vec::new()),
        marks: RefCell::new(Marks::default()),
        current: Cell::new(None),
        words_run: RefCell::new(None),
        words_tried: Cell::new(false),
        run: RefCell::new(None),
        bar: Bars::new({
            let window = window.as_weak();
            move |bar, shown| {
                if let Some(window) = window.upgrade() {
                    let s = window.global::<HistoryState>();
                    match bar {
                        Bar::Run => s.set_progress(shown),
                        Bar::Times => s.set_timing_progress(shown),
                        Bar::File => s.set_file_progress(shown),
                    }
                }
            }
        }),
        next_run: Cell::new(FIRST_RUN),
        other: RefCell::new(None),
        restored: Cell::new(false),
        diff_open: Cell::new(false),
        edit: RefCell::new(None),
        offer: RefCell::new(None),
        offered: RefCell::new(Vec::new()),
        search: RefCell::new(Search::default()),
        search_rest: Timer::default(),
        file: Cell::new(None),
    });
    s.set_items(ModelRc::from(page.rows.clone()));
    s.set_selected(0);
    s.set_peaks(ModelRc::default());
    s.set_progress(0.0);
    s.set_speed(i32::try_from(NORMAL_SPEED).unwrap_or(1));
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
    bind_offers(&s, &page);
    bind_player(&s, &page);
    bind_search(&s, &page);
}

/// The player: a click on a word, Listen, speed, loop and skip pauses.
fn bind_player(s: &HistoryState<'_>, page: &Rc<PageState>) {
    let p = Rc::clone(page);
    s.on_word_click(move |at| p.word_click(at));
    let p = Rc::clone(page);
    s.on_listen(move |a, b| p.listen(a, b));
    let p = Rc::clone(page);
    s.on_speed_picked(move |i| p.set_speed(i));
    let p = Rc::clone(page);
    s.on_toggle_loop(move || p.toggle_loop());
    let p = Rc::clone(page);
    s.on_toggle_skip(move || p.toggle_skip());
}

/// Search, filters, export and file transcription.
fn bind_search(s: &HistoryState<'_>, page: &Rc<PageState>) {
    let p = Rc::clone(page);
    s.on_search_edited(move |text| p.query_edited(&text));
    let p = Rc::clone(page);
    s.on_length_picked(move |i| p.length_picked(i));
    let p = Rc::clone(page);
    s.on_clear_day(move || p.set_day(None));
    let p = Rc::clone(page);
    s.on_export(move |i| p.export(i));
    let p = Rc::clone(page);
    s.on_export_opened(move || p.need_words());
    let p = Rc::clone(page);
    s.on_pick_file(move || {
        if let Some(path) = pick_audio() {
            p.transcribe_file(&path);
        }
    });
    let p = Rc::clone(page);
    s.on_stop_file(move || p.stop_file());
    let p = Rc::clone(page);
    s.on_open_day(move |day| p.open_day(day.parse().ok()));
}

/// A Transcribe again that is out.
struct Run {
    id: u32,
    entry: String,
    set: SettingsSet,
    language: Language,
    /// The new text, once heard; then `HistorySetText` is out.
    text: Option<String>,
    /// The words of the run, which the daemon sends before its text.
    words: Words,
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
    /// The samples of that audio and their rate, kept while the entry is picked: a click on a word
    /// plays from them without reading the file again.
    audio: RefCell<Option<Rc<Clip>>>,
    /// The pauses of that audio the player jumps over while `[history] skip_pauses` is on; empty
    /// while it is off (made only then, once per pick).
    skips: RefCell<Vec<Range<Duration>>>,
    playing: RefCell<Option<Box<dyn Voice>>>,
    /// The plan of the sound in hand, to play it again at another speed.
    plan: Cell<Option<Plan>>,
    /// Follows the sound for the highlight; runs only while one plays.
    tick: Timer,
    /// An index of [`SPEEDS`].
    speed: Cell<usize>,
    looping: Cell<bool>,
    /// The words of the picked entry's record (times once a pass made them).
    heard: RefCell<Vec<Word>>,
    /// Those words lined up with the shown text.
    marks: RefCell<Marks>,
    /// The word the sound is on.
    current: Cell<Option<usize>>,
    /// The word-times run that is out: its id and the entry it is for.
    words_run: RefCell<Option<(u32, String)>>,
    /// The picked entry's word times were asked for (once per pick, so a Busy makes no loop).
    words_tried: Cell<bool>,
    run: RefCell<Option<Run>>,
    /// The bars of the run and of the word times: the one progress driver, not a percent of
    /// their own.
    bar: Rc<Bars<Bar>>,
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
    /// The "Always fix" offer an edit made: the entry and the pair (arch-plan 9.1, idea 7).
    offer: RefCell<Option<(String, Suggestion)>>,
    /// Pairs offered once already: never offered again in this window.
    offered: RefCell<Vec<Suggestion>>,
    /// What the list is narrowed to.
    search: RefCell<Search>,
    /// Waits for the typing to rest before the search is sent.
    search_rest: Timer,
    /// The run id of the file being transcribed.
    file: Cell<Option<u32>>,
}

/// The query and filters of the list. A list with none of them is the whole history.
#[derive(Default)]
struct Search {
    query: String,
    day: Option<Date>,
    /// 0 any, 1 short (to 10 s), 2 medium (to 1 min), 3 long.
    length: usize,
    /// Counts the searches sent: an answer to an older one is stale.
    seq: u32,
}

impl Search {
    fn active(&self) -> bool {
        !self.query.trim().is_empty() || self.day.is_some() || self.length != 0
    }

    /// The inclusive length bounds in ms of the length filter.
    fn bounds(&self) -> (Option<u64>, Option<u64>) {
        match self.length {
            1 => (None, Some(10_000)),
            2 => (Some(10_001), Some(60_000)),
            3 => (Some(60_001), None),
            _ => (None, None),
        }
    }

    fn filter(&self) -> Filter {
        let (min_ms, max_ms) = self.bounds();
        Filter {
            day: self.day,
            min_ms,
            max_ms,
        }
    }

    fn request(&self, older_than: Option<String>) -> Request {
        let (min_ms, max_ms) = self.bounds();
        Request::HistorySearch {
            seq: self.seq,
            query: Spoken(self.query.clone()),
            day: self.day.map(|d| d.to_string()),
            min_ms,
            max_ms,
            older_than,
        }
    }

    /// Would the daemon list `item` for this search?
    fn accepts(&self, item: &HistoryItem) -> bool {
        let Some(day) = item.id.get(..10).and_then(history::day_dir) else {
            return false;
        };
        let searched = Searched {
            text_lower: &history::fold(&item.text.0),
            day,
            duration_ms: item.duration_ms,
        };
        history::matches(&self.query, &self.filter(), &searched)
    }
}

/// A file a record can be exported to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Format {
    Txt,
    Json,
    Srt,
    Vtt,
}

impl Format {
    /// The index of the Export menu's lines.
    fn from_index(i: i32) -> Option<Self> {
        [Self::Txt, Self::Json, Self::Srt, Self::Vtt]
            .get(usize::try_from(i).ok()?)
            .copied()
    }

    const fn ext(self) -> &'static str {
        match self {
            Self::Txt => "txt",
            Self::Json => "json",
            Self::Srt => "srt",
            Self::Vtt => "vtt",
        }
    }
}

/// The contents of `text` exported as `format`. `meta` is the record's json (for the JSON file),
/// `heard` its words with times (for the subtitles; the shown words are lined up with them, so an
/// edited text exports as edited). `None`: the subtitles have no times to use, or the json no meta.
fn exported(format: Format, text: &str, meta: Option<&Meta>, heard: &[Word]) -> Option<String> {
    let timed = || {
        let shown: Vec<&str> = text.split_whitespace().collect();
        let ms = |t: Millis| u32::try_from(t.0).ok();
        let words: Vec<Word> = align(&shown, heard)
            .into_iter()
            .zip(shown)
            .map(|(t, w)| Word {
                w: w.into(),
                p: 1.0,
                t0: t.and_then(|t| ms(t.0)),
                t1: t.and_then(|t| ms(t.1)),
            })
            .collect();
        words.iter().any(|w| w.t0.is_some()).then_some(words)
    };
    match format {
        Format::Txt => Some(export::txt(text)),
        Format::Json => export::json(meta?, text).ok(),
        Format::Srt => timed().map(|w| export::srt(&w)),
        Format::Vtt => timed().map(|w| export::vtt(&w)),
    }
}

/// Why a file run ended without a record.
fn file_failed(why: &FileFailure) -> String {
    match why {
        FileFailure::Unsupported { what } => {
            format!("{what} files cannot be read. Use WAV, MP3, M4A, MP4, FLAC or OGG (Vorbis).")
        }
        FileFailure::Corrupt => "The file is damaged or has no audio.".into(),
        FileFailure::TooLong => "The audio is longer than 2 hours.".into(),
        FileFailure::Unreadable => "The file could not be opened.".into(),
        FileFailure::HistoryOff => {
            "Turn on saving recordings first: the text is kept there.".into()
        }
        FileFailure::Busy => FILE_BUSY.into(),
        FileFailure::NoModel => "The model is not downloaded.".into(),
        FileFailure::NothingHeard => NOTHING_HEARD.into(),
        FileFailure::Failed => FILE_NOT_SAVED.into(),
    }
}

#[cfg(test)]
thread_local! {
    /// What the file dialogs of a test answer: the file picked and the file saved to.
    static PICKED: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    static SAVED: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// The audio file dialog; `None` when it is cancelled (or the OS has none).
fn pick_audio() -> Option<PathBuf> {
    #[cfg(test)]
    return PICKED.take();
    #[cfg(all(windows, not(test)))]
    return rfd::FileDialog::new()
        .add_filter("Audio", &oxi_platform::decode::EXTENSIONS)
        .pick_file();
    #[cfg(all(not(windows), not(test)))]
    None
}

/// The save dialog for an export named `name`.
#[cfg_attr(
    test,
    allow(unused_variables, reason = "the test dialog ignores the name")
)]
fn save_as(name: &str, ext: &str) -> Option<PathBuf> {
    #[cfg(test)]
    return SAVED.take();
    #[cfg(all(windows, not(test)))]
    return rfd::FileDialog::new()
        .set_file_name(name)
        .add_filter(ext.to_uppercase(), &[ext])
        .save_file();
    #[cfg(all(not(windows), not(test)))]
    None
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
        s.set_skip_on(self.ctx.config.borrow().config.history.skip_pauses);
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
        let search = self.search.borrow();
        s.set_search_on(search.active());
        let today = oxi_platform::stats_store::today();
        s.set_day(
            search
                .day
                .map(|d| super::home::day_label(d, today))
                .unwrap_or_default()
                .into(),
        );
        s.set_file_running(self.file.get().is_some());
        s.set_file_off(if self.online() { "" } else { FILE_NEEDS_DAEMON }.into());
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
            let search = self.search.borrow();
            self.ctx.ipc.send(if search.active() {
                search.request(older_than)
            } else {
                Request::HistoryList { older_than }
            });
        }
    }

    /// A key in the search field: the search goes out once the typing rests.
    fn query_edited(&self, text: &str) {
        text.clone_into(&mut self.search.borrow_mut().query);
        let me = self.me.clone();
        self.search_rest
            .start(TimerMode::SingleShot, SEARCH_REST, move || {
                if let Some(page) = me.upgrade() {
                    page.run_search();
                }
            });
    }

    fn length_picked(&self, i: i32) {
        self.search.borrow_mut().length = usize::try_from(i).unwrap_or(0).min(3);
        self.state(|s| s.set_length(i));
        self.run_search();
    }

    /// Home's heatmap day click: that day's list, with no query left from before.
    fn open_day(&self, day: Option<Date>) {
        self.search.borrow_mut().query.clear();
        self.state(|s| s.set_query("".into()));
        self.set_day(day);
    }

    /// The day chip's clear (the query stays), or [`Self::open_day`].
    fn set_day(&self, day: Option<Date>) {
        self.search.borrow_mut().day = day;
        self.run_search();
    }

    /// Lists again from the newest with the search as it stands; an older search's answers are
    /// stale from here on.
    fn run_search(&self) {
        self.search_rest.stop();
        {
            let mut search = self.search.borrow_mut();
            search.seq = search.seq.wrapping_add(1);
        }
        self.fresh.set(true);
        self.added_since.borrow_mut().clear();
        self.list(None);
        self.state(|s| self.refresh(s));
    }

    /// Writes the picked recording to a file of the user's choice.
    fn export(&self, index: i32) {
        let (Some(format), Some(id)) = (Format::from_index(index), self.selected_id()) else {
            return;
        };
        let name = format!(
            "{}.{}",
            id.replace('/', " ").replace(' ', "_"),
            format.ext()
        );
        let Some(path) = save_as(&name, format.ext()) else {
            return;
        };
        let done = self.write_export(&id, format, &path);
        self.toast(match done {
            Ok(()) => "Saved.",
            Err(why) => why,
        });
    }

    fn write_export(&self, id: &str, format: Format, path: &Path) -> Result<(), &'static str> {
        let item = self
            .items
            .borrow()
            .iter()
            .find(|i| i.id == id)
            .cloned()
            .ok_or(EXPORT_NOT_SAVED)?;
        // An entry without a meta file is listed with one the daemon made up: export the same.
        let meta = fs::read_to_string(entry_path(&self.folder(), id, history::JSON))
            .ok()
            .and_then(|json| Meta::from_json(&json).ok())
            .unwrap_or_else(|| {
                Meta::basic(
                    id,
                    item.duration_ms,
                    item.status,
                    &item.model,
                    &item.language,
                )
            });
        let text = item.text.0;
        let contents = exported(format, &text, Some(&meta), &self.heard.borrow()).ok_or(
            if matches!(format, Format::Srt | Format::Vtt) {
                EXPORT_NO_TIMES
            } else {
                EXPORT_NOT_SAVED
            },
        )?;
        fs::write(path, contents).map_err(|_| EXPORT_NOT_SAVED)
    }

    /// Sends the audio file at `path` to the daemon to be transcribed into a new record.
    fn transcribe_file(&self, path: &Path) {
        if !self.online() || self.file.get().is_some() {
            return;
        }
        let run = self.next_run.get();
        self.next_run.set(run.wrapping_add(1).max(FIRST_RUN));
        let language = self.ctx.config.borrow().config.model.language;
        self.ctx.ipc.send(Request::TranscribeFile {
            run,
            path: path.to_string_lossy().into_owned(),
            language,
        });
        self.file.set(Some(run));
        self.bar.clear(&Bar::File);
        self.state(|s| self.refresh(s));
    }

    fn stop_file(&self) {
        if let Some(run) = self.file.get() {
            self.ctx.ipc.send(Request::TranscribeFileCancel { run });
        }
    }

    /// The daemon ended the file run.
    fn file_done(&self, run: u32, result: &FileResult) {
        if self.file.get() != Some(run) {
            return;
        }
        self.file.set(None);
        match result {
            FileResult::Done { .. } => self.bar.finish(&Bar::File),
            FileResult::Cancelled => self.bar.clear(&Bar::File),
            FileResult::Failed { why } => {
                self.bar.clear(&Bar::File);
                self.toast(&file_failed(why));
            }
        }
        self.state(|s| self.refresh(s));
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
        if self.search.borrow().active() && !self.search.borrow().accepts(item) {
            return;
        }
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
            let bars = audio
                .as_ref()
                .map_or_else(Vec::new, |(samples, _)| peaks(samples, BARS));
            self.state(|s| s.set_peaks(ModelRc::from(Rc::new(VecModel::from(bars)))));
            *self.audio.borrow_mut() = audio.map(|(samples, rate)| Rc::new((samples.into(), rate)));
            self.find_pauses();
            self.shown_id.borrow_mut().clone_from(&id);
            self.forget_words_run();
        }
        self.remark();
        self.state(|s| {
            self.refresh(s);
            self.show_versions(s);
            self.show_words(s);
        });
    }

    /// A new pick: the run for the last one stops. Nothing is asked for until [`Self::need_words`],
    /// so resting on a record (or scrolling the list) loads no model.
    fn forget_words_run(&self) {
        self.words_tried.set(false);
        // The run for the last pick is no longer wanted: it stops and frees the daemon's slot.
        if let Some((run, _)) = self.words_run.borrow_mut().take()
            && self.online()
        {
            self.ctx.ipc.send(Request::HistoryWordsCancel { run });
        }
        self.bar.clear(&Bar::Times);
        self.state(|s| s.set_timing(false));
    }

    /// The word times of the picked entry, asked for at the first thing that needs them (Play, a
    /// word click, Listen, the Export menu: its SRT and VTT stay off until they come). Nothing when the entry has them, has no audio, was
    /// asked already or the daemon is away; the answer is [`Self::words_came`].
    fn need_words(&self) {
        if self.words_tried.get() || !self.audible.get() || !self.online() {
            return;
        }
        if self.heard.borrow().iter().any(|w| w.t0.is_some()) {
            return;
        }
        let Some(id) = self.selected_id() else {
            return;
        };
        self.words_tried.set(true);
        self.send_words_ask(&id);
    }

    fn send_words_ask(&self, id: &str) {
        if self.selected_id().as_deref() != Some(id) || !self.online() {
            return;
        }
        let run = self.next_run.get();
        self.next_run.set(run.wrapping_add(1).max(FIRST_RUN));
        *self.words_run.borrow_mut() = Some((run, id.to_owned()));
        self.ctx.ipc.send(Request::HistoryWords {
            run,
            id: id.to_owned(),
        });
        self.bar.clear(&Bar::Times);
        // The SRT/VTT tooltip says the times are on their way.
        self.state(|s| {
            s.set_timing(true);
            self.show_words(s);
        });
    }

    /// The answer to a word-times ask. Words for the picked entry replace the held ones; an answer
    /// to a run the pick left (it was cancelled) is dropped. A Busy or a failure for the pick
    /// itself is not asked again until it is picked again, so a dictation under way makes no loop
    /// of asks; the features that need times stay hidden and the text stays usable.
    fn words_came(&self, run: u32, id: &str, result: &WordsResult) {
        let mine = self
            .words_run
            .borrow()
            .as_ref()
            .is_some_and(|(r, i)| *r == run && i == id);
        if !mine {
            return;
        }
        *self.words_run.borrow_mut() = None;
        self.bar.clear(&Bar::Times);
        if let WordsResult::Ready { words, .. } = result {
            self.heard.borrow_mut().clone_from(&words.0);
            self.remark();
        }
        self.state(|s| {
            s.set_timing(false);
            self.show_words(s);
        });
    }

    /// The picked entry's words lined up with its shown text again (the text or the words changed).
    fn remark(&self) {
        let text = self.shown_text().unwrap_or_default();
        *self.marks.borrow_mut() = Marks::new(&text, &self.heard.borrow());
    }

    /// What the text area and the player strip say of the words.
    fn show_words(&self, s: &HistoryState<'_>) {
        let marks = self.marks.borrow();
        s.set_timed(marks.timed());
        s.set_times_off(
            if marks.timed() {
                ""
            } else if s.get_timing() {
                TIMING
            } else {
                NO_TIMES
            }
            .into(),
        );
        let styled = self.current.get().is_some() || marks.doubtful.iter().any(|d| *d);
        s.set_marks_on(styled);
        if styled {
            let accent = self
                .window
                .upgrade()
                .map(|w| hex(w.global::<Theme>().get_accent_text()))
                .unwrap_or_default();
            let text = self.shown_text().unwrap_or_default();
            let markup = marks.markup(&text, self.current.get(), &accent);
            s.set_marked(
                StyledText::from_markdown(&markup)
                    .unwrap_or_else(|_| StyledText::from_plain_text(&text)),
            );
        }
        s.set_speed(i32::try_from(self.speed.get()).unwrap_or(1));
        s.set_loop_on(self.looping.get());
        let word = self.current.get().and_then(|i| marks.spans.get(i));
        s.set_current_word(word.map_or(WordSpan { a: 0, b: 0 }, |r| WordSpan {
            a: i32::try_from(r.start).unwrap_or(0),
            b: i32::try_from(r.end).unwrap_or(0),
        }));
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

    /// The pauses of the picked audio, while `[history] skip_pauses` is on (arch-plan 9.13).
    fn find_pauses(&self) {
        let on = self.ctx.config.borrow().config.history.skip_pauses;
        let sensitivity = self.ctx.config.borrow().config.audio.vad_sensitivity;
        let audio = self.audio.borrow().clone();
        *self.skips.borrow_mut() = match audio {
            Some(audio) if on => pauses(&audio.0, audio.1, sensitivity),
            _ => Vec::new(),
        };
    }

    /// Play, or Stop while it plays.
    fn play(&self) {
        if self.playing.borrow().is_some() {
            self.stop_playing();
            self.state(|s| self.refresh(s));
        } else {
            self.need_words();
            self.play_plan(Plan {
                from: Duration::ZERO,
                to: None,
                speed: SPEEDS[self.speed.get()],
                looping: self.looping.get(),
            });
        }
    }

    /// Starts the sound of the picked entry by `plan`, instead of the one in hand.
    fn play_plan(&self, plan: Plan) {
        self.stop_playing();
        let audio = self.audio.borrow().clone();
        if let Some(audio) = audio {
            let (samples, rate) = &*audio;
            match (self.player)(samples, *rate, &plan, &self.skips.borrow()) {
                Ok(sound) => {
                    *self.playing.borrow_mut() = Some(sound);
                    self.plan.set(Some(plan));
                    self.follow();
                }
                Err(e) => self.toast(&e.to_string()),
            }
        } else {
            self.audible.set(false);
            self.toast(NOT_FOUND);
        }
        self.state(|s| self.refresh(s));
    }

    /// Moves the highlight with the sound, while one plays, and ends the play when it is over.
    fn follow(&self) {
        let me = self.me.clone();
        self.tick.start(TimerMode::Repeated, TICK, move || {
            if let Some(page) = me.upgrade() {
                page.moved();
            }
        });
        self.moved();
    }

    fn moved(&self) {
        let at = self
            .playing
            .borrow()
            .as_ref()
            .map(|v| (v.finished(), v.position()));
        match at {
            None | Some((true, _)) => {
                self.stop_playing();
                self.state(|s| self.refresh(s));
            }
            Some((false, position)) => {
                let now = self.marks.borrow().at_time(position);
                if now != self.current.replace(now) {
                    self.state(|s| self.show_words(s));
                }
            }
        }
    }

    fn stop_playing(&self) {
        self.tick.stop();
        self.plan.set(None);
        self.playing.borrow_mut().take();
        if self.current.take().is_some() {
            self.state(|s| self.show_words(s));
        }
    }

    /// The speed control picked entry `i` of [`SPEEDS`]; a sound in hand starts again at it.
    fn set_speed(&self, i: i32) {
        let Some(at) = usize::try_from(i).ok().filter(|i| *i < SPEEDS.len()) else {
            return;
        };
        self.speed.set(at);
        self.replay();
    }

    fn toggle_loop(&self) {
        self.looping.set(!self.looping.get());
        self.replay();
    }

    /// Skip pauses: `[history] skip_pauses` flips and is saved; a sound in hand starts again with
    /// it.
    fn toggle_skip(&self) {
        // A disabled button still runs its accessible default action.
        if self.ctx.config.borrow().read_only {
            return;
        }
        let on = !self.ctx.config.borrow().config.history.skip_pauses;
        self.ctx.config.borrow_mut().config.history.skip_pauses = on;
        let saved = self.ctx.save();
        report(&self.window, saved);
        self.find_pauses();
        self.state(|s| self.refresh(s));
        self.replay();
    }

    /// A sound in hand starts again from the start of its plan at the speed and loop now set; the
    /// controls show what is set either way.
    fn replay(&self) {
        match self.plan.get() {
            Some(plan) if self.playing.borrow().is_some() => self.play_plan(Plan {
                speed: SPEEDS[self.speed.get()],
                looping: self.looping.get(),
                ..plan
            }),
            _ => self.state(|s| self.show_words(s)),
        }
    }

    /// A click on the text at byte `at`: on a word with a time the sound starts from it; on a
    /// doubtful word the answer is its range, which the text area selects, so the fix bar opens.
    fn word_click(&self, at: i32) -> WordSpan {
        let none = WordSpan { a: 0, b: 0 };
        let Some(at) = usize::try_from(at).ok() else {
            return none;
        };
        let (start, doubtful) = {
            let marks = self.marks.borrow();
            let Some(word) = marks.at_offset(at) else {
                return none;
            };
            (
                marks.times[word].map(|(t0, _)| t0),
                marks.doubtful[word].then(|| marks.spans[word].clone()),
            )
        };
        if start.is_none() {
            self.need_words();
        }
        if let Some(t0) = start {
            self.play_plan(Plan {
                from: Duration::from_millis(t0.0).saturating_sub(LEAD_IN),
                to: None,
                speed: SPEEDS[self.speed.get()],
                looping: self.looping.get(),
            });
        }
        doubtful.map_or(none, |r| WordSpan {
            a: i32::try_from(r.start).unwrap_or(0),
            b: i32::try_from(r.end).unwrap_or(0),
        })
    }

    /// Listen: plays the selected words, over and over when the loop is on.
    fn listen(&self, a: i32, b: i32) {
        let range = usize::try_from(a).ok().zip(usize::try_from(b).ok());
        let times = range.and_then(|(a, b)| self.marks.borrow().times_of(a..b));
        if times.is_none() {
            self.need_words();
        }
        if let Some((t0, t1)) = times {
            self.play_plan(Plan {
                from: Duration::from_millis(t0.0).saturating_sub(LEAD_IN),
                to: Some(Duration::from_millis(t1.0) + LEAD_OUT),
                speed: SPEEDS[self.speed.get()],
                looping: self.looping.get(),
            });
        }
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
            words: Words::default(),
        });
        self.bar.clear(&Bar::Run);
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
                    words: run.words.clone(),
                });
                self.bar.finish(&Bar::Run);
                return;
            }
            TestOutcome::Heard { .. } => NOTHING_HEARD,
            TestOutcome::Failed { why } => failed(*why),
        };
        *slot = None;
        drop(slot);
        self.bar.fail(&Bar::Run);
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
                    // The daemon kept the run's words unless the record had timed ones.
                    self.load_other(Some(id));
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
                    Some((_, text)) if ok => {
                        self.offer_rule(id, &text);
                        self.new_text(id, text, |_| {});
                    }
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

    /// The other version of entry `id` and its words, from its json; also resets what the page
    /// shows of them.
    fn load_other(&self, id: Option<&str>) {
        let folder = self.folder();
        let meta = id.filter(|id| history::is_id(id)).and_then(|id| {
            let json = fs::read_to_string(entry_path(&folder, id, history::JSON)).ok()?;
            Meta::from_json(&json).ok()
        });
        let (kept, words) = meta.map_or((None, Vec::new()), |m| (m.previous, m.words));
        // The record says which version is shown, so a re-pick does not forget a Restore.
        self.restored.set(kept.as_ref().is_some_and(|p| p.newer));
        *self.other.borrow_mut() = kept.map(|p| p.text);
        *self.heard.borrow_mut() = words;
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
        self.state(|s| self.show_words(s));
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
        self.edit_text(id, new)
    }

    /// Add to the text: the held parts of the picked entry go to the end of its text.
    fn add_held(&self) -> bool {
        let Some(window) = self.window.upgrade() else {
            return false;
        };
        let i = usize::try_from(window.global::<HistoryState>().get_selected()).ok();
        let picked = i.and_then(|i| self.items.borrow().get(i).cloned());
        let Some(item) = picked else {
            return false;
        };
        let held = held_text(&item);
        let text = item.text.0.trim();
        if held.is_empty() || text.contains(&held) {
            return false;
        }
        let new = if text.is_empty() {
            held
        } else {
            format!("{text} {held}")
        };
        self.edit_text(item.id, new)
    }

    /// Sends `new` as the edited text of entry `id`; false when the daemon is not there.
    fn edit_text(&self, id: String, new: String) -> bool {
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
            // The System switch, and the pages that follow it (Text: hold back), show it now.
            if let Some(window) = self.window.upgrade() {
                window.global::<SystemState>().set_history_on(true);
            }
            self.toast(HISTORY_ON);
            self.shown();
        }
        report(&self.window, saved);
        self.state(|s| self.refresh(s));
    }

    /// After an edit of entry `id` to `new`: the first pair the edit reads as a misheard word,
    /// and not offered before, becomes the "Always fix" offer. Nothing is added unasked.
    fn offer_rule(&self, id: &str, new: &str) {
        let old = self
            .items
            .borrow()
            .iter()
            .find(|i| i.id == id)
            .map(|i| i.text.0.clone())
            .unwrap_or_default();
        let fresh = suggest(&old, new)
            .into_iter()
            .find(|s| !self.offered.borrow().contains(s));
        let Some(pair) = fresh else {
            return;
        };
        self.offered.borrow_mut().push(pair.clone());
        let tip = format!(
            "\u{201c}{}\u{201d} becomes \u{201c}{}\u{201d} in every later dictation (a replacement rule).",
            pair.heard, pair.wanted
        );
        self.state(|s| {
            s.set_offer_id(id.into());
            s.set_offer_tip(tip.into());
        });
        *self.offer.borrow_mut() = Some((id.to_owned(), pair));
    }

    /// Always fix, from the offer: the pair becomes a replacement rule; the offer goes.
    fn accept_offer(&self) {
        let Some((_, pair)) = self.offer.borrow_mut().take() else {
            return;
        };
        self.state(|s| s.set_offer_id("".into()));
        self.add_replacement(&pair.heard, &pair.wanted);
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
            // A plain list while a search is on is the Test page's, not ours.
            Event::Push(Push::History { items, more }) if !self.search.borrow().active() => {
                self.listed(items, *more);
            }
            Event::Push(Push::HistoryFound { seq, items, more }) => {
                let current = {
                    let search = self.search.borrow();
                    search.active() && search.seq == *seq
                };
                if current {
                    self.listed(items, *more);
                }
            }
            Event::Push(Push::FileResult { run, result }) => self.file_done(*run, result),
            Event::Push(Push::HistoryAdded { item }) => self.added(item),
            Event::Push(Push::HistoryRemoved { ids }) => self.removed(ids),
            Event::Push(Push::TestProgress {
                id,
                percent,
                est_ms,
            }) => {
                let asked = self.words_run.borrow().as_ref().map(|(run, _)| *run);
                let bar = if self.file.get() == Some(*id) {
                    Some(Bar::File)
                } else if asked == Some(*id) {
                    Some(Bar::Times)
                } else if self.run.borrow().as_ref().is_some_and(|r| r.id == *id) {
                    Some(Bar::Run)
                } else {
                    None
                };
                if let Some(bar) = bar {
                    // The first push carries the estimate: the bar starts there, and until it
                    // does the bar is the indeterminate sweep (the model may still be loading).
                    if !self.bar.live(&bar) {
                        self.bar.start(bar, est_ms.map(|ms| Millis(u64::from(ms))));
                    }
                    self.bar.real(&bar, f32::from((*percent).min(100)) / 100.0);
                }
            }
            Event::Push(Push::HistoryRunWords { id, words }) => {
                if let Some(run) = self.run.borrow_mut().as_mut().filter(|r| r.id == *id) {
                    run.words = words.clone();
                }
            }
            Event::Push(Push::HistoryWords { run, id, result }) => {
                self.words_came(*run, id, result);
            }
            Event::Push(Push::TestResult { id, outcome }) => {
                // A word-times run the coordinator turned away (a dictation is under way).
                let asked = self.words_run.borrow().clone();
                if let Some((run, record)) = asked.filter(|(run, _)| run == id) {
                    self.words_came(run, &record, &WordsResult::Busy);
                } else {
                    self.answered(*id, outcome);
                }
            }
            Event::Push(Push::HistoryResult { id, op, ok }) => self.changed(id, *op, *ok),
            Event::Link(link) => {
                if *link == Link::Online {
                    self.fresh.set(true);
                    self.list(None);
                } else {
                    self.loading.set(false);
                    // The ask died with the link: the next Play, click or Listen asks again.
                    self.words_tried.set(false);
                    *self.words_run.borrow_mut() = None;
                    self.bar.clear(&Bar::Times);
                    self.state(|s| {
                        s.set_timing(false);
                        self.show_words(s);
                    });
                    if self.run.borrow_mut().take().is_some() {
                        self.bar.fail(&Bar::Run);
                        self.toast(LINK_LOST);
                    }
                    if self.file.take().is_some() {
                        self.bar.clear(&Bar::File);
                        self.toast(LINK_LOST);
                    }
                }
                self.state(|s| self.refresh(s));
            }
            Event::Push(_) => {}
        }
    }
}

/// The words of the shown text with their times and doubts, for the player: a click on a word, the
/// highlight that follows the sound, the underline of words the model was unsure of.
#[derive(Default)]
struct Marks {
    /// The byte range of each word of the shown text.
    spans: Vec<Range<usize>>,
    /// When each is said, in the audio's time; `None` for every word until a pass made times.
    times: Vec<Option<(Millis, Millis)>>,
    /// The model's confidence in each was under [`history::LOW_CONFIDENCE`].
    doubtful: Vec<bool>,
}

/// The byte ranges of the runs between white space.
fn word_spans(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = None;
    for (at, c) in text.char_indices() {
        match (c.is_whitespace(), start) {
            (true, Some(s)) => {
                out.push(s..at);
                start = None;
            }
            (false, None) => start = Some(at),
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push(s..text.len());
    }
    out
}

/// Letters and digits in lower case: what two spellings of a word are compared by.
fn plain(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

impl Marks {
    /// The words of `text` lined up with the `heard` words of its record. The text may differ from
    /// them (replacements, edits): times come from [`align`], and a word is doubtful only when the
    /// same word is found within a few places of where the last pair was.
    fn new(text: &str, heard: &[Word]) -> Self {
        let spans = word_spans(text);
        let shown: Vec<&str> = spans.iter().map(|r| &text[r.clone()]).collect();
        let times = align(&shown, heard);
        let heard_plain: Vec<String> = heard.iter().map(|w| plain(&w.w)).collect();
        let mut doubtful = vec![false; spans.len()];
        let mut next = 0;
        for (i, word) in shown.iter().enumerate() {
            let want = plain(word);
            let found = (next..heard.len().min(next + 4))
                .find(|j| !want.is_empty() && heard_plain[*j] == want);
            if let Some(j) = found {
                doubtful[i] = heard[j].is_doubtful();
                next = j + 1;
            }
        }
        Self {
            spans,
            times,
            doubtful,
        }
    }

    /// Words have times: click, highlight and Listen work.
    fn timed(&self) -> bool {
        self.times.iter().any(Option::is_some)
    }

    /// The word a cursor at byte `at` is in or right after.
    fn at_offset(&self, at: usize) -> Option<usize> {
        let i = self
            .spans
            .partition_point(|r| r.start <= at)
            .checked_sub(1)?;
        (at <= self.spans[i].end).then_some(i)
    }

    /// The word being said at `t` (the audio's time): the last one begun, kept over a pause for a
    /// moment, and none before the first or long after the last.
    fn at_time(&self, t: Duration) -> Option<usize> {
        let t = u64::try_from(t.as_millis()).unwrap_or(u64::MAX);
        let begun = self
            .times
            .partition_point(|w| w.is_some_and(|(t0, _)| t0.0 <= t));
        let i = begun.checked_sub(1)?;
        let (_, end) = self.times[i]?;
        (t <= end.0 + HOLD).then_some(i)
    }

    /// When the words touched by the byte range `r` start and end; `None` without times.
    fn times_of(&self, r: Range<usize>) -> Option<(Millis, Millis)> {
        let first = self
            .spans
            .iter()
            .position(|s| s.end > r.start && s.start < r.end)?;
        let last = self
            .spans
            .iter()
            .rposition(|s| s.end > r.start && s.start < r.end)?;
        Some((self.times[first]?.0, self.times[last]?.1))
    }

    /// `text` as markdown for the layer under the text area: the words the model doubted
    /// underlined (colour is not the only sign), the word being said in the accent colour. Every
    /// character is escaped, so dictated text never reads as markup.
    ///
    /// shortcut: markdown folds a run of spaces into one; the text area under it does not, so a
    /// text with double spaces drifts from this layer (dictated text has none).
    fn markup(&self, text: &str, current: Option<usize>, accent: &str) -> String {
        let mut out = String::new();
        let mut at = 0;
        for (i, span) in self.spans.iter().enumerate() {
            escape_markdown(&text[at..span.start], &mut out);
            let (mut open, mut close) = (String::new(), String::new());
            if current == Some(i) {
                open.push_str("<font color=\"");
                open.push_str(accent);
                open.push_str("\">");
                close.insert_str(0, "</font>");
            }
            if self.doubtful[i] {
                open.push_str("<u>");
                close.insert_str(0, "</u>");
            }
            out.push_str(&open);
            escape_markdown(&text[span.clone()], &mut out);
            out.push_str(&close);
            at = span.end;
        }
        escape_markdown(&text[at..], &mut out);
        out
    }
}

/// The pauses of `samples` at `rate` the player jumps over, in their time. The voice gate scores
/// 16 kHz audio: a recording at another rate (a recovered raw one) plays as it is.
///
/// shortcut: no skips for a rate other than 16 kHz; resample first if raw recordings get played
/// often.
fn pauses(
    samples: &[f32],
    rate: u32,
    sensitivity: oxi_core::config::VadSensitivity,
) -> Vec<Range<Duration>> {
    if rate != SAMPLE_RATE {
        return Vec::new();
    }
    let per_ms = u64::from(SAMPLE_RATE / 1000);
    let ms = |at: usize| Millis(u64::try_from(at).unwrap_or(u64::MAX) / per_ms);
    let voiced: Vec<Range<Millis>> = voiced_runs(samples, sensitivity)
        .into_iter()
        .map(|r| ms(r.start)..ms(r.end))
        .collect();
    history::skips(&voiced, ms(samples.len()))
        .into_iter()
        .map(|r| Duration::from_millis(r.start.0)..Duration::from_millis(r.end.0))
        .collect()
}

/// The held parts of `item`, joined with a space.
fn held_text(item: &HistoryItem) -> String {
    item.held
        .iter()
        .map(|h| h.text.0.trim())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Why the parts of `item` were held, for the tooltip of the "Not inserted" row.
fn held_why(item: &HistoryItem) -> &'static str {
    if item.held.iter().any(|h| h.why == HeldWhy::Hallucination) {
        "Likely hallucination: whisper may have made this up, so it was not inserted. Copy it or add it to the text if it is what you said."
    } else {
        "Low confidence: whisper was unsure of these words, so they were not inserted. Copy them or add them to the text if they are right."
    }
}

/// The held row's "Add to the text" and the "Always fix" offer.
fn bind_offers(s: &HistoryState<'_>, page: &Rc<PageState>) {
    let p = Rc::clone(page);
    s.on_add_held(move || {
        p.add_held();
    });
    let p = Rc::clone(page);
    s.on_accept_offer(move || p.accept_offer());
}

/// One line of the list.
fn row(item: &HistoryItem, folder: &Path) -> HistoryRow {
    let text = item.text.0.trim();
    let held = held_text(item);
    HistoryRow {
        held_why: held_why(item).into(),
        held_add: !held.is_empty() && !text.contains(&held),
        held: held.into(),
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

    use oxi_core::ipc::{HeldText, Request};
    use slint::{ComponentHandle, Model};

    use super::*;
    use crate::ipc::{Cmd, Ipc};
    use crate::testing::{has, linked};

    /// A speaker that plays nothing and remembers the plans and skips it was given (`PLANS`,
    /// `SKIPS`).
    #[expect(clippy::unnecessary_wraps, reason = "the `Player` signature")]
    fn silent(
        _: &Arc<[f32]>,
        _: u32,
        plan: &Plan,
        skips: &[Range<Duration>],
    ) -> Result<Box<dyn Voice>, CaptureError> {
        PLANS.with_borrow_mut(|plans| plans.push(*plan));
        SKIPS.with_borrow_mut(|all| *all = skips.to_vec());
        Ok(Box::new(Heard))
    }

    fn item(id: &str, status: Status, text: &str) -> HistoryItem {
        HistoryItem {
            id: id.into(),
            status,
            duration_ms: 4_500,
            model: String::new(),
            language: String::new(),
            text: Spoken(text.into()),
            held: Vec::new(),
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
        // The words of the run come before its text and go back with it.
        ipc.deliver(&Event::Push(Push::HistoryRunWords {
            id: 1,
            words: Words(vec![said("theirs", 0.9, None)]),
        }));
        ipc.deliver(&Event::Push(Push::HistoryRunWords {
            id: run,
            words: Words(vec![said("new", 0.8, None), said("text", 0.2, None)]),
        }));
        ipc.deliver(&Event::Push(Push::TestResult {
            id: run,
            outcome: TestOutcome::Heard {
                text: "new text".into(),
                ms: 900,
            },
        }));
        match sent_now(&sent).as_slice() {
            [
                Request::HistorySetText {
                    id, text, words, ..
                },
            ] => {
                assert_eq!(id, "2026-11-08/120000-000");
                assert_eq!(text.0, "new text");
                assert_eq!(words.0, [said("new", 0.8, None), said("text", 0.2, None)]);
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

    /// d12: after Copy (a long single line) nothing of the page passes the window's right edge.
    fn copy_fits(name: &str, w: f32, h: f32) {
        let (window, _ipc, _sent, _ctx) = picked_page(name, &"word".repeat(400));
        window.window().set_size(slint::LogicalSize::new(w, h));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(50));
        crate::testing::with_role(
            &window,
            "Copy text",
            i_slint_backend_testing::AccessibleRole::Button,
        )
        .invoke_accessible_default_action();
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(50));
        assert_eq!(window.global::<Toast>().get_text(), "Copied.");
        for label in ["Copy text", "Delete", "Transcript"] {
            let e = crate::testing::one(&window, label);
            let right = e.absolute_position().x + e.size().width;
            assert!(
                right <= w,
                "{label} ends at {right}, past the {w} px window"
            );
        }
    }

    #[test]
    fn copy_leaves_the_detail_inside_a_small_window() {
        copy_fits("copy-fits-720", 720.0, 520.0);
    }

    #[test]
    fn copy_leaves_the_detail_inside_a_large_window() {
        copy_fits("copy-fits-1280", 1280.0, 860.0);
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

    /// Review d03 #6: a held part shows as "not inserted" with Copy and "Add to the text".
    #[test]
    fn a_held_part_shows_not_inserted_and_add_puts_it_at_the_end() {
        let (window, ipc, sent, _ctx) = page("held");
        let s = window.global::<HistoryState>();
        s.invoke_shown();
        let part = |text: &str, why| HeldText {
            text: Spoken(text.into()),
            why,
        };
        let mut partly = item("2026-11-08/134501-123", Status::Done, "call Anna");
        partly.held = vec![part("at noon", HeldWhy::LowConfidence)];
        // All held: the text is the held part, nothing to add.
        let mut all = item("2026-11-08/134400-000", Status::Done, "Thanks for watching");
        all.held = vec![part("Thanks for watching", HeldWhy::Hallucination)];
        listed(
            &ipc,
            vec![
                partly,
                all,
                item("2026-11-08/134300-000", Status::Done, "x"),
            ],
            false,
        );
        sent_now(&sent);
        let r = rows(&window);
        assert_eq!((r[0].held.as_str(), r[0].held_add), ("at noon", true));
        assert!(r[0].held_why.starts_with("Low confidence"));
        assert_eq!(
            (r[1].held.as_str(), r[1].held_add),
            ("Thanks for watching", false)
        );
        assert!(r[1].held_why.starts_with("Likely hallucination"));
        assert_eq!((r[2].held.as_str(), r[2].held_add), ("", false));
        s.invoke_add_held();
        assert_eq!(
            sent_now(&sent),
            [Request::HistoryEdit {
                id: "2026-11-08/134501-123".into(),
                text: Spoken("call Anna at noon".into()),
            }]
        );
        s.set_selected(1);
        s.invoke_add_held();
        assert_eq!(sent_now(&sent), [], "already the text");
    }

    /// Review d03 (d): an edit that reads as a misheard word offers "Always fix" once; a click
    /// adds the rule, nothing is added without it.
    #[test]
    fn an_edit_offers_always_fix_once_and_a_click_adds_the_rule() {
        let (window, ipc, _sent, ctx) = picked_page("offer", "call Anna at noon");
        let s = window.global::<HistoryState>();
        let file = ctx.layout.replace_file();
        assert!(s.invoke_fix_here(5, 9, "Hanna".into()));
        assert_eq!(s.get_offer_id(), "", "not before the answer");
        reply(&ipc, HistoryOp::Edit, true);
        assert_eq!(s.get_offer_id(), "2026-11-08/134501-123");
        assert!(s.get_offer_tip().contains("Anna") && s.get_offer_tip().contains("Hanna"));
        assert!(!file.exists(), "nothing added unasked");
        s.invoke_accept_offer();
        assert!(fs::read_to_string(&file).unwrap().contains("Anna => Hanna"));
        assert_eq!(s.get_offer_id(), "");
        // The same pair again: not offered a second time.
        assert!(s.invoke_fix_here(5, 10, "Anna".into()));
        reply(&ipc, HistoryOp::Edit, true);
        assert!(s.invoke_fix_here(5, 9, "Hanna".into()));
        reply(&ipc, HistoryOp::Edit, true);
        assert_eq!(
            s.get_offer_id(),
            "2026-11-08/134501-123",
            "the way back was new"
        );
        s.invoke_accept_offer();
        assert!(s.invoke_fix_here(5, 10, "Anna".into()));
        reply(&ipc, HistoryOp::Edit, true);
        assert_eq!(s.get_offer_id(), "", "offered once");
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
            words: Vec::new(),
            timings: None,
            raw: false,
            held: Vec::new(),
            delivered: None,
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

    const SAID: &str = "alpha beta gamma delta";
    const ID: &str = "2026-11-08/134501-123";

    fn said(text: &str, p: f32, at: Option<(u32, u32)>) -> Word {
        Word {
            w: text.into(),
            p,
            t0: at.map(|a| a.0),
            t1: at.map(|a| a.1),
        }
    }

    /// The four words of [`SAID`], the second one doubtful; timed or not.
    fn four(timed: bool) -> Vec<Word> {
        let at = |a, b| timed.then_some((a, b));
        vec![
            said("alpha", 0.9, at(0, 400)),
            said("beta", 0.3, at(500, 900)),
            said("gamma", 0.9, at(1000, 1400)),
            said("delta", 0.9, at(1500, 1900)),
        ]
    }

    thread_local! {
        /// The plans the fake speaker was given, the clip time it says it is at, and whether it is over.
        static PLANS: RefCell<Vec<Plan>> = const { RefCell::new(Vec::new()) };
        static SKIPS: RefCell<Vec<Range<Duration>>> = const { RefCell::new(Vec::new()) };
        static AT: Cell<u64> = const { Cell::new(0) };
        static OVER: Cell<bool> = const { Cell::new(false) };
    }

    struct Heard;

    impl Voice for Heard {
        fn position(&self) -> Duration {
            Duration::from_millis(AT.get())
        }

        fn finished(&self) -> bool {
            OVER.get()
        }
    }

    fn plans() -> Vec<Plan> {
        PLANS.with_borrow(Clone::clone)
    }

    /// The page with one recording: audio, [`SAID`] as its text and `words` in its json, picked.
    fn recorded(name: &str, words: Vec<Word>) -> (AppWindow, Ipc, Receiver<Cmd>, Ctx) {
        PLANS.with_borrow_mut(Vec::clear);
        AT.set(0);
        OVER.set(false);
        let (window, ipc, sent, ctx) = page(name);
        let folder = history::folder(
            &ctx.config.borrow().config.history,
            &ctx.layout.history_dir(),
        );
        fs::create_dir_all(folder.join("2026-11-08")).unwrap();
        let samples: Vec<f32> = (0..32_000_u16).map(|i| f32::from(i % 50) / 100.0).collect();
        oxi_platform::wav::write_wav(&wav_path(&folder, ID), &samples).unwrap();
        let timed = words.iter().any(|w| w.t0.is_some());
        let meta = Meta {
            version: Meta::VERSION,
            started: String::new(),
            duration_ms: 2000,
            status: Status::Done,
            model: String::new(),
            language: String::new(),
            settings: None,
            previous: None,
            words,
            timings: timed.then(|| history::Timings {
                by: history::TimingBy::Tokens,
                preset: String::new(),
                model: String::new(),
            }),
            raw: false,
            held: Vec::new(),
            delivered: None,
        };
        fs::write(
            entry_path(&folder, ID, history::JSON),
            meta.to_json().unwrap(),
        )
        .unwrap();
        window.global::<HistoryState>().invoke_shown();
        listed(&ipc, vec![item(ID, Status::Done, SAID)], false);
        sent_now(&sent);
        (window, ipc, sent, ctx)
    }

    #[test]
    fn a_click_on_a_timed_word_plays_from_it_and_a_doubtful_one_also_offers_the_fix() {
        let (window, _ipc, _sent, _ctx) = recorded("click", four(true));
        let s = window.global::<HistoryState>();
        assert!(s.get_timed());
        assert!(s.get_marks_on(), "the doubtful word is underlined at rest");
        let span = s.invoke_word_click(14);
        assert_eq!((span.a, span.b), (0, 0), "gamma is not doubtful");
        assert!(s.get_playing());
        let [plan] = plans()[..] else {
            panic!("{:?}", plans());
        };
        assert_eq!(
            plan.from,
            Duration::from_millis(850),
            "150 ms before the word"
        );
        assert_eq!((plan.to, plan.speed, plan.looping), (None, 1.0, false));
        // The doubtful beta: it plays from there, and its range comes back to be selected.
        let span = s.invoke_word_click(7);
        assert_eq!((span.a, span.b), (6, 10));
        assert_eq!(plans().last().unwrap().from, Duration::from_millis(350));
        // Between nothing and the first word: no word, no play.
        let before = plans().len();
        s.invoke_word_click(-1);
        assert_eq!(plans().len(), before);
    }

    #[test]
    fn listen_plays_the_selected_words_and_speed_and_loop_restart_it() {
        let (window, _ipc, _sent, _ctx) = recorded("listen", four(true));
        let s = window.global::<HistoryState>();
        s.invoke_listen(6, 16);
        let ms = Duration::from_millis;
        let first = plans()[0];
        assert_eq!((first.from, first.to), (ms(350), Some(ms(1550))));
        s.invoke_speed_picked(3);
        let second = plans()[1];
        assert_eq!(
            (second.from, second.to, second.speed),
            (ms(350), Some(ms(1550)), 1.5)
        );
        assert_eq!(s.get_speed(), 3);
        s.invoke_toggle_loop();
        let third = plans()[2];
        assert!(third.looping && (third.speed - 1.5).abs() < f32::EPSILON);
        assert!(s.get_loop_on());
        // With nothing playing the controls only set what the next play uses.
        s.invoke_play();
        assert!(!s.get_playing(), "a press while it plays stops");
        s.invoke_speed_picked(4);
        let before = plans().len();
        assert!(!s.get_playing() && plans().len() == before);
        s.invoke_play();
        let last = *plans().last().unwrap();
        assert_eq!(
            (last.from, last.to, last.speed, last.looping),
            (ms(0), None, 2.0, true)
        );
        // A selection outside every word, and a bad range, play nothing.
        let before = plans().len();
        s.invoke_listen(5, 5);
        s.invoke_listen(-3, 4);
        assert_eq!(plans().len(), before);
    }

    #[test]
    fn without_times_nothing_plays_from_a_word_and_listen_is_off() {
        let (window, _ipc, _sent, _ctx) = recorded("untimed", four(false));
        let s = window.global::<HistoryState>();
        assert!(!s.get_timed());
        let span = s.invoke_word_click(7);
        assert_eq!(
            (span.a, span.b),
            (6, 10),
            "the doubtful word still offers the fix"
        );
        s.invoke_listen(6, 10);
        assert!(plans().is_empty() && !s.get_playing());
        assert!(s.get_marks_on(), "the underline needs no times");
    }

    #[test]
    fn the_sound_ends_the_play_and_the_highlight() {
        let (window, _ipc, _sent, _ctx) =
            recorded("follow", vec![said("alpha", 0.9, Some((0, 400)))]);
        let s = window.global::<HistoryState>();
        assert!(
            !s.get_marks_on(),
            "no doubt, nothing playing: the plain text"
        );
        s.invoke_play();
        assert!(s.get_playing() && s.get_marks_on(), "the word being said");
        AT.set(2_000);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(60));
        assert!(
            s.get_playing() && !s.get_marks_on(),
            "long past the last word"
        );
        OVER.set(true);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(60));
        assert!(!s.get_playing());
    }

    #[test]
    fn word_times_are_asked_for_at_the_first_play_not_when_the_pick_rests() {
        let (window, ipc, sent, _ctx) = recorded("ask", four(false));
        let s = window.global::<HistoryState>();
        i_slint_backend_testing::mock_elapsed_time(Duration::from_secs(5));
        assert_eq!(sent_now(&sent), [], "resting on the record starts no run");
        s.invoke_play();
        let run = match sent_now(&sent).as_slice() {
            [Request::HistoryWords { run, id }] => {
                assert_eq!(id, ID);
                *run
            }
            other => panic!("{other:?}"),
        };
        assert!(run >= FIRST_RUN);
        assert!(s.get_timing() && !s.get_timed());
        ipc.deliver(&Event::Push(Push::TestProgress {
            id: run,
            percent: 50,
            est_ms: None,
        }));
        // The bar is the shared driver's: it glides, never jumps to whisper's percent.
        assert!(s.get_timing_progress() < 0.5, "{}", s.get_timing_progress());
        ipc.deliver(&Event::Push(Push::HistoryWords {
            run,
            id: ID.into(),
            result: WordsResult::Ready {
                words: Words(four(true)),
                timed: true,
            },
        }));
        assert!(!s.get_timing() && s.get_timed());
        // Play already started the sound; Listen plays the first word's range on top of it.
        s.invoke_listen(0, 5);
        assert_eq!(plans().len(), 2);
        assert!(plans()[1].to.is_some(), "{:?}", plans());
    }

    #[test]
    fn an_ask_lost_with_the_link_is_asked_again_once_it_is_back() {
        let (window, ipc, sent, _ctx) = recorded("relink", four(false));
        let s = window.global::<HistoryState>();
        s.invoke_play();
        assert!(matches!(
            sent_now(&sent)[..],
            [Request::HistoryWords { .. }]
        ));
        ipc.deliver(&Event::Link(Link::Offline));
        ipc.deliver(&Event::Link(Link::Online));
        sent_now(&sent);
        s.invoke_listen(0, 5);
        assert!(
            matches!(sent_now(&sent)[..], [Request::HistoryWords { .. }]),
            "the lost ask is asked again"
        );
    }

    #[test]
    fn opening_the_export_menu_asks_for_word_times() {
        let (window, _ipc, sent, _ctx) = recorded("menu", four(false));
        let s = window.global::<HistoryState>();
        assert_eq!(sent_now(&sent), []);
        s.invoke_export_opened();
        assert!(matches!(
            sent_now(&sent)[..],
            [Request::HistoryWords { .. }]
        ));
        assert_eq!(s.get_times_off(), TIMING, "SRT and VTT say why they wait");
    }

    #[test]
    fn a_busy_or_failed_answer_is_not_asked_again_and_a_stale_one_is_dropped() {
        let (window, ipc, sent, _ctx) = recorded("busy", four(false));
        let s = window.global::<HistoryState>();
        s.invoke_play();
        let [Request::HistoryWords { run, .. }] = sent_now(&sent)[..] else {
            panic!("no ask");
        };
        ipc.deliver(&Event::Push(Push::HistoryWords {
            run,
            id: ID.into(),
            result: WordsResult::Busy,
        }));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_secs(5));
        assert_eq!(sent_now(&sent), [], "no loop of asks");
        assert!(!s.get_timing() && !s.get_timed());
        s.invoke_play();
        s.invoke_play();
        s.invoke_play();
        assert_eq!(sent_now(&sent), [], "a later Play does not ask again");
        // An answer for a run the pick left (it was cancelled) asks for nothing.
        ipc.deliver(&Event::Push(Push::HistoryWords {
            run: run + 77,
            id: "2026-11-01/000000-000".into(),
            result: WordsResult::Busy,
        }));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(500));
        assert_eq!(sent_now(&sent), []);
        // The coordinator turned the run away (a dictation): it ends the wait the same way.
        ipc.deliver(&Event::Push(Push::TestResult {
            id: run + 1,
            outcome: TestOutcome::Failed {
                why: TestFailure::Busy,
            },
        }));
        assert!(!s.get_timing());
        i_slint_backend_testing::mock_elapsed_time(Duration::from_secs(5));
        assert_eq!(sent_now(&sent), []);
    }

    #[test]
    fn a_new_pick_cancels_the_word_times_run_of_the_last() {
        let (window, ipc, sent, _ctx) = recorded("cancel", four(false));
        let s = window.global::<HistoryState>();
        listed(
            &ipc,
            vec![
                item(ID, Status::Done, SAID),
                item("2026-11-07/090000-000", Status::Done, "other"),
            ],
            false,
        );
        s.invoke_play();
        let [Request::HistoryWords { run, .. }] = sent_now(&sent)[..] else {
            panic!("no ask");
        };
        assert!(s.get_timing());
        s.invoke_select(1);
        assert_eq!(sent_now(&sent), [Request::HistoryWordsCancel { run }]);
        assert!(!s.get_timing(), "the bar goes with the run");
        // Its late answer changes nothing.
        ipc.deliver(&Event::Push(Push::HistoryWords {
            run,
            id: ID.into(),
            result: WordsResult::Busy,
        }));
        assert!(!s.get_timing() && sent_now(&sent).is_empty());
    }

    /// A 16 kHz record of a tone, 2 s of silence, and the tone again.
    fn with_a_pause(name: &str) -> (AppWindow, Ipc, Receiver<Cmd>, Ctx) {
        let (window, ipc, sent, ctx) = recorded(name, four(true));
        let folder = history::folder(
            &ctx.config.borrow().config.history,
            &ctx.layout.history_dir(),
        );
        // 300 Hz at about -20 dBFS: what the voice gate's own tests count as voice.
        let tone = |n: u16| {
            (0..n).map(|i| (std::f32::consts::TAU * 300.0 * f32::from(i) / 16_000.0).sin() * 0.14)
        };
        let samples: Vec<f32> = tone(8_000)
            .chain(std::iter::repeat_n(0.0, 32_000))
            .chain(tone(8_000))
            .collect();
        oxi_platform::wav::write_wav(&wav_path(&folder, ID), &samples).unwrap();
        // Picked again, so the new audio is read.
        let s = window.global::<HistoryState>();
        s.invoke_select(-1);
        s.invoke_select(0);
        (window, ipc, sent, ctx)
    }

    #[test]
    fn skip_pauses_is_a_saved_switch_and_the_player_jumps_the_pause() {
        let (window, _ipc, _sent, ctx) = with_a_pause("skip");
        let s = window.global::<HistoryState>();
        assert!(!s.get_skip_on());
        s.invoke_play();
        assert!(SKIPS.with_borrow(Vec::is_empty), "off: the record as it is");
        s.invoke_toggle_skip();
        assert!(s.get_skip_on());
        let saved = fs::read_to_string(ctx.layout.config_file()).unwrap();
        assert!(saved.contains("skip_pauses = true"), "{saved}");
        // The sound in hand starts again with the pause skipped.
        assert_eq!(plans().len(), 2);
        let skips = SKIPS.with_borrow(Clone::clone);
        assert!(
            skips.iter().any(|p| p.start >= Duration::from_millis(400)
                && p.end <= Duration::from_millis(2600)
                && p.end.saturating_sub(p.start) >= Duration::from_millis(1000)),
            "{skips:?}"
        );
        s.invoke_toggle_skip();
        s.invoke_play();
        s.invoke_play();
        assert!(SKIPS.with_borrow(Vec::is_empty));
    }

    #[test]
    fn the_word_being_said_is_boxed_as_well_as_coloured() {
        let (window, _ipc, _sent, _ctx) = recorded("boxed", four(true));
        let s = window.global::<HistoryState>();
        AT.set(1_100);
        s.invoke_play();
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(20));
        let word = s.get_current_word();
        assert_eq!((word.a, word.b), (11, 16), "gamma has the box");
        s.invoke_play();
        let word = s.get_current_word();
        assert_eq!((word.a, word.b), (0, 0), "stopped: no box");
    }

    /// Presses and releases the left button at `at` (window coordinates).
    fn click(window: &AppWindow, at: slint::LogicalPosition) {
        use slint::platform::{PointerEventButton, WindowEvent};
        let w = window.window();
        let button = PointerEventButton::Left;
        w.dispatch_event(WindowEvent::PointerMoved { position: at });
        w.dispatch_event(WindowEvent::PointerPressed {
            position: at,
            button,
        });
        w.dispatch_event(WindowEvent::PointerReleased {
            position: at,
            button,
        });
        // The click rests: no drag follows.
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(300));
    }

    #[test]
    fn a_click_on_the_first_character_or_after_ctrl_c_still_plays() {
        let (window, _ipc, _sent, _ctx) = recorded("clicks", four(true));
        window
            .window()
            .set_size(slint::LogicalSize::new(1280.0, 860.0));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(50));
        let text = crate::testing::one(&window, "Transcript");
        let (at, size) = (text.absolute_position(), text.size());
        assert!(size.width > 100.0, "the text area is laid out");
        // The left edge of the first character: caret 0, the first word.
        let first = slint::LogicalPosition::new(at.x + 13.0, at.y + 16.0);
        click(&window, first);
        let [plan] = plans()[..] else {
            panic!("the first character did not play: {:?}", plans());
        };
        assert_eq!(plan.from, Duration::ZERO);
        // An arrow key moves both ends of the parked caret: neither change is a click.
        let w = window.window();
        let key = |text: slint::SharedString, down: bool| {
            use slint::platform::WindowEvent::{KeyPressed, KeyReleased};
            w.dispatch_event(if down {
                KeyPressed { text }
            } else {
                KeyReleased { text }
            });
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(50));
        };
        i_slint_backend_testing::mock_elapsed_time(Duration::from_secs(1));
        key(slint::platform::Key::LeftArrow.into(), true);
        key(slint::platform::Key::LeftArrow.into(), false);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(300));
        assert_eq!(plans().len(), 1, "an arrow key played a word");
        // Select from the first character, copy, then click where the drag ended (one end of the
        // caret moves): Ctrl+C moved nothing, so that click plays.
        let end = slint::LogicalPosition::new(first.x + 40.0, first.y);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_secs(1));
        {
            use slint::platform::{PointerEventButton, WindowEvent};
            let button = PointerEventButton::Left;
            w.dispatch_event(WindowEvent::PointerMoved { position: first });
            w.dispatch_event(WindowEvent::PointerPressed {
                position: first,
                button,
            });
            w.dispatch_event(WindowEvent::PointerMoved { position: end });
            w.dispatch_event(WindowEvent::PointerReleased {
                position: end,
                button,
            });
        }
        i_slint_backend_testing::mock_elapsed_time(Duration::from_secs(1));
        assert_eq!(plans().len(), 1, "a drag is no click");
        key(slint::platform::Key::Control.into(), true);
        key("c".into(), true);
        key("c".into(), false);
        key(slint::platform::Key::Control.into(), false);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_secs(1));
        click(&window, end);
        assert_eq!(plans().len(), 2, "the click after Ctrl+C was swallowed");
    }

    /// How many elements are labelled `label`.
    fn count(window: &AppWindow, label: &str) -> usize {
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(window, label).count()
    }

    /// Drags across the first word of the text, which opens the selection bar.
    fn select_first_word(window: &AppWindow) {
        use slint::platform::{PointerEventButton, WindowEvent};
        window
            .window()
            .set_size(slint::LogicalSize::new(1280.0, 860.0));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(50));
        let at = crate::testing::one(window, "Transcript").absolute_position();
        let (from, to) = (
            slint::LogicalPosition::new(at.x + 13.0, at.y + 16.0),
            slint::LogicalPosition::new(at.x + 50.0, at.y + 16.0),
        );
        let w = window.window();
        let button = PointerEventButton::Left;
        w.dispatch_event(WindowEvent::PointerMoved { position: from });
        w.dispatch_event(WindowEvent::PointerPressed {
            position: from,
            button,
        });
        w.dispatch_event(WindowEvent::PointerMoved { position: to });
        w.dispatch_event(WindowEvent::PointerReleased {
            position: to,
            button,
        });
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(300));
    }

    #[test]
    fn with_word_times_the_selection_bar_has_listen() {
        let (window, _ipc, _sent, _ctx) = recorded("bar-timed", four(true));
        select_first_word(&window);
        assert_eq!(count(&window, "Copy"), 1, "the bar is open");
        assert_eq!(count(&window, "Listen"), 1);
        assert_eq!(count(&window, "Speed, loop and pauses"), 1);
    }

    #[test]
    fn without_word_times_listen_is_absent_not_disabled() {
        let (window, _ipc, _sent, _ctx) = recorded("bar-untimed", four(false));
        select_first_word(&window);
        assert_eq!(count(&window, "Copy"), 1, "the bar is open");
        assert_eq!(count(&window, "Listen"), 0);
        // The audio plays, so its options stay.
        assert_eq!(count(&window, "Speed, loop and pauses"), 1);
    }

    #[test]
    fn without_audio_the_player_options_are_absent() {
        // An interrupted entry with no .wav.
        let (window, ipc, _sent, _ctx) = page("bar-silent");
        listed(&ipc, vec![item(ID, Status::Interrupted, SAID)], false);
        assert_eq!(count(&window, "Speed, loop and pauses"), 0);
    }

    #[test]
    fn a_record_with_times_asks_for_nothing() {
        let (window, _ipc, sent, _ctx) = recorded("timed", four(true));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_secs(2));
        assert_eq!(sent_now(&sent), []);
        assert!(!window.global::<HistoryState>().get_timing());
    }

    #[test]
    fn words_line_up_with_the_text_and_map_to_times_and_back() {
        let marks = Marks::new(SAID, &four(true));
        assert_eq!(marks.spans, [0..5, 6..10, 11..16, 17..22]);
        assert_eq!(marks.doubtful, [false, true, false, false]);
        assert!(marks.timed());
        // The cursor in a word or right after it is in it; in the gap between, the next word.
        assert_eq!(marks.at_offset(0), Some(0));
        assert_eq!(marks.at_offset(5), Some(0));
        assert_eq!(marks.at_offset(6), Some(1));
        assert_eq!(marks.at_offset(22), Some(3));
        assert_eq!(marks.at_offset(99), None);
        let at = |ms| marks.at_time(Duration::from_millis(ms));
        assert_eq!(at(0), Some(0));
        assert_eq!(at(450), Some(0), "kept over the pause");
        assert_eq!(at(500), Some(1));
        assert_eq!(at(1950), Some(3));
        assert_eq!(at(1900 + HOLD + 1), None, "long after the last word");
        assert_eq!(
            marks.times_of(8..12),
            Some((Millis(500), Millis(1400))),
            "a selection touching two words"
        );
        assert_eq!(marks.times_of(5..6), None, "only white space");
        // Edited text: the replaced word is interpolated, the rest keep their own times.
        let edited = Marks::new("alpha BETA! gamma delta", &four(true));
        assert_eq!(edited.times[1], Some((Millis(500), Millis(900))));
        let other = Marks::new("alpha omega gamma delta", &four(true));
        assert!(
            other.times[1].is_some(),
            "interpolated between its neighbours"
        );
        assert!(
            !other.doubtful[1],
            "a word the model never heard is not doubtful"
        );
        // No times: nothing to play, and the underline stays.
        let plain = Marks::new(SAID, &four(false));
        assert!(!plain.timed() && plain.at_time(Duration::from_millis(100)).is_none());
        assert_eq!(plain.doubtful, [false, true, false, false]);
        // No words at all (an old record): empty marks that never panic.
        let none = Marks::new("", &[]);
        assert!(none.at_offset(0).is_none() && none.times_of(0..1).is_none());
        assert_eq!(word_spans("  \n "), Vec::<Range<usize>>::new());
    }

    #[test]
    fn the_marked_text_is_escaped_and_marks_only_the_doubtful_and_the_current_word() {
        let words = vec![said("a*b", 0.9, None), said("(c)", 0.1, None)];
        let marks = Marks::new("a*b\n(c) <u>", &words);
        let markup = marks.markup("a*b\n(c) <u>", Some(0), "#112233");
        assert_eq!(
            markup,
            "<font color=\"#112233\">a\\*b</font>\\\n<u>\\(c\\)</u> \\<u\\>"
        );
        assert!(marks.doubtful[1] && !marks.doubtful[2]);
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
        let spoken = few[0].text.0.clone();
        for state in [
            "empty", "off", "few", "many", "changes", "selected", "doubtful", "playing", "search",
            "nothing", "file",
        ] {
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
            if matches!(state, "doubtful" | "playing" | "selected") {
                // The first entry has audio and words: two the model doubted, every word timed.
                let day = layout.history_dir().join("2026-11-08");
                fs::create_dir_all(&day).unwrap();
                let speech: Vec<f32> = (0..64_000_u16)
                    .map(|i| (f32::from(i) / 9.0).sin() * 0.3)
                    .collect();
                oxi_platform::wav::write_wav(&day.join("134501-123.wav"), &speech).unwrap();
                let words = spoken
                    .split_whitespace()
                    .enumerate()
                    .map(|(i, w)| {
                        let at = u32::try_from(i).unwrap() * 330;
                        let doubtful = w.starts_with("Anna") || w.starts_with("tickets");
                        said(w, if doubtful { 0.2 } else { 0.9 }, Some((at, at + 300)))
                    })
                    .collect();
                let meta = Meta {
                    version: Meta::VERSION,
                    started: String::new(),
                    duration_ms: 4500,
                    status: Status::Done,
                    model: String::new(),
                    language: String::new(),
                    settings: None,
                    previous: None,
                    words,
                    timings: Some(history::Timings {
                        by: history::TimingBy::Tokens,
                        preset: String::new(),
                        model: String::new(),
                    }),
                    raw: false,
                    held: Vec::new(),
                    delivered: None,
                };
                fs::write(day.join("134501-123.json"), meta.to_json().unwrap()).unwrap();
            }
            if state == "playing" {
                TEST_PLAYER.set(Some(silent));
                AT.set(900);
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
                "few" | "changes" | "selected" | "doubtful" | "playing" | "search" | "nothing"
                | "file" => few.clone(),
                "many" => many.clone(),
                _ => Vec::new(),
            };
            listed(&ipc, items, false);
            if state == "few" {
                window.global::<HistoryState>().invoke_select(1);
            }
            if matches!(state, "search" | "nothing") {
                // A day from Home's heatmap and a query: the one match, or none.
                let s = window.global::<HistoryState>();
                s.invoke_open_day("2026-11-08".into());
                s.set_query(if state == "search" { "report" } else { "zzz" }.into());
                let found = if state == "search" {
                    vec![few[0].clone()]
                } else {
                    Vec::new()
                };
                ipc.deliver(&Event::Push(Push::HistoryFound {
                    seq: 1,
                    items: found,
                    more: false,
                }));
            }
            if state == "file" {
                let s = window.global::<HistoryState>();
                s.set_file_running(true);
                s.set_file_progress(0.45);
            }
            if state == "changes" {
                window.global::<HistoryState>().invoke_toggle_diff();
            }
            if state == "playing" {
                // A click on "report" while the sound is on it.
                let at = i32::try_from(spoken.find("report").unwrap()).unwrap() + 2;
                window.global::<HistoryState>().invoke_word_click(at);
                TEST_PLAYER.set(None);
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

    const A: &str = "2026-11-08/100000-000";
    const B: &str = "2026-11-08/110000-000";

    #[test]
    fn typing_sends_one_search_after_the_input_rests_and_clearing_brings_the_list_back() {
        let (window, ipc, sent, _ctx) = page("search");
        let s = window.global::<HistoryState>();
        s.invoke_shown();
        sent_now(&sent);
        listed(
            &ipc,
            vec![
                item(B, Status::Done, "beta"),
                item(A, Status::Done, "alpha"),
            ],
            false,
        );
        for typed in ["a", "al", "alp"] {
            s.invoke_search_edited(typed.into());
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(100));
        }
        assert_eq!(sent_now(&sent), [], "still typing");
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(300));
        let [
            Request::HistorySearch {
                seq, query, day, ..
            },
        ] = &sent_now(&sent)[..]
        else {
            panic!("one search");
        };
        assert_eq!((query, day), (&Spoken("alp".into()), &None));
        let seq = *seq;
        assert!(s.get_search_on());
        // The answer of an older search, and a plain list (the Test page's), change nothing.
        ipc.deliver(&Event::Push(Push::HistoryFound {
            seq: seq - 1,
            items: vec![],
            more: false,
        }));
        listed(&ipc, vec![], false);
        assert_eq!(rows_len(&window), 2);
        ipc.deliver(&Event::Push(Push::HistoryFound {
            seq,
            items: vec![item(A, Status::Done, "alpha")],
            more: false,
        }));
        assert_eq!(rows_len(&window), 1);
        // A dictation that does not match is not added; one that does is.
        ipc.deliver(&Event::Push(Push::HistoryAdded {
            item: item("2026-11-08/120000-000", Status::Done, "gamma"),
        }));
        assert_eq!(rows_len(&window), 1);
        ipc.deliver(&Event::Push(Push::HistoryAdded {
            item: item("2026-11-08/130000-000", Status::Done, "Alphabet"),
        }));
        assert_eq!(rows_len(&window), 2);
        // Nothing found says so; clearing the field asks for the plain list again.
        s.invoke_search_edited("zzz".into());
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(400));
        let [Request::HistorySearch { seq, .. }] = sent_now(&sent)[..] else {
            panic!("one search");
        };
        ipc.deliver(&Event::Push(Push::HistoryFound {
            seq,
            items: vec![],
            more: false,
        }));
        assert!(has(&window, "Nothing found"));
        s.invoke_search_edited("".into());
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(400));
        assert_eq!(sent_now(&sent), [Request::HistoryList { older_than: None }]);
        assert!(!s.get_search_on());
    }

    #[test]
    fn a_day_and_a_length_narrow_the_search_and_the_day_chip_clears() {
        let (window, _ipc, sent, _ctx) = page("filters");
        let s = window.global::<HistoryState>();
        s.invoke_open_day("2026-11-08".into());
        let [Request::HistorySearch { day, query, .. }] = &sent_now(&sent)[..] else {
            panic!("one search");
        };
        assert_eq!(
            (day.as_deref(), query),
            (Some("2026-11-08"), &Spoken(String::new()))
        );
        let today = oxi_platform::stats_store::today();
        let nov8 = Date::from_ymd(2026, 11, 8).unwrap();
        assert_eq!(s.get_day(), super::super::home::day_label(nov8, today));
        assert!(s.get_day().starts_with("Nov 8"));
        s.invoke_length_picked(1);
        let [
            Request::HistorySearch {
                min_ms,
                max_ms,
                day,
                ..
            },
        ] = &sent_now(&sent)[..]
        else {
            panic!("one search");
        };
        assert_eq!(
            (*min_ms, *max_ms, day.is_some()),
            (None, Some(10_000), true)
        );
        s.invoke_length_picked(3);
        let [Request::HistorySearch { min_ms, max_ms, .. }] = sent_now(&sent)[..] else {
            panic!("one search");
        };
        assert_eq!((min_ms, max_ms), (Some(60_001), None));
        s.invoke_clear_day();
        let [Request::HistorySearch { day, .. }] = &sent_now(&sent)[..] else {
            panic!("the length still narrows");
        };
        assert_eq!(*day, None);
        assert_eq!(s.get_day(), "");
        s.invoke_length_picked(0);
        assert_eq!(sent_now(&sent), [Request::HistoryList { older_than: None }]);
    }

    #[test]
    fn a_heatmap_day_on_home_opens_history_on_that_day_with_no_query() {
        use crate::generated::HomeState;
        let (window, ipc, sent, _layout) = linked("history", "home-day", Page::Home);
        let today = oxi_platform::stats_store::today();
        let mut stats = oxi_core::stats::Stats::default();
        stats.record(today.add_days(-2), 10, Millis(5_000));
        ipc.deliver(&Event::Push(Push::Stats(
            stats.summary(today, oxi_core::stats::HEAT_WEEKS),
        )));
        // A query typed on History before: the day click starts clean.
        let history = window.global::<HistoryState>();
        history.invoke_search_edited("old words".into());
        history.set_query("old words".into());
        let home = window.global::<HomeState>();
        home.invoke_open_day(home.get_today() - 2);
        assert_eq!(window.get_page(), Page::History);
        has(&window, "History");
        let day = today.add_days(-2);
        let asked = sent_now(&sent);
        assert!(
            asked.iter().any(|r| matches!(r,
                Request::HistorySearch { day: Some(d), query, .. }
                    if *d == day.to_string() && query.0.is_empty())),
            "{asked:?}"
        );
        assert_eq!(history.get_query(), "");
        assert_eq!(history.get_day(), super::super::home::day_label(day, today));
        // The typed query's rest timer is gone too: no late search with the old words.
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(400));
        assert!(!sent_now(&sent).iter().any(|r| matches!(r,
                Request::HistorySearch { query, .. } if !query.0.is_empty())),);
    }

    #[test]
    fn export_formats_cover_text_json_and_subtitles_that_follow_an_edited_text() {
        let heard = four(true);
        let meta = Meta {
            version: Meta::VERSION,
            started: "2026-11-08T13:45:01.123".into(),
            duration_ms: 2000,
            status: Status::Done,
            model: "turbo".into(),
            language: "ru".into(),
            settings: None,
            previous: None,
            words: heard.clone(),
            timings: None,
            raw: false,
            held: Vec::new(),
            delivered: None,
        };
        assert_eq!(
            exported(Format::Txt, SAID, None, &[]).as_deref(),
            Some(SAID)
        );
        let json = exported(Format::Json, SAID, Some(&meta), &[]).unwrap();
        assert!(
            json.contains("\"text\": \"alpha beta gamma delta\""),
            "{json}"
        );
        assert_eq!(
            exported(Format::Json, SAID, None, &[]),
            None,
            "no meta, no json"
        );
        // The text was edited (beta -> bravo): the cues carry the shown words with their times.
        let srt = exported(Format::Srt, "alpha bravo gamma delta", None, &heard).unwrap();
        assert!(srt.contains("alpha bravo"), "{srt}");
        assert!(srt.starts_with("1\n00:00:00,000 --> "), "{srt}");
        assert!(
            exported(Format::Vtt, SAID, None, &heard)
                .unwrap()
                .starts_with("WEBVTT")
        );
        // No times: no subtitles.
        assert_eq!(exported(Format::Srt, SAID, None, &four(false)), None);
        assert_eq!(exported(Format::Vtt, SAID, None, &[]), None);
    }

    #[test]
    fn the_export_menu_writes_the_picked_record_and_says_why_subtitles_are_off() {
        let (window, ipc, _sent, ctx) = page("export");
        let s = window.global::<HistoryState>();
        s.invoke_shown();
        listed(&ipc, vec![item(A, Status::Done, "alpha beta")], false);
        assert_eq!(s.get_times_off(), NO_TIMES);
        let dir = ctx.layout.data_dir.clone();
        let out = dir.join("out.txt");
        SAVED.set(Some(out.clone()));
        s.invoke_export(0);
        assert_eq!(fs::read_to_string(&out).unwrap(), "alpha beta");
        assert_eq!(window.global::<Toast>().get_text(), "Saved.");
        // No meta file on disk: the JSON carries the meta the list was given.
        let json = dir.join("out.json");
        SAVED.set(Some(json.clone()));
        s.invoke_export(1);
        let json = fs::read_to_string(&json).unwrap();
        assert!(json.contains("\"text\": \"alpha beta\""), "{json}");
        assert!(json.contains("\"status\": \"done\""), "{json}");
        // SRT with no word times writes nothing and tells why.
        let none = dir.join("out.srt");
        SAVED.set(Some(none.clone()));
        s.invoke_export(2);
        assert!(!none.exists());
        assert_eq!(window.global::<Toast>().get_text(), EXPORT_NO_TIMES);
        // A dialog that is cancelled writes nothing and says nothing new.
        s.invoke_export(0);
    }

    #[test]
    fn a_file_run_shows_real_progress_ends_with_its_result_and_can_be_stopped() {
        let (window, ipc, sent, _ctx) = page("file");
        let s = window.global::<HistoryState>();
        s.invoke_shown();
        sent_now(&sent);
        listed(&ipc, vec![], false);
        PICKED.set(Some(PathBuf::from("talk.mp3")));
        s.invoke_pick_file();
        let [Request::TranscribeFile { run, path, .. }] = &sent_now(&sent)[..] else {
            panic!("no file run");
        };
        let run = *run;
        assert_eq!(path, "talk.mp3");
        assert!(s.get_file_running());
        // One run at a time.
        PICKED.set(Some(PathBuf::from("other.mp3")));
        s.invoke_pick_file();
        assert_eq!(sent_now(&sent), []);
        ipc.deliver(&Event::Push(Push::TestProgress {
            id: run,
            percent: 40,
            est_ms: Some(1000),
        }));
        // The bar is the shared driver's, on the real clock like the other bars of this page.
        std::thread::sleep(Duration::from_millis(250));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(100));
        let shown = s.get_file_progress();
        assert!(shown > 0.0 && shown < 1.0, "{shown}");
        s.invoke_stop_file();
        assert_eq!(sent_now(&sent), [Request::TranscribeFileCancel { run }]);
        ipc.deliver(&Event::Push(Push::FileResult {
            run,
            result: FileResult::Cancelled,
        }));
        assert!(!s.get_file_running());
        // A failure names the format; a stale result changes nothing.
        PICKED.set(Some(PathBuf::from("talk.opus")));
        s.invoke_pick_file();
        let [Request::TranscribeFile { run, .. }] = sent_now(&sent)[..] else {
            panic!("no file run");
        };
        ipc.deliver(&Event::Push(Push::FileResult {
            run: run + 5,
            result: FileResult::Cancelled,
        }));
        assert!(s.get_file_running());
        ipc.deliver(&Event::Push(Push::FileResult {
            run,
            result: FileResult::Failed {
                why: FileFailure::Unsupported {
                    what: "Opus".into(),
                },
            },
        }));
        assert!(!s.get_file_running());
        assert!(
            window
                .global::<Toast>()
                .get_text()
                .starts_with("Opus files cannot be read")
        );
        // The result record arrives as an ordinary HistoryAdded.
        PICKED.set(Some(PathBuf::from("talk.wav")));
        s.invoke_pick_file();
        let [Request::TranscribeFile { run, .. }] = sent_now(&sent)[..] else {
            panic!("no file run");
        };
        ipc.deliver(&Event::Push(Push::HistoryAdded {
            item: item(A, Status::Done, "from a file"),
        }));
        ipc.deliver(&Event::Push(Push::FileResult {
            run,
            result: FileResult::Done { id: A.into() },
        }));
        assert_eq!(rows_len(&window), 1);
        assert!(!s.get_file_running());
    }
}
