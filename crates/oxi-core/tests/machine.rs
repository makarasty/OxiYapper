//! Table tests for ARCHITECTURE section 11 items 1-12 (and the few neighbours they touch),
//! with time driven by hand.

use oxi_core::config::Config;
use oxi_core::device::DeviceChoice;
use oxi_core::machine::{
    Effect, Effects, Event, JobId, MachineConfig, Mode, ModelFailure, Notice, OverlayState, Phase,
    PhaseCell, Sound, State, step,
};
use oxi_core::paste::{ClipboardOnlyWhy, PasteOutcome};
use oxi_core::types::Millis;

const REC: OverlayState = OverlayState::Recording {
    locked: false,
    ending_soon: false,
};
const LOCKED: OverlayState = OverlayState::Recording {
    locked: true,
    ending_soon: false,
};

/// SPEC defaults without the release tail, so a stop shows in the event that causes it.
/// The tail has its own tests below.
fn untailed() -> MachineConfig {
    MachineConfig {
        release_tail: Millis(0),
        ..MachineConfig::default()
    }
}

struct Sim(State);

impl Sim {
    /// Push mode, model already loaded.
    fn loaded() -> Self {
        Self::with(untailed())
    }

    fn toggle() -> Self {
        Self::with(MachineConfig {
            mode: Mode::Toggle,
            ..untailed()
        })
    }

    fn with(cfg: MachineConfig) -> Self {
        let mut sim = Self(State::new(cfg));
        assert_eq!(sim.at(0, Event::ModelLoaded), Effects::new());
        sim
    }

    fn at(&mut self, ms: u64, event: Event) -> Effects {
        step(&mut self.0, event, Millis(ms))
    }

    /// Effects without the overlay updates, which tests check where they matter.
    fn core(&mut self, ms: u64, event: Event) -> Effects {
        without_overlay(self.at(ms, event))
    }

    fn phase(&self) -> Phase {
        self.0.phase()
    }

    /// Press at `down`, release at `up`; returns the effects of the release.
    fn press(&mut self, down: u64, up: u64) -> Effects {
        self.at(down, Event::ChordDown);
        self.at(up, Event::ChordUp)
    }
}

fn pasted(job: JobId) -> Event {
    Event::PasteDone {
        job,
        outcome: PasteOutcome::Pasted,
    }
}

fn without_overlay(fx: Effects) -> Effects {
    fx.into_iter()
        .filter(|e| !matches!(e, Effect::Overlay(_)))
        .collect()
}

fn overlay_of(fx: &[Effect]) -> Option<OverlayState> {
    fx.iter().find_map(|e| match e {
        Effect::Overlay(o) => Some(*o),
        _ => None,
    })
}

const START: [Effect; 2] = [Effect::StartCapture, Effect::Play(Sound::Start)];
const J0: JobId = JobId(0);
const J1: JobId = JobId(1);
const J2: JobId = JobId(2);

fn stop(job: JobId) -> [Effect; 2] {
    [Effect::StopCapture(job), Effect::Play(Sound::Stop)]
}

fn done(job: JobId) -> Event {
    Event::TranscriptionDone { job, empty: false }
}

// ---- 1. hotkey during transcription or paste ------------------------------------------

#[test]
fn edge_01_key_during_transcription_starts_a_new_recording() {
    let mut s = Sim::loaded();
    assert_eq!(s.core(0, Event::ChordDown), START);
    assert_eq!(s.core(400, Event::ChordUp), stop(J0));
    assert_eq!(
        s.core(450, Event::JobAudioReady(J0)),
        [Effect::Transcribe(J0)]
    );
    assert_eq!(s.phase(), Phase::Transcribing);

    let fx = s.at(500, Event::ChordDown);
    assert_eq!(without_overlay(fx.clone()), START);
    assert_eq!(overlay_of(&fx), Some(REC));
    assert_eq!(s.phase(), Phase::Recording);
}

#[test]
fn edge_01_key_during_paste_starts_a_new_recording() {
    let mut s = Sim::loaded();
    s.press(0, 400);
    s.at(450, Event::JobAudioReady(J0));
    assert_eq!(s.core(500, done(J0)), [Effect::Paste(J0)]);
    assert_eq!(s.phase(), Phase::Pasting);
    assert_eq!(s.core(600, Event::ChordDown), START);
    assert_eq!(s.phase(), Phase::Recording);
}

#[test]
fn edge_01_jobs_run_and_paste_in_order_and_a_third_waiting_job_is_refused() {
    let mut s = Sim::loaded();
    s.press(0, 400); // job 0 goes to whisper
    s.at(450, Event::JobAudioReady(J0));
    s.press(500, 1000); // job 1 waits for whisper
    assert_eq!(s.core(1050, Event::JobAudioReady(J1)), []);

    // One job waits, the one in whisper does not count: a new recording starts.
    assert_eq!(s.core(1100, Event::ChordDown), START);
    assert_eq!(s.core(1500, Event::ChordUp), stop(J2));
    assert_eq!(s.core(1550, Event::JobAudioReady(J2)), []);

    // Two jobs wait: the next key-down is refused with the error sound, nothing else.
    assert_eq!(s.at(1600, Event::ChordDown), [Effect::Play(Sound::Error)]);
    assert_eq!(s.core(1700, Event::ChordUp), []);
    assert_eq!(s.phase(), Phase::Transcribing);

    // Job 0 finishes: job 1 goes to whisper, job 0 to paste.
    assert_eq!(
        s.core(1800, done(J0)),
        [Effect::Transcribe(J1), Effect::Paste(J0)]
    );
    // Job 1 is transcribed while job 0 still pastes: it waits for its turn.
    assert_eq!(s.core(1900, done(J1)), [Effect::Transcribe(J2)]);
    assert_eq!(s.core(2000, pasted(J0)), [Effect::Paste(J1)]);
    assert_eq!(s.core(2100, done(J2)), []);
    assert_eq!(s.core(2200, pasted(J1)), [Effect::Paste(J2)]);
    s.at(2300, pasted(J2));
    assert_eq!(s.phase(), Phase::Idle);

    // The queue has room again.
    assert_eq!(s.core(2400, Event::ChordDown), START);
}

#[test]
fn edge_01_jobs_in_whisper_and_paste_are_not_waiting() {
    let mut s = Sim::loaded();
    s.press(0, 400);
    s.at(450, Event::JobAudioReady(J0));
    s.press(500, 1000);
    s.at(1050, Event::JobAudioReady(J1));
    assert_eq!(
        s.core(1200, done(J0)),
        [Effect::Transcribe(J1), Effect::Paste(J0)]
    );
    // Job 0 pastes, job 1 is in whisper: nothing waits, the press records.
    assert_eq!(s.core(1250, Event::ChordDown), START);
    assert_eq!(s.phase(), Phase::Recording);
}

// ---- 2. double tap -------------------------------------------------------------------------

#[test]
fn edge_02_double_tap_locks_recording_on() {
    // (first press ms, gap ms, second press ms, locked)
    let table = [
        (100, 200, 100, true),
        (249, 299, 249, true),
        (250, 100, 100, false), // first press too long to count
        (100, 300, 100, false), // gap must be under 300 ms
        (100, 301, 100, false), // gap too long
        (100, 200, 250, false), // second press too long
        (100, 200, 400, false), // second press is a normal push recording
    ];
    for (p1, gap, p2, locked) in table {
        let mut s = Sim::loaded();
        let t2 = 1000 + p1 + gap;
        s.press(1000, 1000 + p1);
        s.at(t2, Event::ChordDown);
        let fx = s.at(t2 + p2, Event::ChordUp);
        let case = format!("press {p1}, gap {gap}, press {p2}");
        if locked {
            assert_eq!(s.phase(), Phase::Recording, "{case}");
            assert_eq!(overlay_of(&fx), Some(LOCKED), "{case}");
            assert_eq!(without_overlay(fx), [], "{case}");
        } else if p2 >= 300 {
            assert_eq!(s.phase(), Phase::Transcribing, "{case}");
        } else {
            assert_eq!(s.phase(), Phase::Idle, "{case}");
        }
    }
}

#[test]
fn edge_02_locked_audio_counts_from_the_second_press_and_next_press_stops_it() {
    let mut s = Sim::loaded();
    assert_eq!(s.core(1000, Event::ChordDown), START);
    // The first tap's capture is thrown away; the second press opens a fresh one.
    assert_eq!(s.core(1100, Event::ChordUp), [Effect::DiscardCapture]);
    assert_eq!(s.core(1200, Event::ChordDown), START);
    assert_eq!(s.core(1300, Event::ChordUp), []);
    // Locked recording does not poll the key and ignores other keys.
    assert_eq!(s.core(2000, Event::Tick), []);
    assert_eq!(s.core(2100, Event::OtherKey), []);
    assert_eq!(s.phase(), Phase::Recording);
    // The next full press stops it, its release is ignored.
    assert_eq!(s.core(5000, Event::ChordDown), stop(J0));
    assert_eq!(s.core(5200, Event::ChordUp), []);
    assert_eq!(s.phase(), Phase::Transcribing);
}

// ---- 3. tap ---------------------------------------------------------------------------------

#[test]
fn edge_03_press_under_300_ms_is_discarded() {
    // (press ms, kept as a job)
    let table = [
        (0, false),
        (100, false),
        (299, false),
        (300, true),
        (900, true),
    ];
    for (ms, kept) in table {
        let mut s = Sim::loaded();
        let fx = s.press(1000, 1000 + ms);
        let case = format!("press {ms} ms");
        if kept {
            assert_eq!(without_overlay(fx), stop(J0), "{case}");
            assert_eq!(s.phase(), Phase::Transcribing, "{case}");
        } else {
            assert_eq!(without_overlay(fx), [Effect::DiscardCapture], "{case}");
            assert_eq!(s.phase(), Phase::Idle, "{case}");
            assert_eq!(
                overlay_of(&s.at(1000 + ms + 1, Event::Tick)),
                None,
                "{case}"
            );
        }
    }
}

// ---- 4. toggle mode -------------------------------------------------------------------------

#[test]
fn edge_04_toggle_ignores_key_up_and_has_no_double_tap() {
    let mut s = Sim::toggle();
    assert_eq!(s.core(1000, Event::ChordDown), START);
    assert_eq!(s.core(1050, Event::ChordUp), []); // key-up ignored
    assert_eq!(s.phase(), Phase::Recording);
    // Second press 200 ms after the first: under 300 ms of audio, discarded.
    assert_eq!(s.core(1200, Event::ChordDown), [Effect::DiscardCapture]);
    assert_eq!(s.core(1250, Event::ChordUp), []);
    assert_eq!(s.phase(), Phase::Idle);
    // No lock after a quick pair: the next press is a plain toggle start, polled only
    // while the chord is held.
    assert_eq!(s.core(1300, Event::ChordDown), START);
    assert_eq!(s.core(1350, Event::ChordUp), []);
    assert_eq!(s.core(1400, Event::Tick), []);
    assert_eq!(s.phase(), Phase::Recording);
}

#[test]
fn edge_04_toggle_stops_on_second_press_at_300_ms() {
    for (gap, kept) in [(299, false), (300, true), (5000, true)] {
        let mut s = Sim::toggle();
        s.at(1000, Event::ChordDown);
        let fx = without_overlay(s.at(1000 + gap, Event::ChordDown));
        if kept {
            assert_eq!(fx, stop(J0), "gap {gap}");
        } else {
            assert_eq!(fx, [Effect::DiscardCapture], "gap {gap}");
        }
    }
}

// ---- 5. max recording -----------------------------------------------------------------------

#[test]
fn edge_05_warns_30_s_before_the_cap_then_stops_and_keeps_the_audio() {
    let mut s = Sim::toggle();
    s.at(1000, Event::ChordDown);
    s.at(1050, Event::ChordUp);
    assert_eq!(s.0.next_deadline(), Some(Millis(1000 + 570_000)));

    assert_eq!(s.at(1000 + 569_999, Event::Tick), Effects::new());
    let fx = s.at(1000 + 570_000, Event::Tick);
    assert_eq!(
        fx,
        [Effect::Overlay(OverlayState::Recording {
            locked: false,
            ending_soon: true
        })]
    );
    assert_eq!(s.0.next_deadline(), Some(Millis(1000 + 600_000)));

    // At the cap: stop and transcribe, never discard.
    let fx = s.core(1000 + 600_000, Event::Tick);
    assert_eq!(fx, stop(J0));
    assert_eq!(s.phase(), Phase::Transcribing);
}

#[test]
fn edge_05_push_cap_stop_ignores_the_later_key_up() {
    let mut s = Sim::loaded();
    s.at(0, Event::ChordDown);
    // Polls come every 100 ms; jump straight to the cap.
    assert_eq!(s.core(600_000, Event::Tick), stop(J0));
    assert_eq!(s.core(600_500, Event::ChordUp), []);
    assert_eq!(s.phase(), Phase::Transcribing);
}

// ---- 6. Esc ---------------------------------------------------------------------------------

#[test]
fn edge_06_esc_cancels_a_recording_and_shows_it_for_a_moment() {
    let mut s = Sim::loaded();
    s.at(0, Event::ChordDown);
    let fx = s.at(500, Event::Esc);
    assert_eq!(
        fx,
        [
            Effect::DiscardCapture,
            Effect::Play(Sound::Cancel),
            Effect::Overlay(OverlayState::Text(Notice::Cancelled))
        ]
    );
    assert_eq!(s.phase(), Phase::Idle);
    // The key-up after the cancel does nothing, and the text goes away after 1.5 s.
    assert_eq!(s.core(600, Event::ChordUp), []);
    assert_eq!(s.0.next_deadline(), Some(Millis(2000)));
    assert_eq!(
        s.at(2000, Event::Tick),
        [Effect::Overlay(OverlayState::Hidden)]
    );
}

#[test]
fn edge_06_esc_when_not_recording_is_not_ours() {
    let mut s = Sim::loaded();
    assert_eq!(s.at(0, Event::Esc), []); // idle
    s.press(100, 600);
    s.at(650, Event::JobAudioReady(J0));
    assert_eq!(s.at(700, Event::Esc), []); // transcribing
    assert_eq!(s.phase(), Phase::Transcribing);
    s.at(800, done(J0));
    assert_eq!(s.at(850, Event::Esc), []); // pasting
    assert_eq!(s.phase(), Phase::Pasting);
}

// ---- 7. another key while the chord is held --------------------------------------------------

#[test]
fn edge_07_other_key_cancels_silently_in_both_modes() {
    for mode in [Mode::Push, Mode::Toggle] {
        let mut s = Sim::with(MachineConfig { mode, ..untailed() });
        s.at(0, Event::ChordDown);
        let fx = s.at(50, Event::OtherKey);
        // No sound, no notification, no overlay text: just gone.
        assert_eq!(
            fx,
            [
                Effect::DiscardCapture,
                Effect::Overlay(OverlayState::Hidden)
            ],
            "{mode:?}"
        );
        assert_eq!(s.core(900, Event::ChordUp), [], "{mode:?}");
        assert_eq!(s.phase(), Phase::Idle, "{mode:?}");
        // Only the idle unload is armed.
        assert_eq!(s.0.next_deadline(), Some(Millis(50 + 300_000)), "{mode:?}");
    }
}

#[test]
fn edge_07_08_lost_key_up_in_toggle_is_polled_and_typing_keeps_the_recording() {
    // (event, ms, effects without overlay): the hook never delivers the key-up (UAC).
    let table = [
        (Event::ChordDown, 0, START.to_vec()),
        (Event::Tick, 100, vec![Effect::PollChord]),
        (Event::ChordPolled { held: false }, 110, vec![]),
        (Event::Tick, 1000, vec![]), // released: no more polls
        (Event::OtherKey, 5000, vec![]),
    ];
    let mut s = Sim::toggle();
    for (event, ms, want) in table {
        assert_eq!(s.core(ms, event.clone()), want, "{event:?} at {ms}");
    }
    assert_eq!(s.phase(), Phase::Recording);
    assert_eq!(s.core(6000, Event::ChordDown), stop(J0));
}

#[test]
fn edge_07_other_key_during_the_stopping_press_keeps_the_job() {
    let mut s = Sim::loaded();
    assert_eq!(s.at(0, Event::OtherKey), []); // idle
    let mut t = Sim::toggle();
    t.at(0, Event::ChordDown);
    t.at(50, Event::ChordUp);
    // The press that stops a toggle recording, then a shortcut key: the job stays.
    assert_eq!(t.core(500, Event::ChordDown), stop(J0));
    assert_eq!(t.core(550, Event::OtherKey), []);
    assert_eq!(t.phase(), Phase::Transcribing);
}

// ---- 8. lost key-up and the watchdog ---------------------------------------------------------

#[test]
fn edge_08_watchdog_polls_every_100_ms_and_a_released_chord_is_a_key_up() {
    let mut s = Sim::loaded();
    s.at(1000, Event::ChordDown);
    assert_eq!(s.0.next_deadline(), Some(Millis(1100)));
    assert_eq!(s.core(1100, Event::Tick), [Effect::PollChord]);
    assert_eq!(s.core(1150, Event::Tick), []); // not due yet
    assert_eq!(s.core(1200, Event::Tick), [Effect::PollChord]);
    assert_eq!(s.core(1210, Event::ChordPolled { held: true }), []);
    assert_eq!(s.phase(), Phase::Recording);

    assert_eq!(s.core(1500, Event::ChordPolled { held: false }), stop(J0));
    assert_eq!(s.phase(), Phase::Transcribing);
    // The real key-up arriving late is harmless.
    assert_eq!(s.core(1600, Event::ChordUp), []);
}

#[test]
fn edge_08_toggle_start_press_held_three_periods_polls_three_times() {
    let mut s = Sim::toggle();
    s.at(0, Event::ChordDown);
    let mut polls = 0;
    for ms in [100, 200, 300] {
        polls += s
            .core(ms, Event::Tick)
            .iter()
            .filter(|e| **e == Effect::PollChord)
            .count();
        assert_eq!(s.core(ms, Event::ChordPolled { held: true }), []);
    }
    assert_eq!(polls, 3);
    assert_eq!(s.core(350, Event::ChordUp), []);
    assert_eq!(s.core(400, Event::Tick), []);
    assert_eq!(s.phase(), Phase::Recording);
}

#[test]
fn edge_08_a_poll_that_sees_a_short_press_end_discards_it() {
    let mut s = Sim::loaded();
    s.at(1000, Event::ChordDown);
    assert_eq!(
        s.core(1100, Event::ChordPolled { held: false }),
        [Effect::DiscardCapture]
    );
}

#[test]
fn edge_08_polling_stops_with_the_recording() {
    let mut s = Sim::loaded();
    s.press(0, 500);
    assert_eq!(s.core(10_000, Event::Tick), []);
    assert_eq!(s.0.next_deadline(), None);
}

// ---- 9, 10. chord keys are never swallowed, Win release belongs to the hook ------------------

#[test]
fn edge_09_10_key_events_produce_no_key_injection_or_swallow_effect() {
    // VK 0xE8 injection and "chord keys pass through" live in the hook. The machine's whole
    // answer to a press and a release is the list below: no effect can hold a key back.
    let mut s = Sim::loaded();
    assert_eq!(
        s.at(0, Event::ChordDown),
        [
            Effect::StartCapture,
            Effect::Play(Sound::Start),
            Effect::Overlay(REC)
        ]
    );
    assert_eq!(
        s.at(500, Event::ChordUp),
        [
            Effect::StopCapture(J0),
            Effect::Play(Sound::Stop),
            Effect::Overlay(OverlayState::Transcribing)
        ]
    );
}

#[test]
fn edge_10_phase_is_recording_only_while_recording_so_the_hook_swallows_esc_only_then() {
    let mut s = Sim::loaded();
    let mut seen = vec![s.phase()];
    s.at(0, Event::ChordDown);
    seen.push(s.phase());
    s.at(500, Event::ChordUp);
    seen.push(s.phase());
    s.at(550, Event::JobAudioReady(J0));
    s.at(600, done(J0));
    seen.push(s.phase());
    s.at(700, pasted(J0));
    seen.push(s.phase());
    assert_eq!(
        seen,
        [
            Phase::Idle,
            Phase::Recording,
            Phase::Transcribing,
            Phase::Pasting,
            Phase::Idle
        ]
    );
}

// ---- 11. hotkey changed while recording ------------------------------------------------------

#[test]
fn edge_11_new_config_waits_for_idle() {
    let toggle = MachineConfig {
        mode: Mode::Toggle,
        ..untailed()
    };
    let mut s = Sim::loaded();
    s.at(0, Event::ChordDown);
    assert_eq!(s.at(100, Event::ConfigChanged(toggle)), []);
    // Still push-to-talk: the release stops this recording.
    assert_eq!(s.core(500, Event::ChordUp), stop(J0));
    s.at(550, Event::JobAudioReady(J0));
    assert_eq!(s.core(600, done(J0)), [Effect::Paste(J0)]);
    // Back to Idle: the new config goes live.
    assert_eq!(s.core(700, pasted(J0)), [Effect::ApplyConfig]);
    // Toggle now: the release does not stop.
    s.at(800, Event::ChordDown);
    assert_eq!(s.core(900, Event::ChordUp), []);
    assert_eq!(s.phase(), Phase::Recording);
}

#[test]
fn edge_11_config_at_idle_applies_at_once() {
    let mut s = Sim::loaded();
    assert_eq!(
        s.at(0, Event::ConfigChanged(untailed())),
        [Effect::ApplyConfig]
    );
}

fn with_model(model: &str) -> MachineConfig {
    MachineConfig {
        model: model.into(),
        ..untailed()
    }
}

/// Same model file, loaded onto another device.
fn on_cpu() -> MachineConfig {
    MachineConfig {
        gpu: DeviceChoice::Cpu,
        ..untailed()
    }
}

#[test]
fn new_model_setting_unloads_the_loaded_model() {
    let unload = vec![Effect::ApplyConfig, Effect::Unload];
    // (case, new config, model loaded before, effects of the config, next press preloads)
    let cases = [
        (
            "same model",
            untailed(),
            true,
            vec![Effect::ApplyConfig],
            false,
        ),
        ("new id", with_model("large-v3"), true, unload.clone(), true),
        ("new gpu", on_cpu(), true, unload, true),
        (
            "new id, unloaded",
            with_model("large-v3"),
            false,
            vec![Effect::ApplyConfig],
            true,
        ),
    ];
    for (case, cfg, loaded, want, preload) in cases {
        let mut s = if loaded {
            Sim::loaded()
        } else {
            Sim(State::new(untailed()))
        };
        assert_eq!(s.core(0, Event::ConfigChanged(cfg)), want, "{case}");
        let fx = s.core(100, Event::ChordDown);
        assert_eq!(fx.contains(&Effect::Preload), preload, "{case}");
    }
}

#[test]
fn new_model_setting_during_a_load_unloads_once_the_old_one_lands() {
    for cfg in [with_model("large-v3"), on_cpu()] {
        let case = format!("{cfg:?}");
        let mut s = Sim(State::new(untailed()));
        assert!(s.core(0, Event::ChordDown).contains(&Effect::Preload));
        s.at(100, Event::Esc);
        assert_eq!(
            s.core(200, Event::ConfigChanged(cfg)),
            [Effect::ApplyConfig],
            "{case}"
        );
        assert_eq!(s.core(300, Event::ModelLoaded), [Effect::Unload], "{case}");
        assert!(
            s.core(400, Event::ChordDown).contains(&Effect::Preload),
            "{case}"
        );
    }
}

// ---- 12. hook removed after a stall ----------------------------------------------------------

#[test]
fn edge_12_hook_is_reinstalled_on_resume_and_unlock() {
    for event in [Event::Resume, Event::Unlock] {
        let mut s = Sim::loaded();
        assert_eq!(s.at(0, event.clone()), [Effect::ReinstallHook], "{event:?}");
    }
}

#[test]
fn edge_46_47_sleep_and_lock_cancel_the_recording() {
    for event in [Event::Suspend, Event::Lock] {
        let mut s = Sim::loaded();
        s.at(0, Event::ChordDown);
        let fx = s.core(300, event.clone());
        assert!(fx.contains(&Effect::DiscardCapture), "{event:?}");
        assert!(
            !fx.iter().any(|e| matches!(e, Effect::Play(_))),
            "{event:?}"
        );
        assert_eq!(s.phase(), Phase::Idle, "{event:?}");
    }
}

#[test]
fn edge_46_sleep_unloads_the_model_once_nothing_is_running() {
    let mut s = Sim::loaded();
    s.press(0, 500);
    s.at(550, Event::JobAudioReady(J0));
    // A job is in flight: unload waits for it.
    assert!(!s.core(600, Event::Suspend).contains(&Effect::Unload));
    s.at(700, done(J0));
    assert!(s.core(800, pasted(J0)).contains(&Effect::Unload));

    let mut idle = Sim::loaded();
    assert_eq!(idle.core(0, Event::Suspend), [Effect::Unload]);
}

// ---- neighbours: model, mic, empty text, idle unload -----------------------------------------

#[test]
fn model_loads_on_key_down_and_a_job_waits_for_it() {
    let mut s = Sim::loaded();
    s.at(100, Event::Suspend); // unloads: model is now cold
    assert_eq!(
        s.core(200, Event::ChordDown),
        [
            Effect::StartCapture,
            Effect::Preload,
            Effect::Play(Sound::Start)
        ]
    );
    let fx = s.at(700, Event::ChordUp);
    assert_eq!(overlay_of(&fx), Some(OverlayState::LoadingModel));
    // Audio is ready but the model is not: no whisper call yet.
    assert_eq!(s.core(750, Event::JobAudioReady(J0)), []);
    let fx = s.at(2000, Event::ModelLoaded);
    assert_eq!(
        fx,
        [
            Effect::Transcribe(J0),
            Effect::Overlay(OverlayState::Transcribing)
        ]
    );
}

#[test]
fn model_missing_at_key_down_gives_error_and_no_recording() {
    let mut s = Sim::loaded();
    s.at(0, Event::ChordDown);
    // The load fails while the key is down: the recording is dropped.
    let fx = s.core(50, Event::ModelFailed(ModelFailure::Missing));
    assert_eq!(
        fx,
        [
            Effect::DiscardCapture,
            Effect::Play(Sound::Error),
            Effect::Notify(Notice::NoModel)
        ]
    );
    // Later presses are refused the same way until the config changes.
    assert_eq!(
        s.core(5000, Event::ChordDown),
        [Effect::Play(Sound::Error), Effect::Notify(Notice::NoModel)]
    );
    assert_eq!(s.phase(), Phase::Idle);
    s.at(6000, Event::ConfigChanged(untailed()));
    assert!(
        s.core(7000, Event::ChordDown)
            .contains(&Effect::StartCapture)
    );
}

#[test]
fn mic_unplugged_keeps_what_was_captured_and_notifies() {
    let mut s = Sim::loaded();
    s.at(0, Event::ChordDown);
    let fx = s.core(2000, Event::DeviceLost);
    assert_eq!(
        fx,
        [
            Effect::StopCapture(J0),
            Effect::Play(Sound::Stop),
            Effect::Notify(Notice::DeviceLost)
        ]
    );
    assert_eq!(s.core(2500, Event::ChordUp), []);
}

#[test]
fn mic_open_failure_gives_error_and_no_whisper_call() {
    let mut s = Sim::loaded();
    s.at(0, Event::ChordDown);
    let fx = s.core(40, Event::CaptureFailed);
    assert_eq!(
        fx,
        [
            Effect::DiscardCapture,
            Effect::Play(Sound::Error),
            Effect::Notify(Notice::CaptureFailed)
        ]
    );
    assert_eq!(s.phase(), Phase::Idle);
}

#[test]
fn empty_transcript_pastes_nothing_and_says_so() {
    let mut s = Sim::loaded();
    s.press(0, 500);
    s.at(550, Event::JobAudioReady(J0));
    let fx = s.at(
        900,
        Event::TranscriptionDone {
            job: J0,
            empty: true,
        },
    );
    assert_eq!(
        fx,
        [Effect::Overlay(OverlayState::Text(Notice::NothingHeard))]
    );
    assert_eq!(s.phase(), Phase::Idle);
}

#[test]
fn model_unloads_after_the_idle_time_and_a_press_cancels_it() {
    let mut s = Sim::loaded();
    assert_eq!(s.0.next_deadline(), Some(Millis(300_000)));
    s.at(100_000, Event::ChordDown);
    assert_eq!(s.0.next_deadline(), Some(Millis(100_100))); // poll only, unload disarmed
    s.at(100_500, Event::ChordUp);
    s.at(100_600, Event::JobAudioReady(J0));
    s.at(100_900, done(J0));
    s.at(101_000, pasted(J0));
    assert_eq!(s.0.next_deadline(), Some(Millis(401_000)));
    assert_eq!(s.core(400_999, Event::Tick), []);
    assert_eq!(s.core(401_000, Event::Tick), [Effect::Unload]);
    assert_eq!(s.0.next_deadline(), None);
}

#[test]
fn zero_idle_unload_means_never() {
    let mut s = Sim::with(MachineConfig {
        idle_unload: Millis(0),
        ..untailed()
    });
    assert_eq!(s.0.next_deadline(), None);
    s.press(0, 500);
    assert_eq!(s.0.next_deadline(), None);
}

// ---- release tail (ARCHITECTURE 3: keeps the last syllable) ---------------------------------

fn tailed(mode: Mode, tail: u64) -> Sim {
    Sim::with(MachineConfig {
        mode,
        release_tail: Millis(tail),
        ..MachineConfig::default()
    })
}

#[test]
fn tail_delays_the_stop_of_every_kept_recording() {
    // (mode, tail ms, stopping event at 1000 after a press at 0, when StopCapture comes)
    let table = [
        (Mode::Push, 100, Event::ChordUp, 1100),
        (Mode::Push, 100, Event::ChordPolled { held: false }, 1100),
        (Mode::Toggle, 100, Event::ChordDown, 1100),
        (Mode::Push, 250, Event::ChordUp, 1250),
        (Mode::Push, 0, Event::ChordUp, 1000),
        (Mode::Toggle, 0, Event::ChordDown, 1000),
    ];
    for (mode, tail, event, stop_at) in table {
        let case = format!("{mode:?}, tail {tail}, {event:?}");
        let mut s = tailed(mode, tail);
        s.at(0, Event::ChordDown);
        if mode == Mode::Toggle {
            s.at(50, Event::ChordUp);
        }
        let mut fx = s.core(1000, event);
        if stop_at > 1000 {
            assert_eq!(fx, [], "{case}");
            assert_eq!(s.phase(), Phase::Recording, "{case}");
            assert_eq!(s.0.next_deadline(), Some(Millis(stop_at)), "{case}");
            assert_eq!(s.core(stop_at - 1, Event::Tick), [], "{case}");
            fx = s.core(stop_at, Event::Tick);
        }
        assert_eq!(fx, stop(J0), "{case}");
        assert_eq!(s.phase(), Phase::Transcribing, "{case}");
    }
}

#[test]
fn tail_does_not_hold_back_a_discarded_tap() {
    let mut s = tailed(Mode::Push, 100);
    s.at(0, Event::ChordDown);
    assert_eq!(s.core(100, Event::ChordUp), [Effect::DiscardCapture]);
    assert_eq!(s.phase(), Phase::Idle);
}

#[test]
fn esc_during_the_tail_cancels() {
    let mut s = tailed(Mode::Push, 100);
    s.at(0, Event::ChordDown);
    s.at(1000, Event::ChordUp);
    assert_eq!(
        s.core(1050, Event::Esc),
        [Effect::DiscardCapture, Effect::Play(Sound::Cancel)]
    );
    assert_eq!(s.phase(), Phase::Idle);
    assert_eq!(s.core(1100, Event::Tick), []);
}

#[test]
fn key_down_during_the_tail_stops_the_job_at_once_and_records_again() {
    let mut s = tailed(Mode::Push, 100);
    s.at(0, Event::ChordDown);
    s.at(1000, Event::ChordUp);
    let fx = s.core(1050, Event::ChordDown);
    assert_eq!(fx, [stop(J0).as_slice(), START.as_slice()].concat());
    assert_eq!(s.phase(), Phase::Recording);
    // The new recording is a plain press: its own release goes through the tail.
    assert_eq!(s.core(1150, Event::Tick), [Effect::PollChord]);
    assert_eq!(s.core(2000, Event::ChordUp), []);
    assert_eq!(s.core(2100, Event::Tick), stop(J1));
}

#[test]
fn typing_during_a_toggle_tail_keeps_the_job() {
    let mut s = tailed(Mode::Toggle, 100);
    s.at(0, Event::ChordDown);
    s.at(50, Event::ChordUp);
    assert_eq!(s.core(1000, Event::ChordDown), []);
    assert_eq!(s.core(1050, Event::OtherKey), []);
    // A down the hook repeats during the stopping press is not a new press.
    assert_eq!(s.core(1060, Event::ChordDown), []);
    assert_eq!(s.core(1100, Event::Tick), stop(J0));
}

#[test]
fn max_recording_stops_at_once_without_a_tail() {
    let mut s = tailed(Mode::Push, 100);
    s.at(0, Event::ChordDown);
    assert_eq!(s.core(600_000, Event::Tick), stop(J0));
    // A release inside what would have been the tail changes nothing.
    assert_eq!(s.core(600_050, Event::ChordUp), []);
    assert_eq!(s.phase(), Phase::Transcribing);
}

#[test]
fn machine_config_maps_the_file_settings() {
    let d = MachineConfig::default();
    // SPEC defaults, read from `Config::default` through the same mapping.
    assert_eq!(
        (d.mode, d.max_recording, d.idle_unload, d.release_tail),
        (
            Mode::Push,
            Millis::from_secs(600),
            Millis::from_secs(300),
            Millis(100)
        )
    );
    let mut file = Config::default();
    file.hotkey.mode = Mode::Toggle;
    file.audio.tail_ms = 150;
    file.audio.max_seconds = 300;
    file.model.idle_unload_minutes = 0;
    assert_eq!(
        MachineConfig::from(&file),
        MachineConfig {
            mode: Mode::Toggle,
            release_tail: Millis(150),
            max_recording: Millis::from_secs(300),
            idle_unload: Millis(0),
            ..MachineConfig::default()
        }
    );
    // The model is the file to load: the id, or the path of a custom model.
    for (id, path, want) in [
        ("large-v3", "", "large-v3"),
        ("custom", "D:/m.bin", "D:/m.bin"),
    ] {
        file.model.id = id.into();
        file.model.custom_path = path.into();
        assert_eq!(MachineConfig::from(&file).model, want);
    }
    file.model.gpu = DeviceChoice::Named("GPU 1".into());
    assert_eq!(
        MachineConfig::from(&file).gpu,
        DeviceChoice::Named("GPU 1".into())
    );
}

#[test]
fn tail_does_not_hold_back_a_short_toggle_or_locked_stop() {
    // (mode, events before the stop press, stop press ms): under 300 ms of audio either way.
    let table = [
        (
            Mode::Toggle,
            vec![(0, Event::ChordDown), (50, Event::ChordUp)],
            250,
        ),
        (
            Mode::Push,
            vec![
                (0, Event::ChordDown),
                (100, Event::ChordUp),
                (200, Event::ChordDown),
                (300, Event::ChordUp), // double tap: locked from 200
            ],
            400,
        ),
    ];
    for (mode, before, stop_ms) in table {
        let mut s = tailed(mode, 100);
        for (ms, event) in before {
            s.at(ms, event);
        }
        assert_eq!(s.phase(), Phase::Recording, "{mode:?}");
        assert_eq!(
            s.core(stop_ms, Event::ChordDown),
            [Effect::DiscardCapture],
            "{mode:?}"
        );
        assert_eq!(s.phase(), Phase::Idle, "{mode:?}");
        assert_eq!(s.core(stop_ms + 100, Event::Tick), [], "{mode:?}");
    }
}

// ---- paste outcomes and mic fallback (items 15, 35, 36) -----------------------------------

/// A loaded machine with job 0 handed to the paste worker.
fn pasting() -> Sim {
    let mut s = Sim::loaded();
    s.press(0, 500);
    s.at(550, Event::JobAudioReady(J0));
    assert_eq!(s.core(900, done(J0)), [Effect::Paste(J0)]);
    s
}

/// `pasting`, then job 0 pasted with `first` and job 1 recorded and handed to paste.
fn second_paste(first: PasteOutcome) -> Sim {
    let mut s = pasting();
    s.at(
        1000,
        Event::PasteDone {
            job: J0,
            outcome: first,
        },
    );
    s.press(2000, 2500);
    s.at(2550, Event::JobAudioReady(J1));
    assert_eq!(s.core(2900, done(J1)), [Effect::Paste(J1)]);
    s
}

#[test]
fn paste_outcome_notices() {
    let table = [
        (PasteOutcome::Pasted, None),
        (PasteOutcome::Typed, None),
        (
            PasteOutcome::ClipboardOnly(ClipboardOnlyWhy::ModifiersHeld),
            Some(Notice::PressPaste),
        ),
        (
            PasteOutcome::ClipboardOnly(ClipboardOnlyWhy::Elevated),
            Some(Notice::AdminWindow),
        ),
        (PasteOutcome::PastedNoRestore, Some(Notice::RestoreSkipped)),
        (PasteOutcome::Failed, Some(Notice::PasteFailed)),
    ];
    for (outcome, notice) in table {
        let mut s = pasting();
        let fx = s.core(1000, Event::PasteDone { job: J0, outcome });
        let mut want: Effects = notice.map(Effect::Notify).into_iter().collect();
        if outcome == PasteOutcome::Failed {
            want.insert(0, Effect::Play(Sound::Error));
        }
        assert_eq!(fx, want, "{outcome:?}");
        assert_eq!(s.phase(), Phase::Idle, "{outcome:?}");
    }
}

#[test]
fn clipboard_only_notifies_every_time() {
    let outcome = PasteOutcome::ClipboardOnly(ClipboardOnlyWhy::Elevated);
    let mut s = second_paste(outcome);
    assert_eq!(
        s.core(3000, Event::PasteDone { job: J1, outcome }),
        [Effect::Notify(Notice::AdminWindow)]
    );
}

#[test]
fn paste_failure_errors_every_time_and_shows_it() {
    let outcome = PasteOutcome::Failed;
    let mut s = second_paste(outcome);
    let fx = s.at(3000, Event::PasteDone { job: J1, outcome });
    assert_eq!(
        fx,
        [
            Effect::Play(Sound::Error),
            Effect::Notify(Notice::PasteFailed),
            Effect::Overlay(OverlayState::Text(Notice::PasteFailed))
        ]
    );
    assert_eq!(s.phase(), Phase::Idle);
}

#[test]
fn skipped_restore_is_noted_once() {
    let outcome = PasteOutcome::PastedNoRestore;
    let mut s = second_paste(outcome);
    assert_eq!(s.core(3000, Event::PasteDone { job: J1, outcome }), []);
    assert_eq!(s.phase(), Phase::Idle);
}

#[test]
fn paste_done_for_a_job_not_pasting_says_nothing() {
    let mut s = pasting();
    let outcome = PasteOutcome::ClipboardOnly(ClipboardOnlyWhy::ModifiersHeld);
    assert_eq!(s.core(1000, Event::PasteDone { job: J1, outcome }), []);
    assert_eq!(s.phase(), Phase::Pasting);
}

#[test]
fn mic_fallback_is_noted_once() {
    let mut s = Sim::loaded();
    s.at(0, Event::ChordDown);
    assert_eq!(
        s.core(10, Event::MicFellBack),
        [Effect::Notify(Notice::MicFallback)]
    );
    s.at(500, Event::ChordUp);
    s.at(1000, Event::ChordDown);
    assert_eq!(s.core(1010, Event::MicFellBack), []);
    assert_eq!(s.phase(), Phase::Recording);
}

#[test]
fn phase_cell_round_trips_every_phase() {
    let cell = PhaseCell::default();
    let reader = cell.clone();
    assert_eq!(reader.load(), Phase::Idle);
    for phase in [
        Phase::Recording,
        Phase::Transcribing,
        Phase::Pasting,
        Phase::Idle,
    ] {
        cell.store(phase);
        assert_eq!(reader.load(), phase);
        assert_eq!(Phase::from_u8(phase.as_u8()), Some(phase));
    }
    assert_eq!(Phase::from_u8(4), None);
}

#[test]
fn phase_serialises_lowercase() {
    let v = toml::Value::try_from(Phase::Transcribing).unwrap();
    assert_eq!(v.as_str(), Some("transcribing"));
    assert_eq!(v.try_into::<Phase>().unwrap(), Phase::Transcribing);
}
