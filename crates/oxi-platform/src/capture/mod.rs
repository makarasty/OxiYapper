//! Microphone capture through cpal into a lock-free ring (ARCHITECTURE section 9, steps 1-2).
//!
//! The cpal callback only converts samples to f32, pushes them into a preallocated ring and
//! updates the level atomic. A drain thread downmixes and feeds [`oxi_core::audio::Pipeline`]
//! (DSP lives in `oxi-core`, not here).

mod stream;

use oxi_core::audio::AudioError;
use oxi_core::config::Audio;
use thiserror::Error;

pub use stream::{CpalCapture, list_input_devices};

/// Something the daemon must react to while capture runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureEvent {
    /// The device went away mid-stream (USB unplug). What was captured is kept; `finish` still works.
    DeviceLost,
}

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("no input device")]
    NoDevice,
    /// The OS privacy setting (or macOS TCC) blocks the microphone.
    #[error("microphone permission denied")]
    PermissionDenied,
    #[error("unsupported sample format {0}")]
    UnsupportedFormat(String),
    #[error("audio device: {0}")]
    Device(cpal::Error),
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
    /// Open `device` by name; an empty name, or a name that is not found, opens the system default
    /// (see [`Capture::fell_back_to_default`]). `on_event` runs on a cpal thread.
    fn open(
        device: &str,
        opts: &Audio,
        on_event: impl FnMut(CaptureEvent) + Send + 'static,
    ) -> Result<Self, CaptureError>;

    /// Native sample rate of the device.
    fn input_rate(&self) -> u32;

    /// Peak (linear, before gain) of the latest callback block, for the meter.
    fn peak(&self) -> f32;

    /// Interleaved samples dropped because the ring was full (drain starved).
    fn dropped_samples(&self) -> u64;

    /// True when a configured device name was not found and the default was opened instead.
    fn fell_back_to_default(&self) -> bool;

    /// Stop the stream and return the processed 16 kHz mono audio, also after a lost device.
    fn finish(self) -> Result<Vec<f32>, CaptureError>;
}
