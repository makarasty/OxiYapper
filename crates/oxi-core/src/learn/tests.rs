//! Synthetic samples only (AGENTS rule 4): no real dictation text.

use super::*;
use crate::history::{Delivered, Status};

fn said(text: &str) -> Sample {
    Sample {
        text: text.into(),
        ..Sample::default()
    }
}

fn edited(from: &str, to: &str) -> Sample {
    Sample {
        text: to.into(),
        dictated: Some(from.into()),
        ..Sample::default()
    }
}

fn word(w: &str, p: f32) -> Word {
    Word {
        w: w.into(),
        p,
        t0: None,
        t1: None,
    }
}

fn prev(text: &str, model: &str) -> Previous {
    Previous {
        text: text.into(),
        model: model.into(),
        language: "ru".into(),
        settings: None,
        newer: false,
        edited: false,
    }
}

#[test]
fn an_edit_is_a_kept_text_of_the_same_run_and_a_transcribe_again_is_not() {
    let meta = |original: Option<Previous>| Meta {
        original,
        ..Meta::basic("2026-11-13/100000-000", 1000, Status::Done, "turbo", "ru")
    };
    let s = Sample::new(
        meta(Some(prev("Hello there.", "turbo"))),
        "hello there".into(),
    );
    assert_eq!(s.dictated.as_deref(), Some("Hello there."));
    let again = Sample::new(
        meta(Some(prev("Hello there.", "small"))),
        "hello there".into(),
    );
    assert_eq!(again.dictated, None, "another model made it: not an edit");
    let same = Sample::new(meta(Some(prev("Hi.", "turbo"))), "Hi.".into());
    assert_eq!(same.dictated, None, "unchanged");
    let filtered = Sample::new(
        Meta {
            delivered: Some(Delivered::Filtered),
            ..meta(None)
        },
        "x".into(),
    );
    assert!(filtered.filtered);
}

#[test]
fn doubted_words_need_three_dictations_and_skip_known_and_short_ones() {
    let doubt = |ws: &[(&str, f32)]| Sample {
        words: ws.iter().map(|&(w, p)| word(w, p)).collect(),
        ..Sample::default()
    };
    let samples = vec![
        doubt(&[
            (" Zorblat,", 0.2),
            ("zorblat", 0.1),
            ("ok", 0.1),
            ("Kvex", 0.3),
        ]),
        doubt(&[("zorblat", 0.3), ("Kvex", 0.2), ("fine", 0.9)]),
        doubt(&[("Zorblat", 0.4), ("kvex", 0.44), ("12345", 0.1)]),
        doubt(&[("Quillon", 0.1), ("fine", 0.2)]),
        doubt(&[("Quillon", 0.1), ("fine", 0.2)]),
    ];
    let known: HashSet<String> = ["kvex".to_owned()].into();
    let found = doubted_words(&samples, &known);
    assert_eq!(
        found,
        [Doubted {
            word: "Zorblat".into(),
            count: 3
        }],
        "twice in one dictation counts once; the newest form is shown; known, short, digits and \
         two-dictation words stay out"
    );
    assert_eq!(doubted_words(&samples, &HashSet::new()).len(), 2);
}

#[test]
fn habits_need_five_edits_of_a_kind_and_seventy_percent() {
    let mut s: Vec<Sample> = (0..5)
        .map(|_| edited("Hello there.", "hello there"))
        .collect();
    let h = habits(&s);
    assert_eq!(h.no_final_period, Some(Count { hits: 5, of: 5 }));
    assert_eq!(h.small_first_letter, Some(Count { hits: 5, of: 5 }));
    // Three other edits: 5 of 8 is under 70 %.
    s.extend((0..3).map(|_| edited("Wisper is.", "Whisper is.")));
    assert_eq!(habits(&s), Habits::default());
    // Four edits are too few, and a full stop turned into "!" is no removal.
    let few: Vec<Sample> = (0..4).map(|_| edited("Go.", "Go")).collect();
    assert_eq!(habits(&few).no_final_period, None);
    let bang: Vec<Sample> = (0..6).map(|_| edited("Go.", "Go!")).collect();
    assert_eq!(habits(&bang).no_final_period, None);
    let dots: Vec<Sample> = (0..6).map(|_| edited("Well...", "Well")).collect();
    assert_eq!(
        habits(&dots).no_final_period,
        None,
        "an ellipsis is no full stop"
    );
    // Texts never edited count for nothing.
    assert_eq!(habits(&vec![said("Hi."); 20]), Habits::default());
}

#[test]
fn fillers_are_short_words_deleted_in_three_edits() {
    let s = vec![
        edited("ну, привет мир", "привет мир"),
        edited("Ну это работает", "это работает"),
        edited("ну типа так", "так"),
        edited("типа да", "да"),
        edited("um okay", "okay"),
        edited("wonderfully done", "done"),
    ];
    let (words, count) = deleted_fillers(&s, &[]).unwrap();
    assert_eq!(words, ["ну"]);
    assert_eq!(count, Count { hits: 3, of: 6 });
    let known = ["ну".to_owned()];
    assert_eq!(deleted_fillers(&s, &known), None, "already in the list");
    let more = [s.clone(), vec![edited("типа нет", "нет")]].concat();
    let (words, count) = deleted_fillers(&more, &[]).unwrap();
    assert_eq!(words, ["ну", "типа"]);
    assert_eq!(count.hits, 5);
}

#[test]
fn a_phrase_said_four_times_becomes_a_shortcut_with_a_two_word_trigger() {
    let mut s = vec![
        said("Ok. Best regards, Alex Smith"),
        said("best regards, Alex Smith."),
        said("Thanks! Best regards, Alex Smith"),
        said("Best regards, alex smith"),
        said("something else entirely here"),
    ];
    let p = repeated_phrase(&s, &[]).unwrap();
    assert_eq!(p.text, "Best regards, Alex Smith");
    assert_eq!(p.trigger, "best regards");
    assert_eq!(p.count, Count { hits: 4, of: 5 });
    let snip = [Snippet {
        trigger: "sign".into(),
        text: "Best regards, Alex Smith".into(),
    }];
    assert_eq!(repeated_phrase(&s, &snip), None, "already a shortcut");
    s.truncate(3);
    assert_eq!(repeated_phrase(&s, &[]), None, "three times is not enough");
}

#[test]
fn certainty_drops_to_the_median_of_the_parts_added_back_and_never_steps_twice() {
    let held = |text: &str| HeldPart {
        text: text.into(),
        t0: 0,
        t1: 1,
        why: HeldWhy::LowConfidence,
    };
    let entry = |text: &str, part: &str, p: f32| Sample {
        text: text.into(),
        // The text before the edit: the held part was not in it.
        dictated: Some("start".into()),
        words: [word("start", 0.9)]
            .into_iter()
            .chain(part.split(' ').map(|w| word(w, p)))
            .collect(),
        held: vec![held(part)],
        ..Sample::default()
    };
    // 4 of 10 parts were added back, their certainty 0.22, 0.28, 0.31, 0.33.
    let mut s: Vec<Sample> = [0.22, 0.28, 0.31, 0.33]
        .iter()
        .map(|&p| entry("start alpha beta", "alpha beta", p))
        .collect();
    s.extend((0..6).map(|_| entry("start", "gamma delta", 0.1)));
    let (value, count) = certainty(&s, 0.35).unwrap();
    assert!((value - 0.30).abs() < 1e-6, "{value}");
    assert_eq!(count, Count { hits: 4, of: 10 });
    assert_eq!(
        certainty(&s, value),
        None,
        "once applied, the same parts say nothing"
    );
    // 2 of 10 added back: under 30 %; fewer than 10 parts: nothing.
    assert_eq!(certainty(&s[2..], 0.35), None);
    let few: Vec<Sample> = s.iter().take(4).cloned().collect();
    assert_eq!(certainty(&few, 0.35), None);
    // An all-held entry has its parts in the text anyway: not counted.
    let filtered: Vec<Sample> = (0..12)
        .map(|_| Sample {
            filtered: true,
            ..entry("alpha beta", "alpha beta", 0.2)
        })
        .collect();
    assert_eq!(certainty(&filtered, 0.35), None);
}

fn held_part(text: &str, t0: u32) -> HeldPart {
    HeldPart {
        text: text.into(),
        t0,
        t1: t0 + 100,
        why: HeldWhy::LowConfidence,
    }
}

#[test]
fn a_held_part_is_added_back_only_when_the_edit_put_it_in() {
    // "и" is inside "привет", and said elsewhere in the text, but nothing was added back.
    let sample = |dictated: Option<&str>| Sample {
        text: "привет и мир".into(),
        dictated: dictated.map(str::to_owned),
        words: vec![word("привет", 0.9), word("и", 0.2), word("мир", 0.9)],
        held: vec![held_part("и", 500)],
        ..Sample::default()
    };
    let never_edited: Vec<Sample> = (0..10).map(|_| sample(None)).collect();
    assert_eq!(certainty(&never_edited, 0.5), None, "never edited");
    let edited_elsewhere: Vec<Sample> = (0..10).map(|_| sample(Some("привет и мир."))).collect();
    assert_eq!(
        certainty(&edited_elsewhere, 0.5),
        None,
        "the word was in the text before the edit"
    );
    let inside: Vec<Sample> = (0..10)
        .map(|_| Sample {
            text: "привет мир".into(),
            dictated: Some("привет мир.".into()),
            held: vec![held_part("и", 500)],
            ..Sample::default()
        })
        .collect();
    assert_eq!(
        certainty(&inside, 0.5),
        None,
        "a part of a word is no match"
    );
    // The edit really put it in: that counts.
    let restored: Vec<Sample> = (0..10).map(|_| sample(Some("привет мир"))).collect();
    let (value, count) = certainty(&restored, 0.5).unwrap();
    assert!((value - 0.2).abs() < 1e-6, "{value}");
    assert_eq!(count, Count { hits: 10, of: 10 });
}

#[test]
fn a_repeated_word_takes_the_certainty_of_the_run_that_was_held() {
    let make = |timed: bool, held_at: u32| {
        let at = |t: u32| if timed { Some(t) } else { None };
        Sample {
            text: "the cat the dog".into(),
            dictated: Some("the cat dog".into()),
            words: vec![
                Word {
                    t0: at(0),
                    ..word("the", 0.9)
                },
                word("cat", 0.9),
                Word {
                    t0: at(2000),
                    ..word("the", 0.2)
                },
                word("dog", 0.9),
            ],
            held: vec![held_part("the", held_at)],
            ..Sample::default()
        }
    };
    for (timed, held_at) in [(true, 2000), (false, 0)] {
        let s: Vec<Sample> = (0..10).map(|_| make(timed, held_at)).collect();
        let (value, _) = certainty(&s, 0.9).unwrap();
        assert!((value - 0.2).abs() < 1e-6, "timed {timed}: {value}");
    }
    // Two held parts of the same word are two different runs, least certain first.
    let mut two = make(false, 0);
    two.held = vec![held_part("the", 0), held_part("the", 2000)];
    assert_eq!(part_p(&two.held[0], 0, &two.words), Some(0.2));
    assert_eq!(part_p(&two.held[1], 1, &two.words), Some(0.9));
}

#[test]
fn a_dismiss_key_is_stable_case_blind_and_tells_kinds_apart() {
    assert_eq!(
        dismiss_key("word", "Zorblat"),
        dismiss_key("word", "zorblat")
    );
    assert_ne!(
        dismiss_key("word", "zorblat"),
        dismiss_key("phrase", "zorblat")
    );
    assert_ne!(dismiss_key("word", "ab"), dismiss_key("worda", "b"));
    // Fixed value: the file on disk must keep meaning the same thing.
    assert_eq!(dismiss_key("period", ""), "b29a0f2cef1925ec".to_owned());
    assert_eq!(dismiss_key("word", "zorblat").len(), 16);
}
