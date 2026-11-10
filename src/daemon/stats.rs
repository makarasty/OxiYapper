//! Usage stats: a thread that adds one row per finished dictation to the stats file, and the
//! summary the IPC thread reads from that file. Only counts reach this module: the coordinator
//! sends word count and audio length, never the text (SPEC, History).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};

use oxi_core::stats::{self, Date, HEAT_WEEKS, Stats, Summary};
use oxi_core::types::Millis;
use oxi_platform::stats_store;

/// One finished dictation.
struct Entry {
    words: u64,
    speech: Millis,
}

/// Entries kept while the file cannot be loaded; the oldest go first.
const MAX_PENDING: usize = 64;

/// The stats file and the entries not yet counted in it.
struct Writer {
    path: PathBuf,
    /// Loaded on the first entry, and again after a failed load.
    stats: Option<Stats>,
    pending: VecDeque<Entry>,
}

impl Writer {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            stats: None,
            pending: VecDeque::new(),
        }
    }

    /// Counts `entry` and any earlier one the file could not take. A file that cannot be loaded
    /// keeps them for the next call.
    fn add(&mut self, entry: Entry, today: Date) {
        if self.pending.len() == MAX_PENDING {
            self.pending.pop_front();
        }
        self.pending.push_back(entry);
        if self.stats.is_none() {
            self.stats = stats_store::load(&self.path)
                .inspect_err(|e| tracing::warn!(kind = ?e.kind(), "stats not loaded"))
                .ok();
        }
        let Some(stats) = self.stats.as_mut() else {
            return;
        };
        for e in self.pending.drain(..) {
            stats.record(today, e.words, e.speech);
        }
        // A file of a newer build is counted in memory only.
        if stats.writable()
            && let Err(e) = stats_store::save(&self.path, stats)
        {
            tracing::warn!(kind = ?e.kind(), "stats not saved");
        }
    }
}

/// Sends entries to the stats thread. Dropping it waits for the last write.
pub struct Recorder {
    tx: Option<Sender<Entry>>,
    thread: Option<JoinHandle<()>>,
}

impl Recorder {
    /// The stats thread for the file at `path`; `today` is the local date.
    pub fn spawn(path: PathBuf, today: fn() -> Date) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel::<Entry>();
        let thread = thread::Builder::new().name("stats".into()).spawn(move || {
            let mut writer = Writer::new(path);
            for entry in rx {
                writer.add(entry, today());
            }
        })?;
        Ok(Self {
            tx: Some(tx),
            thread: Some(thread),
        })
    }

    /// The recorder of the daemon's data dir. Unit tests get none, so they never write the
    /// user's own file.
    pub fn system() -> Option<Self> {
        if cfg!(test) {
            return None;
        }
        Self::spawn(path()?, stats_store::today)
            .inspect_err(|e| tracing::warn!(kind = ?e.kind(), "stats thread not started"))
            .ok()
    }

    pub fn add(&self, words: u64, speech: Millis) {
        if let Some(tx) = &self.tx {
            // A panicked stats thread loses the count; dictation goes on.
            let _ = tx.send(Entry { words, speech });
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The stats file of the daemon's data dir.
pub fn path() -> Option<PathBuf> {
    oxi_platform::dirs::layout()
        .inspect_err(|e| tracing::warn!(kind = ?e.kind(), "no data dir for stats"))
        .ok()
        .map(|layout| layout.data_dir.join(stats::FILE))
}

/// The Home page numbers from the file at `path` (empty without one, or when it is corrupt:
/// the stats thread resets it on the next dictation).
pub fn summary(path: Option<&Path>, today: Date) -> Summary {
    path.map(stats_store::read)
        .transpose()
        .unwrap_or_else(|e| {
            tracing::warn!(kind = ?e.kind(), "stats not read");
            None
        })
        .unwrap_or_default()
        .summary(today, HEAT_WEEKS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::scratch::scratch_dir;

    #[test]
    fn entries_land_in_the_file_and_the_summary() {
        let dir = scratch_dir(&format!("oxi-dstats-{}", std::process::id()));
        let path = dir.join(stats::FILE);
        let today = || Date::from_ymd(2026, 11, 7).unwrap();
        let recorder = Recorder::spawn(path.clone(), today).unwrap();
        recorder.add(3, Millis(1_200));
        recorder.add(2, Millis(800));
        drop(recorder); // waits for the writes
        let sum = summary(Some(&path), today());
        assert_eq!(
            (sum.dictations, sum.words, sum.wpm, sum.streak),
            (2, 5, 150, 1)
        );
        assert_eq!(
            summary(None, today()),
            Stats::default().summary(today(), HEAT_WEEKS)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn entries_wait_for_a_file_that_cannot_be_loaded() {
        let dir = scratch_dir(&format!("oxi-dstats-wait-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(stats::FILE)).unwrap(); // reading a directory fails
        let today = Date::from_ymd(2026, 11, 7).unwrap();
        let mut writer = Writer::new(dir.join(stats::FILE));
        writer.add(
            Entry {
                words: 3,
                speech: Millis(1_000),
            },
            today,
        );
        assert_eq!(writer.pending.len(), 1);
        std::fs::remove_dir(dir.join(stats::FILE)).unwrap();
        writer.add(
            Entry {
                words: 2,
                speech: Millis(1_000),
            },
            today,
        );
        assert!(writer.pending.is_empty());
        let sum = summary(Some(&dir.join(stats::FILE)), today);
        assert_eq!((sum.dictations, sum.words), (2, 5));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
