//! English text of every user-facing message of the daemon, one place. Each lookup is an
//! exhaustive match: a new variant does not compile until it has its text.

use crate::APP_NAME;
use crate::machine::{EngineWarning, Notice, OverlayState, Phase};

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
        Notice::ConfigInvalid => {
            "config.toml has an error. Using the last good settings. Open Settings to see it"
        }
        Notice::ConfigAdjusted => "config.toml had values out of range. They were adjusted",
        Notice::MicSilence => "The microphone gives silence. Check its mute switch",
    }
}

/// Tray and `Status` line of an [`EngineWarning`].
#[must_use]
pub fn engine_warning(warning: &EngineWarning) -> String {
    match warning {
        EngineWarning::CpuFallback { reason } => format!("running on CPU: {reason}"),
        EngineWarning::GpuNotFound { name } => format!("GPU {name:?} not found, using auto"),
    }
}

/// Longest overlay label, in characters: the pill is 168 DIP wide, about 16 characters at 12 px.
pub const OVERLAY_LABEL_MAX: usize = 16;

/// Short form of a [`Notice`] for the overlay pill; [`notice`] has the full text.
#[must_use]
pub const fn notice_short(notice: Notice) -> &'static str {
    match notice {
        Notice::Cancelled => "Cancelled",
        Notice::NothingHeard => "Nothing heard",
        Notice::NoModel => "No model",
        Notice::ModelBad => "Model error",
        Notice::DeviceLost => "Mic lost",
        Notice::CaptureFailed => "Mic error",
        Notice::TranscriptionFailed => "Speech failed",
        Notice::PressPaste => "Press Ctrl+V",
        Notice::AdminWindow => "Admin window",
        Notice::RestoreSkipped => "Not restored",
        Notice::PasteFailed => "Paste failed",
        Notice::MicFallback => "Mic changed",
        Notice::ConfigInvalid => "Config error",
        Notice::ConfigAdjusted => "Config adjusted",
        Notice::MicSilence => "Mic silent",
    }
}

/// The line the overlay pill shows (at most [`OVERLAY_LABEL_MAX`] characters); empty while
/// hidden.
#[must_use]
pub const fn overlay_label(state: OverlayState) -> &'static str {
    match state {
        OverlayState::Hidden => "",
        // Locked and ending: the lock icon shows the lock, the label the warning.
        OverlayState::Recording {
            ending_soon: true, ..
        } => "Ending soon",
        OverlayState::Recording { locked: true, .. } => "Locked",
        OverlayState::Recording { .. } => "Recording",
        OverlayState::LoadingModel => "Loading model",
        OverlayState::Transcribing => "Transcribing",
        OverlayState::Pasting => "Pasting",
        OverlayState::Text(n) => notice_short(n),
    }
}

#[must_use]
pub fn tray_tooltip(phase: Phase) -> String {
    let state = match phase {
        Phase::Idle => "ready",
        Phase::Recording => "recording",
        Phase::Transcribing => "transcribing",
        Phase::Pasting => "pasting",
    };
    format!("{APP_NAME}: {state}")
}
