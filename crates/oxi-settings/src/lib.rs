//! Slint settings window and first-run wizard (ARCHITECTURE section 4).

mod ctx;
mod debounce;
pub mod ipc;
mod pages;
#[cfg(test)]
mod testing;

use std::io;
use std::time::Duration;

use oxi_core::config::{ConfigError, Theme};
use oxi_core::ipc::Push;
#[cfg(windows)]
use oxi_core::overlay::Rect;
use oxi_core::paths::Layout;
use oxi_core::strings::CONFIG_READ_ONLY_NEXT;
use slint::{Color, ComponentHandle, Timer, TimerMode, Weak};
#[cfg(windows)]
use slint::{LogicalSize, PhysicalPosition};

pub use ctx::Ctx;
pub use ipc::Ipc;
use ipc::{Action, Event, Link, Options};

// Slint's generated code trips the workspace `unused_qualifications` lint; it is not ours to fix.
#[allow(unused_qualifications)]
mod generated {
    slint::include_modules!();
}
pub use generated::{AppWindow, DaemonStatus, Page, Shell, Theme as Tokens, ThemeChoice};

/// How often the window looks for an edit of `config.toml` from outside (an editor, "Open
/// file"): a fixed file clears the banner and refreshes the pages without reopening.
// shortcut: a metadata poll, not a file watcher (`notify` is not a dependency of this crate);
// switch to a watcher if the second of delay matters.
pub const FILE_POLL: Duration = Duration::from_secs(1);

/// The accent the std widgets (`LineEdit`, `TextEdit`, `ComboBox`) derive their focus line and
/// selection from: our teal (token `accent`), never the Windows accent (redesign spec 7.3).
const ACCENT: Color = Color::from_rgb_u8(0x0F, 0x76, 0x6E);

/// Default size: `min(1280, 85 %)` x `min(800, 88 %)` of the work area, at least the 720 x 520
/// minimum (redesign spec section 2).
#[cfg(windows)]
const DEFAULT: (f32, f32) = (1280.0, 800.0);
#[cfg(windows)]
const MIN: (f32, f32) = (720.0, 520.0);

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
    if let Ok((area, scale)) = oxi_platform::os::display::work_area() {
        let (size, position) = placement(area, scale);
        window.window().set_size(size);
        window.window().set_position(position);
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
fn use_skia() -> bool {
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
        DEFAULT.0.min(w * 0.85).max(MIN.0),
        DEFAULT.1.min(h * 0.88).max(MIN.1),
    );
    let position = PhysicalPosition::new(
        area.x + ((w - size.width) * scale / 2.0).round() as i32,
        area.y + ((h - size.height) * scale / 2.0).round() as i32,
    );
    (size, position)
}

/// Puts our teal where Slint keeps the system accent, which the fluent std widgets tint from.
// shortcut: private Slint API, safe only while `slint` is pinned `=1.18.1`; re-check it on every
// Slint bump. The backend writes the system accent back when it creates the window and on a theme
// change: `run` calls this once the window is up and the file poll repeats it (a no-op write when
// nothing changed), so a new Windows accent can show for up to a second.
fn own_accent(window: &AppWindow) {
    slint::private_unstable_api::re_exports::WindowInner::from_pub(window.window())
        .context()
        .set_accent_color(ACCENT);
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
    let (mut ctx, problem) = Ctx::load(layout)?;
    let followed = link(&window)?;
    if let Some(ipc) = &followed {
        ctx.ipc = ipc.clone();
    }
    show_problem(&window, problem.as_ref());

    let weak = window.as_weak();
    let c = ctx.clone();
    window.on_theme_changed(move |choice| {
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
                on_daemon_event(&window, &Event::Link(c.ipc.link()));
            }
            bind(&window, &c);
        }
        report(&weak, result);
    });

    let banner = followed.is_some().then(|| {
        let weak = window.as_weak();
        let events = ctx.ipc.subscribe(move |event| {
            if let Some(window) = weak.upgrade() {
                on_daemon_event(&window, event);
            }
        });
        // The worker may have reported before the subscription: show the state as it is now.
        on_daemon_event(&window, &Event::Link(ctx.ipc.link()));
        events
    });
    let poll = Timer::default();
    poll.start(TimerMode::Repeated, FILE_POLL, {
        let (weak, c) = (window.as_weak(), ctx.clone());
        move || {
            if let Some(window) = weak.upgrade() {
                own_accent(&window);
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
            on_daemon_event(window, &Event::Link(ctx.ipc.link()));
        }
    }
    bind(window, ctx);
}

/// The daemon banner follows the link; a push that is not a notice leaves it alone.
fn on_daemon_event(window: &AppWindow, event: &Event) {
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
    if let Event::Link(link) = event {
        window.set_daemon_status(status(*link));
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

/// The rail's daemon chip for `link`.
const fn status(link: Link) -> DaemonStatus {
    match link {
        Link::Online | Link::Mismatch { .. } => DaemonStatus::Running,
        Link::Connecting | Link::Offline => DaemonStatus::Stopped,
        Link::Busy | Link::Stuck | Link::Untrusted | Link::Foreign => DaemonStatus::Stuck,
    }
}

/// Push the config into the shell and every page.
fn bind(window: &AppWindow, ctx: &Ctx) {
    let file = ctx.config.borrow();
    window.set_theme(choice(file.config.ui.theme));
    window.global::<Shell>().set_locked(file.read_only);
    drop(file);
    pages::bind(window, ctx);
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
    use super::{AppWindow, DaemonStatus, Link, ThemeChoice, choice, spawn_note, status, theme};
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
        // The banner, Home, Hotkey, Model and Overlay.
        assert_eq!(ipc.subscribers(), 5);
        for _ in 0..3 {
            window.invoke_reset_config();
        }
        assert_eq!(ipc.subscribers(), 5);
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

    #[test]
    fn daemon_chip_follows_the_link() {
        for (link, want) in [
            (Link::Online, DaemonStatus::Running),
            (Link::Mismatch { daemon_proto: 9 }, DaemonStatus::Running),
            (Link::Connecting, DaemonStatus::Stopped),
            (Link::Offline, DaemonStatus::Stopped),
            (Link::Busy, DaemonStatus::Stuck),
            (Link::Stuck, DaemonStatus::Stuck),
            (Link::Untrusted, DaemonStatus::Stuck),
            (Link::Foreign, DaemonStatus::Stuck),
        ] {
            assert_eq!(status(link), want, "{link:?}");
        }
    }

    #[test]
    fn theme_maps_both_ways() {
        for t in [Theme::System, Theme::Dark, Theme::Light] {
            assert_eq!(theme(choice(t)), t);
        }
        assert_eq!(choice(Theme::Dark), ThemeChoice::Dark);
    }
}
