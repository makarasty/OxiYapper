//! Paste algorithm (ARCHITECTURE 10) against fake clipboard and input that log every call.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use oxi_core::config::{Paste, PasteMode, PasteRule};
use oxi_core::hotkey::Key;
use oxi_core::paste::{
    ClipError, Clipboard, ClipboardOnlyWhy, Foreground, Input, InputBlocked, PasteOutcome, Plan,
    Snap, TYPE_GAP, paste, plan,
};
use oxi_core::types::Millis;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Mods,
    Foreground,
    Snapshot,
    Set(String),
    Seq,
    Restore,
    Chord(Vec<Key>),
    Type(Vec<u16>),
    Sleep(u64),
}

type Log = Rc<RefCell<Vec<Call>>>;

#[derive(Clone, Copy)]
enum SnapKind {
    Complete,
    Incomplete,
    Locked,
}

struct FakeClip {
    log: Log,
    snap: SnapKind,
    seq: u32,
    /// Another app writes the clipboard after our set.
    foreign_write: bool,
}

impl Clipboard for FakeClip {
    type Snapshot = ();
    fn snapshot(&mut self) -> Snap<()> {
        self.log.borrow_mut().push(Call::Snapshot);
        match self.snap {
            SnapKind::Complete => Snap::Complete(()),
            SnapKind::Incomplete => Snap::Incomplete,
            SnapKind::Locked => Snap::Locked,
        }
    }
    fn set_text(&mut self, text: &str) -> Result<u32, ClipError> {
        self.log.borrow_mut().push(Call::Set(text.to_owned()));
        self.seq += 1;
        Ok(self.seq)
    }
    fn sequence(&self) -> u32 {
        self.log.borrow_mut().push(Call::Seq);
        self.seq + u32::from(self.foreign_write)
    }
    fn restore(&mut self, (): ()) -> Result<(), ClipError> {
        self.log.borrow_mut().push(Call::Restore);
        Ok(())
    }
}

struct FakeInput {
    log: Log,
    app: &'static str,
    elevated: bool,
    /// Modifiers stay down until this much time has been slept.
    held_ms: u64,
    clock: Cell<u64>,
}

impl Input for FakeInput {
    fn modifiers_held(&self) -> bool {
        self.log.borrow_mut().push(Call::Mods);
        self.clock.get() < self.held_ms
    }
    fn foreground(&self) -> Foreground {
        self.log.borrow_mut().push(Call::Foreground);
        Foreground {
            app: self.app.to_owned(),
            elevated: self.elevated,
        }
    }
    fn send_chord(&mut self, keys: &[Key]) -> Result<(), InputBlocked> {
        self.log.borrow_mut().push(Call::Chord(keys.to_vec()));
        Ok(())
    }
    fn type_units(&mut self, units: &[u16], gap: Millis) -> Result<(), InputBlocked> {
        assert_eq!(gap, TYPE_GAP);
        self.log.borrow_mut().push(Call::Type(units.to_vec()));
        Ok(())
    }
    fn sleep(&mut self, time: Millis) {
        self.log.borrow_mut().push(Call::Sleep(time.0));
        self.clock.set(self.clock.get() + time.0);
    }
}

struct Case {
    app: &'static str,
    elevated: bool,
    held_ms: u64,
    snap: SnapKind,
    foreign_write: bool,
    config: Paste,
}

impl Default for Case {
    fn default() -> Self {
        Self {
            app: "notepad.exe",
            elevated: false,
            held_ms: 0,
            snap: SnapKind::Complete,
            foreign_write: false,
            config: Paste::default(),
        }
    }
}

fn run(case: &Case, text: &str) -> (PasteOutcome, Vec<Call>) {
    let log = Log::default();
    let mut clip = FakeClip {
        log: log.clone(),
        snap: case.snap,
        seq: 7,
        foreign_write: case.foreign_write,
    };
    let mut input = FakeInput {
        log: log.clone(),
        app: case.app,
        elevated: case.elevated,
        held_ms: case.held_ms,
        clock: Cell::new(0),
    };
    let out = paste(text, &case.config, &mut clip, &mut input);
    let calls = log.borrow().clone();
    (out, calls)
}

fn has(calls: &[Call], pred: impl Fn(&Call) -> bool) -> bool {
    calls.iter().any(pred)
}

fn ctrl_v() -> Call {
    Call::Chord(vec![Key::LCtrl, Key::V])
}

#[test]
fn normal_paste_snapshots_sets_sends_waits_checks_and_restores() {
    let (out, calls) = run(&Case::default(), "hi ");
    assert_eq!(out, PasteOutcome::Pasted);
    assert_eq!(
        calls,
        [
            Call::Mods,
            Call::Foreground,
            Call::Snapshot,
            Call::Set("hi ".into()),
            ctrl_v(),
            Call::Sleep(300),
            Call::Seq,
            Call::Restore,
        ]
    );
}

#[test]
fn modifiers_released_within_the_timeout_are_waited_for() {
    let (out, calls) = run(
        &Case {
            held_ms: 100,
            ..Case::default()
        },
        "hi",
    );
    assert_eq!(out, PasteOutcome::Pasted);
    let polls = calls.iter().filter(|c| **c == Call::Sleep(5)).count();
    assert_eq!(polls, 20);
}

#[test]
fn modifiers_held_two_seconds_leave_the_text_on_the_clipboard() {
    let (out, calls) = run(
        &Case {
            held_ms: 2000,
            ..Case::default()
        },
        "hi",
    );
    assert_eq!(
        out,
        PasteOutcome::ClipboardOnly(ClipboardOnlyWhy::ModifiersHeld)
    );
    assert!(has(&calls, |c| *c == Call::Set("hi".into())));
    assert!(!has(&calls, |c| matches!(
        c,
        Call::Chord(_) | Call::Restore | Call::Snapshot
    )));
    let waited: u64 = calls
        .iter()
        .map(|c| if let Call::Sleep(ms) = c { *ms } else { 0 })
        .sum();
    assert_eq!(waited, 1500);
}

#[test]
fn elevated_target_is_clipboard_only() {
    let (out, calls) = run(
        &Case {
            elevated: true,
            ..Case::default()
        },
        "hi",
    );
    assert_eq!(out, PasteOutcome::ClipboardOnly(ClipboardOnlyWhy::Elevated));
    assert!(!has(&calls, |c| matches!(
        c,
        Call::Chord(_) | Call::Type(_) | Call::Restore
    )));
}

#[test]
fn locked_clipboard_types_and_leaves_the_clipboard_alone() {
    let (out, calls) = run(
        &Case {
            snap: SnapKind::Locked,
            ..Case::default()
        },
        "hi",
    );
    assert_eq!(out, PasteOutcome::Typed);
    assert!(!has(&calls, |c| matches!(c, Call::Set(_) | Call::Chord(_))));
}

#[test]
fn incomplete_snapshot_types_up_to_300_characters() {
    // Cyrillic: characters, not bytes, are counted.
    let text = "я".repeat(300);
    let (out, calls) = run(
        &Case {
            snap: SnapKind::Incomplete,
            ..Case::default()
        },
        &text,
    );
    assert_eq!(out, PasteOutcome::Typed);
    assert!(!has(&calls, |c| matches!(c, Call::Set(_))));
}

#[test]
fn incomplete_snapshot_with_longer_text_pastes_without_restore() {
    let text = "я".repeat(301);
    let (out, calls) = run(
        &Case {
            snap: SnapKind::Incomplete,
            ..Case::default()
        },
        &text,
    );
    assert_eq!(out, PasteOutcome::PastedNoRestore);
    assert!(has(&calls, |c| *c == ctrl_v()));
    assert!(!has(&calls, |c| matches!(c, Call::Restore | Call::Type(_))));
}

#[test]
fn clipboard_changed_by_someone_else_is_not_restored() {
    let (out, calls) = run(
        &Case {
            foreign_write: true,
            ..Case::default()
        },
        "hi",
    );
    assert_eq!(out, PasteOutcome::Pasted);
    assert!(has(&calls, |c| *c == Call::Seq));
    assert!(!has(&calls, |c| *c == Call::Restore));
}

#[test]
fn default_mstsc_rule_types_whatever_the_case() {
    let (out, calls) = run(
        &Case {
            app: "MSTSC.EXE",
            ..Case::default()
        },
        "hi",
    );
    assert_eq!(out, PasteOutcome::Typed);
    assert!(!has(&calls, |c| matches!(c, Call::Snapshot | Call::Set(_))));
}

#[test]
fn shift_insert_rule_sends_that_chord() {
    let mut config = Paste::default();
    config.rules.push(PasteRule {
        app: "putty.exe".into(),
        mode: None,
        chord: Some(vec!["Shift".into(), "Insert".into()]),
        restore_delay_ms: None,
    });
    let (out, calls) = run(
        &Case {
            app: "putty.exe",
            config,
            ..Case::default()
        },
        "hi",
    );
    assert_eq!(out, PasteOutcome::Pasted);
    assert!(has(&calls, |c| *c
        == Call::Chord(vec![Key::LShift, Key::Insert])));
}

#[test]
fn emoji_is_typed_as_two_utf16_units() {
    let config = Paste {
        mode: PasteMode::Type,
        ..Paste::default()
    };
    let (out, calls) = run(
        &Case {
            config,
            ..Case::default()
        },
        "\u{1F600}",
    );
    assert_eq!(out, PasteOutcome::Typed);
    assert!(has(&calls, |c| *c == Call::Type(vec![0xD83D, 0xDE00])));
}

#[test]
fn plan_takes_each_field_from_the_rule_or_the_default() {
    let mut config = Paste::default();
    config.rules.push(PasteRule {
        app: "slack.exe".into(),
        mode: None,
        chord: Some(vec!["NoSuchKey".into()]),
        restore_delay_ms: Some(800),
    });
    let fg = |app: &str| Foreground {
        app: app.into(),
        elevated: false,
    };
    assert_eq!(
        plan(&config, &fg("Slack.exe")),
        Plan {
            mode: PasteMode::Clipboard,
            chord: vec![Key::LCtrl, Key::V],
            restore_delay: Millis(800),
        }
    );
    assert_eq!(
        plan(&config, &fg("code.exe")),
        Plan {
            mode: PasteMode::Clipboard,
            chord: vec![Key::LCtrl, Key::V],
            restore_delay: Millis(300),
        }
    );
}
