//! Paste: the outcome vocabulary, the clipboard and input traits each OS implements, and the
//! paste algorithm itself (ARCHITECTURE 10, decision D6).

use crate::config::{Paste, PasteMode, PasteRule, TypeSpeed};
use crate::hotkey::{Chord, Key};
use crate::types::Millis;

mod typing;
pub use typing::chunk_end;

#[cfg(test)]
mod tests;

/// How the paste worker delivered a job's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteOutcome {
    /// Clipboard set, paste chord sent, clipboard restored.
    Pasted,
    /// Typed key by key (type mode, or the clipboard could not be saved whole). Why is reported
    /// as [`PasteStep::Typed`].
    Typed,
    /// Left on the clipboard for the user to paste.
    ClipboardOnly(ClipboardOnlyWhy),
    /// Pasted, but the user's old clipboard was not put back: every restore try failed
    /// (item 52). The user is told.
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
/// Item 52: gap between looks at the OS history while it registers a clipboard write.
pub const HISTORY_POLL: Millis = Millis(100);
/// Item 52: looks at most, so a write the history never shows costs 0.9 s, not more.
pub const HISTORY_POLLS: u32 = 10;
/// Item 52: a call into the OS clipboard history that has not answered by then is given up, so
/// a stalled history service never hangs the paste worker.
pub const HISTORY_TIMEOUT: Millis = Millis(2000);
/// Item 52: one paste's history work (list, deletes, waiting for the new entry, not counting the
/// restore delay) stops once this much time has passed; what is left waits for the next paste.
pub const HISTORY_BUDGET: Millis = Millis(3000);
/// Item 52: Win+V holds this many entries and pushes the oldest out for a new one, so the
/// dictation enters it only while it holds fewer (owner 2026-11-10: no entry is ever pushed out).
pub const HISTORY_CAP: usize = 25;
/// Item 52: pastes that look for an unseen write of ours and find another entry in its place
/// before the mark is given up (that write most likely never registered).
const UNSEEN_MISSES: u8 = 3;
/// Item 52: tries to put the user's copy back while the target app or the history service
/// still holds the clipboard (each try already retries the open for 200 ms).
pub const RESTORE_TRIES: u32 = 5;
/// Item 52: the wait before the second try, doubled before each later one (50 to 400 ms).
pub const RESTORE_RETRY_GAP: Millis = Millis(50);
/// Type mode: gap between characters at the safe pace (games drop faster input).
pub const TYPE_GAP: Millis = Millis(5);

/// How typed text is paced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypePace {
    /// UTF-16 units handed to one [`Input::send_units`] call at most.
    pub units_per_call: usize,
    /// Wait before every call but the first.
    pub gap: Millis,
}

impl TypePace {
    /// 300 characters in about 40 ms (19 calls).
    pub const FAST: Self = Self {
        units_per_call: 16,
        gap: Millis(2),
    };
    /// One character per call, [`TYPE_GAP`] apart: the pace before chunking, for apps that drop
    /// fast synthetic input.
    pub const SAFE: Self = Self {
        units_per_call: 1,
        gap: TYPE_GAP,
    };

    fn of(speed: TypeSpeed) -> Self {
        match speed {
            TypeSpeed::Fast => Self::FAST,
            TypeSpeed::Safe => Self::SAFE,
        }
    }
}

/// What [`Clipboard::snapshot`] could save of the current clipboard.
#[derive(Debug, PartialEq, Eq)]
pub enum Snap<S> {
    /// Every format was saved.
    Complete(S),
    /// Some format was too big, delayed-render or unreadable: a restore would lose data, so
    /// the clipboard is left alone and the text typed. Why: [`Clipboard::incomplete_why`].
    Incomplete,
    /// Another app held the clipboard open through every retry.
    Locked,
}

/// Why a snapshot is not whole. Format ids, OS codes and sizes only, never clipboard content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncompleteWhy {
    /// `GetClipboardData` gave no data for this format.
    NoData { format: u32, code: u32 },
    /// The format's memory block could not be sized or locked.
    NoLock { format: u32, code: u32 },
    /// Listing the formats failed.
    EnumFailed { code: u32 },
    /// One format is bigger than the room left under [`SNAPSHOT_CAP`].
    TooBig { format: u32, bytes: u64, room: u64 },
    /// Windows marks the clipboard owner's window hung.
    HungOwner,
    /// Only handle formats (a lone metafile, say): a restore would put back nothing.
    OnlyHandles,
    /// The helper thread or its window could not be made.
    NoHelper,
    /// The owner did not render this delayed format in [`DELAYED_RENDER_TIMEOUT`].
    RenderTimeout { format: u32 },
    /// Our earlier helper still holds the clipboard open (it gave up on a slow owner).
    HelperBusy,
}

/// Why the text was typed instead of pasted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypedWhy {
    /// `paste.mode = "type"`.
    Mode,
    /// A rule for the foreground app says `mode = "type"`.
    Rule,
    /// The clipboard could not be saved whole; `None` when the clipboard does not say why.
    Incomplete(Option<IncompleteWhy>),
    /// Another app held the clipboard open.
    Locked,
    /// The user copied during the history work and that copy could not be saved whole.
    ChangedDuringHistory,
}

/// What the paste worker can say about one paste while it runs (no text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteStep {
    /// The text will be typed; sent before the first unit goes out.
    Typed(TypedWhy),
    /// `done` of `total` UTF-16 units typed so far.
    Typing { done: usize, total: usize },
    /// The text is in the field: right after the paste chord, or after the last typed unit.
    /// The clipboard restore and history work may still run.
    Landed,
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
    /// Why the last [`snapshot`](Self::snapshot) answered [`Snap::Incomplete`]; `None` when
    /// this clipboard does not say (the default).
    fn incomplete_why(&self) -> Option<IncompleteWhy> {
        None
    }
    /// Whether the saved data stays out of the OS clipboard history: its owner kept it out (a
    /// password manager's copy, say) or the history never takes it (files).
    /// The paste then never waits for it to show there.
    fn is_excluded(&self, snapshot: &Self::Snapshot) -> bool;
    /// Puts `text` on the clipboard, never uploaded to the cloud clipboard and in the OS
    /// clipboard history (Win+V) only if `in_history`; returns the new sequence number.
    fn set_text(&mut self, text: &str, in_history: bool) -> Result<u32, ClipError>;
    /// The current clipboard sequence number (`changeCount` on macOS).
    fn sequence(&self) -> u32;
    /// Puts saved data back, never uploaded to the cloud clipboard and in history only if
    /// `in_history`; returns the new sequence number. Borrowed, so a failed try can be retried.
    fn restore(&mut self, snapshot: &Self::Snapshot, in_history: bool) -> Result<u32, ClipError>;
    /// The OS clipboard history (Win+V), newest first. `None` when history is off, unreadable
    /// or the OS has none (the default).
    fn history(&mut self) -> Option<Vec<HistoryItem>> {
        None
    }
    /// The history entries' ids, newest first. `Ok(None)`: history off or the OS has none;
    /// `Err`: the call failed or timed out. A clipboard that can read ids without the texts
    /// makes this cheaper than [`Clipboard::history`].
    fn history_ids(&mut self) -> Result<Option<Vec<String>>, ClipError> {
        Ok(self
            .history()
            .map(|items| items.into_iter().map(|item| item.id).collect()))
    }
    /// The text of the history entry with this id; `None` when it is gone, has no text or there
    /// is no history. A clipboard that reads one entry makes this cheaper than the whole list.
    fn history_text(&mut self, id: &str) -> Option<String> {
        self.history()?.into_iter().find(|item| item.id == id)?.text
    }
    /// Removes the history entry with this id.
    fn delete_from_history(&mut self, _id: &str) -> Result<(), ClipError> {
        Ok(())
    }
    /// A monotonic clock for the overall limit on one paste's history work
    /// ([`HISTORY_BUDGET`]). The default never moves: each call keeps only its own limit.
    fn now(&self) -> Millis {
        Millis(0)
    }
    /// Caps the next history calls at `limit` (never above [`HISTORY_TIMEOUT`]): the time left of
    /// [`HISTORY_BUDGET`], so the overall limit is a deadline. The default keeps each call's own.
    fn limit_history_calls(&mut self, _limit: Millis) {}
}

/// One entry of the OS clipboard history.
#[derive(Clone, PartialEq, Eq)]
pub struct HistoryItem {
    /// The OS id, valid for [`Clipboard::delete_from_history`].
    pub id: String,
    /// The entry's text, `None` for a non-text entry.
    pub text: Option<String>,
}

/// The text is the user's clipboard: never printed.
impl std::fmt::Debug for HistoryItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoryItem")
            .field("id", &self.id)
            .field("text_len", &self.text.as_ref().map(String::len))
            .finish()
    }
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
    /// One input batch: key-down and key-up for each UTF-16 unit, in order. No sleep inside.
    /// The default is one [`type_units`](Self::type_units) call at [`TYPE_GAP`].
    // shortcut: `send_units` and `type_units` default to each other so inputs written against
    // the old per-character call still build; an input must override at least one. Upgrade:
    // make `send_units` required and drop `type_units` once no input implements only it.
    fn send_units(&mut self, units: &[u16]) -> Result<(), InputBlocked> {
        self.type_units(units, TYPE_GAP)
    }
    /// Types each UTF-16 unit as a Unicode key event pair, characters `gap` apart; the two
    /// units of a surrogate pair go together with no gap.
    fn type_units(&mut self, units: &[u16], gap: Millis) -> Result<(), InputBlocked> {
        let mut from = 0;
        while from < units.len() {
            if from > 0 {
                self.sleep(gap);
            }
            let end = chunk_end(units, from, 1);
            self.send_units(&units[from..end])?;
            from = end;
        }
        Ok(())
    }
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
    let rule = rule_for(&paste.rules, &fg.app);
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

/// The first rule in `rules` whose app matches `app` (case-insensitive).
fn rule_for<'a>(rules: &'a [PasteRule], app: &str) -> Option<&'a PasteRule> {
    let lower = |s: &str| s.chars().flat_map(char::to_lowercase).collect::<String>();
    let app = lower(app);
    rules.iter().find(|r| lower(&r.app) == app)
}

/// The pace typed text goes at in `fg`: the matching rule's `type_speed`; else safe when a rule
/// for the app forces typing (the user's, or the built-in one when the user's list holds no rule
/// for it), so every app typed into by rule keeps the old pace; else `paste.type_speed`.
pub fn type_pace(paste: &Paste, fg: &Foreground) -> TypePace {
    let rule = rule_for(&paste.rules, &fg.app);
    if let Some(speed) = rule.and_then(|r| r.type_speed) {
        return TypePace::of(speed);
    }
    let forced = match rule {
        Some(r) => r.mode == Some(PasteMode::Type),
        None => rule_for(&Paste::default().rules, &fg.app)
            .is_some_and(|r| r.mode == Some(PasteMode::Type)),
    };
    if forced {
        TypePace::SAFE
    } else {
        TypePace::of(paste.type_speed)
    }
}

/// Puts `text` on the clipboard and leaves it there (tray "Copy last text", item 42).
pub fn copy_only(text: &str, in_history: bool, clip: &mut impl Clipboard) -> Result<(), ClipError> {
    clip.set_text(text, in_history).map(drop)
}

/// [`paste_with`] for a caller that keeps no list of its history entries.
pub fn paste<C: Clipboard>(
    text: &str,
    paste: &Paste,
    clip: &mut C,
    input: &mut impl Input,
) -> PasteOutcome {
    paste_with(
        text,
        paste,
        clip,
        input,
        &mut OurEntries::default(),
        &mut |_| {},
    )
}

/// A write of ours the history did not show in time (item 52): a later paste finds it by its place
/// and text and deletes it, so it never stays behind. No text is kept, only its hash.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Unseen {
    /// The newest entry before our write: the history puts ours right above it.
    above: Option<String>,
    /// [`fnv`] of the dictation, so another entry in that place is never taken for ours.
    hash: u64,
    /// Pastes that looked in that place and found nothing of ours.
    misses: u8,
}

/// Our dictation entries still in Win+V, kept by the caller from paste to paste (item 52). The
/// next paste deletes them by id before it writes, so Win+V keeps only the latest dictation and
/// its count never rises above the one before the dictation. An entry whose delete failed or
/// timed out stays listed and is retried by the next paste.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OurEntries {
    ids: Vec<String>,
    unseen: Vec<Unseen>,
    /// The clipboard number our last paste left behind: another one at the next paste means
    /// someone copied since, and the OS may still be registering that copy.
    seq: Option<u32>,
    /// The newest history entry our last paste knew (`None`: unknown or an empty history).
    top: Option<String>,
}

impl OurEntries {
    fn is_empty(&self) -> bool {
        self.ids.is_empty() && self.unseen.is_empty()
    }

    /// What outlives the daemon, so the next session's first paste still deletes the last
    /// dictation of this one: one `key value` line each for the clipboard number, the newest
    /// history entry and every id of ours. Ids and a number only; the marks of unseen writes
    /// (a hash of the text) are not kept.
    // shortcut: a number of an earlier boot can equal the new session's by chance and hide a
    // copy still registering; upgrade by saving the boot time with it.
    #[must_use]
    pub fn to_text(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        if let Some(seq) = self.seq {
            let _ = writeln!(out, "seq {seq}");
        }
        let ids = self.top.iter().map(|id| ("top", id));
        for (key, id) in ids.chain(self.ids.iter().map(|id| ("id", id))) {
            if is_id(id) {
                let _ = writeln!(out, "{key} {id}");
            }
        }
        out
    }

    /// [`to_text`](Self::to_text) read back. A line it does not know is skipped, so a torn or
    /// edited file loses that line only.
    #[must_use]
    pub fn from_text(text: &str) -> Self {
        let mut ours = Self::default();
        for line in text.lines() {
            match line.trim_end().split_once(' ') {
                Some(("seq", n)) => ours.seq = n.parse().ok(),
                Some(("top", id)) if is_id(id) => ours.top = Some(id.to_owned()),
                Some(("id", id)) if is_id(id) && !ours.ids.iter().any(|x| x == id) => {
                    ours.ids.push(id.to_owned());
                }
                _ => {}
            }
        }
        ours
    }
}

impl OurEntries {
    /// Ids of our entries still in Win+V, oldest first (ids only, no text).
    #[must_use]
    pub fn ids(&self) -> &[String] {
        &self.ids
    }
}

/// An id fits on one `key value` line.
fn is_id(id: &str) -> bool {
    !id.is_empty() && !id.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// What the dictation's write may do with the history.
#[derive(Debug, PartialEq, Eq)]
enum Room {
    /// History off, or an OS without one: the setting alone decides.
    Off,
    /// Full, unreadable or out of time: the dictation stays out, so no entry of the user's is
    /// pushed out of the history.
    Closed,
    /// One more entry fits; the newest entry's id (`None`: the history is empty).
    Free(Option<String>),
}

impl Room {
    fn in_history(&self, keep: bool) -> bool {
        match self {
            Self::Off => keep,
            Self::Closed => false,
            Self::Free(_) => true,
        }
    }
}

/// FNV-1a: a stable hash of a dictation, so an [`Unseen`] mark carries no text.
fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Whether time is left for one more history call; that call is capped at the time left.
fn time_left(deadline: Millis, clip: &mut impl Clipboard) -> bool {
    let now = clip.now();
    if now >= deadline {
        return false;
    }
    clip.limit_history_calls((deadline - now).min(HISTORY_TIMEOUT));
    true
}

/// Deletes `id` while time is left and nobody wrote the clipboard since `seq` (a delete makes
/// the OS drop a write it is still registering); true when it went.
fn delete(
    id: &str,
    list: &mut Vec<String>,
    seq: u32,
    deadline: Millis,
    clip: &mut impl Clipboard,
) -> bool {
    if clip.sequence() != seq || !time_left(deadline, clip) || clip.delete_from_history(id).is_err()
    {
        return false;
    }
    list.retain(|x| x != id);
    true
}

/// Looks again (up to [`HISTORY_POLLS`] looks in all) until the newest entry is no longer `top`,
/// the one our last paste knew: a copy made since then has registered. True while it has not.
fn still_registering(
    top: &str,
    list: &mut Vec<String>,
    deadline: Millis,
    clip: &mut impl Clipboard,
    input: &mut impl Input,
) -> bool {
    for _ in 1..HISTORY_POLLS {
        // shortcut: a write of ours not seen in time that registers now also changes the top,
        // and a second copy still registering behind a first one is not seen. Upgrade: compare
        // entry timestamps with the copy's time.
        if list.first().map(String::as_str) != Some(top) {
            return false;
        }
        input.sleep(HISTORY_POLL);
        if !time_left(deadline, clip) {
            return true;
        }
        match clip.history_ids() {
            Ok(Some(new)) => *list = new,
            _ => return true,
        }
    }
    list.first().map(String::as_str) == Some(top)
}

/// The newest entry when it is an unseen write of ours that registered late: right above `top`,
/// where its mark put it, with the dictation's text. It moved the top as a copy would.
fn late_write(
    top: &str,
    list: &[String],
    unseen: &[Unseen],
    deadline: Millis,
    clip: &mut impl Clipboard,
) -> Option<String> {
    let [new, below, ..] = list else {
        return None;
    };
    let hash = unseen
        .iter()
        .find(|m| m.above.as_deref() == Some(top))?
        .hash;
    (below == top
        && time_left(deadline, clip)
        && clip.history_text(new).is_some_and(|t| fnv(&t) == hash))
    .then(|| new.clone())
}

/// Counts a look that found nothing of ours in the mark's place; the mark is kept until
/// [`UNSEEN_MISSES`] such looks.
fn missed(mut mark: Unseen, ours: &mut OurEntries) {
    mark.misses += 1;
    if mark.misses < UNSEEN_MISSES {
        ours.unseen.push(mark);
    }
}

/// Step 1 of the history order (owner 2026-11-10): our earlier entries go, by id, before the new
/// write, so the count never rises above the one before the dictation and no entry of the user's
/// is pushed out of the [`HISTORY_CAP`]. One list call, then one delete per entry of ours.
///
/// `seq` is the clipboard number before the snapshot, `excluded` whether the user's copy stays
/// out of the history. A copy made since our last paste may still be registering: a delete would
/// make the OS drop it, so the worker waits until it shows. One that never shows in time keeps
/// our entries for the next paste and a slot free for itself, so it pushes nothing out either.
fn clear_ours(
    ours: &mut OurEntries,
    seq: u32,
    excluded: bool,
    deadline: Millis,
    clip: &mut impl Clipboard,
    input: &mut impl Input,
) -> Room {
    let last_top = ours.top.take();
    if !time_left(deadline, clip) {
        return Room::Closed;
    }
    let mut list = match clip.history_ids() {
        Ok(Some(list)) => list,
        Ok(None) => return Room::Off,
        Err(_) => return Room::Closed,
    };
    let copied = ours.seq != Some(seq);
    let mut pending = copied
        && !excluded
        && last_top
            .as_deref()
            .is_none_or(|top| still_registering(top, &mut list, deadline, clip, input));
    // The top moved because our late write registered: the copy may still be registering above
    // it, so the worker waits for that too.
    if copied
        && !excluded
        && !pending
        && let Some(top) = last_top
        && let Some(late) = late_write(&top, &list, &ours.unseen, deadline, clip)
    {
        pending = still_registering(&late, &mut list, deadline, clip, input);
        if pending {
            // Nothing came above it: that entry may be the copy itself, with the dictation's text.
            // A doubt is never deleted, so the mark goes (our entry may stay behind).
            ours.unseen.retain(|mark| mark.above.as_ref() != Some(&top));
        }
    }
    // Places are read before any delete: the entry an unseen write sits above may be ours too.
    let all = list.clone();
    for mark in std::mem::take(&mut ours.unseen) {
        // A copy still registering: no delete now (it would drop that copy), the next paste looks.
        if pending {
            ours.unseen.push(mark);
            continue;
        }
        let at = match &mark.above {
            Some(above) => all.iter().position(|x| x == above),
            None => Some(all.len()),
        };
        let Some(i) = at.and_then(|i| i.checked_sub(1)) else {
            missed(mark, ours);
            continue;
        };
        // A copy made since our last paste, if the history took it, is the newest entry: one with
        // the dictation's text on top may be that copy (`late_write` could not tell), so it is
        // left alone and the mark dropped. Below the top it is our write, registered late.
        // shortcut: our late write on top under a copy that never shows in time leaks one entry,
        // and a same-text copy followed by another copy is taken for ours. Upgrade: entry
        // timestamps.
        if copied && !excluded && i == 0 {
            continue;
        }
        let id = all[i].clone();
        if !time_left(deadline, clip) {
            ours.unseen.push(mark);
            continue;
        }
        match clip.history_text(&id) {
            Some(text) if fnv(&text) == mark.hash => {
                if !delete(&id, &mut list, seq, deadline, clip) {
                    ours.ids.push(id);
                }
            }
            // shortcut: another entry in that place means ours most likely never registered; it
            // is given up after UNSEEN_MISSES pastes. Upgrade: match by the entry's timestamp.
            Some(_) => missed(mark, ours),
            // Failed, timed out or a non-text entry: looked for again next time.
            None => ours.unseen.push(mark),
        }
    }
    // Only the newest few marks are kept, so a broken history never grows the list.
    let extra = ours.unseen.len().saturating_sub(usize::from(UNSEEN_MISSES));
    ours.unseen.drain(..extra);
    for id in std::mem::take(&mut ours.ids) {
        // One no longer listed is gone already (the user deleted it, the history was cleared).
        if list.contains(&id) && (pending || !delete(&id, &mut list, seq, deadline, clip)) {
            ours.ids.push(id);
        }
    }
    ours.top = list.first().cloned();
    let cap = HISTORY_CAP - usize::from(pending);
    if clip.now() >= deadline || list.len() >= cap {
        Room::Closed
    } else {
        Room::Free(list.into_iter().next())
    }
}

/// With the setting off our earlier entries still go, and nothing else is read.
fn make_room(
    keep: bool,
    ours: &mut OurEntries,
    seq: u32,
    excluded: bool,
    deadline: Millis,
    clip: &mut impl Clipboard,
    input: &mut impl Input,
) -> Room {
    if keep {
        let room = clear_ours(ours, seq, excluded, deadline, clip, input);
        // The user copied while the history work ran: Win+V may still be registering that copy,
        // so the dictation stays out and nothing is pushed out.
        return if matches!(room, Room::Free(_)) && clip.sequence() != seq {
            Room::Closed
        } else {
            room
        };
    }
    if ours.is_empty() {
        ours.top = None;
    } else {
        clear_ours(ours, seq, excluded, deadline, clip, input);
    }
    Room::Off
}

/// Waits until the OS history shows the write numbered `seq` as a new newest entry above `base`
/// with the dictation's text (hash `hash`) and returns its id. A new entry with other text is a
/// copy of the user's that registered late: ours goes above it, so it becomes `base`. `None`: no
/// history, a failed call, someone else wrote meanwhile, out of time, or the write did not show
/// within [`HISTORY_POLLS`] looks.
fn wait_for_new_top(
    base: &mut Option<String>,
    hash: u64,
    seq: u32,
    deadline: Millis,
    clip: &mut impl Clipboard,
    input: &mut impl Input,
) -> Option<String> {
    for n in 0..HISTORY_POLLS {
        if n > 0 {
            input.sleep(HISTORY_POLL);
        }
        if !time_left(deadline, clip) {
            return None;
        }
        let list = clip.history_ids().ok()??;
        // Checked after the look: another app's write after ours bumps the number before its
        // entry shows. Its entry may already sit above ours, so ours can only be the one right
        // above `base`; nothing there with the dictation's text and the write counts as unseen.
        if clip.sequence() != seq {
            let at = base
                .as_ref()
                .map_or(Some(list.len()), |b| list.iter().position(|x| x == b));
            let id = list.get(at?.checked_sub(1)?)?;
            return (time_left(deadline, clip)
                && clip.history_text(id).is_some_and(|text| fnv(&text) == hash))
            .then(|| id.clone());
        }
        let top = list.into_iter().next();
        let Some(top) = top.filter(|top| Some(top) != base.as_ref()) else {
            continue;
        };
        if !time_left(deadline, clip) {
            return None;
        }
        // shortcut: a late copy of the user's with exactly the dictated text, registering before
        // ours, is taken for ours. Upgrade: compare entry timestamps with the write's time.
        match clip.history_text(&top) {
            Some(text) if fnv(&text) == hash => return Some(top),
            Some(_) => *base = Some(top),
            // Failed or a non-text entry (never ours): read again at the next look.
            None => {}
        }
    }
    None
}

/// Step 3: lists the dictation's entry for the next paste to delete. A write not seen in time
/// is listed by its place and text instead, so it is never left unrecorded.
fn record(
    text: &str,
    mut base: Option<String>,
    seq: u32,
    deadline: Millis,
    ours: &mut OurEntries,
    clip: &mut impl Clipboard,
    input: &mut impl Input,
) {
    let hash = fnv(text);
    if let Some(id) = wait_for_new_top(&mut base, hash, seq, deadline, clip, input) {
        ours.top = Some(id.clone());
        ours.ids.push(id);
    } else {
        ours.top.clone_from(&base);
        ours.unseen.push(Unseen {
            above: base,
            hash,
            misses: 0,
        });
    }
    // More than the history holds cannot all be in it.
    let extra = ours.ids.len().saturating_sub(HISTORY_CAP);
    ours.ids.drain(..extra);
}

/// How putting the user's copy back went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Restored {
    /// Put back; the clipboard number it left.
    Done(u32),
    /// Someone else wrote the clipboard after our dictation: theirs stays (item 39).
    Theirs,
    /// Every try failed: the user's copy is lost and the user is told.
    Failed,
}

/// Step 4: puts the saved copy back outside the history (Win+V still has its own entry of it;
/// a restore into history would add a twin and push an old entry out), retrying while the
/// clipboard is held (item 52): the target app or the history service often still has it open
/// right after the paste.
fn restore_retrying<C: Clipboard>(
    saved: &C::Snapshot,
    seq: u32,
    clip: &mut C,
    input: &mut impl Input,
) -> Restored {
    let mut gap = RESTORE_RETRY_GAP;
    // A locked try writes nothing, so a new number is someone else's; any other failure may
    // have emptied the clipboard itself, and from then on the number says nothing.
    let mut ours_unchanged = true;
    for n in 1..=RESTORE_TRIES {
        if ours_unchanged && clip.sequence() != seq {
            return Restored::Theirs;
        }
        match clip.restore(saved, false) {
            Ok(seq) => return Restored::Done(seq),
            Err(e) if n < RESTORE_TRIES => {
                ours_unchanged &= e == ClipError::Locked;
                input.sleep(gap);
                gap = gap + gap;
            }
            Err(_) => {}
        }
    }
    Restored::Failed
}

/// Delivers `text` to the foreground app: ARCHITECTURE section 10, steps 1-7 and type mode.
/// History order (item 52, owner 2026-11-10): our earlier entries are deleted by id, the
/// dictation is written into the history (when one more entry fits) and pasted, the worker waits
/// until the history shows it and lists it in `ours` (kept by the caller), and the user's copy
/// goes back outside the history. Ctrl+V gives the user's copy, Win+V shows this dictation on
/// top, and no entry of the user's is pushed out. A copy that cannot be kept is never
/// overwritten: the text is typed instead. `report` hears why the text is typed, its typing
/// progress and the moment it has landed (never for `Failed` or `ClipboardOnly`).
pub fn paste_with<C: Clipboard>(
    text: &str,
    paste: &Paste,
    clip: &mut C,
    input: &mut impl Input,
    ours: &mut OurEntries,
    report: &mut dyn FnMut(PasteStep),
) -> PasteOutcome {
    let keep = paste.keep_in_clipboard_history;
    let mut waited = Millis(0);
    while input.modifiers_held() {
        if waited >= MODIFIER_TIMEOUT {
            let why = ClipboardOnlyWhy::ModifiersHeld;
            return clipboard_only(text, keep, clip, input, ours, why);
        }
        input.sleep(MODIFIER_POLL);
        waited = waited + MODIFIER_POLL;
    }
    // Read after the wait: the text goes where focus is at paste time (item 41).
    let fg = input.foreground();
    if fg.elevated {
        return clipboard_only(text, keep, clip, input, ours, ClipboardOnlyWhy::Elevated);
    }
    let plan = plan(paste, &fg);
    let pace = type_pace(paste, &fg);
    if plan.mode == PasteMode::Type {
        let by_rule = rule_for(&paste.rules, &fg.app).is_some_and(|r| r.mode.is_some());
        let why = if by_rule {
            TypedWhy::Rule
        } else {
            TypedWhy::Mode
        };
        return type_text(text, why, pace, input, report);
    }
    // Read before the snapshot: a copy in between only makes the snapshot newer.
    let before = clip.sequence();
    let mut saved = match snapshot(clip) {
        Ok(saved) => saved,
        Err(why) => return type_text(text, why, pace, input, report),
    };
    let deadline = clip.now() + HISTORY_BUDGET + plan.restore_delay;
    let excluded = clip.is_excluded(&saved);
    let room = make_room(keep, ours, before, excluded, deadline, clip, input);
    if clip.sequence() != before {
        // The user copied while the history work ran: that copy is what goes back (make_room
        // already kept the dictation out of Win+V).
        let Ok(newer) = snapshot(clip) else {
            return type_text(text, TypedWhy::ChangedDuringHistory, pace, input, report);
        };
        saved = newer;
    }
    let Ok(seq) = clip.set_text(text, room.in_history(keep)) else {
        // The set may have emptied the clipboard: put the user's data back.
        let _ = clip.restore(&saved, false);
        return PasteOutcome::Failed;
    };
    let sent = input.send_chord(&plan.chord);
    if sent.is_ok() {
        report(PasteStep::Landed);
        input.sleep(plan.restore_delay);
    }
    // Before the restore: the history reads the clipboard late, and a restore before it saw
    // the dictation would lose the entry. Usually registered within the restore delay: one look.
    if let Room::Free(base) = room {
        record(text, base, seq, deadline, ours, clip, input);
    }
    let restored = restore_retrying(&saved, seq, clip, input);
    // Someone else's write, or a failed restore that may have written: the next paste looks
    // for a copy still registering.
    ours.seq = Some(match restored {
        Restored::Done(after) => after,
        Restored::Theirs | Restored::Failed => seq,
    });
    match (sent.is_ok(), restored) {
        (false, _) => PasteOutcome::Failed,
        // The text has landed, only the user's old clipboard is lost: say so.
        (true, Restored::Failed) => PasteOutcome::PastedNoRestore,
        (true, Restored::Done(_) | Restored::Theirs) => PasteOutcome::Pasted,
    }
}

/// Step 4: the whole clipboard, or why not when it is too big, has a hung or slow owner, or is
/// held open: a restore would lose the user's copy, so it is not overwritten at all (item 52,
/// owner 2026-11-10).
fn snapshot<C: Clipboard>(clip: &mut C) -> Result<C::Snapshot, TypedWhy> {
    match clip.snapshot() {
        Snap::Complete(saved) => Ok(saved),
        Snap::Incomplete => Err(TypedWhy::Incomplete(clip.incomplete_why())),
        Snap::Locked => Err(TypedWhy::Locked),
    }
}

fn clipboard_only(
    text: &str,
    keep: bool,
    clip: &mut impl Clipboard,
    input: &mut impl Input,
    ours: &mut OurEntries,
    why: ClipboardOnlyWhy,
) -> PasteOutcome {
    let before = clip.sequence();
    // Judged as in `paste_with`: a copy the history never takes costs no wait. Unreadable: wait.
    let excluded = snapshot(clip).is_ok_and(|saved| clip.is_excluded(&saved));
    let deadline = clip.now() + HISTORY_BUDGET;
    let room = make_room(keep, ours, before, excluded, deadline, clip, input);
    let Ok(seq) = clip.set_text(text, room.in_history(keep)) else {
        return PasteOutcome::Failed;
    };
    if let Room::Free(base) = room {
        record(text, base, seq, deadline, ours, clip, input);
    }
    ours.seq = Some(seq);
    PasteOutcome::ClipboardOnly(why)
}

/// Types `text` in chunks of [`chunk_end`], `pace.gap` apart, reporting progress after each.
fn type_text(
    text: &str,
    why: TypedWhy,
    pace: TypePace,
    input: &mut impl Input,
    report: &mut dyn FnMut(PasteStep),
) -> PasteOutcome {
    report(PasteStep::Typed(why));
    let units: Vec<u16> = text.encode_utf16().collect();
    let total = units.len();
    let mut from = 0;
    while from < total {
        if from > 0 {
            input.sleep(pace.gap);
        }
        let end = chunk_end(&units, from, pace.units_per_call);
        if input.send_units(&units[from..end]).is_err() {
            return PasteOutcome::Failed;
        }
        from = end;
        report(PasteStep::Typing { done: from, total });
    }
    report(PasteStep::Landed);
    PasteOutcome::Typed
}
