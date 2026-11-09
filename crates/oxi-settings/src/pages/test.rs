//! Test page: record a phrase, run it through the settings sets of a scenario, one set at a time
//! (`TestAudio`, then the next `TestTranscribe` only after each `TestResult`), show the word error
//! percent per set and apply the best one.
//!
//! The recording lives in [`PageState::audio`] and on the daemon's connection, in memory only;
//! closing the window drops it ([`PageState::forget`]) and ends the connection.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::{Rc, Weak as RcWeak};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use oxi_core::audio::{AudioError, SAMPLE_RATE};
use oxi_core::config::Audio;
use oxi_core::ipc::{Push, Request, TestFailure, TestOutcome};
use oxi_core::machine::Notice;
use oxi_core::models;
use oxi_core::strings::{self, NO_MIC};
use oxi_core::tuning::{
    self, MAX_TEST_SAMPLES, PCM_CHUNK_SAMPLES, Profile, SettingsSet, WordErrors, encode_pcm,
    word_errors,
};
use oxi_core::types::Language;
use oxi_platform::capture::{Capture, CaptureError, CaptureEvent, CpalCapture, LevelMeter};
use slint::{
    CloseRequestResponse, ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode,
    VecModel, Weak,
};

use crate::generated::{TestRow, TestState, Toast};
use crate::ipc::{Event, Link, Subscription};
use crate::{AppWindow, Ctx, Page, report};

/// An open recording: stops it and gives the samples. Dropped uncalled, it discards them.
pub type Finish = Box<dyn FnOnce() -> Result<Vec<f32>, CaptureError>>;
/// Opens the microphone for a recording; tests swap in one that needs no device. The flag goes up
/// when the device is gone or capture failed: the samples so far stay readable through [`Finish`].
pub type Mic = fn(&Audio, Arc<AtomicBool>) -> Result<Finish, CaptureError>;

/// How often the elapsed time and the limits are looked at while recording. The clock counts
/// ticks: a tick a busy window skips makes it run a little late, and `stop` truncates the samples
/// to the engine's limit whatever the clock said.
const TICK: Duration = Duration::from_millis(200);
const TICKS_PER_SECOND: usize = 5;
const MAX_TICKS: usize = MAX_TEST_SAMPLES / SAMPLE_RATE as usize * TICKS_PER_SECOND;

const OWN_PHRASE: &str = "My own phrase";
const PHRASE_NOTE: &str = "Ready text in your dictation language.";
const NO_READY_PHRASES: &str = "No ready phrases in this language: write your own.";
const NOT_RECORDED: &str = "Nothing recorded yet.";
const RUN_NOTE: &str = "Tries every set of the scenario on the recording.";
const NO_RESULTS: &str = "No results yet. Record a phrase and run the test.";
const NOTHING_HEARD: &str = "No set finished, so there is nothing to apply.";
const OFF_RUNNING: &str = "The test is running";
const OFF_RECORDING: &str = "Stop the recording first";
const OFF_NO_AUDIO: &str = "Record the phrase first";
const OFF_NO_PHRASE: &str = "Write the phrase first";
const OFF_NEEDS_DAEMON: &str = "Start OxiYapper to run the test";
const OFF_NO_BEST: &str = "Run the test first";
const WAITING: &str = "Waiting";
const RUNNING: &str = "Running…";
const NOT_RUN: &str = "Not run";
const TOO_LONG: &str = "Recording stopped at 5 minutes.";
const BUSY: &str = "OxiYapper is dictating. Run the test when it is free.";
const LAST_TEST_BUSY: &str = "The last test is still finishing. Run the test in a moment.";
const PHRASE_CHANGED: &str = "The phrase changed, so the recording is gone. Record it again.";
const NO_AUDIO: &str = "OxiYapper did not get the recording. Run the test again.";
const LINK_LOST: &str = "OxiYapper stopped answering, so the test ended.";
const APPLIED: &str = "Best set applied.";

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

/// Once per window: a config reload only calls `shown`, so the recording and the run outlive it.
pub fn bind(window: &AppWindow, ctx: &Ctx) {
    bind_with(window, ctx, open_mic);
}

/// [`bind`] on another microphone. Binding again starts the page over, and the old recording goes.
pub fn bind_with(window: &AppWindow, ctx: &Ctx, mic: Mic) {
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
        phrases: Cell::new(&[]),
        take: RefCell::new(None),
        lost: Arc::new(AtomicBool::new(false)),
        read_as: Cell::new(None),
        ticks: Cell::new(0),
        timer: Timer::default(),
        audio: RefCell::new(Vec::new()),
        run: RefCell::new(None),
        left_out: Cell::new(None),
        rows: Rc::new(VecModel::default()),
        results: RefCell::new(Vec::new()),
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
    s.set_own_text(SharedString::new());
    s.set_rows(ModelRc::from(page.rows.clone()));
    s.set_best_text(NO_RESULTS.into());
    s.set_apply_off(OFF_NO_BEST.into());
    page.load_phrases(&s);
    page.refresh(&s);

    let p = Rc::clone(&page);
    s.on_shown(move || p.shown());
    let p = Rc::clone(&page);
    s.on_phrase_picked(move |_| p.phrase_changed());
    let p = Rc::clone(&page);
    s.on_own_edited(move |_| p.phrase_changed());
    let p = Rc::clone(&page);
    s.on_record(move || p.record());
    let p = Rc::clone(&page);
    s.on_run(move || p.start_run());
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

struct PageState {
    me: RcWeak<Self>,
    _events: Subscription,
    window: Weak<AppWindow>,
    ctx: Ctx,
    mic: Mic,
    /// The ready phrases of the dictation language.
    phrases: Cell<&'static [&'static str]>,
    /// The recording being made.
    take: RefCell<Option<Finish>>,
    /// Up when the microphone of the open take is gone; set from a capture thread.
    lost: Arc<AtomicBool>,
    /// The phrase row and language the take was recorded for.
    read_as: Cell<Option<(i32, Language)>>,
    ticks: Cell<usize>,
    timer: Timer,
    /// The finished recording, 16 kHz mono; empty is none.
    audio: RefCell<Vec<f32>>,
    run: RefCell<Option<Run>>,
    /// The set a closed window left with the daemon, until its answer comes.
    left_out: Cell<Option<u32>>,
    rows: Rc<VecModel<TestRow>>,
    /// Per row: the set and, once it was heard, its word errors.
    results: RefCell<Vec<(SettingsSet, Option<WordErrors>)>>,
    best: RefCell<Option<SettingsSet>>,
    next_id: Cell<u32>,
}

/// A run in progress: one set is out with the daemon, the rest wait for its answer.
struct Run {
    id: u32,
    row: usize,
    reference: String,
    language: Language,
    left: VecDeque<SettingsSet>,
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

    /// The page was opened or the config reloaded: the dictation language may have changed.
    fn shown(&self) {
        self.state(|s| {
            self.load_phrases(s);
            self.check_take(s);
            self.refresh(s);
        });
    }

    fn script(&self, s: &TestState<'_>) -> (i32, Language) {
        (
            s.get_phrase(),
            self.ctx.config.borrow().config.model.language,
        )
    }

    /// A take read from another phrase or language would be scored against text never read. Edits
    /// of the own phrase keep it: they are usually a typo fixed or the text written after.
    fn check_take(&self, s: &TestState<'_>) {
        let held = self.take.borrow().is_some() || !self.audio.borrow().is_empty();
        if held && self.read_as.get() != Some(self.script(s)) {
            self.drop_take();
            self.toast(PHRASE_CHANGED);
        }
    }

    fn drop_take(&self) {
        self.stop(false);
        *self.audio.borrow_mut() = Vec::new();
    }

    fn load_phrases(&self, s: &TestState<'_>) {
        let language = self.ctx.config.borrow().config.model.language;
        let phrases = tuning::phrases(language);
        if !std::ptr::eq(self.phrases.replace(phrases), phrases) || s.get_phrases().row_count() == 0
        {
            let mut labels: Vec<SharedString> = (1..=phrases.len())
                .map(|n| format!("Phrase {n}").into())
                .collect();
            labels.push(OWN_PHRASE.into());
            s.set_phrases(ModelRc::from(Rc::new(VecModel::from(labels))));
            s.set_phrase(0);
        }
        s.set_phrase_note(
            if phrases.is_empty() {
                NO_READY_PHRASES
            } else {
                PHRASE_NOTE
            }
            .into(),
        );
    }

    fn ready_phrase(&self, s: &TestState<'_>) -> Option<&'static str> {
        let phrases = self.phrases.get();
        usize::try_from(s.get_phrase())
            .ok()
            .and_then(|i| phrases.get(i).copied())
    }

    fn reference(&self, s: &TestState<'_>) -> String {
        self.ready_phrase(s)
            .map_or_else(|| s.get_own_text().trim().to_owned(), str::to_owned)
    }

    fn phrase_changed(&self) {
        self.state(|s| {
            self.check_take(s);
            self.refresh(s);
        });
    }

    /// Everything that follows from the recording, the phrase, the run and the link.
    fn refresh(&self, s: &TestState<'_>) {
        let ready = self.ready_phrase(s);
        s.set_own(ready.is_none());
        s.set_phrase_text(ready.unwrap_or_default().into());
        let recording = self.take.borrow().is_some();
        s.set_recording(recording);
        s.set_take(self.take_text(recording).into());
        let run = self.run.borrow();
        s.set_record_off(if run.is_some() { OFF_RUNNING } else { "" }.into());
        s.set_run_note(
            run.as_ref()
                .map_or_else(
                    || RUN_NOTE.to_owned(),
                    |r| format!("Set {} of {}", r.row + 1, self.rows.row_count()),
                )
                .into(),
        );
        let off = if run.is_some() {
            OFF_RUNNING
        } else if recording {
            OFF_RECORDING
        } else if self.audio.borrow().is_empty() {
            OFF_NO_AUDIO
        } else if word_errors(&self.reference(s), "").words == 0 {
            // Scored over no words, heard text is 100 % and silence 0 %: Apply would prefer silence.
            OFF_NO_PHRASE
        } else {
            self.ctx.ipc.link().off_reason(OFF_NEEDS_DAEMON)
        };
        s.set_run_off(off.into());
    }

    fn take_text(&self, recording: bool) -> String {
        let clock = |seconds: usize| format!("{}:{:02}", seconds / 60, seconds % 60);
        let held = self.audio.borrow().len();
        if recording {
            format!("Recording {}", clock(self.ticks.get() / TICKS_PER_SECOND))
        } else if held == 0 {
            NOT_RECORDED.to_owned()
        } else {
            format!("Recorded {}", clock(held / SAMPLE_RATE as usize))
        }
    }

    /// The Record and Stop button.
    fn record(&self) {
        if self.take.borrow().is_some() {
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
                *self.take.borrow_mut() = Some(finish);
                *self.audio.borrow_mut() = Vec::new();
                self.state(|s| self.read_as.set(Some(self.script(s))));
                self.ticks.set(0);
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
            // First: a take that cannot be read says why in the toast of `stop`.
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
        }
        self.state(|s| self.refresh(s));
    }

    /// Ends the recording; `keep` holds on to its samples.
    fn stop(&self, keep: bool) {
        self.timer.stop();
        let Some(finish) = self.take.borrow_mut().take() else {
            return;
        };
        if !keep {
            return;
        }
        match finish() {
            Ok(mut samples) => {
                samples.truncate(MAX_TEST_SAMPLES);
                *self.audio.borrow_mut() = samples;
            }
            Err(e) => self.toast(&failure(&e)),
        }
    }

    /// The window is closing: the recording goes, here and in an open take.
    fn forget(&self) {
        self.drop_take();
        if let Some(run) = self.run.borrow_mut().take() {
            self.left_out.set(Some(run.id));
        }
        self.state(|s| self.refresh(s));
    }

    /// Run: sends the recording, then the first set; [`Self::answered`] sends the others.
    fn start_run(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let s = window.global::<TestState>();
        self.refresh(&s);
        if s.get_run_off() != "" {
            return;
        }
        let profile = usize::try_from(s.get_profile())
            .ok()
            .and_then(|i| Profile::ALL.get(i))
            .copied()
            .unwrap_or(Profile::General);
        let sets = profile.sets();
        self.rows
            .set_vec(sets.iter().map(row).collect::<Vec<TestRow>>());
        *self.results.borrow_mut() = sets.iter().map(|set| (set.clone(), None)).collect();
        *self.best.borrow_mut() = None;
        s.set_best_text(NO_RESULTS.into());
        s.set_apply_off(OFF_NO_BEST.into());
        s.set_view(1);
        for (i, piece) in self.audio.borrow().chunks(PCM_CHUNK_SAMPLES).enumerate() {
            self.ctx.ipc.send(Request::TestAudio {
                reset: i == 0,
                pcm: encode_pcm(piece),
            });
        }
        let run = Run {
            id: 0,
            row: 0,
            reference: self.reference(&s),
            language: self.ctx.config.borrow().config.model.language,
            left: sets.into(),
        };
        self.send_next(run);
    }

    /// Sends the next set; with none left the run is over.
    fn send_next(&self, mut run: Run) {
        let Some(set) = run.left.pop_front() else {
            self.finish_run();
            return;
        };
        run.id = self.next_id.get();
        self.next_id.set(run.id.wrapping_add(1));
        run.row = self.rows.row_count() - run.left.len() - 1;
        self.ctx.ipc.send(Request::TestTranscribe {
            id: run.id,
            set,
            language: run.language,
        });
        self.set_outcome(run.row, RUNNING, "");
        *self.run.borrow_mut() = Some(run);
        self.state(|s| self.refresh(s));
    }

    fn set_outcome(&self, row: usize, errors: &str, time: &str) {
        if let Some(mut line) = self.rows.row_data(row) {
            line.errors = errors.into();
            line.time = time.into();
            self.rows.set_row_data(row, line);
        }
    }

    fn event(&self, event: &Event) {
        match event {
            Event::Push(Push::TestResult { id, outcome }) => self.answered(*id, outcome),
            Event::Link(link) => {
                if *link != Link::Online {
                    self.left_out.set(None);
                    self.abort_run(LINK_LOST);
                }
                self.state(|s| self.refresh(s));
            }
            Event::Push(_) => {}
        }
    }

    /// The daemon answered the set that is out: score it, or stop on a failure of the whole run.
    fn answered(&self, id: u32, outcome: &TestOutcome) {
        if self.left_out.get() == Some(id) {
            self.left_out.set(None);
            return;
        }
        let run = self.run.borrow_mut().take_if(|run| run.id == id);
        let Some(run) = run else {
            return;
        };
        match outcome {
            TestOutcome::Heard { text, ms } => {
                let errors = word_errors(&run.reference, text);
                self.results.borrow_mut()[run.row].1 = Some(errors);
                self.set_outcome(run.row, &percent(errors), &seconds(*ms));
            }
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
                *self.run.borrow_mut() = Some(run);
                self.abort_run(NO_AUDIO);
                return;
            }
            TestOutcome::Failed { why } => self.set_outcome(run.row, failed(*why), ""),
        }
        self.send_next(run);
    }

    /// Ends the run early: what was not answered is marked, the sets that were answered stay.
    fn abort_run(&self, why: &str) {
        let Some(run) = self.run.borrow_mut().take() else {
            return;
        };
        for row in run.row..self.rows.row_count() {
            self.set_outcome(row, NOT_RUN, "");
        }
        self.toast(why);
        self.finish_run();
    }

    /// Finds the best set among the ones heard.
    fn finish_run(&self) {
        let (heard, scored): (Vec<usize>, Vec<(SettingsSet, WordErrors)>) = self
            .results
            .borrow()
            .iter()
            .enumerate()
            .filter_map(|(i, (set, errors))| errors.map(|e| (i, (set.clone(), e))))
            .unzip();
        let best = tuning::best(&scored).map(|i| (heard[i], scored[i].clone()));
        self.state(|s| {
            if let Some((row, (_, errors))) = &best {
                if let Some(mut line) = self.rows.row_data(*row) {
                    line.best = true;
                    self.rows.set_row_data(*row, line);
                }
                s.set_best_text(format!("Lowest error rate: {}", percent(*errors)).into());
                s.set_apply_off(SharedString::new());
            } else {
                s.set_best_text(NOTHING_HEARD.into());
                s.set_apply_off(NOTHING_HEARD.into());
            }
            self.refresh(s);
        });
        *self.best.borrow_mut() = best.map(|(_, (set, _))| set);
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

/// How a run of one set that gives no text shows in its line.
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
