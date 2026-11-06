use super::chord::Chord;
use super::key::Key;
use crate::machine::Event;

/// One key event as the OS hook saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyInput {
    /// `None`: a key outside [`Key`] (ISO `<>`, media, volume, browser keys). It is never a
    /// chord key and never swallowed, but it still counts as another key.
    pub key: Option<Key>,
    pub down: bool,
    /// Injected by `OxiYapper` itself (paste chord, Start blocker): passed, never tracked.
    pub ours: bool,
}

/// What the hook knows besides the key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Ctx {
    /// The phase is Recording (read from the `PHASE` atomic).
    pub recording: bool,
    /// Capture mode only: the settings window is foreground, so captured keys are swallowed.
    pub capture_swallow: bool,
}

/// What the hook must do with one key event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Verdict {
    /// Post this to the coordinator.
    pub event: Option<Event>,
    /// Do not pass the key on to other apps.
    pub swallow: bool,
    /// Inject the Start-menu blocker (VK 0xE8 down/up) before this Win key-up passes.
    pub suppress_start: bool,
    /// Capture mode: forward `(key, down)` to the settings window.
    pub captured: Option<(Key, bool)>,
}

/// What happened during the current Win press, for `suppress_start`.
#[derive(Debug, Clone, Copy, Default)]
struct WinPress {
    chord: bool,
    other: bool,
}

/// Turns raw key events into machine events and swallow decisions.
#[derive(Debug, Clone)]
pub struct Tracker {
    chord: Chord,
    capture: bool,
    down: [bool; Key::ALL.len()],
    /// The key's down was swallowed: its repeats and its up are swallowed too, and only then.
    swallowed: [bool; Key::ALL.len()],
    /// `ChordDown` was sent and its `ChordUp` was not.
    active: bool,
    win: WinPress,
}

impl Tracker {
    pub fn new(chord: Chord) -> Self {
        Self {
            chord,
            capture: false,
            down: [false; Key::ALL.len()],
            swallowed: [false; Key::ALL.len()],
            active: false,
            win: WinPress::default(),
        }
    }

    pub fn chord(&self) -> &Chord {
        &self.chord
    }

    /// Replaces the chord. A held old chord ends without `ChordUp`; the watchdog sees it.
    pub fn set_chord(&mut self, chord: Chord) {
        self.chord = chord;
        self.active = false;
    }

    /// Hotkey capture on or off. Turning it on drops a held chord without `ChordUp`.
    pub fn set_capture(&mut self, on: bool) {
        self.capture = on;
        self.active = false;
    }

    /// Forgets every key that `held` (the physical key state) says is up: its key-up was lost
    /// on the secure desktop (Win+L, Ctrl+Alt+Del, UAC). The hook calls it on every watchdog
    /// poll and on session unlock and resume (ARCHITECTURE section 11 items 8 and 12).
    /// Returns `ChordUp` if the chord was down and no longer is; nothing else.
    pub fn resync(&mut self, held: impl Fn(Key) -> bool) -> Option<Event> {
        for key in Key::ALL {
            if !held(key) {
                self.down[key as usize] = false;
                self.swallowed[key as usize] = false;
            }
        }
        if !self.win_held() {
            self.win = WinPress::default();
        }
        if self.active && !self.chord_held() {
            self.active = false;
            return Some(Event::ChordUp);
        }
        None
    }

    pub fn on_key(&mut self, input: KeyInput, ctx: Ctx) -> Verdict {
        match input.key {
            _ if input.ours => Verdict::default(),
            None => self.on_unknown(input.down),
            Some(key) if input.down => self.on_down(key, ctx),
            Some(key) => self.on_up(key),
        }
    }

    fn on_unknown(&mut self, down: bool) -> Verdict {
        if !down || self.capture {
            return Verdict::default();
        }
        self.win.other = true;
        Verdict {
            // Repeats are not tracked here: the machine ignores a second OtherKey.
            event: self.active.then_some(Event::OtherKey),
            ..Verdict::default()
        }
    }

    fn on_down(&mut self, key: Key, ctx: Ctx) -> Verdict {
        let i = key as usize;
        if self.down[i] {
            // Auto-repeat: never an event; swallowed exactly when its first down was.
            return Verdict {
                swallow: self.swallowed[i],
                ..Verdict::default()
            };
        }
        if is_win(key) && !self.win_held() {
            self.win = WinPress::default();
        }
        self.down[i] = true;
        let verdict = self.decide_down(key, ctx);
        self.swallowed[i] = verdict.swallow;
        verdict
    }

    fn decide_down(&mut self, key: Key, ctx: Ctx) -> Verdict {
        if self.capture {
            return Verdict {
                swallow: ctx.capture_swallow,
                captured: Some((key, true)),
                ..Verdict::default()
            };
        }
        if key == Key::Esc && ctx.recording {
            return Verdict {
                event: Some(Event::Esc),
                swallow: true,
                ..Verdict::default()
            };
        }
        let mut event = None;
        if self.chord.contains(key) {
            if !self.active && self.chord_held() {
                self.active = true;
                self.win.chord = true;
                event = Some(Event::ChordDown);
            }
        } else {
            // Any key Windows sees during a Win press keeps Start closed by itself.
            self.win.other = true;
            if self.active {
                event = Some(Event::OtherKey);
            }
        }
        Verdict {
            event,
            ..Verdict::default()
        }
    }

    fn on_up(&mut self, key: Key) -> Verdict {
        let i = key as usize;
        self.down[i] = false;
        let swallow = std::mem::take(&mut self.swallowed[i]);
        if self.capture {
            return Verdict {
                swallow,
                captured: Some((key, false)),
                ..Verdict::default()
            };
        }
        let mut verdict = Verdict {
            swallow,
            ..Verdict::default()
        };
        if self.active && self.chord.contains(key) && !self.chord_held() {
            self.active = false;
            verdict.event = Some(Event::ChordUp);
        }
        if is_win(key) && !self.win_held() {
            verdict.suppress_start = self.win.chord && !self.win.other;
            self.win = WinPress::default();
        }
        verdict
    }

    fn chord_held(&self) -> bool {
        self.chord.is_held(|k| self.down[k as usize])
    }

    fn win_held(&self) -> bool {
        self.down[Key::LWin as usize] || self.down[Key::RWin as usize]
    }
}

fn is_win(key: Key) -> bool {
    matches!(key, Key::LWin | Key::RWin)
}
