//! Coordinator thread: state machine, config, timers (ARCHITECTURE section 2).
//!
//! Executes the effects of `oxi_core::machine` and owns the data they refer to: job audio and
//! text by `JobId`, the active and pending config, the last pasted text. Every decision stays in
//! the machine (ARCHITECTURE 3); this module only carries values to the right worker.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use oxi_core::config::Config;
use oxi_core::machine::{
    self, Effect, EngineWarning, Event, JobId, MachineConfig, Phase, PhaseCell, State,
};
use oxi_core::paste::PasteOutcome;
use oxi_core::tray::{self, TrayBadge};
use oxi_core::types::Millis;
use oxi_core::{stats, strings};

use super::cmd::{
    AudioCmd, ChordHook, Inbox, IpcAsk, PasteCmd, StatusReport, UiCmd, WhisperCmd, Workers,
};
use super::stats::Recorder;

pub struct Coordinator<H> {
    state: State,
    phase: PhaseCell,
    /// Last phase sent as `UiCmd::Tray`.
    tray: Phase,
    /// An error notice was raised and no dictation reached a field or the clipboard since.
    failed: bool,
    /// Last badge sent as `UiCmd::Badge`.
    badge: TrayBadge,
    active: Config,
    warnings: Vec<String>,
    /// The whisper worker's latest `EngineWarning`.
    engine_warning: Option<EngineWarning>,
    /// The whisper worker's latest `Inbox::Backend`.
    backend: String,
    config_error: Option<String>,
    /// Last warnings sent as `UiCmd::Warnings`.
    shown_warnings: Vec<String>,
    /// A config received while busy, with its warnings; becomes active on `ApplyConfig`.
    pending: Option<(Config, Vec<String>)>,
    /// `session` of the last `AudioCmd::Start`; capture events of older ones are stale.
    capture: u64,
    /// Samples of stopped jobs, until whisper gets them.
    audio: HashMap<JobId, Vec<f32>>,
    /// Text of transcribed jobs, until their paste is done.
    text: HashMap<JobId, String>,
    /// For "Copy last text". Memory only: never written to disk or logs.
    last_text: Option<String>,
    /// Audio length of each job from its audio to its paste, for the usage stats.
    spoken: HashMap<JobId, Millis>,
    /// Counts of finished dictations go here; never their text.
    stats: Option<Recorder>,
    workers: Workers,
    hook: H,
}

impl<H: ChordHook> Coordinator<H> {
    /// Sends `config` to the workers and the hook, so they start in step with the machine.
    /// `phase` is the shared `PHASE` cell the hook and UI read.
    pub fn new(
        config: Config,
        warnings: Vec<String>,
        workers: Workers,
        hook: H,
        phase: PhaseCell,
    ) -> Self {
        phase.store(Phase::Idle);
        let mut c = Self {
            state: State::new(MachineConfig::from(&config)),
            phase,
            tray: Phase::Idle,
            failed: false,
            badge: TrayBadge::None,
            active: config,
            warnings,
            engine_warning: None,
            backend: "none".into(),
            config_error: None,
            shown_warnings: Vec::new(),
            pending: None,
            capture: 0,
            audio: HashMap::new(),
            text: HashMap::new(),
            last_text: None,
            spoken: HashMap::new(),
            stats: Recorder::system(),
            workers,
            hook,
        };
        c.send_config();
        c.sync_warnings();
        c
    }

    /// The thread body: waits for the next message or the machine's next deadline (then a
    /// `Tick`), until `Shutdown` or every sender is gone. `now` is the clock.
    pub fn run(mut self, inbox: &Receiver<Inbox>, now: impl Fn() -> Millis) {
        loop {
            let msg = match wait(self.state.next_deadline(), now()) {
                Some(timeout) => inbox.recv_timeout(timeout),
                None => inbox.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match msg {
                Ok(Inbox::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(msg) => self.handle(msg, now()),
                Err(RecvTimeoutError::Timeout) => self.handle(Inbox::Event(Event::Tick), now()),
            }
        }
    }

    pub fn handle(&mut self, msg: Inbox, now: Millis) {
        self.dispatch(msg, now);
        self.sync_warnings();
        self.sync_badge();
    }

    fn dispatch(&mut self, msg: Inbox, now: Millis) {
        match msg {
            Inbox::Event(event) => {
                if let Event::PasteDone { job, outcome } = event {
                    self.count(job, outcome);
                    self.keep_last_text(job);
                    // A failed paste raises its notice again in `feed`.
                    self.failed = false;
                }
                self.feed(event, now);
            }
            Inbox::Capture { session, event } => {
                if session == self.capture {
                    self.feed(event, now);
                }
            }
            Inbox::JobAudio { job, samples } => {
                self.spoken.insert(job, stats::audio_length(samples.len()));
                self.audio.insert(job, samples);
                self.feed(Event::JobAudioReady(job), now);
            }
            Inbox::Transcribed { job, text: None } => {
                self.spoken.remove(&job);
                self.feed(Event::TranscriptionFailed(job), now);
            }
            Inbox::Transcribed {
                job,
                text: Some(text),
            } => {
                let empty = text.trim().is_empty();
                if empty {
                    self.spoken.remove(&job);
                } else {
                    self.text.insert(job, text);
                }
                self.feed(Event::TranscriptionDone { job, empty }, now);
            }
            Inbox::Config { config, warnings } => {
                let machine = MachineConfig::from(&*config);
                self.pending = Some((*config, warnings));
                self.config_error = None;
                self.feed(Event::ConfigChanged(machine), now);
            }
            Inbox::ConfigError(e) => {
                tracing::warn!("config not applied: {e}");
                self.config_error = Some(e.to_string());
            }
            Inbox::EngineWarning(warning) => self.engine_warning = warning,
            Inbox::Backend(device) => self.backend = device,
            Inbox::AssetsChanged => send(&self.workers.whisper, WhisperCmd::ReloadAssets),
            Inbox::Ipc(IpcAsk::Status(reply)) => {
                // The asking connection may be gone already; nothing to do then.
                let _ = reply.send(self.status());
            }
            Inbox::Ipc(IpcAsk::PreviewOverlay(position)) => {
                self.ui(UiCmd::PreviewOverlay(position));
            }
            Inbox::CopyLastText => {
                if let Some(text) = &self.last_text {
                    send(&self.workers.paste, PasteCmd::CopyOnly(text.clone()));
                }
            }
            Inbox::Shutdown => {} // `run` ends the loop before it gets here
        }
    }

    /// Machine steps until no effect answers with an event. Mirror the phase first (the hook
    /// reads it for Esc), then execute. An answer waits until every effect of its step ran, so a
    /// later effect of that step (an overlay) never overrides what the answer's step decided.
    fn feed(&mut self, event: Event, now: Millis) {
        let mut queue = VecDeque::from([event]);
        while let Some(event) = queue.pop_front() {
            let effects = machine::step(&mut self.state, event, now);
            let phase = self.state.phase();
            self.phase.store(phase);
            if phase != self.tray {
                self.tray = phase;
                self.ui(UiCmd::Tray(phase));
            }
            for effect in effects {
                queue.extend(self.execute(&effect));
            }
        }
    }

    /// Runs one effect; returns the event it answers with, if any.
    fn execute(&mut self, effect: &Effect) -> Option<Event> {
        match *effect {
            Effect::StartCapture => {
                self.capture += 1;
                let session = self.capture;
                send(&self.workers.audio, AudioCmd::Start { session });
            }
            Effect::StopCapture(job) => send(&self.workers.audio, AudioCmd::Stop(job)),
            Effect::DiscardCapture => send(&self.workers.audio, AudioCmd::Discard),
            Effect::DropJob(job) => {
                self.audio.remove(&job);
                self.text.remove(&job);
                self.spoken.remove(&job);
            }
            Effect::Preload => send(&self.workers.whisper, WhisperCmd::Preload),
            Effect::Unload => send(&self.workers.whisper, WhisperCmd::Unload),
            Effect::Transcribe(job) => {
                if let Some(samples) = self.audio.remove(&job) {
                    send(
                        &self.workers.whisper,
                        WhisperCmd::Transcribe { job, samples },
                    );
                } else {
                    // Cannot happen (the machine waits for JobAudioReady); fail the job, not the machine.
                    self.spoken.remove(&job);
                    return Some(Event::TranscriptionFailed(job));
                }
            }
            Effect::Paste(job) => match self.text.get(&job) {
                Some(text) => send(
                    &self.workers.paste,
                    PasteCmd::Paste {
                        job,
                        text: text.clone(),
                        cfg: self.active.paste.clone(),
                    },
                ),
                // Cannot happen (empty text is never pasted); fail the job, not the machine.
                None => {
                    return Some(Event::PasteDone {
                        job,
                        outcome: PasteOutcome::Failed,
                    });
                }
            },
            Effect::PollChord => {
                return Some(Event::ChordPolled {
                    held: self.hook.chord_held(),
                });
            }
            Effect::ReinstallHook => self.hook.reinstall(),
            Effect::ApplyConfig => {
                if let Some((config, warnings)) = self.pending.take() {
                    self.active = config;
                    self.warnings = warnings;
                    self.send_config();
                }
            }
            // shortcut: sounds land in M10; until then a played sound is only logged.
            Effect::Play(sound) => tracing::debug!(?sound, "sound"),
            Effect::Overlay(state) => self.ui(UiCmd::Overlay(state)),
            Effect::Notify(notice) => {
                self.failed |= tray::is_error(notice);
                self.ui(UiCmd::Notify(notice));
            }
        }
        None
    }

    /// A dictation that reached the field or the clipboard adds its word count and audio length
    /// to the usage stats.
    fn count(&mut self, job: JobId, outcome: PasteOutcome) {
        let speech = self.spoken.remove(&job).unwrap_or_default();
        if let (Some(stats), Some(text)) = (&self.stats, self.text.get(&job))
            && outcome != PasteOutcome::Failed
        {
            stats.add(stats::word_count(text), speech);
        }
    }

    /// Every outcome keeps the text, a failed paste too: "Copy last text" still has it (item 42).
    fn keep_last_text(&mut self, job: JobId) {
        if let Some(text) = self.text.remove(&job)
            && self.last_text.replace(text).is_none()
        {
            self.ui(UiCmd::CanCopy);
        }
    }

    fn send_config(&self) {
        let cfg = &self.active;
        send(&self.workers.audio, AudioCmd::Config(cfg.audio.clone()));
        send(
            &self.workers.whisper,
            WhisperCmd::Config(Box::new(cfg.clone())),
        );
        self.ui(UiCmd::Config {
            overlay: cfg.overlay.clone(),
            meter_sensitivity_db: cfg.audio.meter_sensitivity_db,
        });
        self.hook.set_hotkey(&cfg.hotkey);
    }

    fn status(&self) -> StatusReport {
        StatusReport {
            phase: self.state.phase(),
            model: self.active.model.id.clone(),
            device: self.active.audio.device.clone(),
            gpu: self.active.model.gpu.clone(),
            backend: self.backend.clone(),
            warnings: self.warnings(),
        }
    }

    /// Load corrections of the active config, the engine warning, then the last parse error.
    fn warnings(&self) -> Vec<String> {
        self.warnings
            .iter()
            .cloned()
            .chain(self.engine_warning.as_ref().map(strings::engine_warning))
            .chain(self.config_error.clone())
            .collect()
    }

    /// Tells the tray when the warnings changed.
    fn sync_warnings(&mut self) {
        let now = self.warnings();
        if now != self.shown_warnings {
            self.shown_warnings.clone_from(&now);
            self.ui(UiCmd::Warnings(now));
        }
    }

    /// The error badge while the last dictation failed or the config file is invalid; tells
    /// the tray when it changed.
    fn sync_badge(&mut self) {
        let badge = if self.failed || self.config_error.is_some() {
            TrayBadge::Error
        } else {
            TrayBadge::None
        };
        if badge != self.badge {
            self.badge = badge;
            self.ui(UiCmd::Badge(badge));
        }
    }

    fn ui(&self, cmd: UiCmd) {
        send(&self.workers.ui, cmd);
        (self.workers.wake_ui)();
    }
}

/// A worker that is gone (panicked) loses the command; the daemon keeps running.
fn send<T>(tx: &Sender<T>, cmd: T) {
    if tx.send(cmd).is_err() {
        tracing::error!(
            command = std::any::type_name::<T>(),
            "worker gone, command dropped"
        );
    }
}

/// How long to wait for the next message: until `deadline`, at once if it passed, forever
/// without one.
fn wait(deadline: Option<Millis>, now: Millis) -> Option<Duration> {
    deadline.map(|at| Duration::from_millis((at - now).0))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::mpsc::channel;

    use oxi_core::config::{ConfigError, Hotkey};
    use oxi_core::machine::{EmptyAudio, Notice, OverlayState};

    use super::*;

    #[derive(Default)]
    struct FakeHook {
        held: Cell<bool>,
        polls: Cell<u32>,
        hotkeys: RefCell<Vec<Hotkey>>,
    }

    impl ChordHook for Rc<FakeHook> {
        fn chord_held(&self) -> bool {
            self.polls.set(self.polls.get() + 1);
            self.held.get()
        }
        fn reinstall(&self) {}
        fn set_hotkey(&self, hotkey: &Hotkey) {
            self.hotkeys.borrow_mut().push(hotkey.clone());
        }
    }

    struct Rig {
        c: Coordinator<Rc<FakeHook>>,
        hook: Rc<FakeHook>,
        phase: PhaseCell,
        audio: Receiver<AudioCmd>,
        whisper: Receiver<WhisperCmd>,
        paste: Receiver<PasteCmd>,
        ui: Receiver<UiCmd>,
    }

    fn drain<T>(rx: &Receiver<T>) -> Vec<T> {
        rx.try_iter().collect()
    }

    impl Rig {
        /// A coordinator with the default config; the start-up config commands are drained.
        fn new() -> Self {
            Self::with(Config::default())
        }

        fn with(config: Config) -> Self {
            let (audio_tx, audio) = channel();
            let (whisper_tx, whisper) = channel();
            let (paste_tx, paste) = channel();
            let (ui_tx, ui) = channel();
            let workers = Workers {
                audio: audio_tx,
                whisper: whisper_tx,
                paste: paste_tx,
                ui: ui_tx,
                wake_ui: Box::new(|| {}),
            };
            let hook = Rc::new(FakeHook {
                held: Cell::new(true),
                ..FakeHook::default()
            });
            let phase = PhaseCell::default();
            let c = Coordinator::new(config, Vec::new(), workers, Rc::clone(&hook), phase.clone());
            let rig = Self {
                c,
                hook,
                phase,
                audio,
                whisper,
                paste,
                ui,
            };
            rig.drain_all();
            rig
        }

        fn drain_all(&self) {
            drain(&self.audio);
            drain(&self.whisper);
            drain(&self.paste);
            drain(&self.ui);
        }

        fn event(&mut self, event: Event, at: u64) {
            self.c.handle(Inbox::Event(event), Millis(at));
        }

        /// Holds the chord from 0 to 1000 ms; returns the stopped job.
        fn record(&mut self) -> JobId {
            self.event(Event::ChordDown, 0);
            self.event(Event::ChordUp, 1000);
            self.event(Event::Tick, 2000); // past the release tail
            let stops: Vec<_> = drain(&self.audio)
                .into_iter()
                .filter_map(|cmd| match cmd {
                    AudioCmd::Stop(job) => Some(job),
                    _ => None,
                })
                .collect();
            assert_eq!(stops.len(), 1, "one Stop per recording");
            stops[0]
        }

        /// Records, transcribes `text` and returns the job being pasted.
        fn transcribed(&mut self, text: &str) -> JobId {
            self.event(Event::ModelLoaded, 0);
            let job = self.record();
            self.c.handle(
                Inbox::JobAudio {
                    job,
                    samples: vec![0.5; 4],
                },
                Millis(2100),
            );
            self.c.handle(
                Inbox::Transcribed {
                    job,
                    text: Some(text.to_owned()),
                },
                Millis(2500),
            );
            job
        }
    }

    #[test]
    fn chord_down_starts_capture_preloads_and_shows_overlay() {
        let mut rig = Rig::new();
        rig.event(Event::ChordDown, 0);

        assert_eq!(drain(&rig.audio), [AudioCmd::Start { session: 1 }]);
        assert!(
            drain(&rig.whisper)
                .iter()
                .any(|c| matches!(c, WhisperCmd::Preload))
        );
        let ui = drain(&rig.ui);
        assert!(ui.contains(&UiCmd::Tray(Phase::Recording)));
        assert!(
            ui.iter()
                .any(|c| matches!(c, UiCmd::Overlay(OverlayState::Recording { .. })))
        );
        assert_eq!(rig.phase.load(), Phase::Recording);
    }

    #[test]
    fn job_audio_goes_to_whisper_and_text_transcribed() {
        let mut rig = Rig::new();
        rig.event(Event::ModelLoaded, 0);
        let job = rig.record();
        rig.c.handle(
            Inbox::JobAudio {
                job,
                samples: vec![0.25, -0.5],
            },
            Millis(2100),
        );
        let transcribe: Vec<_> = drain(&rig.whisper)
            .into_iter()
            .filter_map(|c| match c {
                WhisperCmd::Transcribe { job, samples } => Some((job, samples)),
                _ => None,
            })
            .collect();
        assert_eq!(transcribe, [(job, vec![0.25, -0.5])]);

        rig.c.handle(
            Inbox::Transcribed {
                job,
                text: Some("hello".into()),
            },
            Millis(2500),
        );
        let pastes = drain(&rig.paste);
        let [PasteCmd::Paste { job: j, text, cfg }] = pastes.as_slice() else {
            panic!("expected one Paste");
        };
        assert_eq!((*j, text.as_str()), (job, "hello"));
        assert_eq!(*cfg, Config::default().paste);
    }

    #[test]
    fn paste_done_keeps_last_text_for_copy_even_when_it_failed() {
        for outcome in [PasteOutcome::Pasted, PasteOutcome::Failed] {
            let mut rig = Rig::new();
            rig.c.handle(Inbox::CopyLastText, Millis(0));
            assert!(drain(&rig.paste).is_empty(), "no text yet, nothing to copy");

            let job = rig.transcribed("hello");
            rig.drain_all(); // the Paste itself
            rig.event(Event::PasteDone { job, outcome }, 3000);
            assert!(drain(&rig.ui).contains(&UiCmd::CanCopy));

            rig.c.handle(Inbox::CopyLastText, Millis(3100));
            let copies = drain(&rig.paste);
            let [PasteCmd::CopyOnly(text)] = copies.as_slice() else {
                panic!("expected one CopyOnly after {outcome:?}");
            };
            assert_eq!(text, "hello");
        }
    }

    #[test]
    fn a_finished_dictation_updates_the_stats_file_a_failed_one_does_not() {
        let dir = std::env::temp_dir().join(format!("oxi-coord-stats-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(stats::FILE);
        let today = || stats::Date::from_ymd(2026, 11, 7).unwrap();
        for outcome in [PasteOutcome::Failed, PasteOutcome::Pasted] {
            let mut rig = Rig::new();
            rig.c.stats = Some(Recorder::spawn(path.clone(), today).unwrap());
            rig.event(Event::ModelLoaded, 0);
            let job = rig.record();
            let samples = vec![0.1; 16_000]; // 1 s
            rig.c.handle(Inbox::JobAudio { job, samples }, Millis(2100));
            let text = Some("Привет, мир".to_owned());
            rig.c.handle(Inbox::Transcribed { job, text }, Millis(2500));
            rig.event(Event::PasteDone { job, outcome }, 3000);
            drop(rig.c.stats.take()); // waits for the write
            assert!(rig.c.spoken.is_empty());
            if outcome == PasteOutcome::Failed {
                assert!(!path.exists(), "a failed paste is not counted");
            }
        }
        let sum = crate::daemon::stats::summary(Some(&path), today());
        assert_eq!(
            (sum.dictations, sum.words, sum.wpm, sum.streak),
            (1, 2, 120, 1)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_job_without_audio_leaves_no_spoken_entry() {
        let mut rig = Rig::new();
        let job = JobId(9);
        rig.c.spoken.insert(job, Millis(500));
        let answer = rig.c.execute(&Effect::Transcribe(job));
        assert_eq!(answer, Some(Event::TranscriptionFailed(job)));
        assert!(rig.c.spoken.is_empty());
    }

    #[test]
    fn empty_or_failed_transcription_pastes_nothing() {
        for text in [Some("  ".to_owned()), None] {
            let mut rig = Rig::new();
            rig.event(Event::ModelLoaded, 0);
            let job = rig.record();
            rig.c.handle(
                Inbox::JobAudio {
                    job,
                    samples: vec![0.0],
                },
                Millis(2100),
            );
            rig.c.handle(Inbox::Transcribed { job, text }, Millis(2500));
            assert!(drain(&rig.paste).is_empty());
            assert_eq!(rig.phase.load(), Phase::Idle);
        }
    }

    #[test]
    fn job_without_audio_never_reaches_whisper() {
        let cases = [
            (EmptyAudio::NothingHeard, None),
            (EmptyAudio::MicSilence, Some(Notice::MicSilence)),
            (EmptyAudio::Failed, Some(Notice::CaptureFailed)),
        ];
        for (why, notify) in cases {
            let mut rig = Rig::new();
            rig.event(Event::ModelLoaded, 0);
            let job = rig.record();
            rig.drain_all();
            rig.event(Event::JobAudioEmpty { job, why }, 2100);
            assert!(
                !drain(&rig.whisper)
                    .iter()
                    .any(|c| matches!(c, WhisperCmd::Transcribe { .. })),
                "{why:?}"
            );
            let notices: Vec<_> = drain(&rig.ui)
                .into_iter()
                .filter_map(|c| match c {
                    UiCmd::Notify(n) => Some(n),
                    _ => None,
                })
                .collect();
            assert_eq!(notices, Vec::from_iter(notify), "{why:?}");
            assert_eq!(rig.phase.load(), Phase::Idle);
        }
    }

    #[test]
    fn stale_capture_event_does_not_touch_the_next_recording() {
        let mut rig = Rig::new();
        rig.record(); // session 1
        rig.event(Event::ChordDown, 3000); // session 2
        assert_eq!(drain(&rig.audio), [AudioCmd::Start { session: 2 }]);

        let lost = |session| Inbox::Capture {
            session,
            event: Event::DeviceLost,
        };
        rig.c.handle(lost(1), Millis(3500));
        assert!(drain(&rig.audio).is_empty(), "the old capture's loss");
        assert_eq!(rig.phase.load(), Phase::Recording);

        rig.c.handle(lost(2), Millis(3600));
        assert!(
            drain(&rig.audio)
                .iter()
                .any(|c| matches!(c, AudioCmd::Stop(_)))
        );
    }

    #[test]
    fn esc_discards_and_never_pastes() {
        let mut rig = Rig::new();
        rig.event(Event::ModelLoaded, 0);
        rig.event(Event::ChordDown, 0);
        rig.event(Event::Esc, 500);
        rig.event(Event::ChordUp, 1000);
        rig.event(Event::Tick, 2000);

        let audio = drain(&rig.audio);
        assert!(audio.contains(&AudioCmd::Discard));
        assert!(!audio.iter().any(|c| matches!(c, AudioCmd::Stop(_))));
        assert!(
            !drain(&rig.whisper)
                .iter()
                .any(|c| matches!(c, WhisperCmd::Transcribe { .. }))
        );
        assert!(drain(&rig.paste).is_empty());
        assert_eq!(rig.phase.load(), Phase::Idle);
    }

    #[test]
    fn config_received_while_recording_applies_when_idle() {
        let mut rig = Rig::new();
        rig.event(Event::ChordDown, 0);
        let mut config = Config::default();
        config.audio.device = "Mic X".into();
        config.audio.meter_sensitivity_db = -6.0;
        config.model.gpu = oxi_core::device::DeviceChoice::Named("Vulkan1".into());
        rig.c.handle(
            Inbox::Config {
                config: Box::new(config.clone()),
                warnings: vec!["gain clamped".into()],
            },
            Millis(100),
        );
        let configs = |audio: Vec<AudioCmd>| {
            audio
                .into_iter()
                .filter(|c| matches!(c, AudioCmd::Config(_)))
                .collect::<Vec<_>>()
        };
        assert!(configs(drain(&rig.audio)).is_empty(), "not while recording");
        let hotkeys_before = rig.hook.hotkeys.borrow().len();

        rig.event(Event::Esc, 500);
        assert_eq!(
            configs(drain(&rig.audio)),
            [AudioCmd::Config(config.audio.clone())]
        );
        assert_eq!(rig.hook.hotkeys.borrow().len(), hotkeys_before + 1);
        let ui = drain(&rig.ui);
        assert!(ui.contains(&UiCmd::Config {
            overlay: config.overlay.clone(),
            meter_sensitivity_db: config.audio.meter_sensitivity_db,
        }));
        assert!(ui.contains(&UiCmd::Warnings(vec!["gain clamped".into()])));

        let (tx, rx) = channel();
        rig.c.handle(Inbox::Ipc(IpcAsk::Status(tx)), Millis(600));
        let status = rx.try_recv().expect("status answered");
        assert_eq!(status.device, "Mic X");
        assert_eq!(
            status.gpu,
            oxi_core::device::DeviceChoice::Named("Vulkan1".into())
        );
        assert_eq!(status.warnings, ["gain clamped"]);
    }

    #[test]
    fn engine_warning_shows_in_status_until_cleared() {
        let mut rig = Rig::new();
        let status = |rig: &mut Rig| {
            let (tx, rx) = channel();
            rig.c.handle(Inbox::Ipc(IpcAsk::Status(tx)), Millis(0));
            rx.try_recv().expect("status answered").warnings
        };
        rig.c
            .handle(Inbox::ConfigError(ConfigError::ReadOnly), Millis(0));
        let error = ConfigError::ReadOnly.to_string();
        assert_eq!(status(&mut rig), std::slice::from_ref(&error));
        let oom = EngineWarning::CpuFallback {
            reason: "out of memory".into(),
        };
        rig.c.handle(Inbox::EngineWarning(Some(oom)), Millis(0));
        assert_eq!(
            status(&mut rig),
            ["running on CPU: out of memory".to_owned(), error.clone()]
        );
        rig.c.handle(Inbox::EngineWarning(None), Millis(0));
        assert_eq!(status(&mut rig), std::slice::from_ref(&error));
        // The tray heard each change once, the last one being the current list.
        let tray: Vec<_> = drain(&rig.ui)
            .into_iter()
            .filter_map(|c| match c {
                UiCmd::Warnings(w) => Some(w),
                _ => None,
            })
            .collect();
        assert_eq!(
            tray,
            [
                vec![error.clone()],
                vec!["running on CPU: out of memory".to_owned(), error.clone()],
                vec![error],
            ]
        );
    }

    #[test]
    fn errors_raise_the_tray_badge_until_a_dictation_lands_or_the_config_is_fixed() {
        let badges = |rig: &Rig| -> Vec<TrayBadge> {
            drain(&rig.ui)
                .into_iter()
                .filter_map(|c| match c {
                    UiCmd::Badge(b) => Some(b),
                    _ => None,
                })
                .collect()
        };
        let mut rig = Rig::new();
        let job = rig.transcribed("one");
        rig.event(
            Event::PasteDone {
                job,
                outcome: PasteOutcome::Failed,
            },
            3000,
        );
        assert_eq!(badges(&rig), [TrayBadge::Error], "a failed paste");
        let job = rig.transcribed("two");
        rig.event(
            Event::PasteDone {
                job,
                outcome: PasteOutcome::Pasted,
            },
            4000,
        );
        assert_eq!(badges(&rig), [TrayBadge::None], "the next one landed");

        let job = rig.record();
        rig.event(
            Event::JobAudioEmpty {
                job,
                why: EmptyAudio::MicSilence,
            },
            2100,
        );
        assert!(badges(&rig).is_empty(), "a hint is no error");

        rig.c
            .handle(Inbox::ConfigError(ConfigError::ReadOnly), Millis(5000));
        assert_eq!(badges(&rig), [TrayBadge::Error], "an invalid config");
        rig.c.handle(
            Inbox::Config {
                config: Box::new(Config::default()),
                warnings: Vec::new(),
            },
            Millis(5100),
        );
        assert_eq!(badges(&rig), [TrayBadge::None], "the config was fixed");
    }

    #[test]
    fn status_backend_is_the_device_the_model_loaded_on() {
        let mut rig = Rig::new();
        let backend = |rig: &mut Rig| {
            let (tx, rx) = channel();
            rig.c.handle(Inbox::Ipc(IpcAsk::Status(tx)), Millis(0));
            rx.try_recv().expect("status answered").backend
        };
        assert_eq!(backend(&mut rig), "none");
        rig.c.handle(Inbox::Backend("Vulkan0".into()), Millis(0));
        assert_eq!(backend(&mut rig), "Vulkan0");
        rig.c.handle(Inbox::Backend("cpu".into()), Millis(0));
        assert_eq!(backend(&mut rig), "cpu");
    }

    #[test]
    fn poll_finding_the_chord_released_stops_the_recording() {
        let mut rig = Rig::new();
        rig.hook.held.set(false);
        rig.event(Event::ChordDown, 0);
        rig.event(Event::Tick, 1000); // watchdog poll due; the key-up was never delivered
        rig.event(Event::Tick, 2000); // past the release tail
        assert!(rig.hook.polls.get() >= 1);
        assert!(
            drain(&rig.audio)
                .iter()
                .any(|c| matches!(c, AudioCmd::Stop(_)))
        );
    }

    #[test]
    fn poll_answer_overlay_is_not_overridden_by_its_own_step() {
        let mut config = Config::default();
        config.audio.tail_ms = 0;
        config.audio.max_seconds = 31; // `ending_soon` from 1 s, the tick that also polls
        let mut rig = Rig::with(config);
        rig.hook.held.set(false);
        rig.event(Event::ModelLoaded, 0);
        rig.event(Event::ChordDown, 0);
        rig.drain_all();
        rig.event(Event::Tick, 1000);
        let overlays: Vec<_> = drain(&rig.ui)
            .into_iter()
            .filter(|c| matches!(c, UiCmd::Overlay(_)))
            .collect();
        assert_eq!(
            overlays.last(),
            Some(&UiCmd::Overlay(OverlayState::Transcribing)),
            "{overlays:?}"
        );
    }

    #[test]
    fn run_handles_messages_until_shutdown() {
        let rig = Rig::new();
        let (tx, rx) = channel();
        for msg in [Inbox::Event(Event::ChordDown), Inbox::Shutdown] {
            tx.send(msg).expect("open channel");
        }
        rig.c.run(&rx, || Millis(0));
        assert_eq!(drain(&rig.audio), [AudioCmd::Start { session: 1 }]);
    }

    #[test]
    fn wait_until_the_deadline() {
        let cases = [
            (None, 500, None),
            (Some(800), 500, Some(300)),
            (Some(500), 500, Some(0)),
            (Some(200), 500, Some(0)),
        ];
        for (deadline, now, want) in cases {
            assert_eq!(
                wait(deadline.map(Millis), Millis(now)),
                want.map(Duration::from_millis),
                "deadline {deadline:?} at {now}"
            );
        }
    }
}
