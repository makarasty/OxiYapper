//! One module per page. Each `bind` pushes the config into its page and wires its callbacks.
//! `bind` runs again after Reset to defaults, so it sets every value its page shows.

mod about;
mod audio;
mod home;
mod hotkey;
mod model;
mod overlay;
mod system;
pub mod test;
mod text;

use crate::{AppWindow, Ctx};

/// Called on the file poll: the pages that show a file follow it.
pub fn poll(window: &AppWindow, ctx: &Ctx) {
    text::poll(window, &ctx.layout);
}

pub fn bind(window: &AppWindow, ctx: &Ctx) {
    home::bind(window, ctx);
    hotkey::bind(window, ctx);
    audio::bind(window, ctx);
    model::bind(window, ctx);
    text::bind(window, ctx);
    overlay::bind(window, ctx);
    system::bind(window, ctx);
    about::bind(window, ctx);
}
