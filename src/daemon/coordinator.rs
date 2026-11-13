//! Coordinator thread: state machine, config, timers (ARCHITECTURE section 2).
//!
//! Executes the effects of `oxi_core::machine` and owns the data they refer to: job audio and
//! text by `JobId`, the active and pending config, the last pasted text. Every decision stays in
//! the machine (ARCHITECTURE 3); this module only carries values to the right worker.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use oxi_core::config::FileMode;
use oxi_core::config::{Config, History, TypeSpeed};
use oxi_core::ipc::{DeleteResult, Push, SoundWarning, TestFailure, TestOutcome};
use oxi_core::machine::{
    self, Effect, EmptyAudio, EngineWarning, Event, JobId, MachineConfig, ModelFailure, Note,
    Notice, OverlayState, Phase, PhaseCell, State,
};
use oxi_core::overlay;
use oxi_core::paste::{ClipboardOnlyWhy, Focus, PasteOutcome, Profile};
use oxi_core::tray::{self, TrayBadge};
use oxi_core::types::Millis;
use oxi_core::{stats, strings};

use super::cmd::{
    AudioCmd, ChordHook, HistoryCmd, Inbox, IpcAsk, PasteCmd, SoundCmd, StatusReport, ToFile,
    UiCmd, WhisperCmd, Workers,
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
    /// The app in front at key-down, now: the exe name (`Input::foreground`). A field so a test
    /// can name an app without a window.
    foreground: Box<dyn Fn() -> String>,
    /// Where input goes now (`Input::focus`), asked at key-down while `paste.press_enter` is on.
    /// A field so a test can move the focus.
    focus: Box<dyn Fn() -> Option<Focus>>,
    /// The session is locked or asleep (`Event::Lock`, `Suspend`, until `Unlock`, `Resume`): a
    /// recording started now gets no focus, so no Enter (review d08 #5).
    locked: bool,
    /// The profile of the recording that is running; goes to its job at `StopCapture`.
    next_profile: Option<Profile>,
    /// The profile each job runs with, until its paste (or its end without one).
    profiles: HashMap<JobId, Profile>,
    /// Jobs whose text goes to the file only: their entry says `File`, not a paste.
    file_only: HashSet<JobId>,
    /// What the pill says instead of the machine's plain end of this step (d03 review #5): the
    /// machine does not know the confidence filter or to-file mode. Gone when the step ends.
    cue: Option<OverlayState>,
    /// Text of transcribed jobs, until their paste is done.
    text: HashMap<JobId, String>,
    /// Whisper's words of transcribed jobs (with their confidence), until the history entry.
    words: HashMap<JobId, Vec<oxi_core::history::Word>>,
    /// Parts the confidence filter held back, until the history entry (never pasted, never lost).
    held: HashMap<JobId, Vec<oxi_core::history::HeldPart>>,
    /// The held parts of a job with text, joined, until its paste: "Copy last text" gets them
    /// after the text, so they are not lost when the entry is not written (arch-plan 9.1).
    held_copy: HashMap<JobId, String>,
    /// Job -> the jobs whose audio goes in front of its own (`Effect::Merge`), until its audio
    /// arrives.
    merges: HashMap<JobId, Vec<JobId>>,
    /// For "Copy last text". Memory only: never written to disk or logs.
    last_text: Option<String>,
    /// Audio length of each job, and the voiced part of it, from its audio to its paste, for the
    /// usage stats.
    spoken: HashMap<JobId, (Millis, Millis)>,
    /// When the text of the latest dictation arrived, to log how long the hold gap kept it.
    /// shortcut: one slot, a job overtaken by a newer text logs no wait; upgrade to a map if
    /// the log shows many `held_ms` missing.
    transcribed_at: Option<(JobId, std::time::Instant)>,
    /// Counts of finished dictations go here; never their text.
    stats: Option<Recorder>,
    workers: Workers,
    hook: H,
    /// Quit is waiting for the dictation in flight: its text goes to the clipboard, not pasted.
    quitting: bool,
    /// When the quit grace ends: a to-file write at quit gives up then.
    quit_by: Option<std::time::Instant>,
    /// The paste worker's to-file warning (arch-plan 9.4); `None` once a write worked again.
    file_warning: Option<String>,
    /// To-file writes sent to the paste worker whose `Inbox::FileWarning` has not come back, by
    /// job: each write answers exactly one. `true`: a write next to a paste, whose success the
    /// pill tells ("Also saved", d03-label review #1).
    file_writes: HashMap<JobId, bool>,
    /// Writes of a folder no longer written (the folder changed or file mode went off while they
    /// waited): their warnings are dropped (d04-fix 2).
    stale_file_writes: HashSet<JobId>,
    /// The model the newest recording runs with: a recording that starts while jobs still wait
    /// for whisper keeps it (arch-plan 9.2, no switch mid-queue).
    model: String,
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
            foreground: Box::new(|| {
                use oxi_core::paste::Input;
                oxi_platform::os::input::Keyboard.foreground().app
            }),
            focus: Box::new(|| {
                use oxi_core::paste::Input;
                oxi_platform::os::input::Keyboard.focus()
            }),
            locked: false,
            next_profile: None,
            profiles: HashMap::new(),
            file_only: HashSet::new(),
            cue: None,
            text: HashMap::new(),
            words: HashMap::new(),
            held: HashMap::new(),
            held_copy: HashMap::new(),
            merges: HashMap::new(),
            last_text: None,
            spoken: HashMap::new(),
            transcribed_at: None,
            stats: Recorder::system(),
            workers,
            hook,
            quitting: false,
            quit_by: None,
            file_warning: None,
            file_writes: HashMap::new(),
            stale_file_writes: HashSet::new(),
            model: String::new(),
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
        self.quit_by = Some(std::time::Instant::now() + Duration::from_millis(grace.0));
        self.handle(Inbox::Event(Event::Lock), now());
        let end = now() + grace;
        while self.state.busy() {
            let left = end - now();
            if left == Millis(0) {
                // Whisper's cancel check reads the phase: Idle ends its run.
                self.phase.store(Phase::Idle);
                tracing::warn!("shutdown: a dictation was cut short; a saved recording is kept");
                // Text in its paste or waiting for one: its `PasteDone` will not come, so the
                // entry is filed now, delivered unknown (review d03-fix #1: it was lost).
                let mut texts: Vec<JobId> = self.text.keys().copied().collect();
                texts.sort_unstable();
                for job in texts {
                    self.file_entry(job, Some(oxi_core::history::Delivered::Unknown));
                }
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
                voiced_ms,
            } => self.job_audio(job, samples, spool, Millis(voiced_ms), now),
            // A job merged into a longer dictation: whatever whisper made of it is not used. Its
            // audio may still wait for the merge.
            Inbox::Transcribed { job, .. } if self.state.superseded(job).is_none() => {}
            Inbox::Transcribed {
                job, text: None, ..
            } => {
                // The reason kind only: a model failure, else whisper itself.
                tracing::info!(?job, model_failure = ?self.model_failure, "transcription failed");
                self.no_text(job);
                self.feed(Event::TranscriptionFailed(job), now);
            }
            Inbox::Transcribed {
                job,
                text: Some(text),
                words: heard,
                mut held,
            } => {
                // Whisper holds every invention (its config waits for idle): kept only when this
                // job's entry is written with history on now, else dropped as a count (d03-fix #3).
                if !(self.history_wanted.contains(&job) && self.latest_history().enabled) {
                    let before = held.len();
                    held.retain(|h| h.why != oxi_core::text::HeldWhy::Hallucination);
                    let dropped = before - held.len();
                    if dropped > 0 {
                        tracing::info!(?job, dropped, "inventions dropped, no history entry");
                    }
                }
                self.clear_failed(|n, _| n == Notice::TranscriptionFailed);
                // Counts only: dictated text never goes into the log (AGENTS.md rule 5).
                let words = text.split_whitespace().count();
                let audio_ms = self.spoken.get(&job).map_or(0, |m| m.0.0);
                tracing::info!(?job, words, audio_ms, "transcription done");
                let empty = text.trim().is_empty();
                if empty && !held.is_empty() && self.state.superseded(job) == Some(true) {
                    // A newer press carries it on: its audio goes in front of the merged one,
                    // which is transcribed again. Kept for a give-back (`DropJob`).
                    self.words.insert(job, heard);
                    self.held.insert(job, held);
                } else if empty && !held.is_empty() {
                    self.file_filtered(job, heard, held);
                } else if empty {
                    self.no_text(job);
                } else {
                    if !held.is_empty() {
                        self.held_copy.insert(job, joined(&held));
                    }
                    self.text.insert(job, text);
                    self.words.insert(job, heard);
                    self.held.insert(job, held);
                    self.transcribed_at = Some((job, std::time::Instant::now()));
                    // The entry is filed once the paste says how the text arrived (`delivered`);
                    // with the hold gap a longer dictation may still carry this one on till then.
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
            Inbox::FileWarning { job, line } => self.file_answered(job, line),
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
            self.landed(job, outcome);
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
                let (merged, voiced) = self.take_merged(job, Vec::new());
                if !merged.is_empty() {
                    return self.job_audio(job, merged, None, voiced, now);
                }
            }
            // The job ends here: its profile must not hold the next recording's model (d04-fix 2).
            self.profiles.remove(&job);
        }
        match event {
            Event::ModelFailed(why) => self.model_failure = Some(why),
            Event::ModelLoaded => self.model_failure = None,
            // The lock screen may hold the focus at both reads, and what was typed meanwhile is
            // unseen: every dictation of before sends no Enter (review d08 #5).
            Event::Lock | Event::Suspend => {
                self.locked = true;
                let profiles = self.profiles.values_mut().chain(&mut self.next_profile);
                profiles.for_each(|p| p.focus_at_start = None);
            }
            // What the window drew may be stale after a sleep or a lock.
            Event::Resume | Event::Unlock => {
                self.locked = false;
                super::ipc::announce(Push::Woke);
            }
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

    /// The paste of `job` ended with `outcome`: the stats, the entry with how the text arrived,
    /// and "Copy last text".
    fn landed(&mut self, job: JobId, outcome: PasteOutcome) {
        use oxi_core::history::Delivered;
        let only = self.file_only.remove(&job);
        self.count(job, outcome);
        if matches!(
            outcome,
            PasteOutcome::Pasted | PasteOutcome::PastedNoRestore | PasteOutcome::Typed
        ) {
            // One cue: the file first, then the part held back. The file next to a paste is told
            // when its write answers (`Inbox::FileWarning`), not here: it may still fail.
            let some_held = self.held.get(&job).is_some_and(|h| confidence_held(h));
            self.cue = if only {
                Some(OverlayState::SavedToFile { also: false })
            } else {
                some_held.then_some(OverlayState::HeldBack { all: false })
            };
        }
        let delivered = match outcome {
            PasteOutcome::Pasted if only => Delivered::File,
            PasteOutcome::Pasted | PasteOutcome::PastedNoRestore => Delivered::Pasted,
            PasteOutcome::Typed => Delivered::Typed,
            PasteOutcome::ClipboardOnly(_) => Delivered::Clipboard,
            PasteOutcome::Failed => Delivered::Failed,
            PasteOutcome::FileFailed => Delivered::FileFailed,
        };
        self.file_entry(job, Some(delivered));
        self.keep_last_text(job);
    }

    /// The text of `job` is final: its recording and text go to the history worker, with how
    /// the text reached the user.
    fn file_entry(&mut self, job: JobId, delivered: Option<oxi_core::history::Delivered>) {
        let cfg = self.latest_history();
        let spool = self.spools.remove(&job);
        let samples = self.history_audio.remove(&job);
        let text = self.text.get(&job).cloned();
        let words = self.words.remove(&job).unwrap_or_default();
        let held = self.held.remove(&job).unwrap_or_default();
        match (samples, text) {
            (Some(samples), Some(text)) if self.history_wanted.remove(&job) => {
                let carried = self.carried_spools.remove(&job).unwrap_or_default();
                send(
                    &self.workers.history,
                    HistoryCmd::Save {
                        samples,
                        text,
                        words,
                        held,
                        delivered,
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

    /// The confidence filter held back all of `job`: nothing is pasted, but the entry keeps the
    /// held text as its own (`delivered` filtered), so the dictation is not lost.
    fn file_filtered(
        &mut self,
        job: JobId,
        words: Vec<oxi_core::history::Word>,
        held: Vec<oxi_core::history::HeldPart>,
    ) {
        self.profiles.remove(&job);
        if confidence_held(&held) {
            self.cue = Some(OverlayState::HeldBack { all: true });
        }
        self.text.insert(job, joined(&held));
        self.words.insert(job, words);
        self.held.insert(job, held);
        self.file_entry(job, Some(oxi_core::history::Delivered::Filtered));
        // Nothing was inserted: "Copy last text" is where it can be had at once.
        self.keep_last_text(job);
        self.spoken.remove(&job);
    }

    /// `samples` is the audio of `job`; when a press inside the hold gap carried earlier jobs
    /// on, their audio goes in front. Their results are dropped; their recordings on disk go with
    /// the merged save (`carried_spools`). Also returns their voiced time, to add to the job's.
    fn take_merged(&mut self, job: JobId, samples: Vec<f32>) -> (Vec<f32>, Millis) {
        let Some(from) = self.merges.remove(&job) else {
            return (samples, Millis(0));
        };
        // The Enter checks against where the whole dictation started: its first part.
        let first = from.first().and_then(|j| self.profiles.get(j));
        let start = first.and_then(|p| p.focus_at_start);
        if let Some(p) = self.profiles.get_mut(&job) {
            p.focus_at_start = p.focus_at_start.and(start);
        }
        let mut voiced = 0;
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
            self.words.remove(&j);
            self.held.remove(&j);
            self.held_copy.remove(&j);
            // Its profile too: it ends here, with no paste and no `no_text` (review d04 #3).
            self.profiles.remove(&j);
            voiced += self.spoken.remove(&j).map_or(0, |(_, v)| v.0);
            self.history_wanted.remove(&j);
            // Its own carried parts too. They stay waiting in the spool registry, so a crash or
            // a quit before the merged save keeps them as failed; the history worker deletes
            // them once the merged audio is written.
            let carried = self.carried_spools.remove(&j).unwrap_or_default();
            let carried = carried.into_iter().chain(self.spools.remove(&j));
            self.carried_spools.entry(job).or_default().extend(carried);
        }
        all.extend(samples);
        (all, Millis(voiced))
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
                    words: Vec::new(),
                    held: Vec::new(),
                    delivered: None,
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

    fn job_audio(
        &mut self,
        job: JobId,
        samples: Vec<f32>,
        spool: Option<PathBuf>,
        voiced: Millis,
        now: Millis,
    ) {
        let (samples, merged_voiced) = self.take_merged(job, samples);
        let voiced = Millis(voiced.0.saturating_add(merged_voiced.0));
        if let Some(spool) = spool {
            self.spools.insert(job, spool);
        }
        // Sound from a recording started after the failure; the one that lost its
        // device delivers its own tail first.
        let capture = self.capture;
        self.clear_failed(|n, session| {
            matches!(
                n,
                Notice::DeviceLost | Notice::CaptureFailed | Notice::NoMicrophone
            ) && session < capture
        });
        self.spoken
            .insert(job, (stats::audio_length(samples.len()), voiced));
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
            self.profiles.remove(&job);
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
        // Whisper keeps a list of per-app models that failed to load; its `Config` clears it, so
        // a model downloaded again (any, not only the configured one) is tried once more.
        send(
            &self.workers.whisper,
            WhisperCmd::Config(Box::new(self.active.clone())),
        );
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
        // A cue no step showed: the next queued dictation runs on under the same pill. The cue
        // goes up now and that pill after it, once the cue had its time (d03-label review #2).
        if let Some(cue) = self.cue.take() {
            self.ui(UiCmd::Overlay(cue));
            self.ui(UiCmd::Overlay(self.pill));
        }
    }

    /// The pill the machine asks for, with the cue of the dictation that just ended.
    fn overlay(&mut self, state: OverlayState) {
        let shown = self.cued(state);
        match self.cue.take() {
            // Shown in its place, or a new press: that is the news now.
            Some(_) if shown != state || matches!(state, OverlayState::Recording { .. }) => {}
            // The next queued dictation goes on at once: the cue of this one goes first, and the
            // pill keeps it for its time (d03-label review #2).
            Some(cue) => self.ui(UiCmd::Overlay(cue)),
            None => {}
        }
        self.overlay_progress(shown);
        self.ui(UiCmd::Overlay(shown));
    }

    /// The to-file write of `job` answered: its warning, and "Also saved" for a write next to a
    /// paste that worked (d03-label review #1). By job: a write the full queue refuses answers
    /// before the writes ahead of it.
    fn file_answered(&mut self, job: JobId, line: Option<String>) {
        let also = self.file_writes.remove(&job).unwrap_or(false);
        // Only on a pill with nothing else to say: a dictation running or a part held back is
        // the news.
        if also && line.is_none() && self.pill == OverlayState::Hidden {
            let cue = OverlayState::SavedToFile { also: true };
            self.overlay_progress(cue);
            self.ui(UiCmd::Overlay(cue));
        }
        if !self.stale_file_writes.remove(&job) {
            self.file_warning = line;
        }
    }

    /// `state` the machine asks for, or the cue that takes its place: the end of a dictation
    /// (the pill would hide) or its "nothing heard" when the filter held back all of it.
    fn cued(&self, state: OverlayState) -> OverlayState {
        match (state, self.cue) {
            (
                OverlayState::Text(Notice::NothingHeard),
                Some(c @ OverlayState::HeldBack { all: true }),
            )
            | (
                OverlayState::Hidden,
                Some(
                    c @ (OverlayState::HeldBack { all: false } | OverlayState::SavedToFile { .. }),
                ),
            ) => c,
            _ => state,
        }
    }

    /// The machine gave `job` back with no text to paste: its recording is kept as failed, or,
    /// when all of it was held, filed like any all-held dictation.
    fn drop_job(&mut self, job: JobId) {
        self.profiles.remove(&job);
        self.audio.remove(&job);
        self.text.remove(&job);
        self.held_copy.remove(&job);
        let words = self.words.remove(&job).unwrap_or_default();
        match self.held.remove(&job) {
            Some(held) if !held.is_empty() => self.file_filtered(job, words, held),
            _ => {
                self.spoken.remove(&job);
                self.no_transcript(job);
            }
        }
    }

    /// A cancelled recording is kept: its audio goes to the history as an entry with no text.
    fn keep_cancelled(&mut self, job: JobId) {
        self.profiles.remove(&job);
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
                    words: Vec::new(),
                    held: Vec::new(),
                    delivered: None,
                    cfg,
                    spool,
                    carried,
                },
            );
        } else {
            self.give_up_carried(job);
        }
    }

    /// Runs one effect; returns the event it answers with, if any.
    fn execute(&mut self, effect: &Effect) -> Option<Event> {
        match *effect {
            Effect::StartCapture => {
                self.capture += 1;
                let session = self.capture;
                // The microphone first: the profile asks the OS for the app in front (d11).
                let pressed = self.hook.take_pressed();
                send(&self.workers.audio, AudioCmd::Start { session, pressed });
                tracing::info!(session, "recording started");
                self.start_profile();
            }
            Effect::StopCapture(job) => {
                tracing::info!(?job, "recording stopped");
                self.stop_profile(job);
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
            Effect::DropJob(job) => self.drop_job(job),
            Effect::KeepCancelled(job) => self.keep_cancelled(job),
            Effect::Preload => send(&self.workers.whisper, WhisperCmd::Preload),
            Effect::Unload => send(&self.workers.whisper, WhisperCmd::Unload),
            Effect::Transcribe(job) => return self.transcribe(job),
            Effect::Paste(job) => {
                // How long the hold gap kept the finished text; no text in the line.
                let held_ms = match self.transcribed_at.take() {
                    Some((j, at)) if j == job => at.elapsed().as_millis(),
                    other => {
                        self.transcribed_at = other;
                        0
                    }
                };
                tracing::info!(?job, held_ms, "paste released");
                let answer = self.paste(job);
                // Answered here (quit, no text), not by the paste worker: the same steps.
                if let Some(Event::PasteDone { job, outcome }) = answer {
                    self.landed(job, outcome);
                }
                return answer;
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
                    self.file_output_changed(&config.output);
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
                self.overlay(state);
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

    /// The app in front now decides the language, model and text switches of this recording.
    /// Whisper hears the model at once: it loads it while the user speaks, or its preload does
    /// (arch-plan 9.2). While earlier jobs still wait for whisper the recording keeps their
    /// model: a busy machine never switches mid-queue; language and text switches still apply.
    fn start_profile(&mut self) {
        let mut profile = oxi_core::paste::profile(&self.active, &(self.foreground)());
        let queued = self.profiles.keys().any(|job| !self.text.contains_key(job));
        if queued && !self.model.is_empty() {
            profile.model.clone_from(&self.model);
        }
        self.model.clone_from(&profile.model);
        send(
            &self.workers.whisper,
            WhisperCmd::UseModel(profile.model.clone()),
        );
        // Asked only for the optional Enter: no OS call on the default path.
        if self.active.paste.press_enter && !self.locked {
            profile.focus_at_start = (self.focus)();
        }
        self.next_profile = Some(profile);
    }

    /// The recording `job` ended with the profile it started with.
    fn stop_profile(&mut self, job: JobId) {
        let profile = self.next_profile.take();
        let profile = profile.unwrap_or_else(|| oxi_core::paste::profile(&self.active, ""));
        self.profiles.insert(job, profile);
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
        let profile = self.profiles.get(&job).cloned();
        let profile = profile.unwrap_or_else(|| oxi_core::paste::profile(&self.active, ""));
        send(
            &self.workers.whisper,
            WhisperCmd::Transcribe {
                job,
                samples,
                profile: Box::new(profile),
            },
        );
        None
    }

    /// Hands the text of `job` to the paste worker; the event when the paste is answered here.
    fn paste(&mut self, job: JobId) -> Option<Event> {
        let profile = self.profiles.remove(&job);
        let focus_at_start = profile.as_ref().and_then(|p| p.focus_at_start);
        let to_file = profile.map(|p| p.to_file);
        // Cannot happen (empty text is never pasted); fail the job, not the machine.
        let Some(text) = self.text.get(&job) else {
            let outcome = PasteOutcome::Failed;
            return Some(Event::PasteDone { job, outcome });
        };
        let text = text.clone();
        let out = &self.active.output;
        // No folder means no file (`effective_mode`): a rule's mode counts only when one is set.
        let file = match to_file.unwrap_or(FileMode::Off) {
            FileMode::Off => None,
            _ if out.file_folder.trim().is_empty() => None,
            mode => Some(ToFile {
                folder: PathBuf::from(out.file_folder.trim()),
                format: out.file_format,
                only: mode == FileMode::Only,
            }),
        };
        if self.quitting {
            // At quit the focus is the tray or a closing window: the text waits on the
            // clipboard, and the notice says so ("press Ctrl+V"). Its file still gets it, within
            // the quit grace (review d04 #7).
            let in_history = self.active.paste.keep_in_clipboard_history;
            let copy = PasteCmd::CopyOnly {
                text: text.clone(),
                in_history,
            };
            send(&self.workers.paste, copy);
            if let Some(to) = file {
                let until = self.quit_by.unwrap_or_else(std::time::Instant::now);
                send(&self.workers.paste, PasteCmd::File { text, to, until });
            }
            let outcome = PasteOutcome::ClipboardOnly(ClipboardOnlyWhy::ModifiersHeld);
            return Some(Event::PasteDone { job, outcome });
        }
        if let Some(f) = &file {
            if f.only {
                self.file_only.insert(job);
            }
            self.file_writes.insert(job, !f.only);
        }
        let cfg = self.active.paste.clone();
        send(
            &self.workers.paste,
            PasteCmd::Paste {
                job,
                text,
                cfg,
                file,
                focus_at_start,
            },
        );
        None
    }

    /// A dictation that reached the field or the clipboard adds its word count and audio length
    /// to the usage stats.
    fn count(&mut self, job: JobId, outcome: PasteOutcome) {
        // `voiced` 0 is "not known" (`stats::Stats::voiced_time`): the row stays out of the pause share.
        let (speech, voiced) = self.spoken.remove(&job).unwrap_or_default();
        if let (Some(stats), Some(text)) = (&self.stats, self.text.get(&job))
            && !matches!(outcome, PasteOutcome::Failed | PasteOutcome::FileFailed)
        {
            stats.add_voiced(stats::word_count(text), speech, voiced);
        }
    }

    /// Every outcome keeps the text, a failed paste too: "Copy last text" still has it (item 42).
    fn keep_last_text(&mut self, job: JobId) {
        let held = self.held_copy.remove(&job);
        let Some(mut text) = self.text.remove(&job) else {
            return;
        };
        if let Some(held) = held {
            text = format!("{} {held}", text.trim_end());
        }
        if self.last_text.replace(text).is_none() {
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
            .chain(self.file_warning.clone())
            .chain(self.config_error.clone())
            .collect()
    }

    /// The to-file warning goes when the failing folder is no longer written to: file mode
    /// turned off or another folder (d04-fix 3). A later failure says so again.
    fn file_output_changed(&mut self, new: &oxi_core::config::Output) {
        let old = &self.active.output;
        let off = new.effective_mode() == FileMode::Off;
        if old.file_folder.trim() != new.file_folder.trim()
            || off && old.effective_mode() != FileMode::Off
        {
            self.file_warning = None;
            self.stale_file_writes.extend(self.file_writes.keys());
        }
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

/// Whether the confidence filter held something back; inventions kept for the history do not
/// count: the user never heard those.
fn confidence_held(held: &[oxi_core::history::HeldPart]) -> bool {
    held.iter()
        .any(|h| h.why != oxi_core::text::HeldWhy::Hallucination)
}

/// The texts of held parts joined with a space.
fn joined(held: &[oxi_core::history::HeldPart]) -> String {
    held.iter()
        .map(|h| h.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
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
mod tests;
