//! Paste worker: clipboard and synthetic input (ARCHITECTURE section 10). It runs one command at
//! a time in arrival order, so two queued pastes never interleave. Dictated text is never logged.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::thread::{Builder, JoinHandle};

use oxi_core::machine::Event;
use oxi_core::paste::{Clipboard, Input, OurEntries, PasteOutcome, copy_only, paste_with};
use oxi_platform::os::clipboard::WinClipboard;
use oxi_platform::os::input::Keyboard;

use super::cmd::{Inbox, PasteCmd};

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
        run(&rx, &inbox, clip, Keyboard, ours, |ours| save(&store, ours));
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
/// them each time a paste changed them.
fn run<C: Clipboard>(
    rx: &Receiver<PasteCmd>,
    inbox: &Sender<Inbox>,
    mut clip: Option<C>,
    mut input: impl Input,
    mut ours: OurEntries,
    mut save: impl FnMut(&OurEntries),
) {
    for cmd in rx {
        match cmd {
            PasteCmd::Paste { job, text, cfg } => {
                let before = ours.clone();
                let outcome = clip.as_mut().map_or(PasteOutcome::Failed, |clip| {
                    paste_with(&text, &cfg, clip, &mut input, &mut ours)
                });
                if ours != before {
                    save(&ours);
                }
                tracing::info!(?job, ?outcome, "paste done");
                // A send fails only when the coordinator is gone; nothing is left to tell then.
                let _ = inbox.send(Inbox::Event(Event::PasteDone { job, outcome }));
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
    use oxi_core::paste::{ClipError, Foreground, InputBlocked, Snap};
    use oxi_core::types::Millis;

    use super::*;
    use crate::log::scratch::scratch_dir;

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Set(String),
        InHistory(bool),
        Chord,
        Type,
    }

    type Log = Rc<RefCell<Vec<Call>>>;

    struct FakeClip(Log);

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
        fn send_chord(&mut self, _keys: &[Key]) -> Result<(), InputBlocked> {
            self.0.borrow_mut().push(Call::Chord);
            Ok(())
        }
        fn type_units(&mut self, _units: &[u16], _gap: Millis) -> Result<(), InputBlocked> {
            self.0.borrow_mut().push(Call::Type);
            Ok(())
        }
        fn sleep(&mut self, _time: Millis) {}
    }

    /// Runs the worker over `cmds` (already queued) and returns the call log and the answers.
    fn drive(cmds: Vec<PasteCmd>, clip: bool) -> (Vec<Call>, Vec<(JobId, PasteOutcome)>) {
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
            clip.then(|| FakeClip(log.clone())),
            FakeInput(log.clone()),
            OurEntries::default(),
            |ours| saved.push(ours.to_text()),
        );
        // Ids and a number only: no dictated text reaches the file.
        for call in log.borrow().iter() {
            if let Call::Set(text) = call {
                assert!(saved.iter().all(|file| !file.contains(text.as_str())));
            }
        }
        drop(inbox);
        let done = answers
            .iter()
            .map(|msg| match msg {
                Inbox::Event(Event::PasteDone { job, outcome }) => (job, outcome),
                _ => panic!("unexpected inbox message"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            clip && !done.is_empty(),
            !saved.is_empty(),
            "a paste saves its ids"
        );
        (log.take(), done)
    }

    fn job(id: u64, text: &str) -> PasteCmd {
        PasteCmd::Paste {
            job: JobId(id),
            text: text.into(),
            cfg: Paste::default(),
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

    #[test]
    fn without_a_clipboard_a_paste_still_answers_failed() {
        let (calls, done) = drive(vec![job(7, "x"), copy("y", true)], false);
        assert_eq!(calls.len(), 0);
        assert_eq!(done, [(JobId(7), PasteOutcome::Failed)]);
    }
}
