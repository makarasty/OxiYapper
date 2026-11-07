//! Usage statistics for the Home page: per-day counts, totals, words per minute, streaks and
//! the activity heatmap (SPEC, History).
//!
//! What is kept, for each local calendar day: the number of finished dictations, their word
//! count and the length of their audio. Never the text, an app name, or a time finer than the
//! day. The caller supplies "today" as a local [`Date`], so this module needs no clock.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::audio::SAMPLE_RATE;
use crate::types::Millis;

/// The stats file in the data dir.
pub const FILE: &str = "stats.json";
/// A stats file that could not be read is renamed to this, and counting starts fresh.
pub const BAD_FILE: &str = "stats.bad";
/// Format version of [`FILE`]. A newer file is read for the fields this build knows and never
/// written.
pub const VERSION: u32 = 1;
/// Rows older than this many days fold into [`Stats::before`], so the file stays small.
pub const KEEP_DAYS: i32 = 3 * 366;
/// Weeks the Home heatmap shows.
pub const HEAT_WEEKS: u16 = 26;

/// A local calendar day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Date(
    /// Days since 1970-01-01.
    i32,
);

impl Date {
    /// `None` unless `y-m-d` is a real day of the years 1..=9999.
    pub fn from_ymd(y: i32, m: u32, d: u32) -> Option<Self> {
        let len = match m {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) => 29,
            2 => 28,
            _ => return None,
        };
        ((1..=9999).contains(&y) && (1..=len).contains(&d))
            .then(|| Self(days_from_civil(y, m.cast_signed(), d.cast_signed())))
    }

    /// The day `days` after 1970-01-01 (negative: before).
    pub const fn from_epoch_days(days: i32) -> Self {
        Self(days)
    }

    /// Year, month 1..=12, day 1..=31.
    pub fn ymd(self) -> (i32, u32, u32) {
        let (y, m, d) = civil_from_days(self.0);
        (y, m.cast_unsigned(), d.cast_unsigned())
    }

    #[must_use]
    pub const fn add_days(self, days: i32) -> Self {
        Self(self.0 + days)
    }

    /// 0 is Monday, 6 is Sunday.
    pub const fn weekday(self) -> i32 {
        // 1970-01-01 was a Thursday.
        (self.0 + 3).rem_euclid(7)
    }
}

/// Howard Hinnant's `days_from_civil`: proleptic Gregorian, March-based years.
const fn days_from_civil(y: i32, m: i32, d: i32) -> i32 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`].
const fn civil_from_days(days: i32) -> (i32, i32, i32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (y, m, d) = self.ymd();
        write!(f, "{y:04}-{m:02}-{d:02}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not a YYYY-MM-DD date")]
pub struct BadDate;

impl FromStr for Date {
    type Err = BadDate;

    fn from_str(s: &str) -> Result<Self, BadDate> {
        let mut parts = s.splitn(3, '-').map(str::parse::<u32>);
        let (Some(Ok(y)), Some(Ok(m)), Some(Ok(d))) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(BadDate);
        };
        let y = i32::try_from(y).map_err(|_| BadDate)?;
        Self::from_ymd(y, m, d).ok_or(BadDate)
    }
}

impl Serialize for Date {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Date {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Counts of one day, or folded counts of many.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    pub dictations: u64,
    pub words: u64,
    /// Length of the dictations' audio.
    pub speech_ms: u64,
}

impl Totals {
    fn add(&mut self, other: Self) {
        self.dictations = self.dictations.saturating_add(other.dictations);
        self.words = self.words.saturating_add(other.words);
        self.speech_ms = self.speech_ms.saturating_add(other.speech_ms);
    }
}

/// One row of [`FILE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Day {
    pub date: Date,
    #[serde(flatten)]
    pub n: Totals,
}

#[derive(Debug, thiserror::Error)]
pub enum StatsError {
    #[error("stats file: {0}")]
    Json(#[from] serde_json::Error),
}

/// The content of [`FILE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    pub version: u32,
    /// Rows older than [`KEEP_DAYS`], folded: all-time totals stay right.
    #[serde(default)]
    pub before: Totals,
    /// One row per day with at least one dictation, oldest first.
    #[serde(default)]
    pub days: Vec<Day>,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            version: VERSION,
            before: Totals::default(),
            days: Vec::new(),
        }
    }
}

impl Stats {
    /// Parses [`FILE`]; rows come out sorted with one per date, whatever order the file had.
    pub fn parse(json: &[u8]) -> Result<Self, StatsError> {
        let file: Self = serde_json::from_slice(json)?;
        let mut stats = Self {
            days: Vec::with_capacity(file.days.len()),
            ..file
        };
        for day in file.days {
            stats.add(day);
        }
        Ok(stats)
    }

    pub fn to_json(&self) -> Result<Vec<u8>, StatsError> {
        Ok(serde_json::to_vec_pretty(self)?)
    }

    /// A file of a newer build is never overwritten.
    pub const fn writable(&self) -> bool {
        self.version <= VERSION
    }

    /// One finished dictation on `today`, then rows older than [`KEEP_DAYS`] fold away.
    pub fn record(&mut self, today: Date, words: u64, speech: Millis) {
        self.add(Day {
            date: today,
            n: Totals {
                dictations: 1,
                words,
                speech_ms: speech.0,
            },
        });
        let keep = self
            .days
            .partition_point(|d| d.date <= today.add_days(-KEEP_DAYS));
        for old in self.days.drain(..keep) {
            self.before.add(old.n);
        }
    }

    /// Adds `day` to its row, keeping rows sorted (the clock may have gone back).
    fn add(&mut self, day: Day) {
        match self.days.binary_search_by_key(&day.date, |d| d.date) {
            Ok(i) => self.days[i].n.add(day.n),
            Err(i) => self.days.insert(i, day),
        }
    }

    /// What the Home page shows on `today`, with a heatmap of `weeks` weeks.
    pub fn summary(&self, today: Date, weeks: u16) -> Summary {
        let mut all = self.before;
        let mut spoken = Totals::default();
        for day in &self.days {
            all.add(day.n);
            if day.n.speech_ms > 0 {
                spoken.add(day.n);
            }
        }
        let (streak, longest_streak) = streaks(&self.days, today);
        let (heat_start, heat) = heatmap(&self.days, today, weeks);
        Summary {
            dictations: all.dictations,
            words: all.words,
            wpm: wpm(spoken.words, spoken.speech_ms),
            streak,
            longest_streak,
            heat_start,
            heat,
        }
    }
}

/// The Home page numbers (IPC `Push::Stats`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    /// All time.
    pub dictations: u64,
    /// All time.
    pub words: u64,
    /// Words per minute of audio over the kept days with audio; 0 before the first one.
    pub wpm: u32,
    /// Consecutive days with a dictation, ending today, or yesterday if today has none yet.
    pub streak: u32,
    pub longest_streak: u32,
    /// The Monday the heatmap starts on.
    pub heat_start: Date,
    /// One level 0..=4 per day from `heat_start` to today.
    pub heat: Vec<u8>,
}

/// Rounded words per minute; 0 without audio.
fn wpm(words: u64, speech_ms: u64) -> u32 {
    if speech_ms == 0 {
        return 0;
    }
    let wpm = words.saturating_mul(60_000).saturating_add(speech_ms / 2) / speech_ms;
    u32::try_from(wpm).unwrap_or(u32::MAX)
}

/// Current and longest run of consecutive days with a dictation, up to `today`.
fn streaks(days: &[Day], today: Date) -> (u32, u32) {
    let mut run = 0;
    let mut longest = 0;
    let mut last: Option<Date> = None;
    for date in days
        .iter()
        .filter(|d| d.n.dictations > 0 && d.date <= today)
        .map(|d| d.date)
    {
        run = if last.is_some_and(|l| l.add_days(1) == date) {
            run + 1
        } else {
            1
        };
        longest = longest.max(run);
        last = Some(date);
    }
    let current = if last.is_some_and(|l| l >= today.add_days(-1)) {
        run
    } else {
        0
    };
    (current, longest)
}

/// Levels from the Monday `weeks - 1` weeks before this week's to `today`. A day without a
/// dictation is 0; the others are 1..=4 by the quartiles of their word counts in the window.
fn heatmap(days: &[Day], today: Date, weeks: u16) -> (Date, Vec<u8>) {
    let Some(back) = weeks.checked_sub(1) else {
        return (today, Vec::new());
    };
    let start = today.add_days(-today.weekday() - 7 * i32::from(back));
    let from = days.partition_point(|d| d.date < start);
    let window: Vec<&Day> = days[from..]
        .iter()
        .filter(|d| d.date <= today && d.n.dictations > 0)
        .collect();
    let mut words: Vec<u64> = window.iter().map(|d| d.n.words).collect();
    words.sort_unstable();
    let quartiles = match words.len() {
        0 => [0; 3],
        n => [1, 2, 3].map(|q| words[(n - 1) * q / 4]),
    };
    let mut heat = Vec::new();
    let mut rows = window.iter().peekable();
    let mut date = start;
    while date <= today {
        let level = match rows.next_if(|d| d.date == date) {
            Some(d) => 1 + quartiles.iter().filter(|&&q| d.n.words > q).count(),
            None => 0,
        };
        heat.push(u8::try_from(level).unwrap_or(4));
        date = date.add_days(1);
    }
    (start, heat)
}

/// Words of a finished text: runs of letters and digits; `'`, `’` and `-` inside a run keep it
/// one word ("don't", "кто-то"). Any script.
pub fn word_count(text: &str) -> u64 {
    let words = text
        .split(|c: char| !(c.is_alphanumeric() || matches!(c, '\'' | '’' | '-')))
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .count();
    u64::try_from(words).unwrap_or(u64::MAX)
}

/// Length of `samples` mono samples at [`SAMPLE_RATE`].
pub fn audio_length(samples: usize) -> Millis {
    let samples = u64::try_from(samples).unwrap_or(u64::MAX);
    Millis(samples.saturating_mul(1000) / u64::from(SAMPLE_RATE))
}

/// A count for a stat tile: `146`, `11.3K`, `299.9K`, `1.2M`. Tenths are cut, not rounded, so
/// `999_999` never reads as `1000.0K`.
pub fn format_count(n: u64) -> String {
    let (unit, suffix) = match n {
        0..1_000 => return n.to_string(),
        1_000..1_000_000 => (1_000, 'K'),
        _ => (1_000_000, 'M'),
    };
    let tenths = n / (unit / 10);
    format!("{}.{}{suffix}", tenths / 10, tenths % 10)
}
