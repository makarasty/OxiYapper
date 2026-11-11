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
    /// Green, top-right over the record dot: an update is available (M10).
    Update,
}

/// A notice the pill shows in the error colour; it raises [`TrayBadge::Error`].
#[must_use]
pub fn is_error(notice: Notice) -> bool {
    look(OverlayState::Text(notice)).accent == palette::ERROR
}

#[cfg(test)]
#[path = "../tests/common/notices.rs"]
mod notices;

#[cfg(test)]
mod tests {
    use super::notices::ALL_NOTICES;
    use super::*;

    /// Exhaustive on purpose: a new notice must be put on one side here.
    fn raises_the_badge(n: Notice) -> bool {
        match n {
            Notice::NoModel
            | Notice::ModelBad
            | Notice::OutOfMemory
            | Notice::DeviceLost
            | Notice::CaptureFailed
            | Notice::TranscriptionFailed
            | Notice::PasteFailed
            | Notice::ConfigInvalid => true,
            Notice::Cancelled
            | Notice::CancelledKept
            | Notice::NothingHeard
            | Notice::PressPaste
            | Notice::AdminWindow
            | Notice::RestoreSkipped
            | Notice::MicFallback
            | Notice::MicSilence
            | Notice::ConfigAdjusted
            | Notice::HistoryOn
            | Notice::FailedKept
            | Notice::Recovered => false,
        }
    }

    #[test]
    fn failures_raise_the_badge_and_hints_do_not() {
        for n in ALL_NOTICES {
            assert_eq!(is_error(n), raises_the_badge(n), "{n:?}");
        }
    }
}
