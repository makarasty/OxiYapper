//! Coordinator thread: state machine, config, timers (ARCHITECTURE section 2).
//!
//! Executes the effects of `oxi_core::machine` and owns the data they refer to: job audio and
//! text by `JobId`, the active and pending config, the last pasted text. Every decision stays in
//! the machine (ARCHITECTURE 3); this module only carries values to the right worker.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use oxi_core::config::{Config, History, TypeSpeed};
use oxi_core::ipc::{DeleteResult, Push, SoundWarning, TestFailure, TestOutcome};
use oxi_core::machine::{
    self, Effect, EmptyAudio, EngineWarning, Event, JobId, MachineConfig, ModelFailure, Note,
    Notice, OverlayState, Phase, PhaseCell, State,
};
use oxi_core::overlay;
use oxi_core::paste::{ClipboardOnlyWhy, PasteOutcome};
use oxi_core::tray::{self, TrayBadge};
use oxi_core::types::Millis;
use oxi_core::{stats, strings};

use super::cmd::{
    AudioCmd, ChordHook, HistoryCmd, Inbox, IpcAsk, PasteCmd, SoundCmd, StatusReport, UiCmd,
    WhisperCmd, Workers,
};
use super::stats::Recorder;

/// How long a quit waits for the dictation in flight: its text, or its audio back from the
/// capture. Short, so a settings Restart (3 s wait for the pipe) finds this daemon gone.
const SHUTDOWN_GRACE: Millis = Millis(2000);

/// Whose progress the overlay's bar shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bar {
    None,
    Transcribing(JobId),
    Typing(JobId),
}

/// A transcription the coordinator times, to learn the speed.
#[derive(Debug, Clone, Copy)]
struct Run {
    job: JobId,
    audio_ms: u64,
    /// `None` until the bar starts (the model is loaded).
    started: Option<Millis>,
    /// Sent while an earlier job was still in whisper: its time includes that wait, so the
    /// speed is not learned from it.
    queued: bool,
}

pub struct Coordinator<H> {
    state: State,
    phase: PhaseCell,
    /// Last phase sent as `UiCmd::Tray`.
    tray: Phase,
    /// Error notices whose cause is still there, each with the capture session it was raised in.
    /// A cause that goes away clears its notice (`clear_failed`); a landed dictation clears all.
    failed: Vec<(Notice, u64)>,
    /// Last badge sent as `UiCmd::Badge`.
    badge: TrayBadge,
    active: Config,
    warnings: Vec<String>,
    /// The whisper worker's latest `EngineWarning`.
    engine_warning: Option<EngineWarning>,
    /// The sound worker's latest `Inbox::SoundWarnings`.
    sound_warnings: Vec<SoundWarning>,
    /// The history worker's latest `Inbox::HistoryWarning`.
    history_warning: Option<String>,
    /// The 16 kHz samples of a job being transcribed, kept for the history file; only while
    /// `[history]` is on or the job streamed a recording.
    history_audio: HashMap<JobId, Vec<f32>>,
    /// The recording each job streamed to the history folder (`Inbox::JobAudio`), until its save
    /// or `no_transcript`. It is what says "history was on" for that job: a spool made while
    /// saving was on always ends as an entry or is deleted, whatever the setting is by then.
    spools: HashMap<JobId, PathBuf>,
    /// The streamed recordings of the earlier parts a merged job carries, kept until the merged
    /// dictation is filed (a crash before that still finds them), then deleted.
    carried_spools: HashMap<JobId, Vec<PathBuf>>,
    /// Jobs in `history_audio` because saving was on (or they streamed a recording): they are
    /// filed as the setting says at their end. The others are there for the hold gap only.
    history_wanted: HashSet<JobId>,
    /// The whisper worker's latest `Inbox::Backend`.
    backend: String,
    /// The last `Event::ModelFailed` until a load works or the model setting changes.
    model_failure: Option<ModelFailure>,
    /// Which job the bar in `Workers::progress` belongs to (DS-26.5: a new phase is a new bar).
    bar: Bar,
    /// Learned speed of the transcriptions, per (model id, backend): how many times the fit of
    /// `overlay::transcribe_base_ms` a run took, averaged over recent runs. Memory only.
    speed: HashMap<(String, String), f32>,
    /// The job in `transcribing` with its audio length, and when its bar started.
    run: Option<Run>,
    /// The time of the message being handled (the machine's clock).
    clock: Millis,
    /// The overlay state last asked for.
    pill: OverlayState,
    /// Was a settings window connected at the last message?
    window_up: bool,
    /// The last status the settings window was sent unasked.
    pushed: Option<StatusReport>,
    /// The job last sent to whisper while `Phase::Transcribing`; a progress of another is late.
    transcribing: Option<JobId>,
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
    /// Job -> the jobs whose audio goes in front of its own (`Effect::Merge`), until its audio
    /// arrives.
    merges: HashMap<JobId, Vec<JobId>>,
    /// For "Copy last text". Memory only: never written to disk or logs.
    last_text: Option<String>,
    /// Audio length of each job from its audio to its paste, for the usage stats.
    spoken: HashMap<JobId, Millis>,
    /// Counts of finished dictations go here; never their text.
    stats: Option<Recorder>,
    workers: Workers,
    hook: H,
    /// Quit is waiting for the dictation in flight: its text goes to the clipboard, not pasted.
    quitting: bool,
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
            failed: Vec::new(),
            badge: TrayBadge::None,
            active: config,
            warnings,
            engine_warning: None,
            sound_warnings: Vec::new(),
            history_warning: None,
            history_audio: HashMap::new(),
            spools: HashMap::new(),
            carried_spools: HashMap::new(),
            history_wanted: HashSet::new(),
            backend: "none".into(),
            model_failure: None,
            bar: Bar::None,
            speed: HashMap::new(),
            run: None,
            clock: Millis(0),
            pill: OverlayState::Hidden,
            window_up: false,
            pushed: None,
            transcribing: None,
            config_error: None,
            shown_warnings: Vec::new(),
            pending: None,
            capture: 0,
            audio: HashMap::new(),
            text: HashMap::new(),
            merges: HashMap::new(),
            last_text: None,
            spoken: HashMap::new(),
            stats: Recorder::system(),
            workers,
            hook,
            quitting: false,
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
                Ok(Inbox::Shutdown) => return self.shutdown(inbox, &now, SHUTDOWN_GRACE),
                Err(RecvTimeoutError::Disconnected) => return,
                Ok(msg) => self.handle(msg, now()),
                Err(RecvTimeoutError::Timeout) => self.handle(Inbox::Event(Event::Tick), now()),
            }
        }
    }

    /// Quit (tray, IPC `Shutdown`, settings Restart): a recording stops as on a session lock (its
    /// audio kept after speech), then the dictation in flight gets `grace` to finish. Its text
    /// goes to the history and the clipboard. What is still running then is cut: whisper stops at
    /// its next check, and a streamed recording stays in the history folder (kept as failed;
    /// a carried part is deleted instead when saving was turned off).
    fn shutdown(mut self, inbox: &Receiver<Inbox>, now: &impl Fn() -> Millis, grace: Millis) {
        tracing::info!(phase = ?self.state.phase(), "shutdown requested");
        self.quitting = true;
        self.handle(Inbox::Event(Event::Lock), now());
        let end = now() + grace;
        while self.state.busy() {
            let left = end - now();
            if left == Millis(0) {
                // Whisper's cancel check reads the phase: Idle ends its run.
                self.phase.store(Phase::Idle);
                tracing::warn!("shutdown: a dictation was cut short; a saved recording is kept");
                // Earlier parts still waiting for the merge: kept as failed, or deleted when
                // saving was turned off since.
                let jobs: Vec<JobId> = self.carried_spools.keys().copied().collect();
                for job in jobs {
                    self.give_up_carried(job);
                }
                return;
            }
            // A job waiting out its hold gap is pasted at the machine's next deadline.
            let wake = self
                .state
                .next_deadline()
                .map_or(left, |at| left.min(at - now()));
            match inbox.recv_timeout(Duration::from_millis(wake.0)) {
                // Nothing new starts now; an IPC ask is dropped, its connection sees it.
                Ok(Inbox::Event(Event::ChordDown) | Inbox::Ipc(_) | Inbox::Shutdown) => {}
                Err(RecvTimeoutError::Timeout) => self.handle(Inbox::Event(Event::Tick), now()),
                Ok(msg) => self.handle(msg, now()),
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        tracing::info!("shutdown: no dictation in flight");
    }

    pub fn handle(&mut self, msg: Inbox, now: Millis) {
        let up = super::ipc::connected();
        if up != self.window_up {
            // A window that just connected has seen nothing (but what this message answers it).
            self.window_up = up;
            self.pushed = None;
        }
        self.dispatch(msg, now);
        self.sync_warnings();
        self.sync_badge();
        if up && let Some(report) = self.changed_status() {
            super::ipc::announce_status(&report);
        }
    }

    /// The status to push when what the window shows changed since the last push (the first
    /// one after it connected too); a change of the phase alone is not worth a push.
    fn changed_status(&mut self) -> Option<StatusReport> {
        let report = self.status();
        let same = self.pushed.as_ref().is_some_and(|p| {
            *p == StatusReport {
                phase: p.phase,
                ..report.clone()
            }
        });
        if same {
            return None;
        }
        self.pushed = Some(report.clone());
        Some(report)
    }

    fn dispatch(&mut self, msg: Inbox, now: Millis) {
        match msg {
            Inbox::Event(event) => self.event(event, now),
            Inbox::Capture { session, event } => {
                if session == self.capture {
                    self.feed(event, now);
                }
            }
            Inbox::JobAudio {
                job,
                samples,
                spool,
            } => self.job_audio(job, samples, spool, now),
            // A job merged into a longer dictation: whatever whisper made of it is not used. Its
            // audio may still wait for the merge.
            Inbox::Transcribed { job, .. } if self.state.superseded(job).is_none() => {}
            Inbox::Transcribed { job, text: None } => {
                // The reason kind only: a model failure, else whisper itself.
                tracing::info!(?job, model_failure = ?self.model_failure, "transcription failed");
                self.no_text(job);
                self.feed(Event::TranscriptionFailed(job), now);
            }
            Inbox::Transcribed {
                job,
                text: Some(text),
            } => {
                self.clear_failed(|n, _| n == Notice::TranscriptionFailed);
                // Counts only: dictated text never goes into the log (AGENTS.md rule 5).
                let words = text.split_whitespace().count();
                let audio_ms = self.spoken.get(&job).map_or(0, |m| m.0);
                tracing::info!(?job, words, audio_ms, "transcription done");
                let empty = text.trim().is_empty();
                if empty {
                    self.no_text(job);
                } else {
                    self.text.insert(job, text);
                    // With the hold gap a longer dictation may still carry this one on: the
                    // entry is filed when the text is pasted.
                    if !self.hold_gap_on() {
                        self.file_history(job);
                    }
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
            Inbox::SoundWarnings(lines) => self.sound_warnings = lines,
            Inbox::HistoryWarning(line) => self.history_warning = line,
            Inbox::Backend(device) => self.backend = device,
            Inbox::Progress { job, percent } => self.progress_report(job, percent),
            Inbox::ModelDownloaded(id) => self.model_downloaded(&id, now),
            Inbox::AssetsChanged => send(&self.workers.whisper, WhisperCmd::ReloadAssets),
            Inbox::Ipc(ask) => self.ipc(ask),
            Inbox::CopyLastText => {
                if let Some(text) = &self.last_text {
                    send(
                        &self.workers.paste,
                        PasteCmd::CopyOnly {
                            text: text.clone(),
                            in_history: self.active.paste.keep_in_clipboard_history,
                        },
                    );
                }
            }
            Inbox::Shutdown => {} // `run` ends the loop before it gets here
        }
    }

    /// A hook, worker or session event: the coordinator's own part, then the machine's.
    fn event(&mut self, event: Event, now: Millis) {
        if let Event::PasteDone { job, outcome } = event {
            // Logged once, with its reason, by the paste worker.
            self.count(job, outcome);
            self.keep_last_text(job);
            // A failed paste raises its notice again in `feed`.
            self.failed.clear();
        }
        if let Event::JobAudioEmpty { job, why } = event {
            // A broken capture (kept as failed) or a quiet word under the gate kept what it
            // streamed: file it now, not at the next config pass. Also when the earlier parts
            // of the dictation go on without it.
            if matches!(why, EmptyAudio::Failed | EmptyAudio::NothingHeard) {
                send(
                    &self.workers.history,
                    HistoryCmd::Config(self.latest_history()),
                );
            }
            // Nothing after the trim, but the earlier parts of the dictation are there.
            if self.merges.contains_key(&job) {
                let merged = self.take_merged(job, Vec::new());
                if !merged.is_empty() {
                    return self.job_audio(job, merged, None, now);
                }
            }
        }
        match event {
            Event::ModelFailed(why) => self.model_failure = Some(why),
            Event::ModelLoaded => self.model_failure = None,
            // What the window drew may be stale after a sleep or a lock.
            Event::Resume | Event::Unlock => super::ipc::announce(Push::Woke),
            _ => {}
        }
        if event == Event::ModelLoaded {
            self.clear_failed(|n, _| {
                matches!(n, Notice::NoModel | Notice::ModelBad | Notice::OutOfMemory)
            });
        }
        self.feed(event, now);
    }

    fn hold_gap_on(&self) -> bool {
        self.active.hotkey.hold_gap_ms > 0
    }

    /// The text of `job` is final: its recording and text go to the history worker.
    fn file_history(&mut self, job: JobId) {
        let cfg = self.latest_history();
        let spool = self.spools.remove(&job);
        let samples = self.history_audio.remove(&job);
        let text = self.text.get(&job).cloned();
        match (samples, text) {
            (Some(samples), Some(text)) if self.history_wanted.remove(&job) => {
                let carried = self.carried_spools.remove(&job).unwrap_or_default();
                send(
                    &self.workers.history,
                    HistoryCmd::Save {
                        samples,
                        text,
                        cfg,
                        spool,
                        carried,
                    },
                );
            }
            // Saving is off: nothing to write, but the limits still act (SPEC History).
            _ => {
                self.give_up_carried(job);
                send(&self.workers.history, HistoryCmd::Config(cfg));
            }
        }
    }

    /// `samples` is the audio of `job`; when a press inside the hold gap carried earlier jobs
    /// on, their audio goes in front. Their results are dropped; their recordings on disk go with
    /// the merged save (`carried_spools`).
    fn take_merged(&mut self, job: JobId, samples: Vec<f32>) -> Vec<f32> {
        let Some(from) = self.merges.remove(&job) else {
            return samples;
        };
        tracing::info!(?job, parts = from.len(), "recording continued");
        let mut all = Vec::new();
        for j in from {
            // Not yet in whisper, or the copy `history_audio` keeps.
            let part = self
                .audio
                .remove(&j)
                .or_else(|| self.history_audio.remove(&j));
            all.extend(part.unwrap_or_default());
            self.text.remove(&j);
            self.spoken.remove(&j);
            self.history_wanted.remove(&j);
            // Its own carried parts too. They stay waiting in the spool registry, so a crash or
            // a quit before the merged save keeps them as failed; the history worker deletes
            // them once the merged audio is written.
            let carried = self.carried_spools.remove(&j).unwrap_or_default();
            let carried = carried.into_iter().chain(self.spools.remove(&j));
            self.carried_spools.entry(job).or_default().extend(carried);
        }
        all.extend(samples);
        all
    }

    /// No merged audio of `job` will be written: the recordings of its earlier parts are kept as
    /// failed at the next history command. Returns whether one was waiting. Saving turned off
    /// since: nothing may stay, so they go with a save the worker discards (deleted off this
    /// thread, before its pass that files orphans).
    fn give_up_carried(&mut self, job: JobId) -> bool {
        let carried = self.carried_spools.remove(&job).unwrap_or_default();
        let cfg = self.latest_history();
        if !cfg.enabled {
            if !carried.is_empty() {
                let (samples, text, spool) = (Vec::new(), String::new(), None);
                let save = HistoryCmd::Save {
                    samples,
                    text,
                    cfg,
                    spool,
                    carried,
                };
                send(&self.workers.history, save);
            }
            return false;
        }
        let mut any = false;
        for spool in carried {
            any |= super::history::no_transcript(&spool);
        }
        any
    }

    fn job_audio(&mut self, job: JobId, samples: Vec<f32>, spool: Option<PathBuf>, now: Millis) {
        let samples = self.take_merged(job, samples);
        if let Some(spool) = spool {
            self.spools.insert(job, spool);
        }
        // Sound from a recording started after the failure; the one that lost its
        // device delivers its own tail first.
        let capture = self.capture;
        self.clear_failed(|n, session| {
            matches!(n, Notice::DeviceLost | Notice::CaptureFailed) && session < capture
        });
        self.spoken.insert(job, stats::audio_length(samples.len()));
        self.audio.insert(job, samples);
        self.feed(Event::JobAudioReady(job), now);
    }

    fn ipc(&mut self, ask: IpcAsk) {
        match ask {
            IpcAsk::Status(reply) => {
                // The window gets this one with its Attention: no second push of the same.
                let report = self.status();
                self.pushed = Some(report.clone());
                // The asking connection may be gone already; nothing to do then.
                let _ = reply.send(report);
            }
            IpcAsk::PreviewOverlay(position) => {
                self.ui(UiCmd::PreviewOverlay(position));
            }
            IpcAsk::PreviewSound {
                choice,
                volume_percent,
            } => send(
                &self.workers.sound,
                SoundCmd::Preview {
                    choice,
                    volume_percent,
                },
            ),
            IpcAsk::Test(job) => {
                if self.state.phase() == Phase::Idle {
                    send(&self.workers.whisper, WhisperCmd::Test(job));
                } else {
                    job.reply.send(Push::TestResult {
                        id: job.id,
                        outcome: TestOutcome::Failed {
                            why: TestFailure::Busy,
                        },
                    });
                }
            }
            IpcAsk::TestDone => send(&self.workers.whisper, WhisperCmd::TestDone),
            IpcAsk::DeleteModel { id, reply } => {
                if self.state.phase() == Phase::Idle {
                    send(&self.workers.whisper, WhisperCmd::DeleteModel { id, reply });
                } else {
                    reply.send(Push::ModelDeleted {
                        id,
                        result: DeleteResult::InUse,
                    });
                }
            }
        }
    }

    /// Whisper gave no text for `job`. While a newer press carries it on its audio stays for the
    /// merge; if that press fails, the machine gives the job back and drops it (`DropJob`).
    fn no_text(&mut self, job: JobId) {
        if self.state.superseded(job) != Some(true) {
            self.spoken.remove(&job);
            self.no_transcript(job);
        }
    }

    /// No text will come for `job`: its recording, streamed to the history folder, is kept as
    /// failed. The history worker files it at once.
    fn no_transcript(&mut self, job: JobId) {
        self.history_audio.remove(&job);
        self.history_wanted.remove(&job);
        let own = self
            .spools
            .remove(&job)
            .is_some_and(|spool| super::history::no_transcript(&spool));
        // A merged job: its first parts' recordings are kept too, each as its own failed entry.
        let carried = self.give_up_carried(job);
        if own || carried {
            send(
                &self.workers.history,
                HistoryCmd::Config(self.latest_history()),
            );
        }
    }

    /// Model `id` was downloaded. When it is the configured one and its load failed (deleted, a
    /// bad file), the machine tries it again instead of refusing every dictation until a config
    /// change, and the failure's tray badge goes at once.
    fn model_downloaded(&mut self, id: &str, now: Millis) {
        let config = self.pending.as_ref().map_or(&self.active, |(c, _)| c);
        if config.model.id == id && self.model_failure.is_some() {
            let machine = MachineConfig::from(config);
            self.model_failure = None;
            self.clear_failed(|n, _| {
                matches!(n, Notice::NoModel | Notice::ModelBad | Notice::OutOfMemory)
            });
            // The same config again: the machine leaves Failed when it applies it (at Idle).
            self.feed(Event::ConfigChanged(machine), now);
        }
    }

    /// A real fraction from whisper (a transcription) or the paste worker (typing; 100 is the
    /// text landing). The estimate drives the bar; this only corrects it.
    fn progress_report(&mut self, job: JobId, percent: u8) {
        let bar = &self.workers.progress;
        match (self.state.phase(), self.bar) {
            (Phase::Transcribing, Bar::Transcribing(on)) if on == job => {
                bar.real(OverlayState::Transcribing, f32::from(percent) / 100.0);
            }
            (Phase::Pasting, Bar::Typing(on)) if on == job => {
                if percent >= 100 {
                    bar.finish(OverlayState::Pasting);
                    self.bar = Bar::None;
                } else {
                    bar.real(OverlayState::Pasting, f32::from(percent) / 100.0);
                }
            }
            // The first typing report starts the typing bar; a clipboard paste only reports the
            // landing (100) and has no bar (DS-26.4).
            (Phase::Pasting, Bar::None) if percent < 100 => {
                let units = self.text.get(&job).map_or(0, |t| t.encode_utf16().count());
                let safe = self.active.paste.type_speed == TypeSpeed::Safe;
                // Its own bar: a Transcribing pill still held keeps running its bar to the end.
                bar.start(
                    OverlayState::Pasting,
                    Some(overlay::typing_est(units, safe)),
                );
                bar.real(OverlayState::Pasting, f32::from(percent) / 100.0);
                self.bar = Bar::Typing(job);
            }
            _ => {}
        }
    }

    /// The bar follows the overlay state the machine asks for: a transcription starts it once
    /// the model is loaded (before that the pill sweeps), the text going to the field finishes
    /// it, and any other state ends it.
    fn overlay_progress(&mut self, state: OverlayState) {
        self.pill = state;
        let bar = &self.workers.progress;
        match (state, self.bar) {
            (OverlayState::Transcribing, Bar::None) => {
                if let (Some(job), Some(run)) = (self.transcribing, self.run.as_mut())
                    && run.job == job
                {
                    let key = (self.active.model.id.clone(), self.backend.clone());
                    let scale = self.speed.get(&key).copied().unwrap_or(1.0);
                    bar.start(
                        OverlayState::Transcribing,
                        Some(overlay::transcribe_est(run.audio_ms, scale)),
                    );
                    run.started = Some(self.clock);
                    self.bar = Bar::Transcribing(job);
                }
            }
            (OverlayState::Pasting, Bar::Transcribing(_)) => {
                if let Some(Run {
                    audio_ms,
                    started: Some(at),
                    queued: false,
                    ..
                }) = self.run.take()
                {
                    let key = (self.active.model.id.clone(), self.backend.clone());
                    let scale = self.speed.get(&key).copied().unwrap_or(1.0);
                    let took = (self.clock - at).0;
                    self.speed
                        .insert(key, overlay::learn(scale, audio_ms, took));
                }
                bar.finish(OverlayState::Transcribing);
                self.bar = Bar::None;
            }
            (OverlayState::Transcribing | OverlayState::Pasting, _) => {}
            // A bar still finishing runs out for the pill held to show it (ui.rs).
            _ => {
                bar.clear();
                self.bar = Bar::None;
            }
        }
    }

    /// The newest `[history]`: turned off during a job counts, the worker then writes nothing.
    fn latest_history(&self) -> History {
        let config = self.pending.as_ref().map_or(&self.active, |(c, _)| c);
        config.history.clone()
    }

    /// Machine steps until no effect answers with an event. Mirror the phase first (the hook
    /// reads it for Esc), then execute. An answer waits until every effect of its step ran, so a
    /// later effect of that step (an overlay) never overrides what the answer's step decided.
    fn feed(&mut self, event: Event, now: Millis) {
        self.clock = now;
        let mut queue = VecDeque::from([event]);
        while let Some(event) = queue.pop_front() {
            let effects = machine::step(&mut self.state, event, now);
            for note in self.state.take_notes() {
                match note {
                    Note::Cancelled { why, speech } => {
                        tracing::info!(%why, speech, "recording cancelled");
                    }
                    Note::RefusedQueueFull => {
                        tracing::info!(why = %"queue_full", "recording refused");
                    }
                }
            }
            let phase = self.state.phase();
            self.phase.store(phase);
            if phase != Phase::Transcribing {
                self.transcribing = None;
            }
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
                tracing::info!(session, "recording started");
                send(&self.workers.audio, AudioCmd::Start { session });
            }
            Effect::StopCapture(job) => {
                tracing::info!(?job, "recording stopped");
                send(&self.workers.audio, AudioCmd::Stop(job));
            }
            Effect::DiscardCapture => {
                tracing::info!("recording discarded");
                send(&self.workers.audio, AudioCmd::Discard);
            }
            // Nothing to do yet: the job may still be given back; `Merge` names what ends. Until
            // then `no_text` keeps its audio.
            Effect::Supersede(_) => {}
            Effect::Merge { ref from, into } => {
                self.merges.insert(into, from.clone());
            }
            Effect::DropJob(job) => {
                self.audio.remove(&job);
                self.text.remove(&job);
                self.spoken.remove(&job);
                self.no_transcript(job);
            }
            Effect::KeepCancelled(job) => {
                self.spoken.remove(&job);
                if let Some(samples) = self.audio.remove(&job) {
                    // Interim route until t10's history API has a cancelled entry: the audio is
                    // saved as a dictation with no text, so "kept" is true while history is on.
                    let cfg = self.latest_history();
                    let text = String::new();
                    let spool = self.spools.remove(&job);
                    let carried = self.carried_spools.remove(&job).unwrap_or_default();
                    send(
                        &self.workers.history,
                        HistoryCmd::Save {
                            samples,
                            text,
                            cfg,
                            spool,
                            carried,
                        },
                    );
                } else {
                    self.give_up_carried(job);
                }
            }
            Effect::Preload => send(&self.workers.whisper, WhisperCmd::Preload),
            Effect::Unload => send(&self.workers.whisper, WhisperCmd::Unload),
            Effect::Transcribe(job) => return self.transcribe(job),
            Effect::Paste(job) => {
                if self.hold_gap_on() {
                    self.file_history(job);
                }
                return self.paste(job);
            }
            Effect::PollChord => {
                return Some(Event::ChordPolled {
                    held: self.hook.chord_held(),
                });
            }
            Effect::ReinstallHook => self.hook.reinstall(),
            Effect::ApplyConfig => {
                if let Some((config, warnings)) = self.pending.take() {
                    if config.model.id != self.active.model.id {
                        self.model_failure = None; // another model: not tried yet
                    }
                    self.active = config;
                    self.warnings = warnings;
                    self.send_config();
                }
            }
            Effect::Play(sound) => send(&self.workers.sound, SoundCmd::Play(sound)),
            Effect::Overlay(state) => {
                if let OverlayState::Recording {
                    locked: true,
                    ending_soon: false,
                } = state
                {
                    tracing::info!("recording locked");
                }
                self.overlay_progress(state);
                self.ui(UiCmd::Overlay(state));
            }
            Effect::Notify(notice) => {
                if tray::is_error(notice) {
                    self.failed.retain(|&(n, _)| n != notice);
                    self.failed.push((notice, self.capture));
                }
                self.ui(UiCmd::Notify(notice));
            }
        }
        None
    }

    /// Sends the audio of `job` to whisper; the event when the job fails here.
    fn transcribe(&mut self, job: JobId) -> Option<Event> {
        let Some(samples) = self.audio.remove(&job) else {
            // Cannot happen (the machine waits for JobAudioReady); fail the job, not the machine.
            self.spoken.remove(&job);
            return Some(Event::TranscriptionFailed(job));
        };
        self.transcribing = Some(job);
        let queued = matches!(self.bar, Bar::Transcribing(_));
        self.run = Some(Run {
            job,
            audio_ms: samples.len() as u64 * 1000 / u64::from(oxi_core::audio::SAMPLE_RATE),
            started: None,
            queued,
        });
        // The pill said Transcribing while the audio was still coming, or showed an earlier
        // job still in whisper: this job's bar starts now (DS-26.5: a new job is a new bar).
        if self.pill == OverlayState::Transcribing {
            if queued {
                self.bar = Bar::None;
            }
            self.overlay_progress(OverlayState::Transcribing);
        }
        // The same `[history]` the save is sent with; a streamed recording needs its save
        // whatever the setting is now. With the hold gap the copy also serves a longer dictation
        // that carries this job on. Carried parts count too: with saving off the worker's save
        // deletes them, instead of the pass that files orphans keeping them as failed.
        let wanted = self.latest_history().enabled
            || self.spools.contains_key(&job)
            || self.carried_spools.get(&job).is_some_and(|c| !c.is_empty());
        if wanted {
            self.history_wanted.insert(job);
        }
        if wanted || self.hold_gap_on() {
            self.history_audio.insert(job, samples.clone());
        }
        send(
            &self.workers.whisper,
            WhisperCmd::Transcribe { job, samples },
        );
        None
    }

    /// Hands the text of `job` to the paste worker; the event when the paste is answered here.
    fn paste(&self, job: JobId) -> Option<Event> {
        // Cannot happen (empty text is never pasted); fail the job, not the machine.
        let Some(text) = self.text.get(&job) else {
            let outcome = PasteOutcome::Failed;
            return Some(Event::PasteDone { job, outcome });
        };
        let text = text.clone();
        if self.quitting {
            // At quit the focus is the tray or a closing window: the text waits on the
            // clipboard, and the notice says so ("press Ctrl+V").
            let in_history = self.active.paste.keep_in_clipboard_history;
            send(&self.workers.paste, PasteCmd::CopyOnly { text, in_history });
            let outcome = PasteOutcome::ClipboardOnly(ClipboardOnlyWhy::ModifiersHeld);
            return Some(Event::PasteDone { job, outcome });
        }
        let cfg = self.active.paste.clone();
        send(&self.workers.paste, PasteCmd::Paste { job, text, cfg });
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
        send(&self.workers.sound, SoundCmd::Config(cfg.sounds.clone()));
        send(
            &self.workers.history,
            HistoryCmd::Config(cfg.history.clone()),
        );
        self.ui(UiCmd::Config {
            overlay: cfg.overlay.clone(),
            meter_sensitivity_db: cfg.audio.meter_sensitivity_db,
            accent: cfg.ui.accent,
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
            model_failure: self.model_failure,
            warnings: self.warnings(),
            sounds: self.sound_warnings.clone(),
        }
    }

    /// Load corrections of the active config, the engine warning, the sound and history warnings,
    /// then the last parse error.
    fn warnings(&self) -> Vec<String> {
        self.warnings
            .iter()
            .cloned()
            .chain(self.engine_warning.as_ref().map(strings::engine_warning))
            .chain(self.sound_warnings.iter().map(|w| w.line.clone()))
            .chain(self.history_warning.clone())
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

    /// Drops the error notices whose cause `gone` reports (notice, session it was raised in).
    fn clear_failed(&mut self, gone: impl Fn(Notice, u64) -> bool) {
        self.failed.retain(|&(n, session)| !gone(n, session));
    }

    /// The error badge while a failure's cause remains or the config file is invalid; tells
    /// the tray when it changed.
    fn sync_badge(&mut self) {
        let badge = if !self.failed.is_empty() || self.config_error.is_some() {
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
    use std::fs;
    use std::path::Path;
    use std::rc::Rc;
    use std::sync::mpsc::channel;

    use oxi_core::config::{ConfigError, Hotkey};
    use oxi_core::machine::{EmptyAudio, ModelFailure, Notice, OverlayState, Sound};
    use oxi_core::tuning::SettingsSet;
    use oxi_core::types::Language;

    use crate::daemon::cmd::TestJob;
    use oxi_platform::os::overlay::Progress;

    use super::*;
    use crate::log::scratch::scratch_dir;

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
        sound: Receiver<SoundCmd>,
        history: Receiver<HistoryCmd>,
        ui: Receiver<UiCmd>,
        /// What the overlay would draw.
        progress: Progress,
    }

    fn drain<T>(rx: &Receiver<T>) -> Vec<T> {
        rx.try_iter().collect()
    }

    impl Rig {
        /// A coordinator with the default config; the start-up config commands are drained.
        fn new() -> Self {
            let mut config = Config::default();
            config.hotkey.esc_cancels = true; // the Esc tests; off is the default
            Self::with(config)
        }

        fn with(config: Config) -> Self {
            let (audio_tx, audio) = channel();
            let (whisper_tx, whisper) = channel();
            let (paste_tx, paste) = channel();
            let (sound_tx, sound) = channel();
            let (history_tx, history) = channel();
            let (ui_tx, ui) = channel();
            let progress = Progress::default();
            let workers = Workers {
                audio: audio_tx,
                whisper: whisper_tx,
                paste: paste_tx,
                sound: sound_tx,
                history: history_tx,
                ui: ui_tx,
                wake_ui: Box::new(|| {}),
                progress: progress.clone(),
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
                sound,
                history,
                ui,
                progress,
            };
            rig.drain_all();
            rig
        }

        fn drain_all(&self) {
            drain(&self.audio);
            drain(&self.whisper);
            drain(&self.paste);
            drain(&self.sound);
            drain(&self.history);
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
                    spool: None,
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
    fn the_last_model_load_failure_shows_in_status_until_a_load_works() {
        let mut rig = Rig::new();
        assert_eq!(rig.c.status().model_failure, None);
        for why in [ModelFailure::Bad, ModelFailure::OutOfMemory] {
            rig.event(Event::ModelFailed(why), 0);
            assert_eq!(rig.c.status().model_failure, Some(why));
            rig.event(Event::ModelLoaded, 10);
            assert_eq!(rig.c.status().model_failure, None);
        }
    }

    #[test]
    fn a_resume_or_an_unlock_tells_the_window_to_redraw() {
        let bus = crate::daemon::ipc::test_bus::OnBus::connected();
        let mut rig = Rig::new();
        let woke = || bus.next(500, |p| matches!(p, Push::Woke).then_some(()));
        rig.event(Event::Tick, 0);
        assert!(woke().is_none(), "nothing woke yet");
        for (event, at) in [(Event::Resume, 10), (Event::Unlock, 20)] {
            let name = format!("{event:?}");
            rig.event(event, at);
            assert!(woke().is_some(), "no Woke after {name}");
        }
    }

    #[test]
    fn the_overlay_bar_follows_the_phases_and_never_waits_for_whisper() {
        let mut rig = Rig::new();
        rig.event(Event::ModelLoaded, 0);
        let job = rig.record();
        assert!(!rig.progress.active());
        rig.c.handle(
            Inbox::JobAudio {
                job,
                samples: vec![0.5; 4],
                spool: None,
            },
            Millis(2100),
        );
        assert_eq!(rig.c.state.phase(), Phase::Transcribing);
        assert!(rig.progress.active(), "the estimate starts the bar at once");
        assert_eq!(rig.c.bar, Bar::Transcribing(job));
        let other = JobId(job.0 + 1);
        rig.c.handle(
            Inbox::Progress {
                job: other,
                percent: 70,
            },
            Millis(2150),
        );
        assert_eq!(
            rig.c.bar,
            Bar::Transcribing(job),
            "another job's report is late"
        );
        rig.c
            .handle(Inbox::Progress { job, percent: 40 }, Millis(2200));
        rig.c.handle(
            Inbox::Transcribed {
                job,
                text: Some("x".into()),
            },
            Millis(2500),
        );
        assert_eq!(rig.c.state.phase(), Phase::Pasting);
        assert_eq!(rig.c.bar, Bar::None, "a new phase is a new bar");
        assert!(
            rig.progress.finishing_left().is_some(),
            "the transcription bar runs to its end"
        );
        let scale = rig.c.speed.values().next().copied().unwrap_or(1.0);
        assert!(scale > 0.0, "the run was timed");
        // A clipboard paste only reports that the text landed: no bar, and the Pasting pill
        // shows none (review c04 #4: not the finished transcription's 100).
        rig.c
            .handle(Inbox::Progress { job, percent: 100 }, Millis(2520));
        assert_eq!(rig.c.bar, Bar::None);
        assert_eq!(rig.progress.get(OverlayState::Pasting), None);
        // Typing: the first report starts its bar, the landing finishes it. The held
        // Transcribing pill keeps its own bar (review c04 #3: no 100 -> 3 jump).
        rig.c
            .handle(Inbox::Progress { job, percent: 40 }, Millis(2540));
        assert_eq!(rig.c.bar, Bar::Typing(job));
        assert!(rig.progress.get(OverlayState::Pasting).is_some());
        assert!(
            rig.progress.get(OverlayState::Transcribing).is_some(),
            "the transcription bar is still finishing"
        );
        rig.c
            .handle(Inbox::Progress { job, percent: 100 }, Millis(2560));
        assert_eq!(rig.c.bar, Bar::None);
        assert!(rig.progress.finishing_left().is_some());
        rig.event(
            Event::PasteDone {
                job,
                outcome: PasteOutcome::Typed,
            },
            2600,
        );
        assert!(!rig.progress.active(), "the next pill starts clean");
        // Review c04 #5: hiding does not cut a finish the held pill is showing.
        assert!(rig.progress.finishing_left().is_some());
        // A late report after the job ended is not kept.
        rig.c
            .handle(Inbox::Progress { job, percent: 90 }, Millis(2700));
        assert_eq!(rig.c.bar, Bar::None);
    }

    /// Review c04 #6: a job sent to whisper while an earlier one is still in it gets its own
    /// bar from 0 (its reports are not dropped), and its time, which includes the wait, is not
    /// learned as the speed.
    #[test]
    fn a_job_queued_behind_another_gets_its_own_bar_and_teaches_no_speed() {
        let mut rig = Rig::new();
        rig.event(Event::ModelLoaded, 0);
        let job = rig.record();
        rig.c.handle(
            Inbox::JobAudio {
                job,
                samples: vec![0.5; 4],
                spool: None,
            },
            Millis(2100),
        );
        assert_eq!(rig.c.bar, Bar::Transcribing(job));
        let next = JobId(job.0 + 1);
        rig.c.audio.insert(next, vec![0.5; 4]);
        assert!(rig.c.transcribe(next).is_none());
        assert_eq!(rig.c.bar, Bar::Transcribing(next));
        let run = rig.c.run.expect("timed");
        assert!(run.job == next && run.started.is_some() && run.queued);
        rig.c.overlay_progress(OverlayState::Pasting);
        assert!(rig.c.speed.is_empty(), "a queued run teaches nothing");
    }

    #[test]
    fn the_window_is_told_when_its_status_changes_and_not_for_the_phase_alone() {
        // A bus another test set would make `handle` take the changed status before we do.
        let _no_bus = crate::daemon::lock(&crate::daemon::ipc::test_bus::BUS_TEST);
        let mut rig = Rig::new();
        assert!(
            rig.c.changed_status().is_some(),
            "the first one after it connects"
        );
        assert!(rig.c.changed_status().is_none());
        rig.record();
        assert_ne!(rig.c.state.phase(), Phase::Idle);
        let pushed = rig.c.changed_status();
        assert!(pushed.is_none(), "a phase alone is no push: {pushed:?}");
        rig.c.handle(Inbox::Backend("Vulkan0".into()), Millis(3000));
        let report = rig.c.changed_status().expect("a new backend is");
        assert_eq!(report.backend, "Vulkan0");
        assert!(rig.c.changed_status().is_none());
        // Review c04 #7: what the window asked for itself is not pushed a second time.
        rig.c.handle(Inbox::Backend("Vulkan1".into()), Millis(3100));
        let (tx, rx) = channel();
        rig.c.handle(Inbox::Ipc(IpcAsk::Status(tx)), Millis(3200));
        assert_eq!(rx.try_recv().unwrap().backend, "Vulkan1");
        assert!(rig.c.changed_status().is_none());
    }

    #[test]
    fn a_model_delete_waits_for_idle() {
        let mut rig = Rig::new();
        let reply = crate::daemon::ipc::Pushes::default();
        let (_, window) = reply.claim(1);
        let ask = || {
            Inbox::Ipc(IpcAsk::DeleteModel {
                id: "turbo".into(),
                reply: reply.clone(),
            })
        };
        rig.c.handle(ask(), Millis(0));
        assert!(
            drain(&rig.whisper)
                .iter()
                .any(|c| matches!(c, WhisperCmd::DeleteModel { id, .. } if id == "turbo"))
        );
        assert!(window.try_recv().is_err(), "the whisper worker answers");
        rig.event(Event::ChordDown, 100);
        rig.c.handle(ask(), Millis(200));
        assert!(
            !drain(&rig.whisper)
                .iter()
                .any(|c| matches!(c, WhisperCmd::DeleteModel { .. }))
        );
        assert_eq!(
            window.try_recv().unwrap(),
            Push::ModelDeleted {
                id: "turbo".into(),
                result: DeleteResult::InUse
            }
        );
    }

    #[test]
    fn a_recognition_test_runs_only_while_the_machine_is_idle() {
        let mut rig = Rig::new();
        let reply = crate::daemon::ipc::Pushes::default();
        let (_, pushes) = reply.claim(1);
        let job = |id| {
            Inbox::Ipc(IpcAsk::Test(TestJob {
                id,
                set: SettingsSet::defaults(),
                language: Language::RU,
                samples: vec![0.1],
                reply: reply.clone(),
            }))
        };
        rig.c.handle(job(1), Millis(0));
        assert!(
            drain(&rig.whisper)
                .iter()
                .any(|c| matches!(c, WhisperCmd::Test(j) if j.id == 1))
        );
        assert!(pushes.try_recv().is_err());

        rig.event(Event::ChordDown, 0);
        drain(&rig.whisper);
        rig.c.handle(job(2), Millis(1));
        assert!(drain(&rig.whisper).is_empty());
        assert_eq!(
            pushes.try_recv().ok(),
            Some(Push::TestResult {
                id: 2,
                outcome: TestOutcome::Failed {
                    why: TestFailure::Busy
                }
            })
        );
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
                spool: None,
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
            let [PasteCmd::CopyOnly { text, .. }] = copies.as_slice() else {
                panic!("expected one CopyOnly after {outcome:?}");
            };
            assert_eq!(text, "hello");
        }
    }

    #[test]
    fn copy_last_text_follows_the_history_setting_applied_after_the_dictation() {
        for dictated_with in [true, false] {
            let mut config = Config::default();
            config.paste.keep_in_clipboard_history = dictated_with;
            let mut rig = Rig::with(config.clone());
            let job = rig.transcribed("hello");
            rig.event(
                Event::PasteDone {
                    job,
                    outcome: PasteOutcome::Pasted,
                },
                3000,
            );
            rig.drain_all();

            config.paste.keep_in_clipboard_history = !dictated_with;
            rig.c.handle(
                Inbox::Config {
                    config: Box::new(config),
                    warnings: Vec::new(),
                },
                Millis(3050),
            );
            rig.drain_all();

            rig.c.handle(Inbox::CopyLastText, Millis(3100));
            let copies = drain(&rig.paste);
            let [PasteCmd::CopyOnly { in_history, .. }] = copies.as_slice() else {
                panic!("expected one CopyOnly");
            };
            assert_eq!(*in_history, !dictated_with);
        }
    }

    #[test]
    fn history_gets_the_audio_and_text_only_when_on_and_only_for_a_heard_dictation() {
        let saves = |rig: &Rig| {
            drain(&rig.history)
                .into_iter()
                .filter_map(|c| match c {
                    HistoryCmd::Save {
                        samples, text, cfg, ..
                    } => Some((samples.len(), text, cfg)),
                    HistoryCmd::Config(_) => None,
                })
                .collect::<Vec<_>>()
        };
        let dictate = |rig: &mut Rig, text: Option<&str>| {
            rig.event(Event::ModelLoaded, 0);
            let job = rig.record();
            let samples = vec![0.1; 16_000];
            rig.c.handle(
                Inbox::JobAudio {
                    job,
                    samples,
                    spool: None,
                },
                Millis(2100),
            );
            let text = text.map(str::to_owned);
            rig.c.handle(Inbox::Transcribed { job, text }, Millis(2500));
        };
        let mut config = Config::default();
        config.history.enabled = false;
        let mut off = Rig::with(config);
        dictate(&mut off, Some("hello"));
        assert!(saves(&off).is_empty(), "off writes nothing");
        assert!(off.c.history_audio.is_empty());

        let mut config = Config::default();
        config.history.enabled = true;
        let mut on = Rig::with(config);
        dictate(&mut on, Some("hello"));
        let [(len, text, cfg)] = saves(&on).try_into().unwrap();
        assert_eq!((len, text.as_str(), cfg.enabled), (16_000, "hello", true));
        assert!(on.c.history_audio.is_empty());

        for text in [None, Some("  ")] {
            let mut rig = Rig::with(on.c.active.clone());
            dictate(&mut rig, text);
            assert!(saves(&rig).is_empty(), "nothing heard is not saved");
            assert!(rig.c.history_audio.is_empty(), "no audio kept for it");
        }
    }

    #[test]
    fn a_dictation_with_history_off_still_sends_the_prune() {
        let mut config = Config::default();
        config.history.enabled = false;
        let mut rig = Rig::with(config);
        rig.event(Event::ModelLoaded, 0);
        let job = rig.record();
        let samples = vec![0.1; 16_000];
        rig.c.handle(
            Inbox::JobAudio {
                job,
                samples,
                spool: None,
            },
            Millis(2100),
        );
        let text = Some("hello".to_owned());
        rig.c.handle(Inbox::Transcribed { job, text }, Millis(2500));
        let sent = drain(&rig.history);
        let [HistoryCmd::Config(cfg)] = sent.as_slice() else {
            panic!("expected one prune, got {} commands", sent.len());
        };
        assert!(!cfg.enabled);
    }

    #[test]
    fn a_re_press_inside_the_hold_gap_makes_one_dictation_from_both_parts() {
        let mut config = Config::default();
        config.history.enabled = true;
        let mut rig = Rig::with(config);
        rig.event(Event::ModelLoaded, 0);
        rig.event(Event::ChordDown, 0);
        rig.event(Event::ChordUp, 1000);
        rig.event(Event::Tick, 1100); // the tail ends: job 0, its gap lasts until 1400
        let audio = |job, len| Inbox::JobAudio {
            job,
            samples: vec![0.1; len],
            spool: None,
        };
        let done = |job, text: &str| Inbox::Transcribed {
            job,
            text: Some(text.to_owned()),
        };
        rig.c.handle(audio(JobId(0), 8000), Millis(1150));
        rig.c.handle(done(JobId(0), "one"), Millis(1200));
        assert!(drain(&rig.paste).is_empty(), "the paste waits for the gap");
        rig.event(Event::ChordDown, 1300); // inside the gap: the dictation goes on
        rig.c.handle(Inbox::Event(Event::ChordUp), Millis(2000));
        rig.event(Event::Tick, 2100);
        rig.c.handle(audio(JobId(1), 4000), Millis(2150));
        rig.c.handle(done(JobId(1), "one two"), Millis(2200));
        rig.event(Event::Tick, 2600);

        let lens: Vec<_> = drain(&rig.whisper)
            .into_iter()
            .filter_map(|c| match c {
                WhisperCmd::Transcribe { job, samples } => Some((job, samples.len())),
                _ => None,
            })
            .collect();
        assert_eq!(lens, [(JobId(0), 8000), (JobId(1), 12_000)]);
        let pasted: Vec<_> = drain(&rig.paste)
            .into_iter()
            .filter_map(|c| match c {
                PasteCmd::Paste { job, text, .. } => Some((job, text)),
                PasteCmd::CopyOnly { .. } => None,
            })
            .collect();
        assert_eq!(pasted, [(JobId(1), "one two".to_owned())], "one paste");
        let saves: Vec<_> = drain(&rig.history)
            .into_iter()
            .filter_map(|c| match c {
                HistoryCmd::Save { samples, text, .. } => Some((samples.len(), text)),
                HistoryCmd::Config(_) => None,
            })
            .collect();
        assert_eq!(saves, [(12_000, "one two".to_owned())], "one history entry");
        assert!(rig.c.history_audio.is_empty() && rig.c.audio.is_empty());
        assert!(rig.c.merges.is_empty());
    }

    /// Whisper answers the first part with no text (a short word, a filtered hallucination) or
    /// fails while the user records the continuation: no notice, no error sound, and the first
    /// part's audio still goes in front of the merged dictation, with history off too.
    #[test]
    fn no_text_for_a_carried_part_keeps_its_audio_for_the_merge() {
        for (history, text) in [(false, None), (false, Some(String::new())), (true, None)] {
            let case = format!("history {history}, text {text:?}");
            let mut config = Config::default();
            config.history.enabled = history;
            let mut rig = Rig::with(config);
            rig.event(Event::ModelLoaded, 0);
            rig.event(Event::ChordDown, 0);
            rig.event(Event::ChordUp, 1000);
            rig.event(Event::Tick, 1100); // job 0, its gap lasts until 1400
            let audio = |job, len| Inbox::JobAudio {
                job,
                samples: vec![0.1; len],
                spool: None,
            };
            rig.c.handle(audio(JobId(0), 8000), Millis(1150)); // job 0 goes to whisper
            rig.event(Event::ChordDown, 1300); // carries job 0 on while whisper has it
            rig.drain_all();
            let job = JobId(0);
            rig.c.handle(Inbox::Transcribed { job, text }, Millis(1350));
            assert!(drain(&rig.sound).is_empty(), "{case}");
            let said = drain(&rig.ui)
                .into_iter()
                .any(|c| matches!(c, UiCmd::Notify(_)));
            assert!(!said, "{case}");
            rig.c.handle(Inbox::Event(Event::ChordUp), Millis(2000));
            rig.event(Event::Tick, 2100);
            rig.c.handle(audio(JobId(1), 4000), Millis(2150));
            let sent: Vec<_> = drain(&rig.whisper)
                .into_iter()
                .filter_map(|c| match c {
                    WhisperCmd::Transcribe { job, samples } => Some((job, samples.len())),
                    _ => None,
                })
                .collect();
            assert_eq!(sent, [(JobId(1), 12_000)], "{case}");
            let text = Some("one two".to_owned());
            rig.c.handle(
                Inbox::Transcribed {
                    job: JobId(1),
                    text,
                },
                Millis(2200),
            );
            rig.event(Event::Tick, 2600);
            let saves: Vec<_> = drain(&rig.history)
                .into_iter()
                .filter_map(|c| match c {
                    HistoryCmd::Save { samples, .. } => Some(samples.len()),
                    HistoryCmd::Config(_) => None,
                })
                .collect();
            let want: &[usize] = if history { &[12_000] } else { &[] };
            assert_eq!(saves, want, "{case}");
            assert!(rig.c.history_audio.is_empty() && rig.c.merges.is_empty());
        }
    }

    /// A lock or sleep during a continuation cancels the whole dictation: nothing pastes into
    /// the locking session, and with history on both parts are kept as one cancelled entry.
    #[test]
    fn a_lock_during_a_continuation_keeps_both_parts_as_one_cancel() {
        for event in [Event::Lock, Event::Suspend] {
            let mut config = Config::default();
            config.history.enabled = true;
            let mut rig = Rig::with(config);
            rig.event(Event::ModelLoaded, 0);
            rig.event(Event::ChordDown, 0);
            rig.event(Event::ChordUp, 1000);
            rig.event(Event::Tick, 1100);
            let audio = |job, len| Inbox::JobAudio {
                job,
                samples: vec![0.1; len],
                spool: None,
            };
            rig.c.handle(audio(JobId(0), 8000), Millis(1150));
            let text = Some("one".to_owned());
            let job = JobId(0);
            rig.c.handle(Inbox::Transcribed { job, text }, Millis(1200));
            rig.event(Event::ChordDown, 1300);
            rig.event(event.clone(), 1500);
            rig.c.handle(audio(JobId(1), 4000), Millis(1600));
            rig.event(Event::Tick, 5000);
            assert!(drain(&rig.paste).is_empty(), "{event:?}");
            let saves: Vec<_> = drain(&rig.history)
                .into_iter()
                .filter_map(|c| match c {
                    HistoryCmd::Save { samples, text, .. } => Some((samples.len(), text)),
                    HistoryCmd::Config(_) => None,
                })
                .collect();
            assert_eq!(saves, [(12_000, String::new())], "{event:?}");
            assert!(rig.c.history_audio.is_empty() && rig.c.text.is_empty());
            assert!(!rig.c.state.busy(), "{event:?}");
        }
    }

    /// A press inside the hold gap, then the mic or the model fails during the continuation:
    /// the first part is no part of the failure. Its text pastes and its recording is saved as
    /// it was, nothing of it is dropped.
    #[test]
    fn a_failed_continuation_gives_the_first_part_back_to_paste_and_save() {
        let capture_failed = |rig: &Rig| Inbox::Capture {
            session: rig.c.capture,
            event: Event::CaptureFailed,
        };
        let model_failed = |_: &Rig| Inbox::Event(Event::ModelFailed(ModelFailure::Bad));
        let failures: [&dyn Fn(&Rig) -> Inbox; 2] = [&capture_failed, &model_failed];
        for failure in failures {
            let mut config = Config::default();
            config.history.enabled = true;
            let mut rig = Rig::with(config);
            rig.event(Event::ModelLoaded, 0);
            rig.event(Event::ChordDown, 0);
            rig.event(Event::ChordUp, 1000);
            rig.event(Event::Tick, 1100); // job 0, its gap lasts until 1400
            let audio = Inbox::JobAudio {
                job: JobId(0),
                samples: vec![0.1; 8000],
                spool: None,
            };
            rig.c.handle(audio, Millis(1150));
            let text = Some("one".to_owned());
            rig.c.handle(
                Inbox::Transcribed {
                    job: JobId(0),
                    text,
                },
                Millis(1200),
            );
            rig.event(Event::ChordDown, 1300); // carries job 0 on
            let fail = failure(&rig);
            rig.c.handle(fail, Millis(1500));
            // The failed part keeps its own audio as job 1, alone.
            let audio = Inbox::JobAudio {
                job: JobId(1),
                samples: vec![0.1; 4000],
                spool: None,
            };
            rig.c.handle(audio, Millis(1600));

            let pasted: Vec<_> = drain(&rig.paste)
                .into_iter()
                .filter_map(|c| match c {
                    PasteCmd::Paste { job, text, .. } => Some((job, text)),
                    PasteCmd::CopyOnly { .. } => None,
                })
                .collect();
            assert_eq!(pasted, [(JobId(0), "one".to_owned())]);
            let saves: Vec<_> = drain(&rig.history)
                .into_iter()
                .filter_map(|c| match c {
                    HistoryCmd::Save { samples, text, .. } => Some((samples.len(), text)),
                    HistoryCmd::Config(_) => None,
                })
                .collect();
            assert_eq!(saves, [(8000, "one".to_owned())]);
            assert!(rig.c.merges.is_empty());
            let outcome = PasteOutcome::Pasted;
            rig.event(
                Event::PasteDone {
                    job: JobId(0),
                    outcome,
                },
                1700,
            );
            assert!(rig.c.audio.is_empty() && rig.c.history_audio.is_empty());
            assert!(rig.c.spoken.is_empty() && rig.c.text.is_empty());
        }
    }

    /// History off, the failure comes before the first part's audio: that audio still arrives,
    /// is transcribed and pasted, and nothing of it stays in memory.
    #[test]
    fn a_failure_before_the_first_parts_audio_leaks_nothing_and_loses_nothing() {
        let mut config = Config::default();
        config.history.enabled = false;
        let mut rig = Rig::with(config);
        rig.event(Event::ModelLoaded, 0);
        rig.event(Event::ChordDown, 0);
        rig.event(Event::ChordUp, 1000);
        rig.event(Event::Tick, 1100);
        rig.event(Event::ChordDown, 1300); // job 0 still waits for its audio
        let fail = Inbox::Capture {
            session: rig.c.capture,
            event: Event::CaptureFailed,
        };
        rig.c.handle(fail, Millis(1400));
        let audio = Inbox::JobAudio {
            job: JobId(0),
            samples: vec![0.1; 8000],
            spool: None,
        };
        rig.c.handle(audio, Millis(1500));
        let sent: Vec<_> = drain(&rig.whisper)
            .into_iter()
            .filter_map(|c| match c {
                WhisperCmd::Transcribe { job, samples } => Some((job, samples.len())),
                _ => None,
            })
            .collect();
        assert_eq!(sent, [(JobId(0), 8000)]);
        let text = Some("one".to_owned());
        rig.c.handle(
            Inbox::Transcribed {
                job: JobId(0),
                text,
            },
            Millis(1600),
        );
        let pasted = drain(&rig.paste)
            .into_iter()
            .any(|c| matches!(c, PasteCmd::Paste { job: JobId(0), .. }));
        assert!(pasted);
        let outcome = PasteOutcome::Pasted;
        rig.event(
            Event::PasteDone {
                job: JobId(0),
                outcome,
            },
            1700,
        );
        assert!(rig.c.audio.is_empty() && rig.c.spoken.is_empty());
        assert!(rig.c.history_audio.is_empty() && rig.c.text.is_empty());
        assert!(!rig.c.state.busy());
    }

    /// A quiet word under the gate with saving on: the recording it streamed is filed at once.
    #[test]
    fn nothing_heard_files_the_streamed_recording_now() {
        let mut config = Config::default();
        config.history.enabled = true;
        let mut rig = Rig::with(config);
        rig.event(Event::ModelLoaded, 0);
        let job = rig.record();
        drain(&rig.history);
        let why = EmptyAudio::NothingHeard;
        rig.event(Event::JobAudioEmpty { job, why }, 2100);
        let sent = drain(&rig.history);
        assert!(
            matches!(sent.as_slice(), [HistoryCmd::Config(cfg)] if cfg.enabled),
            "one config pass files the kept part"
        );
    }

    #[test]
    fn history_turned_off_during_a_job_is_off_at_its_save() {
        let mut config = Config::default();
        config.history.enabled = true;
        let mut rig = Rig::with(config.clone());
        rig.event(Event::ModelLoaded, 0);
        let job = rig.record();
        let samples = vec![0.1; 16_000];
        rig.c.handle(
            Inbox::JobAudio {
                job,
                samples,
                spool: None,
            },
            Millis(2100),
        );
        config.history.enabled = false;
        rig.c.handle(
            Inbox::Config {
                config: Box::new(config),
                warnings: Vec::new(),
            },
            Millis(2200),
        );
        let text = Some("hello".to_owned());
        rig.c.handle(Inbox::Transcribed { job, text }, Millis(2500));
        let saves: Vec<_> = drain(&rig.history)
            .into_iter()
            .filter_map(|c| match c {
                HistoryCmd::Save { cfg, .. } => Some(cfg.enabled),
                HistoryCmd::Config(_) => None,
            })
            .collect();
        assert_eq!(saves, [false]);
    }

    #[test]
    fn a_history_warning_shows_in_the_status_until_it_clears() {
        let mut rig = Rig::new();
        let line = "History not saved (StorageFull)";
        rig.c
            .handle(Inbox::HistoryWarning(Some(line.into())), Millis(10));
        assert_eq!(rig.c.warnings(), [line]);
        rig.c.handle(Inbox::HistoryWarning(None), Millis(20));
        assert_eq!(rig.c.warnings(), Vec::<String>::new());
    }

    #[test]
    fn a_finished_dictation_updates_the_stats_file_a_failed_one_does_not() {
        let dir = scratch_dir(&format!("oxi-coord-stats-{}", std::process::id()));
        let path = dir.join(stats::FILE);
        let today = || stats::Date::from_ymd(2026, 11, 7).unwrap();
        for outcome in [PasteOutcome::Failed, PasteOutcome::Pasted] {
            let mut rig = Rig::new();
            rig.c.stats = Some(Recorder::spawn(path.clone(), today).unwrap());
            rig.event(Event::ModelLoaded, 0);
            let job = rig.record();
            let samples = vec![0.1; 16_000]; // 1 s
            rig.c.handle(
                Inbox::JobAudio {
                    job,
                    samples,
                    spool: None,
                },
                Millis(2100),
            );
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
        let _ = fs::remove_dir_all(&dir);
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
                    spool: None,
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
    fn esc_after_speech_keeps_the_audio_in_the_history_and_never_transcribes() {
        let mut rig = Rig::new();
        rig.event(Event::ModelLoaded, 0);
        rig.event(Event::ChordDown, 0);
        // Capture session 1 heard speech; a stale session's speech is ignored.
        rig.c.handle(
            Inbox::Capture {
                session: 1,
                event: Event::SpeechHeard,
            },
            Millis(100),
        );
        rig.event(Event::Esc, 500);
        let job = drain(&rig.audio)
            .into_iter()
            .find_map(|c| match c {
                AudioCmd::Stop(job) => Some(job),
                _ => None,
            })
            .expect("capture stopped, not discarded");
        drain(&rig.history);
        let samples = vec![0.1; 8_000];
        rig.c.handle(
            Inbox::JobAudio {
                job,
                samples,
                spool: None,
            },
            Millis(600),
        );

        let saved = drain(&rig.history);
        assert!(
            matches!(saved.as_slice(), [HistoryCmd::Save { samples, text, .. }]
                if samples.len() == 8_000 && text.is_empty()),
            "{} history commands",
            saved.len()
        );
        assert!(
            !drain(&rig.whisper)
                .iter()
                .any(|c| matches!(c, WhisperCmd::Transcribe { .. }))
        );
        assert!(rig.c.audio.is_empty());
        assert_eq!(rig.phase.load(), Phase::Idle);
    }

    #[test]
    fn config_received_while_recording_applies_when_idle() {
        let mut rig = Rig::new();
        rig.event(Event::ChordDown, 0);
        let mut config = Config::default();
        config.audio.device = "Mic X".into();
        config.audio.meter_sensitivity_db = -6.0;
        config.ui.accent = oxi_core::config::Accent::Hex(oxi_core::config::Rgb(0x5B, 0x5B, 0xD6));
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
            accent: config.ui.accent,
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
    fn sounds_go_to_the_sound_worker_and_its_warnings_show_in_status() {
        let mut rig = Rig::new();
        rig.event(Event::ChordDown, 0);
        assert_eq!(drain(&rig.sound), [SoundCmd::Play(Sound::Start)]);
        rig.c.handle(
            Inbox::Ipc(IpcAsk::PreviewSound {
                choice: "pop".into(),
                volume_percent: 30,
            }),
            Millis(10),
        );
        assert_eq!(
            drain(&rig.sound),
            [SoundCmd::Preview {
                choice: "pop".into(),
                volume_percent: 30
            }]
        );
        let status = |rig: &mut Rig| {
            let (tx, rx) = channel();
            rig.c.handle(Inbox::Ipc(IpcAsk::Status(tx)), Millis(20));
            let report = rx.try_recv().expect("status answered");
            (report.warnings, report.sounds)
        };
        let warning = SoundWarning {
            kind: oxi_core::ipc::SoundWarningKind::Unused(Sound::Start),
            line: "sounds.start: cannot read the file (entity not found), using rise".into(),
        };
        rig.c
            .handle(Inbox::SoundWarnings(vec![warning.clone()]), Millis(20));
        let line = warning.line.clone();
        assert_eq!(status(&mut rig), (vec![line], vec![warning]));
        rig.c.handle(Inbox::SoundWarnings(Vec::new()), Millis(30));
        assert_eq!(status(&mut rig), (Vec::new(), Vec::new()));
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
    fn a_failure_badge_clears_when_its_cause_is_gone() {
        let badges = |rig: &Rig| -> Vec<TrayBadge> {
            drain(&rig.ui)
                .into_iter()
                .filter_map(|c| match c {
                    UiCmd::Badge(b) => Some(b),
                    _ => None,
                })
                .collect()
        };
        let audio = |rig: &mut Rig, job, at| {
            let samples = vec![0.5; 4];
            rig.c.handle(
                Inbox::JobAudio {
                    job,
                    samples,
                    spool: None,
                },
                Millis(at),
            );
        };
        let mut rig = Rig::new();

        // No model: the load that follows clears it.
        rig.event(Event::ModelFailed(ModelFailure::Missing), 0);
        assert_eq!(badges(&rig), [TrayBadge::Error], "no model");
        rig.event(Event::ModelLoaded, 100);
        assert_eq!(badges(&rig), [TrayBadge::None], "a model loaded");

        // Lost mic: its own tail does not clear it, sound from the next recording does.
        rig.event(Event::ChordDown, 200);
        drain(&rig.audio);
        let lost = Inbox::Capture {
            session: 1,
            event: Event::DeviceLost,
        };
        rig.c.handle(lost, Millis(1200));
        assert_eq!(badges(&rig), [TrayBadge::Error], "the mic is lost");
        rig.event(Event::ChordUp, 1300);
        let job = drain(&rig.audio)
            .into_iter()
            .find_map(|c| match c {
                AudioCmd::Stop(job) => Some(job),
                _ => None,
            })
            .expect("the lost recording stops");
        audio(&mut rig, job, 400);
        assert!(badges(&rig).is_empty(), "the tail of the lost recording");
        // Jobs run in order: answer each with silence so the next one can be asked.
        let silence = |rig: &mut Rig, job, at| {
            let text = Some(String::new());
            rig.c.handle(Inbox::Transcribed { job, text }, Millis(at));
        };
        silence(&mut rig, job, 450);
        let job = rig.record();
        audio(&mut rig, job, 2100);
        silence(&mut rig, job, 2150);
        assert_eq!(
            badges(&rig),
            [TrayBadge::None],
            "the next recording had sound"
        );

        // Failed transcription: the next one that works clears it, before any paste.
        let job = rig.record();
        audio(&mut rig, job, 2100);
        let failed = Inbox::Transcribed { job, text: None };
        rig.c.handle(failed, Millis(2200));
        assert_eq!(badges(&rig), [TrayBadge::Error], "transcription failed");
        let job = rig.record();
        audio(&mut rig, job, 2100);
        assert!(badges(&rig).is_empty(), "sound alone does not fix it");
        let text = Some("three".to_owned());
        rig.c.handle(Inbox::Transcribed { job, text }, Millis(2300));
        assert_eq!(badges(&rig), [TrayBadge::None], "a transcription worked");
    }

    /// History turned on while busy: the worker streams already (it gets the newest `[history]`)
    /// but the active config, applied at Idle, still says off. The transcribed job must be saved,
    /// not left waiting until quit and kept as failed.
    #[test]
    fn a_job_that_streamed_a_recording_is_saved_whatever_the_active_setting_says() {
        let mut config = Config::default();
        config.history.enabled = false;
        let mut rig = Rig::with(config.clone());
        rig.event(Event::ModelLoaded, 0);
        let job = rig.record();
        config.history.enabled = true;
        let (config, warnings) = (Box::new(config), Vec::new());
        rig.c
            .handle(Inbox::Config { config, warnings }, Millis(1500));
        let spool = PathBuf::from("h/2026-11-10/120000-000.wav.part");
        let (samples, text) = (vec![0.5; 4], Some("hello".to_owned()));
        let audio = Inbox::JobAudio {
            job,
            samples,
            spool: Some(spool.clone()),
        };
        rig.c.handle(audio, Millis(2100));
        rig.c.handle(Inbox::Transcribed { job, text }, Millis(2500));
        let saves: Vec<_> = drain(&rig.history)
            .into_iter()
            .filter_map(|cmd| match cmd {
                HistoryCmd::Save { spool, cfg, .. } => Some((spool, cfg.enabled)),
                HistoryCmd::Config(_) => None,
            })
            .collect();
        assert_eq!(saves, [(Some(spool), true)]);
        assert!(rig.c.spools.is_empty() && rig.c.history_audio.is_empty());
    }

    /// t13: deleting the configured model fails it as `Missing`; downloading it again must let
    /// dictation start without a config change.
    #[test]
    fn a_download_of_the_deleted_model_lets_dictation_start_again() {
        let mut config = Config::default();
        config.model.id = "t10b-downloaded-again".into(); // no other test downloads this id
        let mut rig = Rig::with(config);
        rig.event(Event::ModelLoaded, 0);
        rig.event(Event::ModelFailed(ModelFailure::Missing), 10);
        let starts = |rig: &Rig| {
            drain(&rig.audio)
                .iter()
                .filter(|c| matches!(c, AudioCmd::Start { .. }))
                .count()
        };
        rig.event(Event::ChordDown, 100);
        rig.event(Event::ChordUp, 150);
        assert_eq!(starts(&rig), 0, "refused while the file is gone");
        assert_eq!(rig.c.badge, TrayBadge::Error);
        rig.c
            .handle(Inbox::ModelDownloaded("another-model".into()), Millis(200));
        assert_eq!(rig.c.status().model_failure, Some(ModelFailure::Missing));
        rig.c.handle(
            Inbox::ModelDownloaded("t10b-downloaded-again".into()),
            Millis(300),
        );
        assert_eq!(rig.c.badge, TrayBadge::None, "the cause is gone");
        rig.event(Event::ChordDown, 5000);
        assert_eq!(starts(&rig), 1, "the downloaded model is tried again");
        assert_eq!(rig.c.status().model_failure, None);
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
        // Quit stops the recording; nothing was heard, so nothing is kept.
        assert_eq!(
            drain(&rig.audio),
            [AudioCmd::Start { session: 1 }, AudioCmd::Discard]
        );
    }

    /// What a quit left on the worker channels.
    struct Quit {
        phase: PhaseCell,
        audio: Vec<AudioCmd>,
        whisper: Vec<WhisperCmd>,
        paste: Vec<PasteCmd>,
        /// The text of each `HistoryCmd::Save`.
        saved: Vec<String>,
        took: Duration,
    }

    /// Quits `rig` with `msgs` already queued for the grace period.
    fn quit(rig: Rig, msgs: Vec<Inbox>, grace: u64) -> Quit {
        let (tx, rx) = channel();
        for msg in msgs {
            tx.send(msg).expect("open channel");
        }
        let start = std::time::Instant::now();
        let clock = || Millis(u64::try_from(start.elapsed().as_millis()).unwrap());
        rig.c.shutdown(&rx, &clock, Millis(grace));
        let saved = drain(&rig.history)
            .into_iter()
            .filter_map(|c| match c {
                HistoryCmd::Save { text, .. } => Some(text),
                HistoryCmd::Config(_) => None,
            })
            .collect();
        Quit {
            phase: rig.phase,
            audio: drain(&rig.audio),
            whisper: drain(&rig.whisper),
            paste: drain(&rig.paste),
            saved,
            took: start.elapsed(),
        }
    }

    // t15: a quit during transcription lost the text. It now waits for it: the text goes to
    // the history and the clipboard, never pasted into whatever has the focus at quit.
    #[test]
    fn quit_during_transcription_keeps_the_text_on_the_clipboard_and_in_the_history() {
        let mut config = Config::default();
        config.history.enabled = true;
        let mut rig = Rig::with(config);
        rig.event(Event::ModelLoaded, 0);
        let job = rig.record();
        rig.c.handle(
            Inbox::JobAudio {
                job,
                samples: vec![0.5; 4],
                spool: None,
            },
            Millis(2100),
        );
        rig.drain_all();
        let msgs = vec![
            Inbox::Event(Event::ChordDown), // a press while quitting starts nothing
            Inbox::Transcribed {
                job,
                text: Some("hello".into()),
            },
        ];
        let q = quit(rig, msgs, 60_000);
        assert!(
            q.took < Duration::from_secs(10),
            "waited for the grace: {:?}",
            q.took
        );
        let pastes: Vec<_> = q
            .paste
            .into_iter()
            .map(|c| match c {
                PasteCmd::CopyOnly { text, .. } => text,
                PasteCmd::Paste { .. } => panic!("pasted at quit"),
            })
            .collect();
        assert_eq!(pastes, ["hello"]);
        assert_eq!(q.saved, ["hello"]);
        assert!(!q.audio.iter().any(|c| matches!(c, AudioCmd::Start { .. })));
    }

    // t15: a transcription that does not end in the grace is cut: whisper's cancel check sees
    // Idle, and the quit does not wait for it.
    #[test]
    fn quit_cuts_a_transcription_that_outlives_the_grace() {
        let mut rig = Rig::new();
        rig.event(Event::ModelLoaded, 0);
        let job = rig.record();
        rig.c.handle(
            Inbox::JobAudio {
                job,
                samples: vec![0.5; 4],
                spool: None,
            },
            Millis(2100),
        );
        assert_eq!(rig.phase.load(), Phase::Transcribing);
        let q = quit(rig, Vec::new(), 50);
        assert!(q.took >= Duration::from_millis(50), "{:?}", q.took);
        assert_eq!(q.phase.load(), Phase::Idle, "whisper is told to stop");
        assert!(q.paste.is_empty());
    }

    // t15: a quit while recording after speech keeps the audio, as a session lock does.
    #[test]
    fn quit_while_recording_keeps_the_heard_audio() {
        let mut rig = Rig::new();
        rig.event(Event::ModelLoaded, 0);
        rig.event(Event::ChordDown, 0);
        rig.c.handle(
            Inbox::Capture {
                session: 1,
                event: Event::SpeechHeard,
            },
            Millis(100),
        );
        rig.drain_all();
        // The capture's answer to the quit's Stop, for the first job.
        let msgs = vec![Inbox::JobAudio {
            job: JobId(0),
            samples: vec![0.1; 800],
            spool: None,
        }];
        let q = quit(rig, msgs, 60_000);
        assert!(q.audio.contains(&AudioCmd::Stop(JobId(0))));
        assert_eq!(q.saved, [""], "kept with no text");
        assert!(
            !q.whisper
                .iter()
                .any(|c| matches!(c, WhisperCmd::Transcribe { .. }))
        );
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

    /// The log lines of what `run` does, without a global subscriber: the machine's reasons
    /// reach the log through the coordinator, so the text is what the test reads.
    fn logged(run: impl FnOnce()) -> String {
        use std::io::Write;
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sink = Sink::default();
        let writer = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        let bytes = sink.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    /// arch-plan 4.3: the log says why a recording was cancelled and whether its audio was kept,
    /// with the machine's own reason; no text.
    #[test]
    fn a_cancel_is_logged_with_the_machines_reason_and_speech() {
        let cases = [
            (Event::Esc, false, "why=esc speech=false"),
            (Event::Esc, true, "why=esc speech=true"),
            (Event::Lock, false, "why=lock speech=false"),
            (Event::Suspend, true, "why=suspend speech=true"),
        ];
        for (event, speech, want) in cases {
            let mut rig = Rig::new();
            rig.event(Event::ModelLoaded, 0);
            rig.event(Event::ChordDown, 0);
            if speech {
                rig.event(Event::SpeechHeard, 100);
            }
            let log = logged(|| rig.event(event.clone(), 200));
            let line = log.lines().find(|l| l.contains("recording cancelled"));
            assert!(
                line.is_some_and(|l| l.contains(want)),
                "{event:?} speech {speech}: {log}"
            );
        }
    }

    #[test]
    fn a_shortcut_and_another_key_are_logged_as_such() {
        let mut config = Config::default();
        config.hotkey.other_key = oxi_core::config::OtherKey::Auto;
        let mut rig = Rig::with(config);
        rig.event(Event::ModelLoaded, 0);
        rig.event(Event::ChordDown, 0);
        rig.event(Event::OtherKey, 50);
        let log = logged(|| rig.event(Event::Tick, 5000));
        assert!(log.contains("why=shortcut speech=false"), "{log}");

        let mut config = Config::default();
        config.hotkey.other_key = oxi_core::config::OtherKey::Always;
        let mut rig = Rig::with(config);
        rig.event(Event::ModelLoaded, 0);
        rig.event(Event::ChordDown, 0);
        let log = logged(|| rig.event(Event::OtherKey, 50));
        assert!(log.contains("why=other_key speech=false"), "{log}");
    }

    #[test]
    fn a_refused_key_down_is_logged_with_its_reason() {
        let mut rig = Rig::new();
        rig.event(Event::ModelLoaded, 0);
        for t in [0, 10_000] {
            rig.event(Event::ChordDown, t);
            rig.event(Event::ChordUp, t + 1000);
            rig.event(Event::Tick, t + 2000); // a job waiting for its audio
        }
        let log = logged(|| rig.event(Event::ChordDown, 30_000));
        let line = log.lines().find(|l| l.contains("recording refused"));
        assert!(line.is_some_and(|l| l.contains("why=queue_full")), "{log}");
        assert!(!log.contains("recording cancelled"), "{log}");
    }

    /// Makes `dir` the folder new recordings stream into; off again when dropped.
    struct SpoolDir;
    impl SpoolDir {
        fn on(dir: PathBuf) -> Self {
            oxi_platform::wav::set_spool_dir(Some(dir));
            Self
        }
    }
    impl Drop for SpoolDir {
        fn drop(&mut self) {
            oxi_platform::wav::set_spool_dir(None);
        }
    }

    /// Records a first part (job 0, streamed to `part`) and a press inside the gap that carries
    /// it on into job 1, whose audio has arrived; `config` has saving on.
    fn carry_on(rig: &mut Rig, part: &Path) {
        rig.event(Event::ModelLoaded, 0);
        rig.event(Event::ChordDown, 0);
        rig.event(Event::ChordUp, 1000);
        rig.event(Event::Tick, 1100);
        rig.c
            .handle(spooled_audio(JobId(0), Some(part.to_owned())), Millis(1150));
        rig.c
            .handle(transcribed(JobId(0), Some("one")), Millis(1200));
        rig.event(Event::ChordDown, 1300); // inside the gap: the dictation goes on
        rig.event(Event::ChordUp, 2000);
        rig.event(Event::Tick, 2100);
        rig.c.handle(spooled_audio(JobId(1), None), Millis(2150));
    }

    fn spooled_audio(job: JobId, spool: Option<PathBuf>) -> Inbox {
        Inbox::JobAudio {
            job,
            samples: vec![0.1; 4000],
            spool,
        }
    }

    fn transcribed(job: JobId, text: Option<&str>) -> Inbox {
        Inbox::Transcribed {
            job,
            text: text.map(str::to_owned),
        }
    }

    /// A finished first part streamed into a spool folder that is on for this test only.
    fn first_part(name: &str) -> (SpoolDir, PathBuf) {
        // Holds the history tests' lock: the spool folder is process-wide.
        let on = SpoolDir::on(super::super::history::tests::temp(name));
        let mut spool = oxi_platform::wav::WavSpool::begin(48_000).unwrap();
        spool.push(&vec![0.1; 480]);
        (on, spool.finish())
    }

    /// The first part's recording on disk is all there is of it until the merged audio is
    /// written: the save carries it to the history worker, which deletes it after the write.
    #[test]
    fn a_carried_parts_recording_goes_with_the_merged_save_and_stays_on_disk() {
        let (_on, part) = first_part("coord-carry");
        let mut config = Config::default();
        config.history.enabled = true;
        let mut rig = Rig::with(config);
        carry_on(&mut rig, &part);
        assert_eq!(
            rig.c.carried_spools.get(&JobId(1)),
            Some(&vec![part.clone()])
        );

        drain(&rig.history);
        rig.c
            .handle(transcribed(JobId(1), Some("one two")), Millis(2200));
        rig.event(Event::Tick, 2600); // the gap ends: pasted and filed
        let carried = drain(&rig.history).into_iter().find_map(|c| match c {
            HistoryCmd::Save { text, carried, .. } if text == "one two" => Some(carried),
            _ => None,
        });
        assert_eq!(
            carried,
            Some(vec![part.clone()]),
            "the merged save carries it"
        );
        assert!(rig.c.carried_spools.is_empty());
        // Still waiting (a quit before the write keeps it as failed) and on disk once closed:
        // only the history worker deletes it, after the write. Taken here in its place.
        assert!(oxi_platform::wav::spool_take(&part));
        assert!(part.is_file());
    }

    /// The merged dictation's transcription fails: the first part's recording is kept as a
    /// failed entry, not deleted with the merge.
    #[test]
    fn a_carried_parts_recording_is_kept_as_failed_when_the_merged_text_never_comes() {
        let (_on, part) = first_part("coord-carry-fail");
        let mut config = Config::default();
        config.history.enabled = true;
        let mut rig = Rig::with(config);
        carry_on(&mut rig, &part);
        drain(&rig.history);
        rig.c.handle(transcribed(JobId(1), None), Millis(2200));
        let orphans = oxi_platform::wav::spool_take_orphans();
        assert_eq!(orphans, vec![part.clone()], "given up: filed as failed");
        assert!(part.is_file(), "the first part's audio must stay");
        let filed = drain(&rig.history)
            .iter()
            .any(|c| matches!(c, HistoryCmd::Config(_)));
        assert!(filed, "the history worker gets the pass that files it");
        assert!(rig.c.carried_spools.is_empty());
    }

    /// Saving is turned off after the first part was streamed: nothing may be saved, so the
    /// carried part goes with the save the worker discards, not to the pass that files orphans.
    #[test]
    fn a_carried_part_is_deleted_not_filed_when_saving_is_turned_off() {
        let (_on, part) = first_part("coord-carry-off");
        let mut config = Config::default();
        config.history.enabled = true;
        let mut rig = Rig::with(config.clone());
        rig.event(Event::ModelLoaded, 0);
        rig.event(Event::ChordDown, 0);
        rig.event(Event::ChordUp, 1000);
        rig.event(Event::Tick, 1100);
        rig.c
            .handle(spooled_audio(JobId(0), Some(part.clone())), Millis(1150));
        rig.c
            .handle(transcribed(JobId(0), Some("one")), Millis(1200));
        rig.event(Event::ChordDown, 1300); // inside the gap: the dictation goes on
        config.history.enabled = false;
        rig.c.handle(
            Inbox::Config {
                config: Box::new(config),
                warnings: Vec::new(),
            },
            Millis(1400),
        );
        rig.event(Event::ChordUp, 2000);
        rig.event(Event::Tick, 2100);
        drain(&rig.history);
        rig.c.handle(spooled_audio(JobId(1), None), Millis(2150));
        rig.c
            .handle(transcribed(JobId(1), Some("two")), Millis(2200));
        rig.event(Event::Tick, 2600);
        let carried = drain(&rig.history).into_iter().find_map(|c| match c {
            HistoryCmd::Save { carried, cfg, .. } if !cfg.enabled => Some(carried),
            _ => None,
        });
        assert_eq!(carried, Some(vec![part]), "the worker deletes it");
        assert!(rig.c.carried_spools.is_empty());
    }

    /// Turns saving off while job 1, which carries the first part, is still in whisper.
    fn saving_off(rig: &mut Rig, at: u64) {
        let mut config = Config::default();
        config.history.enabled = false;
        rig.c.handle(
            Inbox::Config {
                config: Box::new(config),
                warnings: Vec::new(),
            },
            Millis(at),
        );
    }

    /// The save `sent` holds that deletes `part`: one with saving off.
    fn deleting_save(sent: Vec<HistoryCmd>) -> Option<Vec<PathBuf>> {
        sent.into_iter().find_map(|c| match c {
            HistoryCmd::Save { carried, cfg, .. } if !cfg.enabled => Some(carried),
            _ => None,
        })
    }

    /// Review c06 #1: saving turned off, then the merged transcription fails. The first part
    /// was given up as an orphan, which the pass with saving off filed as failed.
    #[test]
    fn a_carried_part_is_deleted_when_saving_is_off_and_the_merged_text_never_comes() {
        let (_on, part) = first_part("coord-carry-off-fail");
        let mut config = Config::default();
        config.history.enabled = true;
        let mut rig = Rig::with(config);
        carry_on(&mut rig, &part);
        saving_off(&mut rig, 2160);
        drain(&rig.history);
        rig.c.handle(transcribed(JobId(1), None), Millis(2200));
        let orphans = oxi_platform::wav::spool_take_orphans();
        assert!(orphans.is_empty(), "not to be filed: {orphans:?}");
        assert_eq!(deleting_save(drain(&rig.history)), Some(vec![part.clone()]));
        assert!(rig.c.carried_spools.is_empty());
        // Still waiting for the worker that deletes it; taken here in its place.
        assert!(oxi_platform::wav::spool_take(&part));
    }

    /// Review c06 #2: saving turned off, then a quit cuts the merged transcription short. The
    /// first part stayed waiting and the worker's last pass filed it as interrupted.
    #[test]
    fn a_carried_part_is_deleted_when_saving_is_off_and_a_quit_cuts_the_merge() {
        let (_on, part) = first_part("coord-carry-off-quit");
        let mut config = Config::default();
        config.history.enabled = true;
        let mut rig = Rig::with(config);
        carry_on(&mut rig, &part);
        saving_off(&mut rig, 2160);
        drain(&rig.history);
        let (_tx, rx) = channel();
        rig.c.shutdown(&rx, &|| Millis(0), Millis(0));
        assert_eq!(deleting_save(drain(&rig.history)), Some(vec![part.clone()]));
        let orphans = oxi_platform::wav::spool_take_orphans();
        assert!(orphans.is_empty(), "not to be filed: {orphans:?}");
        assert!(oxi_platform::wav::spool_take(&part));
    }

    /// The continuation had nothing after the trim: the earlier parts still go on as the
    /// dictation, and the history worker still gets the pass that files the kept recording.
    #[test]
    fn a_continuation_with_nothing_heard_still_files_its_kept_recording() {
        let mut config = Config::default();
        config.history.enabled = true;
        let mut rig = Rig::with(config);
        rig.event(Event::ModelLoaded, 0);
        rig.event(Event::ChordDown, 0);
        rig.event(Event::ChordUp, 1000);
        rig.event(Event::Tick, 1100);
        rig.c.handle(
            Inbox::JobAudio {
                job: JobId(0),
                samples: vec![0.1; 8000],
                spool: None,
            },
            Millis(1150),
        );
        rig.c.handle(
            Inbox::Transcribed {
                job: JobId(0),
                text: Some("one".to_owned()),
            },
            Millis(1200),
        );
        rig.event(Event::ChordDown, 1300);
        rig.event(Event::ChordUp, 2000);
        rig.event(Event::Tick, 2100);
        drain(&rig.history);
        let why = EmptyAudio::NothingHeard;
        rig.event(Event::JobAudioEmpty { job: JobId(1), why }, 2150);
        let sent = drain(&rig.history);
        assert!(
            sent.iter().any(|c| matches!(c, HistoryCmd::Config(_))),
            "the kept part is filed on the merge path too"
        );
        let sent: Vec<_> = drain(&rig.whisper)
            .into_iter()
            .filter_map(|c| match c {
                WhisperCmd::Transcribe {
                    job: JobId(1),
                    samples,
                } => Some(samples.len()),
                _ => None,
            })
            .collect();
        assert_eq!(sent, [8000], "the earlier parts go on as the dictation");
    }
}
