//! Overlay page: `[overlay]` (on/off, position, hide over fullscreen) and a preview the daemon
//! shows at the picked position.

use oxi_core::config::{Overlay, OverlayPosition};
use oxi_core::ipc::Request;
use oxi_core::strings;
use slint::{ComponentHandle, Weak};

use crate::generated::OverlayState;
use crate::ipc::{Event, Link, notice};
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
    set_link(&s, ctx.ipc.link());
    let w = window.as_weak();
    let events = ctx.ipc.subscribe(move |event| {
        if let (Event::Link(link), Some(window)) = (event, w.upgrade()) {
            set_link(&window.global::<OverlayState>(), *link);
        }
    });

    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_enabled_toggled(move |on| apply(&w, &c, |o| o.enabled = on));
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_hide_over_fullscreen_toggled(move |on| apply(&w, &c, |o| o.hide_over_fullscreen = on));
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_position_picked(move |i| {
        if let Some(&position) = usize::try_from(i).ok().and_then(|i| POSITIONS.get(i)) {
            apply(&w, &c, |o| o.position = position);
        }
    });
    let c = ctx.clone();
    s.on_preview(move || {
        // Held by this callback: a rebind (Reset) replaces it and so unsubscribes.
        let _ = &events;
        let position = c.config.borrow().config.overlay.position;
        c.ipc.send(Request::PreviewOverlay { position });
    });
}

/// Preview needs `Online`; any other state says why: a short text for the two usual ones, the
/// shell banner's text for the rest.
fn set_link(s: &OverlayState, link: Link) {
    s.set_online(link == Link::Online);
    let note = match link {
        Link::Offline => Some(strings::PREVIEW_OFFLINE.to_owned()),
        Link::Mismatch { .. } => Some(strings::PREVIEW_MISMATCH.to_owned()),
        _ => notice(&Event::Link(link)).map(|n| n.text),
    };
    s.set_note(note.unwrap_or_default().into());
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
    use crate::ipc::{Cmd, Event, Link};
    use crate::testing::{linked, with_role};

    #[test]
    fn every_position_has_its_own_card() {
        for (i, &p) in POSITIONS.iter().enumerate() {
            assert_eq!(usize::try_from(index(p)).ok(), Some(i), "{p:?}");
        }
    }

    #[test]
    fn preview_sends_the_picked_position() {
        let (window, ipc, sent, _) = linked("overlay", "preview", Page::Overlay);
        let find = |label: &str, role| with_role(&window, label, role);

        find("Top centre", AccessibleRole::RadioButton).invoke_accessible_default_action();
        assert!(sent.try_recv().is_err(), "a pick alone sends nothing");
        find("Preview", AccessibleRole::Button).invoke_accessible_default_action();
        assert_eq!(
            sent.try_recv().ok(),
            Some(Cmd::Send(Request::PreviewOverlay {
                position: OverlayPosition::TopCentre
            }))
        );

        // The daemon goes away: Preview turns off and says why.
        ipc.deliver(&Event::Link(Link::Offline));
        let preview = find("Preview", AccessibleRole::Button);
        assert_eq!(preview.accessible_enabled(), Some(false));
        assert_eq!(
            preview.accessible_description().as_deref(),
            Some(oxi_core::strings::PREVIEW_OFFLINE)
        );
        ipc.deliver(&Event::Link(Link::Online));
        assert_eq!(
            find("Preview", AccessibleRole::Button).accessible_enabled(),
            Some(true)
        );

        // Connecting says nothing; a version mismatch is not "not running".
        ipc.deliver(&Event::Link(Link::Connecting));
        let preview = find("Preview", AccessibleRole::Button);
        assert_eq!(preview.accessible_enabled(), Some(false));
        assert_eq!(preview.accessible_description().as_deref(), Some(""));
        ipc.deliver(&Event::Link(Link::Mismatch { daemon_proto: 9 }));
        let preview = find("Preview", AccessibleRole::Button);
        assert_eq!(preview.accessible_enabled(), Some(false));
        assert_eq!(
            preview.accessible_description().as_deref(),
            Some(oxi_core::strings::PREVIEW_MISMATCH)
        );
    }
}
