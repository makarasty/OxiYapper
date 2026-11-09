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

#[test]
fn no_data_says_to_hold_the_current_chord() {
    let window = open(&layout("empty", Some(CHORD)), Page::Home);
    assert!(has(&window, "Hold Ctrl + F13 and speak"));
    assert!(has(&window, "Current streak: 0 days, Longest: 0 days"));
    assert!(has(
        &window,
        "Words per minute: 0, Based on recorded speech"
    ));
}

/// UI rules 1 and 5: the first dictation does not shrink the activity card, so the History folder
/// row under it stays where it was.
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
    let chord = "version = 1\n[hotkey]\nchord = [\"LCtrl\", \"BracketLeft\"]\n";
    let window = open(&layout("empty-label", Some(chord)), Page::Home);
    assert!(has(&window, "Hold Ctrl + [ and speak"));
}

#[test]
fn toggle_mode_says_press_not_hold() {
    let window = open(&layout("empty-toggle", Some(TOGGLE)), Page::Home);
    assert!(has(
        &window,
        "Press Ctrl + F13 once to start, again to stop"
    ));
    assert!(!has(&window, "Hold Ctrl + F13 and speak"));
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
        (0, "0 days, highest 0"),
        (1, "1 day, highest 5"),
        (366, "365 days, highest 5"),
    ] {
        let l = layout(&format!("trend-{days}"), None);
        write_days(&l, days);
        let window = common::open_again(&l, Page::Home);
        click_words(&window);
        assert_eq!(chart(&window).unwrap(), want);
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
            assert!(chart(&window).is_some());
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
    assert_eq!(chart(&window).unwrap(), "0 days, highest 0");
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
    // Backwards from the end of the tab order: the folder button, then the last card.
    for _ in 0..2 {
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

const OFF_NOTE: &str =
    "Statistics keep only dates, word and dictation counts and speech time, never the text itself.";
const ON_NOTE: &str = "Statistics keep only dates, word and dictation counts and speech time. History is on, so its folder also holds your dictated text and audio.";

#[test]
fn the_disclaimer_follows_history() {
    i_slint_backend_testing::init_no_event_loop();
    let window = common::open_again(&layout("disclaimer", None), Page::Home);
    assert!(has(&window, OFF_NOTE));
    assert!(!has(&window, ON_NOTE));

    let config = "version = 1\n[history]\nenabled = true\n";
    let window = common::open_again(&layout("disclaimer-on", Some(config)), Page::Home);
    assert!(has(&window, ON_NOTE));
    assert!(!has(&window, OFF_NOTE));
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
            "version = 1\n[history]\nfolder = {:?}\n",
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
        let right = heatmap(window);
        assert!(right.absolute_position().x + right.size().width <= 720.0);
        assert_eq!(window.get_scroll_y(), 0.0);
    };
    whole(&window); // empty: the hint is on the card
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
