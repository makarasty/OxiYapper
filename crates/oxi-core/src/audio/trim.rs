use super::{AudioError, MIN_VOICED, SAMPLE_RATE, TRIM_PADDING, samples_for};
use crate::types::Millis;
use earshot::Detector;
use std::ops::Range;

/// earshot takes exactly this many samples (16 ms at 16 kHz) per call.
const VAD_FRAME: usize = 256;
const VAD_FRAME_MS: u64 = VAD_FRAME as u64 * 1000 / SAMPLE_RATE as u64;
/// Score at or above this counts as voice (earshot's suggested default).
const VOICE_THRESHOLD: f32 = 0.5;
/// earshot keeps scoring voice after speech ends (its 48 ms window, 3 context frames, RNN state);
/// this many frames at the end of each voiced run are not counted as speech. Measured on 200-600 ms
/// 300 Hz tones, clean and over -50/-55 dBFS white noise, denoise off and on: the hold was 4-10
/// frames, runs 17-19 frames for 200 ms and 24-30 for 300 ms. 6 puts 200 ms under [`MIN_VOICED`]
/// and 300 ms over it, each by 2 frames; the full 10 would reject 300 ms of clean speech.
const HOLD_FRAMES: usize = 6;

/// Incremental speech gate over the 16 kHz stream: every frame is scored once, as it arrives, so
/// [`Gate::span`] at key-up only counts.
pub(super) struct Gate {
    detector: Detector,
    /// Frames scored so far; the first `frames * VAD_FRAME` samples of the stream are done.
    pub(super) frames: usize,
    /// First and last frame earshot scores as voice (hold included: it only widens the cut).
    voiced: Option<(usize, usize)>,
    /// Length of the voiced run in progress, in frames.
    run: usize,
    /// Frames of speech in the finished runs, hold excluded.
    speech: usize,
}

impl Gate {
    pub(super) fn new() -> Self {
        Self {
            detector: Detector::default(),
            frames: 0,
            voiced: None,
            run: 0,
            speech: 0,
        }
    }

    /// Score the whole frames of `stream` (all 16 kHz output so far) not yet scored.
    pub(super) fn feed(&mut self, stream: &[f32]) {
        for frame in stream[self.frames * VAD_FRAME..].as_chunks::<VAD_FRAME>().0 {
            if self.detector.predict_f32(frame) >= VOICE_THRESHOLD {
                let first = self.voiced.map_or(self.frames, |(first, _)| first);
                self.voiced = Some((first, self.frames));
                self.run += 1;
            } else if self.run > 0 {
                self.speech += self.run.saturating_sub(HOLD_FRAMES);
                self.run = 0;
            }
            self.frames += 1;
        }
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
