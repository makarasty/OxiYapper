use super::{AudioError, MIN_VOICED, SAMPLE_RATE, TRIM_PADDING, level, samples_for};
use crate::types::Millis;
use earshot::Detector;
use std::ops::Range;

/// earshot takes exactly this many samples (16 ms at 16 kHz) per call.
const VAD_FRAME: usize = 256;
const VAD_FRAME_MS: u64 = VAD_FRAME as u64 * 1000 / SAMPLE_RATE as u64;
/// Score at or above this counts as voice (earshot's suggested default).
const VOICE_THRESHOLD: f32 = 0.5;
/// earshot keeps scoring voice for 50-160 ms after speech ends (its window and RNN state). Such a
/// frame is no louder than the noise floor, so a voiced frame counts as speech only above
/// floor * this (12 dB).
const HOLD_MARGIN: f32 = 4.0;

/// Incremental speech gate over the 16 kHz stream: every frame is scored once, as it arrives, so
/// [`Gate::span`] at key-up only counts.
pub(super) struct Gate {
    detector: Detector,
    /// Frames scored so far; the first `frames * VAD_FRAME` samples of the stream are done.
    pub(super) frames: usize,
    /// First and last frame earshot scores as voice (hold included: it only widens the cut).
    voiced: Option<(usize, usize)>,
    /// RMS of each voiced frame, judged against the final noise floor in `span`.
    voiced_rms: Vec<f32>,
    /// Quietest non-silent frame earshot scores as not voice.
    floor: Option<f32>,
}

impl Gate {
    pub(super) fn new() -> Self {
        Self {
            detector: Detector::default(),
            frames: 0,
            voiced: None,
            voiced_rms: Vec::new(),
            floor: None,
        }
    }

    /// Score the whole frames of `stream` (all 16 kHz output so far) not yet scored.
    pub(super) fn feed(&mut self, stream: &[f32]) {
        for frame in stream[self.frames * VAD_FRAME..].as_chunks::<VAD_FRAME>().0 {
            let rms = level(frame).rms;
            if self.detector.predict_f32(frame) >= VOICE_THRESHOLD {
                let first = self.voiced.map_or(self.frames, |(first, _)| first);
                self.voiced = Some((first, self.frames));
                self.voiced_rms.push(rms);
            } else if rms > 0.0 {
                self.floor = Some(self.floor.map_or(rms, |f| f.min(rms)));
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
        let min_rms = self.floor.unwrap_or(0.0) * HOLD_MARGIN;
        let speech = self.voiced_rms.iter().filter(|&&r| r > min_rms).count();
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
