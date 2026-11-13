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

fn hid(id: &str) -> HistoryId {
    HistoryId::parse(id).unwrap()
}
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
            let gpu_fails = s.gpu_load_fails || s.gpu_fails_on.is_some_and(|f| path.ends_with(f));
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
    w.handle(as_origin(test_job(&pushes, "custom"), Origin::History));
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
        origin: Origin::Mic,
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
    w.handle(as_origin(test_job(&pushes, "custom"), Origin::History));
    let _ = outcome(&rx);
    let heard = s.heard.borrow();
    assert_ne!(
        heard[0], samples,
        "a microphone clip gets gain and normalise"
    );
    assert_eq!(heard[1], samples, "a stored one went in as saved");
}

/// A 200 ms clip at -45 dBFS: too quiet for the gate to count five voiced frames.
/// `cmd` (a [`test_job`]) with its audio coming from `origin`.
fn as_origin(cmd: WhisperCmd, origin: Origin) -> WhisperCmd {
    let WhisperCmd::Test(mut job) = cmd else {
        unreachable!()
    };
    job.origin = origin;
    WhisperCmd::Test(job)
}

fn quiet_job(w: &TestWorker, reply: &Pushes, origin: Origin) -> WhisperCmd {
    let WhisperCmd::Test(mut job) = test_job(reply, "custom") else {
        unreachable!()
    };
    job.origin = origin;
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
    let job = quiet_job(&w, &pushes, Origin::HistoryRaw);
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
    let job = quiet_job(&w, &pushes, Origin::Mic);
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
    let reply = pushes.with_words_for(&hid(id));
    let job = WhisperCmd::Test(TestJob {
        id: 9,
        set: SettingsSet::defaults(),
        language: Language::AUTO,
        samples: vec![0.1; 1600],
        origin: Origin::History,
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
    let (stored, how) = crate::daemon::history::read_words(&dir, &hid(id)).unwrap();
    assert_eq!(stored, words.0, "the record holds what the window got");
    assert_eq!(how.unwrap().by, oxi_core::history::TimingBy::Tokens);
    // A dictation under way: nothing runs, nothing is stored over the timed words.
    w.phase.store(Phase::Recording);
    let busy = pushes.with_words_for(&hid(id));
    w.handle(WhisperCmd::Test(TestJob {
        id: 10,
        set: SettingsSet::defaults(),
        language: Language::AUTO,
        samples: vec![0.1; 1600],
        origin: Origin::History,
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
    let gone = pushes.with_words_for(&hid("2026-11-08/000000-000"));
    w.handle(WhisperCmd::Test(TestJob {
        id: 11,
        set: SettingsSet::defaults(),
        language: Language::AUTO,
        samples: vec![0.1; 1600],
        origin: Origin::History,
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
            origin: Origin::History,
            reply: pushes.with_words_for(&hid(id)),
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
    let reply = pushes.with_words_for(&hid(id));
    reply.words_stop().unwrap().store(true, Ordering::Release);
    w.handle(WhisperCmd::Test(TestJob {
        id: 4,
        set: SettingsSet::defaults(),
        language: Language::AUTO,
        samples: vec![0.1; 1600],
        origin: Origin::History,
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
    let (stored, _) = crate::daemon::history::read_words(&dir, &hid(id)).unwrap();
    assert!(stored.iter().all(|w| w.t0.is_none()));
}

#[test]
fn a_history_run_sends_its_words_before_its_text() {
    let (r, s) = (rig("run-words"), Script::new());
    let (mut w, _inbox) = worker(&s, &r);
    w.handle(WhisperCmd::Config(r.cfg(DeviceChoice::Cpu)));
    let pushes = Pushes::default();
    let (_, rx) = pushes.claim(1);
    w.handle(as_origin(test_job(&pushes, "custom"), Origin::History));
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
