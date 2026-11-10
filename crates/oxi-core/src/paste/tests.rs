//! Item 52: the user's copy survives dictation, Win+V shows the latest dictation on top, keeps
//! one dictation and never loses an entry of the user's, against a clipboard that models the OS
//! history (a write with `in_history` puts a new entry on top; a full history drops its oldest).

use super::*;

/// Data the fake treats as an image: no text, shown in history as a non-text entry.
const IMAGE: &str = "img:";
/// Data the fake's history never takes (files).
const UNREGISTERED: &str = "big:";
/// What a history call that did not answer in time returns.
const TIMED_OUT: ClipError = ClipError::Os {
    call: "WinRT clipboard history",
    code: 0x8007_05B4,
};

/// A clipboard with a Win+V list. Restoring writes the user's data again, as the OS does.
struct HistClip {
    current: String,
    /// Win+V, newest first: (id, data). Never longer than [`HISTORY_CAP`].
    list: Vec<(String, String)>,
    next_id: u32,
    seq: u32,
    history_on: bool,
    /// The snapshot cannot save the clipboard whole (too big, hung or slow owner).
    incomplete: bool,
    /// Every restore fails with an OS error.
    restore_fails: bool,
    /// The next this many restores find the clipboard held open.
    restore_locked: u32,
    /// Another app writes this while a restore finds the clipboard held.
    foreign_on_lock: Option<&'static str>,
    restores: u32,
    /// Looks at the list before a write shows in it, as the OS registers it late.
    lag: u32,
    /// Writes not in the list yet, with the looks left. A delete drops them: the OS loses a
    /// write it was still registering (seen live).
    pending: Vec<(u32, (String, String))>,
    /// History calls so far (list reads and deletes).
    looks: u32,
    /// Every call whose number is a multiple of this times out (0: none).
    fail_every: u32,
    /// What each history call costs on the clock.
    call_ms: u64,
    clock: u64,
    /// Entries the full history pushed out.
    evicted: u32,
    /// What the worker last capped a history call at ([`Clipboard::limit_history_calls`]).
    limit: u64,
    /// The user copies this (into history) as history call number `.0` starts.
    copy_on_call: Option<(u32, &'static str)>,
}

impl HistClip {
    fn new(user: &str) -> Self {
        Self::with_entries(1, user)
    }

    /// A history of `n` entries, the user's copy the newest and current.
    fn with_entries(n: u32, user: &str) -> Self {
        let mut clip = Self {
            current: String::new(),
            list: Vec::new(),
            next_id: 0,
            seq: 0,
            history_on: true,
            incomplete: false,
            restore_fails: false,
            restore_locked: 0,
            foreign_on_lock: None,
            restores: 0,
            lag: 0,
            pending: Vec::new(),
            looks: 0,
            fail_every: 0,
            call_ms: 0,
            clock: 0,
            evicted: 0,
            limit: HISTORY_TIMEOUT.0,
            copy_on_call: None,
        };
        for k in 1..n {
            clip.write(&format!("old {k}"), true);
        }
        clip.write(user, true);
        clip
    }

    fn write(&mut self, data: &str, in_history: bool) -> u32 {
        data.clone_into(&mut self.current);
        self.seq += 1;
        if in_history && self.history_on && !data.starts_with(UNREGISTERED) {
            self.next_id += 1;
            let entry = (format!("id{}", self.next_id), data.to_owned());
            if self.lag == 0 {
                self.add(entry);
            } else {
                self.pending.push((self.lag, entry));
            }
        }
        self.seq
    }

    fn add(&mut self, entry: (String, String)) {
        self.list.insert(0, entry);
        if self.list.len() > HISTORY_CAP {
            self.list.pop();
            self.evicted += 1;
        }
    }

    /// One call into the history service: it may time out; otherwise the OS registers a bit
    /// more. `Ok(false)`: history off.
    fn call(&mut self) -> Result<bool, ClipError> {
        self.looks += 1;
        if let Some((n, data)) = self.copy_on_call
            && n == self.looks
        {
            self.write(data, true);
        }
        // A call slower than its limit is given up at the limit.
        self.clock += self.call_ms.min(self.limit);
        if self.call_ms > self.limit
            || self.fail_every > 0 && self.looks.is_multiple_of(self.fail_every)
        {
            return Err(TIMED_OUT);
        }
        if !self.history_on {
            return Ok(false);
        }
        for (left, _) in &mut self.pending {
            *left -= 1;
        }
        while let Some(i) = self.pending.iter().position(|(left, _)| *left == 0) {
            let (_, entry) = self.pending.remove(i);
            self.add(entry);
        }
        Ok(true)
    }

    fn texts(&self) -> Vec<&str> {
        self.list.iter().map(|(_, t)| t.as_str()).collect()
    }

    fn dictations(&self) -> usize {
        self.texts()
            .iter()
            .filter(|t| t.starts_with("dictation"))
            .count()
    }
}

impl Clipboard for HistClip {
    type Snapshot = String;
    fn snapshot(&mut self) -> Snap<String> {
        if self.incomplete {
            Snap::Incomplete
        } else {
            Snap::Complete(self.current.clone())
        }
    }
    fn is_excluded(&self, saved: &String) -> bool {
        saved.starts_with(UNREGISTERED)
    }
    fn set_text(&mut self, text: &str, in_history: bool) -> Result<u32, ClipError> {
        Ok(self.write(text, in_history))
    }
    fn sequence(&self) -> u32 {
        self.seq
    }
    fn restore(&mut self, snapshot: &String, in_history: bool) -> Result<u32, ClipError> {
        self.restores += 1;
        if self.restore_locked > 0 {
            self.restore_locked -= 1;
            if let Some(theirs) = self.foreign_on_lock {
                self.write(theirs, true);
            }
            return Err(ClipError::Locked);
        }
        if self.restore_fails {
            // Failed after emptying the clipboard: the number moves, the copy is gone.
            self.current.clear();
            self.seq += 1;
            return Err(ClipError::Os {
                call: "SetClipboardData",
                code: 5,
            });
        }
        Ok(self.write(snapshot, in_history))
    }
    fn history(&mut self) -> Option<Vec<HistoryItem>> {
        if !self.call().ok()? {
            return None;
        }
        Some(
            self.list
                .iter()
                .map(|(id, data)| HistoryItem {
                    id: id.clone(),
                    text: (!data.starts_with(IMAGE)).then(|| data.clone()),
                })
                .collect(),
        )
    }
    fn history_ids(&mut self) -> Result<Option<Vec<String>>, ClipError> {
        let on = self.call()?;
        Ok(on.then(|| self.list.iter().map(|(id, _)| id.clone()).collect()))
    }
    fn delete_from_history(&mut self, id: &str) -> Result<(), ClipError> {
        self.call()?;
        self.pending.clear();
        let before = self.list.len();
        self.list.retain(|(i, _)| i != id);
        if self.list.len() == before {
            return Err(ClipError::Os {
                call: "DeleteItemFromHistory",
                code: 0,
            });
        }
        Ok(())
    }
    fn now(&self) -> Millis {
        Millis(self.clock)
    }
    fn limit_history_calls(&mut self, limit: Millis) {
        self.limit = limit.0;
    }
}

#[derive(Default)]
struct Keys {
    held: bool,
    elevated: bool,
    typed: Option<String>,
    slept: u64,
}

impl Input for Keys {
    fn modifiers_held(&self) -> bool {
        self.held
    }
    fn foreground(&self) -> Foreground {
        Foreground {
            app: "notepad.exe".into(),
            elevated: self.elevated,
        }
    }
    fn send_chord(&mut self, _: &[Key]) -> Result<(), InputBlocked> {
        Ok(())
    }
    fn type_units(&mut self, units: &[u16], _: Millis) -> Result<(), InputBlocked> {
        self.typed = Some(String::from_utf16_lossy(units));
        Ok(())
    }
    fn sleep(&mut self, time: Millis) {
        self.slept += time.0;
    }
}

/// One dictation; asserts it pushed no entry out of the history.
fn dictate(clip: &mut HistClip, cfg: &Paste, ours: &mut OurEntries, n: u32) -> PasteOutcome {
    let evicted = clip.evicted;
    let out = paste_with(
        &format!("dictation {n}"),
        cfg,
        clip,
        &mut Keys::default(),
        ours,
    );
    assert_eq!(clip.evicted, evicted, "dictation {n} pushed an entry out");
    out
}

/// Lets the fake OS finish registering whatever the worker did not wait for.
fn settle(clip: &mut HistClip) {
    let fail = std::mem::take(&mut clip.fail_every);
    for _ in 0..clip.lag {
        clip.history();
    }
    clip.fail_every = fail;
}

#[test]
fn ctrl_v_gives_the_users_copy_and_win_v_the_latest_dictation_after_30_dictations() {
    let mut clip = HistClip::new("my copy");
    let (cfg, mut ours) = (Paste::default(), OurEntries::default());
    for n in 1..=30 {
        assert_eq!(dictate(&mut clip, &cfg, &mut ours, n), PasteOutcome::Pasted);
        assert_eq!(clip.current, "my copy", "after dictation {n}");
        // The restore adds no twin of the copy: one dictation was added, nothing else.
        assert_eq!(clip.texts(), [&format!("dictation {n}"), "my copy"]);
    }
}

#[test]
fn only_the_latest_dictation_stays_in_history() {
    let mut clip = HistClip::new("my copy");
    let (cfg, mut ours) = (Paste::default(), OurEntries::default());
    for n in 1..=5 {
        dictate(&mut clip, &cfg, &mut ours, n);
    }
    let dictations: Vec<_> = clip
        .texts()
        .into_iter()
        .filter(|t| t.starts_with("dictation"))
        .collect();
    assert_eq!(dictations, ["dictation 5"]);
}

/// NEW 2: a write seen late, or after the worker stopped looking, is still found and deleted.
#[test]
fn a_slow_history_never_loses_or_leaks_a_dictation() {
    for lag in [1, 3, 9, 15, 40] {
        let mut clip = HistClip::new("my copy");
        clip.lag = lag;
        let (cfg, mut ours) = (Paste::default(), OurEntries::default());
        for n in 1..=6 {
            dictate(&mut clip, &cfg, &mut ours, n);
            settle(&mut clip);
            assert_eq!(
                clip.texts(),
                [&format!("dictation {n}"), "my copy"],
                "lag {lag}, dictation {n}"
            );
        }
    }
}

#[test]
fn the_same_text_dictated_twice_leaves_one_entry() {
    let mut clip = HistClip::new("my copy");
    let (cfg, mut ours) = (Paste::default(), OurEntries::default());
    for _ in 0..3 {
        paste_with("same", &cfg, &mut clip, &mut Keys::default(), &mut ours);
    }
    assert_eq!(clip.texts(), ["same", "my copy"]);
}

#[test]
fn a_user_copy_equal_to_the_last_dictation_is_kept() {
    let mut clip = HistClip::new("my copy");
    let (cfg, mut ours) = (Paste::default(), OurEntries::default());
    dictate(&mut clip, &cfg, &mut ours, 1);
    // The user copies the dictated text from somewhere (e.g. "Copy last text").
    clip.write("dictation 1", true);
    dictate(&mut clip, &cfg, &mut ours, 2);
    assert_eq!(clip.current, "dictation 1");
    assert_eq!(clip.texts(), ["dictation 2", "dictation 1", "my copy"]);
}

/// Defect 8: deletion goes by the dictation's own id, never by text.
#[test]
fn an_older_or_pinned_user_entry_with_the_dictated_text_is_never_deleted() {
    let mut clip = HistClip::new("hello");
    clip.write("my copy", true);
    let (cfg, mut ours) = (Paste::default(), OurEntries::default());
    for _ in 0..3 {
        paste_with("hello", &cfg, &mut clip, &mut Keys::default(), &mut ours);
    }
    assert_eq!(clip.texts(), ["hello", "my copy", "hello"]);
    // The user's own "hello" (id1) is the older one and still there.
    assert_eq!(clip.list[2].0, "id1");
}

#[test]
fn history_off_restores_plainly_and_lists_nothing() {
    let mut clip = HistClip::new("my copy");
    clip.history_on = false;
    clip.list.clear();
    let (cfg, mut ours) = (Paste::default(), OurEntries::default());
    assert_eq!(dictate(&mut clip, &cfg, &mut ours, 1), PasteOutcome::Pasted);
    assert_eq!(clip.current, "my copy");
    assert_eq!(clip.list.len(), 0);
    assert!(ours.is_empty());
}

#[test]
fn keep_off_with_nothing_of_ours_reads_no_history() {
    let mut clip = HistClip::new("my copy");
    let cfg = Paste {
        keep_in_clipboard_history: false,
        ..Paste::default()
    };
    let mut ours = OurEntries::default();
    dictate(&mut clip, &cfg, &mut ours, 1);
    assert_eq!(clip.current, "my copy");
    assert_eq!(clip.texts(), ["my copy"]);
    assert_eq!(clip.looks, 0, "no history calls with the setting off");
}

/// NEW 1: turning the setting off still deletes the dictation the last paste left.
#[test]
fn keep_off_still_deletes_our_last_dictation() {
    let mut clip = HistClip::new("my copy");
    let mut ours = OurEntries::default();
    dictate(&mut clip, &Paste::default(), &mut ours, 1);
    let off = Paste {
        keep_in_clipboard_history: false,
        ..Paste::default()
    };
    dictate(&mut clip, &off, &mut ours, 2);
    assert_eq!(clip.texts(), ["my copy"]);
    assert!(ours.is_empty());
}

/// Defects 1 and 2: a copy over the snapshot cap, or from an owner that hangs, is not saved
/// whole; it is never overwritten, the text is typed instead.
#[test]
fn a_copy_that_cannot_be_saved_whole_is_left_alone_and_the_text_typed() {
    let mut clip = HistClip::new("50 MB screenshot");
    clip.incomplete = true;
    let mut ours = OurEntries {
        ids: vec!["id0".into()],
        ..OurEntries::default()
    };
    let mut keys = Keys::default();
    let out = paste_with(
        "hi there",
        &Paste::default(),
        &mut clip,
        &mut keys,
        &mut ours,
    );
    assert_eq!(out, PasteOutcome::Typed);
    assert_eq!(keys.typed.as_deref(), Some("hi there"));
    assert_eq!(clip.current, "50 MB screenshot");
    assert_eq!(clip.seq, 1, "the clipboard was never written");
    assert_eq!(clip.texts(), ["50 MB screenshot"]);
    assert_eq!(ours.ids, ["id0"], "nothing deleted");
}

/// Defect 3: a clipboard held open right after the paste is waited out.
#[test]
fn a_restore_that_finds_the_clipboard_held_is_retried() {
    let mut clip = HistClip::new("my copy");
    clip.restore_locked = RESTORE_TRIES - 1;
    let (cfg, mut ours) = (Paste::default(), OurEntries::default());
    assert_eq!(dictate(&mut clip, &cfg, &mut ours, 1), PasteOutcome::Pasted);
    assert_eq!(clip.current, "my copy");
    assert_eq!(clip.texts(), ["dictation 1", "my copy"]);
}

#[test]
fn a_restore_held_through_every_try_is_reported_and_bounded() {
    let mut clip = HistClip::new("my copy");
    clip.restore_locked = u32::MAX;
    let (cfg, mut ours) = (Paste::default(), OurEntries::default());
    let mut keys = Keys::default();
    let out = paste_with("dictation 1", &cfg, &mut clip, &mut keys, &mut ours);
    assert_eq!(out, PasteOutcome::PastedNoRestore);
    assert_eq!(clip.restores, RESTORE_TRIES);
    // Restore delay plus 50 + 100 + 200 + 400 ms of gaps.
    assert_eq!(keys.slept, 300 + 750);
    assert_eq!(clip.current, "dictation 1");
}

#[test]
fn a_write_by_another_app_during_the_retries_is_kept() {
    let mut clip = HistClip::new("my copy");
    clip.restore_locked = 1;
    clip.foreign_on_lock = Some("theirs");
    let (cfg, mut ours) = (Paste::default(), OurEntries::default());
    assert_eq!(dictate(&mut clip, &cfg, &mut ours, 1), PasteOutcome::Pasted);
    assert_eq!(clip.current, "theirs");
    assert_eq!(clip.restores, 1);
}

/// NEW 1: a failed restore no longer keeps the previous dictation: it went before the write.
#[test]
fn a_failed_restore_is_reported_and_leaves_one_dictation() {
    let mut clip = HistClip::new("my copy");
    let (cfg, mut ours) = (Paste::default(), OurEntries::default());
    dictate(&mut clip, &cfg, &mut ours, 1);
    clip.restore_fails = true;
    assert_eq!(
        dictate(&mut clip, &cfg, &mut ours, 2),
        PasteOutcome::PastedNoRestore
    );
    // An OS failure moves the number itself: still retried, not taken for another app.
    assert_eq!(clip.restores, 1 + RESTORE_TRIES);
    assert_eq!(clip.texts(), ["dictation 2", "my copy"]);
    // The next dictation (restore works again) replaces it.
    clip.restore_fails = false;
    clip.write("my copy", true);
    assert_eq!(dictate(&mut clip, &cfg, &mut ours, 3), PasteOutcome::Pasted);
    assert_eq!(clip.current, "my copy");
    assert_eq!(clip.texts(), ["dictation 3", "my copy", "my copy"]);
}

/// Defect 5: an image copy adds no history entries.
#[test]
fn an_image_copy_is_restored_without_adding_history_entries() {
    let mut clip = HistClip::new("img:cat");
    let (cfg, mut ours) = (Paste::default(), OurEntries::default());
    for n in 1..=30 {
        assert_eq!(dictate(&mut clip, &cfg, &mut ours, n), PasteOutcome::Pasted);
        assert_eq!(clip.current, "img:cat");
        assert_eq!(clip.texts(), [&format!("dictation {n}"), "img:cat"]);
    }
}

/// Defect 6: one list call, one delete, one look and one text read per dictation, and no wait.
#[test]
fn a_dictation_costs_four_history_calls_and_no_wait() {
    for user in ["my copy", "img:files from Explorer", "big:files"] {
        let mut clip = HistClip::new(user);
        let (cfg, mut ours) = (Paste::default(), OurEntries::default());
        for n in 1..=3 {
            let mut keys = Keys::default();
            let looks = clip.looks;
            paste_with(&format!("d{n}"), &cfg, &mut clip, &mut keys, &mut ours);
            // The first has nothing of ours to delete.
            assert_eq!(clip.looks - looks, if n == 1 { 3 } else { 4 }, "{user}");
            assert_eq!(keys.slept, 300, "{user}");
            assert_eq!(clip.current, user);
        }
    }
}

/// The overall limit: a slow history service costs about [`HISTORY_BUDGET`] per paste, never
/// a timeout per call, and nothing is pushed out or left behind once it is fast again.
#[test]
fn a_slow_history_service_stops_at_the_overall_limit() {
    for call_ms in [500, 1000, 2000, 5000] {
        let mut clip = HistClip::new("my copy");
        let (cfg, mut ours) = (Paste::default(), OurEntries::default());
        dictate(&mut clip, &cfg, &mut ours, 0);
        clip.call_ms = call_ms;
        for n in 1..=5 {
            let start = clip.clock;
            dictate(&mut clip, &cfg, &mut ours, n);
            // Each call is capped at the time left: the limit is a deadline.
            assert!(
                clip.clock - start <= HISTORY_BUDGET.0 + 300,
                "{call_ms} ms a call"
            );
            assert_eq!(clip.current, "my copy");
        }
        clip.call_ms = 0;
        dictate(&mut clip, &cfg, &mut ours, 6);
        assert_eq!(
            clip.texts(),
            ["dictation 6", "my copy"],
            "{call_ms} ms a call"
        );
    }
}

/// NEW 3: 30 dictations into a full or nearly full history push none of the user's entries out
/// (checked in `dictate`), and nothing of ours is left.
#[test]
fn a_full_history_loses_no_entry() {
    for start in [24, 25] {
        let mut clip = HistClip::with_entries(start, "my copy");
        let before: Vec<_> = clip.list.clone();
        let (cfg, mut ours) = (Paste::default(), OurEntries::default());
        for n in 1..=30 {
            assert_eq!(dictate(&mut clip, &cfg, &mut ours, n), PasteOutcome::Pasted);
            assert_eq!(clip.current, "my copy");
            assert!(clip.list.len() <= HISTORY_CAP);
        }
        assert!(
            before.iter().all(|e| clip.list.contains(e)),
            "start {start}"
        );
        // Room for one: the latest dictation on top. Full: the dictation stays out.
        let want = usize::from(start < 25);
        assert_eq!(clip.dictations(), want, "start {start}");
        if want == 1 {
            assert_eq!(clip.texts()[0], "dictation 30");
        }
    }
}

#[test]
fn a_full_history_loses_no_entry_with_copies_in_between() {
    let mut clip = HistClip::with_entries(20, "my copy");
    let (cfg, mut ours) = (Paste::default(), OurEntries::default());
    for n in 1..=30 {
        dictate(&mut clip, &cfg, &mut ours, n);
        if n % 3 == 0 {
            // The user's own copies may push old entries out: that is Windows, not us.
            clip.write(&format!("copy {n}"), true);
        }
        assert!(clip.dictations() <= 1, "dictation {n}");
        assert_eq!(
            clip.current,
            if n >= 3 {
                format!("copy {}", n / 3 * 3)
            } else {
                "my copy".into()
            }
        );
    }
    for n in 1..=10 {
        assert!(clip.texts().contains(&format!("copy {}", n * 3).as_str()));
    }
}

/// NEW 1 and 2 with timeouts: a list, delete or look that times out pushes nothing out, and
/// whatever it left is deleted by a later paste.
#[test]
fn a_full_history_loses_no_entry_when_history_calls_time_out() {
    for start in [3, 24] {
        for fail_every in [2, 3, 4, 5, 7] {
            for lag in [0, 2, 15] {
                let mut clip = HistClip::with_entries(start, "my copy");
                let before: Vec<_> = clip.list.clone();
                clip.fail_every = fail_every;
                clip.lag = lag;
                let (cfg, mut ours) = (Paste::default(), OurEntries::default());
                for n in 1..=30 {
                    dictate(&mut clip, &cfg, &mut ours, n);
                    settle(&mut clip);
                    assert_eq!(clip.current, "my copy");
                }
                clip.fail_every = 0;
                for n in 31..=33 {
                    dictate(&mut clip, &cfg, &mut ours, n);
                    settle(&mut clip);
                }
                let case = format!("start {start}, every {fail_every}, lag {lag}");
                assert!(before.iter().all(|e| clip.list.contains(e)), "{case}");
                assert_eq!(clip.dictations(), 1, "{case}: {:?}", clip.texts());
                assert_eq!(clip.texts()[0], "dictation 33", "{case}");
            }
        }
    }
}

/// Defect 7: the clipboard-only paths leave the text current and one dictation in Win+V.
#[test]
fn clipboard_only_paths_never_lose_or_leak_the_dictation() {
    for lag in [0, 1, 3, 9, 15] {
        for (held, elevated) in [(true, false), (false, true)] {
            let mut clip = HistClip::new("my copy");
            clip.lag = lag;
            let mut ours = OurEntries::default();
            for n in 1..=4 {
                let mut keys = Keys {
                    held,
                    elevated,
                    ..Keys::default()
                };
                let text = format!("dictation {n}");
                let out = paste_with(&text, &Paste::default(), &mut clip, &mut keys, &mut ours);
                assert!(matches!(out, PasteOutcome::ClipboardOnly(_)));
                settle(&mut clip);
                assert_eq!(clip.current, text);
                assert_eq!(clip.texts(), [text.as_str(), "my copy"], "lag {lag}");
            }
        }
    }
}

#[test]
fn a_clipboard_without_history_still_pastes_and_restores() {
    // Default `history`/`delete_from_history`: nothing to tidy.
    struct Plain(String);
    impl Clipboard for Plain {
        type Snapshot = String;
        fn snapshot(&mut self) -> Snap<String> {
            Snap::Complete(self.0.clone())
        }
        fn is_excluded(&self, _: &String) -> bool {
            false
        }
        fn set_text(&mut self, text: &str, _: bool) -> Result<u32, ClipError> {
            text.clone_into(&mut self.0);
            Ok(1)
        }
        fn sequence(&self) -> u32 {
            1
        }
        fn restore(&mut self, s: &String, _: bool) -> Result<u32, ClipError> {
            s.clone_into(&mut self.0);
            Ok(2)
        }
    }
    let mut clip = Plain("mine".into());
    let out = paste("hi", &Paste::default(), &mut clip, &mut Keys::default());
    assert_eq!(out, PasteOutcome::Pasted);
    assert_eq!(clip.0, "mine");
}

/// Race 1: a copy the history registers late, after the list call, is never taken for the
/// dictation's entry (it would be deleted by the next paste) and the dictation is still recorded.
#[test]
fn a_copy_registering_late_is_never_taken_for_ours() {
    for lag in [0, 1, 2, 3] {
        let mut clip = HistClip::new("my copy");
        clip.lag = lag;
        clip.write("late copy", true);
        let (cfg, mut ours) = (Paste::default(), OurEntries::default());
        for n in 1..=3 {
            dictate(&mut clip, &cfg, &mut ours, n);
            settle(&mut clip);
            assert_eq!(
                clip.texts(),
                [&format!("dictation {n}"), "late copy", "my copy"],
                "lag {lag}, dictation {n}"
            );
        }
    }
}

/// Races 1 and 2 with copies between dictations, registered 0 to 3 history calls late: no
/// copy is dropped by a delete or deleted as ours, and one dictation stays.
#[test]
fn copies_in_between_registering_late_are_never_lost() {
    for start in [3, 15] {
        for lag in [0, 1, 2, 3] {
            let mut clip = HistClip::with_entries(start, "my copy");
            let before: Vec<_> = clip.list.clone();
            clip.lag = lag;
            let (cfg, mut ours) = (Paste::default(), OurEntries::default());
            for n in 1..=8 {
                dictate(&mut clip, &cfg, &mut ours, n);
                assert_eq!(
                    clip.current,
                    if n == 1 {
                        "my copy".into()
                    } else {
                        format!("copy {}", n - 1)
                    }
                );
                // Registers during the next paste's history work.
                clip.write(&format!("copy {n}"), true);
            }
            dictate(&mut clip, &cfg, &mut ours, 9);
            settle(&mut clip);
            let case = format!("start {start}, lag {lag}: {:?}", clip.texts());
            assert!(before.iter().all(|e| clip.list.contains(e)), "{case}");
            for n in 1..=8 {
                assert!(
                    clip.texts().contains(&format!("copy {n}").as_str()),
                    "{case}"
                );
            }
            assert_eq!(clip.dictations(), 1, "{case}");
            assert_eq!(clip.texts()[0], "dictation 9", "{case}");
        }
    }
}

/// Race 1 at 24 entries: a copy still registering takes the 25th place, so the dictation stays
/// out instead of pushing the oldest entry out (checked in `dictate`).
#[test]
fn a_late_copy_into_a_nearly_full_history_pushes_nothing_out() {
    for lag in [0, 1, 2] {
        let mut clip = HistClip::with_entries(24, "my copy");
        let before: Vec<_> = clip.list.clone();
        clip.lag = lag;
        clip.write("late copy", true);
        let (cfg, mut ours) = (Paste::default(), OurEntries::default());
        dictate(&mut clip, &cfg, &mut ours, 1);
        settle(&mut clip);
        assert_eq!(clip.texts()[0], "late copy", "lag {lag}");
        assert!(before.iter().all(|e| clip.list.contains(e)), "lag {lag}");
    }
}

/// Races 2 and 3: the user copies while the history work runs (at the list call or the delete).
/// That copy is not dropped by a delete and is what goes back on the clipboard.
#[test]
fn a_copy_during_the_history_work_is_kept_and_restored() {
    for (at, lag) in [(1, 0), (1, 1), (1, 2), (1, 3), (2, 0)] {
        let mut clip = HistClip::new("my copy");
        clip.lag = lag;
        let (cfg, mut ours) = (Paste::default(), OurEntries::default());
        dictate(&mut clip, &cfg, &mut ours, 1);
        settle(&mut clip);
        clip.copy_on_call = Some((clip.looks + at, "new copy"));
        assert_eq!(dictate(&mut clip, &cfg, &mut ours, 2), PasteOutcome::Pasted);
        let case = format!("call {at}, lag {lag}");
        assert_eq!(clip.current, "new copy", "{case}");
        settle(&mut clip);
        assert!(
            clip.texts().contains(&"new copy"),
            "{case}: {:?}",
            clip.texts()
        );
        dictate(&mut clip, &cfg, &mut ours, 3);
        settle(&mut clip);
        assert_eq!(clip.current, "new copy", "{case}");
        assert_eq!(
            clip.texts(),
            ["dictation 3", "new copy", "my copy"],
            "{case}"
        );
    }
}

/// Race 4: our write never registered, then the user copies the same text into its place: that
/// entry is the user's and is never deleted.
#[test]
fn a_same_text_copy_in_the_place_of_an_unseen_write_is_kept() {
    let mut clip = HistClip::new("my copy");
    clip.lag = 100;
    let (cfg, mut ours) = (Paste::default(), OurEntries::default());
    dictate(&mut clip, &cfg, &mut ours, 1);
    // The OS never shows it.
    clip.pending.clear();
    clip.lag = 0;
    clip.write("dictation 1", true);
    for n in 2..=3 {
        dictate(&mut clip, &cfg, &mut ours, n);
        assert_eq!(
            clip.texts(),
            [&format!("dictation {n}"), "dictation 1", "my copy"]
        );
    }
}

/// Review defect 1: a copy during the clipboard-only path's history work, registering late, keeps
/// the dictation out of a 24-entry history, so the oldest entry is never pushed out.
#[test]
fn a_copy_during_clipboard_only_history_work_pushes_nothing_out() {
    for (held, elevated) in [(true, false), (false, true)] {
        let mut clip = HistClip::with_entries(23, "my copy");
        let (cfg, mut ours) = (Paste::default(), OurEntries::default());
        dictate(&mut clip, &cfg, &mut ours, 1);
        assert_eq!(clip.list.len(), 24);
        let before: Vec<_> = clip.list[1..].to_vec();
        clip.lag = 2;
        clip.copy_on_call = Some((clip.looks + 1, "new copy"));
        let mut keys = Keys {
            held,
            elevated,
            ..Keys::default()
        };
        let evicted = clip.evicted;
        let out = paste_with("dictation 2", &cfg, &mut clip, &mut keys, &mut ours);
        assert!(matches!(out, PasteOutcome::ClipboardOnly(_)));
        settle(&mut clip);
        let case = format!("held {held}, elevated {elevated}: {:?}", clip.texts());
        assert_eq!(clip.evicted, evicted, "{case}");
        assert!(before.iter().all(|e| clip.list.contains(e)), "{case}");
        assert!(clip.texts().contains(&"new copy"), "{case}");
    }
}

/// Review defect 2: a copy the history never takes (files) made between dictations
/// costs no wait, and our earlier dictation still goes.
#[test]
fn a_copy_the_history_never_takes_costs_no_wait() {
    let mut clip = HistClip::new("my copy");
    let (cfg, mut ours) = (Paste::default(), OurEntries::default());
    for n in 1..=5 {
        clip.write(&format!("big:files {n}"), true);
        let mut keys = Keys::default();
        let out = paste_with(
            &format!("dictation {n}"),
            &cfg,
            &mut clip,
            &mut keys,
            &mut ours,
        );
        assert_eq!(out, PasteOutcome::Pasted);
        assert_eq!(keys.slept, 300, "dictation {n}");
        assert_eq!(clip.current, format!("big:files {n}"));
        assert_eq!(clip.texts(), [&format!("dictation {n}"), "my copy"]);
    }
}

/// Review defect 3: the user copies right after our write (during the restore delay, or after a
/// write the worker stopped waiting for): the dictation is still found and deleted next time.
#[test]
fn a_copy_right_after_our_write_never_leaves_the_dictation_behind() {
    // The copy lands as the first look after the paste starts.
    let mut clip = HistClip::new("my copy");
    let (cfg, mut ours) = (Paste::default(), OurEntries::default());
    dictate(&mut clip, &cfg, &mut ours, 1);
    // List, delete of dictation 1, then the first look for dictation 2.
    clip.copy_on_call = Some((clip.looks + 3, "new copy"));
    assert_eq!(dictate(&mut clip, &cfg, &mut ours, 2), PasteOutcome::Pasted);
    assert_eq!(clip.current, "new copy");
    assert_eq!(clip.texts(), ["new copy", "dictation 2", "my copy"]);
    dictate(&mut clip, &cfg, &mut ours, 3);
    assert_eq!(clip.texts(), ["dictation 3", "new copy", "my copy"]);

    // Our write registers only after the worker's last look, then the user copies.
    let mut clip = HistClip::new("my copy");
    let mut ours = OurEntries::default();
    clip.lag = HISTORY_POLLS + 2;
    dictate(&mut clip, &cfg, &mut ours, 1);
    settle(&mut clip);
    assert_eq!(clip.texts(), ["dictation 1", "my copy"]);
    clip.lag = 0;
    clip.write("new copy", true);
    dictate(&mut clip, &cfg, &mut ours, 2);
    assert_eq!(clip.texts(), ["dictation 2", "new copy", "my copy"]);
}

/// t08c step 1: a copy the history never takes, then a dictation into an elevated window: the
/// clipboard-only path judges that copy too, so it costs no wait and our earlier dictation goes.
#[test]
fn a_copy_the_history_never_takes_costs_no_wait_on_the_clipboard_only_path() {
    let mut clip = HistClip::new("my copy");
    let (cfg, mut ours) = (Paste::default(), OurEntries::default());
    for n in 1..=4 {
        clip.write(&format!("big:files {n}"), true);
        let mut keys = Keys {
            elevated: true,
            ..Keys::default()
        };
        let text = format!("dictation {n}");
        let out = paste_with(&text, &cfg, &mut clip, &mut keys, &mut ours);
        assert_eq!(out, PasteOutcome::ClipboardOnly(ClipboardOnlyWhy::Elevated));
        assert_eq!(keys.slept, 0, "dictation {n}");
        assert_eq!(clip.texts(), [text.as_str(), "my copy"], "dictation {n}");
    }
}

/// t08c step 2: our write registers after the worker's last look, then the user copies and that
/// copy registers late too. Our entry on top is not taken for the copy: the worker waits for the
/// copy above it, then deletes the dictation, and the copy stays.
#[test]
fn our_late_write_on_top_under_a_late_copy_is_still_deleted() {
    for lag in 1..HISTORY_POLLS {
        let mut clip = HistClip::new("my copy");
        let (cfg, mut ours) = (Paste::default(), OurEntries::default());
        clip.lag = HISTORY_POLLS + 2;
        dictate(&mut clip, &cfg, &mut ours, 1);
        settle(&mut clip);
        assert_eq!(clip.texts(), ["dictation 1", "my copy"]);
        clip.lag = lag;
        clip.write("new copy", true);
        dictate(&mut clip, &cfg, &mut ours, 2);
        settle(&mut clip);
        assert_eq!(clip.current, "new copy", "lag {lag}");
        assert_eq!(
            clip.texts(),
            ["dictation 2", "new copy", "my copy"],
            "lag {lag}"
        );
    }
}
