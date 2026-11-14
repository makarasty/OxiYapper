//! Every settings screen drawn by the software renderer, at the default size and at the minimum
//! size, light and dark. With `OXI_SHOTS_DIR` set, each lands in
//! `settings-<screen>-<width>-<theme>.png` for visual review.

mod common;

use std::cell::RefCell;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use oxi_core::device::Device;
use oxi_core::hotkey::Key;
use oxi_core::ipc::{DownloadResult, Push};
use oxi_core::paths::Layout;
use oxi_core::stats::{Date, Stats, Summary};
use oxi_settings::constants::{DEFAULT, MIN};
use oxi_settings::ipc::{Event, Link};
use oxi_settings::{
    AppWindow, AudioState, Ipc, ModelState, Page, SystemState, TextState, ThemeChoice,
};
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Platform, WindowAdapter, WindowEvent};
use slint::{Color, ComponentHandle, PhysicalSize, PlatformError};
use tiny_skia::{IntSize, Pixmap};

const SIZES: [(f32, f32); 2] = [DEFAULT, MIN];
const THEMES: [(&str, ThemeChoice); 2] =
    [("light", ThemeChoice::Light), ("dark", ThemeChoice::Dark)];
const PAGES: [(&str, Page); 10] = [
    ("home", Page::Home),
    ("hotkey", Page::Hotkey),
    ("audio", Page::Audio),
    ("model", Page::Model),
    ("text", Page::Text),
    ("paste", Page::Paste),
    ("overlay", Page::Overlay),
    ("test", Page::Test),
    ("system", Page::System),
    // System > About (ia-map 3.1): the old page value opens that tab.
    ("about", Page::About),
];

thread_local! {
    /// The window of the component created last on this thread.
    static LAST: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
}

struct Shots;

thread_local! {
    /// Time the test clock has skipped ahead of the real one.
    static SKIPPED: std::cell::Cell<std::time::Duration> = const { std::cell::Cell::new(std::time::Duration::ZERO) };
}

/// Moves the clock of timers and animations on by `by`, with no real wait.
fn advance(by: std::time::Duration) {
    SKIPPED.set(SKIPPED.get() + by);
}

thread_local! {
    /// The size `matrix` renders next; the window takes it when created, because the element
    /// search finds only what is in view.
    static SIZE: std::cell::Cell<(f32, f32)> = const { std::cell::Cell::new(DEFAULT) };
}

impl Platform for Shots {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        let (w, h) = SIZE.get();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // whole positive pixels
        window.set_size(PhysicalSize::new(w as u32, h as u32));
        LAST.with_borrow_mut(|last| *last = Some(window.clone()));
        Ok(window)
    }

    // Timers (the model page's path check) run on this clock; the default one stands still.
    // `advance` moves it on without a real wait.
    fn duration_since_start(&self) -> std::time::Duration {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        START.get_or_init(std::time::Instant::now).elapsed() + SKIPPED.get()
    }
}

fn open(screen: &str, config: Option<&str>) -> AppWindow {
    open_with(screen, config, |_| {})
}

/// `prepare` runs on the fresh layout before the window reads it.
fn open_with(screen: &str, config: Option<&str>, prepare: fn(&Layout)) -> AppWindow {
    // Once per thread; a second call on the same thread is the same platform.
    let _ = slint::platform::set_platform(Box::new(Shots));
    let layout = common::layout(screen, config);
    prepare(&layout);
    oxi_settings::open(layout).unwrap()
}

/// `window` at `size` logical px and `scale`.
fn render(window: &AppWindow, size: (f32, f32), scale: f32) -> Pixmap {
    let surface = LAST.with_borrow(Clone::clone).unwrap();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // whole positive pixels
    let (w, h) = ((size.0 * scale) as u32, (size.1 * scale) as u32);
    surface.dispatch_event(WindowEvent::ScaleFactorChanged {
        scale_factor: scale,
    });
    surface.set_size(PhysicalSize::new(w, h));
    window.show().unwrap();
    // Runs the `changed` handlers (the theme) and settles animations.
    slint::platform::update_timers_and_animations();
    surface.request_redraw();
    let mut buffer = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
    assert!(surface.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, w as usize);
    }));
    let bytes = buffer
        .iter()
        .flat_map(|p| [p.red, p.green, p.blue, p.alpha])
        .collect();
    Pixmap::from_vec(bytes, IntSize::from_wh(w, h).unwrap()).unwrap()
}

fn save(pixmap: &Pixmap, name: &str) {
    let pixels = pixmap.pixels();
    assert!(pixels.iter().any(|p| p.alpha() > 0), "{name} drew nothing");
    assert!(
        pixels.iter().any(|p| *p != pixels[0]),
        "{name} is one flat colour"
    );
    if let Some(dir) = std::env::var_os("OXI_SHOTS_DIR") {
        let dir = PathBuf::from(dir);
        fs::create_dir_all(&dir).unwrap();
        pixmap
            .save_png(dir.join(format!("settings-{name}.png")))
            .unwrap();
    }
}

/// `screen` at both sizes in both themes; `make` opens the window in the theme it gets.
fn matrix(screen: &str, make: impl Fn(ThemeChoice) -> AppWindow) {
    for size in SIZES {
        for (theme, choice) in THEMES {
            SIZE.set(size);
            let window = make(choice);
            save(
                &render(&window, size, 1.0),
                &format!("{screen}-{}-{theme}", size.0),
            );
        }
    }
}

fn shot(screen: &str, page: Page, config: Option<&str>) {
    shot_with(screen, page, config, |_| {});
}

fn shot_with(screen: &str, page: Page, config: Option<&str>, prepare: fn(&Layout)) {
    matrix(screen, |choice| {
        let window = open_with(screen, config, prepare);
        window.set_page(page);
        window.set_theme(choice);
        window
    });
}

const NEWER: &str = "version = 99\n";
const DENOISE: &str = "version = 2
[audio]
denoise = true
";
const BROKEN: &str = "version = 2\n[audio]\ngain_db = \n";

#[test]
fn shot_main() {
    shot("main", Page::Hotkey, None);
}

#[test]
fn shot_main_scaled() {
    let window = open("main-1.5x", None);
    let pixmap = render(&window, DEFAULT, 1.5);
    assert_eq!((pixmap.width(), pixmap.height()), (1920, 1200));
    save(&pixmap, "main-1.5x-system");
}

#[test]
fn shot_pages_scaled() {
    for (screen, page) in PAGES {
        let window = open("pages-1.5x", None);
        window.set_page(page);
        window.set_theme(ThemeChoice::Light);
        save(
            &render(&window, DEFAULT, 1.5),
            &format!("{screen}-1.5x-light"),
        );
    }
}

#[test]
fn shot_large() {
    // The window's maximum: the column fills it and the keyboard has no width of its own.
    for (theme, choice) in THEMES {
        for (screen, page) in [("audio", Page::Audio), ("hotkey", Page::Hotkey)] {
            let window = open("large", None);
            window.set_page(page);
            window.set_theme(choice);
            save(
                &render(&window, (1280.0, 860.0), 1.0),
                &format!("{screen}-max-{theme}"),
            );
        }
    }
}

#[test]
fn shot_focused_nav() {
    // Tab, then Down twice: the keyboard focus ring sits on Model, Hotkey stays selected.
    use slint::platform::Key as K;
    for (theme, choice) in THEMES {
        for size in SIZES {
            let window = open("focused-nav", None);
            window.set_page(Page::Hotkey);
            window.set_theme(choice);
            let _ = render(&window, size, 1.0);
            let surface = LAST.with_borrow(Clone::clone).unwrap();
            for key in [K::Tab, K::DownArrow, K::DownArrow] {
                surface.dispatch_event(WindowEvent::KeyPressed { text: key.into() });
                surface.dispatch_event(WindowEvent::KeyReleased { text: key.into() });
            }
            save(
                &render(&window, size, 1.0),
                &format!("focused-nav-{}-{theme}", size.0),
            );
        }
    }
}

/// UI rule 17: focus follows the update confirm swap, the ring only if the keyboard did it.
#[test]
fn update_confirm_rings_keep_checking_for_the_keyboard_only() {
    use slint::platform::{Key as K, PointerEventButton};
    const TALL: (f32, f32) = (1280.0, 1600.0);
    SIZE.set(TALL);
    // The pixel on the ring's band (2 px gap, then a 2 px band) against the card just outside it,
    // left of the button.
    let ringed = |window: &AppWindow| {
        let keep = common::with_role(window, "Keep checking", AccessibleRole::Button);
        let (at, height) = (keep.absolute_position(), keep.size().height);
        let pixmap = render(window, TALL, 1.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // whole positive pixels
        let pixel = |x: f32| {
            pixmap
                .pixel(x as u32, (at.y + height / 2.0) as u32)
                .unwrap()
        };
        pixel(at.x - 3.0) != pixel(at.x - 6.0)
    };
    let press = |text: slint::SharedString| {
        let surface = LAST.with_borrow(Clone::clone).unwrap();
        surface.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        surface.dispatch_event(WindowEvent::KeyReleased { text });
    };

    let window = open("swap-key", None);
    window.set_page(Page::System);
    let _ = render(&window, TALL, 1.0);
    // Tab to the Check for updates switch: the stops before it (the rail, the sub-tabs, the theme
    // tiles, the "i" tips) are not counted, its own ring says it has the focus.
    let switch = common::with_role(&window, "Check for updates", AccessibleRole::Switch);
    let (at, size) = (switch.absolute_position(), switch.size());
    // The ring is the only change in the switch's box (and 8 px around it) since the first frame.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // whole positive pixels
    let around = |window: &AppWindow| {
        let pixmap = render(window, TALL, 1.0);
        let (x0, x1) = ((at.x - 8.0) as u32, (at.x + size.width + 8.0) as u32);
        let (y0, y1) = ((at.y - 8.0) as u32, (at.y + size.height + 8.0) as u32);
        let mut seen = Vec::new();
        for y in y0..y1 {
            for x in x0..x1 {
                let p = pixmap.pixel(x, y).unwrap();
                seen.push((p.red(), p.green(), p.blue()));
            }
        }
        seen
    };
    let resting = around(&window);
    let mut tabs = 0;
    while around(&window) == resting {
        assert!(tabs < 60, "Tab never reaches the Check for updates switch");
        press(K::Tab.into());
        tabs += 1;
    }
    press(" ".into());
    assert!(ringed(&window), "keyboard swap: the ring moves along");

    let window = open("swap-mouse", None);
    window.set_page(Page::System);
    let _ = render(&window, TALL, 1.0);
    let switch = common::with_role(&window, "Check for updates", AccessibleRole::Switch);
    let (at, size) = (switch.absolute_position(), switch.size());
    let position = slint::LogicalPosition::new(at.x + size.width / 2.0, at.y + size.height / 2.0);
    let surface = LAST.with_borrow(Clone::clone).unwrap();
    let button = PointerEventButton::Left;
    surface.dispatch_event(WindowEvent::PointerPressed { position, button });
    surface.dispatch_event(WindowEvent::PointerReleased { position, button });
    assert!(!ringed(&window), "mouse swap: no ring");
    // It holds the focus all the same.
    press(" ".into());
    assert!(!common::has(&window, "Keep checking"));
}

/// UI rule 16: the update confirm's "Turn off" is the risky choice and fills red under the pointer.
#[test]
fn update_confirm_turn_off_turns_red_on_hover() {
    const TALL: (f32, f32) = (1280.0, 1600.0);
    SIZE.set(TALL);
    let window = open("turn-off-red", None);
    window.set_page(Page::System);
    let _ = render(&window, TALL, 1.0);
    common::with_role(&window, "Check for updates", AccessibleRole::Switch)
        .invoke_accessible_default_action();
    let _ = render(&window, TALL, 1.0);
    let button = common::with_role(&window, "Turn off update checks", AccessibleRole::Button);
    let (at, size) = (button.absolute_position(), button.size());
    let red = window.global::<oxi_settings::Tokens>().get_error_fill();
    // Inside the button, left of its label.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // whole positive pixels
    let (x, y) = ((at.x + 6.0) as u32, (at.y + size.height / 2.0) as u32);
    let is_red = |window: &AppWindow| {
        let p = render(window, TALL, 1.0).pixel(x, y).unwrap();
        (p.red(), p.green(), p.blue()) == (red.red(), red.green(), red.blue())
    };
    assert!(!is_red(&window), "red at rest");
    let position = slint::LogicalPosition::new(at.x + size.width / 2.0, at.y + size.height / 2.0);
    let surface = LAST.with_borrow(Clone::clone).unwrap();
    surface.dispatch_event(WindowEvent::PointerMoved { position });
    // The fill eases in over `motion-quick`: wait for the colour, not for a time.
    let end = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !is_red(&window) {
        assert!(std::time::Instant::now() < end, "no red under the pointer");
    }
}

/// UI rule 17 on the Overlay tiles: a mouse click on a tile draws no ring, Tab does.
#[test]
fn overlay_tile_ring_is_for_the_keyboard_only() {
    use slint::platform::PointerEventButton;
    // The ring's band just above the tile (2 px out, 2 px wide) against the row gap beyond it.
    let ringed = |window: &AppWindow| {
        let tile = common::with_role(window, "Bottom right", AccessibleRole::RadioButton);
        let (at, size) = (tile.absolute_position(), tile.size());
        let pixmap = render(window, DEFAULT, 1.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // whole positive pixels
        let pixel = |y: f32| {
            pixmap
                .pixel((at.x + size.width / 2.0) as u32, y as u32)
                .unwrap()
        };
        pixel(at.y - 3.0) != pixel(at.y - 6.0)
    };
    let surface = || LAST.with_borrow(Clone::clone).unwrap();

    let window = open("overlay-ring-key", None);
    window.set_page(Page::Overlay);
    let _ = render(&window, DEFAULT, 1.0);
    let reached = (0..30).any(|_| {
        let text = slint::SharedString::from(slint::platform::Key::Tab);
        surface().dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        surface().dispatch_event(WindowEvent::KeyReleased { text });
        ringed(&window)
    });
    assert!(reached, "Tab never drew the ring on the picked tile");

    let window = open("overlay-ring-mouse", None);
    window.set_page(Page::Overlay);
    let _ = render(&window, DEFAULT, 1.0);
    let tile = common::with_role(&window, "Bottom right", AccessibleRole::RadioButton);
    let (at, size) = (tile.absolute_position(), tile.size());
    let position = slint::LogicalPosition::new(at.x + size.width / 2.0, at.y + size.height / 2.0);
    let button = PointerEventButton::Left;
    surface().dispatch_event(WindowEvent::PointerPressed { position, button });
    surface().dispatch_event(WindowEvent::PointerReleased { position, button });
    assert!(!ringed(&window), "a mouse click drew the ring");
}

/// Read-only config: the Position title and description dim like the switches' rows (rule 31).
#[test]
fn overlay_position_row_dims_when_the_config_is_read_only() {
    // Pixels of the "Position" title's box in the text colour.
    let solid = |name: &str, config: Option<&str>| {
        let window = open(name, config);
        window.set_page(Page::Overlay);
        let pixmap = render(&window, DEFAULT, 1.0);
        let primary = window.global::<oxi_settings::Tokens>().get_text_primary();
        let title = ElementHandle::find_by_accessible_label(&window, "Position")
            .find(|e| e.accessible_role() == Some(AccessibleRole::Text))
            .expect("the Position title");
        let (at, size) = (title.absolute_position(), title.size());
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // whole positive pixels
        let (x0, y0, x1, y1) = (
            at.x as u32,
            at.y as u32,
            (at.x + size.width) as u32,
            (at.y + size.height) as u32,
        );
        (y0..y1)
            .flat_map(|y| (x0..x1).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let p = pixmap.pixel(x, y).unwrap();
                (p.red(), p.green(), p.blue()) == (primary.red(), primary.green(), primary.blue())
            })
            .count()
    };
    assert!(
        solid("overlay-dim-open", None) > 0,
        "the title has no text-colour pixels"
    );
    assert_eq!(
        solid("overlay-dim-locked", Some(NEWER)),
        0,
        "the title stays full colour"
    );
}

/// The "!" of a row, reached by Tab: its ring, and its tooltip laid over the window.
#[test]
fn shot_warning_focused() {
    use slint::platform::Key as K;
    const SIZE_HERE: (f32, f32) = (1280.0, 1000.0);
    SIZE.set(SIZE_HERE);
    for (theme, choice) in THEMES {
        let window = open(
            "warning-focused",
            Some(
                "[debug]
log_text = true
",
            ),
        );
        window.set_page(Page::System);
        window.global::<SystemState>().set_tab(1);
        window.set_theme(choice);
        let _ = render(&window, SIZE_HERE, 1.0);
        let surface = LAST.with_borrow(Clone::clone).unwrap();
        // The rail, the sub-tabs, the two recording limits and their switch, the two folder
        // buttons, Settings, Logs, the "!".
        for _ in 0..10 {
            surface.dispatch_event(WindowEvent::KeyPressed {
                text: K::Tab.into(),
            });
            surface.dispatch_event(WindowEvent::KeyReleased {
                text: K::Tab.into(),
            });
        }
        let _ = render(&window, SIZE_HERE, 1.0);
        save(
            &render(&window, SIZE_HERE, 1.0),
            &format!("warning-focused-{theme}"),
        );
    }
}

fn purple_pixels(pixmap: &Pixmap) -> usize {
    pixmap
        .pixels()
        .iter()
        .map(tiny_skia::PremultipliedColorU8::demultiply)
        .filter(|p| common::purple(p.red(), p.green(), p.blue()))
        .count()
}

const PURPLE: Color = Color::from_rgb_u8(0x87, 0x64, 0xB8);
const GREEN: Color = Color::from_rgb_u8(0x2E, 0x7D, 0x32);

#[test]
fn no_system_accent_on_any_page() {
    // Windows reports a purple accent before the window opens: the window puts its teal over it.
    // The first window sets up Slint, which the accent call needs.
    drop(open("purple-setup", None));
    for (theme, choice) in THEMES {
        for (screen, page) in PAGES {
            i_slint_backend_testing::set_system_accent_color(PURPLE);
            let window = open("purple", None);
            window.set_page(page);
            window.set_theme(choice);
            let pixmap = render(&window, DEFAULT, 1.0);
            save(&pixmap, &format!("purple-{screen}-{theme}"));
            let leaks = purple_pixels(&pixmap);
            // The accent picker's Iris and Purple swatches (DS-20.7) are purple on purpose: a leak
            // is a purple count that follows the system accent, so compare with a green one.
            i_slint_backend_testing::set_system_accent_color(GREEN);
            let window = open("purple", None);
            window.set_page(page);
            window.set_theme(choice);
            let baseline = purple_pixels(&render(&window, DEFAULT, 1.0));
            assert_eq!(
                leaks,
                baseline,
                "{} purple pixels follow the system accent on {screen} ({theme})",
                leaks.abs_diff(baseline)
            );
        }
    }
}

/// Home with `summary` from a fake daemon, at the narrowest, default and widest sizes, and 960:
/// the first width with the four stat cards in one row, where their texts are tightest.
fn shot_home(screen: &str, summary: &Summary) {
    for size in [(720.0, 720.0), (960.0, 720.0), DEFAULT, (1600.0, 1000.0)] {
        for (theme, choice) in THEMES {
            SIZE.set(size);
            let _ = slint::platform::set_platform(Box::new(Shots));
            let (ipc, _sent) = Ipc::fake();
            let layout = common::layout(screen, None);
            let window = oxi_settings::open_with(layout, ipc.clone()).unwrap();
            window.set_theme(choice);
            window.show().unwrap();
            push(&ipc, Push::Stats(summary.clone()));
            save(
                &render(&window, size, 1.0),
                &format!("{screen}-{}-{theme}", size.0),
            );
        }
    }
}

#[test]
fn shot_home_sample() {
    // Big numbers, a long streak, every level, a partial last week.
    let heat = (0..178_u32)
        .map(|i| u8::try_from((i * i + i / 3) % 5).unwrap())
        .collect();
    shot_home(
        "home",
        &Summary {
            dictations: 11_300,
            words: 299_900,
            wpm: 146,
            streak: 123,
            longest_streak: 1040,
            heat_start: Date::from_ymd(2026, 5, 11).unwrap(),
            heat,
        },
    );
}

#[test]
fn shot_home_empty() {
    let today = Date::from_ymd(2026, 11, 4).unwrap();
    shot_home("home-empty", &Stats::default().summary(today, 26));
}

/// Home with the words chart open over no history, one day and a full year.
#[test]
fn shot_home_trend() {
    let today = oxi_platform::stats_store::today();
    for (screen, days) in [("zero", 0), ("one", 1), ("year", 365)] {
        for size in [(720.0, 720.0), DEFAULT, (1600.0, 1000.0)] {
            for (theme, choice) in THEMES {
                SIZE.set(size);
                let _ = slint::platform::set_platform(Box::new(Shots));
                let layout = common::layout(&format!("home-trend-{screen}"), None);
                let mut stats = Stats::default();
                for back in (0..days).rev() {
                    let words = u64::try_from((back * back + 7 * back) % 900 + 40).unwrap();
                    stats.record(
                        today.add_days(-back),
                        words,
                        oxi_core::types::Millis(60_000),
                    );
                }
                fs::write(
                    layout.data_dir.join(oxi_core::stats::FILE),
                    stats.to_json().unwrap(),
                )
                .unwrap();
                let (ipc, _sent) = Ipc::fake();
                let window = oxi_settings::open_with(layout, ipc).unwrap();
                window.set_theme(choice);
                window.show().unwrap();
                let card = i_slint_backend_testing::ElementQuery::from_root(&window)
                    .match_descendants()
                    .match_accessible_role(AccessibleRole::Button)
                    .find_all()
                    .into_iter()
                    .find(|e| {
                        e.accessible_label()
                            .is_some_and(|l| l.starts_with("Words spoken"))
                    })
                    .unwrap();
                card.invoke_accessible_default_action();
                // Past the bars' reveal, or the shot is its first, empty frame; the first frame
                // starts it.
                let _ = render(&window, size, 1.0);
                advance(std::time::Duration::from_millis(600));
                save(
                    &render(&window, size, 1.0),
                    &format!("home-trend-{screen}-{}-{theme}", size.0),
                );
            }
        }
    }
}

#[test]
fn shot_home_history_on() {
    // Taller than the minimum: the two-line disclaimer sits at the page's foot.
    let window = open(
        "home-history-on",
        Some("version = 2\n[history]\nenabled = true\n"),
    );
    save(&render(&window, (720.0, 900.0), 1.0), "home-history-on-720");
}

/// The real fill of a stat card: at rest, hovered, pressed and open all differ.
#[test]
fn a_stat_card_fills_for_each_state() {
    let window = open("home-card-states", None);
    let _ = render(&window, DEFAULT, 1.0);
    let card = i_slint_backend_testing::ElementQuery::from_root(&window)
        .match_descendants()
        .match_accessible_role(AccessibleRole::Button)
        .find_all()
        .into_iter()
        .find(|e| {
            e.accessible_label()
                .is_some_and(|l| l.starts_with("Words spoken"))
        })
        .unwrap();
    let (at, size) = (card.absolute_position(), card.size());
    // In the card's top padding: nothing but the fill is drawn there.
    let (x, y) = (at.x + size.width / 2.0, at.y + 6.0);
    let fill = || {
        // The fill fades over motion-quick on the clock: skip past it.
        advance(std::time::Duration::from_millis(150));
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // whole positive pixels
        let pixel = render(&window, DEFAULT, 1.0)
            .pixel(x as u32, y as u32)
            .unwrap();
        (pixel.red(), pixel.green(), pixel.blue())
    };
    let position = slint::LogicalPosition::new(x, y);
    let button = slint::platform::PointerEventButton::Left;
    let surface = LAST.with_borrow(Clone::clone).unwrap();

    let rest = fill();
    surface.dispatch_event(WindowEvent::PointerMoved { position });
    let hovered = fill();
    surface.dispatch_event(WindowEvent::PointerPressed { position, button });
    let pressed = fill();
    surface.dispatch_event(WindowEvent::PointerReleased { position, button });
    let open = fill();
    assert_ne!(rest, hovered);
    assert_ne!(hovered, pressed);
    assert_ne!(rest, pressed);
    assert_ne!(open, hovered);
    assert_ne!(open, pressed);
}

#[test]
fn shot_about() {
    shot("about", Page::About, None);
}

#[test]
fn shot_banner_error() {
    shot("banner_error", Page::Hotkey, Some(BROKEN));
}

#[test]
fn shot_banner_readonly() {
    shot("banner_readonly", Page::Hotkey, Some(NEWER));
}

#[test]
fn shot_hotkey() {
    shot("hotkey", Page::Hotkey, None);
}

/// The hotkey page with its keyboard open on a fake daemon link; `drive` presses keys or drops the
/// link before the shot.
fn shot_board(screen: &str, drive: fn(&Ipc)) {
    matrix(screen, |choice| {
        let _ = slint::platform::set_platform(Box::new(Shots));
        let (ipc, _sent) = Ipc::fake();
        let window = oxi_settings::open_with(common::layout(screen, None), ipc.clone()).unwrap();
        window.set_page(Page::Hotkey);
        window.set_theme(choice);
        window.show().unwrap();
        ElementHandle::find_by_accessible_label(&window, "Change hotkey")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        drive(&ipc);
        window
    });
}

fn key(ipc: &Ipc, code: Key, down: bool) {
    push(ipc, Push::Key { code, down });
}

#[test]
fn shot_keyboard() {
    // Open with nothing pressed.
    shot_board("keyboard", |_| {});
}

#[test]
fn shot_keyboard_held() {
    // Ctrl + Shift + F13 down: held keys lit.
    shot_board("keyboard-held", |ipc| {
        for k in [Key::LCtrl, Key::LShift, Key::F13] {
            key(ipc, k, true);
        }
    });
}

#[test]
fn shot_keyboard_invalid() {
    // Esc inside a chord: the error line.
    shot_board("keyboard-invalid", |ipc| {
        key(ipc, Key::LCtrl, true);
        key(ipc, Key::Esc, true);
    });
}

#[test]
fn shot_keyboard_ready() {
    // Every key released after Ctrl + F13: the focus moves to Apply.
    shot_board("keyboard-ready", |ipc| {
        for k in [Key::LCtrl, Key::F13] {
            key(ipc, k, true);
        }
        for k in [Key::LCtrl, Key::F13] {
            key(ipc, k, false);
        }
    });
}

#[test]
fn shot_keyboard_large() {
    // The board at the widest window it grows to: Ctrl + F13 picked, Space down.
    for (theme, choice) in THEMES {
        SIZE.set((1600.0, 1000.0));
        let _ = slint::platform::set_platform(Box::new(Shots));
        let (ipc, _sent) = Ipc::fake();
        let layout = common::layout("keyboard-large", None);
        let window = oxi_settings::open_with(layout, ipc.clone()).unwrap();
        window.set_page(Page::Hotkey);
        window.set_theme(choice);
        window.show().unwrap();
        ElementHandle::find_by_accessible_label(&window, "Change hotkey")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        for k in [Key::LCtrl, Key::F13] {
            key(&ipc, k, true);
        }
        key(&ipc, Key::F13, false);
        key(&ipc, Key::Space, true);
        save(
            &render(&window, (1600.0, 1000.0), 1.0),
            &format!("keyboard-1600-{theme}"),
        );
    }
}

#[test]
fn shot_keyboard_offline() {
    // The daemon goes away with the board open.
    shot_board("keyboard-offline", |ipc| {
        key(ipc, Key::LCtrl, true);
        ipc.deliver(&Event::Link(Link::Offline));
    });
}

#[test]
fn shot_audio() {
    shot("audio", Page::Audio, None);
}

#[test]
fn shot_audio_denoise() {
    matrix("audio-denoise", |choice| {
        let window = open("audio-denoise", Some(DENOISE));
        window.set_page(Page::Audio);
        window.set_theme(choice);
        sub_tab(&window, "Recording");
        window
    });
}

fn sub_tab(window: &AppWindow, label: &str) {
    common::with_role(window, label, AccessibleRole::Tab).invoke_accessible_default_action();
}

/// The Sounds sub-tab on the start sound: the built-in sounds and an own file with a long name.
#[test]
fn shot_audio_sounds() {
    const OWN: &str = "version = 2
[sounds]
start = 'C:\\Sounds\\a very long own file name that does not fit its tile at all.wav'
";
    matrix("audio-sounds", |choice| {
        let window = open("audio-sounds", Some(OWN));
        window.set_page(Page::Audio);
        window.set_theme(choice);
        sub_tab(&window, "Sounds");
        window
    });
}

// The not-connected, level and clipping states are `shots_audio` in `pages::audio`: they list the
// devices through a fake, where this binary could only ask the real sound system.

#[test]
fn shot_model() {
    shot("model", Page::Model, None);
}

/// Opens the sub-tab `label` of the open page. Paste, Write to file and Per-app rules open the
/// Paste page on it first (ia-map 3.2; the rail has an item named Paste too).
fn model_tab(window: &AppWindow, label: &str) {
    if let Some(tab) = ["Paste", "Write to file", "Per-app rules"]
        .iter()
        .position(|t| *t == label)
    {
        window.set_page(Page::Paste);
        window
            .global::<TextState>()
            .set_paste_tab(i32::try_from(tab).unwrap());
        return;
    }
    common::with_role(window, label, AccessibleRole::Tab).invoke_accessible_default_action();
}

#[test]
fn shot_model_tabs() {
    for (screen, tab) in [
        ("model-languages", "Languages"),
        ("model-speed", "Speed"),
        ("model-accuracy", "Accuracy"),
    ] {
        matrix(screen, |choice| {
            let window = open(screen, None);
            window.set_page(Page::Model);
            window.set_theme(choice);
            model_tab(&window, tab);
            window
        });
    }
}

/// Items 33, 36: Model > Languages with two language rows (one in use, one whose model is not on
/// disk), at both sizes.
#[test]
fn shot_model_language_rows() {
    let config = "version = 2
[model]
language = \"ru\"
[model.by_language]
en = \"turbo\"
ru = \"large-v3\"
";
    matrix("model-language-rows", |choice| {
        let window = open("model-language-rows", Some(config));
        window.set_page(Page::Model);
        window.set_theme(choice);
        model_tab(&window, "Languages");
        window
    });
}

#[test]
fn shot_model_weak() {
    // Only an integrated GPU: both 3.1 GB models carry the "!", the picked one says both things.
    shot_model_linked(
        "model-weak",
        "version = 2\n[model]\nid = \"podlodka\"\n",
        |l| {
            let dir = l.models_dir();
            fs::create_dir_all(&dir).unwrap();
            common::fake_model(&dir.join("ggml-large-v3.bin.part"), 1_200_000_000);
        },
        |ipc| {
            push(
                ipc,
                Push::Devices {
                    mics: vec![],
                    gpus: vec![Device {
                        name: "Vulkan0".into(),
                        description: "AMD Radeon(TM) Graphics".into(),
                        uma: true,
                    }],
                },
            );
        },
    );
}

#[test]
fn shot_model_damaged() {
    // A damaged file of the model in use, only an integrated GPU: the status, what to do, the "!".
    shot_model_linked(
        "model-damaged",
        "version = 2
[model]
id = \"large-v3\"
",
        |l| {
            let dir = l.models_dir();
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("ggml-large-v3.bin"), b"synthetic").unwrap();
        },
        |ipc| {
            push(
                ipc,
                Push::Devices {
                    mics: vec![],
                    gpus: vec![Device {
                        name: "Vulkan0".into(),
                        description: "AMD Radeon(TM) Graphics".into(),
                        uma: true,
                    }],
                },
            );
        },
    );
}

/// A model page on a fake daemon link: `prepare` fills the layout, `drive` pushes what the daemon
/// would send once the page is up.
fn shot_model_linked(screen: &str, config: &str, prepare: fn(&Layout), drive: fn(&Ipc)) {
    matrix(screen, |choice| {
        let _ = slint::platform::set_platform(Box::new(Shots));
        let layout = common::layout(screen, Some(config));
        prepare(&layout);
        let (ipc, _sent) = Ipc::fake();
        let window = oxi_settings::open_with(layout, ipc.clone()).unwrap();
        window.set_page(Page::Model);
        window.set_theme(choice);
        // Showing builds the page, whose `init` starts the path check again.
        window.show().unwrap();
        drive(&ipc);
        // The path check answers from its worker thread; let the page's poll timer take it.
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            slint::platform::update_timers_and_animations();
        }
        window
    });
}

fn push(ipc: &Ipc, push: Push) {
    ipc.deliver(&Event::Push(push));
}

const BUSY: &str = "version = 2
[model]
id = \"turbo\"
gpu = \"Vulkan0\"
";

#[test]
fn shot_model_busy() {
    // Downloading with progress (selected), a failed download, a paused one, the GPU named.
    shot_model_linked(
        "model-busy",
        BUSY,
        |l| {
            let dir = l.models_dir();
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("ggml-large-v3-russian-ties-podlodka-v1.2-f16.bin.part"),
                b"half",
            )
            .unwrap();
        },
        |ipc| {
            push(
                ipc,
                Push::Devices {
                    mics: vec![],
                    gpus: vec![Device {
                        name: "Vulkan0".into(),
                        description: "NVIDIA GeForce RTX 4070 SUPER".into(),
                        uma: false,
                    }],
                },
            );
            push(
                ipc,
                Push::DownloadProgress {
                    id: "turbo".into(),
                    done: 650_000_000,
                    total: 1_624_555_275,
                },
            );
            push(
                ipc,
                Push::DownloadDone {
                    id: "large-v3".into(),
                    result: DownloadResult::Failed {
                        message: "connection reset (os error 10054)".into(),
                    },
                },
            );
        },
    );
}

#[test]
fn shot_model_downloaded() {
    // Ready (selected), downloaded but unchecked, and a damaged file.
    shot_model_linked(
        "model-downloaded",
        BUSY,
        |l| {
            let dir = l.models_dir();
            fs::create_dir_all(&dir).unwrap();
            for (name, size) in [
                ("ggml-large-v3-turbo.bin", 1_624_555_275),
                ("ggml-large-v3.bin", 3_095_033_483),
                ("ggml-large-v3-russian-ties-podlodka-v1.2-f16.bin", 1000),
            ] {
                let file = dir.join(name);
                common::fake_model(&file, size);
                if name.ends_with("turbo.bin") {
                    oxi_core::models::write_stamp(&file).unwrap();
                }
            }
        },
        |_| {},
    );
}

#[test]
fn shot_model_custom_relative() {
    shot_model_linked(
        "model-custom-relative",
        "version = 2
[model]
id = \"custom\"
custom_path = \"mine.bin\"
",
        |_| {},
        |_| {},
    );
}

#[test]
fn shot_model_custom_missing() {
    shot_model_linked(
        "model-custom-missing",
        "version = 2
[model]
id = \"custom\"
custom_path = \"C:/nowhere/gone.bin\"
",
        |_| {},
        |_| {},
    );
}

#[test]
fn shot_model_offline() {
    shot_model_linked(
        "model-offline",
        BUSY,
        |_| {},
        |ipc| {
            ipc.deliver(&Event::Link(Link::Offline));
        },
    );
}

#[test]
fn shot_text() {
    shot("text", Page::Text, None);
}

#[test]
fn shot_text_notes() {
    // Over the token budget (red count and note), a line without `=>`, and the broken-config banner.
    shot_with("text-notes", Page::Text, Some(BROKEN), |l| {
        let words: Vec<String> = (0..400).map(|i| format!("word{i}")).collect();
        fs::write(
            l.dictionary_file(),
            words.join(
                "
",
            ),
        )
        .unwrap();
        fs::write(
            l.replace_file(),
            "heard => wanted
no arrow here
",
        )
        .unwrap();
    });
}

#[test]
fn shot_text_unreadable() {
    // A directory where replace.txt belongs: reading it fails; its tab is the one that shows it.
    shot_text_tab("text-unreadable", "Replacements", None, |l| {
        fs::create_dir(l.replace_file()).unwrap();
    });
}

#[test]
fn shot_text_conflict() {
    // Unsaved edits against a file changed on disk: the conflict note with Reload and Overwrite.
    matrix("text-conflict", |choice| {
        let _ = slint::platform::set_platform(Box::new(Shots));
        let layout = common::layout("text-conflict", None);
        fs::write(layout.dictionary_file(), "synthalpha").unwrap();
        let window = oxi_settings::open(layout.clone()).unwrap();
        window.set_page(Page::Text);
        window.set_theme(choice);
        window.show().unwrap();
        ElementHandle::find_by_accessible_label(&window, "Dictionary")
            .find(|e| e.accessible_role() == Some(AccessibleRole::TextInput))
            .unwrap()
            .set_accessible_value("synthbeta");
        fs::write(layout.dictionary_file(), "synthgamma").unwrap();
        common::one(&window, "Save dictionary.txt").invoke_accessible_default_action();
        window
    });
}

/// One Text sub-tab at both sizes in both themes; `prepare` fills the layout first.
fn shot_text_tab(screen: &str, tab: &'static str, config: Option<&str>, prepare: fn(&Layout)) {
    matrix(screen, |choice| {
        let window = open_with(screen, config, prepare);
        window.set_page(Page::Text);
        window.set_theme(choice);
        model_tab(&window, tab);
        window
    });
}

#[test]
fn shot_text_replacements_bad_line() {
    shot_text_tab("text-replacements", "Replacements", None, |l| {
        fs::write(l.replace_file(), "heard => wanted\nno arrow here\n").unwrap();
    });
}

#[test]
fn shot_text_paste() {
    shot_text_tab("text-paste", "Paste", None, |_| {});
}

#[test]
fn shot_text_rules() {
    shot_text_tab("text-rules", "Per-app rules", None, |_| {});
}

#[test]
fn shot_text_dictionary_500() {
    // A 500-word dictionary: the editor scrolls inside, the page does not.
    shot_with("text-dictionary-500", Page::Text, None, |l| {
        let words: Vec<String> = (0..500).map(|i| format!("synthword{i}")).collect();
        fs::write(l.dictionary_file(), words.join("\n")).unwrap();
    });
}

/// 150 rules with long names: the list scrolls in its own area.
fn many_rules() -> String {
    let mut text = String::from("version = 2\n");
    for i in 0..150 {
        let _ = write!(
            text,
            "[[paste.rules]]\napp = \"a-very-long-synthetic-application-name-{i}.exe\"\n"
        );
    }
    text
}

#[test]
fn shot_text_worst_case() {
    // Daemon banner, 150 rules with long names and the duplicate toast: the rule list scrolls by
    // itself, Add stays on screen and Tab reaches it. Items outside the viewport are not in the
    // element search, so "found" means "in view".
    use slint::platform::Key as Tab;
    let config = many_rules();
    for (theme, choice) in THEMES {
        let _ = slint::platform::set_platform(Box::new(Shots));
        let layout = common::layout("text-worst", Some(&config));
        let (ipc, _sent) = Ipc::fake();
        let window = oxi_settings::open_with(layout, ipc.clone()).unwrap();
        window.set_theme(choice);
        window.show().unwrap();
        ipc.deliver(&Event::Link(Link::Offline));
        model_tab(&window, "Per-app rules");
        let surface = LAST.with_borrow(Clone::clone).unwrap();
        let _ = render(&window, MIN, 1.0);
        // The search field and the Scan button, whole on screen.
        let add_in_view = || {
            ["New rule app", "Scan for apps"].iter().all(|label| {
                ElementHandle::find_by_accessible_label(&window, label)
                    .next()
                    .is_some_and(|e| e.absolute_position().y + e.size().height <= MIN.1)
            })
        };
        assert!(add_in_view(), "the bar is off screen at 720 x 520");
        for app in ["b.exe", "B.EXE"] {
            common::one(&window, "New rule app").set_accessible_value(app);
            common::one(&window, "Add rule").invoke_accessible_default_action();
        }
        assert!(!common::has(&window, "already has a rule: change its mode"));
        save(
            &render(&window, MIN, 1.0),
            &format!("text-worst-top-720-{theme}"),
        );
        // The refused duplicate left its text in the field, which filters the list: clear it.
        common::one(&window, "New rule app").set_accessible_value("");
        // The wheel over the list moves the list only.
        let first = common::one(
            &window,
            "Remove a-very-long-synthetic-application-name-0.exe",
        );
        let at = first.absolute_position();
        surface.dispatch_event(WindowEvent::PointerScrolled {
            position: slint::LogicalPosition::new(at.x, at.y),
            delta_x: 0.0,
            delta_y: -100_000.0,
        });
        slint::platform::update_timers_and_animations();
        assert_eq!(
            window.get_scroll_y(),
            0.0,
            "the page scrolled with the list"
        );
        assert!(add_in_view(), "the bar left the screen");
        save(
            &render(&window, MIN, 1.0),
            &format!("text-worst-bottom-720-{theme}"),
        );
        // Keyboard only: Tab until the focus is on the app field, a letter shows it.
        let field = || common::one(&window, "New rule app").accessible_value();
        let mut tabs = 0;
        while field().as_deref() != Some("c") && tabs < 12 {
            for text in [slint::SharedString::from(Tab::Tab), "c".into()] {
                surface.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
                surface.dispatch_event(WindowEvent::KeyReleased { text });
            }
            slint::platform::update_timers_and_animations();
            tabs += 1;
        }
        assert_eq!(field().as_deref(), Some("c"), "Tab never reached the field");
        save(
            &render(&window, MIN, 1.0),
            &format!("text-worst-tabbed-720-{theme}"),
        );
    }
}

/// Every sub-tab of the pages ia-map 3.2 changed, at 720 x 520, a 500-word dictionary behind
/// Text: with no banner the page does not scroll on any of them (UI rule 25); under the offline
/// daemon's banner Text and Paste (but its five-row Paste tab) still fit (the shots of the rest
/// are saved for review).
#[test]
fn shot_every_tab_at_720x520() {
    // (screen, page, sub-tab index)
    let tabs: [(&str, Page, i32); 18] = [
        ("model-models", Page::Model, 0),
        ("model-languages", Page::Model, 1),
        ("model-speed", Page::Model, 2),
        ("model-accuracy", Page::Model, 3),
        ("text-dictionary", Page::Text, 0),
        ("text-replacements", Page::Text, 1),
        ("text-words", Page::Text, 2),
        ("text-formatting", Page::Text, 3),
        ("text-commands", Page::Text, 4),
        ("paste-paste", Page::Paste, 0),
        ("paste-file", Page::Paste, 1),
        ("paste-rules", Page::Paste, 2),
        ("system-general", Page::System, 0),
        ("system-files", Page::System, 1),
        ("system-about", Page::System, 2),
        ("audio-microphone", Page::Audio, 0),
        ("audio-recording", Page::Audio, 1),
        ("audio-sounds", Page::Audio, 2),
    ];
    for (theme, choice) in THEMES {
        for banner in [false, true] {
            for (name, page, tab) in tabs {
                let _ = slint::platform::set_platform(Box::new(Shots));
                SIZE.set(MIN);
                let layout = common::layout("every-tab", None);
                let words: Vec<String> = (0..500).map(|i| format!("synthword{i}")).collect();
                fs::write(layout.dictionary_file(), words.join("\n")).unwrap();
                let (ipc, _sent) = Ipc::fake();
                let window = oxi_settings::open_with(layout, ipc.clone()).unwrap();
                window.set_page(page);
                match page {
                    Page::Model => window.global::<ModelState>().set_tab(tab),
                    Page::Text => window.global::<TextState>().set_tab(tab),
                    Page::Paste => window.global::<TextState>().set_paste_tab(tab),
                    Page::System => window.global::<SystemState>().set_tab(tab),
                    _ => window.global::<AudioState>().set_view(tab),
                }
                window.set_theme(choice);
                window.show().unwrap();
                ipc.deliver(&Event::Link(if banner {
                    Link::Offline
                } else {
                    Link::Online
                }));
                let surface = LAST.with_borrow(Clone::clone).unwrap();
                let pixmap = render(&window, MIN, 1.0);
                let under = if banner { "-banner" } else { "" };
                save(&pixmap, &format!("tab-{name}{under}-720-{theme}"));
                // A wheel over the page moves it only if it is taller than its room.
                surface.dispatch_event(WindowEvent::PointerScrolled {
                    position: slint::LogicalPosition::new(400.0, 30.0),
                    delta_x: 0.0,
                    delta_y: -100_000.0,
                });
                slint::platform::update_timers_and_animations();
                let scrolled = window.get_scroll_y() != 0.0;
                if scrolled {
                    eprintln!("{name} scrolls at 720 x 520 (banner {banner})");
                }
                // The fit is measured in Segoe UI Variable Text, which only Windows has.
                // Paste > Paste: five rows, 32 px left with no banner (ia-map 3.2), so a 72 px banner makes
                // it scroll (ia-map RT R12, UI rule 1 (a)).
                let held = page == Page::Text || (page == Page::Paste && tab > 0);
                if cfg!(windows) && (!banner || held) {
                    assert!(!scrolled, "{name} scrolls at 720 x 520 (banner {banner})");
                }
            }
        }
    }
}

#[test]
fn shot_overlay() {
    shot("overlay", Page::Overlay, None);
}

#[test]
fn shot_overlay_narrow() {
    // The minimum window: the six position tiles still fit the narrowest control column.
    for (theme, choice) in THEMES {
        let window = open("overlay-narrow", None);
        window.set_page(Page::Overlay);
        window.set_theme(choice);
        save(
            &render(&window, (720.0, 520.0), 1.0),
            &format!("overlay-narrow-{theme}"),
        );
    }
}

#[test]
fn shot_system() {
    shot("system", Page::System, None);
}

/// Recording storage with every warning on: a vanished folder with a long path, the limits at
/// their widest, dictated text logged.
#[test]
fn shot_system_recordings_warned() {
    // An absent drive on Windows; elsewhere a folder whose parents are not there either.
    let root = if cfg!(windows) { "D:/" } else { "/" };
    let folder = format!("{root}{}", "a very long folder name/".repeat(6));
    let config = format!(
        "version = 2\n[debug]\nlog_text = true\n[history]\nenabled = true\nmax_days = 999\nmax_bytes = 1072668082176\nfolder = {folder:?}\n"
    );
    // Taller than the page, at the minimum width and the maximum window: the row is on screen.
    for size in [(720.0, 900.0), (1280.0, 860.0)] {
        for (theme, choice) in THEMES {
            SIZE.set(size);
            let window = open("system-recordings-warned", Some(&config));
            window.set_page(Page::System);
            window.global::<SystemState>().set_tab(1);
            window.set_theme(choice);
            window.show().unwrap();
            window
                .window()
                .set_size(slint::LogicalSize::new(size.0, size.1));
            // The folder check answers from a thread; the page polls for it.
            let warned = (0..200).any(|_| {
                std::thread::sleep(std::time::Duration::from_millis(5));
                slint::platform::update_timers_and_animations();
                ElementHandle::find_by_accessible_label(
                    &window,
                    "This folder is gone. Nothing is saved until it is back or you choose another one.",
                )
                .next()
                .is_some()
            });
            assert!(warned, "the folder check did not answer");
            save(
                &render(&window, size, 1.0),
                &format!("system-recordings-warned-{}-{theme}", size.0),
            );
        }
    }
}

#[test]
fn shot_system_update_off() {
    // Turning the update check off asks for a confirmation first.
    matrix("system-update-off", |choice| {
        let window = open("system-update-off", None);
        window.set_page(Page::System);
        window.set_theme(choice);
        window.show().unwrap();
        ElementHandle::find_by_accessible_label(&window, "Check for updates")
            .find(|e| e.accessible_checkable() == Some(true))
            .unwrap()
            .invoke_accessible_default_action();
        window
    });
}

#[test]
fn shot_overlay_mismatch() {
    // An old daemon still runs after an update: the position tiles are off and say to restart.
    matrix("overlay-mismatch", |choice| {
        let _ = slint::platform::set_platform(Box::new(Shots));
        let (ipc, _sent) = Ipc::fake();
        let layout = common::layout("overlay-mismatch", None);
        let window = oxi_settings::open_with(layout, ipc.clone()).unwrap();
        window.set_page(Page::Overlay);
        window.set_theme(choice);
        window.show().unwrap();
        ipc.deliver(&Event::Link(Link::Mismatch { daemon_proto: 9 }));
        window
    });
}

#[test]
fn tile_icon_renders_inside_its_card() {
    let window = open("thumb", None);
    window.set_page(Page::System);
    window.set_theme(ThemeChoice::Light);
    let pixmap = render(&window, DEFAULT, 1.0);
    let card = ElementHandle::find_by_accessible_label(&window, "Dark")
        .find(|e| e.accessible_role() == Some(AccessibleRole::RadioButton))
        .unwrap();
    let (pos, size) = (card.absolute_position(), card.size());
    // The icon is 32px wide and centred, above the title: its column in the card's top half holds
    // no title ink, so only the icon can fill it (on a light page with no other dark fill).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // logical px at scale 1
    let dark = (pos.y as u32..(pos.y + size.height / 2.0) as u32)
        .flat_map(|y| {
            let mid = pos.x + size.width / 2.0;
            ((mid - 16.0) as u32..(mid + 16.0) as u32).map(move |x| (x, y))
        })
        .filter_map(|(x, y)| pixmap.pixel(x, y))
        .filter(|p| p.red() < 0x40 && p.green() < 0x40 && p.blue() < 0x40)
        .count();
    assert!(dark > 60, "{dark} dark pixels in the Dark card");
}

/// At 960 the Words per minute label wraps to two lines; the four values must still start on
/// one pixel row (before, the wrapped card's value sat 6 px higher than the others).
#[test]
fn stat_values_share_a_baseline_where_a_label_wraps() {
    let size = (960.0, 720.0);
    SIZE.set(size);
    let _ = slint::platform::set_platform(Box::new(Shots));
    let (ipc, _sent) = Ipc::fake();
    let window = oxi_settings::open_with(common::layout("baseline", None), ipc.clone()).unwrap();
    window.set_theme(ThemeChoice::Light);
    window.show().unwrap();
    push(
        &ipc,
        Push::Stats(Summary {
            dictations: 11_300,
            words: 299_900,
            wpm: 146,
            streak: 6,
            longest_streak: 40,
            heat_start: Date::from_ymd(2026, 11, 2).unwrap(),
            heat: vec![0, 1, 4],
        }),
    );
    let pixmap = render(&window, size, 1.0);
    // The value is the only text in text-primary (26); labels and captions are lighter. The
    // streak card is left out: "days" has a descender and "d" an ascender.
    let ink_top = |label: &str| {
        let card = common::one(&window, label);
        let (at, size) = (card.absolute_position(), card.size());
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // whole positive px
        let (x0, y0, x1, y1) = (
            at.x as u32,
            at.y as u32,
            (at.x + size.width) as u32,
            (at.y + size.height) as u32,
        );
        (y0..y1)
            .find(|&y| (x0..x1).any(|x| pixmap.pixel(x, y).is_some_and(|p| p.red() < 60)))
            .unwrap_or_else(|| panic!("no value ink in {label}"))
    };
    let tops = [
        ink_top("Words spoken: 299.9K, All time"),
        ink_top("Words per minute: 146, Based on recorded speech"),
        ink_top("Dictations: 11.3K, All time"),
    ];
    assert!(
        tops.iter().all(|t| t.abs_diff(tops[0]) <= 1),
        "value tops {tops:?}"
    );
}

/// d12: History after Copy of one long line. The detail card must end inside the window, so the
/// right border and the whole toolbar are still there (the Copy used to widen the card to the
/// line's length).
#[test]
fn history_after_copy_stays_inside_the_window() {
    use oxi_core::history::Status;
    use oxi_core::ipc::{HistoryItem, Spoken};
    let text = "a long dictation without any pause in it ".repeat(40);
    for size in [(720.0, 520.0), (1280.0, 860.0)] {
        for (theme, choice) in THEMES {
            SIZE.set(size);
            let _ = slint::platform::set_platform(Box::new(Shots));
            let layout = common::layout(
                "history-copy",
                Some("version = 2\n[history]\nenabled = true\n"),
            );
            let (ipc, _sent) = Ipc::fake();
            let window = oxi_settings::open_with(layout, ipc.clone()).unwrap();
            window.set_page(Page::History);
            window.set_theme(choice);
            window.show().unwrap();
            push(
                &ipc,
                Push::History {
                    items: vec![HistoryItem {
                        id: oxi_core::history::HistoryId::parse("2026-11-10/120000-000").unwrap(),
                        status: Status::Done,
                        duration_ms: 4_500,
                        model: String::new(),
                        language: String::new(),
                        text: Spoken(text.clone()),
                        held: Vec::new(),
                    }],
                    more: false,
                },
            );
            slint::platform::update_timers_and_animations();
            common::with_role(&window, "Copy text", AccessibleRole::Button)
                .invoke_accessible_default_action();
            let shot = render(&window, size, 1.0);
            for label in ["Copy text", "Delete", "Transcript"] {
                let e = common::one(&window, label);
                let right = e.absolute_position().x + e.size().width;
                assert!(right <= size.0, "{label} ends at {right}, window {size:?}");
            }
            save(&shot, &format!("history-copy-{}-{theme}", size.0));
        }
    }
}

/// ia-map 3.5: "Add language" sorts the new row in at its place (Yoruba last of seven) and
/// scrolls it into view at 720 x 520, in the real fonts (the testing backend's lay out taller).
#[test]
fn shot_model_language_added() {
    let config = "version = 2
[model.by_language]
de = \"turbo\"
en = \"turbo\"
es = \"turbo\"
fr = \"turbo\"
it = \"turbo\"
ru = \"turbo\"
";
    let window = open("model-language-added", Some(config));
    window.set_page(Page::Model);
    window.set_theme(ThemeChoice::Dark);
    model_tab(&window, "Languages");
    render(&window, MIN, 1.0);
    let s = window.global::<ModelState>();
    s.invoke_add_search("yoruba".into());
    s.invoke_add_chosen(0);
    save(&render(&window, MIN, 1.0), "model-language-added-720-dark");
    let combo = ElementHandle::find_by_accessible_label(&window, "Model for Yoruba")
        .find(|e| e.accessible_role() == Some(AccessibleRole::Combobox))
        .expect("Yoruba's Combo in view");
    let (y, h) = (combo.absolute_position().y, combo.size().height);
    assert!(y >= 0.0 && y + h <= MIN.1, "Yoruba's Combo is cut: y {y}");
    // The list scrolled: its first row, the default model's, is out of view.
    assert!(
        ElementHandle::find_by_accessible_label(&window, "Change the default model")
            .next()
            .is_none()
    );
}

/// Owner item 17: a search pick opens Audio > Recording and flashes "Longest recording" in the
/// accent; the flash is gone once it has run. Shot mid-flash at the minimum size, both themes.
#[test]
fn shot_search_flash() {
    let ms = std::time::Duration::from_millis;
    for (theme, choice) in THEMES {
        SIZE.set(MIN);
        let window = open("search-flash", None);
        window.set_theme(choice);
        render(&window, MIN, 1.0);
        window.invoke_search("longest".into());
        window.invoke_search_picked(0);
        render(&window, MIN, 1.0);
        // A frame for the layout, then the first pulse at full strength.
        advance(ms(20));
        slint::platform::update_timers_and_animations();
        advance(ms(120));
        let lit = render(&window, MIN, 1.0);
        save(&lit, &format!("search-flash-{}-{theme}", MIN.0));
        advance(ms(1000));
        for _ in 0..8 {
            advance(ms(160));
            slint::platform::update_timers_and_animations();
        }
        let after = render(&window, MIN, 1.0);
        assert_eq!(window.get_page(), Page::Audio);
        assert_ne!(
            lit.data(),
            after.data(),
            "{theme}: the flash shows, then goes"
        );
    }
}

/// The pixels of `pixmap` around `element` (6 px past each side, where a focus ring is drawn).
fn around(pixmap: &Pixmap, element: &ElementHandle) -> Vec<u8> {
    let (at, size) = (element.absolute_position(), element.size());
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // whole positive pixels
    let (x0, y0, x1, y1) = (
        (at.x as u32).saturating_sub(6),
        (at.y as u32).saturating_sub(6),
        ((at.x + size.width) as u32 + 6).min(pixmap.width()),
        ((at.y + size.height) as u32 + 6).min(pixmap.height()),
    );
    let width = pixmap.width() as usize;
    (y0..y1)
        .flat_map(|y| {
            let row = y as usize * width * 4;
            pixmap.data()[row + x0 as usize * 4..row + x1 as usize * 4].to_vec()
        })
        .collect()
}

/// Owner item 17, ia-map 3.7: a search pick moves the keyboard focus to the row's control, with no
/// focus ring (a pick is not a keyboard walk); the next Tab and back draws the ring there.
#[test]
fn search_pick_focuses_the_rows_control_and_draws_no_ring() {
    use slint::platform::Key as K;
    let ms = std::time::Duration::from_millis;
    for (theme, choice) in THEMES {
        SIZE.set(MIN);
        let window = open("search-focus", None);
        window.set_theme(choice);
        render(&window, MIN, 1.0);
        let surface = LAST.with_borrow(Clone::clone).unwrap();
        let press = |text: slint::SharedString| {
            surface.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
            surface.dispatch_event(WindowEvent::KeyReleased { text });
        };
        window.invoke_search("log dictated".into());
        window.invoke_search_picked(0);
        render(&window, MIN, 1.0);
        // A frame for the layout, then the row asks for the focus; let the flash run out.
        advance(ms(20));
        slint::platform::update_timers_and_animations();
        advance(ms(1500));
        slint::platform::update_timers_and_animations();
        let switch = || common::with_role(&window, "Log dictated text", AccessibleRole::Switch);
        let calm = render(&window, MIN, 1.0);
        let at_pick = around(&calm, &switch());
        assert_eq!(switch().accessible_checked(), Some(false), "{theme}");
        // Space acts on the found setting: the focus is on its switch.
        press(" ".into());
        assert_eq!(
            switch().accessible_checked(),
            Some(true),
            "{theme}: Space did not reach the picked row's switch"
        );
        press(" ".into());
        let switch_back = render(&window, MIN, 1.0);
        assert_eq!(
            around(&switch_back, &switch()),
            at_pick,
            "{theme}: a ring was drawn on a pick"
        );
        // A real Tab away and Shift+Tab back is a keyboard walk: the ring shows.
        press(K::Tab.into());
        surface.dispatch_event(WindowEvent::KeyPressed {
            text: K::Shift.into(),
        });
        press(K::Backtab.into());
        surface.dispatch_event(WindowEvent::KeyReleased {
            text: K::Shift.into(),
        });
        assert_ne!(
            around(&render(&window, MIN, 1.0), &switch()),
            at_pick,
            "{theme}: Tab draws the ring, a pick does not"
        );
    }
}

/// A pick of a row whose controls are off keeps the focus on the row: Space and the arrows change
/// nothing, here or on the next row, and the focus does not wrap to the page's first control
/// (UI rule 17).
#[test]
fn search_pick_of_a_disabled_row_changes_no_setting() {
    const BIG: (f32, f32) = (1280.0, 1600.0);
    use slint::platform::Key as K;
    let ms = std::time::Duration::from_millis;
    let second_try_off = "version = 2
[model.advanced]
temperature_fallback = false
";
    for (query, config) in [
        ("Second try when a phrase repeats", second_try_off),
        (
            "Certainty needed",
            "version = 2
",
        ),
    ] {
        SIZE.set(BIG);
        let window = open("search-disabled", Some(config));
        render(&window, BIG, 1.0);
        let surface = LAST.with_borrow(Clone::clone).unwrap();
        window.invoke_search(query.into());
        window.invoke_search_picked(0);
        render(&window, BIG, 1.0);
        advance(ms(20));
        slint::platform::update_timers_and_animations();
        advance(ms(1500));
        slint::platform::update_timers_and_animations();
        let calm = render(&window, BIG, 1.0);
        // What the rows near the pick say of themselves: a popup is no part of the frame.
        let snap = || -> Vec<String> {
            [
                "Second try when confidence is low",
                "Second try when a phrase repeats",
                "Silence cut-off",
                "Skip non-speech sounds",
                "Hold back unsure words",
                "Certainty needed",
            ]
            .into_iter()
            .flat_map(|label| ElementHandle::find_by_accessible_label(&window, label))
            .map(|e| {
                format!(
                    "{:?} {:?} {:?} {:?} {:?}",
                    e.accessible_label(),
                    e.accessible_value(),
                    e.accessible_checked(),
                    e.accessible_expanded(),
                    e.accessible_enabled()
                )
            })
            .collect()
        };
        let rows = snap();
        assert!(
            rows.iter().any(|r| r.ends_with("Some(false)")),
            "{query}: the row's control is off: {rows:?}"
        );
        let page = window.get_page();
        for key in [
            K::Space.into(),
            K::DownArrow.into(),
            K::RightArrow.into(),
            " ".into(),
        ] {
            let key: slint::SharedString = key;
            surface.dispatch_event(WindowEvent::KeyPressed { text: key.clone() });
            surface.dispatch_event(WindowEvent::KeyReleased { text: key });
        }
        let after = render(&window, BIG, 1.0);
        assert_eq!(window.get_page(), page, "{query}");
        assert_eq!(snap(), rows, "{query}: a key changed a row near the pick");
        assert!(
            calm.data() == after.data(),
            "{query}: a key changed the frame"
        );
    }
}

/// What each setting near the search picks holds: label and state. Not `enabled`: Remove filler
/// words turns the filler field on by design.
fn held(window: &AppWindow) -> Vec<(String, String)> {
    [
        "Remove filler words",
        "Filler words",
        "Phrase shortcuts",
        "Second try when confidence is low",
        "Second try when a phrase repeats",
        "Silence cut-off",
        "Skip non-speech sounds",
    ]
    .into_iter()
    .flat_map(|label| {
        ElementHandle::find_by_accessible_label(window, label).map(move |e| {
            let state = format!(
                "{:?} {:?} {:?}",
                e.accessible_value(),
                e.accessible_checked(),
                e.accessible_expanded()
            );
            (label.to_owned(), state)
        })
    })
    .collect()
}

/// The entries of `rows` for `label` (`keep`) or all the others.
fn of(rows: &[(String, String)], label: &str, keep: bool) -> Vec<(String, String)> {
    rows.iter()
        .filter(|(l, _)| (l == label) == keep)
        .cloned()
        .collect()
}

/// After a pick the focus is on an enabled control of the found row or on the row itself, never past
/// it: Space and typing change nothing outside the found row, and the next Tab reaches the stop right
/// after the row (UI rule 17).
#[test]
fn search_pick_keeps_the_focus_in_the_found_row() {
    use slint::platform::Key as K;
    const BIG: (f32, f32) = (1280.0, 1600.0);
    let ms = std::time::Duration::from_millis;
    let second_try_off = "version = 2
[model.advanced]
temperature_fallback = false
";
    // The query, its config, the setting Space changes (the row's own control), the setting a walk
    // from the row reaches and its Tabs (a row's "i" is a stop). Filler words are off by default.
    let cases = [
        // A row with an enabled control: Space toggles it, which turns the filler field on; then
        // the Filler words "i" and field.
        (
            "Remove filler words",
            None,
            Some("Remove filler words"),
            "Filler words",
            2,
        ),
        // A text-only row whose field is off: past it, the Phrase shortcuts "i" and field.
        ("Filler words", None, None, "Phrase shortcuts", 2),
        // A text-only row followed by a live text field: the pick puts the focus in that field (the
        // row's `take-pick`), so typing lands there and no Tab is needed.
        (
            "Phrase shortcuts",
            None,
            Some("Phrase shortcuts"),
            "Phrase shortcuts",
            0,
        ),
        // A disabled row with controls: the next row's "i" and control.
        (
            "Second try when a phrase repeats",
            Some(second_try_off),
            None,
            "Silence cut-off",
            2,
        ),
    ];
    for (query, config, own, next, tabs) in cases {
        SIZE.set(BIG);
        let window = open("search-in-row", config);
        render(&window, BIG, 1.0);
        let surface = LAST.with_borrow(Clone::clone).unwrap();
        let press = |text: slint::SharedString| {
            surface.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
            surface.dispatch_event(WindowEvent::KeyReleased { text });
        };
        window.invoke_search(query.into());
        window.invoke_search_picked(0);
        render(&window, BIG, 1.0);
        advance(ms(20));
        slint::platform::update_timers_and_animations();
        advance(ms(1500));
        slint::platform::update_timers_and_animations();
        let calm = render(&window, BIG, 1.0);
        let picked = held(&window);
        let page = window.get_page();
        press(" ".into());
        press("x".into());
        let typed = held(&window);
        assert_eq!(window.get_page(), page, "{query}: a key left the page");
        if let Some(label) = own {
            assert_ne!(
                of(&picked, label, true),
                of(&typed, label, true),
                "{query}: Space did not reach the row's own control"
            );
            assert_eq!(
                of(&picked, label, false),
                of(&typed, label, false),
                "{query}: a key changed a setting outside the found row"
            );
        } else {
            assert_eq!(
                picked, typed,
                "{query}: a key changed a setting outside the found row"
            );
            assert!(
                calm.data() == render(&window, BIG, 1.0).data(),
                "{query}: a key changed the frame"
            );
        }
        // The focus was in the found row: the walk from there reaches the setting after it.
        for _ in 0..tabs {
            press(K::Tab.into());
        }
        press("y".into());
        press(" ".into());
        let moved = held(&window);
        assert_ne!(
            of(&typed, next, true),
            of(&moved, next, true),
            "{query}: {tabs} Tab after the pick did not reach {next}"
        );
        assert_eq!(
            of(&typed, next, false),
            of(&moved, next, false),
            "{query}: {tabs} Tab after the pick reached another setting than {next}"
        );
    }
}
