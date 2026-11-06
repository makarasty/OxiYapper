use super::denoise::Denoiser;
use super::resample::StreamResampler;
use super::trim::Gate;
use super::{AudioError, DENOISE_RATE, Level, SAMPLE_RATE, db_to_linear, level, normalise};
use crate::config::Audio;

/// The whole DSP chain for one dictation, steps 3-7 of ARCHITECTURE section 9.
///
/// Feed it mono samples ([`super::downmix`]) at the device rate as they are drained, any length
/// (capture uses [`super::CHUNK`]). It applies `gain_db` itself, then denoises (if on), resamples to
/// 16 kHz and runs the speech gate (if `trim` is on) while recording, so [`Pipeline::finish`] at
/// key-up only flushes the last chunk, cuts and normalises.
pub struct Pipeline {
    gain: f32,
    /// Device rate to 48 kHz, then `RNNoise`, when denoise is on.
    denoise: Option<(StreamResampler, Denoiser)>,
    to_16k: StreamResampler,
    gate: Option<Gate>,
    normalize: bool,
    /// Any non-zero input sample so far.
    heard: bool,
    out: Vec<f32>,
}

impl Pipeline {
    /// Uses `gain_db`, `denoise`, `trim` and `normalize` of `opts`.
    ///
    /// # Errors
    /// [`AudioError::Resample`] for a rate of zero.
    pub fn new(in_rate: u32, opts: &Audio) -> Result<Self, AudioError> {
        let denoise = if opts.denoise {
            Some((
                StreamResampler::new(in_rate, DENOISE_RATE)?,
                Denoiser::new(),
            ))
        } else {
            None
        };
        let from = if opts.denoise { DENOISE_RATE } else { in_rate };
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a gain of a few dB fits f32 with room to spare"
        )]
        let gain = db_to_linear(opts.gain_db as f32);
        Ok(Self {
            gain,
            denoise,
            to_16k: StreamResampler::new(from, SAMPLE_RATE)?,
            gate: opts.trim.then(Gate::new),
            normalize: opts.normalize,
            heard: false,
            out: Vec::new(),
        })
    }

    /// Add mono samples; returns their level after gain, for the meter.
    ///
    /// # Errors
    /// [`AudioError::Resample`] if rubato rejects a buffer.
    pub fn push(&mut self, mono: &[f32]) -> Result<Level, AudioError> {
        let gained: Vec<f32> = mono.iter().map(|s| s * self.gain).collect();
        self.heard |= gained.iter().any(|&s| s != 0.0);
        let at_16k = match &mut self.denoise {
            Some((to_48k, rnn)) => self.to_16k.push(&rnn.push(&to_48k.push(&gained)?))?,
            None => self.to_16k.push(&gained)?,
        };
        self.append(&at_16k);
        Ok(level(&gained))
    }

    /// Key-up: the 16 kHz clip for whisper.
    ///
    /// # Errors
    /// [`AudioError::MicSilence`] for all-zero input, checked first; then
    /// [`AudioError::NothingHeard`] from the speech gate; [`AudioError::Resample`] from the flush.
    pub fn finish(mut self) -> Result<Vec<f32>, AudioError> {
        self.flush()?;
        if !self.heard {
            return Err(AudioError::MicSilence);
        }
        let keep = match &self.gate {
            Some(gate) => gate.span(self.out.len())?,
            None => 0..self.out.len(),
        };
        let mut clip = self.out;
        clip.truncate(keep.end);
        clip.drain(..keep.start);
        if self.normalize {
            normalise(&mut clip);
        }
        Ok(clip)
    }

    /// Push what is still inside the resamplers and the denoiser through.
    fn flush(&mut self) -> Result<(), AudioError> {
        let mut tail = match &mut self.denoise {
            Some((to_48k, rnn)) => {
                let mut at_48k = rnn.push(&to_48k.finish()?);
                at_48k.extend(rnn.finish());
                self.to_16k.push(&at_48k)?
            }
            None => Vec::new(),
        };
        tail.extend(self.to_16k.finish()?);
        self.append(&tail);
        Ok(())
    }

    fn append(&mut self, at_16k: &[f32]) {
        self.out.extend_from_slice(at_16k);
        if let Some(gate) = &mut self.gate {
            gate.feed(&self.out);
        }
    }
}

/// A whole recording in one call (bench): `mono` at `in_rate` through a [`Pipeline`].
///
/// # Errors
/// As [`Pipeline::new`] and [`Pipeline::finish`].
pub fn process(mono: &[f32], in_rate: u32, opts: &Audio) -> Result<Vec<f32>, AudioError> {
    let mut pipeline = Pipeline::new(in_rate, opts)?;
    pipeline.push(mono)?;
    pipeline.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{CHUNK, samples_for};
    use std::f32::consts::TAU;

    #[test]
    fn finish_scores_only_the_flushed_tail() {
        // 60 s of tone at 48 kHz in 20 ms chunks: the gate keeps up while recording.
        let rate = 48_000;
        let mut p = Pipeline::new(rate, &Audio::default()).unwrap();
        let chunk = samples_for(CHUNK, rate);
        for c in 0..60 * 50 {
            let tone: Vec<f32> = (0..chunk)
                .map(|i| 0.5 * (TAU * 300.0 * (c * chunk + i) as f32 / rate as f32).sin())
                .collect();
            p.push(&tone).unwrap();
        }
        let before = p.gate.as_ref().unwrap().frames;
        assert!(
            before >= 60 * 16_000 / 256 - 4,
            "only {before} frames scored while recording"
        );
        p.flush().unwrap();
        let in_finish = p.gate.as_ref().unwrap().frames - before;
        assert!(in_finish <= 4, "{in_finish} frames scored at key-up");
    }
}
