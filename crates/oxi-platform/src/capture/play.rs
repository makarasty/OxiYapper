//! A short mono clip on the default output: the Audio page's microphone test plays back what it
//! recorded. The clip stays in memory only.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, OutputCallbackInfo, SampleFormat, SizedSample, Stream};
use oxi_core::audio::resample;

use super::CaptureError;

/// A clip playing; dropping it stops the sound. The caller knows the clip length.
pub struct Playback {
    _stream: Stream,
    failed: Arc<Mutex<Option<String>>>,
    latency_us: Arc<AtomicU64>,
}

impl Playback {
    /// The longest time the device has reported between a buffer being written and it being
    /// heard (seconds over Bluetooth); zero until the first callback.
    #[must_use]
    pub fn latency(&self) -> Duration {
        Duration::from_micros(self.latency_us.load(Ordering::Relaxed))
    }

    /// The stream error that cut the sound short, if one came (a device that went away).
    #[must_use]
    pub fn failure(&self) -> Option<String> {
        self.failed.lock().ok()?.clone()
    }
}

/// Play mono `clip` at `rate` Hz on the default output, on every channel.
///
/// # Errors
/// [`CaptureError::NoOutput`] without an output device; device and resampler errors.
pub fn play(clip: &[f32], rate: u32) -> Result<Playback, CaptureError> {
    let dev = cpal::default_host()
        .default_output_device()
        .ok_or(CaptureError::NoOutput)?;
    let supported = dev.default_output_config()?;
    let src = Source {
        samples: resample(clip, rate, supported.sample_rate())?.into(),
        pos: 0,
        channels: usize::from(supported.channels()),
    };
    let config = supported.config();
    let failed = Arc::new(Mutex::new(None));
    let latency_us = Arc::new(AtomicU64::new(0));
    let shared = (&failed, &latency_us);
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build::<f32>(&dev, config, src, shared),
        SampleFormat::F64 => build::<f64>(&dev, config, src, shared),
        SampleFormat::I8 => build::<i8>(&dev, config, src, shared),
        SampleFormat::I16 => build::<i16>(&dev, config, src, shared),
        SampleFormat::I32 => build::<i32>(&dev, config, src, shared),
        SampleFormat::U8 => build::<u8>(&dev, config, src, shared),
        SampleFormat::U16 => build::<u16>(&dev, config, src, shared),
        SampleFormat::U32 => build::<u32>(&dev, config, src, shared),
        other => return Err(CaptureError::UnsupportedFormat(other.to_string())),
    }?;
    stream.play()?;
    Ok(Playback {
        _stream: stream,
        failed,
        latency_us,
    })
}

/// The clip and how far the output callback got.
struct Source {
    samples: Arc<[f32]>,
    pos: usize,
    channels: usize,
}

impl Source {
    /// Fill one interleaved buffer: each clip sample on every channel, silence after the end.
    fn fill<T: SizedSample + FromSample<f32>>(&mut self, out: &mut [T]) {
        for frame in out.chunks_mut(self.channels) {
            let s = self.samples.get(self.pos).copied().unwrap_or(0.0);
            self.pos += 1;
            frame.fill(T::from_sample(s));
        }
    }
}

fn build<T: SizedSample + FromSample<f32>>(
    dev: &cpal::Device,
    config: cpal::StreamConfig,
    mut src: Source,
    (failed, latency_us): (&Arc<Mutex<Option<String>>>, &Arc<AtomicU64>),
) -> Result<Stream, CaptureError> {
    let failed = Arc::clone(failed);
    let latency_us = Arc::clone(latency_us);
    Ok(dev.build_output_stream(
        config,
        move |out: &mut [T], info: &OutputCallbackInfo| {
            src.fill(out);
            let at = info.timestamp();
            let late = at.playback.duration_since(at.callback).as_micros();
            latency_us.fetch_max(u64::try_from(late).unwrap_or(u64::MAX), Ordering::Relaxed);
        },
        // The owner of the `Playback` reads it with `failure`; the first error is kept.
        move |e| {
            if let Ok(mut slot) = failed.lock() {
                slot.get_or_insert_with(|| e.to_string());
            }
        },
        None,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_copies_each_sample_to_every_channel_then_silence() {
        let mut src = Source {
            samples: vec![0.5, -0.5, 0.25].into(),
            pos: 0,
            channels: 2,
        };
        let mut out = [9_i16; 4];
        src.fill(&mut out);
        assert_eq!(out, [16_384, 16_384, -16_384, -16_384]);
        src.fill(&mut out);
        assert_eq!(out, [8_192, 8_192, 0, 0]);
    }
}
