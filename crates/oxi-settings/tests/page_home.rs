//! Home page: the numbers from the daemon or the file, the empty state and the reflow.

mod common;

use std::fs;
use std::sync::mpsc::Receiver;

use common::{has, layout, open, open_linked};
use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery};
use oxi_core::ipc::{Push, Request};
use oxi_core::paths::Layout;
use oxi_core::stats::{Date, FILE, Stats, Summary};
use oxi_core::types::Millis;
use oxi_platform::stats_store;
use oxi_settings::ipc::{Cmd, Event, Link};
use oxi_settings::{AppWindow, FILE_POLL, Ipc, Page};
use slint::ComponentHandle;

const CHORD: &str = "version = 1\n[hotkey]\nchord = [\"LCtrl\", \"F13\"]\n";
const TOGGLE: &str = "version = 1
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
    std::thread::sleep(std::time::Duration::from_millis(20));
    write_stats(&l, 43);
    poll();
    assert_eq!(asked(&rx), 1, "the file changed");
}

#[test]
fn no_data_says_to_hold_the_current_chord() {
    let window = open(&layout("empty", Some(CHORD)), Page::Home);
    assert!(has(&window, "Hold LCtrl + F13 and speak"));
    assert!(has(&window, "Current streak: 0 days, Longest: 0 days"));
    assert!(has(
        &window,
        "Words per minute: 0, Based on recorded speech"
    ));
}

#[test]
fn toggle_mode_says_press_not_hold() {
    let window = open(&layout("empty-toggle", Some(TOGGLE)), Page::Home);
    assert!(has(
        &window,
        "Press LCtrl + F13 once to start, again to stop"
    ));
    assert!(!has(&window, "Hold LCtrl + F13 and speak"));
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
    let file = l.data_dir.join(FILE);
    write_stats(&l, 42);
    let window = open(&l, Page::Home);
    assert!(has(&window, "Words spoken: 42, All time"));

    // A directory in the file's place: reading it fails with something other than NotFound.
    fs::remove_file(&file).unwrap();
    fs::create_dir(&file).unwrap();
    let locked = fs::metadata(&file).unwrap().modified().unwrap();
    poll();
    assert!(
        has(&window, "Words spoken: 42, All time"),
        "kept, not zeroed"
    );

    // Back to a readable file with the very time the failed read saw: only a retry shows it.
    fs::remove_dir(&file).unwrap();
    write_stats(&l, 43);
    fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(locked)
        .unwrap();
    poll();
    assert!(has(&window, "Words spoken: 43, All time"));
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
