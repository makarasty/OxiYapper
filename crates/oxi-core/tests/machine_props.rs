//! Invariants of the state machine over arbitrary event streams (ARCHITECTURE section 13).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use oxi_core::machine::{
    BOUNCE, Effect, EmptyAudio, Event, JobId, MachineConfig, Mode, ModelFailure, Phase, Sound,
    State, step,
};
use oxi_core::paste::{ClipboardOnlyWhy, PasteOutcome};
use oxi_core::types::Millis;
use proptest::prelude::*;

const JOB_IDS: u64 = 6;

fn job() -> impl Strategy<Value = JobId> {
    (0..JOB_IDS).prop_map(JobId)
}

fn outcome() -> impl Strategy<Value = PasteOutcome> {
    prop_oneof![
        Just(PasteOutcome::Pasted),
        Just(PasteOutcome::Typed),
        Just(PasteOutcome::ClipboardOnly(ClipboardOnlyWhy::ModifiersHeld)),
        Just(PasteOutcome::ClipboardOnly(ClipboardOnlyWhy::Elevated)),
        Just(PasteOutcome::PastedNoRestore),
        Just(PasteOutcome::Failed),
    ]
}

fn config() -> impl Strategy<Value = MachineConfig> {
    (
        prop_oneof![Just(Mode::Push), Just(Mode::Toggle)],
        prop_oneof![Just("turbo"), Just("tiny")],
    )
        .prop_map(|(mode, file)| {
            let mut cfg = MachineConfig {
                mode,
                esc_cancels: true,
                ..MachineConfig::default()
            };
            cfg.model.file = file.into();
            cfg
        })
}

/// Every event except `ChordDown`, which is the only way to start a recording.
fn quiet_event() -> impl Strategy<Value = Event> {
    prop_oneof![
        3 => Just(Event::ChordUp),
        2 => any::<bool>().prop_map(|held| Event::ChordPolled { held }),
        2 => Just(Event::Esc),
        1 => Just(Event::OtherKey),
        1 => Just(Event::SpeechHeard),
        1 => Just(Event::DeviceLost),
        1 => Just(Event::CaptureFailed),
        1 => Just(Event::NoMicrophone),
        4 => job().prop_map(Event::JobAudioReady),
        2 => (job(), prop_oneof![
            Just(EmptyAudio::NothingHeard),
            Just(EmptyAudio::MicSilence),
            Just(EmptyAudio::Failed),
        ]).prop_map(|(job, why)| Event::JobAudioEmpty { job, why }),
        4 => (job(), any::<bool>()).prop_map(|(job, empty)| Event::TranscriptionDone { job, empty }),
        1 => job().prop_map(Event::TranscriptionFailed),
        4 => (job(), outcome()).prop_map(|(job, outcome)| Event::PasteDone { job, outcome }),
        1 => Just(Event::MicFellBack),
        2 => Just(Event::ModelLoaded),
        1 => prop_oneof![Just(ModelFailure::Missing), Just(ModelFailure::Bad)].prop_map(Event::ModelFailed),
        1 => config().prop_map(Event::ConfigChanged),
        1 => Just(Event::Suspend),
        1 => Just(Event::Resume),
        1 => Just(Event::Lock),
        1 => Just(Event::Unlock),
        3 => Just(Event::Tick),
    ]
}

fn event() -> impl Strategy<Value = Event> {
    prop_oneof![5 => Just(Event::ChordDown), 20 => quiet_event()]
}

/// Mostly short gaps (taps, double taps), sometimes minutes (max recording, idle unload).
fn delay() -> impl Strategy<Value = u64> {
    prop_oneof![8 => 0..400u64, 1 => 0..700_000u64]
}

fn script(events: impl Strategy<Value = Event>) -> impl Strategy<Value = Vec<(Event, u64)>> {
    prop::collection::vec((events, delay()), 0..60)
}

/// What the user's hand does; the hook may lose a key-up.
#[derive(Debug, Clone, Copy)]
enum Key {
    Press,
    Release { delivered: bool },
    Other,
    Esc,
}

fn keyboard() -> impl Strategy<Value = Vec<(Key, u64)>> {
    let key = prop_oneof![
        3 => Just(Key::Press),
        3 => any::<bool>().prop_map(|delivered| Key::Release { delivered }),
        3 => Just(Key::Other),
        1 => Just(Key::Esc),
    ];
    let delay = prop_oneof![8 => 0..400u64, 1 => 0..5_000u64];
    prop::collection::vec((key, delay), 0..40)
}

/// The machine plus a physical chord key and a coordinator that fires every deadline on
/// time and answers each `PollChord` with the key's real state.
struct Keyboard {
    state: State,
    now: u64,
    down: bool,
    released_at: Option<u64>,
    /// Every effect so far, the answered polls' too.
    seen: Vec<Effect>,
}

impl Keyboard {
    fn new(cfg: MachineConfig) -> Self {
        let mut state = State::new(cfg);
        step(&mut state, Event::ModelLoaded, Millis(0));
        Self {
            state,
            now: 0,
            down: false,
            released_at: None,
            seen: Vec::new(),
        }
    }

    fn send(&mut self, event: Event) -> Vec<Effect> {
        let fx = step(&mut self.state, event, Millis(self.now));
        self.seen.extend(fx.iter().cloned());
        if fx.contains(&Effect::PollChord) {
            let held = self.down;
            let polled = step(
                &mut self.state,
                Event::ChordPolled { held },
                Millis(self.now),
            );
            self.seen.extend(polled);
        }
        fx
    }

    /// Move the clock forward, firing every deadline on the way.
    fn advance(&mut self, dt: u64) {
        let until = self.now + dt;
        while let Some(t) = self.state.next_deadline().filter(|t| t.0 <= until) {
            self.now = self.now.max(t.0);
            self.send(Event::Tick);
        }
        self.now = until;
    }
}

/// Keeps what the workers would know about the jobs the machine handed out.
#[derive(Default)]
struct Workers {
    issued: BTreeSet<JobId>,
    /// First audio answer of each issued job: true when it had audio.
    audio: BTreeMap<JobId, bool>,
    pasted: Vec<JobId>,
    transcribed: Vec<JobId>,
    whisper_busy: BTreeSet<JobId>,
    paste_busy: BTreeSet<JobId>,
    /// Jobs a longer dictation took in (`Merge`): they never paste.
    merged: BTreeSet<JobId>,
    /// An `Unload` was sent and no `Preload` or `ModelLoaded` came after it.
    unloaded: bool,
}

impl Workers {
    /// Feed the event the way the coordinator would see it, then check the effects.
    fn observe(&mut self, event: &Event, effects: &[Effect]) {
        match *event {
            Event::JobAudioReady(job) if self.issued.contains(&job) => {
                self.audio.entry(job).or_insert(true);
            }
            Event::JobAudioEmpty { job, .. } if self.issued.contains(&job) => {
                self.audio.entry(job).or_insert(false);
            }
            Event::TranscriptionDone { job, .. } | Event::TranscriptionFailed(job) => {
                self.whisper_busy.remove(&job);
            }
            Event::PasteDone { job, .. } => {
                self.paste_busy.remove(&job);
            }
            Event::ModelLoaded => self.unloaded = false,
            _ => {}
        }
        for effect in effects {
            match *effect {
                Effect::Preload => self.unloaded = false,
                Effect::Unload => {
                    assert!(self.whisper_busy.is_empty(), "unload under a transcription");
                    assert!(!self.unloaded, "two unloads without a load between");
                    self.unloaded = true;
                }
                Effect::StopCapture(id) => {
                    assert!(self.issued.insert(id), "job {id:?} issued twice");
                }
                Effect::Supersede(id) => {
                    assert!(
                        !self.pasted.contains(&id),
                        "{id:?} carried on after its paste"
                    );
                }
                Effect::Merge { ref from, into } => {
                    assert!(from.iter().all(|f| *f < into), "merged into an older job");
                    for f in from {
                        assert!(!self.pasted.contains(f), "{f:?} merged after its paste");
                        // The coordinator drops a merged job's whisper result.
                        self.whisper_busy.remove(f);
                        self.merged.insert(*f);
                    }
                }
                Effect::Transcribe(id) => {
                    assert!(self.issued.contains(&id), "{id:?} was never recorded");
                    assert!(!self.transcribed.contains(&id), "{id:?} transcribed twice");
                    assert_eq!(
                        self.audio.get(&id),
                        Some(&true),
                        "{id:?} sent without audio"
                    );
                    self.transcribed.push(id);
                    self.whisper_busy.insert(id);
                    assert!(self.whisper_busy.len() <= 1, "two whisper jobs in flight");
                }
                Effect::Paste(id) => {
                    assert!(self.issued.contains(&id), "{id:?} was never recorded");
                    assert!(
                        self.transcribed.contains(&id),
                        "{id:?} pasted untranscribed"
                    );
                    assert!(!self.pasted.contains(&id), "{id:?} pasted twice");
                    assert!(!self.merged.contains(&id), "{id:?} pasted after a merge");
                    self.pasted.push(id);
                    self.paste_busy.insert(id);
                    assert!(self.paste_busy.len() <= 1, "two pastes at once");
                }
                _ => {}
            }
        }
        assert!(self.pasted.is_sorted(), "pasted out of order");
        assert!(self.transcribed.is_sorted(), "transcribed out of order");
    }
}

/// Steps `event` and answers every effect the way the workers would, at once: audio for a
/// stop, a text for whisper, a done paste, a load.
fn answered(state: &mut State, event: Event, now: Millis) -> Vec<Effect> {
    let mut all = Vec::new();
    let mut queue = VecDeque::from([event]);
    while let Some(event) = queue.pop_front() {
        for effect in step(state, event, now) {
            queue.extend(match effect {
                Effect::StopCapture(job) => Some(Event::JobAudioReady(job)),
                Effect::Transcribe(job) => Some(Event::TranscriptionDone { job, empty: false }),
                Effect::Paste(job) => Some(Event::PasteDone {
                    job,
                    outcome: PasteOutcome::Pasted,
                }),
                Effect::Preload => Some(Event::ModelLoaded),
                _ => None,
            });
            all.push(effect);
        }
    }
    all
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    /// One whisper job at a time, every job transcribed and pasted at most once and in
    /// order and only with audio, no unload under a transcription and no second unload without a load between,
    /// whatever the workers and the keyboard do.
    #[test]
    fn jobs_are_serial_ordered_and_unique(cfg in config(), steps in script(event())) {
        let mut state = State::new(cfg);
        let mut workers = Workers::default();
        let mut now = 0;
        for (event, dt) in steps {
            now += dt;
            let fx = step(&mut state, event.clone(), Millis(now));
            workers.observe(&event, &fx);
            for effect in &fx {
                if let Effect::Paste(job) = *effect {
                    let hold = state.hold_until(job);
                    prop_assert!(hold.is_none_or(|t| now >= t.0), "{job:?} pasted before {hold:?}");
                }
            }
        }
    }

    /// Once the events stop, the workers answer and the deadlines run out (a job waiting out
    /// its hold gap pastes at its end), the machine is Idle with no job left behind.
    #[test]
    fn drains_back_to_idle(cfg in config(), steps in script(event())) {
        let mut state = State::new(cfg);
        let mut now = 0;
        let mut issued = 0;
        let note = |fx: &[Effect], issued: &mut u64| {
            for e in fx {
                if let Effect::StopCapture(JobId(n)) = *e {
                    *issued = (*issued).max(n + 1);
                }
            }
        };
        for (event, dt) in steps {
            now += dt;
            note(&step(&mut state, event, Millis(now)), &mut issued);
        }

        note(&answered(&mut state, Event::Esc, Millis(now)), &mut issued);
        for _ in 0..3 {
            answered(&mut state, Event::ModelLoaded, Millis(now));
            for n in 0..issued {
                let job = JobId(n);
                answered(&mut state, Event::JobAudioReady(job), Millis(now));
                answered(&mut state, Event::TranscriptionDone { job, empty: false }, Millis(now));
                let outcome = PasteOutcome::Pasted;
                answered(&mut state, Event::PasteDone { job, outcome }, Millis(now));
            }
        }
        let mut ticks = 0;
        while let Some(deadline) = state.next_deadline() {
            now = now.max(deadline.0);
            answered(&mut state, Event::Tick, Millis(now));
            ticks += 1;
            prop_assert!(ticks < 10_000, "deadlines never drain");
        }
        prop_assert_eq!(state.phase(), Phase::Idle);
        prop_assert!(!state.busy(), "a job was left behind");
    }

    /// A model that fails to load for the job in whisper (the worker answers `ModelFailed`,
    /// then `TranscriptionFailed`) gives one error sound and one notice, whatever waits behind
    /// it, and every job ends.
    #[test]
    fn one_failed_load_gives_at_most_one_error_notice(
        jobs in 1..=3u64,
        why in prop_oneof![Just(ModelFailure::Missing), Just(ModelFailure::Bad)],
    ) {
        let mut state = State::new(MachineConfig::default());
        step(&mut state, Event::ModelLoaded, Millis(0));
        let mut now = 0;
        for n in 0..jobs {
            step(&mut state, Event::ChordDown, Millis(now));
            step(&mut state, Event::ChordUp, Millis(now + 1000));
            now += 3000; // past the release tail
            step(&mut state, Event::Tick, Millis(now));
            step(&mut state, Event::JobAudioReady(JobId(n)), Millis(now));
        }
        let mut fx = step(&mut state, Event::ModelFailed(why), Millis(now));
        fx.extend(step(&mut state, Event::TranscriptionFailed(JobId(0)), Millis(now)));
        let errors = fx.iter().filter(|e| **e == Effect::Play(Sound::Error)).count();
        let notices = fx.iter().filter(|e| matches!(e, Effect::Notify(_))).count();
        prop_assert_eq!((errors, notices), (1, 1), "{:?}", fx);
        prop_assert_eq!(state.phase(), Phase::Idle);
    }

    /// Typing after the chord is physically released never throws a recording away, in
    /// either mode, even when the hook lost the key-up: one poll later the machine knows.
    #[test]
    fn other_key_after_release_never_discards(cfg in config(), acts in keyboard()) {
        let period = cfg.watchdog_period.0;
        let mut kb = Keyboard::new(cfg);
        for (act, dt) in acts {
            kb.advance(dt);
            match act {
                Key::Press if !kb.down => {
                    kb.down = true;
                    kb.released_at = None;
                    kb.send(Event::ChordDown);
                }
                Key::Release { delivered } if kb.down => {
                    kb.down = false;
                    kb.released_at = Some(kb.now);
                    if delivered {
                        kb.send(Event::ChordUp);
                    }
                }
                Key::Other => {
                    let recording = kb.state.phase() == Phase::Recording;
                    let settled = kb.released_at.is_some_and(|t| kb.now >= t + period);
                    let fx = kb.send(Event::OtherKey);
                    if recording && settled {
                        prop_assert!(!fx.contains(&Effect::DiscardCapture), "{fx:?}");
                        prop_assert_eq!(kb.state.phase(), Phase::Recording);
                    }
                }
                Key::Esc => {
                    kb.send(Event::Esc);
                }
                Key::Press | Key::Release { .. } => {}
            }
        }
    }

    /// A recording cancelled by Esc never reaches whisper or the paste worker: after it,
    /// with no new key-down, nothing is ever handed out.
    #[test]
    fn esc_cancelled_recording_is_never_pasted(
        cfg in config(),
        held_ms in 0..5_000u64,
        tail in script(quiet_event()),
    ) {
        let mut state = State::new(cfg);
        step(&mut state, Event::ModelLoaded, Millis(0));
        step(&mut state, Event::ChordDown, Millis(10));
        prop_assert_eq!(state.phase(), Phase::Recording);
        let mut now = 10 + held_ms;
        let fx = step(&mut state, Event::Esc, Millis(now));
        prop_assert!(fx.contains(&Effect::DiscardCapture));
        prop_assert_eq!(state.phase(), Phase::Idle);

        for (event, dt) in tail {
            now += dt;
            for effect in step(&mut state, event, Millis(now)) {
                prop_assert!(
                    !matches!(
                        effect,
                        Effect::StopCapture(_) | Effect::Transcribe(_) | Effect::Paste(_)
                    ),
                    "{effect:?} after Esc"
                );
            }
        }
    }

    /// Held keys mean dictation (owner, 2026-11-12): a held push chord that bounces (up and
    /// down again within `BOUNCE`) any number of times, early (a tap so far) or late, with or
    /// without a tail or a hold gap, is one capture and one job. In toggle mode the start and
    /// the stop press bounce the same way: one capture, one job, and the microphone ends off.
    #[test]
    fn a_bouncing_chord_is_one_dictation(
        toggle in any::<bool>(),
        tail in 0..300u64,
        gap in prop_oneof![Just(0u64), 100..1500u64],
        start in prop::collection::vec((1..2000u64, 0..BOUNCE.0), 0..8),
        stop in prop::collection::vec((1..2000u64, 0..BOUNCE.0), 0..8),
        last in 300..5000u64,
    ) {
        let mut kb = Keyboard::new(MachineConfig {
            mode: if toggle { Mode::Toggle } else { Mode::Push },
            release_tail: Millis(tail),
            hold_gap: Millis(gap),
            ..MachineConfig::default()
        });
        let press = |kb: &mut Keyboard, bounces: Vec<(u64, u64)>, held: u64| {
            kb.down = true;
            kb.send(Event::ChordDown);
            for (held, off) in bounces {
                kb.advance(held);
                kb.down = false;
                kb.send(Event::ChordUp);
                kb.advance(off);
                kb.down = true;
                kb.send(Event::ChordDown);
            }
            kb.advance(held);
            kb.down = false;
            kb.send(Event::ChordUp);
        };
        if toggle {
            press(&mut kb, start, 50);
            kb.advance(last);
            press(&mut kb, stop, 50);
        } else {
            press(&mut kb, start, last);
        }
        kb.advance(10_000);
        let starts = kb.seen.iter().filter(|e| **e == Effect::StartCapture).count();
        let stops: Vec<_> = kb.seen.iter().filter(|e| matches!(e, Effect::StopCapture(_))).collect();
        prop_assert_eq!(starts, 1, "{:?}", kb.seen);
        prop_assert_eq!(stops, [&Effect::StopCapture(JobId(0))], "{:?}", kb.seen);
        prop_assert!(!kb.seen.contains(&Effect::DiscardCapture), "{:?}", kb.seen);
        prop_assert_ne!(kb.state.phase(), Phase::Recording);
    }
}
