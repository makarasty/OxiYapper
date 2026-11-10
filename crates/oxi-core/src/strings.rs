//! English text of every user-facing message of the daemon and the settings window, one place.
//! Each lookup is an exhaustive match: a new variant does not compile until it has its text.

use crate::APP_NAME;
use crate::ipc::Attention;
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
        Notice::CancelledKept => "Cancelled. The recording was kept",
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
        Notice::MicSilence => {
            "The microphone gives no sound. Check mute, reconnect it or pick another mic"
        }
        Notice::OutOfMemory => {
            "Not enough memory to load the speech model. Close other apps or pick a smaller model"
        }
        Notice::HistoryOn => "Recordings are now kept for 30 days. Turn off in Audio.",
    }
}

/// Start of the `Status` line of [`EngineWarning::CpuFallback`]; [`cpu_fallback_reason`] reads it back.
const CPU_FALLBACK_PREFIX: &str = "running on CPU: ";

/// Tray and `Status` line of an [`EngineWarning`].
#[must_use]
pub fn engine_warning(warning: &EngineWarning) -> String {
    match warning {
        EngineWarning::CpuFallback { reason } => format!("{CPU_FALLBACK_PREFIX}{reason}"),
        EngineWarning::GpuNotFound { name } => format!("GPU {name:?} not found, using auto"),
    }
}

/// The reason in a `Status` line made by [`engine_warning`] for a CPU fallback.
#[must_use]
pub fn cpu_fallback_reason(line: &str) -> Option<&str> {
    line.strip_prefix(CPU_FALLBACK_PREFIX)
}

/// Is a CPU fallback's reason a memory failure of the GPU load.
#[must_use]
pub fn is_out_of_memory(reason: &str) -> bool {
    reason.to_lowercase().contains("out of memory")
}

// The status chip of the settings window, one text per state.
pub const HEALTH_RUNNING: &str = "Running";
pub const HEALTH_NOT_RUNNING: &str = "Not running";
pub const HEALTH_NOT_ANSWERING: &str = "Not answering";

/// Chip text of an [`Attention`](crate::ipc::Attention): a few words, the state and not the fix.
#[must_use]
pub const fn attention_chip(attention: &Attention) -> &'static str {
    match attention {
        Attention::NoModel => "No model",
        Attention::ModelDamaged => "Model damaged",
        Attention::NoMicrophone => "No microphone",
        Attention::CpuFallback { .. } => "Running on CPU",
        Attention::OutOfMemory { cpu: true } => "Out of memory",
        Attention::OutOfMemory { cpu: false } => "GPU out of memory",
        Attention::SoundFailed { .. } => "Sound failed",
    }
}

/// The chip's tooltip: what happened and where to fix it.
#[must_use]
pub fn attention_text(attention: &Attention) -> String {
    match attention {
        Attention::NoModel => "No speech model is downloaded. Open Model to download one".into(),
        Attention::ModelDamaged => {
            "The speech model file is damaged. Download it again in Model".into()
        }
        Attention::NoMicrophone => {
            "The chosen microphone is not connected. Pick another in Audio".into()
        }
        Attention::CpuFallback { reason } => {
            format!("The GPU is not used, so dictation is slower ({reason}). See Model")
        }
        Attention::OutOfMemory { cpu: true } => {
            "Not enough memory to load the model. Close other programs or pick a smaller model in Model"
                .into()
        }
        Attention::OutOfMemory { cpu: false } => {
            "The GPU ran out of memory loading the model, so it runs on the CPU. Pick a smaller model in Model"
                .into()
        }
        Attention::SoundFailed { line, .. } => format!("A sound did not play ({line}). See Audio"),
    }
}

/// Longest overlay label, in characters: the pill is 168 DIP wide, about 16 characters at 12 px.
pub const OVERLAY_LABEL_MAX: usize = 16;

/// Short form of a [`Notice`] for the overlay pill; [`notice`] has the full text.
#[must_use]
pub const fn notice_short(notice: Notice) -> &'static str {
    match notice {
        Notice::Cancelled => "Cancelled",
        Notice::CancelledKept => "Cancelled, kept",
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
        Notice::OutOfMemory => "Out of memory",
        Notice::HistoryOn => "History on",
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
        // The lock icon says "locked"; the label says how to end it (16 characters at most).
        OverlayState::Recording { locked: true, .. } => "Press to stop",
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

/// Banner of a config reload that failed; `line` is 1-based. Reads like the parser's own
/// `ConfigError::Syntax` ("config.toml line 3, column 11: ..."), which has the column too.
#[must_use]
pub fn config_error(file: &str, line: usize, message: &str) -> String {
    format!("{file} line {line}: {message}")
}

/// What follows the "newer version, read-only" banner: the way out.
pub const CONFIG_READ_ONLY_NEXT: &str = "Update OxiYapper to change settings here.";

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

/// The open microphone gives only exact zeros: muted, or behind a noise gate while nobody speaks.
#[must_use]
pub fn no_signal(name: &str) -> String {
    format!("No sound from {name} yet. Speak, check that it is not muted, or reconnect it.")
}

/// Status of the Audio page's Test button: record, then play back.
pub const MIC_TEST_RECORDING: &str = "Speak now: recording 3 s";
pub const MIC_TEST_PLAYING: &str = "Playing back";
/// The test recording or its playback failed; the error kind only, never audio.
#[must_use]
pub fn mic_test_failed(kind: &str) -> String {
    format!("Test failed: {kind}")
}
pub const SYSTEM_DEFAULT_MIC: &str = "System default";

pub const DOWNLOAD_NEEDS_DAEMON: &str = "Start OxiYapper to download.";
/// Apply's tooltip on the hotkey page while there is no daemon to report the pressed keys.
pub const HOTKEY_NEEDS_DAEMON: &str = "Pressed keys are seen only while OxiYapper runs";

/// The daemon's reason for a failed model download, under the model's row.
#[must_use]
pub fn download_failed(message: &str) -> String {
    format!("Download failed: {message}")
}

/// Decimal gigabytes with one digit, the unit Hugging Face shows.
#[must_use]
pub fn gb(bytes: u64) -> String {
    format!("{} GB", gb_digits(bytes))
}

fn gb_digits(bytes: u64) -> String {
    #[expect(clippy::cast_precision_loss, reason = "one decimal digit is shown")]
    let gb = bytes as f64 / 1e9;
    format!("{gb:.1}")
}

/// Progress label beside a model's download bar: "0.7 of 1.6 GB". Under 0.05 GB the GB digit
/// reads 0.0, as if nothing were kept, so a started download says its MB: "12 MB of 3.1 GB".
#[must_use]
pub fn download_progress(done: u64, total: u64) -> String {
    if done > 0 && done < 50_000_000 {
        return format!("{} MB of {}", done.div_ceil(1_000_000), gb(total));
    }
    format!("{} of {}", gb_digits(done), gb(total))
}

/// What to do about a registry model in a state its badge only names; empty when the badge says
/// it all (the row then shows the model's hint).
#[must_use]
pub const fn model_status(status: ModelStatus) -> &'static str {
    match status {
        ModelStatus::Missing | ModelStatus::Ready => "",
        ModelStatus::Partial => "resumes where it stopped",
        ModelStatus::NeedsHash => "checked before first use",
        ModelStatus::Bad => "download it again",
    }
}

/// Short state of a registry model, for its badge; [`model_status`] says what to do.
#[must_use]
pub const fn model_badge(status: ModelStatus) -> &'static str {
    match status {
        ModelStatus::Missing => "Not downloaded",
        ModelStatus::Partial => "Paused",
        ModelStatus::Ready => "Ready",
        ModelStatus::NeedsHash => "Downloaded",
        ModelStatus::Bad => "Damaged",
    }
}

pub const MODEL_BADGE_DOWNLOADING: &str = "Downloading";

/// Which curated model to pick, in a few words; empty for one with no advice.
#[must_use]
pub fn model_hint(id: &str) -> &'static str {
    match id {
        "turbo" => "fast, good for most",
        "large-v3" => "slower, more accurate",
        "podlodka" => "tuned for Russian",
        _ => "",
    }
}

/// Status under the replacements editor; `lines` are 1-based. Empty when every line parses.
#[must_use]
pub fn replace_bad_lines(lines: &[usize]) -> String {
    let numbers = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    match lines {
        [] => String::new(),
        [_] => format!("Line {numbers} has no =>: ignored"),
        _ => format!("Lines {numbers} have no =>: ignored"),
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
            download_progress(650_000_000, 1_624_555_275),
            "0.7 of 1.6 GB"
        );
        assert_eq!(download_progress(0, 1_624_555_275), "0.0 of 1.6 GB");
        assert_eq!(download_progress(1, 3_100_000_000), "1 MB of 3.1 GB");
        assert_eq!(
            download_progress(12_300_000, 3_100_000_000),
            "13 MB of 3.1 GB"
        );
        assert_eq!(
            download_progress(50_000_000, 3_100_000_000),
            "0.1 of 3.1 GB"
        );
        assert_eq!(
            spawn_failed(std::io::ErrorKind::NotFound),
            "Could not start OxiYapper: entity not found."
        );
        assert_eq!(
            config_error("config.toml", 3, "bad"),
            "config.toml line 3: bad"
        );
        assert_eq!(not_connected("USB Mic"), "Not connected: USB Mic");
        assert_eq!(
            no_signal("USB Mic"),
            "No sound from USB Mic yet. Speak, check that it is not muted, or reconnect it."
        );
        assert_eq!(
            mic_test_failed("no output device"),
            "Test failed: no output device"
        );
        assert_eq!(download_failed("reset"), "Download failed: reset");
    }

    #[test]
    fn a_model_badge_and_its_caption_never_say_the_same_thing() {
        for status in [
            ModelStatus::Missing,
            ModelStatus::Partial,
            ModelStatus::Ready,
            ModelStatus::NeedsHash,
            ModelStatus::Bad,
        ] {
            let badge = model_badge(status).to_lowercase();
            let caption = model_status(status).to_lowercase();
            let first = badge.split(' ').next().unwrap();
            assert!(!caption.contains(first), "{status:?}: {badge} / {caption}");
            assert!(!caption.contains(&badge), "{status:?}: {badge} / {caption}");
        }
        assert_eq!(
            model_status(ModelStatus::Partial),
            "resumes where it stopped"
        );
    }

    #[test]
    fn engine_lines_read_back() {
        let line = engine_warning(&EngineWarning::CpuFallback {
            reason: "out of memory".into(),
        });
        assert_eq!(line, "running on CPU: out of memory");
        assert_eq!(cpu_fallback_reason(&line), Some("out of memory"));
        assert!(is_out_of_memory("Vulkan: Out of Memory"));
        let gpu = engine_warning(&EngineWarning::GpuNotFound { name: "x".into() });
        assert_eq!(cpu_fallback_reason(&gpu), None);
    }

    #[test]
    fn a_chip_text_is_short_and_the_tooltip_names_the_fix() {
        for a in [
            Attention::NoModel,
            Attention::ModelDamaged,
            Attention::NoMicrophone,
            Attention::CpuFallback { reason: "x".into() },
            Attention::OutOfMemory { cpu: true },
            Attention::OutOfMemory { cpu: false },
            Attention::SoundFailed {
                line: "x".into(),
                stop: false,
            },
        ] {
            assert!(attention_chip(&a).len() <= 20, "{a:?}");
            assert!(attention_text(&a).len() > attention_chip(&a).len(), "{a:?}");
        }
        assert_ne!(
            attention_text(&Attention::OutOfMemory { cpu: true }),
            attention_text(&Attention::OutOfMemory { cpu: false })
        );
    }

    #[test]
    fn replace_bad_lines_counts_its_lines() {
        assert_eq!(replace_bad_lines(&[]), "");
        assert_eq!(replace_bad_lines(&[2]), "Line 2 has no =>: ignored");
        assert_eq!(replace_bad_lines(&[2, 5]), "Lines 2, 5 have no =>: ignored");
    }
}
