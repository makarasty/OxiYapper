//! The level meter: a ring of bars on a dBFS scale, and the clipping flag.

use std::collections::VecDeque;

use crate::audio::Level;
use crate::types::Millis;

/// New bars per second.
pub const METER_HZ: u64 = 30;
/// Silence floor: this level and below is an empty bar; 0 dBFS is a full one.
pub const FLOOR_DBFS: f32 = -60.0;
/// A frame whose peak is at or above this (linear, -1.0 dBFS) is hot (DS-21.3).
pub const HOT_PEAK: f32 = 0.891;
/// Length of the frame one hot reading stands for.
pub const HOT_FRAME: Millis = Millis(20);
/// Overload starts when hot frames add up to this much...
pub const OVERLOAD_HOT: Millis = Millis(60);
/// ...within this window.
pub const OVERLOAD_WINDOW: Millis = Millis(300);
/// How long overload stays on after the last hot frame.
pub const OVERLOAD_HOLD: Millis = Millis(600);

const PERIOD: Millis = Millis(1000 / METER_HZ);

/// Microphone overload as a level, not one clipped sample (DS-21.3): the one rule of the overlay
/// wave and the Audio page's meter. Feed it once per reading with that reading's peak; a hot
/// reading counts as one [`HOT_FRAME`] (the lower bound: the reading cannot say how many frames
/// of it were hot).
#[derive(Debug, Clone, Default)]
pub struct Overload {
    /// Times of the last three hot readings, oldest first: only they can make 60 ms in 300 ms.
    /// A fixed array, so a recording frame allocates nothing.
    hot: [Option<Millis>; 3],
    /// Overload began and has not yet lapsed.
    on: bool,
}

const _: () = assert!(OVERLOAD_HOT.0 == 3 * HOT_FRAME.0);

impl Overload {
    pub fn push(&mut self, peak: f32, now: Millis) {
        if self.on && !self.overloaded(now) {
            self.on = false;
        }
        if peak >= HOT_PEAK {
            self.hot.rotate_left(1);
            self.hot[2] = Some(now);
            self.on |= self.hot[0].is_some_and(|t| now - t < OVERLOAD_WINDOW);
        }
    }

    /// Whether the input is overloaded at `now` (a monotonic time at or after the last push).
    #[must_use]
    pub fn overloaded(&self, now: Millis) -> bool {
        self.on && self.hot[2].is_some_and(|t| now - t < OVERLOAD_HOLD)
    }
}

/// The one dB formula of overlay and settings. Bars run 0.0..=1.0, oldest first.
#[derive(Debug, Clone)]
pub struct Meter {
    bars: VecDeque<f32>,
    sensitivity_db: f32,
    /// Loudest RMS since the last bar.
    pending: f32,
    last_bar: Option<Millis>,
    overload: Overload,
    /// Per bar, parallel to `bars`: the input was overloaded while it was made.
    clipped: VecDeque<bool>,
    /// The input was overloaded in a block since the last bar.
    pending_clip: bool,
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
            overload: Overload::default(),
            clipped: VecDeque::from(vec![false; len.max(1)]),
            pending_clip: false,
        }
    }

    /// Feeds one block's level (any rate; a bar is added at most every 1/`METER_HZ` s, from the
    /// loudest block since the last bar). `now` is a monotonic time.
    pub fn push(&mut self, level: Level, now: Millis) {
        self.overload.push(level.peak, now);
        // The overload level, not one hot block: a lone peak must not paint a bar error-red.
        self.pending_clip |= self.overload.overloaded(now);
        self.pending = self.pending.max(level.rms);
        if self.last_bar.is_none_or(|t| now >= t + PERIOD) {
            self.bars.pop_front();
            self.bars.push_back(bar(self.pending, self.sensitivity_db));
            self.pending = 0.0;
            self.clipped.pop_front();
            self.clipped.push_back(self.pending_clip);
            self.pending_clip = false;
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

    /// Which bars clipped, parallel to [`Meter::bars`]: a long history marks where, not all of it.
    pub fn clipped(&self) -> impl ExactSizeIterator<Item = bool> + '_ {
        self.clipped.iter().copied()
    }

    /// Overloaded now (DS-21.3): three hot readings within 300 ms, until 600 ms after the last.
    #[must_use]
    pub fn clipping(&self, now: Millis) -> bool {
        self.overload.overloaded(now)
    }
}

/// Linear RMS to a 0..=1 bar height.
fn bar(rms: f32, sensitivity_db: f32) -> f32 {
    let dbfs = 20.0 * rms.max(0.0).log10() + sensitivity_db;
    ((dbfs - FLOOR_DBFS) / -FLOOR_DBFS).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_bars_made_in_overload_are_marked() {
        let mut m = Meter::new(5, 0.0);
        let calm = Level {
            rms: 0.1,
            peak: 0.2,
        };
        let loud = Level {
            rms: 0.3,
            peak: 1.0,
        };
        m.push(calm, Millis(0));
        for t in [40, 80, 120] {
            m.push(loud, Millis(t));
        }
        assert_eq!(
            m.clipped().collect::<Vec<_>>(),
            [false, false, false, false, true],
            "overload began with the third hot block"
        );
        assert_eq!(m.clipped().len(), m.bars().len());
    }

    /// A lone peak (or two) is not overload, so `clipping` and the bar flags agree: no red bar.
    #[test]
    fn a_lone_hot_peak_marks_no_bar() {
        let mut m = Meter::new(4, 0.0);
        let calm = Level {
            rms: 0.1,
            peak: 0.2,
        };
        let hot = Level {
            rms: 0.3,
            peak: 0.92,
        };
        m.push(calm, Millis(0));
        m.push(hot, Millis(40));
        m.push(calm, Millis(80));
        assert!(!m.clipping(Millis(80)));
        assert!(m.clipped().all(|c| !c));
    }

    fn feed(o: &mut Overload, peaks: &[(f32, u64)]) {
        for &(peak, at) in peaks {
            o.push(peak, Millis(at));
        }
    }

    /// DS-21.3: one hot frame is not overload; three in 300 ms are; it clears 600 ms after the last.
    #[test]
    fn overload_needs_three_hot_frames_and_holds_600_ms() {
        let mut o = Overload::default();
        feed(&mut o, &[(1.0, 0)]);
        assert!(!o.overloaded(Millis(0)));
        feed(&mut o, &[(0.2, 33), (1.0, 66)]);
        assert!(!o.overloaded(Millis(66)), "two are not enough");
        feed(&mut o, &[(0.95, 100)]);
        assert!(o.overloaded(Millis(100)));
        assert!(o.overloaded(Millis(100 + 599)));
        assert!(!o.overloaded(Millis(100 + 600)));
        // The same three readings after a clear start from nothing.
        feed(&mut o, &[(1.0, 2000)]);
        assert!(!o.overloaded(Millis(2000)));
    }

    #[test]
    fn hot_frames_spread_over_more_than_300_ms_do_not_add_up() {
        let mut o = Overload::default();
        feed(&mut o, &[(1.0, 0), (1.0, 150), (1.0, 300), (1.0, 450)]);
        assert!(!o.overloaded(Millis(450)));
        feed(&mut o, &[(1.0, 500)]);
        assert!(o.overloaded(Millis(500)), "300, 450, 500 are inside 300 ms");
        // Just under the peak threshold is not hot.
        let mut quiet = Overload::default();
        feed(&mut quiet, &[(0.89, 0), (0.89, 33), (0.89, 66), (0.89, 99)]);
        assert!(!quiet.overloaded(Millis(99)));
    }

    #[test]
    fn the_meter_reports_overload_by_the_same_rule() {
        let mut m = Meter::new(4, 0.0);
        let loud = Level {
            rms: 0.3,
            peak: 1.0,
        };
        for t in [0, 33, 66] {
            m.push(loud, Millis(t));
        }
        assert!(m.clipping(Millis(66)));
        assert!(!m.clipping(Millis(66 + 600)));
    }
}
