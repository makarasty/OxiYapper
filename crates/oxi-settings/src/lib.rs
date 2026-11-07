//! Slint settings window and first-run wizard (ARCHITECTURE section 4).

mod ctx;
mod debounce;
pub mod ipc;
mod pages;

use std::io;

use oxi_core::config::{ConfigError, Theme};
use oxi_core::ipc::Push;
use oxi_core::paths::Layout;
use slint::{ComponentHandle, Weak};

pub use ctx::Ctx;
pub use ipc::Ipc;
use ipc::{Action, Event, Link, Options};

// Slint's generated code trips the workspace `unused_qualifications` lint; it is not ours to fix.
#[allow(unused_qualifications)]
mod generated {
    slint::include_modules!();
}
pub use generated::{AppWindow, Page, Shell, ThemeChoice};

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
    // The window comes first: it sets up the Slint platform, and the worker's events need that
    // to reach the UI thread.
    let window = open_inner(layout, |window| {
        let mut options = Options::daemon()?;
        options.spawn = watch_spawn(options.spawn, window.as_weak());
        Ok(Some(Ipc::connect(options)?))
    })?;
    window.run()?;
    Ok(())
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
    format!("Could not start OxiYapper: {}.", e.kind())
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
    let (mut ctx, problem) = Ctx::load(layout)?;
    let followed = link(&window)?;
    if let Some(ipc) = &followed {
        ctx.ipc = ipc.clone();
    }
    if let Some(problem) = &problem {
        window.set_banner_text(problem.to_string().into());
        // A broken file gets "Open file" and "Reset to defaults"; a newer one only the message.
        window.set_banner_fixable(!matches!(problem, ConfigError::ReadOnly));
    }

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
    window.on_reset_config(move || {
        let result = c.reset();
        if let (Ok(()), Some(window)) = (&result, weak.upgrade()) {
            window.set_banner_text("".into());
            bind(&window, &c);
        }
        report(&weak, result);
    });

    if followed.is_some() {
        let weak = window.as_weak();
        ctx.ipc.subscribe(move |event| {
            if let Some(window) = weak.upgrade() {
                on_daemon_event(&window, event);
            }
        });
        // The worker may have reported before the subscription: show the state as it is now.
        on_daemon_event(&window, &Event::Link(ctx.ipc.link()));
    }
    let ipc = ctx.ipc.clone();
    window.on_start_daemon({
        let weak = window.as_weak();
        move || {
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
    let action = notice.as_ref().and_then(|n| n.action);
    window.set_daemon_text(notice.map(|n| n.text).unwrap_or_default().into());
    window.set_daemon_start(action == Some(Action::Start));
    window.set_daemon_restart(action == Some(Action::Restart));
    // Connecting and Offline keep it: the failure is reported just before them.
    if matches!(event, Event::Link(link) if !matches!(link, Link::Connecting | Link::Offline)) {
        window.set_spawn_error("".into());
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
    use super::{AppWindow, ThemeChoice, choice, spawn_note, theme};
    use oxi_core::config::Theme;
    use slint::ComponentHandle;
    use std::io::{Error, ErrorKind};

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
    fn theme_maps_both_ways() {
        for t in [Theme::System, Theme::Dark, Theme::Light] {
            assert_eq!(theme(choice(t)), t);
        }
        assert_eq!(choice(Theme::Dark), ThemeChoice::Dark);
    }
}
