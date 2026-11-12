//! Paste worker: clipboard and synthetic input (ARCHITECTURE section 10). It runs one command at
//! a time in arrival order, so two queued pastes never interleave. Dictated text is never logged.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, SyncSender, sync_channel};
use std::thread::{Builder, JoinHandle};
use std::time::{Duration, Instant};

use oxi_core::machine::{Event, JobId};
use oxi_core::paste::file;
use oxi_core::paste::{
    Clipboard, EnterSkip, Focus, Input, OurEntries, PasteOutcome, PasteStep, copy_only,
    enter_after, paste_with,
};
use oxi_core::stats::Date;
use oxi_platform::os::clipboard::WinClipboard;
use oxi_platform::os::input::Keyboard;

use super::cmd::{Inbox, PasteCmd, ToFile};

/// Typing progress goes to the coordinator at most this often, plus the last report.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);
/// To-file writes waiting at most; past that a dictation is not queued, never held.
const FILE_QUEUE: usize = 16;
/// At exit with no quit dictation (no `PasteCmd::File` deadline), how long queued to-file writes
/// are waited for. Inside the session-end budget (`run.rs`).
const EXIT_FILE_WAIT: Duration = Duration::from_secs(1);

/// `done` of `total` typed units as a percent, at most 99: 100 is the `Landed` report.
fn typed_percent(done: usize, total: usize) -> u8 {
    u8::try_from((done * 100 / total.max(1)).min(99)).unwrap_or(99)
}

/// Appends one dictation to the daily file of `day` in the folder of `to`, which must exist.
/// One `write_all`, so a crash leaves a whole line or none. Never the text in an error.
fn append(to: &ToFile, day: Date, ms_of_day: u32, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let path = to.folder.join(file::file_name(to.format, day));
    let line = file::line(to.format, ms_of_day, text);
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?
        .write_all(line.as_bytes())
}

/// The status line of a failed to-file write: its kind, never the text or the path.
fn file_warning(kind: std::io::ErrorKind) -> String {
    format!("{}: {kind}", file::WRITE_FAILED)
}

/// Appends one dictation to the file of today.
fn append_now(to: &ToFile, text: &str) -> std::io::Result<()> {
    let (day, ms) = oxi_platform::fs::local_now();
    append(to, day, ms, text)
}

/// One dictation for the to-file thread. `job` is `None` at quit: the coordinator is going, the
/// result is only logged. `only`: the write is the delivery, so the thread answers the job.
struct FileWrite {
    job: Option<JobId>,
    only: bool,
    text: String,
    to: ToFile,
}

/// The to-file thread: a folder on an offline share holds only this thread, never a paste
/// (review d04 #4). Every write tells its result (`Inbox::FileWarning`, the error kind, never the
/// text or the path), so a warning the coordinator cleared comes back at the next failure. Each
/// finished write is one `()` on `done`.
fn write_files(
    queue: &Receiver<FileWrite>,
    inbox: &Sender<Inbox>,
    done: &Sender<()>,
    write: impl Fn(&ToFile, &str) -> std::io::Result<()>,
) {
    for FileWrite {
        job,
        only,
        text,
        to,
    } in queue
    {
        let result = write(&to, &text);
        let _ = done.send(());
        let Some(job) = job else {
            match result {
                Ok(()) => tracing::info!("to-file written at quit"),
                Err(e) => tracing::warn!(kind = ?e.kind(), "to-file write failed at quit"),
            }
            continue;
        };
        let line = result.as_ref().err().map(|e| {
            tracing::warn!(kind = ?e.kind(), "to-file write failed");
            file_warning(e.kind())
        });
        let _ = inbox.send(Inbox::FileWarning { job, line });
        if only {
            let outcome = if result.is_ok() {
                let _ = inbox.send(Inbox::Progress { job, percent: 100 });
                PasteOutcome::Pasted
            } else {
                PasteOutcome::FileFailed
            };
            tracing::info!(?job, ?outcome, "to-file only");
            let _ = inbox.send(Inbox::Event(Event::PasteDone { job, outcome }));
        }
    }
}

/// The to-file thread, the one writer of every daily file, so lines land in dictation order.
struct Writer {
    files: SyncSender<FileWrite>,
    /// One `()` per finished write; closed when the thread ends.
    done: Receiver<()>,
    /// Writes queued and not yet finished.
    queued: usize,
    /// The quit grace (`PasteCmd::File`): how long the exit waits for the queue.
    until: Option<Instant>,
}

impl Writer {
    /// Starts the to-file thread; its queue closes at [`Writer::finish`]. With no thread every
    /// write is refused, as with a full queue.
    fn spawn(
        inbox: &Sender<Inbox>,
        write: impl Fn(&ToFile, &str) -> std::io::Result<()> + Send + 'static,
    ) -> Self {
        let (files, queue) = sync_channel(FILE_QUEUE);
        let (tell, done) = std::sync::mpsc::channel();
        let inbox = inbox.clone();
        let spawned = Builder::new()
            .name("to-file".into())
            .spawn(move || write_files(&queue, &inbox, &tell, write));
        if let Err(e) = spawned {
            tracing::warn!("to-file thread not started: {e}");
        }
        Self {
            files,
            done,
            queued: 0,
            until: None,
        }
    }

    /// Hands `write` to the to-file thread; a full queue (the folder hangs) or no thread drops
    /// it with a warning instead of waiting.
    fn queue(&mut self, inbox: &Sender<Inbox>, write: FileWrite) -> bool {
        self.queued -= self.done.try_iter().count();
        let job = write.job;
        let queued = self.files.try_send(write).is_ok();
        if queued {
            self.queued += 1;
        } else if let Some(job) = job {
            tracing::warn!(?job, "to-file queue full, not written");
            let line = Some(file_warning(std::io::ErrorKind::TimedOut));
            let _ = inbox.send(Inbox::FileWarning { job, line });
        } else {
            tracing::warn!("to-file queue full at quit, not written");
        }
        queued
    }

    /// Closes the queue and waits for the writes still in it, until the quit grace (or
    /// [`EXIT_FILE_WAIT`] with no quit dictation); a folder that hangs past that is left to the
    /// process exit. Answers how many writes were not done.
    fn finish(self) -> usize {
        let Self {
            files,
            done,
            mut queued,
            until,
        } = self;
        drop(files);
        let until = until.unwrap_or_else(|| Instant::now() + EXIT_FILE_WAIT);
        while queued > 0
            && done
                .recv_timeout(until.saturating_duration_since(Instant::now()))
                .is_ok()
        {
            queued -= 1;
        }
        if queued > 0 {
            tracing::warn!(unwritten = queued, "to-file writes outlived the quit grace");
        }
        queued
    }
}

/// The delayed-render proof of a clipboard paste for the optional Enter (owner 2026-11-12).
trait Proof {
    /// The next set of the dictation renders late, so the target's own read is seen.
    fn arm(&mut self);
    /// `[asked, gone]` of that paste (`WinClipboard::proof`). Disarms.
    fn proof(&mut self) -> [Option<Instant>; 2];
}

impl Proof for WinClipboard {
    fn arm(&mut self) {
        self.arm_proof();
    }
    fn proof(&mut self) -> [Option<Instant>; 2] {
        WinClipboard::proof(self)
    }
}

/// Arms the clipboard proof for this paste only with the Enter on: the default paste keeps the
/// plain clipboard write. Only the set a Ctrl+V follows honours it (`Clipboard::set_pasted`); a
/// clipboard-only paste writes plainly.
fn arm(clip: Option<&mut impl Proof>, on: bool) {
    if let Some(clip) = clip.filter(|_| on) {
        clip.arm();
    }
}

/// The optional Enter after one paste: the clipboard proof is collected (and disarmed) here, a
/// skip is logged by its reason, never with the text.
fn enter(
    job: JobId,
    on: bool,
    outcome: PasteOutcome,
    start: Option<Focus>,
    chord_at: Option<Instant>,
    clip: Option<&mut impl Proof>,
    input: &mut impl Input,
) {
    let [asked, gone] = match clip {
        Some(clip) if on => clip.proof(),
        _ => [None; 2],
    };
    match enter_after(on, outcome, start, [chord_at, asked, gone], input) {
        Ok(()) => tracing::info!(?job, "enter sent"),
        Err(EnterSkip::Off | EnterSkip::NotLanded) => {}
        Err(skip) => tracing::info!(?job, ?skip, "enter not sent"),
    }
}

/// Start the paste worker on the real clipboard and keyboard. It returns when `rx` closes.
/// `store` keeps the ids of our Win+V entries across restarts (`OurEntries::to_text`, no text).
pub fn spawn(
    rx: Receiver<PasteCmd>,
    inbox: Sender<Inbox>,
    store: PathBuf,
) -> std::io::Result<JoinHandle<()>> {
    Builder::new().name("paste".into()).spawn(move || {
        let clip = WinClipboard::new()
            .inspect_err(|e| tracing::warn!("clipboard unavailable: {e}"))
            .ok();
        let ours = load(&store);
        let mut files = Writer::spawn(&inbox, append_now);
        run(
            &rx,
            &inbox,
            clip,
            Keyboard,
            ours,
            |ours| save(&store, ours),
            &mut files,
        );
        files.finish();
    })
}

/// The entries the last session left in Win+V; none when the file is missing or unreadable.
fn load(store: &Path) -> OurEntries {
    match std::fs::read_to_string(store) {
        Ok(text) => OurEntries::from_text(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => OurEntries::default(),
        Err(e) => {
            tracing::warn!(kind = ?e.kind(), "clipboard entries of the last session not read");
            OurEntries::default()
        }
    }
}

/// A failed write only costs the next session one entry left in Win+V. Atomic: Windows ending
/// the session may kill the process mid-write, which must leave the old file, not a cut one.
fn save(store: &Path, ours: &OurEntries) {
    if let Err(e) = oxi_platform::fs::write_atomic(store, ours.to_text().as_bytes()) {
        tracing::warn!(kind = ?e.kind(), "clipboard entries not saved");
    }
}

/// The command loop. Without a clipboard (`None`) every paste answers `Failed`, so the job is
/// not stranded in `Pasting` and the coordinator keeps the text for "Copy last text". `ours`
/// are our dictation entries still in Win+V, deleted by the next paste (item 52); `save` gets
/// them each time a paste changed them. `files` is the to-file thread.
fn run<C: Clipboard + Proof>(
    rx: &Receiver<PasteCmd>,
    inbox: &Sender<Inbox>,
    mut clip: Option<C>,
    mut input: impl Input,
    mut ours: OurEntries,
    mut save: impl FnMut(&OurEntries),
    files: &mut Writer,
) {
    for cmd in rx {
        match cmd {
            PasteCmd::Paste {
                job,
                text,
                file: Some(to),
                ..
            } if to.only => {
                // Nothing is pasted: the to-file thread answers the job. A write it cannot take
                // fails now; "Copy last text" keeps the text.
                let write = FileWrite {
                    job: Some(job),
                    only: true,
                    text,
                    to,
                };
                if !files.queue(inbox, write) {
                    let outcome = PasteOutcome::FileFailed;
                    let _ = inbox.send(Inbox::Event(Event::PasteDone { job, outcome }));
                }
            }
            PasteCmd::Paste {
                job,
                text,
                cfg,
                file,
                focus_at_start: start,
            } => {
                let before = ours.clone();
                let started = Instant::now();
                let mut why = None;
                let mut sent_at: Option<Instant> = None;
                // When Ctrl+V went out: a read of our text before it is no answer to the paste.
                let mut chord_at: Option<Instant> = None;
                arm(clip.as_mut(), cfg.press_enter);
                let mut report = |step| match step {
                    PasteStep::Typed(typed) => why = Some(typed),
                    PasteStep::Typing { done, total } => {
                        if done == total || sent_at.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY) {
                            sent_at = Some(Instant::now());
                            let _ = inbox.send(Inbox::Progress {
                                job,
                                percent: typed_percent(done, total),
                            });
                        }
                    }
                    PasteStep::Landed => {
                        chord_at.get_or_insert_with(Instant::now);
                        let _ = inbox.send(Inbox::Progress { job, percent: 100 });
                    }
                };
                let outcome = clip.as_mut().map_or(PasteOutcome::Failed, |clip| {
                    paste_with(&text, &cfg, clip, &mut input, &mut ours, &mut report)
                });
                if ours != before {
                    save(&ours);
                }
                // One Enter per delivered dictation: a superseded part of a hold-gap continuation
                // never reaches this worker, the coordinator pastes only the merged text.
                enter(
                    job,
                    cfg.press_enter,
                    outcome,
                    start,
                    chord_at,
                    clip.as_mut(),
                    &mut input,
                );
                // The length and the reason only, never the text.
                tracing::info!(
                    ?job,
                    ?outcome,
                    ?why,
                    units = text.encode_utf16().count(),
                    took_ms = started.elapsed().as_millis(),
                    "paste done"
                );
                // A send fails only when the coordinator is gone; nothing is left to tell then.
                let _ = inbox.send(Inbox::Event(Event::PasteDone { job, outcome }));
                // After the paste, on the to-file thread: a folder on an offline share holds
                // neither this text nor the next dictation (review d04 #4).
                if let Some(to) = file {
                    let write = FileWrite {
                        job: Some(job),
                        only: false,
                        text,
                        to,
                    };
                    files.queue(inbox, write);
                }
            }
            PasteCmd::File { text, to, until } => {
                // Behind the dictations queued before it: one writer keeps the file in order.
                files.until = Some(until);
                let write = FileWrite {
                    job: None,
                    only: false,
                    text,
                    to,
                };
                files.queue(inbox, write);
            }
            PasteCmd::CopyOnly { text, in_history } => {
                if let Some(Err(e)) = clip.as_mut().map(|clip| copy_only(&text, in_history, clip)) {
                    tracing::warn!("copy last text failed: {e}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::mpsc::channel;

    use oxi_core::config::Paste;
    use oxi_core::hotkey::Key;
    use oxi_core::machine::JobId;
    use oxi_core::paste::{ClipError, Focus, Foreground, InputBlocked, Snap};
    use oxi_core::types::Millis;

    use super::*;
    use crate::log::scratch::scratch_dir;

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Set(String),
        InHistory(bool),
        Chord,
        Enter,
        Type,
    }

    type Log = Rc<RefCell<Vec<Call>>>;

    /// The fake clipboard; `.1` is armed for the Enter proof.
    struct FakeClip(Log, bool);

    /// Text the fake target reads through the delayed render: the proof for the Enter.
    const READ: &str = "read by the target";

    impl Proof for FakeClip {
        fn arm(&mut self) {
            self.1 = true;
        }
        /// The target asked after Ctrl+V when the armed text was [`READ`]; the restore began
        /// later still.
        fn proof(&mut self) -> [Option<Instant>; 2] {
            let read =
                std::mem::take(&mut self.1) && self.0.borrow().contains(&Call::Set(READ.into()));
            let asked = read.then(Instant::now);
            [asked, asked.map(|t| t + Duration::from_millis(1))]
        }
    }

    impl Clipboard for FakeClip {
        type Snapshot = ();
        fn snapshot(&mut self) -> Snap<()> {
            Snap::Complete(())
        }
        fn is_excluded(&self, (): &()) -> bool {
            false
        }
        fn set_text(&mut self, text: &str, in_history: bool) -> Result<u32, ClipError> {
            let mut log = self.0.borrow_mut();
            log.push(Call::Set(text.to_owned()));
            log.push(Call::InHistory(in_history));
            Ok(1)
        }
        fn sequence(&self) -> u32 {
            1
        }
        fn restore(&mut self, (): &(), _in_history: bool) -> Result<u32, ClipError> {
            Ok(1)
        }
    }

    struct FakeInput(Log);

    /// Where [`FakeInput`] always has the focus.
    const HERE: Focus = Focus {
        window: 1,
        control: 2,
        keys: 0,
    };

    impl Input for FakeInput {
        fn modifiers_held(&self) -> bool {
            false
        }
        fn foreground(&self) -> Foreground {
            Foreground {
                app: "notepad.exe".into(),
                elevated: false,
            }
        }
        fn focus(&mut self) -> Option<Focus> {
            Some(HERE)
        }
        fn send_chord(&mut self, keys: &[Key]) -> Result<(), InputBlocked> {
            let call = if keys == [Key::Enter] {
                Call::Enter
            } else {
                Call::Chord
            };
            self.0.borrow_mut().push(call);
            Ok(())
        }
        fn send_units(&mut self, _units: &[u16]) -> Result<(), InputBlocked> {
            self.0.borrow_mut().push(Call::Type);
            Ok(())
        }
        fn sleep(&mut self, _time: Millis) {}
    }

    /// Runs the worker over `cmds` (already queued) and returns the call log and the answers.
    fn drive(cmds: Vec<PasteCmd>, clip: bool) -> (Vec<Call>, Vec<(JobId, PasteOutcome)>) {
        let (calls, msgs) = drive_all(cmds, clip);
        let done = msgs.into_iter().filter_map(Result::err).collect();
        (calls, done)
    }

    /// Every inbox message in order: `Ok` a progress percent, `Err` a paste answer.
    #[allow(clippy::type_complexity, reason = "a test helper's tuple of logs")]
    fn drive_all(
        cmds: Vec<PasteCmd>,
        clip: bool,
    ) -> (Vec<Call>, Vec<Result<(JobId, u8), (JobId, PasteOutcome)>>) {
        let log = Log::default();
        let (tx, rx) = channel();
        let (inbox, answers) = channel();
        for cmd in cmds {
            tx.send(cmd).unwrap();
        }
        drop(tx);
        let mut saved = Vec::new();
        run(
            &rx,
            &inbox,
            clip.then(|| FakeClip(log.clone(), false)),
            FakeInput(log.clone()),
            OurEntries::default(),
            |ours| saved.push(ours.to_text()),
            &mut Writer::spawn(&inbox, append_now),
        );
        // Ids and a number only: no dictated text reaches the file.
        for call in log.borrow().iter() {
            if let Call::Set(text) = call {
                assert!(saved.iter().all(|file| !file.contains(text.as_str())));
            }
        }
        drop(inbox);
        let msgs = answers
            .iter()
            .filter_map(|msg| match msg {
                Inbox::Event(Event::PasteDone { job, outcome }) => Some(Err((job, outcome))),
                Inbox::Progress { job, percent } => Some(Ok((job, percent))),
                // Checked by the to-file tests, which run the worker themselves.
                Inbox::FileWarning { .. } => None,
                _ => panic!("unexpected inbox message"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            // A typed paste, or one that only went to a file, never touches the history: nothing
            // to save.
            clip && msgs
                .iter()
                .any(|m| matches!(m, Err((_, PasteOutcome::Pasted))))
                && log.borrow().contains(&Call::Chord),
            !saved.is_empty(),
            "a paste saves its ids"
        );
        (log.take(), msgs)
    }

    #[test]
    fn a_typed_paste_reports_capped_progress_then_100_before_its_answer() {
        let cfg = Paste {
            mode: oxi_core::config::PasteMode::Type,
            ..Paste::default()
        };
        let paste = PasteCmd::Paste {
            job: JobId(3),
            text: "a".repeat(40),
            cfg,
            file: None,
            focus_at_start: None,
        };
        let (calls, msgs) = drive_all(vec![paste], true);
        // 40 units at the fast pace: 3 calls. The first and the last progress always go out;
        // the middle one only on a box slow enough to pass the 100 ms throttle.
        assert_eq!(calls, [Call::Type, Call::Type, Call::Type]);
        let (last, progress) = msgs.split_last().unwrap();
        assert_eq!(*last, Err((JobId(3), PasteOutcome::Typed)));
        let percents: Vec<u8> = progress.iter().map(|m| m.unwrap().1).collect();
        assert!(progress.iter().all(|m| m.unwrap().0 == JobId(3)));
        assert_eq!(percents.first(), Some(&40));
        assert_eq!(percents[percents.len() - 2..], [99, 100]);
        assert!(percents.is_sorted_by(|a, b| a < b), "{percents:?}");
        assert_eq!(typed_percent(0, 0), 0);
        assert_eq!(typed_percent(7, 10), 70);
    }

    #[test]
    fn a_clipboard_paste_reports_100_once_when_it_landed() {
        let (_, msgs) = drive_all(vec![job(1, "one")], true);
        assert_eq!(
            msgs,
            [Ok((JobId(1), 100)), Err((JobId(1), PasteOutcome::Pasted))]
        );
    }

    fn enter_job(id: u64, mode: oxi_core::config::PasteMode, start: Option<Focus>) -> PasteCmd {
        let cfg = Paste {
            mode,
            press_enter: true,
            ..Paste::default()
        };
        PasteCmd::Paste {
            job: JobId(id),
            text: "one".into(),
            cfg,
            file: None,
            focus_at_start: start,
        }
    }

    fn enters(calls: &[Call]) -> usize {
        calls.iter().filter(|c| **c == Call::Enter).count()
    }

    #[test]
    fn enter_follows_the_last_typed_chunk_once_and_only_when_asked() {
        use oxi_core::config::PasteMode::Type;
        let (calls, _) = drive_all(vec![enter_job(1, Type, Some(HERE))], true);
        assert_eq!(calls.last(), Some(&Call::Enter), "{calls:?}");
        assert_eq!(enters(&calls), 1);
        // Off by default.
        let (calls, _) = drive_all(vec![job(1, "one")], true);
        assert_eq!(enters(&calls), 0, "{calls:?}");
    }

    /// Review d08 #1: the start focus is the record start's, from the coordinator; the worker
    /// never takes its own at pickup (a switch while speaking had both reads in the new chat).
    /// No start focus (setting off at the record start, a lock since): no Enter.
    #[test]
    fn enter_compares_with_the_focus_of_the_record_start() {
        use oxi_core::config::PasteMode::Type;
        let elsewhere = Focus { window: 9, ..HERE };
        for start in [Some(elsewhere), None] {
            let (calls, done) = drive(vec![enter_job(1, Type, start)], true);
            assert_eq!(done, [(JobId(1), PasteOutcome::Typed)]);
            assert_eq!(enters(&calls), 0, "{start:?}: {calls:?}");
        }
    }

    /// Owner 2026-11-12: after Ctrl+V the Enter goes only when the target was seen reading our
    /// delayed text; Ctrl+V sent alone is not proof (review d08 #2).
    #[test]
    fn a_clipboard_paste_enters_only_when_the_target_read_the_text() {
        let clip = oxi_core::config::PasteMode::Clipboard;
        let (calls, done) = drive(vec![enter_job(1, clip, Some(HERE))], true);
        assert_eq!(done, [(JobId(1), PasteOutcome::Pasted)]);
        assert_eq!(enters(&calls), 0, "{calls:?}");
        let mut read = enter_job(2, clip, Some(HERE));
        if let PasteCmd::Paste { text, .. } = &mut read {
            READ.clone_into(text);
        }
        let (calls, done) = drive(vec![read], true);
        assert_eq!(done, [(JobId(2), PasteOutcome::Pasted)]);
        assert_eq!(calls.last(), Some(&Call::Enter), "{calls:?}");
        assert_eq!(enters(&calls), 1);
        // The same read with the setting off: no Enter, and no arming.
        let (calls, _) = drive(vec![job(3, READ)], true);
        assert_eq!(enters(&calls), 0, "{calls:?}");
    }

    #[test]
    fn no_enter_when_the_paste_failed() {
        // No clipboard: the job answers Failed.
        let (calls, done) = drive(
            vec![enter_job(1, oxi_core::config::PasteMode::Type, Some(HERE))],
            false,
        );
        assert_eq!(done, [(JobId(1), PasteOutcome::Failed)]);
        assert!(!calls.contains(&Call::Enter), "{calls:?}");
    }

    fn job(id: u64, text: &str) -> PasteCmd {
        PasteCmd::Paste {
            job: JobId(id),
            text: text.into(),
            cfg: Paste::default(),
            file: None,
            focus_at_start: None,
        }
    }

    fn copy(text: &str, in_history: bool) -> PasteCmd {
        PasteCmd::CopyOnly {
            text: text.into(),
            in_history,
        }
    }

    #[test]
    fn two_queued_pastes_run_in_order() {
        let (calls, done) = drive(vec![job(1, "one"), job(2, "two")], true);
        assert_eq!(
            calls,
            [
                Call::Set("one".into()),
                Call::InHistory(true),
                Call::Chord,
                Call::Set("two".into()),
                Call::InHistory(true),
                Call::Chord
            ]
        );
        assert_eq!(
            done,
            [
                (JobId(1), PasteOutcome::Pasted),
                (JobId(2), PasteOutcome::Pasted)
            ]
        );
    }

    #[test]
    fn copy_only_sets_the_clipboard_without_a_chord_or_answer() {
        let (calls, done) = drive(vec![copy("last", true)], true);
        assert_eq!(calls, [Call::Set("last".into()), Call::InHistory(true)]);
        assert_eq!(done.len(), 0);
    }

    #[test]
    fn copy_only_uses_its_own_history_flag_not_the_last_paste() {
        for keep in [true, false] {
            let cfg = Paste {
                keep_in_clipboard_history: keep,
                ..Paste::default()
            };
            let paste = PasteCmd::Paste {
                job: JobId(1),
                text: "one".into(),
                cfg,
                file: None,
                focus_at_start: None,
            };
            let (calls, _) = drive(vec![paste, copy("last", !keep)], true);
            assert_eq!(calls[calls.len() - 2..][1], Call::InHistory(!keep));
        }
    }

    #[test]
    fn the_entries_file_is_replaced_whole_and_read_back() {
        let dir = scratch_dir(&format!("oxi-paste-store-{}", std::process::id()));
        let store = dir.join("clipboard-entries.txt");
        std::fs::write(&store, "seq 1\nid old\n").unwrap();
        let ours = OurEntries::from_text("seq 9\ntop t1\nid a1\nid a2\n");
        save(&store, &ours);
        assert_eq!(load(&store), ours);
        // Through `<file>.tmp` and a rename: no temp file stays next to it.
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(names, [store.file_name().unwrap()]);
    }

    fn to_file(dir: &Path, only: bool) -> ToFile {
        ToFile {
            folder: dir.to_path_buf(),
            format: oxi_core::config::FileFormat::Md,
            only,
        }
    }

    #[test]
    fn a_dictation_is_appended_to_the_daily_file_and_kept_whole() {
        let dir = scratch_dir(&format!("oxi-paste-file-{}", std::process::id()));
        let to = to_file(&dir, false);
        let day = Date::from_ymd(2026, 11, 3).unwrap();
        let path = dir.join("2026-11-03.md");
        std::fs::write(
            &path,
            "- 08:00 earlier
",
        )
        .unwrap();
        append(
            &to,
            day,
            9 * 3_600_000 + 5 * 60_000,
            "one
two",
        )
        .unwrap();
        append(&to, day, 9 * 3_600_000 + 6 * 60_000, "three").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "- 08:00 earlier
- 09:05 one
  two
- 09:06 three
"
        );
        let missing = ToFile {
            folder: dir.join("gone"),
            ..to
        };
        assert!(
            append(&missing, day, 0, "x").is_err(),
            "a missing folder is not made"
        );
        assert!(!dir.join("gone").exists());
    }

    #[test]
    fn also_pastes_and_writes_while_only_writes_and_pastes_nothing() {
        let dir = scratch_dir(&format!("oxi-paste-file2-{}", std::process::id()));
        let cmd = |id, only| PasteCmd::Paste {
            job: JobId(id),
            text: "hello".into(),
            cfg: Paste::default(),
            file: Some(to_file(&dir, only)),
            focus_at_start: None,
        };
        let (calls, done) = drive(vec![cmd(1, false)], true);
        assert!(calls.contains(&Call::Chord));
        assert_eq!(done, [(JobId(1), PasteOutcome::Pasted)]);
        let (calls, msgs) = drive_all(vec![cmd(2, true)], true);
        assert_eq!(calls, [], "only: no clipboard, no chord");
        assert_eq!(
            msgs,
            [Ok((JobId(2), 100)), Err((JobId(2), PasteOutcome::Pasted))]
        );
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1, "one daily file");
        let text = std::fs::read_to_string(&files[0]).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.lines().all(|l| l.ends_with(" hello")));
    }

    #[test]
    fn a_failed_write_never_stops_also_and_fails_only() {
        let gone = std::env::temp_dir().join(format!("oxi-paste-nofolder-{}", std::process::id()));
        let cmd = |only| PasteCmd::Paste {
            job: JobId(1),
            text: "hello".into(),
            cfg: Paste::default(),
            file: Some(to_file(&gone, only)),
            focus_at_start: None,
        };
        assert_eq!(
            drive(vec![cmd(false)], true).1,
            [(JobId(1), PasteOutcome::Pasted)]
        );
        let (calls, done) = drive(vec![cmd(true)], true);
        assert_eq!(calls, []);
        assert_eq!(done, [(JobId(1), PasteOutcome::FileFailed)]);
        assert!(!gone.exists());
    }

    /// What the worker told the coordinator, in order: `Ok` a paste answer, `Err` a file warning
    /// with its job.
    #[allow(clippy::type_complexity, reason = "a test helper's log of two answers")]
    fn told(cmds: Vec<PasteCmd>) -> Vec<Result<(JobId, PasteOutcome), (JobId, Option<String>)>> {
        let log = Log::default();
        let (tx, rx) = channel();
        let (inbox, answers) = channel();
        for cmd in cmds {
            tx.send(cmd).unwrap();
        }
        drop(tx);
        let clip = Some(FakeClip(log.clone(), false));
        run(
            &rx,
            &inbox,
            clip,
            FakeInput(log),
            OurEntries::default(),
            |_| {},
            &mut Writer::spawn(&inbox, append_now),
        );
        drop(inbox);
        answers
            .iter()
            .filter_map(|msg| match msg {
                Inbox::Event(Event::PasteDone { job, outcome }) => Some(Ok((job, outcome))),
                Inbox::FileWarning { job, line } => Some(Err((job, line))),
                _ => None,
            })
            .collect()
    }

    /// Review d04 #4, #5: `also` pastes before it writes (a folder on an offline share would hold
    /// the paste for its timeout), and a failed write is a status warning until a write works.
    #[test]
    fn also_pastes_first_and_a_failed_write_warns_until_one_works() {
        let dir = scratch_dir(&format!("oxi-paste-file3-{}", std::process::id()));
        let cmd = |id, folder: &Path| PasteCmd::Paste {
            job: JobId(id),
            text: "hello".into(),
            cfg: Paste::default(),
            file: Some(to_file(folder, false)),
            focus_at_start: None,
        };
        let gone = dir.join("gone");
        let said = told(vec![
            cmd(1, &gone),
            cmd(2, &gone),
            cmd(3, &dir),
            cmd(4, &dir),
        ]);
        let warning = file_warning(std::io::ErrorKind::NotFound);
        // The pastes and the writes run on two threads: each side keeps its own order.
        let pasted: Vec<_> = said.iter().filter_map(|m| m.clone().ok()).collect();
        let warned: Vec<_> = said.iter().filter_map(|m| m.clone().err()).collect();
        assert_eq!(
            pasted,
            (1..=4)
                .map(|j| (JobId(j), PasteOutcome::Pasted))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            warned,
            [
                (JobId(1), Some(warning.clone())),
                (JobId(2), Some(warning)),
                (JobId(3), None),
                (JobId(4), None)
            ],
            "every write says how it went: a cleared warning comes back at the next failure"
        );
        assert!(!warning_has_text(&said, "hello"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// d04-fix 4: a write that hangs (an offline share) holds no paste, and a full queue drops a
    /// write with a warning instead of waiting; the `only` job it drops fails as a file failure.
    #[test]
    fn a_hanging_write_holds_no_paste_and_a_full_queue_waits_for_nothing() {
        let (gate, hold) = channel::<()>();
        let hold = std::sync::Mutex::new(hold);
        let (inbox, answers) = channel();
        // Every write waits until the test lets it go.
        let mut files = Writer::spawn(&inbox, move |_, _| {
            let _ = hold.lock().unwrap().recv();
            Ok(())
        });
        let (tx, rx) = channel();
        let to = |only| Some(to_file(Path::new("unused"), only));
        let n = u64::try_from(FILE_QUEUE).unwrap() + 2;
        for id in 1..=n {
            tx.send(PasteCmd::Paste {
                job: JobId(id),
                text: "hello".into(),
                cfg: Paste::default(),
                file: to(false),
                focus_at_start: None,
            })
            .unwrap();
        }
        tx.send(PasteCmd::Paste {
            job: JobId(99),
            text: "hello".into(),
            cfg: Paste::default(),
            file: to(true),
            focus_at_start: None,
        })
        .unwrap();
        drop(tx);
        let log = Log::default();
        // Returns with the first write still hanging: no paste waited for it.
        let clip = Some(FakeClip(log.clone(), false));
        run(
            &rx,
            &inbox,
            clip,
            FakeInput(log),
            OurEntries::default(),
            |_| {},
            &mut files,
        );
        let said: Vec<_> = answers.try_iter().collect();
        let done = |job: JobId, outcome: PasteOutcome| {
            said.iter()
                .any(|m| matches!(m, Inbox::Event(Event::PasteDone { job: j, outcome: o }) if *j == job && *o == outcome))
        };
        assert!((1..=n).all(|j| done(JobId(j), PasteOutcome::Pasted)));
        assert!(
            done(JobId(99), PasteOutcome::FileFailed),
            "no room: fails now"
        );
        // d03-label: the refused write answers at once, ahead of the hanging ones, with its own
        // job, so the coordinator ties it to the right dictation.
        let full = Some(file_warning(std::io::ErrorKind::TimedOut));
        assert!(said.iter().any(|m| matches!(
            m,
            Inbox::FileWarning { job: JobId(99), line } if *line == full
        )));
        drop((files, inbox, gate));
        // The writer ends once its queue is empty and closed.
        assert!(
            answers
                .iter()
                .all(|m| matches!(m, Inbox::FileWarning { line: None, .. }))
        );
    }

    #[allow(clippy::type_complexity, reason = "the log of `told`")]
    fn warning_has_text(
        said: &[Result<(JobId, PasteOutcome), (JobId, Option<String>)>],
        text: &str,
    ) -> bool {
        said.iter()
            .any(|m| matches!(m, Err((_, Some(w))) if w.contains(text)))
    }

    /// Review d04 #6: a failed write in `only` mode says the file failed (the status warning)
    /// before the job fails, so the text stays in "Copy last text".
    #[test]
    fn a_failed_write_only_warns_about_the_file_before_the_failure() {
        let gone = std::env::temp_dir().join(format!("oxi-paste-gone-{}", std::process::id()));
        let said = told(vec![PasteCmd::Paste {
            job: JobId(1),
            text: "hello".into(),
            cfg: Paste::default(),
            file: Some(to_file(&gone, true)),
            focus_at_start: None,
        }]);
        assert_eq!(
            said,
            [
                Err((JobId(1), Some(file_warning(std::io::ErrorKind::NotFound)))),
                Ok((JobId(1), PasteOutcome::FileFailed)),
            ]
        );
    }

    /// Review d04 #7: at quit the text that is not pasted still reaches its file.
    #[test]
    fn the_quit_write_appends_the_text_and_answers_nothing() {
        let dir = scratch_dir(&format!("oxi-paste-quit-{}", std::process::id()));
        let until = Instant::now() + Duration::from_secs(5);
        let to = to_file(&dir, true);
        let said = told(vec![PasteCmd::File {
            text: "late".into(),
            to,
            until,
        }]);
        assert_eq!(said, []);
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1);
        assert!(std::fs::read_to_string(&files[0]).unwrap().ends_with(
            " late
"
        ));
    }

    /// d04-fix 3: the quit's write goes behind the dictations queued before it (one writer), and
    /// the exit waits for the queue, until the quit grace; what hangs past it is counted, not
    /// waited for.
    #[test]
    fn the_exit_drains_the_file_queue_in_order_within_the_quit_grace() {
        let (inbox, _answers) = channel();
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = std::sync::Arc::clone(&lines);
        let mut files = Writer::spawn(&inbox, move |_, text| {
            // The first write is slow: a quit write on its own thread would land before it.
            if text == "one" {
                std::thread::sleep(Duration::from_millis(100));
            }
            seen.lock().unwrap().push(text.to_owned());
            Ok(())
        });
        let (tx, rx) = channel();
        let to = || to_file(Path::new("unused"), false);
        tx.send(PasteCmd::Paste {
            job: JobId(1),
            text: "one".into(),
            cfg: Paste::default(),
            file: Some(to()),
            focus_at_start: None,
        })
        .unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        tx.send(PasteCmd::File {
            text: "two".into(),
            to: to(),
            until,
        })
        .unwrap();
        drop(tx);
        let log = Log::default();
        run(
            &rx,
            &inbox,
            Some(FakeClip(log.clone(), false)),
            FakeInput(log),
            OurEntries::default(),
            |_| {},
            &mut files,
        );
        assert_eq!(files.finish(), 0, "everything written before the exit");
        assert_eq!(*lines.lock().unwrap(), ["one", "two"]);
        // A folder that hangs: the exit gives up at the grace and counts what is left.
        let (gate, hold) = channel::<()>();
        let hold = std::sync::Mutex::new(hold);
        let mut files = Writer::spawn(&inbox, move |_, _| {
            let _ = hold.lock().unwrap().recv();
            Ok(())
        });
        let write = |text: &str| FileWrite {
            job: None,
            only: false,
            text: text.into(),
            to: to(),
        };
        assert!(files.queue(&inbox, write("a")) && files.queue(&inbox, write("b")));
        files.until = Some(Instant::now() + Duration::from_millis(50));
        let started = Instant::now();
        assert_eq!(files.finish(), 2);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "bounded by the grace"
        );
        drop(gate);
    }

    #[test]
    fn without_a_clipboard_a_paste_still_answers_failed() {
        let (calls, done) = drive(vec![job(7, "x"), copy("y", true)], false);
        assert_eq!(calls.len(), 0);
        assert_eq!(done, [(JobId(7), PasteOutcome::Failed)]);
    }
}
