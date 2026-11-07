//! Microphone capture through cpal into a lock-free ring (ARCHITECTURE section 9, steps 1-2).
//!
//! The cpal callback only converts samples to f32 and pushes them into a preallocated ring. A drain
//! thread downmixes, feeds [`oxi_core::audio::Pipeline`] in 20 ms windows (DSP lives in `oxi-core`,
//! not here) and publishes each window's post-gain level to the [`LevelMeter`].

mod play;
mod stream;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use oxi_core::audio::{AudioError, Level};
use oxi_core::config::Audio;
use thiserror::Error;

pub use play::{Playback, play};
pub use stream::{CpalCapture, list_input_devices};

/// Post-gain peak and RMS of the last 20 ms window, for the overlay meter (ARCHITECTURE section 2).
/// Clones share one value; reading is one atomic load, so the UI thread polls it lock-free.
/// Zero while no capture runs.
#[derive(Debug, Clone, Default)]
pub struct LevelMeter(Arc<AtomicU64>);

impl LevelMeter {
    #[must_use]
    pub fn read(&self) -> Level {
        // One word for both: a reader never sees the peak of one window with the RMS of another.
        let bits = self.0.load(Ordering::Relaxed);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the low 32 bits are the RMS"
        )]
        let rms = f32::from_bits(bits as u32);
        Level {
            peak: f32::from_bits((bits >> 32) as u32),
            rms,
        }
    }

    fn store(&self, level: Level) {
        let bits = u64::from(level.peak.to_bits()) << 32 | u64::from(level.rms.to_bits());
        self.0.store(bits, Ordering::Relaxed);
    }

    fn clear(&self) {
        self.0.store(0, Ordering::Relaxed);
    }
}

/// Something the daemon must react to while capture runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureEvent {
    /// The device went away mid-stream (USB unplug). What was captured is kept; `finish` still works.
    DeviceLost,
    /// Processing failed mid-recording (sent once). The meter reads zero, the ring keeps being
    /// emptied without processing, and `finish` returns the error.
    Failed,
}

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("no input device")]
    NoDevice,
    #[error("no output device")]
    NoOutput,
    /// The OS privacy setting (or macOS TCC) blocks the microphone.
    #[error("microphone permission denied")]
    PermissionDenied,
    #[error("unsupported sample format {0}")]
    UnsupportedFormat(String),
    #[error("audio device")]
    Device(#[source] cpal::Error),
    /// The drain thread did not start or panicked.
    #[error("capture thread")]
    Thread(#[source] std::io::Error),
    #[error(transparent)]
    Audio(#[from] AudioError),
}

impl From<cpal::Error> for CaptureError {
    fn from(e: cpal::Error) -> Self {
        match e.kind() {
            cpal::ErrorKind::PermissionDenied => Self::PermissionDenied,
            _ => Self::Device(e),
        }
    }
}

/// One microphone recording: open on key-down, [`Capture::finish`] on key-up.
pub trait Capture: Sized {
    /// Open the device named `opts.device`; an empty name, or a name that is not found, opens the
    /// system default (see [`Capture::fell_back_to_default`]). `meter` gets the level while capture
    /// runs and is zeroed when it stops. `on_event` runs on a cpal thread.
    fn open(
        opts: &Audio,
        meter: LevelMeter,
        on_event: impl FnMut(CaptureEvent) + Send + 'static,
    ) -> Result<Self, CaptureError>;

    /// Native sample rate of the device.
    fn input_rate(&self) -> u32;

    /// Interleaved samples dropped because the ring was full (drain starved).
    fn dropped_samples(&self) -> u64;

    /// True when a configured device name was not found and the default was opened instead.
    fn fell_back_to_default(&self) -> bool;

    /// Stop the stream and return the processed 16 kHz mono audio, also after a lost device.
    ///
    /// Dropping without `finish` (Esc, an early `?`) stops the stream and discards the audio.
    fn finish(self) -> Result<Vec<f32>, CaptureError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_round_trips_both_values() {
        let meter = LevelMeter::default();
        let level = Level {
            peak: 0.75,
            rms: 0.125,
        };
        meter.clone().store(level);
        assert_eq!(meter.read(), level);
    }
}
