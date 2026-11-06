//! Slint settings window and first-run wizard (ARCHITECTURE section 4).

use slint::ComponentHandle;

// Slint's generated code trips the workspace `unused_qualifications` lint; it is not ours to fix.
#[allow(unused_qualifications)]
mod generated {
    slint::include_modules!();
}
use generated::AppWindow;

/// Show the settings window and block until it is closed.
pub fn run() -> Result<(), slint::PlatformError> {
    AppWindow::new()?.run()
}

#[cfg(test)]
mod tests {
    use super::AppWindow;
    use slint::ComponentHandle;

    #[test]
    fn window_builds_under_the_testing_backend() {
        i_slint_backend_testing::init_no_event_loop();
        let window = AppWindow::new().expect("window builds");
        assert!(window.show().is_ok());
    }
}
