//! Paste algorithm (ARCHITECTURE 10) against fake clipboard and input that log every call.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use oxi_core::config::{Paste, PasteMode, PasteRule};
use oxi_core::hotkey::Key;
use oxi_core::paste::{
    ClipError, Clipboard, ClipboardOnlyWhy, Foreground, Input, InputBlocked, OurEntries,
    PasteOutcome, Plan, Snap, TYPE_GAP, copy_only, paste, plan,
};
use oxi_core::types::Millis;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    KeepInHistory(bool),
    Mods,
    Foreground,
    Snapshot,
    Set(String),
    Seq,
    RestoreInHistory(bool),
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
    /// `set_text` fails (nothing is written, the sequence stays).
    set_error: Option<ClipError>,
    /// The saved data carries an exclusion marker (a password manager's copy).
    excluded: bool,
}

impl Clipboard for FakeClip {
    type Snapshot = bool;
    fn snapshot(&mut self) -> Snap<bool> {
        self.log.borrow_mut().push(Call::Snapshot);
        match self.snap {
            SnapKind::Complete => Snap::Complete(self.excluded),
            SnapKind::Incomplete => Snap::Incomplete,
            SnapKind::Locked => Snap::Locked,
        }
    }
    fn is_excluded(&self, snapshot: &bool) -> bool {
        *snapshot
    }
    fn set_text(&mut self, text: &str, in_history: bool) -> Result<u32, ClipError> {
        {
            let mut log = self.log.borrow_mut();
            log.push(Call::KeepInHistory(in_history));
            log.push(Call::Set(text.to_owned()));
        }
        if let Some(err) = &self.set_error {
            return Err(err.clone());
        }
        self.seq += 1;
        Ok(self.seq)
    }
    fn sequence(&self) -> u32 {
        self.log.borrow_mut().push(Call::Seq);
        self.seq + u32::from(self.foreign_write)
    }
    fn restore(&mut self, _snapshot: &bool, in_history: bool) -> Result<u32, ClipError> {
        let mut log = self.log.borrow_mut();
        log.push(Call::RestoreInHistory(in_history));
        log.push(Call::Restore);
        Ok(self.seq)
    }
}

struct FakeInput {
    log: Log,
    app: &'static str,
    elevated: bool,
    /// Modifiers stay down until this much time has been slept.
    held_ms: u64,
    clock: Cell<u64>,
    chord_blocked: bool,
    type_blocked: bool,
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
        if self.chord_blocked {
            Err(InputBlocked)
        } else {
            Ok(())
        }
    }
    fn type_units(&mut self, units: &[u16], gap: Millis) -> Result<(), InputBlocked> {
        assert_eq!(gap, TYPE_GAP);
        self.log.borrow_mut().push(Call::Type(units.to_vec()));
        if self.type_blocked {
            Err(InputBlocked)
        } else {
            Ok(())
        }
    }
    fn sleep(&mut self, time: Millis) {
        self.log.borrow_mut().push(Call::Sleep(time.0));
        self.clock.set(self.clock.get() + time.0);
    }
}

/// Which call the fakes make fail.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fail {
    Nothing,
    SetText,
    Chord,
    Typing,
}

struct Case {
    app: &'static str,
    elevated: bool,
    held_ms: u64,
    snap: SnapKind,
    foreign_write: bool,
    excluded: bool,
    fail: Fail,
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
            excluded: false,
            fail: Fail::Nothing,
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
        set_error: match case.fail {
            Fail::SetText => Some(ClipError::Os {
                call: "SetClipboardData",
                code: 5,
            }),
            _ => None,
        },
        excluded: case.excluded,
    };
    let mut input = FakeInput {
        log: log.clone(),
        app: case.app,
        elevated: case.elevated,
        held_ms: case.held_ms,
        clock: Cell::new(0),
        chord_blocked: case.fail == Fail::Chord,
        type_blocked: case.fail == Fail::Typing,
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
            // Before the snapshot and after the history work: a copy in between is restored.
            Call::Seq,
            Call::Snapshot,
            Call::Seq,
            Call::KeepInHistory(true),
            Call::Set("hi ".into()),
            ctrl_v(),
            Call::Sleep(300),
            Call::Seq,
            Call::RestoreInHistory(false),
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
    // A snapshot is read (is the user's copy kept out of Win+V?), never restored.
    assert!(!has(&calls, |c| matches!(
        c,
        Call::Chord(_) | Call::Restore
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
fn incomplete_snapshot_types_and_never_overwrites_the_clipboard() {
    // Item 52 (owner 2026-11-10): a copy that cannot be put back whole is not overwritten.
    for text in ["hi".to_owned(), "я".repeat(300), "я".repeat(301)] {
        let (out, calls) = run(
            &Case {
                snap: SnapKind::Incomplete,
                ..Case::default()
            },
            &text,
        );
        assert_eq!(out, PasteOutcome::Typed);
        assert!(has(&calls, |c| matches!(c, Call::Type(_))));
        assert!(!has(&calls, |c| matches!(
            c,
            Call::Set(_) | Call::Chord(_) | Call::Restore
        )));
    }
}

#[test]
fn type_mode_types_and_never_touches_the_clipboard_text() {
    let config = Paste {
        mode: PasteMode::Type,
        ..Paste::default()
    };
    let (out, calls) = run(
        &Case {
            snap: SnapKind::Incomplete,
            config,
            ..Case::default()
        },
        "hi",
    );
    assert_eq!(out, PasteOutcome::Typed);
    assert!(!has(&calls, |c| matches!(
        c,
        Call::Snapshot | Call::Set(_) | Call::Chord(_)
    )));
}

#[test]
fn the_history_setting_reaches_the_clipboard_before_any_set() {
    for keep in [true, false] {
        let config = Paste {
            keep_in_clipboard_history: keep,
            ..Paste::default()
        };
        // A normal paste and a clipboard-only path (elevated target) both set text.
        for elevated in [false, true] {
            let (_, calls) = run(
                &Case {
                    config: config.clone(),
                    elevated,
                    ..Case::default()
                },
                "hi",
            );
            let set = calls
                .iter()
                .position(|c| matches!(c, Call::Set(_)))
                .unwrap();
            assert_eq!(calls[set - 1], Call::KeepInHistory(keep));
        }
    }
}

#[test]
fn the_restored_copy_never_enters_history() {
    // Win+V still has its own entry of the copy: a restore into history would add a twin and
    // push the oldest entry out of a full history (item 52, owner 2026-11-10).
    for (keep, excluded) in [(true, false), (true, true), (false, false), (false, true)] {
        let (out, calls) = run(
            &Case {
                excluded,
                config: Paste {
                    keep_in_clipboard_history: keep,
                    ..Paste::default()
                },
                ..Case::default()
            },
            "hi",
        );
        assert_eq!(out, PasteOutcome::Pasted);
        assert!(
            has(&calls, |c| *c == Call::KeepInHistory(keep)),
            "the dictation follows the setting"
        );
        assert!(
            has(&calls, |c| *c == Call::RestoreInHistory(false)),
            "keep {keep} excluded {excluded}"
        );
    }
}

#[test]
fn a_failed_set_restores_an_excluded_copy_still_excluded() {
    let (out, calls) = run(
        &Case {
            excluded: true,
            fail: Fail::SetText,
            ..Case::default()
        },
        "hi",
    );
    assert_eq!(out, PasteOutcome::Failed);
    assert!(has(&calls, |c| *c == Call::RestoreInHistory(false)));
}

#[test]
fn copy_only_follows_its_flag_after_a_paste_with_the_opposite_setting() {
    for keep in [true, false] {
        let log = Log::default();
        let mut clip = FakeClip {
            log: log.clone(),
            snap: SnapKind::Complete,
            seq: 7,
            foreign_write: false,
            set_error: None,
            excluded: false,
        };
        let mut input = FakeInput {
            log: log.clone(),
            app: "notepad.exe",
            elevated: false,
            held_ms: 0,
            clock: Cell::new(0),
            chord_blocked: false,
            type_blocked: false,
        };
        let config = Paste {
            keep_in_clipboard_history: keep,
            ..Paste::default()
        };
        paste("dictated", &config, &mut clip, &mut input);
        copy_only("last", !keep, &mut clip).unwrap();
        let calls = log.borrow();
        assert_eq!(
            calls[calls.len() - 2..],
            [Call::KeepInHistory(!keep), Call::Set("last".into())]
        );
    }
}

#[test]
fn history_is_kept_by_default() {
    assert!(Paste::default().keep_in_clipboard_history);
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

fn sends(calls: &[Call]) -> bool {
    has(calls, |c| matches!(c, Call::Chord(_) | Call::Type(_)))
}

#[test]
fn set_text_error_fails_restores_the_clipboard_and_sends_nothing() {
    let (out, calls) = run(
        &Case {
            fail: Fail::SetText,
            ..Case::default()
        },
        "hi",
    );
    assert_eq!(out, PasteOutcome::Failed);
    // The failed set may have emptied the clipboard: the saved data goes back.
    assert!(has(&calls, |c| *c == Call::Restore));
    assert!(!sends(&calls));
}

#[test]
fn set_text_error_on_a_clipboard_only_path_fails() {
    let (out, calls) = run(
        &Case {
            elevated: true,
            fail: Fail::SetText,
            ..Case::default()
        },
        "hi",
    );
    assert_eq!(out, PasteOutcome::Failed);
    assert!(!sends(&calls));
}

#[test]
fn blocked_chord_fails_without_waiting_but_still_restores() {
    let (out, calls) = run(
        &Case {
            fail: Fail::Chord,
            ..Case::default()
        },
        "hi",
    );
    assert_eq!(out, PasteOutcome::Failed);
    assert!(has(&calls, |c| *c == ctrl_v()));
    // Nothing was pasted, so no restore delay; our text is still removed from the clipboard.
    assert!(!has(&calls, |c| matches!(c, Call::Sleep(_))));
    assert!(has(&calls, |c| *c == Call::Restore));
}

#[test]
fn blocked_typing_fails_in_every_typing_path() {
    let type_mode = Paste {
        mode: PasteMode::Type,
        ..Paste::default()
    };
    for case in [
        Case {
            snap: SnapKind::Locked,
            ..Case::default()
        },
        Case {
            snap: SnapKind::Incomplete,
            ..Case::default()
        },
        Case {
            config: type_mode,
            ..Case::default()
        },
    ] {
        let case = Case {
            fail: Fail::Typing,
            ..case
        };
        let (out, calls) = run(&case, "hi");
        assert_eq!(out, PasteOutcome::Failed);
        assert!(has(&calls, |c| matches!(c, Call::Type(_))));
        assert!(!has(&calls, |c| matches!(
            c,
            Call::Set(_) | Call::Chord(_) | Call::Restore
        )));
    }
}

/// t15 step 4: the entries of ours outlive a daemon restart as ids and a number, never text.
#[test]
fn our_entries_go_to_text_and_back_and_a_bad_line_is_skipped() {
    let text = "seq 41\ntop {B}\nid {A}\nid {C}\n";
    let ours = OurEntries::from_text(text);
    assert_eq!(ours.to_text(), text);
    assert_eq!(OurEntries::from_text(&ours.to_text()), ours);

    let torn = "seq x\nid {A}\nid {A}\nid \nwhat {D}\nid {E}\r\ntop";
    assert_eq!(OurEntries::from_text(torn).to_text(), "id {A}\nid {E}\n");
    assert_eq!(OurEntries::default().to_text(), "");
    assert_eq!(OurEntries::from_text(""), OurEntries::default());
}
