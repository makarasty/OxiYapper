//! Another key, Esc and cancels while recording (R2.1): every edge case, with speech or without.

use super::{
    Effect, Effects, EmptyAudio, Event, JobId, MachineConfig, Mode, Notice, OverlayState, Phase,
    Sound, State, step,
};
use crate::config::OtherKey;
use crate::paste::{ClipboardOnlyWhy, PasteOutcome};
use crate::types::Millis;

const J0: JobId = JobId(0);

/// What the real detector takes from the first spoken sound to `SpeechHeard` at the machine:
/// 80 ms hold, 20 ms meter window, 10 ms poll, 10 ms hop to the coordinator (`audio/speech.rs`).
const HEARD_AFTER: u64 = 120;

fn cfg(mode: Mode, other_key: OtherKey, esc_cancels: bool) -> MachineConfig {
    MachineConfig {
        mode,
        other_key,
        esc_cancels,
        release_tail: Millis(0),
        ..MachineConfig::default()
    }
}

struct Sim(State);

impl Sim {
    /// Model loaded, chord pressed at 0, so a recording runs.
    fn recording(cfg: MachineConfig) -> Self {
        let mut s = Self(State::new(cfg));
        s.at(0, Event::ModelLoaded);
        s.at(0, Event::ChordDown);
        assert_eq!(s.0.phase(), Phase::Recording);
        s
    }

    fn at(&mut self, ms: u64, event: Event) -> Effects {
        step(&mut self.0, event, Millis(ms))
    }

    /// Effects without the overlay updates.
    fn core(&mut self, ms: u64, event: Event) -> Effects {
        self.at(ms, event)
            .into_iter()
            .filter(|e| !matches!(e, Effect::Overlay(_)))
            .collect()
    }

    fn phase(&self) -> Phase {
        self.0.phase()
    }

    /// Ticks at every deadline up to `ms`, as the coordinator does; effects without the overlay
    /// and the watchdog polls.
    fn until(&mut self, ms: u64) -> Effects {
        let mut fx = Effects::new();
        while let Some(t) = self.0.next_deadline().filter(|t| t.0 <= ms) {
            fx.extend(self.core(t.0, Event::Tick));
        }
        fx.retain(|e| *e != Effect::PollChord);
        fx
    }
}

fn stop(job: JobId) -> [Effect; 2] {
    [Effect::StopCapture(job), Effect::Play(Sound::Stop)]
}

// ---- another key: auto ------------------------------------------------------------------------

#[test]
fn auto_without_speech_is_a_shortcut_and_cancels_silently_in_both_modes() {
    for mode in [Mode::Push, Mode::Toggle] {
        let mut s = Sim::recording(cfg(mode, OtherKey::Auto, false));
        // The detector gets its latency to report speech already started; none comes.
        assert_eq!(s.core(50, Event::OtherKey), [], "{mode:?}");
        assert_eq!(s.until(199), [], "{mode:?}");
        assert_eq!(s.phase(), Phase::Recording, "{mode:?}");
        // No sound, no notice, no kept audio: Ctrl+Win+D reaches Windows and nothing else.
        assert_eq!(
            s.at(200, Event::Tick),
            [
                Effect::DiscardCapture,
                Effect::Overlay(OverlayState::Hidden)
            ],
            "{mode:?}"
        );
        assert_eq!(s.phase(), Phase::Idle, "{mode:?}");
        // The key-up of the chord afterwards does nothing.
        assert_eq!(s.core(900, Event::ChordUp), [], "{mode:?}");
    }
}

#[test]
fn auto_after_speech_keeps_recording_and_the_release_still_stops_it() {
    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Auto, false));
    s.at(100, Event::SpeechHeard);
    assert_eq!(s.core(200, Event::OtherKey), []);
    assert_eq!(s.phase(), Phase::Recording);
    assert_eq!(s.core(1000, Event::ChordUp), stop(J0));
    s.at(1050, Event::JobAudioReady(J0));
    assert_eq!(s.phase(), Phase::Transcribing);
}

#[test]
fn a_300_ms_utterance_with_an_accidental_extra_key_is_kept() {
    // The phrase starts at `spoken`, the detector reports it `HEARD_AFTER` later, the extra key
    // lands anywhere from the first sound until the report; the release ends the 300 ms phrase.
    for spoken in [0, 50, 100, 250] {
        for into in (0..=HEARD_AFTER).step_by(10) {
            let key = spoken + into;
            let heard = spoken + HEARD_AFTER;
            let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Auto, false));
            let case = format!("spoken {spoken}, key {key}");
            assert_eq!(s.until(key), [], "{case}");
            assert_eq!(s.core(key, Event::OtherKey), [], "{case}");
            assert_eq!(s.until(heard), [], "{case}");
            s.at(heard, Event::SpeechHeard);
            let up = (spoken + 300).max(heard);
            assert_eq!(s.until(up), [], "{case}");
            assert_eq!(s.core(up, Event::ChordUp), stop(J0), "{case}");
        }
    }
}

#[test]
fn a_quiet_mic_never_loses_a_dictation_to_a_key_after_the_window() {
    // Speech never crosses the detector's level; every key after the shortcut window is typing.
    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Auto, false));
    for t in [401, 500, 2000, 9000] {
        assert_eq!(s.until(t), [], "{t}");
        assert_eq!(s.core(t, Event::OtherKey), [], "{t}");
    }
    assert_eq!(s.until(12_000), []);
    assert_eq!(s.core(12_000, Event::ChordUp), stop(J0));
}

#[test]
fn a_key_at_the_end_of_the_window_without_speech_is_still_a_shortcut() {
    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Auto, false));
    s.until(400);
    assert_eq!(s.core(400, Event::OtherKey), []);
    // A second key does not push the decision back.
    assert_eq!(s.core(500, Event::OtherKey), []);
    assert_eq!(s.until(549), []);
    assert_eq!(s.until(550), [Effect::DiscardCapture]);
}

#[test]
fn a_release_before_the_decision_waits_for_it() {
    // Push, no tail: the chord comes up after `tap_max` while the detector may still report.
    for (speech, want) in [
        (true, stop(J0).to_vec()),
        (false, vec![Effect::DiscardCapture]),
    ] {
        let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Auto, false));
        s.until(250);
        s.at(250, Event::OtherKey);
        assert_eq!(s.core(310, Event::ChordUp), [], "{speech}");
        if speech {
            // Spoken from 210, reported 120 ms later.
            s.at(330, Event::SpeechHeard);
        }
        assert_eq!(s.until(400), want, "{speech}");
        assert_eq!(
            s.phase(),
            if speech {
                Phase::Transcribing
            } else {
                Phase::Idle
            }
        );
    }
}

#[test]
fn a_shortcut_released_quickly_is_no_tap_for_a_double_tap_lock() {
    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Auto, false));
    s.at(50, Event::OtherKey);
    assert_eq!(s.core(100, Event::ChordUp), [Effect::DiscardCapture]);
    // A press right after is a fresh hold, not a lock: its release stops it.
    s.at(200, Event::ChordDown);
    s.at(260, Event::SpeechHeard);
    assert_eq!(s.until(900), []);
    assert_eq!(s.core(900, Event::ChordUp), stop(J0));
}

#[test]
fn auto_keeps_going_for_every_extra_key_after_speech() {
    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Auto, false));
    s.at(100, Event::SpeechHeard);
    for t in [200, 210, 220, 230] {
        assert_eq!(s.core(t, Event::OtherKey), [], "{t}");
    }
    assert_eq!(s.phase(), Phase::Recording);
}

#[test]
fn speech_heard_without_a_recording_is_ignored() {
    let mut s = Sim(State::new(cfg(Mode::Push, OtherKey::Auto, false)));
    s.at(0, Event::ModelLoaded);
    assert_eq!(s.at(10, Event::SpeechHeard), []);
    // The next recording starts without speech: a key at once is a shortcut.
    s.at(100, Event::ChordDown);
    s.at(120, Event::OtherKey);
    assert_eq!(
        s.until(270),
        [Effect::DiscardCapture],
        "speech of an earlier recording does not carry over"
    );
}

#[test]
fn speech_of_a_discarded_tap_does_not_carry_into_the_next_recording() {
    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Auto, false));
    s.at(50, Event::SpeechHeard);
    assert_eq!(s.core(100, Event::ChordUp), [Effect::DiscardCapture]); // a tap
    s.at(1000, Event::ChordDown);
    s.at(1020, Event::OtherKey);
    assert_eq!(s.until(1170), [Effect::DiscardCapture]);
}

// ---- another key: never and always ------------------------------------------------------------

#[test]
fn never_does_not_cancel_before_or_after_speech() {
    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Never, false));
    assert_eq!(s.core(50, Event::OtherKey), []);
    s.at(100, Event::SpeechHeard);
    assert_eq!(s.core(150, Event::OtherKey), []);
    assert_eq!(s.phase(), Phase::Recording);
    assert_eq!(s.core(1000, Event::ChordUp), stop(J0));
}

#[test]
fn always_without_speech_cancels_silently() {
    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Always, false));
    assert_eq!(s.core(50, Event::OtherKey), [Effect::DiscardCapture]);
    assert_eq!(s.phase(), Phase::Idle);
}

#[test]
fn always_after_speech_cancels_and_keeps_the_audio() {
    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Always, false));
    s.at(100, Event::SpeechHeard);
    assert_eq!(
        s.core(200, Event::OtherKey),
        [Effect::StopCapture(J0), Effect::Play(Sound::Cancel)]
    );
    // Not a dictation: nothing in the phase until the audio arrives, and no overlay yet.
    assert_eq!(s.phase(), Phase::Idle);
    let fx = s.at(250, Event::JobAudioReady(J0));
    assert_eq!(
        fx,
        [
            Effect::KeepCancelled(J0),
            Effect::Overlay(OverlayState::Text(Notice::CancelledKept))
        ]
    );
    // Never whisper, never paste; the overlay text goes after its time.
    assert_eq!(s.phase(), Phase::Idle);
    assert_eq!(
        s.at(1750, Event::Tick),
        [Effect::Overlay(OverlayState::Hidden)]
    );
}

#[test]
fn a_cancelled_job_with_empty_audio_keeps_nothing_and_says_cancelled() {
    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Always, false));
    s.at(100, Event::SpeechHeard);
    s.at(200, Event::OtherKey);
    let fx = s.at(
        250,
        Event::JobAudioEmpty {
            job: J0,
            why: EmptyAudio::NothingHeard,
        },
    );
    assert_eq!(fx, [Effect::Overlay(OverlayState::Text(Notice::Cancelled))]);
    assert_eq!(s.phase(), Phase::Idle);
}

#[test]
fn other_key_with_the_chord_up_never_cancels_a_locked_or_toggled_recording() {
    for mode in [Mode::Push, Mode::Toggle] {
        let mut s = Sim::recording(cfg(mode, OtherKey::Always, false));
        s.at(100, Event::SpeechHeard);
        if mode == Mode::Push {
            // A double tap locks it on.
            s.at(100, Event::ChordUp);
            s.at(150, Event::ChordDown);
            s.at(200, Event::ChordUp);
        } else {
            s.at(200, Event::ChordUp);
        }
        assert_eq!(s.core(900, Event::OtherKey), [], "{mode:?}");
        assert_eq!(s.phase(), Phase::Recording, "{mode:?}");
    }
}

#[test]
fn other_key_while_idle_or_transcribing_is_not_ours() {
    let mut s = Sim(State::new(cfg(Mode::Push, OtherKey::Always, true)));
    s.at(0, Event::ModelLoaded);
    assert_eq!(s.at(10, Event::OtherKey), []);
    s.at(100, Event::ChordDown);
    s.at(600, Event::ChordUp);
    s.at(650, Event::JobAudioReady(J0));
    assert_eq!(s.at(700, Event::OtherKey), []);
    assert_eq!(s.phase(), Phase::Transcribing);
}

// ---- Esc --------------------------------------------------------------------------------------

#[test]
fn esc_off_is_not_a_cancel() {
    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Auto, false));
    s.at(100, Event::SpeechHeard);
    assert_eq!(s.at(200, Event::Esc), []);
    assert_eq!(s.phase(), Phase::Recording);
}

#[test]
fn esc_on_without_speech_discards_with_sound_and_notice() {
    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Auto, true));
    assert_eq!(
        s.at(200, Event::Esc),
        [
            Effect::DiscardCapture,
            Effect::Play(Sound::Cancel),
            Effect::Overlay(OverlayState::Text(Notice::Cancelled))
        ]
    );
}

#[test]
fn esc_on_after_speech_keeps_the_audio() {
    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Auto, true));
    s.at(100, Event::SpeechHeard);
    assert_eq!(
        s.core(200, Event::Esc),
        [Effect::StopCapture(J0), Effect::Play(Sound::Cancel)]
    );
    assert_eq!(
        s.core(220, Event::JobAudioReady(J0)),
        [Effect::KeepCancelled(J0)]
    );
}

#[test]
fn esc_during_the_release_tail_keeps_what_was_said() {
    let mut s = Sim::recording(MachineConfig {
        release_tail: Millis(100),
        ..cfg(Mode::Push, OtherKey::Auto, true)
    });
    s.at(100, Event::SpeechHeard);
    s.at(1000, Event::ChordUp); // the tail runs
    assert_eq!(
        s.core(1050, Event::Esc),
        [Effect::StopCapture(J0), Effect::Play(Sound::Cancel)]
    );
    assert_eq!(s.core(1100, Event::Tick), []); // the tail's stop is gone
}

// ---- order of release, repeat, tail -----------------------------------------------------------

#[test]
fn chord_released_after_a_cancel_in_any_order_does_nothing() {
    for ups in [1, 2] {
        let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Auto, false));
        s.at(50, Event::OtherKey);
        s.until(200);
        for i in 0..ups {
            assert_eq!(s.core(100 + i, Event::ChordUp), []);
        }
        assert_eq!(s.phase(), Phase::Idle);
    }
}

#[test]
fn a_chord_press_after_a_cancel_records_again() {
    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Auto, false));
    s.at(50, Event::OtherKey);
    s.until(200);
    s.at(300, Event::ChordUp);
    assert_eq!(
        s.core(5000, Event::ChordDown),
        [Effect::StartCapture, Effect::Play(Sound::Start)]
    );
}

#[test]
fn key_repeat_of_the_chord_is_not_a_second_recording() {
    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Auto, false));
    assert_eq!(s.core(30, Event::ChordDown), []);
    assert_eq!(s.core(60, Event::ChordDown), []);
    s.at(100, Event::SpeechHeard);
    assert_eq!(s.core(1000, Event::ChordUp), stop(J0));
}

#[test]
fn other_key_during_the_tail_keeps_the_job_in_every_mode() {
    for other_key in [OtherKey::Auto, OtherKey::Always, OtherKey::Never] {
        let mut s = Sim::recording(MachineConfig {
            release_tail: Millis(100),
            ..cfg(Mode::Push, other_key, false)
        });
        s.at(100, Event::SpeechHeard);
        s.at(1000, Event::ChordUp);
        assert_eq!(s.core(1050, Event::OtherKey), [], "{other_key:?}");
        assert_eq!(s.core(1100, Event::Tick), stop(J0), "{other_key:?}");
    }
}

#[test]
fn chord_pressed_again_during_the_tail_stops_the_old_job_and_starts_a_new_one() {
    let mut s = Sim::recording(MachineConfig {
        release_tail: Millis(100),
        ..cfg(Mode::Push, OtherKey::Auto, false)
    });
    s.at(100, Event::SpeechHeard);
    s.at(1000, Event::ChordUp);
    let fx = s.core(1050, Event::ChordDown);
    assert_eq!(
        fx,
        [
            Effect::StopCapture(J0),
            Effect::Play(Sound::Stop),
            Effect::StartCapture,
            Effect::Play(Sound::Start)
        ]
    );
    // The new recording has not heard speech: a shortcut key cancels it, the old job stays.
    s.at(1070, Event::OtherKey);
    assert_eq!(s.until(1220), [Effect::DiscardCapture]);
    assert_eq!(s.phase(), Phase::Transcribing);
}

// ---- lock, sleep ------------------------------------------------------------------------------

#[test]
fn lock_mid_recording_discards_without_speech_and_keeps_with_speech() {
    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Auto, false));
    assert_eq!(s.core(100, Event::Lock), [Effect::DiscardCapture]);
    assert_eq!(s.core(200, Event::Unlock), [Effect::ReinstallHook]);

    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Auto, false));
    s.at(50, Event::SpeechHeard);
    // No sound on the lock screen: the audio is kept.
    assert_eq!(s.core(100, Event::Lock), [Effect::StopCapture(J0)]);
    assert_eq!(
        s.core(150, Event::JobAudioReady(J0)),
        [Effect::KeepCancelled(J0)]
    );
    // The key-up never came (secure desktop); the poll finds the chord released.
    assert_eq!(s.core(300, Event::ChordPolled { held: false }), []);
    assert_eq!(s.phase(), Phase::Idle);
}

#[test]
fn sleep_mid_recording_keeps_speech_and_resume_reinstalls_the_hook() {
    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Auto, false));
    s.at(50, Event::SpeechHeard);
    // The kept job needs no model: it unloads at once.
    assert_eq!(
        s.core(100, Event::Suspend),
        [Effect::StopCapture(J0), Effect::Unload]
    );
    s.at(120, Event::JobAudioReady(J0));
    assert_eq!(s.core(5000, Event::Resume), [Effect::ReinstallHook]);
}

#[test]
fn a_kept_cancel_shows_no_text_over_a_newer_recording() {
    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Always, false));
    s.at(100, Event::SpeechHeard);
    s.at(200, Event::OtherKey);
    // The audio is slow; the user presses the chord again first.
    s.at(210, Event::ChordUp);
    s.at(220, Event::ChordDown);
    let fx = s.at(230, Event::JobAudioReady(J0));
    assert_eq!(
        fx,
        [Effect::KeepCancelled(J0)],
        "no overlay text over a recording"
    );
    assert_eq!(s.phase(), Phase::Recording);
}

#[test]
fn chord_pressed_again_with_a_shortcut_cancel_pending_sends_no_audio_to_whisper() {
    // Repeated Ctrl+Win+Right: extra key at 250 (cancel due 400), release at 320 holds the
    // tail to 400, the next press at 380 must cancel the old recording, not transcribe it.
    let mut s = Sim::recording(MachineConfig {
        tap_max: Millis(300),
        ..cfg(Mode::Push, OtherKey::Auto, false)
    });
    s.at(250, Event::OtherKey);
    assert_eq!(s.core(320, Event::ChordUp), []);
    assert_eq!(
        s.core(380, Event::ChordDown),
        [
            Effect::DiscardCapture,
            Effect::StartCapture,
            Effect::Play(Sound::Start)
        ]
    );
    assert_eq!(s.phase(), Phase::Recording);
    // The old deadline is gone: nothing stops the new recording at 400.
    assert_eq!(s.until(400), []);
    assert_eq!(s.phase(), Phase::Recording);
}

/// Job 0 transcribed: the machine hands it to the paste worker.
fn pasting(s: &mut Sim) {
    s.at(0, Event::ModelLoaded);
    s.at(0, Event::ChordDown);
    s.at(500, Event::ChordUp);
    s.at(550, Event::JobAudioReady(J0));
    s.at(
        600,
        Event::TranscriptionDone {
            job: J0,
            empty: false,
        },
    );
}

#[test]
fn a_failed_clipboard_restore_is_told_every_time() {
    let mut s = Sim(State::new(cfg(Mode::Push, OtherKey::Auto, false)));
    for job in 0..2u64 {
        let base = job * 10_000;
        s.at(base, Event::ModelLoaded);
        s.at(base, Event::ChordDown);
        s.at(base + 500, Event::ChordUp);
        s.at(base + 550, Event::JobAudioReady(JobId(job)));
        s.at(
            base + 600,
            Event::TranscriptionDone {
                job: JobId(job),
                empty: false,
            },
        );
        let fx = s.core(
            base + 900,
            Event::PasteDone {
                job: JobId(job),
                outcome: PasteOutcome::PastedNoRestore,
            },
        );
        assert_eq!(
            fx,
            [
                Effect::Play(Sound::Done),
                Effect::Notify(Notice::RestoreSkipped)
            ],
            "paste {job}"
        );
    }
}

#[test]
fn the_done_sound_plays_when_text_was_pasted_and_only_then() {
    let table = [
        (PasteOutcome::Pasted, true),
        (PasteOutcome::Typed, true),
        (PasteOutcome::PastedNoRestore, true),
        (
            PasteOutcome::ClipboardOnly(ClipboardOnlyWhy::Elevated),
            false,
        ),
        (PasteOutcome::Failed, false),
    ];
    for (outcome, done) in table {
        let mut s = Sim(State::new(cfg(Mode::Push, OtherKey::Auto, false)));
        pasting(&mut s);
        let fx = s.core(900, Event::PasteDone { job: J0, outcome });
        assert_eq!(fx.contains(&Effect::Play(Sound::Done)), done, "{outcome:?}");
    }
    // Not on Stop: the sound belongs to the paste.
    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Auto, false));
    assert!(
        !s.core(1000, Event::ChordUp)
            .contains(&Effect::Play(Sound::Done))
    );
}
