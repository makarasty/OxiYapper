//! Dictionary: whisper.cpp's prompt tokenizer and the 223-token prompt budget
//! (ARCHITECTURE sections 7, 11 item 29 and 13).
//!
//! `data/vocab/multilingual.vocab` is openai/whisper's `multilingual.tiktoken`
//! (MIT licence, Copyright (c) 2022 `OpenAI`, full text in `data/vocab/LICENSE`)
//! in a compact form: each line's token base64-decoded, the empty token dropped,
//! the byte strings sorted byte-wise, each written as one length byte followed by
//! its bytes (50256 tokens, longest 33 bytes, 357313 bytes). whisper.cpp loads the
//! same tokens from the model file; `oxi-whisper/tests/tokenizer_regression.rs`
//! checks the file against `ggml-tiny.bin`.
//!
//! The algorithm is `tokenize` in whisper.cpp's `src/whisper.cpp`: split the bytes
//! with a regex, then take the longest vocab entry at each position. Its regex uses
//! C-locale `[[:alpha:]]`, `[[:digit:]]` and `\s`, which see ASCII only. Every byte
//! of a Cyrillic letter is therefore "other", and Cyrillic runs are split and counted
//! the same way whisper.cpp does it, not the way a Unicode-aware tokenizer would.

use std::sync::OnceLock;

mod suggest;

pub use suggest::{Suggestion, suggest};

use crate::constants::VOCAB_FILE;

/// Prompt tokens whisper.cpp keeps: half of the 448-token text context, less one
/// for the PREV marker it puts first. Vendored whisper.cpp (whisper-rs-sys 0.15,
/// `src/whisper.cpp`) keeps `max_prompt_ctx - n_take0 - 1` tokens of the prompt
/// (line 7106; `max_prompt_ctx - 1` in carry mode, line 6927) and drops the rest
/// from the front.
pub const BUDGET: usize = 223;

const CONTRACTIONS: [&[u8]; 7] = [b"'s", b"'t", b"'re", b"'ve", b"'m", b"'ll", b"'d"];

struct Vocab {
    /// Sorted token byte strings.
    tokens: Vec<&'static [u8]>,
    /// Length of the longest token: no match can end further than this from its start.
    max_len: usize,
}

/// The vocab, parsed once from [`VOCAB_FILE`].
fn vocab() -> &'static Vocab {
    static VOCAB: OnceLock<Vocab> = OnceLock::new();
    VOCAB.get_or_init(|| {
        let mut tokens = Vec::with_capacity(51_000);
        let mut rest = VOCAB_FILE;
        while let Some((&len, tail)) = rest.split_first() {
            let Some((token, tail)) = tail.split_at_checked(usize::from(len)) else {
                break;
            };
            tokens.push(token);
            rest = tail;
        }
        let max_len = tokens.iter().map(|t| t.len()).max().unwrap_or(0);
        Vocab { tokens, max_len }
    })
}

/// C-locale `isspace`: space, \t, \n, \v, \f, \r.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | 9..=13)
}

fn is_other(b: u8) -> bool {
    !is_space(b) && !b.is_ascii_alphanumeric()
}

/// Length of the regex match at the start of the non-empty `b`:
/// `'s|'t|'re|'ve|'m|'ll|'d| ?alpha+| ?digit+| ?other+|\s+(?!\S)|\s+`.
fn next_word_len(b: &[u8]) -> usize {
    if let Some(c) = CONTRACTIONS.iter().find(|c| b.starts_with(c)) {
        return c.len();
    }
    let lead = usize::from(b[0] == b' ');
    let classes: [fn(u8) -> bool; 3] = [
        |c| c.is_ascii_alphabetic(),
        |c| c.is_ascii_digit(),
        is_other,
    ];
    for class in classes {
        let run = b[lead..].iter().take_while(|&&c| class(c)).count();
        if run > 0 {
            return lead + run;
        }
    }
    let spaces = b.iter().take_while(|&&c| is_space(c)).count();
    // `\s+(?!\S)` leaves the last space of a run for the word that follows.
    if spaces > 1 && spaces < b.len() {
        spaces - 1
    } else {
        spaces
    }
}

/// Tokens `text` takes in the multilingual vocab, as `whisper_tokenize` counts them.
#[must_use]
pub fn token_count(text: &str) -> usize {
    let vocab = vocab();
    let mut count = 0;
    let mut rest = text.as_bytes();
    while !rest.is_empty() {
        let (word, tail) = rest.split_at(next_word_len(rest));
        rest = tail;
        let mut at = 0;
        while at < word.len() {
            // Longest vocab entry starting at `at`; a byte nothing covers is skipped, as in whisper.cpp.
            // Capping the end at `max_len` keeps a long word linear, not quadratic.
            let end = (at + 1..=word.len().min(at + vocab.max_len))
                .rev()
                .find(|&end| vocab.tokens.binary_search(&&word[at..end]).is_ok());
            if let Some(end) = end {
                count += 1;
                at = end;
            } else {
                at += 1;
            }
        }
    }
    count
}

/// The dictionary lines that go into the whisper prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    /// Kept lines joined with ", ".
    pub text: String,
    /// `token_count(&text)`, at most [`BUDGET`].
    pub tokens: usize,
    /// Indexes (into the input) of the non-blank lines left out, always a tail of the input.
    pub dropped: Vec<usize>,
}

/// First lines that fit [`BUDGET`] tokens win. The first line that does not fit and
/// every line after it are dropped, so the prompt is a prefix of the dictionary.
/// Blank lines are ignored and never reported.
#[must_use]
pub fn build_prompt<'a>(lines: impl IntoIterator<Item = &'a str>) -> Prompt {
    let mut prompt = Prompt {
        text: String::new(),
        tokens: 0,
        dropped: Vec::new(),
    };
    for (i, line) in lines.into_iter().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if prompt.dropped.is_empty() {
            let sep = if prompt.text.is_empty() { "" } else { ", " };
            let candidate = format!("{}{sep}{line}", prompt.text);
            let tokens = token_count(&candidate);
            if tokens <= BUDGET {
                prompt.text = candidate;
                prompt.tokens = tokens;
                continue;
            }
        }
        prompt.dropped.push(i);
    }
    prompt
}
