//! About page: name, version and licence of this build; and the title bar's icon size.

use crate::{AppWindow, Ctx};

pub fn bind(window: &AppWindow, _ctx: &Ctx) {
    window.set_app_name(oxi_core::APP_NAME.into());
    window.set_version(env!("CARGO_PKG_VERSION").into());
    window.set_licence(env!("CARGO_PKG_LICENSE").into());
    window.set_repository(env!("CARGO_PKG_REPOSITORY").into());
    window.set_icon_scale(opening_scale());
}

/// The scale the window opens at: Slint's override, else the monitor `show` places it on.
fn opening_scale() -> f32 {
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
