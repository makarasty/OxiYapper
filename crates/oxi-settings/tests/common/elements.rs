//! Element lookups shared by the unit tests (`src/testing.rs`) and the integration tests
//! (`tests/common`): this file is included by both, so it names no type of the crate.

use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementRoot};

/// The element labelled `label` with `role`: a row's text and its control can share a label.
pub fn with_role(window: &impl ElementRoot, label: &str, role: AccessibleRole) -> ElementHandle {
    ElementHandle::find_by_accessible_label(window, label)
        .find(|e| e.accessible_role() == Some(role))
        .unwrap_or_else(|| panic!("no {role:?} labelled {label}"))
}

pub fn has(window: &impl ElementRoot, label: &str) -> bool {
    ElementHandle::find_by_accessible_label(window, label)
        .next()
        .is_some()
}
