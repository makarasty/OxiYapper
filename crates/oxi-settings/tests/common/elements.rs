//! Element lookups shared by the unit tests (`src/testing.rs`) and the integration tests
//! (`tests/common`): this file is included by both, so it names no type of the crate.

use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery, ElementRoot};

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

/// A labelled element: label, role, and x, y, width, height.
pub type Spot = (String, Option<AccessibleRole>, [f32; 4]);

/// Where every labelled element of the window is now.
pub fn spots(window: &impl ElementRoot) -> Vec<Spot> {
    // Runs the `changed` handlers of the last change (the shell's page width is one).
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(1));
    ElementQuery::from_root(window)
        .find_all()
        .iter()
        .filter_map(|e| {
            let label = e.accessible_label().filter(|l| !l.is_empty())?;
            let (at, size) = (e.absolute_position(), e.size());
            Some((
                label.to_string(),
                e.accessible_role(),
                [at.x, at.y, size.width, size.height],
            ))
        })
        .collect()
}

/// UI rule 1: whatever is in both snapshots has not moved or changed size, so a row's height and
/// the y of the row after it are what they were. An element only one of them has is the state
/// itself (the "!", a toast, a button that takes the switch's place).
pub fn assert_nothing_moved(before: &[Spot], after: &[Spot]) {
    let mut compared = 0;
    for spot in before {
        let same: Vec<_> = after
            .iter()
            .filter(|a| a.0 == spot.0 && a.1 == spot.1)
            .collect();
        if same.is_empty() {
            continue;
        }
        compared += 1;
        assert!(
            // Exact: layout units are whole logical px, and a moved row is any difference.
            same.iter()
                .any(|a| a.2.map(f32::to_bits) == spot.2.map(f32::to_bits)),
            "{} moved: {:?} is now {:?}",
            spot.0,
            spot.2,
            same.iter().map(|a| a.2).collect::<Vec<_>>()
        );
    }
    assert!(compared > 3, "only {compared} elements compared");
}
