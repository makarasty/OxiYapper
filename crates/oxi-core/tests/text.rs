use oxi_core::text::{
    DecodeError, HallucinationFilter, Outcome, ReplaceRules, decode_text_file, process,
};
use proptest::prelude::*;
use std::fmt::Write as _;

fn run(segments: &[&str], rules: &str, trailing: bool) -> Outcome {
    process(
        segments,
        &HallucinationFilter::default(),
        &ReplaceRules::parse(rules).rules,
        trailing,
    )
}

fn text(s: &str) -> Outcome {
    Outcome::Text(s.to_owned())
}

#[test]
fn newline_stripped_without_space_then_trimmed() {
    assert_eq!(run(&["  con\nference\r\n "], "", false), text("conference"));
}

#[test]
fn trailing_space_only_on_text() {
    assert_eq!(run(&["hello"], "", true), text("hello "));
    assert_eq!(run(&["  \n "], "", true), Outcome::NothingHeard);
}

#[test]
fn segments_join_with_one_space() {
    assert_eq!(run(&[" one", " two\n"], "", false), text("one two"));
}

#[test]
fn hallucinated_segments_dropped() {
    let cases: &[(&[&str], Outcome)] = &[
        (&["Субтитры сделал DimaTorzok"], Outcome::NothingHeard),
        (&[" Продолжение следует..."], Outcome::NothingHeard),
        (&["РЕДАКТОР СУБТИТРОВ Иван Иванов"], Outcome::NothingHeard),
        (&["Thanks for watching!", "Real words"], text("Real words")),
        // A phrase inside a longer sentence is real speech.
        (
            &["Thanks for watching my kids"],
            text("Thanks for watching my kids"),
        ),
        // The prefix entry needs a word boundary.
        (&["Корректорский совет"], text("Корректорский совет")),
        // Real speech starting with a credit word is kept.
        (
            &["Корректор проверил договор."],
            text("Корректор проверил договор."),
        ),
        (&["Корректор субтитров Иван"], Outcome::NothingHeard),
    ];
    for (segments, want) in cases {
        assert_eq!(&run(segments, "", false), want, "{segments:?}");
    }
}

#[test]
fn user_lines_extend_the_default_list() {
    let filter = HallucinationFilter::new("# comment\nmy custom noise\n");
    assert!(filter.is_hallucination("My Custom Noise."));
    assert!(filter.is_hallucination("Thanks for watching"));
    assert!(!filter.is_hallucination("custom"));
}

#[test]
fn replace_rules_table() {
    let rules = "git hub => GitHub\ngithub => GitHub\npull => fetch\npull request => PR\nджава скрипт => JavaScript\nc++ => C++\num =>\nm => meters\nll => will\n";
    let cases = [
        ("Open Git Hub now", "Open GitHub now"),
        // Longest match first.
        ("a pull request", "a PR"),
        ("a pull", "a fetch"),
        // Whole word only.
        ("pulled pulls", "pulled pulls"),
        ("unpull", "unpull"),
        // Case-insensitive, Cyrillic included.
        ("ДЖАВА Скрипт", "JavaScript"),
        // Punctuation next to the match is a boundary.
        ("pull, then (git hub).", "fetch, then (GitHub)."),
        // A non-word edge needs no boundary.
        ("c++x", "C++x"),
        // Empty wanted deletes.
        ("well um fine", "well fine"),
        // No space is left before punctuation.
        ("well um, fine", "well, fine"),
        ("so um\u{2026} ok", "so\u{2026} ok"),
        // Apostrophes inside a word do not make a boundary.
        ("I'm here", "I'm here"),
        ("I'll go", "I'll go"),
        ("I\u{2019}m here", "I\u{2019}m here"),
        // The whole word before an apostrophe suffix still matches.
        ("github's repo", "GitHub's repo"),
        ("git hub's API", "GitHub's API"),
        ("git hub\u{2019}s API", "GitHub\u{2019}s API"),
        // A quote around a word does.
        ("'pull' it", "'fetch' it"),
    ];
    for (input, want) in cases {
        assert_eq!(
            ReplaceRules::parse(rules).rules.apply(input),
            want,
            "{input}"
        );
    }
}

#[test]
fn deleted_word_leaves_no_space_before_punctuation() {
    let rules = ReplaceRules::parse("well =>\n").rules;
    assert_eq!(rules.apply("fine well."), "fine.");
    assert_eq!(rules.apply("fine well, ok"), "fine, ok");
}

#[test]
fn replace_is_one_pass() {
    let rules = ReplaceRules::parse("a => b\nb => c\n").rules;
    assert_eq!(rules.apply("a b"), "b c");
}

#[test]
fn first_duplicate_wins() {
    assert_eq!(
        ReplaceRules::parse("x => one\nX => two\n").rules.apply("x"),
        "one"
    );
}

#[test]
fn bad_lines_reported_with_numbers() {
    let parsed = ReplaceRules::parse("ok => fine\n\nno arrow\r\n => empty left\nlast => ok");
    assert_eq!(parsed.bad_lines, vec![3, 4]);
    assert_eq!(parsed.rules.apply("ok last"), "fine ok");
}

#[test]
fn rule_deleting_everything_is_nothing_heard() {
    assert_eq!(run(&["um"], "um =>", true), Outcome::NothingHeard);
}

#[test]
fn decode_table() {
    let le_bytes: Vec<u8> = [0xFF, 0xFE]
        .into_iter()
        .chain("Привет a".encode_utf16().flat_map(u16::to_le_bytes))
        .collect();
    let be_bytes: Vec<u8> = [0xFE, 0xFF]
        .into_iter()
        .chain("Привет a".encode_utf16().flat_map(u16::to_be_bytes))
        .collect();
    assert_eq!(
        decode_text_file(b"\xEF\xBB\xBFa => b").as_deref(),
        Ok("a => b")
    );
    assert_eq!(
        decode_text_file("Привет".as_bytes()).as_deref(),
        Ok("Привет")
    );
    assert_eq!(decode_text_file(&le_bytes).as_deref(), Ok("Привет a"));
    assert_eq!(decode_text_file(&be_bytes).as_deref(), Ok("Привет a"));
    assert_eq!(
        decode_text_file(b"\xFF\xFFabc"),
        Err(DecodeError::InvalidUtf8)
    );
    assert_eq!(
        decode_text_file(&[0xFF, 0xFE, 0x41]),
        Err(DecodeError::InvalidUtf16)
    );
    assert_eq!(
        decode_text_file(&[0xFF, 0xFE, 0x00, 0xD8]),
        Err(DecodeError::InvalidUtf16)
    );
}

#[test]
fn snapshot_corpus() {
    let filter = HallucinationFilter::default();
    let rules = ReplaceRules::parse(include_str!("fixtures/text/replace.txt"));
    let mut out = format!("bad lines: {:?}\n", rules.bad_lines);
    for line in include_str!("fixtures/text/corpus.txt").lines() {
        let seg = line.replace("\\n", "\n");
        let res = process(&[seg], &filter, &rules.rules, true);
        writeln!(out, "{res:?}").unwrap();
    }
    insta::assert_snapshot!(out);
}

proptest! {
    #[test]
    fn no_rules_is_identity_after_strip_and_trim(s in "\\PC{0,60}|[a-zа-я \\n\\r]{0,60}") {
        // Skip inputs the default list would legitimately drop.
        prop_assume!(!HallucinationFilter::default().is_hallucination(s.replace(['\n', '\r'], "").trim()));
        let want = s.replace(['\n', '\r'], "").trim().to_owned();
        let got = run(&[&s], "", false);
        if want.is_empty() {
            prop_assert_eq!(got, Outcome::NothingHeard);
        } else {
            prop_assert_eq!(got, Outcome::Text(want));
        }
    }

    #[test]
    fn output_never_contains_newline(
        s in "[a-zа-я \\n\\r]{0,60}",
        heard in "[a-zа-я]{1,4}",
        wanted in "[a-zа-я ]{0,4}",
    ) {
        if let Outcome::Text(t) = run(&[&s], &format!("{heard} => {wanted}"), true) {
            prop_assert!(!t.contains('\n') && !t.contains('\r'));
        }
    }

    #[test]
    fn whole_word_rule_never_fires_inside_a_word(
        prefix in "[a-zа-я0-9]{0,5}",
        heard in "[a-zа-я]{1,4}",
        suffix in "[a-zа-я0-9]{0,5}",
    ) {
        prop_assume!(!prefix.is_empty() || !suffix.is_empty());
        let word = format!("{prefix}{heard}{suffix}");
        let rules = ReplaceRules::parse(&format!("{heard} => @@")).rules;
        prop_assert_eq!(rules.apply(&word), word);
    }
}
