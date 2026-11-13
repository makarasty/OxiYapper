//! One progress model for every bar and percent (DESIGN-SYSTEM section 26): the overlay pill and
//! the settings window draw the value [`Smooth::shown`] answers, and nothing else moves a bar.
//!
//! The estimate drives the bar and a real fraction only corrects it, so a job that reports nothing
//! until it is done (whisper under 30 s) never sits at 0 and never jumps. A caller that has neither
//! an estimate nor a real fraction holds no `Smooth` at all: `None` is the indeterminate sweep.
//!
//! Whisper reports once per 30 s window, so a real fraction can be the only one for half the job
//! (a 60 s clip says 0.5 once). After a real value the bar keeps the measured pace and bends into
//! a tail that slows but never stops ([`soft`]); it never waits at a fixed distance above it.

use crate::types::Millis;

/// The bar reaches this much within [`KICK_MS`] whatever is known.
pub const KICK: f32 = 0.03;
/// The kick takes this long, ease-out.
pub const KICK_MS: f32 = 200.0;
/// With no real fraction the bar never passes this.
pub const EST_CEIL: f32 = 0.95;
/// The share of the way from the start (or the last real value) to the cap that the bar runs at
/// the plain pace; past it the tail slows and approaches the cap without reaching it.
pub const KNEE: f32 = 0.70;
/// The estimate of a job whose caller has none.
pub const DEFAULT_EST: Millis = Millis(4000);
/// The estimate of a transcription is at least this long.
pub const MIN_EST: Millis = Millis(400);
/// A real fraction below this is not used for the projection yet.
pub const REAL_MIN: f32 = 0.01;
/// The bar never aims above this before the finish.
pub const TARGET_CAP: f32 = 0.99;
/// The chase of `shown` towards the target.
pub const CHASE_MS: f32 = 200.0;
/// The most the bar moves, per second (0.04 per 33 ms frame).
pub const SPEED_LIMIT: f32 = 1.2;
/// The finish runs from the current value to 1.0 in this long, ease-out.
pub const FINISH_MS: u64 = 150;
/// The full bar stays this long after the finish before the job is [`State::Finished`].
pub const HOLD_MS: u64 = 250;
/// The longest step the integration takes, so a coarse caller and a fine one agree.
const SUBSTEP_MS: u64 = 8;

/// The first guess of a transcription: a fixed cost and a share of the audio's length (the
/// measured fit of the dev machine, perf-baseline). [`learn`] corrects it per model and backend.
const TRANSCRIBE_BASE_MS: f32 = 282.0;
const TRANSCRIBE_PER_AUDIO_MS: f32 = 0.088;
/// How much of each finished run the learned scale takes.
const LEARN_WEIGHT: f32 = 0.3;
/// The learned scale stays within these, so one odd run never makes a useless estimate.
const SCALE_RANGE: (f32, f32) = (0.25, 8.0);
/// Typing, per UTF-16 unit, at the fast and the safe pace (chunks of 16 at 2 ms; one at 5 ms).
const TYPE_FAST_MS: f32 = 0.2;
const TYPE_SAFE_MS: f32 = 6.0;

/// The fit before any run was timed: what a transcription of `audio_ms` should take, unscaled.
#[must_use]
pub fn transcribe_base_ms(audio_ms: u64) -> f32 {
    TRANSCRIBE_BASE_MS + TRANSCRIBE_PER_AUDIO_MS * ms(audio_ms)
}

/// The estimate of a transcription of `audio_ms` with the learned `scale` (1.0 before any run),
/// at least [`MIN_EST`].
#[must_use]
pub fn transcribe_est(audio_ms: u64, scale: f32) -> Millis {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a positive length"
    )]
    let est = (transcribe_base_ms(audio_ms) * scale) as u64;
    Millis(est.max(MIN_EST.0))
}

/// The scale after a run of `audio_ms` that took `took_ms`: an exponential average.
#[must_use]
pub fn learn(scale: f32, audio_ms: u64, took_ms: u64) -> f32 {
    (scale * (1.0 - LEARN_WEIGHT) + learn_first(audio_ms, took_ms) * LEARN_WEIGHT)
        .clamp(SCALE_RANGE.0, SCALE_RANGE.1)
}

/// The scale after the first run of a model on a device: that run whole, since the fit is only
/// the dev machine's guess (tiny on a CPU here runs 3 to 4 times faster than it says).
#[must_use]
pub fn learn_first(audio_ms: u64, took_ms: u64) -> f32 {
    (ms(took_ms) / transcribe_base_ms(audio_ms)).clamp(SCALE_RANGE.0, SCALE_RANGE.1)
}

/// What typing `units` UTF-16 units takes at the fast or the safe pace.
#[must_use]
pub fn typing_est(units: usize, safe: bool) -> Millis {
    let per = if safe { TYPE_SAFE_MS } else { TYPE_FAST_MS };
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a positive length"
    )]
    let est = (ms(units as u64) * per) as u64;
    Millis(est.max(1))
}

/// Where a job stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    /// The full bar has stayed its time: the holder may drop the `Smooth`.
    Finished,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum End {
    Running,
    Finishing { from: f32, at: Millis },
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Real {
    fraction: f32,
    at: Millis,
    /// Where the bar goes on from: the bar if it was ahead (it never goes back), else `fraction`.
    from: f32,
    /// Fraction per ms between the first report and this one; `None` for the first report,
    /// which alone says nothing of the pace (a resumed download starts at half).
    pace: Option<f32>,
}

/// `p` as drawn: itself up to the knee between `from` and `cap`, then a tail with the same slope
/// at the knee that approaches `cap` and never reaches it (`cap - g^2 / (g + over)`), so a job
/// that runs long keeps the bar moving, slower and slower, instead of stopping it.
#[must_use]
pub fn soft(p: f32, from: f32, cap: f32) -> f32 {
    let knee = from + KNEE * (cap - from);
    if p <= knee {
        return p;
    }
    let g = cap - knee;
    if g <= f32::EPSILON {
        return cap;
    }
    cap - g * g / (g + (p - knee))
}

/// One job's progress as DESIGN-SYSTEM section 26 defines it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Smooth {
    started: Millis,
    est_ms: f32,
    /// The first real fraction and when it came: the pace is measured from it.
    first: Option<(f32, Millis)>,
    /// The last real fraction.
    real: Option<Real>,
    shown: f32,
    shown_at: Millis,
    end: End,
}

#[allow(
    clippy::cast_precision_loss,
    reason = "a few hours of ms fit an f32 well enough"
)]
fn ms(m: u64) -> f32 {
    m as f32
}

impl Smooth {
    /// `est`: expected length of the whole job; `None` takes [`DEFAULT_EST`].
    #[must_use]
    pub fn start(est: Option<Millis>, now: Millis) -> Self {
        Self {
            started: now,
            est_ms: ms(est.unwrap_or(DEFAULT_EST).0.max(1)),
            first: None,
            real: None,
            shown: 0.0,
            shown_at: now,
            end: End::Running,
        }
    }

    /// A real fraction 0..=1; one not above an earlier one is ignored.
    pub fn real(&mut self, fraction: f32, now: Millis) {
        if fraction.is_nan() {
            return;
        }
        let fraction = fraction.clamp(0.0, 1.0);
        // The same value again is no news: it must not restart the pace from a later time.
        if self.end != End::Running || self.real.is_some_and(|r| fraction <= r.fraction) {
            return;
        }
        self.advance(now);
        let (f0, t0) = *self.first.get_or_insert((fraction, now));
        self.real = Some(Real {
            fraction,
            at: now,
            from: self.shown.max(fraction).min(TARGET_CAP),
            pace: (now > t0).then(|| (fraction - f0) / ms(now.0 - t0.0)),
        });
    }

    /// The job is done: the bar runs to 1.0 from where it is.
    pub fn finish(&mut self, now: Millis) {
        if self.end == End::Running {
            self.advance(now);
            self.end = End::Finishing {
                from: self.shown,
                at: now,
            };
        }
    }

    /// Failed or cancelled: the bar stops where it is.
    pub fn fail(&mut self, now: Millis) {
        if self.end == End::Running {
            self.advance(now);
            self.end = End::Failed;
        }
    }

    /// Advances to `now` and answers the value to draw, 0..=1.
    pub fn shown(&mut self, now: Millis) -> f32 {
        if let End::Finishing { from, at } = self.end {
            let x = (ms(now.0.saturating_sub(at.0)) / ms(FINISH_MS)).clamp(0.0, 1.0);
            self.shown = self
                .shown
                .max(from + (1.0 - from) * (1.0 - (1.0 - x) * (1.0 - x)));
            self.shown_at = now;
        } else {
            self.advance(now);
        }
        self.shown
    }

    /// `floor(shown * 100)` as of the last [`Self::shown`], at most 99 until the finish is done.
    #[must_use]
    pub fn percent(&self) -> u8 {
        if self.shown >= 1.0 {
            return 100;
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "0..=99"
        )]
        {
            ((self.shown * 100.0) as u8).min(99)
        }
    }

    /// When the finish began, while the bar finishes (or stays full).
    #[must_use]
    pub fn finishing_since(&self) -> Option<Millis> {
        match self.end {
            End::Finishing { at, .. } => Some(at),
            _ => None,
        }
    }

    #[must_use]
    pub fn state(&self, now: Millis) -> State {
        match self.end {
            End::Failed => State::Failed,
            End::Finishing { at, .. } if now.0.saturating_sub(at.0) >= FINISH_MS + HOLD_MS => {
                State::Finished
            }
            End::Running | End::Finishing { .. } => State::Running,
        }
    }

    /// Where the bar aims at `now`, before the speed limit.
    fn target(&self, now: Millis) -> f32 {
        let t = ms(now.0.saturating_sub(self.started.0));
        let target = match self.real {
            // The measured pace (the estimate's until a second report), from where the bar was.
            Some(r) if r.fraction >= REAL_MIN => {
                let dt = ms(now.0.saturating_sub(r.at.0));
                soft(r.from + self.pace(r) * dt, r.from, TARGET_CAP)
            }
            _ => soft(t / self.est_ms, 0.0, EST_CEIL),
        };
        target.min(TARGET_CAP)
    }

    fn pace(&self, r: Real) -> f32 {
        r.pace.unwrap_or(1.0 / self.est_ms)
    }

    /// What the job still takes at the pace so far (the estimate's until a second report);
    /// `None` once that has run out with the job still going, or once it ended: an honest blank
    /// rather than a number that sits there.
    #[must_use]
    pub fn left(&self, now: Millis) -> Option<Millis> {
        if self.end != End::Running {
            return None;
        }
        let ms_left = match self.real {
            Some(r) if r.fraction >= REAL_MIN => {
                (1.0 - r.fraction) / self.pace(r) - ms(now.0.saturating_sub(r.at.0))
            }
            _ => self.est_ms - ms(now.0.saturating_sub(self.started.0)),
        };
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "positive, a few hours of ms at most"
        )]
        (ms_left >= 1.0).then(|| Millis(ms_left.round() as u64))
    }

    fn kick(&self, now: Millis) -> f32 {
        let x = (ms(now.0.saturating_sub(self.started.0)) / KICK_MS).min(1.0);
        KICK * (1.0 - (1.0 - x) * (1.0 - x))
    }

    /// Chase and speed limit, in steps of at most [`SUBSTEP_MS`] whatever spacing the caller has.
    fn advance(&mut self, now: Millis) {
        if self.end != End::Running {
            return;
        }
        while self.shown_at < now {
            let step = (now.0 - self.shown_at.0).min(SUBSTEP_MS);
            let at = Millis(self.shown_at.0 + step);
            let dt = ms(step);
            let want = (self.target(at) - self.shown).max(0.0);
            let moved = (want * (1.0 - (-dt / CHASE_MS).exp())).min(SPEED_LIMIT * dt / 1000.0);
            self.shown = (self.shown + moved).max(self.kick(at));
            self.shown_at = at;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: u64 = 33;
    const STEP: usize = 33;

    /// A small deterministic generator: no dependency for a property test.
    struct Lcg(u64);
    impl Lcg {
        fn below(&mut self, below: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (self.0 >> 33) % below
        }
    }

    fn run(est: Option<u64>, until: u64, step: u64) -> f32 {
        let mut s = Smooth::start(est.map(Millis), Millis(0));
        let mut t = 0;
        while t < until {
            t = (t + step).min(until);
            s.shown(Millis(t));
        }
        s.shown(Millis(until))
    }

    #[test]
    fn it_moves_in_the_first_frames_whatever_is_known() {
        // DS-26.1, even with an estimate of an hour and nothing real.
        for est in [None, Some(400), Some(3_600_000)] {
            assert!(run(est, 200, FRAME) >= 0.02, "{est:?}");
        }
    }

    #[test]
    fn the_estimate_curve_is_about_080_at_the_estimate_and_never_above_095() {
        let at_est = run(Some(4000), 4000, FRAME);
        assert!((0.75..=0.83).contains(&at_est), "{at_est}");
        assert!(run(Some(4000), 600_000, 100) <= EST_CEIL + 1e-4);
    }

    /// The owner's freeze (2026-11-13): a 60 s clip, whisper says 0 at the start, 0.5 when the
    /// first 30 s window is done, 1.0 just before the end. The old projection stopped at
    /// `0.5 + 0.10` and sat there for the whole second window.
    #[test]
    fn a_single_mid_job_report_does_not_freeze_the_bar_for_the_rest_of_the_job() {
        let mut s = Smooth::start(Some(Millis(10_000)), Millis(0));
        s.real(0.0, Millis(0));
        s.shown(Millis(5000));
        s.real(0.5, Millis(5000));
        let mut last = s.shown(Millis(5500));
        for t in (6000..=9900).step_by(500) {
            let v = s.shown(Millis(t));
            assert!(v - last >= 0.005, "stuck at {t} ms: {last} -> {v}");
            last = v;
        }
        assert!(
            last >= 0.85,
            "near the end of the job the bar is near the end: {last}"
        );
        s.real(1.0, Millis(9950));
        assert!(s.shown(Millis(9990)) <= TARGET_CAP);
    }

    #[test]
    fn regular_reports_are_followed_at_their_pace() {
        // A 2 min clip in four windows of 3 s each, the estimate twice too long.
        let mut s = Smooth::start(Some(Millis(24_000)), Millis(0));
        for (i, t) in [3000, 6000, 9000].into_iter().enumerate() {
            let truth = 0.25 * (i as f32 + 1.0);
            s.shown(Millis(t));
            s.real(truth, Millis(t));
            let v = s.shown(Millis(t + 1000));
            assert!((v - truth).abs() <= 0.12, "{t}: {v} against {truth}");
        }
    }

    #[test]
    fn an_estimate_ten_times_too_short_still_moves_the_bar_without_a_report() {
        let mut s = Smooth::start(Some(Millis(1000)), Millis(0));
        let mut last = s.shown(Millis(1000));
        for t in (2000..=10_000).step_by(1000) {
            let v = s.shown(Millis(t));
            assert!(v > last && v < EST_CEIL, "stuck at {t} ms: {v}");
            last = v;
        }
    }

    #[test]
    fn a_report_below_a_bar_that_ran_ahead_slows_it_but_never_stops_it() {
        let mut s = Smooth::start(Some(Millis(2000)), Millis(0));
        let ahead = s.shown(Millis(4000));
        s.real(0.3, Millis(4000));
        assert!(s.shown(Millis(4033)) >= ahead, "never back");
        let mut last = ahead;
        for t in (4500..=12_000).step_by(500) {
            let v = s.shown(Millis(t));
            assert!(v > last, "stuck at {t} ms: {v}");
            last = v;
        }
        assert!(last <= TARGET_CAP);
    }

    #[test]
    fn the_time_left_follows_the_measured_pace_and_goes_blank_when_overdue() {
        let mut s = Smooth::start(Some(Millis(10_000)), Millis(0));
        assert_eq!(
            s.left(Millis(4000)),
            Some(Millis(6000)),
            "the estimate before any report"
        );
        s.real(0.0, Millis(0));
        s.real(0.5, Millis(8000));
        assert_eq!(
            s.left(Millis(8000)),
            Some(Millis(8000)),
            "the pace, not the estimate"
        );
        assert_eq!(s.left(Millis(16_000)), None, "overdue: no number");
        // One report alone says nothing of the pace: a resumed download at half.
        let mut d = Smooth::start(Some(Millis(10_000)), Millis(0));
        d.real(0.5, Millis(50));
        assert_eq!(d.left(Millis(50)), Some(Millis(5000)));
        // The estimate's pace (0.1 a second), not "half in 50 ms" (the knee at once).
        let v = d.shown(Millis(1000));
        assert!(v < 0.6, "it does not race ahead of the one report: {v}");
        d.finish(Millis(1000));
        assert_eq!(d.left(Millis(1000)), None);
    }

    #[test]
    fn the_tail_keeps_the_slope_at_the_knee_and_never_reaches_the_cap() {
        let knee = 0.5 + KNEE * (0.99 - 0.5);
        assert!(
            (soft(0.6, 0.5, 0.99) - 0.6).abs() < 1e-6,
            "plain pace below the knee"
        );
        let d = 1e-3;
        let slope = (soft(knee + d, 0.5, 0.99) - soft(knee, 0.5, 0.99)) / d;
        assert!((slope - 1.0).abs() < 0.01, "{slope}");
        assert!(soft(1000.0, 0.5, 0.99) < 0.99);
        assert!(
            (soft(5.0, 1.0, 0.99) - 0.99).abs() < 1e-6,
            "a start past the cap"
        );
    }

    #[test]
    fn no_real_report_still_rises_at_every_100_ms_step() {
        let mut s = Smooth::start(Some(Millis(5000)), Millis(0));
        let mut last = 0.0;
        for t in (100..=5000).step_by(100) {
            let v = s.shown(Millis(t));
            assert!(v > last, "stuck at {t} ms: {v}");
            last = v;
        }
    }

    #[test]
    fn a_jump_of_the_real_fraction_is_a_glide_never_a_leap() {
        // DS-26.2: 0 to 0.67 to 1.0 at random times; each 33 ms step is within [0, 0.04].
        for seed in 0..40 {
            let mut rng = Lcg(seed);
            let mut s = Smooth::start(Some(Millis(2000 + rng.below(8000))), Millis(0));
            let mut marks = [
                rng.below(3000),
                3000 + rng.below(3000),
                6000 + rng.below(3000),
            ];
            marks.sort_unstable();
            let reals = [0.0, 0.67, 1.0];
            let mut next = 0;
            let mut last = s.shown(Millis(0));
            for t in (FRAME..12_000).step_by(STEP) {
                while next < 3 && marks[next] <= t {
                    s.real(reals[next], Millis(marks[next]));
                    next += 1;
                }
                let v = s.shown(Millis(t));
                let d = v - last;
                assert!((0.0..=0.04 + 1e-4).contains(&d), "seed {seed} t {t}: {d}");
                last = v;
            }
        }
    }

    #[test]
    fn a_lower_real_fraction_changes_nothing() {
        let mut a = Smooth::start(None, Millis(0));
        a.real(0.5, Millis(1000));
        let mut b = a;
        b.real(0.2, Millis(1100));
        b.real(0.5, Millis(1500));
        assert_eq!(a.shown(Millis(2000)), b.shown(Millis(2000)));
    }

    #[test]
    fn with_no_report_after_the_last_the_bar_stays_below_the_cap() {
        let mut s = Smooth::start(Some(Millis(60_000)), Millis(0));
        s.real(0.30, Millis(10_000));
        // Twenty times the job the pace promised and no word: close to the cap, never at it.
        let v = run_to(&mut s, 600_000);
        assert!(v > 0.9 && v < TARGET_CAP, "{v}");
    }

    fn run_to(s: &mut Smooth, until: u64) -> f32 {
        let mut v = 0.0;
        for t in (0..=until).step_by(STEP) {
            v = s.shown(Millis(t));
        }
        v
    }

    #[test]
    fn finish_reaches_the_full_bar_in_time_from_any_value_and_then_ends() {
        for at in [0, 50, 1000, 20_000] {
            let mut s = Smooth::start(Some(Millis(4000)), Millis(0));
            s.shown(Millis(at));
            s.finish(Millis(at));
            assert_eq!(s.state(Millis(at)), State::Running);
            assert!(s.shown(Millis(at + FINISH_MS)) >= 1.0 - 1e-6, "{at}");
            assert_eq!(s.percent(), 100);
            assert_eq!(
                s.state(Millis(at + FINISH_MS + HOLD_MS - 1)),
                State::Running
            );
            assert_eq!(s.state(Millis(at + FINISH_MS + HOLD_MS)), State::Finished);
        }
    }

    #[test]
    fn a_phase_shorter_than_the_kick_still_shows_kick_and_finish() {
        let mut s = Smooth::start(Some(Millis(400)), Millis(0));
        let kicked = s.shown(Millis(60));
        assert!(kicked > 0.0);
        s.finish(Millis(60));
        assert!(s.shown(Millis(120)) > kicked);
    }

    #[test]
    fn fail_freezes_the_bar_and_never_runs_to_full() {
        let mut s = Smooth::start(Some(Millis(1000)), Millis(0));
        let at_fail = s.shown(Millis(500));
        s.fail(Millis(500));
        assert_eq!(s.state(Millis(500)), State::Failed);
        s.real(1.0, Millis(600));
        s.finish(Millis(600));
        assert_eq!(s.shown(Millis(5000)), at_fail);
        assert!(s.percent() < 100);
    }

    #[test]
    fn the_label_is_at_most_99_until_the_finish() {
        let mut s = Smooth::start(Some(Millis(400)), Millis(0));
        s.real(1.0, Millis(300));
        for t in (0..20_000).step_by(33) {
            s.shown(Millis(t));
            assert!(s.percent() <= 99, "{t}");
        }
    }

    #[test]
    fn the_transcription_estimate_follows_the_fit_and_learns_from_runs() {
        assert_eq!(transcribe_est(0, 1.0), MIN_EST);
        assert_eq!(transcribe_est(10_000, 1.0), Millis(1162));
        // Runs twice as slow as the fit: the scale moves towards 2 by the weight, never past.
        let mut scale = 1.0;
        for _ in 0..30 {
            scale = learn(scale, 10_000, 2324);
        }
        assert!((scale - 2.0).abs() < 0.01, "{scale}");
        assert!(learn(1.0, 10_000, 10_000_000) <= 8.0);
        assert!(
            (learn_first(10_000, 2324) - 2.0).abs() < 0.01,
            "the first run whole"
        );
        assert!(
            (learn_first(10_000, 1) - 0.25).abs() < 1e-6,
            "within the range"
        );
        assert_eq!(typing_est(300, true), Millis(1800));
    }

    /// The edge cases the owner named, at the progress model: each bar is monotonic, never at 100
    /// before its finish, and moves in every frame-sized step by at most the speed limit.
    fn drive(s: &mut Smooth, until: u64, from: u64, reports: &[(u64, f32)]) -> f32 {
        let mut last = s.shown(Millis(from));
        for t in (from + FRAME..=until).step_by(STEP) {
            for &(at, f) in reports.iter().filter(|(at, _)| (t - FRAME..t).contains(at)) {
                s.real(f, Millis(at));
            }
            let v = s.shown(Millis(t));
            assert!(
                (0.0..=0.04 + 1e-4).contains(&(v - last)),
                "t {t}: {last} -> {v}"
            );
            assert!(s.percent() <= 99, "no 100 before the finish at {t}");
            last = v;
        }
        last
    }

    #[test]
    fn a_first_model_load_is_not_part_of_the_bar() {
        // The bar starts when the model is loaded (the pill sweeps before): 5 s of load, then it
        // starts from 0 like any other and moves in its first frames.
        let load = 5000;
        let mut s = Smooth::start(Some(transcribe_est(10_000, 1.0)), Millis(load));
        assert_eq!(s.shown(Millis(load)), 0.0);
        assert!(s.shown(Millis(load + 200)) >= 0.02);
        assert!(
            s.shown(Millis(load + 600)) < 0.5,
            "the load time did not count"
        );
    }

    #[test]
    fn a_retranscribe_run_follows_whispers_reports_to_a_full_bar() {
        // A 45 s clip from History: the estimate, 0 at the start, 30 s of 45 done, then the end.
        let est = transcribe_est(45_000, 1.0);
        let mut s = Smooth::start(Some(est), Millis(0));
        s.real(0.0, Millis(0));
        let end = est.0 * 3 / 2;
        let v = drive(&mut s, end, 0, &[(est.0, 30.0 / 45.0)]);
        assert!(v > 0.7 && v <= TARGET_CAP, "{v}");
        s.real(1.0, Millis(end));
        s.finish(Millis(end));
        assert_eq!(s.shown(Millis(end + FINISH_MS)), 1.0);
        assert_eq!(s.percent(), 100);
    }

    #[test]
    fn a_long_clip_with_many_windows_never_stalls_or_overshoots() {
        // 10 min of audio, twenty windows; the run takes 2.5 times the estimate.
        let est = transcribe_est(600_000, 1.0);
        let took = est.0 * 5 / 2;
        let reports: Vec<_> = (1u8..20)
            .map(|i| (took * u64::from(i) / 20, f32::from(i) / 20.0))
            .collect();
        let mut s = Smooth::start(Some(est), Millis(0));
        let v = drive(&mut s, took * 19 / 20 + 1000, 0, &reports);
        assert!(
            (0.8..=TARGET_CAP).contains(&v),
            "close to the last report: {v}"
        );
        // Between two reports (about 6.6 s apart) the bar always moved.
        let mut a = s.shown(Millis(took * 19 / 20 + 2000));
        for t in (took * 19 / 20 + 3000..took).step_by(1000) {
            let b = s.shown(Millis(t));
            // Within 0.01 of the cap the f32 steps are the only thing left to see.
            assert!(b > a || a > TARGET_CAP - 0.01, "stuck at {t}: {a}");
            a = b;
        }
    }

    #[test]
    fn a_job_of_unknown_length_runs_long_without_ever_reaching_full() {
        let one_min = run(None, 60_000, 100);
        let ten_min = run(None, 600_000, 100);
        assert!(
            one_min < ten_min && ten_min < EST_CEIL,
            "{one_min} {ten_min}"
        );
        let mut s = Smooth::start(None, Millis(0));
        s.shown(Millis(600_000));
        assert!(s.percent() <= 99);
        assert_eq!(s.state(Millis(600_000)), State::Running);
        assert_eq!(s.left(Millis(600_000)), None, "overdue: no number");
    }

    #[test]
    fn a_fine_caller_and_a_coarse_one_agree() {
        for until in [500, 2000, 7000] {
            let fine = run(Some(4000), until, 5);
            let coarse = run(Some(4000), until, FRAME);
            assert!((fine - coarse).abs() < 0.01, "{until}: {fine} {coarse}");
        }
    }
}
