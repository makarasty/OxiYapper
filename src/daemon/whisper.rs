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
use super::ipc::Pushes;
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
                    reply,
                } = job;
                if let Some(record) = reply.words_for().map(str::to_owned) {
                    // The player's word times: its own answer, no `TestResult`.
                    self.words_run(id, &record, &samples, &reply);
                    return;
                }
                let outcome = if self.phase.load() == Phase::Idle {
                    self.tap = Tap::Test {
                        id,
                        reply: reply.clone(),
                    };
                    let outcome = self.test(&samples, &reply, &set, language);
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
        origin: &Pushes,
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
                let again = matches!(&self.tap, Tap::Test { reply, .. } if reply.is_from_history());
                if let Tap::Test { id, reply } = &self.tap
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
    fn words_run(&mut self, run: u32, record: &str, clip: &[f32], reply: &Pushes) {
        // As a test run: a model loaded for it is a test's, dropped after the idle limit (arch-plan
        // 8.3); a model a dictation or a test left loaded keeps what it was.
        if self.loaded_for != Some(self.identity()) {
            self.loaded = None;
        }
        self.test_loaded |= self.loaded.is_none();
        self.tap = Tap::Test {
            id: run,
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
            id: record.to_owned(),
            result,
        });
    }

    /// Writes the timed words of `transcript` into record `record`; no times is a failure.
    fn store_words(&self, record: &str, transcript: &Transcript) -> WordsResult {
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
        if let Tap::Test { id, reply } = &tap {
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
            Tap::Test { id, reply } => {
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
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::mpsc::{Receiver, channel};

    use oxi_core::machine::JobId;
    use oxi_core::paths::{BaseDirs, Os, layout};
    use oxi_core::types::Millis;
    use oxi_whisper::Segment;

    use super::*;
    use crate::daemon::ipc::Origin;
    use crate::log::scratch::{fake_model, scratch_dir};

    /// What a fake load was asked for, and what its engine does.
    #[derive(Clone)]
    #[allow(
        clippy::struct_excessive_bools,
        reason = "independent failure switches of a test fake"
    )]
    struct Script {
        calls: Rc<RefCell<Vec<DeviceChoice>>>,
        /// Loads on any device other than CPU fail.
        gpu_load_fails: bool,
        /// Loads of this model file on any device other than CPU fail.
        gpu_fails_on: Option<&'static str>,
        /// Loads of this model file fail on every device.
        fails_on: Option<&'static str>,
        /// Loads on CPU fail.
        cpu_load_fails: bool,
        /// Loads on CPU fail the way whisper says memory ran out.
        cpu_oom: bool,
        /// `LoadInfo.warning` of a GPU engine (a named device gone).
        load_warning: Option<EngineWarning>,
        /// The first transcription on a GPU engine fails.
        gpu_run_fails: bool,
        /// A GPU request is answered by an engine on CPU (the real `Engine::load` fallback).
        silent_cpu: bool,
        /// No GPU at all (CPU-only build): a GPU request runs on CPU with no warning.
        no_gpu: bool,
        /// The samples every fake engine was handed, in order.
        heard: Rc<RefCell<Vec<Vec<f32>>>>,
        /// A dictation starts (this phase leaves Idle) in the middle of every run.
        overtake: Rc<RefCell<Option<PhaseCell>>>,
        /// What every fake engine hears.
        said: &'static str,
    }

    type Loader = Box<dyn FnMut(&Path, &DeviceChoice, &Advanced) -> anyhow::Result<Fake>>;
    type TestWorker = Worker<Fake, Loader>;

    struct Fake {
        gpu: bool,
        info: LoadInfo,
        fail_run: bool,
        heard: Rc<RefCell<Vec<Vec<f32>>>>,
        /// Word times are on.
        times: bool,
        overtake: Rc<RefCell<Option<PhaseCell>>>,
        said: &'static str,
    }

    impl Recognizer for Fake {
        fn info(&self) -> &LoadInfo {
            &self.info
        }
        fn set_token_times(&mut self, on: bool) {
            self.times = on;
        }
        fn transcribe(
            &mut self,
            samples: &[f32],
            _: Language,
            _: &str,
            hooks: Hooks<'_>,
        ) -> anyhow::Result<Transcript> {
            self.heard.borrow_mut().push(samples.to_vec());
            (hooks.progress)(40);
            if let Some(phase) = &*self.overtake.borrow() {
                phase.store(Phase::Recording);
            }
            if hooks.cancel.is_some_and(|cancel| cancel()) {
                return Err(oxi_whisper::Error::Cancelled.into());
            }
            if self.fail_run && self.gpu {
                anyhow::bail!("device lost");
            }
            Ok(Transcript {
                segments: vec![Segment {
                    text: format!(" {} ", self.said),
                    t0: Millis(0),
                    t1: Millis(1),
                    no_speech_prob: 0.0,
                    words: vec![oxi_whisper::Word {
                        text: "hello".into(),
                        p: 0.9,
                        t0: self.times.then_some(Millis(100)),
                        t1: self.times.then_some(Millis(400)),
                    }],
                }],
            })
        }
    }

    impl Script {
        fn new() -> Self {
            Self {
                calls: Rc::default(),
                gpu_load_fails: false,
                gpu_fails_on: None,
                fails_on: None,
                cpu_load_fails: false,
                cpu_oom: false,
                load_warning: None,
                gpu_run_fails: false,
                silent_cpu: false,
                no_gpu: false,
                heard: Rc::default(),
                overtake: Rc::default(),
                said: "hello",
            }
        }

        fn loader(&self) -> Loader {
            let s = self.clone();
            Box::new(move |path, choice, _| {
                s.calls.borrow_mut().push(choice.clone());
                let asked_gpu = *choice != DeviceChoice::Cpu;
                let cpu = !asked_gpu || s.silent_cpu || s.no_gpu;
                if cpu && s.cpu_oom {
                    return Err(oxi_whisper::Error::OutOfMemory.into());
                }
                let gpu_fails =
                    s.gpu_load_fails || s.gpu_fails_on.is_some_and(|f| path.ends_with(f));
                let file_fails = s.fails_on.is_some_and(|f| path.ends_with(f));
                if !cpu && gpu_fails || cpu && s.cpu_load_fails || file_fails {
                    anyhow::bail!("out of memory");
                }
                Ok(Fake {
                    gpu: !cpu,
                    fail_run: s.gpu_run_fails,
                    heard: s.heard.clone(),
                    times: false,
                    overtake: s.overtake.clone(),
                    said: s.said,
                    info: LoadInfo {
                        device: (!cpu).then(|| "Vulkan0".into()),
                        warning: if asked_gpu && s.silent_cpu {
                            Some(oom())
                        } else {
                            s.load_warning.clone().filter(|_| !cpu)
                        },
                    },
                })
            })
        }

        fn calls(&self) -> Vec<DeviceChoice> {
            self.calls.borrow().clone()
        }
    }

    fn oom() -> EngineWarning {
        EngineWarning::CpuFallback {
            reason: "out of memory".into(),
        }
    }

    struct Rig {
        dir: PathBuf,
        model: PathBuf,
    }

    fn rig(name: &str) -> Rig {
        let dir = scratch_dir(&format!("oxi-whisper-{}-{name}", std::process::id()));
        let model = dir.join("mine.bin");
        fs::write(&model, b"x").unwrap();
        Rig { dir, model }
    }

    impl Rig {
        fn layout(&self) -> Layout {
            let base = BaseDirs {
                config: self.dir.join("c"),
                data: self.dir.join("d"),
                logs: self.dir.join("l"),
            };
            layout(Os::Windows, &base, "x")
        }

        fn cfg(&self, gpu: DeviceChoice) -> Box<Config> {
            let mut cfg = Config::default();
            cfg.model.id = "custom".into();
            cfg.model.custom_path = self.model.to_str().unwrap().into();
            cfg.model.gpu = gpu;
            Box::new(cfg)
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn worker(script: &Script, rig: &Rig) -> (TestWorker, Receiver<Inbox>) {
        let (tx, rx) = channel();
        let phase = PhaseCell::default();
        (Worker::new(script.loader(), tx, rig.layout(), phase), rx)
    }

    /// The next message that is not an `EngineWarning`, a `Backend` or a `Progress`.
    fn next(rx: &Receiver<Inbox>) -> Option<Inbox> {
        rx.try_iter().find(|m| {
            !matches!(
                m,
                Inbox::EngineWarning(_) | Inbox::Backend(_) | Inbox::Progress { .. }
            )
        })
    }

    fn event(rx: &Receiver<Inbox>) -> Event {
        match next(rx) {
            Some(Inbox::Event(e)) => e,
            _ => panic!("expected an Event"),
        }
    }

    fn text(rx: &Receiver<Inbox>) -> Option<String> {
        match next(rx) {
            Some(Inbox::Transcribed { text, .. }) => text,
            _ => panic!("expected Transcribed"),
        }
    }

    /// Every `EngineWarning` sent so far; drains the other messages too.
    fn warnings(rx: &Receiver<Inbox>) -> Vec<Option<EngineWarning>> {
        rx.try_iter()
            .filter_map(|m| match m {
                Inbox::EngineWarning(w) => Some(w),
                _ => None,
            })
            .collect()
    }

    /// A dictation job as the daemon runs it: the machine is Transcribing (Idle means every job
    /// was cancelled, which stops the run).
    fn job(w: &mut TestWorker) {
        w.phase.store(Phase::Transcribing);
        w.handle(WhisperCmd::Transcribe {
            job: JobId(1),
            samples: vec![0.0; 16],
            profile: Box::new(oxi_core::paste::profile(&w.cfg, "")),
        });
    }

    #[test]
    fn a_profile_picks_the_model_per_dictation_and_the_configured_one_comes_back() {
        let (r, s) = (rig("profile"), Script::new());
        let (mut w, rx) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        w.handle(WhisperCmd::Preload);
        assert_eq!(event(&rx), Event::ModelLoaded);
        // A rule naming "tiny", which is not on disk: the press keeps the loaded model, the job
        // runs on it and says so; never `ModelFailed`, which would refuse every press (d04-fix 1).
        w.handle(WhisperCmd::UseModel("tiny".into()));
        let other = profile_for(&w, "tiny");
        w.phase.store(Phase::Transcribing);
        w.handle(WhisperCmd::Transcribe {
            job: JobId(1),
            samples: vec![0.0; 16],
            profile: Box::new(other),
        });
        assert_eq!(event(&rx), Event::ModelFellBack("tiny".into()));
        assert_eq!(text(&rx).as_deref(), Some("hello "));
        assert_eq!(s.calls().len(), 1, "the configured model stayed loaded");
        assert_eq!(w.cfg.model.id, "custom", "the configured model is put back");
        job(&mut w);
        assert_eq!(text(&rx).as_deref(), Some("hello "));
        assert_eq!(next(&rx).map(|_| ()), None, "no fallback without a rule");
        // Nothing loaded: the key-down preload of the missing model loads the configured one.
        w.handle(WhisperCmd::Unload);
        w.handle(WhisperCmd::UseModel("tiny".into()));
        w.handle(WhisperCmd::Preload);
        assert_eq!(event(&rx), Event::ModelLoaded);
        assert!(w.next_model.is_none());
        assert_eq!(
            w.loaded_for.as_ref().unwrap().file,
            r.cfg(DeviceChoice::Cpu).model.custom_path
        );
    }

    /// d04-fix 1: an app's model on disk that fails to load gives way to the configured one, at
    /// the preload and in the job, and the machine hears a fallback, not a model failure.
    #[test]
    fn an_apps_model_that_fails_to_load_falls_back_to_the_configured_one() {
        let (r, mut s) = (rig("rulebad"), Script::new());
        s.fails_on = Some(on_disk(&r, "tiny"));
        let (mut w, rx) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        w.handle(WhisperCmd::UseModel("tiny".into()));
        w.handle(WhisperCmd::Preload);
        assert_eq!(event(&rx), Event::ModelLoaded);
        assert_eq!(w.loaded_for.as_ref().unwrap().file, w.cfg.model.custom_path);
        w.phase.store(Phase::Transcribing);
        let profile = Box::new(profile_for(&w, "tiny"));
        w.handle(WhisperCmd::Transcribe {
            job: JobId(1),
            samples: vec![0.0; 16],
            profile,
        });
        assert_eq!(event(&rx), Event::ModelFellBack("tiny".into()));
        assert_eq!(text(&rx).as_deref(), Some("hello "));
        assert_eq!(w.cfg.model.id, "custom");
        // d04-fix review 1: the next dictation in that app does not try the broken file again
        // (no load at key-down, none in the job), until a new config.
        let loads = s.calls().len();
        let dictate = |w: &mut TestWorker| {
            w.handle(WhisperCmd::UseModel("tiny".into()));
            w.handle(WhisperCmd::Preload);
            assert_eq!(event(&rx), Event::ModelLoaded);
            let profile = Box::new(profile_for(w, "tiny"));
            w.handle(WhisperCmd::Transcribe {
                job: JobId(2),
                samples: vec![0.0; 16],
                profile,
            });
            assert_eq!(event(&rx), Event::ModelFellBack("tiny".into()));
            assert_eq!(text(&rx).as_deref(), Some("hello "));
        };
        dictate(&mut w);
        assert_eq!(
            s.calls().len(),
            loads,
            "the failed model is not loaded again"
        );
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        dictate(&mut w);
        assert!(s.calls().len() > loads, "a new config tries it once more");
    }

    /// d04-fix re-review: deleting a model that failed to load takes it off the failed list, so
    /// the download after it is tried at once.
    #[test]
    fn deleting_a_failed_model_takes_it_off_the_failed_list() {
        let (r, s) = (rig("faildel"), Script::new());
        on_disk(&r, "tiny");
        let (mut w, _rx) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        w.remember_failed("tiny");
        assert_eq!(
            w.usable("tiny".into()),
            w.cfg.model.id,
            "skipped while failed"
        );
        let _ = w.delete_model("tiny");
        assert_eq!(w.failed, Vec::<String>::new());
    }

    /// Registry model `id` on disk as a sparse fake with its stamp, so it loads.
    fn on_disk(r: &Rig, id: &str) -> &'static str {
        let entry = models::find(id).unwrap();
        let dir = r.layout().models_dir();
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join(entry.file_name);
        fake_model(&file, entry.size);
        models::write_stamp(&file).unwrap();
        entry.file_name
    }

    fn profile_for(w: &TestWorker, model: &str) -> Profile {
        let mut p = oxi_core::paste::profile(&w.cfg, "");
        p.model = model.into();
        p
    }

    /// Review d04 #1: an app's model is loaded at key-down while the user speaks, not after the
    /// release; its jobs then run on it with no load, and the configured one comes back the
    /// same way.
    #[test]
    fn the_apps_model_loads_at_key_down_and_its_job_loads_nothing() {
        let (r, s) = (rig("usemodel"), Script::new());
        on_disk(&r, "tiny");
        let (mut w, rx) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        w.handle(WhisperCmd::Preload);
        assert_eq!(event(&rx), Event::ModelLoaded);
        w.handle(WhisperCmd::UseModel("tiny".into()));
        assert_eq!(s.calls().len(), 2, "loaded at the press");
        assert_eq!(w.loaded_for.as_ref().unwrap().file, "tiny");
        w.phase.store(Phase::Transcribing);
        let profile = Box::new(profile_for(&w, "tiny"));
        w.handle(WhisperCmd::Transcribe {
            job: JobId(1),
            samples: vec![0.0; 16],
            profile,
        });
        assert_eq!(text(&rx).as_deref(), Some("hello "));
        assert_eq!(s.calls().len(), 2, "the job found it loaded");
        // The same model again: nothing to load. The configured one: loaded at its press.
        w.handle(WhisperCmd::UseModel("tiny".into()));
        assert_eq!(s.calls().len(), 2);
        w.handle(WhisperCmd::UseModel("custom".into()));
        assert_eq!(s.calls().len(), 3);
        job(&mut w);
        assert_eq!(text(&rx).as_deref(), Some("hello "));
        assert_eq!(s.calls().len(), 3);
        // Nothing loaded (idle unload): the machine's preload loads the app's model.
        w.handle(WhisperCmd::Unload);
        w.handle(WhisperCmd::UseModel("tiny".into()));
        assert_eq!(s.calls().len(), 3, "the preload does it");
        w.handle(WhisperCmd::Preload);
        assert_eq!(event(&rx), Event::ModelLoaded);
        assert_eq!(w.loaded_for.as_ref().unwrap().file, "tiny");
    }

    /// Review d04 #2: an app's model that ran out of GPU memory falls back to the CPU for itself;
    /// the configured model loads on the GPU again afterwards.
    #[test]
    fn an_apps_model_on_the_cpu_leaves_the_configured_one_on_the_gpu() {
        let (r, mut s) = (rig("rulepin"), Script::new());
        s.gpu_fails_on = Some(on_disk(&r, "tiny"));
        let (mut w, rx) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Auto)));
        w.handle(WhisperCmd::Preload);
        assert_eq!(event(&rx), Event::ModelLoaded);
        w.handle(WhisperCmd::UseModel("tiny".into()));
        assert_eq!(
            s.calls()[1..],
            [DeviceChoice::Auto, DeviceChoice::Cpu],
            "tiny went to the CPU"
        );
        assert!(w.cpu_reason.is_none(), "the pin was tiny's alone");
        w.handle(WhisperCmd::UseModel("custom".into()));
        assert_eq!(s.calls().last(), Some(&DeviceChoice::Auto));
        assert!(w.loaded.as_ref().unwrap().info().device.is_some());
    }

    #[test]
    fn a_rule_can_turn_the_replace_rules_off_for_one_app() {
        let (r, s) = (rig("noreplace"), Script::new());
        fs::create_dir_all(r.layout().config_dir).unwrap();
        fs::write(r.layout().replace_file(), "hello => bye\n").unwrap();
        let (mut w, rx) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        job(&mut w);
        assert_eq!(text(&rx).as_deref(), Some("bye "));
        let mut bare = oxi_core::paste::profile(&w.cfg, "");
        bare.replacements = false;
        w.handle(WhisperCmd::Transcribe {
            job: JobId(2),
            samples: vec![0.0; 16],
            profile: Box::new(bare),
        });
        assert_eq!(text(&rx).as_deref(), Some("hello "));
        job(&mut w);
        assert_eq!(
            text(&rx).as_deref(),
            Some("bye "),
            "the next app has them again"
        );
    }

    #[test]
    fn a_cpu_load_that_ran_out_of_memory_is_not_a_damaged_model() {
        let (r, mut s) = (rig("cpuoom"), Script::new());
        (s.gpu_load_fails, s.cpu_oom) = (true, true);
        let (mut w, rx) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Auto)));
        w.handle(WhisperCmd::Preload);
        assert_eq!(event(&rx), Event::ModelFailed(ModelFailure::OutOfMemory));
        // CPU only, no GPU to retry from.
        let (r2, mut s2) = (rig("cpuoom2"), Script::new());
        s2.cpu_oom = true;
        let (mut w2, rx2) = worker(&s2, &r2);
        w2.handle(WhisperCmd::Config(r2.cfg(DeviceChoice::Cpu)));
        w2.handle(WhisperCmd::Preload);
        assert_eq!(event(&rx2), Event::ModelFailed(ModelFailure::OutOfMemory));
    }

    #[test]
    fn a_dictation_reports_whispers_progress_and_stops_when_cancelled() {
        let (r, s) = (rig("progress"), Script::new());
        let (mut w, rx) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        job(&mut w);
        let seen: Vec<_> = rx.try_iter().collect();
        assert!(
            seen.iter().any(|m| matches!(
                m,
                Inbox::Progress {
                    job: JobId(1),
                    percent: 40
                }
            )),
            "progress reaches the coordinator"
        );
        assert!(
            seen.iter()
                .any(|m| matches!(m, Inbox::Transcribed { text: Some(_), .. }))
        );
        // Every job cancelled (the machine is Idle): the run stops, no text, no GPU retry.
        w.phase.store(Phase::Idle);
        w.handle(WhisperCmd::Transcribe {
            job: JobId(2),
            samples: vec![0.0; 16],
            profile: Box::new(oxi_core::paste::profile(&w.cfg, "")),
        });
        assert_eq!(text(&rx), None);
        assert_eq!(s.calls().len(), 1, "no reload after a cancel");
    }

    #[test]
    fn a_test_run_reports_progress_to_the_window() {
        let (r, s) = (rig("testprogress"), Script::new());
        let (mut w, _rx) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        let pushes = Pushes::default();
        let (_, window) = pushes.claim(0);
        w.handle(test_job(&pushes, "custom"));
        let got: Vec<_> = window.try_iter().collect();
        assert!(
            matches!(
                got[0],
                Push::TestProgress {
                    id: 9,
                    percent: 0,
                    est_ms: Some(_)
                }
            ),
            "the run starts with its estimate: {:?}",
            got[0]
        );
        assert_eq!(
            got[1],
            Push::TestProgress {
                id: 9,
                percent: 40,
                est_ms: None
            }
        );
        assert!(matches!(got[2], Push::TestResult { id: 9, .. }));
    }

    #[test]
    fn delete_model_unloads_what_is_loaded_and_removes_its_files() {
        let (r, s) = (rig("delete"), Script::new());
        let (mut w, rx) = worker(&s, &r);
        let mut cfg = Config::default();
        cfg.model.id = "tiny".into();
        w.handle(WhisperCmd::Config(Box::new(cfg)));
        let dir = r.layout().models_dir();
        fs::create_dir_all(&dir).unwrap();
        let entry = models::find("tiny").unwrap();
        fs::write(dir.join(entry.file_name), [0; 6]).unwrap();
        w.loaded = Some(Fake {
            gpu: false,
            info: LoadInfo {
                device: None,
                warning: None,
            },
            fail_run: false,
            heard: Rc::default(),
            times: false,
            overtake: Rc::default(),
            said: "hello",
        });
        w.loaded_for = Some(w.identity());
        assert_eq!(w.delete_model("tiny"), DeleteResult::Done { freed: 6 });
        assert!(w.loaded.is_none(), "unloaded first");
        assert!(!dir.join(entry.file_name).exists());
        // The active model is gone: the machine hears it, so chord-down refuses at once.
        assert_eq!(event(&rx), Event::ModelFailed(ModelFailure::Missing));
        assert!(matches!(
            w.delete_model("nonsense"),
            DeleteResult::Failed { .. }
        ));
        // Another model's files: the active one is untouched, nothing to tell.
        assert_eq!(w.delete_model("turbo"), DeleteResult::Done { freed: 0 });
        assert!(next(&rx).is_none());
    }

    #[test]
    fn preload_loads_once_and_unload_drops_it() {
        let (r, s) = (rig("load"), Script::new());
        let (mut w, rx) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Auto)));
        w.handle(WhisperCmd::Preload);
        w.handle(WhisperCmd::Preload);
        assert_eq!(event(&rx), Event::ModelLoaded);
        assert_eq!(event(&rx), Event::ModelLoaded);
        assert_eq!(s.calls().len(), 1);
        w.handle(WhisperCmd::Unload);
        assert!(w.loaded.is_none());
        w.handle(WhisperCmd::Preload);
        assert_eq!(s.calls().len(), 2);
    }

    #[test]
    fn config_alone_never_unloads_or_reloads() {
        let (r, s) = (rig("cfgkeep"), Script::new());
        let (mut w, rx) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Auto)));
        w.handle(WhisperCmd::Preload);
        let _ = event(&rx);
        // Config(B) then Config(A) with no Unload between: the loaded model stays.
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Auto)));
        assert!(w.loaded.is_some());
        w.handle(WhisperCmd::Preload);
        assert_eq!(event(&rx), Event::ModelLoaded);
        assert_eq!(s.calls().len(), 1);
    }

    #[test]
    fn missing_model_fails_without_calling_the_loader() {
        let (r, s) = (rig("missing"), Script::new());
        let (mut w, rx) = worker(&s, &r);
        // Default config: registry model "turbo" is not in the empty models dir.
        w.handle(WhisperCmd::Preload);
        assert_eq!(event(&rx), Event::ModelFailed(ModelFailure::Missing));
        job(&mut w);
        assert_eq!(event(&rx), Event::ModelFailed(ModelFailure::Missing));
        assert_eq!(text(&rx), None);
        assert_eq!(s.calls(), Vec::<DeviceChoice>::new());
    }

    #[test]
    fn gpu_load_error_retries_on_cpu_and_stays_there_until_the_model_setting_changes() {
        let (r, mut s) = (rig("fallback"), Script::new());
        s.gpu_load_fails = true;
        let (mut w, rx) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Auto)));
        w.handle(WhisperCmd::Preload);
        assert_eq!(event(&rx), Event::ModelLoaded);
        assert_eq!(s.calls(), [DeviceChoice::Auto, DeviceChoice::Cpu]);

        // Next load goes straight to CPU.
        w.handle(WhisperCmd::Unload);
        w.handle(WhisperCmd::Preload);
        assert_eq!(s.calls()[2..], [DeviceChoice::Cpu]);

        // A changed device setting tries the GPU again.
        w.handle(WhisperCmd::Unload);
        w.handle(WhisperCmd::Config(
            r.cfg(DeviceChoice::Named("Vulkan1".into())),
        ));
        w.handle(WhisperCmd::Preload);
        assert_eq!(
            s.calls()[3..],
            [DeviceChoice::Named("Vulkan1".into()), DeviceChoice::Cpu]
        );
    }

    #[test]
    fn each_load_reports_the_device_it_ended_on() {
        let backends = |rx: &Receiver<Inbox>| -> Vec<String> {
            rx.try_iter()
                .filter_map(|m| match m {
                    Inbox::Backend(b) => Some(b),
                    _ => None,
                })
                .collect()
        };
        // (script, expected): a GPU load, the engine's own CPU fallback, a failed GPU load.
        let mut silent = Script::new();
        silent.silent_cpu = true;
        let mut failing = Script::new();
        failing.gpu_load_fails = true;
        for (i, (s, want)) in [
            (Script::new(), "Vulkan0"),
            (silent, "cpu"),
            (failing, "cpu"),
        ]
        .into_iter()
        .enumerate()
        {
            let r = rig(&format!("backend{i}"));
            let (mut w, rx) = worker(&s, &r);
            w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Auto)));
            w.handle(WhisperCmd::Preload);
            assert_eq!(backends(&rx), [want], "case {i}");
        }
    }

    #[test]
    fn unload_and_a_failed_load_report_no_backend() {
        let backends = |rx: &Receiver<Inbox>| -> Vec<String> {
            rx.try_iter()
                .filter_map(|m| match m {
                    Inbox::Backend(b) => Some(b),
                    _ => None,
                })
                .collect()
        };
        let (r, s) = (rig("nobackend"), Script::new());
        let (mut w, rx) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Auto)));
        w.handle(WhisperCmd::Preload);
        assert_eq!(backends(&rx), ["Vulkan0"]);
        w.handle(WhisperCmd::Unload);
        assert_eq!(backends(&rx), ["none"]);
        // A model that cannot load (GPU and CPU both fail) leaves "none", not the old device.
        let (r2, mut bad) = (rig("nobackend2"), Script::new());
        (bad.gpu_load_fails, bad.cpu_load_fails) = (true, true);
        let (mut w2, rx2) = worker(&bad, &r2);
        w2.handle(WhisperCmd::Config(r2.cfg(DeviceChoice::Auto)));
        w2.handle(WhisperCmd::Preload);
        assert_eq!(backends(&rx2), ["none"]);
        // And a missing model.
        let (r3, s3) = (rig("nobackend3"), Script::new());
        let (mut w3, rx3) = worker(&s3, &r3);
        w3.handle(WhisperCmd::Preload);
        assert_eq!(backends(&rx3), ["none"]);
    }

    #[test]
    fn engine_that_fell_back_to_cpu_itself_pins_cpu() {
        let (r, mut s) = (rig("silent"), Script::new());
        s.silent_cpu = true;
        let (mut w, rx) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Auto)));
        w.handle(WhisperCmd::Preload);
        let _ = event(&rx);
        w.handle(WhisperCmd::Unload);
        w.handle(WhisperCmd::Preload);
        assert_eq!(s.calls(), [DeviceChoice::Auto, DeviceChoice::Cpu]);
    }

    #[test]
    fn a_load_that_fails_on_cpu_too_is_bad_and_the_next_load_tries_the_gpu_again() {
        let (r, mut s) = (rig("bad"), Script::new());
        (s.gpu_load_fails, s.cpu_load_fails) = (true, true);
        let (mut w, rx) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Auto)));
        w.handle(WhisperCmd::Preload);
        assert_eq!(event(&rx), Event::ModelFailed(ModelFailure::Bad));
        w.handle(WhisperCmd::Preload);
        assert_eq!(event(&rx), Event::ModelFailed(ModelFailure::Bad));
        assert_eq!(
            s.calls(),
            [
                DeviceChoice::Auto,
                DeviceChoice::Cpu,
                DeviceChoice::Auto,
                DeviceChoice::Cpu
            ]
        );
        assert_eq!(warnings(&rx), Vec::<Option<EngineWarning>>::new());
    }

    #[test]
    fn cpu_fallback_is_reported_across_unloads_until_the_setting_changes() {
        let (r, mut s) = (rig("warn"), Script::new());
        s.gpu_load_fails = true;
        let (mut w, rx) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Auto)));
        w.handle(WhisperCmd::Preload);
        w.handle(WhisperCmd::Preload);
        assert_eq!(warnings(&rx), [Some(oom())], "sent once");
        // Idle unload and reload: still pinned to CPU, the warning stays without a flicker.
        w.handle(WhisperCmd::Unload);
        w.handle(WhisperCmd::Preload);
        w.handle(WhisperCmd::Unload);
        assert_eq!(warnings(&rx), Vec::<Option<EngineWarning>>::new());
        // CPU chosen in the config: the warning clears at once, and no load brings it back.
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        assert_eq!(warnings(&rx), [None]);
        w.handle(WhisperCmd::Preload);
        assert_eq!(warnings(&rx), Vec::<Option<EngineWarning>>::new());
    }

    #[test]
    fn no_gpu_at_all_is_no_warning_and_no_pin() {
        let (r, mut s) = (rig("nogpu"), Script::new());
        s.no_gpu = true;
        let (mut w, rx) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Auto)));
        w.handle(WhisperCmd::Preload);
        w.handle(WhisperCmd::Unload);
        w.handle(WhisperCmd::Preload);
        assert_eq!(warnings(&rx), Vec::<Option<EngineWarning>>::new());
        assert_eq!(s.calls(), [DeviceChoice::Auto, DeviceChoice::Auto]);
    }

    #[test]
    fn engine_load_warning_is_reported_and_cleared_when_a_reload_fails() {
        let (r, mut s) = (rig("gone"), Script::new());
        let gone = EngineWarning::GpuNotFound {
            name: "Vulkan3".into(),
        };
        s.load_warning = Some(gone.clone());
        (s.gpu_run_fails, s.cpu_load_fails) = (true, true);
        let (mut w, rx) = worker(&s, &r);
        w.handle(WhisperCmd::Config(
            r.cfg(DeviceChoice::Named("Vulkan3".into())),
        ));
        w.handle(WhisperCmd::Preload);
        assert_eq!(warnings(&rx), [Some(gone)]);
        // GPU lost mid-job, CPU reload fails: no model, so no device warning either.
        job(&mut w);
        assert_eq!(warnings(&rx), [None]);
    }

    #[test]
    fn gpu_lost_mid_job_and_cpu_load_failing_fails_the_model() {
        let (r, mut s) = (rig("lost"), Script::new());
        (s.gpu_run_fails, s.cpu_load_fails) = (true, true);
        let (mut w, rx) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Auto)));
        w.handle(WhisperCmd::Preload);
        assert_eq!(event(&rx), Event::ModelLoaded);
        job(&mut w);
        assert_eq!(event(&rx), Event::ModelFailed(ModelFailure::Bad));
        assert_eq!(text(&rx), None);
        // CPU never came up, so the next load tries the GPU again.
        w.handle(WhisperCmd::Preload);
        assert_eq!(
            s.calls(),
            [DeviceChoice::Auto, DeviceChoice::Cpu, DeviceChoice::Auto]
        );
    }

    #[test]
    fn transcribe_answers_processed_text_and_loads_when_needed() {
        let (r, s) = (rig("text"), Script::new());
        let (mut w, rx) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        job(&mut w);
        assert_eq!(text(&rx).as_deref(), Some("hello "));
        // Replace rules apply after a reload, and the trailing space follows the config.
        fs::create_dir_all(r.layout().config_dir).unwrap();
        fs::write(r.layout().replace_file(), "hello => bye\n").unwrap();
        w.handle(WhisperCmd::ReloadAssets);
        let mut cfg = r.cfg(DeviceChoice::Cpu);
        cfg.paste.trailing_space = false;
        w.handle(WhisperCmd::Config(cfg));
        job(&mut w);
        // The words for the history entry are whisper's, before the replace rules, with no times.
        let Some(Inbox::Transcribed { text, words, .. }) = next(&rx) else {
            panic!("expected Transcribed");
        };
        assert_eq!(text.as_deref(), Some("bye"));
        assert_eq!(
            words,
            [oxi_core::history::Word {
                w: "hello".into(),
                p: 0.9,
                t0: None,
                t1: None,
            }]
        );
        assert_eq!(s.calls().len(), 1);
    }

    #[test]
    fn the_text_settings_reach_the_pipeline_and_word_times_follow_pause_punctuation() {
        let (r, s) = (rig("text-settings"), Script::new());
        let (mut w, rx) = worker(&s, &r);
        let mut cfg = r.cfg(DeviceChoice::Cpu);
        cfg.paste.trailing_space = false;
        w.handle(WhisperCmd::Config(cfg.clone()));
        job(&mut w);
        let Some(Inbox::Transcribed {
            text: first, words, ..
        }) = next(&rx)
        else {
            panic!("expected Transcribed");
        };
        assert_eq!((first.as_deref(), words[0].t0), (Some("hello"), None));

        // A snippet is the whole text; the filler filter removes the whole text.
        cfg.text.snippets = vec![oxi_core::config::Snippet {
            trigger: "Hello".into(),
            text: "hi there".into(),
        }];
        w.handle(WhisperCmd::Config(cfg.clone()));
        job(&mut w);
        assert_eq!(text(&rx).as_deref(), Some("hi there"));
        cfg.text.snippets.clear();
        cfg.text.filler_filter = true;
        cfg.text.fillers = vec!["hello".into()];
        w.handle(WhisperCmd::Config(cfg.clone()));
        job(&mut w);
        assert_eq!(text(&rx).as_deref(), Some(""));

        // Word times are made for a dictation only while pause punctuation is on.
        cfg.text.filler_filter = false;
        cfg.text.pause_punctuation = true;
        w.handle(WhisperCmd::Config(cfg));
        job(&mut w);
        let Some(Inbox::Transcribed { words, .. }) = next(&rx) else {
            panic!("expected Transcribed");
        };
        assert_eq!(words[0].t0, Some(100));
    }

    #[test]
    fn the_confidence_filter_holds_a_part_only_with_history_on() {
        let (r, s) = (rig("held"), Script::new());
        let (mut w, rx) = worker(&s, &r);
        let mut cfg = r.cfg(DeviceChoice::Cpu);
        cfg.paste.trailing_space = false;
        cfg.text.confidence_filter = true;
        cfg.text.confidence_min = 0.95; // the fake word has p 0.9
        cfg.history.enabled = true;
        w.handle(WhisperCmd::Config(cfg.clone()));
        job(&mut w);
        let Some(Inbox::Transcribed { text, held, .. }) = next(&rx) else {
            panic!("expected Transcribed");
        };
        assert_eq!(text.as_deref(), Some(""), "all of it was held");
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].text, "hello");
        // No history entry to keep it in: nothing is held, the text is pasted.
        cfg.history.enabled = false;
        w.handle(WhisperCmd::Config(cfg));
        job(&mut w);
        let Some(Inbox::Transcribed { text, held, .. }) = next(&rx) else {
            panic!("expected Transcribed");
        };
        assert_eq!((text.as_deref(), held.len()), (Some("hello"), 0));
    }

    /// Review d03 #1, #2: a listed hallucination is held (the coordinator drops it with history
    /// off), and a Transcribe again of the clip gives it back as heard.
    #[test]
    fn a_likely_hallucination_is_held_and_transcribe_again_keeps_it() {
        let (r, mut s) = (rig("halluc"), Script::new());
        s.said = "Thanks for watching";
        let (mut w, rx) = worker(&s, &r);
        let mut cfg = r.cfg(DeviceChoice::Cpu);
        cfg.paste.trailing_space = false;
        cfg.history.enabled = true;
        w.handle(WhisperCmd::Config(cfg.clone()));
        job(&mut w);
        let Some(Inbox::Transcribed { text, held, .. }) = next(&rx) else {
            panic!("expected Transcribed");
        };
        assert_eq!(text.as_deref(), Some(""));
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].why, oxi_core::text::HeldWhy::Hallucination);
        cfg.history.enabled = false;
        w.handle(WhisperCmd::Config(cfg));
        job(&mut w);
        let Some(Inbox::Transcribed { text, held, .. }) = next(&rx) else {
            panic!("expected Transcribed");
        };
        // Review d03-fix #3: this config may be stale (it waits for idle), so the invention is
        // still held; the coordinator drops it when no entry is written.
        assert_eq!((text.as_deref(), held.len()), (Some(""), 1));
        // Transcribe again: the user asked for this clip, nothing is taken out.
        w.phase.store(Phase::Idle);
        let pushes = Pushes::default();
        let (_, prx) = pushes.claim(1);
        w.handle(test_job(&pushes.with_origin(Origin::History), "custom"));
        let TestOutcome::Heard { text, .. } = outcome(&prx) else {
            panic!("expected a text");
        };
        assert_eq!(text, "Thanks for watching");
        // The Test page's run keeps the check.
        w.handle(test_job(&pushes, "custom"));
        let TestOutcome::Heard { text, .. } = outcome(&prx) else {
            panic!("expected a text");
        };
        assert_eq!(text, "");
    }

    #[test]
    fn a_flash_attention_change_loads_the_model_again_but_other_parameters_do_not() {
        let (r, s) = (rig("flash"), Script::new());
        let (mut w, rx) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        job(&mut w);
        let _ = text(&rx);
        let mut cfg = r.cfg(DeviceChoice::Cpu);
        cfg.model.advanced.threads = oxi_core::config::Auto::Set(2);
        w.handle(WhisperCmd::Config(cfg.clone()));
        job(&mut w);
        assert_eq!(s.calls().len(), 1, "threads apply per run");
        cfg.model.advanced.flash_attn = oxi_core::config::Auto::Set(true);
        w.handle(WhisperCmd::Config(cfg));
        job(&mut w);
        assert_eq!(s.calls().len(), 2, "flash attention is a load parameter");
    }

    #[test]
    fn a_flash_attention_choice_that_resolves_the_same_keeps_the_model() {
        use oxi_core::config::Auto;
        let flash = |w: &mut TestWorker, r: &Rig, device, v: Auto<bool>| {
            let mut cfg = r.cfg(device);
            cfg.model.advanced.flash_attn = v;
            w.handle(WhisperCmd::Config(cfg));
        };
        // GPU: Auto is on, so On changes nothing and Off does.
        let (r, s) = (rig("flash-gpu"), Script::new());
        let (mut w, rx) = worker(&s, &r);
        flash(&mut w, &r, DeviceChoice::Auto, Auto::Auto);
        job(&mut w);
        let _ = text(&rx);
        flash(&mut w, &r, DeviceChoice::Auto, Auto::Set(true));
        job(&mut w);
        assert_eq!(s.calls().len(), 1, "Auto and On are the same on a GPU");
        flash(&mut w, &r, DeviceChoice::Auto, Auto::Set(false));
        job(&mut w);
        assert_eq!(s.calls().len(), 2);
        // CPU: Auto is off, so Off changes nothing.
        let (r, s) = (rig("flash-cpu"), Script::new());
        let (mut w, rx) = worker(&s, &r);
        flash(&mut w, &r, DeviceChoice::Cpu, Auto::Auto);
        job(&mut w);
        let _ = text(&rx);
        flash(&mut w, &r, DeviceChoice::Cpu, Auto::Set(false));
        job(&mut w);
        assert_eq!(
            s.calls().len(),
            1,
            "Auto and Off are the same on the processor"
        );
    }

    #[test]
    fn gpu_failure_during_a_job_retries_that_job_on_cpu() {
        let (r, mut s) = (rig("gpurun"), Script::new());
        s.gpu_run_fails = true;
        let (mut w, rx) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Auto)));
        w.handle(WhisperCmd::Preload);
        let _ = event(&rx);
        job(&mut w);
        assert_eq!(text(&rx).as_deref(), Some("hello "));
        assert_eq!(s.calls(), [DeviceChoice::Auto, DeviceChoice::Cpu]);
        job(&mut w);
        assert_eq!(text(&rx).as_deref(), Some("hello "));
        assert_eq!(s.calls().len(), 2, "CPU stays loaded");
        w.handle(WhisperCmd::Unload);
        w.handle(WhisperCmd::Preload);
        assert_eq!(
            s.calls()[2..],
            [DeviceChoice::Cpu],
            "CPU until a config change"
        );
    }

    /// An audible second of raw samples and a set that runs the rig's custom model.
    fn test_job(reply: &Pushes, model: &str) -> WhisperCmd {
        WhisperCmd::Test(TestJob {
            id: 9,
            set: SettingsSet {
                normalize: true,
                denoise: false,
                trim: false,
                model: model.into(),
            },
            language: Language::EN,
            samples: (0..16_000u16).map(|i| f32::from(i % 40) / 80.0).collect(),
            reply: reply.clone(),
        })
    }

    fn outcome(rx: &Receiver<Push>) -> TestOutcome {
        match rx
            .try_iter()
            .find(|p| !matches!(p, Push::TestProgress { .. } | Push::HistoryRunWords { .. }))
        {
            Some(Push::TestResult { id: 9, outcome }) => outcome,
            _ => panic!("expected TestResult"),
        }
    }

    #[test]
    fn test_answers_final_text_and_done_drops_only_a_model_it_loaded() {
        let (r, s) = (rig("testrun"), Script::new());
        let (mut w, _inbox) = worker(&s, &r);
        let pushes = Pushes::default();
        let (_, rx) = pushes.claim(1);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        w.handle(test_job(&pushes, "custom"));
        assert!(matches!(outcome(&rx), TestOutcome::Heard { text, .. } if text == "hello"));
        w.handle(test_job(&pushes, "custom"));
        let _ = outcome(&rx);
        assert_eq!(s.calls().len(), 1, "same model stays loaded between sets");
        w.handle(WhisperCmd::TestDone);
        assert!(w.loaded.is_none(), "nothing asked for it but the test");

        // A model the dictation path loaded stays.
        w.handle(WhisperCmd::Preload);
        w.handle(test_job(&pushes, "custom"));
        w.handle(WhisperCmd::TestDone);
        assert!(w.loaded.is_some());
        assert_eq!(s.calls().len(), 2);
    }

    #[test]
    fn test_with_a_missing_model_is_an_answer_not_a_machine_event() {
        let (r, s) = (rig("testmissing"), Script::new());
        let (mut w, inbox) = worker(&s, &r);
        let pushes = Pushes::default();
        let (_, rx) = pushes.claim(1);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        w.handle(test_job(&pushes, "tiny"));
        assert_eq!(
            outcome(&rx),
            TestOutcome::Failed {
                why: TestFailure::ModelMissing
            }
        );
        assert!(next(&inbox).is_none(), "no ModelFailed for a test");
        assert_eq!(w.cfg.model.id, "custom", "config restored");
    }

    #[test]
    fn a_stored_recording_is_not_processed_again_but_a_microphone_clip_is() {
        let (r, s) = (rig("teststored"), Script::new());
        let (mut w, _inbox) = worker(&s, &r);
        let pushes = Pushes::default();
        let (_, rx) = pushes.claim(1);
        let mut cfg = r.cfg(DeviceChoice::Cpu);
        cfg.audio.gain_db = 12.0;
        w.handle(WhisperCmd::Config(cfg));
        let WhisperCmd::Test(raw) = test_job(&pushes, "custom") else {
            unreachable!()
        };
        let samples = raw.samples.clone();
        w.handle(WhisperCmd::Test(raw));
        let _ = outcome(&rx);
        w.handle(test_job(&pushes.for_stored_audio(), "custom"));
        let _ = outcome(&rx);
        let heard = s.heard.borrow();
        assert_ne!(
            heard[0], samples,
            "a microphone clip gets gain and normalise"
        );
        assert_eq!(heard[1], samples, "a stored one went in as saved");
    }

    /// A 200 ms clip at -45 dBFS: too quiet for the gate to count five voiced frames.
    fn quiet_job(w: &TestWorker, reply: &Pushes) -> WhisperCmd {
        let WhisperCmd::Test(mut job) = test_job(reply, "custom") else {
            unreachable!()
        };
        job.samples = (0..3_200u16)
            .map(|i| 0.0056 * (f32::from(i) * 0.12).sin())
            .collect();
        job.set.trim = true;
        job.set.normalize = false;
        let opts = Audio {
            trim: true,
            normalize: false,
            ..w.cfg.audio.clone()
        };
        assert!(
            matches!(
                audio::process(&job.samples, audio::SAMPLE_RATE, &opts),
                Err(AudioError::NothingHeard)
            ),
            "the premise: the gate hears nothing"
        );
        WhisperCmd::Test(job)
    }

    #[test]
    fn a_quiet_clip_the_gate_misses_is_sent_whole_on_a_retest() {
        let (r, s) = (rig("testwholeclip"), Script::new());
        let (mut w, _inbox) = worker(&s, &r);
        let pushes = Pushes::default();
        let (_, rx) = pushes.claim(1);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        let job = quiet_job(&w, &pushes.with_origin(Origin::HistoryRaw));
        w.handle(job);
        let _ = outcome(&rx);
        assert_eq!(s.heard.borrow().len(), 1, "it reached whisper");
    }

    #[test]
    fn a_silent_test_page_clip_is_not_sent_to_whisper() {
        let (r, s) = (rig("testsilentmic"), Script::new());
        let (mut w, _inbox) = worker(&s, &r);
        let pushes = Pushes::default();
        let (_, rx) = pushes.claim(1);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        let job = quiet_job(&w, &pushes);
        w.handle(job);
        assert_eq!(
            outcome(&rx),
            TestOutcome::Heard {
                text: String::new(),
                ms: 0
            }
        );
        assert!(
            s.heard.borrow().is_empty(),
            "nothing heard: no whisper call"
        );
    }

    #[test]
    fn empty_recording_is_no_audio() {
        let (r, s) = (rig("testempty"), Script::new());
        let (mut w, _inbox) = worker(&s, &r);
        let pushes = Pushes::default();
        let (_, rx) = pushes.claim(1);
        let WhisperCmd::Test(mut job) = test_job(&pushes, "custom") else {
            unreachable!()
        };
        job.samples.clear();
        w.handle(WhisperCmd::Test(job));
        assert_eq!(
            outcome(&rx),
            TestOutcome::Failed {
                why: TestFailure::NoAudio
            }
        );
        assert_eq!(s.calls().len(), 0);
    }

    #[test]
    fn another_models_gpu_failure_does_not_pin_the_configured_model_to_cpu() {
        let (r, mut s) = (rig("testpin"), Script::new());
        s.gpu_load_fails = true;
        let (mut w, _inbox) = worker(&s, &r);
        let pushes = Pushes::default();
        let (_, rx) = pushes.claim(1);
        let mut cfg = r.cfg(DeviceChoice::Auto);
        cfg.model.id = "tiny".into();
        w.handle(WhisperCmd::Config(cfg));
        w.handle(test_job(&pushes, "custom"));
        assert!(matches!(outcome(&rx), TestOutcome::Heard { .. }));
        assert_eq!(s.calls(), [DeviceChoice::Auto, DeviceChoice::Cpu]);
        assert_eq!(w.cpu_reason, None);
        assert!(w.test_loaded);
        assert_eq!(w.cfg.model.id, "tiny");
        // The next dictation drops the test's model; "tiny" is not on disk, so it fails.
        w.handle(WhisperCmd::Preload);
        assert!(w.loaded.is_none() && !w.test_loaded);
    }

    #[test]
    fn a_test_queued_before_a_dictation_answers_busy_and_loads_nothing() {
        let (r, s) = (rig("testbusy"), Script::new());
        let (mut w, _inbox) = worker(&s, &r);
        let pushes = Pushes::default();
        let (_, rx) = pushes.claim(1);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        w.phase.store(Phase::Recording);
        w.handle(test_job(&pushes, "custom"));
        assert_eq!(
            outcome(&rx),
            TestOutcome::Failed {
                why: TestFailure::Busy
            }
        );
        assert_eq!(s.calls(), Vec::<DeviceChoice>::new());
    }

    #[test]
    fn a_dictation_reloads_a_test_model_whose_device_or_file_changed() {
        let (r, s) = (rig("testadopt"), Script::new());
        let (mut w, _inbox) = worker(&s, &r);
        let pushes = Pushes::default();
        let (_, rx) = pushes.claim(1);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Auto)));
        w.handle(test_job(&pushes, "custom"));
        let _ = outcome(&rx);
        // Same id, other device: the test's model must not serve the dictation.
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        w.handle(WhisperCmd::Preload);
        assert_eq!(s.calls(), [DeviceChoice::Auto, DeviceChoice::Cpu]);

        // Unchanged settings: the dictation keeps it.
        w.handle(WhisperCmd::Unload);
        w.handle(test_job(&pushes, "custom"));
        let _ = outcome(&rx);
        w.handle(WhisperCmd::Preload);
        assert_eq!(s.calls().len(), 3);

        // Same id, other file: a test loads again, and so does a dictation.
        let other = r.dir.join("other.bin");
        fs::write(&other, b"y").unwrap();
        let mut moved = r.cfg(DeviceChoice::Cpu);
        moved.model.custom_path = other.to_str().unwrap().into();
        w.handle(WhisperCmd::Config(moved));
        w.handle(test_job(&pushes, "custom"));
        let _ = outcome(&rx);
        assert_eq!(s.calls().len(), 4, "the test ran on the old file");
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        w.handle(WhisperCmd::Preload);
        assert_eq!(s.calls().len(), 5, "the dictation ran on the old file");
    }

    /// What `Status` was told so far: backends and warnings; drains the inbox.
    fn status_messages(rx: &Receiver<Inbox>) -> (Vec<String>, Vec<Option<EngineWarning>>) {
        let (mut backends, mut warnings) = (vec![], vec![]);
        for m in rx.try_iter() {
            match m {
                Inbox::Backend(b) => backends.push(b),
                Inbox::EngineWarning(w) => warnings.push(w),
                _ => {}
            }
        }
        (backends, warnings)
    }

    #[test]
    fn a_test_of_another_model_leaves_status_alone() {
        // Its GPU falls back to CPU: not the configured model's warning or device.
        let (r, mut s) = (rig("testquiet"), Script::new());
        s.gpu_load_fails = true;
        let (mut w, inbox) = worker(&s, &r);
        let pushes = Pushes::default();
        let (_, rx) = pushes.claim(1);
        let mut cfg = r.cfg(DeviceChoice::Auto);
        cfg.model.id = "tiny".into();
        w.handle(WhisperCmd::Config(cfg));
        w.handle(test_job(&pushes, "custom"));
        assert!(matches!(outcome(&rx), TestOutcome::Heard { .. }));
        assert_eq!(status_messages(&inbox), (vec![], vec![]));

        // The configured model is pinned to CPU, and a test of a missing model must not wipe it.
        let (r2, mut s2) = (rig("testquiet2"), Script::new());
        s2.gpu_load_fails = true;
        let (mut w2, inbox2) = worker(&s2, &r2);
        let (_, rx2) = pushes.claim(1);
        w2.handle(WhisperCmd::Config(r2.cfg(DeviceChoice::Auto)));
        w2.handle(WhisperCmd::Preload);
        assert_eq!(
            status_messages(&inbox2),
            (vec!["cpu".to_owned()], vec![Some(oom())])
        );
        w2.handle(test_job(&pushes, "tiny"));
        assert_eq!(
            outcome(&rx2),
            TestOutcome::Failed {
                why: TestFailure::ModelMissing
            }
        );
        assert_eq!(status_messages(&inbox2), (vec![], vec![]));
        assert_eq!(w2.reported, Some(oom()));
    }

    #[test]
    fn a_test_of_the_configured_model_still_reports_its_device() {
        let (r, s) = (rig("testloud"), Script::new());
        let (mut w, inbox) = worker(&s, &r);
        let pushes = Pushes::default();
        let (_, rx) = pushes.claim(1);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Auto)));
        w.handle(test_job(&pushes, "custom"));
        let _ = outcome(&rx);
        assert_eq!(status_messages(&inbox).0, ["Vulkan0"]);
    }

    #[test]
    fn a_test_only_model_goes_after_the_idle_limit() {
        let (r, s) = (rig("testidle"), Script::new());
        let (mut w, _inbox) = worker(&s, &r);
        let pushes = Pushes::default();
        let (_, rx) = pushes.claim(1);
        let (tx, cmds) = channel();
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        assert_eq!(w.test_deadline(), None, "nothing to drop yet");
        w.handle(test_job(&pushes, "custom"));
        let _ = outcome(&rx);
        assert_eq!(w.test_deadline(), Some(w.last_test + TEST_MODEL_IDLE));
        // A waiting command is taken first, and a recent test keeps the model.
        tx.send(WhisperCmd::ReloadAssets).unwrap();
        assert!(matches!(w.next_cmd(&cmds), Some(WhisperCmd::ReloadAssets)));

        w.last_test -= TEST_MODEL_IDLE;
        let cmd = w.next_cmd(&cmds).unwrap();
        assert!(matches!(cmd, WhisperCmd::TestDone));
        w.handle(cmd);
        assert!(w.loaded.is_none());
        assert_eq!(w.test_deadline(), None);

        // A model the dictation path adopted has no limit.
        w.handle(test_job(&pushes, "custom"));
        let _ = outcome(&rx);
        w.handle(WhisperCmd::Preload);
        assert_eq!(w.test_deadline(), None);
        assert!(w.loaded.is_some());
    }

    /// Real tiny model on CPU through the whole worker; skips loudly without the env vars.
    #[test]
    fn tiny_model_transcribes_jfk_through_the_worker() {
        let (Some(model), Some(wav)) = (
            std::env::var_os("OXI_TINY_MODEL"),
            std::env::var_os("OXI_JFK_WAV"),
        ) else {
            // Straight to stderr: libtest captures `eprintln!` of a passing test.
            let _ = writeln!(
                std::io::stderr(),
                "SKIPPED tiny_model_transcribes_jfk_through_the_worker: set OXI_TINY_MODEL and OXI_JFK_WAV"
            );
            return;
        };
        let (samples, rate) = oxi_platform::wav::read_wav_mono_f32(Path::new(&wav)).unwrap();
        assert_eq!(rate, audio::SAMPLE_RATE);
        let r = rig("real");
        let (tx, rx) = channel();
        let load = |p: &Path, c: &DeviceChoice, a: &Advanced| -> anyhow::Result<Engine> {
            Ok(Engine::load_with(p, c, a)?)
        };
        let mut w = Worker::new(load, tx, r.layout(), PhaseCell::default());
        let mut cfg = r.cfg(DeviceChoice::Cpu);
        cfg.model.custom_path = model.to_str().unwrap().into();
        cfg.model.language = Language::EN;
        w.handle(WhisperCmd::Config(cfg));
        w.handle(WhisperCmd::Preload);
        assert_eq!(event(&rx), Event::ModelLoaded);
        w.handle(WhisperCmd::Transcribe {
            job: JobId(7),
            samples,
            profile: Box::new(oxi_core::paste::profile(&w.cfg, "")),
        });
        let got = text(&rx).unwrap().to_lowercase();
        assert!(
            got.contains("ask not what your country can do for you"),
            "{got}"
        );
    }

    /// A record with `audio` in the history folder of `cfg`; its id.
    fn history_record(r: &Rig, cfg: &Config) -> (PathBuf, &'static str) {
        let dir = oxi_core::history::folder(&cfg.history, &r.layout().history_dir());
        let id = "2026-11-08/134501-123";
        fs::create_dir_all(dir.join("2026-11-08")).unwrap();
        fs::write(dir.join(oxi_core::history::MARKER), b"x").unwrap();
        let base = dir.join(id);
        oxi_platform::wav::write_wav(&base.with_extension("wav"), &[0.1; 1600]).unwrap();
        let meta = oxi_core::history::Meta {
            version: oxi_core::history::Meta::VERSION,
            started: String::new(),
            duration_ms: 100,
            status: oxi_core::history::Status::Done,
            model: String::new(),
            language: String::new(),
            settings: None,
            previous: None,
            words: Vec::new(),
            timings: None,
            raw: false,
            held: Vec::new(),
            delivered: None,
        };
        fs::write(base.with_extension("json"), meta.to_json().unwrap()).unwrap();
        (dir, id)
    }

    #[test]
    fn a_words_run_stores_the_times_in_its_record_and_answers_with_them() {
        let (r, s) = (rig("words-run"), Script::new());
        let (mut w, _inbox) = worker(&s, &r);
        let cfg = r.cfg(DeviceChoice::Cpu);
        let (dir, id) = history_record(&r, &cfg);
        w.handle(WhisperCmd::Config(cfg));
        let pushes = Pushes::default();
        let (_, rx) = pushes.claim(1);
        let reply = pushes.with_origin(Origin::History).with_words_for(id);
        let job = WhisperCmd::Test(TestJob {
            id: 9,
            set: SettingsSet::defaults(),
            language: Language::AUTO,
            samples: vec![0.1; 1600],
            reply,
        });
        w.handle(job);
        let answer = rx
            .try_iter()
            .find(|p| !matches!(p, Push::TestProgress { .. }))
            .unwrap();
        let Push::HistoryWords {
            run: 9,
            result: WordsResult::Ready { words, timed: true },
            ..
        } = answer
        else {
            panic!("expected Ready words");
        };
        assert_eq!((words.0[0].t0, words.0[0].t1), (Some(100), Some(400)));
        let (stored, how) = crate::daemon::history::read_words(&dir, id).unwrap();
        assert_eq!(stored, words.0, "the record holds what the window got");
        assert_eq!(how.unwrap().by, oxi_core::history::TimingBy::Tokens);
        // A dictation under way: nothing runs, nothing is stored over the timed words.
        w.phase.store(Phase::Recording);
        let busy = pushes.with_origin(Origin::History).with_words_for(id);
        w.handle(WhisperCmd::Test(TestJob {
            id: 10,
            set: SettingsSet::defaults(),
            language: Language::AUTO,
            samples: vec![0.1; 1600],
            reply: busy,
        }));
        assert!(matches!(
            rx.try_iter().last(),
            Some(Push::HistoryWords {
                run: 10,
                result: WordsResult::Busy,
                ..
            })
        ));
        // A record that is gone: the run still answers.
        w.phase.store(Phase::Idle);
        let gone = pushes
            .with_origin(Origin::History)
            .with_words_for("2026-11-08/000000-000");
        w.handle(WhisperCmd::Test(TestJob {
            id: 11,
            set: SettingsSet::defaults(),
            language: Language::AUTO,
            samples: vec![0.1; 1600],
            reply: gone,
        }));
        assert!(matches!(
            rx.try_iter().last(),
            Some(Push::HistoryWords {
                run: 11,
                result: WordsResult::Unavailable {
                    why: WordsFailure::Failed
                },
                ..
            })
        ));
    }

    #[test]
    fn a_words_run_model_is_a_tests_and_goes_after_the_idle_limit() {
        let (r, s) = (rig("words-idle"), Script::new());
        let (mut w, _inbox) = worker(&s, &r);
        let cfg = r.cfg(DeviceChoice::Cpu);
        let (_dir, id) = history_record(&r, &cfg);
        w.handle(WhisperCmd::Config(cfg));
        let pushes = Pushes::default();
        let (_, rx) = pushes.claim(1);
        let words = |run| {
            WhisperCmd::Test(TestJob {
                id: run,
                set: SettingsSet::defaults(),
                language: Language::AUTO,
                samples: vec![0.1; 1600],
                reply: pushes.with_origin(Origin::History).with_words_for(id),
            })
        };
        // Nothing loaded (an idle unload went before): the run's model goes after the limit.
        w.handle(words(1));
        assert!(w.loaded.is_some());
        assert_eq!(w.test_deadline(), Some(w.last_test + TEST_MODEL_IDLE));
        // A model a Test page run left stays a test's.
        w.handle(WhisperCmd::Unload);
        w.handle(test_job(&pushes, "custom"));
        w.handle(words(2));
        assert_eq!(w.test_deadline(), Some(w.last_test + TEST_MODEL_IDLE));
        // A model a dictation loaded stays the dictation's.
        w.handle(WhisperCmd::Preload);
        w.handle(words(3));
        assert_eq!(w.test_deadline(), None);
        assert!(w.loaded.is_some());
        let _ = rx.try_iter().count();
    }

    #[test]
    fn a_stopped_words_run_answers_busy_and_stores_nothing() {
        let (r, s) = (rig("words-stopped"), Script::new());
        let (mut w, _inbox) = worker(&s, &r);
        let cfg = r.cfg(DeviceChoice::Cpu);
        let (dir, id) = history_record(&r, &cfg);
        w.handle(WhisperCmd::Config(cfg));
        let pushes = Pushes::default();
        let (_, rx) = pushes.claim(1);
        let reply = pushes.with_origin(Origin::History).with_words_for(id);
        reply.words_stop().unwrap().store(true, Ordering::Release);
        w.handle(WhisperCmd::Test(TestJob {
            id: 4,
            set: SettingsSet::defaults(),
            language: Language::AUTO,
            samples: vec![0.1; 1600],
            reply,
        }));
        assert!(matches!(
            rx.try_iter().last(),
            Some(Push::HistoryWords {
                run: 4,
                result: WordsResult::Busy,
                ..
            })
        ));
        assert!(w.loaded.is_none(), "nothing was loaded for it");
        let (stored, _) = crate::daemon::history::read_words(&dir, id).unwrap();
        assert!(stored.iter().all(|w| w.t0.is_none()));
    }

    #[test]
    fn a_history_run_sends_its_words_before_its_text() {
        let (r, s) = (rig("run-words"), Script::new());
        let (mut w, _inbox) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        let pushes = Pushes::default();
        let (_, rx) = pushes.claim(1);
        w.handle(test_job(&pushes.with_origin(Origin::History), "custom"));
        let sent: Vec<Push> = rx
            .try_iter()
            .filter(|p| !matches!(p, Push::TestProgress { .. }))
            .collect();
        let [
            Push::HistoryRunWords { id: 9, words },
            Push::TestResult { id: 9, .. },
        ] = &sent[..]
        else {
            panic!("expected the words, then the result: {} pushes", sent.len());
        };
        assert_eq!((words.0[0].w.as_str(), words.0[0].t0), ("hello", None));
        // The Test page's run has no use for them.
        w.handle(test_job(&pushes, "custom"));
        assert!(
            !rx.try_iter()
                .any(|p| matches!(p, Push::HistoryRunWords { .. }))
        );
    }

    #[test]
    fn word_times_are_on_for_that_run_only() {
        let (r, s) = (rig("wordtimes"), Script::new());
        let (mut w, _inbox) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        let clip = [0.1_f32; 1600];
        let timed = w.word_times(&clip).unwrap();
        let word = &timed.segments[0].words[0];
        assert_eq!((word.t0, word.t1), (Some(Millis(100)), Some(Millis(400))));
        assert!((word.p - 0.9).abs() < f32::EPSILON);
        // The model stays loaded, and the next run (a dictation) makes no times.
        let plain = w.recognise(&clip).unwrap().unwrap();
        assert_eq!(plain.segments[0].words[0].t0, None);
        assert_eq!(s.calls().len(), 1, "one load served both");
    }

    #[test]
    fn a_dictation_goes_before_a_timing_run_and_stops_it() {
        let (r, s) = (rig("wordtimes-busy"), Script::new());
        let (mut w, _inbox) = worker(&s, &r);
        w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
        let clip = [0.1_f32; 1600];
        // A dictation under way: the run does not start.
        w.phase.store(Phase::Recording);
        assert!(matches!(w.word_times(&clip), Err(TestFailure::Busy)));
        assert!(s.heard.borrow().is_empty());
        // One that starts during the run stops it.
        w.phase.store(Phase::Idle);
        *s.overtake.borrow_mut() = Some(w.phase.clone());
        assert!(matches!(w.word_times(&clip), Err(TestFailure::Busy)));
        assert_eq!(
            s.heard.borrow().len(),
            1,
            "stopped, not run again on another device"
        );
        // A test is not a timing run: a dictation starting during it does not stop it.
        w.phase.store(Phase::Idle);
        assert!(w.recognise(&clip).unwrap().is_some());
    }
}
