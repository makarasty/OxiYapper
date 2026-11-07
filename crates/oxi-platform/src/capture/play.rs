//! A short mono clip on the default output: the Audio page's microphone test plays back what it
//! recorded. The clip stays in memory only.

use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, OutputCallbackInfo, SampleFormat, SizedSample, Stream};
use oxi_core::audio::resample;

use super::CaptureError;

/// A clip playing; dropping it stops the sound. The caller knows the clip length.
pub struct Playback(#[expect(dead_code, reason = "held only to keep the stream playing")] Stream);

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
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build::<f32>(&dev, config, src),
        SampleFormat::F64 => build::<f64>(&dev, config, src),
        SampleFormat::I8 => build::<i8>(&dev, config, src),
        SampleFormat::I16 => build::<i16>(&dev, config, src),
        SampleFormat::I32 => build::<i32>(&dev, config, src),
        SampleFormat::U8 => build::<u8>(&dev, config, src),
        SampleFormat::U16 => build::<u16>(&dev, config, src),
        SampleFormat::U32 => build::<u32>(&dev, config, src),
        other => return Err(CaptureError::UnsupportedFormat(other.to_string())),
    }?;
    stream.play()?;
    Ok(Playback(stream))
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
) -> Result<Stream, CaptureError> {
    Ok(dev.build_output_stream(
        config,
        move |out: &mut [T], _: &OutputCallbackInfo| src.fill(out),
        // shortcut: an output error mid-clip only ends the sound early; the test is a few seconds.
        |_| {},
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
