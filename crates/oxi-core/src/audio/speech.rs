use super::{Level, db_to_linear};
use crate::types::Millis;

/// A window at or above this RMS (post-gain, dBFS) counts as the user speaking. A key click or
/// room noise sits well below; speech at a normal distance is 10-20 dB above.
const SPEECH_RMS_DBFS: f32 = -40.0;
/// The level must hold this long without a gap: a click is a few milliseconds.
const SPEECH_HOLD: Millis = Millis(80);
/// Worst input time from the first spoken sound to the gate path reporting speech (a quiet voice
/// under the level; `audio_quiet_short_phrase_is_heard_and_kept` pins it with a -45 dBFS tone).
pub(super) const GATE_REPORT_MAX: Millis = Millis(150);
/// The level path: the hold plus the one 20 ms window that carries it.
pub(super) const LEVEL_REPORT_MAX: Millis = Millis(SPEECH_HOLD.0 + 20);
/// What is added after the pipeline reports: the 10 ms capture drain poll, ~20 ms of device buffer
/// and the hop to the coordinator with margin.
pub(super) const REPORT_OVERHEAD: Millis = Millis(50);
/// Worst time from the first spoken sound to [`crate::machine::Event::SpeechHeard`] reaching the
/// machine, over the slower of the two paths (the gate: a cancel must wait for it too, or a stray
/// key just after a quiet start cancels a recording that has speech). A cancel waits this long.
pub const DETECTOR_LATENCY: Millis = Millis(GATE_REPORT_MAX.0 + REPORT_OVERHEAD.0);
// The budget covers both report paths.
const _: () = assert!(LEVEL_REPORT_MAX.0 + REPORT_OVERHEAD.0 <= DETECTOR_LATENCY.0);

/// Says when the user started speaking, from the level of each window over the capture so far.
/// [`super::Pipeline::speech_started`] joins it with the speech gate (a quiet voice under the
/// level); the machine uses that to tell a shortcut pressed before any speech from a stray key
/// during dictation (R2.1).
#[derive(Debug, Clone, Default)]
pub struct SpeechDetector {
    /// When the level went over the threshold, if it is over now.
    loud_since: Option<Millis>,
}

impl SpeechDetector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the meter level read at `now` (any rate, repeated windows are harmless); true when
    /// speech has been heard.
    pub fn feed(&mut self, level: Level, now: Millis) -> bool {
        if level.rms < db_to_linear(SPEECH_RMS_DBFS) {
            self.loud_since = None;
            return false;
        }
        let since = *self.loud_since.get_or_insert(now);
        now - since >= SPEECH_HOLD
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(rms: f32) -> Level {
        Level { peak: rms, rms }
    }

    #[test]
    fn speech_needs_a_sustained_level() {
        let mut d = SpeechDetector::new();
        assert!(!d.feed(level(0.0), Millis(0)));
        assert!(!d.feed(level(0.2), Millis(10)));
        assert!(!d.feed(level(0.2), Millis(50)));
        assert!(d.feed(level(0.2), Millis(90)));
    }

    #[test]
    fn a_click_is_not_speech() {
        let mut d = SpeechDetector::new();
        // Loud for 20 ms, quiet, loud again: the hold restarts each time.
        assert!(!d.feed(level(0.5), Millis(0)));
        assert!(!d.feed(level(0.5), Millis(20)));
        assert!(!d.feed(level(0.001), Millis(40)));
        assert!(!d.feed(level(0.5), Millis(60)));
        assert!(!d.feed(level(0.5), Millis(130)));
        assert!(d.feed(level(0.5), Millis(140)));
    }

    #[test]
    fn room_noise_is_not_speech() {
        let mut d = SpeechDetector::new();
        for t in (0..2000).step_by(10) {
            assert!(!d.feed(level(0.003), Millis(t)), "{t}");
        }
    }
}
