//! Settings shell under the testing backend, on a temporary config directory.

mod common;

use std::fs;

use common::{has, layout, one, open, open_default, open_linked, with_role};
use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery};
use oxi_core::config::{ConfigFile, Theme};
use oxi_core::ipc::Push;
use oxi_settings::ipc::{Cmd, Event, Link};
use oxi_settings::{AppWindow, Ipc, Page, Shell, ThemeChoice};
use slint::ComponentHandle;
use slint::platform::{Key, PointerEventButton, WindowEvent};

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
    assert_eq!(window.get_page(), Page::Hotkey);
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
    ] {
        assert!(has(&window, text), "About lacks {text}");
    }
}

#[test]
fn theme_pick_writes_ui_theme() {
    let l = layout("theme", Some("version = 1\n# mine\n"));
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
    let original = "version = 1\n[audio]\ngain_db = \n";
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
fn thumb_picker_takes_a_click_then_arrow_keys() {
    let l = layout("keys", None);
    let window = open(&l, Page::Hotkey);
    window.set_page(Page::System);
    card(&window, "Light").mock_single_click(PointerEventButton::Left);
    assert_eq!(window.get_theme(), ThemeChoice::Light);
    let key = |key: Key| {
        window
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
        window
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
    };
    key(Key::LeftArrow);
    assert_eq!(window.get_theme(), ThemeChoice::Dark);
    assert_eq!(card(&window, "Dark").accessible_checked(), Some(true));
    key(Key::RightArrow);
    key(Key::RightArrow);
    assert_eq!(
        window.get_theme(),
        ThemeChoice::Light,
        "stops at the last card"
    );
    let text = fs::read_to_string(l.config_file()).unwrap();
    assert_eq!(
        ConfigFile::parse(&text).unwrap().config.ui.theme,
        Theme::Light
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
    let original = "version = 1
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
            "version = 1
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
    assert_eq!(banners(&window), ["config.toml, line 7: bad value"]);
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
    let l = layout("fixed-outside", Some("version = 1\n[audio]\ngain_db = \n"));
    let window = open(&l, Page::Hotkey);
    assert!(window.get_banner_text().contains("line 3"));
    assert!(window.global::<Shell>().get_locked());

    fs::write(l.config_file(), "version = 1\n[ui]\ntheme = \"dark\"\n").unwrap();
    poll();
    assert_eq!(window.get_banner_text(), "");
    assert!(!window.global::<Shell>().get_locked());
    assert_eq!(window.get_theme(), ThemeChoice::Dark);
    assert!(!has(&window, "Reset to defaults"));

    // Broken again: the banner comes back, the hand edit stays on disk.
    let broken = "version = 1\n[ui]\ntheme = \n";
    fs::write(l.config_file(), broken).unwrap();
    poll();
    assert!(window.get_banner_text().contains("line 3"));
    assert!(window.global::<Shell>().get_locked());
    assert_eq!(fs::read_to_string(l.config_file()).unwrap(), broken);
}

#[test]
fn a_file_fixed_outside_drops_the_daemons_config_error() {
    let l = layout("fixed-linked", Some("version = 1\n[audio]\ngain_db = \n"));
    let (ipc, _rx) = Ipc::fake();
    let window = open_linked(&l, Page::Hotkey, ipc.clone());
    ipc.deliver(&Event::Push(Push::ConfigError {
        file: "config.toml".into(),
        line: 3,
        message: "bad value".into(),
    }));
    assert_eq!(
        banners(&window).len(),
        2,
        "the file's banner and the daemon's"
    );

    fs::write(l.config_file(), "version = 1\n").unwrap();
    poll();
    assert_eq!(banners(&window), Vec::<String>::new());
}
