//! Another key, Esc and cancels while recording (R2.1): every edge case, with speech or without.

use super::{
    CancelWhy, Effect, Effects, EmptyAudio, Event, JobId, MachineConfig, Mode, ModelFailure, Note,
    Notice, OverlayState, Phase, Sound, State, step,
};
use crate::audio::DETECTOR_LATENCY;
use crate::config::OtherKey;
use crate::paste::{ClipboardOnlyWhy, PasteOutcome};
use crate::types::Millis;

const J0: JobId = JobId(0);

/// What the real detector takes from the first spoken sound to `SpeechHeard` at the machine:
/// 80 ms hold, 20 ms meter window, 10 ms poll, 10 ms hop to the coordinator (`audio/speech.rs`).
const HEARD_AFTER: u64 = 120;

/// How long the machine waits after a shortcut key for speech to be reported.
const DL: u64 = DETECTOR_LATENCY.0;

fn cfg(mode: Mode, other_key: OtherKey, esc_cancels: bool) -> MachineConfig {
    MachineConfig {
        mode,
        other_key,
        esc_cancels,
        release_tail: Millis(0),
        hold_gap: Millis(0),
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
        assert_eq!(s.until(50 + DL - 1), [], "{mode:?}");
        assert_eq!(s.phase(), Phase::Recording, "{mode:?}");
        // No sound, no notice, no kept audio: Ctrl+Win+D reaches Windows and nothing else.
        assert_eq!(
            s.at(50 + DL, Event::Tick),
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
    assert_eq!(s.until(400 + DL - 1), []);
    assert_eq!(s.until(400 + DL), [Effect::DiscardCapture]);
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
        assert_eq!(s.until(250 + DL), want, "{speech}");
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
        s.until(120 + DL),
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
    assert_eq!(s.until(1020 + DL), [Effect::DiscardCapture]);
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
        s.until(50 + DL);
        for i in 0..ups {
            assert_eq!(s.core(50 + DL + i, Event::ChordUp), []);
        }
        assert_eq!(s.phase(), Phase::Idle);
    }
}

#[test]
fn a_chord_press_after_a_cancel_records_again() {
    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Auto, false));
    s.at(50, Event::OtherKey);
    s.until(50 + DL);
    s.at(50 + DL + 100, Event::ChordUp);
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
    assert_eq!(s.until(1070 + DL), [Effect::DiscardCapture]);
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
    // Repeated Ctrl+Win+Right: extra key at 250 (cancel due 250 + DL), release at 320 holds the
    // tail to then, the next press at 380 must cancel the old recording, not transcribe it.
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
    // The old deadline is gone: nothing stops the new recording then.
    assert_eq!(s.until(250 + DL), []);
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

// ---- hold re-press gap (push mode) ------------------------------------------------------------

const J1: JobId = JobId(1);
const J2: JobId = JobId(2);

fn gap_cfg() -> MachineConfig {
    MachineConfig {
        release_tail: Millis(100),
        hold_gap: Millis(400),
        ..cfg(Mode::Push, OtherKey::Auto, true)
    }
}

/// A 1 s dictation: pressed at 0, released at 1000, stopped as J0 at 1100 (its gap ends at 1400).
fn first(cfg: MachineConfig) -> Sim {
    let mut s = Sim::recording(cfg);
    s.at(100, Event::SpeechHeard);
    s.at(1000, Event::ChordUp);
    assert_eq!(s.core(1100, Event::Tick), stop(J0));
    s
}

fn transcribed(s: &mut Sim, job: JobId, ms: u64) -> Effects {
    s.at(ms, Event::JobAudioReady(job));
    s.core(ms + 50, Event::TranscriptionDone { job, empty: false })
}

fn merged(from: &[JobId], into: JobId) -> [Effect; 3] {
    [
        Effect::StopCapture(into),
        Effect::Merge {
            from: from.to_vec(),
            into,
        },
        Effect::Play(Sound::Stop),
    ]
}

#[test]
fn a_re_press_inside_the_tail_goes_on_with_the_same_recording() {
    let mut s = Sim::recording(gap_cfg());
    s.at(100, Event::SpeechHeard);
    s.at(1000, Event::ChordUp);
    // No stop, no start, no sound: the recording simply goes on.
    assert_eq!(s.core(1050, Event::ChordDown), []);
    assert_eq!(s.until(1500), []);
    assert_eq!(s.phase(), Phase::Recording);
    // The next release starts a new tail and one job at its end.
    s.at(2000, Event::ChordUp);
    assert_eq!(s.core(2100, Event::Tick), stop(J0));
}

#[test]
fn the_paste_waits_for_the_end_of_the_gap_when_whisper_is_faster() {
    let mut s = first(gap_cfg());
    assert_eq!(transcribed(&mut s, J0, 1150), []);
    assert_eq!(s.0.next_deadline(), Some(Millis(1400)));
    assert_eq!(s.core(1400, Event::Tick), [Effect::Paste(J0)]);
}

#[test]
fn the_paste_is_not_delayed_when_whisper_takes_longer_than_the_gap() {
    let mut s = first(gap_cfg());
    s.at(1150, Event::JobAudioReady(J0));
    let done = Event::TranscriptionDone {
        job: J0,
        empty: false,
    };
    assert_eq!(s.core(1700, done), [Effect::Paste(J0)]);
}

#[test]
fn a_re_press_one_ms_before_the_gap_ends_continues_and_one_ms_after_starts_anew() {
    let mut s = first(gap_cfg());
    transcribed(&mut s, J0, 1150);
    assert_eq!(
        s.core(1399, Event::ChordDown),
        [Effect::Supersede(J0), Effect::StartCapture]
    );
    // J0 is transcribed but never pastes.
    assert_eq!(s.until(1600), []);
    assert_eq!(s.phase(), Phase::Recording);

    let mut s = first(gap_cfg());
    transcribed(&mut s, J0, 1150);
    assert_eq!(s.until(1401), [Effect::Paste(J0)]);
    assert_eq!(
        s.core(1401, Event::ChordDown),
        [Effect::StartCapture, Effect::Play(Sound::Start)]
    );
}

#[test]
fn a_continuation_ends_as_one_job_that_takes_the_earlier_audio() {
    let mut s = first(gap_cfg());
    transcribed(&mut s, J0, 1150);
    s.at(1300, Event::ChordDown);
    s.at(2000, Event::ChordUp);
    assert_eq!(s.core(2100, Event::Tick), merged(&[J0], J1));
    // Only the merged job goes to whisper and, after its own gap, to the paste.
    let audio = Event::JobAudioReady(J1);
    assert_eq!(s.core(2150, audio), [Effect::Transcribe(J1)]);
    let done = Event::TranscriptionDone {
        job: J1,
        empty: false,
    };
    assert_eq!(s.core(2200, done), []);
    assert_eq!(s.until(2500), [Effect::Paste(J1)]);
}

#[test]
fn three_bounces_make_one_job_at_the_end() {
    let mut s = first(gap_cfg());
    s.at(1300, Event::ChordDown); // continues J0
    s.at(2000, Event::ChordUp);
    assert_eq!(s.core(2100, Event::Tick), merged(&[J0], J1));
    assert_eq!(
        s.core(2300, Event::ChordDown),
        [Effect::Supersede(J1), Effect::StartCapture]
    );
    s.at(3000, Event::ChordUp);
    assert_eq!(s.core(3100, Event::Tick), merged(&[J1], J2));
}

#[test]
fn events_of_a_superseded_job_change_nothing() {
    // J0 is still in whisper when the user presses again; its result comes late and pastes nothing.
    let mut s = first(gap_cfg());
    s.at(1150, Event::JobAudioReady(J0));
    s.at(1300, Event::ChordDown);
    let done = Event::TranscriptionDone {
        job: J0,
        empty: false,
    };
    assert_eq!(s.core(1350, done), []);
    assert_eq!(s.phase(), Phase::Recording);
    s.at(2000, Event::ChordUp);
    s.at(2100, Event::Tick);
    // J0 in whisper never blocks the merged job.
    let audio = Event::JobAudioReady(J1);
    assert_eq!(s.core(2150, audio), [Effect::Transcribe(J1)]);
}

#[test]
fn a_job_still_waiting_for_its_audio_can_be_continued() {
    let mut s = first(gap_cfg());
    assert_eq!(
        s.core(1300, Event::ChordDown),
        [Effect::Supersede(J0), Effect::StartCapture]
    );
    // Its audio arrives later: it goes to no whisper, the coordinator holds it for the merge.
    assert_eq!(s.core(1350, Event::JobAudioReady(J0)), []);
    s.at(2000, Event::ChordUp);
    assert_eq!(s.core(2100, Event::Tick), merged(&[J0], J1));
    let audio = Event::JobAudioReady(J1);
    assert_eq!(s.core(2150, audio), [Effect::Transcribe(J1)]);
}

#[test]
fn a_continuation_released_as_a_tap_still_becomes_a_job() {
    let mut s = first(gap_cfg());
    s.at(1300, Event::ChordDown);
    s.at(1400, Event::ChordUp); // 100 ms: a tap, but it carries J0 on
    assert_eq!(s.until(1500), merged(&[J0], J1));
}

#[test]
fn a_cancel_of_a_continuation_cancels_the_whole_dictation() {
    let mut s = first(gap_cfg());
    transcribed(&mut s, J0, 1150);
    s.at(1300, Event::ChordDown);
    assert_eq!(
        s.core(1500, Event::Esc),
        [
            Effect::StopCapture(J1),
            Effect::Merge {
                from: vec![J0],
                into: J1
            },
            Effect::Play(Sound::Cancel)
        ]
    );
    let audio = Event::JobAudioReady(J1);
    assert_eq!(s.core(1600, audio), [Effect::KeepCancelled(J1)]);
    assert_eq!(s.until(5000), []);

    // `other_key = always` cancels it as well; `auto` does not (speech was heard).
    for (other_key, cancels) in [(OtherKey::Always, true), (OtherKey::Auto, false)] {
        let mut s = first(MachineConfig {
            other_key,
            ..gap_cfg()
        });
        s.at(1300, Event::ChordDown);
        let fx = s.core(1500, Event::OtherKey);
        assert_eq!(
            fx.contains(&Effect::StopCapture(J1)),
            cancels,
            "{other_key:?}"
        );
    }
}

#[test]
fn toggle_mode_and_device_loss_never_wait_for_a_gap() {
    let mut s = Sim::recording(MachineConfig {
        mode: Mode::Toggle,
        ..gap_cfg()
    });
    s.at(100, Event::SpeechHeard);
    s.at(1000, Event::ChordDown); // the stopping press
    s.at(1100, Event::Tick);
    assert_eq!(transcribed(&mut s, J0, 1150), [Effect::Paste(J0)]);

    let mut s = Sim::recording(gap_cfg());
    s.at(100, Event::SpeechHeard);
    s.at(1000, Event::ChordUp); // released: the tail runs
    s.at(1050, Event::DeviceLost);
    assert_eq!(transcribed(&mut s, J0, 1150), [Effect::Paste(J0)]);
}

#[test]
fn a_gap_shorter_than_the_tail_continues_inside_the_gap_only() {
    let short = MachineConfig {
        release_tail: Millis(300),
        hold_gap: Millis(100),
        ..gap_cfg()
    };
    let mut s = Sim::recording(short.clone());
    s.at(100, Event::SpeechHeard);
    s.at(1000, Event::ChordUp);
    assert_eq!(s.core(1050, Event::ChordDown), []);

    let mut s = Sim::recording(short);
    s.at(100, Event::SpeechHeard);
    s.at(1000, Event::ChordUp);
    // Past the gap but inside the tail: as before, the old job stops and a new one starts.
    assert_eq!(
        s.core(1150, Event::ChordDown),
        [
            Effect::StopCapture(J0),
            Effect::Play(Sound::Stop),
            Effect::StartCapture,
            Effect::Play(Sound::Start)
        ]
    );
    assert_eq!(transcribed(&mut s, J0, 1200), [Effect::Paste(J0)]);
}

#[test]
fn a_continuation_adds_no_waiting_job_but_a_new_dictation_is_refused() {
    let busy = |s: &mut Sim| {
        // J0 in whisper; J1 queued behind it; J2 waiting for its audio, gap until 4500.
        s.at(1150, Event::JobAudioReady(J0));
        s.at(1500, Event::ChordDown);
        s.at(2500, Event::ChordUp);
        s.at(2600, Event::Tick);
        s.at(2650, Event::JobAudioReady(J1));
        s.at(3000, Event::ChordDown);
        s.at(4000, Event::ChordUp);
        s.at(4100, Event::Tick);
    };
    let mut s = first(gap_cfg());
    busy(&mut s);
    assert_eq!(
        s.core(4200, Event::ChordDown),
        [Effect::Supersede(J2), Effect::StartCapture]
    );
    let mut s = first(gap_cfg());
    busy(&mut s);
    assert_eq!(s.core(4600, Event::ChordDown), [Effect::Play(Sound::Error)]);
}

#[test]
fn a_failed_continuation_gives_the_carried_job_back_to_paste() {
    // (failure, history on): the failed part keeps its own audio only with history on.
    let failures = [
        (Event::CaptureFailed, true),
        (Event::CaptureFailed, false),
        (Event::ModelFailed(ModelFailure::Bad), true),
    ];
    for (failure, keep_history) in failures {
        let case = format!("{failure:?}, history {keep_history}");
        let mut s = first(MachineConfig {
            keep_history,
            ..gap_cfg()
        });
        transcribed(&mut s, J0, 1150);
        s.at(1300, Event::ChordDown); // carries J0 on
        let fx = s.core(1500, failure);
        assert!(fx.contains(&Effect::Paste(J0)), "{case}: {fx:?}");
        assert!(
            !fx.iter()
                .any(|e| matches!(e, Effect::Merge { .. } | Effect::DropJob(_))),
            "{case}: {fx:?}"
        );
        assert_eq!(
            fx.contains(&Effect::StopCapture(J1)),
            keep_history,
            "{case}: {fx:?}"
        );
        if keep_history {
            // The failed part alone is kept as failed.
            let fx = s.core(1600, Event::JobAudioReady(J1));
            assert!(fx.contains(&Effect::DropJob(J1)), "{case}: {fx:?}");
        }
        let outcome = PasteOutcome::Pasted;
        s.at(1700, Event::PasteDone { job: J0, outcome });
        s.until(10_000);
        assert!(!s.0.busy(), "{case}");
    }
}

#[test]
fn a_failure_before_the_carried_jobs_audio_still_transcribes_it() {
    let mut s = first(MachineConfig {
        keep_history: false,
        ..gap_cfg()
    });
    s.at(1300, Event::ChordDown); // J0 still waits for its audio
    assert_eq!(
        s.core(1400, Event::CaptureFailed)[0],
        Effect::DiscardCapture
    );
    assert_eq!(
        s.core(1500, Event::JobAudioReady(J0)),
        [Effect::Transcribe(J0)]
    );
    let done = Event::TranscriptionDone {
        job: J0,
        empty: false,
    };
    assert_eq!(s.core(1600, done), [Effect::Paste(J0)]);
}

#[test]
fn a_lock_or_sleep_during_a_continuation_cancels_the_whole_dictation() {
    for event in [Event::Lock, Event::Suspend] {
        let mut s = first(gap_cfg());
        transcribed(&mut s, J0, 1150);
        s.at(1300, Event::ChordDown);
        // Silent, as without a continuation; both parts become one cancelled job.
        let mut fx = s.core(1500, event.clone());
        fx.retain(|e| *e != Effect::Unload);
        assert_eq!(
            fx,
            [
                Effect::StopCapture(J1),
                Effect::Merge {
                    from: vec![J0],
                    into: J1
                }
            ],
            "{event:?}"
        );
        // Nothing pastes into the locking session; the merged audio is kept as cancelled.
        let audio = Event::JobAudioReady(J1);
        assert_eq!(
            s.core(1600, audio),
            [Effect::KeepCancelled(J1)],
            "{event:?}"
        );
        assert_eq!(s.until(10_000), [], "{event:?}");
        assert!(!s.0.busy(), "{event:?}");
    }
}

#[test]
fn no_audio_or_no_text_for_a_carried_job_says_nothing_while_recording() {
    // (J0's audio came before the re-press, the answer for J0)
    let answers = [
        (
            true,
            Event::TranscriptionDone {
                job: J0,
                empty: true,
            },
        ),
        (true, Event::TranscriptionFailed(J0)),
        (
            false,
            Event::JobAudioEmpty {
                job: J0,
                why: EmptyAudio::MicSilence,
            },
        ),
    ];
    for (audio_first, answer) in answers {
        let start = |s: &mut Sim| {
            if audio_first {
                s.at(1150, Event::JobAudioReady(J0)); // J0 goes to whisper
            }
            s.at(1300, Event::ChordDown);
        };
        let mut s = first(gap_cfg());
        start(&mut s);
        // No sound, no notice, J0 stays for the merge.
        assert_eq!(s.core(1350, answer.clone()), [], "{answer:?}");
        assert_eq!(s.phase(), Phase::Recording);
        assert_eq!(s.0.superseded(J0), Some(true));
        s.at(2000, Event::ChordUp);
        assert_eq!(s.core(2100, Event::Tick), merged(&[J0], J1), "{answer:?}");
        assert_eq!(s.0.superseded(J0), None);
        let audio = Event::JobAudioReady(J1);
        assert_eq!(s.core(2150, audio), [Effect::Transcribe(J1)], "{answer:?}");

        // The continuation fails instead: J0 is given back and its answer counts now.
        let mut s = first(MachineConfig {
            keep_history: false,
            ..gap_cfg()
        });
        start(&mut s);
        s.at(1350, answer.clone());
        let fx = s.core(1500, Event::CaptureFailed);
        let whisper = !matches!(answer, Event::JobAudioEmpty { .. });
        assert_eq!(
            fx.contains(&Effect::DropJob(J0)),
            whisper,
            "{answer:?}: {fx:?}"
        );
        assert!(!fx.contains(&Effect::Paste(J0)), "{answer:?}: {fx:?}");
        s.until(10_000);
        assert!(!s.0.busy(), "{answer:?}");
    }
}

#[test]
fn a_locked_recording_and_the_length_cap_never_wait_for_a_gap() {
    // Locked by a double tap, stopped by the next press: no gap.
    let mut s = Sim::recording(gap_cfg());
    s.at(100, Event::ChordUp); // a tap
    s.at(200, Event::ChordDown);
    s.at(250, Event::SpeechHeard);
    s.at(300, Event::ChordUp); // locked
    s.at(1000, Event::ChordDown); // the stopping press
    s.at(1050, Event::ChordUp);
    assert_eq!(s.until(1100), stop(J0));
    assert_eq!(transcribed(&mut s, J0, 1150), [Effect::Paste(J0)]);

    // Released 50 ms before the cap: the cap stops it inside the tail, and that is no release.
    let cap = gap_cfg().max_recording.0;
    let mut s = Sim::recording(gap_cfg());
    s.at(100, Event::SpeechHeard);
    s.until(cap - 50);
    s.at(cap - 50, Event::ChordUp);
    assert_eq!(s.until(cap), stop(J0));
    assert_eq!(transcribed(&mut s, J0, cap + 50), [Effect::Paste(J0)]);
}

// ---- log notes: why a recording ended or was refused ------------------------------------------

fn cancelled(why: CancelWhy, speech: bool) -> Vec<Note> {
    vec![Note::Cancelled { why, speech }]
}

#[test]
fn every_cancel_names_its_reason_and_whether_speech_was_kept() {
    let mut esc = cfg(Mode::Push, OtherKey::Auto, true);
    esc.esc_cancels = true;
    let cases: [(&str, MachineConfig, Event, bool, CancelWhy); 8] = [
        ("esc", esc.clone(), Event::Esc, false, CancelWhy::Esc),
        ("esc speech", esc, Event::Esc, true, CancelWhy::Esc),
        (
            "other key",
            cfg(Mode::Push, OtherKey::Always, false),
            Event::OtherKey,
            false,
            CancelWhy::OtherKey,
        ),
        (
            "other key speech",
            cfg(Mode::Push, OtherKey::Always, false),
            Event::OtherKey,
            true,
            CancelWhy::OtherKey,
        ),
        (
            "lock",
            cfg(Mode::Push, OtherKey::Auto, false),
            Event::Lock,
            false,
            CancelWhy::Lock,
        ),
        (
            "lock speech",
            cfg(Mode::Push, OtherKey::Auto, false),
            Event::Lock,
            true,
            CancelWhy::Lock,
        ),
        (
            "suspend",
            cfg(Mode::Push, OtherKey::Auto, false),
            Event::Suspend,
            false,
            CancelWhy::Suspend,
        ),
        (
            "suspend speech",
            cfg(Mode::Push, OtherKey::Auto, false),
            Event::Suspend,
            true,
            CancelWhy::Suspend,
        ),
    ];
    for (case, config, event, speech, why) in cases {
        let mut s = Sim::recording(config);
        if speech {
            s.at(10, Event::SpeechHeard);
        }
        s.at(100, event);
        assert_eq!(s.0.take_notes(), cancelled(why, speech), "{case}");
        assert_eq!(s.0.take_notes(), [], "{case}: notes are taken once");
    }
}

#[test]
fn a_shortcut_cancel_is_logged_when_the_detector_wait_ends_and_not_before() {
    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Auto, false));
    s.at(50, Event::OtherKey);
    s.until(50 + DL - 1);
    assert_eq!(s.0.take_notes(), []);
    s.at(50 + DL, Event::Tick);
    assert_eq!(s.0.take_notes(), cancelled(CancelWhy::Shortcut, false));

    // A new press during the tail ends a pending shortcut cancel the same way.
    let mut c = cfg(Mode::Push, OtherKey::Auto, false);
    c.release_tail = Millis(300);
    let mut s = Sim::recording(c);
    s.at(50, Event::OtherKey);
    s.at(1000, Event::ChordUp);
    s.at(1010, Event::ChordDown);
    assert_eq!(s.0.take_notes(), cancelled(CancelWhy::Shortcut, false));
}

#[test]
fn a_tap_and_a_quiet_esc_are_not_cancels() {
    let mut s = Sim::recording(cfg(Mode::Push, OtherKey::Auto, true));
    s.at(100, Event::ChordUp); // a tap: discarded, not cancelled
    s.at(200, Event::Esc); // nothing recording
    assert_eq!(s.0.take_notes(), []);
}

#[test]
fn a_key_down_with_the_queue_full_is_refused_with_a_note() {
    let mut s = Sim(State::new(cfg(Mode::Push, OtherKey::Auto, false)));
    s.at(0, Event::ModelLoaded);
    s.at(0, Event::ChordDown);
    s.at(400, Event::ChordUp);
    s.at(450, Event::JobAudioReady(J0)); // job 0 goes to whisper
    for (down, up, job) in [(500, 1000, 1), (1100, 1500, 2)] {
        s.at(down, Event::ChordDown);
        s.at(up, Event::ChordUp);
        s.at(up + 50, Event::JobAudioReady(JobId(job)));
    }
    assert_eq!(s.0.take_notes(), []);
    assert_eq!(s.core(1600, Event::ChordDown), [Effect::Play(Sound::Error)]);
    assert_eq!(s.0.take_notes(), [Note::RefusedQueueFull]);
}

#[test]
fn the_reasons_read_as_the_log_names_them() {
    let names = [
        (CancelWhy::Shortcut, "shortcut"),
        (CancelWhy::Esc, "esc"),
        (CancelWhy::OtherKey, "other_key"),
        (CancelWhy::Lock, "lock"),
        (CancelWhy::Suspend, "suspend"),
    ];
    for (why, name) in names {
        assert_eq!(why.as_str(), name);
    }
}
