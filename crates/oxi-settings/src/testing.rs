//! Helpers of the unit tests that drive the window; `tests/common` has the integration ones.

use std::sync::mpsc::Receiver;

use i_slint_backend_testing::ElementHandle;
use oxi_core::paths::Layout;
use slint::ComponentHandle;

use crate::ipc::{Cmd, Ipc};
use crate::{AppWindow, Page};

#[path = "../tests/common/elements.rs"]
mod elements;
pub use elements::{assert_nothing_moved, has, spots, with_role};
#[path = "../tests/common/scratch.rs"]
mod scratch;
pub use scratch::{fake_model, scratch_dir};

/// A fresh config directory per `module` and test `name`, removed when the test ends.
pub fn layout(module: &str, name: &str) -> Layout {
    let dir = scratch_dir(&format!(
        "oxi-settings-unit-{}-{module}-{name}",
        std::process::id()
    ));
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
    // The page's change handler moves the focus: let it run now, not in the middle of the test.
    slint::platform::update_timers_and_animations();
    (window, ipc, sent, layout)
}

/// The first element labelled `label`; a label may repeat (a chord shows in more than one place).
pub fn one(window: &AppWindow, label: &str) -> ElementHandle {
    ElementHandle::find_by_accessible_label(window, label)
        .next()
        .unwrap_or_else(|| panic!("no element labelled {label}"))
}
