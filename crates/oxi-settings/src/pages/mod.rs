//! One module per page. Each `bind` pushes the config into its page and wires its callbacks.
//! `bind` runs again after Reset to defaults, so it sets every value its page shows.

mod about;
mod audio;
mod hotkey;
mod model;
mod overlay;
mod system;
mod text;

use crate::{AppWindow, Ctx};

pub fn bind(window: &AppWindow, ctx: &Ctx) {
    hotkey::bind(window, ctx);
    audio::bind(window, ctx);
    model::bind(window, ctx);
    text::bind(window, ctx);
    overlay::bind(window, ctx);
    system::bind(window, ctx);
    about::bind(window, ctx);
}
