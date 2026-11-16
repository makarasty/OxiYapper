//! Text suggestions from the person's own history (hold item 41, candidates 6-9): words the model
//! doubted, editing habits, a phrase said again and again, filler words deleted by hand, and the
//! certainty the held-back parts say. Only suggestions: the settings page shows them and changes
//! nothing until the person presses Apply or Add. Pure counting over texts handed in by the
//! caller, in memory; nothing here is stored or logged (AGENTS rule 5). Thresholds are starting
//! points, not measured.

use std::collections::{HashMap, HashSet};

use crate::config::Snippet;
use crate::history::{HeldPart, Meta, Previous, Word};
use crate::text::HeldWhy;

/// The newest this many entries with a text are read.
pub const RECENT: usize = 200;
/// A doubted word is suggested once it was doubted in this many dictations.
pub const MIN_DOUBTED: usize = 3;
/// A kind of edit is suggested once it was made this many times ...
pub const MIN_HABIT: usize = 5;
/// ... and in at least this share (percent) of the edits.
pub const HABIT_SHARE: usize = 70;
/// A phrase is suggested once it was dictated in this many dictations.
pub const MIN_PHRASE: usize = 4;
/// A word is a filler candidate once it was deleted by hand in this many edits.
pub const MIN_FILLER: usize = 3;
/// No certainty suggestion from fewer held-back parts than this.
pub const MIN_HELD: usize = 10;
/// The longest list of doubted words shown.
const MAX_DOUBTED: usize = 30;
/// Phrases are 3 to 8 words long.
const PHRASE_WORDS: std::ops::RangeInclusive<usize> = 3..=8;
/// Only the first words of a long dictation are searched for phrases (keeps the count small).
const PHRASE_SCAN: usize = 120;
/// A filler is a short word.
const FILLER_CHARS: usize = 6;

/// What one history entry gives the analysis, read in memory only.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    /// The `.txt` now.
    pub text: String,
    /// The text before the person edited it; `None` when it was never edited by hand.
    pub dictated: Option<String>,
    pub words: Vec<Word>,
    pub held: Vec<HeldPart>,
    /// All of the dictation was held back: the `.txt` has the held parts too.
    pub filtered: bool,
}

impl Sample {
    /// The sample of an entry with meta `meta` and text `text`.
    #[must_use]
    pub fn new(meta: Meta, text: String) -> Self {
        let dictated = edited_from(&meta)
            .filter(|d| d.trim() != text.trim())
            .map(str::to_owned);
        Self {
            dictated,
            filtered: meta.delivered == Some(crate::history::Delivered::Filtered),
            words: meta.words,
            held: meta.held,
            text,
        }
    }
}

/// The text the person edited into the current one: the kept version made by the same run
/// (model, language, settings) as the text now. shortcut: a Transcribe again with the same model
/// and settings reads as an edit; whisper gives the same text then, and an unchanged text is not
/// written, so it is rare.
fn edited_from(meta: &Meta) -> Option<&str> {
    let same = |p: &&Previous| {
        p.model == meta.model && p.language == meta.language && p.settings == meta.settings
    };
    meta.original
        .as_ref()
        .filter(same)
        .or(meta.previous.as_ref().filter(|p| !p.newer).filter(same))
        .map(|p| p.text.as_str())
}

/// A word in lower case without the punctuation around it; empty when nothing is left.
fn key(word: &str) -> String {
    word.trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

/// The words of a text as keys, punctuation-only tokens dropped.
fn keys(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(key)
        .filter(|k| !k.is_empty())
        .collect()
}

/// A word made of letters (and inner `-` or `'`).
fn wordlike(w: &str) -> bool {
    w.chars().any(char::is_alphabetic)
        && w.chars()
            .all(|c| c.is_alphabetic() || c == '-' || c == '\'')
}

/// How many times something showed, and out of how many.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Count {
    pub hits: usize,
    pub of: usize,
}

/// A word the model doubted in `count` dictations, as it was written the last time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doubted {
    pub word: String,
    pub count: usize,
}

/// Words the model doubted (`p` under the doubt line) in at least [`MIN_DOUBTED`] of the newest
/// [`RECENT`] samples (newest first), most often first; `known` (lower case: the dictionary's and
/// the replacements' words) are left out, and so are words under 3 letters.
#[must_use]
pub fn doubted_words<H: std::hash::BuildHasher>(
    samples: &[Sample],
    known: &HashSet<String, H>,
) -> Vec<Doubted> {
    let mut seen: HashMap<String, (usize, String)> = HashMap::new();
    for s in samples.iter().take(RECENT) {
        let mut once = HashSet::new();
        for w in s.words.iter().filter(|w| w.is_doubtful()) {
            let shown = w.w.trim_matches(|c: char| !c.is_alphanumeric());
            let k = shown.to_lowercase();
            if shown.chars().count() < 3 || !wordlike(shown) || known.contains(&k) {
                continue;
            }
            if once.insert(k.clone()) {
                // Newest first: the first form seen is the latest.
                seen.entry(k).or_insert((0, shown.to_owned())).0 += 1;
            }
        }
    }
    let mut out: Vec<Doubted> = seen
        .into_values()
        .filter(|(n, _)| *n >= MIN_DOUBTED)
        .map(|(count, word)| Doubted { word, count })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.word.cmp(&b.word)));
    out.truncate(MAX_DOUBTED);
    out
}

/// What the person's edits suggest turning on; `None`: not enough of that kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Habits {
    pub no_final_period: Option<Count>,
    pub small_first_letter: Option<Count>,
}

fn ends_with_period(s: &str) -> bool {
    let s = s.trim_end();
    s.ends_with('.') && !s.ends_with("..")
}

/// The person took the final full stop away.
fn removed_period(from: &str, to: &str) -> bool {
    let to = to.trim_end();
    ends_with_period(from) && !to.is_empty() && !to.ends_with(['.', '!', '?', '\u{2026}'])
}

/// The person turned the capital first letter into a small one.
fn lowered_first(from: &str, to: &str) -> bool {
    let first = |s: &str| s.chars().find(|c| c.is_alphabetic());
    match (first(from), first(to)) {
        (Some(a), Some(b)) => {
            a.is_uppercase() && b.is_lowercase() && a.to_lowercase().eq(b.to_lowercase())
        }
        _ => false,
    }
}

/// The kinds of edit made in at least [`MIN_HABIT`] edits and [`HABIT_SHARE`] % of all edits of
/// the newest [`RECENT`] samples.
#[must_use]
pub fn habits(samples: &[Sample]) -> Habits {
    let edits: Vec<(&str, &str)> = samples
        .iter()
        .take(RECENT)
        .filter_map(|s| Some((s.dictated.as_deref()?, s.text.as_str())))
        .collect();
    let of = edits.len();
    let kind = |f: fn(&str, &str) -> bool| {
        let hits = edits.iter().filter(|(a, b)| f(a, b)).count();
        (hits >= MIN_HABIT && hits * 100 >= of * HABIT_SHARE).then_some(Count { hits, of })
    };
    Habits {
        no_final_period: kind(removed_period),
        small_first_letter: kind(lowered_first),
    }
}

/// Short words deleted by hand in at least [`MIN_FILLER`] edits, not in `fillers` (lower case):
/// at most three, most often first; `hits` is how many edits deleted one of them.
#[must_use]
pub fn deleted_fillers(samples: &[Sample], fillers: &[String]) -> Option<(Vec<String>, Count)> {
    let mut per_edit: Vec<HashSet<String>> = Vec::new();
    for s in samples.iter().take(RECENT) {
        let Some(from) = s.dictated.as_deref() else {
            continue;
        };
        let mut left: HashMap<String, i32> = HashMap::new();
        for k in keys(from) {
            *left.entry(k).or_default() += 1;
        }
        for k in keys(&s.text) {
            *left.entry(k).or_default() -= 1;
        }
        per_edit.push(
            left.into_iter()
                .filter(|(k, n)| {
                    *n > 0
                        && k.chars().count() <= FILLER_CHARS
                        && wordlike(k)
                        && !fillers.contains(k)
                })
                .map(|(k, _)| k)
                .collect(),
        );
    }
    let mut count: HashMap<&str, usize> = HashMap::new();
    for k in per_edit.iter().flatten() {
        *count.entry(k).or_default() += 1;
    }
    let mut top: Vec<(&str, usize)> = count
        .into_iter()
        .filter(|(_, n)| *n >= MIN_FILLER)
        .collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    top.truncate(3);
    if top.is_empty() {
        return None;
    }
    let words: Vec<String> = top.iter().map(|(k, _)| (*k).to_owned()).collect();
    let hits = per_edit
        .iter()
        .filter(|deleted| words.iter().any(|w| deleted.contains(w)))
        .count();
    Some((
        words,
        Count {
            hits,
            of: per_edit.len(),
        },
    ))
}

/// A phrase dictated again and again, offered as a phrase shortcut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phrase {
    /// As it was dictated the last time.
    pub text: String,
    /// Its first two words, in lower case: what to say for it.
    pub trigger: String,
    pub count: Count,
}

/// The longest phrase of 3-8 words found in at least [`MIN_PHRASE`] of the newest [`RECENT`]
/// samples (most often among equals) that no snippet in `snippets` already holds.
#[must_use]
pub fn repeated_phrase(samples: &[Sample], snippets: &[Snippet]) -> Option<Phrase> {
    let taken: Vec<String> = snippets
        .iter()
        .flat_map(|s| [keys(&s.text).join(" "), keys(&s.trigger).join(" ")])
        .collect();
    // key -> (dictations, the newest form, words)
    let mut seen: HashMap<String, (usize, String, usize)> = HashMap::new();
    let mut of = 0;
    for s in samples.iter().take(RECENT) {
        let toks: Vec<&str> = s.text.split_whitespace().take(PHRASE_SCAN).collect();
        if toks.is_empty() {
            continue;
        }
        of += 1;
        let ks: Vec<String> = toks.iter().map(|t| key(t)).collect();
        let mut once = HashSet::new();
        for n in PHRASE_WORDS {
            for at in 0..toks.len().saturating_sub(n - 1) {
                let part = &ks[at..at + n];
                if part.iter().any(String::is_empty) {
                    continue;
                }
                let k = part.join(" ");
                if once.insert(k.clone()) {
                    let shown = toks[at..at + n]
                        .join(" ")
                        .trim_end_matches([',', ';', ':'])
                        .to_owned();
                    seen.entry(k).or_insert((0, shown, n)).0 += 1;
                }
            }
        }
    }
    let (k, (hits, text, _)) = seen
        .into_iter()
        .filter(|(k, (n, ..))| *n >= MIN_PHRASE && !taken.iter().any(|t| t.contains(k.as_str())))
        .max_by(|(ka, a), (kb, b)| (a.2, a.0, kb).cmp(&(b.2, b.0, ka)))?;
    let trigger = k.split(' ').take(2).collect::<Vec<_>>().join(" ");
    Some(Phrase {
        text,
        trigger,
        count: Count { hits, of },
    })
}

/// The key a dismissed suggestion ("Not now", "Not this one") is remembered by: the 64-bit FNV-1a
/// hash of `kind` and `what` (lower case), in hex. Stable between runs; keeps no dictated text.
#[must_use]
pub fn dismiss_key(kind: &str, what: &str) -> String {
    let hash = kind
        .bytes()
        .chain([0])
        .chain(what.to_lowercase().bytes())
        .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
    format!("{hash:016x}")
}

/// How many times the run of words `want` is in `hay` (whole words, not parts of words).
fn runs(hay: &[String], want: &[String]) -> usize {
    if want.is_empty() || hay.len() < want.len() {
        return 0;
    }
    hay.windows(want.len()).filter(|w| *w == want).count()
}

/// The mean confidence of the words of the held part `part` in `words`. The part's words may be in
/// the dictation more than once: with times, the run nearest the part's start is the one; without,
/// the held parts were doubted ones, so the `nth` (0-based, among held parts of the same words)
/// least certain run is.
fn part_p(part: &HeldPart, nth: usize, words: &[Word]) -> Option<f32> {
    let want = keys(&part.text);
    let have: Vec<String> = words.iter().map(|w| key(&w.w)).collect();
    if want.is_empty() || have.len() < want.len() {
        return None;
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "a held part has a handful of words"
    )]
    let n = want.len() as f32;
    let mut found: Vec<(usize, f32)> = (0..=have.len() - want.len())
        .filter(|&i| have[i..i + want.len()] == want[..])
        .map(|i| {
            (
                i,
                words[i..i + want.len()].iter().map(|w| w.p).sum::<f32>() / n,
            )
        })
        .collect();
    if found.iter().all(|&(i, _)| words[i].t0.is_some()) {
        found.sort_by_key(|&(i, _)| words[i].t0.unwrap_or(0).abs_diff(part.t0));
        return found.first().map(|f| f.1);
    }
    found.sort_by(|a, b| a.1.total_cmp(&b.1));
    found.get(nth).or(found.last()).map(|f| f.1)
}

/// A lower "Certainty needed" when the person added at least 30 % of the parts the confidence
/// filter held back (in the newest [`RECENT`] samples, at least [`MIN_HELD`] parts) back into the
/// text: the median certainty of those added parts, down to a 5 % step, so half of them would
/// have passed. `now` is the setting now (0.05..=0.95); `None` when the parts say nothing lower.
/// A part's certainty is its own, not the setting's at the time, so Apply never steps again.
#[must_use]
pub fn certainty(samples: &[Sample], now: f32) -> Option<(f32, Count)> {
    let mut of = 0;
    let mut added = Vec::new();
    for s in samples.iter().take(RECENT).filter(|s| !s.filtered) {
        let after = keys(&s.text);
        let before = s.dictated.as_deref().map(keys);
        let mut seen: HashMap<String, usize> = HashMap::new();
        for h in s.held.iter().filter(|h| h.why == HeldWhy::LowConfidence) {
            let part = keys(&h.text);
            if part.is_empty() {
                continue;
            }
            of += 1;
            let nth = seen.entry(part.join(" ")).or_default();
            // Added back = the edit put the words in: more runs of them than before the edit.
            let restored = before
                .as_ref()
                .map_or(0, |b| runs(&after, &part).saturating_sub(runs(b, &part)));
            if *nth < restored {
                added.push(part_p(h, *nth, &s.words));
            }
            *nth += 1;
        }
    }
    let hits = added.len();
    if of < MIN_HELD || hits * 10 < of * 3 {
        return None;
    }
    let mut ps: Vec<f32> = added.into_iter().flatten().collect();
    if ps.is_empty() {
        return None;
    }
    ps.sort_by(f32::total_cmp);
    let median = ps[ps.len() / 2];
    let value = ((median * 20.0).floor() / 20.0).clamp(0.05, 0.95);
    (value < now - 0.004).then_some((value, Count { hits, of }))
}

#[cfg(test)]
mod tests;
