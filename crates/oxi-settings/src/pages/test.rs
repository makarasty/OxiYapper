//! Test page: pick several recordings (new ones, or saved ones from history), each with the text
//! of what was said; run every settings set of a scenario over all of them, one request at a time
//! (`TestTranscribe` or `HistoryRetranscribe`, the next only after each `TestResult`); show the
//! word error percent of each set over all recordings, each recording's transcript, and apply the
//! best set.
//!
//! New recordings live in [`PageState::takes`] and, during a run, on the daemon's connection, in
//! memory only; closing the window drops them ([`PageState::forget`]) and ends the connection.
//! History recordings are read from their folder to be played and drawn.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak as RcWeak};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use oxi_core::audio::{AudioError, SAMPLE_RATE};
use oxi_core::config::Audio;
use oxi_core::history;
use oxi_core::ipc::{HistoryItem, HistoryOp, Push, Request, TestFailure, TestOutcome};
use oxi_core::machine::Notice;
use oxi_core::models;
use oxi_core::strings::{self, NO_MIC};
use oxi_core::tuning::{
    self, MAX_TEST_SAMPLES, PCM_CHUNK_SAMPLES, Profile, SettingsSet, WordErrors, encode_pcm,
    word_errors,
};
use oxi_core::types::Language;
use oxi_platform::capture::{Capture, CaptureError, CaptureEvent, CpalCapture, LevelMeter, play};
use slint::{
    CloseRequestResponse, ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode,
    VecModel, Weak,
};

use crate::generated::{TestHeard, TestRow, TestState, TestTake, Toast};
use crate::ipc::{Event, Link, Subscription};
use crate::{AppWindow, Ctx, Page, report};

use super::tuning as tuning_view;
use tuning_view::{BARS, clock, history_label, peaks, total};

/// An open recording: stops it and gives the samples. Dropped uncalled, it discards them.
pub type Finish = Box<dyn FnOnce() -> Result<Vec<f32>, CaptureError>>;
/// Opens the microphone for a recording; tests swap in one that needs no device. The flag goes up
/// when the device is gone or capture failed: the samples so far stay readable through [`Finish`].
pub type Mic = fn(&Audio, Arc<AtomicBool>) -> Result<Finish, CaptureError>;
/// Plays mono samples at a rate; dropping what it gives stops the sound. Tests swap in a silent one.
pub type Player = fn(&[f32], u32) -> Result<Box<dyn Any>, CaptureError>;

/// How often the elapsed time and the limits are looked at while recording. The clock counts
/// ticks: a tick a busy window skips makes it run a little late, and `stop` truncates the samples
/// to the engine's limit whatever the clock said.
const TICK: Duration = Duration::from_millis(200);
const TICKS_PER_SECOND: usize = 5;
const MAX_TICKS: usize = MAX_TEST_SAMPLES / SAMPLE_RATE as usize * TICKS_PER_SECOND;
/// Playback ends this long after the clip: the device's buffer is still sounding.
const PLAY_TAIL: Duration = Duration::from_millis(300);

const RECORD_TIP: &str = "Recordings here use the microphone settings from Audio. A new one gets a ready phrase to read aloud; edit its text if you said something else.";
const NO_READY_PHRASES: &str = "No ready phrases in this language: write what you said next to each recording. Recordings here use the microphone settings from Audio.";
const PICK: &str = "Pick a recording to add";
const NO_HISTORY: &str = "No recordings in history";
const PICKED_ALL: &str = "Every history recording is picked";
const NOT_FOUND: &str = "The recording file is gone";
const OFF_RUNNING: &str = "The test is running";
const OFF_RECORDING: &str = "Stop the recording first";
const OFF_NO_TAKES: &str = "Add a recording first";
const OFF_NO_TEXT: &str = "Write what was said in every recording";
const OFF_NEEDS_DAEMON: &str = "Start OxiYapper to run the test";
const OFF_NO_BEST: &str = "Run the test first";
const WAITING: &str = "Waiting";
const RUNNING: &str = "Running…";
const NOT_RUN: &str = "Not run";
const TOO_LONG: &str = "Recording stopped at 5 minutes.";
const BUSY: &str = "OxiYapper is dictating. Run the test when it is free.";
const LAST_TEST_BUSY: &str = "The last test is still finishing. Run the test in a moment.";
const NO_AUDIO: &str = "OxiYapper did not get the recording. Run the test again.";
const LINK_LOST: &str = "OxiYapper stopped answering, so the test ended.";
const NOTHING_HEARD: &str = "No set heard every recording, so there is nothing to apply.";
const APPLIED: &str = "Best set applied.";
const HISTORY_ON: &str = "History is on. New dictations are kept.";

fn profile_name(profile: Profile) -> &'static str {
    match profile {
        Profile::General => "General",
        Profile::FastSpeech => "Fast speech",
        Profile::BadMicrophone => "Bad microphone",
        Profile::LongDictation => "Long dictation",
        Profile::SeveralPasses => "Several passes",
    }
}

/// The raw audio the daemon wants: before gain, normalise, noise suppression and trim.
fn open_mic(opts: &Audio, lost: Arc<AtomicBool>) -> Result<Finish, CaptureError> {
    let raw = Audio {
        gain_db: 0.0,
        normalize: false,
        denoise: false,
        trim: false,
        ..opts.clone()
    };
    let capture = CpalCapture::open(&raw, LevelMeter::default(), move |_: CaptureEvent| {
        lost.store(true, Ordering::Relaxed);
    })?;
    Ok(Box::new(move || capture.finish()))
}

fn play_cpal(clip: &[f32], rate: u32) -> Result<Box<dyn Any>, CaptureError> {
    Ok(Box::new(play(clip, rate)?))
}

/// Once per window: a config reload only calls `shown`, so the recordings and the run outlive it.
pub fn bind(window: &AppWindow, ctx: &Ctx) {
    bind_with(window, ctx, open_mic, play_cpal);
}

/// [`bind`] on another microphone and speaker. Binding again starts the page over, and the
/// recordings go.
pub fn bind_with(window: &AppWindow, ctx: &Ctx, mic: Mic, player: Player) {
    let s = window.global::<TestState>();
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
        mic,
        player,
        takes: RefCell::new(Vec::new()),
        cards: Rc::new(VecModel::default()),
        new_count: Cell::new(0),
        open: RefCell::new(None),
        lost: Arc::new(AtomicBool::new(false)),
        ticks: Cell::new(0),
        timer: Timer::default(),
        playing: RefCell::new(None),
        play_timer: Timer::default(),
        history: RefCell::new(Vec::new()),
        offered: RefCell::new(Vec::new()),
        offered_model: Rc::new(VecModel::default()),
        run: RefCell::new(None),
        left_out: Cell::new(None),
        rows: Rc::new(VecModel::default()),
        heard: Rc::new(VecModel::default()),
        results: RefCell::new(Results::default()),
        best: RefCell::new(None),
        next_id: Cell::new(1),
    });
    let profiles: Vec<SharedString> = Profile::ALL
        .iter()
        .map(|p| profile_name(*p).into())
        .collect();
    s.set_profiles(ModelRc::from(Rc::new(VecModel::from(profiles))));
    s.set_profile(0);
    s.set_view(0);
    s.set_takes(ModelRc::from(page.cards.clone()));
    s.set_history(ModelRc::from(page.offered_model.clone()));
    s.set_history_pick(0);
    s.set_playing(-1);
    s.set_running(false);
    s.set_rows(ModelRc::from(page.rows.clone()));
    s.set_heard(ModelRc::from(page.heard.clone()));
    s.set_selected(0);
    s.set_best_text(SharedString::new());
    s.set_apply_off(OFF_NO_BEST.into());
    page.refresh(&s);

    let p = Rc::clone(&page);
    s.on_shown(move || p.shown());
    let p = Rc::clone(&page);
    s.on_profile_picked(move || p.state(|s| p.refresh(s)));
    let p = Rc::clone(&page);
    s.on_record(move || p.record());
    let p = Rc::clone(&page);
    s.on_add_history(move || p.add_history());
    let p = Rc::clone(&page);
    s.on_turn_on_history(move || p.turn_on_history());
    let p = Rc::clone(&page);
    s.on_play(move |i| p.play(i));
    let p = Rc::clone(&page);
    s.on_remove(move |i| p.remove(i));
    let p = Rc::clone(&page);
    s.on_reference_edited(move |i, text| p.reference_edited(i, &text));
    let p = Rc::clone(&page);
    s.on_run(move || p.start_run());
    let p = Rc::clone(&page);
    s.on_select(move |i| p.select(i));
    let p = Rc::clone(&page);
    s.on_apply(move || p.apply());
    let weak = Rc::downgrade(&page);
    window.window().on_close_requested(move || {
        if let Some(page) = weak.upgrade() {
            page.forget();
        }
        CloseRequestResponse::HideWindow
    });
}

/// Where a picked recording comes from.
#[derive(Clone)]
enum Source {
    /// Recorded on this page: sent to the daemon with `TestAudio`.
    New,
    /// A history entry, by id: the daemon reads it from its folder.
    History(String),
}

/// A picked recording.
struct Take {
    source: Source,
    /// "New recording 2", or when a history recording was made.
    label: String,
    /// What was said.
    reference: String,
    /// Mono samples at `rate`, to play and draw (and for a new one, to send); empty while it
    /// records, or when a history file could not be read.
    audio: Rc<Vec<f32>>,
    rate: u32,
    duration_ms: u64,
}

/// How one recording went under one set.
#[derive(Clone)]
enum Answer {
    Waiting,
    Running,
    Heard {
        errors: WordErrors,
        text: String,
        ms: u64,
    },
    Failed(&'static str),
    NotRun,
}

/// The last run's table: the sets, the recordings' labels, and `cells[set][take]`.
#[derive(Default)]
struct Results {
    sets: Vec<SettingsSet>,
    labels: Vec<String>,
    cells: Vec<Vec<Answer>>,
}

/// A run in progress: one request is out with the daemon, the rest wait for its answer. Jobs go
/// set by set, every recording under each set, so each set's total comes in as soon as it can.
struct Run {
    /// The request out.
    id: u32,
    /// The job out: set `job / takes.len()`, recording `job % takes.len()`.
    job: usize,
    takes: Vec<(Source, Rc<Vec<f32>>, String)>,
    /// The new recording whose audio the daemon holds.
    ///
    /// shortcut: with several new recordings each set sends each one again; keep them per id on the
    /// daemon if long recordings make that slow.
    held: Option<usize>,
    language: Language,
}

struct PageState {
    me: RcWeak<Self>,
    _events: Subscription,
    window: Weak<AppWindow>,
    ctx: Ctx,
    mic: Mic,
    player: Player,
    takes: RefCell<Vec<Take>>,
    cards: Rc<VecModel<TestTake>>,
    /// New recordings made, for their names and their ready phrases.
    new_count: Cell<usize>,
    /// The recording being made: it is the last take.
    open: RefCell<Option<Finish>>,
    /// Up when the microphone of the open recording is gone; set from a capture thread.
    lost: Arc<AtomicBool>,
    ticks: Cell<usize>,
    timer: Timer,
    /// The take that plays and its sound.
    playing: RefCell<Option<(usize, Box<dyn Any>)>>,
    play_timer: Timer,
    /// The history as the daemon listed it, newest first.
    history: RefCell<Vec<HistoryItem>>,
    /// The ids in the picker: the history not picked yet.
    offered: RefCell<Vec<String>>,
    offered_model: Rc<VecModel<SharedString>>,
    run: RefCell<Option<Run>>,
    /// The request a closed window left with the daemon, until its answer comes.
    left_out: Cell<Option<u32>>,
    rows: Rc<VecModel<TestRow>>,
    heard: Rc<VecModel<TestHeard>>,
    results: RefCell<Results>,
    best: RefCell<Option<SettingsSet>>,
    next_id: Cell<u32>,
}

impl PageState {
    fn state(&self, f: impl FnOnce(&TestState<'_>)) {
        if let Some(window) = self.window.upgrade() {
            f(&window.global::<TestState>());
        }
    }

    fn toast(&self, text: &str) {
        if let Some(window) = self.window.upgrade() {
            window.global::<Toast>().invoke_show(text.into());
        }
    }

    /// The page was opened or the config reloaded: the language or the history setting may have
    /// changed.
    fn shown(&self) {
        self.list_history();
        self.state(|s| self.refresh(s));
    }

    fn history_on(&self) -> bool {
        self.ctx.config.borrow().config.history.enabled
    }

    /// Asks the daemon for the newest history recordings, while this page is open: a reload or a
    /// reconnect with another page open asks nothing.
    ///
    /// shortcut: only the first page the daemon gives; ask with `older_than` when people pick
    /// recordings older than that.
    fn list_history(&self) {
        let open = self
            .window
            .upgrade()
            .is_some_and(|w| w.get_page() == Page::Test);
        if open && self.history_on() && self.ctx.ipc.link() == Link::Online {
            self.ctx.ipc.send(Request::HistoryList { older_than: None });
        }
    }

    fn phrases(&self) -> &'static [&'static str] {
        tuning::phrases(self.ctx.config.borrow().config.model.language)
    }

    /// Everything that follows from the recordings, the run and the link.
    fn refresh(&self, s: &TestState<'_>) {
        let recording = self.open.borrow().is_some();
        let running = self.run.borrow().is_some();
        let takes = self.takes.borrow();
        s.set_recording(recording);
        s.set_running(running);
        s.set_record_tip(
            if self.phrases().is_empty() {
                NO_READY_PHRASES
            } else {
                RECORD_TIP
            }
            .into(),
        );
        s.set_record_off(if running { OFF_RUNNING } else { "" }.into());
        s.set_history_on(self.history_on());
        s.set_add_off(
            if running {
                OFF_RUNNING
            } else if recording {
                OFF_RECORDING
            } else if self.offered.borrow().is_empty() {
                if self.history.borrow().is_empty() {
                    NO_HISTORY
                } else {
                    PICKED_ALL
                }
            } else {
                ""
            }
            .into(),
        );
        let off = if running {
            OFF_RUNNING
        } else if recording {
            OFF_RECORDING
        } else if takes.is_empty() {
            OFF_NO_TAKES
        } else if takes
            .iter()
            .any(|t| word_errors(&t.reference, "").words == 0)
        {
            // Scored over no words, heard text is 100 % and silence 0 %: Apply would prefer silence.
            OFF_NO_TEXT
        } else {
            self.ctx.ipc.link().off_reason(OFF_NEEDS_DAEMON)
        };
        s.set_run_off(off.into());
        s.set_playing(
            self.playing
                .borrow()
                .as_ref()
                .and_then(|(i, _)| i32::try_from(*i).ok())
                .unwrap_or(-1),
        );
    }

    fn profile(s: &TestState<'_>) -> Profile {
        usize::try_from(s.get_profile())
            .ok()
            .and_then(|i| Profile::ALL.get(i))
            .copied()
            .unwrap_or(Profile::General)
    }

    /// The card of take `i`, ready to show.
    fn card(&self, take: &Take, recording: bool) -> TestTake {
        let duration = if recording {
            format!(
                "Recording {}",
                clock(self.ticks.get() as u64 / TICKS_PER_SECOND as u64 * 1000)
            )
        } else {
            clock(take.duration_ms)
        };
        let play_off = if recording {
            OFF_RECORDING
        } else if take.audio.is_empty() {
            NOT_FOUND
        } else {
            ""
        };
        let bars: Vec<f32> = if take.audio.is_empty() {
            Vec::new()
        } else {
            peaks(&take.audio, BARS)
        };
        TestTake {
            source: take.label.as_str().into(),
            duration: duration.into(),
            peaks: ModelRc::from(Rc::new(VecModel::from(bars))),
            reference: take.reference.as_str().into(),
            recording,
            play_off: play_off.into(),
        }
    }

    /// Draws take `i` again.
    fn redraw(&self, i: usize) {
        let takes = self.takes.borrow();
        let Some(take) = takes.get(i) else {
            return;
        };
        let recording = self.open.borrow().is_some() && i + 1 == takes.len();
        self.cards.set_row_data(i, self.card(take, recording));
    }

    fn push_take(&self, take: Take, recording: bool) {
        self.cards.push(self.card(&take, recording));
        self.takes.borrow_mut().push(take);
    }

    /// The Record and Stop button.
    fn record(&self) {
        if self.open.borrow().is_some() {
            self.stop(true);
        } else if self.run.borrow().is_none() {
            self.start_recording();
        }
        self.state(|s| self.refresh(s));
    }

    fn start_recording(&self) {
        let opts = self.ctx.config.borrow().config.audio.clone();
        self.lost.store(false, Ordering::Relaxed);
        match (self.mic)(&opts, Arc::clone(&self.lost)) {
            Ok(finish) => {
                self.stop_playing();
                *self.open.borrow_mut() = Some(finish);
                self.ticks.set(0);
                let n = self.new_count.get();
                self.new_count.set(n + 1);
                let phrases = self.phrases();
                let reference = if phrases.is_empty() {
                    String::new()
                } else {
                    phrases[n % phrases.len()].to_owned()
                };
                self.push_take(
                    Take {
                        source: Source::New,
                        label: format!("New recording {}", n + 1),
                        reference,
                        audio: Rc::new(Vec::new()),
                        rate: SAMPLE_RATE,
                        duration_ms: 0,
                    },
                    true,
                );
                let me = self.me.clone();
                self.timer.start(TimerMode::Repeated, TICK, move || {
                    if let Some(page) = me.upgrade() {
                        page.tick();
                    }
                });
            }
            Err(e) => self.toast(&failure(&e)),
        }
    }

    /// A lost microphone, leaving the page or the 5 minute limit ends the recording and keeps what
    /// it got.
    fn tick(&self) {
        let ticks = self.ticks.get() + 1;
        self.ticks.set(ticks);
        let shown = self
            .window
            .upgrade()
            .is_some_and(|w| w.get_page() == Page::Test && w.window().is_visible());
        if self.lost.load(Ordering::Relaxed) {
            // First: a recording that cannot be read says why in the toast of `stop`.
            self.toast(&format!(
                "{}. What was recorded is kept.",
                strings::notice(Notice::DeviceLost)
            ));
            self.stop(true);
        } else if !shown {
            self.stop(true);
        } else if ticks >= MAX_TICKS {
            self.stop(true);
            self.toast(TOO_LONG);
        } else if ticks.is_multiple_of(TICKS_PER_SECOND) {
            let last = self.takes.borrow().len().saturating_sub(1);
            self.redraw(last);
        }
        self.state(|s| self.refresh(s));
    }

    /// Ends the recording; `keep` holds on to its samples, else its take goes.
    fn stop(&self, keep: bool) {
        self.timer.stop();
        let Some(finish) = self.open.borrow_mut().take() else {
            return;
        };
        let last = self.takes.borrow().len().saturating_sub(1);
        let samples = if keep {
            finish().map_err(|e| self.toast(&failure(&e))).ok()
        } else {
            None
        };
        match samples.filter(|s| !s.is_empty()) {
            Some(mut samples) => {
                samples.truncate(MAX_TEST_SAMPLES);
                if let Some(take) = self.takes.borrow_mut().get_mut(last) {
                    take.duration_ms = samples.len() as u64 * 1000 / u64::from(SAMPLE_RATE);
                    take.audio = Rc::new(samples);
                }
                self.redraw(last);
            }
            None => self.remove_take(last),
        }
    }

    /// Adds the history recording picked in the picker.
    fn add_history(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let s = window.global::<TestState>();
        self.refresh(&s);
        if s.get_add_off() != "" {
            return;
        }
        // Line 0 is the prompt.
        let id = usize::try_from(s.get_history_pick() - 1)
            .ok()
            .and_then(|i| self.offered.borrow().get(i).cloned());
        s.set_history_pick(0);
        let item = id.and_then(|id| {
            self.history
                .borrow()
                .iter()
                .find(|item| item.id == id)
                .cloned()
        });
        let Some(item) = item else {
            return;
        };
        let (audio, rate) = self
            .read_history(&item.id)
            .unwrap_or((Vec::new(), SAMPLE_RATE));
        self.push_take(
            Take {
                source: Source::History(item.id.clone()),
                label: tuning_view::when(&item.id),
                reference: item.text.0.trim().to_owned(),
                audio: Rc::new(audio),
                rate,
                duration_ms: item.duration_ms,
            },
            false,
        );
        self.offer();
        self.refresh(&s);
    }

    /// The samples of history recording `id`, to play and draw; `None` when it cannot be read.
    fn read_history(&self, id: &str) -> Option<(Vec<f32>, u32)> {
        if !history::is_id(id) {
            return None;
        }
        let dir = {
            let config = self.ctx.config.borrow();
            history::folder(&config.config.history, &self.ctx.layout.history_dir())
        };
        oxi_platform::wav::read_wav_mono_f32(&dir.join(format!("{id}.wav"))).ok()
    }

    /// Fills the picker with the history not picked yet.
    fn offer(&self) {
        let picked: Vec<String> = self
            .takes
            .borrow()
            .iter()
            .filter_map(|t| match &t.source {
                Source::History(id) => Some(id.clone()),
                Source::New => None,
            })
            .collect();
        let (ids, labels): (Vec<String>, Vec<SharedString>) = self
            .history
            .borrow()
            .iter()
            .filter(|item| !picked.contains(&item.id))
            .map(|item| (item.id.clone(), history_label(item).into()))
            .unzip();
        if *self.offered.borrow() != ids {
            let lines = if ids.is_empty() {
                Vec::new()
            } else {
                std::iter::once(PICK.into()).chain(labels).collect()
            };
            *self.offered.borrow_mut() = ids;
            self.offered_model.set_vec(lines);
            self.state(|s| s.set_history_pick(0));
        }
    }

    /// The daemon listed history: newer pages and older pages both join one list, newest first.
    fn listed(&self, items: &[HistoryItem]) {
        {
            let mut history = self.history.borrow_mut();
            for item in items {
                match history.iter_mut().find(|h| h.id == item.id) {
                    Some(held) => *held = item.clone(),
                    None => history.push(item.clone()),
                }
            }
            // Ids are `YYYY-MM-DD/HHMMSS-mmm`: they sort by time.
            history.sort_by(|a, b| b.id.cmp(&a.id));
        }
        self.offer();
        self.state(|s| self.refresh(s));
    }

    fn turn_on_history(&self) {
        self.ctx.config.borrow_mut().config.history.enabled = true;
        let saved = self.ctx.save();
        if saved.is_ok() {
            self.toast(HISTORY_ON);
            self.list_history();
        }
        report(&self.window, saved);
        self.state(|s| self.refresh(s));
    }

    /// Play or Stop on take `i`.
    fn play(&self, i: i32) {
        let Ok(i) = usize::try_from(i) else {
            return;
        };
        let again = self
            .playing
            .borrow()
            .as_ref()
            .is_some_and(|(at, _)| *at == i);
        self.stop_playing();
        if !again {
            let takes = self.takes.borrow();
            let Some(take) = takes.get(i).filter(|t| !t.audio.is_empty()) else {
                return;
            };
            match (self.player)(&take.audio, take.rate) {
                Ok(sound) => {
                    *self.playing.borrow_mut() = Some((i, sound));
                    let length = Duration::from_millis(
                        take.duration_ms
                            .max(take.audio.len() as u64 * 1000 / u64::from(take.rate.max(1))),
                    );
                    let me = self.me.clone();
                    self.play_timer
                        .start(TimerMode::SingleShot, length + PLAY_TAIL, move || {
                            if let Some(page) = me.upgrade() {
                                page.stop_playing();
                                page.state(|s| page.refresh(s));
                            }
                        });
                }
                Err(e) => self.toast(&failure(&e)),
            }
        }
        self.state(|s| self.refresh(s));
    }

    fn stop_playing(&self) {
        self.play_timer.stop();
        self.playing.borrow_mut().take();
    }

    fn remove(&self, i: i32) {
        let Ok(i) = usize::try_from(i) else {
            return;
        };
        let recording = self.open.borrow().is_some() && i + 1 == self.takes.borrow().len();
        if self.run.borrow().is_some() || recording {
            return;
        }
        self.remove_take(i);
        self.state(|s| self.refresh(s));
    }

    fn remove_take(&self, i: usize) {
        if i >= self.takes.borrow().len() {
            return;
        }
        let playing = self.playing.borrow().as_ref().map(|(at, _)| *at);
        match playing {
            Some(at) if at == i => self.stop_playing(),
            Some(at) if at > i => {
                if let Some(p) = self.playing.borrow_mut().as_mut() {
                    p.0 -= 1;
                }
            }
            _ => {}
        }
        self.takes.borrow_mut().remove(i);
        self.cards.remove(i);
        self.offer();
    }

    fn reference_edited(&self, i: i32, text: &str) {
        let Ok(i) = usize::try_from(i) else {
            return;
        };
        if let Some(take) = self.takes.borrow_mut().get_mut(i) {
            text.clone_into(&mut take.reference);
        }
        // The card keeps the text, so a removal above hands the right one down.
        if let Some(mut card) = self.cards.row_data(i) {
            card.reference = text.into();
            self.cards.set_row_data(i, card);
        }
        self.state(|s| self.refresh(s));
    }

    /// The window is closing: the new recordings go, here and in an open one.
    fn forget(&self) {
        self.stop(false);
        self.stop_playing();
        if let Some(run) = self.run.borrow_mut().take() {
            self.left_out.set(Some(run.id));
            self.mark_not_run(&run);
        }
        let mut i = 0;
        while i < self.takes.borrow().len() {
            if matches!(self.takes.borrow()[i].source, Source::New) {
                self.remove_take(i);
            } else {
                i += 1;
            }
        }
        self.state(|s| self.refresh(s));
    }

    /// Run: the jobs of every set over every recording, one at a time.
    fn start_run(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let s = window.global::<TestState>();
        self.refresh(&s);
        if s.get_run_off() != "" {
            return;
        }
        self.stop_playing();
        let sets = Self::profile(&s).sets();
        let takes: Vec<(Source, Rc<Vec<f32>>, String)> = self
            .takes
            .borrow()
            .iter()
            .map(|t| (t.source.clone(), Rc::clone(&t.audio), t.reference.clone()))
            .collect();
        *self.results.borrow_mut() = Results {
            cells: vec![vec![Answer::Waiting; takes.len()]; sets.len()],
            labels: self
                .takes
                .borrow()
                .iter()
                .map(|t| t.label.clone())
                .collect(),
            sets: sets.clone(),
        };
        self.rows
            .set_vec(sets.iter().map(row).collect::<Vec<TestRow>>());
        *self.best.borrow_mut() = None;
        s.set_selected(0);
        s.set_best_text(SharedString::new());
        s.set_apply_off(OFF_NO_BEST.into());
        s.set_progress(0.0);
        s.set_progress_text("0 %".into());
        s.set_view(1);
        let run = Run {
            id: 0,
            job: 0,
            takes,
            held: None,
            language: self.ctx.config.borrow().config.model.language,
        };
        self.send_job(run);
    }

    /// Sends the job `run.job`; with none left the run is over.
    fn send_job(&self, mut run: Run) {
        let n = run.takes.len();
        let sets = self.results.borrow().sets.clone();
        if n == 0 || run.job >= sets.len() * n {
            self.finish_run();
            return;
        }
        let (set, take) = (run.job / n, run.job % n);
        run.id = self.next_id.get();
        self.next_id.set(run.id.wrapping_add(1));
        let (source, audio, _) = &run.takes[take];
        let request = match source {
            Source::New => {
                if run.held != Some(take) {
                    for (i, piece) in audio.chunks(PCM_CHUNK_SAMPLES).enumerate() {
                        self.ctx.ipc.send(Request::TestAudio {
                            reset: i == 0,
                            pcm: encode_pcm(piece),
                        });
                    }
                    run.held = Some(take);
                }
                Request::TestTranscribe {
                    id: run.id,
                    set: sets[set].clone(),
                    language: run.language,
                }
            }
            Source::History(id) => Request::HistoryRetranscribe {
                run: run.id,
                id: id.clone(),
                set: sets[set].clone(),
                language: run.language,
            },
        };
        self.ctx.ipc.send(request);
        self.set_cell(set, take, Answer::Running);
        self.progress(&run, 0);
        *self.run.borrow_mut() = Some(run);
        self.state(|s| self.refresh(s));
    }

    fn set_cell(&self, set: usize, take: usize, cell: Answer) {
        if let Some(c) = self
            .results
            .borrow_mut()
            .cells
            .get_mut(set)
            .and_then(|row| row.get_mut(take))
        {
            *c = cell;
        }
        self.show_row(set);
        self.state(|s| {
            if usize::try_from(s.get_selected()) == Ok(set) {
                self.show_heard(set);
            }
        });
    }

    /// The table line of `set` from its cells.
    fn show_row(&self, set: usize) {
        let Some(mut line) = self.rows.row_data(set) else {
            return;
        };
        let results = self.results.borrow();
        let cells = &results.cells[set];
        let failed = cells.iter().find_map(|c| match c {
            Answer::Failed(why) => Some(*why),
            _ => None,
        });
        let heard: Vec<Option<WordErrors>> = cells
            .iter()
            .map(|c| match c {
                Answer::Heard { errors, .. } => Some(*errors),
                _ => None,
            })
            .collect();
        let (errors, time) = if let Some(why) = failed {
            (why.to_owned(), String::new())
        } else if cells.iter().any(|c| matches!(c, Answer::NotRun)) {
            (NOT_RUN.to_owned(), String::new())
        } else if let Some(sum) = total(&heard) {
            let ms = cells
                .iter()
                .map(|c| match c {
                    Answer::Heard { ms, .. } => *ms,
                    _ => 0,
                })
                .sum();
            (percent(sum), seconds(ms))
        } else if cells
            .iter()
            .any(|c| matches!(c, Answer::Running | Answer::Heard { .. }))
        {
            (RUNNING.to_owned(), String::new())
        } else {
            (WAITING.to_owned(), String::new())
        };
        line.errors = errors.into();
        line.time = time.into();
        self.rows.set_row_data(set, line);
    }

    /// The recordings under `set`, below the table.
    fn show_heard(&self, set: usize) {
        let results = self.results.borrow();
        let Some(cells) = results.cells.get(set) else {
            self.heard.set_vec(Vec::new());
            return;
        };
        let cards: Vec<TestHeard> = cells
            .iter()
            .zip(&results.labels)
            .map(|(cell, label)| {
                let (errors, text) = match cell {
                    Answer::Waiting => (WAITING.to_owned(), ""),
                    Answer::Running => (RUNNING.to_owned(), ""),
                    Answer::Heard { errors, text, .. } => (percent(*errors), text.as_str()),
                    Answer::Failed(why) => ((*why).to_owned(), ""),
                    Answer::NotRun => (NOT_RUN.to_owned(), ""),
                };
                TestHeard {
                    source: label.as_str().into(),
                    errors: errors.into(),
                    text: text.into(),
                }
            })
            .collect();
        self.heard.set_vec(cards);
    }

    fn select(&self, i: i32) {
        let Ok(set) = usize::try_from(i) else {
            return;
        };
        self.state(|s| s.set_selected(i));
        self.show_heard(set);
    }

    /// The live percent of the whole run: the jobs done and `percent` of the one out.
    fn progress(&self, run: &Run, percent: u8) {
        let jobs = self.results.borrow().sets.len() * run.takes.len();
        #[expect(clippy::cast_precision_loss, reason = "job counts are tiny")]
        let done = (run.job as f32 + f32::from(percent.min(100)) / 100.0) / jobs.max(1) as f32;
        self.state(|s| {
            s.set_progress(done);
            s.set_progress_text(format!("{:.0} %", done * 100.0).into());
        });
    }

    fn event(&self, event: &Event) {
        match event {
            Event::Push(Push::TestResult { id, outcome }) => self.answered(*id, outcome),
            Event::Push(Push::TestProgress { id, percent }) => {
                if let Some(run) = self.run.borrow().as_ref().filter(|r| r.id == *id) {
                    self.progress(run, *percent);
                }
            }
            Event::Push(Push::History { items, .. }) => self.listed(items),
            Event::Push(Push::HistoryResult {
                id,
                op: HistoryOp::Delete,
                ok: true,
            }) => {
                self.history.borrow_mut().retain(|item| item.id != *id);
                self.offer();
                self.state(|s| self.refresh(s));
            }
            Event::Link(link) => {
                if *link == Link::Online {
                    self.list_history();
                } else {
                    self.left_out.set(None);
                    self.abort_run(LINK_LOST);
                }
                self.state(|s| self.refresh(s));
            }
            Event::Push(_) => {}
        }
    }

    /// The daemon answered the job that is out: score it, or stop on a failure of the whole run.
    fn answered(&self, id: u32, outcome: &TestOutcome) {
        if self.left_out.get() == Some(id) {
            self.left_out.set(None);
            return;
        }
        let run = self.run.borrow_mut().take_if(|run| run.id == id);
        let Some(mut run) = run else {
            return;
        };
        let n = run.takes.len();
        let (set, take) = (run.job / n, run.job % n);
        let cell = match outcome {
            TestOutcome::Heard { text, ms } => Answer::Heard {
                errors: word_errors(&run.takes[take].2, text),
                text: text.clone(),
                ms: *ms,
            },
            TestOutcome::Failed {
                why: TestFailure::Busy,
            } => {
                *self.run.borrow_mut() = Some(run);
                // The daemon runs one test per connection and says Busy for that too.
                self.abort_run(if self.left_out.get().is_some() {
                    LAST_TEST_BUSY
                } else {
                    BUSY
                });
                return;
            }
            TestOutcome::Failed {
                why: TestFailure::NoAudio,
            } => {
                if matches!(run.takes[take].0, Source::History(_)) {
                    Answer::Failed(NOT_FOUND)
                } else {
                    *self.run.borrow_mut() = Some(run);
                    self.abort_run(NO_AUDIO);
                    return;
                }
            }
            TestOutcome::Failed { why } => Answer::Failed(failed(*why)),
        };
        self.set_cell(set, take, cell);
        run.job += 1;
        self.send_job(run);
    }

    /// Marks every job of `run` not answered yet.
    fn mark_not_run(&self, run: &Run) {
        let n = run.takes.len();
        let jobs = self.results.borrow().sets.len() * n;
        for job in run.job..jobs {
            self.set_cell(job / n, job % n, Answer::NotRun);
        }
    }

    /// Ends the run early: what was not answered is marked, the answers so far stay.
    fn abort_run(&self, why: &str) {
        let Some(run) = self.run.borrow_mut().take() else {
            return;
        };
        self.mark_not_run(&run);
        self.toast(why);
        self.finish_run();
    }

    /// Finds the best set among the ones that heard every recording.
    fn finish_run(&self) {
        *self.run.borrow_mut() = None;
        let scored: Vec<(usize, SettingsSet, WordErrors)> = {
            let results = self.results.borrow();
            results
                .cells
                .iter()
                .enumerate()
                .filter_map(|(i, cells)| {
                    let heard: Vec<Option<WordErrors>> = cells
                        .iter()
                        .map(|c| match c {
                            Answer::Heard { errors, .. } => Some(*errors),
                            _ => None,
                        })
                        .collect();
                    total(&heard).map(|sum| (i, results.sets[i].clone(), sum))
                })
                .collect()
        };
        let pairs: Vec<(SettingsSet, WordErrors)> = scored
            .iter()
            .map(|(_, set, errors)| (set.clone(), *errors))
            .collect();
        let best = tuning::best(&pairs).map(|i| scored[i].clone());
        self.state(|s| {
            if let Some((row, _, errors)) = &best {
                if let Some(mut line) = self.rows.row_data(*row) {
                    line.best = true;
                    self.rows.set_row_data(*row, line);
                }
                s.set_best_text(format!("Lowest error rate: {}", percent(*errors)).into());
                s.set_apply_off(SharedString::new());
                s.set_selected(i32::try_from(*row).unwrap_or(0));
                self.show_heard(*row);
            } else {
                s.set_best_text(NOTHING_HEARD.into());
                s.set_apply_off(NOTHING_HEARD.into());
            }
            s.set_progress(1.0);
            self.refresh(s);
        });
        *self.best.borrow_mut() = best.map(|(_, set, _)| set);
    }

    /// Apply best: writes the set to the config, as the Audio and Model pages would.
    fn apply(&self) {
        let Some(set) = self.best.borrow().clone() else {
            return;
        };
        {
            let mut file = self.ctx.config.borrow_mut();
            file.config.audio.normalize = set.normalize;
            file.config.audio.denoise = set.denoise;
            file.config.audio.trim = set.trim;
            file.config.model.id = set.model;
        }
        let saved = self.ctx.save();
        if saved.is_ok() {
            self.toast(APPLIED);
        }
        report(&self.window, saved);
    }
}

/// A results line for `set`, not run yet.
fn row(set: &SettingsSet) -> TestRow {
    let on = |value: bool| SharedString::from(if value { "On" } else { "Off" });
    TestRow {
        model: models::find(&set.model)
            .map_or(set.model.as_str(), |entry| entry.display_name)
            .into(),
        normalize: on(set.normalize),
        denoise: on(set.denoise),
        trim: on(set.trim),
        errors: WAITING.into(),
        time: SharedString::new(),
        best: false,
    }
}

fn percent(errors: WordErrors) -> String {
    format!("{:.1} %", errors.percent())
}

#[expect(clippy::cast_precision_loss, reason = "a run is far below 2^52 ms")]
fn seconds(ms: u64) -> String {
    format!("{:.1} s", ms as f64 / 1000.0)
}

/// How a run of one recording that gives no text shows.
fn failed(why: TestFailure) -> &'static str {
    match why {
        TestFailure::ModelMissing => "Not downloaded",
        TestFailure::ModelBad => "Model failed",
        TestFailure::NoAudio | TestFailure::Busy | TestFailure::Failed => "Failed",
    }
}

fn failure(e: &CaptureError) -> String {
    match e {
        CaptureError::NoDevice => NO_MIC.to_owned(),
        CaptureError::Audio(AudioError::MicSilence) => strings::no_signal("the microphone"),
        e => strings::mic_test_failed(&e.to_string()),
    }
}
