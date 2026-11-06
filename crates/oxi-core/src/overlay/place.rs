//! Pill geometry and the fullscreen rule.

use crate::config::{Overlay, OverlayPosition};

/// Pill width in device-independent pixels (1 DIP = 1 px at 96 dpi).
pub const PILL_W_DIP: i32 = 168;
pub const PILL_H_DIP: i32 = 40;
/// Gap between the pill and the work-area edge.
pub const MARGIN_DIP: i32 = 16;

/// A rectangle in physical pixels of the virtual desktop; the origin is negative on a monitor
/// left of or above the primary one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// The pill rectangle for `position` inside `work_area` (taskbar already excluded).
/// `scale` is dpi / 96. The result never leaves `work_area`: on a tiny area the pill shrinks
/// and the margin collapses.
#[must_use]
pub fn place(position: OverlayPosition, work_area: Rect, scale: f32) -> Rect {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    #[allow(clippy::cast_possible_truncation)] // pixel sizes are small; the cast saturates
    let px = |dip: i32| (dip as f32 * scale).round() as i32;
    let w = px(PILL_W_DIP).min(work_area.w).max(0);
    let h = px(PILL_H_DIP).min(work_area.h).max(0);
    let margin = px(MARGIN_DIP);
    let left = (work_area.x + margin).min(work_area.x + work_area.w - w);
    let right = (work_area.x + work_area.w - w - margin).max(work_area.x);
    let top = (work_area.y + margin).min(work_area.y + work_area.h - h);
    let bottom = (work_area.y + work_area.h - h - margin).max(work_area.y);
    let centre = work_area.x + (work_area.w - w) / 2;
    let (x, y) = match position {
        OverlayPosition::BottomRight => (right, bottom),
        OverlayPosition::BottomCentre => (centre, bottom),
        OverlayPosition::BottomLeft => (left, bottom),
        OverlayPosition::TopRight => (right, top),
        OverlayPosition::TopCentre => (centre, top),
        OverlayPosition::TopLeft => (left, top),
    };
    Rect { x, y, w, h }
}

/// `SHQueryUserNotificationState`, mirrored so this crate stays free of OS calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quns {
    NotPresent,
    Busy,
    D3dFullScreen,
    PresentationMode,
    AcceptsNotifications,
    QuietTime,
    App,
}

/// Whether the overlay may be on screen: it is enabled, and it is not hidden over a
/// fullscreen app (busy, D3D fullscreen or presentation) when `hide_over_fullscreen` is set.
#[must_use]
pub fn visible(overlay: &Overlay, state: Quns) -> bool {
    let fullscreen = match state {
        Quns::Busy | Quns::D3dFullScreen | Quns::PresentationMode => true,
        Quns::NotPresent | Quns::AcceptsNotifications | Quns::QuietTime | Quns::App => false,
    };
    overlay.enabled && !(overlay.hide_over_fullscreen && fullscreen)
}
