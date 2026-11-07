//! Helpers shared by the settings test binaries.
#![allow(dead_code, reason = "each test binary uses only some of the helpers")]

use std::fs;

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use oxi_core::config::ConfigFile;
use oxi_core::paths::Layout;
use oxi_settings::{AppWindow, Ipc, Page};
use slint::ComponentHandle;

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

/// The element labelled `label` with `role`: a row's text and its control can share a label.
pub fn with_role(window: &AppWindow, label: &str, role: AccessibleRole) -> ElementHandle {
    ElementHandle::find_by_accessible_label(window, label)
        .find(|e| e.accessible_role() == Some(role))
        .unwrap_or_else(|| panic!("no {role:?} labelled {label}"))
}

pub fn has(window: &AppWindow, label: &str) -> bool {
    ElementHandle::find_by_accessible_label(window, label)
        .next()
        .is_some()
}

/// The saved `config.toml`.
pub fn config(l: &Layout) -> ConfigFile {
    ConfigFile::parse(&fs::read_to_string(l.config_file()).unwrap()).unwrap()
}
