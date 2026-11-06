//! `oxi_core::dictionary::token_count` must equal whisper.cpp's `whisper_tokenize`
//! on the real multilingual vocab, and the embedded vocab and budget must match the
//! model. Needs `OXI_TINY_MODEL` (ggml-tiny.bin); skips without it.

use std::path::PathBuf;

use oxi_core::dictionary::{BUDGET, token_count};
use oxi_whisper::{DeviceChoice, Engine};
use whisper_rs::{WhisperContext, WhisperContextParameters};

const VOCAB_FILE: &[u8] = include_bytes!("../../oxi-core/data/vocab/multilingual.vocab");

fn tiny_model() -> Option<PathBuf> {
    let model = std::env::var_os("OXI_TINY_MODEL").map(PathBuf::from);
    if model.is_none() {
        eprintln!("skipped: set OXI_TINY_MODEL");
    }
    model
}

const RU: [&str; 12] = [
    "под",
    "лодка",
    "сер",
    "вер",
    "про",
    "грамма",
    "ми",
    "ро",
    "вание",
    "кон",
    "тей",
    "нер",
];
const EN: [&str; 12] = [
    "kube", "net", "es", "whis", "per", "code", "base", "stream", "pipe", "line", "dev", "ops",
];

/// 300 synthetic ru+en words from a fixed LCG: mixed case, digits, hyphens, apostrophes, Latin/Cyrillic joins.
fn synthetic_words() -> Vec<String> {
    let mut seed = 12345_u64;
    let mut next = move |n: usize| {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        usize::try_from(seed >> 33).unwrap() % n
    };
    (0..300)
        .map(|_| {
            let (a, b) = (next(12), next(12));
            match next(8) {
                0 => format!("{}{}", RU[a], RU[b]),
                1 => format!("{}{}", EN[a], EN[b]),
                2 => format!("{}-{}", RU[a], EN[b]),
                3 => format!("{}{}", EN[a], next(2025)),
                4 => format!("{}'s", EN[a]),
                5 => format!("{}{}", RU[a].to_uppercase(), EN[b].to_uppercase()),
                6 => format!("{}{}", RU[a], EN[b]),
                _ => format!("{}{}.", EN[a], RU[b]),
            }
        })
        .collect()
}

#[test]
fn core_count_equals_whisper_tokenize() {
    let Some(model) = tiny_model() else {
        return;
    };
    let engine = Engine::load(&model, &DeviceChoice::Cpu).unwrap();
    let words = synthetic_words();
    let joined = words.join(", ");
    let newlines = words.join("\n");
    let edge = [
        "hello",
        " hello",
        "Hello, world!",
        "don't",
        "a b",
        "a  b",
        "a\n\nb",
        "a  ",
        "a \n",
        "x1",
        " 2024",
        "  ",
        "\n",
        "'s",
        "it's 'twas",
        "тест",
        "Привет, мир!",
        "a\u{a0}b",
        "emoji 🎉",
        "tab\tsep",
        "e\u{301}",
        "синхрофазотрон",
        "привет, мир",
    ];
    let texts = words
        .iter()
        .map(String::as_str)
        .chain([joined.as_str(), newlines.as_str()])
        .chain(edge);
    let mut total = 0;
    for text in texts {
        let want = engine.token_count(text).unwrap();
        assert_eq!(token_count(text), want, "{text:?}");
        total += want;
    }
    assert!(
        engine.token_count(&joined).unwrap() > 300,
        "list too small to mean anything"
    );
    eprintln!(
        "core == whisper_tokenize on all texts; 300-word list: {} tokens, {total} summed",
        token_count(&joined)
    );
}

#[test]
fn vocab_file_and_budget_match_the_model() {
    let Some(model) = tiny_model() else {
        return;
    };
    let ctx = WhisperContext::new_with_params(&model, WhisperContextParameters::default()).unwrap();
    // whisper.cpp keeps n_text_ctx/2 - 1 prompt tokens: one slot goes to the PREV marker.
    assert_eq!(usize::try_from(ctx.n_text_ctx() / 2 - 1).unwrap(), BUDGET);

    // Text tokens are the ids below EOT; the file is them sorted, empty one dropped, length-prefixed.
    let mut tokens: Vec<&[u8]> = (0..ctx.token_eot())
        .map(|id| ctx.token_to_bytes(id).unwrap())
        .filter(|t| !t.is_empty())
        .collect();
    // `token_to_bytes` reads a C string, so the one-byte NUL token comes back empty: put it back.
    tokens.push(b"\0");
    tokens.sort_unstable();
    let mut file = Vec::with_capacity(VOCAB_FILE.len());
    for t in tokens {
        file.push(u8::try_from(t.len()).unwrap());
        file.extend_from_slice(t);
    }
    assert!(
        file == VOCAB_FILE,
        "embedded vocab differs from the model's tokens"
    );
}
