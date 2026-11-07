use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, I24, InputCallbackInfo, Sample, SampleFormat, SizedSample, Stream};
use oxi_core::audio::{AudioError, CHUNK, Level, Pipeline, downmix, samples_for};
use oxi_core::config::Audio;
use rtrb::{Consumer, Producer, RingBuffer};

use super::{Capture, CaptureError, CaptureEvent, LevelMeter};

/// Ring length: the drain thread polls every [`DRAIN_POLL`], this covers a long stall.
const RING_SECONDS: usize = 2;
const DRAIN_POLL: Duration = Duration::from_millis(10);

/// State shared by the cpal callbacks and the drain thread. Only atomics: no lock on the audio thread.
#[derive(Default)]
struct Shared {
    dropped: AtomicU64,
    lost: AtomicBool,
    stop: AtomicBool,
    /// Set before `stop` on cancel: the drain exits without processing what it holds.
    cancel: AtomicBool,
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
    /// Dropped first in [`CpalCapture::stop`]: no callback runs after that.
    stream: Option<Stream>,
    shared: Arc<Shared>,
    /// Taken by the first [`CpalCapture::stop`], from `finish` or `Drop`.
    drain: Option<JoinHandle<Result<Vec<f32>, AudioError>>>,
    meter: LevelMeter,
    rate: u32,
    fell_back: bool,
    name: String,
}

impl CpalCapture {
    /// Name of the device that was opened (the default's own name when none was configured).
    #[must_use]
    pub fn device_name(&self) -> &str {
        &self.name
    }

    /// Stop the callbacks, then the drain thread, and zero the meter; idempotent. With `cancel`
    /// the drain skips the pipeline flush and returns nothing.
    fn stop(&mut self, cancel: bool) -> Result<Vec<f32>, CaptureError> {
        drop(self.stream.take());
        self.shared.cancel.store(cancel, Ordering::Relaxed);
        // Release also publishes `cancel` to the drain's Acquire load of `stop`.
        self.shared.stop.store(true, Ordering::Release);
        let Some(drain) = self.drain.take() else {
            // Only `Drop` after `finish` gets here; nothing is left to return.
            return Ok(Vec::new());
        };
        drain.thread().unpark();
        let joined = drain.join();
        self.meter.clear();
        match joined {
            Ok(result) => Ok(result?),
            Err(_) => Err(CaptureError::Thread(std::io::Error::other(
                "drain thread panicked",
            ))),
        }
    }
}

impl Drop for CpalCapture {
    fn drop(&mut self) {
        // Cancel path: the audio is discarded unprocessed, so there is no error to report.
        let _ = self.stop(true);
    }
}

impl Capture for CpalCapture {
    fn open(
        opts: &Audio,
        meter: LevelMeter,
        on_event: impl FnMut(CaptureEvent) + Send + 'static,
    ) -> Result<Self, CaptureError> {
        let device = opts.device.as_str();
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
        let name = dev
            .description()
            .map(|d| d.name().to_owned())
            .unwrap_or_default();

        let supported = dev.default_input_config()?;
        let (channels, rate, format) = (
            usize::from(supported.channels()),
            supported.sample_rate(),
            supported.sample_format(),
        );
        let pipeline = Pipeline::new(rate, opts)?;
        let (tx, rx) = RingBuffer::new(rate as usize * channels * RING_SECONDS);
        let shared = Arc::new(Shared::default());
        // Two senders (cpal error thread, drain thread), both off the data callback.
        let on_event = Arc::new(Mutex::new(on_event));
        let drain_event = Arc::clone(&on_event);
        let notify = move |event| {
            if let Ok(mut f) = drain_event.lock() {
                f(event);
            }
        };

        let err_shared = Arc::clone(&shared);
        let on_error = move |e: cpal::Error| {
            // Other kinds (xrun, rerouted default) leave the stream running.
            if matches!(
                e.kind(),
                cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated
            ) && !err_shared.lost.swap(true, Ordering::AcqRel)
                && let Ok(mut f) = on_event.lock()
            {
                f(CaptureEvent::DeviceLost);
            }
        };

        let config = supported.config();
        let cb_shared = Arc::clone(&shared);
        let stream = match format {
            SampleFormat::F32 => build::<f32>(&dev, config, tx, channels, cb_shared, on_error),
            SampleFormat::F64 => build::<f64>(&dev, config, tx, channels, cb_shared, on_error),
            SampleFormat::I8 => build::<i8>(&dev, config, tx, channels, cb_shared, on_error),
            SampleFormat::I16 => build::<i16>(&dev, config, tx, channels, cb_shared, on_error),
            SampleFormat::I24 => build::<I24>(&dev, config, tx, channels, cb_shared, on_error),
            SampleFormat::I32 => build::<i32>(&dev, config, tx, channels, cb_shared, on_error),
            SampleFormat::U8 => build::<u8>(&dev, config, tx, channels, cb_shared, on_error),
            SampleFormat::U16 => build::<u16>(&dev, config, tx, channels, cb_shared, on_error),
            SampleFormat::U32 => build::<u32>(&dev, config, tx, channels, cb_shared, on_error),
            other => return Err(CaptureError::UnsupportedFormat(other.to_string())),
        }?;

        // Play before the spawn: a play error then leaves no thread behind, and the ring holds
        // what arrives before the drain starts.
        stream.play()?;
        let drain_shared = Arc::clone(&shared);
        let drain_meter = meter.clone();
        // The pipeline and the meter get one chunk per window (ARCHITECTURE section 2).
        let window = samples_for(CHUNK, rate);
        let drain = thread::Builder::new()
            .name("oxi-capture-drain".into())
            .spawn(move || {
                let drain = Drain {
                    channels,
                    window,
                    shared: &drain_shared,
                    meter: &drain_meter,
                };
                drain.run(rx, pipeline, notify)
            })
            .map_err(CaptureError::Thread)?;
        Ok(Self {
            stream: Some(stream),
            shared,
            drain: Some(drain),
            meter,
            rate,
            fell_back,
            name,
        })
    }

    fn input_rate(&self) -> u32 {
        self.rate
    }

    fn dropped_samples(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    fn fell_back_to_default(&self) -> bool {
        self.fell_back
    }

    fn finish(mut self) -> Result<Vec<f32>, CaptureError> {
        self.stop(false)
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

/// The whole cpal callback: convert to f32 and push whole frames into the ring.
/// Counts what does not fit as dropped. No allocation, no lock.
fn push_converted<T>(tx: &mut Producer<f32>, data: &[T], channels: usize, shared: &Shared)
where
    T: Sample,
    f32: FromSample<T>,
{
    let fits = tx.slots().min(data.len()) / channels * channels;
    if let Ok(chunk) = tx.write_chunk_uninit(fits) {
        chunk.fill_from_iter(data[..fits].iter().map(|s| s.to_sample::<f32>()));
    }
    if fits < data.len() {
        shared
            .dropped
            .fetch_add((data.len() - fits) as u64, Ordering::Relaxed);
    }
}

/// What the drain feeds; a trait only so tests can inject a failing pipeline.
trait Sink {
    fn push(&mut self, mono: &[f32]) -> Result<Level, AudioError>;
    fn finish(self) -> Result<Vec<f32>, AudioError>;
}

impl Sink for Pipeline {
    fn push(&mut self, mono: &[f32]) -> Result<Level, AudioError> {
        Pipeline::push(self, mono)
    }

    fn finish(self) -> Result<Vec<f32>, AudioError> {
        Pipeline::finish(self)
    }
}

/// The drain thread's fixed inputs.
struct Drain<'a> {
    channels: usize,
    window: usize,
    shared: &'a Shared,
    meter: &'a LevelMeter,
}

impl Drain<'_> {
    /// Moves the ring into `sink` until stopped, then drains once more so nothing captured is
    /// lost. After a failed push the ring is still emptied (no overrun) but not processed.
    fn run(
        &self,
        mut rx: Consumer<f32>,
        mut sink: impl Sink,
        mut notify: impl FnMut(CaptureEvent),
    ) -> Result<Vec<f32>, AudioError> {
        let mut failed = None;
        loop {
            // Read before draining: everything pushed before `stop` is seen by the last pass.
            let stopping = self.shared.stop.load(Ordering::Acquire);
            if stopping && self.shared.cancel.load(Ordering::Relaxed) {
                return Ok(Vec::new());
            }
            if failed.is_some() {
                if let Ok(chunk) = rx.read_chunk(rx.slots()) {
                    chunk.commit_all();
                }
            } else if let Err(e) = drain_once(
                &mut rx,
                self.channels,
                self.window,
                &mut sink,
                self.meter,
                stopping,
            ) {
                failed = Some(e);
                self.meter.clear();
                notify(CaptureEvent::Failed);
            }
            if stopping {
                break;
            }
            thread::park_timeout(DRAIN_POLL);
        }
        match failed {
            Some(e) => Err(e),
            None => sink.finish(),
        }
    }
}

/// One pass: whole blocks of `window` frames (everything when `all`) go from the ring into
/// `sink`, one window per push, and the meter gets each window's post-gain level. A part
/// window waits in the ring for the next pass.
fn drain_once(
    rx: &mut Consumer<f32>,
    channels: usize,
    window: usize,
    sink: &mut impl Sink,
    meter: &LevelMeter,
    all: bool,
) -> Result<(), AudioError> {
    let block = window * channels;
    let available = if all {
        rx.slots()
    } else {
        rx.slots() / block * block
    };
    if available == 0 {
        return Ok(());
    }
    let Ok(chunk) = rx.read_chunk(available) else {
        return Ok(());
    };
    let (a, b) = chunk.as_slices();
    let interleaved = [a, b].concat();
    chunk.commit_all();
    for piece in downmix(&interleaved, channels).chunks(window) {
        meter.store(sink.push(piece)?);
    }
    Ok(())
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

    #[test]
    fn i16_scales_to_unit_range() {
        let (out, _) = run(&[0_i16, i16::MIN, 16_384], 1, 8);
        assert_eq!(out, [0.0, -1.0, 0.5]);
    }

    #[test]
    fn i24_scales_to_unit_range() {
        let samples = [0, -(1 << 23), 1 << 22].map(|v| I24::new(v).unwrap());
        let (out, _) = run(&samples, 1, 8);
        assert_eq!(out, [0.0, -1.0, 0.5]);
    }

    #[test]
    fn u32_origin_is_the_middle() {
        let (out, _) = run(&[1_u32 << 31, 0, 3 << 30], 1, 8);
        assert_eq!(out, [0.0, -1.0, 0.5]);
    }

    #[test]
    fn u16_origin_is_the_middle() {
        let (out, _) = run(&[32_768_u16, 0, 49_152], 1, 8);
        assert_eq!(out, [0.0, -1.0, 0.5]);
    }

    #[test]
    fn f32_passes_through() {
        let (out, _) = run(&[0.25_f32, -0.75], 2, 8);
        assert_eq!(out, [0.25, -0.75]);
    }

    #[test]
    fn full_ring_drops_whole_frames_and_counts() {
        // Room for 5 samples, stereo: 2 frames (4 samples) fit, 4 of 8 are dropped.
        let (out, shared) = run(&[0.5_f32; 8], 2, 5);
        assert_eq!(out.len(), 4);
        assert_eq!(shared.dropped.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn meter_shows_post_gain_level_of_the_last_whole_window() {
        const RATE: u32 = 16_000;
        let window = samples_for(CHUNK, RATE);
        let opts = Audio {
            gain_db: 20.0 * 2.0_f64.log10(),
            denoise: false,
            trim: false,
            normalize: false,
            ..Audio::default()
        };
        let mut pipeline = Pipeline::new(RATE, &opts).unwrap();
        // Stereo: two windows at 0.1, one at 0.25, half a window at 0.9 that must wait in the ring.
        let (mut tx, mut rx) = RingBuffer::new(window * 2 * 4);
        let frames = [(0.1_f32, 2 * window), (0.25, window), (0.9, window / 2)];
        for (value, count) in frames {
            for _ in 0..count * 2 {
                tx.push(value).unwrap();
            }
        }
        let meter = LevelMeter::default();
        drain_once(&mut rx, 2, window, &mut pipeline, &meter, false).unwrap();
        let level = meter.read();
        assert!((level.peak - 0.5).abs() < 1e-5, "{level:?}");
        assert!((level.rms - 0.5).abs() < 1e-5, "{level:?}");
        assert_eq!(rx.slots(), window);

        // The last pass takes the part window too.
        drain_once(&mut rx, 2, window, &mut pipeline, &meter, true).unwrap();
        assert!((meter.read().peak - 1.8).abs() < 1e-4);
        assert_eq!(rx.slots(), 0);
    }

    /// All-zero input: `Pipeline::finish` answers `MicSilence`, so an `Ok` shows it did not run.
    fn stopped_drain(cancel: bool) -> Result<Vec<f32>, AudioError> {
        const RATE: u32 = 16_000;
        let opts = Audio {
            denoise: false,
            ..Audio::default()
        };
        let pipeline = Pipeline::new(RATE, &opts).unwrap();
        let (mut tx, rx) = RingBuffer::new(1024);
        for _ in 0..1024 {
            tx.push(0.0).unwrap();
        }
        let shared = Shared::default();
        shared.cancel.store(cancel, Ordering::Relaxed);
        shared.stop.store(true, Ordering::Release);
        let drain = Drain {
            channels: 1,
            window: samples_for(CHUNK, RATE),
            shared: &shared,
            meter: &LevelMeter::default(),
        };
        drain.run(rx, pipeline, |_| {})
    }

    #[test]
    fn finish_processes_what_was_captured() {
        assert!(matches!(stopped_drain(false), Err(AudioError::MicSilence)));
    }

    #[test]
    fn cancel_discards_without_processing() {
        assert_eq!(stopped_drain(true).unwrap(), Vec::<f32>::new());
    }

    /// Fails from the second push on; counts pushes.
    struct FailingSink(Arc<AtomicU64>);

    impl Sink for FailingSink {
        fn push(&mut self, _: &[f32]) -> Result<Level, AudioError> {
            if self.0.fetch_add(1, Ordering::Relaxed) == 0 {
                Ok(Level {
                    peak: 0.5,
                    rms: 0.25,
                })
            } else {
                Err(AudioError::Resample("injected".into()))
            }
        }

        fn finish(self) -> Result<Vec<f32>, AudioError> {
            Ok(vec![1.0])
        }
    }

    #[test]
    fn failed_push_zeroes_meter_notifies_once_keeps_emptying_ring() {
        const WINDOW_LEN: usize = 4;
        const CAPACITY: usize = WINDOW_LEN * 8;
        let pushes = Arc::new(AtomicU64::new(0));
        let shared = Arc::new(Shared::default());
        let meter = LevelMeter::default();
        let (mut tx, rx) = RingBuffer::new(CAPACITY);
        let (event_tx, events) = std::sync::mpsc::channel();
        let wait_empty = |tx: &Producer<f32>| {
            let start = std::time::Instant::now();
            while tx.slots() < CAPACITY && start.elapsed() < Duration::from_secs(5) {
                thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(tx.slots(), CAPACITY, "ring emptied");
        };
        let worker = {
            let (shared, meter, sink) = (
                Arc::clone(&shared),
                meter.clone(),
                FailingSink(Arc::clone(&pushes)),
            );
            thread::spawn(move || {
                let drain = Drain {
                    channels: 1,
                    window: WINDOW_LEN,
                    shared: &shared,
                    meter: &meter,
                };
                drain.run(rx, sink, |e| event_tx.send(e).unwrap())
            })
        };
        // Two windows: the first push sets the meter, the second fails and clears it.
        for _ in 0..2 * WINDOW_LEN {
            tx.push(0.1).unwrap();
        }
        let event = events.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(event, CaptureEvent::Failed);
        wait_empty(&tx);
        assert_eq!(
            meter.read(),
            Level {
                peak: 0.0,
                rms: 0.0
            }
        );
        // A full ring later is still emptied, without more pushes or events.
        for _ in 0..CAPACITY {
            tx.push(0.1).unwrap();
        }
        wait_empty(&tx);
        shared.stop.store(true, Ordering::Release);
        worker.thread().unpark();
        let result = worker.join().unwrap();
        assert!(matches!(result, Err(AudioError::Resample(_))), "{result:?}");
        assert_eq!(pushes.load(Ordering::Relaxed), 2);
        assert!(events.try_recv().is_err(), "one event only");
    }
}
