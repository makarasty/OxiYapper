//! English text of every user-facing message of the daemon, one place. Each lookup is an
//! exhaustive match: a new variant does not compile until it has its text.

use crate::machine::{Notice, OverlayState, Phase};

pub const MENU_SETTINGS: &str = "Settings";
pub const MENU_COPY_LAST: &str = "Copy last text";
pub const MENU_QUIT: &str = "Quit";

/// Overlay line and OS notification text of a [`Notice`].
#[must_use]
pub const fn notice(notice: Notice) -> &'static str {
    match notice {
        Notice::Cancelled => "Cancelled",
        Notice::NothingHeard => "Nothing heard",
        Notice::NoModel => "No speech model. Open Settings to download one",
        Notice::ModelBad => "The speech model file is damaged. Download it again in Settings",
        Notice::DeviceLost => "Microphone disconnected",
        Notice::CaptureFailed => "Could not record from the microphone",
        Notice::TranscriptionFailed => "Transcription failed",
        Notice::PressPaste => "Text is on the clipboard, press Ctrl+V",
        Notice::AdminWindow => "Admin window: press Ctrl+V",
        Notice::RestoreSkipped => "Pasted. The previous clipboard could not be restored",
        Notice::PasteFailed => "Could not paste. Use Copy last text in the tray menu",
        Notice::MicFallback => "Selected microphone not found, using the system default",
    }
}

/// The line the overlay pill shows; empty while hidden.
#[must_use]
pub const fn overlay_label(state: OverlayState) -> &'static str {
    match state {
        OverlayState::Hidden => "",
        OverlayState::Recording {
            ending_soon: true, ..
        } => "Recording ends soon",
        OverlayState::Recording { locked: true, .. } => "Recording (locked)",
        OverlayState::Recording { .. } => "Recording",
        OverlayState::LoadingModel => "Loading model",
        OverlayState::Transcribing => "Transcribing",
        OverlayState::Pasting => "Pasting",
        OverlayState::Text(n) => notice(n),
    }
}

#[must_use]
pub const fn tray_tooltip(phase: Phase) -> &'static str {
    match phase {
        Phase::Idle => "OxiYapper: ready",
        Phase::Recording => "OxiYapper: recording",
        Phase::Transcribing => "OxiYapper: transcribing",
        Phase::Pasting => "OxiYapper: pasting",
    }
}
