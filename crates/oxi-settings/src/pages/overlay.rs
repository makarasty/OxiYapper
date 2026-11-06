//! Overlay page: `[overlay]` (on/off, position, hide over fullscreen) and a preview the daemon
//! shows at the picked position.

use oxi_core::config::{Overlay, OverlayPosition};
use oxi_core::ipc::Request;
use slint::{ComponentHandle, Weak};

use crate::generated::OverlayState;
use crate::ipc::{Event, Link};
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
    s.set_online(ctx.ipc.link() == Link::Online);
    let w = window.as_weak();
    // shortcut: Reset rebinds and adds one more subscriber, each setting the same value; fine
    // for a handful of resets, subscribe once in the shell if pages ever do more here.
    ctx.ipc.subscribe(move |event| {
        if let (Event::Link(link), Some(window)) = (event, w.upgrade()) {
            window
                .global::<OverlayState>()
                .set_online(*link == Link::Online);
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
        let position = c.config.borrow().config.overlay.position;
        c.ipc.send(Request::PreviewOverlay { position });
    });
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
    use std::cell::RefCell;
    use std::rc::Rc;

    use i_slint_backend_testing::{AccessibleRole, ElementHandle};
    use oxi_core::config::{ConfigFile, OverlayPosition};
    use oxi_core::ipc::Request;
    use oxi_core::paths::Layout;
    use slint::ComponentHandle;

    use super::{POSITIONS, index};
    use crate::ipc::{Cmd, Event, Ipc, Link};
    use crate::{AppWindow, Ctx, Page};

    #[test]
    fn every_position_has_its_own_card() {
        for (i, &p) in POSITIONS.iter().enumerate() {
            assert_eq!(usize::try_from(index(p)).ok(), Some(i), "{p:?}");
        }
    }

    // A unit test: `open` gives no way to hand the window a fake daemon link yet.
    #[test]
    fn preview_sends_the_picked_position() {
        i_slint_backend_testing::init_no_event_loop();
        let dir =
            std::env::temp_dir().join(format!("oxi-settings-unit-preview-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (ipc, sent) = Ipc::fake();
        let ctx = Ctx {
            layout: Layout {
                config_dir: dir.clone(),
                data_dir: dir.clone(),
                logs_dir: dir,
            },
            config: Rc::new(RefCell::new(ConfigFile::default())),
            ipc,
        };
        let window = AppWindow::new().unwrap();
        crate::bind(&window, &ctx);
        window.show().unwrap();
        window.set_page(Page::Overlay);
        let find = |label: &str, role| {
            ElementHandle::find_by_accessible_label(&window, label)
                .find(|e| e.accessible_role() == Some(role))
                .unwrap_or_else(|| panic!("no {role:?} labelled {label}"))
        };

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
        ctx.ipc.deliver(&Event::Link(Link::Offline));
        let preview = find("Preview", AccessibleRole::Button);
        assert_eq!(preview.accessible_enabled(), Some(false));
        assert_eq!(
            preview.accessible_description().as_deref(),
            Some("OxiYapper is not running")
        );
        ctx.ipc.deliver(&Event::Link(Link::Online));
        assert_eq!(
            find("Preview", AccessibleRole::Button).accessible_enabled(),
            Some(true)
        );
    }
}
