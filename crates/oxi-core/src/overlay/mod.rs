//! Overlay view logic: where the pill sits, when it hides, what it shows, how the meter moves.

mod look;
mod meter;
mod place;

pub use look::{Icon, Look, Rgba, look, palette};
pub use meter::{CLIP_HOLD, CLIP_PEAK, FLOOR_DBFS, METER_HZ, Meter};
pub use place::{MARGIN_DIP, PILL_H_DIP, PILL_W_DIP, Quns, Rect, place, visible};
