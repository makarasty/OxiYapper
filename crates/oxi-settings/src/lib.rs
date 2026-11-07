//! Slint settings window and first-run wizard (ARCHITECTURE section 4).

mod ctx;
mod debounce;
pub mod ipc;
mod pages;

use std::io;

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
