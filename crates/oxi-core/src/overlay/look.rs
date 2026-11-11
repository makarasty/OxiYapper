//! What each overlay state looks like. One palette for overlay, tray and settings.

use crate::machine::{Notice, OverlayState};
use crate::strings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

pub mod palette {
    use super::Rgba;

    pub const NONE: Rgba = Rgba(0, 0, 0, 0);
    /// Recording and its meter, the record dot of the mark: red means "recording now" and
    /// nothing else (brand concept C, settings token `recording`).
    pub const RECORDING: Rgba = Rgba(0xE5, 0x48, 0x4D, 0xFF);
    /// Loading and transcribing: calm grey (10.2:1 on [`SURFACE`]); the hourglass carries the
    /// meaning, so red stays the one recording signal.
    pub const BUSY: Rgba = Rgba(0xC8, 0xCC, 0xD4, 0xFF);
    pub const OK: Rgba = Rgba(0x30, 0xA4, 0x6C, 0xFF);
    pub const INFO: Rgba = Rgba(0x8B, 0x8D, 0x98, 0xFF);
    pub const WARN: Rgba = Rgba(0xF7, 0xB0, 0x1B, 0xFF);
    /// Errors (pill notices and the tray error badge): deep orange, since red means recording.
    /// 3.43:1 on a #EEEEEE light taskbar, 4.10 on #202020 dark, 4.13 on [`SURFACE`].
    pub const ERROR: Rgba = Rgba(0xC8, 0x64, 0x00, 0xFF);
    /// The meter while the input clips: white, "blown out", since red is the normal meter.
    pub const CLIP: Rgba = Rgba(0xFF, 0xFF, 0xFF, 0xFF);
    /// The pill body: opaque near-black, readable over light and dark desktops alike.
    pub const SURFACE: Rgba = Rgba(0x1E, 0x1F, 0x24, 0xFF);
    /// Pill label text.
    pub const TEXT: Rgba = Rgba(0xEE, 0xEF, 0xF2, 0xFF);
    /// Hairline pill rim that keeps the outline visible on a dark desktop (3:1 over #202020).
    pub const EDGE: Rgba = Rgba(0xFF, 0xFF, 0xFF, 0x66);
    /// The app icon tile: paper, with a hairline edge that holds it on a light background.
    pub const PAPER: Rgba = Rgba(0xFA, 0xF7, 0xF1, 0xFF);
    pub const PAPER_EDGE: Rgba = Rgba(0xE4, 0xDE, 0xD2, 0xFF);
    /// The ring of the app icon's mark: the light theme's ink (settings token `text-primary`).
    pub const MARK: Rgba = Rgba(0x1C, 0x1B, 0x19, 0xFF);
    /// Dark rim of the recording tray icon on a light taskbar.
    pub const RECORDING_RIM: Rgba = Rgba(0x8F, 0x1D, 0x22, 0xFF);
    /// The update badge of the tray icon: green, so red stays the recording signal.
    pub const UPDATE: Rgba = Rgba(0x30, 0xA4, 0x6C, 0xFF);
    /// Tray glyphs on a light taskbar.
    pub const INK: Rgba = Rgba(0x1A, 0x1A, 0x1A, 0xFF);
    /// Tray glyphs on a dark or accent taskbar; the hole of the recording tray icon.
    pub const WHITE: Rgba = Rgba(0xFF, 0xFF, 0xFF, 0xFF);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    None,
    Mic,
    Lock,
    Hourglass,
    Paste,
    Info,
    Warning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Look {
    pub accent: Rgba,
    pub label: &'static str,
    pub icon: Icon,
    /// Draw the level meter.
    pub meter: bool,
}

#[must_use]
pub fn look(state: OverlayState) -> Look {
    let label = strings::overlay_label(state);
    let (accent, icon, meter) = match state {
        OverlayState::Hidden => (palette::NONE, Icon::None, false),
        OverlayState::Recording {
            locked,
            ending_soon,
        } => {
            let accent = if ending_soon {
                palette::WARN
            } else {
                palette::RECORDING
            };
            (accent, if locked { Icon::Lock } else { Icon::Mic }, true)
        }
        OverlayState::LoadingModel | OverlayState::Transcribing => {
            (palette::BUSY, Icon::Hourglass, false)
        }
        OverlayState::Pasting => (palette::OK, Icon::Paste, false),
        OverlayState::Text(notice) => {
            let (accent, icon) = notice_look(notice);
            (accent, icon, false)
        }
    };
    Look {
        accent,
        label,
        icon,
        meter,
    }
}

fn notice_look(notice: Notice) -> (Rgba, Icon) {
    match notice {
        Notice::Cancelled
        | Notice::CancelledKept
        | Notice::NothingHeard
        | Notice::HistoryOn
        | Notice::FailedKept
        | Notice::Recovered => (palette::INFO, Icon::Info),
        Notice::PressPaste | Notice::AdminWindow => (palette::WARN, Icon::Paste),
        Notice::RestoreSkipped
        | Notice::MicFallback
        | Notice::MicSilence
        | Notice::ConfigAdjusted => (palette::WARN, Icon::Warning),
        Notice::NoModel
        | Notice::ModelBad
        | Notice::OutOfMemory
        | Notice::DeviceLost
        | Notice::CaptureFailed
        | Notice::TranscriptionFailed
        | Notice::PasteFailed
        | Notice::ConfigInvalid => (palette::ERROR, Icon::Warning),
    }
}
