//! Invariants of the state machine over arbitrary event streams (ARCHITECTURE section 13).

use std::collections::BTreeSet;

use oxi_core::machine::{
    Effect, Event, JobId, MachineConfig, Mode, ModelFailure, Phase, State, step,
};
use oxi_core::types::Millis;
use proptest::prelude::*;

const JOB_IDS: u64 = 6;

fn job() -> impl Strategy<Value = JobId> {
    (0..JOB_IDS).prop_map(JobId)
}

fn config() -> impl Strategy<Value = MachineConfig> {
    prop_oneof![Just(Mode::Push), Just(Mode::Toggle)].prop_map(|mode| MachineConfig {
        mode,
        ..MachineConfig::default()
    })
}

/// Every event except `ChordDown`, which is the only way to start a recording.
fn quiet_event() -> impl Strategy<Value = Event> {
    prop_oneof![
        3 => Just(Event::ChordUp),
        2 => any::<bool>().prop_map(|held| Event::ChordPolled { held }),
        2 => Just(Event::Esc),
        1 => Just(Event::OtherKey),
        1 => Just(Event::AudioStopped),
        1 => Just(Event::AudioFailed),
        4 => job().prop_map(Event::JobAudioReady),
        4 => (job(), any::<bool>()).prop_map(|(job, empty)| Event::TranscriptionDone { job, empty }),
        1 => job().prop_map(Event::TranscriptionFailed),
        4 => job().prop_map(Event::PasteDone),
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

/// Keeps what the workers would know about the jobs the machine handed out.
#[derive(Default)]
struct Workers {
    issued: BTreeSet<JobId>,
    pasted: Vec<JobId>,
    transcribed: Vec<JobId>,
    whisper_busy: BTreeSet<JobId>,
    paste_busy: BTreeSet<JobId>,
}

impl Workers {
    /// Feed the event the way the coordinator would see it, then check the effects.
    fn observe(&mut self, event: Event, effects: &[Effect]) {
        match event {
            Event::TranscriptionDone { job, .. } | Event::TranscriptionFailed(job) => {
                self.whisper_busy.remove(&job);
            }
            Event::PasteDone(job) => {
                self.paste_busy.remove(&job);
            }
            _ => {}
        }
        for effect in effects {
            match *effect {
                Effect::StopCapture(id) => {
                    assert!(self.issued.insert(id), "job {id:?} issued twice");
                }
                Effect::Transcribe(id) => {
                    assert!(self.issued.contains(&id), "{id:?} was never recorded");
                    assert!(!self.transcribed.contains(&id), "{id:?} transcribed twice");
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
    /// order, whatever the workers and the keyboard do.
    #[test]
    fn jobs_are_serial_ordered_and_unique(cfg in config(), steps in script(event())) {
        let mut state = State::new(cfg);
        let mut workers = Workers::default();
        let mut now = 0;
        for (event, dt) in steps {
            now += dt;
            let fx = step(&mut state, event, Millis(now));
            workers.observe(event, &fx);
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
                step(&mut state, Event::PasteDone(job), Millis(now));
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

    /// Typing after the chord is released never throws a recording away, in either mode:
    /// only a key pressed while the chord is held cancels.
    #[test]
    fn other_key_after_release_never_discards(cfg in config(), steps in script(event())) {
        let mut state = State::new(cfg);
        let mut now = 0;
        let mut released = false;
        for (event, dt) in steps {
            now += dt;
            let recording = state.phase() == Phase::Recording;
            let fx = step(&mut state, event, Millis(now));
            match event {
                Event::ChordDown => released = false,
                Event::ChordUp | Event::ChordPolled { held: false } => released = true,
                Event::OtherKey if released && recording => {
                    prop_assert!(!fx.contains(&Effect::DiscardCapture), "{fx:?}");
                    prop_assert_eq!(state.phase(), Phase::Recording);
                }
                _ => {}
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
