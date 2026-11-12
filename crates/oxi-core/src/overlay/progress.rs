//! One progress model for every bar and percent (DESIGN-SYSTEM section 26): the overlay pill and
//! the settings window draw the value [`Smooth::shown`] answers, and nothing else moves a bar.
//!
//! The estimate drives the bar and a real fraction only corrects it, so a job that reports nothing
//! until it is done (whisper under 30 s) never sits at 0 and never jumps. A caller that has neither
//! an estimate nor a real fraction holds no `Smooth` at all: `None` is the indeterminate sweep.

use crate::types::Millis;

/// The bar reaches this much within [`KICK_MS`] whatever is known.
pub const KICK: f32 = 0.03;
/// The kick takes this long, ease-out.
pub const KICK_MS: f32 = 200.0;
/// The estimate curve never passes this.
pub const EST_CEIL: f32 = 0.90;
/// The estimate curve's time constant, as a share of the estimate.
pub const EST_TAU: f32 = 0.30;
/// The estimate of a job whose caller has none.
pub const DEFAULT_EST: Millis = Millis(4000);
/// The estimate of a transcription is at least this long.
pub const MIN_EST: Millis = Millis(400);
/// Between two real updates the bar runs at most this far ahead of the last real value.
pub const PROJECT_AHEAD: f32 = 0.10;
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
    let seen = ms(took_ms) / transcribe_base_ms(audio_ms);
    (scale * (1.0 - LEARN_WEIGHT) + seen * LEARN_WEIGHT).clamp(SCALE_RANGE.0, SCALE_RANGE.1)
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

/// One job's progress as DESIGN-SYSTEM section 26 defines it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Smooth {
    started: Millis,
    est_ms: f32,
    /// The last real fraction and when it came.
    real: Option<(f32, Millis)>,
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
            real: None,
            shown: 0.0,
            shown_at: now,
            end: End::Running,
        }
    }

    /// A real fraction 0..=1; one lower than an earlier one is ignored.
    pub fn real(&mut self, fraction: f32, now: Millis) {
        if fraction.is_nan() {
            return;
        }
        let fraction = fraction.clamp(0.0, 1.0);
        if self.end != End::Running || self.real.is_some_and(|(f, _)| fraction < f) {
            return;
        }
        self.advance(now);
        self.real = Some((fraction, now));
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
        let eased =
            EST_CEIL * (1.0 - (-t / (EST_TAU * self.est_ms) * std::f32::consts::LN_2).exp());
        let target = match self.real {
            Some((real, at)) if real >= REAL_MIN => {
                let t_real = ms(at.0.saturating_sub(self.started.0)).max(1.0);
                let dt = (t - t_real).max(0.0);
                (real + real / t_real * dt).min(real + PROJECT_AHEAD)
            }
            _ => eased,
        };
        target.min(TARGET_CAP)
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
    fn the_estimate_curve_is_about_081_at_the_estimate_and_never_above_090() {
        let at_est = run(Some(4000), 4000, FRAME);
        assert!((0.70..=0.81).contains(&at_est), "{at_est}");
        assert!(run(Some(4000), 600_000, 100) <= 0.90 + 1e-4);
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
        assert_eq!(a.shown(Millis(2000)), b.shown(Millis(2000)));
    }

    #[test]
    fn the_projection_stays_within_ten_points_of_the_last_real_value() {
        let mut s = Smooth::start(Some(Millis(60_000)), Millis(0));
        s.real(0.30, Millis(10_000));
        // No report for a long time: the pace is 0.03 per second but the cap is 0.40.
        assert!(run_to(&mut s, 60_000) <= 0.40 + 1e-3);
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
        assert_eq!(typing_est(300, true), Millis(1800));
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
