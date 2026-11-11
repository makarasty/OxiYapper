//! Settings shell under the testing backend, on a temporary config directory.

mod common;

use std::fmt::Write as _;
use std::fs;

use common::{
    assert_nothing_moved, has, layout, one, open, open_default, open_linked, spots, with_role,
};
use i_slint_backend_testing::{
    AccessibleRole, ElementHandle, ElementQuery, TestingBackend, TestingBackendOptions,
};
use oxi_core::config::{ConfigFile, Theme};
use oxi_core::ipc::Push;
use oxi_settings::ipc::{Cmd, Event, Link};
use oxi_settings::{AppWindow, ChipLevel, Ipc, Page, Shell, ThemeChoice, Toast, Tokens};
use slint::ComponentHandle;
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Key, Platform, PointerEventButton, WindowAdapter, WindowEvent};

fn tab(window: &AppWindow, label: &str) -> ElementHandle {
    with_role(window, label, AccessibleRole::Tab)
}

fn card(window: &AppWindow, label: &str) -> ElementHandle {
    with_role(window, label, AccessibleRole::RadioButton)
}

#[test]
fn rail_switches_pages() {
    let l = layout("rail", None);
    let window = open_default(&l);
    assert_eq!(window.get_page(), Page::Home);
    assert!(!has(&window, "Version"));
    tab(&window, "About").invoke_accessible_default_action();
    assert_eq!(window.get_page(), Page::About);
    assert!(has(&window, "Version"));
    assert_eq!(tab(&window, "About").accessible_item_selected(), Some(true));
    tab(&window, "System").invoke_accessible_default_action();
    assert_eq!(window.get_page(), Page::System);
    assert!(!has(&window, "Version"));
}

#[test]
fn about_shows_name_version_and_licence() {
    let l = layout("about", None);
    let window = open(&l, Page::Hotkey);
    window.set_page(Page::About);
    for text in [
        oxi_core::APP_NAME,
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_LICENSE"),
        "MIT licence",
        "Apache-2.0 licence",
    ] {
        assert!(has(&window, text), "About lacks {text}");
    }
}

/// Owner 2026-11-10: the author line says "Made in Ukraine" (it was cut once). The std `AboutSlint`
/// widget stays on About, in the popup the Slint tile opens (R2.3): the Slint royalty-free licence
/// requires it.
#[test]
fn about_has_made_in_ukraine_and_the_std_slint_widget() {
    let window = open(&layout("about-slint-badge", None), Page::About);
    assert!(has(&window, "Made in Ukraine"));
    open_slint_badge(&window);
    assert!(has(&window, "#MadeWithSlint"), "AboutSlint is missing");
}

/// Clicks the About page's Slint tile, which opens the "Made with Slint" badge.
fn open_slint_badge(window: &AppWindow) {
    with_role(window, "Slint", AccessibleRole::Button).invoke_accessible_default_action();
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(1));
}

thread_local! {
    /// The window of the component created last on this thread, drawn by `Soft`.
    static SURFACE: std::cell::RefCell<Option<std::rc::Rc<MinimalSoftwareWindow>>> = const { std::cell::RefCell::new(None) };
    static START: std::time::Instant = std::time::Instant::now();
}

/// A software-rendered platform: the testing backend draws nothing, and the focus ring is a pixel.
struct Soft;

impl Platform for Soft {
    fn create_window_adapter(
        &self,
    ) -> Result<std::rc::Rc<dyn WindowAdapter>, slint::PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        SURFACE.with_borrow_mut(|s| *s = Some(window.clone()));
        Ok(window)
    }

    fn duration_since_start(&self) -> std::time::Duration {
        START.with(std::time::Instant::elapsed)
    }
}

/// Whether the 2 px focus ring is drawn on `card`'s top edge: its pixel differs from the page's
/// just above it.
fn ring_on_top_edge(card: &ElementHandle) -> bool {
    slint::platform::update_timers_and_animations();
    let (width, height) = (1000_usize, 700_usize);
    let surface = SURFACE.with_borrow(Clone::clone).unwrap();
    surface.request_redraw();
    let mut buffer = vec![PremultipliedRgbaColor::default(); width * height];
    assert!(surface.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, width);
    }));
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "whole positive window pixels"
    )]
    let (x, y) = {
        let (at, size) = (card.absolute_position(), card.size());
        ((at.x + size.width * 0.75) as usize, at.y as usize)
    };
    let at = |dy: usize| {
        let p = buffer[(y - dy) * width + x];
        [p.red, p.green, p.blue]
    };
    // The ring sits 2 to 4 px outside the card.
    at(3) != at(9)
}

/// Esc on the Slint badge hands the focus back to the card that opened it, and the keyboard user
/// keeps the ring (`KeyFocus.after`, not `from-key`).
#[test]
fn about_slint_card_keeps_its_ring_when_esc_closes_its_badge() {
    slint::platform::set_platform(Box::new(Soft)).unwrap();
    let window = common::open_again(&layout("about-ring", None), Page::About);
    window
        .window()
        .set_size(slint::PhysicalSize::new(1000, 700));
    let card = with_role(&window, "Slint", AccessibleRole::Button);
    assert!(!ring_on_top_edge(&card), "a ring before any key");
    // The rail, Project page, MIT, Apache-2.0, Rust, then Slint.
    for _ in 0..6 {
        press(&window, Key::Tab);
    }
    assert!(
        ring_on_top_edge(&card),
        "Tab draws no ring on the Slint card"
    );
    press(&window, Key::Return);
    assert!(has(&window, "#MadeWithSlint"), "Enter opens no badge");
    press(&window, Key::Escape);
    assert!(!has(&window, "#MadeWithSlint"), "Esc leaves the badge open");
    assert!(ring_on_top_edge(&card), "the ring went with the badge");
}

#[test]
fn about_links_open_from_the_keyboard() {
    let (window, opened) = about_with_links("about-keys");
    let opened = || opened.borrow().clone().unwrap_or_default();
    window
        .window()
        .set_size(slint::LogicalSize::new(1000.0, 700.0));
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(1));
    let tab = |n| {
        for _ in 0..n {
            press(&window, Key::Tab);
        }
    };
    // The rail, then Project page.
    tab(2);
    press(&window, Key::Return);
    assert_eq!(opened(), env!("CARGO_PKG_REPOSITORY"));
    // MIT, Apache-2.0, then the tiles: Rust, Slint, whisper.cpp.
    tab(5);
    press(&window, " ");
    assert_eq!(opened(), "https://github.com/ggml-org/whisper.cpp");
    // The six tiles left, then the author's button.
    tab(7);
    press(&window, Key::Return);
    assert_eq!(opened(), "https://github.com/makarasty");
}

#[test]
fn about_tiles_open_their_project() {
    let (window, opened) = about_with_links("about-tiles");
    with_role(&window, "cpal", AccessibleRole::Button).invoke_accessible_default_action();
    assert_eq!(
        opened.borrow().as_deref(),
        Some("https://github.com/RustAudio/cpal")
    );
}

/// What the testing backend keeps of `Platform.open-url`: the last URL handed to it.
type OpenedUrl = std::rc::Rc<std::cell::RefCell<Option<slint::SharedString>>>;

/// The About page on the testing backend `open` would set up, and that backend's record of links.
fn about_with_links(name: &str) -> (AppWindow, OpenedUrl) {
    let backend = TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        ..Default::default()
    });
    let opened = backend.open_url.clone();
    slint::platform::set_platform(Box::new(backend)).unwrap();
    (common::open_again(&layout(name, None), Page::About), opened)
}

/// Whether the element labelled `label` is whole inside the 520 px tall window.
fn whole_at_minimum(window: &AppWindow, label: &str) -> bool {
    ElementHandle::find_by_accessible_label(window, label)
        .next()
        .is_some_and(|e| {
            let y = e.absolute_position().y;
            y >= 0.0 && y + e.size().height <= 520.0
        })
}

/// UI rule 25 and the About row of the edge-case table: at 720 x 520 every block is whole and the
/// page does not scroll, with no banner, the offline daemon's and a broken config's.
#[test]
fn about_fits_the_minimum_size_with_a_banner() {
    let window = open(&layout("about-fit", None), Page::About);
    window
        .window()
        .set_size(slint::LogicalSize::new(720.0, 520.0));
    for (banner, daemon) in [
        ("", ""),
        ("", oxi_core::strings::DAEMON_OFFLINE),
        ("Synthetic config trouble.", ""),
    ] {
        window.set_banner_text(banner.into());
        window.set_daemon_text(daemon.into());
        window.set_daemon_start(!daemon.is_empty());
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(1));
        for label in [
            "Apache-2.0 licence",
            "tiny-skia",
            "github.com/makarasty",
            "Made in Ukraine",
        ] {
            assert!(
                whole_at_minimum(&window, label),
                "{label} is cut (banner {banner:?}, daemon {daemon:?})"
            );
        }
        // The badge lives in the Slint tile's popup: whole once open, then Esc closes it.
        open_slint_badge(&window);
        assert!(
            whole_at_minimum(&window, "#MadeWithSlint"),
            "the Slint badge is cut (banner {banner:?}, daemon {daemon:?})"
        );
        press(&window, Key::Escape);
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(1));
        assert!(
            !has(&window, "#MadeWithSlint"),
            "Esc left the Slint badge open"
        );
        let at = one(&window, "Version").absolute_position();
        window
            .window()
            .dispatch_event(WindowEvent::PointerScrolled {
                position: slint::LogicalPosition::new(at.x, at.y),
                delta_x: 0.0,
                delta_y: -100_000.0,
            });
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(500));
        assert_eq!(
            window.get_scroll_y(),
            0.0,
            "About scrolls at 720 x 520 (banner {banner:?}, daemon {daemon:?})"
        );
    }
}

/// The Built-with tiles sit on whole pixels, so their 1 px strokes stay crisp (UI rule 20), and
/// every gap between columns is one tile gap.
#[test]
fn about_tiles_sit_on_whole_pixels() {
    let window = open(&layout("about-crisp", None), Page::About);
    for (w, h) in [(720.0_f32, 520.0_f32), (1000.0, 560.0), (1280.0, 860.0)] {
        window.window().set_size(slint::LogicalSize::new(w, h));
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(1));
        let row = ["Rust", "Slint", "whisper.cpp"].map(|name| {
            let tile = with_role(&window, name, AccessibleRole::Button);
            (tile.absolute_position().x, tile.size().width)
        });
        for (x, width) in row {
            assert!(
                x.fract().abs() < 1e-3 && width.fract().abs() < 1e-3,
                "{w}: tile at {x}, {width} wide"
            );
        }
        for pair in row.windows(2) {
            let gap = pair[1].0 - (pair[0].0 + pair[0].1);
            assert!((gap - 8.0).abs() < 0.01, "{w}: column gap {gap}");
        }
    }
}

#[test]
fn theme_pick_writes_ui_theme() {
    let l = layout("theme", Some("version = 2\n# mine\n"));
    let window = open(&l, Page::Hotkey);
    window.set_page(Page::System);
    with_role(&window, "Theme", AccessibleRole::RadioGroup);
    let dark = card(&window, "Dark");
    assert_eq!(dark.accessible_checked(), Some(false));
    dark.invoke_accessible_default_action();
    assert_eq!(window.get_theme(), ThemeChoice::Dark);
    assert_eq!(card(&window, "Dark").accessible_checked(), Some(true));
    let text = fs::read_to_string(l.config_file()).unwrap();
    assert!(text.contains("# mine"), "comment lost: {text}");
    assert_eq!(
        ConfigFile::parse(&text).unwrap().config.ui.theme,
        Theme::Dark
    );
}

#[test]
fn newer_config_is_read_only() {
    let original = "version = 99\n[ui]\ntheme = \"light\"\n";
    let l = layout("newer", Some(original));
    let window = open(&l, Page::Hotkey);
    assert!(window.global::<Shell>().get_locked());
    assert!(window.get_banner_text().contains("newer version"));
    assert!(
        window
            .get_banner_text()
            .ends_with("Update OxiYapper to change settings here.")
    );
    assert!(has(&window, "Open file"), "the banner gives a way out");
    assert!(!has(&window, "Reset to defaults"));
    assert_eq!(window.get_theme(), ThemeChoice::Light);

    window.set_page(Page::System);
    let dark = card(&window, "Dark");
    assert_eq!(dark.accessible_enabled(), Some(false));
    dark.invoke_accessible_default_action();
    assert_eq!(window.get_theme(), ThemeChoice::Light);
    assert_eq!(fs::read_to_string(l.config_file()).unwrap(), original);
}

#[test]
fn broken_config_shows_its_line_and_resets_keeping_a_backup() {
    let original = "version = 2\n[audio]\ngain_db = \n";
    let l = layout("broken", Some(original));
    let window = open(&l, Page::Hotkey);
    let banner = window.get_banner_text();
    assert!(banner.contains("line 3"), "banner: {banner}");
    assert!(window.global::<Shell>().get_locked());
    assert!(has(&window, "Open file"));

    one(&window, "Reset to defaults").invoke_accessible_default_action();
    let backup = l.config_dir.join("config.toml.bak");
    assert_eq!(fs::read_to_string(backup).unwrap(), original);
    let text = fs::read_to_string(l.config_file()).unwrap();
    assert!(!ConfigFile::parse(&text).unwrap().read_only);
    assert_eq!(window.get_banner_text(), "");
    assert!(!window.global::<Shell>().get_locked());
    assert!(!has(&window, "Reset to defaults"));
}

#[test]
fn reset_drops_the_daemons_stale_config_error() {
    let l = layout("reset-linked", Some("version = 2\n[audio]\ngain_db = \n"));
    let (ipc, _rx) = Ipc::fake();
    let window = open_linked(&l, Page::Hotkey, ipc.clone());
    ipc.deliver(&Event::Push(Push::ConfigError {
        file: "config.toml".into(),
        line: 3,
        message: "bad value".into(),
    }));
    assert_eq!(banners(&window).len(), 1);

    one(&window, "Reset to defaults").invoke_accessible_default_action();
    assert_eq!(window.get_banner_text(), "");
    assert_eq!(banners(&window), Vec::<String>::new());
}

#[test]
fn thumb_picker_takes_a_click_then_arrow_keys() {
    let l = layout("keys", None);
    let window = open(&l, Page::Hotkey);
    window.set_page(Page::System);
    card(&window, "Dark").mock_single_click(PointerEventButton::Left);
    assert_eq!(window.get_theme(), ThemeChoice::Dark);
    let key = |key: Key| {
        window
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
        window
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
    };
    key(Key::LeftArrow);
    assert_eq!(window.get_theme(), ThemeChoice::Light);
    assert_eq!(card(&window, "Light").accessible_checked(), Some(true));
    key(Key::RightArrow);
    key(Key::RightArrow);
    assert_eq!(
        window.get_theme(),
        ThemeChoice::Dark,
        "stops at the last card"
    );
    let text = fs::read_to_string(l.config_file()).unwrap();
    assert_eq!(
        ConfigFile::parse(&text).unwrap().config.ui.theme,
        Theme::Dark
    );
}

/// Labels of the banners on screen, sorted, each once (a component instance and its root
/// element both match the role).
fn banners(window: &AppWindow) -> Vec<String> {
    let mut labels: Vec<String> = ElementQuery::from_root(window)
        .match_descendants()
        .match_accessible_role(AccessibleRole::Banner)
        .find_all()
        .iter()
        .filter_map(|e| e.accessible_label().map(|l| l.to_string()))
        .collect();
    labels.sort();
    labels.dedup();
    labels
}

#[test]
fn failed_reset_keeps_the_parse_banner_and_its_buttons() {
    let original = "version = 2
[audio]
gain_db = 
";
    let l = layout("reset-fails", Some(original));
    // A directory where the backup goes makes the copy fail.
    fs::create_dir(l.config_dir.join("config.toml.bak")).unwrap();
    let window = open(&l, Page::Hotkey);
    one(&window, "Reset to defaults").invoke_accessible_default_action();

    assert!(window.global::<Shell>().get_locked());
    assert!(window.get_banner_text().contains("line 3"));
    assert!(has(&window, "Open file"));
    assert!(has(&window, "Reset to defaults"));
    assert_eq!(banners(&window).len(), 2, "parse banner plus the failure");
    assert_eq!(fs::read_to_string(l.config_file()).unwrap(), original);
}

#[test]
fn failed_open_file_keeps_the_parse_banner_and_its_buttons() {
    let l = layout(
        "open-fails",
        Some(
            "version = 2
[audio]
gain_db = 
",
        ),
    );
    let window = open(&l, Page::Hotkey);
    // Gone before "Open file", so the shell has nothing to open.
    fs::remove_file(l.config_file()).unwrap();
    one(&window, "Open file").invoke_accessible_default_action();

    assert!(window.get_banner_text().contains("line 3"));
    assert!(has(&window, "Reset to defaults"));
    assert_eq!(banners(&window).len(), 2, "parse banner plus the failure");
}

#[test]
fn a_successful_save_clears_the_save_error() {
    let l = layout("save-recovers", None);
    let window = open(&l, Page::Hotkey);
    window.set_page(Page::System);
    // A directory in the file's place makes the atomic rename fail.
    fs::create_dir(l.config_file()).unwrap();
    card(&window, "Dark").invoke_accessible_default_action();
    assert_eq!(banners(&window).len(), 1, "the save error shows");

    fs::remove_dir(l.config_file()).unwrap();
    card(&window, "Light").invoke_accessible_default_action();
    assert_eq!(banners(&window), Vec::<String>::new());
    let text = fs::read_to_string(l.config_file()).unwrap();
    assert_eq!(
        ConfigFile::parse(&text).unwrap().config.ui.theme,
        Theme::Light
    );
}

/// A window on a fake daemon link; the receiver gets what the window sends to the daemon.
fn linked(name: &str) -> (AppWindow, Ipc, std::sync::mpsc::Receiver<Cmd>) {
    let (ipc, rx) = Ipc::fake();
    let window = open_linked(&layout(name, None), Page::Hotkey, ipc.clone());
    (window, ipc, rx)
}

#[test]
fn offline_daemon_shows_start_and_start_is_sent() {
    let (window, ipc, rx) = linked("daemon-offline");
    assert_eq!(banners(&window), Vec::<String>::new(), "online: no banner");
    ipc.deliver(&Event::Link(Link::Offline));
    assert_eq!(banners(&window).len(), 1);
    assert!(window.get_daemon_text().contains("not running"));
    assert!(!has(&window, "Restart"));
    one(&window, "Start").invoke_accessible_default_action();
    assert_eq!(rx.try_recv(), Ok(Cmd::Start));
    ipc.deliver(&Event::Link(Link::Online));
    assert_eq!(banners(&window), Vec::<String>::new(), "online clears it");
}

#[test]
fn update_notice_shows_restart_and_restart_is_sent() {
    let (window, ipc, rx) = linked("daemon-restart");
    ipc.deliver(&Event::Link(Link::Mismatch { daemon_proto: 9 }));
    assert_eq!(banners(&window), ["Restart OxiYapper to finish the update"]);
    assert!(!has(&window, "Start"));
    one(&window, "Restart").invoke_accessible_default_action();
    assert_eq!(rx.try_recv(), Ok(Cmd::Restart));
}

#[test]
fn untrusted_pipe_shows_the_note_without_a_button() {
    let (window, ipc, rx) = linked("daemon-untrusted");
    ipc.deliver(&Event::Link(Link::Untrusted));
    assert!(window.get_daemon_text().contains("untrusted"));
    assert_eq!(banners(&window).len(), 1);
    assert!(!has(&window, "Start") && !has(&window, "Restart"));
    assert!(rx.try_recv().is_err());
}

#[test]
fn daemon_config_error_uses_the_same_banner() {
    let (window, ipc, _rx) = linked("daemon-config-error");
    ipc.deliver(&Event::Push(Push::ConfigError {
        file: "config.toml".into(),
        line: 7,
        message: "bad value".into(),
    }));
    assert_eq!(banners(&window), ["config.toml line 7: bad value"]);
    assert!(!has(&window, "Start") && !has(&window, "Restart"));
    // Other pushes leave the banner alone.
    ipc.deliver(&Event::Push(Push::Raise));
    assert_eq!(banners(&window).len(), 1);
}

#[test]
fn raise_unminimises_the_window() {
    let (window, ipc, _rx) = linked("daemon-raise");
    window.window().set_minimized(true);
    assert!(window.window().is_minimized());
    ipc.deliver(&Event::Push(Push::Raise));
    assert!(!window.window().is_minimized());
}

#[test]
fn a_failed_spawn_reason_shows_under_the_offline_note_until_online() {
    let (window, ipc, _rx) = linked("daemon-spawn");
    window.set_spawn_error("Could not start OxiYapper: entity not found.".into());
    ipc.deliver(&Event::Link(Link::Offline));
    let shown = banners(&window);
    assert!(shown[0].contains("not running") && shown[0].contains("entity not found"));
    ipc.deliver(&Event::Link(Link::Online));
    assert_eq!(window.get_spawn_error(), "");
}

#[test]
fn a_state_reported_before_the_window_followed_the_link_still_shows() {
    i_slint_backend_testing::init_no_event_loop();
    let (ipc, _rx) = Ipc::fake();
    ipc.deliver(&Event::Link(Link::Untrusted));
    let window = oxi_settings::open_with(layout("daemon-late", None), ipc).unwrap();
    assert!(window.get_daemon_text().contains("untrusted"));
    assert_eq!(banners(&window).len(), 1);
}

#[test]
fn spawn_reason_clears_on_a_blocked_link_and_on_restart_but_not_on_connecting() {
    let (window, ipc, _rx) = linked("daemon-spawn-clears");
    let failed = || window.set_spawn_error("Could not start OxiYapper: entity not found.".into());
    failed();
    ipc.deliver(&Event::Link(Link::Connecting));
    ipc.deliver(&Event::Link(Link::Offline));
    assert!(window.get_spawn_error().contains("entity not found"));
    for link in [
        Link::Mismatch { daemon_proto: 9 },
        Link::Busy,
        Link::Stuck,
        Link::Untrusted,
        Link::Foreign,
    ] {
        failed();
        ipc.deliver(&Event::Link(link));
        assert_eq!(window.get_spawn_error(), "", "{link:?}");
    }
    failed();
    ipc.deliver(&Event::Link(Link::Mismatch { daemon_proto: 9 }));
    failed();
    one(&window, "Restart").invoke_accessible_default_action();
    assert_eq!(window.get_spawn_error(), "");
}

/// One poll of the file from outside.
fn poll() {
    i_slint_backend_testing::mock_elapsed_time(oxi_settings::FILE_POLL);
}

#[test]
fn a_file_fixed_outside_clears_the_banner_and_refreshes_the_pages() {
    let l = layout("fixed-outside", Some("version = 2\n[audio]\ngain_db = \n"));
    let window = open(&l, Page::Hotkey);
    assert!(window.get_banner_text().contains("line 3"));
    assert!(window.global::<Shell>().get_locked());

    fs::write(l.config_file(), "version = 2\n[ui]\ntheme = \"dark\"\n").unwrap();
    poll();
    assert_eq!(window.get_banner_text(), "");
    assert!(!window.global::<Shell>().get_locked());
    assert_eq!(window.get_theme(), ThemeChoice::Dark);
    assert!(!has(&window, "Reset to defaults"));

    // Broken again: the banner comes back, the hand edit stays on disk.
    let broken = "version = 2\n[ui]\ntheme = \n";
    fs::write(l.config_file(), broken).unwrap();
    poll();
    assert!(window.get_banner_text().contains("line 3"));
    assert!(window.global::<Shell>().get_locked());
    assert_eq!(fs::read_to_string(l.config_file()).unwrap(), broken);
}

#[test]
fn a_file_fixed_outside_drops_the_daemons_config_error() {
    let l = layout("fixed-linked", Some("version = 2\n[audio]\ngain_db = \n"));
    let (ipc, _rx) = Ipc::fake();
    let window = open_linked(&l, Page::Hotkey, ipc.clone());
    ipc.deliver(&Event::Push(Push::ConfigError {
        file: "config.toml".into(),
        line: 3,
        message: "bad value".into(),
    }));
    assert_eq!(
        banners(&window),
        [window.get_banner_text().to_string()],
        "the same broken file is reported once"
    );

    fs::write(l.config_file(), "version = 2\n").unwrap();
    poll();
    assert_eq!(banners(&window), Vec::<String>::new());
}

fn press(window: &AppWindow, key: impl Into<slint::SharedString> + Clone) {
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: key.clone().into(),
    });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
}

/// `key` with Ctrl (and Shift) held, as the platform delivers a chord.
fn chord(window: &AppWindow, shift: bool, key: Key) {
    let held: &[Key] = if shift {
        &[Key::Control, Key::Shift]
    } else {
        &[Key::Control]
    };
    for k in held {
        window
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: (*k).into() });
    }
    press(window, key);
    for k in held.iter().rev() {
        window
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text: (*k).into() });
    }
}

#[test]
fn ctrl_tab_cycles_the_pages_both_ways() {
    let window = open(&layout("ctrl-tab", None), Page::System);
    chord(&window, false, Key::Tab);
    assert_eq!(window.get_page(), Page::About);
    chord(&window, false, Key::Tab);
    assert_eq!(window.get_page(), Page::Home, "wraps to the first page");
    chord(&window, true, Key::Tab);
    assert_eq!(window.get_page(), Page::About, "Ctrl+Shift+Tab goes back");
    chord(&window, true, Key::Backtab);
    assert_eq!(window.get_page(), Page::System);
}

#[test]
fn ctrl_tab_keeps_working_after_the_focused_field_leaves_with_its_page() {
    let window = open(&layout("ctrl-tab-focus", None), Page::Hotkey);
    // The rail, then the first field of the page.
    press(&window, Key::Tab);
    press(&window, Key::Tab);
    for want in [Page::Audio, Page::Text, Page::Overlay] {
        chord(&window, false, Key::Tab);
        assert_eq!(window.get_page(), want);
    }
}

#[test]
fn rail_cursor_follows_the_page_ctrl_tab_opened() {
    let window = open(&layout("rail-cursor", None), Page::Hotkey);
    press(&window, Key::Tab);
    chord(&window, false, Key::Tab);
    assert_eq!(window.get_page(), Page::Audio);
    press(&window, Key::Return);
    assert_eq!(
        window.get_page(),
        Page::Audio,
        "Enter opens the page it is on"
    );
    press(&window, Key::DownArrow);
    press(&window, Key::Return);
    assert_eq!(
        window.get_page(),
        Page::Text,
        "arrows start from the open page"
    );
}

#[test]
fn a_page_opens_at_its_top() {
    // Home with both banners is the page that still scrolls at the minimum size.
    let window = open(&layout("scroll-top", None), Page::Home);
    window.set_banner_text("Synthetic config trouble.".into());
    window.set_daemon_text(oxi_core::strings::DAEMON_OFFLINE.into());
    window.set_daemon_start(true);
    window
        .window()
        .set_size(slint::LogicalSize::new(720.0, 520.0));
    window
        .window()
        .dispatch_event(WindowEvent::PointerScrolled {
            position: slint::LogicalPosition::new(500.0, 300.0),
            delta_x: 0.0,
            delta_y: -400.0,
        });
    assert!(
        window.get_scroll_y() < 0.0,
        "Home with banners scrolls at 720 x 520"
    );
    window.set_page(Page::About);
    // The event loop runs the change handlers; the testing backend has none.
    slint::platform::update_timers_and_animations();
    assert_eq!(window.get_scroll_y(), 0.0, "About opens at its top");
    window.set_page(Page::Home);
    slint::platform::update_timers_and_animations();
    assert_eq!(window.get_scroll_y(), 0.0, "and so does Home again");
}

#[test]
fn rail_takes_up_down_and_enter() {
    let window = open(&layout("rail-keys", None), Page::Hotkey);
    // The first Tab stop is the rail; the focus starts on the open page.
    press(&window, Key::Tab);
    press(&window, Key::DownArrow);
    press(&window, Key::DownArrow);
    assert_eq!(
        window.get_page(),
        Page::Hotkey,
        "arrows only move the focus"
    );
    press(&window, Key::Return);
    assert_eq!(window.get_page(), Page::Text);
    for _ in 0..5 {
        press(&window, Key::UpArrow);
    }
    press(&window, " ");
    assert_eq!(window.get_page(), Page::Home, "stops at the first page");
}

#[test]
fn rail_turns_compact_under_960_px() {
    let window = open(&layout("compact", None), Page::Hotkey);
    let width = |w: f32| {
        window.window().set_size(slint::LogicalSize::new(w, 780.0));
        tab(&window, "Audio").size().width
    };
    assert!((width(1120.0) - 212.0).abs() < 0.5, "full rail");
    assert!((width(959.0) - 48.0).abs() < 0.5, "icons only");
    assert!((width(960.0) - 212.0).abs() < 0.5, "full again at 960");
}

#[test]
fn rail_starts_with_what_a_person_configures_first() {
    let window = open(&layout("rail-order", None), Page::Home);
    let labels = [
        "Home", "Model", "Hotkey", "Audio", "Text", "Overlay", "Test", "System", "About",
    ];
    let tops: Vec<f32> = labels
        .iter()
        .map(|l| tab(&window, l).absolute_position().y)
        .collect();
    assert!(tops.is_sorted(), "{tops:?}");
}

/// The real window on the real backend; `OXI_WINDOW_TEST=1 cargo test -p oxi-settings --test
/// shell real_window -- --test-threads=1` (the event loop needs the main thread).
#[cfg(windows)]
#[allow(clippy::cast_precision_loss)] // window pixels
#[test]
fn real_window_has_a_maximize_box_and_no_maximum_size() {
    use slint::{LogicalSize, PhysicalPosition, Timer, TimerMode};
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;
    use win32::{GetClientRect, GetWindowLongW, GetWindowRect, SendMessageW, this_window};

    const GWL_STYLE: i32 = -16;
    const WS_MAXIMIZEBOX: i32 = 0x0001_0000;
    const WM_GETMINMAXINFO: u32 = 0x0024;

    /// This thread's visible window as the OS has it: the style, and the largest client size
    /// dragging an edge can reach (`ptMaxTrackSize`, which counts the frame), in physical pixels.
    fn frame() -> Option<(i32, i32, i32)> {
        let hwnd = this_window();
        if hwnd.is_null() {
            return None;
        }
        let (mut client, mut outer, mut info) = ([0; 4], [0; 4], [0; 10]);
        // SAFETY: `hwnd` is a live window of this thread; `info` has the layout of MINMAXINFO,
        // ten LONGs.
        unsafe {
            GetClientRect(hwnd, &raw mut client);
            GetWindowRect(hwnd, &raw mut outer);
            SendMessageW(hwnd, WM_GETMINMAXINFO, 0, &raw mut info as isize);
            let extra = (
                outer[2] - outer[0] - client[2],
                outer[3] - outer[1] - client[3],
            );
            Some((
                GetWindowLongW(hwnd, GWL_STYLE),
                info[8] - extra.0,
                info[9] - extra.1,
            ))
        }
    }

    if std::env::var_os("OXI_WINDOW_TEST").is_none() {
        return;
    }
    oxi_settings::use_skia();
    let window = oxi_settings::open(layout("real-frame", None)).unwrap();
    window
        .window()
        .set_position(PhysicalPosition::new(-20_000, -20_000));
    window.window().set_size(LogicalSize::new(1000.0, 700.0));

    let seen = Rc::new(Cell::new(None));
    Timer::single_shot(Duration::from_secs(20), || {
        let _ = slint::quit_event_loop();
    });
    let poll = Timer::default();
    poll.start(TimerMode::Repeated, Duration::from_millis(20), {
        let seen = seen.clone();
        move || {
            if let Some(found) = frame() {
                seen.set(Some(found));
                let _ = slint::quit_event_loop();
            }
        }
    });
    window.run().unwrap();

    let (style, w, h) = seen.get().expect("the window showed");
    assert_ne!(style & WS_MAXIMIZEBOX, 0, "style {style:#x}");
    // UI rule 24: no maximum, so dragging an edge is not stopped at the old 1280 x 860.
    let scale = window.window().scale_factor();
    assert_ne!(
        ((w as f32 / scale).round(), (h as f32 / scale).round()),
        (1280.0, 860.0)
    );
}

/// Item 3: the title bar shows the sidebar's mark at the OS's small-icon size, not a 64 px render
/// the OS shrinks, and the taskbar keeps the exe's icon (no small bitmap as `ICON_BIG`). Run:
/// `SLINT_SCALE_FACTOR=1.5 OXI_WINDOW_TEST=1 cargo test -p oxi-settings --test shell
/// real_window_icon -- --test-threads=1` (the event loop needs the main thread).
#[cfg(windows)]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)] // window pixels
#[test]
fn real_window_icon_is_a_small_icon() {
    use slint::{LogicalSize, PhysicalPosition, Timer, TimerMode};
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;

    const ICON_SMALL: usize = 0;
    const ICON_BIG: usize = 1;

    if std::env::var_os("OXI_WINDOW_TEST").is_none() {
        return;
    }
    win32::open_behind();
    oxi_settings::use_skia();
    let window = oxi_settings::open(layout("real-icon", None)).unwrap();
    window
        .window()
        .set_position(PhysicalPosition::new(-20_000, -20_000));
    window.window().set_size(LogicalSize::new(1000.0, 700.0));

    // (small icon, scale) once up, then the big icon after the scale changed.
    let opened = Rc::new(Cell::new(None));
    let big = Rc::new(Cell::new(None));
    Timer::single_shot(Duration::from_secs(20), || {
        let _ = slint::quit_event_loop();
    });
    let poll = Timer::default();
    poll.start(TimerMode::Repeated, Duration::from_millis(20), {
        let (opened, big, weak) = (opened.clone(), big.clone(), window.as_weak());
        move || {
            let hwnd = win32::this_window();
            let (Some(window), false) = (weak.upgrade(), hwnd.is_null()) else {
                return;
            };
            if opened.get().is_some() {
                // The title is set in the same backend pass as the icon, right after it.
                if win32::title(hwnd).starts_with("Synth") {
                    big.set(Some(win32::icon_width(hwnd, ICON_BIG)));
                    let _ = slint::quit_event_loop();
                }
                return;
            }
            {
                let scale = window.window().scale_factor();
                opened.set(Some((win32::icon_width(hwnd, ICON_SMALL), scale)));
                // What moving to another display does: a scale with another small bitmap.
                let other = if scale > 1.25 && scale <= 1.5 {
                    1.2
                } else {
                    1.5
                };
                window
                    .window()
                    .dispatch_event(WindowEvent::ScaleFactorChanged {
                        scale_factor: other,
                    });
                window.set_app_name("Synth".into());
            }
        }
    });
    window.run().unwrap();

    let (small, scale) = opened.get().expect("the window showed");
    let big = big.get().expect("the title changed");
    let want = [16, 20, 24, 32]
        .into_iter()
        .find(|size| *size as f32 >= 16.0 * scale)
        .unwrap_or(32);
    assert_eq!(small, want, "at scale {scale}");
    assert!(
        big == 0 || big >= 32,
        "ICON_BIG is {big} px after a scale change: the taskbar would stretch it"
    );
}

/// Pixels the OS dropped come back, with a paint message (`InvalidateRect`) and without one (the
/// file poll). The wipe leaves them black with alpha 0, what a lost frame shows as (see
/// `draw_whole_frames`). Run: `OXI_WINDOW_TEST=1 cargo test -p oxi-settings --test shell os_lost --
/// --test-threads=1` (the event loop needs the main thread).
#[cfg(windows)]
#[test]
fn os_lost_pixels_are_repainted() {
    use slint::{LogicalSize, PhysicalPosition, Timer, TimerMode};
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;
    use win32::{InvalidateRect, client_pixels, this_window, wipe};

    if std::env::var_os("OXI_WINDOW_TEST").is_none() {
        return;
    }
    // The OS clips a window DC to the screen, so this one stays on it, behind every other window.
    win32::open_behind();
    oxi_settings::use_skia();
    let window = oxi_settings::open(layout("real-repaint", None)).unwrap();
    window
        .window()
        .set_position(PhysicalPosition::new(120, 120));
    window.window().set_size(LogicalSize::new(1000.0, 700.0));

    // The stage reached: 0 waits for the first frame, 1 and 2 for the repaint after a wipe.
    let stage = Rc::new(Cell::new(0));
    let different = Rc::new(Cell::new(0));
    Timer::single_shot(Duration::from_secs(20), || {
        let _ = slint::quit_event_loop();
    });
    let poll = Timer::default();
    poll.start(TimerMode::Repeated, Duration::from_millis(50), {
        let (weak, stage, different) = (window.as_weak(), stage.clone(), different.clone());
        move || {
            let hwnd = this_window();
            let (Some(window), false) = (weak.upgrade(), hwnd.is_null()) else {
                return;
            };
            let Ok(want) = window.window().take_snapshot() else {
                return;
            };
            let off = client_pixels(hwnd)
                .iter()
                .zip(want.as_bytes().as_chunks::<4>().0)
                .filter(|(got, want)| {
                    let [b, g, r, _] = got.to_le_bytes();
                    [r, g, b]
                        .iter()
                        .zip(*want)
                        .any(|(a, b)| a.abs_diff(*b) > 24)
                })
                .count();
            different.set(off);
            if off * 100 > want.as_bytes().len() / 4 {
                return;
            }
            match stage.replace(stage.get() + 1) {
                0 => {
                    wipe(hwnd);
                    // SAFETY: a null rectangle means the whole client area of a live window.
                    unsafe { InvalidateRect(hwnd, std::ptr::null(), 1) };
                }
                1 => wipe(hwnd),
                _ => {
                    let _ = slint::quit_event_loop();
                }
            }
        }
    });
    window.run().unwrap();
    assert_eq!(stage.get(), 3, "{} pixels still differ", different.get());
}

/// Left edge of the page title, the keyboard panel's edges when it is open, and the page width.
#[test]
fn opening_the_keyboard_moves_no_content_edge() {
    let heading_x = |window: &AppWindow| {
        ElementHandle::find_by_element_type_name(window, "HeaderRow")
            .next()
            .expect("the page header row")
            .absolute_position()
            .x
    };
    i_slint_backend_testing::init_no_event_loop();
    for (i, (w, h)) in [(720.0, 520.0), (1000.0, 700.0), (1280.0, 860.0)]
        .into_iter()
        .enumerate()
    {
        let l = layout(&format!("keyboard-width-{i}"), None);
        let window = oxi_settings::open_with(l, Ipc::fake().0).unwrap();
        window.set_page(Page::Hotkey);
        window.show().unwrap();
        window.window().set_size(slint::LogicalSize::new(w, h));
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(1));
        let shell = window.global::<Shell>();
        let (left, width) = (heading_x(&window), shell.get_page_width());
        let edit = one(&window, "Change hotkey");
        let edit_right = edit.absolute_position().x + edit.size().width;

        edit.invoke_accessible_default_action();
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(1));
        assert_eq!(heading_x(&window), left, "{w}x{h}: left edge");
        assert_eq!(shell.get_page_width(), width, "{w}x{h}: page width");
        let edit = one(&window, "Change hotkey");
        assert_eq!(
            edit.absolute_position().x + edit.size().width,
            edit_right,
            "{w}x{h}: right edge"
        );
        let board = with_role(&window, "Hotkey keyboard", AccessibleRole::Groupbox);
        assert_eq!(board.absolute_position().x, left, "{w}x{h}: panel left");
        assert_eq!(board.size().width, width, "{w}x{h}: panel width");
    }
}

#[test]
fn page_title_centres_on_the_first_rail_item() {
    let (window, _ipc, _rx) = linked("title-line");
    let centre = |e: &ElementHandle| e.absolute_position().y + e.size().height / 2.0;
    for (w, h) in [(720.0, 520.0), (1000.0, 700.0), (1280.0, 800.0)] {
        window.window().set_size(slint::LogicalSize::new(w, h));
        for page in [
            Page::Home,
            Page::Model,
            Page::Hotkey,
            Page::Audio,
            Page::Text,
            Page::Overlay,
            Page::History,
            Page::Test,
            Page::System,
            Page::About,
        ] {
            window.set_page(page);
            let rail = centre(&tab(&window, "Home"));
            // The header row is the page name's HeaderRow or the page's own SubTabs; both are
            // centred on the first rail item.
            let (row, label) =
                match ElementHandle::find_by_element_type_name(&window, "HeaderRow").next() {
                    // The page name is centred in its row.
                    Some(row) => (centre(&row), centre(&row)),
                    None => (
                        centre(
                            &ElementHandle::find_by_element_type_name(&window, "SubTabs")
                                .next()
                                .unwrap_or_else(|| panic!("{page:?}: no header row, no SubTabs")),
                        ),
                        centre(
                            &ElementHandle::find_by_element_id(&window, "SubTabs::strong")
                                .next()
                                .expect("a tab label"),
                        ),
                    ),
                };
            assert!(
                (rail - row).abs() <= 1.0,
                "{page:?} at {w}x{h}: rail {rail}, header row {row}"
            );
            // The status chip's text sits on the line of the page name or the tab labels.
            let chip = centre(&with_role(&window, "Running", AccessibleRole::Button));
            assert!(
                (chip - label).abs() <= 1.0,
                "{page:?} at {w}x{h}: chip {chip}, header label {label}"
            );
        }
    }
}

#[test]
fn daemon_chip_shows_the_state_and_starts_the_daemon() {
    let (window, ipc, rx) = linked("chip");
    let chip = |w: &AppWindow| (w.get_chip_text().to_string(), w.get_chip_level());
    assert_eq!(chip(&window), ("Running".into(), ChipLevel::Ok));
    assert!(has(&window, "Running"));
    ipc.deliver(&Event::Link(Link::Offline));
    assert_eq!(chip(&window), ("Not running".into(), ChipLevel::Down));
    with_role(&window, "Not running", AccessibleRole::Button).invoke_accessible_default_action();
    assert_eq!(rx.try_recv(), Ok(Cmd::Start));
    // A daemon of another version still runs: the chip is the normal state and takes no click;
    // the banner's Restart does.
    ipc.deliver(&Event::Link(Link::Mismatch { daemon_proto: 9 }));
    assert_eq!(chip(&window), ("Running".into(), ChipLevel::Ok));
    window.invoke_status_clicked();
    assert!(rx.try_recv().is_err(), "the normal state takes no click");
    one(&window, "Restart").invoke_accessible_default_action();
    assert_eq!(rx.try_recv(), Ok(Cmd::Restart));
    ipc.deliver(&Event::Link(Link::Stuck));
    assert!(has(&window, "Not answering"));

    let alone = common::open_again(&layout("chip-none", None), Page::Hotkey);
    assert_eq!(alone.get_chip_level(), ChipLevel::Hidden);
    assert!(
        !has(&alone, "Running") && !has(&alone, "Not running"),
        "no link, no chip"
    );
}

/// Room for a whole page: elements outside the viewport are not in the element search.
fn tall(window: &AppWindow) {
    window
        .window()
        .set_size(slint::LogicalSize::new(1280.0, 2400.0));
}

fn system_tab(window: &AppWindow, label: &str) {
    with_role(window, label, AccessibleRole::Tab).invoke_accessible_default_action();
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(1));
}

fn press_switch(window: &AppWindow, label: &str) {
    with_role(window, label, AccessibleRole::Switch).invoke_accessible_default_action();
}

fn editor(window: &AppWindow, label: &str) -> ElementHandle {
    with_role(window, label, AccessibleRole::TextInput)
}

/// Opens the Text sub-tab `label`.
fn sub_tab(window: &AppWindow, label: &str) {
    with_role(window, label, AccessibleRole::Tab).invoke_accessible_default_action();
}

#[test]
fn system_warnings_move_no_row() {
    let l = layout("still-system", None);
    let window = open(&l, Page::System);
    system_tab(&window, "Recordings and files");
    tall(&window);
    let before = spots(&window);

    // Item 20: the "!" of the row, with the old warning line as its text.
    press_switch(&window, "Log dictated text");
    assert!(has(
        &window,
        "Dictated text is written to the log files while this is on. Turn it off when you are done."
    ));
    assert_nothing_moved(&before, &spots(&window));
    press_switch(&window, "Log dictated text");

    // The update confirm takes the row's own title and switch.
    system_tab(&window, "General");
    let before = spots(&window);
    press_switch(&window, "Check for updates");
    assert!(has(&window, "Update checks tell you when a fix is out."));
    assert!(has(&window, "Keep checking"));
    assert_nothing_moved(&before, &spots(&window));
}

#[test]
fn update_confirm_takes_the_focus_and_gives_it_back() {
    let l = layout("swap-focus", None);
    let window = open(&l, Page::System);
    tall(&window);
    let space = || {
        for event in [
            WindowEvent::KeyPressed { text: " ".into() },
            WindowEvent::KeyReleased { text: " ".into() },
        ] {
            window.window().dispatch_event(event);
        }
    };
    // The rail, the sub-tabs, the theme tiles, the "i" and the box of App language, Start at
    // sign-in, the "i" of Check for updates and its switch.
    for _ in 0..8 {
        press(&window, Key::Tab);
    }
    space();
    assert!(has(&window, "Keep checking"));
    // The next key lands on Keep checking, the one after on the switch again.
    space();
    assert!(!has(&window, "Keep checking"));
    assert_eq!(
        with_role(&window, "Check for updates", AccessibleRole::Switch).accessible_checked(),
        Some(true)
    );
    space();
    assert!(has(&window, "Keep checking"));
}

/// A hover tooltip leaves the keyboard focus where it was: the next key still reaches the focused
/// control (a `PopupWindow` tip took the focus, and the next Tab started over at the sidebar).
#[test]
fn a_hover_tip_does_not_take_the_keyboard_focus() {
    let l = layout("hover-tip-focus", None);
    let window = open(&l, Page::System);
    system_tab(&window, "Recordings and files");
    tall(&window);
    // Four Tabs from the opened sub-tab reach the Save recordings switch (limits, then switch).
    for _ in 0..4 {
        press(&window, Key::Tab);
    }
    let folder = with_role(&window, "Open recordings folder", AccessibleRole::Button);
    let (at, size) = (folder.absolute_position(), folder.size());
    window.window().dispatch_event(WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(at.x + size.width / 2.0, at.y + size.height / 2.0),
    });
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(700));
    assert!(has(&window, "Open recordings folder"), "the tip is open");
    let space = |window: &AppWindow| {
        for event in [
            WindowEvent::KeyPressed { text: " ".into() },
            WindowEvent::KeyReleased { text: " ".into() },
        ] {
            window.window().dispatch_event(event);
        }
    };
    space(&window);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(1));
    assert!(
        common::config(&l).config.history.enabled,
        "the key went elsewhere"
    );
}

#[test]
fn the_warning_is_a_tab_stop_that_opens_its_tooltip() {
    let l = layout("slot-focus", None);
    let window = open(&l, Page::System);
    system_tab(&window, "Recordings and files");
    tall(&window);
    let text = "Dictated text is written to the log files while this is on. Turn it off when you are done.";
    // The slot's own label, and the tooltip's text while that is open.
    let copies = || {
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(1));
        ElementHandle::find_by_accessible_label(&window, text).count()
    };
    press_switch(&window, "Log dictated text");
    assert_eq!(copies(), 1);
    // The rail, the sub-tabs, the "i" of Keep recordings, its two limits and their switch, the
    // folder field, the two folder buttons, Settings, Logs, the "!". Change folder shows only
    // where a folder dialog exists (can-browse).
    let stops = if cfg!(windows) { 12 } else { 11 };
    for _ in 0..stops {
        press(&window, Key::Tab);
    }
    assert_eq!(copies(), 2, "focus on the \"!\" opens the tooltip");
    press(&window, Key::Tab);
    assert_eq!(copies(), 1, "leaving it closes the tooltip");
    // The focus went on to the row's switch, not back to the top of the window.
    press(&window, " ");
    assert_eq!(copies(), 0, "the switch took the key");
}

#[test]
fn idle_never_and_the_engine_warning_move_no_row() {
    let l = layout("still-model", None);
    let (ipc, _sent) = Ipc::fake();
    let window = open_linked(&l, Page::Model, ipc.clone());
    tall(&window);

    // Item 4: "Never" is inside the field, at the field's own place. The field is on the
    // Settings tab; the tab switch is the one move this test allows, so measure after it.
    tab(&window, "Settings").invoke_accessible_default_action();
    let before = spots(&window);
    let unload = with_role(&window, "Unload after idle", AccessibleRole::Combobox);
    assert_eq!(unload.accessible_value().as_deref(), Some("After 5 min"));
    // A real click opens the list; the arrows walk it to the last choice, Enter takes it.
    unload.mock_single_click(PointerEventButton::Left);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(350));
    for _ in 0..5 {
        press(&window, Key::DownArrow);
    }
    press(&window, Key::Return);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(350));
    assert_eq!(unload.accessible_value().as_deref(), Some("Never"));
    assert_nothing_moved(&before, &spots(&window));

    let warning = "Synth GPU failed, running on CPU";
    ipc.deliver(&Event::Push(Push::Status {
        phase: oxi_core::machine::Phase::Idle,
        model: "turbo".into(),
        device: oxi_core::device::DeviceChoice::Auto,
        backend: "cpu".into(),
        warnings: vec![warning.into()],
    }));
    assert!(has(&window, warning));
    assert_nothing_moved(&before, &spots(&window));
}

#[test]
fn denoise_warning_moves_no_row() {
    let l = layout("still-audio", None);
    let window = open(&l, Page::Audio);
    tall(&window);
    with_role(&window, "Recording", AccessibleRole::Tab).invoke_accessible_default_action();
    let before = spots(&window);
    press_switch(&window, "Noise suppression");
    assert!(has(&window, "Noise suppression can make Whisper worse."));
    assert_nothing_moved(&before, &spots(&window));
}

#[test]
fn text_states_move_no_row() {
    let l = layout("still-text", None);
    fs::write(l.dictionary_file(), "synthalpha").unwrap();
    let window = open(&l, Page::Text);
    tall(&window);
    let before = spots(&window);
    let moved = |what: &str| {
        assert!(has(&window, what), "{what} is not shown");
        assert_nothing_moved(&before, &spots(&window));
    };

    editor(&window, "Dictionary").set_accessible_value("synthbeta");
    moved("Unsaved changes");

    let words = (0..3000).fold(String::new(), |mut s, i| {
        let _ = writeln!(s, "synthword{i}");
        s
    });
    editor(&window, "Dictionary").set_accessible_value(&words);
    assert!(
        spots(&window)
            .iter()
            .any(|s| s.0.starts_with("Over the limit")),
        "the dictionary should be over the budget"
    );
    assert_nothing_moved(&before, &spots(&window));

    // The file changed on disk under unsaved edits: Reload and Overwrite join the status line.
    fs::write(l.dictionary_file(), "synthgamma").unwrap();
    one(&window, "Save dictionary.txt").invoke_accessible_default_action();
    // The conflict is the worse of the two warnings the "!" now holds.
    assert!(
        spots(&window).iter().any(|s| s.0.starts_with(
            "dictionary.txt changed on disk since it was loaded.
Over the limit"
        )),
        "the \"!\" should say both"
    );
    assert_nothing_moved(&before, &spots(&window));
    assert!(has(&window, "Overwrite dictionary.txt"));

    // Each sub-tab is measured after its own switch, the one move this test allows.
    // A line of the replacements with no "=>".
    sub_tab(&window, "Replacements");
    let before = spots(&window);
    editor(&window, "Replacements").set_accessible_value("synth line without an arrow");
    assert!(!has(&window, "All lines valid"));
    assert_nothing_moved(&before, &spots(&window));

    // A refused duplicate rule is a toast.
    sub_tab(&window, "Per-app rules");
    one(&window, "New rule app").set_accessible_value("synth.exe");
    one(&window, "Add rule").invoke_accessible_default_action();
    let with_rule = spots(&window);
    one(&window, "New rule app").set_accessible_value("SYNTH.EXE");
    one(&window, "Add rule").invoke_accessible_default_action();
    assert_eq!(
        window.global::<Toast>().get_text(),
        "SYNTH.EXE already has a rule: change its mode in the list below."
    );
    assert_nothing_moved(&with_rule, &spots(&window));
}

#[test]
fn unreadable_file_moves_no_row() {
    let good = layout("still-readable", None);
    let window = open(&good, Page::Text);
    tall(&window);
    let before = spots(&window);
    let bad = layout("still-unreadable", None);
    fs::create_dir(bad.dictionary_file()).unwrap();
    let broken = common::open_again(&bad, Page::Text);
    tall(&broken);
    assert!(has(&broken, "Reload dictionary.txt"));
    assert_nothing_moved(&before, &spots(&broken));
}

#[test]
fn refused_hotkey_moves_nothing_in_the_panel() {
    let l = layout("still-hotkey", None);
    let window = open_linked(&l, Page::Hotkey, Ipc::fake().0);
    tall(&window);
    one(&window, "Change hotkey").invoke_accessible_default_action();
    let before = spots(&window);
    one(&window, "Esc").invoke_accessible_default_action();
    assert!(has(
        &window,
        "Esc cancels a recording and cannot be part of the hotkey"
    ));
    assert_nothing_moved(&before, &spots(&window));
}

#[test]
fn toast_pushes_nothing_replaces_the_last_and_leaves_by_itself() {
    use std::time::Duration;
    let elapse = |ms| i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(ms));
    let l = layout("toast", None);
    let window = open(&l, Page::System);
    let toast = window.global::<Toast>();
    let before = spots(&window);

    toast.invoke_show("Synth first".into());
    elapse(1);
    assert!(has(&window, "Synth first"));
    assert_nothing_moved(&before, &spots(&window));

    // The newest takes the slot and its full time.
    elapse(2_000);
    toast.invoke_show("Synth second".into());
    elapse(1);
    assert!(!has(&window, "Synth first"));
    elapse(2_000);
    assert_eq!(toast.get_text(), "Synth second");
    elapse(1_100);
    assert_eq!(toast.get_text(), "", "a toast leaves by itself");
    assert!(!has(&window, "Synth second"));
}

#[test]
fn system_theme_follows_the_os_scheme() {
    use slint::private_unstable_api::re_exports::{ColorScheme, WindowInner};

    let l = layout("system-theme", None);
    let window = open_default(&l);
    // What the backend writes when the window appears and when Windows switches.
    let os = |scheme| {
        WindowInner::from_pub(window.window())
            .context()
            .set_color_scheme(scheme);
    };
    let dark = |choice| {
        window.set_theme(choice);
        // `changed theme` handlers run from the event loop's timer pass.
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(1));
        window.global::<Tokens>().get_dark()
    };
    os(ColorScheme::Dark);
    assert!(dark(ThemeChoice::System), "System on a dark OS");
    os(ColorScheme::Light);
    assert!(!dark(ThemeChoice::System), "the OS switched while open");
    assert!(dark(ThemeChoice::Dark), "a choice beats a light OS");
    os(ColorScheme::Dark);
    assert!(!dark(ThemeChoice::Light), "a choice beats a dark OS");
    assert!(dark(ThemeChoice::System), "back to System after a choice");
}

/// Win32 for the tests on the real window.
#[cfg(windows)]
#[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)] // window pixels
mod win32 {
    use std::ffi::c_void;

    pub type Hwnd = *mut c_void;

    const SRCCOPY: u32 = 0x00CC_0020;
    const BLACKNESS: u32 = 0x0000_0042;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn EnumThreadWindows(
            thread: u32,
            callback: unsafe extern "system" fn(Hwnd, isize) -> i32,
            data: isize,
        ) -> i32;
        fn IsWindowVisible(hwnd: Hwnd) -> i32;
        fn GetDC(hwnd: Hwnd) -> Hwnd;
        fn ReleaseDC(hwnd: Hwnd, dc: Hwnd) -> i32;
        pub fn GetWindowLongW(hwnd: Hwnd, index: i32) -> i32;
        pub fn GetClientRect(hwnd: Hwnd, rect: *mut [i32; 4]) -> i32;
        pub fn GetWindowRect(hwnd: Hwnd, rect: *mut [i32; 4]) -> i32;
        pub fn SendMessageW(hwnd: Hwnd, message: u32, w: usize, l: isize) -> isize;
        pub fn InvalidateRect(hwnd: Hwnd, rect: *const c_void, erase: i32) -> i32;
    }
    #[link(name = "gdi32")]
    unsafe extern "system" {
        fn CreateCompatibleDC(dc: Hwnd) -> Hwnd;
        fn DeleteDC(dc: Hwnd) -> i32;
        fn DeleteObject(object: Hwnd) -> i32;
        fn SelectObject(dc: Hwnd, object: Hwnd) -> Hwnd;
        #[allow(clippy::too_many_arguments)] // the shape of the Win32 call
        fn BitBlt(
            to: Hwnd,
            x: i32,
            y: i32,
            w: i32,
            h: i32,
            from: Hwnd,
            from_x: i32,
            from_y: i32,
            op: u32,
        ) -> i32;
        fn PatBlt(dc: Hwnd, x: i32, y: i32, w: i32, h: i32, op: u32) -> i32;
        fn CreateDIBSection(
            dc: Hwnd,
            info: *const [u32; 11],
            usage: u32,
            bits: *mut *mut u32,
            section: Hwnd,
            offset: u32,
        ) -> Hwnd;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetWindowsHookExW(
            id: i32,
            hook: unsafe extern "system" fn(i32, usize, isize) -> isize,
            module: Hwnd,
            thread: u32,
        ) -> Hwnd;
        fn CallNextHookEx(hook: Hwnd, code: i32, w: usize, l: isize) -> isize;
        fn SetWindowLongW(hwnd: Hwnd, index: i32, value: i32) -> i32;
        fn SetWindowPos(hwnd: Hwnd, after: Hwnd, x: i32, y: i32, w: i32, h: i32, flags: u32)
        -> i32;
    }

    /// `CWPSTRUCT`.
    #[repr(C)]
    struct Sent {
        l: isize,
        w: usize,
        message: u32,
        hwnd: Hwnd,
    }

    unsafe extern "system" fn behind(code: i32, w: usize, l: isize) -> isize {
        const WM_SHOWWINDOW: u32 = 0x0018;
        const GWL_EXSTYLE: i32 = -20;
        const WS_EX_NOACTIVATE: i32 = 0x0800_0000;
        const WS_EX_TOOLWINDOW: i32 = 0x0080;
        const HWND_BOTTOM: isize = 1;
        const SWP_NOSIZE_NOMOVE_NOACTIVATE: u32 = 0x13;
        // SAFETY: for `HC_ACTION` (0) `l` points at the live `CWPSTRUCT` of the system; the calls
        // take the window the message is for.
        unsafe {
            let sent = &*(l as *const Sent);
            if code == 0 && sent.message == WM_SHOWWINDOW && sent.w != 0 {
                let style = GetWindowLongW(sent.hwnd, GWL_EXSTYLE);
                SetWindowLongW(
                    sent.hwnd,
                    GWL_EXSTYLE,
                    style | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                );
                let after = HWND_BOTTOM as Hwnd;
                SetWindowPos(sent.hwnd, after, 0, 0, 0, 0, SWP_NOSIZE_NOMOVE_NOACTIVATE);
            }
            CallNextHookEx(std::ptr::null_mut(), code, w, l)
        }
    }

    /// A window this thread shows from now on stays behind the others and takes no focus.
    pub fn open_behind() {
        const WH_CALLWNDPROC: i32 = 4;
        // SAFETY: `behind` matches HOOKPROC and lives for the whole process; a thread hook needs
        // no module handle.
        unsafe {
            SetWindowsHookExW(
                WH_CALLWNDPROC,
                behind,
                std::ptr::null_mut(),
                GetCurrentThreadId(),
            );
        }
    }

    unsafe extern "system" fn first_visible(hwnd: Hwnd, data: isize) -> i32 {
        // SAFETY: `data` is the local of `this_window` below, alive during the enumeration.
        unsafe {
            if IsWindowVisible(hwnd) == 0 {
                return 1;
            }
            *(data as *mut Hwnd) = hwnd;
        }
        0
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetIconInfo(icon: Hwnd, info: *mut IconInfo) -> i32;
        fn GetWindowTextW(hwnd: Hwnd, text: *mut u16, max: i32) -> i32;
    }
    #[link(name = "gdi32")]
    unsafe extern "system" {
        fn GetObjectW(object: Hwnd, size: i32, out: *mut [i32; 6]) -> i32;
    }

    /// `ICONINFO`.
    #[repr(C)]
    struct IconInfo {
        is_icon: i32,
        x: u32,
        y: u32,
        mask: Hwnd,
        color: Hwnd,
    }

    /// The window's title.
    pub fn title(hwnd: Hwnd) -> String {
        let mut text = [0u16; 128];
        // SAFETY: `text` has the room the call is told.
        let n = unsafe { GetWindowTextW(hwnd, text.as_mut_ptr(), 128) };
        String::from_utf16_lossy(&text[..n as usize])
    }

    /// The width in pixels of the window's icon of type `which` (`ICON_SMALL`, `ICON_BIG`), 0 if
    /// it has none.
    pub fn icon_width(hwnd: Hwnd, which: usize) -> i32 {
        const WM_GETICON: u32 = 0x007F;
        // SAFETY: `hwnd` is a live window of this thread; `info` has the layout of ICONINFO and
        // `bitmap` the room of a BITMAP (its width is the second LONG); both bitmaps are freed here.
        unsafe {
            let icon = SendMessageW(hwnd, WM_GETICON, which, 0) as Hwnd;
            if icon.is_null() {
                return 0;
            }
            let mut info: IconInfo = std::mem::zeroed();
            GetIconInfo(icon, &raw mut info);
            let mut bitmap = [0i32; 6];
            GetObjectW(info.color, 32, &raw mut bitmap);
            DeleteObject(info.mask);
            DeleteObject(info.color);
            bitmap[1]
        }
    }

    /// This thread's visible window, null if it has none.
    pub fn this_window() -> Hwnd {
        let mut hwnd: Hwnd = std::ptr::null_mut();
        // SAFETY: the callback matches WNDENUMPROC and only writes `hwnd`.
        unsafe { EnumThreadWindows(GetCurrentThreadId(), first_visible, &raw mut hwnd as isize) };
        hwnd
    }

    fn client_size(hwnd: Hwnd) -> (i32, i32) {
        let mut rect = [0; 4];
        // SAFETY: `rect` has the layout of RECT.
        unsafe { GetClientRect(hwnd, &raw mut rect) };
        (rect[2], rect[3])
    }

    /// The client area as the OS holds it, BGRA in rows from the top.
    pub fn client_pixels(hwnd: Hwnd) -> Vec<u32> {
        let (w, h) = client_size(hwnd);
        // BITMAPINFOHEADER: top-down, 32 bits per pixel, no compression.
        let mut info = [0u32; 11];
        (info[0], info[1], info[2], info[3]) = (40, w as u32, (-h) as u32, 1 | (32 << 16));
        let mut bits: *mut u32 = std::ptr::null_mut();
        // SAFETY: the handles are created and freed here; `bits` covers `w * h` pixels until
        // `DeleteObject`, and the copy ends before it.
        unsafe {
            let window = GetDC(hwnd);
            let memory = CreateCompatibleDC(window);
            let null = std::ptr::null_mut();
            let bitmap = CreateDIBSection(memory, &raw const info, 0, &raw mut bits, null, 0);
            let old = SelectObject(memory, bitmap);
            BitBlt(memory, 0, 0, w, h, window, 0, 0, SRCCOPY);
            let pixels = std::slice::from_raw_parts(bits, (w * h) as usize).to_vec();
            SelectObject(memory, old);
            DeleteObject(bitmap);
            DeleteDC(memory);
            ReleaseDC(hwnd, window);
            pixels
        }
    }

    /// Blanks what the OS holds for the client area, as a lost frame does.
    pub fn wipe(hwnd: Hwnd) {
        let (w, h) = client_size(hwnd);
        // SAFETY: the DC is released right after the fill.
        unsafe {
            let dc = GetDC(hwnd);
            PatBlt(dc, 0, 0, w, h, BLACKNESS);
            ReleaseDC(hwnd, dc);
        }
    }
}

/// The std widgets (the Combo and its list) set no size: they read the window's, which is body size.
#[test]
fn window_default_font_size_is_body_size() {
    let l = layout("font-size", None);
    let window = open_default(&l);
    let body = window.global::<Tokens>().get_body_size();
    assert!(body > 0.0);
    assert!((window.get_text_size() - body).abs() < 0.01);
}
