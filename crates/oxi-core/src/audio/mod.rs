//! Audio DSP: mono, gain, level, denoise, resample, trim, normalise (ARCHITECTURE section 9, steps 3-7).
//! Pure functions on `f32` samples in `[-1, 1]`; the capture side (t07) imports the constants below.

mod denoise;
mod resample;
mod trim;

pub use denoise::Denoiser;
pub use resample::StreamResampler;
pub use trim::trim;

use crate::types::Millis;

/// Sample rate whisper and the speech gate work at.
pub const SAMPLE_RATE: u32 = 16_000;
/// Streaming chunk length for resampling and level metering.
pub const CHUNK: Millis = Millis(20);
/// One chunk at [`SAMPLE_RATE`].
pub const CHUNK_SAMPLES: usize = samples_for(CHUNK, SAMPLE_RATE);
/// `RNNoise` only works at this rate.
pub const DENOISE_RATE: u32 = 48_000;
/// `RNNoise` frame length in samples at [`DENOISE_RATE`].
pub const DENOISE_FRAME: usize = 480;
/// Silence kept on both sides of the voiced part by [`trim`].
pub const TRIM_PADDING: Millis = Millis(200);
/// Less voiced audio than this is "nothing heard".
pub const MIN_VOICED: Millis = Millis(250);
/// [`normalise`] aims at this RMS level (dBFS).
pub const TARGET_RMS_DBFS: f32 = -20.0;
/// [`normalise`] never amplifies more than this.
pub const MAX_GAIN_DB: f32 = 20.0;
/// [`normalise`] keeps the peak at or below this (linear, about -0.9 dBFS).
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

/// Multiply by `db` decibels (manual gain, applied before the meter).
pub fn apply_gain_db(samples: &mut [f32], db: f32) {
    let gain = db_to_linear(db);
    for s in samples {
        *s *= gain;
    }
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

/// [`level`] per [`CHUNK`] at `rate`; the last block may be shorter.
#[must_use]
pub fn levels(samples: &[f32], rate: u32) -> Vec<Level> {
    samples
        .chunks(samples_for(CHUNK, rate).max(1))
        .map(level)
        .collect()
}

/// Scale `samples` toward [`TARGET_RMS_DBFS`]: at most [`MAX_GAIN_DB`], never past [`PEAK_LIMIT`].
/// Returns the gain applied in dB.
///
/// # Errors
/// All zeros (or empty) is [`AudioError::MicSilence`]; the samples are left alone.
pub fn normalise(samples: &mut [f32]) -> Result<f32, AudioError> {
    let Level { peak, rms } = level(samples);
    if peak == 0.0 {
        return Err(AudioError::MicSilence);
    }
    let gain = (db_to_linear(TARGET_RMS_DBFS) / rms)
        .min(db_to_linear(MAX_GAIN_DB))
        .min(PEAK_LIMIT / peak);
    for s in samples {
        *s *= gain;
    }
    Ok(20.0 * gain.log10())
}

fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}
