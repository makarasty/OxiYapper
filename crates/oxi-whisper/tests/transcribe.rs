//! Needs `OXI_TINY_MODEL` (ggml-tiny.bin) and `OXI_JFK_WAV` (whisper.cpp samples/jfk.wav);
//! each test skips with a message when they are unset.

use std::path::{Path, PathBuf};

use oxi_core::types::Language;
use oxi_whisper::{DeviceChoice, Engine};

const JFK: &str = "And so my fellow Americans, ask not what your country can do for you, \
                   ask what you can do for your country.";

fn model() -> Option<PathBuf> {
    let m = std::env::var_os("OXI_TINY_MODEL").map(PathBuf::from);
    if m.is_none() {
        eprintln!("skipped: set OXI_TINY_MODEL");
    }
    m
}

fn jfk() -> Option<Vec<f32>> {
    let w = std::env::var_os("OXI_JFK_WAV").map(PathBuf::from);
    if w.is_none() {
        eprintln!("skipped: set OXI_JFK_WAV");
    }
    w.map(|p| read_wav(&p))
}

/// 16-bit PCM mono WAV to f32; walks the chunks to `data`.
fn read_wav(path: &Path) -> Vec<f32> {
    let b = std::fs::read(path).unwrap();
    let mut at = 12;
    while at + 8 <= b.len() {
        let size = u32::from_le_bytes(b[at + 4..at + 8].try_into().unwrap()) as usize;
        if &b[at..at + 4] == b"data" {
            let end = (at + 8 + size).min(b.len());
            return b[at + 8..end]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&c| f32::from(i16::from_le_bytes(c)) / 32768.0)
                .collect();
        }
        at += 8 + size + (size & 1);
    }
    panic!("no data chunk in {}", path.display());
}

fn words(s: &str) -> Vec<String> {
    s.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// Word error rate: edit distance over normalised words / reference length.
fn wer(reference: &str, hypothesis: &str) -> f64 {
    let (r, h) = (words(reference), words(hypothesis));
    let mut row: Vec<usize> = (0..=h.len()).collect();
    for i in 1..=r.len() {
        let mut prev = row[0];
        row[0] = i;
        for j in 1..=h.len() {
            let cur = row[j];
            row[j] = (prev + usize::from(r[i - 1] != h[j - 1]))
                .min(row[j] + 1)
                .min(row[j - 1] + 1);
            prev = cur;
        }
    }
    row[h.len()] as f64 / r.len() as f64
}

#[test]
fn wer_normalises_and_counts() {
    assert!(wer("Hello, World!", "hello world").abs() < f64::EPSILON);
    assert!((wer("a b c d", "a x c") - 0.5).abs() < f64::EPSILON); // 1 sub + 1 del
    assert!((wer("a b", "") - 1.0).abs() < f64::EPSILON);
}

#[test]
fn jfk_wer_under_0_2() {
    let (Some(model), Some(audio)) = (model(), jfk()) else {
        return;
    };
    let engine = Engine::load(&model, &DeviceChoice::Cpu).unwrap();
    assert_eq!(engine.info().device, None);
    let t = engine.transcribe(&audio, Language::En, "").unwrap();
    let text: String = t.segments.iter().map(|s| s.text.as_str()).collect();
    let w = wer(JFK, &text);
    eprintln!("WER {w:.3}: {text}");
    assert!(w < 0.2, "WER {w}: {text}");
    let last = t.segments.last().unwrap();
    assert!(last.t1 > last.t0 && last.no_speech_prob < 0.5);
}

#[test]
fn auto_device_loads_and_prompt_is_accepted() {
    let (Some(model), Some(audio)) = (model(), jfk()) else {
        return;
    };
    let engine = Engine::load(&model, &DeviceChoice::Auto).unwrap();
    let t = engine
        .transcribe(&audio, Language::Auto, "Kennedy, Americans")
        .unwrap();
    assert!(
        t.segments
            .iter()
            .any(|s| s.text.to_lowercase().contains("country"))
    );
}

#[test]
fn token_count_is_positive_and_monotone() {
    let Some(model) = model() else {
        return;
    };
    let engine = Engine::load(&model, &DeviceChoice::Cpu).unwrap();
    assert_eq!(engine.token_count("").unwrap(), 0);
    let one = engine.token_count("hello").unwrap();
    let many = engine
        .token_count("hello there, how are you doing today")
        .unwrap();
    assert!(one >= 1 && many > one);
}

#[test]
fn missing_model_is_a_load_error() {
    let e = Engine::load(Path::new("no/such/model.bin"), &DeviceChoice::Cpu);
    assert!(matches!(e, Err(oxi_whisper::Error::Load(_))));
}
