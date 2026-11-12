//! History retention (ARCHITECTURE sections 7 and 9): the file names of saved dictations, where
//! the folder is, and which entries go when the age or size limit is hit. Pure: the daemon lists
//! the folder, calls [`expired`] and deletes what it names. The app only works inside a folder
//! it made ([`OWNED_DIR`], [`MARKER`]) and only on names that pass the strict checks here.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::History;
use crate::stats::Date;
use crate::tuning::SettingsSet;
use crate::types::Millis;

/// Extension of the saved audio.
pub const WAV: &str = "wav";
/// Extension of the saved text.
pub const TXT: &str = "txt";
/// Extension of the meta file (see [`Meta`] and `docs/HISTORY-FORMAT.md`).
pub const JSON: &str = "json";
/// The files of one entry, in the order they are written.
pub const EXTENSIONS: [&str; 3] = [WAV, TXT, JSON];

/// The subfolder made inside a folder the user chose.
pub const OWNED_DIR: &str = "OxiYapper history";
/// The file that marks a history folder as made by this app; a folder without it is not touched.
pub const MARKER: &str = ".oxiyapper-history";

/// One saved dictation: its `YYYY-MM-DD/HHMMSS-mmm` stem and the bytes of its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub stem: String,
    pub bytes: u64,
}

/// `YYYY-MM-DD/HHMMSS-mmm` for a local day and the milliseconds since its midnight.
pub fn stem(date: Date, ms_of_day: u32) -> String {
    let ms = ms_of_day % 86_400_000;
    let (h, m, s) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60);
    format!("{date}/{h:02}{m:02}{s:02}-{:03}", ms % 1000)
}

/// The day a folder name stands for, when the name is exactly `YYYY-MM-DD`.
pub fn day_dir(name: &str) -> Option<Date> {
    let date = name.parse::<Date>().ok()?;
    (date.to_string() == name).then_some(date)
}

/// Is `name` exactly `HHMMSS-mmm` with a real time of day (the stem of a file in a day folder)?
pub fn is_file_stem(name: &str) -> bool {
    name.len() == 10
        && name.bytes().enumerate().all(|(i, b)| {
            if i == 6 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        })
        && &name[..2] < "24"
        && &name[2..4] < "60"
        && &name[4..6] < "60"
}

/// Is `name` what a killed write left: the `.wav.part`, `.txt.tmp` or `.json.tmp` of a history stem?
pub fn is_leftover(name: &str) -> bool {
    name.strip_suffix(".wav.part")
        .or_else(|| name.strip_suffix(".txt.tmp"))
        .or_else(|| name.strip_suffix(".json.tmp"))
        .is_some_and(is_file_stem)
}

/// Is `id` exactly the `YYYY-MM-DD/HHMMSS-mmm` stem of an entry? The only form the daemon accepts
/// from a caller, so an id can never name a path outside the folder.
pub fn is_id(id: &str) -> bool {
    id.split_once('/')
        .is_some_and(|(day, file)| day_dir(day).is_some() && is_file_stem(file))
}

/// `2026-11-08T13:00:00.123`: the local time an entry's stem stands for; `None` for a bad id.
pub fn started(id: &str) -> Option<String> {
    let (day, t) = id.split_once('/').filter(|_| is_id(id))?;
    Some(format!(
        "{day}T{}:{}:{}.{}",
        &t[..2],
        &t[2..4],
        &t[4..6],
        &t[7..]
    ))
}

/// How an entry ended (`status` in the meta file).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Transcribed and delivered.
    Done,
    /// Cancelled after speech was heard; the audio was kept, there is no text.
    Cancelled,
    /// The daemon died while recording; the audio was recovered at the next start.
    Interrupted,
    /// Recorded, but no transcript came out of it.
    Failed,
}

/// The meta file `<stem>.json` of an entry. The layout is a stable contract (R3.3): fields are
/// only ever added, and a reader must ignore the ones it does not know.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    /// Layout version of this file, [`Meta::VERSION`].
    pub version: u32,
    /// Local start of the recording, `YYYY-MM-DDTHH:MM:SS.mmm` (no zone).
    pub started: String,
    pub duration_ms: u64,
    pub status: Status,
    /// Registry id of the model that made the text, `custom` for a custom file; empty when unknown.
    pub model: String,
    /// `auto` or a language code; empty when unknown.
    pub language: String,
    /// The audio settings and model of the run that made the text; `None` when unknown.
    pub settings: Option<SettingsSet>,
    /// The other version of the text: what it was before the last transcribe-again or edit, or
    /// what a restore put aside. Absent until a text was replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<Previous>,
    /// The words of the audio with the confidence of each (and times once they were computed).
    /// Empty for an entry from before this field; see [`Word`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub words: Vec<Word>,
    /// How the word times were made; present when `words` carry times.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timings: Option<Timings>,
    /// The `.wav` is the raw microphone audio at the device's own rate, not what whisper got
    /// (a recording recovered after a crash). Stays set after a transcribe-again or an edit
    /// rewrote `status`, so a microphone that records at 16 kHz itself still reads as raw.
    #[serde(default, skip_serializing_if = "is_false")]
    pub raw: bool,
    /// The parts of the dictation the confidence filter held back, in order; empty when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub held: Vec<HeldPart>,
    /// How the text reached the user; `None` when not recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered: Option<Delivered>,
}

impl Meta {
    /// The meta of entry `id` known by its list fields only: what a reader makes up for an entry
    /// whose meta file is missing (saved by an older build, or a save that stopped early).
    #[must_use]
    pub fn basic(id: &str, duration_ms: u64, status: Status, model: &str, language: &str) -> Self {
        Self {
            version: Self::VERSION,
            started: started(id).unwrap_or_default(),
            duration_ms,
            status,
            model: model.to_owned(),
            language: language.to_owned(),
            settings: None,
            previous: None,
            words: Vec::new(),
            timings: None,
            raw: false,
            held: Vec::new(),
            delivered: None,
        }
    }
}

/// One part of a dictation held back by the confidence filter: its text is kept, not inserted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldPart {
    pub text: String,
    /// Start and end in ms from the start of the `.wav`.
    pub t0: u32,
    pub t1: u32,
    pub why: crate::text::HeldWhy,
}

/// How the text of an entry reached the user (`delivered` in the meta file).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Delivered {
    Pasted,
    Typed,
    /// Left on the clipboard.
    Clipboard,
    /// To-file mode.
    File,
    /// The confidence filter held all of it back; nothing was inserted, the `.txt` still has it.
    Filtered,
    Failed,
    /// To-file only, and the write failed: nothing reached the file or the field.
    #[serde(rename = "file_failed")]
    FileFailed,
    /// Also any value a newer version writes, so its entry keeps the rest of its meta.
    #[serde(other)]
    Unknown,
}

/// The text an entry had before it was replaced, with the run that made it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Previous {
    pub text: String,
    pub model: String,
    pub language: String,
    pub settings: Option<SettingsSet>,
    /// The kept text is the newer version: the current one is an old text a Restore brought back.
    #[serde(default, skip_serializing_if = "is_false")]
    pub newer: bool,
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "the `skip_serializing_if` signature"
)]
fn is_false(b: &bool) -> bool {
    !*b
}

/// A word with a confidence under this is shown as doubtful (display only, no setting).
pub const LOW_CONFIDENCE: f32 = 0.45;

/// One word of the audio: its text, the model's confidence in it and, once computed, its time.
/// `p` is clamped to 0..=1 by the producer, so words compare as equal values (no NaN).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub w: String,
    pub p: f32,
    /// Start and end in ms from the start of the `.wav`; absent until the times were computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t0: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t1: Option<u32>,
}

impl Eq for Word {}

impl Word {
    /// The word is one the model doubted.
    #[must_use]
    pub fn is_doubtful(&self) -> bool {
        self.p < LOW_CONFIDENCE
    }
}

/// How the times of `words` were made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimingBy {
    /// Cross-attention alignment (needs a model with a preset).
    Dtw,
    /// Whisper's own token timestamps.
    Tokens,
    /// Also any value a newer version writes, so its entry keeps the rest of its meta.
    #[serde(other)]
    Unknown,
}

/// The `timings` field: which method made the word times, with which preset and model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timings {
    pub by: TimingBy,
    /// DTW preset name (`large-v3`); empty for `tokens`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub preset: String,
    /// Registry id of the model that made them; empty when unknown.
    #[serde(default)]
    pub model: String,
}

impl Meta {
    pub const VERSION: u32 = 1;

    /// This entry's current text and run as the "other version" kept beside a new one.
    #[must_use]
    pub fn as_previous(&self, text: &str) -> Previous {
        Previous {
            text: text.to_owned(),
            model: self.model.clone(),
            language: self.language.clone(),
            settings: self.settings.clone(),
            newer: false,
        }
    }

    /// The entry after Restore (or Use new): the kept version becomes the current one and the
    /// current one is kept in its place, so the same call undoes it. `text` is the current text;
    /// the answer is the new meta and the new text. `None` when no other version is kept.
    #[must_use]
    pub fn swapped(&self, text: &str) -> Option<(Self, String)> {
        let kept = self.previous.as_ref()?;
        let meta = Self {
            status: Status::Done,
            model: kept.model.clone(),
            language: kept.language.clone(),
            settings: kept.settings.clone(),
            previous: Some(Previous {
                newer: !kept.newer,
                ..self.as_previous(text)
            }),
            ..self.clone()
        };
        Some((meta, kept.text.clone()))
    }

    /// # Errors
    /// Never in practice: every field serialises.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// # Errors
    /// The text is not a meta file of this layout.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

/// One piece of a word-by-word comparison of two texts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffPart {
    pub kind: DiffKind,
    /// The words with the spaces that follow them, as they stand in their text.
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    /// In both texts.
    Same,
    /// Only in the old text.
    Removed,
    /// Only in the new text.
    Added,
}

/// Most cells of the comparison table; above it the middle of two texts is shown as one removed
/// and one added run.
const DIFF_CELLS: usize = 4_000_000;

/// A word-by-word comparison of `old` and `new` (longest common subsequence of their words, exact
/// match including punctuation). The `Same` and `Added` parts joined give `new`; `Same` and
/// `Removed` give `old` (up to the white space between words). A removed run comes before the
/// added run that replaces it.
///
/// shortcut: two long texts that differ in the middle by more than `DIFF_CELLS` word pairs show
/// the middle as one removed and one added run; use a linear-space diff if that ever shows.
#[must_use]
pub fn word_diff(old: &str, new: &str) -> Vec<DiffPart> {
    let (old_t, new_t) = (tokens(old), tokens(new));
    let same = |left: &str, right: &str| left.trim() == right.trim();
    let head = old_t
        .iter()
        .zip(&new_t)
        .take_while(|(left, right)| same(left, right))
        .count();
    let tail = old_t[head..]
        .iter()
        .rev()
        .zip(new_t[head..].iter().rev())
        .take_while(|(left, right)| same(left, right))
        .count();
    let (old_mid, new_mid) = (
        &old_t[head..old_t.len() - tail],
        &new_t[head..new_t.len() - tail],
    );
    let mut out: Vec<DiffPart> = Vec::new();
    let mut push = |kind: DiffKind, token: &str| match out.last_mut() {
        Some(part) if part.kind == kind => part.text.push_str(token),
        _ => out.push(DiffPart {
            kind,
            text: token.to_owned(),
        }),
    };
    for token in &new_t[..head] {
        push(DiffKind::Same, token);
    }
    if (old_mid.len() + 1).saturating_mul(new_mid.len() + 1) > DIFF_CELLS {
        for token in old_mid {
            push(DiffKind::Removed, token);
        }
        for token in new_mid {
            push(DiffKind::Added, token);
        }
    } else {
        // `lcs[i * stride + j]`: common words of `old_mid[i..]` and `new_mid[j..]`.
        let stride = new_mid.len() + 1;
        let mut lcs = vec![0_u32; (old_mid.len() + 1) * stride];
        for i in (0..old_mid.len()).rev() {
            for j in (0..new_mid.len()).rev() {
                lcs[i * stride + j] = if same(old_mid[i], new_mid[j]) {
                    lcs[(i + 1) * stride + j + 1] + 1
                } else {
                    lcs[(i + 1) * stride + j].max(lcs[i * stride + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < old_mid.len() || j < new_mid.len() {
            if i < old_mid.len() && j < new_mid.len() && same(old_mid[i], new_mid[j]) {
                push(DiffKind::Same, new_mid[j]);
                (i, j) = (i + 1, j + 1);
            } else if i < old_mid.len()
                && (j == new_mid.len() || lcs[(i + 1) * stride + j] >= lcs[i * stride + j + 1])
            {
                push(DiffKind::Removed, old_mid[i]);
                i += 1;
            } else {
                push(DiffKind::Added, new_mid[j]);
                j += 1;
            }
        }
    }
    for token in &new_t[new_t.len() - tail..] {
        push(DiffKind::Same, token);
    }
    out
}

/// The words of `text`, each with the white space after it; leading white space goes with the
/// first word.
fn tokens(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let (mut in_space, mut seen_word) = (false, false);
    for (at, c) in text.char_indices() {
        if c.is_whitespace() {
            in_space = true;
        } else {
            if in_space && seen_word {
                out.push(&text[start..at]);
                start = at;
            }
            (in_space, seen_word) = (false, true);
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// Lower case, letters and digits only: what `align` compares.
fn plain(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Times for the `shown` words of a text from the `timed` words of a pass over the audio, which
/// can differ from it (replacements, edits). Words are paired by longest common subsequence of
/// their lower-cased letters; a word with no partner (or a partner with no time) gets a slice of
/// the gap between its neighbours' times, shared by letter count. `None` for every word when
/// `timed` has no times at all.
///
/// shortcut: two lists whose table would pass `DIFF_CELLS` pair by position (index scaled to the
/// other list) instead of by words; use a linear-space alignment if that ever shows.
#[must_use]
pub fn align(shown: &[&str], timed: &[Word]) -> Vec<Option<(Millis, Millis)>> {
    let mut out = vec![None; shown.len()];
    let stamps: Vec<Option<(u32, u32)>> = timed.iter().map(|w| w.t0.zip(w.t1)).collect();
    let (Some(first), Some(last)) = (
        stamps.iter().flatten().next().copied(),
        stamps.iter().flatten().next_back().copied(),
    ) else {
        return out;
    };
    let (a, b): (Vec<String>, Vec<String>) = (
        shown.iter().map(|w| plain(w)).collect(),
        timed.iter().map(|w| plain(&w.w)).collect(),
    );
    let same = |i: usize, j: usize| !a[i].is_empty() && a[i] == b[j];
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    if (a.len() + 1).saturating_mul(b.len() + 1) > DIFF_CELLS {
        pairs.extend((0..a.len()).map(|i| (i, (i * b.len() / a.len()).min(b.len() - 1))));
    } else {
        let stride = b.len() + 1;
        let mut lcs = vec![0_u32; (a.len() + 1) * stride];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                lcs[i * stride + j] = if same(i, j) {
                    lcs[(i + 1) * stride + j + 1] + 1
                } else {
                    lcs[(i + 1) * stride + j].max(lcs[i * stride + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < a.len() && j < b.len() {
            if same(i, j) {
                pairs.push((i, j));
                (i, j) = (i + 1, j + 1);
            } else if lcs[(i + 1) * stride + j] >= lcs[i * stride + j + 1] {
                i += 1;
            } else {
                j += 1;
            }
        }
    }
    for (i, j) in pairs {
        out[i] = stamps[j].map(|(t0, t1)| (Millis(t0.into()), Millis(t1.max(t0).into())));
    }
    // Words without a time share the gap before the next timed one; the ends of the list use the
    // first and last time of the pass.
    let mut at = 0;
    let mut from = u64::from(first.0);
    while at < out.len() {
        if let Some((_, end)) = out[at] {
            from = end.0;
            at += 1;
            continue;
        }
        let run = out[at..].iter().take_while(|t| t.is_none()).count();
        let to = out
            .get(at + run)
            .copied()
            .flatten()
            .map_or(u64::from(last.1), |(start, _)| start.0)
            .max(from);
        let weights: Vec<u64> = a[at..at + run]
            .iter()
            .map(|w| w.chars().count().max(1) as u64)
            .collect();
        let total: u64 = weights.iter().sum();
        let mut done = 0;
        for (slot, weight) in out[at..at + run].iter_mut().zip(weights) {
            let (s, e) = (done, done + weight);
            done = e;
            *slot = Some((
                Millis(from + (to - from) * s / total),
                Millis(from + (to - from) * e / total),
            ));
        }
        at += run;
    }
    out
}

/// A stretch with no voice this long or longer is a pause the player can jump over.
pub const SKIP_MIN_PAUSE: Millis = Millis(1000);
/// What of a pause stays at each side that touches voice: a word edge is never cut and a breath
/// stays.
pub const SKIP_KEEP: Millis = Millis(150);

/// Parts of the audio the player jumps over: every stretch with no voice of [`SKIP_MIN_PAUSE`] or
/// longer (also at the start and the end), shortened by [`SKIP_KEEP`] at each side that touches
/// voice. `voiced` are the voice runs in order. With no voice run at all it returns nothing: a
/// record with no voice found plays as it is (it may be a quiet one, and the gate is not proof of
/// silence).
#[must_use]
pub fn skips(voiced: &[std::ops::Range<Millis>], total: Millis) -> Vec<std::ops::Range<Millis>> {
    if voiced.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    // The gaps: before the first run, between runs, after the last; each side that is voice keeps
    // `SKIP_KEEP`.
    let mut gap_start = (0, false);
    let ends = voiced
        .iter()
        .map(|r| (r.start.0.min(total.0), r.end.0.min(total.0)))
        .chain(std::iter::once((total.0, total.0)));
    for (i, (start, end)) in ends.enumerate() {
        let last = i == voiced.len();
        let (from, after_voice) = gap_start;
        if start.saturating_sub(from) >= SKIP_MIN_PAUSE.0 {
            let a = if after_voice {
                from + SKIP_KEEP.0
            } else {
                from
            };
            let b = if last { start } else { start - SKIP_KEEP.0 };
            if a < b {
                out.push(Millis(a)..Millis(b));
            }
        }
        gap_start = (gap_start.0.max(end), true);
    }
    out
}

/// Lower case with `ё` read as `е`, the form search compares in.
#[must_use]
pub fn fold(text: &str) -> String {
    text.to_lowercase().replace('ё', "е")
}

/// What a History search narrows to; every part must hold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    /// The recordings of this local day.
    pub day: Option<Date>,
    /// Inclusive bounds on the length of the recording.
    pub min_ms: Option<u64>,
    pub max_ms: Option<u64>,
}

/// One recording as search sees it. `text_lower` is the text through [`fold`].
#[derive(Debug, Clone, Copy)]
pub struct Searched<'a> {
    pub text_lower: &'a str,
    pub day: Date,
    pub duration_ms: u64,
}

/// Does `entry` pass `filter` and hold every word of `query` (any order, as part of a word)? A
/// query with no words matches all. No ranking: a dictation list is a timeline.
#[must_use]
pub fn matches(query: &str, filter: &Filter, entry: &Searched<'_>) -> bool {
    filter.day.is_none_or(|d| d == entry.day)
        && filter.min_ms.is_none_or(|m| entry.duration_ms >= m)
        && filter.max_ms.is_none_or(|m| entry.duration_ms <= m)
        && fold(query)
            .split_whitespace()
            .all(|word| entry.text_lower.contains(word))
}

/// How far back from the end of a chunk [`chunks`] looks for a pause to cut in.
pub const CHUNK_SEEK: Millis = Millis(30_000);

/// The stretches of the audio to transcribe one by one: none longer than `max`, in order, covering
/// `0..total` once. A cut goes into the longest pause (a gap between `voiced` runs) in the last
/// [`CHUNK_SEEK`] of a chunk, at its middle; with no pause there it is a hard cut at `max`.
#[must_use]
pub fn chunks(
    voiced: &[std::ops::Range<Millis>],
    total: Millis,
    max: Millis,
) -> Vec<std::ops::Range<Millis>> {
    let max = max.0.max(1);
    let mut out = Vec::new();
    let mut start = 0;
    while total.0 - start > max {
        let end = start + max;
        let from = end.saturating_sub(CHUNK_SEEK.0).max(start + 1);
        // The longest part of a gap inside `from..end`.
        let mut best: Option<(u64, u64)> = None;
        let mut gap_from = 0;
        let runs = voiced
            .iter()
            .map(|r| (r.start.0, r.end.0))
            .chain(std::iter::once((total.0, total.0)));
        for (run_start, run_end) in runs {
            let (a, b) = (gap_from.max(from), run_start.min(end));
            if a < b && best.is_none_or(|(x, y)| b - a > y - x) {
                best = Some((a, b));
            }
            gap_from = gap_from.max(run_end);
        }
        let cut = best.map_or(end, |(a, b)| a + (b - a) / 2);
        out.push(Millis(start)..Millis(cut));
        start = cut;
    }
    if start < total.0 || out.is_empty() {
        out.push(Millis(start)..total);
    }
    out
}

/// A record written to a file (History export). Pure; the window writes the file.
pub mod export {
    use std::fmt::Write;

    use super::{Meta, Word};

    /// A cue ends at a pause this long between two words (ms)...
    const PAUSE_MS: u32 = 400;
    /// ...at this many characters...
    const MAX_CHARS: usize = 84;
    /// ...or this long (ms).
    const MAX_MS: u32 = 5_000;

    /// The text as it is.
    #[must_use]
    pub fn txt(text: &str) -> String {
        text.to_owned()
    }

    /// The meta object of the record plus `"text"`.
    ///
    /// # Errors
    /// Only a meta that cannot be written as JSON.
    pub fn json(meta: &Meta, text: &str) -> Result<String, serde_json::Error> {
        let mut value = serde_json::to_value(meta)?;
        if let Some(object) = value.as_object_mut() {
            object.insert("text".into(), text.into());
        }
        serde_json::to_string_pretty(&value)
    }

    /// `SubRip` cues; empty when no word has times.
    #[must_use]
    pub fn srt(words: &[Word]) -> String {
        cues(words, ',', true)
    }

    /// `WebVTT` cues; the bare header when no word has times.
    #[must_use]
    pub fn vtt(words: &[Word]) -> String {
        format!("WEBVTT\n\n{}", cues(words, '.', false))
    }

    fn clock(ms: u32, mark: char) -> String {
        format!(
            "{:02}:{:02}:{:02}{mark}{:03}",
            ms / 3_600_000,
            ms / 60_000 % 60,
            ms / 1000 % 60,
            ms % 1000
        )
    }

    /// One cue being built: start, end, text.
    type Cue = (u32, u32, String);

    fn cues(words: &[Word], mark: char, numbered: bool) -> String {
        let mut out = String::new();
        let mut n = 0;
        let mut cue: Option<Cue> = None;
        let mut flush = |cue: &mut Option<Cue>, out: &mut String| {
            if let Some((a, b, text)) = cue.take() {
                n += 1;
                if numbered {
                    let _ = writeln!(out, "{n}");
                }
                let _ = write!(out, "{} --> {}\n{text}\n\n", clock(a, mark), clock(b, mark));
            }
        };
        for word in words {
            let (Some(t0), Some(t1)) = (word.t0, word.t1) else {
                continue;
            };
            let text = word.w.trim();
            if text.is_empty() {
                continue;
            }
            if let Some((a, b, held)) = &cue {
                let wide = t0.saturating_sub(*b) >= PAUSE_MS
                    || held.chars().count() + 1 + text.chars().count() > MAX_CHARS
                    || t1.saturating_sub(*a) > MAX_MS
                    || held.ends_with(['.', '!', '?', '\u{2026}']);
                if wide {
                    flush(&mut cue, &mut out);
                }
            }
            match &mut cue {
                Some((_, b, held)) => {
                    *b = t1;
                    held.push(' ');
                    held.push_str(text);
                }
                None => cue = Some((t0, t1, text.to_owned())),
            }
        }
        flush(&mut cue, &mut out);
        out
    }
}

/// Bytes of the canonical 16-bit mono PCM wav header the spool writes.
pub const WAV_HEADER: usize = 44;

/// A spool file with its header made true to the bytes on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repaired {
    pub header: [u8; WAV_HEADER],
    /// Whole bytes of audio the file keeps; a half sample at the end is cut.
    pub data_bytes: u64,
    pub sample_rate: u32,
}

/// The header of a recording killed mid-write: the sizes in it are those of the last flush, so
/// they are set again from the real `file_len`. `None` when `header` is not the canonical
/// 16-bit mono PCM header (then the file is not ours to repair).
pub fn repaired_header(header: &[u8], file_len: u64) -> Option<Repaired> {
    let header: &[u8; WAV_HEADER] = header.get(..WAV_HEADER)?.try_into().ok()?;
    let word = |at: usize| {
        u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
    };
    let half = |at: usize| u16::from_le_bytes([header[at], header[at + 1]]);
    let canonical = &header[..4] == b"RIFF"
        && &header[8..16] == b"WAVEfmt "
        && word(16) == 16
        && half(20) == 1 // PCM
        && half(22) == 1 // mono
        && half(34) == 16
        && &header[36..40] == b"data"
        && word(24) > 0;
    if !canonical {
        return None;
    }
    let size =
        u32::try_from((file_len.checked_sub(WAV_HEADER as u64)?).min(u64::from(u32::MAX - 36)))
            .ok()?
            & !1; // whole samples only
    let mut fixed = *header;
    fixed[4..8].copy_from_slice(&(36 + size).to_le_bytes());
    fixed[40..44].copy_from_slice(&size.to_le_bytes());
    Some(Repaired {
        header: fixed,
        data_bytes: u64::from(size),
        sample_rate: word(24),
    })
}

/// The app's own folder history goes to: [`OWNED_DIR`] inside `[history] folder` when that is an
/// absolute path, else `default` (the data directory's `history/`, which is the app's already).
pub fn folder(cfg: &History, default: &Path) -> PathBuf {
    let set = Path::new(cfg.folder.trim());
    if set.is_absolute() {
        set.join(OWNED_DIR)
    } else {
        default.to_owned()
    }
}

fn entry_day(e: &Entry) -> Option<Date> {
    e.stem.split_once('/').and_then(|(day, _)| day_dir(day))
}

/// The stems to delete: every entry older than `max_days` local days (the last 30 days keep 30
/// day folders, today included), strictly by date, then the oldest ones until the rest fits
/// `max_bytes`. An entry dated after `today` (the clock was set back) is never deleted by age.
/// A limit of 0 is off. The size limit needs no clock.
pub fn expired(entries: &[Entry], today: Date, cfg: &History) -> Vec<String> {
    let mut sorted: Vec<&Entry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.stem.cmp(&b.stem)); // fixed-width names: oldest first
    let cutoff = (cfg.max_days > 0)
        .then(|| today.add_days(-i32::try_from(cfg.max_days).unwrap_or(i32::MAX)));
    // `cutoff` is before `today`, so a future-dated entry is never past it.
    let old = |e: &Entry| cutoff.is_some_and(|c| entry_day(e).is_some_and(|d| d <= c));
    // Age frees size first: the cap counts only what the age limit leaves.
    let mut total: u64 = sorted.iter().filter(|e| !old(e)).map(|e| e.bytes).sum();
    let mut gone = Vec::new();
    for e in sorted {
        if old(e) {
            gone.push(e.stem.clone());
        } else if cfg.max_bytes > 0 && total > cfg.max_bytes {
            total -= e.bytes;
            gone.push(e.stem.clone());
        }
    }
    gone
}

#[cfg(test)]
mod tests {
    use super::*;

    /// d04-fix review: a `delivered` value from a newer version reads as `unknown`, it does not
    /// fail the whole meta.
    #[test]
    fn an_unknown_delivered_value_reads_as_unknown() {
        let read = |v: &str| serde_json::from_str::<Delivered>(&format!("\"{v}\"")).ok();
        assert_eq!(read("file_failed"), Some(Delivered::FileFailed));
        assert_eq!(read("sent_by_mail"), Some(Delivered::Unknown));
    }

    /// A `why` or a `timings.by` from a newer version reads as `unknown`; the rest of the meta loads.
    #[test]
    fn unknown_held_why_and_timing_by_keep_the_rest_of_the_meta() {
        let basic = Meta::basic("2026-11-08/000000-000", 900, Status::Done, "m", "en");
        let mut v = serde_json::to_value(basic).unwrap();
        v["held"] = serde_json::json!([{"text": "x", "t0": 0, "t1": 5, "why": "from_the_future"}]);
        v["timings"] = serde_json::json!({"by": "from_the_future"});
        let meta: Meta = serde_json::from_value(v).unwrap();
        assert_eq!(meta.held[0].why, crate::text::HeldWhy::Unknown);
        assert_eq!(meta.timings.unwrap().by, TimingBy::Unknown);
        assert_eq!(meta.model, "m");
    }

    fn day(y: i32, m: u32, d: u32) -> Date {
        Date::from_ymd(y, m, d).unwrap()
    }

    fn entry(date: Date, time: &str, bytes: u64) -> Entry {
        Entry {
            stem: format!("{date}/{time}"),
            bytes,
        }
    }

    fn cfg(max_days: u32, max_bytes: u64) -> History {
        History {
            max_bytes,
            max_days,
            ..History::default()
        }
    }

    #[test]
    fn stem_is_local_date_and_time_of_day() {
        assert_eq!(
            stem(day(2026, 11, 8), 13 * 3_600_000 + 45 * 60_000 + 1_123),
            "2026-11-08/134501-123"
        );
        assert_eq!(stem(day(2026, 1, 2), 0), "2026-01-02/000000-000");
    }

    #[test]
    fn only_exact_history_names_pass() {
        assert!(day_dir("2026-11-08").is_some());
        for bad in ["2026-1-8", "2026-11-08x", "2026-13-01", "notes", ""] {
            assert!(day_dir(bad).is_none(), "{bad}");
        }
        assert!(is_file_stem("134501-123"));
        assert!(is_file_stem("235959-999"));
        for bad in [
            "134501-12",
            "13450a-123",
            "134501_123",
            "readme",
            "",
            "240000-000",
            "136000-000",
            "130060-000",
        ] {
            assert!(!is_file_stem(bad), "{bad}");
        }
    }

    #[test]
    fn only_leftovers_of_a_history_stem_count() {
        for good in ["134501-123.wav.part", "134501-123.txt.tmp"] {
            assert!(is_leftover(good), "{good}");
        }
        for bad in [
            "134501-123.wav",
            "134501-123.txt.part",
            "134501-123.wav.tmp",
            "song.wav.part",
            "240000-000.wav.part",
            "notes.txt.tmp",
            ".wav.part",
        ] {
            assert!(!is_leftover(bad), "{bad}");
        }
    }

    #[test]
    fn folder_is_the_owned_subfolder_or_the_default() {
        let default = Path::new("data/history");
        assert_eq!(folder(&History::default(), default), default);
        let rel = History {
            folder: "rel/dir".into(),
            ..History::default()
        };
        assert_eq!(folder(&rel, default), default);
        let abs = std::env::temp_dir();
        let set = History {
            folder: abs.to_string_lossy().into_owned(),
            ..History::default()
        };
        assert_eq!(folder(&set, default), abs.join(OWNED_DIR));
    }

    #[test]
    fn nothing_to_delete_inside_both_limits() {
        let today = day(2026, 11, 8);
        let entries = [
            entry(day(2026, 10, 20), "100000-000", 10),
            entry(today, "100000-000", 10),
        ];
        assert_eq!(
            expired(&entries, today, &cfg(30, 100)),
            Vec::<String>::new()
        );
        assert_eq!(expired(&[], today, &cfg(30, 100)), Vec::<String>::new());
    }

    #[test]
    fn age_removes_days_older_than_the_last_n() {
        let today = day(2026, 11, 8);
        // The last 3 days are 11-06, 11-07 and 11-08.
        let entries = [
            entry(day(2026, 11, 5), "100000-000", 1),
            entry(day(2026, 11, 6), "100000-000", 1),
            entry(today, "100000-000", 1),
        ];
        assert_eq!(
            expired(&entries, today, &cfg(3, 0)),
            ["2026-11-05/100000-000"]
        );
    }

    #[test]
    fn size_removes_the_oldest_until_it_fits() {
        let today = day(2026, 11, 8);
        let entries = [
            entry(today, "120000-000", 40), // given out of order on purpose
            entry(today, "100000-000", 40),
            entry(today, "110000-000", 40),
        ];
        assert_eq!(
            expired(&entries, today, &cfg(0, 100)),
            ["2026-11-08/100000-000"]
        );
        assert_eq!(expired(&entries, today, &cfg(0, 119)).len(), 1);
        assert_eq!(expired(&entries, today, &cfg(0, 79)).len(), 2);
    }

    #[test]
    fn both_limits_apply_and_the_age_frees_size_first() {
        let today = day(2026, 11, 8);
        let entries = [
            entry(day(2026, 10, 8), "100000-000", 90), // too old
            entry(day(2026, 11, 7), "100000-000", 60),
            entry(today, "100000-000", 60),
        ];
        // Age alone leaves 120 bytes, over the cap of 100: the older recent entry goes too.
        assert_eq!(
            expired(&entries, today, &cfg(30, 100)),
            ["2026-10-08/100000-000", "2026-11-07/100000-000"]
        );
        // Without the age, 210 bytes drop to 120 and then to 60: two oldest.
        assert_eq!(expired(&entries, today, &cfg(0, 100)).len(), 2);
    }

    /// SPEC History (owner 2026-11-10): age retention is strict by date; only entries dated after
    /// today (the clock was set back) are kept from it.
    #[test]
    fn the_age_limit_is_strict_by_date_and_skips_future_entries() {
        let now = day(2026, 11, 8);
        let far = day(2099, 1, 1);
        let table = [
            // today, entry days, how many the age limit of 30 days deletes
            (
                "dictation 60 days apart",
                now,
                vec![day(2026, 8, 1), day(2026, 10, 1)],
                2,
            ),
            (
                "gap between two entries",
                now,
                vec![day(2026, 8, 1), day(2026, 10, 20), now],
                1,
            ),
            ("clock jumped forward", far, vec![day(2026, 11, 7), now], 2),
            (
                "clock set back: future entries stay",
                now,
                vec![day(2026, 10, 1), day(2026, 11, 9), far],
                1,
            ),
            (
                "normal daily use",
                now,
                (1..=10).map(|d| day(2026, 10, d)).chain([now]).collect(),
                9,
            ),
            (
                "the 31st day back goes, the 30th stays",
                now,
                vec![day(2026, 10, 9), day(2026, 10, 10)],
                1,
            ),
        ];
        for (name, today, days, gone) in table {
            let entries: Vec<_> = days.iter().map(|&d| entry(d, "100000-000", 1)).collect();
            assert_eq!(expired(&entries, today, &cfg(30, 0)).len(), gone, "{name}");
        }
    }

    #[test]
    fn the_size_limit_works_with_any_clock() {
        let entries = [
            entry(day(2026, 11, 7), "100000-000", 60),
            entry(day(2026, 11, 8), "100000-000", 60),
        ];
        assert_eq!(
            expired(&entries, day(2000, 1, 1), &cfg(30, 100)),
            ["2026-11-07/100000-000"]
        );
    }

    #[test]
    fn limit_zero_keeps_everything() {
        let today = day(2026, 11, 8);
        let entries = [entry(day(2020, 1, 1), "100000-000", u64::MAX / 4)];
        assert_eq!(expired(&entries, today, &cfg(0, 0)), Vec::<String>::new());
    }

    fn meta(status: Status) -> Meta {
        Meta {
            version: Meta::VERSION,
            started: "2026-11-08T13:00:00.123".into(),
            duration_ms: 4_500,
            status,
            model: "small".into(),
            language: "ru".into(),
            settings: Some(SettingsSet {
                normalize: true,
                denoise: false,
                trim: true,
                model: "small".into(),
            }),
            previous: None,
            words: Vec::new(),
            timings: None,
            raw: false,
            held: Vec::new(),
            delivered: None,
        }
    }

    #[test]
    fn meta_round_trips_and_names_the_status_in_lowercase() {
        for status in [
            Status::Done,
            Status::Cancelled,
            Status::Interrupted,
            Status::Failed,
        ] {
            let m = meta(status);
            assert_eq!(Meta::from_json(&m.to_json().unwrap()).unwrap(), m);
        }
        let json = meta(Status::Interrupted).to_json().unwrap();
        assert!(json.contains("\"status\": \"interrupted\""), "{json}");
        let none = Meta {
            settings: None,
            ..meta(Status::Done)
        };
        assert_eq!(Meta::from_json(&none.to_json().unwrap()).unwrap(), none);
    }

    #[test]
    fn a_reader_ignores_fields_it_does_not_know() {
        let mut json: serde_json::Value =
            serde_json::from_str(&meta(Status::Done).to_json().unwrap()).unwrap();
        json["added_later"] = serde_json::json!([1, 2]);
        assert_eq!(
            Meta::from_json(&json.to_string()).unwrap(),
            meta(Status::Done)
        );
    }

    #[test]
    fn an_id_is_a_day_and_a_time_and_nothing_else() {
        assert!(is_id("2026-11-08/134501-123"));
        for bad in [
            "",
            "../x/134501-123",
            "2026-11-08/../134501-123",
            "2026-11-08/134501-123/x",
            r"2026-11-08\134501-123",
            "2026-11-08/134501-123.wav",
            "134501-123",
        ] {
            assert!(!is_id(bad), "{bad}");
        }
        assert_eq!(
            started("2026-11-08/134501-123").as_deref(),
            Some("2026-11-08T13:45:01.123")
        );
        assert_eq!(started("nope"), None);
    }

    #[test]
    fn a_meta_leftover_is_a_leftover() {
        assert!(is_leftover("134501-123.json.tmp"));
        assert!(!is_leftover("notes.json.tmp"));
    }

    /// A header as the spool writes it (hound, 16 kHz mono 16-bit), with the sizes of an old flush.
    fn header(rate: u32, flushed_data: u32) -> Vec<u8> {
        let mut h = b"RIFF".to_vec();
        h.extend((36 + flushed_data).to_le_bytes());
        h.extend(b"WAVEfmt ");
        h.extend(16_u32.to_le_bytes());
        h.extend([1, 0, 1, 0]); // PCM, mono
        h.extend(rate.to_le_bytes());
        h.extend((rate * 2).to_le_bytes());
        h.extend([2, 0, 16, 0]);
        h.extend(b"data");
        h.extend(flushed_data.to_le_bytes());
        h
    }

    #[test]
    fn a_truncated_spool_gets_the_sizes_of_the_bytes_it_has() {
        // Flushed at 100 bytes of audio, killed after 1001 bytes (the last sample is half).
        let got = repaired_header(&header(16_000, 100), 44 + 1001).unwrap();
        assert_eq!(got.data_bytes, 1000);
        assert_eq!(got.sample_rate, 16_000);
        assert_eq!(got.header[..], header(16_000, 1000)[..]);
        // Only the header was ever written: nothing to keep, but a valid result.
        assert_eq!(
            repaired_header(&header(48_000, 0), 44).unwrap().data_bytes,
            0
        );
    }

    #[test]
    fn a_file_that_is_not_our_spool_is_not_repaired() {
        let good = header(16_000, 0);
        assert!(repaired_header(&good[..40], 100).is_none(), "short header");
        assert!(
            repaired_header(&good, 20).is_none(),
            "shorter than a header"
        );
        for (at, byte) in [(0, b'X'), (22, 2), (34, 8), (20, 3), (24, 0)] {
            let mut bad = good.clone();
            bad[at] = byte;
            if at == 24 {
                bad[25..28].fill(0);
            }
            assert!(repaired_header(&bad, 100).is_none(), "byte {at}");
        }
    }

    fn joined(parts: &[DiffPart], keep: [DiffKind; 2]) -> String {
        parts
            .iter()
            .filter(|p| keep.contains(&p.kind))
            .map(|p| p.text.as_str())
            .collect()
    }

    fn shape(parts: &[DiffPart]) -> Vec<(DiffKind, &str)> {
        parts.iter().map(|p| (p.kind, p.text.as_str())).collect()
    }

    #[test]
    fn the_diff_of_equal_texts_is_one_unchanged_part() {
        assert_eq!(
            shape(&word_diff("a b  c", "a b  c")),
            [(DiffKind::Same, "a b  c")]
        );
        assert_eq!(word_diff("", ""), []);
    }

    #[test]
    fn the_diff_marks_a_replaced_an_added_and_a_removed_word() {
        use DiffKind::{Added, Removed, Same};
        assert_eq!(
            shape(&word_diff("one two three", "one 2 three")),
            [
                (Same, "one "),
                (Removed, "two "),
                (Added, "2 "),
                (Same, "three")
            ]
        );
        assert_eq!(
            shape(&word_diff("one three", "one two three")),
            [(Same, "one "), (Added, "two "), (Same, "three")]
        );
        assert_eq!(
            shape(&word_diff("one two three", "one three")),
            [(Same, "one "), (Removed, "two "), (Same, "three")]
        );
        assert_eq!(shape(&word_diff("", "new text")), [(Added, "new text")]);
        assert_eq!(shape(&word_diff("gone", "")), [(Removed, "gone")]);
    }

    #[test]
    fn the_diff_parts_give_both_texts_back() {
        let pairs = [
            ("привет мир как дела", "Привет, мир. Как дела"),
            ("a b c d e f", "f e d c b a"),
            ("  lead and trail  ", "lead and  trail"),
            ("x", "x x x"),
        ];
        let squeeze = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
        for (old, new) in pairs {
            let parts = word_diff(old, new);
            let old_back = joined(&parts, [DiffKind::Same, DiffKind::Removed]);
            let new_back = joined(&parts, [DiffKind::Same, DiffKind::Added]);
            assert_eq!(squeeze(&old_back), squeeze(old), "{old:?} -> {new:?}");
            assert_eq!(squeeze(&new_back), squeeze(new), "{old:?} -> {new:?}");
        }
    }

    #[test]
    fn a_huge_difference_falls_back_to_one_removed_and_one_added_run() {
        let words = |p: &str| {
            (0..2500)
                .map(|i| format!("{p}{i}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let parts = word_diff(&words("a"), &words("b"));
        assert_eq!(
            parts.iter().map(|p| p.kind).collect::<Vec<_>>(),
            [DiffKind::Removed, DiffKind::Added]
        );
    }

    #[test]
    fn the_kept_version_swaps_with_the_current_one_and_back() {
        assert!(meta(Status::Done).swapped("text").is_none(), "nothing kept");
        let now = Meta {
            previous: Some(Previous {
                text: "old text".into(),
                model: "turbo".into(),
                language: "en".into(),
                settings: None,
                newer: false,
            }),
            ..meta(Status::Done)
        };
        let (back, text) = now.swapped("new text").unwrap();
        assert_eq!(text, "old text");
        assert_eq!(
            (back.model.as_str(), back.language.as_str()),
            ("turbo", "en")
        );
        assert!(back.settings.is_none());
        let kept = back.previous.as_ref().unwrap();
        assert_eq!(
            (kept.text.as_str(), kept.model.as_str()),
            ("new text", "small")
        );
        assert!(kept.newer, "the old text is shown, the kept one is newer");
        let (again, text) = back.swapped(&text).unwrap();
        assert_eq!(text, "new text");
        assert_eq!(again.model, "small");
        let kept = again.previous.unwrap();
        assert_eq!(kept.text, "old text");
        assert!(!kept.newer, "the new text is shown again");
    }

    #[test]
    fn the_kept_version_is_written_only_when_there_is_one_and_old_files_still_read() {
        let json = meta(Status::Done).to_json().unwrap();
        assert!(!json.contains("previous"), "{json}");
        assert_eq!(Meta::from_json(&json).unwrap().previous, None);
        let with = Meta {
            previous: Some(meta(Status::Done).as_previous("before")),
            ..meta(Status::Done)
        };
        assert_eq!(Meta::from_json(&with.to_json().unwrap()).unwrap(), with);
    }

    fn timed(words: &[(&str, Option<(u32, u32)>)]) -> Vec<Word> {
        words
            .iter()
            .map(|&(w, t)| Word {
                w: w.into(),
                p: 0.9,
                t0: t.map(|t| t.0),
                t1: t.map(|t| t.1),
            })
            .collect()
    }

    fn spans(v: &[Option<(Millis, Millis)>]) -> Vec<Option<(u64, u64)>> {
        v.iter().map(|t| t.map(|(a, b)| (a.0, b.0))).collect()
    }

    #[test]
    fn an_old_record_has_no_words_and_a_new_one_round_trips() {
        // A record from before the field: no `words`, no `timings`.
        let old = r#"{"version":1,"started":"2026-11-08T13:00:00.123","duration_ms":4500,
            "status":"done","model":"small","language":"ru","settings":null}"#;
        let m = Meta::from_json(old).unwrap();
        assert!(m.words.is_empty() && m.timings.is_none());
        // And it is written back without the fields.
        let json = m.to_json().unwrap();
        assert!(
            !json.contains("words") && !json.contains("timings"),
            "{json}"
        );

        let with = Meta {
            words: timed(&[("привет", Some((120, 480))), ("мир", None)]),
            timings: Some(Timings {
                by: TimingBy::Dtw,
                preset: "large-v3".into(),
                model: "podlodka".into(),
            }),
            ..m
        };
        let json = with.to_json().unwrap();
        assert!(json.contains("\"by\": \"dtw\""), "{json}");
        // A word with no time writes no `t0`/`t1`.
        assert_eq!(json.matches("\"t0\"").count(), 1, "{json}");
        assert_eq!(Meta::from_json(&json).unwrap(), with);
    }

    #[test]
    fn low_confidence_is_under_the_constant() {
        let w = |p| Word {
            w: "x".into(),
            p,
            t0: None,
            t1: None,
        };
        assert!(w(0.44).is_doubtful() && !w(0.45).is_doubtful());
    }

    #[test]
    fn align_identical_lists_take_the_times_as_they_are() {
        let t = timed(&[("Привет,", Some((0, 400))), ("мир.", Some((500, 900)))]);
        let got = align(&["привет", "МИР"], &t);
        assert_eq!(spans(&got), [Some((0, 400)), Some((500, 900))]);
    }

    #[test]
    fn align_a_replaced_word_gets_the_gap_between_its_neighbours() {
        let t = timed(&[
            ("one", Some((0, 100))),
            ("two", Some((200, 300))),
            ("three", Some((400, 500))),
        ]);
        let got = align(&["one", "2", "three"], &t);
        assert_eq!(
            spans(&got),
            [Some((0, 100)), Some((100, 400)), Some((400, 500))]
        );
    }

    #[test]
    fn align_a_deleted_word_just_leaves_the_others_in_place() {
        let t = timed(&[
            ("a", Some((0, 100))),
            ("b", Some((100, 200))),
            ("c", Some((200, 300))),
        ]);
        let got = align(&["a", "c"], &t);
        assert_eq!(spans(&got), [Some((0, 100)), Some((200, 300))]);
    }

    #[test]
    fn align_an_inserted_sentence_is_shared_out_by_letters() {
        let t = timed(&[("start", Some((0, 100))), ("end", Some((900, 1000)))]);
        // "ab" has 2 letters and "cdef" 4: the gap 100..900 splits 1 : 2.
        let got = align(&["start", "ab", "cdef", "end"], &t);
        let got = spans(&got);
        assert_eq!(got[0], Some((0, 100)));
        assert_eq!(got[3], Some((900, 1000)));
        assert_eq!(got[1], Some((100, 366)));
        assert_eq!(got[2], Some((366, 900)));
    }

    #[test]
    fn align_words_outside_the_timed_ones_use_the_ends_of_the_pass() {
        let t = timed(&[("mid", Some((300, 400)))]);
        let got = align(&["before", "mid", "after"], &t);
        assert_eq!(
            spans(&got),
            [Some((300, 300)), Some((300, 400)), Some((400, 400))]
        );
        // No time anywhere: nothing to say.
        let t = timed(&[("mid", None)]);
        assert_eq!(align(&["mid"], &t), [None]);
    }

    fn ms(r: &[(u64, u64)]) -> Vec<std::ops::Range<Millis>> {
        r.iter().map(|&(a, b)| Millis(a)..Millis(b)).collect()
    }

    #[test]
    fn skips_jump_long_pauses_only_and_keep_a_margin_at_voice() {
        // No pause.
        assert_eq!(skips(&ms(&[(0, 5000)]), Millis(5000)), []);
        // A 3 s pause between two runs: skipped from 150 ms after the first to 150 ms before the
        // second.
        assert_eq!(
            skips(&ms(&[(0, 1000), (4000, 5000)]), Millis(5000)),
            ms(&[(1150, 3850)])
        );
        // A 900 ms pause stays.
        assert_eq!(skips(&ms(&[(0, 1000), (1900, 3000)]), Millis(3000)), []);
        // 2 s of silence at the start and at the end.
        assert_eq!(
            skips(&ms(&[(2000, 3000)]), Millis(5000)),
            ms(&[(0, 1850), (3150, 5000)])
        );
        // No voice at all: the record plays as it is.
        assert_eq!(skips(&[], Millis(5000)), []);
    }

    proptest::proptest! {
        #[test]
        fn skips_are_ordered_apart_inside_and_never_on_voice(
            cuts in proptest::collection::vec(0_u64..20_000, 0..12),
            total in 0_u64..25_000,
        ) {
            let mut cuts = cuts;
            cuts.sort_unstable();
            let voiced: Vec<_> = cuts.as_chunks::<2>().0.iter().map(|[a, b]| Millis(*a)..Millis(*b)).collect();
            let got = skips(&voiced, Millis(total));
            for pair in got.windows(2) {
                proptest::prop_assert!(pair[0].end < pair[1].start);
            }
            for s in &got {
                proptest::prop_assert!(s.start < s.end && s.end.0 <= total);
                for v in &voiced {
                    proptest::prop_assert!(s.end <= v.start || s.start >= v.end || v.start == v.end);
                }
            }
        }
    }

    fn found(query: &str, filter: &Filter, text: &str, day: Date, ms: u64) -> bool {
        matches(
            query,
            filter,
            &Searched {
                text_lower: &fold(text),
                day,
                duration_ms: ms,
            },
        )
    }

    #[test]
    fn search_words_are_parts_of_words_in_any_order_ignoring_case_and_yo() {
        let (d, none) = (day(2026, 11, 8), Filter::default());
        assert!(found("", &none, "Привет мир", d, 0), "no words match all");
        assert!(found("  ", &none, "Привет мир", d, 0));
        assert!(found("прив", &none, "Привет мир", d, 0));
        assert!(found("мир привет", &none, "Привет мир", d, 0));
        assert!(found("ПРИВЕТ", &none, "привет", d, 0));
        assert!(found("еж", &none, "Ёж", d, 0));
        assert!(found("ёж", &none, "еж", d, 0));
        assert!(!found("привет пока", &none, "Привет мир", d, 0));
    }

    #[test]
    fn search_filters_are_inclusive_and_anded_with_the_query() {
        let d = day(2026, 11, 8);
        let f = Filter {
            day: Some(d),
            min_ms: Some(1000),
            max_ms: Some(2000),
        };
        assert!(found("a", &f, "a", d, 1000) && found("a", &f, "a", d, 2000));
        assert!(!found("a", &f, "a", d, 999) && !found("a", &f, "a", d, 2001));
        assert!(!found("a", &f, "a", day(2026, 11, 9), 1500), "other day");
        assert!(
            !found("b", &f, "a", d, 1500),
            "filters pass, query does not"
        );
        assert!(found("", &f, "", d, 1500), "no query, filters pass");
    }

    proptest::proptest! {
        #[test]
        fn a_longer_query_never_adds_a_match(q in "[a-zа-я ]{0,12}", extra in "[a-zа-я]{1,4}", text in "[a-zа-я ]{0,30}") {
            let (d, none) = (day(2026, 11, 8), Filter::default());
            let longer = format!("{q} {extra}");
            proptest::prop_assert!(!found(&longer, &none, &text, d, 0) || found(&q, &none, &text, d, 0));
        }
    }

    fn spoken(words: &[(&str, u32, u32)]) -> Vec<Word> {
        words
            .iter()
            .map(|&(w, a, b)| Word {
                w: w.into(),
                p: 1.0,
                t0: Some(a),
                t1: Some(b),
            })
            .collect()
    }

    #[test]
    fn srt_and_vtt_break_at_pauses_and_sentence_ends_with_their_own_time_marks() {
        let words = spoken(&[
            ("Hello", 0, 400),
            (" there.", 450, 900),
            ("Next", 950, 1_500),
            ("one", 2_000, 3_723_004),
        ]);
        assert_eq!(
            export::srt(&words),
            "1\n00:00:00,000 --> 00:00:00,900\nHello there.\n\n\
             2\n00:00:00,950 --> 00:00:01,500\nNext\n\n\
             3\n00:00:02,000 --> 01:02:03,004\none\n\n"
        );
        let vtt = export::vtt(&words);
        assert!(
            vtt.starts_with("WEBVTT\n\n00:00:00.000 --> 00:00:00.900\n"),
            "{vtt}"
        );
        assert!(!vtt.contains("\n1\n"), "VTT cues are not numbered");
    }

    #[test]
    fn cues_break_at_84_characters_and_at_5_seconds() {
        let long = "x".repeat(40);
        let words = spoken(&[(&long, 0, 100), (&long, 100, 200), (&long, 200, 300)]);
        assert_eq!(
            export::srt(&words).matches("-->").count(),
            2,
            "84 characters"
        );
        let slow = spoken(&[("a", 0, 100), ("b", 4_900, 5_200)]);
        assert_eq!(export::srt(&slow).matches("-->").count(), 2, "5 seconds");
    }

    #[test]
    fn export_without_times_is_empty_and_json_carries_the_text() {
        let untimed = vec![Word {
            w: "a".into(),
            p: 1.0,
            t0: None,
            t1: None,
        }];
        assert_eq!(export::srt(&untimed), "");
        assert_eq!(export::srt(&[]), "");
        assert_eq!(export::vtt(&[]), "WEBVTT\n\n");
        assert_eq!(export::txt("a b"), "a b");
        let json = export::json(&meta(Status::Done), "hello").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["text"], "hello");
        assert_eq!(value["status"], "done");
    }

    fn at(spans: &[(u64, u64)]) -> Vec<std::ops::Range<Millis>> {
        spans.iter().map(|&(a, b)| Millis(a)..Millis(b)).collect()
    }

    #[test]
    fn chunks_cut_in_the_longest_pause_near_the_border() {
        let max = Millis(300_000);
        // Short audio is one chunk, with or without voice.
        assert_eq!(chunks(&[], Millis(10_000), max), at(&[(0, 10_000)]));
        assert_eq!(chunks(&[], Millis(0), max), at(&[(0, 0)]));
        // Pauses of 1 s at 280 s and 2 s at 290 s, both in the last 30 s: the longer one wins.
        let voiced = at(&[(0, 280_000), (281_000, 290_000), (292_000, 400_000)]);
        assert_eq!(
            chunks(&voiced, Millis(400_000), max),
            at(&[(0, 291_000), (291_000, 400_000)])
        );
        // No pause: a hard cut at the maximum.
        assert_eq!(
            chunks(&at(&[(0, 700_000)]), Millis(700_000), max),
            at(&[(0, 300_000), (300_000, 600_000), (600_000, 700_000)])
        );
        // A pause before the last 30 s is not used.
        assert_eq!(
            chunks(
                &at(&[(0, 100_000), (150_000, 400_000)]),
                Millis(400_000),
                max
            ),
            at(&[(0, 300_000), (300_000, 400_000)])
        );
    }

    proptest::proptest! {
        #[test]
        fn chunks_cover_the_audio_once_in_order_and_none_is_over_the_maximum(
            cuts in proptest::collection::vec(0_u64..900_000, 0..16),
            total in 0_u64..900_000,
            max in 1_000_u64..400_000,
        ) {
            let mut cuts = cuts;
            cuts.sort_unstable();
            let voiced: Vec<_> = cuts.as_chunks::<2>().0.iter().map(|[a, b]| Millis(*a)..Millis(*b)).collect();
            let got = chunks(&voiced, Millis(total), Millis(max));
            proptest::prop_assert_eq!(got.first().map(|c| c.start.0), Some(0));
            proptest::prop_assert_eq!(got.last().map(|c| c.end.0), Some(total));
            for pair in got.windows(2) {
                proptest::prop_assert_eq!(pair[0].end, pair[1].start);
            }
            for c in &got {
                proptest::prop_assert!(c.end.0 - c.start.0 <= max);
                proptest::prop_assert!(c.end.0 > c.start.0 || total == 0);
            }
        }
    }
}
