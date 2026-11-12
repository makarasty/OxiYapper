//! Optional Enter after a dictation has fully landed (`paste.press_enter`, default off). It is the
//! only Enter the app ever sends: typed line breaks are flattened to spaces (SPEC). The decision
//! fails closed (owner 2026-11-12): whatever cannot be shown safe sends no Enter.
//!
//! After a Ctrl+V paste the proof is the target's own read (owner 2026-11-12): the text goes on the
//! clipboard by delayed rendering, so the first reader makes Windows ask us for it. Only that
//! request, made by the foreground app after our Ctrl+V and before the restore began, lets the
//! Enter go. A clipboard monitor or Win+V history reading first leaves no request for the target.

use std::time::Instant;

use super::{Input, PasteOutcome};
use crate::hotkey::Key;

/// Where input goes and what the person typed: the foreground window, its focused control and
/// the count of the person's own key-downs (not ours, not the dictation chord), compared for
/// equality only. A `0` control means the window reports none (compared as it is).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Focus {
    pub window: usize,
    pub control: usize,
    pub keys: u64,
}

/// Why no Enter went out. Logged by name; there is no text in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnterSkip {
    /// The setting is off (not logged).
    Off,
    /// The text did not land whole: failed, clipboard only, or nothing to type.
    NotLanded,
    /// A clipboard paste the foreground app was not seen to read: no request for our text came
    /// from it (it ignored Ctrl+V, or someone else read first, a monitor or Win+V history).
    ClipNotTaken,
    /// The foreground app asked for our text before our Ctrl+V: not an answer to the paste.
    ClipTakenEarly,
    /// The request came after the restore began: the app may paste the user's old copy.
    ClipTakenLate,
    /// The OS gave no focus, now or at the record start: a locked or secure desktop (the
    /// coordinator drops the start focus on a lock or a sleep), no foreground, or no key hook.
    FocusUnknown,
    /// Another window or control has the focus than at the record start.
    FocusChanged,
    /// The person pressed a key of their own since the record start: the field may hold their
    /// edit, which the Enter would send.
    Edited,
    /// A modifier is down: Enter would become Shift+Enter (a line break) or Ctrl+Enter.
    ModifiersHeld,
    /// `SendInput` inserted nothing.
    Blocked,
}

/// Whether the text of this outcome is in front of an Enter sent now. Typed text is: it is
/// queued in the same input stream ahead of the Enter. Pasted text is when the foreground app
/// asked for it (`asked`) at or after Ctrl+V went out (`chord`) and before the restore (`gone`).
fn landed(
    outcome: PasteOutcome,
    chord: Option<Instant>,
    asked: Option<Instant>,
    gone: Option<Instant>,
) -> Result<(), EnterSkip> {
    match outcome {
        PasteOutcome::Typed => Ok(()),
        PasteOutcome::Pasted | PasteOutcome::PastedNoRestore => match (chord, asked) {
            (Some(c), Some(a)) if a < c => Err(EnterSkip::ClipTakenEarly),
            (Some(_), Some(a)) if gone.is_some_and(|g| a >= g) => Err(EnterSkip::ClipTakenLate),
            (Some(_), Some(_)) => Ok(()),
            _ => Err(EnterSkip::ClipNotTaken),
        },
        _ => Err(EnterSkip::NotLanded),
    }
}

/// The pure part of the decision, before the keyboard is asked: `start` is the focus at the
/// record start (the first part of a continued dictation), `now` the focus right before Enter.
/// For a clipboard paste: `chord` is when Ctrl+V went out, `asked` when the foreground app asked
/// for our delayed text (`None`: nobody did, or another reader was first), `gone` when the
/// restore of the user's copy began (`None`: it never ran).
pub fn enter_decision(
    on: bool,
    outcome: PasteOutcome,
    start: Option<Focus>,
    now: Option<Focus>,
    [chord, asked, gone]: [Option<Instant>; 3],
) -> Result<(), EnterSkip> {
    if !on {
        return Err(EnterSkip::Off);
    }
    landed(outcome, chord, asked, gone)?;
    match (start, now) {
        (Some(a), Some(b)) if (a.window, a.control) != (b.window, b.control) => {
            Err(EnterSkip::FocusChanged)
        }
        (Some(a), Some(b)) if a.keys != b.keys => Err(EnterSkip::Edited),
        (Some(_), Some(_)) => Ok(()),
        _ => Err(EnterSkip::FocusUnknown),
    }
}

/// Sends one Enter when [`enter_decision`] allows it, no modifier is down and the keys go out.
/// Call it once, after the paste of the final (merged) dictation has returned. `clip` is
/// [`enter_decision`]'s `[chord, asked, gone]`, all `None` for typed text.
pub fn enter_after(
    on: bool,
    outcome: PasteOutcome,
    start: Option<Focus>,
    clip: [Option<Instant>; 3],
    input: &mut impl Input,
) -> Result<(), EnterSkip> {
    // Asked only when the setting is on and the text landed: no OS call on the default path.
    let [chord, asked, gone] = clip;
    let now = if on && landed(outcome, chord, asked, gone).is_ok() {
        input.focus()
    } else {
        None
    };
    enter_decision(on, outcome, start, now, clip)?;
    if input.modifiers_held() {
        return Err(EnterSkip::ModifiersHeld);
    }
    input
        .send_chord(&[Key::Enter])
        .map_err(|_| EnterSkip::Blocked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paste::{ClipboardOnlyWhy, Foreground, InputBlocked};
    use crate::types::Millis;
    use std::time::Duration;

    /// Typed text: no clipboard timeline.
    const NO_CLIP: [Option<Instant>; 3] = [None; 3];

    const A: Focus = Focus {
        window: 1,
        control: 2,
        keys: 7,
    };
    const B: Focus = Focus {
        window: 1,
        control: 3,
        keys: 7,
    };

    /// No key sent (`assert_eq!` shows what was).
    const NOTHING: [Vec<Key>; 0] = [];

    #[derive(Default)]
    struct Kb {
        focus: Option<Focus>,
        held: bool,
        blocked: bool,
        looked: u32,
        sent: Vec<Vec<Key>>,
    }

    impl Input for Kb {
        fn modifiers_held(&self) -> bool {
            self.held
        }
        fn foreground(&self) -> Foreground {
            Foreground {
                app: String::new(),
                elevated: false,
            }
        }
        fn focus(&mut self) -> Option<Focus> {
            self.looked += 1;
            self.focus
        }
        fn send_chord(&mut self, keys: &[Key]) -> Result<(), InputBlocked> {
            if self.blocked {
                return Err(InputBlocked);
            }
            self.sent.push(keys.to_vec());
            Ok(())
        }
        fn send_units(&mut self, _: &[u16]) -> Result<(), InputBlocked> {
            Ok(())
        }
        fn sleep(&mut self, _: Millis) {}
    }

    fn kb(focus: Option<Focus>) -> Kb {
        Kb {
            focus,
            ..Kb::default()
        }
    }

    #[test]
    fn paste_enter_goes_once_after_typed_text_with_the_same_focus() {
        let mut k = kb(Some(A));
        assert_eq!(
            enter_after(true, PasteOutcome::Typed, Some(A), NO_CLIP, &mut k),
            Ok(())
        );
        assert_eq!(k.sent, [vec![Key::Enter]]);
    }

    #[test]
    fn paste_enter_is_off_by_default_and_asks_the_os_nothing() {
        let mut k = kb(Some(A));
        assert_eq!(
            enter_after(false, PasteOutcome::Typed, Some(A), NO_CLIP, &mut k),
            Err(EnterSkip::Off)
        );
        assert!(k.sent.is_empty() && k.looked == 0);
    }

    #[test]
    fn paste_enter_never_follows_a_text_that_did_not_land_whole() {
        for outcome in [
            PasteOutcome::Failed,
            PasteOutcome::FileFailed,
            PasteOutcome::ClipboardOnly(ClipboardOnlyWhy::ModifiersHeld),
            PasteOutcome::ClipboardOnly(ClipboardOnlyWhy::Elevated),
        ] {
            let mut k = kb(Some(A));
            assert_eq!(
                enter_after(true, outcome, Some(A), NO_CLIP, &mut k),
                Err(EnterSkip::NotLanded),
                "{outcome:?}"
            );
            assert!(k.sent.is_empty() && k.looked == 0, "{outcome:?}");
        }
    }

    /// Owner 2026-11-12: after Ctrl+V the Enter goes only when the foreground app asked for our
    /// delayed text after Ctrl+V and before the restore. A fake clipboard owner's timeline:
    /// set at `t0`, Ctrl+V at +5 ms, restore at +300 ms.
    #[test]
    fn paste_enter_after_a_clipboard_paste_needs_the_targets_own_read() {
        let t0 = Instant::now();
        let at = |ms| Some(t0 + Duration::from_millis(ms));
        let (chord, gone) = (at(5), at(300));
        for outcome in [PasteOutcome::Pasted, PasteOutcome::PastedNoRestore] {
            for (asked, want) in [
                // The target read it within the restore delay: proof.
                (at(40), Ok(())),
                // Read in the same instant Ctrl+V went out still counts.
                (chord, Ok(())),
                // No request from the target: it ignored Ctrl+V, or a monitor read first.
                (None, Err(EnterSkip::ClipNotTaken)),
                // Before Ctrl+V: the app read on its own, not because of the paste.
                (at(1), Err(EnterSkip::ClipTakenEarly)),
                // After the restore began: the app may have pasted the user's old copy.
                (gone, Err(EnterSkip::ClipTakenLate)),
                (at(900), Err(EnterSkip::ClipTakenLate)),
            ] {
                let mut k = kb(Some(A));
                let got = enter_after(true, outcome, Some(A), [chord, asked, gone], &mut k);
                assert_eq!(got, want, "{outcome:?} asked {asked:?}");
                assert_eq!(
                    k.sent.len(),
                    usize::from(want.is_ok()),
                    "{outcome:?} {asked:?}"
                );
                // The OS is asked for the focus only once the text is proven in.
                assert_eq!(k.looked, u32::from(want.is_ok()), "{outcome:?} {asked:?}");
            }
            // No restore ran (the user copied meanwhile): a read after Ctrl+V is proof.
            let mut k = kb(Some(A));
            let clip = [chord, at(40), None];
            assert_eq!(enter_after(true, outcome, Some(A), clip, &mut k), Ok(()));
            // Ctrl+V never went out: nothing to prove.
            let mut k = kb(Some(A));
            let clip = [None, at(40), gone];
            assert_eq!(
                enter_after(true, outcome, Some(A), clip, &mut k),
                Err(EnterSkip::ClipNotTaken)
            );
        }
        // Proof does not lift the focus checks.
        let mut k = kb(Some(B));
        let clip = [chord, at(40), gone];
        assert_eq!(
            enter_after(true, PasteOutcome::Pasted, Some(A), clip, &mut k),
            Err(EnterSkip::FocusChanged)
        );
        assert_eq!(k.sent, NOTHING);
    }

    #[test]
    fn paste_enter_skips_when_the_window_or_the_control_changed() {
        let other_window = Focus { window: 9, ..A };
        for now in [B, other_window] {
            let mut k = kb(Some(now));
            assert_eq!(
                enter_after(true, PasteOutcome::Typed, Some(A), NO_CLIP, &mut k),
                Err(EnterSkip::FocusChanged)
            );
            assert_eq!(k.sent, NOTHING);
        }
    }

    /// Review d08 #4: the person typed in the same field while the dictation ran ("ok "): the
    /// Enter would send their edit. Any own key-down since the record start counts.
    #[test]
    fn paste_enter_skips_when_the_person_typed_since_the_record_start() {
        let mut k = kb(Some(Focus { keys: 8, ..A }));
        assert_eq!(
            enter_after(true, PasteOutcome::Typed, Some(A), NO_CLIP, &mut k),
            Err(EnterSkip::Edited)
        );
        assert_eq!(k.sent, NOTHING);
        // A switch away and back is a changed focus first, whatever was typed.
        let now = Some(Focus { keys: 9, ..B });
        assert_eq!(
            enter_decision(true, PasteOutcome::Typed, Some(A), now, NO_CLIP),
            Err(EnterSkip::FocusChanged)
        );
    }

    #[test]
    fn paste_enter_fails_closed_when_the_focus_is_unknown() {
        for (start, now) in [(None, Some(A)), (Some(A), None), (None, None)] {
            let mut k = kb(now);
            assert_eq!(
                enter_after(true, PasteOutcome::Typed, start, NO_CLIP, &mut k),
                Err(EnterSkip::FocusUnknown)
            );
            assert_eq!(k.sent, NOTHING);
        }
    }

    #[test]
    fn paste_enter_waits_for_no_modifier_and_reports_blocked_input() {
        let mut k = Kb {
            held: true,
            ..kb(Some(A))
        };
        assert_eq!(
            enter_after(true, PasteOutcome::Typed, Some(A), NO_CLIP, &mut k),
            Err(EnterSkip::ModifiersHeld)
        );
        let mut k = Kb {
            blocked: true,
            ..kb(Some(A))
        };
        assert_eq!(
            enter_after(true, PasteOutcome::Typed, Some(A), NO_CLIP, &mut k),
            Err(EnterSkip::Blocked)
        );
        assert_eq!(k.sent, NOTHING);
    }
}
