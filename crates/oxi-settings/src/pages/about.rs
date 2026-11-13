//! About page: name, version and licence of this build; and the title bar's icon size.

use crate::{AppWindow, Ctx};

pub fn bind(window: &AppWindow, _ctx: &Ctx) {
    window.set_app_name(oxi_core::APP_NAME.into());
    window.set_version(env!("CARGO_PKG_VERSION").into());
    window.set_licence(env!("CARGO_PKG_LICENSE").into());
    window.set_repository(env!("CARGO_PKG_REPOSITORY").into());
}

/// Sets the scale the title-bar icon is sized at, and returns it. `open_inner` calls it right after
/// the window is made, before any page binds: the accent (`set_accent`, bound by System) builds the
/// icon at this scale, so no bind order can build it at 1.0. With no window (`None`) it only reads
/// the scale: the OS window's creation hook runs before there is one.
pub fn set_scale(window: Option<&AppWindow>) -> f32 {
    let scale = opening_scale();
    if let Some(window) = window {
        window.set_icon_scale(scale);
    }
    scale
}

#[cfg(test)]
thread_local! {
    /// A display scale for the test: a 96 DPI box never shows the scaled icon otherwise.
    pub(crate) static FORCED: std::cell::Cell<Option<f32>> = const { std::cell::Cell::new(None) };
}

/// The scale the window opens at: Slint's override, else the monitor `show` places it on.
fn opening_scale() -> f32 {
    #[cfg(test)]
    if let Some(scale) = FORCED.get() {
        return scale;
    }
    let forced = std::env::var("SLINT_SCALE_FACTOR")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|s| *s > 0.0);
    #[cfg(windows)]
    let os = oxi_platform::os::display::work_area().map_or(1.0, |(_, scale)| scale);
    #[cfg(not(windows))]
    let os = 1.0;
    forced.unwrap_or(os)
}
