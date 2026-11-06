//! Paste worker: clipboard and synthetic input (ARCHITECTURE section 10). It runs one command at
//! a time in arrival order, so two queued pastes never interleave. Dictated text is never logged.

#![allow(
    dead_code,
    reason = "run.rs (t40) spawns the worker; until then only the tests do"
)]

use std::sync::mpsc::{Receiver, Sender};
use std::thread::{Builder, JoinHandle};

use oxi_core::machine::Event;
use oxi_core::paste::{Clipboard, Input, PasteOutcome, copy_only, paste};
use oxi_platform::os::clipboard::WinClipboard;
use oxi_platform::os::input::Keyboard;

use super::cmd::{Inbox, PasteCmd};

/// Start the paste worker on the real clipboard and keyboard. It returns when `rx` closes.
pub fn spawn(rx: Receiver<PasteCmd>, inbox: Sender<Inbox>) -> std::io::Result<JoinHandle<()>> {
    Builder::new().name("paste".into()).spawn(move || {
        let clip = WinClipboard::new()
            .inspect_err(|e| tracing::warn!("clipboard unavailable: {e}"))
            .ok();
        run(&rx, &inbox, clip, Keyboard);
    })
}

/// The command loop. Without a clipboard (`None`) every paste answers `Failed`, so the job is
/// not stranded in `Pasting` and the coordinator keeps the text for "Copy last text".
fn run<C: Clipboard>(
    rx: &Receiver<PasteCmd>,
    inbox: &Sender<Inbox>,
    mut clip: Option<C>,
    mut input: impl Input,
) {
    for cmd in rx {
        match cmd {
            PasteCmd::Paste { job, text, cfg } => {
                let outcome = clip.as_mut().map_or(PasteOutcome::Failed, |clip| {
                    paste(&text, &cfg, clip, &mut input)
                });
                tracing::debug!(?job, ?outcome, "paste done");
                // A send fails only when the coordinator is gone; nothing is left to tell then.
                let _ = inbox.send(Inbox::Event(Event::PasteDone { job, outcome }));
            }
            PasteCmd::CopyOnly(text) => {
                if let Some(Err(e)) = clip.as_mut().map(|clip| copy_only(&text, clip)) {
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

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Set(String),
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
        fn set_text(&mut self, text: &str) -> Result<u32, ClipError> {
            self.0.borrow_mut().push(Call::Set(text.to_owned()));
            Ok(1)
        }
        fn sequence(&self) -> u32 {
            1
        }
        fn restore(&mut self, (): ()) -> Result<(), ClipError> {
            Ok(())
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
        run(
            &rx,
            &inbox,
            clip.then(|| FakeClip(log.clone())),
            FakeInput(log.clone()),
        );
        drop(inbox);
        let done = answers
            .iter()
            .map(|msg| match msg {
                Inbox::Event(Event::PasteDone { job, outcome }) => (job, outcome),
                _ => panic!("unexpected inbox message"),
            })
            .collect();
        (log.take(), done)
    }

    fn job(id: u64, text: &str) -> PasteCmd {
        PasteCmd::Paste {
            job: JobId(id),
            text: text.into(),
            cfg: Paste::default(),
        }
    }

    #[test]
    fn two_queued_pastes_run_in_order() {
        let (calls, done) = drive(vec![job(1, "one"), job(2, "two")], true);
        assert_eq!(
            calls,
            [
                Call::Set("one".into()),
                Call::Chord,
                Call::Set("two".into()),
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
        let (calls, done) = drive(vec![PasteCmd::CopyOnly("last".into())], true);
        assert_eq!(calls, [Call::Set("last".into())]);
        assert_eq!(done.len(), 0);
    }

    #[test]
    fn without_a_clipboard_a_paste_still_answers_failed() {
        let (calls, done) = drive(vec![job(7, "x"), PasteCmd::CopyOnly("y".into())], false);
        assert_eq!(calls.len(), 0);
        assert_eq!(done, [(JobId(7), PasteOutcome::Failed)]);
    }
}
