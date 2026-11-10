//! I/O worker: model download (ARCHITECTURE section 8). History writes and the update check join
//! it later.
//!
//! The worker thread only routes [`IoCmd`]s. Each download runs on its own thread, so a
//! `CancelDownload` is heard while the transfer runs. Progress and the end go to the settings
//! window as pushes; with no window open they are dropped and the download carries on. A window
//! that connects gets the last progress of each running download (`IoCmd::Subscribed`). The
//! coordinator hears of each finished download (`Inbox::ModelDownloaded`).

use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use oxi_core::ipc::{DeleteResult, DownloadResult, Push};
use oxi_core::models::{self, ModelEntry};
use oxi_core::paths::part_path;

use super::cmd::{Inbox, IoCmd};
use super::ipc::Pushes;
use super::lock;
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

/// The `.part` of every running download in the process. The whisper worker deletes and this
/// worker downloads: a delete of a model whose download runs answers `InUse`, and the lock is
/// held across the delete so no download starts in the middle of it.
static DOWNLOADING: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// Starts the worker thread. Returns when `rx` closes; running downloads end on their own.
pub fn spawn(
    rx: Receiver<IoCmd>,
    inbox: Sender<Inbox>,
    pushes: Pushes,
    models_dir: PathBuf,
) -> std::io::Result<JoinHandle<()>> {
    let worker = Worker::new(Arc::new(net::download), inbox, pushes, models_dir);
    thread::Builder::new().name("io".into()).spawn(move || {
        for cmd in rx {
            worker.handle(cmd);
        }
    })
}

struct Worker {
    download: Arc<DownloadFn>,
    /// The coordinator, told of each finished download.
    inbox: Sender<Inbox>,
    pushes: Pushes,
    models_dir: PathBuf,
    /// Every running download, by model id.
    active: Arc<Mutex<HashMap<String, Arc<Running>>>>,
}

/// One running download: its cancel flag and the last progress it reported.
struct Running {
    cancel: AtomicBool,
    /// Held across "record progress, then push it" and across "read progress, then replay it", so
    /// a replay never lands after a newer push.
    order: Mutex<()>,
    done: AtomicU64,
    total: AtomicU64,
}

impl Worker {
    fn new(
        download: Arc<DownloadFn>,
        inbox: Sender<Inbox>,
        pushes: Pushes,
        models_dir: PathBuf,
    ) -> Self {
        Self {
            download,
            inbox,
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
            order: Mutex::new(()),
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
        let part = part_path(&self.models_dir.join(entry.file_name));
        lock(&DOWNLOADING).insert(part.clone());
        let (download, inbox, pushes, dir, active) = (
            self.download.clone(),
            self.inbox.clone(),
            self.pushes.clone(),
            self.models_dir.clone(),
            self.active.clone(),
        );
        let job = move || {
            let result = transfer(&*download, entry, &dir, &pushes, &run);
            // Free the id before the push: a window that asks again on `Done` must be heard.
            lock(&DOWNLOADING).remove(&part);
            lock(&active).remove(entry.id);
            // A send fails only when the coordinator is gone: nothing is left to retry then.
            if result == DownloadResult::Ok {
                let _ = inbox.send(Inbox::ModelDownloaded(entry.id.into()));
            }
            pushes.send(Push::DownloadDone {
                id: entry.id.into(),
                result,
            });
        };
        if let Err(e) = thread::Builder::new().name("download".into()).spawn(job) {
            lock(&DOWNLOADING).remove(&part_path(&self.models_dir.join(entry.file_name)));
            lock(&self.active).remove(&id);
            self.failed(id, format!("no thread: {e}"));
        }
    }

    /// The last progress of each running download, for a window that just connected. Under the
    /// lock: a download leaves the map before its `DownloadDone`, so none of these can follow it.
    fn replay(&self) {
        for (id, run) in lock(&self.active).iter() {
            let _ordered = lock(&run.order);
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

/// Removes a registry model's file, its `.ok` stamp and its `.part` from `dir`; a file that is
/// not there is fine. The caller has unloaded the model and refused a running dictation.
/// `freed` counts the bytes of what was removed. A download running for it answers `InUse`: the
/// window cancels it first.
pub fn delete_model(dir: &Path, entry: &ModelEntry) -> DeleteResult {
    let file = dir.join(entry.file_name);
    let downloading = lock(&DOWNLOADING);
    if downloading.contains(&part_path(&file)) {
        return DeleteResult::InUse;
    }
    let mut freed = 0;
    for path in [
        file.clone(),
        dir.join(models::stamp_file_name(entry.file_name)),
        part_path(&file),
    ] {
        let size = std::fs::metadata(&path).map_or(0, |m| m.len());
        match std::fs::remove_file(&path) {
            Ok(()) => freed += size,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(kind = ?e.kind(), model = entry.id, "model file not deleted");
                return DeleteResult::Failed {
                    message: e.to_string(),
                };
            }
        }
    }
    DeleteResult::Done { freed }
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
        let _ordered = lock(&run.order);
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

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Receiver};

    use super::*;
    use crate::log::scratch::scratch_dir;

    const WAIT: Duration = Duration::from_secs(3);

    fn rig(download: Arc<DownloadFn>) -> (Worker, Receiver<Push>) {
        let pushes = Pushes::default();
        let (_, rx) = pushes.claim(0);
        (
            Worker::new(
                download,
                mpsc::channel().0,
                pushes,
                PathBuf::from("models-test"),
            ),
            rx,
        )
    }

    fn downloaded(entry: &ModelEntry, dir: &Path) -> Downloaded {
        Downloaded {
            path: dir.join(entry.file_name),
            stamp_error: None,
        }
    }

    #[test]
    fn delete_removes_model_stamp_and_part_and_counts_bytes() {
        let dir = scratch_dir(&format!("oxi-io-delete-{}", std::process::id()));
        let entry = models::find("tiny").unwrap();
        let file = dir.join(entry.file_name);
        std::fs::write(&file, [0; 10]).unwrap();
        std::fs::write(dir.join(models::stamp_file_name(entry.file_name)), [0; 2]).unwrap();
        std::fs::write(part_path(&file), [0; 5]).unwrap();
        let other = dir.join("keep.txt");
        std::fs::write(&other, "x").unwrap();
        assert_eq!(delete_model(&dir, entry), DeleteResult::Done { freed: 17 });
        assert!(!file.exists() && !part_path(&file).exists() && other.exists());
        // Only a partial download left: it goes too; nothing there is not an error.
        std::fs::write(part_path(&file), [0; 4]).unwrap();
        assert_eq!(delete_model(&dir, entry), DeleteResult::Done { freed: 4 });
        assert_eq!(delete_model(&dir, entry), DeleteResult::Done { freed: 0 });
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn delete_of_a_model_that_is_downloading_is_refused_until_it_ends() {
        let dir = scratch_dir(&format!("oxi-io-busy-{}", std::process::id()));
        let entry = models::find("tiny").unwrap();
        let part = part_path(&dir.join(entry.file_name));
        std::fs::write(&part, [0; 3]).unwrap();
        let (go, gate) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let fake: Arc<DownloadFn> = Arc::new(move |_, _, progress, _| {
            progress(3, 40);
            let _ = lock(&gate).recv_timeout(WAIT);
            Err(DownloadError::Cancelled)
        });
        let pushes = Pushes::default();
        let (_, rx) = pushes.claim(0);
        let worker = Worker::new(fake, mpsc::channel().0, pushes, dir.clone());
        worker.handle(id("tiny"));
        assert!(matches!(
            rx.recv_timeout(WAIT).unwrap(),
            Push::DownloadProgress { .. }
        ));
        assert_eq!(delete_model(&dir, entry), DeleteResult::InUse);
        assert!(part.exists(), "the running download's file stays");
        drop(go);
        assert!(matches!(
            rx.recv_timeout(WAIT).unwrap(),
            Push::DownloadDone { .. }
        ));
        assert_eq!(delete_model(&dir, entry), DeleteResult::Done { freed: 3 });
        std::fs::remove_dir_all(&dir).unwrap();
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
        let (inbox, told) = mpsc::channel();
        let worker = Worker { inbox, ..worker };
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
        assert!(
            matches!(told.recv_timeout(WAIT), Ok(Inbox::ModelDownloaded(id)) if id == "tiny"),
            "the coordinator hears of the download"
        );
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
    fn a_replay_waits_for_a_progress_that_is_mid_push() {
        let (go, gate) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let fake: Arc<DownloadFn> = Arc::new(move |entry, dir, progress, _| {
            progress(5, 40);
            let _ = lock(&gate).recv_timeout(WAIT);
            Ok(downloaded(entry, dir))
        });
        let (worker, old) = rig(fake);
        worker.handle(id("tiny"));
        let _ = old.recv_timeout(WAIT).unwrap();
        let (_, new) = worker.pushes.claim(0);
        let run = lock(&worker.active)["tiny"].clone();
        // The download thread is between its store and its push: replay must not read yet.
        let held = lock(&run.order);
        thread::scope(|s| {
            s.spawn(|| worker.handle(IoCmd::Subscribed));
            assert!(new.recv_timeout(Duration::from_millis(200)).is_err());
            drop(held);
            assert!(matches!(
                new.recv_timeout(WAIT).unwrap(),
                Push::DownloadProgress { done: 5, .. }
            ));
        });
        drop(go);
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
