//! Helpers of the unit tests that drive the window; `tests/common` has the integration ones.

use std::sync::mpsc::Receiver;

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use oxi_core::paths::Layout;
use slint::ComponentHandle;

use crate::ipc::{Cmd, Ipc};
use crate::{AppWindow, Page};

/// A fresh config directory per `module` and test `name`.
pub fn layout(module: &str, name: &str) -> Layout {
    let dir = std::env::temp_dir().join(format!(
        "oxi-settings-unit-{}-{module}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Layout {
        config_dir: dir.clone(),
        data_dir: dir.clone(),
        logs_dir: dir,
    }
}

/// The window on a fresh config directory and a fake daemon link, shown on `page`; the receiver
/// gets what would go to the daemon.
pub fn linked(module: &str, name: &str, page: Page) -> (AppWindow, Ipc, Receiver<Cmd>, Layout) {
    i_slint_backend_testing::init_no_event_loop();
    let layout = layout(module, name);
    let (ipc, sent) = Ipc::fake();
    let window = crate::open_with(layout.clone(), ipc.clone()).unwrap();
    window.show().unwrap();
    window.set_page(page);
    (window, ipc, sent, layout)
}

/// The first element labelled `label`; a label may repeat (a chord shows in more than one place).
pub fn one(window: &AppWindow, label: &str) -> ElementHandle {
    ElementHandle::find_by_accessible_label(window, label)
        .next()
        .unwrap_or_else(|| panic!("no element labelled {label}"))
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
