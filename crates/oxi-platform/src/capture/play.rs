//! A short mono clip on the default output: the Audio page's microphone test plays back what it
//! recorded, and the History player plays a part of a recording. The clip stays in memory only.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
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
    shared: Arc<Shared>,
    looping: bool,
}

/// What the output callback tells the owner of the playback.
#[derive(Default)]
struct Shared {
    /// Where the sound is, in microseconds of the clip's own time.
    at_us: AtomicU64,
    /// The part played to its end and the device sounded its buffer.
    done: AtomicBool,
}

/// What [`Playback::start`] plays: the part `from..to` of the clip (clip time; `to` `None` is the
/// end), `speed` times as fast, over and over while `looping`, jumping over `skips` that start at
/// `from` or later (a start inside a skip plays from there: the user asked for that place).
///
/// shortcut: the speed resamples, so the pitch follows it (2x sounds high); use a time stretch
/// (WSOLA) if listening at speed ever needs the voice kept.
#[derive(Debug, Clone, PartialEq)]
struct Plan {
    from: Duration,
    to: Option<Duration>,
    speed: f32,
    looping: bool,
    skips: Vec<Range<Duration>>,
}

impl Default for Plan {
    fn default() -> Self {
        Self {
            from: Duration::ZERO,
            to: None,
            speed: 1.0,
            looping: false,
            skips: Vec::new(),
        }
    }
}

/// Clip samples resampled at a time, off the output callback and off the caller's thread.
const BLOCK_MS: u64 = 2000;
/// Each block is resampled with this much of its neighbours on both sides and cut back, so the
/// resampler's edges never sound at a seam.
const MARGIN_MS: u64 = 20;
/// Blocks ready ahead of the output: enough to ride over a busy moment, little memory.
const AHEAD: usize = 2;

/// The clip ranges of a plan, in the order they sound, a block at a time.
#[derive(Debug)]
struct Part {
    start: usize,
    end: usize,
    pos: usize,
    looping: bool,
    /// In clip samples, in order; only those that start at `start` or later.
    skips: Vec<Range<usize>>,
    block: usize,
}

impl Part {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "times and rates are positive and far below the range of the target type"
    )]
    fn new(plan: &Plan, rate: u32, len: usize) -> Self {
        let at = |d: Duration| ((d.as_secs_f64() * f64::from(rate)) as usize).min(len);
        let end = plan.to.map_or(len, at);
        let start = at(plan.from).min(end);
        let mut skips: Vec<Range<usize>> = plan
            .skips
            .iter()
            .map(|s| at(s.start)..at(s.end))
            .filter(|s| s.start >= start && s.start < s.end)
            .collect();
        skips.sort_by_key(|s| s.start);
        Self {
            start,
            end,
            pos: start,
            looping: plan.looping,
            skips,
            block: (u64::from(rate) * BLOCK_MS / 1000).max(1) as usize,
        }
    }

    /// The next clip range to sound; `None` once the part is over (never while it loops, unless
    /// it is empty).
    fn next_block(&mut self) -> Option<Range<usize>> {
        loop {
            if self.pos >= self.end {
                if !self.looping || self.start >= self.end {
                    return None;
                }
                self.pos = self.start;
            }
            let skip = self.skips.iter().find(|s| s.end > self.pos);
            match skip {
                Some(s) if s.start <= self.pos => self.pos = s.end.min(self.end),
                _ => {
                    let stop = skip.map_or(self.end, |s| s.start.min(self.end));
                    let r = self.pos..(self.pos + self.block).min(stop);
                    self.pos = r.end;
                    return Some(r);
                }
            }
        }
    }
}

/// One block for the output: output frames, and the clip sample the first of them is.
#[derive(Debug, Default)]
struct Chunk {
    at: usize,
    samples: Vec<f32>,
}

/// Output frame of clip sample `i` when the clip is resampled from `from` to `to` Hz.
#[allow(
    clippy::cast_possible_truncation,
    reason = "a frame index of a clip in memory fits a usize"
)]
fn frame_of(i: usize, from: u32, to: u32) -> usize {
    (i as u64 * u64::from(to) / u64::from(from.max(1))) as usize
}

/// Clip range `r` resampled from `from` to `to` Hz, as the same range of one resampling of the
/// whole clip would be: resampled with [`MARGIN_MS`] of its neighbours, then cut back.
fn resampled(clip: &[f32], r: Range<usize>, from: u32, to: u32) -> Vec<f32> {
    if from == to {
        return clip[r].to_vec();
    }
    let margin = usize::try_from(u64::from(from) * MARGIN_MS / 1000).unwrap_or(0);
    let wide = r.start.saturating_sub(margin)..(r.end + margin).min(clip.len());
    let base = frame_of(wide.start, from, to);
    let Ok(out) = resample(&clip[wide], from, to) else {
        return Vec::new();
    };
    let first = frame_of(r.start, from, to) - base;
    let last = frame_of(r.end, from, to) - base;
    out.get(first..last.min(out.len()))
        .map(<[f32]>::to_vec)
        .unwrap_or_default()
}

/// Sends the blocks of `part` to the output, resampled from `from` to `to` Hz, until the part is
/// over or the output is gone.
fn produce(clip: &[f32], mut part: Part, from: u32, to: u32, tx: &SyncSender<Chunk>) {
    while let Some(r) = part.next_block() {
        let chunk = Chunk {
            at: r.start,
            samples: resampled(clip, r, from, to),
        };
        if tx.send(chunk).is_err() {
            return;
        }
    }
}

impl Playback {
    /// Plays the part `from..to` of the mono `clip` (`to` `None`: to its end) at `rate` Hz,
    /// `speed` times as fast, and again from `from` while `looping`, jumping over `skips` (clip
    /// time) that start at `from` or later. Returns at once: the clip is resampled a block at a
    /// time on a thread of its own, only the part that is played.
    ///
    /// # Errors
    /// As [`play`].
    pub fn start(
        clip: Arc<[f32]>,
        rate: u32,
        from: Duration,
        to: Option<Duration>,
        speed: f32,
        looping: bool,
        skips: &[Range<Duration>],
    ) -> Result<Self, CaptureError> {
        let plan = Plan {
            from,
            to,
            speed,
            looping,
            skips: skips.to_vec(),
        };
        open(clip, rate, &plan)
    }

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

    /// Where the sound is in the clip's own time (not the sped-up one); held at the end of the
    /// plan once it is over.
    #[must_use]
    pub fn position(&self) -> Duration {
        Duration::from_micros(self.shared.at_us.load(Ordering::Relaxed))
    }

    /// The plan played to its end and the device sounded its buffer; never while it loops.
    #[must_use]
    pub fn finished(&self) -> bool {
        !self.looping && self.shared.done.load(Ordering::Relaxed)
    }
}

/// Play mono `clip` at `rate` Hz on the default output, on every channel.
///
/// # Errors
/// [`CaptureError::NoOutput`] without an output device; device and resampler errors.
pub fn play(clip: &[f32], rate: u32) -> Result<Playback, CaptureError> {
    open(clip.into(), rate, &Plan::default())
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "rates and speeds are positive and far below the range of the target type"
)]
fn open(clip: Arc<[f32]>, rate: u32, plan: &Plan) -> Result<Playback, CaptureError> {
    let dev = cpal::default_host()
        .default_output_device()
        .ok_or(CaptureError::NoOutput)?;
    let supported = dev.default_output_config()?;
    let device_rate = supported.sample_rate();
    // Resampled from `speed` times the clip's rate, the clip comes out shorter or longer.
    let fast = (f64::from(rate) * f64::from(plan.speed.max(0.1))).round() as u32;
    // The rates are checked here, so the thread meets no error it could not report.
    resample(&[], fast, device_rate)?;
    let part = Part::new(plan, rate, clip.len());
    let (tx, feed) = sync_channel(AHEAD);
    let src = Source::new(feed, &part, (rate, fast, device_rate), supported.channels());
    let shared = Arc::clone(&src.shared);
    thread::Builder::new()
        .name("playback-feed".into())
        .spawn(move || produce(&clip, part, fast, device_rate, &tx))
        .map_err(CaptureError::Thread)?;
    let config = supported.config();
    let failed = Arc::new(Mutex::new(None));
    let latency_us = Arc::new(AtomicU64::new(0));
    let refs = (&failed, &latency_us);
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build::<f32>(&dev, config, src, refs),
        SampleFormat::F64 => build::<f64>(&dev, config, src, refs),
        SampleFormat::I8 => build::<i8>(&dev, config, src, refs),
        SampleFormat::I16 => build::<i16>(&dev, config, src, refs),
        SampleFormat::I32 => build::<i32>(&dev, config, src, refs),
        SampleFormat::U8 => build::<u8>(&dev, config, src, refs),
        SampleFormat::U16 => build::<u16>(&dev, config, src, refs),
        SampleFormat::U32 => build::<u32>(&dev, config, src, refs),
        other => return Err(CaptureError::UnsupportedFormat(other.to_string())),
    }?;
    stream.play()?;
    Ok(Playback {
        _stream: stream,
        failed,
        latency_us,
        shared,
        looping: plan.looping,
    })
}

/// The blocks the feed thread makes, as the output callback plays them.
struct Source {
    feed: Receiver<Chunk>,
    chunk: Chunk,
    /// The next frame of `chunk` to play.
    idx: usize,
    channels: usize,
    /// Clip samples per output frame, and the clip's rate.
    step: f64,
    rate: f64,
    /// The feed is over; then `tail` frames of silence the device still sounds.
    ended: bool,
    tail: usize,
    shared: Arc<Shared>,
}

impl Source {
    /// `rates`: the clip's, the sped-up one it is resampled from, the device's.
    fn new(feed: Receiver<Chunk>, part: &Part, rates: (u32, u32, u32), channels: u16) -> Self {
        let (rate, fast, device) = rates;
        let me = Self {
            feed,
            chunk: Chunk {
                at: part.start,
                samples: Vec::new(),
            },
            idx: 0,
            channels: usize::from(channels).max(1),
            step: f64::from(fast) / f64::from(device.max(1)),
            rate: f64::from(rate.max(1)),
            ended: false,
            tail: (device / 3) as usize,
            shared: Arc::default(),
        };
        me.tell();
        me
    }

    /// Fill one interleaved buffer: each frame on every channel; a frame not made yet, and every
    /// frame after the end of the part, is silence. The position runs on through the device's
    /// tail, so the owner sees when it is over.
    fn fill<T: SizedSample + FromSample<f32>>(&mut self, out: &mut [T]) {
        for frame in out.chunks_mut(self.channels) {
            if self.idx >= self.chunk.samples.len() && !self.ended {
                match self.feed.try_recv() {
                    Ok(chunk) => {
                        self.chunk = chunk;
                        self.idx = 0;
                    }
                    Err(TryRecvError::Disconnected) => self.ended = true,
                    Err(TryRecvError::Empty) => {}
                }
            }
            let s = if let Some(s) = self.chunk.samples.get(self.idx) {
                self.idx += 1;
                *s
            } else {
                if self.ended {
                    self.tail = self.tail.saturating_sub(1);
                }
                0.0
            };
            frame.fill(T::from_sample(s));
        }
        self.tell();
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "clip positions are positive and far below 2^52 samples"
    )]
    fn tell(&self) {
        let clip = self.chunk.at as f64 + self.idx as f64 * self.step;
        let us = (clip / self.rate * 1e6) as u64;
        self.shared.at_us.store(us, Ordering::Relaxed);
        if self.ended && self.tail == 0 {
            self.shared.done.store(true, Ordering::Relaxed);
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

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// The clip samples a plan sounds, at 1000 Hz (a sample is a millisecond), at most `cap`.
    fn sounded(clip: &[f32], plan: &Plan, cap: usize) -> Vec<f32> {
        let mut part = Part::new(plan, 1000, clip.len());
        let mut out = Vec::new();
        while let Some(r) = part.next_block() {
            out.extend_from_slice(&clip[r]);
            if out.len() >= cap {
                out.truncate(cap);
                break;
            }
        }
        out
    }

    fn ramp(n: usize) -> Vec<f32> {
        (0..n).map(|i| i as f32).collect()
    }

    /// A source fed `chunks` at 1000 Hz on both sides, `speed` 1.
    fn source(chunks: Vec<Chunk>, start: usize) -> Source {
        let (tx, rx) = sync_channel(chunks.len());
        for c in chunks {
            tx.send(c).unwrap();
        }
        let part = Part {
            start,
            ..Part::new(&Plan::default(), 1000, 0)
        };
        Source::new(rx, &part, (1000, 1000, 1000), 1)
    }

    fn played(src: &mut Source, frames: usize) -> Vec<f32> {
        let mut out = vec![9.0_f32; frames];
        src.fill(&mut out);
        out
    }

    #[test]
    fn fill_copies_each_sample_to_every_channel_then_silence() {
        let chunk = Chunk {
            at: 0,
            samples: vec![0.5, -0.5, 0.25],
        };
        let mut src = source(vec![chunk], 0);
        src.channels = 2;
        let mut out = [9_i16; 4];
        src.fill(&mut out);
        assert_eq!(out, [16_384, 16_384, -16_384, -16_384]);
        src.fill(&mut out);
        assert_eq!(out, [8_192, 8_192, 0, 0]);
    }

    #[test]
    fn a_plan_plays_its_part_and_reports_the_clip_time_then_its_end() {
        let clip = ramp(10);
        let plan = Plan {
            from: ms(3),
            to: Some(ms(6)),
            ..Plan::default()
        };
        assert_eq!(sounded(&clip, &plan, 100), [3.0, 4.0, 5.0]);
        // A start past the end of the clip plays nothing.
        let past = Plan {
            from: ms(99),
            ..Plan::default()
        };
        assert_eq!(sounded(&clip, &past, 100), [0.0_f32; 0]);

        let chunk = Chunk {
            at: 3,
            samples: vec![3.0, 4.0, 5.0],
        };
        let mut src = source(vec![chunk], 3);
        assert_eq!(
            src.shared.at_us.load(Ordering::Relaxed),
            3000,
            "the start before a sound"
        );
        assert_eq!(played(&mut src, 5), [3.0, 4.0, 5.0, 0.0, 0.0]);
        assert_eq!(
            src.shared.at_us.load(Ordering::Relaxed),
            6000,
            "held at the end"
        );
        // The device's tail (a third of a second) is not over yet; then it is done.
        assert!(!src.shared.done.load(Ordering::Relaxed));
        played(&mut src, 400);
        assert!(src.shared.done.load(Ordering::Relaxed));
    }

    #[test]
    fn a_frame_not_made_yet_is_silence_and_does_not_move_the_position() {
        let (tx, rx) = sync_channel(1);
        let part = Part::new(&Plan::default(), 1000, 10);
        let mut src = Source::new(rx, &part, (1000, 1000, 1000), 1);
        assert_eq!(played(&mut src, 2), [0.0, 0.0]);
        assert_eq!(src.shared.at_us.load(Ordering::Relaxed), 0);
        assert!(
            !src.shared.done.load(Ordering::Relaxed),
            "the feed is still there"
        );
        tx.send(Chunk {
            at: 0,
            samples: vec![1.0],
        })
        .unwrap();
        assert_eq!(played(&mut src, 2), [1.0, 0.0]);
    }

    #[test]
    fn a_loop_starts_again_at_its_start() {
        let plan = Plan {
            from: ms(2),
            to: Some(ms(4)),
            looping: true,
            ..Plan::default()
        };
        assert_eq!(sounded(&ramp(10), &plan, 5), [2.0, 3.0, 2.0, 3.0, 2.0]);
        // An empty loop ends instead of spinning.
        let empty = Plan {
            from: ms(4),
            ..plan
        };
        assert_eq!(sounded(&ramp(10), &empty, 5), [0.0_f32; 0]);
    }

    #[test]
    fn speed_maps_output_frames_to_clip_time() {
        // At 2x a 1000 Hz clip is resampled from 2000 Hz: an output frame is two clip samples.
        let part = Part::new(&Plan::default(), 1000, 1000);
        let (_tx, rx) = sync_channel(1);
        let mut src = Source::new(rx, &part, (1000, 2000, 1000), 1);
        src.chunk = Chunk {
            at: 400,
            samples: vec![0.0; 200],
        };
        src.idx = 100;
        src.tell();
        assert_eq!(src.shared.at_us.load(Ordering::Relaxed), 600_000);
        assert_eq!(frame_of(300, 2000, 1000), 150);
    }

    #[test]
    fn skips_are_jumped_and_a_start_inside_one_plays_from_there() {
        let clip = ramp(20);
        let skips = vec![ms(0)..ms(2), ms(5)..ms(8), ms(12)..ms(15)];
        let off = Plan::default();
        assert_eq!(
            sounded(&clip, &off, 100),
            clip,
            "switch off: the clip as it is"
        );
        let on = Plan {
            skips: skips.clone(),
            ..Plan::default()
        };
        let want: Vec<f32> = clip
            .iter()
            .enumerate()
            .filter(|(i, _)| !skips.iter().any(|s| s.contains(&ms(*i as u64))))
            .map(|(_, s)| *s)
            .collect();
        assert_eq!(sounded(&clip, &on, 100), want);
        // A start inside the second skip plays from there; the third is still jumped.
        let seek = Plan {
            from: ms(6),
            skips,
            ..Plan::default()
        };
        assert_eq!(
            sounded(&clip, &seek, 100),
            [6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 15.0, 16.0, 17.0, 18.0, 19.0]
        );
    }

    #[test]
    fn blocks_resampled_apart_join_like_one_resampling() {
        let tone: Vec<f32> = (0..16_000)
            .map(|i| 0.5 * (std::f32::consts::TAU * 440.0 * i as f32 / 16_000.0).sin())
            .collect();
        let whole = resample(&tone, 16_000, 48_000).unwrap();
        let mut joined = resampled(&tone, 0..7_001, 16_000, 48_000);
        joined.extend(resampled(&tone, 7_001..16_000, 16_000, 48_000));
        assert_eq!(joined.len(), whole.len());
        // Away from the clip's own ends the seam is not heard.
        let worst = (1_000..47_000)
            .map(|i| (joined[i] - whole[i]).abs())
            .fold(0.0_f32, f32::max);
        assert!(worst < 1e-3, "{worst}");
        assert_eq!(resampled(&tone, 10..13, 16_000, 16_000), &tone[10..13]);
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
