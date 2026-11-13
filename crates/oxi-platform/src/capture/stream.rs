use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, I24, InputCallbackInfo, Sample, SampleFormat, SizedSample, Stream};
use oxi_core::audio::{
    AudioError, CHUNK, InputDevice, InputPick, Level, Pipeline, downmix, pick_input, samples_for,
};
use oxi_core::config::{Audio, AudioChannel};
use rtrb::{Consumer, Producer, RingBuffer};

use super::{Capture, CaptureError, CaptureEvent, LevelMeter, Recorded};
use crate::wav::WavSpool;

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
    /// Microseconds from the start of `open` to the device's first callback; 0 until then. Set
    /// once by the callback, logged once by the drain as `capture started`.
    first_us: AtomicU64,
    /// Microseconds from the key-down to the start of `open`; set before the stream exists.
    lead_us: u64,
}

/// Names of the input devices, for the settings list.
pub fn list_input_devices() -> Result<Vec<String>, CaptureError> {
    let devices = cpal::default_host().input_devices()?;
    Ok(devices
        .filter_map(|d| d.description().ok().map(|d| d.name().to_owned()))
        .collect())
}

/// Name of the system default input now; `None` without one. The settings page reads it with
/// its device list to follow a default changed while its meter runs.
#[must_use]
pub fn default_input_name() -> Option<String> {
    let dev = cpal::default_host().default_input_device()?;
    dev.description().ok().map(|d| d.name().to_owned())
}

/// The device for `configured` (`audio.device`) among those present now, and whether it is the
/// default standing in for a missing one ([`pick_input`]). Read at every open, never cached, so a
/// Windows default changed while the app runs is the next recording's (e09).
///
/// The default is opened as the endpoint it is now, not as cpal's default handle: on WASAPI that
/// handle is activated through `DEVINTERFACE_AUDIO_CAPTURE` (Windows picks the endpoint then, apart
/// from the `GetDefaultAudioEndpoint` read that gives its name), and a default change mid-recording
/// ends its stream as `StreamInvalidated`, which reads as an unplugged mic. A concrete endpoint
/// records on to the key-up; only a real unplug ends it.
fn resolve_input(
    host: &cpal::Host,
    configured: &str,
) -> Result<(cpal::Device, bool), CaptureError> {
    let default = host.default_input_device();
    let default_id = default.as_ref().and_then(|d| d.id().ok());
    let mut devices: Vec<(cpal::Device, String, bool)> = host
        .input_devices()?
        .map(|d| {
            let is_default = default_id.is_some() && d.id().ok() == default_id;
            // A name costs a property-store read; only a configured device is matched by it.
            let name = if configured.is_empty() {
                String::new()
            } else {
                d.description()
                    .map(|n| n.name().to_owned())
                    .unwrap_or_default()
            };
            (d, name, is_default)
        })
        .collect();
    let present: Vec<InputDevice<'_>> = devices
        .iter()
        .map(|(_, name, is_default)| InputDevice {
            name,
            is_default: *is_default,
        })
        .collect();
    match pick_input(configured, &present) {
        InputPick::Open { index, fell_back } => Ok((devices.swap_remove(index).0, fell_back)),
        // No listed device is the default (a host without device ids, or Windows between two
        // defaults): cpal's own default handle is the last try, as before e09.
        InputPick::NoDevice => default
            .map(|d| (d, !configured.is_empty()))
            .ok_or(CaptureError::NoDevice),
    }
}

/// cpal implementation of [`Capture`] (one for all OSes).
pub struct CpalCapture {
    /// Dropped first in [`CpalCapture::stop`]: no callback runs after that.
    stream: Option<Stream>,
    shared: Arc<Shared>,
    /// Taken by the first [`CpalCapture::stop`], from `finish` or `Drop`.
    drain: Option<JoinHandle<Result<Recorded, AudioError>>>,
    meter: LevelMeter,
    rate: u32,
    fell_back: bool,
    name: String,
    /// Ends the recording as a lost device when its endpoint goes; dropped with the stream.
    #[cfg(windows)]
    watch: Option<super::watch::EndpointWatch>,
}

impl CpalCapture {
    /// Name of the device that was opened (the default's own name when none was configured).
    #[must_use]
    pub fn device_name(&self) -> &str {
        &self.name
    }

    /// Stop the callbacks, then the drain thread, and zero the meter; idempotent. With `cancel`
    /// the drain skips the pipeline flush and returns nothing.
    fn stop(&mut self, cancel: bool) -> Result<Recorded, CaptureError> {
        self.stop_stream();
        self.shared.cancel.store(cancel, Ordering::Relaxed);
        // Release also publishes `cancel` to the drain's Acquire load of `stop`.
        self.shared.stop.store(true, Ordering::Release);
        let Some(drain) = self.drain.take() else {
            // Only `Drop` after `finish` gets here; nothing is left to return.
            return Ok(Recorded::default());
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
        Self::open_pressed(opts, meter, Instant::now(), on_event)
    }

    fn open_pressed(
        opts: &Audio,
        meter: LevelMeter,
        pressed: Instant,
        on_event: impl FnMut(CaptureEvent) + Send + 'static,
    ) -> Result<Self, CaptureError> {
        let opened = Instant::now();
        let lead_us = micros(opened.saturating_duration_since(pressed));
        let (dev, fell_back) = resolve_input(&cpal::default_host(), &opts.device)?;
        let name = dev
            .description()
            .map(|d| d.name().to_owned())
            .unwrap_or_default();
        // The device name, never audio: a later "it recorded the old mic" is read off this line.
        tracing::info!(device = %name, fell_back, "input device picked");

        let supported = dev.default_input_config()?;
        let (channels, rate, format) = (
            usize::from(supported.channels()),
            supported.sample_rate(),
            supported.sample_format(),
        );
        let pipeline = Pipeline::new(rate, opts)?;
        let channel = opts.channel;
        let (tx, rx) = RingBuffer::new(rate as usize * channels * RING_SECONDS);
        let shared = Arc::new(Shared {
            lead_us,
            ..Shared::default()
        });
        // Two senders (cpal error thread, drain thread), both off the data callback.
        let on_event = Arc::new(Mutex::new(on_event));
        let drain_event = Arc::clone(&on_event);
        let notify = move |event| {
            if let Ok(mut f) = drain_event.lock() {
                f(event);
            }
        };

        // Once per recording, from cpal's error thread or the endpoint watch.
        let lost_shared = Arc::clone(&shared);
        let lose = Arc::new(move || {
            if !lost_shared.lost.swap(true, Ordering::AcqRel)
                && let Ok(mut f) = on_event.lock()
            {
                f(CaptureEvent::DeviceLost);
            }
        });
        let err_lose = Arc::clone(&lose);
        let on_error = move |e: cpal::Error| {
            // Other kinds (xrun, rerouted default) leave the stream running.
            if matches!(
                e.kind(),
                cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated
            ) {
                err_lose();
            }
        };

        let config = supported.config();
        // The callback's state, and when the open began: its first call logs the latency.
        let cb = (Arc::clone(&shared), opened);
        let stream = match format {
            SampleFormat::F32 => build::<f32>(&dev, config, tx, channels, cb, on_error),
            SampleFormat::F64 => build::<f64>(&dev, config, tx, channels, cb, on_error),
            SampleFormat::I8 => build::<i8>(&dev, config, tx, channels, cb, on_error),
            SampleFormat::I16 => build::<i16>(&dev, config, tx, channels, cb, on_error),
            SampleFormat::I24 => build::<I24>(&dev, config, tx, channels, cb, on_error),
            SampleFormat::I32 => build::<i32>(&dev, config, tx, channels, cb, on_error),
            SampleFormat::U8 => build::<u8>(&dev, config, tx, channels, cb, on_error),
            SampleFormat::U16 => build::<u16>(&dev, config, tx, channels, cb, on_error),
            SampleFormat::U32 => build::<u32>(&dev, config, tx, channels, cb, on_error),
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
                    channel,
                    window,
                    shared: &drain_shared,
                    meter: &drain_meter,
                };
                let sink = Spooled {
                    inner: pipeline,
                    spool: WavSpool::begin(rate),
                };
                drain.run(rx, sink, notify)
            })
            .map_err(CaptureError::Thread)?;
        // After the stream runs, so the press waits for none of it; nothing fails after it.
        #[cfg(windows)]
        let watch = dev
            .id()
            .ok()
            .and_then(|id| super::watch::EndpointWatch::start(id.id(), lose));
        #[cfg(not(windows))]
        drop(lose);
        Ok(Self {
            stream: Some(stream),
            shared,
            drain: Some(drain),
            meter,
            rate,
            fell_back,
            name,
            #[cfg(windows)]
            watch,
        })
    }

    fn stop_stream(&mut self) {
        drop(self.stream.take());
        #[cfg(windows)]
        drop(self.watch.take());
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

    fn finish(self) -> Result<Vec<f32>, CaptureError> {
        self.finish_recording().map(|r| r.samples)
    }

    fn finish_recording(mut self) -> Result<Recorded, CaptureError> {
        self.stop(false)
    }
}

fn build<T>(
    dev: &cpal::Device,
    config: cpal::StreamConfig,
    mut tx: Producer<f32>,
    channels: usize,
    (shared, opened): (Arc<Shared>, Instant),
    on_error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<Stream, CaptureError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    Ok(dev.build_input_stream(
        config,
        move |data: &[T], _: &InputCallbackInfo| {
            // From here the audio is kept; one clock read, no lock.
            if shared.first_us.load(Ordering::Relaxed) == 0 {
                let us = micros(opened.elapsed());
                shared.first_us.store(us.max(1), Ordering::Relaxed);
            }
            push_converted(&mut tx, data, channels, &shared);
        },
        on_error,
        None,
    )?)
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
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
    fn finish(self) -> Result<Recorded, AudioError>;
    /// The user is heard speaking ([`Pipeline::speech_started`]).
    fn speech_started(&self) -> bool {
        false
    }
    /// The gate scored at least one frame as voice ([`Pipeline::voice_seen`]).
    fn voice_seen(&self) -> bool {
        false
    }
    /// The recording was cancelled: nothing of it is kept.
    fn cancel(self)
    where
        Self: Sized,
    {
    }
    /// Processing failed: what the microphone gave is still worth keeping.
    fn failed(self)
    where
        Self: Sized,
    {
    }
}

impl Sink for Pipeline {
    fn push(&mut self, mono: &[f32]) -> Result<Level, AudioError> {
        Pipeline::push(self, mono)
    }

    fn finish(self) -> Result<Recorded, AudioError> {
        Pipeline::finish_with_stats(self).map(|f| Recorded {
            samples: f.samples,
            spool: None,
            trim: f.trim,
            stats: f.stats,
        })
    }

    fn speech_started(&self) -> bool {
        Pipeline::speech_started(self)
    }

    fn voice_seen(&self) -> bool {
        Pipeline::voice_seen(self)
    }
}

/// Queues the raw mono windows for a [`WavSpool`] as they arrive (its own thread writes them, so
/// a slow disk never holds up the drain), then hands them to `inner`.
struct Spooled<S> {
    inner: S,
    spool: Option<WavSpool>,
}

impl<S: Sink> Sink for Spooled<S> {
    fn push(&mut self, mono: &[f32]) -> Result<Level, AudioError> {
        if let Some(spool) = &mut self.spool {
            spool.push(mono);
        }
        self.inner.push(mono)
    }

    fn speech_started(&self) -> bool {
        self.inner.speech_started()
    }

    fn voice_seen(&self) -> bool {
        self.inner.voice_seen()
    }

    fn finish(self) -> Result<Recorded, AudioError> {
        let voice = self.inner.voice_seen();
        let mut result = self.inner.finish();
        if let Some(spool) = self.spool {
            match &mut result {
                // The job carries the path to its history save.
                Ok(r) if !r.samples.is_empty() => r.spool = Some(spool.finish()),
                // An all-zero mic has nothing to retranscribe.
                Ok(_) | Err(AudioError::MicSilence) => spool.discard(),
                // Rule: nothing heard is kept only with a hint of speech (the gate scored at least
                // one voiced frame), so a very quiet word can be retranscribed from History; a
                // hold with no voiced frame at all is deleted.
                Err(AudioError::NothingHeard) if !voice => spool.discard(),
                // Any other failure is kept too. No transcript will come.
                Err(_) => spool.fail(),
            }
        }
        result
    }

    fn cancel(self) {
        if let Some(spool) = self.spool {
            spool.discard();
        }
    }

    fn failed(self) {
        if let Some(spool) = self.spool {
            spool.fail();
        }
    }
}

/// The drain thread's fixed inputs.
struct Drain<'a> {
    channels: usize,
    /// Which side of a stereo mic is kept (`audio.channel`).
    channel: AudioChannel,
    window: usize,
    shared: &'a Shared,
    meter: &'a LevelMeter,
}

impl Drain<'_> {
    /// Moves the ring into `sink` until stopped, then drains once more so nothing captured is
    /// lost. After a failed push the ring is still emptied (no overrun) but not processed.
    /// Sends [`CaptureEvent::SpeechHeard`] once, after the pass in which the sink first hears it.
    fn run(
        &self,
        mut rx: Consumer<f32>,
        mut sink: impl Sink,
        mut notify: impl FnMut(CaptureEvent),
    ) -> Result<Recorded, AudioError> {
        let mut failed = None;
        let mut heard = false;
        let mut logged = false;
        loop {
            // Read before draining: everything pushed before `stop` is seen by the last pass.
            let stopping = self.shared.stop.load(Ordering::Acquire);
            let first_us = self.shared.first_us.load(Ordering::Relaxed);
            if !logged && first_us > 0 {
                logged = true;
                // From the key-down, and from the open alone (d11).
                let from_key_us = first_us.saturating_add(self.shared.lead_us);
                tracing::info!(
                    latency_ms = from_key_us / 1000,
                    from_open_ms = first_us / 1000,
                    "capture started"
                );
            }
            if stopping && self.shared.cancel.load(Ordering::Relaxed) {
                // Discarded, also after a failed push: the machine stops (never discards) a
                // recording with speech, so this one has nothing worth keeping (triage #12).
                sink.cancel();
                return failed.map_or(Ok(Recorded::default()), Err);
            }
            if failed.is_some() {
                if let Ok(chunk) = rx.read_chunk(rx.slots()) {
                    chunk.commit_all();
                }
            } else if let Err(e) = drain_once(
                &mut rx,
                self.channels,
                self.channel,
                self.window,
                &mut sink,
                self.meter,
                stopping,
            ) {
                failed = Some(e);
                self.meter.clear();
                notify(CaptureEvent::Failed);
            } else if !heard && !stopping && sink.speech_started() {
                heard = true;
                notify(CaptureEvent::SpeechHeard);
            }
            if stopping {
                break;
            }
            thread::park_timeout(DRAIN_POLL);
        }
        match failed {
            Some(e) => {
                sink.failed();
                Err(e)
            }
            None => sink.finish(),
        }
    }
}

/// One channel of each frame for `Left` and `Right` (a mono mic has only the one), else the
/// average of them all.
fn pick_channel(interleaved: &[f32], channels: usize, channel: AudioChannel) -> Vec<f32> {
    let side = match channel {
        AudioChannel::Left => 0,
        AudioChannel::Right => 1,
        AudioChannel::Mix => return downmix(interleaved, channels),
    };
    if channels < 2 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks_exact(channels)
        .map(|frame| frame[side])
        .collect()
}

/// One pass: whole blocks of `window` frames (everything when `all`) go from the ring into
/// `sink`, one window per push, and the meter gets each window's post-gain level. A part
/// window waits in the ring for the next pass.
fn drain_once(
    rx: &mut Consumer<f32>,
    channels: usize,
    channel: AudioChannel,
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
    for piece in pick_channel(&interleaved, channels, channel).chunks(window) {
        meter.store(sink.push(piece)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use oxi_core::audio::SAMPLE_RATE as RATE;

    use super::*;
    use crate::scratch::scratch_dir;

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
        drain_once(
            &mut rx,
            2,
            AudioChannel::Mix,
            window,
            &mut pipeline,
            &meter,
            false,
        )
        .unwrap();
        let level = meter.read();
        assert!((level.peak - 0.5).abs() < 1e-5, "{level:?}");
        assert!((level.rms - 0.5).abs() < 1e-5, "{level:?}");
        assert_eq!(rx.slots(), window);

        // The last pass takes the part window too.
        drain_once(
            &mut rx,
            2,
            AudioChannel::Mix,
            window,
            &mut pipeline,
            &meter,
            true,
        )
        .unwrap();
        assert!((meter.read().peak - 1.8).abs() < 1e-4);
        assert_eq!(rx.slots(), 0);
    }

    /// All-zero input: `Pipeline::finish` answers `MicSilence`, so an `Ok` shows it did not run.
    fn stopped_drain(cancel: bool) -> Result<Recorded, AudioError> {
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
            channel: AudioChannel::Mix,
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
        assert_eq!(stopped_drain(true).unwrap(), Recorded::default());
    }

    /// The drain says once that speech started, while the recording runs, not at the stop.
    #[test]
    fn speech_is_reported_once_while_recording() {
        let opts = Audio {
            denoise: false,
            ..Audio::default()
        };
        let pipeline = Pipeline::new(RATE, &opts).unwrap();
        let shared = Arc::new(Shared::default());
        let (mut tx, rx) = RingBuffer::new(RATE as usize);
        let (event_tx, events) = std::sync::mpsc::channel();
        let worker = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                let drain = Drain {
                    channels: 1,
                    channel: AudioChannel::Mix,
                    window: samples_for(CHUNK, RATE),
                    shared: &shared,
                    meter: &LevelMeter::default(),
                };
                drain.run(rx, pipeline, |e| event_tx.send(e).unwrap())
            })
        };
        // 300 ms of a loud 300 Hz tone.
        for i in 0..300 * 16 {
            let t = i as f32 / RATE as f32;
            tx.push(0.5 * (std::f32::consts::TAU * 300.0 * t).sin())
                .unwrap();
        }
        let event = events.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(event, CaptureEvent::SpeechHeard);
        shared.stop.store(true, Ordering::Release);
        worker.thread().unpark();
        let recorded = worker.join().unwrap().unwrap();
        assert_ne!(recorded.samples.len(), 0);
        assert!(events.try_recv().is_err(), "one event only");
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

        fn finish(self) -> Result<Recorded, AudioError> {
            Ok(Recorded {
                samples: vec![1.0],
                ..Recorded::default()
            })
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
            let start = Instant::now();
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
                    channel: AudioChannel::Mix,
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

    /// Fails every push; records whether the drain ended it as cancelled or as failed.
    struct EndSink(Arc<Mutex<&'static str>>);

    impl Sink for EndSink {
        fn push(&mut self, _: &[f32]) -> Result<Level, AudioError> {
            Err(AudioError::Resample("injected".into()))
        }

        fn finish(self) -> Result<Recorded, AudioError> {
            *self.0.lock().unwrap() = "finish";
            Ok(Recorded::default())
        }

        fn cancel(self) {
            *self.0.lock().unwrap() = "cancel";
        }

        fn failed(self) {
            *self.0.lock().unwrap() = "failed";
        }
    }

    /// Pushes until one fails, then stops the drain over `sink` (`cancel`: discarded).
    fn failed_then_stopped<S: Sink + Send + 'static>(
        sink: S,
        cancel: bool,
    ) -> Result<Recorded, AudioError> {
        let shared = Arc::new(Shared::default());
        let (mut tx, rx) = RingBuffer::new(16);
        let (event_tx, events) = std::sync::mpsc::channel();
        let worker = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                let drain = Drain {
                    channels: 1,
                    channel: AudioChannel::Mix,
                    window: 4,
                    shared: &shared,
                    meter: &LevelMeter::default(),
                };
                drain.run(rx, sink, |e| event_tx.send(e).unwrap())
            })
        };
        for _ in 0..4 {
            tx.push(0.1).unwrap();
        }
        let event = events.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(event, CaptureEvent::Failed);
        shared.cancel.store(cancel, Ordering::Relaxed);
        shared.stop.store(true, Ordering::Release);
        worker.thread().unpark();
        worker.join().unwrap()
    }

    /// A mic that fails mid-recording: stopped (speech heard) it is kept as failed (triage #12);
    /// discarded (no speech) it is cancelled, nothing kept.
    #[test]
    fn a_failed_push_is_kept_when_stopped_and_dropped_when_discarded() {
        for (cancel, end) in [(false, "failed"), (true, "cancel")] {
            let ended = Arc::new(Mutex::new("running"));
            let result = failed_then_stopped(EndSink(Arc::clone(&ended)), cancel);
            assert!(matches!(result, Err(AudioError::Resample(_))), "{result:?}");
            assert_eq!(*ended.lock().unwrap(), end, "cancel {cancel}");
        }
    }

    /// Spool files (and the ones still being written) in the day folders of `dir`.
    fn parts(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .flat_map(|day| std::fs::read_dir(day.unwrap().path()).unwrap())
            .map(|f| f.unwrap().path())
            .filter(|p| crate::wav::spool_busy(p) || p.is_file())
            .collect()
    }

    /// The writer thread deletes a discarded spool; it is busy until it is gone. Waits for the
    /// writer too: one that has not created its file yet would delete the next spool's (same path
    /// within one clock tick).
    fn wait_until_no_parts(dir: &std::path::Path, why: &str) {
        let start = Instant::now();
        while !parts(dir).is_empty() || !crate::wav::spool_idle() {
            assert!(start.elapsed() < Duration::from_secs(5), "{why}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// A push fails before speech, so the machine discards: the spool goes too, never an orphan.
    #[test]
    fn a_discard_after_a_failed_push_keeps_no_spool() {
        let _guard = crate::wav::SPOOL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = scratch_dir(&format!("oxi-stream-failspool-{}", std::process::id()));
        crate::wav::spool_give_up_all();
        let _ = crate::wav::spool_take_orphans();
        crate::wav::set_spool_dir(Some(dir.clone()));
        let sink = Spooled {
            inner: EndSink(Arc::new(Mutex::new("running"))),
            spool: WavSpool::begin(RATE),
        };
        assert!(sink.spool.is_some());
        let result = failed_then_stopped(sink, true);
        assert!(matches!(result, Err(AudioError::Resample(_))), "{result:?}");
        wait_until_no_parts(&dir, "a discard keeps nothing");
        assert_eq!(
            crate::wav::spool_take_orphans(),
            Vec::<std::path::PathBuf>::new()
        );
        crate::wav::set_spool_dir(None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Mono 16 kHz tone in, for a drain over a recording that is spooled to `dir`.
    fn spooled_drain(dir: &std::path::Path, cancel: bool) -> Result<Recorded, AudioError> {
        crate::wav::set_spool_dir(Some(dir.to_owned()));
        let opts = Audio {
            denoise: false,
            trim: false,
            ..Audio::default()
        };
        let sink = Spooled {
            inner: Pipeline::new(RATE, &opts).unwrap(),
            spool: WavSpool::begin(RATE),
        };
        let (mut tx, rx) = RingBuffer::new(32_000);
        for i in 0..32_000 {
            tx.push(if i % 2 == 0 { 0.3 } else { -0.3 }).unwrap();
        }
        let shared = Shared::default();
        shared.cancel.store(cancel, Ordering::Relaxed);
        shared.stop.store(true, Ordering::Release);
        let drain = Drain {
            channels: 1,
            channel: AudioChannel::Mix,
            window: samples_for(CHUNK, RATE),
            shared: &shared,
            meter: &LevelMeter::default(),
        };
        drain.run(rx, sink, |_| {})
    }

    #[test]
    fn a_finished_recording_leaves_its_stream_and_a_cancelled_one_does_not() {
        let _guard = crate::wav::SPOOL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = scratch_dir(&format!("oxi-stream-spool-{}", std::process::id()));
        crate::wav::spool_give_up_all();
        let _ = crate::wav::spool_take_orphans();

        assert_eq!(spooled_drain(&dir, true).unwrap(), Recorded::default());
        assert_eq!(
            crate::wav::spool_take_orphans(),
            Vec::<std::path::PathBuf>::new()
        );
        wait_until_no_parts(&dir, "cancel keeps nothing");

        let processed = spooled_drain(&dir, false).unwrap();
        let part = processed.spool.expect("the job carries its stream");
        assert!(
            crate::wav::spool_take(&part),
            "the stream waits for its transcript"
        );
        let (samples, rate) = crate::wav::read_wav_mono_f32(&part).unwrap();
        assert_eq!((samples.len(), rate), (32_000, RATE));
        crate::wav::set_spool_dir(None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ends with the given error, as the pipeline does at key-up.
    struct EndsWith(fn() -> AudioError, bool);

    impl Sink for EndsWith {
        fn push(&mut self, _: &[f32]) -> Result<Level, AudioError> {
            Ok(Level {
                peak: 0.0,
                rms: 0.0,
            })
        }

        fn voice_seen(&self) -> bool {
            self.1
        }

        fn finish(self) -> Result<Recorded, AudioError> {
            Err((self.0)())
        }
    }

    /// A quiet word the gate did not count stays in History (retranscribe) when it scored a voiced
    /// frame; an all-zero mic and a hold without one do not.
    #[test]
    fn a_nothing_heard_clip_is_kept_only_with_a_hint_of_speech() {
        type Case = (fn() -> AudioError, bool, usize);
        let _guard = crate::wav::SPOOL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = scratch_dir(&format!("oxi-stream-quiet-{}", std::process::id()));
        crate::wav::spool_give_up_all();
        let _ = crate::wav::spool_take_orphans();
        crate::wav::set_spool_dir(Some(dir.clone()));
        let kept: [Case; 3] = [
            (|| AudioError::MicSilence, true, 0),
            (|| AudioError::NothingHeard, false, 0),
            // Last: its file stays, so a spool begun in the same clock tick would collide.
            (|| AudioError::NothingHeard, true, 1),
        ];
        for (err, voice, orphans) in kept {
            let mut sink = Spooled {
                inner: EndsWith(err, voice),
                spool: WavSpool::begin(RATE),
            };
            sink.push(&[0.01; 320]).unwrap();
            assert!(sink.finish().is_err());
            assert_eq!(crate::wav::spool_take_orphans().len(), orphans);
            let start = Instant::now();
            while !crate::wav::spool_idle() {
                assert!(start.elapsed() < Duration::from_secs(5), "writer settles");
                thread::sleep(Duration::from_millis(5));
            }
        }
        crate::wav::set_spool_dir(None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_side_of_a_stereo_mic_is_kept_and_mix_averages() {
        let frames = [1.0, 0.0, 0.5, -0.5];
        assert_eq!(pick_channel(&frames, 2, AudioChannel::Left), [1.0, 0.5]);
        assert_eq!(pick_channel(&frames, 2, AudioChannel::Right), [0.0, -0.5]);
        assert_eq!(pick_channel(&frames, 2, AudioChannel::Mix), [0.5, 0.0]);
        // A mono mic has no sides: every choice gives the one channel.
        assert_eq!(
            pick_channel(&[0.25, 0.5], 1, AudioChannel::Right),
            [0.25, 0.5]
        );
    }

    /// What resolving the default at every press costs (e09): cpal's default handle alone (before)
    /// and the walk to its concrete endpoint (after), 50 times each; prints p50/p95 in µs. No
    /// stream is opened, so the mic stays off. Run alone with `--nocapture`.
    #[test]
    fn resolve_cost_live() {
        if std::env::var_os("OXI_DEVICE_PROBE").is_none_or(|v| v != "1") {
            eprintln!("skipped: set OXI_DEVICE_PROBE=1");
            return;
        }
        let host = cpal::default_host();
        let time = |f: &dyn Fn() -> Option<String>| {
            let mut v: Vec<u128> = (0..50)
                .map(|_| {
                    let t = Instant::now();
                    assert!(f().is_some(), "no default input");
                    t.elapsed().as_micros()
                })
                .collect();
            v.sort_unstable();
            (v[25], v[47])
        };
        let name = |d: cpal::Device| d.description().ok().map(|d| d.name().to_owned());
        let before = time(&|| host.default_input_device().and_then(name));
        let after = time(&|| resolve_input(&host, "").ok().and_then(|(d, _)| name(d)));
        eprintln!("default handle p50/p95 {before:?} us; concrete endpoint p50/p95 {after:?} us");
        let picked = resolve_input(&host, "").ok().and_then(|(d, _)| name(d));
        assert_eq!(picked, default_input_name(), "the endpoint is the default");
    }

    /// A capture whose first sample has come, and the time from `t` to that sample.
    fn open_started(opts: &Audio, t: Instant) -> (CpalCapture, Duration) {
        let lead = t.elapsed();
        let c = CpalCapture::open(opts, LevelMeter::default(), |_| {}).unwrap();
        // A sleep would add the 15 ms Windows timer tick to every wait.
        loop {
            let us = c.shared.first_us.load(Ordering::Relaxed);
            if us > 0 {
                return (c, lead + Duration::from_micros(us));
            }
            assert!(t.elapsed() < Duration::from_secs(5), "no first sample");
            thread::yield_now();
        }
    }

    fn p50_p95(mut v: Vec<Duration>) -> (u128, u128) {
        v.sort_unstable();
        let at = |q: usize| v[(v.len() * q / 100).min(v.len() - 1)].as_millis();
        (at(50), at(95))
    }

    /// Open to first sample on the real default microphone, idle and with a busy thread per
    /// core at normal priority, then below it (whisper's threads, were they lowered): a fresh press, a re-press that finishes the old capture first (the audio worker
    /// before d11) and one that finishes it on another thread (after), and a fresh press of
    /// the device by name (the device list is walked). Prints p50/p95 in ms; run alone with
    /// `--nocapture`. Audio is never written to disk.
    #[test]
    fn start_latency_live() {
        const RECORD: Duration = Duration::from_secs(2);
        if std::env::var_os("OXI_MIC_TEST").is_none_or(|v| v != "1") {
            eprintln!("skipped: set OXI_MIC_TEST=1");
            return;
        }
        // The spool is process-wide: without the lock a spool test's folder would get mic audio.
        let _guard = crate::wav::SPOOL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::wav::set_spool_dir(None);
        let opts = Audio::default();
        for (load, low) in [(false, false), (true, false), (true, true)] {
            let stop = Arc::new(AtomicBool::new(false));
            let cores = if load {
                thread::available_parallelism().map_or(1, usize::from)
            } else {
                0
            };
            let burners: Vec<_> = (0..cores)
                .map(|_| {
                    let stop = Arc::clone(&stop);
                    thread::spawn(move || {
                        #[cfg(windows)]
                        if low {
                            use windows_sys::Win32::System::Threading::{
                                GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
                            };
                            // SAFETY: the pseudo handle of this thread and a valid level.
                            unsafe {
                                SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL)
                            };
                        }
                        let mut x = 0u64;
                        while !stop.load(Ordering::Relaxed) {
                            x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(7));
                        }
                    })
                })
                .collect();
            let (mut fresh, mut serial, mut parallel, mut named) =
                (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            for _ in 0..10 {
                let (a, took) = open_started(&opts, Instant::now());
                fresh.push(took);
                thread::sleep(RECORD);
                let t = Instant::now();
                let _ = a.finish();
                let (b, took) = open_started(&opts, t);
                serial.push(took);
                thread::sleep(RECORD);
                let t = Instant::now();
                let mut b = b;
                // As the audio worker does: the old stream stops here, its finish goes aside.
                b.stop_stream();
                let old = thread::spawn(move || b.finish().is_ok());
                let (c, took) = open_started(&opts, t);
                parallel.push(took);
                let _ = old.join();
                let by_name = Audio {
                    device: c.device_name().into(),
                    ..Audio::default()
                };
                drop(c);
                named.push(open_started(&by_name, Instant::now()).1);
            }
            stop.store(true, Ordering::Relaxed);
            for b in burners {
                b.join().unwrap();
            }
            for (what, v) in [
                ("fresh", fresh),
                ("re-press finish-first", serial),
                ("re-press finish-aside", parallel),
                ("fresh by name", named),
            ] {
                let (p50, p95) = p50_p95(v);
                eprintln!("load={load} low={low} cores={cores} {what}: p50={p50} ms p95={p95} ms");
            }
        }
    }
}
