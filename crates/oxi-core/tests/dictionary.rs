use std::time::{Duration, Instant};

use oxi_core::dictionary::{BUDGET, build_prompt, token_count};
use proptest::prelude::*;

#[test]
fn counts_follow_the_whisper_cpp_split() {
    // (text, tokens): each case pins one branch of the pre-split regex.
    let table = [
        ("", 0),
        ("hello", 2), // "hel" + "lo": no leading space, no single token
        (" hello", 1),
        ("Hello, world!", 4),
        ("don't", 2),  // "don" + "'t"
        ("a b", 2),    // "a", " b"
        ("a  b", 3),   // "a", " " (all but the last space), " b"
        ("a\n\nb", 4), // "a", "\n", "\n", "b": a lone newline never takes the word after it
        ("a  ", 3),    // "a", then the whole trailing run, which the vocab splits into two spaces
        ("x1", 2),     // letters and digits never share a word
        (" 2024", 1),
    ];
    for (text, want) in table {
        assert_eq!(token_count(text), want, "{text:?}");
    }
}

#[test]
fn cyrillic_is_counted_by_bytes_like_whisper_cpp() {
    // Counts from whisper_tokenize on ggml-tiny.bin (both texts are in tokenizer_regression).
    assert_eq!(token_count("синхрофазотрон"), 8);
    assert_eq!(token_count("привет, мир"), 4);
}

#[test]
fn budget_is_223_tokens() {
    // "a, a, ..., a": n lines take 2n - 1 tokens ("a", then "," and " a" per line).
    let fits = vec!["a"; 112];
    assert_eq!(token_count(&fits.join(", ")), 223);
    let p = build_prompt(fits.iter().copied());
    assert_eq!((p.tokens, p.dropped.len()), (223, 0));

    // "hello" takes 2, so the same shape with 111 more lines is 224: the last one goes.
    let over: Vec<&str> = std::iter::once("hello").chain(vec!["a"; 111]).collect();
    assert_eq!(token_count(&over.join(", ")), 224);
    let p = build_prompt(over.iter().copied());
    assert_eq!((p.tokens, p.dropped.as_slice()), (222, [111].as_slice()));
}

#[test]
fn a_long_word_is_counted_in_linear_time() {
    let word = "x".repeat(32 * 1024);
    let start = Instant::now();
    let n = token_count(&word);
    // Generous for a debug build; the quadratic search took 8-17 s here.
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    assert!(n > 0);
}

#[test]
fn prompt_keeps_everything_that_fits() {
    let p = build_prompt(["Kubernetes", "", "  Podlodka ", "whisper"]);
    assert_eq!(p.text, "Kubernetes, Podlodka, whisper");
    assert_eq!(p.tokens, token_count(&p.text));
    assert_eq!(p.dropped, Vec::<usize>::new());
}

#[test]
fn prompt_drops_the_tail_and_reports_it() {
    let lines: Vec<String> = (0..400).map(|i| format!("слово{i} word{i}")).collect();
    let p = build_prompt(lines.iter().map(String::as_str));
    assert!(p.tokens <= BUDGET && p.tokens > BUDGET - 20, "{}", p.tokens);
    assert_eq!(p.tokens, token_count(&p.text));
    let kept = lines.len() - p.dropped.len();
    assert!(kept > 0 && p.dropped.iter().copied().eq(kept..lines.len()));
    assert!(p.text.starts_with("слово0 word0, слово1 word1"));
}

#[test]
fn a_single_line_over_budget_is_dropped_and_later_lines_too() {
    let long = "word ".repeat(BUDGET + 1);
    let p = build_prompt([long.as_str(), "short"]);
    assert_eq!((p.text.as_str(), p.tokens), ("", 0));
    assert_eq!(p.dropped, [0, 1]);
}

proptest! {
    #[test]
    fn prompt_never_exceeds_the_budget(lines in prop::collection::vec("[ -~а-я]{0,40}", 0..200)) {
        let p = build_prompt(lines.iter().map(String::as_str));
        prop_assert!(p.tokens <= BUDGET);
        prop_assert_eq!(p.tokens, token_count(&p.text));
        // Dropped lines are the non-blank tail; kept ones come first, in order.
        let blank = |i: usize| lines[i].trim().is_empty();
        if let Some(&first) = p.dropped.first() {
            let rest: Vec<usize> = (first..lines.len()).filter(|&i| !blank(i)).collect();
            prop_assert_eq!(&p.dropped, &rest);
        }
    }
}
