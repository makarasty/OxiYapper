//! Test page: a three-step wizard (goal, models, Run; owner 2026-11-13, hold 29-32). The
//! recordings are a few recent ones from History of varied length, or the ones picked by hand on
//! the Recordings view (new ones, or saved ones from history), each with the text of what was
//! said. Every set of the goal goes over all of them, one request at a time (`TestTranscribe` or
//! `HistoryRetranscribe`, the next only after each `TestResult`); the quiet-microphone goal hears
//! each recording again made quieter in software (sent as `TestAudio`). The results show the word
//! error percent of each set over all recordings, each recording's transcript, Apply best, and the
//! offers to download the recommended model the run skipped or delete the models that lost.
//!
//! New recordings live in [`PageState::takes`] and, during a run, on the daemon's connection, in
//! memory only; closing the window drops them ([`PageState::forget`]) and ends the connection.
//! History recordings are read from their folder to be played and drawn.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak as RcWeak};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use oxi_core::audio::{AudioError, SAMPLE_RATE};
use oxi_core::config::Audio;
use oxi_core::device::Device;
use oxi_core::history::{self, HistoryId, Status};
use oxi_core::ipc::{HistoryItem, HistoryOp, Push, Request, TestFailure, TestOutcome};
use oxi_core::machine::Notice;
use oxi_core::models::{self, Adapter, MODELS, Machine, ModelEntry, ModelStatus};
use oxi_core::strings::{self, NO_MIC, gb};
use oxi_core::tuning::{
    self, Goal, MAX_TEST_SAMPLES, ModelChoice, ModelPreset, PCM_CHUNK_SAMPLES, PICK_COUNT,
    SettingsSet, WordErrors, encode_pcm, word_errors,
};
use oxi_core::types::{Language, Millis};
use oxi_platform::capture::{Capture, CaptureError, CaptureEvent, CpalCapture, LevelMeter, play};
use oxi_platform::sysinfo;
use slint::{
    CloseRequestResponse, ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode,
    VecModel, Weak,
};

use crate::generated::{ModelState, TestHeard, TestModel, TestRow, TestState, TestTake};
use crate::ipc::{Event, Link, Subscription};
use crate::{AppWindow, Ctx, Page, report};

use super::progress::{Bars, left_label};
use super::toast;
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
const OFF_NO_RECORDINGS: &str =
    "No recordings yet: dictate a few times with History on, or add one on Recordings";
const OFF_NO_MODEL: &str = "Tick a downloaded model";
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
const CANCELLED: &str = "Test cancelled.";
const NOT_DOWNLOADED: &str = "Not downloaded";
const DROP_SENT: &str = "Deleting the other models.";
const PLAN_TIP: &str = "Picked by hand on Recordings, or else a few recent recordings from History of different lengths, each scored against its saved text. Nothing is pasted anywhere and nothing leaves this PC.";
const HISTORY_ON: &str = "History is on. New dictations are kept.";

/// The goal of tile `i`; the first for anything else.
fn goal_at(i: i32) -> Goal {
    usize::try_from(i)
        .ok()
        .and_then(|i| Goal::ALL.get(i))
        .copied()
        .unwrap_or(Goal::Model)
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
    bind_with(window, ctx, open_mic, play_cpal, None);
}

/// [`bind`] on another microphone and speaker. Binding again starts the page over, and the
/// recordings go. `hardware` is the machine read already (RAM, adapters, free disk): a test hook
/// that skips the worker read; `None` reads the real machine on the first open.
pub fn bind_with(
    window: &AppWindow,
    ctx: &Ctx,
    mic: Mic,
    player: Player,
    hardware: Option<Hardware>,
) {
    let s = window.global::<TestState>();
    let (tx, rx) = channel();
    // The percent and the time left are drawn into one label.
    let label = Rc::new(Cell::new((0.0_f32, None)));
    let draw = {
        let window = window.as_weak();
        move |label: (f32, Option<u64>)| {
            if let Some(window) = window.upgrade() {
                let s = window.global::<TestState>();
                s.set_progress(label.0);
                s.set_progress_text(progress_label(label.0, label.1).into());
            }
        }
    };
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
        bar: Bars::with_left(
            {
                let (label, draw) = (Rc::clone(&label), draw.clone());
                move |(), shown| {
                    label.set((shown, label.get().1));
                    draw(label.get());
                }
            },
            move |(), left| {
                label.set((label.get().0, left));
                draw(label.get());
            },
        ),
        entries: MODELS.iter().filter(|m| m.id != "tiny").collect(),
        choices: RefCell::new(Vec::new()),
        ticked: RefCell::new(Vec::new()),
        preset: Cell::new(Some(ModelPreset::Downloaded)),
        models_model: Rc::new(VecModel::default()),
        hardware: RefCell::new(hardware),
        gpus: RefCell::new(None),
        measuring: Cell::new(false),
        measure_tx: tx,
        measure_rx: rx,
        get_id: Cell::new(None),
        drop_ids: RefCell::new(Vec::new()),
        left_out: Cell::new(None),
        rows: Rc::new(VecModel::default()),
        heard: Rc::new(VecModel::default()),
        results: RefCell::new(Results::default()),
        best: RefCell::new(None),
        next_id: Cell::new(1),
    });
    s.set_goal(0);
    s.set_models(ModelRc::from(page.models_model.clone()));
    s.set_plan_tip(PLAN_TIP.into());
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
    page.clear_offers(&s);
    page.choose_models();
    page.refresh(&s);
    connect(&s, &page);
    let weak = Rc::downgrade(&page);
    window.window().on_close_requested(move || {
        if let Some(page) = weak.upgrade() {
            page.forget();
        }
        CloseRequestResponse::HideWindow
    });
}

/// The page's callbacks.
fn connect(s: &TestState<'_>, page: &Rc<PageState>) {
    let p = Rc::clone(page);
    s.on_shown(move || p.shown());
    let p = Rc::clone(page);
    s.on_goal_picked(move |i| p.goal_picked(i));
    let p = Rc::clone(page);
    s.on_preset_picked(move |i| p.preset_picked(i));
    let p = Rc::clone(page);
    s.on_model_ticked(move |i| p.model_ticked(i));
    let p = Rc::clone(page);
    s.on_measured(move || p.measured());
    let p = Rc::clone(page);
    s.on_cancel(move || p.cancel());
    let p = Rc::clone(page);
    s.on_get(move || p.get());
    let p = Rc::clone(page);
    s.on_drop(move || p.drop_losers());
    let p = Rc::clone(page);
    s.on_record(move || p.record());
    let p = Rc::clone(page);
    s.on_add_history(move || p.add_history());
    let p = Rc::clone(page);
    s.on_turn_on_history(move || p.turn_on_history());
    let p = Rc::clone(page);
    s.on_play(move |i| p.play(i));
    let p = Rc::clone(page);
    s.on_remove(move |i| p.remove(i));
    let p = Rc::clone(page);
    s.on_reference_edited(move |i, text| p.reference_edited(i, &text));
    let p = Rc::clone(page);
    s.on_run(move || p.start_run());
    let p = Rc::clone(page);
    s.on_select(move |i| p.select(i));
    let p = Rc::clone(page);
    s.on_apply(move || p.apply());
}

/// Where a picked recording comes from.
#[derive(Clone)]
enum Source {
    /// Recorded on this page: sent to the daemon with `TestAudio`.
    New,
    /// A history entry, by id: the daemon reads it from its folder.
    History(HistoryId),
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

/// One recording of a run: where it comes from, its samples (sent for a new one), what was said.
type RunTake = (Source, Rc<Vec<f32>>, String);
/// A recording a run starts from: its source, samples and their rate, what was said, its label.
type BaseTake = (Source, Rc<Vec<f32>>, u32, String, String);
/// The system memory, the graphics adapters and the free space of the models drive.
type Hardware = (Option<u64>, Vec<Adapter>, Option<u64>);

/// A run in progress: one request is out with the daemon, the rest wait for its answer. Jobs go
/// set by set, every recording under each set, so each set's total comes in as soon as it can.
/// The jobs of a set the run skips (a model not on disk) are marked from the start and never sent.
struct Run {
    /// The request out.
    id: u32,
    /// The job out: set `job / takes.len()`, recording `job % takes.len()`.
    job: usize,
    /// Jobs answered, and the jobs the run sends in all.
    done: usize,
    jobs: usize,
    takes: Vec<RunTake>,
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
    offered: RefCell<Vec<HistoryId>>,
    offered_model: Rc<VecModel<SharedString>>,
    run: RefCell<Option<Run>>,
    /// The bar of the whole run: the one progress driver, not a percent of its own.
    bar: Rc<Bars<()>>,
    /// The request a closed window left with the daemon, until its answer comes.
    left_out: Cell<Option<u32>>,
    rows: Rc<VecModel<TestRow>>,
    heard: Rc<VecModel<TestHeard>>,
    results: RefCell<Results>,
    best: RefCell<Option<SettingsSet>>,
    next_id: Cell<u32>,
    /// The registry models the wizard lists, in gallery order, and what is known of each.
    entries: Vec<&'static ModelEntry>,
    choices: RefCell<Vec<ModelChoice>>,
    ticked: RefCell<Vec<bool>>,
    /// The quick pick the ticks follow; `None` once ticked by hand.
    preset: Cell<Option<ModelPreset>>,
    models_model: Rc<VecModel<TestModel>>,
    /// The system memory, graphics adapters and free space of the models drive, measured once on
    /// a worker; the daemon's GPUs. Both feed the recommendation.
    hardware: RefCell<Option<Hardware>>,
    gpus: RefCell<Option<Vec<Device>>>,
    measuring: Cell<bool>,
    measure_tx: Sender<Hardware>,
    measure_rx: Receiver<Hardware>,
    /// The offers of the last run: the recommended model it skipped, the models that lost.
    get_id: Cell<Option<&'static str>>,
    drop_ids: RefCell<Vec<&'static str>>,
}

impl PageState {
    fn state(&self, f: impl FnOnce(&TestState<'_>)) {
        if let Some(window) = self.window.upgrade() {
            f(&window.global::<TestState>());
        }
    }

    fn toast(&self, text: &str) {
        if let Some(window) = self.window.upgrade() {
            toast(&window, text);
        }
    }

    /// The page was opened or the config reloaded: the language or the history setting may have
    /// changed.
    fn shown(&self) {
        self.list_history();
        self.choose_models();
        // A reload with another page open measures and asks nothing.
        if self.open() {
            self.measure();
            if self.gpus.borrow().is_none() && self.ctx.ipc.link() == Link::Online {
                self.ctx.ipc.send(Request::ListDevices);
            }
        }
        self.state(|s| self.refresh(s));
    }

    fn open(&self) -> bool {
        self.window
            .upgrade()
            .is_some_and(|w| w.get_page() == Page::Test)
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
        if self.open() && self.history_on() && self.ctx.ipc.link() == Link::Online {
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
        let goal = goal_at(s.get_goal());
        let recordings = if takes.is_empty() {
            self.auto_picks().len()
        } else {
            takes.len()
        };
        let (sets, _) = self.sets(goal);
        let off = if running {
            OFF_RUNNING
        } else if recording {
            OFF_RECORDING
        } else if recordings == 0 {
            OFF_NO_RECORDINGS
        } else if takes
            .iter()
            .any(|t| word_errors(&t.reference, "").words == 0)
        {
            // Scored over no words, heard text is 100 % and silence 0 %: Apply would prefer silence.
            OFF_NO_TEXT
        } else if sets.is_empty() {
            OFF_NO_MODEL
        } else {
            self.ctx.ipc.link().off_reason(OFF_NEEDS_DAEMON)
        };
        s.set_run_off(off.into());
        s.set_plan(plan_text(recordings, !takes.is_empty(), goal, sets.len()).into());
        s.set_playing(
            self.playing
                .borrow()
                .as_ref()
                .and_then(|(i, _)| i32::try_from(*i).ok())
                .unwrap_or(-1),
        );
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
        let picked: Vec<HistoryId> = self
            .takes
            .borrow()
            .iter()
            .filter_map(|t| match &t.source {
                Source::History(id) => Some(id.clone()),
                Source::New => None,
            })
            .collect();
        let (ids, labels): (Vec<HistoryId>, Vec<SharedString>) = self
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
            self.bar.clear(&());
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
        let goal = goal_at(s.get_goal());
        let (runnable, skipped) = self.sets(goal);
        let (takes, labels): (Vec<RunTake>, Vec<String>) = self.run_takes(goal).into_iter().unzip();
        let n = takes.len();
        let mut cells = vec![vec![Answer::Waiting; n]; runnable.len()];
        cells.extend(vec![vec![Answer::Failed(NOT_DOWNLOADED); n]; skipped.len()]);
        let sets: Vec<SettingsSet> = runnable.iter().chain(&skipped).cloned().collect();
        *self.results.borrow_mut() = Results {
            cells,
            labels,
            sets: sets.clone(),
        };
        self.rows
            .set_vec(sets.iter().map(row).collect::<Vec<TestRow>>());
        for i in runnable.len()..sets.len() {
            self.show_row(i);
        }
        *self.best.borrow_mut() = None;
        s.set_selected(0);
        s.set_best_text(SharedString::new());
        s.set_apply_off(OFF_NO_BEST.into());
        s.set_pace(SharedString::new());
        self.clear_offers(&s);
        self.bar.clear(&());
        s.set_view(2);
        let run = Run {
            id: 0,
            job: 0,
            done: 0,
            jobs: runnable.len() * n,
            takes,
            held: None,
            language: self.ctx.config.borrow().config.model.language,
        };
        self.send_job(run);
    }

    /// The recordings of a run with their labels: the ones picked by hand, or else a few recent
    /// ones from History; each once per gain of `goal`. A quieter copy is made here and sent as a
    /// new recording; a history file that cannot be read, or is not at the engine's rate, gets
    /// none.
    fn run_takes(&self, goal: Goal) -> Vec<(RunTake, String)> {
        let base: Vec<BaseTake> = {
            let takes = self.takes.borrow();
            if takes.is_empty() {
                self.auto_picks()
                    .into_iter()
                    .map(|item| {
                        let label = tuning_view::when(&item.id);
                        let text = item.text.0.trim().to_owned();
                        let source = Source::History(item.id);
                        (source, Rc::new(Vec::new()), SAMPLE_RATE, text, label)
                    })
                    .collect()
            } else {
                takes
                    .iter()
                    .map(|t| {
                        let (audio, text) = (Rc::clone(&t.audio), t.reference.clone());
                        (t.source.clone(), audio, t.rate, text, t.label.clone())
                    })
                    .collect()
            }
        };
        let mut out = Vec::new();
        for (source, mut audio, mut rate, reference, label) in base {
            for &db in goal.gains() {
                if db == 0.0 {
                    let take = (source.clone(), Rc::clone(&audio), reference.clone());
                    out.push((take, label.clone()));
                    continue;
                }
                if let (Source::History(id), true) = (&source, audio.is_empty()) {
                    let (samples, r) = self.read_history(id).unwrap_or((Vec::new(), rate));
                    (audio, rate) = (Rc::new(samples), r);
                }
                // Over five minutes (picked by hand) it has no quieter copy; its 0 dB take runs.
                let quieter = (!audio.is_empty() && rate == SAMPLE_RATE)
                    .then(|| tuning::with_gain(&audio, db))
                    .flatten();
                let Some(quieter) = quieter else {
                    continue;
                };
                let take = (Source::New, Rc::new(quieter), reference.clone());
                out.push((take, format!("{label} · {db:.0} dB")));
            }
        }
        out
    }

    /// The history recordings a run takes when none is picked by hand: done, with words to score
    /// against, of varied length (`tuning::pick_recordings`).
    fn auto_picks(&self) -> Vec<HistoryItem> {
        if !self.history_on() {
            return Vec::new();
        }
        let history = self.history.borrow();
        let lengths: Vec<Option<u64>> = history
            .iter()
            .map(|item| {
                (item.status == Status::Done && word_errors(&item.text.0, "").words > 0)
                    .then_some(item.duration_ms)
            })
            .collect();
        tuning::pick_recordings(&lengths, PICK_COUNT)
            .into_iter()
            .map(|i| history[i].clone())
            .collect()
    }

    /// The sets of `goal` over the ticked models: the ones on disk run, the others are listed as
    /// skipped (one line each).
    fn sets(&self, goal: Goal) -> (Vec<SettingsSet>, Vec<SettingsSet>) {
        let base = {
            let audio = &self.ctx.config.borrow().config.audio;
            SettingsSet {
                normalize: audio.normalize,
                denoise: audio.denoise,
                trim: audio.trim,
                model: String::new(),
            }
        };
        let choices = self.choices.borrow();
        let ticks = self.ticked.borrow();
        let ticked = || choices.iter().zip(ticks.iter()).filter(|(_, on)| **on);
        let ready: Vec<&str> = ticked()
            .filter(|(c, _)| c.downloaded)
            .map(|(c, _)| c.id.as_str())
            .collect();
        let missing: Vec<&str> = ticked()
            .filter(|(c, _)| !c.downloaded)
            .map(|(c, _)| c.id.as_str())
            .collect();
        (
            tuning::wizard_sets(goal, &base, &ready),
            tuning::wizard_sets(Goal::Model, &base, &missing),
        )
    }

    /// Sends the next job that waits; with none left the run is over.
    fn send_job(&self, mut run: Run) {
        let n = run.takes.len();
        let sets = self.results.borrow().sets.clone();
        while run.job < sets.len() * n && !self.waiting(run.job / n, run.job % n) {
            run.job += 1;
        }
        if n == 0 || run.job >= sets.len() * n {
            self.finish_run();
            self.bar.finish(&());
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
        // Only moves a bar that runs: the first push of the daemon starts it, with its estimate.
        self.progress(&run, 0, None);
        *self.run.borrow_mut() = Some(run);
        self.state(|s| self.refresh(s));
    }

    fn waiting(&self, set: usize, take: usize) -> bool {
        matches!(
            self.results
                .borrow()
                .cells
                .get(set)
                .and_then(|row| row.get(take)),
            Some(Answer::Waiting)
        )
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

    /// The whole run's bar: the jobs done and `percent` of the one out are what is really known.
    /// A push with `est_ms` (the first of a job) starts the bar, sized for every job; until then
    /// the bar is the indeterminate sweep, and `bar.real` moves nothing.
    fn progress(&self, run: &Run, percent: u8, est_ms: Option<u32>) {
        let jobs = run.jobs.max(1);
        if let Some(est) = est_ms
            && !self.bar.live(&())
        {
            self.bar
                .start((), Some(Millis(u64::from(est) * jobs as u64)));
        }
        #[expect(clippy::cast_precision_loss, reason = "job counts are tiny")]
        let done = (run.done as f32 + f32::from(percent.min(100)) / 100.0) / jobs as f32;
        self.bar.real(&(), done);
    }

    fn event(&self, event: &Event) {
        match event {
            Event::Push(Push::TestResult { id, outcome }) => self.answered(*id, outcome),
            Event::Push(Push::TestProgress {
                id,
                percent,
                est_ms,
            }) => {
                if let Some(run) = self.run.borrow().as_ref().filter(|r| r.id == *id) {
                    self.progress(run, *percent, *est_ms);
                }
            }
            Event::Push(Push::History { items, .. }) => self.listed(items),
            Event::Push(Push::Devices { gpus, .. }) => {
                *self.gpus.borrow_mut() = Some(gpus.clone());
                self.choose_models();
                self.state(|s| self.refresh(s));
            }
            // A model came or went: the list says so.
            Event::Push(Push::ModelDeleted { .. } | Push::DownloadDone { .. }) => {
                self.choose_models();
                // An offer follows the disk: no Download of a model now there, no Delete of one gone.
                let on_disk = |id: &str| {
                    self.choices
                        .borrow()
                        .iter()
                        .any(|c| c.id == id && c.downloaded)
                };
                if self.get_id.get().is_some_and(on_disk) {
                    self.get_id.set(None);
                }
                self.drop_ids.borrow_mut().retain(|id| on_disk(id));
                self.state(|s| {
                    self.show_offers(s);
                    self.refresh(s);
                });
            }
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
        run.done += 1;
        self.show_pace();
        self.send_job(run);
    }

    /// Marks every job of `run` not answered yet.
    fn mark_not_run(&self, run: &Run) {
        let n = run.takes.len();
        let jobs = self.results.borrow().sets.len() * n;
        for job in run.job..jobs {
            let (set, take) = (job / n, job % n);
            let open = matches!(
                self.results.borrow().cells[set][take],
                Answer::Waiting | Answer::Running
            );
            if open {
                self.set_cell(set, take, Answer::NotRun);
            }
        }
    }

    /// Ends the run early: what was not answered is marked, the answers so far stay, and the
    /// bar stops where it is (DS-26: a failed run never runs to 100).
    fn abort_run(&self, why: &str) {
        let Some(run) = self.run.borrow_mut().take() else {
            return;
        };
        self.mark_not_run(&run);
        self.toast(why);
        self.bar.fail(&());
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
        self.offers(
            best.as_ref().map(|(_, set, _)| set.model.as_str()),
            &pairs.iter().map(|(set, _)| set.clone()).collect::<Vec<_>>(),
        );
        self.state(|s| {
            if let Some((row, set, errors)) = &best {
                if let Some(mut line) = self.rows.row_data(*row) {
                    line.best = true;
                    self.rows.set_row_data(*row, line);
                }
                let mut text = format!("{} · {} errors", model_name(&set.model), percent(*errors));
                if let Some(ms) = self.mean_ms(Some(*row)) {
                    text = format!("{text} · {}", per_recording(ms));
                }
                s.set_best_text(text.into());
                s.set_apply_off(SharedString::new());
                s.set_selected(i32::try_from(*row).unwrap_or(0));
                self.show_heard(*row);
            } else {
                s.set_best_text(NOTHING_HEARD.into());
                s.set_apply_off(NOTHING_HEARD.into());
            }
            self.refresh(s);
        });
        *self.best.borrow_mut() = best.map(|(_, set, _)| set);
    }

    /// The offers of a finished run: Download of the recommended model it skipped (not on disk),
    /// Delete of the downloaded models that heard everything but lost to `winner` (never the model
    /// in use). No winner, no Delete.
    fn offers(&self, winner: Option<&str>, scored: &[SettingsSet]) {
        let recommended: Option<String> = self
            .choices
            .borrow()
            .iter()
            .find(|c| c.recommended && !c.downloaded)
            .map(|c| c.id.clone());
        let skipped = {
            let results = self.results.borrow();
            results.sets.iter().zip(&results.cells).any(|(set, cells)| {
                Some(&set.model) == recommended.as_ref()
                    && cells
                        .iter()
                        .all(|c| matches!(c, Answer::Failed(NOT_DOWNLOADED)))
            })
        };
        let entry = |id: &str| self.entries.iter().find(|e| e.id == id).copied();
        self.get_id.set(
            recommended
                .filter(|_| skipped)
                .and_then(|id| entry(&id))
                .map(|e| e.id),
        );
        let in_use = self.ctx.config.borrow().config.model.id.clone();
        let downloaded: Vec<String> = self
            .choices
            .borrow()
            .iter()
            .filter(|c| c.downloaded)
            .map(|c| c.id.clone())
            .collect();
        *self.drop_ids.borrow_mut() = winner
            .map(|winner| tuning::losers(scored, winner, &in_use))
            .unwrap_or_default()
            .into_iter()
            .filter(|id| downloaded.iter().any(|d| d == id))
            .filter_map(|id| entry(id).map(|e| e.id))
            .collect();
        self.state(|s| self.show_offers(s));
    }

    fn clear_offers(&self, s: &TestState<'_>) {
        self.get_id.set(None);
        self.drop_ids.borrow_mut().clear();
        self.show_offers(s);
    }

    fn show_offers(&self, s: &TestState<'_>) {
        let name = |id: &str| model_name(id).to_owned();
        s.set_get_text(
            self.get_id
                .get()
                .map(|id| format!("Download {}", name(id)))
                .unwrap_or_default()
                .into(),
        );
        let drop = self.drop_ids.borrow();
        let frees = gb(drop
            .iter()
            .filter_map(|id| models::find(id))
            .map(|e| e.size)
            .sum());
        let (text, ask) = match drop.as_slice() {
            [] => (String::new(), String::new()),
            [one] => (
                format!("Delete {} ({frees})", name(one)),
                format!("Delete {}? Frees {frees}.", name(one)),
            ),
            many => {
                let names: Vec<String> = many.iter().map(|id| name(id)).collect();
                (
                    format!("Delete {} others ({frees})", many.len()),
                    format!("Delete {}? Frees {frees}.", names.join(", ")),
                )
            }
        };
        if drop.is_empty() {
            // The question row asks about nothing now: the next offer starts at its button.
            s.set_dropping(false);
        }
        s.set_drop_text(text.into());
        s.set_drop_ask(ask.into());
        s.set_drop_off(self.ctx.ipc.link().off_reason(OFF_NEEDS_DAEMON).into());
    }

    /// Download: the model page's own Download does it (never on its own).
    fn get(&self) {
        if let Some(window) = self.window.upgrade() {
            window.set_page(Page::Model);
            window.global::<ModelState>().set_tab(0);
        }
    }

    /// Delete the models that lost, after the row asked; the model page reports each one.
    fn drop_losers(&self) {
        if self.ctx.ipc.link() != Link::Online {
            return;
        }
        let ids: Vec<&'static str> = self.drop_ids.borrow_mut().drain(..).collect();
        if ids.is_empty() {
            self.state(|s| self.show_offers(s));
            return;
        }
        let in_use = self.ctx.config.borrow().config.model.id.clone();
        for id in ids.into_iter().filter(|id| *id != in_use) {
            self.ctx
                .ipc
                .send(Request::DeleteModel { id: id.to_owned() });
        }
        self.toast(DROP_SENT);
        self.state(|s| self.show_offers(s));
    }

    /// Cancel: the job out is left to finish on the daemon and its answer dropped; the sets that
    /// heard every recording so far still compete.
    fn cancel(&self) {
        let Some(run) = self.run.borrow_mut().take() else {
            return;
        };
        self.left_out.set(Some(run.id));
        self.mark_not_run(&run);
        self.bar.clear(&());
        self.toast(CANCELLED);
        self.finish_run();
    }

    /// The mean time of one recording, over the set `set` or over every set; `None` before the
    /// first answer.
    fn mean_ms(&self, set: Option<usize>) -> Option<u64> {
        let results = self.results.borrow();
        let times: Vec<u64> = results
            .cells
            .iter()
            .enumerate()
            .filter(|(i, _)| set.is_none_or(|s| s == *i))
            .flat_map(|(_, cells)| cells)
            .filter_map(|c| match c {
                Answer::Heard { ms, .. } => Some(*ms),
                _ => None,
            })
            .collect();
        let n = u64::try_from(times.len()).ok().filter(|n| *n > 0)?;
        Some(times.iter().sum::<u64>() / n)
    }

    fn show_pace(&self) {
        let pace = self.mean_ms(None).map(per_recording).unwrap_or_default();
        self.state(|s| s.set_pace(pace.into()));
    }

    fn goal_picked(&self, i: i32) {
        if self.run.borrow().is_some() {
            return;
        }
        self.state(|s| {
            s.set_goal(i);
            self.refresh(s);
        });
    }

    fn preset_picked(&self, i: i32) {
        let Some(preset) = usize::try_from(i)
            .ok()
            .and_then(|i| ModelPreset::ALL.get(i))
            .copied()
        else {
            return;
        };
        if self.run.borrow().is_some() {
            return;
        }
        self.preset.set(Some(preset));
        *self.ticked.borrow_mut() = tuning::preset_ticks(preset, &self.choices.borrow());
        self.show_models();
        self.state(|s| self.refresh(s));
    }

    fn model_ticked(&self, i: i32) {
        let Ok(i) = usize::try_from(i) else {
            return;
        };
        if self.run.borrow().is_some() {
            return;
        }
        {
            let mut ticks = self.ticked.borrow_mut();
            let Some(on) = ticks.get_mut(i) else {
                return;
            };
            *on = !*on;
            self.preset
                .set(tuning::preset_of(&ticks, &self.choices.borrow()));
            if let Some(mut line) = self.models_model.row_data(i) {
                line.on = ticks[i];
                self.models_model.set_row_data(i, line);
            }
        }
        self.state(|s| {
            s.set_preset(preset_index(self.preset.get()));
            self.refresh(s);
        });
    }

    /// What is on disk and recommended now; the ticks follow the quick pick unless ticked by hand.
    /// Reads the files' metadata only.
    fn choose_models(&self) {
        let (dir, in_use, language, device) = {
            let file = self.ctx.config.borrow();
            let model = &file.config.model;
            (
                self.ctx.layout.models_dir(),
                model.id.clone(),
                model.language,
                model.gpu.clone(),
            )
        };
        let downloaded: Vec<bool> = self
            .entries
            .iter()
            .map(|e| {
                matches!(
                    models::status(e, &dir),
                    ModelStatus::Ready | ModelStatus::NeedsHash
                )
            })
            .collect();
        let recommended = self
            .hardware
            .borrow()
            .as_ref()
            .and_then(|(ram, adapters, free)| {
                let machine = Machine {
                    ram: *ram,
                    vram: models::graphics_memory(self.gpus.borrow().as_deref(), &device, adapters),
                    free_disk: *free,
                };
                models::recommend(&machine, language, |id| {
                    self.entries
                        .iter()
                        .zip(&downloaded)
                        .find(|(e, _)| e.id == id)
                        .map_or(0, |(e, on)| if *on { e.size } else { 0 })
                })
                .map(|r| r.id)
            });
        let choices: Vec<ModelChoice> = self
            .entries
            .iter()
            .zip(&downloaded)
            .map(|(e, on)| ModelChoice {
                id: e.id.to_owned(),
                downloaded: *on,
                recommended: recommended == Some(e.id),
                in_use: e.id == in_use,
            })
            .collect();
        {
            let mut ticks = self.ticked.borrow_mut();
            match self.preset.get() {
                Some(preset) => *ticks = tuning::preset_ticks(preset, &choices),
                None => ticks.resize(choices.len(), false),
            }
        }
        *self.choices.borrow_mut() = choices;
        self.show_models();
    }

    fn show_models(&self) {
        let lines: Vec<TestModel> = self
            .entries
            .iter()
            .zip(self.choices.borrow().iter())
            .zip(self.ticked.borrow().iter())
            .map(|((e, c), on)| TestModel {
                name: e.display_name.into(),
                size: gb(e.size).into(),
                on: *on,
                downloaded: c.downloaded,
                recommended: c.recommended,
            })
            .collect();
        self.models_model.set_vec(lines);
        self.state(|s| s.set_preset(preset_index(self.preset.get())));
    }

    /// Measures the system memory, the graphics adapters and the free space of the models drive
    /// once, on a worker (a network folder can stall); the answer feeds the recommendation.
    fn measure(&self) {
        if self.hardware.borrow().is_some() || self.measuring.replace(true) {
            return;
        }
        let (tx, dir, window) = (
            self.measure_tx.clone(),
            self.ctx.layout.models_dir(),
            self.window.clone(),
        );
        thread::spawn(move || {
            let measured = (
                sysinfo::ram(),
                sysinfo::adapters(),
                sysinfo::free_space(&dir),
            );
            // A failed send or wake: the page or the window is gone, or no event loop runs.
            if tx.send(measured).is_ok() {
                let _ = window.upgrade_in_event_loop(|w| w.global::<TestState>().invoke_measured());
            }
        });
    }

    fn measured(&self) {
        while let Ok(answer) = self.measure_rx.try_recv() {
            *self.hardware.borrow_mut() = Some(answer);
            self.measuring.set(false);
        }
        self.choose_models();
        self.state(|s| self.refresh(s));
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

/// The display name of model `id`; the id itself when the registry does not know it.
fn model_name(id: &str) -> &str {
    models::find(id).map_or(id, |entry| entry.display_name)
}

/// The quick pick's segment, -1 for a pick by hand.
fn preset_index(preset: Option<ModelPreset>) -> i32 {
    preset
        .and_then(|p| ModelPreset::ALL.iter().position(|q| *q == p))
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(-1)
}

/// "42 %", then "42 % · 1 min left" once the time left is known.
fn progress_label(shown: f32, left: Option<u64>) -> String {
    let percent = format!("{:.0} %", (shown * 100.0).floor());
    match left_label(left) {
        left if left.is_empty() => percent,
        left => format!("{percent} · {left}"),
    }
}

/// "2.1 s per recording".
#[expect(clippy::cast_precision_loss, reason = "a job is far below 2^52 ms")]
fn per_recording(ms: u64) -> String {
    format!("{:.1} s per recording", ms as f64 / 1000.0)
}

/// What Run takes: "3 recordings from History · 12 runs"; empty with nothing to run.
fn plan_text(recordings: usize, by_hand: bool, goal: Goal, sets: usize) -> String {
    if recordings == 0 || sets == 0 {
        return String::new();
    }
    let what = match (by_hand, recordings) {
        (true, 1) => "1 picked recording".to_owned(),
        (true, n) => format!("{n} picked recordings"),
        (false, 1) => "1 recording from History".to_owned(),
        (false, n) => format!("{n} recordings from History"),
    };
    let levels = goal.gains().len();
    let runs = recordings * levels * sets;
    let runs = if runs == 1 {
        "1 run".to_owned()
    } else {
        format!("{runs} runs")
    };
    if levels > 1 {
        format!("{what} · {levels} loudness levels · {runs}")
    } else {
        format!("{what} · {runs}")
    }
}

/// A results line for `set`, not run yet.
fn row(set: &SettingsSet) -> TestRow {
    let on = |value: bool| SharedString::from(if value { "On" } else { "Off" });
    TestRow {
        model: model_name(&set.model).into(),
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
