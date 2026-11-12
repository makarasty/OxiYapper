//! The one driver of every progress bar in the window (arch-plan 6.3): a bar shows what
//! [`Smooth`] answers, so the test, history and model bars never compute a percent of their own.
//! One 33 ms timer runs while a bar is live and stops with the last one, so a still window has no
//! timer at all.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use oxi_core::overlay::{ProgressState as State, Smooth};
use oxi_core::types::Millis;
use slint::{Timer, TimerMode};

/// One frame of the bars, the overlay's tick.
const FRAME: Duration = Duration::from_millis(33);

/// A download's estimate: far beyond any download, so the estimate curve stays flat and the bar
/// follows the bytes (a real fraction takes over from the first byte).
pub const DOWNLOAD_EST: Millis = Millis(10 * 60 * 1000);

fn since_start() -> Millis {
    static START: OnceLock<Instant> = OnceLock::new();
    let elapsed = START.get_or_init(Instant::now).elapsed().as_millis();
    Millis(u64::try_from(elapsed).unwrap_or(u64::MAX))
}

/// Draws the value of one bar.
type Draw<K> = Box<dyn Fn(&K, f32)>;

pub struct Bars<K> {
    live: RefCell<Vec<(K, Smooth)>>,
    timer: Timer,
    apply: Draw<K>,
    clock: fn() -> Millis,
}

impl<K: PartialEq + Clone + 'static> Bars<K> {
    /// `apply` draws the value of the bar `key`; it runs on every frame of a live bar.
    pub fn new(apply: impl Fn(&K, f32) + 'static) -> Rc<Self> {
        Self::with_clock(apply, since_start)
    }

    fn with_clock(apply: impl Fn(&K, f32) + 'static, clock: fn() -> Millis) -> Rc<Self> {
        Rc::new(Self {
            live: RefCell::new(Vec::new()),
            timer: Timer::default(),
            apply: Box::new(apply),
            clock,
        })
    }

    /// Whether the bar `key` has a job (it ends a moment after its finish).
    pub fn live(&self, key: &K) -> bool {
        self.live.borrow().iter().any(|(k, _)| k == key)
    }

    /// A new job for `key`; `est` is its expected length (`None`: the default).
    pub fn start(self: &Rc<Self>, key: K, est: Option<Millis>) {
        let now = (self.clock)();
        {
            let mut live = self.live.borrow_mut();
            live.retain(|(k, _)| *k != key);
            live.push((key, Smooth::start(est, now)));
        }
        if !self.timer.running() {
            let me: Weak<Self> = Rc::downgrade(self);
            self.timer.start(TimerMode::Repeated, FRAME, move || {
                if let Some(bars) = me.upgrade() {
                    bars.step();
                }
            });
        }
        self.step();
    }

    /// A real fraction 0..=1 of the job of `key`.
    pub fn real(&self, key: &K, fraction: f32) {
        let now = (self.clock)();
        if let Some((_, s)) = self.live.borrow_mut().iter_mut().find(|(k, _)| k == key) {
            s.real(fraction, now);
        }
    }

    /// The job is done: the bar runs to full and ends (one that never started runs from empty).
    pub fn finish(self: &Rc<Self>, key: &K) {
        if !self.live(key) {
            self.start(key.clone(), None);
        }
        let now = (self.clock)();
        if let Some((_, s)) = self.live.borrow_mut().iter_mut().find(|(k, _)| k == key) {
            s.finish(now);
        }
    }

    /// The job failed: the bar stays where it is.
    pub fn fail(&self, key: &K) {
        let now = (self.clock)();
        if let Some((_, s)) = self.live.borrow_mut().iter_mut().find(|(k, _)| k == key) {
            s.fail(now);
        }
    }

    /// Drops the bar and draws it empty.
    pub fn clear(&self, key: &K) {
        self.live.borrow_mut().retain(|(k, _)| k != key);
        (self.apply)(key, 0.0);
        self.rest();
    }

    /// Draws every live bar as of now; a bar whose job ended is dropped after its last frame.
    fn step(&self) {
        let now = (self.clock)();
        let frames: Vec<(K, f32, bool)> = self
            .live
            .borrow_mut()
            .iter_mut()
            .map(|(k, s)| {
                let shown = s.shown(now);
                (k.clone(), shown, s.state(now) != State::Running)
            })
            .collect();
        // Not under the borrow: a draw may start or end a bar of its own.
        for (key, shown, _) in &frames {
            (self.apply)(key, *shown);
        }
        self.live
            .borrow_mut()
            .retain(|(k, _)| !frames.iter().any(|(done, _, ended)| *ended && done == k));
        self.rest();
    }

    /// Stops the timer when no bar is left.
    fn rest(&self) {
        if self.live.borrow().is_empty() {
            self.timer.stop();
        }
    }

    #[cfg(test)]
    pub fn timer_running(&self) -> bool {
        self.timer.running()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;

    thread_local! {
        static NOW: Cell<u64> = const { Cell::new(0) };
    }

    fn at(ms: u64) {
        NOW.set(ms);
    }

    type Drawn = Rc<RefCell<Vec<(u8, f32)>>>;

    fn rig() -> (Rc<Bars<u8>>, Drawn) {
        i_slint_backend_testing::init_no_event_loop();
        at(0);
        let drawn = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&drawn);
        let bars = Bars::with_clock(
            move |k: &u8, v| sink.borrow_mut().push((*k, v)),
            || Millis(NOW.get()),
        );
        (bars, drawn)
    }

    fn last(drawn: &Drawn, key: u8) -> f32 {
        drawn
            .borrow()
            .iter()
            .rev()
            .find(|(k, _)| *k == key)
            .unwrap()
            .1
    }

    #[test]
    fn a_bar_moves_at_once_never_goes_back_and_ends_full_with_its_timer_stopped() {
        let (bars, drawn) = rig();
        assert!(!bars.timer_running(), "no job, no timer");
        bars.start(1, Some(Millis(1000)));
        assert!(bars.timer_running());
        let mut seen = 0.0;
        for t in (33..=1000).step_by(33) {
            at(t);
            bars.step();
            let v = last(&drawn, 1);
            assert!(
                v >= seen && v - seen <= 0.04 + f32::EPSILON,
                "{seen} -> {v} at {t}"
            );
            seen = v;
        }
        assert!(seen > 0.02);
        at(1010);
        bars.real(&1, 0.5);
        bars.finish(&1);
        at(1010 + 150);
        bars.step();
        assert!(
            (last(&drawn, 1) - 1.0).abs() < f32::EPSILON,
            "no 100 -> 0 jump after the end"
        );
        at(1010 + 400);
        bars.step();
        assert!(!bars.live(&1) && !bars.timer_running());
        assert!((last(&drawn, 1) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn a_failed_bar_stays_where_it_was_and_two_bars_are_independent() {
        let (bars, drawn) = rig();
        bars.start(1, None);
        bars.start(2, Some(DOWNLOAD_EST));
        at(500);
        bars.real(&2, 0.25);
        bars.step();
        let one = last(&drawn, 1);
        bars.fail(&1);
        at(900);
        bars.step();
        assert!((last(&drawn, 1) - one).abs() < f32::EPSILON, "frozen");
        assert!(!bars.live(&1) && bars.live(&2));
        assert!(last(&drawn, 2) > 0.0 && last(&drawn, 2) <= 0.35);
        bars.clear(&2);
        assert!(last(&drawn, 2).abs() < f32::EPSILON && !bars.timer_running());
    }
}
