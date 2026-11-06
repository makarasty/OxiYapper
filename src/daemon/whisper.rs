//! Whisper worker: load, unload, transcribe (ARCHITECTURE section 8).
//!
//! Obeys the coordinator's commands in order. The state machine decides when a model is stale
//! (Unload, then Preload); a `Config` alone never unloads or reloads. Dictated text and audio
//! never reach a log: only lengths, kinds and model names do.

#![allow(dead_code, reason = "run.rs (t40) calls spawn")]

use std::path::Path;
use std::sync::mpsc::{Receiver, Sender};
use std::thread::{self, JoinHandle};

use oxi_core::config::Config;
use oxi_core::device::DeviceChoice;
use oxi_core::machine::{EngineWarning, Event, MachineConfig, ModelFailure};
use oxi_core::paths::Layout;
use oxi_core::strings;
use oxi_core::text::Outcome;
use oxi_core::types::Language;
use oxi_whisper::{Engine, LoadInfo, Transcript};

use super::cmd::{Inbox, WhisperCmd};
use crate::pipeline::{self, TextAssets};

/// One loaded model; [`Engine`] in the daemon, a fake in the tests.
trait Recognizer {
    fn info(&self) -> &LoadInfo;
    fn transcribe(
        &mut self,
        samples: &[f32],
        language: Language,
        prompt: &str,
    ) -> anyhow::Result<Transcript>;
}

impl Recognizer for Engine {
    fn info(&self) -> &LoadInfo {
        Engine::info(self)
    }

    fn transcribe(
        &mut self,
        samples: &[f32],
        language: Language,
        prompt: &str,
    ) -> anyhow::Result<Transcript> {
        Ok(Engine::transcribe(self, samples, language, prompt)?)
    }
}

/// Starts the worker thread. It starts on the default config; the coordinator sends the active
/// one first (`Coordinator::new`). Returns when `rx` closes.
pub fn spawn(
    rx: Receiver<WhisperCmd>,
    inbox: Sender<Inbox>,
    layout: Layout,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("whisper".into())
        .spawn(move || {
            let load = |path: &Path, choice: &DeviceChoice| -> anyhow::Result<Engine> {
                Ok(Engine::load(path, choice)?)
            };
            let mut worker = Worker::new(load, inbox, layout);
            for cmd in rx {
                worker.handle(cmd);
            }
        })
}

struct Worker<R, L> {
    load: L,
    inbox: Sender<Inbox>,
    layout: Layout,
    cfg: Config,
    assets: TextAssets,
    loaded: Option<R>,
    /// Why the GPU is off: set once a CPU load after a GPU failure succeeded, cleared when the
    /// model or device setting changes. Loads go to CPU while set.
    cpu_reason: Option<String>,
    /// The last `Inbox::EngineWarning` sent.
    reported: Option<EngineWarning>,
}

impl<R: Recognizer, L: FnMut(&Path, &DeviceChoice) -> anyhow::Result<R>> Worker<R, L> {
    fn new(load: L, inbox: Sender<Inbox>, layout: Layout) -> Self {
        let assets = TextAssets::load(&layout);
        Self {
            load,
            inbox,
            layout,
            cfg: Config::default(),
            assets,
            loaded: None,
            cpu_reason: None,
            reported: None,
        }
    }

    fn handle(&mut self, cmd: WhisperCmd) {
        match cmd {
            WhisperCmd::Preload => {
                let event = match self.ensure_loaded(None) {
                    Ok(()) => Event::ModelLoaded,
                    Err(failure) => Event::ModelFailed(failure),
                };
                let _ = self.inbox.send(Inbox::Event(event));
            }
            WhisperCmd::Unload => {
                self.loaded = None;
                // A pinned CPU fallback still holds for the next load: keep saying so (no
                // tray flicker around each idle unload).
                if self.cpu_reason.is_none() {
                    self.report(None);
                }
            }
            WhisperCmd::Transcribe { job, samples } => {
                let text = self.transcribe(&samples).unwrap_or_else(|failure| {
                    // First, so the machine leaves Loaded before this job ends: the jobs queued
                    // behind it are not dispatched to a model that is not there, one notice.
                    let _ = self.inbox.send(Inbox::Event(Event::ModelFailed(failure)));
                    None
                });
                let _ = self.inbox.send(Inbox::Transcribed { job, text });
            }
            WhisperCmd::Config(cfg) => {
                if MachineConfig::from(&*cfg).model != MachineConfig::from(&self.cfg).model {
                    self.cpu_reason = None;
                    if self.loaded.is_none() {
                        self.report(None);
                    }
                }
                self.cfg = *cfg;
                self.assets = TextAssets::load(&self.layout);
            }
            WhisperCmd::ReloadAssets => self.assets = TextAssets::load(&self.layout),
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
        let engine = match (self.load)(&model.path, &choice) {
            Ok(engine) => engine,
            Err(e) if choice != DeviceChoice::Cpu => {
                tracing::warn!("model load on the GPU failed ({e:#}), retrying on CPU");
                reason = Some(e.to_string());
                (self.load)(&model.path, &DeviceChoice::Cpu).map_err(|e| {
                    tracing::warn!("model load on CPU failed: {e:#}");
                    ModelFailure::Bad
                })?
            }
            Err(e) => {
                tracing::warn!("model load failed: {e:#}");
                return Err(ModelFailure::Bad);
            }
        };
        let info = engine.info();
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
        Ok(warning)
    }

    /// Sends `warning` when it differs from the last one sent.
    fn report(&mut self, warning: Option<EngineWarning>) {
        if warning != self.reported {
            self.reported.clone_from(&warning);
            let _ = self.inbox.send(Inbox::EngineWarning(warning));
        }
    }

    /// `Ok(Some(""))` is nothing heard, `Ok(None)` a whisper failure, `Err` no model to run.
    fn transcribe(&mut self, samples: &[f32]) -> Result<Option<String>, ModelFailure> {
        self.ensure_loaded(None)?;
        let transcript = match self.run(samples) {
            Ok(t) => t,
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
        Ok(
            match pipeline::finish(&transcript, &self.assets, self.cfg.paste.trailing_space) {
                Outcome::Text(text) => Some(text),
                Outcome::NothingHeard => Some(String::new()),
            },
        )
    }

    fn run(&mut self, samples: &[f32]) -> anyhow::Result<Transcript> {
        let engine = self
            .loaded
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no model loaded"))?;
        engine.transcribe(samples, self.cfg.model.language, &self.assets.prompt)
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
        /// `LoadInfo.warning` of a GPU engine (a named device gone).
        load_warning: Option<EngineWarning>,
        /// The first transcription on a GPU engine fails.
        gpu_run_fails: bool,
        /// A GPU request is answered by an engine on CPU (the real `Engine::load` fallback).
        silent_cpu: bool,
        /// No GPU at all (CPU-only build): a GPU request runs on CPU with no warning.
        no_gpu: bool,
    }

    type Loader = Box<dyn FnMut(&Path, &DeviceChoice) -> anyhow::Result<Fake>>;
    type TestWorker = Worker<Fake, Loader>;

    struct Fake {
        gpu: bool,
        info: LoadInfo,
        fail_run: bool,
    }

    impl Recognizer for Fake {
        fn info(&self) -> &LoadInfo {
            &self.info
        }
        fn transcribe(&mut self, _: &[f32], _: Language, _: &str) -> anyhow::Result<Transcript> {
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
                load_warning: None,
                gpu_run_fails: false,
                silent_cpu: false,
                no_gpu: false,
            }
        }

        fn loader(&self) -> Loader {
            let s = self.clone();
            Box::new(move |_, choice| {
                s.calls.borrow_mut().push(choice.clone());
                let asked_gpu = *choice != DeviceChoice::Cpu;
                let cpu = !asked_gpu || s.silent_cpu || s.no_gpu;
                if !cpu && s.gpu_load_fails || cpu && s.cpu_load_fails {
                    anyhow::bail!("out of memory");
                }
                Ok(Fake {
                    gpu: !cpu,
                    fail_run: s.gpu_run_fails,
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
        let dir = std::env::temp_dir().join(format!("oxi-whisper-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
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
        (Worker::new(script.loader(), tx, rig.layout()), rx)
    }

    /// The next message that is not an `EngineWarning`.
    fn next(rx: &Receiver<Inbox>) -> Option<Inbox> {
        rx.try_iter()
            .find(|m| !matches!(m, Inbox::EngineWarning(_)))
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

    fn job(w: &mut TestWorker) {
        w.handle(WhisperCmd::Transcribe {
            job: JobId(1),
            samples: vec![0.0; 16],
        });
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
        assert_eq!(rate, oxi_core::audio::SAMPLE_RATE);
        let r = rig("real");
        let (tx, rx) = channel();
        let load =
            |p: &Path, c: &DeviceChoice| -> anyhow::Result<Engine> { Ok(Engine::load(p, c)?) };
        let mut w = Worker::new(load, tx, r.layout());
        let mut cfg = r.cfg(DeviceChoice::Cpu);
        cfg.model.custom_path = model.to_str().unwrap().into();
        cfg.model.language = Language::En;
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
