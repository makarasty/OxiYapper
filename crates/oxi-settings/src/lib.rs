//! Slint settings window and first-run wizard (ARCHITECTURE section 4).

pub mod constants;
mod ctx;
mod debounce;
pub mod ipc;
mod pages;
#[cfg(test)]
mod testing;

use std::io;

use oxi_core::config::{ConfigError, Theme};
use oxi_core::ipc::{Push, SettingsPage, SettingsTab, Target};
#[cfg(windows)]
use oxi_core::overlay::Rect;
use oxi_core::paths::Layout;
use oxi_core::strings::CONFIG_READ_ONLY_NEXT;
use slint::{ComponentHandle, Timer, TimerMode, Weak};
#[cfg(windows)]
use slint::{LogicalSize, PhysicalPosition};

pub use ctx::Ctx;
pub use ipc::Ipc;
use ipc::{Action, Event, Level, Link, Options};

// Slint's generated code trips the workspace `unused_qualifications` lint; it is not ours to fix.
#[allow(unused_qualifications)]
mod generated {
    slint::include_modules!();
}
pub use constants::FILE_POLL;
pub use generated::{
    AppWindow, AudioState, ChipLevel, ModelState, Page, PathText, Shell, SystemState, TestRow,
    TestState, TextState, Theme as Tokens, ThemeChoice, Toast,
};
pub use pages::test::{Finish, Mic, bind_with as bind_test_page};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Ui(#[from] slint::PlatformError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Config(#[from] ConfigError),
}

/// Show the settings window and block until it is closed.
pub fn run() -> Result<(), Error> {
    let layout = oxi_platform::dirs::layout()?;
    oxi_platform::dirs::ensure(&layout)?;
    let skia = use_skia();
    match show(layout) {
        // Skia is built into this process, but its surface can fail at the first show (no usable
        // display driver); the platform cannot be re-selected after that, so run a fresh process.
        Err(Error::Ui(e)) if skia => {
            eprintln!("skia renderer failed ({e}); retrying with the software renderer");
            rerun_with_software(e)
        }
        result => result,
    }
}

fn show(layout: Layout) -> Result<(), Error> {
    // The window comes first: it sets up the Slint platform, and the worker's events need that
    // to reach the UI thread.
    let window = open_inner(layout, |window| {
        let mut options = Options::daemon()?;
        options.spawn = watch_spawn(options.spawn, window.as_weak());
        Ok(Some(Ipc::connect(options)?))
    })?;
    #[cfg(windows)]
    {
        let size = if let Ok((area, scale)) = oxi_platform::os::display::work_area() {
            let (size, position) = placement(area, scale);
            window.window().set_position(position);
            size
        } else {
            LogicalSize::new(constants::DEFAULT.0, constants::DEFAULT.1)
        };
        window.window().set_size(size);
    }
    // A window opened by a second launch must not show behind the app the user is in; the
    // daemon allowed this process to take the foreground. The callback runs once the event loop
    // has created and shown the window.
    #[cfg(windows)]
    let _ = slint::invoke_from_event_loop({
        let weak = window.as_weak();
        move || {
            // The backend wrote the system accent when it created the window.
            if let Some(window) = weak.upgrade() {
                own_accent(&window);
                caption_theme(&window, window.get_theme());
            }
            oxi_platform::os::foreground::raise_current_thread_window();
        }
    });
    window.run()?;
    Ok(())
}

/// Env var that forces the software renderer; also how a failed Skia run restarts itself.
#[cfg(windows)]
const RENDERER_ENV: &str = "OXI_RENDERER";

/// Slint's renderer name for the env var's value: Skia's CPU surface unless it says `software`.
#[cfg(windows)]
fn renderer_name(env: Option<&str>) -> &'static str {
    if env == Some("software") {
        "software"
    } else {
        "skia-software"
    }
}

/// Picks Skia's CPU raster surface and says whether it did. Its text is the same as on Skia's GPU
/// surface (real-window captures), with gamma and hinting the software renderer lacks, and it needs
/// no GPU, so it also runs over RDP; measured with the window open, 69 MB private bytes against
/// 272 MB on D3D. `OXI_RENDERER=software`, or a Skia that cannot be selected, picks the software
/// renderer: never Slint's default, which would be Skia on D3D.
pub fn use_skia() -> bool {
    // Skia is built on Windows only; elsewhere Slint's only renderer is the software one.
    #[cfg(windows)]
    {
        let name = renderer_name(std::env::var(RENDERER_ENV).ok().as_deref());
        let select = |name: &str| {
            slint::BackendSelector::new()
                .renderer_name(name.into())
                .select()
        };
        if select(name).is_ok() {
            return name == "skia-software";
        }
        let _ = select("software");
    }
    false
}

/// Runs this program again with the software renderer and waits for it; `e` is what to report if
/// that cannot start or fails too.
fn rerun_with_software(e: slint::PlatformError) -> Result<(), Error> {
    #[cfg(windows)]
    {
        let status = std::process::Command::new(std::env::current_exe()?)
            .args(std::env::args_os().skip(1))
            .env(RENDERER_ENV, "software")
            .status()?;
        if status.success() {
            return Ok(());
        }
    }
    Err(Error::Ui(e))
}

/// `spawn` that also shows why it failed (the error kind only) in the window.
fn watch_spawn(
    spawn: Box<dyn Fn() -> io::Result<()> + Send>,
    window: Weak<AppWindow>,
) -> Box<dyn Fn() -> io::Result<()> + Send> {
    Box::new(move || {
        let result = spawn();
        if let Err(e) = &result {
            let note = spawn_note(e);
            // Err means the event loop is gone: the window is closing.
            let _ = window.upgrade_in_event_loop(move |window| window.set_spawn_error(note.into()));
        }
        result
    })
}

fn spawn_note(e: &io::Error) -> String {
    oxi_core::strings::spawn_failed(e.kind())
}

/// Logical size and physical top-left of a new window centred in `area` (physical pixels, at
/// `scale`).
#[cfg(windows)]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)] // screen pixels
fn placement(area: Rect, scale: f32) -> (LogicalSize, PhysicalPosition) {
    let (w, h) = (area.w as f32 / scale, area.h as f32 / scale);
    let size = LogicalSize::new(
        constants::DEFAULT.0.min(w * 0.85).max(constants::MIN.0),
        constants::DEFAULT.1.min(h * 0.88).max(constants::MIN.1),
    );
    let position = PhysicalPosition::new(
        area.x + ((w - size.width) * scale / 2.0).round() as i32,
        area.y + ((h - size.height) * scale / 2.0).round() as i32,
    );
    (size, position)
}

/// Puts our accent token (the theme's ink, DS-19.1) where Slint keeps the system accent, which the
/// std widgets still in use tint from; never the Windows accent (redesign spec 7.3).
// shortcut: private Slint API, safe only while `slint` is pinned `=1.18.1`; re-check it on every
// Slint bump. The backend writes the system accent back when it creates the window and on a theme
// change: `run` calls this once the window is up and the file poll repeats it (a no-op write when
// nothing changed), so a new Windows accent can show for up to a second.
pub fn own_accent(window: &AppWindow) {
    slint::private_unstable_api::re_exports::WindowInner::from_pub(window.window())
        .context()
        .set_accent_color(window.global::<Tokens>().get_accent());
}

/// The title bar follows an explicit Light or Dark pick: Slint sets only its own palette, and on
/// Windows the OS keeps drawing the caption in the system theme. A no-op before the window exists
/// and under the testing backend.
// shortcut: winit's window behind the pinned `slint =1.18.1` (`unstable-winit-030`); re-check it on
// every Slint bump. A DWM call behind `oxi-platform` would not depend on it.
pub fn caption_theme(window: &AppWindow, choice: ThemeChoice) {
    use slint::winit_030::WinitWindowAccessor as _;
    use slint::winit_030::winit::window::Theme as Caption;
    let theme = match choice {
        ThemeChoice::System => None,
        ThemeChoice::Dark => Some(Caption::Dark),
        ThemeChoice::Light => Some(Caption::Light),
    };
    window.window().with_winit_window(|w| w.set_theme(theme));
}

/// Makes every frame repaint and present the whole window. Skia's CPU surface otherwise presents
/// only what changed and treats a paint message as nothing to do, so pixels the OS dropped never
/// come back: the window is created see-through (winit's blur-behind), the desktop shows where
/// they were and only hovered rows are drawn again (`os_lost_pixels_are_repainted`).
// shortcut: Slint turns partial rendering off for a window with a rendering notifier, and offers no
// direct switch; re-check it on every Slint bump. The software renderer has no notifier (`Err`) and
// keeps partial rendering; it runs only when Skia cannot start.
fn draw_whole_frames(window: &AppWindow) {
    let _ = window.window().set_rendering_notifier(|_, _| {});
}

/// Follows the OS "Animation effects" setting (UI rule 22): off zeroes `motion-quick` and
/// `motion-open`.
fn follow_os_motion(window: &AppWindow) {
    set_reduced_motion(window, oxi_platform::reduced_motion());
}

fn set_reduced_motion(window: &AppWindow, reduced: bool) {
    window.global::<Shell>().set_reduced_motion(reduced);
}

/// Every `PathField` shows a long path elided in the middle (DS-13.1), not cut at its end.
fn elide_paths(window: &AppWindow) {
    window.global::<PathText>().on_elide(|path, budget| {
        oxi_core::paths::elide_path(&path, usize::try_from(budget).unwrap_or(0)).into()
    });
}

/// The settings window on `layout`'s config with no daemon link (it stays offline and shows no
/// daemon banner); the caller shows it.
pub fn open(layout: Layout) -> Result<AppWindow, Error> {
    open_inner(layout, |_| Ok(None))
}

/// [`open`] with the link to the daemon: its state and notices show in the daemon banner.
pub fn open_with(layout: Layout, ipc: Ipc) -> Result<AppWindow, Error> {
    open_inner(layout, |_| Ok(Some(ipc)))
}

/// `link` gets the new window (so the platform is up) and returns the daemon link to follow, if any.
fn open_inner(
    layout: Layout,
    link: impl FnOnce(&AppWindow) -> Result<Option<Ipc>, Error>,
) -> Result<AppWindow, Error> {
    let window = AppWindow::new()?;
    own_accent(&window);
    draw_whole_frames(&window);
    follow_os_motion(&window);
    elide_paths(&window);
    let (mut ctx, problem) = Ctx::load(layout)?;
    let followed = link(&window)?;
    if let Some(ipc) = &followed {
        ctx.ipc = ipc.clone();
    }
    show_problem(&window, problem.as_ref());

    let weak = window.as_weak();
    let c = ctx.clone();
    window.on_theme_changed(move |choice| {
        if let Some(window) = weak.upgrade() {
            caption_theme(&window, choice);
        }
        // The token follows the new scheme once the window has applied it.
        let w = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = w.upgrade() {
                own_accent(&window);
            }
        });
        c.config.borrow_mut().config.ui.theme = theme(choice);
        report(&weak, c.save());
    });

    let weak = window.as_weak();
    let path = ctx.layout.config_file();
    window.on_open_file(move || {
        report(&weak, oxi_platform::open::open(&path).map_err(Error::from));
    });

    let weak = window.as_weak();
    let c = ctx.clone();
    let linked = followed.is_some();
    window.on_reset_config(move || {
        let result = c.reset();
        if let (Ok(()), Some(window)) = (&result, weak.upgrade()) {
            window.set_banner_text("".into());
            // The file is valid now: drop the daemon's stale `ConfigError` notice, as `reload` does.
            if linked {
                on_daemon_event(&window, &c.ipc, &Event::Link(c.ipc.link()));
            }
            bind(&window, &c);
        }
        report(&weak, result);
    });

    let banner = followed.is_some().then(|| {
        let (weak, ipc) = (window.as_weak(), ctx.ipc.clone());
        let events = ctx.ipc.subscribe(move |event| {
            if let Some(window) = weak.upgrade() {
                on_daemon_event(&window, &ipc, event);
            }
        });
        // The worker may have reported before the subscription: show the state as it is now.
        on_daemon_event(&window, &ctx.ipc, &Event::Link(ctx.ipc.link()));
        events
    });
    let poll = Timer::default();
    poll.start(TimerMode::Repeated, FILE_POLL, {
        let (weak, c) = (window.as_weak(), ctx.clone());
        move || {
            if let Some(window) = weak.upgrade() {
                own_accent(&window);
                // Puts back a frame the OS lost with no paint message (see `draw_whole_frames`).
                window.window().request_redraw();
                follow_os_motion(&window);
                reload(&window, &c, linked);
                pages::poll(&window, &c);
            }
        }
    });
    let ipc = ctx.ipc.clone();
    window.on_start_daemon({
        let weak = window.as_weak();
        move || {
            // Held by this callback, which lives as long as the window.
            let _ = (&banner, &poll);
            if let Some(window) = weak.upgrade() {
                window.set_spawn_error("".into());
            }
            ipc.start_daemon();
        }
    });
    let ipc = ctx.ipc.clone();
    window.on_restart_daemon({
        let weak = window.as_weak();
        move || {
            if let Some(window) = weak.upgrade() {
                window.set_spawn_error("".into());
            }
            ipc.restart_daemon();
        }
    });
    window.on_status_clicked({
        let (weak, ipc) = (window.as_weak(), ctx.ipc.clone());
        move || {
            if let Some(window) = weak.upgrade() {
                status_clicked(&window, &ipc.chip());
            }
        }
    });

    pages::test::bind(&window, &ctx);
    bind(&window, &ctx);
    Ok(window)
}

/// The config banner for `problem`; `None` clears it.
fn show_problem(window: &AppWindow, problem: Option<&ConfigError>) {
    let text = match problem {
        Some(ConfigError::ReadOnly) => {
            format!("{}. {CONFIG_READ_ONLY_NEXT}", ConfigError::ReadOnly)
        }
        Some(other) => other.to_string(),
        None => String::new(),
    };
    window.set_banner_text(text.into());
    // Both get "Open file"; only a broken file gets "Reset to defaults", the newer one stays safe.
    window.set_banner_fixable(!matches!(problem, Some(ConfigError::ReadOnly)));
}

/// Applies an edit of the file from outside: the banner, the lock and every page follow it. A
/// valid file also drops what the last failure left: the save error and the daemon's
/// `ConfigError` notice (the daemon sends nothing when a reload succeeds).
fn reload(window: &AppWindow, ctx: &Ctx, linked: bool) {
    let Some(problem) = ctx.reload_if_changed() else {
        return;
    };
    show_problem(window, problem.as_ref());
    if problem.is_none() {
        window.set_action_error("".into());
        if linked {
            on_daemon_event(window, &ctx.ipc, &Event::Link(ctx.ipc.link()));
        }
    }
    bind(window, ctx);
}

/// The status chip follows every event; the daemon banner follows the link, and a push that is
/// not a notice leaves it alone.
fn on_daemon_event(window: &AppWindow, ipc: &Ipc, event: &Event) {
    show_chip(window, &ipc.chip());
    if matches!(event, Event::Push(Push::Raise)) {
        window.window().set_minimized(false);
        let _ = window.show();
        // `show` leaves a window that is already visible behind other apps; the daemon has
        // allowed this process to take the foreground.
        #[cfg(windows)]
        oxi_platform::os::foreground::raise_current_thread_window();
    }
    let notice = ipc::notice(event);
    if notice.is_none() && !matches!(event, Event::Link(_)) {
        return;
    }
    let action = notice.as_ref().and_then(|n| n.action);
    window.set_daemon_config_error(matches!(event, Event::Push(Push::ConfigError { .. })));
    window.set_daemon_text(notice.map(|n| n.text).unwrap_or_default().into());
    window.set_daemon_start(action == Some(Action::Start));
    window.set_daemon_restart(action == Some(Action::Restart));
    // Connecting and Offline keep it: the failure is reported just before them.
    if matches!(event, Event::Link(link) if !matches!(link, Link::Connecting | Link::Offline)) {
        window.set_spawn_error("".into());
    }
}

fn show_chip(window: &AppWindow, chip: &ipc::Chip) {
    window.set_chip_text(chip.text.into());
    window.set_chip_tooltip(chip.tooltip.as_str().into());
    window.set_chip_level(match chip.level {
        Level::Ok => ChipLevel::Ok,
        Level::Attention => ChipLevel::Attention,
        Level::Down => ChipLevel::Down,
    });
}

/// A click on the chip: open the setting that fixes it, or start the daemon as the banner's
/// button does. The normal state takes no click.
fn status_clicked(window: &AppWindow, chip: &ipc::Chip) {
    match (chip.level, chip.target) {
        (Level::Attention, Some(target)) => open_target(window, target),
        (Level::Down, _) if window.get_daemon_start() => window.invoke_start_daemon(),
        (Level::Down, _) if window.get_daemon_restart() => window.invoke_restart_daemon(),
        _ => {}
    }
}

/// Opens the page and sub-tab that fix the chip's status; no tab means the page's first one, not
/// the sub-tab the user left it on.
fn open_target(window: &AppWindow, target: Target) {
    // The Audio page's tabs, in `SettingsTab` order.
    let view = target.tab.map_or(0, |tab| match tab {
        SettingsTab::Microphone => 0,
        SettingsTab::Recording => 1,
        SettingsTab::StartSound => 2,
        SettingsTab::StopSound => 3,
    });
    match target.page {
        SettingsPage::Model => {
            window.set_page(Page::Model);
            window.global::<ModelState>().set_tab(0);
        }
        SettingsPage::Audio => {
            window.set_page(Page::Audio);
            window.global::<AudioState>().set_view(view);
        }
    }
}

/// Push the config into the shell and every page.
fn bind(window: &AppWindow, ctx: &Ctx) {
    let file = ctx.config.borrow();
    window.set_theme(choice(file.config.ui.theme));
    caption_theme(window, choice(file.config.ui.theme));
    window.global::<Shell>().set_locked(file.read_only);
    drop(file);
    pages::bind(window, ctx);
    // Bound once in `open_inner`: its recording and run outlive a reload.
    window.global::<TestState>().invoke_shown();
}

/// Outcome of a save or file action: a failure shows under the config banner, which stays with
/// its buttons; a success clears the failure shown before.
pub(crate) fn report(window: &Weak<AppWindow>, result: Result<(), Error>) {
    if let Some(window) = window.upgrade() {
        let text = result.err().map(|e| e.to_string()).unwrap_or_default();
        window.set_action_error(text.into());
    }
}

const fn choice(theme: Theme) -> ThemeChoice {
    match theme {
        Theme::System => ThemeChoice::System,
        Theme::Dark => ThemeChoice::Dark,
        Theme::Light => ThemeChoice::Light,
    }
}

const fn theme(choice: ThemeChoice) -> Theme {
    match choice {
        ThemeChoice::System => Theme::System,
        ThemeChoice::Dark => Theme::Dark,
        ThemeChoice::Light => Theme::Light,
    }
}

#[cfg(test)]
mod tests {
    use super::{AppWindow, ThemeChoice, choice, spawn_note, theme};
    use oxi_core::config::Theme;
    use slint::ComponentHandle;
    use std::io::{Error, ErrorKind};
    #[cfg(windows)]
    use {super::placement, oxi_core::overlay::Rect};

    #[test]
    fn window_builds_under_the_testing_backend() {
        i_slint_backend_testing::init_no_event_loop();
        let window = AppWindow::new().expect("window builds");
        assert!(window.show().is_ok());
    }

    #[test]
    fn window_takes_reduced_motion_from_the_os() {
        let (window, _ipc, _sent, _) = crate::testing::linked("lib", "motion", super::Page::Home);
        let (shell, tokens) = (
            window.global::<super::Shell>(),
            window.global::<super::Tokens>(),
        );
        // The testing backend has no OS setting: `open` read the platform's value.
        assert_eq!(shell.get_reduced_motion(), oxi_platform::reduced_motion());
        super::set_reduced_motion(&window, false);
        assert_eq!(tokens.get_motion_quick(), 100);
        super::set_reduced_motion(&window, true);
        assert!(shell.get_reduced_motion());
        assert_eq!(tokens.get_motion_quick(), 0);
        assert_eq!(tokens.get_motion_open(), 0);
    }

    #[test]
    fn spawn_note_gives_the_kind_and_nothing_else() {
        let e = Error::new(ErrorKind::NotFound, r"C:\Users\someone\oxi.exe");
        assert_eq!(
            spawn_note(&e),
            "Could not start OxiYapper: entity not found."
        );
    }

    #[test]
    fn resets_leave_one_subscriber_per_page() {
        let (window, ipc, _sent, _) = crate::testing::linked("lib", "resets", super::Page::Hotkey);
        // The banner, Home, Hotkey, Audio, Model, Overlay, History and Test.
        assert_eq!(ipc.subscribers(), 8);
        for _ in 0..3 {
            window.invoke_reset_config();
        }
        assert_eq!(ipc.subscribers(), 8);
    }

    #[cfg(windows)]
    #[test]
    fn placement_fits_the_work_area_and_centres() {
        let rect = |x, y, w, h| Rect { x, y, w, h };
        // (work area, scale, logical size, physical top-left)
        let cases = [
            // 1920x1080 at 100 %, taskbar 48: the full default size.
            (rect(0, 0, 1920, 1032), 1.0, (1280.0, 800.0), (320, 116)),
            // 1366x768 laptop: 85 % of 1366 is 1161, 88 % of 720 is 633.6.
            (rect(0, 0, 1366, 720), 1.0, (1161.1, 633.6), (102, 43)),
            // 2560x1440 at 150 %: 1706x928 logical, the default size again.
            (rect(0, 0, 2560, 1392), 1.5, (1280.0, 800.0), (320, 96)),
            // A tiny screen still gets the minimum; a monitor left of the primary keeps its origin.
            (rect(-800, 0, 800, 600), 1.0, (720.0, 528.0), (-760, 36)),
        ];
        for (area, scale, (w, h), (x, y)) in cases {
            let (size, pos) = placement(area, scale);
            assert!(
                (size.width - w).abs() < 0.01 && (size.height - h).abs() < 0.01,
                "{area:?}: {size:?}"
            );
            assert_eq!((pos.x, pos.y), (x, y), "{area:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn the_env_var_picks_the_software_renderer() {
        use super::renderer_name;
        assert_eq!(renderer_name(Some("software")), "software");
        assert_eq!(renderer_name(None), "skia-software");
        assert_eq!(renderer_name(Some("skia")), "skia-software");
    }

    /// Each status t12 reports shows in the chip with its level, and a click opens the page and
    /// tab that fix it (item 33).
    #[test]
    fn chip_shows_each_status_and_opens_its_fix() {
        use super::{AudioState, ChipLevel, Event, ModelState, Page, Push};
        use crate::ipc::{Cmd, Link};
        use oxi_core::ipc::Attention;
        use oxi_core::strings;
        let (window, ipc, sent, _) = crate::testing::linked("lib", "chip", Page::Home);
        let chip = |w: &AppWindow| (w.get_chip_text().to_string(), w.get_chip_level());
        let attention = |items: Vec<Attention>| Event::Push(Push::Attention { items });
        ipc.deliver(&Event::Link(Link::Online));
        assert_eq!(
            chip(&window),
            (strings::HEALTH_RUNNING.into(), ChipLevel::Ok)
        );
        window.invoke_status_clicked();
        assert_eq!(
            window.get_page(),
            Page::Home,
            "the normal state takes no click"
        );
        let sound = |stop| Attention::SoundFailed {
            line: "x".into(),
            stop,
        };
        for (item, page, view) in [
            (Attention::NoModel, Page::Model, None),
            (Attention::ModelDamaged, Page::Model, None),
            (
                Attention::CpuFallback { reason: "x".into() },
                Page::Model,
                None,
            ),
            (Attention::OutOfMemory { cpu: true }, Page::Model, None),
            (Attention::OutOfMemory { cpu: false }, Page::Model, None),
            (Attention::NoMicrophone, Page::Audio, Some(0)),
            (sound(false), Page::Audio, Some(2)),
            (sound(true), Page::Audio, Some(3)),
        ] {
            // Both pages left on another sub-tab: the click opens the one that fixes it.
            window.set_page(Page::Home);
            window.global::<AudioState>().set_view(1);
            window.global::<ModelState>().set_tab(1);
            ipc.deliver(&attention(vec![item.clone()]));
            assert_eq!(
                chip(&window),
                (strings::attention_chip(&item).into(), ChipLevel::Attention),
                "{item:?}"
            );
            assert_eq!(
                window.get_chip_tooltip().as_str(),
                strings::attention_text(&item),
                "{item:?}: the reason and the fix on hover"
            );
            window.invoke_status_clicked();
            assert_eq!(window.get_page(), page, "{item:?}");
            match view {
                Some(view) => {
                    assert_eq!(window.global::<AudioState>().get_view(), view, "{item:?}");
                }
                None => assert_eq!(
                    window.global::<ModelState>().get_tab(),
                    0,
                    "{item:?}: the Models list"
                ),
            }
        }
        ipc.deliver(&attention(vec![]));
        assert_eq!(chip(&window).1, ChipLevel::Ok, "all well again");
        ipc.deliver(&Event::Link(Link::Offline));
        assert_eq!(
            chip(&window),
            (strings::HEALTH_NOT_RUNNING.into(), ChipLevel::Down)
        );
        while sent.try_recv().is_ok() {}
        window.invoke_status_clicked();
        assert!(
            sent.try_iter().any(|c| matches!(c, Cmd::Start)),
            "a click starts the daemon"
        );
        ipc.deliver(&Event::Link(Link::Stuck));
        assert_eq!(
            chip(&window),
            (strings::HEALTH_NOT_ANSWERING.into(), ChipLevel::Down)
        );
    }

    #[test]
    fn theme_maps_both_ways() {
        for t in [Theme::System, Theme::Dark, Theme::Light] {
            assert_eq!(theme(choice(t)), t);
        }
        assert_eq!(choice(Theme::Dark), ThemeChoice::Dark);
    }
}
