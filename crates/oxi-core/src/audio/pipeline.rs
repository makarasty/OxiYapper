use super::denoise::Denoiser;
use super::resample::StreamResampler;
use super::speech::SpeechDetector;
use super::trim::{Gate, SPEECH_FRAMES, to_samples};
use super::{AudioError, DENOISE_RATE, Level, SAMPLE_RATE, db_to_linear, level, normalise};
use crate::config::Audio;
use crate::types::Millis;

/// What the speech gate did to a finished recording.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Trim {
    /// The gate found the voiced span; the silence around it (past the padding) is cut.
    Cut,
    /// The gate found too little voice but the level heard speech: the whole audio is kept.
    Skipped,
    /// `audio.trim` is off: no gate ran.
    #[default]
    Off,
}

/// Where the voice sits in a recording, for the log (never the audio itself). All zero or `None`
/// with the trim off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EdgeStats {
    /// The whole recording at 16 kHz, before the cut.
    pub audio_ms: u64,
    /// Time the gate scored as voice.
    pub voiced_ms: u64,
    /// From the first sample to the first voice: under 60 ms the user spoke before the mic was ready.
    pub first_voiced_ms: Option<u64>,
    /// From the last voice to the last sample: under 60 ms the tail cut the speech.
    pub last_voiced_to_end_ms: Option<u64>,
}

/// A finished recording: the clip for whisper and what the gate saw.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Finished {
    pub samples: Vec<f32>,
    pub trim: Trim,
    pub stats: EdgeStats,
}

/// What has told a [`Pipeline`] the user speaks so far; a later, stronger sign replaces a weaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Heard {
    Nothing,
    /// The gate scored [`SPEECH_FRAMES`] of voice in a row.
    Voice,
    /// The level detector fired (over -40 dBFS for 80 ms).
    Loud,
}

/// Milliseconds of `n` samples at 16 kHz.
fn ms(n: usize) -> u64 {
    n as u64 * 1000 / u64::from(SAMPLE_RATE)
}

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
    in_rate: u32,
    /// Input samples so far: the clock of the level detector.
    in_samples: u64,
    level: SpeechDetector,
    speech: Heard,
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
            gate: opts.trim.then(|| Gate::new(opts.vad_sensitivity)),
            normalize: opts.normalize,
            heard: false,
            in_rate,
            in_samples: 0,
            level: SpeechDetector::new(),
            speech: Heard::Nothing,
            out: Vec::new(),
        })
    }

    /// Add mono samples; returns their level after gain, for the meter. Push in windows of
    /// [`super::CHUNK`] (as capture does): the level detector judges each push as one window.
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
        let level = level(&gained);
        self.in_samples += mono.len() as u64;
        let now = Millis(self.in_samples * 1000 / u64::from(self.in_rate));
        if self.level.feed(level, now) {
            self.speech = Heard::Loud;
        }
        Ok(level)
    }

    /// True once the user is heard speaking: the level was over -40 dBFS for 80 ms, or the gate
    /// scored 80 ms of voice in a row (a quiet voice under the level). Never goes back to false.
    #[must_use]
    pub fn speech_started(&self) -> bool {
        self.speech != Heard::Nothing
    }

    /// Key-up: the 16 kHz clip for whisper.
    ///
    /// # Errors
    /// As [`Pipeline::finish_with_stats`].
    pub fn finish(self) -> Result<Vec<f32>, AudioError> {
        self.finish_with_stats().map(|f| f.samples)
    }

    /// Key-up: the 16 kHz clip for whisper, with what the gate did and saw. When the gate finds
    /// too little voice but the level heard speech, the whole audio is kept ([`Trim::Skipped`]):
    /// a recording the user was heard in is never thrown away by the trim.
    ///
    /// # Errors
    /// [`AudioError::MicSilence`] for all-zero input, checked first; then
    /// [`AudioError::NothingHeard`] when the gate found too little voice and the level never
    /// fired; [`AudioError::Resample`] from the flush.
    pub fn finish_with_stats(mut self) -> Result<Finished, AudioError> {
        self.flush()?;
        if !self.heard {
            return Err(AudioError::MicSilence);
        }
        let len = self.out.len();
        let (keep, trim, stats) = match &self.gate {
            Some(gate) => {
                let (keep, trim) = match gate.span(len) {
                    Ok(keep) => (keep, Trim::Cut),
                    Err(_) if self.speech >= Heard::Voice => (0..len, Trim::Skipped),
                    Err(e) => return Err(e),
                };
                let runs: Vec<_> = gate.runs().iter().map(to_samples).collect();
                let stats = EdgeStats {
                    audio_ms: ms(len),
                    voiced_ms: runs.iter().map(|r| ms(r.len())).sum(),
                    first_voiced_ms: runs.first().map(|r| ms(r.start)),
                    last_voiced_to_end_ms: runs.last().map(|r| ms(len.saturating_sub(r.end))),
                };
                (keep, trim, stats)
            }
            None => (
                0..len,
                Trim::Off,
                EdgeStats {
                    audio_ms: ms(len),
                    ..EdgeStats::default()
                },
            ),
        };
        let mut clip = self.out;
        clip.truncate(keep.end);
        clip.drain(..keep.start);
        if self.normalize {
            normalise(&mut clip);
        }
        Ok(Finished {
            samples: clip,
            trim,
            stats,
        })
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
            if gate.run >= SPEECH_FRAMES {
                self.speech = self.speech.max(Heard::Voice);
            }
        }
    }
}

/// A whole recording in one call (bench, the recognition test): `mono` at `in_rate` through a
/// [`Pipeline`] in one push, so the level detector never fires and the gate alone decides
/// "nothing heard", as before (bench numbers stay comparable).
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

/// Synthetic signals at 16 kHz (ARCHITECTURE section 4.4 of the run plan): 300 Hz tones, which
/// earshot scores as voice, white noise, silence.
#[cfg(test)]
mod signals {
    use super::*;
    use crate::audio::speech::{DETECTOR_LATENCY, GATE_REPORT_MAX, REPORT_OVERHEAD};
    use crate::audio::{CHUNK_SAMPLES, voiced_runs};
    use crate::config::VadSensitivity;
    use std::f32::consts::TAU;

    const MS: usize = SAMPLE_RATE as usize / 1000;

    fn opts() -> Audio {
        Audio {
            denoise: false,
            ..Audio::default()
        }
    }

    /// Peak of a sine with an RMS of `dbfs`.
    fn sine_amp(dbfs: f32) -> f32 {
        db_to_linear(dbfs) * 2.0_f32.sqrt()
    }

    fn tone(ms: usize, dbfs: f32) -> Vec<f32> {
        let amp = sine_amp(dbfs);
        (0..ms * MS)
            .map(|i| amp * (TAU * 300.0 * i as f32 / SAMPLE_RATE as f32).sin())
            .collect()
    }

    /// Deterministic uniform white noise with an RMS of `dbfs`.
    fn noise(ms: usize, dbfs: f32) -> Vec<f32> {
        let amp = db_to_linear(dbfs) * 3.0_f32.sqrt();
        let mut state = 12345_u32;
        (0..ms * MS)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                amp * ((state >> 8) as f32 / (1 << 23) as f32 - 1.0)
            })
            .collect()
    }

    fn silence(ms: usize) -> Vec<f32> {
        noise(ms, -70.0)
    }

    /// Pushed in 20 ms windows as capture does; also the input time (ms) when speech was first
    /// reported.
    fn record(parts: &[Vec<f32>], opts: &Audio) -> (Pipeline, Option<usize>) {
        let s = parts.concat();
        let mut p = Pipeline::new(SAMPLE_RATE, opts).unwrap();
        let mut heard_at = None;
        for (i, window) in s.chunks(CHUNK_SAMPLES).enumerate() {
            p.push(window).unwrap();
            if heard_at.is_none() && p.speech_started() {
                heard_at = Some((i + 1) * 20);
            }
        }
        (p, heard_at)
    }

    #[test]
    fn audio_quiet_short_phrase_is_heard_and_kept() {
        // 300 ms at -45 dBFS: under the level detector, the gate alone hears it.
        let (p, heard_at) = record(&[silence(1000), tone(300, -45.0), silence(1000)], &opts());
        let at = heard_at.expect("speech heard");
        let after = at - 1000;
        assert!(
            after as u64 <= GATE_REPORT_MAX.0,
            "heard {after} ms after the start"
        );
        // The machine waits DETECTOR_LATENCY for a cancel: the report plus its transport fits.
        assert!(after as u64 + REPORT_OVERHEAD.0 <= DETECTOR_LATENCY.0);
        let f = p.finish_with_stats().unwrap();
        assert_eq!(f.trim, Trim::Cut);
        assert!(f.samples.len() >= 300 * MS, "{}", f.samples.len());
    }

    #[test]
    fn audio_quiet_short_word_the_machine_heard_is_kept_whole() {
        // 200 ms at -45 dBFS: the gate reports speech (SpeechHeard is sent) but its voiced
        // total is under the 250 ms minimum. A recording announced as speech is never dropped.
        let (p, heard_at) = record(&[silence(1000), tone(200, -45.0), silence(1000)], &opts());
        assert!(heard_at.is_some(), "the gate hears it");
        let f = p.finish_with_stats().unwrap();
        assert_eq!(f.trim, Trim::Skipped);
        assert!(f.samples.len().abs_diff(2200 * MS) <= CHUNK_SAMPLES);
    }

    #[test]
    fn audio_loud_non_voice_is_kept_whole() {
        let (p, heard_at) = record(&[silence(500), noise(500, -30.0), silence(500)], &opts());
        assert!(heard_at.is_some(), "the level hears it");
        let f = p.finish_with_stats().unwrap();
        assert_eq!(f.trim, Trim::Skipped);
        assert!(
            f.samples.len().abs_diff(1500 * MS) <= CHUNK_SAMPLES,
            "{}",
            f.samples.len()
        );
    }

    #[test]
    fn audio_silence_is_nothing_heard() {
        let (p, heard_at) = record(&[silence(2000)], &opts());
        assert_eq!(heard_at, None);
        assert!(matches!(
            p.finish_with_stats(),
            Err(AudioError::NothingHeard)
        ));
    }

    #[test]
    fn audio_long_pause_keeps_one_span() {
        let parts = [
            silence(500),
            tone(400, -20.0),
            silence(3000),
            tone(400, -20.0),
            silence(500),
        ];
        let (p, _) = record(&parts, &opts());
        let f = p.finish_with_stats().unwrap();
        assert_eq!(f.trim, Trim::Cut);
        assert!(
            f.samples.len() >= 3800 * MS,
            "pause cut: {}",
            f.samples.len()
        );
        assert!(f.stats.voiced_ms.abs_diff(800) <= 150, "{:?}", f.stats);
        assert!(f.stats.audio_ms.abs_diff(4800) <= 20, "{:?}", f.stats);

        let runs = voiced_runs(&parts.concat(), VadSensitivity::Normal);
        assert_eq!(runs.len(), 2, "{runs:?}");
        for r in &runs {
            assert!((r.len() / MS).abs_diff(400) <= 100, "{runs:?}");
        }
        assert_eq!(voiced_runs(&silence(2000), VadSensitivity::Normal), []);
    }

    #[test]
    fn audio_phrase_ending_at_release_keeps_its_end() {
        let (p, _) = record(&[silence(500), tone(600, -20.0)], &opts());
        let f = p.finish_with_stats().unwrap();
        assert_eq!(f.trim, Trim::Cut);
        let to_end = f.stats.last_voiced_to_end_ms.unwrap();
        assert!(to_end < 60, "{:?}", f.stats);
        // The first 300 ms before the padding go, the end stays.
        assert!(f.samples.len() >= 800 * MS - 3 * 256, "{}", f.samples.len());
        assert!(
            f.stats.first_voiced_ms.unwrap().abs_diff(500) <= 60,
            "{:?}",
            f.stats
        );
    }

    #[test]
    fn audio_level_does_not_change_a_cut() {
        // A loud phrase the gate accepts: the windows (level fires) and one push (it cannot)
        // give the same clip, so the level path only adds `Skipped`.
        let parts = [silence(700), tone(800, -15.0), silence(700)];
        let (p, heard_at) = record(&parts, &opts());
        assert!(heard_at.is_some());
        let windows = p.finish_with_stats().unwrap();
        assert_eq!(windows.trim, Trim::Cut);
        assert_eq!(
            windows.samples,
            process(&parts.concat(), SAMPLE_RATE, &opts()).unwrap()
        );
    }

    #[test]
    fn audio_trim_off_keeps_everything() {
        let o = Audio {
            trim: false,
            ..opts()
        };
        let (p, _) = record(&[silence(300)], &o);
        let f = p.finish_with_stats().unwrap();
        assert_eq!(f.trim, Trim::Off);
        assert_eq!(f.stats.voiced_ms, 0);
        assert_eq!(f.samples.len(), 300 * MS);
    }
}
