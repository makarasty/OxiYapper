use super::{AudioError, MIN_VOICED, SAMPLE_RATE, TRIM_PADDING, samples_for};
use crate::config::VadSensitivity;
use crate::types::Millis;
use earshot::Detector;
use std::ops::Range;

/// earshot takes exactly this many samples (16 ms at 16 kHz) per call.
const VAD_FRAME: usize = 256;
const VAD_FRAME_MS: u64 = VAD_FRAME as u64 * 1000 / SAMPLE_RATE as u64;
/// earshot keeps scoring voice after speech ends (its 48 ms window, 3 context frames, RNN state);
/// this many frames at the end of each voiced run are not counted as speech. Measured on 200-600 ms
/// 300 Hz tones, clean and over -50/-55 dBFS white noise, denoise off and on: the hold was 4-10
/// frames, runs 17-19 frames for 200 ms and 24-30 for 300 ms. 6 puts 200 ms under [`MIN_VOICED`]
/// and 300 ms over it, each by 2 frames; the full 10 would reject 300 ms of clean speech.
const HOLD_FRAMES: usize = 6;
/// A voiced run this long (80 ms, the level detector's hold) means the user is speaking.
pub(super) const SPEECH_FRAMES: usize = 5;

/// Score at or above this counts as voice; `normal` is earshot's suggested default.
fn threshold(sensitivity: VadSensitivity) -> f32 {
    match sensitivity {
        VadSensitivity::Low => 0.65,
        VadSensitivity::Normal => 0.5,
        VadSensitivity::High => 0.35,
    }
}

/// Incremental speech gate over the 16 kHz stream: every frame is scored once, as it arrives, so
/// [`Gate::span`] at key-up only counts.
pub(super) struct Gate {
    detector: Detector,
    threshold: f32,
    /// Frames scored so far; the first `frames * VAD_FRAME` samples of the stream are done.
    pub(super) frames: usize,
    /// First and last frame earshot scores as voice (hold included: it only widens the cut).
    voiced: Option<(usize, usize)>,
    /// Length of the voiced run in progress, in frames.
    pub(super) run: usize,
    /// Frames of speech in the finished runs, hold excluded.
    speech: usize,
    /// The finished runs in frames, hold excluded; a run no longer than the hold is left out.
    runs: Vec<Range<usize>>,
}

impl Gate {
    pub(super) fn new(sensitivity: VadSensitivity) -> Self {
        Self {
            detector: Detector::default(),
            threshold: threshold(sensitivity),
            frames: 0,
            voiced: None,
            run: 0,
            speech: 0,
            runs: Vec::new(),
        }
    }

    /// Score the whole frames of `stream` (all 16 kHz output so far) not yet scored.
    pub(super) fn feed(&mut self, stream: &[f32]) {
        for frame in stream[self.frames * VAD_FRAME..].as_chunks::<VAD_FRAME>().0 {
            let score = self.detector.predict_f32(frame);
            self.observe(score);
        }
    }

    /// Count one frame earshot scored `score`.
    fn observe(&mut self, score: f32) {
        if score >= self.threshold {
            let first = self.voiced.map_or(self.frames, |(first, _)| first);
            self.voiced = Some((first, self.frames));
            self.run += 1;
        } else if self.run > 0 {
            let kept = self.run.saturating_sub(HOLD_FRAMES);
            self.speech += kept;
            if kept > 0 {
                let start = self.frames - self.run;
                self.runs.push(start..start + kept);
            }
            self.run = 0;
        }
        self.frames += 1;
    }

    /// The voiced runs in frames, in order. A run still going at the end of the stream is whole:
    /// the voice reaches the last frame scored, so no hold is in it to take off.
    pub(super) fn runs(&self) -> Vec<Range<usize>> {
        let mut runs = self.runs.clone();
        if self.run > 0 {
            runs.push(self.frames - self.run..self.frames);
        }
        runs
    }

    /// The part of a `len`-sample stream to keep: the voiced span plus [`TRIM_PADDING`] each side,
    /// clamped to the stream. Silence between voiced parts is kept.
    ///
    /// # Errors
    /// [`AudioError::NothingHeard`] when less than [`MIN_VOICED`] of speech was found.
    pub(super) fn span(&self, len: usize) -> Result<Range<usize>, AudioError> {
        let speech = self.speech + self.run.saturating_sub(HOLD_FRAMES);
        match self.voiced {
            Some((first, last)) if Millis(speech as u64 * VAD_FRAME_MS) >= MIN_VOICED => {
                let pad = samples_for(TRIM_PADDING, SAMPLE_RATE);
                Ok(
                    (first * VAD_FRAME).saturating_sub(pad)
                        ..((last + 1) * VAD_FRAME + pad).min(len),
                )
            }
            _ => Err(AudioError::NothingHeard),
        }
    }
}

/// Frame range to sample range.
pub(super) fn to_samples(frames: &Range<usize>) -> Range<usize> {
    frames.start * VAD_FRAME..frames.end * VAD_FRAME
}

/// The parts of `samples` (16 kHz mono) the trim scores as voice, as sample ranges in order, not
/// overlapping. The same scoring the trim uses, at `sensitivity`.
#[must_use]
pub fn voiced_runs(samples: &[f32], sensitivity: VadSensitivity) -> Vec<Range<usize>> {
    let mut gate = Gate::new(sensitivity);
    gate.feed(samples);
    gate.runs().iter().map(to_samples).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` frames scored `score`.
    fn scores(gate: &mut Gate, score: f32, n: usize) {
        for _ in 0..n {
            gate.observe(score);
        }
    }

    #[test]
    fn audio_vad_sensitivity_moves_the_voice_threshold() {
        // 40 frames (640 ms) scored 0.4: voice only on `high`.
        for (sensitivity, kept) in [
            (VadSensitivity::High, true),
            (VadSensitivity::Normal, false),
            (VadSensitivity::Low, false),
        ] {
            let mut gate = Gate::new(sensitivity);
            scores(&mut gate, 0.0, 10);
            scores(&mut gate, 0.4, 40);
            scores(&mut gate, 0.0, 10);
            assert_eq!(gate.span(60 * VAD_FRAME).is_ok(), kept, "{sensitivity:?}");
        }
        // 0.6 is voice on `normal`, not on `low`.
        let mut low = Gate::new(VadSensitivity::Low);
        scores(&mut low, 0.6, 40);
        assert!(low.span(40 * VAD_FRAME).is_err());
    }

    #[test]
    fn audio_runs_drop_the_hold_except_at_the_end() {
        let mut gate = Gate::new(VadSensitivity::Normal);
        scores(&mut gate, 0.0, 2);
        scores(&mut gate, 0.9, 20);
        scores(&mut gate, 0.0, 30);
        scores(&mut gate, 0.9, 4); // a blip shorter than the hold: no run
        scores(&mut gate, 0.0, 5);
        scores(&mut gate, 0.9, 10); // still voiced at the end
        assert_eq!(gate.runs(), [2..16, 61..71]);
    }
}
