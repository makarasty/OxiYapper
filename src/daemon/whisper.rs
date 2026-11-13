//! Whisper worker: load, unload, transcribe (ARCHITECTURE section 8).
//!
//! Obeys the coordinator's commands in order. The state machine decides when a model is stale
//! (Unload, then Preload); a `Config` alone never unloads or reloads. Dictated text and audio
//! never reach a log: only lengths, kinds and model names do.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use oxi_core::audio::{self, AudioError};
use oxi_core::config::{Advanced, Audio, Config, PasteMode};
use oxi_core::device::DeviceChoice;
use oxi_core::history::HistoryId;
use oxi_core::ipc::{
    DeleteResult, Push, TestFailure, TestOutcome, Words, WordsFailure, WordsResult,
};
use oxi_core::machine::{
    EngineWarning, Event, JobId, MachineConfig, ModelFailure, ModelIdentity, Phase, PhaseCell,
};
use oxi_core::models;
use oxi_core::overlay::{learn, transcribe_est};
use oxi_core::paste::Profile;
use oxi_core::paths::Layout;
use oxi_core::strings;
use oxi_core::text::{Outcome, TextOptions};
use oxi_core::tuning::{self, SettingsSet};
use oxi_core::types::Language;
use oxi_whisper::{Cancel, Engine, LoadInfo, Transcript};

use super::cmd::{Inbox, TestJob, WhisperCmd};
use super::io;
use super::ipc::{Origin, Pushes};
use crate::pipeline::{self, TextAssets};

/// A dictation of at least this long (ms) with fewer characters per second than [`SPARSE_CPS`]
/// logs a warning: whisper may have dropped most of it (counts only).
const SPARSE_MIN_MS: u64 = 5000;
const SPARSE_CPS: u64 = 5;

/// A finished dictation: the text, whisper's words, the parts held back.
type Transcribed = (
    String,
    Vec<oxi_core::history::Word>,
    Vec<oxi_core::history::HeldPart>,
);

/// What a run reports and obeys: whisper's progress, and a check that stops it.
struct Hooks<'a> {
    progress: &'a mut dyn FnMut(u8),
    cancel: Option<&'a Cancel>,
}

/// One loaded model; [`Engine`] in the daemon, a fake in the tests.
trait Recognizer {
    fn info(&self) -> &LoadInfo;
    /// Takes the per-run engine parameters for the next run.
    fn set_advanced(&mut self, _: &Advanced) {}
    /// Makes the next runs give word times (whisper's token timestamps), or stops that.
    fn set_token_times(&mut self, _: bool) {}
    fn transcribe(
        &mut self,
        samples: &[f32],
        language: Language,
        prompt: &str,
        hooks: Hooks<'_>,
    ) -> anyhow::Result<Transcript>;
}

/// Where the progress of the run in hand goes.
#[derive(Clone)]
enum Tap {
    None,
    /// A dictation: to the coordinator, and the run stops when every job is cancelled.
    Job(JobId),
    /// A recognition test: to the settings window as `TestProgress`.
    Test {
        id: u32,
        origin: Origin,
        reply: Pushes,
    },
}

/// The failure a failed model load is: memory, or a model that cannot be used.
fn load_failure(e: &anyhow::Error) -> ModelFailure {
    match e.downcast_ref::<oxi_whisper::Error>() {
        Some(oxi_whisper::Error::OutOfMemory) => ModelFailure::OutOfMemory,
        _ => ModelFailure::Bad,
    }
}

fn was_cancelled(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<oxi_whisper::Error>(),
        Some(oxi_whisper::Error::Cancelled)
    )
}

impl Recognizer for Engine {
    fn info(&self) -> &LoadInfo {
        Engine::info(self)
    }

    fn set_advanced(&mut self, adv: &Advanced) {
        Engine::set_advanced(self, adv);
    }

    fn set_token_times(&mut self, on: bool) {
        Engine::set_token_times(self, on);
    }

    fn transcribe(
        &mut self,
        samples: &[f32],
        language: Language,
        prompt: &str,
        hooks: Hooks<'_>,
    ) -> anyhow::Result<Transcript> {
        Ok(Engine::transcribe_with(
            self,
            samples,
            language,
            prompt,
            Some(hooks.progress),
            hooks.cancel,
        )?)
    }
}

/// Starts the worker thread. It starts on the default config; the coordinator sends the active
/// one first (`Coordinator::new`). Returns when `rx` closes.
pub fn spawn(
    rx: Receiver<WhisperCmd>,
    inbox: Sender<Inbox>,
    layout: Layout,
    phase: PhaseCell,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("whisper".into())
        .spawn(move || {
            let load =
                |path: &Path, choice: &DeviceChoice, adv: &Advanced| -> anyhow::Result<Engine> {
                    Ok(Engine::load_with(path, choice, adv)?)
                };
            let mut worker = Worker::new(load, inbox, layout, phase);
            while let Some(cmd) = worker.next_cmd(&rx) {
                worker.handle(cmd);
            }
        })
}

/// `Status.backend` while no model is loaded (the coordinator's value before the first load).
pub(super) const NO_BACKEND: &str = "none";
/// How long a model only a test asked for stays with no further test request.
const TEST_MODEL_IDLE: Duration = Duration::from_secs(60);

struct Worker<R, L> {
    load: L,
    inbox: Sender<Inbox>,
    layout: Layout,
    /// The machine's phase: a queued test must not run in front of a dictation.
    phase: PhaseCell,
    cfg: Config,
    assets: TextAssets,
    loaded: Option<R>,
    /// The settings of the last load, which are `loaded`'s while it is there.
    loaded_for: Option<ModelIdentity>,
    /// `loaded` came from a recognition test.
    test_loaded: bool,
    /// When the last recognition test ended.
    last_test: Instant,
    /// A test of a model that is not the configured one is running: what its load says about
    /// the device must not reach `Status`, which describes the configured model.
    quiet: bool,
    /// Why the GPU is off: set once a CPU load after a GPU failure succeeded, cleared when the
    /// model or device setting changes. Loads go to CPU while set.
    cpu_reason: Option<String>,
    /// The last `Inbox::EngineWarning` sent.
    reported: Option<EngineWarning>,
    /// Where the progress of the running job or test goes.
    tap: Tap,
    /// How much slower (or faster) than the first-guess fit this machine ran the last tests,
    /// for the estimate a test run starts its bar with.
    test_speed: f32,
    /// The run in hand is a word-times run ([`Worker::word_times`]); a dictation never is.
    want_times: bool,
    /// The model the next `Preload` loads (`UseModel`, from the app in front at key-down).
    next_model: Option<String>,
    /// What the dictation in hand uses of the user's text files (its profile).
    uses: Uses,
    /// App or language models that failed to load since the last `Config`: the main model
    /// stands in without trying them again at every dictation (d04-fix 1).
    failed: Vec<String>,
}

/// Which of the user's text files a dictation uses; a test uses both.
#[derive(Debug, Clone, Copy)]
struct Uses {
    dictionary: bool,
    replacements: bool,
}

impl Uses {
    const ALL: Self = Self {
        dictionary: true,
        replacements: true,
    };
}

impl<R: Recognizer, L: FnMut(&Path, &DeviceChoice, &Advanced) -> anyhow::Result<R>> Worker<R, L> {
    fn new(load: L, inbox: Sender<Inbox>, layout: Layout, phase: PhaseCell) -> Self {
        let assets = TextAssets::load(&layout);
        Self {
            load,
            inbox,
            layout,
            phase,
            cfg: Config::default(),
            assets,
            loaded: None,
            loaded_for: None,
            test_loaded: false,
            last_test: Instant::now(),
            quiet: false,
            cpu_reason: None,
            reported: None,
            tap: Tap::None,
            test_speed: 1.0,
            want_times: false,
            next_model: None,
            uses: Uses::ALL,
            failed: Vec::new(),
        }
    }

    /// When a model only a test asked for is due to go; `None` while there is none.
    fn test_deadline(&self) -> Option<Instant> {
        (self.test_loaded && self.loaded.is_some()).then(|| self.last_test + TEST_MODEL_IDLE)
    }

    /// The next command, or `TestDone` once a test-only model has sat idle for too long. `None`
    /// when `rx` closed.
    fn next_cmd(&self, rx: &Receiver<WhisperCmd>) -> Option<WhisperCmd> {
        let Some(deadline) = self.test_deadline() else {
            return rx.recv().ok();
        };
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(cmd) => Some(cmd),
            Err(RecvTimeoutError::Timeout) => Some(WhisperCmd::TestDone),
            Err(RecvTimeoutError::Disconnected) => None,
        }
    }

    fn handle(&mut self, cmd: WhisperCmd) {
        match cmd {
            WhisperCmd::UseModel(id) => {
                self.adopt_test_model();
                // A model not on disk never drops the loaded one: the job falls back to it.
                let id = self.usable(id);
                if self.loaded.is_some() && self.holds_other(&id) {
                    // Another model is in memory: load this one now, while the user speaks
                    // (arch-plan 9.2). The machine counts a model as loaded already; a failure
                    // is the dictation's, told at its transcription.
                    if self.on_model(&id, |w| w.ensure_loaded(None)).is_err() {
                        self.remember_failed(&id);
                    }
                } else {
                    self.next_model = Some(id);
                }
            }
            WhisperCmd::Preload => {
                self.adopt_test_model();
                let id = self
                    .next_model
                    .take()
                    .map_or_else(|| self.cfg.model.id.clone(), |id| self.usable(id));
                let mut loaded = self.on_model(&id, |w| w.ensure_loaded(None));
                if loaded.is_err() && id != self.cfg.model.id {
                    // An app's model that cannot load is that app's problem: the machine counts
                    // the configured model, and the job tells the fallback (`transcribe`).
                    self.remember_failed(&id);
                    loaded = self.ensure_loaded(None);
                }
                let event = match loaded {
                    Ok(()) => Event::ModelLoaded,
                    Err(failure) => Event::ModelFailed(failure),
                };
                let _ = self.inbox.send(Inbox::Event(event));
            }
            WhisperCmd::Unload => self.unload(),
            WhisperCmd::DeleteModel { id, reply } => {
                let result = self.delete_model(&id);
                reply.send(Push::ModelDeleted { id, result });
            }
            WhisperCmd::Test(job) => {
                let TestJob {
                    id,
                    set,
                    language,
                    samples,
                    origin,
                    reply,
                } = job;
                if let Some(record) = reply.words_for().cloned() {
                    // The player's word times: its own answer, no `TestResult`.
                    self.words_run(id, &record, &samples, &reply);
                    return;
                }
                let outcome = if self.phase.load() == Phase::Idle {
                    self.tap = Tap::Test {
                        id,
                        origin,
                        reply: reply.clone(),
                    };
                    let outcome = self.test(&samples, origin, &set, language);
                    self.tap = Tap::None;
                    outcome
                } else {
                    TestOutcome::Failed {
                        why: TestFailure::Busy,
                    }
                };
                self.last_test = Instant::now();
                reply.send(Push::TestResult { id, outcome });
            }
            WhisperCmd::TestDone => {
                if self.test_loaded {
                    self.unload();
                }
            }
            WhisperCmd::Transcribe {
                job,
                samples,
                profile,
            } => self.dictate(job, &samples, &profile),
            WhisperCmd::Config(cfg) => {
                if MachineConfig::from(&*cfg).model != self.identity() {
                    self.cpu_reason = None;
                    if self.loaded.is_none() {
                        self.report(None);
                    }
                }
                // Flash attention is a context parameter: the next job loads the model again,
                // but only when what it resolves to on the loaded device differs.
                if let Some(engine) = &self.loaded {
                    let on_gpu = engine.info().device.is_some();
                    let resolved =
                        |c: &Config| tuning::flash_attn(&c.model.advanced, on_gpu).unwrap_or(false);
                    if resolved(&cfg) != resolved(&self.cfg) {
                        self.loaded = None;
                        self.loaded_for = None;
                    }
                }
                self.cfg = *cfg;
                self.assets = TextAssets::load(&self.layout);
                // A new config may fix a model (re-downloaded, another device): try them again.
                self.failed.clear();
            }
            WhisperCmd::ReloadAssets => self.assets = TextAssets::load(&self.layout),
        }
    }

    /// One dictation job, as its profile says; answers `Inbox::Transcribed`.
    fn dictate(&mut self, job: JobId, samples: &[f32], profile: &Profile) {
        self.tap = Tap::Job(job);
        let started = Instant::now();
        let text = self.transcribe(samples, profile);
        self.tap = Tap::None;
        // Lengths only, never the text (AGENTS.md rule 5).
        let audio_ms = oxi_core::stats::audio_length(samples.len()).0;
        let took_ms = started.elapsed().as_millis();
        match &text {
            Ok(Some((t, _, _))) => {
                let chars = t.chars().count();
                tracing::info!(?job, audio_ms, took_ms, chars, "transcription done");
            }
            Ok(None) => tracing::info!(?job, took_ms, "transcription failed or cut"),
            Err(why) => tracing::info!(?job, ?why, "transcription had no model"),
        }
        let (text, words, held) = text
            .unwrap_or_else(|failure| {
                // First, so the machine leaves Loaded before this job ends: the jobs queued
                // behind it are not dispatched to a model that is not there, one notice.
                let _ = self.inbox.send(Inbox::Event(Event::ModelFailed(failure)));
                None
            })
            .map_or((None, None, None), |(t, w, h)| (Some(t), Some(w), Some(h)));
        let words = words.unwrap_or_default();
        let held = held.unwrap_or_default();
        let _ = self.inbox.send(Inbox::Transcribed {
            job,
            text,
            words,
            held,
        });
    }

    /// Deletes the files of registry model `id`. A model this worker holds is unloaded first,
    /// so its file is not open; the coordinator refused the request while a job runs.
    fn delete_model(&mut self, id: &str) -> DeleteResult {
        let Some(entry) = models::find(id) else {
            return DeleteResult::Failed {
                message: "unknown model".into(),
            };
        };
        if self.loaded_for.as_ref().is_some_and(|i| i.file == id) {
            self.unload();
        }
        // A deleted file is not a failed one: the next download starts clean.
        self.failed.retain(|f| f != id);
        let result = io::delete_model(&self.layout.models_dir(), entry);
        if matches!(result, DeleteResult::Done { .. }) && self.cfg.model.id == id {
            // The machine still counts the model as loaded: without this a dictation records,
            // then fails after the release instead of being refused at chord-down.
            let _ = self
                .inbox
                .send(Inbox::Event(Event::ModelFailed(ModelFailure::Missing)));
        }
        result
    }

    fn unload(&mut self) {
        self.loaded = None;
        // The pages the model touched stay resident (about 100 MB) until the system needs them.
        oxi_platform::os::trim_working_set();
        self.test_loaded = false;
        self.send_backend(NO_BACKEND.into());
        // A pinned CPU fallback still holds for the next load: keep saying so (no
        // tray flicker around each idle unload).
        if self.cpu_reason.is_none() {
            self.report(None);
        }
    }

    /// The settings a load would use now.
    fn identity(&self) -> ModelIdentity {
        MachineConfig::from(&self.cfg).model
    }

    /// A dictation keeps what a test left loaded only when that is the configured model, on the
    /// configured device.
    fn adopt_test_model(&mut self) {
        if std::mem::take(&mut self.test_loaded) && self.loaded_for != Some(self.identity()) {
            self.loaded = None;
        }
    }

    /// One recognition test run on `samples`. The audio stays in memory all the way: it is
    /// handed to whisper as a slice (`Recognizer::transcribe`), no temp file exists.
    fn test(
        &mut self,
        samples: &[f32],
        origin: Origin,
        set: &SettingsSet,
        language: Language,
    ) -> TestOutcome {
        if samples.is_empty() {
            return TestOutcome::Failed {
                why: TestFailure::NoAudio,
            };
        }
        // A stored recording went through gain, denoise, trim and normalise when the app saved it:
        // doing it again would double the gain.
        let processed;
        let clip: &[f32] = if origin.is_stored_audio() {
            samples
        } else {
            let opts = Audio {
                normalize: set.normalize,
                denoise: set.denoise,
                trim: set.trim,
                ..self.cfg.audio.clone()
            };
            let mut done = audio::process(samples, audio::SAMPLE_RATE, &opts);
            if origin.is_from_history() && matches!(done, Err(AudioError::NothingHeard)) {
                // The gate may miss a quiet word, and History keeps such a recording to be heard
                // again: send it whole instead of answering "nothing heard" a second time. A
                // Test-page clip of silence stays silence: whisper would invent a phrase.
                let whole = Audio {
                    trim: false,
                    ..opts
                };
                done = audio::process(samples, audio::SAMPLE_RATE, &whole);
            }
            processed = match done {
                Ok(clip) => clip,
                Err(AudioError::NothingHeard | AudioError::MicSilence) => {
                    return TestOutcome::Heard {
                        text: String::new(),
                        ms: 0,
                    };
                }
                Err(e) => {
                    tracing::warn!("recognition test audio failed: {e}");
                    return TestOutcome::Failed {
                        why: TestFailure::Failed,
                    };
                }
            };
            &processed
        };
        // The existing load and retry paths run on the set's model, then the config comes back.
        // A GPU failure of another model says nothing about the configured one: no CPU pin.
        let (active, pin) = (self.cfg.model.clone(), self.cpu_reason.clone());
        self.cfg.model.id.clone_from(&set.model);
        self.cfg.model.language = language;
        if self.loaded_for != Some(self.identity()) {
            self.loaded = None;
        }
        if self.loaded.is_none() {
            self.test_loaded = true;
        }
        self.quiet = set.model != active.id;
        let outcome = self.run_test(clip);
        self.quiet = false;
        if set.model != active.id {
            self.cpu_reason = pin;
        }
        self.cfg.model = active;
        outcome
    }

    fn run_test(&mut self, clip: &[f32]) -> TestOutcome {
        let model_failed = |failure| TestOutcome::Failed {
            why: match failure {
                ModelFailure::Missing => TestFailure::ModelMissing,
                ModelFailure::Bad => TestFailure::ModelBad,
                ModelFailure::OutOfMemory => TestFailure::Failed,
            },
        };
        if let Err(failure) = self.ensure_loaded(None) {
            return model_failed(failure);
        }
        let started = Instant::now();
        let transcript = self.recognise(clip);
        let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match transcript {
            Ok(Some(transcript)) => {
                // A Transcribe again keeps these words with its text (`HistorySetText`).
                let again =
                    matches!(&self.tap, Tap::Test { origin, .. } if origin.is_from_history());
                if let Tap::Test { id, reply, .. } = &self.tap
                    && again
                {
                    reply.send(Push::HistoryRunWords {
                        id: *id,
                        words: Words(super::history::words_of(&transcript)),
                    });
                }
                // Transcribe again is the user asking for this clip: the hallucination checks
                // take nothing out of it.
                let mut opts = self.text_options(false, clip.len());
                opts.again = again;
                TestOutcome::Heard {
                    text: self.final_text(&transcript, &opts).0,
                    ms,
                }
            }
            Ok(None) => TestOutcome::Failed {
                why: TestFailure::Failed,
            },
            Err(failure) => model_failed(failure),
        }
    }

    /// The word-times run of History record `record` (`clip` is its audio): the times go into the
    /// record, and the window gets the words with them, or why it gets none.
    fn words_run(&mut self, run: u32, record: &HistoryId, clip: &[f32], reply: &Pushes) {
        // As a test run: a model loaded for it is a test's, dropped after the idle limit (arch-plan
        // 8.3); a model a dictation or a test left loaded keeps what it was.
        if self.loaded_for != Some(self.identity()) {
            self.loaded = None;
        }
        self.test_loaded |= self.loaded.is_none();
        self.tap = Tap::Test {
            id: run,
            origin: Origin::History,
            reply: reply.clone(),
        };
        let ran = if reply.words_stopped() {
            Err(TestFailure::Busy)
        } else {
            self.word_times(clip)
        };
        self.tap = Tap::None;
        self.last_test = Instant::now();
        let unavailable = |why| WordsResult::Unavailable { why };
        let result = match ran {
            Ok(transcript) => self.store_words(record, &transcript),
            Err(TestFailure::Busy) => WordsResult::Busy,
            Err(TestFailure::ModelMissing | TestFailure::ModelBad) => {
                unavailable(WordsFailure::ModelMissing)
            }
            Err(TestFailure::NoAudio) => unavailable(WordsFailure::NoAudio),
            Err(TestFailure::Failed) => unavailable(WordsFailure::Failed),
        };
        reply.send(Push::HistoryWords {
            run,
            id: record.clone(),
            result,
        });
    }

    /// Writes the timed words of `transcript` into record `record`; no times is a failure.
    fn store_words(&self, record: &HistoryId, transcript: &Transcript) -> WordsResult {
        let words = super::history::words_of(transcript);
        let failed = WordsResult::Unavailable {
            why: WordsFailure::Failed,
        };
        if !words.iter().any(|w| w.t0.is_some()) {
            return failed;
        }
        let dir = oxi_core::history::folder(&self.cfg.history, &self.layout.history_dir());
        let how = super::history::timings_of(oxi_whisper::Timing::Tokens, &self.cfg.model.id);
        match super::history::store_timed_words(&dir, record, words.clone(), how) {
            Ok(()) => WordsResult::Ready {
                words: Words(words),
                timed: true,
            },
            Err(e) => {
                tracing::warn!(kind = ?e.kind(), "word times not stored");
                failed
            }
        }
    }

    /// Word times of `clip`, a stored (already processed) recording, from token timestamps of
    /// the configured model, in the slot of a test: the live engine, flash attention as it is.
    /// The times are on for this run only, so a dictation after it never pays for them. A
    /// dictation goes first (arch-plan 8.3): one under way, or one that starts during the
    /// run (which then stops), answers `Busy`.
    fn word_times(&mut self, clip: &[f32]) -> Result<Transcript, TestFailure> {
        if self.phase.load() != Phase::Idle {
            return Err(TestFailure::Busy);
        }
        self.want_times = true;
        let ran = self.ensure_loaded(None).map(|()| self.recognise(clip));
        self.want_times = false;
        if let Some(engine) = self.loaded.as_mut() {
            engine.set_token_times(false);
        }
        let stopped = matches!(&self.tap, Tap::Test { reply, .. } if reply.words_stopped());
        match ran {
            Ok(Ok(Some(transcript))) => Ok(transcript),
            Ok(Ok(None)) if stopped || self.phase.load() != Phase::Idle => Err(TestFailure::Busy),
            Ok(Ok(None)) => Err(TestFailure::Failed),
            Ok(Err(failure)) | Err(failure) => Err(match failure {
                ModelFailure::Missing => TestFailure::ModelMissing,
                ModelFailure::Bad => TestFailure::ModelBad,
                ModelFailure::OutOfMemory => TestFailure::Failed,
            }),
        }
    }

    /// Loads the configured model unless one is loaded (the state machine unloads a stale one)
    /// and reports what the load says about the device; a failed load clears the warning.
    /// `gpu_failed`: the GPU just failed mid-job for this reason, so load on CPU now.
    fn ensure_loaded(&mut self, gpu_failed: Option<String>) -> Result<(), ModelFailure> {
        if self.loaded.is_some() {
            return Ok(());
        }
        let warning = self.load_model(gpu_failed);
        if warning.is_err() {
            // Nothing is loaded: do not leave the last model's device in Status.
            self.send_backend(NO_BACKEND.into());
        }
        self.report(warning.clone().unwrap_or(None));
        warning.map(drop)
    }

    fn load_model(
        &mut self,
        gpu_failed: Option<String>,
    ) -> Result<Option<EngineWarning>, ModelFailure> {
        let models_dir = self.layout.models_dir();
        let model = pipeline::resolve_model(&self.cfg.model, &models_dir)?;
        pipeline::verify_stamped(&model, &models_dir)?;
        let mut reason = gpu_failed.or_else(|| self.cpu_reason.clone());
        let choice = if reason.is_some() {
            DeviceChoice::Cpu
        } else {
            self.cfg.model.gpu.clone()
        };
        let adv = self.cfg.model.advanced.clone();
        let engine = match (self.load)(&model.path, &choice, &adv) {
            Ok(engine) => engine,
            Err(e) if choice != DeviceChoice::Cpu => {
                tracing::warn!("model load on the GPU failed ({e:#}), retrying on CPU");
                reason = Some(e.to_string());
                (self.load)(&model.path, &DeviceChoice::Cpu, &adv).map_err(|e| {
                    tracing::warn!("model load on CPU failed: {e:#}");
                    load_failure(&e)
                })?
            }
            Err(e) => {
                tracing::warn!("model load failed: {e:#}");
                return Err(load_failure(&e));
            }
        };
        let info = engine.info();
        let device = info.device.clone().unwrap_or_else(|| "cpu".into());
        self.send_backend(device);
        // The engine's own GPU-to-CPU retry pins CPU too; CPU with no GPU to fail is no warning.
        if reason.is_none()
            && let Some(EngineWarning::CpuFallback { reason: why }) = &info.warning
        {
            reason = Some(why.clone());
        }
        let warning = match &reason {
            Some(why) => Some(EngineWarning::CpuFallback {
                reason: why.clone(),
            }),
            None => info.warning.clone(),
        };
        if let Some(w) = &warning {
            tracing::warn!("{}", strings::engine_warning(w));
        }
        self.cpu_reason = reason;
        self.loaded = Some(engine);
        self.loaded_for = Some(self.identity());
        Ok(warning)
    }

    fn send_backend(&self, device: String) {
        if !self.quiet {
            let _ = self.inbox.send(Inbox::Backend(device));
        }
    }

    /// Sends `warning` when it differs from the last one sent.
    fn report(&mut self, warning: Option<EngineWarning>) {
        if !self.quiet && warning != self.reported {
            self.reported.clone_from(&warning);
            let _ = self.inbox.send(Inbox::EngineWarning(warning));
        }
    }

    /// Runs `f` with model `id` as the configured one: a model loaded for another file is
    /// dropped first (the unload policy is the machine's and is not touched), and `id` is put
    /// back after. What stays loaded is what `loaded_for` says.
    fn on_model<T>(&mut self, id: &str, f: impl FnOnce(&mut Self) -> T) -> T {
        if self.loaded.is_some() && self.holds_other(id) {
            self.loaded = None;
        }
        let active = std::mem::replace(&mut self.cfg.model.id, id.to_owned());
        let pin = self.cpu_reason.clone();
        let out = f(self);
        // A GPU failure of an app's model says nothing about the configured one: its CPU pin
        // goes, like a test's (review d04 #2).
        if active != id {
            self.cpu_reason = pin;
        }
        self.cfg.model.id = active;
        out
    }

    /// `id`, or the configured model when `id` is another model that is not on disk (no hash,
    /// no load: a stamp and a file check). A rule or language entry naming a model that was
    /// never downloaded must not stop dictation (review d04-profiles item 1).
    fn usable(&self, id: String) -> String {
        if id == self.cfg.model.id {
            return id;
        }
        if self.failed.contains(&id) {
            return self.cfg.model.id.clone();
        }
        let model = oxi_core::config::Model {
            id: id.clone(),
            ..self.cfg.model.clone()
        };
        let dir = self.layout.models_dir();
        let on_disk = pipeline::resolve_model(&model, &dir).is_ok_and(|m| {
            m.entry.is_none_or(|e| {
                matches!(
                    models::status(e, &dir),
                    models::ModelStatus::Ready | models::ModelStatus::NeedsHash
                )
            })
        });
        if on_disk {
            id
        } else {
            tracing::warn!(model = %id, main = %self.cfg.model.id, "app model not on disk, using the main model");
            self.cfg.model.id.clone()
        }
    }

    /// Model `id` (not the configured one) failed to load: [`Worker::usable`] skips it until
    /// the next `Config`.
    fn remember_failed(&mut self, id: &str) {
        if id != self.cfg.model.id && !self.failed.iter().any(|f| f == id) {
            self.failed.push(id.to_owned());
        }
    }

    /// What is loaded is another file than model `id` would be.
    fn holds_other(&mut self, id: &str) -> bool {
        let active = std::mem::replace(&mut self.cfg.model.id, id.to_owned());
        let file = self.identity().file;
        self.cfg.model.id = active;
        self.loaded_for.as_ref().is_some_and(|l| l.file != file)
    }

    /// Runs `f` as the dictation's `profile` says: `model`, its language, dictionary, replace
    /// rules and pause punctuation stand in for the configured ones, and are put back after.
    fn with_profile<T>(
        &mut self,
        profile: &Profile,
        model: &str,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let language = std::mem::replace(&mut self.cfg.model.language, profile.language);
        let pause = std::mem::replace(
            &mut self.cfg.text.pause_punctuation,
            profile.text.pause_punctuation,
        );
        self.uses = Uses {
            dictionary: profile.dictionary,
            replacements: profile.replacements,
        };
        let out = self.on_model(model, f);
        self.uses = Uses::ALL;
        self.cfg.text.pause_punctuation = pause;
        self.cfg.model.language = language;
        out
    }

    /// `Ok(Some(""))` is nothing heard, `Ok(None)` a whisper failure, `Err` no model to run. The
    /// text comes with whisper's words and their confidence, and the parts the confidence filter
    /// held back, for the history entry. A profile model that cannot be used gives way to the
    /// configured one, and the machine hears of it (`Event::ModelFellBack`), never `ModelFailed`.
    fn transcribe(
        &mut self,
        samples: &[f32],
        profile: &Profile,
    ) -> Result<Option<Transcribed>, ModelFailure> {
        self.adopt_test_model();
        let main = self.cfg.model.id.clone();
        let model = self.usable(profile.model.clone());
        let run = |w: &mut Self| {
            Ok(w.recognise(samples)?.map(|t| {
                let mut opts = profile.text.clone();
                opts.clip_ms = Some(oxi_core::stats::audio_length(samples.len()).0);
                // A held part needs a history entry to live in. This config waits for idle, so
                // an invention is always held: the coordinator drops it when no entry is written.
                opts.can_hold = w.cfg.history.enabled;
                opts.hold_invented = true;
                let (text, held) = w.final_text(&t, &opts);
                (text, super::history::words_of(&t), held)
            }))
        };
        let mut out = self.with_profile(profile, &model, run);
        let mut fell_back = model != profile.model;
        if let Err(why) = &out
            && model != main
        {
            tracing::warn!(model = %model, ?why, "app model failed to load, using the main model");
            self.remember_failed(&model);
            out = self.with_profile(profile, &main, run);
            fell_back = true;
        }
        if fell_back {
            let _ = self
                .inbox
                .send(Inbox::Event(Event::ModelFellBack(profile.model.clone())));
        }
        out
    }

    /// `Ok(None)` is a whisper failure, `Err` no model to run.
    fn recognise(&mut self, samples: &[f32]) -> Result<Option<Transcript>, ModelFailure> {
        self.ensure_loaded(None)?;
        let transcript = match self.run(samples) {
            Ok(t) => t,
            Err(e) if was_cancelled(&e) => return Ok(None),
            Err(e) => {
                let on_gpu = self
                    .loaded
                    .take()
                    .is_some_and(|engine| engine.info().device.is_some());
                if !on_gpu {
                    tracing::warn!("transcription failed: {e:#}");
                    return Ok(None);
                }
                // ARCHITECTURE 11 item 25: this job again on CPU, then CPU until a config change.
                tracing::warn!("transcription on the GPU failed ({e:#}), retrying on CPU");
                self.ensure_loaded(Some(e.to_string()))?;
                match self.run(samples) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!("transcription failed: {e:#}");
                        return Ok(None);
                    }
                }
            }
        };
        Ok(Some(transcript))
    }

    /// The `[text]` options for a clip of `samples`; nothing held (`can_hold` false).
    fn text_options(&self, trailing_space: bool, samples: usize) -> TextOptions {
        let mut opts = TextOptions::from_config(&self.cfg.text, trailing_space);
        // A typed line break would send a message; the clipboard keeps it. A clipboard paste
        // that ends up typed (an app rule, a clipboard that cannot be saved) turns it into a
        // space in the paste worker (`paste::type_text`), never an Enter.
        opts.newlines_allowed = self.cfg.paste.mode == PasteMode::Clipboard;
        opts.clip_ms = Some(oxi_core::stats::audio_length(samples).0);
        opts
    }

    /// Empty when nothing is left to paste.
    fn final_text(
        &self,
        transcript: &Transcript,
        opts: &TextOptions,
    ) -> (String, Vec<oxi_core::history::HeldPart>) {
        let text = &self.cfg.text;
        let audio_ms = opts.clip_ms.unwrap_or(0);
        let bare;
        let assets = if self.uses.replacements {
            &self.assets
        } else {
            // No replace rules for this app: the same assets without them.
            bare = TextAssets::default();
            &bare
        };
        let done = pipeline::finish_with(transcript, assets, text, opts);
        let chars = match &done.outcome {
            Outcome::Text(t) => t.chars().count(),
            Outcome::NothingHeard => 0,
        };
        // Counts only, never the text (AGENTS.md rule 5).
        tracing::info!(
            segments = transcript.segments.len(),
            dropped_hallucination = done.dropped_hallucination,
            dropped_implausible = done.dropped_implausible,
            held = done.held.len(),
            chars,
            "text pipeline"
        );
        if audio_ms >= SPARSE_MIN_MS && (chars as u64) * 1000 < audio_ms * SPARSE_CPS {
            tracing::warn!(audio_ms, chars, "sparse transcript");
        }
        let ms = |t: oxi_core::types::Millis| u32::try_from(t.0).unwrap_or(u32::MAX);
        let held = done
            .held
            .iter()
            .map(|h| oxi_core::history::HeldPart {
                text: h.text.clone(),
                t0: ms(h.t0),
                t1: ms(h.t1),
                why: h.why,
            })
            .collect();
        match done.outcome {
            Outcome::Text(text) => (text, held),
            Outcome::NothingHeard => (String::new(), held),
        }
    }

    fn run(&mut self, samples: &[f32]) -> anyhow::Result<Transcript> {
        let engine = self
            .loaded
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no model loaded"))?;
        let tap = self.tap.clone();
        let inbox = self.inbox.clone();
        let phase = self.phase.clone();
        let audio_ms = samples.len() as u64 * 1000 / u64::from(audio::SAMPLE_RATE);
        if let Tap::Test { id, reply, .. } = &tap {
            // The window's bar starts from this, before whisper says anything.
            let est = transcribe_est(audio_ms, self.test_speed).0;
            reply.send(Push::TestProgress {
                id: *id,
                percent: 0,
                est_ms: Some(u32::try_from(est).unwrap_or(u32::MAX)),
            });
        }
        let mut progress = |percent: u8| match &tap {
            Tap::None => {}
            Tap::Job(job) => {
                let _ = inbox.send(Inbox::Progress { job: *job, percent });
            }
            Tap::Test { id, reply, .. } => {
                reply.send(Push::TestProgress {
                    id: *id,
                    percent,
                    est_ms: None,
                });
            }
        };
        // Every job cancelled leaves the phase Idle; a test only runs while it is Idle already. A
        // timing run stops for a dictation (it leaves Idle) and when the window stops it.
        let is_job = matches!(self.tap, Tap::Job(_));
        let timing = self.want_times;
        let stop = match &tap {
            Tap::Test { reply, .. } => reply.words_stop().cloned(),
            _ => None,
        };
        let cancel = move || {
            let idle = phase.load() == Phase::Idle;
            let stopped = stop.as_ref().is_some_and(|s| s.load(Ordering::Acquire));
            if is_job {
                idle
            } else {
                timing && (!idle || stopped)
            }
        };
        let hooks = Hooks {
            progress: &mut progress,
            cancel: Some(&cancel),
        };
        engine.set_advanced(&self.cfg.model.advanced);
        // Pause punctuation reads the word times; they cost nothing while it is off.
        engine.set_token_times(self.want_times || self.cfg.text.pause_punctuation);
        let dictionary = if self.uses.dictionary {
            self.assets.prompt.as_str()
        } else {
            ""
        };
        let prompt = tuning::prompt(&self.cfg.model.advanced, dictionary);
        let started = Instant::now();
        let transcript = engine.transcribe(samples, self.cfg.model.language, &prompt, hooks);
        if matches!(tap, Tap::Test { .. }) && transcript.is_ok() {
            let took = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            self.test_speed = learn(self.test_speed, audio_ms, took);
        }
        transcript
    }
}

#[cfg(test)]
mod tests;
