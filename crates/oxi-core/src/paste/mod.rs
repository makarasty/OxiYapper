//! Paste: the outcome vocabulary, the clipboard and input traits each OS implements, and the
//! paste algorithm itself (ARCHITECTURE 10, decision D6).

use crate::config::{Paste, PasteMode};
use crate::hotkey::{Chord, Key};
use crate::types::Millis;

/// How the paste worker delivered a job's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteOutcome {
    /// Clipboard set, paste chord sent, clipboard restored.
    Pasted,
    /// Typed key by key (type mode, or the clipboard was locked).
    Typed,
    /// Left on the clipboard for the user to paste.
    ClipboardOnly(ClipboardOnlyWhy),
    /// Pasted, but the clipboard snapshot was incomplete, so the old one was not restored.
    PastedNoRestore,
    /// Nothing landed: `SendInput` inserted no events, a secure desktop, or setting the
    /// clipboard failed. The coordinator keeps the text for "Copy last text" (item 42).
    Failed,
}

/// Why the paste worker did not send the paste chord itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardOnlyWhy {
    /// Modifiers still held after 1.5 s (item 35).
    ModifiersHeld,
    /// The target window runs elevated (item 36).
    Elevated,
}

/// Step 2: how often the physical modifier state is polled.
pub const MODIFIER_POLL: Millis = Millis(5);
/// Step 2: modifiers still down after this: clipboard-only (item 35).
pub const MODIFIER_TIMEOUT: Millis = Millis(1500);
/// Step 4: tries to open the clipboard before it counts as locked.
pub const OPEN_RETRIES: u32 = 10;
/// Step 4: gap between those tries.
pub const OPEN_RETRY_GAP: Millis = Millis(20);
/// Step 4: a snapshot copies at most this many bytes of clipboard data.
pub const SNAPSHOT_CAP: usize = 32 << 20;
/// Step 4: a delayed-render format its owner does not render in this time is skipped.
pub const DELAYED_RENDER_TIMEOUT: Millis = Millis(500);
/// Step 4: an incomplete snapshot types text up to this many characters, else pastes unrestored.
pub const TYPE_FALLBACK_MAX_CHARS: usize = 300;
/// Type mode: gap between characters (games drop faster input).
pub const TYPE_GAP: Millis = Millis(5);

/// What [`Clipboard::snapshot`] could save of the current clipboard.
#[derive(Debug, PartialEq, Eq)]
pub enum Snap<S> {
    /// Every format was saved.
    Complete(S),
    /// Some format was too big, delayed-render or unreadable: a restore would lose data.
    Incomplete,
    /// Another app held the clipboard open through every retry.
    Locked,
}

/// A clipboard call failed. Carries the call and OS code, never clipboard text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClipError {
    #[error("the clipboard is held open by another app")]
    Locked,
    #[error("clipboard call {call} failed with OS error {code}")]
    Os { call: &'static str, code: u32 },
}

/// `SendInput` (or its OS equivalent) inserted no events: secure desktop or a blocked target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the OS inserted none of the synthetic input events")]
pub struct InputBlocked;

/// The system clipboard, implemented per OS in `oxi-platform`.
pub trait Clipboard {
    /// Saved clipboard contents, handed back to [`Clipboard::restore`].
    type Snapshot;
    fn snapshot(&mut self) -> Snap<Self::Snapshot>;
    /// Puts `text` on the clipboard, excluded from history and cloud sync; returns the new
    /// sequence number.
    fn set_text(&mut self, text: &str) -> Result<u32, ClipError>;
    /// The current clipboard sequence number (`changeCount` on macOS).
    fn sequence(&self) -> u32;
    fn restore(&mut self, snapshot: Self::Snapshot) -> Result<(), ClipError>;
}

/// The window that gets the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Foreground {
    /// Exe name on Windows, bundle id on macOS, `WM_CLASS` on X11; matched by [`PasteRule::app`](crate::config::PasteRule).
    pub app: String,
    /// Runs at a higher integrity level than we do (UIPI drops our input, item 36).
    pub elevated: bool,
}

/// Synthetic input and the facts about the foreground window, implemented per OS.
pub trait Input {
    /// Whether any of Ctrl, Shift, Alt, Win (either side) is physically down.
    fn modifiers_held(&self) -> bool;
    fn foreground(&self) -> Foreground;
    /// Presses `keys` in order and releases them in reverse.
    fn send_chord(&mut self, keys: &[Key]) -> Result<(), InputBlocked>;
    /// Types each UTF-16 unit as a Unicode key event pair, characters `gap` apart; the two
    /// units of a surrogate pair go together with no gap.
    fn type_units(&mut self, units: &[u16], gap: Millis) -> Result<(), InputBlocked>;
    fn sleep(&mut self, time: Millis);
}

/// How one paste is delivered to the foreground app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub mode: PasteMode,
    /// Keys of the paste chord, pressed in this order.
    pub chord: Vec<Key>,
    pub restore_delay: Millis,
}

/// Step 1: the first rule whose app matches (case-insensitive) overrides each field it sets.
/// A rule chord that does not parse falls back to Ctrl+V.
pub fn plan(paste: &Paste, fg: &Foreground) -> Plan {
    let lower = |s: &str| s.chars().flat_map(char::to_lowercase).collect::<String>();
    let app = lower(&fg.app);
    let rule = paste.rules.iter().find(|r| lower(&r.app) == app);
    Plan {
        mode: rule.and_then(|r| r.mode).unwrap_or(paste.mode),
        chord: rule
            .and_then(|r| r.chord.as_deref())
            .and_then(|names| Chord::parse(names).ok())
            .map_or_else(|| vec![Key::LCtrl, Key::V], |c| c.keys_to_send()),
        restore_delay: Millis(u64::from(
            rule.and_then(|r| r.restore_delay_ms)
                .unwrap_or(paste.restore_delay_ms),
        )),
    }
}

/// Puts `text` on the clipboard and leaves it there (tray "Copy last text", item 42).
pub fn copy_only(text: &str, clip: &mut impl Clipboard) -> Result<(), ClipError> {
    clip.set_text(text).map(drop)
}

/// Delivers `text` to the foreground app: ARCHITECTURE section 10, steps 1-7 and type mode.
pub fn paste<C: Clipboard>(
    text: &str,
    paste: &Paste,
    clip: &mut C,
    input: &mut impl Input,
) -> PasteOutcome {
    let mut waited = Millis(0);
    while input.modifiers_held() {
        if waited >= MODIFIER_TIMEOUT {
            return clipboard_only(text, clip, ClipboardOnlyWhy::ModifiersHeld);
        }
        input.sleep(MODIFIER_POLL);
        waited = waited + MODIFIER_POLL;
    }
    // Read after the wait: the text goes where focus is at paste time (item 41).
    let fg = input.foreground();
    if fg.elevated {
        return clipboard_only(text, clip, ClipboardOnlyWhy::Elevated);
    }
    let plan = plan(paste, &fg);
    if plan.mode == PasteMode::Type {
        return type_text(text, input);
    }
    match clip.snapshot() {
        Snap::Locked => type_text(text, input),
        Snap::Incomplete if text.chars().count() <= TYPE_FALLBACK_MAX_CHARS => {
            type_text(text, input)
        }
        Snap::Incomplete => match clip.set_text(text) {
            Ok(_) if input.send_chord(&plan.chord).is_ok() => PasteOutcome::PastedNoRestore,
            _ => PasteOutcome::Failed,
        },
        Snap::Complete(saved) => {
            let Ok(seq) = clip.set_text(text) else {
                // The set may have emptied the clipboard: put the user's data back.
                let _ = clip.restore(saved);
                return PasteOutcome::Failed;
            };
            let sent = input.send_chord(&plan.chord);
            if sent.is_ok() {
                input.sleep(plan.restore_delay);
            }
            // Someone else wrote the clipboard meanwhile: leave theirs (item 39).
            if clip.sequence() == seq {
                // A failed restore loses only the user's old clipboard; the text has landed.
                let _ = clip.restore(saved);
            }
            if sent.is_ok() {
                PasteOutcome::Pasted
            } else {
                PasteOutcome::Failed
            }
        }
    }
}

fn clipboard_only(text: &str, clip: &mut impl Clipboard, why: ClipboardOnlyWhy) -> PasteOutcome {
    match copy_only(text, clip) {
        Ok(()) => PasteOutcome::ClipboardOnly(why),
        Err(_) => PasteOutcome::Failed,
    }
}

fn type_text(text: &str, input: &mut impl Input) -> PasteOutcome {
    let units: Vec<u16> = text.encode_utf16().collect();
    match input.type_units(&units, TYPE_GAP) {
        Ok(()) => PasteOutcome::Typed,
        Err(InputBlocked) => PasteOutcome::Failed,
    }
}
