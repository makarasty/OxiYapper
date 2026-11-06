//! Invariants of the state machine over arbitrary event streams (ARCHITECTURE section 13).

use std::collections::{BTreeMap, BTreeSet};

use oxi_core::machine::{
    Effect, EmptyAudio, Event, JobId, MachineConfig, Mode, ModelFailure, Phase, Sound, State, step,
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
        1 => Just(Event::DeviceLost),
        1 => Just(Event::CaptureFailed),
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
        }
    }

    fn send(&mut self, event: Event) -> Vec<Effect> {
        let fx = step(&mut self.state, event, Millis(self.now));
        if fx.contains(&Effect::PollChord) {
            let held = self.down;
            step(
                &mut self.state,
                Event::ChordPolled { held },
                Millis(self.now),
            );
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
        }
    }

    /// Once the events stop, the workers answer and the deadlines run out, the machine is
    /// Idle with nothing armed.
    #[test]
    fn drains_back_to_idle(cfg in config(), steps in script(event())) {
        let mut state = State::new(cfg);
        let mut now = 0;
        let mut issued = 0;
        for (event, dt) in steps {
            now += dt;
            for e in step(&mut state, event, Millis(now)) {
                if let Effect::StopCapture(JobId(n)) = e {
                    issued = issued.max(n + 1);
                }
            }
        }

        step(&mut state, Event::Esc, Millis(now));
        for _ in 0..3 {
            step(&mut state, Event::ModelLoaded, Millis(now));
            for n in 0..issued {
                let job = JobId(n);
                step(&mut state, Event::JobAudioReady(job), Millis(now));
                step(&mut state, Event::TranscriptionDone { job, empty: false }, Millis(now));
                let outcome = PasteOutcome::Pasted;
                step(&mut state, Event::PasteDone { job, outcome }, Millis(now));
            }
        }
        let mut ticks = 0;
        while let Some(deadline) = state.next_deadline() {
            now = now.max(deadline.0);
            step(&mut state, Event::Tick, Millis(now));
            ticks += 1;
            prop_assert!(ticks < 10_000, "deadlines never drain");
        }
        prop_assert_eq!(state.phase(), Phase::Idle);
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
}
