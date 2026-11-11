//! What each overlay state looks like. One palette for overlay, tray and settings.

use crate::machine::{Notice, OverlayState};
use crate::strings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

pub mod palette {
    use super::Rgba;

    pub const NONE: Rgba = Rgba(0, 0, 0, 0);
    pub const OK: Rgba = Rgba(0x30, 0xA4, 0x6C, 0xFF);
    pub const INFO: Rgba = Rgba(0x8B, 0x8D, 0x98, 0xFF);
    pub const WARN: Rgba = Rgba(0xF7, 0xB0, 0x1B, 0xFF);
    /// Errors and an overloaded input (pill notices, the wave's clip line, the tray error
    /// badge): the dark theme's `error-text`, 8.10:1 on [`SURFACE`]. Never shown without its mark
    /// and words (DS-21.1); the accent may be red too.
    pub const ERROR: Rgba = Rgba(0xFF, 0x99, 0xA4, 0xFF);
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
    /// The update badge of the tray icon: green.
    pub const UPDATE: Rgba = Rgba(0x30, 0xA4, 0x6C, 0xFF);
    /// Tray glyphs on a light taskbar.
    pub const INK: Rgba = Rgba(0x1A, 0x1A, 0x1A, 0xFF);
    /// Tray glyphs on a dark or accent taskbar; the hole of the recording tray icon.
    pub const WHITE: Rgba = Rgba(0xFF, 0xFF, 0xFF, 0xFF);
}

/// The pill's label while the input is overloaded (DS-21.3).
pub const TOO_LOUD: &str = "Too loud";

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

/// [`look_with`] for the default (Red) accent.
#[must_use]
pub fn look(state: OverlayState) -> Look {
    look_with(state, super::pill_accent(super::PRESETS[0].seed))
}

/// What `state` looks like when the live work (recording, transcribing, pasting) shows `accent`,
/// the user's [`pill_accent`](super::pill_accent) (DS-21).
#[must_use]
pub fn look_with(state: OverlayState, accent: Rgba) -> Look {
    let label = strings::overlay_label(state);
    let (accent, icon, meter) = match state {
        OverlayState::Hidden => (palette::NONE, Icon::None, false),
        OverlayState::Recording {
            locked,
            ending_soon,
        } => {
            let accent = if ending_soon { palette::WARN } else { accent };
            (accent, if locked { Icon::Lock } else { Icon::Mic }, true)
        }
        OverlayState::LoadingModel | OverlayState::Transcribing => (accent, Icon::Hourglass, false),
        OverlayState::Pasting => (accent, Icon::Paste, false),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// DS-21: live work shows the user's accent; warning and error keep their own colours.
    #[test]
    fn working_states_take_the_accent_and_the_rest_keep_theirs() {
        let a = Rgba(0x5B, 0x5B, 0xD6, 0xFF);
        let rec = |ending_soon| OverlayState::Recording {
            locked: false,
            ending_soon,
        };
        for s in [
            rec(false),
            OverlayState::LoadingModel,
            OverlayState::Transcribing,
            OverlayState::Pasting,
        ] {
            assert_eq!(look_with(s, a).accent, a, "{s:?}");
        }
        assert_eq!(look_with(rec(true), a).accent, palette::WARN);
        assert_eq!(
            look_with(OverlayState::Text(Notice::NoModel), a).accent,
            palette::ERROR
        );
        assert_eq!(
            look_with(OverlayState::Text(Notice::MicFallback), a).accent,
            palette::WARN
        );
    }
}
