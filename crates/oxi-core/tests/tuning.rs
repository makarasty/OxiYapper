use oxi_core::models::find;
use oxi_core::tuning::{
    MAX_TEST_SAMPLES, PCM_CHUNK_SAMPLES, Profile, SettingsSet, WordErrors, append_pcm, best,
    decode_pcm, encode_pcm, phrases, word_errors,
};
use oxi_core::types::Language;

#[test]
fn word_error_table() {
    // (reference, hypothesis, errors, words, percent)
    for (reference, hypothesis, errors, words, percent) in [
        ("the cat sat", "the cat sat", 0, 3, 0.0),
        ("the cat sat", "the dog sat", 1, 3, 100.0 / 3.0),
        ("the cat sat", "the cat sat down", 1, 3, 100.0 / 3.0),
        ("the cat sat", "the sat", 1, 3, 100.0 / 3.0),
        ("one two", "three four five six", 4, 2, 200.0),
        ("the cat", "", 2, 2, 100.0),
        ("", "", 0, 0, 0.0),
        ("", "something heard", 2, 0, 100.0),
        // Case and punctuation do not count.
        ("Hello, World!", "hello world", 0, 2, 0.0),
        ("don't stop", "dont stop.", 0, 2, 0.0),
        ("don\u{2019}t", "don't", 0, 1, 0.0),
        ("?!...", "—", 0, 0, 0.0),
        // Russian: yo folds to ye, hyphens separate, a real substitution counts.
        ("Зелёная ёлка", "зеленая елка", 0, 2, 0.0),
        ("кто-то пришёл", "кто то пришел.", 0, 3, 0.0),
        ("большой дом", "большой кот", 1, 2, 50.0),
        // Decomposed letters are the same word (NFC).
        ("cafe\u{301} йод", "café и\u{306}од", 0, 2, 0.0),
        ("ёж", "е\u{308}ж", 0, 1, 0.0),
        // A different accent is a different word.
        ("cafe", "café", 1, 1, 100.0),
    ] {
        let got = word_errors(reference, hypothesis);
        assert_eq!(
            got,
            WordErrors { errors, words },
            "{reference:?} {hypothesis:?}"
        );
        assert!(
            (got.percent() - percent).abs() < 1e-9,
            "{reference:?} {hypothesis:?}: {}",
            got.percent()
        );
    }
}

#[test]
fn errors_add_up_over_phrases() {
    let total = word_errors("a b", "a c").plus(word_errors("d e f g", "d e f g"));
    assert_eq!(
        total,
        WordErrors {
            errors: 1,
            words: 6
        }
    );
}

fn set(normalize: bool, denoise: bool, trim: bool, model: &str) -> SettingsSet {
    SettingsSet {
        normalize,
        denoise,
        trim,
        model: model.into(),
    }
}

fn errors(errors: usize, words: usize) -> WordErrors {
    WordErrors { errors, words }
}

#[test]
fn changes_count_from_the_config_defaults() {
    assert_eq!(SettingsSet::defaults().changes(), 0);
    assert_eq!(set(true, true, true, "turbo").changes(), 1);
    assert_eq!(set(false, true, false, "tiny").changes(), 4);
}

#[test]
fn best_takes_the_lowest_error() {
    let results = [
        (SettingsSet::defaults(), errors(2, 10)),
        (set(true, true, true, "large-v3"), errors(1, 10)),
    ];
    assert_eq!(best(&results), Some(1));
    assert_eq!(best(&[]), None);
}

#[test]
fn a_tie_goes_to_fewer_changes_from_the_defaults() {
    let results = [
        (set(false, true, true, "large-v3"), errors(1, 10)),
        (set(true, true, true, "turbo"), errors(1, 10)),
        (set(true, false, true, "large-v3"), errors(1, 10)),
        (SettingsSet::defaults(), errors(3, 10)),
    ];
    assert_eq!(
        best(&results),
        Some(1),
        "one change beats three, first of the ones"
    );
    // Equal fractions tie whatever the word counts.
    let results = [
        (set(true, true, true, "turbo"), errors(1, 3)),
        (SettingsSet::defaults(), errors(2, 6)),
    ];
    assert_eq!(best(&results), Some(1));
    // Same changes: the earliest.
    let results = [
        (set(true, true, true, "turbo"), errors(0, 3)),
        (set(true, false, false, "turbo"), errors(0, 3)),
    ];
    assert_eq!(best(&results), Some(0));
}

#[test]
fn every_profile_has_three_to_six_distinct_real_sets_defaults_first() {
    for profile in Profile::ALL {
        let sets = profile.sets();
        assert!((3..=6).contains(&sets.len()), "{profile:?}");
        assert_eq!(sets[0], SettingsSet::defaults(), "{profile:?}");
        for (i, s) in sets.iter().enumerate() {
            assert!(find(&s.model).is_some(), "{profile:?} {}", s.model);
            assert!(!sets[..i].contains(s), "{profile:?} repeats {s:?}");
        }
    }
}

#[test]
fn phrase_sets_are_usable() {
    for code in ["ru", "en", "de", "es", "fr"] {
        let language: Language = code.parse().unwrap();
        let list = phrases(language);
        assert!(list.len() >= 3, "{code}");
        for phrase in list {
            let own = word_errors(phrase, phrase);
            assert!(own.words > 0, "{phrase}");
            assert_eq!(own.errors, 0, "{phrase}");
        }
    }
    let other: Language = "fi".parse().unwrap();
    assert_eq!(phrases(other).len(), 0);
    assert_eq!(phrases(Language::AUTO).len(), 0);
}

#[test]
fn pcm_text_round_trips_every_bit() {
    let samples = [0.0, -0.0, 0.5, -1.0, 1e-30, 0.123_456_79];
    let text = encode_pcm(&samples);
    assert_eq!(text.len(), samples.len() * 8);
    assert_eq!(encode_pcm(&[0.5]), "3f000000");
    let back = decode_pcm(&text).unwrap();
    assert_eq!(
        back.iter().map(|s| s.to_bits()).collect::<Vec<_>>(),
        samples.iter().map(|s| s.to_bits()).collect::<Vec<_>>()
    );
}

#[test]
fn bad_pcm_text_is_refused() {
    for bad in [
        "3f00000",
        "zzzzzzzz",
        "3f000000x",
        "7fc00000", // NaN
        "7f800000", // infinity
    ] {
        assert_eq!(decode_pcm(bad), None, "{bad}");
    }
    assert_eq!(decode_pcm(""), Some(Vec::new()));
}

#[test]
fn a_full_chunk_fits_one_line() {
    let line = oxi_core::ipc::encode(&oxi_core::ipc::Request::TestAudio {
        reset: true,
        pcm: encode_pcm(&vec![0.5; PCM_CHUNK_SAMPLES]),
    });
    assert!(line.len() <= oxi_core::ipc::MAX_LINE);
}

#[test]
fn recording_grows_resets_and_empties_on_refusal() {
    let mut held = None;
    assert!(append_pcm(&mut held, true, &encode_pcm(&[0.1, 0.2])));
    assert!(append_pcm(&mut held, false, &encode_pcm(&[0.3])));
    assert_eq!(held, Some(vec![0.1, 0.2, 0.3]));
    assert!(append_pcm(&mut held, true, &encode_pcm(&[0.4])));
    assert_eq!(held, Some(vec![0.4]));
    assert!(!append_pcm(&mut held, false, "nope"));
    assert_eq!(held, None);

    let mut full = Some(vec![0.0; MAX_TEST_SAMPLES]);
    assert!(!append_pcm(&mut full, false, &encode_pcm(&[0.1])));
    assert_eq!(full, None, "over the limit nothing stays");
}

#[test]
fn a_refused_recording_stays_refused_until_reset() {
    let over = encode_pcm(&vec![0.0; MAX_TEST_SAMPLES + 1]);
    for refused in ["nope", over.as_str()] {
        let mut held = None;
        assert!(append_pcm(&mut held, true, &encode_pcm(&[0.1])));
        assert!(!append_pcm(&mut held, false, refused));
        assert!(
            !append_pcm(&mut held, false, &encode_pcm(&[0.2])),
            "a tail is not a recording"
        );
        assert_eq!(held, None);
        assert!(append_pcm(&mut held, true, &encode_pcm(&[0.3])));
        assert_eq!(held, Some(vec![0.3]));
    }
    // A piece that never had a start is no recording either.
    let mut held = None;
    assert!(!append_pcm(&mut held, false, &encode_pcm(&[0.1])));
}
