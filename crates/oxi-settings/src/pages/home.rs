//! Home page: the usage numbers and the speaking-activity heatmap. The daemon answers
//! `Request::Stats` while it is online; otherwise the page reads the stats file itself, read-only.

use std::cell::Cell;
use std::fs;
use std::rc::Rc;
use std::time::SystemTime;

use oxi_core::ipc::{Push, Request};
use oxi_core::stats::{self, Date, HEAT_WEEKS, Summary, format_count};
use oxi_platform::stats_store;
use slint::{ComponentHandle, ModelRc, Timer, TimerMode, VecModel};

use crate::generated::{HomeState, MonthLabel};
use crate::ipc::{Event, Link};
use crate::{AppWindow, Ctx, FILE_POLL, Page};

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// When the stats file last changed, as of the last refresh.
type Seen = Rc<Cell<Option<SystemTime>>>;

pub fn bind(window: &AppWindow, ctx: &Ctx) {
    let seen = Seen::default();
    // The file first: numbers on screen before the link is up. Asking the daemon waits for it.
    seen.set(modified(ctx));
    load(window, ctx);

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
                && modified(&c) != s.get()
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
    seen.set(modified(ctx));
    if ctx.ipc.link() == Link::Online {
        set_chord(window, ctx);
        ctx.ipc.send(Request::Stats);
    } else {
        load(window, ctx);
    }
}

/// The page from the stats file. A corrupt one reads as empty, as the daemon's own summary does;
/// only the daemon may reset it.
fn load(window: &AppWindow, ctx: &Ctx) {
    set_chord(window, ctx);
    let stats = stats_store::read(&ctx.layout.data_dir.join(stats::FILE)).unwrap_or_default();
    show(window, &stats.summary(stats_store::today(), HEAT_WEEKS));
}

fn modified(ctx: &Ctx) -> Option<SystemTime> {
    fs::metadata(ctx.layout.data_dir.join(stats::FILE))
        .and_then(|m| m.modified())
        .ok()
}

/// The empty state names the hotkey, which the Hotkey page may have just changed.
fn set_chord(window: &AppWindow, ctx: &Ctx) {
    let chord = ctx.config.borrow().config.hotkey.chord.join(" + ");
    window.global::<HomeState>().set_chord(chord.into());
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
    s.set_today(int(summary.heat.len()) - 1);
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
    use super::{Date, month_labels};

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
}
