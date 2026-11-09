//! Overlay view logic: where the pill sits, when it hides, what it shows, how the meter moves.

mod look;
mod meter;
mod place;

pub use look::{Icon, Look, Rgba, look, palette};
pub use meter::{CLIP_HOLD, CLIP_PEAK, FLOOR_DBFS, METER_HZ, Meter};
pub use place::{
    MARGIN_DIP, PILL_BAR_GAP, PILL_BAR_MAX, PILL_BAR_W, PILL_EM, PILL_EM_MIN, PILL_GAP, PILL_H_DIP,
    PILL_ICON, PILL_METER_MIN, PILL_PAD, PILL_W_DIP, Quns, Rect, label_x, place, visible,
};
