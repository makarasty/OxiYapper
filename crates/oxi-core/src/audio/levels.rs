//! Loudness numbers of a recording (peak, speech, room noise, low rumble) and the second-order
//! filter behind "Cut low rumble". Numbers only: never the audio itself.

use serde::{Deserialize, Serialize};

/// Corner of the rumble cut and of the rumble measure: under any voice's fundamental.
pub const RUMBLE_HZ: f32 = 80.0;
/// Quietest level a number is reported down to (dBFS).
const FLOOR_DB: i16 = -120;
/// Window of one level reading, as the meter (20 ms).
const WINDOW_MS: u32 = 20;

/// Where the voice and the room sit in one recording, measured after the microphone boost and
/// before denoise, trim and normalise (so a quiet microphone still reads as quiet). Saved in the
/// history entry's json (`levels`) for the setting suggestions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Levels {
    /// Loudest sample, dBFS: at -1 or above the recording clipped.
    pub peak_db: i16,
    /// The voice: 90th percentile of the 20 ms levels (RMS, dBFS).
    pub speech_db: i16,
    /// The room: 10th percentile of the 20 ms levels (RMS, dBFS).
    pub floor_db: i16,
    /// Share of the energy under [`RUMBLE_HZ`], dB (0: all of it, -60: almost none).
    pub rumble_db: i16,
    /// From the start to the first voice (the speech gate's), when the trim ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_gap_ms: Option<u32>,
    /// From the last voice to the end, when the trim ran: under 60 ms the end cut the speech.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_gap_ms: Option<u32>,
    /// The microphone boost this was recorded at (whole dB): the numbers above depend on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gain_db: Option<i16>,
}

/// A second-order filter (RBJ cookbook, Butterworth Q), one sample at a time.
#[derive(Debug, Clone, Copy)]
pub struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    x: [f32; 2],
    y: [f32; 2],
}

impl Biquad {
    /// Passes what is above `hz` at `rate`; cuts what is under it, 12 dB per octave.
    #[must_use]
    pub fn high_pass(rate: u32, hz: f32) -> Self {
        Self::new(rate, hz, true)
    }

    /// Passes what is under `hz` at `rate`.
    #[must_use]
    pub fn low_pass(rate: u32, hz: f32) -> Self {
        Self::new(rate, hz, false)
    }

    fn new(rate: u32, hz: f32, high: bool) -> Self {
        #[allow(
            clippy::cast_precision_loss,
            reason = "sample rates are far below 2^24"
        )]
        let w = std::f32::consts::TAU * hz / rate.max(1) as f32;
        let alpha = w.sin() / std::f32::consts::SQRT_2;
        let cos = w.cos();
        let a0 = 1.0 + alpha;
        let (b0, b1) = if high {
            (f32::midpoint(1.0, cos), -(1.0 + cos))
        } else {
            ((1.0 - cos) / 2.0, 1.0 - cos)
        };
        Self {
            b: [b0 / a0, b1 / a0, b0 / a0],
            a: [-2.0 * cos / a0, (1.0 - alpha) / a0],
            x: [0.0; 2],
            y: [0.0; 2],
        }
    }

    pub fn step(&mut self, s: f32) -> f32 {
        let out = self.b[0] * s + self.b[1] * self.x[0] + self.b[2] * self.x[1]
            - self.a[0] * self.y[0]
            - self.a[1] * self.y[1];
        self.x = [s, self.x[0]];
        self.y = [out, self.y[0]];
        out
    }
}

/// dBFS of a linear level, never under [`FLOOR_DB`].
fn db(linear: f64) -> i16 {
    if linear <= 0.0 {
        return FLOOR_DB;
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "clamped to -120..=0 before the cast"
    )]
    let d = (20.0 * linear.log10())
        .round()
        .clamp(f64::from(FLOOR_DB), 0.0) as i16;
    d
}

/// Collects [`Levels`] from samples at one rate, any push size, in constant memory (a 1 dB
/// histogram of the 20 ms levels).
#[derive(Debug, Clone)]
pub struct LevelStats {
    window: usize,
    sum: f64,
    n: usize,
    /// Count of 20 ms windows per dB, index 0 = -120 dBFS.
    hist: [u32; 121],
    peak: f32,
    energy: f64,
    low_energy: f64,
    /// Two low-pass stages (24 dB per octave), so a deep voice is not read as rumble.
    low: [Biquad; 2],
}

impl LevelStats {
    #[must_use]
    pub fn new(rate: u32) -> Self {
        Self {
            window: (rate * WINDOW_MS / 1000).max(1) as usize,
            sum: 0.0,
            n: 0,
            hist: [0; 121],
            peak: 0.0,
            energy: 0.0,
            low_energy: 0.0,
            low: [Biquad::low_pass(rate, RUMBLE_HZ); 2],
        }
    }

    pub fn feed(&mut self, samples: &[f32]) {
        for &s in samples {
            let sq = f64::from(s) * f64::from(s);
            let first = self.low[0].step(s);
            let low = f64::from(self.low[1].step(first));
            self.peak = self.peak.max(s.abs());
            self.energy += sq;
            self.low_energy += low * low;
            self.sum += sq;
            self.n += 1;
            if self.n == self.window {
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "a window is a few hundred samples"
                )]
                let rms = (self.sum / self.n as f64).sqrt();
                let bin = usize::try_from(db(rms) - FLOOR_DB).unwrap_or(0);
                self.hist[bin.min(120)] += 1;
                self.sum = 0.0;
                self.n = 0;
            }
        }
    }

    /// The level at `share` (0..1) of the 20 ms windows, quietest first.
    fn percentile(&self, share: f64) -> i16 {
        let total: u32 = self.hist.iter().sum();
        if total == 0 {
            return FLOOR_DB;
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "share is 0..1 of a window count"
        )]
        let target = (f64::from(total) * share).ceil().max(1.0) as u32;
        let mut seen = 0;
        for (i, &count) in self.hist.iter().enumerate() {
            seen += count;
            if seen >= target {
                return FLOOR_DB + i16::try_from(i).unwrap_or(0);
            }
        }
        0
    }

    /// The numbers so far; the edge gaps are the caller's (the speech gate's).
    #[must_use]
    pub fn levels(&self) -> Levels {
        let rumble = if self.energy > 0.0 {
            db((self.low_energy / self.energy).sqrt())
        } else {
            FLOOR_DB
        };
        Levels {
            peak_db: db(f64::from(self.peak)),
            speech_db: self.percentile(0.9),
            floor_db: self.percentile(0.1),
            rumble_db: rumble.max(-60),
            start_gap_ms: None,
            end_gap_ms: None,
            gain_db: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn sine(hz: f32, amp: f32, rate: u32, ms: u32) -> Vec<f32> {
        (0..rate * ms / 1000)
            .map(|i| amp * (TAU * hz * i as f32 / rate as f32).sin())
            .collect()
    }

    fn rms(s: &[f32]) -> f32 {
        (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt()
    }

    #[test]
    fn high_pass_cuts_rumble_and_keeps_the_voice() {
        for rate in [16_000, 48_000] {
            let run = |hz| {
                let input = sine(hz, 0.5, rate, 1000);
                let mut f = Biquad::high_pass(rate, RUMBLE_HZ);
                let out: Vec<f32> = input.iter().map(|&s| f.step(s)).collect();
                // Past the filter's settling.
                20.0 * (rms(&out[rate as usize / 4..]) / rms(&input[rate as usize / 4..])).log10()
            };
            assert!(run(30.0) < -12.0, "30 Hz at {rate}: {}", run(30.0));
            assert!(run(300.0) > -0.5, "300 Hz at {rate}: {}", run(300.0));
        }
    }

    #[test]
    fn levels_read_peak_speech_floor_and_rumble() {
        let rate = 48_000;
        let mut stats = LevelStats::new(rate);
        // 1 s of quiet room (-60 dBFS RMS hum at 50 Hz), then 1 s of a 300 Hz "voice" at -20.
        let amp = |dbfs: f32| 10f32.powf(dbfs / 20.0) * std::f32::consts::SQRT_2;
        stats.feed(&sine(50.0, amp(-60.0), rate, 1000));
        stats.feed(&sine(300.0, amp(-20.0), rate, 1000));
        let l = stats.levels();
        assert!((-21..=-19).contains(&l.speech_db), "{l:?}");
        assert!((-62..=-58).contains(&l.floor_db), "{l:?}");
        assert!((-18..=-16).contains(&l.peak_db), "{l:?}");
        assert!(l.rumble_db < -30, "a 300 Hz voice is no rumble: {l:?}");

        let mut hum = LevelStats::new(rate);
        hum.feed(&sine(40.0, 0.3, rate, 1000));
        assert!(hum.levels().rumble_db > -6, "{:?}", hum.levels());
    }

    #[test]
    fn silence_reads_as_the_floor_value() {
        let mut stats = LevelStats::new(16_000);
        stats.feed(&[0.0; 1600]);
        let l = stats.levels();
        assert_eq!((l.peak_db, l.speech_db, l.floor_db), (-120, -120, -120));
        assert_eq!(LevelStats::new(16_000).levels().speech_db, -120);
    }
}
