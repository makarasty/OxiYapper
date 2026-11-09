//! Helpers shared by the settings test binaries.
#![allow(
    dead_code,
    unused_imports,
    reason = "each test binary uses only some of the helpers"
)]

use std::fs;

use i_slint_backend_testing::ElementHandle;
use oxi_core::config::ConfigFile;
use oxi_core::paths::Layout;
use oxi_settings::{AppWindow, Ipc, Page};
use slint::ComponentHandle;

mod elements;
pub use elements::{assert_nothing_moved, has, spots, with_role};

/// A fresh directory per test and test binary; `config` is written as `config.toml` when given.
pub fn layout(name: &str, config: Option<&str>) -> Layout {
    let dir = std::env::temp_dir().join(format!(
        "oxi-settings-{}-{}-{name}",
        env!("CARGO_CRATE_NAME"),
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    if let Some(text) = config {
        fs::write(dir.join("config.toml"), text).unwrap();
    }
    Layout {
        config_dir: dir.clone(),
        data_dir: dir.clone(),
        logs_dir: dir,
    }
}

/// The settings window on `layout` with no daemon, shown on `page`. Sets up the testing
/// backend, so once per test; [`open_again`] opens another.
pub fn open(layout: &Layout, page: Page) -> AppWindow {
    i_slint_backend_testing::init_no_event_loop();
    open_again(layout, page)
}

/// [`open`] without choosing a page: the window as it starts, on its default page.
pub fn open_default(layout: &Layout) -> AppWindow {
    i_slint_backend_testing::init_no_event_loop();
    let window = oxi_settings::open(layout.clone()).unwrap();
    window.show().unwrap();
    window
}

/// [`open`] on a backend that is already set up.
pub fn open_again(layout: &Layout, page: Page) -> AppWindow {
    shown(oxi_settings::open(layout.clone()).unwrap(), page)
}

/// [`open`] on the daemon link `ipc` (usually [`Ipc::fake`]).
pub fn open_linked(layout: &Layout, page: Page, ipc: Ipc) -> AppWindow {
    i_slint_backend_testing::init_no_event_loop();
    shown(oxi_settings::open_with(layout.clone(), ipc).unwrap(), page)
}

fn shown(window: AppWindow, page: Page) -> AppWindow {
    window.show().unwrap();
    window.set_page(page);
    window
}

/// The only element labelled `label`.
pub fn one(window: &AppWindow, label: &str) -> ElementHandle {
    let mut found = ElementHandle::find_by_accessible_label(window, label);
    let first = found
        .next()
        .unwrap_or_else(|| panic!("no element labelled {label}"));
    assert!(found.next().is_none(), "more than one labelled {label}");
    first
}

/// The saved `config.toml`.
pub fn config(l: &Layout) -> ConfigFile {
    ConfigFile::parse(&fs::read_to_string(l.config_file()).unwrap_or_default()).unwrap()
}

/// A pixel of the purple system accent family: HSV hue 260..300 with chroma over 0.08.
pub fn purple(r: u8, g: u8, b: u8) -> bool {
    let (r, g, b) = (f32::from(r), f32::from(g), f32::from(b));
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let chroma = (max - min) / 255.0;
    if chroma <= 0.08 {
        return false;
    }
    let d = max - min;
    let hue = if (max - r).abs() < f32::EPSILON {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if (max - g).abs() < f32::EPSILON {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (260.0..=300.0).contains(&hue)
}
