//! Each stage alone, the stage order, and "every stage off is today's pipeline".

use super::*;
use proptest::prelude::*;

fn snip(trigger: &str, text: &str) -> Snippet {
    Snippet {
        trigger: trigger.into(),
        text: text.into(),
    }
}

fn fillers() -> Vec<String> {
    ["э", "эм", "um", "uh"].map(String::from).to_vec()
}

fn word(text: &'static str, p: f32, t0: u64, t1: u64) -> WordIn<'static> {
    WordIn {
        text,
        p,
        t0: Some(Millis(t0)),
        t1: Some(Millis(t1)),
    }
}

/// A segment without word data.
fn seg(text: &str) -> Seg<'_> {
    Seg {
        text,
        t0: Millis(0),
        t1: Millis(1),
        no_speech_prob: 0.0,
        words: &[],
    }
}

fn on(f: impl FnOnce(&mut TextOptions)) -> TextOptions {
    let mut o = TextOptions::default();
    f(&mut o);
    o
}

fn go_with(
    segs: &[Seg<'_>],
    opts: &TextOptions,
    rules: &str,
    fillers: &[String],
    snippets: &[Snippet],
) -> Processed {
    let rules = ReplaceRules::parse(rules).rules;
    run(
        segs,
        opts,
        &Lists {
            hallucinations: &HallucinationFilter::default(),
            replace: &rules,
            fillers,
            snippets,
        },
    )
}

/// One text through the pipeline with `opts` and the default fillers.
fn go(text: &str, opts: &TextOptions) -> Outcome {
    go_with(&[seg(text)], opts, "", &fillers(), &[]).outcome
}

fn text(s: &str) -> Outcome {
    Outcome::Text(s.to_owned())
}

#[test]
fn every_stage_off_leaves_the_text_as_it_was() {
    // Fillers, commands, pauses and snippets are all in the text and all off.
    let snippets = [snip("привет", "BLOCK")];
    let t = "Привет, э, новая строка, um scratch that";
    let o = go_with(
        &[seg(t)],
        &TextOptions::default(),
        "",
        &fillers(),
        &snippets,
    );
    assert_eq!(o.outcome, text(t));
    assert_eq!(o.held, []);
    assert_eq!((o.dropped_hallucination, o.dropped_implausible), (0, 0));
}

#[test]
fn a_snippet_is_never_off() {
    // Snippets have no switch: an empty list is the "off" (arch-plan 9.1 step 8).
    let o = go_with(
        &[seg("Привет.")],
        &TextOptions::default(),
        "",
        &[],
        &[snip("привет", "x")],
    );
    assert_eq!(o.outcome, text("x"));
}

#[test]
fn filler_filter_removes_whole_words_and_their_comma() {
    let o = on(|o| o.filler_filter = true);
    assert_eq!(go("Ну, э, привет", &o), text("Ну, привет"));
    assert_eq!(go("Um, hello UH world", &o), text("Hello world"));
    // Part of a word is not a filler; the list is whole words.
    assert_eq!(go("эмоция umbrella", &o), text("эмоция umbrella"));
    // A sentence start left lower-case is capitalised.
    assert_eq!(go("Э, привет. Эм, пока", &o), text("Привет. Пока"));
    // The mark of a sentence the filler ended moves to the word before.
    assert_eq!(go("Привет, э. Пока", &o), text("Привет. Пока"));
    // Only fillers: nothing is left.
    assert_eq!(go("э, эм...", &o), Outcome::NothingHeard);
}

#[test]
fn filler_filter_uses_the_editable_list() {
    let o = on(|o| o.filler_filter = true);
    let own = ["короче".to_owned()];
    let got = go_with(&[seg("Ну короче, э привет")], &o, "", &own, &[]).outcome;
    assert_eq!(got, text("Ну э привет"));
    assert_eq!(
        go_with(&[seg("Ну короче, э привет")], &o, "", &[], &[]).outcome,
        text("Ну короче, э привет")
    );
}

#[test]
fn voice_commands_make_breaks_and_delete_the_last_sentence() {
    let o = on(|o| o.voice_commands = true);
    assert_eq!(
        go("Привет. Новая строка. Пока.", &o),
        text("Привет.\nПока.")
    );
    assert_eq!(
        go("привет, новый абзац, как дела", &o),
        text("привет\n\nКак дела")
    );
    assert_eq!(go("Hello. New line. World.", &o), text("Hello.\nWorld."));
    assert_eq!(go("One. New paragraph. Two.", &o), text("One.\n\nTwo."));
    assert_eq!(go("Первое. Второе. Удали последнее.", &o), text("Первое."));
    assert_eq!(
        go("Keep. Drop this one. Scratch that. More.", &o),
        text("Keep. More.")
    );
    assert_eq!(go("Keep. Drop this one. Delete that", &o), text("Keep."));
    // At the start there is nothing before it: it removes only itself.
    assert_eq!(go("Удали последнее. Привет.", &o), text("Привет."));
    assert_eq!(
        go("привет как дела удали последнее", &o),
        Outcome::NothingHeard
    );
    // Two words that only look like a command are text.
    assert_eq!(go("a new idea, a line", &o), text("a new idea, a line"));
}

#[test]
fn a_break_at_the_very_end_is_kept_and_gets_no_trailing_space() {
    let o = on(|o| {
        o.voice_commands = true;
        o.trailing_space = true;
    });
    assert_eq!(go("Привет. Новая строка.", &o), text("Привет.\n"));
    assert_eq!(go("Привет.", &o), text("Привет. "));
    let typed = on(|o| {
        o.voice_commands = true;
        o.trailing_space = true;
        o.newlines_allowed = false;
    });
    assert_eq!(
        go("Привет. Новая строка. Пока.", &typed),
        text("Привет. Пока. ")
    );
    assert_eq!(go("Привет. Новая строка.", &typed), text("Привет. "));
}

#[test]
fn a_snippet_needs_the_whole_text_to_be_its_trigger() {
    let snippets = [snip("мой адрес", "ул. Ленина, 1\r\nкв. 5")];
    let run_one =
        |t: &str, opts: &TextOptions| go_with(&[seg(t)], opts, "", &[], &snippets).outcome;
    let o = TextOptions::default();
    assert_eq!(run_one("  Мой   адрес. ", &o), text("ул. Ленина, 1\nкв. 5"));
    assert_eq!(run_one("«Мой адрес»", &o), text("ул. Ленина, 1\nкв. 5"));
    // Inside a longer sentence: no false hit in normal speech.
    assert_eq!(run_one("Скажи мой адрес", &o), text("Скажи мой адрес"));
    assert_eq!(run_one("Мой адрес дома", &o), text("Мой адрес дома"));
    // A typed snippet is one line.
    let typed = on(|o| o.newlines_allowed = false);
    assert_eq!(run_one("мой адрес", &typed), text("ул. Ленина, 1 кв. 5"));
}

#[test]
fn the_stages_run_in_arch_plan_order() {
    let all = on(|o| {
        o.filler_filter = true;
        o.voice_commands = true;
    });
    // A replace rule fixes a misheard command before commands run.
    assert_eq!(
        go_with(
            &[seg("Раз. Новой строки. Два.")],
            &all,
            "новой строки => новая строка\n",
            &[],
            &[]
        )
        .outcome,
        text("Раз.\nДва.")
    );
    // A filler is gone before commands look at the text: it cannot split a command.
    assert_eq!(
        go("Привет. Новая э строка. Пока.", &all),
        text("Привет.\nПока.")
    );
    // A command first, then a snippet that is what is left.
    let snippets = [snip("подпись", "С уважением")];
    assert_eq!(
        go_with(&[seg("Новая строка. Подпись.")], &all, "", &[], &snippets).outcome,
        text("С уважением")
    );
    // A snippet trigger the replace rules rewrote still hits.
    assert_eq!(
        go_with(
            &[seg("Подписи")],
            &TextOptions::default(),
            "подписи => подпись\n",
            &[],
            &snippets
        )
        .outcome,
        text("С уважением")
    );
}

#[test]
fn pauses_end_sentences_and_break_paragraphs_from_word_times() {
    let words = [
        word("привет", 0.9, 0, 400),
        word("как", 0.9, 1400, 1700),
        word("дела", 0.9, 1750, 2000),
        word("хорошо", 0.9, 4000, 4300),
    ];
    let s = Seg {
        text: "привет как дела хорошо",
        t0: Millis(0),
        t1: Millis(4300),
        no_speech_prob: 0.0,
        words: &words,
    };
    let go_p = |sentence, paragraph| {
        let o = on(|o| {
            o.pause_punctuation = true;
            o.pause_sentence_ms = sentence;
            o.pause_paragraph_ms = paragraph;
        });
        go_with(&[s], &o, "", &[], &[]).outcome
    };
    assert_eq!(go_p(800, 0), text("привет. Как дела. Хорошо"));
    // A gap of exactly the threshold counts.
    assert_eq!(go_p(1000, 0), text("привет. Как дела. Хорошо"));
    assert_eq!(go_p(1001, 0), text("привет как дела. Хорошо"));
    assert_eq!(go_p(800, 2000), text("привет. Как дела.\n\nХорошо"));
    // Off: the segment as it came.
    assert_eq!(
        go_with(&[s], &TextOptions::default(), "", &[], &[]).outcome,
        text("привет как дела хорошо")
    );
}

#[test]
fn pauses_between_segments_use_the_segment_times_and_keep_marks() {
    let first = Seg {
        t0: Millis(0),
        t1: Millis(1000),
        ..seg("Привет,")
    };
    let second = Seg {
        t0: Millis(2500),
        t1: Millis(3000),
        ..seg("как дела?")
    };
    let third = Seg {
        t0: Millis(3050),
        t1: Millis(3500),
        ..seg("нормально")
    };
    let o = on(|o| o.pause_punctuation = true);
    // The comma becomes a full stop; a question mark stays; a short gap adds nothing.
    assert_eq!(
        go_with(&[first, second, third], &o, "", &[], &[]).outcome,
        text("Привет. Как дела? нормально")
    );
}

#[test]
fn a_segment_whose_words_do_not_line_up_is_one_piece() {
    let words = [word("привет", 0.9, 0, 400)];
    let s = Seg {
        words: &words,
        ..seg("привет мир")
    };
    let o = on(|o| o.pause_punctuation = true);
    assert_eq!(go_with(&[s], &o, "", &[], &[]).outcome, text("привет мир"));
}

/// A segment of one word of confidence `p` in a window of `no_speech_prob`.
fn conf_seg<'a>(text: &'a str, words: &'a [WordIn<'a>], no_speech_prob: f32) -> Seg<'a> {
    Seg {
        text,
        t0: Millis(100),
        t1: Millis(900),
        no_speech_prob,
        words,
    }
}

fn verdict(p: f32, no_speech_prob: f32, min: f32) -> Option<HeldWhy> {
    let words = [word("x", p, 0, 1)];
    hold(&conf_seg("x", &words, no_speech_prob), min)
}

#[test]
fn the_confidence_table() {
    use HeldWhy::{LowConfidence, NoSpeech};
    let table: [(f32, f32, f32, Option<HeldWhy>); 13] = [
        (0.9, 0.8, 0.35, None),
        (0.5, 0.8, 0.35, Some(NoSpeech)),
        (0.5, 0.5, 0.35, None),
        (0.2, 0.1, 0.35, Some(LowConfidence)),
        (0.2, 0.8, 0.35, Some(LowConfidence)),
        (0.9, 0.1, 0.35, None),
        (0.35, 0.1, 0.35, None),
        (0.59, 0.6, 0.35, Some(NoSpeech)),
        (0.6, 0.6, 0.35, None),
        (0.7, 0.1, 0.8, Some(LowConfidence)),
        (0.7, 0.8, 0.8, Some(LowConfidence)),
        (0.5, 0.59, 0.35, None),
        (0.34, 0.0, 0.35, Some(LowConfidence)),
    ];
    for (p, nsp, min, want) in table {
        assert_eq!(
            verdict(p, nsp, min),
            want,
            "p {p} no_speech {nsp} min {min}"
        );
    }
    // No word data: no evidence, kept.
    for nsp in [0.1, 0.9] {
        assert_eq!(hold(&conf_seg("x", &[], nsp), 0.35), None);
    }
}

fn filtered(segs: &[Seg<'_>], can_hold: bool) -> Processed {
    let o = on(|o| {
        o.confidence_filter = true;
        o.can_hold = can_hold;
    });
    go_with(segs, &o, "", &[], &[])
}

#[test]
fn a_whole_doubtful_window_of_good_words_is_pasted() {
    let good = [word("a", 0.9, 0, 1)];
    let mid = [word("b", 0.5, 0, 1)];
    let (t1, t2, t3) = ("one", "two", "three");
    let segs = [
        conf_seg(t1, &good, 0.62),
        conf_seg(t2, &good, 0.62),
        conf_seg(t3, &good, 0.62),
    ];
    let p = filtered(&segs, true);
    assert_eq!(p.outcome, text("one two three"));
    assert_eq!(p.held, []);
    // The same window with the middle segment at 0.5: only it is held, with its text and times.
    let segs = [
        conf_seg(t1, &good, 0.62),
        conf_seg(t2, &mid, 0.62),
        conf_seg(t3, &good, 0.62),
    ];
    let p = filtered(&segs, true);
    assert_eq!(p.outcome, text("one three"));
    assert_eq!(
        p.held,
        vec![Held {
            text: "two".into(),
            t0: Millis(100),
            t1: Millis(900),
            why: HeldWhy::NoSpeech,
        }]
    );
    assert_eq!(p.raw, "one two three");
}

#[test]
fn when_everything_is_held_nothing_is_pasted_and_nothing_is_lost() {
    let low = [word("a", 0.1, 0, 1)];
    let segs = [conf_seg("first", &low, 0.0), conf_seg("second", &low, 0.0)];
    let p = filtered(&segs, true);
    assert_eq!(p.outcome, Outcome::NothingHeard);
    let texts: Vec<&str> = p.held.iter().map(|h| h.text.as_str()).collect();
    assert_eq!(texts, ["first", "second"]);
    assert_eq!(p.raw, "first second");
}

#[test]
fn nothing_is_held_when_the_filter_is_off_or_there_is_no_place_to_keep_it() {
    let low = [word("a", 0.1, 0, 1)];
    let segs = [conf_seg("first", &low, 0.9), conf_seg("second", &low, 0.9)];
    let want = text("first second");
    assert_eq!(filtered(&segs, false).outcome, want);
    assert_eq!(filtered(&segs, false).held, []);
    let off = on(|o| o.can_hold = true);
    let p = go_with(&segs, &off, "", &[], &[]);
    assert_eq!((p.outcome, p.held.len()), (want, 0));
}

#[test]
fn the_hallucination_list_counts_its_drops_and_the_rest_is_kept() {
    let segs = [
        seg("Hello"),
        seg("Thanks for watching"),
        seg("  "),
        seg("world"),
    ];
    let p = go_with(&segs, &TextOptions::default(), "", &[], &[]);
    assert_eq!(p.outcome, text("Hello world"));
    assert_eq!(p.dropped_hallucination, 1);
    assert_eq!(p.raw, "Hello world");
}

#[test]
fn a_short_blip_that_came_back_as_a_long_phrase_is_not_pasted() {
    let clip = |ms| on(|o| o.clip_ms = Some(ms));
    let invented = "Спасибо большое всем за внимание и до новых встреч";
    let p = go_with(&[seg(invented)], &clip(100), "", &[], &[]);
    assert_eq!(p.outcome, Outcome::NothingHeard);
    assert_eq!(p.dropped_implausible, 1);
    // No entry to keep it in: only a count is reported, and nothing else changes.
    assert_eq!(p.dropped_hallucination, 0);
    assert_eq!(p.raw, invented);
    // A short real word on a tap is kept.
    assert_eq!(
        go_with(&[seg("Да.")], &clip(100), "", &[], &[]).outcome,
        text("Да.")
    );
    assert_eq!(
        go_with(&[seg("Да, хорошо")], &clip(100), "", &[], &[]).outcome,
        text("Да, хорошо")
    );
    // A full-speed sentence of 30 characters in a second is possible speech.
    let line = "Раз два три четыре пять шесть."; // 30 chars
    assert_eq!(
        go_with(&[seg(line)], &clip(1000), "", &[], &[]).outcome,
        text(line)
    );
    // The same text in 400 ms is not.
    assert_eq!(
        go_with(&[seg(line)], &clip(400), "", &[], &[]).outcome,
        Outcome::NothingHeard
    );
    // A long clip is never checked, and neither is a call with no length.
    assert_eq!(
        go_with(&[seg(invented)], &clip(5000), "", &[], &[]).outcome,
        text(invented)
    );
    assert_eq!(
        go_with(&[seg(invented)], &TextOptions::default(), "", &[], &[]).outcome,
        text(invented)
    );
}

/// Review d03 #1, #2: with an entry to keep it in, an invented-looking text is held, not lost.
#[test]
fn a_likely_hallucination_is_held_when_an_entry_keeps_it() {
    let keep = |ms| {
        on(|o| {
            o.clip_ms = ms;
            o.can_hold = true;
        })
    };
    let said = "Хорошо, спасибо большое за всё";
    let p = go_with(&[seg(said)], &keep(Some(300)), "", &[], &[]);
    assert_eq!(p.outcome, Outcome::NothingHeard);
    assert_eq!((p.dropped_implausible, p.raw.as_str()), (0, said));
    assert_eq!(p.held.len(), 1);
    assert_eq!(
        (p.held[0].text.as_str(), p.held[0].why),
        (said, HeldWhy::Hallucination)
    );
    // A listed phrase between real speech: the rest is pasted, the phrase is held.
    let mut segs = [seg("Hello"), seg("Thanks for watching"), seg("world")];
    segs[1].t0 = Millis(1000);
    let p = go_with(&segs, &keep(None), "", &[], &[]);
    assert_eq!(p.outcome, text("Hello world"));
    assert_eq!(p.dropped_hallucination, 0);
    let held: Vec<_> = p.held.iter().map(|h| (h.text.as_str(), h.t0)).collect();
    assert_eq!(held, [("Thanks for watching", Millis(1000))]);
    // Held by the list and by the confidence filter: in the order they were said.
    let low = [word("a", 0.1, 0, 1)];
    let mut segs = [seg("Thanks for watching"), conf_seg("quiet", &low, 0.0)];
    segs[0].t0 = Millis(5000);
    let o = on(|o| {
        o.can_hold = true;
        o.confidence_filter = true;
    });
    let p = go_with(&segs, &o, "", &[], &[]);
    let order: Vec<_> = p.held.iter().map(|h| h.why).collect();
    assert_eq!(order, [HeldWhy::LowConfidence, HeldWhy::Hallucination]);
}

/// Review d03-fix #3: `hold_invented` holds only what the hallucination checks take out; the
/// confidence filter still needs `can_hold`, so the pasted text stays the same.
#[test]
fn hold_invented_holds_the_inventions_and_nothing_the_confidence_filter_would() {
    let o = on(|o| {
        o.hold_invented = true;
        o.confidence_filter = true;
        o.clip_ms = Some(300);
    });
    let said = "Хорошо, спасибо большое за всё";
    let p = go_with(&[seg(said)], &o, "", &[], &[]);
    assert_eq!(
        (p.outcome, p.dropped_implausible),
        (Outcome::NothingHeard, 0)
    );
    assert_eq!(p.held.len(), 1);
    let low = [word("a", 0.1, 0, 1)];
    let segs = [seg("Thanks for watching"), conf_seg("quiet", &low, 0.0)];
    let o = on(|o| {
        o.hold_invented = true;
        o.confidence_filter = true;
    });
    let p = go_with(&segs, &o, "", &[], &[]);
    assert_eq!(p.outcome, text("quiet"));
    let why: Vec<_> = p.held.iter().map(|h| h.why).collect();
    assert_eq!(
        (why, p.dropped_hallucination),
        (vec![HeldWhy::Hallucination], 0)
    );
}

/// Review d03 #1, #2: Transcribe again is the user asking for this clip: nothing is taken out.
#[test]
fn transcribe_again_keeps_what_the_hallucination_checks_would_take() {
    let again = on(|o| {
        o.clip_ms = Some(300);
        o.again = true;
    });
    let said = "Хорошо, спасибо большое за всё";
    assert_eq!(
        go_with(&[seg(said)], &again, "", &[], &[]).outcome,
        text(said)
    );
    let p = go_with(&[seg("Thanks for watching")], &again, "", &[], &[]);
    assert_eq!(p.outcome, text("Thanks for watching"));
    assert_eq!((p.dropped_hallucination, p.held.len()), (0, 0));
}

#[test]
fn the_implausible_rate_border() {
    // 100 ms allows the floor; 1000 ms allows 40 characters; 1500 ms the last checked length.
    assert!(!implausible(100, MIN_CHARS_ALLOWED));
    assert!(implausible(100, MIN_CHARS_ALLOWED + 1));
    assert!(!implausible(1000, 40));
    assert!(implausible(1000, 41));
    assert!(!implausible(RATE_CLIP_MS, 60));
    assert!(implausible(RATE_CLIP_MS, 61));
    assert!(!implausible(RATE_CLIP_MS + 1, 10_000));
}

fn arbitrary_segments() -> impl Strategy<Value = Vec<String>> {
    proptest::collection::vec(
        prop_oneof![
            "[ a-zA-Zа-я.,!?\n\r]{0,24}",
            Just("Thanks for watching".to_owned()),
            Just("э новая строка".to_owned()),
        ],
        0..5,
    )
}

/// The pipeline as it was before d03 (commit 55e48bf), copied so the all-off check cannot pass
/// by comparing `run` with itself (review d03 #10).
fn pre_d03(
    segments: &[String],
    filter: &HallucinationFilter,
    rules: &ReplaceRules,
    trailing_space: bool,
) -> Outcome {
    let kept: Vec<String> = segments
        .iter()
        .map(|s| s.replace(['\n', '\r'], ""))
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty() && !filter.is_hallucination(s))
        .collect();
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

proptest! {
    // Every optional stage off (the lists are full, the switches are not): the pre-d03 pipeline,
    // through `run` and through `process`.
    #[test]
    fn all_off_equals_the_plain_pipeline(segs in arbitrary_segments(), trailing in any::<bool>()) {
        let filter = HallucinationFilter::default();
        let rules = ReplaceRules::parse("a => b\n").rules;
        let before = pre_d03(&segs, &filter, &rules, trailing);
        prop_assert_eq!(&process(&segs, &filter, &rules, trailing), &before);
        let all: Vec<Seg<'_>> = segs.iter().map(|s| seg(s)).collect();
        let opts = on(|o| o.trailing_space = trailing);
        let lists = Lists { hallucinations: &filter, replace: &rules, fillers: &fillers(), snippets: &[] };
        prop_assert_eq!(run(&all, &opts, &lists).outcome, before);
    }

    #[test]
    fn the_output_never_holds_a_cr_and_a_typed_one_never_a_newline(
        segs in arbitrary_segments(),
        newlines in any::<bool>(),
    ) {
        let all: Vec<Seg<'_>> = segs.iter().map(|s| seg(s)).collect();
        let opts = on(|o| {
            o.filler_filter = true;
            o.voice_commands = true;
            o.trailing_space = true;
            o.newlines_allowed = newlines;
        });
        let snippets = [snip("a", "x\r\ny")];
        let p = go_with(&all, &opts, "", &fillers(), &snippets);
        if let Outcome::Text(t) = p.outcome {
            prop_assert!(!t.contains('\r'));
            prop_assert!(newlines || !t.contains('\n'), "{t:?}");
        }
    }

    // With the filter on, what is in the text plus what is held is everything that passed step 2.
    #[test]
    fn nothing_vanishes_in_the_confidence_step(
        ps in proptest::collection::vec((0.0_f32..1.0, 0.0_f32..1.0), 0..6),
    ) {
        let texts: Vec<String> = (0..ps.len()).map(|i| format!("w{i}")).collect();
        let words: Vec<[WordIn<'static>; 1]> =
            ps.iter().map(|&(p, _)| [word("x", p, 0, 1)]).collect();
        let segs: Vec<Seg<'_>> = texts.iter().zip(&words).zip(&ps)
            .map(|((t, w), &(_, nsp))| conf_seg(t, w, nsp)).collect();
        let p = filtered(&segs, true);
        let kept = match &p.outcome {
            Outcome::Text(t) => t.split_whitespace().count(),
            Outcome::NothingHeard => 0,
        };
        prop_assert_eq!(kept + p.held.len(), segs.len());
        prop_assert_eq!(p.raw.split_whitespace().count(), segs.len());
    }
}
