use super::{AudioError, MIN_VOICED, SAMPLE_RATE, TRIM_PADDING, samples_for};
use crate::types::Millis;
use earshot::Detector;

/// earshot takes exactly this many samples (16 ms at 16 kHz) per call.
const VAD_FRAME: usize = 256;
const VAD_FRAME_MS: u64 = VAD_FRAME as u64 * 1000 / SAMPLE_RATE as u64;
/// Score at or above this counts as voice (earshot's suggested default).
const VOICE_THRESHOLD: f32 = 0.5;

/// Cut silence at both ends of 16 kHz mono audio, keeping [`TRIM_PADDING`] on each side.
/// Silence between voiced parts is kept.
///
/// # Errors
/// [`AudioError::NothingHeard`] when less than [`MIN_VOICED`] of speech is found.
pub fn trim(samples: &[f32]) -> Result<&[f32], AudioError> {
    let mut detector = Detector::default();
    let voiced: Vec<usize> = samples
        .as_chunks::<VAD_FRAME>()
        .0
        .iter()
        .enumerate()
        .filter(|(_, frame)| detector.predict_f32(&frame[..]) >= VOICE_THRESHOLD)
        .map(|(i, _)| i)
        .collect();
    let (Some(&first), Some(&last)) = (voiced.first(), voiced.last()) else {
        return Err(AudioError::NothingHeard);
    };
    if Millis(voiced.len() as u64 * VAD_FRAME_MS) < MIN_VOICED {
        return Err(AudioError::NothingHeard);
    }
    let pad = samples_for(TRIM_PADDING, SAMPLE_RATE);
    let start = (first * VAD_FRAME).saturating_sub(pad);
    let end = ((last + 1) * VAD_FRAME + pad).min(samples.len());
    Ok(&samples[start..end])
}
