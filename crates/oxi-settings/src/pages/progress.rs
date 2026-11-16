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
/// Draws the whole seconds left of one bar; `None`: not known (no estimate, too early, the end).
type DrawLeft<K> = Box<dyn Fn(&K, Option<u64>)>;

/// Under this much of the job done the seconds left are a guess too wild to show.
const LEFT_MIN: f32 = 0.05;

/// One bar with a job.
struct Live<K> {
    key: K,
    smooth: Smooth,
    /// The job came with an estimate: only then is a time left shown.
    est: bool,
    /// The seconds left last drawn, and when they were.
    left: Option<u64>,
    left_at: Millis,
}

pub struct Bars<K> {
    live: RefCell<Vec<Live<K>>>,
    timer: Timer,
    apply: Draw<K>,
    draw_left: Option<DrawLeft<K>>,
    clock: fn() -> Millis,
}

/// The whole seconds left of `smooth`'s job at the pace it measured, with `shown` of it drawn;
/// `None` while too little is done to tell, once the job outran that pace, or once it is full.
fn secs_left(smooth: &Smooth, now: Millis, shown: f32) -> Option<u64> {
    if !(LEFT_MIN..1.0).contains(&shown) {
        return None;
    }
    smooth.left(now).map(|ms| ms.0.div_ceil(1000).max(1))
}

/// "12 s left", "3 min left"; empty when not known.
pub fn left_label(secs: Option<u64>) -> String {
    match secs {
        Some(s) if s < 60 => format!("{s} s left"),
        Some(s) => format!("{} min left", s.div_ceil(60)),
        None => String::new(),
    }
}

impl<K: PartialEq + Clone + 'static> Bars<K> {
    /// `apply` draws the value of the bar `key`; it runs on every frame of a live bar.
    pub fn new(apply: impl Fn(&K, f32) + 'static) -> Rc<Self> {
        Self::with_clock(apply, None, since_start)
    }

    /// As [`Self::new`]; `left` also draws the whole seconds left of a bar whose job came with
    /// an estimate, on a change only.
    pub fn with_left(
        apply: impl Fn(&K, f32) + 'static,
        left: impl Fn(&K, Option<u64>) + 'static,
    ) -> Rc<Self> {
        Self::with_clock(apply, Some(Box::new(left)), since_start)
    }

    fn with_clock(
        apply: impl Fn(&K, f32) + 'static,
        draw_left: Option<DrawLeft<K>>,
        clock: fn() -> Millis,
    ) -> Rc<Self> {
        Rc::new(Self {
            live: RefCell::new(Vec::new()),
            timer: Timer::default(),
            apply: Box::new(apply),
            draw_left,
            clock,
        })
    }

    /// Whether the bar `key` has a job (it ends a moment after its finish).
    pub fn live(&self, key: &K) -> bool {
        self.live.borrow().iter().any(|l| l.key == *key)
    }

    /// A new job for `key`; `est` is its expected length (`None`: the default).
    pub fn start(self: &Rc<Self>, key: K, est: Option<Millis>) {
        let now = (self.clock)();
        {
            let mut live = self.live.borrow_mut();
            live.retain(|l| l.key != key);
            live.push(Live {
                key,
                smooth: Smooth::start(est, now),
                est: est.is_some(),
                left: None,
                left_at: now,
            });
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
        if let Some(l) = self.live.borrow_mut().iter_mut().find(|l| l.key == *key) {
            l.smooth.real(fraction, now);
        }
    }

    /// The job is done: the bar runs to full and ends (one that never started runs from empty).
    pub fn finish(self: &Rc<Self>, key: &K) {
        if !self.live(key) {
            self.start(key.clone(), None);
        }
        let now = (self.clock)();
        if let Some(l) = self.live.borrow_mut().iter_mut().find(|l| l.key == *key) {
            l.smooth.finish(now);
        }
    }

    /// The job failed: the bar stays where it is.
    pub fn fail(&self, key: &K) {
        let now = (self.clock)();
        if let Some(l) = self.live.borrow_mut().iter_mut().find(|l| l.key == *key) {
            l.smooth.fail(now);
        }
    }

    /// Drops the bar and draws it empty.
    pub fn clear(&self, key: &K) {
        self.live.borrow_mut().retain(|l| l.key != *key);
        (self.apply)(key, 0.0);
        if let Some(draw) = &self.draw_left {
            draw(key, None);
        }
        self.rest();
    }

    /// Draws every live bar as of now; a bar whose job ended is dropped after its last frame.
    fn step(&self) {
        let now = (self.clock)();
        let frames: Vec<(K, f32, bool, Option<Option<u64>>)> = self
            .live
            .borrow_mut()
            .iter_mut()
            .map(|l| {
                let shown = l.smooth.shown(now);
                let running =
                    l.smooth.state(now) == State::Running && l.smooth.finishing_since().is_none();
                let fresh = (l.est && running)
                    .then(|| secs_left(&l.smooth, now, shown))
                    .flatten();
                // Never up while the time shown lasts: a pace that slows keeps it. Once it ran
                // out with the job still going it was wrong, and the fresh estimate takes over,
                // so a small number never sits there.
                let left = match (fresh, l.left) {
                    (Some(s), Some(was))
                        if s > was
                            && now.0 < l.left_at.0.saturating_add(was.saturating_mul(1000)) =>
                    {
                        Some(was)
                    }
                    _ => fresh,
                };
                let changed = (left != l.left).then_some(left);
                if changed.is_some() {
                    l.left_at = now;
                }
                l.left = left;
                (
                    l.key.clone(),
                    shown,
                    l.smooth.state(now) != State::Running,
                    changed,
                )
            })
            .collect();
        // Not under the borrow: a draw may start or end a bar of its own.
        for (key, shown, _, left) in &frames {
            (self.apply)(key, *shown);
            if let (Some(draw), Some(left)) = (&self.draw_left, left) {
                draw(key, *left);
            }
        }
        self.live.borrow_mut().retain(|l| {
            !frames
                .iter()
                .any(|(done, _, ended, _)| *ended && *done == l.key)
        });
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

    type Drawn<V = f32> = Rc<RefCell<Vec<(u8, V)>>>;

    fn rig() -> (Rc<Bars<u8>>, Drawn) {
        i_slint_backend_testing::init_no_event_loop();
        at(0);
        let drawn = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&drawn);
        let bars = Bars::with_clock(
            move |k: &u8, v| sink.borrow_mut().push((*k, v)),
            None,
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
    #[test]
    fn the_time_left_shows_only_with_an_estimate_never_rises_and_goes_at_the_end() {
        i_slint_backend_testing::init_no_event_loop();
        at(0);
        let lefts: Drawn<Option<u64>> = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&lefts);
        let bars = Bars::with_clock(
            |_: &u8, _| {},
            Some(Box::new(move |k: &u8, s| sink.borrow_mut().push((*k, s)))),
            || Millis(NOW.get()),
        );
        bars.start(1, Some(Millis(4000)));
        bars.start(2, None);
        let mut was = u64::MAX;
        for t in (33..=3000).step_by(33) {
            at(t);
            if t == 990 {
                bars.real(&1, 0.3);
                bars.real(&2, 0.3);
            }
            bars.step();
            let last = lefts.borrow().iter().rev().find(|(k, _)| *k == 1).copied();
            if let Some((_, Some(s))) = last {
                assert!(s <= was, "{was} -> {s} at {t}");
                was = s;
            }
        }
        assert!(was < u64::MAX, "a time left was shown");
        assert!(
            lefts.borrow().iter().all(|(k, s)| *k == 1 || s.is_none()),
            "no estimate, no time left"
        );
        let draws = lefts.borrow().len();
        bars.step();
        assert_eq!(lefts.borrow().len(), draws, "drawn on a change only");
        bars.finish(&1);
        at(3100);
        bars.step();
        assert_eq!(lefts.borrow().last(), Some(&(1, None)), "gone at the end");
        assert_eq!(left_label(Some(12)), "12 s left");
        assert_eq!(left_label(Some(61)), "2 min left");
        assert_eq!(left_label(None), "");
        let mut s = Smooth::start(Some(Millis(10_000)), Millis(0));
        assert_eq!(secs_left(&s, Millis(100), 0.01), None, "too early to tell");
        assert_eq!(secs_left(&s, Millis(5000), 0.5), Some(5));
        s.real(0.0, Millis(0));
        s.real(0.5, Millis(1000));
        assert_eq!(
            secs_left(&s, Millis(1500), 0.6),
            Some(1),
            "the measured pace"
        );
    }

    /// b01 review: a job that runs five times its estimate, with no real fraction after the
    /// first, never keeps a time on screen longer than it said.
    #[test]
    fn a_time_left_that_ran_out_while_the_job_goes_on_is_not_kept() {
        i_slint_backend_testing::init_no_event_loop();
        at(0);
        let lefts: Drawn<Option<u64>> = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&lefts);
        let bars = Bars::with_clock(
            |_: &u8, _| {},
            Some(Box::new(move |k: &u8, s| sink.borrow_mut().push((*k, s)))),
            || Millis(NOW.get()),
        );
        bars.start(1, Some(Millis(4000)));
        let mut since: Option<(u64, u64)> = None;
        for t in (33..=20_000).step_by(33) {
            at(t);
            if t == 495 {
                bars.real(&1, 0.1);
            }
            bars.step();
            let last = lefts.borrow().last().copied();
            match last {
                Some((_, Some(s))) if since.is_none_or(|(_, was)| was != s) => {
                    since = Some((t, s));
                }
                Some((_, None)) => since = None,
                _ => {}
            }
            if let Some((from, s)) = since {
                assert!(
                    t - from <= s * 1000 + 33,
                    "{s} s left shown since {from}, still at {t}"
                );
            }
        }
        // Five times the estimate with no word: the time is blank, not a stuck "1 s left".
        assert_eq!(lefts.borrow().last(), Some(&(1, None)));
    }
}
