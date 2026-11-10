//! WAV reading and writing for recordings and bench input.
//!
//! Recordings are 16 kHz mono 16-bit PCM: half the size of f32 and plays everywhere. A file is
//! written as `<name>.wav.part`, finalized and synced, then renamed, so a crash never leaves a
//! half-written `.wav` (ARCHITECTURE section 9, History). While the microphone runs a [`WavSpool`]
//! streams the raw audio to such a `.wav.part`, so a crash keeps it (`docs/HISTORY-FORMAT.md`).

use std::fs::{File, OpenOptions};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use oxi_core::audio::{SAMPLE_RATE, downmix};
use oxi_core::history;
use oxi_core::paths::part_path;
use thiserror::Error;

use crate::fs::{local_now, write_via};

#[derive(Debug, Error)]
pub enum WavError {
    #[error("wav io")]
    Io(#[from] std::io::Error),
    #[error("wav format")]
    Hound(#[from] hound::Error),
    #[error("unsupported wav: {0}")]
    Unsupported(String),
}

/// Write 16 kHz mono `samples` (f32 in `[-1, 1]`, clamped) to `path`, which must end in `.wav`.
pub fn write_wav(path: &Path, samples: &[f32]) -> Result<(), WavError> {
    if path.extension().is_none_or(|e| e != "wav") {
        return Err(WavError::Unsupported("path must end in .wav".into()));
    }
    write_via(&part_path(path), path, |file| write_into(file, samples))
}

fn write_into(file: &mut File, samples: &[f32]) -> Result<(), WavError> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut writer = WavWriter::new(BufWriter::new(file), spec)?;
    for &s in samples {
        writer.write_sample(pcm(s))?;
    }
    writer.finalize()?;
    Ok(())
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "clamped to the i16 range first"
)]
fn pcm(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16
}

/// The recording that grows on disk while the microphone runs, so a crash keeps what was said.
/// One process-wide state: the history worker names the folder ([`set_spool_dir`]), the capture
/// drain feeds a [`WavSpool`] whose own thread writes the file, and the worker takes the result by
/// its path ([`spool_take`]), which the job carries to the save, so each save gets its own recording.
#[derive(Default)]
struct SpoolState {
    /// Where a new recording goes; `None` while saving is off.
    dir: Option<PathBuf>,
    /// Files a recording or a writer thread still owns (recording, or being deleted).
    live: Vec<PathBuf>,
    /// Finished recordings waiting for their transcript.
    done: Vec<Finished>,
    /// Finished ones no transcript will come for: to be kept as failed.
    orphans: Vec<Finished>,
}

struct Finished {
    path: PathBuf,
    /// The writer thread, which may still be closing the file.
    writer: JoinHandle<()>,
}

impl Finished {
    /// The path once the writer has closed the file.
    fn close(self) -> PathBuf {
        let _ = self.writer.join();
        self.path
    }
}

static SPOOL: Mutex<SpoolState> = Mutex::new(SpoolState {
    dir: None,
    live: Vec::new(),
    done: Vec::new(),
    orphans: Vec::new(),
});

fn spool_state() -> MutexGuard<'static, SpoolState> {
    SPOOL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The day-folder root new recordings stream into (an owned history folder), or `None` for off.
pub fn set_spool_dir(dir: Option<PathBuf>) {
    spool_state().dir = dir;
}

/// Takes the finished recording at `path` (from [`WavSpool::finish`]) once, after its file is
/// closed; `false` when it is not waiting or its file was never made. The caller turns it into an
/// entry.
pub fn spool_take(path: &Path) -> bool {
    let spool = {
        let mut state = spool_state();
        let Some(i) = state.done.iter().position(|d| d.path == path) else {
            return false;
        };
        state.done.remove(i)
    };
    spool.close().is_file()
}

/// No transcript will come for the recording at `path` (it failed, came out empty or was
/// dropped): it is kept as failed. Returns whether there was one.
pub fn spool_give_up(path: &Path) -> bool {
    let mut state = spool_state();
    let Some(i) = state.done.iter().position(|d| d.path == path) else {
        return false;
    };
    let spool = state.done.remove(i);
    state.orphans.push(spool);
    true
}

/// The daemon is stopping: every recording still waiting for a transcript will never get one.
pub fn spool_give_up_all() {
    let mut state = spool_state();
    let done = std::mem::take(&mut state.done);
    state.orphans.extend(done);
}

/// Finished spools that never got a transcript, once, after their files are closed.
pub fn spool_take_orphans() -> Vec<PathBuf> {
    let orphans = std::mem::take(&mut spool_state().orphans);
    orphans.into_iter().map(Finished::close).collect()
}

/// Is `path` a spool this process is still writing or has not handed over yet? Recovery skips it.
pub fn spool_busy(path: &Path) -> bool {
    let state = spool_state();
    state.live.iter().any(|p| p == path)
        || state
            .done
            .iter()
            .chain(&state.orphans)
            .any(|d| d.path == path)
}

/// Audio is flushed and synced to the disk (and the header made true) after this many seconds of
/// it: a kill or a power cut loses at most about that much.
const FLUSH_SECONDS: usize = 1;

/// Windows the drain may queue for the writer thread: 1500 windows of 20 ms, 30 s of audio. A disk
/// stalled longer than that stops the file growing; the recording itself goes on in memory.
const QUEUE: usize = 1500;

/// One recording being streamed to `<dir>/<day>/<stem>.wav.part`: mono 16-bit at the device rate.
/// The caller (the capture drain) never touches the disk: a thread of its own writes the file.
pub struct WavSpool {
    /// Held until the recording ends, also after the writer stopped taking audio: the writer
    /// leaves `live` only when this is dropped, so recovery never takes a file still recording.
    tx: SyncSender<Vec<i16>>,
    /// The writer stopped taking audio (the disk stalled or failed): no more is queued.
    stalled: bool,
    writer: JoinHandle<()>,
    discard: Arc<AtomicBool>,
    path: PathBuf,
}

impl WavSpool {
    /// Starts a spool for a recording at `rate` Hz; `None` when saving is off or no thread can be
    /// started (logged, the recording itself goes on).
    pub fn begin(rate: u32) -> Option<Self> {
        let mut state = spool_state();
        let dir = state.dir.clone()?;
        let (date, ms) = local_now();
        let path = dir.join(format!("{}.wav.part", history::stem(date, ms)));
        let (tx, rx) = sync_channel(QUEUE);
        let discard = Arc::new(AtomicBool::new(false));
        let writer = {
            let (path, discard) = (path.clone(), Arc::clone(&discard));
            // It takes the state lock to leave `live`, so it waits until `begin` added it there.
            thread::Builder::new()
                .name("oxi-spool".into())
                .spawn(move || write_spool(&path, rate, &rx, &discard))
        };
        let writer = writer
            .inspect_err(|e| tracing::warn!("recording not streamed to disk: {e}"))
            .ok()?;
        state.live.push(path.clone());
        Some(Self {
            tx,
            stalled: false,
            writer,
            discard,
            path,
        })
    }

    /// The `.wav.part` this spool writes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Queues mono samples (f32 in `[-1, 1]`, clamped) for the file; never waits for the disk.
    /// When the writer cannot keep up or failed, the file keeps what it has and takes no more.
    pub fn push(&mut self, mono: &[f32]) {
        if self.stalled {
            return;
        }
        if let Err(e) = self.tx.try_send(mono.iter().map(|&s| pcm(s)).collect()) {
            if matches!(e, TrySendError::Full(_)) {
                tracing::warn!("disk too slow: recording stream stopped");
            }
            self.stalled = true;
        }
    }

    /// The recording ended and its processed audio goes to be transcribed: it waits, as
    /// `.wav.part`, for its transcript. Returns its path, for [`spool_take`] or [`spool_give_up`].
    #[must_use = "only the path takes it out again; without it, it waits until the daemon quits"]
    pub fn finish(self) -> PathBuf {
        let path = self.path.clone();
        self.file(true);
        path
    }

    /// The recording ended but processing failed: no transcript will come, it is kept as failed.
    pub fn fail(self) {
        self.file(false);
    }

    fn file(self, waits: bool) {
        // Held until the spool is filed: the writer cannot leave `live` before, so the file is
        // busy all along.
        let mut state = spool_state();
        drop(self.tx);
        state.live.retain(|p| *p != self.path);
        let finished = Finished {
            path: self.path,
            writer: self.writer,
        };
        if waits {
            state.done.push(finished);
        } else {
            state.orphans.push(finished);
        }
    }

    /// Nothing worth keeping (cancelled, nothing heard): the writer deletes the file.
    pub fn discard(self) {
        self.discard.store(true, Ordering::Release);
    }
}

/// The writer thread: streams what the drain queues until it finishes, discards or is dropped
/// (a panic; the file then stays for recovery), and leaves `live` once the file is settled.
fn write_spool(path: &Path, rate: u32, rx: &Receiver<Vec<i16>>, discard: &AtomicBool) {
    if let Err(e) = stream_to(path, rate, rx, discard) {
        tracing::warn!("recording stream stopped: {e}");
    }
    // A failed write leaves the file as it is, but the recording goes on: wait until it ends, so
    // the file stays busy and recovery cannot make a second entry of it (only one save may).
    for _ in rx {}
    if discard.load(Ordering::Acquire) {
        let _ = std::fs::remove_file(path);
    }
    spool_state().live.retain(|p| p != path);
}

fn stream_to(
    path: &Path,
    rate: u32,
    rx: &Receiver<Vec<i16>>,
    discard: &AtomicBool,
) -> Result<(), WavError> {
    if let Some(day) = path.parent() {
        std::fs::create_dir_all(day)?;
    }
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let disk = file.try_clone()?;
    let spec = WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut writer = WavWriter::new(BufWriter::new(file), spec)?;
    let (mut unflushed, flush_every) = (0, rate as usize * FLUSH_SECONDS);
    for chunk in rx {
        if discard.load(Ordering::Acquire) {
            return Ok(());
        }
        for &s in &chunk {
            writer.write_sample(s)?;
        }
        unflushed += chunk.len();
        if unflushed >= flush_every {
            unflushed = 0;
            writer.flush()?;
            disk.sync_data()?; // the flushed audio survives a power cut
        }
    }
    if discard.load(Ordering::Acquire) {
        return Ok(());
    }
    writer.finalize()?;
    disk.sync_data()?;
    Ok(())
}

/// Read any int (8-32 bit) or f32 wav as mono f32 in `[-1, 1]` (channels averaged) plus its rate.
pub fn read_wav_mono_f32(path: &Path) -> Result<(Vec<f32>, u32), WavError> {
    let mut reader = WavReader::open(path)?;
    let spec = reader.spec();
    let interleaved: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (SampleFormat::Float, 32) => reader.samples::<f32>().collect::<Result<_, _>>()?,
        (SampleFormat::Int, bits @ 8..=32) => {
            let scale = 2.0_f32.powi(i32::from(bits) - 1);
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / scale))
                .collect::<Result<_, _>>()?
        }
        (format, bits) => {
            return Err(WavError::Unsupported(format!("{format:?} {bits} bit")));
        }
    };
    Ok((
        downmix(&interleaved, usize::from(spec.channels)),
        spec.sample_rate,
    ))
}

/// The spool state is process-wide: tests that use it run one at a time.
#[cfg(test)]
pub(crate) static SPOOL_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::scratch::scratch_dir;

    /// A fresh folder as the spool dir; the guard keeps the other tests out.
    fn setup(name: &str) -> (MutexGuard<'static, ()>, PathBuf) {
        let guard = SPOOL_TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let dir = scratch_dir(&format!("oxi-spool-{name}-{}", std::process::id()));
        set_spool_dir(Some(dir.clone()));
        spool_give_up_all();
        let _ = spool_take_orphans();
        (guard, dir)
    }

    fn tone(n: usize) -> Vec<f32> {
        (0..n).map(|i| (i % 100) as f32 / 200.0).collect()
    }

    /// Waits up to 5 s for the writer thread to get `path` to `bytes` long.
    fn wait_for_len(path: &Path, bytes: u64) {
        let start = Instant::now();
        while fs::metadata(path).map_or(0, |m| m.len()) < bytes {
            assert!(start.elapsed() < Duration::from_secs(5), "writer stuck");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_finished_spool_is_a_playable_wav_part_taken_by_its_path() {
        let (_guard, dir) = setup("finish");
        let mut first = WavSpool::begin(48_000).unwrap();
        let path = first.path().to_owned();
        assert!(spool_busy(&path));
        first.push(&tone(30_000));
        assert_eq!(first.finish(), path);
        assert!(spool_busy(&path), "done is still busy until taken");
        thread::sleep(Duration::from_millis(2)); // the next one gets its own name
        let second = WavSpool::begin(16_000).unwrap().finish();
        assert!(!spool_take(&dir.join("x.wav.part")), "no such recording");
        assert!(spool_take(&second), "a later one first");
        assert!(spool_take(&path));
        assert!(!spool_take(&path), "once");
        assert!(!spool_busy(&path));
        let (samples, rate) = read_wav_mono_f32(&path).unwrap();
        assert_eq!((samples.len(), rate), (30_000, 48_000));
        assert_eq!(spool_take_orphans(), Vec::<PathBuf>::new());
        set_spool_dir(None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_killed_recording_keeps_what_was_flushed_and_its_header_can_be_repaired() {
        let (_guard, dir) = setup("kill");
        let mut spool = WavSpool::begin(16_000).unwrap();
        let path = spool.path().to_owned();
        for piece in tone(40_000).chunks(320) {
            spool.push(piece); // flushed and synced at every 16 000 samples
        }
        wait_for_len(&path, (history::WAV_HEADER + 2 * 32_000) as u64);
        let bytes = fs::read(&path).unwrap(); // what a kill now leaves
        let fixed = history::repaired_header(&bytes, bytes.len() as u64).unwrap();
        assert!(fixed.data_bytes >= 2 * 32_000, "{}", fixed.data_bytes);
        assert_eq!(fixed.sample_rate, 16_000);
        let mut whole = bytes;
        whole[..history::WAV_HEADER].copy_from_slice(&fixed.header);
        whole.truncate(history::WAV_HEADER + usize::try_from(fixed.data_bytes).unwrap());
        let copy = dir.join("killed.wav");
        fs::write(&copy, whole).unwrap();
        let (samples, _) = read_wav_mono_f32(&copy).unwrap();
        assert_eq!(samples.len() as u64 * 2, fixed.data_bytes);
        assert!((samples[150] - 0.25).abs() < 1e-3, "{}", samples[150]);
        spool.discard();
        set_spool_dir(None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_discarded_spool_leaves_nothing_and_a_given_up_one_becomes_an_orphan() {
        let (_guard, dir) = setup("discard");
        let mut spool = WavSpool::begin(16_000).unwrap();
        let gone = spool.path().to_owned();
        spool.push(&tone(320));
        spool.discard();
        let start = Instant::now();
        while spool_busy(&gone) {
            assert!(start.elapsed() < Duration::from_secs(5), "writer stuck");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!gone.exists(), "removed before it stops being busy");

        let failed = WavSpool::begin(16_000).unwrap();
        let failed_path = failed.path().to_owned();
        failed.fail();
        assert!(spool_busy(&failed_path), "an orphan is busy until taken");
        thread::sleep(Duration::from_millis(2));
        let given_up_path = WavSpool::begin(16_000).unwrap().finish();
        assert!(!spool_give_up(&failed_path), "not waiting for a transcript");
        assert!(spool_give_up(&given_up_path));
        assert!(!spool_take(&given_up_path), "given up: no longer waits");
        assert_eq!(spool_take_orphans(), [failed_path, given_up_path]);
        set_spool_dir(None);
        assert!(WavSpool::begin(16_000).is_none(), "saving off: no spool");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The writer fails (here at once: the day folder cannot be made under a file; a stalled disk
    /// ends the same way) while the microphone still runs. Recovery must not see the file as left
    /// over, or the recording's own save makes a second entry of it.
    #[test]
    fn a_spool_whose_writer_failed_stays_busy_until_its_recording_ends() {
        let (_guard, dir) = setup("failed-write");
        let blocker = dir.join("not-a-folder");
        fs::write(&blocker, b"x").unwrap();
        set_spool_dir(Some(blocker));
        let mut spool = WavSpool::begin(16_000).unwrap();
        let path = spool.path().to_owned();
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(200) {
            spool.push(&tone(320));
            assert!(spool_busy(&path), "still recording: busy");
            thread::sleep(Duration::from_millis(5));
        }
        spool.fail();
        assert_eq!(spool_take_orphans(), [path]);
        set_spool_dir(None);
        let _ = fs::remove_dir_all(&dir);
    }
}
