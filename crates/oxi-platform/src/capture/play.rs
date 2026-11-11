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

/// Clips being played at once on one output; their samples are summed and clipped to -1..1.
#[derive(Default)]
struct Mixer {
    voices: Vec<(Arc<[f32]>, usize)>,
}

impl Mixer {
    /// Fill one interleaved buffer: the voices summed on every channel, silence when none is left.
    /// A voice that ends in this buffer is dropped.
    fn fill<T: SizedSample + FromSample<f32>>(&mut self, out: &mut [T], channels: usize) {
        for frame in out.chunks_mut(channels) {
            let sum: f32 = self
                .voices
                .iter_mut()
                .filter_map(|(clip, pos)| {
                    let s = clip.get(*pos).copied();
                    *pos += 1;
                    s
                })
                .sum();
            frame.fill(T::from_sample(sum.clamp(-1.0, 1.0)));
        }
        self.voices.retain(|(clip, pos)| *pos < clip.len());
    }
}

/// The default output kept open: clips pushed with [`Output::play`] are mixed into one stream, so
/// a sound costs no device open. Dropping it closes the device.
pub struct Output {
    _stream: Stream,
    /// The device this stream was opened on, to notice when the system default moves.
    device: Option<cpal::DeviceId>,
    mixer: Arc<Mutex<Mixer>>,
    rate: u32,
    failed: Arc<Mutex<Option<String>>>,
    latency_us: Arc<AtomicU64>,
}

impl Output {
    /// Opens the default output; it plays silence until a clip is pushed.
    ///
    /// # Errors
    /// [`CaptureError::NoOutput`] without an output device; device errors.
    pub fn open() -> Result<Self, CaptureError> {
        let dev = cpal::default_host()
            .default_output_device()
            .ok_or(CaptureError::NoOutput)?;
        let supported = dev.default_output_config()?;
        let channels = usize::from(supported.channels());
        let config = supported.config();
        let mixer = Arc::new(Mutex::new(Mixer::default()));
        let failed = Arc::new(Mutex::new(None));
        let latency_us = Arc::new(AtomicU64::new(0));
        let shared = (&mixer, &failed, &latency_us);
        let stream = match supported.sample_format() {
            SampleFormat::F32 => build_mixed::<f32>(&dev, config, channels, shared),
            SampleFormat::F64 => build_mixed::<f64>(&dev, config, channels, shared),
            SampleFormat::I8 => build_mixed::<i8>(&dev, config, channels, shared),
            SampleFormat::I16 => build_mixed::<i16>(&dev, config, channels, shared),
            SampleFormat::I32 => build_mixed::<i32>(&dev, config, channels, shared),
            SampleFormat::U8 => build_mixed::<u8>(&dev, config, channels, shared),
            SampleFormat::U16 => build_mixed::<u16>(&dev, config, channels, shared),
            SampleFormat::U32 => build_mixed::<u32>(&dev, config, channels, shared),
            other => return Err(CaptureError::UnsupportedFormat(other.to_string())),
        }?;
        stream.play()?;
        Ok(Self {
            _stream: stream,
            device: dev.id().ok(),
            mixer,
            rate: supported.sample_rate(),
            failed,
            latency_us,
        })
    }

    /// The rate clips must have.
    #[must_use]
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Is this still the system's default output? `false` once the user (or a plugged headset)
    /// moved the default: the sound should follow it, so open again. A device or default that
    /// cannot be read counts as unchanged.
    #[must_use]
    pub fn is_default(&self) -> bool {
        let now = cpal::default_host()
            .default_output_device()
            .and_then(|d| d.id().ok());
        match (&self.device, now) {
            (Some(open), Some(now)) => *open == now,
            _ => true,
        }
    }

    /// Starts `clip` (mono, at [`Self::rate`]) now, over whatever still plays.
    pub fn play(&self, clip: Arc<[f32]>) {
        if let Ok(mut mixer) = self.mixer.lock() {
            mixer.voices.push((clip, 0));
        }
    }

    /// The longest time the device has reported between a buffer being written and it being heard.
    #[must_use]
    pub fn latency(&self) -> Duration {
        Duration::from_micros(self.latency_us.load(Ordering::Relaxed))
    }

    /// The stream error that stopped the output (a device that went away); the output is dead then.
    #[must_use]
    pub fn failure(&self) -> Option<String> {
        self.failed.lock().ok()?.clone()
    }
}

/// The mixer, the stream's first error and its latency, shared with the owner of the output.
type MixShared<'a> = (
    &'a Arc<Mutex<Mixer>>,
    &'a Arc<Mutex<Option<String>>>,
    &'a Arc<AtomicU64>,
);

fn build_mixed<T: SizedSample + FromSample<f32>>(
    dev: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    (mixer, failed, latency_us): MixShared<'_>,
) -> Result<Stream, CaptureError> {
    let (mixer, failed, latency_us) = (
        Arc::clone(mixer),
        Arc::clone(failed),
        Arc::clone(latency_us),
    );
    Ok(dev.build_output_stream(
        config,
        move |out: &mut [T], info: &OutputCallbackInfo| {
            if let Ok(mut mixer) = mixer.lock() {
                mixer.fill(out, channels);
            }
            let at = info.timestamp();
            let late = at.playback.duration_since(at.callback).as_micros();
            latency_us.fetch_max(u64::try_from(late).unwrap_or(u64::MAX), Ordering::Relaxed);
        },
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

    #[test]
    fn the_mixer_sums_overlapping_clips_clips_the_sum_and_drops_a_finished_one() {
        let mut mixer = Mixer::default();
        mixer.voices.push((vec![0.25, 0.25, 0.25].into(), 0));
        mixer.voices.push((vec![0.5, 0.75].into(), 0));
        let mut out = [0.0_f32; 6];
        mixer.fill(&mut out, 2);
        // Frame 0 adds up, frame 1 reaches 1.0 (clipped), frame 2 is the long clip alone.
        assert_eq!(out, [0.75, 0.75, 1.0, 1.0, 0.25, 0.25]);
        assert!(mixer.voices.is_empty(), "both clips are over");
        mixer.fill(&mut out, 2);
        assert_eq!(out, [0.0; 6], "silence with no voice");
    }

    #[test]
    fn a_clip_started_between_buffers_plays_from_its_start() {
        let mut mixer = Mixer::default();
        let mut out = [9.0_f32; 2];
        mixer.fill(&mut out, 1);
        assert_eq!(out, [0.0, 0.0]);
        mixer.voices.push((vec![0.5, 0.25, 0.125].into(), 0));
        mixer.fill(&mut out, 1);
        assert_eq!(out, [0.5, 0.25]);
        mixer.fill(&mut out, 1);
        assert_eq!(out, [0.125, 0.0]);
    }
}
