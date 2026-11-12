//! Whisper worker: load, unload, transcribe (ARCHITECTURE section 8).
//!
//! Obeys the coordinator's commands in order. The state machine decides when a model is stale
//! (Unload, then Preload); a `Config` alone never unloads or reloads. Dictated text and audio
//! never reach a log: only lengths, kinds and model names do.

use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use oxi_core::audio::{self, AudioError};
use oxi_core::config::{Advanced, Audio, Config};
use oxi_core::device::DeviceChoice;
use oxi_core::ipc::{DeleteResult, Push, TestFailure, TestOutcome};
use oxi_core::machine::{
    EngineWarning, Event, JobId, MachineConfig, ModelFailure, ModelIdentity, Phase, PhaseCell,
};
use oxi_core::models;
use oxi_core::overlay::{learn, transcribe_est};
use oxi_core::paths::Layout;
use oxi_core::strings;
use oxi_core::text::Outcome;
use oxi_core::tuning::{self, SettingsSet};
use oxi_core::types::Language;
use oxi_whisper::{Cancel, Engine, LoadInfo, Transcript};

use super::cmd::{Inbox, TestJob, WhisperCmd};
use super::io;
use super::ipc::Pushes;
use crate::pipeline::{self, TextAssets};

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
            WhisperCmd::Preload => {
                self.adopt_test_model();
                let event = match self.ensure_loaded(None) {
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
            WhisperCmd::Transcribe { job, samples } => {
                self.tap = Tap::Job(job);
                let started = Instant::now();
                let text = self.transcribe(&samples);
                self.tap = Tap::None;
                // Lengths only, never the text (AGENTS.md rule 5).
                let audio_ms = oxi_core::stats::audio_length(samples.len()).0;
                let took_ms = started.elapsed().as_millis();
                match &text {
                    Ok(Some(t)) => {
                        let chars = t.chars().count();
                        tracing::info!(?job, audio_ms, took_ms, chars, "transcription done");
                    }
                    Ok(None) => tracing::info!(?job, took_ms, "transcription failed or cut"),
                    Err(why) => tracing::info!(?job, ?why, "transcription had no model"),
                }
                let text = text.unwrap_or_else(|failure| {
                    // First, so the machine leaves Loaded before this job ends: the jobs queued
                    // behind it are not dispatched to a model that is not there, one notice.
                    let _ = self.inbox.send(Inbox::Event(Event::ModelFailed(failure)));
                    None
                });
                let _ = self.inbox.send(Inbox::Transcribed { job, text });
            }
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
            }
            WhisperCmd::ReloadAssets => self.assets = TextAssets::load(&self.layout),
        }
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
            Ok(Some(transcript)) => TestOutcome::Heard {
                text: self.final_text(&transcript, false),
                ms,
            },
            Ok(None) => TestOutcome::Failed {
                why: TestFailure::Failed,
            },
            Err(failure) => model_failed(failure),
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

    /// `Ok(Some(""))` is nothing heard, `Ok(None)` a whisper failure, `Err` no model to run.
    fn transcribe(&mut self, samples: &[f32]) -> Result<Option<String>, ModelFailure> {
        self.adopt_test_model();
        let trailing_space = self.cfg.paste.trailing_space;
        Ok(self
            .recognise(samples)?
            .map(|t| self.final_text(&t, trailing_space)))
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

    /// Empty when nothing is left to paste.
    fn final_text(&self, transcript: &Transcript, trailing_space: bool) -> String {
        match pipeline::finish(transcript, &self.assets, trailing_space) {
            Outcome::Text(text) => text,
            Outcome::NothingHeard => String::new(),
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
        // Every job cancelled leaves the phase Idle; a test only runs while it is Idle already.
        let is_job = matches!(self.tap, Tap::Job(_));
        let cancel = move || is_job && phase.load() == Phase::Idle;
        let hooks = Hooks {
            progress: &mut progress,
            cancel: Some(&cancel),
        };
        engine.set_advanced(&self.cfg.model.advanced);
        let prompt = tuning::prompt(&self.cfg.model.advanced, &self.assets.prompt);
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
    use crate::log::scratch::scratch_dir;

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
    }

    type Loader = Box<dyn FnMut(&Path, &DeviceChoice, &Advanced) -> anyhow::Result<Fake>>;
    type TestWorker = Worker<Fake, Loader>;

    struct Fake {
        gpu: bool,
        info: LoadInfo,
        fail_run: bool,
        heard: Rc<RefCell<Vec<Vec<f32>>>>,
    }

    impl Recognizer for Fake {
        fn info(&self) -> &LoadInfo {
            &self.info
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
            if hooks.cancel.is_some_and(|cancel| cancel()) {
                return Err(oxi_whisper::Error::Cancelled.into());
            }
            if self.fail_run && self.gpu {
                anyhow::bail!("device lost");
            }
            Ok(Transcript {
                segments: vec![Segment {
                    text: " hello ".into(),
                    t0: Millis(0),
                    t1: Millis(1),
                    no_speech_prob: 0.0,
                }],
            })
        }
    }

    impl Script {
        fn new() -> Self {
            Self {
                calls: Rc::default(),
                gpu_load_fails: false,
                cpu_load_fails: false,
                cpu_oom: false,
                load_warning: None,
                gpu_run_fails: false,
                silent_cpu: false,
                no_gpu: false,
                heard: Rc::default(),
            }
        }

        fn loader(&self) -> Loader {
            let s = self.clone();
            Box::new(move |_, choice, _| {
                s.calls.borrow_mut().push(choice.clone());
                let asked_gpu = *choice != DeviceChoice::Cpu;
                let cpu = !asked_gpu || s.silent_cpu || s.no_gpu;
                if cpu && s.cpu_oom {
                    return Err(oxi_whisper::Error::OutOfMemory.into());
                }
                if !cpu && s.gpu_load_fails || cpu && s.cpu_load_fails {
                    anyhow::bail!("out of memory");
                }
                Ok(Fake {
                    gpu: !cpu,
                    fail_run: s.gpu_run_fails,
                    heard: s.heard.clone(),
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
        });
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
        assert_eq!(text(&rx).as_deref(), Some("bye"));
        assert_eq!(s.calls().len(), 1);
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
            .find(|p| !matches!(p, Push::TestProgress { .. }))
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
        });
        let got = text(&rx).unwrap().to_lowercase();
        assert!(
            got.contains("ask not what your country can do for you"),
            "{got}"
        );
    }
}
