//! Overlay view logic: where the pill sits, when it hides, what it shows, how the meter moves.

mod accent;
mod look;
mod meter;
mod place;

pub use accent::{
    AccentTheme, PRESETS, Preset, close_to_error, contrast, derive, hex, lift, mix, parse_seed,
    pill_accent, pill_rim, preset_index, seed, tray_colour,
};
pub use look::{Icon, Look, Rgba, TOO_LOUD, look, look_with, palette};
pub use meter::{
    FLOOR_DBFS, HOT_FRAME, HOT_PEAK, METER_HZ, Meter, OVERLOAD_HOLD, OVERLOAD_HOT, OVERLOAD_WINDOW,
    Overload,
};
pub use place::{
    CARD_RADIUS, LOCK_HINT, MARGIN_DIP, PILL_BAR_GAP, PILL_BAR_MAX, PILL_BAR_W, PILL_EM,
    PILL_EM_MIN, PILL_GAP, PILL_H_DIP, PILL_ICON, PILL_METER_MIN, PILL_PAD, PILL_TRACK,
    PILL_TRACK_BOTTOM, PILL_W_DIP, Quns, RecClock, Rect, SWEEP, SWEEP_LEN, label_variants, label_x,
    pill_label, place, progress_span, visible,
};
