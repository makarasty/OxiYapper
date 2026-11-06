use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, InputCallbackInfo, Sample, SampleFormat, SizedSample, Stream};
use oxi_core::audio::{AudioError, Pipeline, downmix};
use oxi_core::config::Audio;
use rtrb::{Consumer, Producer, RingBuffer};

use super::{Capture, CaptureError, CaptureEvent};

/// Ring length: the drain thread polls every [`DRAIN_POLL`], this covers a long stall.
const RING_SECONDS: usize = 2;
const DRAIN_POLL: Duration = Duration::from_millis(10);

/// State shared by the cpal callbacks and the drain thread. Only atomics: no lock on the audio thread.
#[derive(Default)]
struct Shared {
    /// `f32` bits of the latest block's peak.
    peak: AtomicU32,
    dropped: AtomicU64,
    lost: AtomicBool,
    stop: AtomicBool,
}

/// Names of the input devices, for the settings list.
pub fn list_input_devices() -> Result<Vec<String>, CaptureError> {
    let devices = cpal::default_host().input_devices()?;
    Ok(devices
        .filter_map(|d| d.description().ok().map(|d| d.name().to_owned()))
        .collect())
}

/// cpal implementation of [`Capture`] (one for all OSes).
pub struct CpalCapture {
    /// Dropped first in [`Capture::finish`]: no callback runs after that.
    stream: Stream,
    shared: Arc<Shared>,
    drain: JoinHandle<Result<Vec<f32>, AudioError>>,
    rate: u32,
    fell_back: bool,
}

impl Capture for CpalCapture {
    fn open(
        device: &str,
        opts: &Audio,
        mut on_event: impl FnMut(CaptureEvent) + Send + 'static,
    ) -> Result<Self, CaptureError> {
        let host = cpal::default_host();
        let named = if device.is_empty() {
            None
        } else {
            host.input_devices()?
                .find(|d| d.description().is_ok_and(|d| d.name() == device))
        };
        let fell_back = !device.is_empty() && named.is_none();
        let dev = named
            .or_else(|| host.default_input_device())
            .ok_or(CaptureError::NoDevice)?;

        let supported = dev.default_input_config()?;
        let (channels, rate, format) = (
            usize::from(supported.channels()),
            supported.sample_rate(),
            supported.sample_format(),
        );
        let pipeline = Pipeline::new(rate, opts)?;
        let (tx, rx) = RingBuffer::new(rate as usize * channels * RING_SECONDS);
        let shared = Arc::new(Shared::default());

        let err_shared = Arc::clone(&shared);
        let on_error = move |e: cpal::Error| {
            // Other kinds (xrun, rerouted default) leave the stream running.
            if matches!(
                e.kind(),
                cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated
            ) && !err_shared.lost.swap(true, Ordering::AcqRel)
            {
                on_event(CaptureEvent::DeviceLost);
            }
        };

        let config = supported.config();
        let cb_shared = Arc::clone(&shared);
        let stream = match format {
            SampleFormat::F32 => build::<f32>(&dev, config, tx, channels, cb_shared, on_error),
            SampleFormat::F64 => build::<f64>(&dev, config, tx, channels, cb_shared, on_error),
            SampleFormat::I8 => build::<i8>(&dev, config, tx, channels, cb_shared, on_error),
            SampleFormat::I16 => build::<i16>(&dev, config, tx, channels, cb_shared, on_error),
            SampleFormat::I32 => build::<i32>(&dev, config, tx, channels, cb_shared, on_error),
            SampleFormat::U8 => build::<u8>(&dev, config, tx, channels, cb_shared, on_error),
            SampleFormat::U16 => build::<u16>(&dev, config, tx, channels, cb_shared, on_error),
            other => return Err(CaptureError::UnsupportedFormat(other.to_string())),
        }?;

        let drain_shared = Arc::clone(&shared);
        let drain = thread::Builder::new()
            .name("oxi-capture-drain".into())
            .spawn(move || drain_loop(rx, channels, pipeline, &drain_shared))
            .map_err(|_| AudioError::Resample("drain thread did not start".into()))?;
        stream.play()?;
        Ok(Self {
            stream,
            shared,
            drain,
            rate,
            fell_back,
        })
    }

    fn input_rate(&self) -> u32 {
        self.rate
    }

    fn peak(&self) -> f32 {
        f32::from_bits(self.shared.peak.load(Ordering::Relaxed))
    }

    fn dropped_samples(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    fn fell_back_to_default(&self) -> bool {
        self.fell_back
    }

    fn finish(self) -> Result<Vec<f32>, CaptureError> {
        drop(self.stream);
        self.shared.stop.store(true, Ordering::Release);
        self.drain.thread().unpark();
        match self.drain.join() {
            Ok(result) => Ok(result?),
            Err(_) => Err(AudioError::Resample("drain thread panicked".into()).into()),
        }
    }
}

fn build<T>(
    dev: &cpal::Device,
    config: cpal::StreamConfig,
    mut tx: Producer<f32>,
    channels: usize,
    shared: Arc<Shared>,
    on_error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<Stream, CaptureError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    Ok(dev.build_input_stream(
        config,
        move |data: &[T], _: &InputCallbackInfo| push_converted(&mut tx, data, channels, &shared),
        on_error,
        None,
    )?)
}

/// The whole cpal callback: convert to f32, push whole frames into the ring, publish the peak.
/// Counts what does not fit as dropped. No allocation, no lock.
fn push_converted<T>(tx: &mut Producer<f32>, data: &[T], channels: usize, shared: &Shared)
where
    T: Sample,
    f32: FromSample<T>,
{
    let fits = tx.slots().min(data.len()) / channels * channels;
    let mut peak = 0.0_f32;
    if let Ok(chunk) = tx.write_chunk_uninit(fits) {
        chunk.fill_from_iter(data[..fits].iter().map(|s| {
            let v: f32 = s.to_sample();
            peak = peak.max(v.abs());
            v
        }));
    }
    shared.peak.store(peak.to_bits(), Ordering::Relaxed);
    if fits < data.len() {
        shared
            .dropped
            .fetch_add((data.len() - fits) as u64, Ordering::Relaxed);
    }
}

/// Moves the ring into `pipeline` until stopped, then drains once more so nothing captured is lost.
fn drain_loop(
    mut rx: Consumer<f32>,
    channels: usize,
    mut pipeline: Pipeline,
    shared: &Shared,
) -> Result<Vec<f32>, AudioError> {
    let mut failed = None;
    loop {
        // Read before draining: everything pushed before `stop` is seen by the last pass.
        let stopping = shared.stop.load(Ordering::Acquire);
        let available = rx.slots();
        if available > 0
            && let Ok(chunk) = rx.read_chunk(available)
        {
            let (a, b) = chunk.as_slices();
            let interleaved = [a, b].concat();
            chunk.commit_all();
            if failed.is_none() {
                failed = pipeline.push(&downmix(&interleaved, channels)).err();
            }
        }
        if stopping {
            break;
        }
        thread::park_timeout(DRAIN_POLL);
    }
    match failed {
        Some(e) => Err(e),
        None => pipeline.finish(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run<T: Sample>(data: &[T], channels: usize, ring: usize) -> (Vec<f32>, Shared)
    where
        f32: FromSample<T>,
    {
        let (mut tx, mut rx) = RingBuffer::new(ring);
        let shared = Shared::default();
        push_converted(&mut tx, data, channels, &shared);
        let out = rx
            .read_chunk(rx.slots())
            .map(|c| {
                let v = c.as_slices().0.to_vec();
                c.commit_all();
                v
            })
            .unwrap_or_default();
        (out, shared)
    }

    fn peak(shared: &Shared) -> f32 {
        f32::from_bits(shared.peak.load(Ordering::Relaxed))
    }

    #[test]
    fn i16_scales_to_unit_range() {
        let (out, shared) = run(&[0_i16, i16::MIN, 16_384], 1, 8);
        assert_eq!(out, [0.0, -1.0, 0.5]);
        assert_eq!(peak(&shared), 1.0);
    }

    #[test]
    fn u16_origin_is_the_middle() {
        let (out, _) = run(&[32_768_u16, 0, 49_152], 1, 8);
        assert_eq!(out, [0.0, -1.0, 0.5]);
    }

    #[test]
    fn f32_passes_through() {
        let (out, shared) = run(&[0.25_f32, -0.75], 2, 8);
        assert_eq!(out, [0.25, -0.75]);
        assert_eq!(peak(&shared), 0.75);
    }

    #[test]
    fn full_ring_drops_whole_frames_and_counts() {
        // Room for 5 samples, stereo: 2 frames (4 samples) fit, 4 of 8 are dropped.
        let (out, shared) = run(&[0.5_f32; 8], 2, 5);
        assert_eq!(out.len(), 4);
        assert_eq!(shared.dropped.load(Ordering::Relaxed), 4);
    }
}
