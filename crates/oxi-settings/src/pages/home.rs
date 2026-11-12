//! Home page: the usage numbers and the speaking-activity heatmap. The daemon answers
//! `Request::Stats` while it is online; otherwise the page reads the stats file itself, read-only.

use std::cell::{Cell, RefCell};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use oxi_core::config::History;
use oxi_core::history;
use oxi_core::ipc::{Push, Request};
use oxi_core::machine::Mode;
use oxi_core::stats::{
    self, Date, HEAT_WEEKS, Metric, Stats, Summary, Totals, Trend, format_count,
};
use oxi_platform::stats_store;
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::constants::FOLDER_POLL;
use crate::generated::{HomeState, MonthLabel, Shell, Toast};
use crate::ipc::{Event, Link};
use crate::{AppWindow, Ctx, Error, Page, report};

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
/// What each stat card charts, in the order of the cards.
const METRICS: [Metric; 4] = [
    Metric::Words,
    Metric::Wpm,
    Metric::Dictations,
    Metric::Streak,
];
/// The shortest chart: a history of a few days is drawn as the last days of a month, not as a
/// few bars alone at the right edge.
const MIN_DAYS: usize = 30;
/// The bars grow in left to right within this (UI rule 21): each over `GROW`, the last one
/// starting at `REVEAL - GROW`.
const REVEAL: Duration = Duration::from_millis(200);
const GROW: Duration = Duration::from_millis(120);
const FRAME: Duration = Duration::from_millis(16);
const HISTORY_OFF: &str = "History is off, nothing new is saved";
const HISTORY_EMPTY: &str = "No dictations saved yet";

/// What the page showed last: the stats file's time and the day the heatmap was cut for. A new
/// day changes the window and the streak with no file change.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Stamp {
    modified: Option<SystemTime>,
    day: Date,
}

#[derive(Default)]
struct Shown {
    /// `Stamp::default()` asks for another look: no real stamp equals it.
    stamp: Cell<Stamp>,
    /// The file as last read: a card opened while it is locked still draws its own bars.
    stats: RefCell<Option<Stats>>,
    /// The chart's bars as drawn; one model for the page's life, so a reveal frame changes rows.
    bars: Rc<VecModel<i32>>,
    /// The bars' full heights, which a reveal grows towards.
    target: RefCell<Vec<i32>>,
    /// When the running reveal started.
    since: Cell<Option<Instant>>,
    reveal: Timer,
    /// The last summary, a year of heatmap: the page shows its last half.
    summary: RefCell<Option<Summary>>,
}

type Seen = Rc<Shown>;

pub fn bind(window: &AppWindow, ctx: &Ctx) {
    let seen = Seen::default();
    window
        .global::<HomeState>()
        .set_trend(ModelRc::from(seen.bars.clone()));
    // The file first: numbers on screen before the link is up. Asking the daemon waits for it.
    let now = stamp(ctx);
    if load(window, ctx, &seen) {
        seen.stamp.set(now);
    }

    let (w, c, s) = (window.as_weak(), ctx.clone(), seen.clone());
    let events = ctx.ipc.subscribe(move |event| {
        let Some(window) = w.upgrade() else {
            return;
        };
        match event {
            Event::Push(Push::Stats(summary)) => {
                // The daemon's summary has no per-day counts: the day tips come from the file.
                // File locked: the heatmap and its tips stay as they were, both from the last
                // read, and the next poll asks again.
                let fresh = read_stats(&c);
                let keep_heat = fresh.is_none() && s.stats.borrow().is_some();
                if let Some(stats) = fresh {
                    *s.stats.borrow_mut() = Some(stats);
                } else if keep_heat {
                    s.stamp.set(Stamp::default());
                }
                show(&window, &s, summary, keep_heat);
            }
            Event::Link(_) if window.get_page() == Page::Home => refresh(&window, &c, &s, false),
            // The window got the focus or the machine woke: the stats file may have changed.
            Event::Push(Push::Woke)
                if window.get_page() == Page::Home && stamp(&c) != s.stamp.get() =>
            {
                refresh(&window, &c, &s, false);
            }
            _ => {}
        }
    });

    // A finished dictation arrives as `Push::Stats` (the daemon sends it after each write): no timer.
    let (w, c, s) = (window.as_weak(), ctx.clone(), seen.clone());
    window.global::<HomeState>().on_shown(move || {
        // Held by this callback: a rebind (Reset) replaces it and so stops them.
        let _ = &events;
        if let Some(window) = w.upgrade() {
            refresh(&window, &c, &s, true);
        }
    });

    let (w, s) = (window.as_weak(), seen.clone());
    window.global::<HomeState>().on_fit(move || {
        if let Some(window) = w.upgrade() {
            show_heat(&window, &s);
        }
    });

    let (w, c) = (window.as_weak(), ctx.clone());
    window.global::<HomeState>().on_pick(move |card| {
        if let Some(window) = w.upgrade() {
            let s = window.global::<HomeState>();
            s.set_selected(if s.get_selected() == card { -1 } else { card });
            if !show_trend(&window, &c, &seen, true) {
                seen.stamp.set(Stamp::default());
            }
        }
    });

    // shortcut: History cannot filter by day yet; a click on a day opens it unfiltered.
    let w = window.as_weak();
    window.global::<HomeState>().on_open_day(move || {
        if let Some(window) = w.upgrade() {
            window.set_page(Page::History);
        }
    });

    let (w, c) = (window.as_weak(), ctx.clone());
    let check = Rc::new(Timer::default());
    window.global::<HomeState>().on_open_history(move || {
        // A slow path is still answering: one check at a time.
        if check.running() {
            return;
        }
        let cfg = c.config.borrow().config.history.clone();
        let dir = history::folder(&cfg, &c.layout.history_dir());
        let data_dir = c.layout.data_dir.clone();
        let (tx, rx) = mpsc::channel();
        // A UNC path to an offline host can block for a long time: the window must not freeze.
        thread::spawn(move || {
            // The page is gone when the window closed first.
            let _ = tx.send(history_target(&cfg, &dir, &data_dir));
        });
        let (w, stop) = (w.clone(), Rc::downgrade(&check));
        check.start(TimerMode::Repeated, FOLDER_POLL, move || {
            let target = match rx.try_recv() {
                Err(TryRecvError::Empty) => return,
                answer => answer.ok().flatten(),
            };
            if let Some(check) = stop.upgrade() {
                check.stop();
            }
            open_target(&w, target);
        });
    });
}

/// A folder to open and the note that goes with it.
type Target = Option<(PathBuf, Option<&'static str>)>;

/// What the History folder button opens: `dir`, else its parent (the data directory or the chosen
/// folder), else the data directory. Creates nothing: a folder made here would lack the app's
/// marker and later block saving. Touches the disk, so it runs off the UI thread.
fn history_target(cfg: &History, dir: &Path, data_dir: &Path) -> Target {
    let found = [Some(dir), dir.parent(), Some(data_dir)]
        .into_iter()
        .flatten()
        .find(|p| p.is_dir())?;
    let note = if !cfg.enabled {
        Some(HISTORY_OFF)
    } else if found == dir {
        None
    } else {
        Some(HISTORY_EMPTY)
    };
    Some((found.to_owned(), note))
}

/// shortcut: the shell call stays on the UI thread (it wants COM, which this thread has) and a
/// host dropping right after the check could still stall it; move it once `open` sets up COM.
fn open_target(w: &slint::Weak<AppWindow>, target: Target) {
    let opened = target
        .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        .and_then(|(path, note)| {
            oxi_platform::open::open(&path)?;
            if let (Some(note), Some(window)) = (note, w.upgrade()) {
                window.global::<Toast>().invoke_show(note.into());
            }
            Ok(())
        });
    report(w, opened.map_err(Error::from));
}

/// New numbers: from the daemon while it answers, else from the file. `grow`: the page is being
/// shown, so an open chart's bars grow in.
fn refresh(window: &AppWindow, ctx: &Ctx, seen: &Seen, grow: bool) {
    let now = stamp(ctx);
    let mut read = show_trend(window, ctx, seen, grow);
    if ctx.ipc.link() == Link::Online {
        push_config(window, ctx);
        ctx.ipc.send(Request::Stats);
    } else {
        read &= load(window, ctx, seen);
    }
    seen.stamp.set(if read { now } else { Stamp::default() });
}

/// The page from the stats file; false when the file could not be read: the numbers on screen
/// stay and the next poll tries again.
fn load(window: &AppWindow, ctx: &Ctx, seen: &Seen) -> bool {
    push_config(window, ctx);
    let Some(stats) = read_stats(ctx) else {
        return false;
    };
    let summary = stats.summary(stats_store::today(), HEAT_WEEKS);
    *seen.stats.borrow_mut() = Some(stats);
    show(window, seen, &summary, false);
    true
}

/// The stats file; `None` when it could not be read (briefly locked). A corrupt file reads as
/// empty, as the daemon's own summary does; only the daemon may reset it.
fn read_stats(ctx: &Ctx) -> Option<Stats> {
    match stats_store::read(&ctx.layout.data_dir.join(stats::FILE)) {
        Ok(stats) => Some(stats),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => Some(Stats::default()),
        Err(_) => None,
    }
}

/// The open card's chart, from the file: the daemon's summary has no per-day numbers. False when
/// the file could not be read: the chart then comes from the last read and the next poll retries.
/// `grow`: the chart has just opened or changed card, so its bars grow in.
fn show_trend(window: &AppWindow, ctx: &Ctx, seen: &Seen, grow: bool) -> bool {
    let s = window.global::<HomeState>();
    let metric = usize::try_from(s.get_selected())
        .ok()
        .and_then(|card| METRICS.get(card));
    let Some(&metric) = metric else {
        return true;
    };
    let fresh = read_stats(ctx);
    let read = fresh.is_some();
    if read {
        *seen.stats.borrow_mut() = fresh;
    }
    if let Some(stats) = seen.stats.borrow().as_ref() {
        let today = stats_store::today();
        let trend = stats.trend(today, metric);
        let days = trend.values.len();
        let trend = padded(trend);
        let tips: Vec<SharedString> = tips(&trend, metric, today)
            .into_iter()
            .map(Into::into)
            .collect();
        s.set_trend_days(i32::try_from(days).unwrap_or(i32::MAX));
        s.set_trend_tips(ModelRc::from(Rc::new(VecModel::from(tips))));
        s.set_trend_peak(format_count(trend.peak()).into());
        s.set_trend_first(day_label(trend.start, today).into());
        s.set_trend_last(day_label(today, today).into());
        let grow = grow && !window.global::<Shell>().get_reduced_motion();
        set_bars(seen, trend.heights(), grow);
    }
    read
}

/// At least `MIN_DAYS` days, the missing ones empty before the first; no day stays none (the
/// page shows its empty state).
fn padded(trend: Trend) -> Trend {
    let pad = MIN_DAYS.saturating_sub(trend.values.len());
    if trend.values.is_empty() || pad == 0 {
        return trend;
    }
    let mut values = vec![0; pad];
    values.extend(trend.values);
    Trend {
        start: trend.start.add_days(-i32::try_from(pad).unwrap_or(0)),
        values,
    }
}

/// Each bar's tooltip: "Nov 8: 1,234 words".
fn tips(trend: &Trend, metric: Metric, today: Date) -> Vec<String> {
    let (one, many) = match metric {
        Metric::Words => ("word", "words"),
        Metric::Wpm => ("word per minute", "words per minute"),
        Metric::Dictations => ("dictation", "dictations"),
        Metric::Streak => ("day in a row", "days in a row"),
    };
    (0..)
        .zip(&trend.values)
        .map(|(i, &v)| {
            let unit = if v == 1 { one } else { many };
            let day = day_label(trend.start.add_days(i), today);
            format!("{day}: {} {unit}", grouped(v))
        })
        .collect()
}

/// A heatmap day's tooltip: "Nov 8 · 3 dictations · 120 words"; the bare day without its counts.
fn day_tip(date: Date, today: Date, totals: Option<Totals>) -> String {
    let day = day_label(date, today);
    let Some(t) = totals.filter(|t| t.dictations > 0) else {
        return day;
    };
    let s = |n: u64| if n == 1 { "" } else { "s" };
    format!(
        "{day} · {} dictation{} · {} word{}",
        grouped(t.dictations),
        s(t.dictations),
        grouped(t.words),
        s(t.words)
    )
}

/// `1234567` as "1,234,567": a tooltip shows the exact number, a card the short one.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// New full heights for the bars. `grow` starts a reveal from empty; without it a running reveal
/// carries on towards the new heights, else they show at once.
fn set_bars(seen: &Seen, heights: Vec<i32>, grow: bool) {
    let n = heights.len();
    *seen.target.borrow_mut() = heights;
    if grow {
        seen.since.set(Some(Instant::now()));
    } else if !seen.reveal.running() {
        seen.bars.set_vec(seen.target.borrow().clone());
        return;
    }
    if seen.bars.row_count() != n {
        seen.bars.set_vec(vec![0; n]);
    }
    if !seen.reveal.running() {
        let weak = Rc::downgrade(seen);
        seen.reveal.start(TimerMode::Repeated, FRAME, move || {
            if let Some(seen) = weak.upgrade()
                && reveal_frame(&seen)
            {
                seen.reveal.stop();
            }
        });
    }
    reveal_frame(seen);
}

/// Draws the reveal as it stands now; true once it is over.
fn reveal_frame(seen: &Shown) -> bool {
    let t = seen.since.get().map_or(REVEAL, |s| s.elapsed()).min(REVEAL);
    let target = seen.target.borrow();
    for (i, &h) in target.iter().enumerate() {
        seen.bars.set_row_data(i, revealed(h, i, target.len(), t));
    }
    t >= REVEAL
}

/// Bar `i` of `n` at `t` into the reveal: it starts `i / (n - 1)` of the way into
/// `REVEAL - GROW`, rises over `GROW` with an ease-out and stops at `height`, never above.
fn revealed(height: i32, i: usize, n: usize, t: Duration) -> i32 {
    let micros = |d: Duration| i64::try_from(d.as_micros()).unwrap_or(i64::MAX);
    let i = i64::try_from(i).unwrap_or(0);
    let last = i64::try_from(n.saturating_sub(1).max(1)).unwrap_or(1);
    let grow = micros(GROW);
    let lead = (micros(REVEAL) - grow) * i / last;
    let left = grow - (micros(t) - lead).clamp(0, grow);
    // 1 - (1 - p)^2 in millionths, p the share of `GROW` gone.
    let eased = 1_000_000 - left * left * 1_000_000 / (grow * grow);
    i32::try_from(i64::from(height) * eased / 1_000_000).unwrap_or(height)
}

/// "Nov 8"; with the year when it is not this year's.
fn day_label(date: Date, today: Date) -> String {
    let (y, m, d) = date.ymd();
    let month = MONTHS[m as usize - 1];
    if y == today.ymd().0 {
        format!("{month} {d}")
    } else {
        format!("{month} {d}, {y}")
    }
}

fn stamp(ctx: &Ctx) -> Stamp {
    stamp_of(&ctx.layout.data_dir.join(stats::FILE), stats_store::today())
}

fn stamp_of(file: &Path, day: Date) -> Stamp {
    Stamp {
        modified: fs::metadata(file).and_then(|m| m.modified()).ok(),
        day,
    }
}

/// The empty state names the hotkey and how to use it, and the disclaimer depends on History:
/// other pages may have just changed either.
fn push_config(window: &AppWindow, ctx: &Ctx) {
    let config = ctx.config.borrow();
    let s = window.global::<HomeState>();
    // The same key names the Hotkey page shows (UI rule 30), not the config's.
    s.set_chord(
        super::hotkey::labels(&config.config.hotkey.chord)
            .join(" + ")
            .into(),
    );
    s.set_toggle(config.config.hotkey.mode == Mode::Toggle);
    s.set_history_on(config.config.history.enabled);
}

/// `keep_heat`: the heatmap stays as it is, so it and its day tips are from one read.
fn show(window: &AppWindow, seen: &Shown, summary: &Summary, keep_heat: bool) {
    let s = window.global::<HomeState>();
    s.set_words(format_count(summary.words).into());
    s.set_wpm(format_count(u64::from(summary.wpm)).into());
    s.set_dictations(format_count(summary.dictations).into());
    s.set_streak(i32::try_from(summary.streak).unwrap_or(i32::MAX));
    s.set_longest_streak(i32::try_from(summary.longest_streak).unwrap_or(i32::MAX));
    s.set_empty(summary.dictations == 0);
    if !keep_heat {
        *seen.summary.borrow_mut() = Some(summary.clone());
        show_heat(window, seen);
    }
}

/// The last summary's heatmap, as many weeks as the card has room for.
fn show_heat(window: &AppWindow, seen: &Shown) {
    let int = |n: usize| i32::try_from(n).unwrap_or(i32::MAX);
    let Some(summary) = seen.summary.borrow().clone() else {
        return;
    };
    let s = window.global::<HomeState>();
    // Before the layout settles the room can be 0 or less: the fewest weeks.
    let (start, heat) = summary.heat_tail(u16::try_from(s.get_fit_weeks()).unwrap_or(0));
    s.set_active_days(int(heat.iter().filter(|&&l| l > 0).count()));
    // `start` is a Monday, so the cells fall in Monday-first columns as they come.
    let levels: Vec<i32> = heat.iter().map(|&l| i32::from(l)).collect();
    s.set_levels(ModelRc::from(Rc::new(VecModel::from(levels))));
    let today = stats_store::today();
    let tips: Vec<SharedString> = {
        let stats = seen.stats.borrow();
        (0..)
            .zip(heat)
            .map(|(i, _)| {
                let date = start.add_days(i);
                let totals = stats.as_ref().and_then(|st| {
                    let at = st.days.binary_search_by_key(&date, |d| d.date).ok()?;
                    Some(st.days[at].n)
                });
                day_tip(date, today, totals).into()
            })
            .collect()
    };
    s.set_day_tips(ModelRc::from(Rc::new(VecModel::from(tips))));
    let months = month_labels(start, int(heat.len().div_ceil(7)));
    s.set_months(ModelRc::from(Rc::new(VecModel::from(months))));
    s.set_today(today_cell(heat));
}

/// Today is the last cell: the window ends on it.
fn today_cell(heat: &[u8]) -> i32 {
    i32::try_from(heat.len()).map_or(i32::MAX, |n| n - 1)
}

/// A label over each week column whose Monday starts a new month. The first column keeps its
/// month unless the next one starts another: two labels side by side would touch.
fn month_labels(start: Date, weeks: i32) -> Vec<MonthLabel> {
    let month = |week| start.add_days(7 * week).ymd().1;
    (0..weeks)
        .filter(|&w| {
            if w == 0 {
                month(0) == month(1)
            } else {
                month(w) != month(w - 1)
            }
        })
        .map(|week| MonthLabel {
            week,
            text: MONTHS[month(week) as usize - 1].into(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use oxi_core::stats::{HEAT_WEEKS, Stats};
    use oxi_core::types::Millis;

    use std::time::Duration;

    use super::{
        Date, GROW, HISTORY_EMPTY, HISTORY_OFF, History, MIN_DAYS, Metric, REVEAL, Trend, grouped,
        history_target, month_labels, padded, revealed, stamp_of, tips, today_cell,
    };

    #[test]
    fn a_short_history_is_drawn_as_the_end_of_a_month() {
        let today = Date::from_ymd(2026, 11, 10).unwrap();
        let short = padded(Trend {
            start: today.add_days(-1),
            values: vec![5, 7],
        });
        assert_eq!(short.values.len(), MIN_DAYS);
        assert_eq!(&short.values[MIN_DAYS - 2..], &[5, 7]);
        assert!(short.values[..MIN_DAYS - 2].iter().all(|&v| v == 0));
        assert_eq!(short.start.add_days(29), today, "still ends today");
        let none = padded(Trend {
            start: today,
            values: vec![],
        });
        assert!(none.values.is_empty(), "no day stays no day");
        let year = padded(Trend {
            start: today.add_days(-364),
            values: vec![1; 365],
        });
        assert_eq!(year.values.len(), 365);
    }

    #[test]
    fn a_tip_names_the_day_and_the_exact_number() {
        let today = Date::from_ymd(2026, 11, 10).unwrap();
        let trend = Trend {
            start: Date::from_ymd(2025, 12, 31).unwrap(),
            values: vec![1, 1_234_567],
        };
        assert_eq!(
            tips(&trend, Metric::Words, today),
            ["Dec 31, 2025: 1 word", "Jan 1: 1,234,567 words"]
        );
        assert_eq!(
            tips(&trend, Metric::Streak, today)[0],
            "Dec 31, 2025: 1 day in a row"
        );
        for (n, want) in [
            (0, "0"),
            (999, "999"),
            (1000, "1,000"),
            (123_456, "123,456"),
        ] {
            assert_eq!(grouped(n), want);
        }
    }

    #[test]
    fn a_heatmap_day_tip_names_the_day_and_its_counts() {
        let today = Date::from_ymd(2026, 11, 10).unwrap();
        let day = Date::from_ymd(2026, 11, 8).unwrap();
        let totals = |dictations, words| {
            Some(super::Totals {
                dictations,
                words,
                speech_ms: 0,
            })
        };
        assert_eq!(
            super::day_tip(day, today, totals(3, 1240)),
            "Nov 8 · 3 dictations · 1,240 words"
        );
        assert_eq!(
            super::day_tip(day, today, totals(1, 1)),
            "Nov 8 · 1 dictation · 1 word"
        );
        assert_eq!(super::day_tip(day, today, None), "Nov 8");
    }

    #[test]
    fn the_bars_grow_left_to_right_within_the_cap_and_never_overshoot() {
        let at = |ms| Duration::from_millis(ms);
        let n = 30;
        for i in 0..n {
            assert_eq!(revealed(1000, i, n, at(0)), 0, "bar {i} at the start");
            assert_eq!(revealed(1000, i, n, REVEAL), 1000, "bar {i} at the end");
        }
        let mut last = vec![0; n];
        for ms in (0..=200).step_by(8) {
            let now: Vec<i32> = (0..n).map(|i| revealed(1000, i, n, at(ms))).collect();
            assert!(now.iter().zip(&last).all(|(a, b)| a >= b), "rises only");
            assert!(now.windows(2).all(|p| p[0] >= p[1]), "left first: {now:?}");
            assert!(now.iter().all(|&h| h <= 1000));
            last = now;
        }
        // One bar alone grows at once, over `GROW`.
        assert_eq!(revealed(1000, 0, 1, GROW), 1000);
    }

    #[test]
    fn a_reveal_starts_empty_ends_full_and_stops_its_timer() {
        use slint::Model;
        i_slint_backend_testing::init_no_event_loop();
        let seen = super::Seen::default();
        let bars = |seen: &super::Seen| seen.bars.iter().collect::<Vec<i32>>();
        super::set_bars(&seen, vec![500, 1000], true);
        assert_eq!(bars(&seen), [0, 0], "the first frame is empty");
        assert!(seen.reveal.running());
        std::thread::sleep(REVEAL);
        i_slint_backend_testing::mock_elapsed_time(super::FRAME);
        assert_eq!(bars(&seen), [500, 1000]);
        assert!(!seen.reveal.running(), "a still chart draws nothing");
        // A poll's new numbers, and reduced motion: at once.
        super::set_bars(&seen, vec![1000, 1, 2], false);
        assert_eq!(bars(&seen), [1000, 1, 2]);
        assert!(!seen.reveal.running());
    }

    fn labels(start: (i32, u32, u32), weeks: i32) -> Vec<(i32, String)> {
        month_labels(Date::from_ymd(start.0, start.1, start.2).unwrap(), weeks)
            .into_iter()
            .map(|m| (m.week, m.text.to_string()))
            .collect()
    }

    fn pairs(want: &[(i32, &str)]) -> Vec<(i32, String)> {
        want.iter().map(|&(w, t)| (w, t.to_owned())).collect()
    }

    #[test]
    fn a_month_is_labelled_over_its_first_monday() {
        // Mondays: Jan 5, 12, 19, 26, Feb 2 (week 4), ..., Mar 2 (week 8), Apr 6 (week 13).
        assert_eq!(
            labels((2026, 1, 5), 14),
            pairs(&[(0, "Jan"), (4, "Feb"), (8, "Mar"), (13, "Apr")])
        );
    }

    #[test]
    fn the_first_column_gives_way_when_the_next_starts_a_month() {
        // Jan 26 is the last Monday of January; Feb 2 is the next column.
        assert_eq!(labels((2026, 1, 26), 6), pairs(&[(1, "Feb"), (5, "Mar")]));
    }

    #[test]
    fn a_year_end_counts_as_a_new_month() {
        assert_eq!(labels((2026, 12, 7), 5), pairs(&[(0, "Dec"), (4, "Jan")]));
    }

    #[test]
    fn today_sits_in_its_weekday_row_of_the_last_week() {
        // Monday-first rows: index % 7 is the weekday, index / 7 the week column.
        for (day, row) in [(2, 0), (4, 2), (8, 6)] {
            let today = Date::from_ymd(2026, 11, day).unwrap();
            let mut stats = Stats::default();
            stats.record(today, 5, Millis(1000));
            let summary = stats.summary(today, HEAT_WEEKS);
            let (_, heat) = summary.heat_tail(HEAT_WEEKS);
            let cell = usize::try_from(today_cell(heat)).unwrap();
            assert_eq!(cell % 7, row, "Nov {day}");
            assert_eq!(cell / 7, usize::from(HEAT_WEEKS) - 1, "Nov {day}");
            assert!(heat[cell] > 0, "the dictation lands on today's cell");
        }
    }

    #[test]
    fn a_new_day_is_a_change_with_the_file_untouched() {
        let file = std::path::Path::new("no such stats file");
        let day = Date::from_ymd(2026, 11, 4).unwrap();
        assert!(stamp_of(file, day) == stamp_of(file, day));
        assert!(stamp_of(file, day) != stamp_of(file, day.add_days(1)));
    }

    /// The page's window at `size` with a half year of dictation, on Home.
    fn home_at(name: &str, size: (u32, u32)) -> crate::AppWindow {
        use slint::ComponentHandle;
        let layout = crate::testing::layout("home", name);
        let (ipc, _sent) = crate::ipc::Ipc::fake();
        let window = crate::open_with(layout, ipc.clone()).unwrap();
        window.show().unwrap();
        window
            .window()
            .set_size(slint::PhysicalSize::new(size.0, size.1));
        let mut stats = Stats::default();
        let today = Date::from_ymd(2026, 11, 10).unwrap();
        for back in 0..180 {
            stats.record(today.add_days(-back), 50, Millis(60_000));
        }
        let summary = stats.summary(today, HEAT_WEEKS);
        ipc.deliver(&crate::ipc::Event::Push(oxi_core::ipc::Push::Stats(
            summary,
        )));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        window
    }

    fn heatmap(window: &crate::AppWindow) -> i_slint_backend_testing::ElementHandle {
        i_slint_backend_testing::ElementQuery::from_root(window)
            .match_accessible_role(i_slint_backend_testing::AccessibleRole::Image)
            .find_all()
            .into_iter()
            .find(|e| {
                e.accessible_label()
                    .is_some_and(|l| l.contains("active day"))
            })
            .expect("the heatmap")
    }

    #[test]
    fn the_page_never_needs_a_scrollbar_in_a_low_window() {
        i_slint_backend_testing::init_no_event_loop();
        // The widths of both card layouts and of two-line labels, at and above the window's least.
        for size in [
            (1280, 540),
            (1280, 520),
            (1280, 860),
            (1920, 1080),
            (900, 520),
            (899, 520),
            (899, 720),
            (899, 640),
            (800, 520),
            (800, 720),
            (720, 520),
            (720, 720),
        ] {
            let window = home_at("fits", size);
            let heat = heatmap(&window);
            let (at, h) = (heat.absolute_position(), heat.size());
            // Below the grid: the card's padding and the page's.
            let pad = if size.0 < 900 { 12.0 } else { 16.0 };
            let bottom = at.y + h.height + 12.0 + pad;
            #[allow(clippy::cast_precision_loss, reason = "a window height")]
            let height = size.1 as f32;
            assert!(bottom <= height, "{size:?}: the page ends at {bottom}");
            // Where the height, not the width, limits the cells (one row of cards at 1280 and 899
            // wide, two rows at 899 x 640, the least window that holds both): the grid fills the room
            // to within the 8 px that whole-pixel cells leave, so a chrome token that is off by
            // more than that (`home-chrome`, `home-chrome-row`) fails here. The legend is one 16 px
            // line, not a cell high, and the page's cell formula still reserves a cell: that
            // difference (cell - 16) is spare room too (home.slint `cell-high`).
            // `home-chrome-labels` (two label lines) only comes with a card under 172 px, a width
            // that never limits by height inside the least window: it is not reachable here.
            if [(1280, 540), (1280, 520), (899, 520), (899, 640)].contains(&size) {
                // The heatmap is 20 (months) + 7 x (cell + 3) + 12 + 16 (legend) high.
                let cell = (h.height - 69.0) / 7.0;
                let spare = height - bottom - (cell - 16.0);
                assert!((0.0..8.0).contains(&spare), "{size:?}: {spare} px spare");
            }
        }
    }

    #[test]
    fn a_locked_stats_file_keeps_the_heatmap_and_its_tips_together() {
        use slint::{ComponentHandle, Model};
        i_slint_backend_testing::init_no_event_loop();
        let layout = crate::testing::layout("home", "locked");
        let (ipc, _sent) = crate::ipc::Ipc::fake();
        let window = crate::open_with(layout.clone(), ipc.clone()).unwrap();
        window.show().unwrap();
        let today = oxi_platform::stats_store::today();
        let push = |stats: &Stats| {
            let summary = stats.summary(today, HEAT_WEEKS);
            ipc.deliver(&crate::ipc::Event::Push(oxi_core::ipc::Push::Stats(
                summary,
            )));
        };
        let home = window.global::<crate::generated::HomeState>();
        let tips = || home.get_day_tips().iter().collect::<Vec<_>>();

        let mut stats = Stats::default();
        stats.record(today.add_days(-3), 100, Millis(60_000));
        let file = layout.data_dir.join(oxi_core::stats::FILE);
        std::fs::write(&file, stats.to_json().unwrap()).unwrap();
        push(&stats);
        let (before, levels) = (tips(), home.get_levels().row_count());
        assert!(
            before.iter().any(|t| t.contains("1 dictation")),
            "{before:?}"
        );

        // The file cannot be read (here: a folder in its place) as a second day lands: the numbers are new, the heatmap
        // and its tips are still the old read, not a new day with no counts.
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        stats.record(today, 7, Millis(60_000));
        push(&stats);
        assert_eq!(home.get_dictations(), "2");
        assert_eq!(tips(), before);
        assert_eq!(home.get_levels().row_count(), levels);

        // Readable again: both are new.
        std::fs::remove_dir(&file).unwrap();
        std::fs::write(&file, stats.to_json().unwrap()).unwrap();
        push(&stats);
        assert!(
            tips().last().unwrap().contains("1 dictation"),
            "{:?}",
            tips().last()
        );
    }

    #[test]
    fn the_folder_button_finds_the_nearest_folder_and_creates_none() {
        let root = crate::testing::scratch_dir(&format!("oxi-home-target-{}", std::process::id()));
        let data = root.join("data");
        let default = data.join("history");
        let chosen = root.join("chosen");
        let owned = chosen.join("owned");
        let gone = root.join("gone");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&chosen).unwrap();
        let on = History {
            enabled: true,
            ..History::default()
        };
        let off = History {
            enabled: false,
            ..History::default()
        };
        let target = history_target;

        // Not there yet: the data directory or the chosen parent.
        assert_eq!(
            target(&on, &default, &data),
            Some((data.clone(), Some(HISTORY_EMPTY)))
        );
        assert_eq!(
            target(&off, &default, &data),
            Some((data.clone(), Some(HISTORY_OFF)))
        );
        assert_eq!(
            target(&on, &owned, &data),
            Some((chosen.clone(), Some(HISTORY_EMPTY)))
        );
        // The chosen parent is gone too (an unplugged drive).
        assert_eq!(
            target(&on, &gone.join("owned"), &data),
            Some((data.clone(), Some(HISTORY_EMPTY)))
        );
        // Nothing exists at all.
        assert_eq!(target(&on, &gone.join("owned"), &gone), None);
        assert!(
            !default.exists() && !owned.exists() && !gone.exists(),
            "creates nothing"
        );

        // There: the folder itself; with History off the old files are still reachable.
        std::fs::create_dir(&owned).unwrap();
        assert_eq!(target(&on, &owned, &data), Some((owned.clone(), None)));
        assert_eq!(
            target(&off, &owned, &data),
            Some((owned.clone(), Some(HISTORY_OFF)))
        );
        // A file in the folder's place is not a folder.
        std::fs::write(&default, "a file").unwrap();
        assert_eq!(
            target(&on, &default, &data),
            Some((data.clone(), Some(HISTORY_EMPTY)))
        );
    }
}
