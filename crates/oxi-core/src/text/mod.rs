//! Text pipeline: newline strip, trim, hallucination filter, confidence filter, pause
//! punctuation, replace rules, filler filter, voice commands, snippets, trailing space
//! (ARCHITECTURE sections 10 and 11, items 30-34; arch-plan 9.1). Pure, no OS calls.
//!
//! Every optional stage is off by default: with the default [`TextOptions`] and no fillers or
//! snippets, [`run`] gives what [`process`] always gave.

mod decode;
mod hallucination;
mod replace;
mod snippets;
mod stages;
#[cfg(test)]
mod tests;

pub use decode::{DecodeError, decode_text_file};
pub use hallucination::HallucinationFilter;
pub use replace::{ParsedRules, ReplaceRules};
pub use snippets::{fillers_from_lines, fillers_to_lines, snippets_from_lines, snippets_to_lines};

use crate::config::{Snippet, Text};
use crate::types::Millis;

/// Result of the text pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Text ready to paste.
    Text(String),
    /// Nothing left after filtering: paste nothing, show "nothing heard".
    NothingHeard,
}

/// A window whisper doubts speech in (its `no_speech_prob` at or above this) lowers the bar a
/// segment's own confidence must clear, to [`NO_SPEECH_MAX_P`].
pub const NO_SPEECH_HOLD: f32 = 0.6;
/// See [`NO_SPEECH_HOLD`].
pub const NO_SPEECH_MAX_P: f32 = 0.6;

/// A clip up to this long is checked for a text no voice could say in it.
pub const RATE_CLIP_MS: u64 = 1500;
/// Characters per second above which a short clip's text is taken for an invention. Measured
/// dictation tops out near 25.
pub const MAX_CHARS_PER_SEC: u64 = 40;
/// Characters any short clip may carry, so one short word of a quick tap is never dropped.
pub const MIN_CHARS_ALLOWED: usize = 12;

/// One word of a segment: `p` is its lowest token probability, the times exist only when word
/// times were made.
#[derive(Debug, Clone, Copy)]
pub struct WordIn<'a> {
    pub text: &'a str,
    pub p: f32,
    pub t0: Option<Millis>,
    pub t1: Option<Millis>,
}

/// A whisper segment. `no_speech_prob` belongs to the 30 s window the segment came from: every
/// segment of one window carries the same value.
#[derive(Debug, Clone, Copy)]
pub struct Seg<'a> {
    pub text: &'a str,
    pub t0: Millis,
    pub t1: Millis,
    pub no_speech_prob: f32,
    pub words: &'a [WordIn<'a>],
}

/// The `[text]` switches and the facts about this dictation.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools, reason = "one switch per config key")]
pub struct TextOptions {
    pub pause_punctuation: bool,
    pub pause_sentence_ms: u32,
    pub pause_paragraph_ms: u32,
    pub filler_filter: bool,
    pub voice_commands: bool,
    pub confidence_filter: bool,
    pub confidence_min: f32,
    pub trailing_space: bool,
    /// False when the text is typed: a line break would send a message, so it becomes a space.
    pub newlines_allowed: bool,
    /// A history entry will be saved, so a held segment has a place to be kept. When false the
    /// confidence filter holds nothing.
    pub can_hold: bool,
    /// The hallucination list and the short-clip check hold what they take out even without
    /// `can_hold`; the caller drops it when no entry will keep it (the daemon decides at delivery,
    /// with the newest config: review d03-fix #3).
    pub hold_invented: bool,
    /// Length of the audio whisper got, for the short-clip check; `None` skips the check.
    pub clip_ms: Option<u64>,
    /// The user asked for this clip again (Transcribe again): the hallucination list and the
    /// short-clip check take nothing out.
    pub again: bool,
}

impl Default for TextOptions {
    /// Every stage off, a newline allowed, no trailing space.
    fn default() -> Self {
        Self::from_config(&Text::default(), false)
    }
}

impl TextOptions {
    pub fn from_config(text: &Text, trailing_space: bool) -> Self {
        Self {
            pause_punctuation: text.pause_punctuation,
            pause_sentence_ms: text.pause_sentence_ms,
            pause_paragraph_ms: text.pause_paragraph_ms,
            filler_filter: text.filler_filter,
            voice_commands: text.voice_commands,
            confidence_filter: text.confidence_filter,
            confidence_min: text.confidence_min,
            trailing_space,
            newlines_allowed: true,
            can_hold: false,
            hold_invented: false,
            clip_ms: None,
            again: false,
        }
    }
}

/// The lists the stages read.
#[derive(Debug, Clone, Copy)]
pub struct Lists<'a> {
    pub hallucinations: &'a HallucinationFilter,
    pub replace: &'a ReplaceRules,
    pub fillers: &'a [String],
    pub snippets: &'a [Snippet],
}

/// Why a segment was held back (`why` of a held part in the history meta file).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeldWhy {
    LowConfidence,
    NoSpeech,
    /// The hallucination list or the short-clip check took it for an invention.
    Hallucination,
    /// Also any value a newer version writes, so its entry keeps the rest of its meta.
    #[serde(other)]
    Unknown,
}

/// A segment the confidence filter or the hallucination checks held back: its text is kept,
/// never dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub text: String,
    pub t0: Millis,
    pub t1: Millis,
    pub why: HeldWhy,
}

/// What [`run`] made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Processed {
    pub outcome: Outcome,
    /// What whisper heard: the segments the hallucination list let through, joined with a space
    /// (the short-clip check does not take them out of here).
    pub raw: String,
    pub held: Vec<Held>,
    /// Segments the hallucination list dropped (a count, never the text). With `can_hold` they
    /// are held instead and not counted here.
    pub dropped_hallucination: usize,
    /// Segments dropped because a short clip carried more text than a voice could say in it
    /// (held instead with `can_hold`, and not counted here).
    pub dropped_implausible: usize,
}

fn clean(s: &str) -> String {
    s.replace(['\n', '\r'], "").trim().to_owned()
}

/// Mean word confidence of a segment; `None` with no word data.
fn mean_p(seg: &Seg<'_>) -> Option<f32> {
    #[allow(
        clippy::cast_precision_loss,
        reason = "a segment has a handful of words"
    )]
    let n = seg.words.len() as f32;
    (!seg.words.is_empty()).then(|| seg.words.iter().map(|w| w.p).sum::<f32>() / n)
}

/// Step 3: is the segment held back? A segment with no word data has no evidence and is kept.
fn hold(seg: &Seg<'_>, min: f32) -> Option<HeldWhy> {
    let p = mean_p(seg)?;
    if p < min {
        Some(HeldWhy::LowConfidence)
    } else if seg.no_speech_prob >= NO_SPEECH_HOLD && p < NO_SPEECH_MAX_P {
        Some(HeldWhy::NoSpeech)
    } else {
        None
    }
}

/// More characters than a voice could say in a short clip.
fn implausible(clip_ms: u64, chars: usize) -> bool {
    let allowed = usize::try_from(clip_ms * MAX_CHARS_PER_SEC / 1000).unwrap_or(usize::MAX);
    clip_ms <= RATE_CLIP_MS && chars > allowed.max(MIN_CHARS_ALLOWED)
}

/// Each line break as a space (none after a space).
pub(crate) fn flatten(text: &str) -> String {
    let mut flat = String::with_capacity(text.len());
    for c in text.chars() {
        if c != '\n' {
            flat.push(c);
        } else if !flat.ends_with(' ') {
            flat.push(' ');
        }
    }
    flat
}

/// Run the pipeline over whisper segments (arch-plan 9.1 order).
pub fn run(segments: &[Seg<'_>], opts: &TextOptions, lists: &Lists<'_>) -> Processed {
    // 1 and 2: strip newlines (no space added, whisper wraps mid-word), trim, take out the
    // listed. Taken out is held when an entry will keep it, else dropped (counted).
    let mut dropped_hallucination = 0;
    let mut held = Vec::new();
    let invented = |text: String, seg: &Seg<'_>| Held {
        text,
        t0: seg.t0,
        t1: seg.t1,
        why: HeldWhy::Hallucination,
    };
    let mut passed: Vec<(String, &Seg<'_>)> = Vec::new();
    for seg in segments {
        let text = clean(seg.text);
        if text.is_empty() {
            continue;
        }
        if opts.again || !lists.hallucinations.is_hallucination(&text) {
            passed.push((text, seg));
        } else if opts.can_hold || opts.hold_invented {
            held.push(invented(text, seg));
        } else {
            dropped_hallucination += 1;
        }
    }
    let raw = passed
        .iter()
        .map(|(t, _)| t.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    // A voice-like blip of ~100 ms goes to whisper whole and may come back as an invented phrase.
    let mut dropped_implausible = 0;
    if let Some(ms) = opts.clip_ms.filter(|_| !opts.again) {
        let chars: usize = passed.iter().map(|(t, _)| t.chars().count()).sum();
        if implausible(ms, chars) {
            if opts.can_hold || opts.hold_invented {
                held.extend(passed.drain(..).map(|(t, seg)| invented(t, seg)));
            } else {
                dropped_implausible = passed.len();
                passed.clear();
            }
        }
    }

    // 3: hold the doubtful segments back; they are kept for the history entry.
    if opts.confidence_filter && opts.can_hold {
        passed.retain(|(text, seg)| {
            let Some(why) = hold(seg, opts.confidence_min) else {
                return true;
            };
            held.push(Held {
                text: text.clone(),
                t0: seg.t0,
                t1: seg.t1,
                why,
            });
            false
        });
        // Steps 2 and 3 both hold: the entry lists them in the order they were said.
        held.sort_by_key(|h| h.t0);
    }
    let result = |outcome| Processed {
        outcome,
        raw: raw.clone(),
        held: held.clone(),
        dropped_hallucination,
        dropped_implausible,
    };

    // 4: join.
    let joined = if opts.pause_punctuation {
        stages::join_with_pauses(&passed, opts.pause_sentence_ms, opts.pause_paragraph_ms)
    } else {
        passed
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    };
    // 5: replace rules, before commands and snippets so a rule can fix a misheard one.
    let mut text = lists.replace.apply(&joined);
    // 6: fillers.
    if opts.filler_filter {
        text = stages::drop_fillers(&text, lists.fillers);
    }
    // 7: voice commands.
    let mut tail = 0;
    if opts.voice_commands {
        (text, tail) = stages::voice_commands(&text);
    }
    // 8: a snippet when it is the whole text.
    if let Some(block) = stages::snippet(&text, lists.snippets) {
        text = block.to_owned();
    }
    // 9: trim (a rule may delete a whole word and leave the edge blank), then the line breaks
    // the commands left at the end, then the trailing space.
    text.retain(|c| c != '\r');
    let mut text = text.trim().to_owned();
    if !opts.newlines_allowed {
        text = flatten(&text);
        tail = 0;
    }
    if text.is_empty() {
        return result(Outcome::NothingHeard);
    }
    if tail > 0 {
        text.push_str(&"\n".repeat(tail));
    } else if opts.trailing_space {
        text.push(' ');
    }
    result(Outcome::Text(text))
}

/// Run the pipeline over plain segment texts with every optional stage off: strip newlines, trim,
/// drop hallucinated segments, join, apply `rules`, add the trailing space.
pub fn process<S: AsRef<str>>(
    segments: &[S],
    filter: &HallucinationFilter,
    rules: &ReplaceRules,
    trailing_space: bool,
) -> Outcome {
    let segs: Vec<Seg<'_>> = segments
        .iter()
        .map(|s| Seg {
            text: s.as_ref(),
            t0: Millis(0),
            t1: Millis(0),
            no_speech_prob: 0.0,
            words: &[],
        })
        .collect();
    let opts = TextOptions {
        trailing_space,
        ..TextOptions::default()
    };
    let lists = Lists {
        hallucinations: filter,
        replace: rules,
        fillers: &[],
        snippets: &[],
    };
    run(&segs, &opts, &lists).outcome
}
