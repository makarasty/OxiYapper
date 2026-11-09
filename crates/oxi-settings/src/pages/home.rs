//! Home page: the usage numbers and the speaking-activity heatmap. The daemon answers
//! `Request::Stats` while it is online; otherwise the page reads the stats file itself, read-only.

use std::cell::{Cell, RefCell};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::SystemTime;

use oxi_core::config::History;
use oxi_core::history;
use oxi_core::ipc::{Push, Request};
use oxi_core::machine::Mode;
use oxi_core::stats::{self, Date, HEAT_WEEKS, Metric, Stats, Summary, format_count};
use oxi_platform::stats_store;
use slint::{ComponentHandle, ModelRc, Timer, TimerMode, VecModel};

use crate::constants::{FILE_POLL, FOLDER_POLL};
use crate::generated::{HomeState, MonthLabel, Toast};
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
}

type Seen = Rc<Shown>;

pub fn bind(window: &AppWindow, ctx: &Ctx) {
    let seen = Seen::default();
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
                && stamp(&c) != s.stamp.get()
            {
                refresh(&window, &c, &s);
            }
        }
    });

    let (w, c, s) = (window.as_weak(), ctx.clone(), seen.clone());
    window.global::<HomeState>().on_shown(move || {
        // Held by this callback: a rebind (Reset) replaces it and so stops them.
        let _ = (&events, &poll);
        if let Some(window) = w.upgrade() {
            refresh(&window, &c, &s);
        }
    });

    let (w, c) = (window.as_weak(), ctx.clone());
    window.global::<HomeState>().on_pick(move |card| {
        if let Some(window) = w.upgrade() {
            let s = window.global::<HomeState>();
            s.set_selected(if s.get_selected() == card { -1 } else { card });
            if !show_trend(&window, &c, &seen) {
                seen.stamp.set(Stamp::default());
            }
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

/// New numbers: from the daemon while it answers, else from the file.
fn refresh(window: &AppWindow, ctx: &Ctx, seen: &Seen) {
    let now = stamp(ctx);
    let mut read = show_trend(window, ctx, seen);
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
    show(window, &stats.summary(stats_store::today(), HEAT_WEEKS));
    *seen.stats.borrow_mut() = Some(stats);
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
fn show_trend(window: &AppWindow, ctx: &Ctx, seen: &Seen) -> bool {
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
        s.set_trend(ModelRc::from(Rc::new(VecModel::from(trend.heights()))));
        s.set_trend_peak(format_count(trend.peak()).into());
        s.set_trend_first(day_label(trend.start, today).into());
        s.set_trend_last(day_label(today, today).into());
    }
    read
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

    use super::{
        Date, HISTORY_EMPTY, HISTORY_OFF, History, history_target, month_labels, stamp_of,
        today_cell,
    };

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

    #[test]
    fn the_folder_button_finds_the_nearest_folder_and_creates_none() {
        let root = std::env::temp_dir().join(format!("oxi-home-target-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
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
        let off = History::default();
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
        std::fs::remove_dir_all(&root).unwrap();
    }
}
