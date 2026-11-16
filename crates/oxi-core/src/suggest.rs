//! Audio setting suggestions from the person's own recordings (hold item 41): the loudness
//! numbers each history entry keeps ([`Levels`]) say what to change. Only suggestions: the
//! settings page shows them and changes nothing until the person presses Apply. Numbers only:
//! no text is read. Thresholds are starting points, not measured on the bench.

use crate::audio::Levels;
use crate::config::{Audio, EdgePreset, MemoryMode, Noise};
use crate::stats::Day;

/// Fewer recordings with numbers than this suggest nothing.
pub const MIN_RECORDINGS: usize = 10;
/// The newest this many recordings with numbers are read.
pub const RECENT: usize = 50;
/// A peak at or above this (dBFS) clipped.
const CLIP_DB: i16 = -1;
/// Speech peaks under this (dBFS) are quiet.
const QUIET_PEAK_DB: i16 = -30;
/// The voice under this (dBFS) is under the level detector's -40 for most of the time.
const QUIET_SPEECH_DB: i16 = -40;
/// A voice is aimed at this level (dBFS) when the boost is raised.
const TARGET_SPEECH_DB: i16 = -25;
/// A gap to the clip's edge under this means the speech touched it.
const TOUCH_MS: u32 = 60;
/// More rumble than this share of the energy (dB) is worth the cut.
const RUMBLE_DB: i16 = -10;

/// One suggested value: what to set, and in how many of the recordings read it showed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Advice<T> {
    pub value: T,
    pub hits: usize,
    pub of: usize,
}

/// What the recordings suggest; a field is `None` when they suggest nothing new.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioAdvice {
    pub noise: Option<Advice<Noise>>,
    /// The new microphone boost (dB), not a step.
    pub gain_db: Option<Advice<f64>>,
    pub edges: Option<Advice<EdgePreset>>,
    pub rumble: Option<Advice<bool>>,
}

fn median(mut v: Vec<i16>) -> i16 {
    v.sort_unstable();
    v[v.len() / 2]
}

/// A boost within the slider's -20..=20 dB, whole dB.
fn boost(db: f64) -> f64 {
    db.round().clamp(-20.0, 20.0)
}

/// What `levels` (newest first, at most [`RECENT`] are read) suggest for `audio` as it is now.
#[must_use]
pub fn advise(levels: &[Levels], audio: &Audio) -> AudioAdvice {
    let levels = &levels[..levels.len().min(RECENT)];
    let of = levels.len();
    if of < MIN_RECORDINGS {
        return AudioAdvice::default();
    }
    let count = |f: &dyn Fn(&Levels) -> bool| levels.iter().filter(|l| f(l)).count();

    let noise = (audio.noise() != Noise::Auto)
        .then(|| Noise::for_floor(median(levels.iter().map(|l| l.floor_db).collect())))
        .filter(|&n| n != audio.noise())
        .map(|value| Advice {
            value,
            hits: count(&|l| Noise::for_floor(l.floor_db) == value),
            of,
        });

    // The boost is judged on recordings made at the boost it has now: older ones were heard at
    // another boost, so a step on top of the current one would be counted twice.
    let now_gain = audio.gain_db.round();
    let now: Vec<&Levels> = levels
        .iter()
        .filter(|l| {
            l.gain_db
                .is_some_and(|g| (f64::from(g) - now_gain).abs() < 0.5)
        })
        .collect();
    let gain_db = if now.len() < MIN_RECORDINGS {
        None
    } else {
        let at = now.len();
        let clipped = now.iter().filter(|l| l.peak_db >= CLIP_DB).count();
        let peak = median(now.iter().map(|l| l.peak_db).collect());
        if clipped * 20 > at {
            Some((boost(audio.gain_db - 6.0), clipped))
        } else if peak < QUIET_PEAK_DB {
            let step = f64::from(CLIP_DB - 11 - peak);
            Some((
                boost(audio.gain_db + step),
                now.iter().filter(|l| l.peak_db < QUIET_PEAK_DB).count(),
            ))
        } else {
            None
        }
    }
    .filter(|&(g, _)| (g - audio.gain_db).abs() >= 3.0)
    .map(|(value, hits)| Advice {
        value,
        hits,
        of: now.len(),
    });

    let gaps: Vec<(u32, u32)> = levels
        .iter()
        .filter_map(|l| Some((l.start_gap_ms?, l.end_gap_ms?)))
        .collect();
    let touched = gaps
        .iter()
        .filter(|&&(start, end)| start < TOUCH_MS || end < TOUCH_MS)
        .count();
    let edges = (gaps.len() >= MIN_RECORDINGS
        && touched * 10 >= gaps.len() * 3
        && audio.edges() != EdgePreset::Careful)
        .then_some(Advice {
            value: EdgePreset::Careful,
            hits: touched,
            of: gaps.len(),
        });

    let rumble = (!audio.cut_rumble
        && median(levels.iter().map(|l| l.rumble_db).collect()) > RUMBLE_DB)
        .then(|| Advice {
            value: true,
            hits: count(&|l| l.rumble_db > RUMBLE_DB),
            of,
        });

    AudioAdvice {
        noise,
        gain_db,
        edges,
        rumble,
    }
}

/// What the "Record 3 s" check heard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicLevel {
    Quiet,
    Good,
    /// It clipped.
    Loud,
}

/// The check's verdict on one test recording, and the boost to offer (`None`: keep it).
#[must_use]
pub fn mic_check(levels: &Levels, gain_db: f64) -> (MicLevel, Option<f64>) {
    let (verdict, new) = if levels.peak_db >= CLIP_DB {
        (MicLevel::Loud, boost(gain_db - 6.0))
    } else if levels.speech_db < QUIET_SPEECH_DB {
        let step = f64::from(TARGET_SPEECH_DB - levels.speech_db);
        (MicLevel::Quiet, boost(gain_db + step))
    } else {
        (MicLevel::Good, gain_db)
    };
    (verdict, ((new - gain_db).abs() >= 0.5).then_some(new))
}

/// The newest days of use (a Home stats row each) the memory suggestion reads.
pub const MEMORY_DAYS: usize = 14;
/// Fewer days of use than this suggest no memory choice.
const MEMORY_MIN_DAYS: usize = 7;
/// More dictations a day than this, on average, keep the model always ready.
const BUSY_PER_DAY: u64 = 30;
/// Fewer dictations a day than this, on average, free the memory right after each.
const QUIET_PER_DAY: u64 = 3;

/// What the person's daily counts suggest for "Keep the model ready" set to `now` (hold item
/// 41, candidate 10): `days` are the Home stats rows, oldest first; the newest [`MEMORY_DAYS`]
/// with a dictation are read. Over 30 a day on average: Always ready; under 3: Save memory.
/// `hits` is that average (whole dictations a day), `of` the days read. Per day only: the stats
/// keep nothing finer (SPEC), so the time of day cannot count.
#[must_use]
pub fn memory(days: &[Day], now: MemoryMode) -> Option<Advice<MemoryMode>> {
    let used: Vec<u64> = days
        .iter()
        .rev()
        .map(|d| d.n.dictations)
        .filter(|&n| n > 0)
        .take(MEMORY_DAYS)
        .collect();
    if used.len() < MEMORY_MIN_DAYS {
        return None;
    }
    let of = used.len();
    let per_day = used.iter().sum::<u64>() / of as u64;
    let value = if per_day > BUSY_PER_DAY {
        MemoryMode::AlwaysReady
    } else if per_day < QUIET_PER_DAY {
        MemoryMode::SaveMemory
    } else {
        return None;
    };
    (value != now).then(|| Advice {
        value,
        hits: usize::try_from(per_day).unwrap_or(usize::MAX),
        of,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(peak_db: i16, floor_db: i16) -> Levels {
        Levels {
            peak_db,
            speech_db: peak_db - 10,
            floor_db,
            rumble_db: -40,
            start_gap_ms: Some(300),
            end_gap_ms: Some(300),
            gain_db: Some(0),
        }
    }

    /// `n` recordings made at boost `gain`.
    fn at(gain: i16, n: usize, peak_db: i16) -> Vec<Levels> {
        vec![
            Levels {
                gain_db: Some(gain),
                ..rec(peak_db, -70)
            };
            n
        ]
    }

    #[test]
    fn too_few_recordings_suggest_nothing() {
        let few = vec![rec(-40, -30); MIN_RECORDINGS - 1];
        assert_eq!(advise(&few, &Audio::default()), AudioAdvice::default());
    }

    #[test]
    fn a_noisy_room_suggests_a_blend_and_a_quiet_one_nothing() {
        let audio = Audio::default();
        let noisy = vec![rec(-10, -40); 20];
        let a = advise(&noisy, &audio).noise.unwrap();
        assert_eq!((a.value, a.hits, a.of), (Noise::Medium, 20, 20));
        assert_eq!(advise(&vec![rec(-10, -70); 20], &audio).noise, None);
        let mut strong = audio.clone();
        strong.set_noise(Noise::Strong);
        let off = advise(&vec![rec(-10, -70); 20], &strong).noise.unwrap();
        assert_eq!(
            off.value,
            Noise::Off,
            "strong cleaning in a quiet room: turn it off"
        );
        strong.set_noise(Noise::Auto);
        assert_eq!(
            advise(&noisy, &strong).noise,
            None,
            "Auto decides by itself"
        );
    }

    #[test]
    fn quiet_recordings_raise_the_boost_and_clipping_lowers_it() {
        let audio = Audio::default();
        let quiet = advise(&vec![rec(-36, -70); 12], &audio).gain_db.unwrap();
        assert_eq!(
            (quiet.value, quiet.hits),
            (20.0, 12),
            "capped at the slider's end"
        );
        let mut loud: Vec<Levels> = at(4, 18, -6);
        loud.extend(at(4, 1, 0));
        loud.extend(at(4, 1, -1));
        let mut a = audio.clone();
        a.gain_db = 4.0;
        let down = advise(&loud, &a).gain_db.unwrap();
        assert_eq!((down.value, down.hits), (-2.0, 2));
        assert_eq!(advise(&vec![rec(-6, -70); 20], &audio).gain_db, None);
    }

    #[test]
    fn a_boost_is_judged_on_recordings_made_at_it() {
        let mut audio = Audio::default();
        // Clipped at 0: lower to -6. Once -6 is set, the same old recordings say nothing.
        let old = at(0, 20, 0);
        assert_eq!(advise(&old, &audio).gain_db.unwrap().value, -6.0);
        audio.gain_db = -6.0;
        assert_eq!(
            advise(&old, &audio).gain_db,
            None,
            "no second step on old numbers"
        );
        // New recordings at -6 that still clip count on their own.
        let mut mixed = at(-6, 12, 0);
        mixed.extend(old);
        let a = advise(&mixed, &audio).gain_db.unwrap();
        assert_eq!((a.value, a.hits, a.of), (-12.0, 12, 12));
        // Quiet side: -20 with peaks -31 raised to -1; the old numbers do not offer +18 again.
        let quiet = at(-20, 20, -31);
        audio.gain_db = -20.0;
        assert_eq!(advise(&quiet, &audio).gain_db.unwrap().value, -1.0);
        audio.gain_db = -1.0;
        assert_eq!(advise(&quiet, &audio).gain_db, None);
        // Recordings without a saved boost never count.
        let none = vec![
            Levels {
                gain_db: None,
                ..rec(0, -70)
            };
            20
        ];
        assert_eq!(advise(&none, &Audio::default()).gain_db, None);
    }

    #[test]
    fn speech_touching_the_ends_suggests_careful_edges() {
        let mut cut = vec![rec(-6, -70); 14];
        for l in &mut cut[..5] {
            l.end_gap_ms = Some(20);
        }
        let a = advise(&cut, &Audio::default()).edges.unwrap();
        assert_eq!((a.value, a.hits, a.of), (EdgePreset::Careful, 5, 14));
        let mut careful = Audio::default();
        careful.set_edges(EdgePreset::Careful);
        assert_eq!(advise(&cut, &careful).edges, None);
        let no_trim = vec![
            Levels {
                start_gap_ms: None,
                end_gap_ms: None,
                ..rec(-6, -70)
            };
            20
        ];
        assert_eq!(advise(&no_trim, &Audio::default()).edges, None);
    }

    #[test]
    fn rumble_suggests_the_cut_until_it_is_on() {
        let hum = vec![
            Levels {
                rumble_db: -4,
                ..rec(-6, -70)
            };
            12
        ];
        assert_eq!(advise(&hum, &Audio::default()).rumble.unwrap().hits, 12);
        let on = Audio {
            cut_rumble: true,
            ..Audio::default()
        };
        assert_eq!(advise(&hum, &on).rumble, None);
    }

    #[test]
    fn only_the_newest_recordings_count() {
        let mut v = vec![rec(-6, -70); RECENT];
        v.extend(vec![rec(-6, -30); 100]);
        assert_eq!(advise(&v, &Audio::default()).noise, None);
    }

    #[test]
    fn mic_check_names_the_level_and_offers_a_boost() {
        let l = |peak_db, speech_db| Levels {
            peak_db,
            speech_db,
            ..Levels::default()
        };
        assert_eq!(mic_check(&l(-10, -24), 0.0), (MicLevel::Good, None));
        assert_eq!(mic_check(&l(-30, -45), 0.0), (MicLevel::Quiet, Some(20.0)));
        assert_eq!(mic_check(&l(-25, -41), 0.0), (MicLevel::Quiet, Some(16.0)));
        assert_eq!(mic_check(&l(0, -12), 3.0), (MicLevel::Loud, Some(-3.0)));
        assert_eq!(
            mic_check(&l(0, -12), -20.0),
            (MicLevel::Loud, None),
            "nothing lower to offer"
        );
    }

    #[test]
    fn memory_follows_dictations_a_day_over_the_days_of_use() {
        use crate::stats::{Date, Totals};
        let days = |counts: &[u64]| -> Vec<Day> {
            counts
                .iter()
                .enumerate()
                .map(|(i, &dictations)| Day {
                    date: {
                        let i = u32::try_from(i).unwrap();
                        Date::from_ymd(2026, 9 + i / 28, 1 + i % 28).unwrap()
                    },
                    n: Totals {
                        dictations,
                        ..Totals::default()
                    },
                })
                .collect()
        };
        let busy = memory(&days(&[40; 10]), MemoryMode::Balanced).unwrap();
        assert_eq!(
            (busy.value, busy.hits, busy.of),
            (MemoryMode::AlwaysReady, 40, 10)
        );
        assert_eq!(
            memory(&days(&[40; 10]), MemoryMode::AlwaysReady),
            None,
            "already set"
        );
        let quiet = memory(&days(&[1, 2, 1, 2, 1, 2, 1]), MemoryMode::Balanced).unwrap();
        assert_eq!((quiet.value, quiet.of), (MemoryMode::SaveMemory, 7));
        assert_eq!(
            memory(&days(&[10; 14]), MemoryMode::Custom),
            None,
            "in between: nothing"
        );
        assert_eq!(
            memory(&days(&[40; 6]), MemoryMode::Balanced),
            None,
            "too few days"
        );
        // Only the newest 14 days of use count: a busy past does not outvote a quiet present.
        let mut counts = vec![100; 20];
        counts.extend([1; 14]);
        let now = memory(&days(&counts), MemoryMode::Balanced).unwrap();
        assert_eq!((now.value, now.of), (MemoryMode::SaveMemory, 14));
    }
}
