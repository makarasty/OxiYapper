//! About page: name, version and licence of this build.

use crate::{AppWindow, Ctx};

pub fn bind(window: &AppWindow, _ctx: &Ctx) {
    window.set_app_name(oxi_core::APP_NAME.into());
    window.set_version(env!("CARGO_PKG_VERSION").into());
    window.set_licence(env!("CARGO_PKG_LICENSE").into());
    window.set_repository(env!("CARGO_PKG_REPOSITORY").into());
}
