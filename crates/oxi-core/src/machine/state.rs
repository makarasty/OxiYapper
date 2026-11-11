//! The machine itself: `State` and the transitions behind [`super::step`].

use std::collections::VecDeque;

use crate::audio::DETECTOR_LATENCY;
use crate::config::OtherKey;

use super::types::{
    Effect, Effects, EmptyAudio, Event, JobId, MachineConfig, Mode, ModelFailure, ModelIdentity,
    Notice, OverlayState, Phase, Sound,
};
use crate::paste::{ClipboardOnlyWhy, PasteOutcome};
use crate::types::Millis;

/// Jobs allowed to wait (for audio, whisper or paste) before a key-down is refused. The job in
/// whisper, the one pasting and the recording itself are not waiting (ARCHITECTURE 3).
const MAX_WAITING_JOBS: usize = 2;

/// `other_key = auto`: a key this soon after the chord press, before speech, is a shortcut
/// (Ctrl+Win+D). Later the recording goes on whatever the level, so a quiet mic loses nothing.
const SHORTCUT_WINDOW: Millis = Millis(400);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Push mode, chord held.
    Held,
    /// Push mode, locked on by a double tap; the next press stops it.
    Locked,
    /// Toggle mode; the next press stops it.
    Toggled,
}

#[derive(Debug, Clone)]
struct Recording {
    started: Millis,
    kind: Kind,
    /// Pressed within the double-tap gap of a short press: a short release locks it.
    lock_candidate: bool,
    /// `warn_before` has passed.
    warned: bool,
    next_poll: Option<Millis>,
    /// The stopping key came; capture runs on until then (`release_tail`) and stops.
    tail_until: Option<Millis>,
    /// The audio side heard speech ([`Event::SpeechHeard`]): a cancel now keeps the audio.
    speech: bool,
    /// A shortcut key came: cancelled at this time unless speech is heard first.
    cancel_at: Option<Millis>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    AwaitingAudio,
    Queued,
    Transcribing,
    Transcribed,
    Pasting,
}

#[derive(Debug, Clone)]
struct Job {
    id: JobId,
    stage: Stage,
    /// Cancelled after speech: the audio goes to the history, never to whisper. Not a phase.
    cancelled: bool,
    /// Ended by a mic or model failure (`cancelled` too): its audio is kept as failed.
    failed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelStatus {
    Unloaded,
    Loading,
    Loaded,
    Failed(ModelFailure),
}

/// Everything the machine remembers. Build with [`State::new`], drive with [`super::step`].
#[derive(Debug, Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent flags, one per remembered fact"
)]
pub struct State {
    cfg: MachineConfig,
    /// Config received while busy; swapped in when the machine is Idle (item 11).
    pending_cfg: Option<MachineConfig>,
    rec: Option<Recording>,
    /// Oldest first. Whisper and paste take jobs strictly in this order.
    jobs: VecDeque<Job>,
    next_job: u64,
    model: ModelStatus,
    /// Release time of a lone short press. The double-tap window is checked against it at
    /// the next press, so it needs no timer of its own.
    last_tap: Option<Millis>,
    /// Between the hook's `ChordDown` and its `ChordUp`, in both modes.
    chord_held: bool,
    notice: Option<(Notice, Millis)>,
    idle_unload_at: Option<Millis>,
    /// Sleep asked for an unload; done as soon as nothing is in flight.
    unload_when_idle: bool,
    /// The settings the loading or loaded model was asked for; `None` when there is none.
    /// A load answer for other settings than `cfg.model` is stale.
    model_for: Option<ModelIdentity>,
    shown: OverlayState,
    /// One-time notice already given in this daemon run.
    mic_fallback_noted: bool,
}

impl Default for State {
    fn default() -> Self {
        Self::new(MachineConfig::default())
    }
}

impl State {
    pub fn new(cfg: MachineConfig) -> Self {
        Self {
            cfg,
            pending_cfg: None,
            rec: None,
            jobs: VecDeque::new(),
            next_job: 0,
            model: ModelStatus::Unloaded,
            last_tap: None,
            chord_held: false,
            notice: None,
            idle_unload_at: None,
            unload_when_idle: false,
            model_for: None,
            shown: OverlayState::Hidden,
            mic_fallback_noted: false,
        }
    }

    pub fn phase(&self) -> Phase {
        if self.rec.is_some() {
            Phase::Recording
        } else if self.jobs.iter().all(|j| j.cancelled) {
            Phase::Idle
        } else if self
            .jobs
            .iter()
            .any(|j| !j.cancelled && !matches!(j.stage, Stage::Transcribed | Stage::Pasting))
        {
            Phase::Transcribing
        } else {
            Phase::Pasting
        }
    }

    /// A recording or a job (cancelled ones too, until their audio is kept) is still in flight.
    /// [`Self::phase`] says Idle for a cancelled job whose audio has not come back yet.
    pub fn busy(&self) -> bool {
        self.rec.is_some() || !self.jobs.is_empty()
    }

    /// The earliest armed timer: max recording, its warning, the watchdog poll, the release
    /// tail, the overlay hide, the idle unload.
    pub fn next_deadline(&self) -> Option<Millis> {
        let rec = self
            .rec
            .iter()
            .flat_map(|r| {
                [
                    Some(self.max_at(r)),
                    (!r.warned).then(|| self.warn_at(r)),
                    r.next_poll,
                    r.tail_until,
                    r.cancel_at,
                ]
            })
            .flatten();
        rec.chain(self.notice.map(|(_, t)| t))
            .chain(self.idle_unload_at)
            .min()
    }

    fn max_at(&self, r: &Recording) -> Millis {
        r.started + self.cfg.max_recording
    }

    fn warn_at(&self, r: &Recording) -> Millis {
        r.started + (self.cfg.max_recording - self.cfg.warn_before)
    }

    pub(super) fn handle(&mut self, event: Event, now: Millis) -> Effects {
        let mut fx = Effects::new();
        match event {
            Event::ChordDown => {
                let repeat = std::mem::replace(&mut self.chord_held, true);
                self.chord_down(now, repeat, &mut fx);
            }
            // A poll that finds the chord released is a key-up the hook never delivered.
            Event::ChordUp | Event::ChordPolled { held: false } => {
                self.chord_held = false;
                self.chord_up(now, &mut fx);
            }
            Event::ChordPolled { held: true } => {}
            Event::Esc => {
                if self.cfg.esc_cancels {
                    self.cancel_recording(now, true, &mut fx);
                }
            }
            Event::OtherKey => self.other_key(now, &mut fx),
            Event::SpeechHeard => {
                if let Some(r) = self.rec.as_mut() {
                    r.speech = true;
                    r.cancel_at = None;
                }
            }
            Event::DeviceLost => {
                if self.rec.is_some() {
                    self.stop_recording(now, true, &mut fx);
                    fx.push(Effect::Notify(Notice::DeviceLost));
                }
            }
            Event::CaptureFailed => {
                if self.rec.is_some() {
                    self.end_failed_recording(&mut fx);
                    self.fail(Notice::CaptureFailed, now, &mut fx);
                }
            }
            Event::JobAudioReady(id) => self.audio_ready(id, now, &mut fx),
            Event::JobAudioEmpty { job, why } => self.audio_empty(job, why, now, &mut fx),
            Event::TranscriptionDone { job, empty } => {
                if let Some(j) = self.job_in(job, Stage::Transcribing) {
                    j.stage = Stage::Transcribed;
                    if empty {
                        self.jobs.retain(|j| j.id != job);
                        self.show(Notice::NothingHeard, now);
                    }
                }
            }
            Event::TranscriptionFailed(job) => {
                if self.job_in(job, Stage::Transcribing).is_some() {
                    self.jobs.retain(|j| j.id != job);
                    // A model that failed to load for this job was announced already: one notice.
                    if !matches!(self.model, ModelStatus::Failed(_)) {
                        self.fail(Notice::TranscriptionFailed, now, &mut fx);
                    }
                }
            }
            Event::PasteDone { job, outcome } => {
                if self.job_in(job, Stage::Pasting).is_some() {
                    self.jobs.retain(|j| j.id != job);
                    self.paste_notice(outcome, now, &mut fx);
                }
            }
            Event::MicFellBack => {
                if !std::mem::replace(&mut self.mic_fallback_noted, true) {
                    fx.push(Effect::Notify(Notice::MicFallback));
                }
            }
            Event::ModelLoaded if self.stale_load() => {
                self.unload(&mut fx); // the old settings' model; `dispatch` loads for a job
                self.preload_for_recording(&mut fx);
            }
            Event::ModelLoaded => {
                self.model = ModelStatus::Loaded;
                self.model_for = Some(self.cfg.model.clone());
            }
            Event::ModelFailed(_) if self.stale_load() => {
                // A model no longer selected failed: nothing to tell, the new one may load.
                self.model = ModelStatus::Unloaded;
                self.model_for = None;
                self.preload_for_recording(&mut fx);
            }
            Event::ModelFailed(why) => {
                self.model = ModelStatus::Failed(why);
                self.model_for = None;
                self.end_failed_recording(&mut fx);
                self.fail(model_notice(why), now, &mut fx);
            }
            Event::ConfigChanged(cfg) => self.pending_cfg = Some(cfg),
            Event::Suspend => {
                self.cancel_recording(now, false, &mut fx);
                self.unload_when_idle = true;
            }
            Event::Lock => self.cancel_recording(now, false, &mut fx),
            Event::Resume | Event::Unlock => fx.push(Effect::ReinstallHook),
            Event::Tick => self.tick(now, &mut fx),
        }
        self.dispatch(&mut fx);
        self.settle(now, &mut fx);
        fx
    }

    /// A key outside the chord went down; it reached the system already. Whether it cancels the
    /// recording follows `other_key` and whether speech was heard (R2.1).
    fn other_key(&mut self, now: Millis, fx: &mut Effects) {
        // A recording that exists while the chord is held was started by this press: a press
        // that stops one leaves none, or only its tail.
        let held = self.chord_held;
        let Some(r) = self.rec.as_mut().filter(|r| held && r.tail_until.is_none()) else {
            return;
        };
        match self.cfg.other_key {
            OtherKey::Never => {}
            OtherKey::Always => {
                // Silent unless there is audio to keep.
                let speech = r.speech;
                self.cancel_recording(now, speech, fx);
            }
            // Early and before speech: a shortcut, cancelled once the detector had time to
            // report speech that already started. Otherwise the recording goes on.
            OtherKey::Auto => {
                if !r.speech && r.cancel_at.is_none() && now - r.started <= SHORTCUT_WINDOW {
                    r.cancel_at = Some(now + DETECTOR_LATENCY);
                }
            }
        }
    }

    fn job_in(&mut self, id: JobId, stage: Stage) -> Option<&mut Job> {
        self.jobs
            .iter_mut()
            .find(|j| j.id == id && j.stage == stage)
    }

    /// The audio of a stopped job is ready: for whisper, or for the history when it was cancelled.
    fn audio_ready(&mut self, id: JobId, now: Millis, fx: &mut Effects) {
        let Some(job) = self.job_in(id, Stage::AwaitingAudio) else {
            return;
        };
        if job.failed {
            // No transcript will come: the coordinator files the audio as failed.
            self.jobs.retain(|j| j.id != id);
            fx.push(Effect::DropJob(id));
            self.show_unless_recording(Notice::FailedKept, now);
        } else if job.cancelled {
            self.jobs.retain(|j| j.id != id);
            fx.push(Effect::KeepCancelled(id));
            // With history off nothing is written: "kept" would be false (triage #69).
            let notice = if self.cfg.keep_history {
                Notice::CancelledKept
            } else {
                Notice::Cancelled
            };
            self.show_unless_recording(notice, now);
        } else {
            job.stage = Stage::Queued;
        }
    }

    /// A stopped job with no audio ends here, without a whisper call (ARCHITECTURE 9 step 6).
    fn audio_empty(&mut self, job: JobId, why: EmptyAudio, now: Millis, fx: &mut Effects) {
        let Some(found) = self.job_in(job, Stage::AwaitingAudio) else {
            return;
        };
        let (cancelled, failed) = (found.cancelled, found.failed);
        self.jobs.retain(|j| j.id != job);
        if failed {
            // A broken capture still filed what it streamed; otherwise the failure notice stands.
            if why == EmptyAudio::Failed {
                self.show_unless_recording(Notice::FailedKept, now);
            }
            return;
        }
        if cancelled {
            // Nothing to keep after all: the cancel stands, whatever the audio side says.
            return self.show_unless_recording(Notice::Cancelled, now);
        }
        match why {
            EmptyAudio::NothingHeard => self.show(Notice::NothingHeard, now),
            EmptyAudio::MicSilence => self.fail(Notice::MicSilence, now, fx),
            EmptyAudio::Failed => self.fail(Notice::CaptureFailed, now, fx),
        }
    }

    /// What the user must know about how the text arrived (items 35, 36; ARCHITECTURE 10
    /// step 4, item 42). Clipboard-only, a failure and a skipped restore say so every time:
    /// no silent loss. The done sound plays when the text was pasted.
    fn paste_notice(&mut self, outcome: PasteOutcome, now: Millis, fx: &mut Effects) {
        let notice = match outcome {
            PasteOutcome::Failed => return self.fail(Notice::PasteFailed, now, fx),
            PasteOutcome::Pasted | PasteOutcome::Typed => None,
            PasteOutcome::ClipboardOnly(ClipboardOnlyWhy::ModifiersHeld) => {
                Some(Notice::PressPaste)
            }
            PasteOutcome::ClipboardOnly(ClipboardOnlyWhy::Elevated) => Some(Notice::AdminWindow),
            PasteOutcome::PastedNoRestore => Some(Notice::RestoreSkipped),
        };
        if !matches!(outcome, PasteOutcome::ClipboardOnly(_)) {
            fx.push(Effect::Play(Sound::Done));
        }
        fx.extend(notice.map(Effect::Notify));
    }

    /// Overlay text for `overlay_hide`, then back to the phase view.
    fn show(&mut self, notice: Notice, now: Millis) {
        self.notice = Some((notice, now + self.cfg.overlay_hide));
    }

    fn show_unless_recording(&mut self, notice: Notice, now: Millis) {
        if self.rec.is_none() {
            self.show(notice, now);
        }
    }

    /// Error outcome: sound, overlay text, notification.
    fn fail(&mut self, notice: Notice, now: Millis, fx: &mut Effects) {
        fx.push(Effect::Play(Sound::Error));
        fx.push(Effect::Notify(notice));
        self.show(notice, now);
    }

    fn discard_recording(&mut self, fx: &mut Effects) {
        if self.rec.take().is_some() {
            fx.push(Effect::DiscardCapture);
        }
    }

    /// A mic or model failure ends the recording. With speech and history on its audio is
    /// kept, as a job whisper never sees (triage #12); otherwise it is thrown away.
    fn end_failed_recording(&mut self, fx: &mut Effects) {
        let Some(r) = self.rec.take() else { return };
        if r.speech && self.cfg.keep_history {
            let id = self.push_job(true, true);
            fx.push(Effect::StopCapture(id));
        } else {
            fx.push(Effect::DiscardCapture);
        }
    }

    /// A new job awaiting the audio of the recording that just ended.
    fn push_job(&mut self, cancelled: bool, failed: bool) -> JobId {
        let id = JobId(self.next_job);
        self.next_job += 1;
        self.jobs.push_back(Job {
            id,
            stage: Stage::AwaitingAudio,
            cancelled,
            failed,
        });
        id
    }

    /// Cancel the recording in progress. After speech the audio is kept as a cancelled job
    /// (R2.1) and the notice comes with it; before speech it is thrown away. `explicit`: the
    /// user asked for it (Esc, or a key that always cancels): sound, and the overlay text when
    /// there is no audio to keep. Otherwise it is silent.
    fn cancel_recording(&mut self, now: Millis, explicit: bool, fx: &mut Effects) {
        let Some(r) = self.rec.take() else { return };
        if r.speech {
            let id = self.push_job(true, false);
            fx.push(Effect::StopCapture(id));
        } else {
            fx.push(Effect::DiscardCapture);
            if explicit {
                self.show(Notice::Cancelled, now);
            }
        }
        if explicit {
            fx.push(Effect::Play(Sound::Cancel));
        }
    }

    /// `repeat`: the chord was already held, so this is the hook repeating itself.
    fn chord_down(&mut self, now: Millis, repeat: bool, fx: &mut Effects) {
        let Some(r) = self.rec.as_ref() else {
            return self.start_recording(now, fx);
        };
        if r.tail_until.is_some() {
            if !repeat {
                // A new press must not wait for the old tail: no lost words (ARCHITECTURE 3).
                // A shortcut cancel still pending is not lost to it: shortcut audio must not
                // reach whisper.
                if r.cancel_at.is_some() {
                    self.cancel_recording(now, false, fx);
                } else {
                    self.stop_recording(now, false, fx);
                }
                self.start_recording(now, fx);
            }
        } else if r.kind != Kind::Held {
            self.release(now, fx);
        } // Held: down without an up, the hook repeated itself
    }

    fn start_recording(&mut self, now: Millis, fx: &mut Effects) {
        if let ModelStatus::Failed(why) = self.model {
            self.fail(model_notice(why), now, fx);
            return;
        }
        let waiting = self.jobs.iter().filter(|j| {
            !j.cancelled
                && matches!(
                    j.stage,
                    Stage::AwaitingAudio | Stage::Queued | Stage::Transcribed
                )
        });
        if waiting.count() >= MAX_WAITING_JOBS {
            fx.push(Effect::Play(Sound::Error));
            return;
        }
        let push = self.cfg.mode == Mode::Push;
        let lock_candidate = push
            && self
                .last_tap
                .is_some_and(|t| now - t < self.cfg.double_tap_gap);
        self.last_tap = None;
        self.notice = None;
        self.idle_unload_at = None;
        self.unload_when_idle = false;
        self.rec = Some(Recording {
            started: now,
            kind: if push { Kind::Held } else { Kind::Toggled },
            lock_candidate,
            warned: false,
            // Every recording starts with the chord held: watch for a lost key-up in both
            // modes, so a stale `chord_held` never lets typing cancel a toggle recording.
            next_poll: Some(now + self.cfg.watchdog_period),
            tail_until: None,
            speech: false,
            cancel_at: None,
        });
        fx.push(Effect::StartCapture);
        if self.model == ModelStatus::Unloaded {
            self.preload(fx);
        }
        fx.push(Effect::Play(Sound::Start));
    }

    fn preload(&mut self, fx: &mut Effects) {
        self.model = ModelStatus::Loading;
        self.model_for = Some(self.cfg.model.clone());
        fx.push(Effect::Preload);
    }

    /// After a stale load answer: the recording in progress needs the new model.
    fn preload_for_recording(&mut self, fx: &mut Effects) {
        if self.rec.is_some() {
            self.preload(fx);
        }
    }

    fn stale_load(&self) -> bool {
        self.model_for
            .as_ref()
            .is_some_and(|m| *m != self.cfg.model)
    }

    fn chord_up(&mut self, now: Millis, fx: &mut Effects) {
        let Some(r) = self.rec.as_mut() else { return };
        r.next_poll = None; // chord released: nothing left to watch
        if r.kind != Kind::Held || r.tail_until.is_some() {
            return; // toggle ignores key-up; a poll may repeat a real one
        }
        let held = now - r.started;
        if r.lock_candidate && held < self.cfg.double_tap_max_press {
            r.kind = Kind::Locked;
            // A lock is never silent: the start sound again, now the second one in a row.
            // shortcut: no sound role of its own; a `Sound::Locked` needs the daemon player and config schema.
            fx.push(Effect::Play(Sound::Start));
        } else if held < self.cfg.tap_max {
            // A tap makes "Thank you." hallucinations: throw it away. A shortcut is no tap.
            if !r.lock_candidate && held < self.cfg.double_tap_max_press && r.cancel_at.is_none() {
                self.last_tap = Some(now);
            }
            self.discard_recording(fx);
        } else {
            self.release(now, fx);
        }
    }

    /// The user stopped the recording: discard a tap, else stop after `release_tail`.
    fn release(&mut self, now: Millis, fx: &mut Effects) {
        let tail = self.cfg.release_tail;
        let tap_max = self.cfg.tap_max;
        match self.rec.as_mut() {
            // A pending shortcut cancel decides first: the tail lasts at least until then.
            Some(r)
                if (tail > Millis(0) || r.cancel_at.is_some()) && now - r.started >= tap_max =>
            {
                r.tail_until = Some(r.cancel_at.map_or(now + tail, |c| c.max(now + tail)));
            }
            _ => self.stop_recording(now, true, fx),
        }
    }

    /// End the recording and make it a job. `discard_short`: audio under `tap_max` is dropped.
    fn stop_recording(&mut self, now: Millis, discard_short: bool, fx: &mut Effects) {
        let Some(r) = self.rec.take() else { return };
        if discard_short && now - r.started < self.cfg.tap_max {
            fx.push(Effect::DiscardCapture);
            return;
        }
        let id = self.push_job(false, false);
        fx.push(Effect::StopCapture(id));
        fx.push(Effect::Play(Sound::Stop));
    }

    fn tick(&mut self, now: Millis, fx: &mut Effects) {
        // Still set: no speech came in time, so the key was a shortcut.
        let due = |t: Option<Millis>| t.is_some_and(|t| now >= t);
        if self.rec.as_ref().is_some_and(|r| due(r.cancel_at)) {
            self.cancel_recording(now, false, fx);
        }
        if let Some(r) = self.rec.as_ref() {
            if now >= self.max_at(r) || due(r.tail_until) {
                // Never discard a long recording; the cap stops it without a tail.
                self.stop_recording(now, false, fx);
            } else {
                let warn = !r.warned && now >= self.warn_at(r);
                let poll = r.next_poll.is_some_and(|t| now >= t);
                if let Some(r) = self.rec.as_mut() {
                    r.warned |= warn;
                    if poll {
                        r.next_poll = Some(now + self.cfg.watchdog_period);
                        fx.push(Effect::PollChord);
                    }
                }
            }
        }
        if self.notice.is_some_and(|(_, t)| now >= t) {
            self.notice = None;
        }
        if self.idle_unload_at.is_some_and(|t| now >= t) && self.unloadable() {
            self.unload(fx);
        }
    }

    fn unloadable(&self) -> bool {
        self.phase() == Phase::Idle && self.model == ModelStatus::Loaded
    }

    fn unload(&mut self, fx: &mut Effects) {
        self.model = ModelStatus::Unloaded;
        self.model_for = None;
        self.idle_unload_at = None;
        self.unload_when_idle = false;
        fx.push(Effect::Unload);
    }

    /// Hand out work: one job to whisper (oldest first, model permitting), the oldest
    /// transcribed job to paste. Jobs never overtake each other.
    fn dispatch(&mut self, fx: &mut Effects) {
        while !self.jobs.iter().any(|j| j.stage == Stage::Transcribing) {
            let next = self
                .jobs
                .iter()
                .position(|j| matches!(j.stage, Stage::AwaitingAudio | Stage::Queued));
            let Some(i) = next.filter(|&i| self.jobs[i].stage == Stage::Queued) else {
                break;
            };
            match self.model {
                ModelStatus::Loaded => {
                    self.jobs[i].stage = Stage::Transcribing;
                    fx.push(Effect::Transcribe(self.jobs[i].id));
                    break;
                }
                ModelStatus::Unloaded => {
                    self.preload(fx);
                    break;
                }
                ModelStatus::Loading => break,
                ModelStatus::Failed(_) => {
                    // The failure was announced once; the audio has nowhere to go.
                    if let Some(job) = self.jobs.remove(i) {
                        fx.push(Effect::DropJob(job.id));
                    }
                }
            }
        }
        if let Some(front) = self
            .jobs
            .front_mut()
            .filter(|j| j.stage == Stage::Transcribed)
        {
            front.stage = Stage::Pasting;
            fx.push(Effect::Paste(front.id));
        }
    }

    /// After every event: apply a waiting config, schedule the idle unload, update the overlay.
    fn settle(&mut self, now: Millis, fx: &mut Effects) {
        if self.phase() == Phase::Idle {
            if let Some(cfg) = self.pending_cfg.take() {
                self.cfg = cfg;
                self.idle_unload_at = None;
                fx.push(Effect::ApplyConfig);
                // A load in flight is judged when it answers, against the config of that time.
                match self.model {
                    ModelStatus::Failed(_) => self.model = ModelStatus::Unloaded, // may be fixed now
                    ModelStatus::Loaded if self.stale_load() => self.unload(fx),
                    _ => {}
                }
            }
            if self.unload_when_idle && self.model != ModelStatus::Loading {
                if self.model == ModelStatus::Loaded {
                    self.unload(fx);
                }
                self.unload_when_idle = false;
            } else if self.unloadable() && self.cfg.unload_immediately {
                self.unload(fx);
            } else if self.unloadable()
                && self.cfg.idle_unload != Millis(0)
                && self.idle_unload_at.is_none()
            {
                self.idle_unload_at = Some(now + self.cfg.idle_unload);
            }
        }
        if !self.unloadable() {
            self.idle_unload_at = None;
        }
        let overlay = self.overlay();
        if overlay != self.shown {
            self.shown = overlay;
            fx.push(Effect::Overlay(overlay));
        }
    }

    fn overlay(&self) -> OverlayState {
        if let Some(r) = &self.rec {
            return OverlayState::Recording {
                locked: r.kind == Kind::Locked,
                ending_soon: r.warned,
            };
        }
        if let Some((notice, _)) = self.notice {
            return OverlayState::Text(notice);
        }
        match self.phase() {
            Phase::Idle | Phase::Recording => OverlayState::Hidden,
            Phase::Pasting => OverlayState::Pasting,
            Phase::Transcribing if self.model == ModelStatus::Loading => OverlayState::LoadingModel,
            Phase::Transcribing => OverlayState::Transcribing,
        }
    }
}

fn model_notice(why: ModelFailure) -> Notice {
    match why {
        ModelFailure::Missing => Notice::NoModel,
        ModelFailure::Bad => Notice::ModelBad,
        ModelFailure::OutOfMemory => Notice::OutOfMemory,
    }
}

#[cfg(test)]
mod tests {
    use super::super::step;
    use super::*;

    // A model that did not fit in memory is not called damaged (t13 review).
    #[test]
    fn each_model_failure_has_its_own_notice() {
        assert_eq!(model_notice(ModelFailure::Missing), Notice::NoModel);
        assert_eq!(model_notice(ModelFailure::Bad), Notice::ModelBad);
        assert_eq!(model_notice(ModelFailure::OutOfMemory), Notice::OutOfMemory);
    }

    // Quit waits on `busy`: a cancelled job is Idle by phase, but its kept audio is still due.
    #[test]
    fn busy_lasts_until_a_cancelled_jobs_audio_is_back() {
        let mut s = State::new(MachineConfig::default());
        assert!(!s.busy());
        step(&mut s, Event::ModelLoaded, Millis(0));
        step(&mut s, Event::ChordDown, Millis(0));
        step(&mut s, Event::SpeechHeard, Millis(500));
        assert!(s.busy());
        let fx = step(&mut s, Event::Lock, Millis(1000));
        assert!(fx.contains(&Effect::StopCapture(JobId(0))), "{fx:?}");
        assert_eq!(s.phase(), Phase::Idle);
        assert!(s.busy(), "the cancelled job still waits for its audio");
        let fx = step(&mut s, Event::JobAudioReady(JobId(0)), Millis(1100));
        assert!(fx.contains(&Effect::KeepCancelled(JobId(0))), "{fx:?}");
        assert!(!s.busy());
    }
}
