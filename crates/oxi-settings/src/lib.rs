//! Slint settings window and first-run wizard (ARCHITECTURE section 4).

mod ctx;
mod ipc;
mod pages;

use std::path::Path;
use std::{io, process};

use oxi_core::config::{ConfigError, Theme};
use oxi_core::paths::Layout;
use slint::{ComponentHandle, Weak};

pub use ctx::Ctx;
pub use ipc::Ipc;

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
    open(layout)?.run()?;
    Ok(())
}

/// The settings window on `layout`'s config, every page bound; the caller shows it.
pub fn open(layout: Layout) -> Result<AppWindow, Error> {
    let window = AppWindow::new()?;
    let (ctx, problem) = Ctx::load(layout)?;
    if let Some(problem) = &problem {
        show_banner(&window, problem);
    }

    let weak = window.as_weak();
    let c = ctx.clone();
    window.on_theme_changed(move |choice| {
        c.config.borrow_mut().config.ui.theme = theme(choice);
        if let Err(e) = c.save() {
            report(&weak, &e);
        }
    });

    let weak = window.as_weak();
    let path = ctx.layout.config_file();
    window.on_open_file(move || {
        if let Err(e) = open_in_editor(&path) {
            report(&weak, &e.into());
        }
    });

    let weak = window.as_weak();
    let c = ctx.clone();
    window.on_reset_config(move || {
        let Some(window) = weak.upgrade() else { return };
        match c.reset() {
            Ok(()) => {
                window.set_banner_text("".into());
                bind(&window, &c);
            }
            Err(e) => report(&weak, &e),
        }
    });

    bind(&window, &ctx);
    Ok(window)
}

/// Push the config into the shell and every page.
fn bind(window: &AppWindow, ctx: &Ctx) {
    let file = ctx.config.borrow();
    window.set_theme(choice(file.config.ui.theme));
    window.global::<Shell>().set_locked(file.read_only);
    drop(file);
    pages::bind(window, ctx);
}

/// A broken file gets "Open file" and "Reset to defaults"; a newer one only the message.
fn show_banner(window: &AppWindow, problem: &ConfigError) {
    window.set_banner_text(problem.to_string().into());
    window.set_banner_fixable(!matches!(problem, ConfigError::ReadOnly));
}

/// A failed save or file action: say so in the banner, without the fix buttons.
fn report(window: &Weak<AppWindow>, error: &Error) {
    if let Some(window) = window.upgrade() {
        window.set_banner_text(error.to_string().into());
        window.set_banner_fixable(false);
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

/// Open `path` in the user's default app for it. The path is one argument, never a shell string.
// shortcut: lives here as the only caller; move to oxi_platform when the daemon needs it too.
fn open_in_editor(path: &Path) -> io::Result<()> {
    let opener = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut child = process::Command::new(opener).arg(path).spawn()?;
    // Reap it off the UI thread. explorer exits 1 even when it opened the file, so only a
    // failed spawn is an error.
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AppWindow, ThemeChoice, choice, theme};
    use oxi_core::config::Theme;
    use slint::ComponentHandle;

    #[test]
    fn window_builds_under_the_testing_backend() {
        i_slint_backend_testing::init_no_event_loop();
        let window = AppWindow::new().expect("window builds");
        assert!(window.show().is_ok());
    }

    #[test]
    fn theme_maps_both_ways() {
        for t in [Theme::System, Theme::Dark, Theme::Light] {
            assert_eq!(theme(choice(t)), t);
        }
        assert_eq!(choice(Theme::Dark), ThemeChoice::Dark);
    }
}
