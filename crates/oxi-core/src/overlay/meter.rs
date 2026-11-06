//! The level meter: a ring of bars on a dBFS scale, and the clipping flag.

use std::collections::VecDeque;

use crate::audio::Level;
use crate::types::Millis;

/// New bars per second.
pub const METER_HZ: u64 = 30;
/// Silence floor: this level and below is an empty bar; 0 dBFS is a full one.
pub const FLOOR_DBFS: f32 = -60.0;
/// A peak at or above this (linear) is clipping.
pub const CLIP_PEAK: f32 = 0.99;
/// How long the clipping flag stays on after the last clipped block.
pub const CLIP_HOLD: Millis = Millis(500);

const PERIOD: Millis = Millis(1000 / METER_HZ);

/// The one dB formula of overlay and settings. Bars run 0.0..=1.0, oldest first.
#[derive(Debug, Clone)]
pub struct Meter {
    bars: VecDeque<f32>,
    sensitivity_db: f32,
    /// Loudest RMS since the last bar.
    pending: f32,
    last_bar: Option<Millis>,
    clip_until: Millis,
}

impl Meter {
    /// `len` bars, all empty. `sensitivity_db` is `audio.meter_sensitivity_db`: it shifts the
    /// bars only, never the clipping flag.
    #[must_use]
    pub fn new(len: usize, sensitivity_db: f32) -> Self {
        Self {
            bars: VecDeque::from(vec![0.0; len.max(1)]),
            sensitivity_db,
            pending: 0.0,
            last_bar: None,
            clip_until: Millis(0),
        }
    }

    /// Feeds one block's level (any rate; a bar is added at most every 1/`METER_HZ` s, from the
    /// loudest block since the last bar). `now` is a monotonic time.
    pub fn push(&mut self, level: Level, now: Millis) {
        if level.peak >= CLIP_PEAK {
            self.clip_until = now + CLIP_HOLD;
        }
        self.pending = self.pending.max(level.rms);
        if self.last_bar.is_none_or(|t| now >= t + PERIOD) {
            self.bars.pop_front();
            self.bars.push_back(bar(self.pending, self.sensitivity_db));
            self.pending = 0.0;
            // Step the schedule (not `now`) so a poll slightly slower than the period keeps
            // 30 Hz; `now - PERIOD` drops the backlog after a stall.
            self.last_bar = Some(
                self.last_bar
                    .map_or(now, |t| (t + PERIOD).max(now - PERIOD)),
            );
        }
    }

    pub fn bars(&self) -> impl ExactSizeIterator<Item = f32> + '_ {
        self.bars.iter().copied()
    }

    #[must_use]
    pub fn clipping(&self, now: Millis) -> bool {
        now < self.clip_until
    }
}

/// Linear RMS to a 0..=1 bar height.
fn bar(rms: f32, sensitivity_db: f32) -> f32 {
    let dbfs = 20.0 * rms.max(0.0).log10() + sensitivity_db;
    ((dbfs - FLOOR_DBFS) / -FLOOR_DBFS).clamp(0.0, 1.0)
}
