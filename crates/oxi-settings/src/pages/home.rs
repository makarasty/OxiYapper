//! Home page: the usage numbers and the speaking-activity heatmap. The daemon answers
//! `Request::Stats` while it is online; otherwise the page reads the stats file itself, read-only.

use std::cell::Cell;
use std::fs;
use std::io;
use std::path::Path;
use std::rc::Rc;
use std::time::SystemTime;

use oxi_core::ipc::{Push, Request};
use oxi_core::machine::Mode;
use oxi_core::stats::{self, Date, HEAT_WEEKS, Summary, format_count};
use oxi_platform::stats_store;
use slint::{ComponentHandle, ModelRc, Timer, TimerMode, VecModel};

use crate::generated::{HomeState, MonthLabel};
use crate::ipc::{Event, Link};
use crate::{AppWindow, Ctx, FILE_POLL, Page};

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// What the page showed last: the stats file's time and the day the heatmap was cut for. A new
/// day changes the window and the streak with no file change.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Stamp {
    modified: Option<SystemTime>,
    day: Date,
}

type Seen = Rc<Cell<Stamp>>;

pub fn bind(window: &AppWindow, ctx: &Ctx) {
    let seen = Seen::default();
    // The file first: numbers on screen before the link is up. Asking the daemon waits for it.
    let now = stamp(ctx);
    if load(window, ctx) {
        seen.set(now);
    }

    let (w, c, s) = (window.as_weak(), ctx.clone(), seen.clone());
    let events = ctx.ipc.subscribe(move |event| {
        let Some(window) = w.upgrade() else {
            return;
        };
        match event {
            Event::Push(Push::Stats(summary)) => show(&window, summary),
            Event::Link(_) if window.get_page() == Page::Home => refresh(&window, &c, &s),
            _ => {}
        }
    });

    // A finished dictation sends nothing here; the stats file changing is the sign of one.
    let poll = Timer::default();
    poll.start(TimerMode::Repeated, FILE_POLL, {
        let (w, c, s) = (window.as_weak(), ctx.clone(), seen.clone());
        move || {
            if let Some(window) = w.upgrade()
                && window.get_page() == Page::Home
                && stamp(&c) != s.get()
            {
                refresh(&window, &c, &s);
            }
        }
    });

    let (w, c) = (window.as_weak(), ctx.clone());
    window.global::<HomeState>().on_shown(move || {
        // Held by this callback: a rebind (Reset) replaces it and so stops them.
        let _ = (&events, &poll);
        if let Some(window) = w.upgrade() {
            refresh(&window, &c, &seen);
        }
    });
}

/// New numbers: from the daemon while it answers, else from the file.
fn refresh(window: &AppWindow, ctx: &Ctx, seen: &Seen) {
    let now = stamp(ctx);
    if ctx.ipc.link() == Link::Online {
        seen.set(now);
        set_chord(window, ctx);
        ctx.ipc.send(Request::Stats);
    } else if load(window, ctx) {
        seen.set(now);
    }
}

/// The page from the stats file; false when the file could not be read (briefly locked): the
/// numbers on screen stay and the next poll tries again. A corrupt file reads as empty, as the
/// daemon's own summary does; only the daemon may reset it.
fn load(window: &AppWindow, ctx: &Ctx) -> bool {
    set_chord(window, ctx);
    let stats = match stats_store::read(&ctx.layout.data_dir.join(stats::FILE)) {
        Ok(stats) => stats,
        Err(e) if e.kind() == io::ErrorKind::InvalidData => stats::Stats::default(),
        Err(_) => return false,
    };
    show(window, &stats.summary(stats_store::today(), HEAT_WEEKS));
    true
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

/// The empty state names the hotkey and how to use it, which the Hotkey page may have just changed.
fn set_chord(window: &AppWindow, ctx: &Ctx) {
    let config = ctx.config.borrow();
    let s = window.global::<HomeState>();
    s.set_chord(config.config.hotkey.chord.join(" + ").into());
    s.set_toggle(config.config.hotkey.mode == Mode::Toggle);
}

fn show(window: &AppWindow, summary: &Summary) {
    let int = |n: usize| i32::try_from(n).unwrap_or(i32::MAX);
    let s = window.global::<HomeState>();
    s.set_words(format_count(summary.words).into());
    s.set_wpm(summary.wpm.to_string().into());
    s.set_dictations(format_count(summary.dictations).into());
    s.set_streak(i32::try_from(summary.streak).unwrap_or(i32::MAX));
    s.set_longest_streak(i32::try_from(summary.longest_streak).unwrap_or(i32::MAX));
    s.set_active_days(int(summary.heat.iter().filter(|&&l| l > 0).count()));
    s.set_empty(summary.dictations == 0);
    // `heat_start` is a Monday, so the cells fall in Monday-first columns as they come.
    let levels: Vec<i32> = summary.heat.iter().map(|&l| i32::from(l)).collect();
    s.set_levels(ModelRc::from(Rc::new(VecModel::from(levels))));
    let weeks = int(summary.heat.len().div_ceil(7));
    let months = month_labels(summary.heat_start, weeks);
    s.set_months(ModelRc::from(Rc::new(VecModel::from(months))));
    s.set_today(today_cell(summary));
}

/// Today is the last cell: the window ends on it.
fn today_cell(summary: &Summary) -> i32 {
    i32::try_from(summary.heat.len()).map_or(i32::MAX, |n| n - 1)
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

    use super::{Date, month_labels, stamp_of, today_cell};

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
            let cell = usize::try_from(today_cell(&summary)).unwrap();
            assert_eq!(cell % 7, row, "Nov {day}");
            assert_eq!(cell / 7, usize::from(HEAT_WEEKS) - 1, "Nov {day}");
            assert!(
                summary.heat[cell] > 0,
                "the dictation lands on today's cell"
            );
        }
    }

    #[test]
    fn a_new_day_is_a_change_with_the_file_untouched() {
        let file = std::path::Path::new("no such stats file");
        let day = Date::from_ymd(2026, 11, 4).unwrap();
        assert!(stamp_of(file, day) == stamp_of(file, day));
        assert!(stamp_of(file, day) != stamp_of(file, day.add_days(1)));
    }
}
