//! Pill geometry, what its label and progress track show, and the fullscreen rule.

use std::fmt::Write;

use crate::config::{Overlay, OverlayPosition};
use crate::machine::OverlayState;
use crate::strings;
use crate::types::Millis;

use super::look::TOO_LOUD;

/// Pill width in device-independent pixels (1 DIP = 1 px at 96 dpi).
pub const PILL_W_DIP: i32 = 168;
pub const PILL_H_DIP: i32 = 40;
/// Gap between the pill and the work-area edge.
pub const MARGIN_DIP: i32 = 16;
/// Corner radius of the pill in DIP: the settings window's `radius-card` token (DS-2.2), so the
/// pill is rounded like every card of the app.
pub const CARD_RADIUS: i32 = 8;

// Inside the pill, as shares of its height, so every DPI draws the same picture.
/// Space between the pill's end and the icon or meter.
pub const PILL_PAD: f32 = 0.34;
/// Icon box side.
pub const PILL_ICON: f32 = 0.46;
/// Space between icon, label and meter.
pub const PILL_GAP: f32 = 0.22;
/// Label font em size.
pub const PILL_EM: f32 = 0.33;
/// Smallest label font, as a share of [`PILL_EM`], when the label must fit beside the meter.
pub const PILL_EM_MIN: f32 = 0.85;
/// Narrowest meter beside a label; a longer label shrinks its font instead.
pub const PILL_METER_MIN: f32 = 0.7;
/// Tallest meter bar.
pub const PILL_BAR_MAX: f32 = 0.5;
pub const PILL_BAR_W: f32 = 0.065;
pub const PILL_BAR_GAP: f32 = 0.05;
/// Progress track under the label while loading or transcribing: its thickness, and its gap to
/// the pill's bottom edge. It runs from the left to the right padding.
pub const PILL_TRACK: f32 = 0.05;
pub const PILL_TRACK_BOTTOM: f32 = 0.1;
/// One pass of the live indicator, the track segment shown while the percent is unknown.
pub const SWEEP: Millis = Millis(1200);
/// Length of the live indicator's segment, as a share of the track.
pub const SWEEP_LEN: f32 = 0.3;
/// A locked recording says how to stop for this long, then shows how long it has run.
pub const LOCK_HINT: Millis = Millis(3000);

/// The part of the progress track to fill, as shares `0..=1` of its length: up to `percent`
/// while whisper reports one, else a segment that sweeps left to right once per [`SWEEP`].
#[must_use]
pub fn progress_span(percent: Option<u8>, now: Millis) -> (f32, f32) {
    if let Some(p) = percent {
        return (0.0, f32::from(p.min(100)) / 100.0);
    }
    #[allow(clippy::cast_precision_loss)] // under SWEEP, a few thousand ms
    let t = (now.0 % SWEEP.0) as f32 / SWEEP.0 as f32;
    let head = t * (1.0 + SWEEP_LEN);
    ((head - SWEEP_LEN).max(0.0), head.min(1.0))
}

/// When the recording on the pill started and when it was locked, from the states the pill
/// was drawn with.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RecClock {
    start: Option<Millis>,
    locked: Option<Millis>,
}

impl RecClock {
    /// Notes the state drawn at `now`; any state but a recording ends the recording.
    pub fn see(&mut self, state: OverlayState, now: Millis) {
        if let OverlayState::Recording { locked, .. } = state {
            self.start.get_or_insert(now);
            if locked {
                self.locked.get_or_insert(now);
            } else {
                self.locked = None;
            }
        } else {
            *self = Self::default();
        }
    }
}

/// The pill's text into `out` (cleared first): the state's label, with the percent after it
/// while transcribing and whisper reports one ("Transcribing 42%"), and a locked recording's
/// length as `m:ss` once the lock is [`LOCK_HINT`] old (the lock icon still says locked).
pub fn pill_label(
    out: &mut String,
    state: OverlayState,
    percent: Option<u8>,
    clock: RecClock,
    now: Millis,
) {
    out.clear();
    let label = strings::overlay_label(state);
    let since = |t: Option<Millis>| t.map(|t| now - t);
    match (state, percent) {
        (OverlayState::Transcribing, Some(p)) => {
            let _ = write!(out, "{label} {}%", p.min(100));
        }
        (
            OverlayState::Recording {
                locked: true,
                ending_soon: false,
            },
            _,
        ) if since(clock.locked).is_some_and(|l| l >= LOCK_HINT) => {
            let secs = since(clock.start).unwrap_or_default().0 / 1000;
            let _ = write!(out, "{}:{:02}", secs / 60, secs % 60);
        }
        _ => out.push_str(label),
    }
}

/// Every text the pill can show beside the wave: the `state`'s label, the label of every
/// recording state (recording, locked, ending soon), the overload warning and the longest `m:ss`
/// clock. The wave box starts after the widest, so nothing moves when the label swaps or the
/// recording changes state (DS-21.3, DS-25).
#[must_use]
pub fn label_variants(state: OverlayState) -> [&'static str; 6] {
    let rec = |locked, ending_soon| {
        strings::overlay_label(OverlayState::Recording {
            locked,
            ending_soon,
        })
    };
    [
        strings::overlay_label(state),
        rec(false, false),
        rec(true, false),
        rec(true, true),
        TOO_LOUD,
        "00:00",
    ]
}

/// Left edge of a status label `text_w` wide in a pill `pill_w` wide: its box centred on the
/// pill, kept inside `min_x..=right` (after the icon, before the right padding) when the label is
/// too wide to centre. All values in pixels.
#[must_use]
pub fn label_x(pill_w: f32, min_x: f32, right: f32, text_w: f32) -> f32 {
    ((pill_w - text_w) / 2.0).min(right - text_w).max(min_x)
}

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
