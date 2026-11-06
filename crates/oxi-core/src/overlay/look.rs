//! What each overlay state looks like. One palette for overlay, tray and settings.

use crate::machine::{Notice, OverlayState};
use crate::strings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

pub mod palette {
    use super::Rgba;

    pub const NONE: Rgba = Rgba(0, 0, 0, 0);
    /// Recording and its meter: calm teal, so [`CLIP`] stands out.
    pub const RECORDING: Rgba = Rgba(0x2B, 0xB5, 0xA0, 0xFF);
    pub const BUSY: Rgba = Rgba(0x3E, 0x8E, 0xF7, 0xFF);
    pub const OK: Rgba = Rgba(0x30, 0xA4, 0x6C, 0xFF);
    pub const INFO: Rgba = Rgba(0x8B, 0x8D, 0x98, 0xFF);
    pub const WARN: Rgba = Rgba(0xF7, 0xB0, 0x1B, 0xFF);
    pub const ERROR: Rgba = Rgba(0xE5, 0x48, 0x4D, 0xFF);
    /// The meter while the input clips.
    pub const CLIP: Rgba = Rgba(0xFF, 0x2A, 0x2A, 0xFF);
    /// The pill body: opaque near-black, readable over light and dark desktops alike.
    pub const SURFACE: Rgba = Rgba(0x1E, 0x1F, 0x24, 0xFF);
    /// Pill label text.
    pub const TEXT: Rgba = Rgba(0xEE, 0xEF, 0xF2, 0xFF);
    /// Hairline pill rim that keeps the outline visible on a dark desktop.
    pub const EDGE: Rgba = Rgba(0xFF, 0xFF, 0xFF, 0x24);
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
        Notice::Cancelled | Notice::NothingHeard => (palette::INFO, Icon::Info),
        Notice::PressPaste | Notice::AdminWindow => (palette::WARN, Icon::Paste),
        Notice::RestoreSkipped | Notice::MicFallback | Notice::ConfigAdjusted => {
            (palette::WARN, Icon::Warning)
        }
        Notice::NoModel
        | Notice::ModelBad
        | Notice::DeviceLost
        | Notice::CaptureFailed
        | Notice::TranscriptionFailed
        | Notice::PasteFailed
        | Notice::ConfigInvalid => (palette::ERROR, Icon::Warning),
    }
}
