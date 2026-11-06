//! Text pipeline: newline strip, trim, hallucination filter, replace rules, trailing space
//! (ARCHITECTURE sections 10 and 11, items 30-34). Pure, no OS calls.

mod decode;
mod hallucination;
mod replace;

pub use decode::{DecodeError, decode_text_file};
pub use hallucination::HallucinationFilter;
pub use replace::{ParsedRules, ReplaceRules};

/// Result of the text pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Text ready to paste.
    Text(String),
    /// Nothing left after filtering: paste nothing, show "nothing heard".
    NothingHeard,
}

/// Run the pipeline over whisper segments: strip newlines (no space added, whisper wraps
/// mid-word), trim, drop hallucinated segments, join, apply `rules`, add the trailing space.
pub fn process<S: AsRef<str>>(
    segments: &[S],
    filter: &HallucinationFilter,
    rules: &ReplaceRules,
    trailing_space: bool,
) -> Outcome {
    let kept: Vec<String> = segments
        .iter()
        .map(|s| s.as_ref().replace(['\n', '\r'], ""))
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty() && !filter.is_hallucination(s))
        .collect();
    // Trim again: a rule may delete a whole word and leave the edge blank.
    let text = rules.apply(&kept.join(" "));
    let text = text.trim();
    if text.is_empty() {
        return Outcome::NothingHeard;
    }
    Outcome::Text(if trailing_space {
        format!("{text} ")
    } else {
        text.to_owned()
    })
}
