//! Audio DSP: mono, gain, level, denoise, resample, trim, normalise (ARCHITECTURE section 9, steps 3-7).
//! Pure code on `f32` samples in `[-1, 1]`. Callers use [`downmix`], then only [`Pipeline`] (capture)
//! or [`process`] (bench), so the steps cannot run in the wrong order.

mod denoise;
mod pipeline;
mod resample;
mod trim;

pub use pipeline::{Pipeline, process};

use crate::types::Millis;

/// Sample rate whisper and the speech gate work at.
pub const SAMPLE_RATE: u32 = 16_000;
/// Streaming chunk length for resampling; capture drains the ring in chunks of this length.
pub const CHUNK: Millis = Millis(20);
/// One chunk at [`SAMPLE_RATE`].
pub const CHUNK_SAMPLES: usize = samples_for(CHUNK, SAMPLE_RATE);
/// `RNNoise` only works at this rate.
const DENOISE_RATE: u32 = 48_000;
/// `RNNoise` frame length in samples at [`DENOISE_RATE`].
const DENOISE_FRAME: usize = 480;
/// Silence kept on both sides of the voiced part by the speech gate.
pub const TRIM_PADDING: Millis = Millis(200);
/// Less voiced audio than this is "nothing heard".
pub const MIN_VOICED: Millis = Millis(250);
/// Normalising aims at this RMS level (dBFS).
pub const TARGET_RMS_DBFS: f32 = -20.0;
/// Normalising never amplifies more than this.
pub const MAX_GAIN_DB: f32 = 20.0;
/// Normalising keeps the peak at or below this (linear, about -0.9 dBFS).
pub const PEAK_LIMIT: f32 = 0.9;

/// Number of samples in `ms` at `rate` Hz.
#[must_use]
pub const fn samples_for(ms: Millis, rate: u32) -> usize {
    (ms.0 * rate as u64 / 1000) as usize
}

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    /// Less than [`MIN_VOICED`] of speech: no whisper call.
    #[error("nothing heard")]
    NothingHeard,
    /// All samples are zero: hardware mute or a privacy switch.
    #[error("microphone gives silence")]
    MicSilence,
    #[error("resampler: {0}")]
    Resample(String),
}

/// Peak and RMS of a block of samples, both linear and non-negative.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level {
    pub peak: f32,
    pub rms: f32,
}

impl Level {
    /// The level of the same samples after a linear `gain`: both measures scale with it.
    #[must_use]
    pub fn scaled(self, gain: f32) -> Self {
        Self {
            peak: self.peak * gain,
            rms: self.rms * gain,
        }
    }
}

/// Average the interleaved channels into one.
#[must_use]
pub fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

/// Peak and RMS of `samples`; zero for an empty slice.
#[must_use]
pub fn level(samples: &[f32]) -> Level {
    if samples.is_empty() {
        return Level {
            peak: 0.0,
            rms: 0.0,
        };
    }
    let peak = samples.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
    let mean_sq = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
    Level {
        peak,
        rms: mean_sq.sqrt(),
    }
}

/// Scale `samples` toward [`TARGET_RMS_DBFS`]: at most [`MAX_GAIN_DB`], never past [`PEAK_LIMIT`].
/// All zeros stay zeros (the gain falls to the cap); [`Pipeline`] reports that case as `MicSilence` first.
fn normalise(samples: &mut [f32]) {
    let Level { peak, rms } = level(samples);
    let gain = (db_to_linear(TARGET_RMS_DBFS) / rms)
        .min(db_to_linear(MAX_GAIN_DB))
        .min(PEAK_LIMIT / peak);
    for s in samples {
        *s *= gain;
    }
}

/// A whole clip from `from` Hz to `to` Hz (the mic test plays 16 kHz audio on the output rate).
///
/// # Errors
/// [`AudioError::Resample`] for a rate of zero or a buffer rubato rejects.
pub fn resample(samples: &[f32], from: u32, to: u32) -> Result<Vec<f32>, AudioError> {
    let mut r = resample::StreamResampler::new(from, to)?;
    let mut out = r.push(samples)?;
    out.extend(r.finish()?);
    Ok(out)
}

/// Decibels to a linear amplitude factor.
#[must_use]
pub fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_keeps_duration_and_level() {
        let tone: Vec<f32> = (0..16_000)
            .map(|i| 0.5 * (std::f32::consts::TAU * 440.0 * i as f32 / 16_000.0).sin())
            .collect();
        let out = resample(&tone, 16_000, 48_000).unwrap();
        assert_eq!(out.len(), 48_000);
        let (a, b) = (level(&tone), level(&out[480..out.len() - 480]));
        assert!((a.rms - b.rms).abs() < 0.02, "{a:?} {b:?}");
    }

    /// The settings meter opens its stream at 0 dB and scales the level by the gain instead.
    #[test]
    fn scaled_level_equals_the_level_after_gain() {
        let mono = [0.1, -0.2, 0.05];
        let raw = level(&mono);
        for gain_db in [-12.0_f32, 0.0, 6.0, 20.0] {
            let gain = db_to_linear(gain_db);
            let gained: Vec<f32> = mono.iter().map(|s| s * gain).collect();
            let (after, scaled) = (level(&gained), raw.scaled(gain));
            assert!((after.peak - scaled.peak).abs() < 1e-6, "{gain_db}");
            assert!((after.rms - scaled.rms).abs() < 1e-6, "{gain_db}");
        }
        assert!((db_to_linear(6.0) - 1.9953).abs() < 1e-4);
    }
}
