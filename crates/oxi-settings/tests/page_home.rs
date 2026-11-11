//! Home page: the numbers from the daemon or the file, the empty state and the reflow.

mod common;

use std::fs;
use std::sync::mpsc::Receiver;
use std::time::{Duration, SystemTime};

use common::{has, layout, open, open_linked, with_role};
use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery};
use oxi_core::ipc::{Push, Request};
use oxi_core::paths::Layout;
use oxi_core::stats::{Date, FILE, Stats, Summary};
use oxi_core::types::Millis;
use oxi_platform::stats_store;
use oxi_settings::ipc::{Cmd, Event, Link};
use oxi_settings::{AppWindow, FILE_POLL, Ipc, Page};
use slint::platform::{Key, WindowEvent};
use slint::{ComponentHandle, SharedString};

const CHORD: &str = "version = 2\n[hotkey]\nchord = [\"LCtrl\", \"F13\"]\n";
const TOGGLE: &str = "version = 2
[hotkey]
chord = [\"LCtrl\", \"F13\"]
mode = \"toggle\"
";
const CARDS: [&str; 4] = [
    "Words spoken: 299.9K, All time",
    "Words per minute: 146, Based on recorded speech",
    "Dictations: 11.3K, All time",
    "Current streak: 6 days, Longest: 40 days",
];

fn sample() -> Summary {
    Summary {
        dictations: 11_300,
        words: 299_900,
        wpm: 146,
        streak: 6,
        longest_streak: 40,
        heat_start: Date::from_ymd(2026, 11, 2).unwrap(),
        heat: vec![0, 1, 4],
    }
}

fn stats_push(ipc: &Ipc, summary: Summary) {
    ipc.deliver(&Event::Push(Push::Stats(summary)));
}

fn poll() {
    i_slint_backend_testing::mock_elapsed_time(FILE_POLL);
}

/// Runs the `changed` handlers; the testing backend has no event loop.
fn settle() {
    slint::platform::update_timers_and_animations();
}

fn press(window: &AppWindow, key: impl Into<SharedString> + Clone) {
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: key.clone().into(),
    });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
}

/// The file one dictation of `words` words today leaves.
fn write_stats(l: &Layout, words: u64) {
    let mut stats = Stats::default();
    stats.record(stats_store::today(), words, Millis(60_000));
    fs::write(l.data_dir.join(FILE), stats.to_json().unwrap()).unwrap();
}

fn heatmap(window: &AppWindow) -> ElementHandle {
    ElementQuery::from_root(window)
        .match_descendants()
        .match_accessible_role(AccessibleRole::Image)
        .find_all()
        .into_iter()
        .find(|e| {
            e.accessible_label()
                .is_some_and(|l| l.contains("active day"))
        })
        .expect("the heatmap")
}

/// How many `Stats` requests went out since the last call.
fn asked(rx: &Receiver<Cmd>) -> usize {
    rx.try_iter()
        .filter(|c| *c == Cmd::Send(Request::Stats))
        .count()
}

#[test]
fn home_is_the_default_page() {
    let window = common::open_default(&layout("default", None));
    assert_eq!(window.get_page(), Page::Home);
    assert!(has(&window, "Speaking activity"));
}

#[test]
fn the_daemons_numbers_fill_the_cards() {
    let (ipc, _rx) = Ipc::fake();
    let window = open_linked(&layout("numbers", None), Page::Home, ipc.clone());
    stats_push(&ipc, sample());
    for label in CARDS {
        assert!(has(&window, label), "no card {label}");
    }
    assert_eq!(
        heatmap(&window).accessible_label().unwrap(),
        "2 active days in the last 1 weeks"
    );

    stats_push(
        &ipc,
        Summary {
            streak: 1,
            longest_streak: 1,
            ..sample()
        },
    );
    assert!(has(&window, "Current streak: 1 day, Longest: 1 day"));
}

#[test]
fn showing_the_page_and_coming_online_ask_the_daemon() {
    let (ipc, rx) = Ipc::fake();
    let window = open_linked(&layout("ask", None), Page::Hotkey, ipc.clone());
    settle();
    asked(&rx);
    ipc.deliver(&Event::Link(Link::Online));
    assert_eq!(asked(&rx), 0, "another page: nothing to refresh");

    window.set_page(Page::Home);
    settle();
    assert_eq!(asked(&rx), 1, "page show");
    ipc.deliver(&Event::Link(Link::Online));
    assert_eq!(asked(&rx), 1, "link up");
}

#[test]
fn offline_shows_the_file_and_leaves_it_as_it_is() {
    let l = layout("file", None);
    write_stats(&l, 1234);
    let before = fs::read(l.data_dir.join(FILE)).unwrap();
    let window = open(&l, Page::Home);
    assert!(has(&window, "Words spoken: 1.2K, All time"));
    assert!(has(&window, "Dictations: 1, All time"));
    assert!(has(&window, "Current streak: 1 day, Longest: 1 day"));
    assert_eq!(fs::read(l.data_dir.join(FILE)).unwrap(), before);
}

#[test]
fn a_corrupt_file_reads_as_empty_and_is_left_alone() {
    let l = layout("corrupt", None);
    fs::write(l.data_dir.join(FILE), "{not json").unwrap();
    let window = open(&l, Page::Home);
    assert!(has(&window, "Words spoken: 0, All time"));
    assert_eq!(fs::read(l.data_dir.join(FILE)).unwrap(), b"{not json");
    assert!(!l.data_dir.join("stats.bad").exists());
}

#[test]
fn a_changed_file_refreshes_the_open_page() {
    let l = layout("changes", None);
    let window = open(&l, Page::Home);
    assert!(has(&window, "Words spoken: 0, All time"));
    write_stats(&l, 42);
    poll();
    assert!(has(&window, "Words spoken: 42, All time"));
}

#[test]
fn online_a_changed_file_asks_the_daemon_only_on_home() {
    let l = layout("changes-online", None);
    let (ipc, rx) = Ipc::fake();
    let window = open_linked(&l, Page::Hotkey, ipc);
    poll();
    asked(&rx);
    write_stats(&l, 42);
    poll();
    assert_eq!(asked(&rx), 0, "another page does not look");
    window.set_page(Page::Home);
    settle();
    assert_eq!(asked(&rx), 1, "page show");
    poll();
    assert_eq!(asked(&rx), 0, "nothing changed since");
    // Coarse file times could hide a rewrite within the same tick.
    std::thread::sleep(Duration::from_millis(20));
    write_stats(&l, 43);
    poll();
    assert_eq!(asked(&rx), 1, "the file changed");
}

/// What the activity card's "i" says after `how`.
const FIRST: &str = "Your words, pace and streak show up here after the first dictation.";
const CLICK: &str = "Click a card above to see it day by day.";

#[test]
fn no_data_says_to_hold_the_current_chord() {
    let window = open(&layout("empty", Some(CHORD)), Page::Home);
    assert!(has(&window, &format!("Hold Ctrl + F13 and speak. {FIRST}")));
    assert!(has(&window, "No dictations yet"), "the empty state");
    assert!(has(&window, "Current streak: 0 days, Longest: 0 days"));
    assert!(has(
        &window,
        "Words per minute: 0, Based on recorded speech"
    ));
}

/// UI rules 1 and 5: the first dictation does not resize the activity card, so the History folder
/// button in its title row stays where it was.
#[test]
fn the_first_dictation_moves_nothing_below_the_heatmap() {
    let l = layout("first-dictation", Some(CHORD));
    let window = open(&l, Page::Home);
    window
        .window()
        .set_size(slint::LogicalSize::new(1280.0, 860.0));
    settle();
    let folder = || {
        with_role(&window, "History folder", AccessibleRole::Button)
            .absolute_position()
            .y
    };
    let before = folder();
    write_stats(&l, 42);
    poll();
    settle();
    assert!(
        has(&window, "Dictations: 1, All time"),
        "the stats did not load"
    );
    let after = folder();
    assert!(
        (before - after).abs() < 0.5,
        "History folder moved from {before} to {after}"
    );
}

#[test]
fn the_chord_reads_as_the_hotkey_page_shows_it() {
    let chord = "version = 2\n[hotkey]\nchord = [\"LCtrl\", \"BracketLeft\"]\n";
    let window = open(&layout("empty-label", Some(chord)), Page::Home);
    assert!(has(&window, &format!("Hold Ctrl + [ and speak. {FIRST}")));
}

#[test]
fn toggle_mode_says_press_not_hold() {
    let window = open(&layout("empty-toggle", Some(TOGGLE)), Page::Home);
    assert!(has(
        &window,
        &format!("Press Ctrl + F13 once to start, again to stop. {FIRST}")
    ));
    assert!(!has(
        &window,
        &format!("Hold Ctrl + F13 and speak. {FIRST}")
    ));
}

/// UI rule 35: the hints are the "i" tips' text, not lines of their own, and say what is true now.
#[test]
fn the_hints_live_in_info_tips() {
    let l = layout("hints", Some(CHORD));
    let window = open(&l, Page::Home);
    let tips = |window: &AppWindow| {
        ElementQuery::from_root(window)
            .match_descendants()
            .match_accessible_role(AccessibleRole::Text)
            .find_all()
            .into_iter()
            .filter_map(|e| e.accessible_label())
            .map(|l| l.to_string())
            .filter(|l| l.contains(FIRST) || l.contains(CLICK))
            .fold(Vec::<String>::new(), |mut seen, l| {
                // A component and its inner parts can answer with one label.
                if !seen.contains(&l) {
                    seen.push(l);
                }
                seen
            })
    };
    assert_eq!(
        tips(&window),
        [format!("Hold Ctrl + F13 and speak. {FIRST}")]
    );
    write_stats(&l, 42);
    poll();
    assert_eq!(
        tips(&window),
        [format!("Hold Ctrl + F13 and speak. {CLICK}")]
    );
    assert!(!has(&window, "No dictations yet"), "the heatmap is back");
}

/// UI rule 24: the activity card is as high as its content; a large window's spare height stays
/// under it, the folder button sits in the card's title row, and the page does not scroll.
#[test]
fn the_page_fills_a_large_window() {
    let (ipc, _rx) = Ipc::fake();
    let window = open_linked(&layout("large", None), Page::Home, ipc.clone());
    stats_push(&ipc, sample());
    for (w, h) in [(1280.0, 860.0), (1920.0, 1080.0)] {
        window.window().set_size(slint::LogicalSize::new(w, h));
        settle();
        // `ends_inside` also holds the button above the grid.
        let map = heatmap(&window);
        ends_inside(&window, &map, w, h);
        // The card hugs its content: the grid plus the card's paddings, not the window's height.
        let card = activity_card(&window, &map);
        let pad = window.global::<oxi_settings::Tokens>().get_card_padding_v();
        assert_eq!(
            card.size().height,
            map.size().height + 2.0 * pad,
            "{w}x{h}: the activity card is not as high as its content"
        );
    }
}

/// The card around the heatmap (the one `CardFrame` that holds it).
fn activity_card(window: &AppWindow, map: &ElementHandle) -> ElementHandle {
    let (top, bottom) = (
        map.absolute_position().y,
        map.absolute_position().y + map.size().height,
    );
    ElementHandle::find_by_element_type_name(window, "CardFrame")
        .find(|c| {
            c.absolute_position().y <= top && c.absolute_position().y + c.size().height >= bottom
        })
        .expect("the activity card")
}

/// Item 38: hovering a bar names its day and exact number at once.
#[test]
fn hovering_a_bar_shows_its_day_and_number() {
    let l = layout("trend-hover", None);
    write_days(&l, 2);
    let window = open(&l, Page::Home);
    window
        .window()
        .set_size(slint::LogicalSize::new(1280.0, 860.0));
    settle();
    click_words(&window);
    let chart = ElementQuery::from_root(&window)
        .match_descendants()
        .match_accessible_role(AccessibleRole::Image)
        .find_all()
        .into_iter()
        .find(|e| e.accessible_label().is_some_and(|l| l.contains("highest")))
        .unwrap();
    let (at, size) = (chart.absolute_position(), chart.size());
    let said = |window: &AppWindow| {
        ElementQuery::from_root(window)
            .match_descendants()
            .find_all()
            .into_iter()
            .filter_map(|e| e.accessible_label())
            .map(|l| l.to_string())
            .find(|l| l.ends_with(" words") && l.contains(": "))
    };
    let hover = |x: f32| {
        window.window().dispatch_event(WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(x, at.y + size.height / 2.0),
        });
        settle();
    };
    // The drawn bubble (the page also holds an unsized one to measure it): its text, left, right.
    let bubble = |window: &AppWindow| {
        ElementQuery::from_root(window)
            .match_descendants()
            .find_all()
            .into_iter()
            .filter(|e| e.size().width > 0.0)
            .find_map(|e| {
                let l = e.accessible_label()?.to_string();
                let x = e.absolute_position().x;
                (l.ends_with(" words") && l.contains(": ")).then(|| (l, x, x + e.size().width))
            })
    };
    // Two days of data drawn as the last two of 30: the rightmost bar is today.
    let last_bar = at.x + size.width - size.width / 30.0;
    hover(at.x + size.width - 2.0);
    let today = said(&window).expect("a tooltip on the last bar");
    assert!(today.ends_with(": 5 words"), "{today}");
    // No room right of the last column: the bubble opens left of it, off the hovered bar.
    let (_, _, right) = bubble(&window).expect("a drawn bubble");
    assert!(
        right <= last_bar,
        "the bubble ends at {right}, the bar starts at {last_bar}"
    );
    // A refresh under a still pointer: the bubble says the new number.
    write_stats(&l, 9);
    poll();
    settle();
    let fresh = bubble(&window).expect("still open").0;
    assert!(
        fresh.ends_with(": 9 words"),
        "stale after a refresh: {fresh}"
    );
    hover(at.x + 2.0);
    let first = said(&window).expect("a tooltip on the first day");
    assert!(first.ends_with(": 0 words"), "padding day: {first}");
    let (_, left, _) = bubble(&window).expect("a drawn bubble");
    assert!(
        left > at.x + size.width / 30.0,
        "right of the first column: {left}"
    );
    window.window().dispatch_event(WindowEvent::PointerExited);
    settle();
    assert_eq!(said(&window), None, "gone with the pointer");
}

/// The four cards' (left, top) at `width` wide.
fn card_spots(window: &AppWindow, width: f32) -> Vec<(f32, f32)> {
    window
        .window()
        .set_size(slint::LogicalSize::new(width, 900.0));
    CARDS
        .iter()
        .map(|label| {
            let p = ElementHandle::find_by_accessible_label(window, label)
                .next()
                .unwrap_or_else(|| panic!("no card {label}"))
                .absolute_position();
            (p.x, p.y)
        })
        .collect()
}

#[test]
fn four_cards_in_a_row_reflow_to_two_by_two() {
    let (ipc, _rx) = Ipc::fake();
    let window = open_linked(&layout("reflow", None), Page::Home, ipc.clone());
    stats_push(&ipc, sample());

    let row = card_spots(&window, 1280.0);
    assert!(row.iter().all(|c| (c.1 - row[0].1).abs() < 0.5), "{row:?}");
    assert!(row.windows(2).all(|p| p[0].0 < p[1].0), "{row:?}");

    let grid = card_spots(&window, 720.0);
    assert!((grid[0].1 - grid[1].1).abs() < 0.5, "{grid:?}");
    assert!((grid[2].1 - grid[3].1).abs() < 0.5, "{grid:?}");
    assert!(grid[2].1 > grid[0].1, "{grid:?}");
    assert!((grid[0].0 - grid[2].0).abs() < 0.5, "{grid:?}");
}

/// Whether the four cards share a row at window width `width`.
fn four_in_a_row(window: &AppWindow, width: f32) -> bool {
    let spots = card_spots(window, width);
    spots.iter().all(|c| (c.1 - spots[0].1).abs() < 0.5)
}

#[test]
fn the_row_choice_follows_the_window_width_alone() {
    let (ipc, _rx) = Ipc::fake();
    let window = open_linked(&layout("monotonic", None), Page::Home, ipc.clone());
    stats_push(&ipc, sample());
    // The rail widens at 960 and the page column narrows with it: no flip back to two rows.
    let widths = [
        720.0, 848.0, 899.0, 900.0, 959.0, 960.0, 1043.0, 1044.0, 1280.0,
    ];
    let rows: Vec<bool> = widths.iter().map(|&w| four_in_a_row(&window, w)).collect();
    assert_eq!(
        rows,
        [false, false, false, true, true, true, true, true, true],
        "{widths:?}"
    );
}

#[test]
fn an_unreadable_stats_file_keeps_the_numbers_and_is_retried() {
    let l = layout("locked", None);
    write_stats(&l, 42);
    let window = open(&l, Page::Home);
    assert!(has(&window, "Words spoken: 42, All time"));

    let locked = lock(&l);
    poll();
    assert!(
        has(&window, "Words spoken: 42, All time"),
        "kept, not zeroed"
    );

    unlock(&l, locked, || write_stats(&l, 43));
    poll();
    assert!(has(&window, "Words spoken: 43, All time"));
}

/// A stats file with a dictation on each of the `days` days ending today.
fn write_days(l: &Layout, days: i32) {
    let today = stats_store::today();
    let mut stats = Stats::default();
    for back in (0..days).rev() {
        stats.record(today.add_days(-back), 5, Millis(60_000));
    }
    fs::write(l.data_dir.join(FILE), stats.to_json().unwrap()).unwrap();
}

fn click_card(window: &AppWindow, label: &str) {
    common::one(window, label).invoke_accessible_default_action();
}

/// The open chart's spoken summary, if a chart is open.
fn chart(window: &AppWindow) -> Option<String> {
    ElementQuery::from_root(window)
        .match_descendants()
        .match_accessible_role(AccessibleRole::Image)
        .find_all()
        .into_iter()
        .filter_map(|e| e.accessible_label())
        .map(|l| l.to_string())
        .find(|l| l.contains("highest"))
}

/// The Words spoken card, whatever its number.
fn click_words(window: &AppWindow) {
    ElementQuery::from_root(window)
        .match_descendants()
        .match_accessible_role(AccessibleRole::Button)
        .find_all()
        .into_iter()
        .find(|e| {
            e.accessible_label()
                .is_some_and(|l| l.starts_with("Words spoken"))
        })
        .expect("the card")
        .invoke_accessible_default_action();
}

#[test]
fn a_card_click_opens_its_trend_and_a_second_click_closes_it() {
    let l = layout("trend-click", None);
    write_days(&l, 3);
    let (ipc, _rx) = Ipc::fake();
    let window = open_linked(&l, Page::Home, ipc.clone());
    // Online the cards are the daemon's and the bars still come from the file.
    stats_push(&ipc, sample());
    assert_eq!(chart(&window), None);

    click_card(&window, CARDS[0]);
    assert!(has(&window, "Words by day"));
    assert!(!has(&window, "Speaking activity"));
    assert_eq!(chart(&window).unwrap(), "3 days, highest 5");

    click_card(&window, CARDS[3]);
    assert!(has(&window, "Streak by day"));
    assert_eq!(chart(&window).unwrap(), "3 days, highest 3", "another card");

    click_card(&window, CARDS[3]);
    assert!(has(&window, "Speaking activity"));
    assert_eq!(chart(&window), None);
}

#[test]
fn the_three_shapes_of_history_chart() {
    i_slint_backend_testing::init_no_event_loop();
    for (days, want) in [
        (0, None),
        (1, Some("1 day, highest 5")),
        (366, Some("365 days, highest 5")),
    ] {
        let l = layout(&format!("trend-{days}"), None);
        write_days(&l, days);
        let window = common::open_again(&l, Page::Home);
        click_words(&window);
        assert_eq!(chart(&window).as_deref(), want);
        assert_eq!(has(&window, "No dictations yet"), days == 0, "{days}");
    }
}

#[test]
fn opening_a_chart_moves_nothing() {
    i_slint_backend_testing::init_no_event_loop();
    for days in [0, 1, 366] {
        for width in [720.0, 1280.0] {
            let l = layout(&format!("trend-still-{days}-{width}"), None);
            write_days(&l, days);
            let window = common::open_again(&l, Page::Home);
            window
                .window()
                .set_size(slint::LogicalSize::new(width, 900.0));
            let closed = common::one(&window, "History folder").absolute_position();
            click_words(&window);
            assert!(has(&window, "Words by day"));
            let open = common::one(&window, "History folder").absolute_position();
            assert_eq!((closed.x, closed.y), (open.x, open.y), "{days} {width}");
        }
    }
}

#[test]
fn the_open_chart_follows_the_file() {
    let l = layout("trend-follow", None);
    let window = open(&l, Page::Home);
    click_words(&window);
    assert_eq!(chart(&window), None);
    assert!(has(&window, "No dictations yet"));
    write_days(&l, 1);
    poll();
    assert_eq!(chart(&window).unwrap(), "1 day, highest 5");
}

/// A locked file stand-in: a directory in its place, which reading fails on with something other
/// than `NotFound`. Returns the time the failed read sees.
fn lock(l: &Layout) -> SystemTime {
    let file = l.data_dir.join(FILE);
    fs::remove_file(&file).unwrap();
    fs::create_dir(&file).unwrap();
    fs::metadata(&file).unwrap().modified().unwrap()
}

/// Back to the file `write` leaves, with the very time the failed read saw: only a retry shows it.
fn unlock(l: &Layout, locked: SystemTime, write: impl FnOnce()) {
    let file = l.data_dir.join(FILE);
    fs::remove_dir(&file).unwrap();
    write();
    fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(locked)
        .unwrap();
}

#[test]
fn a_locked_file_keeps_the_open_chart_and_the_poll_retries() {
    let l = layout("trend-locked", None);
    write_days(&l, 2);
    let (ipc, _rx) = Ipc::fake();
    let window = open_linked(&l, Page::Home, ipc);
    click_words(&window);
    assert_eq!(chart(&window).unwrap(), "2 days, highest 5");

    let locked = lock(&l);
    poll();
    assert_eq!(chart(&window).unwrap(), "2 days, highest 5", "kept");

    unlock(&l, locked, || write_days(&l, 3));
    poll();
    assert_eq!(chart(&window).unwrap(), "3 days, highest 5", "retried");
}

#[test]
fn a_card_opened_on_a_locked_file_draws_its_own_bars_and_retries() {
    let l = layout("trend-locked-click", None);
    write_days(&l, 3);
    let window = open(&l, Page::Home);
    click_words(&window);

    let locked = lock(&l);
    click_card(&window, "Current streak: 3 days, Longest: 3 days");
    assert_eq!(
        chart(&window).unwrap(),
        "3 days, highest 3",
        "the last read"
    );

    unlock(&l, locked, || write_days(&l, 4));
    poll();
    assert_eq!(chart(&window).unwrap(), "4 days, highest 4", "retried");
}

#[test]
fn a_new_number_leaves_the_focus_on_the_card() {
    let (ipc, _rx) = Ipc::fake();
    let window = open_linked(&layout("focus", None), Page::Home, ipc.clone());
    stats_push(&ipc, sample());
    // Backwards from the end of the tab order: the folder button, the activity "i", then the
    // last card.
    for _ in 0..3 {
        press(&window, Key::Backtab);
    }
    stats_push(
        &ipc,
        Summary {
            words: 300_000,
            streak: 7,
            ..sample()
        },
    );
    assert!(!has(&window, "Streak by day"));
    press(&window, " ");
    assert!(has(&window, "Streak by day"), "the card lost the focus");
}

/// The folder button's tooltip (not readable through accessibility) says what the folder holds
/// by this flag: it follows the config's history switch.
#[test]
fn the_folder_tip_follows_history() {
    i_slint_backend_testing::init_no_event_loop();
    // History is on by default, so the off case writes `enabled = false` explicitly (R1.3).
    let off = "version = 2\n[history]\nenabled = false\n";
    let window = common::open_again(&layout("disclaimer", Some(off)), Page::Home);
    assert!(!window.global::<oxi_settings::HomeState>().get_history_on());

    let config = "version = 2\n[history]\nenabled = true\n";
    let window = common::open_again(&layout("disclaimer-on", Some(config)), Page::Home);
    assert!(window.global::<oxi_settings::HomeState>().get_history_on());
}

/// With the data directory gone there is nothing to open, so no test ever opens Explorer: the
/// worker's answer ends in the error line. (`history_target`'s unit test covers what is picked.)
fn click_with_nothing_to_open(l: &Layout, window: &AppWindow) {
    fs::remove_dir_all(&l.data_dir).unwrap();
    assert_eq!(window.get_action_error(), "");
    click_card(window, "History folder");
    for _ in 0..400 {
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(30));
        if window.get_action_error() != "" {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the folder check did not answer");
}

#[test]
fn the_folder_button_creates_nothing_for_the_default_folder() {
    let l = layout("folder-default", None);
    let window = open(&l, Page::Home);
    click_with_nothing_to_open(&l, &window);
    assert!(!l.history_dir().exists() && !l.data_dir.exists());
}

#[test]
fn the_folder_button_creates_nothing_for_the_chosen_folder() {
    let l = layout("folder-chosen", None);
    let chosen = l.data_dir.join("chosen");
    fs::write(
        l.config_file(),
        format!(
            "version = 2\n[history]\nfolder = {:?}\n",
            chosen.display().to_string()
        ),
    )
    .unwrap();
    let window = open(&l, Page::Home);
    click_with_nothing_to_open(&l, &window);
    assert!(!chosen.exists() && !l.data_dir.exists());
}

/// UI rule 25: at 720 x 520 Home fits whole in every content state and the page does not scroll.
#[test]
fn the_page_fits_the_minimum_window_without_scrolling() {
    let (ipc, _rx) = Ipc::fake();
    let window = open_linked(&layout("min-fit", None), Page::Home, ipc.clone());
    window
        .window()
        .set_size(slint::LogicalSize::new(720.0, 520.0));
    let whole = |window: &AppWindow| {
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        let button = ElementHandle::find_by_accessible_label(window, "History folder")
            .next()
            .expect("the folder button");
        let (at, size) = (button.absolute_position(), button.size());
        assert!(
            at.y + size.height <= 520.0,
            "the folder button ends at {}",
            at.y + size.height
        );
        let right = if has(window, "No dictations yet") {
            common::one(window, "No dictations yet")
        } else {
            heatmap(window)
        };
        assert!(right.absolute_position().x + right.size().width <= 720.0);
        assert_eq!(window.get_scroll_y(), 0.0);
    };
    whole(&window); // empty: the empty state
    stats_push(&ipc, sample());
    whole(&window); // with data
}

#[test]
fn twenty_six_weeks_fit_the_minimum_window() {
    let (ipc, _rx) = Ipc::fake();
    let window = open_linked(&layout("fits", None), Page::Home, ipc.clone());
    window
        .window()
        .set_size(slint::LogicalSize::new(720.0, 900.0));
    stats_push(
        &ipc,
        Summary {
            heat: vec![1; 26 * 7],
            ..sample()
        },
    );
    let map = heatmap(&window);
    let right = map.absolute_position().x + map.size().width;
    assert!(right <= 720.0 - 16.0, "the grid ends at {right}");
    assert_eq!(
        map.accessible_label().unwrap(),
        format!("{} active days in the last 26 weeks", 26 * 7)
    );
}

/// DS-28.6: a wide window always shows the last 26 weeks, never more; the cells grow to their
/// 36 px cap and the grid stays centred in the card.
#[test]
fn a_wide_window_shows_half_a_year_in_larger_cells() {
    fills_the_card("half", &[(1280.0, 860.0), (1920.0, 1080.0)]);
}

/// The same at a maximised 2560 x 1440: the capped grid stays inside the card.
#[test]
fn a_maximised_window_shows_half_a_year() {
    fills_the_card("half-max", &[(2560.0, 1440.0)]);
}

/// A wide, low window: the height limits the cells, and the grid still ends in the card with the
/// page unscrolled and inside the window.
#[test]
fn a_wide_low_window_holds_half_a_year_without_overflow() {
    fills_the_card("half-low", &[(2560.0, 700.0), (3440.0, 720.0)]);
}

/// The cell's height room never overshoots: from the lowest to a tall window, at 1920 to 3840 wide, a year in a
/// window never pushes the page past the window (a 1 px overshoot fails at 3840 x 814).
#[test]
fn a_year_never_overflows_at_any_height() {
    let (ipc, _rx) = Ipc::fake();
    let window = open_linked(&layout("year-sweep", None), Page::Home, ipc.clone());
    window
        .window()
        .set_size(slint::LogicalSize::new(2560.0, 520.0));
    settle();
    stats_push(
        &ipc,
        Summary {
            heat: vec![1; 53 * 7],
            ..sample()
        },
    );
    for w in [1920.0, 2560.0, 3440.0, 3840.0] {
        for h in (520..=1100).step_by(3).chain([1440]) {
            let h = h as f32;
            window.window().set_size(slint::LogicalSize::new(w, h));
            settle();
            ends_inside(&window, &heatmap(&window), w, h);
        }
    }
}

/// The Slint year width is oxi-core's.
#[test]
fn the_year_in_the_theme_is_the_core_year() {
    let window = open(&layout("year-theme", None), Page::Home);
    assert_eq!(
        window
            .global::<oxi_settings::Tokens>()
            .get_heat_year_weeks(),
        i32::from(oxi_core::stats::HEAT_WEEKS)
    );
}

fn fills_the_card(name: &str, sizes: &[(f32, f32)]) {
    let (ipc, _rx) = Ipc::fake();
    let window = open_linked(&layout(name, None), Page::Home, ipc.clone());
    for &(w, h) in sizes {
        window.window().set_size(slint::LogicalSize::new(w, h));
        settle();
        stats_push(
            &ipc,
            Summary {
                heat: vec![1; 53 * 7],
                ..sample()
            },
        );
        settle();
        let map = heatmap(&window);
        let label = map.accessible_label().unwrap().to_string();
        let shown: usize = label.rsplit(' ').nth(1).unwrap().parse().unwrap();
        assert_eq!(shown, 26, "{w}: half a year, whatever the width: {label}");
        // The activity card is as wide as the row of stat cards: the grid ends inside it.
        let last = ElementHandle::find_by_accessible_label(&window, CARDS[3])
            .next()
            .expect("the streak card");
        let card_right = last.absolute_position().x + last.size().width;
        let right = map.absolute_position().x + map.size().width;
        assert!(
            right <= card_right,
            "{w}: the grid ends at {right}, the card at {card_right}"
        );
        // The cells grow to their cap when the window is big enough, and never past it.
        let t = window.global::<oxi_settings::Tokens>();
        let cell = (map.size().width - t.get_heat_label_width()) / 26.0 - t.get_heat_gap();
        assert!(cell <= t.get_heat_cell_max(), "{w}x{h}: cell {cell}");
        if w >= 1920.0 && h >= 1080.0 {
            assert_eq!(cell, t.get_heat_cell_max(), "{w}x{h}: cells at their cap");
        }
        // Centred in the card's inner width (the stat cards row spans the same width).
        let first = ElementHandle::find_by_accessible_label(&window, CARDS[0])
            .next()
            .expect("the first card");
        let gap_left =
            map.absolute_position().x - (first.absolute_position().x + t.get_card_padding_h());
        let gap_right = (card_right - t.get_card_padding_h()) - right;
        assert!(
            (gap_left - gap_right).abs() <= 1.0,
            "{w}x{h}: the grid is off-centre: {gap_left} left, {gap_right} right"
        );
        ends_inside(&window, &map, w, h);
    }
}

/// The grid starts below the folder button (in the card's title row) and ends inside the window:
/// the page does not overflow or scroll.
fn ends_inside(window: &AppWindow, map: &ElementHandle, w: f32, h: f32) {
    let button = ElementHandle::find_by_accessible_label(window, "History folder")
        .next()
        .expect("the folder button");
    let top = map.absolute_position().y;
    assert!(
        button.absolute_position().y + button.size().height <= top,
        "{w}x{h}: the grid starts at {top}, the button ends lower"
    );
    let end = top + map.size().height;
    // The grid plus the page's bottom padding fit the window, or the page scrolls.
    let pad = window.global::<oxi_settings::Tokens>().get_page_padding();
    assert!(end + pad <= h, "{w}x{h}: the page ends at {}", end + pad);
    assert_eq!(window.get_scroll_y(), 0.0, "{w}x{h} scrolled");
}
