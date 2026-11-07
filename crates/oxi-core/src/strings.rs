//! English text of every user-facing message of the daemon and the settings window, one place.
//! Each lookup is an exhaustive match: a new variant does not compile until it has its text.

use crate::APP_NAME;
use crate::machine::{EngineWarning, Notice, OverlayState, Phase};
use crate::models::ModelStatus;

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

// The settings window's link to the daemon: one text per state, for its banner and its pages.
pub const DAEMON_OFFLINE: &str = "OxiYapper is not running. Settings are saved to the config file.";
pub const DAEMON_MISMATCH: &str = "Restart OxiYapper to finish the update";
/// Also the IPC `Busy` error and the start of the daemon's own refusal.
pub const DAEMON_BUSY: &str = "OxiYapper is running but not answering";
pub const DAEMON_STUCK: &str =
    "The old OxiYapper did not exit. Quit it from the tray, then press Start.";
pub const PIPE_UNTRUSTED: &str =
    "The OxiYapper pipe is held by an untrusted program. Not connecting.";
pub const PIPE_FOREIGN: &str = "The OxiYapper pipe is held by another account. Not connecting.";

// Beside the disabled Preview button there is room for a few words, not the banner's two sentences.
pub const PREVIEW_OFFLINE: &str = "OxiYapper is not running";
pub const PREVIEW_MISMATCH: &str = "Restart OxiYapper first";

/// Banner of a config reload that failed; `line` is 1-based.
#[must_use]
pub fn config_error(file: &str, line: usize, message: &str) -> String {
    format!("{file}, line {line}: {message}")
}

/// The daemon could not be started; only the error kind, never the path.
#[must_use]
pub fn spawn_failed(kind: std::io::ErrorKind) -> String {
    format!("Could not start {APP_NAME}: {kind}.")
}

/// A configured device that is not plugged in, as its picker row shows it: the marker first, so
/// a narrow picker never cuts it off.
#[must_use]
pub fn not_connected(name: &str) -> String {
    format!("Not connected: {name}")
}

pub const NO_MIC: &str = "No microphone";
pub const SYSTEM_DEFAULT_MIC: &str = "System default";

pub const DOWNLOAD_NEEDS_DAEMON: &str = "Start OxiYapper to download models";
pub const DOWNLOAD_STARTING: &str = "Starting the download";

/// The daemon's reason for a failed model download, under the model's row.
#[must_use]
pub fn download_failed(message: &str) -> String {
    format!("Download failed: {message}")
}

/// Decimal gigabytes with one digit, the unit Hugging Face shows.
#[must_use]
pub fn gb(bytes: u64) -> String {
    #[expect(clippy::cast_precision_loss, reason = "one decimal digit is shown")]
    let gb = bytes as f64 / 1e9;
    format!("{gb:.1} GB")
}

/// Progress line of a model download.
#[must_use]
pub fn downloading(done: u64, total: u64) -> String {
    format!("Downloading {} of {}", gb(done), gb(total))
}

/// Status line of a registry model from its files.
#[must_use]
pub const fn model_status(status: ModelStatus) -> &'static str {
    match status {
        ModelStatus::Missing => "Not downloaded",
        ModelStatus::Partial => "Paused, Download resumes",
        ModelStatus::Ready => "Ready",
        ModelStatus::NeedsHash => "Downloaded, checked before first use",
        ModelStatus::Bad => "Damaged or incomplete, download it again",
    }
}

// Why a path cannot be a custom model.
pub const MODEL_PATH_EMPTY: &str = "Choose a .bin model file";
pub const MODEL_PATH_NOT_BIN: &str = "The model file must end in .bin";
pub const MODEL_PATH_RELATIVE: &str = "Use the full path to the file, from the drive letter";
pub const MODEL_PATH_FOLDER: &str = "This is a folder, not a model file";
pub const MODEL_PATH_MISSING: &str = "File not found";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_messages() {
        assert_eq!(gb(1_624_555_275), "1.6 GB");
        assert_eq!(gb(0), "0.0 GB");
        assert_eq!(
            downloading(1_000_000_000, 4_000_000_000),
            "Downloading 1.0 GB of 4.0 GB"
        );
        assert_eq!(
            spawn_failed(std::io::ErrorKind::NotFound),
            "Could not start OxiYapper: entity not found."
        );
        assert_eq!(
            config_error("config.toml", 3, "bad"),
            "config.toml, line 3: bad"
        );
        assert_eq!(not_connected("USB Mic"), "Not connected: USB Mic");
        assert_eq!(download_failed("reset"), "Download failed: reset");
    }
}
