//! Tray menu entries and the icon badge; the platform tray draws them, the daemon acts on them.

use crate::machine::{Notice, OverlayState};
use crate::overlay::{look, palette};

/// A tray menu entry; the discriminant is its menu command id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
    Settings = 1,
    CopyLast = 2,
    Quit = 3,
}

/// The small dot on the tray icon over the look of the phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrayBadge {
    #[default]
    None,
    /// Red, bottom-right: the last dictation failed or the config is invalid.
    Error,
    /// Teal, top-right: an update is available (M10).
    Update,
}

/// A notice the pill shows in the error colour; it raises [`TrayBadge::Error`].
#[must_use]
pub fn is_error(notice: Notice) -> bool {
    look(OverlayState::Text(notice)).accent == palette::ERROR
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_raise_the_badge_and_hints_do_not() {
        for n in [
            Notice::NoModel,
            Notice::ModelBad,
            Notice::DeviceLost,
            Notice::CaptureFailed,
            Notice::TranscriptionFailed,
            Notice::PasteFailed,
            Notice::ConfigInvalid,
        ] {
            assert!(is_error(n), "{n:?}");
        }
        for n in [
            Notice::Cancelled,
            Notice::NothingHeard,
            Notice::PressPaste,
            Notice::MicFallback,
            Notice::MicSilence,
            Notice::ConfigAdjusted,
        ] {
            assert!(!is_error(n), "{n:?}");
        }
    }
}
