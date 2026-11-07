//! I/O worker: model download (ARCHITECTURE section 8). History writes and the update check join
//! it later.
//!
//! The worker thread only routes [`IoCmd`]s. Each download runs on its own thread, so a
//! `CancelDownload` is heard while the transfer runs. Progress and the end go to the settings
//! window as pushes; with no window open they are dropped and the download carries on. A window
//! that connects gets the last progress of each running download (`IoCmd::Subscribed`).

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use oxi_core::ipc::{DownloadResult, Push};
use oxi_core::models::{self, ModelEntry};

use super::cmd::IoCmd;
use super::ipc::Pushes;
use crate::net::{self, DownloadError, Downloaded};

/// Shortest gap between two progress pushes (10 a second).
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// [`net::download`], or a fake in the tests.
type DownloadFn = dyn Fn(
        &ModelEntry,
        &Path,
        &mut dyn FnMut(u64, u64),
        &AtomicBool,
    ) -> Result<Downloaded, DownloadError>
    + Send
    + Sync;

/// Starts the worker thread. Returns when `rx` closes; running downloads end on their own.
pub fn spawn(
    rx: Receiver<IoCmd>,
    pushes: Pushes,
    models_dir: PathBuf,
) -> std::io::Result<JoinHandle<()>> {
    let worker = Worker::new(Arc::new(net::download), pushes, models_dir);
    thread::Builder::new().name("io".into()).spawn(move || {
        for cmd in rx {
            worker.handle(cmd);
        }
    })
}

struct Worker {
    download: Arc<DownloadFn>,
    pushes: Pushes,
    models_dir: PathBuf,
    /// Every running download, by model id.
    active: Arc<Mutex<HashMap<String, Arc<Running>>>>,
}

/// One running download: its cancel flag and the last progress it reported.
struct Running {
    cancel: AtomicBool,
    done: AtomicU64,
    total: AtomicU64,
}

impl Worker {
    fn new(download: Arc<DownloadFn>, pushes: Pushes, models_dir: PathBuf) -> Self {
        Self {
            download,
            pushes,
            models_dir,
            active: Arc::default(),
        }
    }

    fn handle(&self, cmd: IoCmd) {
        match cmd {
            IoCmd::Download { id } => self.start(id),
            IoCmd::CancelDownload { id } => {
                if let Some(run) = lock(&self.active).get(&id) {
                    run.cancel.store(true, Ordering::Relaxed);
                }
            }
            IoCmd::Subscribed => self.replay(),
        }
    }

    fn start(&self, id: String) {
        let Some(entry) = models::find(&id) else {
            self.failed(id, "unknown model".into());
            return;
        };
        let run = Arc::new(Running {
            cancel: AtomicBool::new(false),
            done: AtomicU64::new(0),
            total: AtomicU64::new(entry.size),
        });
        match lock(&self.active).entry(id.clone()) {
            // Already running: its progress reaches the window anyway, and its cancel flag stays.
            Entry::Occupied(_) => return,
            Entry::Vacant(slot) => {
                slot.insert(run.clone());
            }
        }
        let (download, pushes, dir, active) = (
            self.download.clone(),
            self.pushes.clone(),
            self.models_dir.clone(),
            self.active.clone(),
        );
        let job = move || {
            let result = transfer(&*download, entry, &dir, &pushes, &run);
            // Free the id before the push: a window that asks again on `Done` must be heard.
            lock(&active).remove(entry.id);
            pushes.send(Push::DownloadDone {
                id: entry.id.into(),
                result,
            });
        };
        if let Err(e) = thread::Builder::new().name("download".into()).spawn(job) {
            lock(&self.active).remove(&id);
            self.failed(id, format!("no thread: {e}"));
        }
    }

    /// The last progress of each running download, for a window that just connected. Under the
    /// lock: a download leaves the map before its `DownloadDone`, so none of these can follow it.
    fn replay(&self) {
        for (id, run) in lock(&self.active).iter() {
            self.pushes.send(Push::DownloadProgress {
                id: id.clone(),
                done: run.done.load(Ordering::Relaxed),
                total: run.total.load(Ordering::Relaxed),
            });
        }
    }

    fn failed(&self, id: String, message: String) {
        self.pushes.send(Push::DownloadDone {
            id,
            result: DownloadResult::Failed { message },
        });
    }
}

fn transfer(
    download: &DownloadFn,
    entry: &'static ModelEntry,
    dir: &Path,
    pushes: &Pushes,
    run: &Running,
) -> DownloadResult {
    let mut throttle = Throttle::default();
    let mut progress = |done, total| {
        run.done.store(done, Ordering::Relaxed);
        run.total.store(total, Ordering::Relaxed);
        if throttle.allow(Instant::now(), done, total) {
            pushes.send(Push::DownloadProgress {
                id: entry.id.into(),
                done,
                total,
            });
        }
    };
    match download(entry, dir, &mut progress, &run.cancel) {
        Ok(got) => {
            tracing::info!(model = entry.id, file = ?got.path.file_name(), "model downloaded");
            if let Some(e) = got.stamp_error {
                tracing::warn!(model = entry.id, "stamp not written: {e}");
            }
            DownloadResult::Ok
        }
        Err(DownloadError::Cancelled) => DownloadResult::Cancelled,
        Err(e) => {
            tracing::warn!(model = entry.id, "download failed: {e}");
            DownloadResult::Failed {
                message: e.to_string(),
            }
        }
    }
}

/// Lets a progress report through at most every [`PROGRESS_EVERY`], and always the last one.
#[derive(Default)]
struct Throttle {
    last: Option<Instant>,
}

impl Throttle {
    fn allow(&mut self, now: Instant, done: u64, total: u64) -> bool {
        let pass = done >= total
            || self
                .last
                .is_none_or(|t| now.duration_since(t) >= PROGRESS_EVERY);
        if pass {
            self.last = Some(now);
        }
        pass
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Receiver};

    use super::*;

    const WAIT: Duration = Duration::from_secs(3);

    fn rig(download: Arc<DownloadFn>) -> (Worker, Receiver<Push>) {
        let pushes = Pushes::default();
        let (_, rx) = pushes.claim(0);
        (
            Worker::new(download, pushes, PathBuf::from("models-test")),
            rx,
        )
    }

    fn downloaded(entry: &ModelEntry, dir: &Path) -> Downloaded {
        Downloaded {
            path: dir.join(entry.file_name),
            stamp_error: None,
        }
    }

    fn id(name: &str) -> IoCmd {
        IoCmd::Download { id: name.into() }
    }

    #[test]
    fn throttle_passes_ten_a_second_and_the_last() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut t = Throttle::default();
        let got: Vec<bool> = [(0, 1), (50, 2), (99, 3), (100, 4), (150, 5), (160, 10)]
            .into_iter()
            .map(|(at, done)| t.allow(ms(at), done, 10))
            .collect();
        assert_eq!(got, [true, false, false, true, false, true]);
    }

    #[test]
    fn progress_arrives_in_order_then_done() {
        let fake: Arc<DownloadFn> = Arc::new(|entry, dir, progress, _| {
            for done in [10, 20, 30] {
                progress(done, 40);
                // Past the throttle window, so none is dropped.
                thread::sleep(PROGRESS_EVERY);
            }
            Ok(downloaded(entry, dir))
        });
        let (worker, rx) = rig(fake);
        worker.handle(id("tiny"));
        let mut seen = vec![];
        loop {
            match rx.recv_timeout(WAIT).unwrap() {
                Push::DownloadProgress { id, done, total } => {
                    assert_eq!((id.as_str(), total), ("tiny", 40));
                    seen.push(done);
                }
                Push::DownloadDone { id, result } => {
                    assert_eq!((id.as_str(), result), ("tiny", DownloadResult::Ok));
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(seen, [10, 20, 30]);
    }

    #[test]
    fn cancel_reaches_the_running_download() {
        let fake: Arc<DownloadFn> = Arc::new(|_, _, progress, cancel| {
            progress(1, 100);
            let start = Instant::now();
            while !cancel.load(Ordering::Relaxed) {
                assert!(start.elapsed() < WAIT, "cancel never came");
                thread::sleep(Duration::from_millis(5));
            }
            Err(DownloadError::Cancelled)
        });
        let (worker, rx) = rig(fake);
        worker.handle(id("tiny"));
        assert!(matches!(
            rx.recv_timeout(WAIT).unwrap(),
            Push::DownloadProgress { .. }
        ));
        worker.handle(IoCmd::CancelDownload { id: "tiny".into() });
        assert_eq!(
            rx.recv_timeout(WAIT).unwrap(),
            Push::DownloadDone {
                id: "tiny".into(),
                result: DownloadResult::Cancelled
            }
        );
        // The id is free again for a new transfer.
        assert!(lock(&worker.active).is_empty());
    }

    #[test]
    fn a_window_that_connects_mid_download_gets_its_progress() {
        let (go, gate) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let fake: Arc<DownloadFn> = Arc::new(move |entry, dir, progress, _| {
            progress(5, 40);
            progress(7, 40); // inside the throttle window: not pushed, still recorded
            let _ = lock(&gate).recv_timeout(WAIT);
            Ok(downloaded(entry, dir))
        });
        let (worker, old) = rig(fake);
        worker.handle(id("tiny"));
        assert!(matches!(
            old.recv_timeout(WAIT).unwrap(),
            Push::DownloadProgress { done: 5, .. }
        ));
        // The id is in the map before the first progress; wait for the second one to land.
        let deadline = Instant::now() + WAIT;
        while lock(&worker.active)["tiny"].done.load(Ordering::Relaxed) != 7 {
            assert!(Instant::now() < deadline, "second progress never recorded");
            thread::sleep(Duration::from_millis(5));
        }
        let (_, new) = worker.pushes.claim(0);
        worker.handle(IoCmd::Subscribed);
        assert_eq!(
            new.recv_timeout(WAIT).unwrap(),
            Push::DownloadProgress {
                id: "tiny".into(),
                done: 7,
                total: 40
            }
        );
        drop(go);
        assert!(matches!(
            new.recv_timeout(WAIT).unwrap(),
            Push::DownloadDone { .. }
        ));
        // Nothing running: a later window hears nothing.
        let (_, last) = worker.pushes.claim(0);
        worker.handle(IoCmd::Subscribed);
        assert!(last.recv_timeout(Duration::from_millis(200)).is_err());
    }

    #[test]
    fn failure_and_unknown_id_are_reported() {
        let fake: Arc<DownloadFn> = Arc::new(|_, _, _, _| Err(DownloadError::Http(503)));
        let (worker, rx) = rig(fake);
        worker.handle(id("no-such-model"));
        worker.handle(id("tiny"));
        let mut results = vec![];
        for _ in 0..2 {
            let Push::DownloadDone { id, result } = rx.recv_timeout(WAIT).unwrap() else {
                panic!("expected DownloadDone");
            };
            results.push((id, matches!(result, DownloadResult::Failed { .. })));
        }
        results.sort();
        assert_eq!(
            results,
            [("no-such-model".into(), true), ("tiny".into(), true)]
        );
    }

    #[test]
    fn a_second_request_for_a_running_id_is_ignored() {
        let (go, gate) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let runs = Arc::new(Mutex::new(0));
        let counted = runs.clone();
        let fake: Arc<DownloadFn> = Arc::new(move |entry, dir, _, cancel| {
            *lock(&counted) += 1;
            let _ = lock(&gate).recv_timeout(WAIT);
            if cancel.load(Ordering::Relaxed) {
                return Err(DownloadError::Cancelled);
            }
            Ok(downloaded(entry, dir))
        });
        let (worker, rx) = rig(fake);
        worker.handle(id("tiny"));
        worker.handle(id("tiny"));
        // The cancel reaches the running download, not a flag the second request left.
        worker.handle(IoCmd::CancelDownload { id: "tiny".into() });
        drop(go);
        assert_eq!(
            rx.recv_timeout(WAIT).unwrap(),
            Push::DownloadDone {
                id: "tiny".into(),
                result: DownloadResult::Cancelled
            }
        );
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
        assert_eq!(*lock(&runs), 1);
    }
}
