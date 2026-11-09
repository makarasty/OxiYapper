//! Usage statistics: dates, word count, streaks, heatmap, file format.

// One module, so `cargo test -p oxi-core stats` selects every test here.
mod stats {
    use oxi_core::stats::{
        Date, Day, KEEP_DAYS, Metric, Stats, TREND_DAYS, Totals, VERSION, audio_length,
        format_count, word_count,
    };
    use oxi_core::types::Millis;
    use proptest::prelude::*;

    fn date(y: i32, m: u32, d: u32) -> Date {
        Date::from_ymd(y, m, d).unwrap()
    }

    /// Stats with one dictation of `words` words and 1 s of audio on each of `dates`.
    fn used(dates: &[Date], words: u64) -> Stats {
        let mut s = Stats::default();
        for &d in dates {
            s.record(d, words, Millis(1000));
        }
        s
    }

    /// `len` consecutive days from `start`.
    fn run(start: Date, len: i32) -> Vec<Date> {
        (0..len).map(|i| start.add_days(i)).collect()
    }

    #[test]
    fn trend_without_a_dictation_has_no_days() {
        let t = Stats::default().trend(date(2026, 11, 8), Metric::Words);
        assert_eq!(t.values, [] as [u64; 0]);
        assert_eq!(t.peak(), 0);
        assert_eq!(t.heights(), [] as [i32; 0]);
    }

    #[test]
    fn trend_of_one_day_is_one_full_bar() {
        let today = date(2026, 11, 8);
        let t = used(&[today], 7).trend(today, Metric::Words);
        assert_eq!((t.start, t.values.clone()), (today, vec![7]));
        assert_eq!(t.heights(), [1000]);
    }

    #[test]
    fn trend_keeps_the_days_without_a_dictation_up_to_today() {
        let today = date(2026, 11, 8);
        let s = used(&[today.add_days(-3), today.add_days(-1)], 10);
        let t = s.trend(today, Metric::Dictations);
        assert_eq!(t.start, today.add_days(-3));
        assert_eq!(t.values, [1, 0, 1, 0]);
    }

    #[test]
    fn trend_of_a_long_history_stops_at_a_year() {
        let today = date(2026, 11, 8);
        let s = used(&run(today.add_days(-500), 501), 3);
        let t = s.trend(today, Metric::Words);
        assert_eq!(t.values.len(), usize::try_from(TREND_DAYS).unwrap());
        assert_eq!(t.start, today.add_days(1 - TREND_DAYS));
    }

    #[test]
    fn trend_streak_counts_the_run_from_before_the_window() {
        let today = date(2026, 11, 8);
        let s = used(&run(today.add_days(-400), 401), 3);
        let t = s.trend(today, Metric::Streak);
        assert_eq!(t.values.last(), Some(&401));
        assert_eq!(t.values[0], 401 - u64::try_from(TREND_DAYS - 1).unwrap());
    }

    #[test]
    fn trend_streak_restarts_after_a_gap() {
        let today = date(2026, 11, 8);
        let days = [-4, -3, -1, 0].map(|d| today.add_days(d));
        assert_eq!(
            used(&days, 1).trend(today, Metric::Streak).values,
            [1, 2, 0, 1, 2]
        );
    }

    #[test]
    fn trend_wpm_is_that_days_pace_and_zero_without_audio() {
        let today = date(2026, 11, 8);
        let mut s = Stats::default();
        s.record(today.add_days(-1), 100, Millis(0));
        s.record(today, 150, Millis(60_000));
        assert_eq!(s.trend(today, Metric::Wpm).values, [0, 150]);
    }

    #[test]
    fn trend_ignores_days_after_today() {
        let today = date(2026, 11, 8);
        let s = used(&[today.add_days(5)], 9);
        assert_eq!(s.trend(today, Metric::Words).values, [] as [u64; 0]);
    }

    #[test]
    fn trend_heights_survive_a_huge_count() {
        let today = date(2026, 11, 8);
        let mut s = used(&[today.add_days(-1)], u64::MAX);
        s.record(today, u64::MAX / 2, Millis(1000));
        assert_eq!(s.trend(today, Metric::Words).heights(), [1000, 499]);
    }

    #[test]
    fn trend_heights_keep_a_tiny_day_visible() {
        let table: [(&[u64], &[i32]); 5] = [
            (&[2000, 1], &[1000, 1]),
            (&[3, 2000, 0], &[1, 1000, 0]),
            (&[500, 1000], &[500, 1000]),
            (&[0, 0], &[0, 0]),
            (&[u64::MAX, 1], &[1000, 1]),
        ];
        for (values, want) in table {
            let trend = oxi_core::stats::Trend {
                start: date(2026, 11, 8),
                values: values.to_vec(),
            };
            assert_eq!(trend.heights(), want, "{values:?}");
        }
    }

    #[test]
    fn dates_known_days_and_weekdays() {
        assert_eq!(Date::from_epoch_days(0), date(1970, 1, 1));
        assert_eq!(date(1970, 1, 1).weekday(), 3); // Thursday
        assert_eq!(date(2026, 11, 7).weekday(), 5); // Saturday
        assert_eq!(date(2026, 11, 2).weekday(), 0); // Monday
        assert_eq!(date(2024, 2, 28).add_days(1), date(2024, 2, 29));
        assert_eq!(date(2026, 2, 28).add_days(1), date(2026, 3, 1));
        assert_eq!(date(2026, 12, 31).add_days(1), date(2027, 1, 1));
        assert_eq!(date(2026, 11, 7).to_string(), "2026-11-07");
        for bad in [
            (2026, 2, 29),
            (1900, 2, 29),
            (2026, 13, 1),
            (2026, 4, 31),
            (0, 1, 1),
        ] {
            assert_eq!(Date::from_ymd(bad.0, bad.1, bad.2), None, "{bad:?}");
        }
        assert_eq!(date(2000, 2, 29).ymd(), (2000, 2, 29));
        for bad in ["", "2026-11", "2026-02-30", "x-1-1", "2026-11-07-1"] {
            assert!(bad.parse::<Date>().is_err(), "{bad}");
        }
    }

    proptest! {
        /// Every day of 1900..2200 survives ymd and text round trips, and the next day is
        /// the next calendar day.
        #[test]
        fn date_round_trips(days in -25_567..84_000i32) {
            let d = Date::from_epoch_days(days);
            let (y, m, dd) = d.ymd();
            prop_assert_eq!(Date::from_ymd(y, m, dd), Some(d));
            prop_assert_eq!(d.to_string().parse::<Date>(), Ok(d));
            prop_assert_eq!(d.add_days(1).weekday(), (d.weekday() + 1) % 7);
            let (y2, m2, d2) = d.add_days(1).ymd();
            prop_assert!((y2, m2, d2) > (y, m, dd));
            prop_assert!(d2 == dd + 1 || d2 == 1);
        }

        /// A run of `len` days ending today or yesterday is the current streak, wherever it
        /// sits: month, year and leap-day ends, DST change days (dates are local calendar
        /// days, so a 23 or 25 hour day is still one day).
        #[test]
        fn streak_of_any_run(start in 10_957..47_482i32, len in 1..80i32, gap in 0..3i32) {
            let start = Date::from_epoch_days(start);
            let last = start.add_days(len - 1);
            let s = used(&run(start, len), 3).summary(last.add_days(gap), 1);
            let current = if gap <= 1 { u32::try_from(len).unwrap() } else { 0 };
            prop_assert_eq!(s.streak, current);
            prop_assert_eq!(s.longest_streak, u32::try_from(len).unwrap());
        }

        /// Two runs split by at least one empty day: the current streak is the second run,
        /// the longest the longer one.
        #[test]
        fn streak_two_runs(start in 10_957..47_482i32, a in 1..40i32, gap in 2..10i32, b in 1..40i32) {
            let first = Date::from_epoch_days(start);
            let second = first.add_days(a - 1 + gap);
            let mut dates = run(first, a);
            dates.extend(run(second, b));
            let today = second.add_days(b - 1);
            let s = used(&dates, 1).summary(today, 1);
            prop_assert_eq!(s.streak, u32::try_from(b).unwrap());
            prop_assert_eq!(s.longest_streak, u32::try_from(a.max(b)).unwrap());
        }
    }

    #[test]
    fn streak_across_dst_and_month_ends() {
        // EU DST starts 2026-03-29 and ends 2026-10-25; US 2026-03-08 and 2026-11-01.
        for (y, m, d) in [
            (2026, 3, 27),
            (2026, 10, 23),
            (2026, 3, 6),
            (2026, 10, 30),
            (2024, 2, 27),
        ] {
            let start = date(y, m, d);
            let s = used(&run(start, 5), 1).summary(start.add_days(4), 1);
            assert_eq!((s.streak, s.longest_streak), (5, 5), "{start}");
        }
    }

    #[test]
    fn streak_today_yesterday_and_broken() {
        let today = date(2026, 11, 7);
        let s = |dates: &[Date]| {
            let s = used(dates, 1).summary(today, 1);
            (s.streak, s.longest_streak)
        };
        assert_eq!(s(&[]), (0, 0));
        assert_eq!(s(&[today]), (1, 1));
        assert_eq!(
            s(&run(today.add_days(-2), 2)),
            (2, 2),
            "ends yesterday: still on"
        );
        assert_eq!(s(&run(today.add_days(-3), 2)), (0, 2), "ended two days ago");
        // A day the clock went past (a date after today) does not extend today's streak.
        assert_eq!(s(&[today.add_days(-1), today.add_days(1)]), (1, 1));
    }

    #[test]
    fn word_count_table() {
        for (text, n) in [
            ("", 0),
            ("hello world", 2),
            ("Привет, мир!", 2),
            ("don't stop", 2),
            ("кто-то пришёл", 2),
            ("— ... ! -", 0),
            ("5 apples, 10 груш", 4),
            ("OK, давай go.", 3),
            ("  tabs\tand\nnewlines  ", 3),
            ("e-mail и т.д.", 4),
        ] {
            assert_eq!(word_count(text), n, "{text:?}");
        }
    }

    #[test]
    fn audio_length_at_16k() {
        assert_eq!(audio_length(0), Millis(0));
        assert_eq!(audio_length(8_000), Millis(500));
        assert_eq!(audio_length(16_000 * 61), Millis(61_000));
    }

    #[test]
    fn format_count_table() {
        for (n, text) in [
            (0, "0"),
            (146, "146"),
            (999, "999"),
            (1_000, "1.0K"),
            (11_300, "11.3K"),
            (299_900, "299.9K"),
            (299_999, "299.9K"),
            (999_999, "999.9K"),
            (1_000_000, "1.0M"),
            (1_250_000, "1.2M"),
        ] {
            assert_eq!(format_count(n), text, "{n}");
        }
    }

    #[test]
    fn totals_and_wpm() {
        let today = date(2026, 11, 7);
        let mut s = Stats::default();
        s.record(today.add_days(-1), 100, Millis(30_000));
        s.record(today, 50, Millis(30_000));
        s.record(today, 0, Millis(0)); // punctuation only: counted, no words
        let sum = s.summary(today, 1);
        assert_eq!((sum.dictations, sum.words, sum.wpm), (3, 150, 150));
        assert_eq!(s.days.len(), 2, "one row per day");

        // A day without audio counts its words but not in the per-minute rate.
        s.days.push(Day {
            date: today.add_days(1),
            n: Totals {
                dictations: 1,
                words: 1_000,
                speech_ms: 0,
            },
        });
        let sum = s.summary(today.add_days(1), 1);
        assert_eq!((sum.words, sum.wpm), (1_150, 150));
        assert_eq!(Stats::default().summary(today, 1).wpm, 0);
    }

    #[test]
    fn old_rows_fold_into_before_and_totals_stay() {
        let today = date(2026, 11, 7);
        let mut s = Stats::default();
        s.record(today.add_days(-KEEP_DAYS - 10), 7, Millis(1000));
        s.record(today.add_days(-KEEP_DAYS + 1), 5, Millis(1000));
        s.record(today, 3, Millis(1000)); // far after the last row: folds nothing yet
        assert_eq!(s.days.len(), 3);
        s.record(today, 0, Millis(0));
        assert_eq!(s.days.len(), 2);
        assert_eq!(s.before.words, 7);
        let sum = s.summary(today, 1);
        assert_eq!((sum.dictations, sum.words), (4, 15));
    }

    #[test]
    fn a_wrong_future_clock_folds_no_history() {
        let today = date(2026, 11, 7);
        let mut s = used(&[today.add_days(-KEEP_DAYS + 5), today], 4);
        s.record(today.add_days(2 * KEEP_DAYS), 1, Millis(1000)); // clock set years ahead
        assert_eq!((s.days.len(), s.before), (3, Totals::default()));
        s.record(today.add_days(1), 1, Millis(1000)); // clock fixed; the future row stays
        assert_eq!(s.days.len(), 4);
        assert_eq!(s.before, Totals::default());
        s.record(today.add_days(2), 1, Millis(1000));
        assert_eq!(s.before, Totals::default(), "the future row folds nothing");
    }

    #[test]
    fn newer_version_is_read_even_when_the_rest_does_not_parse() {
        let newer = format!(r#"{{"version":{},"days":{{"new":"shape"}}}}"#, VERSION + 1);
        assert!(Stats::parse(newer.as_bytes()).is_err());
        assert_eq!(Stats::newer_version(newer.as_bytes()), Some(VERSION + 1));
        assert_eq!(Stats::newer_version(br#"{"version":1,"days":7}"#), None);
        assert_eq!(Stats::newer_version(b"{half"), None);
    }

    #[test]
    fn a_clock_that_went_back_keeps_rows_sorted() {
        let mut s = used(&[date(2026, 11, 7), date(2026, 11, 5)], 1);
        s.record(date(2026, 11, 7), 1, Millis(1));
        let dates: Vec<_> = s.days.iter().map(|d| d.date).collect();
        assert_eq!(dates, [date(2026, 11, 5), date(2026, 11, 7)]);
        assert_eq!(s.days[1].n.dictations, 2);
    }

    #[test]
    fn heatmap_starts_on_monday_and_levels_by_quartile() {
        let today = date(2026, 11, 7); // Saturday
        let s = Stats::default().summary(today, 26);
        assert_eq!(s.heat_start.weekday(), 0);
        assert_eq!(s.heat.len(), 25 * 7 + 6);
        assert_eq!(
            s.heat_start
                .add_days(i32::try_from(s.heat.len()).unwrap() - 1),
            today
        );
        assert!(s.heat.iter().all(|&l| l == 0));

        let mut st = Stats::default();
        for (back, words) in [(0, 40), (1, 30), (2, 20), (3, 10), (400, 1_000)] {
            st.record(today.add_days(-back), words, Millis(1000));
        }
        let s = st.summary(today, 1); // this week: Monday 2026-11-02 .. today
        assert_eq!(s.heat_start, date(2026, 11, 2));
        assert_eq!(s.heat, [0, 0, 1, 2, 3, 4]);

        let s = used(&run(date(2026, 11, 3), 3), 5).summary(today, 1);
        assert_eq!(s.heat, [0, 1, 1, 1, 0, 0], "equal days share level 1");
        assert_eq!(Stats::default().summary(today, 0).heat, Vec::<u8>::new());
    }

    #[test]
    fn file_round_trips_merges_rows_and_keeps_counts_only() {
        let mut s = Stats::default();
        s.record(date(2026, 11, 6), 12, Millis(4_500));
        s.record(date(2026, 11, 7), 3, Millis(900));
        let json = s.to_json().unwrap();
        assert_eq!(Stats::parse(&json).unwrap(), s);
        insta::assert_snapshot!("stats_file", String::from_utf8(json).unwrap());

        let messy = br#"{"version":1,"days":[
            {"date":"2026-11-07","dictations":1,"words":2,"speech_ms":3},
            {"date":"2026-11-05","dictations":1,"words":1,"speech_ms":1},
            {"date":"2026-11-07","dictations":2,"words":2,"speech_ms":2}]}"#;
        let s = Stats::parse(messy).unwrap();
        let rows: Vec<_> = s.days.iter().map(|d| (d.date, d.n.dictations)).collect();
        assert_eq!(rows, [(date(2026, 11, 5), 1), (date(2026, 11, 7), 3)]);
        assert_eq!(s.before, Totals::default());

        assert!(Stats::parse(b"{not json").is_err());
        assert!(Stats::parse(br#"{"version":1,"days":[{"date":"2026-02-30"}]}"#).is_err());
        let newer = Stats::parse(br#"{"version":99,"days":[],"extra":1}"#).unwrap();
        assert!(!newer.writable());
        assert!(Stats::default().writable());
        assert_eq!(Stats::default().version, VERSION);
    }
}
