//! Overlay page: `[overlay]` (on/off, position, hide over fullscreen). Picking a position, the
//! picked one too, also asks the daemon to show the pill there.

use oxi_core::config::{Overlay, OverlayPosition};
use oxi_core::ipc::Request;
use slint::{ComponentHandle, Weak};

use crate::generated::OverlayState;
use crate::{AppWindow, Ctx, report};

/// Picker order: the top row left to right, then the bottom row.
const POSITIONS: [OverlayPosition; 6] = [
    OverlayPosition::TopLeft,
    OverlayPosition::TopCentre,
    OverlayPosition::TopRight,
    OverlayPosition::BottomLeft,
    OverlayPosition::BottomCentre,
    OverlayPosition::BottomRight,
];

pub fn bind(window: &AppWindow, ctx: &Ctx) {
    let s = window.global::<OverlayState>();
    let file = ctx.config.borrow();
    let overlay = &file.config.overlay;
    s.set_enabled(overlay.enabled);
    s.set_position(index(overlay.position));
    s.set_hide_over_fullscreen(overlay.hide_over_fullscreen);
    drop(file);

    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_enabled_toggled(move |on| apply(&w, &c, |o| o.enabled = on));
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_hide_over_fullscreen_toggled(move |on| apply(&w, &c, |o| o.hide_over_fullscreen = on));
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_position_picked(move |i| pick(&w, &c, i));
}

/// Save the picked spot and show the pill there; the selected tile again shows it again. The send
/// is dropped by the link unless the daemon is online.
fn pick(weak: &Weak<AppWindow>, ctx: &Ctx, i: i32) {
    if let Some(&position) = usize::try_from(i).ok().and_then(|i| POSITIONS.get(i)) {
        apply(weak, ctx, |o| o.position = position);
        ctx.ipc.send(Request::PreviewOverlay { position });
    }
}

/// Change `[overlay]`, show it again and save.
fn apply(weak: &Weak<AppWindow>, ctx: &Ctx, change: impl FnOnce(&mut Overlay)) {
    change(&mut ctx.config.borrow_mut().config.overlay);
    if let Some(window) = weak.upgrade() {
        let position = ctx.config.borrow().config.overlay.position;
        window
            .global::<OverlayState>()
            .set_position(index(position));
    }
    report(weak, ctx.save());
}

fn index(position: OverlayPosition) -> i32 {
    POSITIONS
        .iter()
        .position(|&p| p == position)
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use i_slint_backend_testing::AccessibleRole;
    use oxi_core::config::OverlayPosition;
    use oxi_core::ipc::Request;

    use super::{POSITIONS, index};
    use crate::Page;
    use crate::ipc::Cmd;
    use crate::testing::{linked, with_role};

    #[test]
    fn every_position_has_its_own_card() {
        for (i, &p) in POSITIONS.iter().enumerate() {
            assert_eq!(usize::try_from(index(p)).ok(), Some(i), "{p:?}");
        }
    }

    /// DS-27.1: a click on a tile picks it and previews it at once; on the picked tile too.
    #[test]
    fn a_pick_previews_that_position_and_so_does_the_picked_one_again() {
        let (window, _ipc, sent, _) = linked("overlay", "preview", Page::Overlay);
        let tile = |label: &str| with_role(&window, label, AccessibleRole::RadioButton);
        let preview = |position| Some(Cmd::Send(Request::PreviewOverlay { position }));

        tile("Top centre").invoke_accessible_default_action();
        assert_eq!(sent.try_recv().ok(), preview(OverlayPosition::TopCentre));
        tile("Top centre").invoke_accessible_default_action();
        assert_eq!(sent.try_recv().ok(), preview(OverlayPosition::TopCentre));
        assert!(sent.try_recv().is_err(), "one request per click");
    }
}
