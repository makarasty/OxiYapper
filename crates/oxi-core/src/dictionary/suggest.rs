//! Replacement suggestions from a History text edit (arch-plan 9.1, idea 7): what the user
//! changed, as `heard => wanted` pairs to offer for `replace.txt`. Nothing is stored or added
//! here; the settings window offers each pair once and adds one only on a click.

/// Most words on either side of one replaced place.
const MAX_RUN: usize = 3;
/// Most replaced places an edit may have and still read as "the model misheard".
const MAX_RUNS: usize = 3;
/// Word-pair cells the diff table may take (about 1000 x 1000 words); a longer text gives nothing.
const MAX_CELLS: usize = 1_000_000;

/// One offer: `heard` is what the model wrote, `wanted` what the user typed instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub heard: String,
    pub wanted: String,
}

/// Lower-cased, without punctuation at either end: how words are compared.
fn key(word: &str) -> String {
    word.trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

/// A run of words as one string, without punctuation at its ends.
fn run(words: &[&str]) -> String {
    words
        .join(" ")
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_owned()
}

/// The places where `new` replaced words of `old`, when the edit is a handful of such places;
/// else empty (the user rewrote the text, added or removed words, or only changed case or
/// punctuation: nothing was misheard). `wanted` is spelled as typed.
pub fn suggest(old: &str, new: &str) -> Vec<Suggestion> {
    let old_w: Vec<&str> = old.split_whitespace().collect();
    let new_w: Vec<&str> = new.split_whitespace().collect();
    let (rows, cols) = (old_w.len(), new_w.len());
    if rows.saturating_mul(cols) > MAX_CELLS {
        return Vec::new();
    }
    let keys_old: Vec<String> = old_w.iter().map(|w| key(w)).collect();
    let keys_new: Vec<String> = new_w.iter().map(|w| key(w)).collect();
    // lcs[oi][nj]: longest common subsequence of keys_old[oi..] and keys_new[nj..].
    let mut lcs = vec![vec![0_u32; cols + 1]; rows + 1];
    for oi in (0..rows).rev() {
        for nj in (0..cols).rev() {
            lcs[oi][nj] = if keys_old[oi] == keys_new[nj] {
                lcs[oi + 1][nj + 1] + 1
            } else {
                lcs[oi + 1][nj].max(lcs[oi][nj + 1])
            };
        }
    }
    let mut pairs = Vec::new();
    let (mut oi, mut nj) = (0, 0);
    while oi < rows && nj < cols {
        if keys_old[oi] == keys_new[nj] {
            pairs.push((oi, nj));
            oi += 1;
            nj += 1;
        } else if lcs[oi + 1][nj] >= lcs[oi][nj + 1] {
            oi += 1;
        } else {
            nj += 1;
        }
    }
    pairs.push((rows, cols));
    // A matched word spelled otherwise right after a replaced run belongs to it: "cloud code" ->
    // "Claude Code" is one place, not "cloud" -> "Claude" plus a case change.
    let mut kept = Vec::with_capacity(pairs.len());
    let (mut next, mut in_run) = ((0, 0), false);
    for (pi, pj) in pairs {
        let restyled = pi < rows && run(&old_w[pi..=pi]) != run(&new_w[pj..=pj]);
        in_run = restyled && (in_run || (pi, pj) != next);
        if !in_run {
            kept.push((pi, pj));
        }
        next = (pi + 1, pj + 1);
    }

    let mut out: Vec<Suggestion> = Vec::new();
    let mut runs = 0;
    let (mut from_a, mut from_b) = (0, 0);
    for (pi, pj) in kept {
        let (gap_a, gap_b) = (&old_w[from_a..pi], &new_w[from_b..pj]);
        (from_a, from_b) = (pi + 1, pj + 1);
        if gap_a.is_empty() && gap_b.is_empty() {
            continue;
        }
        // Words only added or only removed, or too long old_w run: not old_w mishearing.
        if gap_a.is_empty() || gap_b.is_empty() || gap_a.len() > MAX_RUN || gap_b.len() > MAX_RUN {
            return Vec::new();
        }
        runs += 1;
        let (heard, wanted) = (run(gap_a), run(gap_b));
        if heard.is_empty() || wanted.is_empty() {
            return Vec::new();
        }
        let again = out
            .iter()
            .any(|s| s.heard.to_lowercase() == heard.to_lowercase() && s.wanted == wanted);
        if !again {
            out.push(Suggestion { heard, wanted });
        }
    }
    if runs > MAX_RUNS {
        return Vec::new();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn pair(heard: &str, wanted: &str) -> Suggestion {
        Suggestion {
            heard: heard.into(),
            wanted: wanted.into(),
        }
    }

    #[test]
    fn equal_texts_give_nothing() {
        assert_eq!(suggest("привет мир", "привет мир"), []);
        assert_eq!(suggest("", ""), []);
    }

    #[test]
    fn one_word_replaced_gives_one_suggestion() {
        assert_eq!(
            suggest("open the cloud code now", "open the Claude Code now"),
            vec![pair("cloud code", "Claude Code")]
        );
        assert_eq!(
            suggest("я люблю рост", "я люблю Rust"),
            vec![pair("рост", "Rust")]
        );
    }

    #[test]
    fn two_words_replaced_by_one_keeps_both_sides_whole() {
        assert_eq!(
            suggest("a git hub repo", "a GitHub repo"),
            vec![pair("git hub", "GitHub")]
        );
    }

    #[test]
    fn two_separate_replaced_words_give_two() {
        assert_eq!(
            suggest("one cat two dog three", "one cow two fox three"),
            vec![pair("cat", "cow"), pair("dog", "fox")]
        );
    }

    #[test]
    fn case_or_punctuation_only_gives_nothing() {
        assert_eq!(suggest("hello world", "Hello, world."), []);
        assert_eq!(suggest("Привет, мир", "привет мир!"), []);
    }

    #[test]
    fn words_only_added_or_removed_give_nothing() {
        assert_eq!(suggest("a b", "a b c"), []);
        assert_eq!(suggest("a b c", "a b"), []);
        // A real replacement next to an added word is a rewrite, not a mishearing.
        assert_eq!(suggest("a cat b", "a cow b c"), []);
    }

    #[test]
    fn a_run_of_four_words_gives_nothing() {
        assert_eq!(suggest("x a b c d y", "x p q r s y"), []);
        assert_eq!(suggest("x a b c y", "x p q r y").len(), 1);
    }

    #[test]
    fn four_replaced_runs_give_nothing_three_are_fine() {
        assert_eq!(suggest("a 1 b 2 c 3 d 4 e", "a w b x c y d z e"), []);
        assert_eq!(suggest("a 1 b 2 c 3 d", "a w b x c y d").len(), 3);
    }

    #[test]
    fn the_same_replacement_twice_is_offered_once() {
        assert_eq!(
            suggest("cat sat cat", "cow sat cow"),
            vec![pair("cat", "cow")]
        );
    }

    proptest! {
        #[test]
        fn any_two_texts_never_panic_and_each_side_is_a_run_of_its_text(
            old in "[a-cA-C.,! ]{0,40}",
            new in "[a-cA-C.,! ]{0,40}",
        ) {
            let words = |s: &str| -> Vec<String> {
                s.split_whitespace().map(key).collect()
            };
            let has_run = |text: &[String], run: &str| {
                let want: Vec<String> = run.split_whitespace().map(key).collect();
                !want.is_empty() && text.windows(want.len()).any(|w| w == want.as_slice())
            };
            let (wo, wn) = (words(&old), words(&new));
            for s in suggest(&old, &new) {
                prop_assert!(has_run(&wo, &s.heard), "{old:?} -> {new:?}: {s:?}");
                prop_assert!(has_run(&wn, &s.wanted), "{old:?} -> {new:?}: {s:?}");
            }
        }
    }
}
