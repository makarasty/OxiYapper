use super::chord::Chord;
use super::key::Key;
use crate::machine::Event;

/// Which key an event is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyId {
    Known(Key),
    /// A key outside [`Key`] (ISO `<>`, media, volume, browser keys), by its OS code (VK on
    /// Windows). Never a chord key, never swallowed or captured, but another key all the same.
    Other(u16),
}

/// Other codes below this are tracked held, so their auto-repeat stays silent.
const OTHER_CODES: u16 = 1024;

/// One key event as the OS hook saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyInput {
    pub key: KeyId,
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
    /// Esc is swallowed and reported while recording; off, it is an ordinary key (R2.1).
    esc_cancels: bool,
    down: [bool; Key::ALL.len()],
    /// The key's down was swallowed: its repeats and its up are swallowed too, and only then.
    swallowed: [bool; Key::ALL.len()],
    /// Held [`KeyId::Other`] codes, one bit each.
    other_down: [u64; OTHER_CODES as usize / 64],
    /// `ChordDown` was sent and its `ChordUp` was not.
    active: bool,
    win: WinPress,
}

impl Tracker {
    pub fn new(chord: Chord) -> Self {
        Self {
            chord,
            capture: false,
            esc_cancels: false,
            down: [false; Key::ALL.len()],
            swallowed: [false; Key::ALL.len()],
            other_down: [0; OTHER_CODES as usize / 64],
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

    /// `hotkey.esc_cancels`: while it is off (the default) Esc passes through untouched.
    pub fn set_esc_cancels(&mut self, on: bool) {
        self.esc_cancels = on;
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
    pub fn resync(&mut self, held: impl Fn(KeyId) -> bool) -> Option<Event> {
        for key in Key::ALL {
            if !held(KeyId::Known(key)) {
                self.down[key as usize] = false;
                self.swallowed[key as usize] = false;
            }
        }
        for code in 0..OTHER_CODES {
            if self.other_held(code) && !held(KeyId::Other(code)) {
                self.set_other(code, false);
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
            KeyId::Other(code) => self.on_other(code, input.down),
            KeyId::Known(key) if input.down => self.on_down(key, ctx),
            KeyId::Known(key) => self.on_up(key),
        }
    }

    fn on_other(&mut self, code: u16, down: bool) -> Verdict {
        let repeat = down && self.other_held(code);
        self.set_other(code, down);
        if !down || repeat || self.capture {
            return Verdict::default();
        }
        self.win.other = true;
        Verdict {
            event: self.active.then_some(Event::OtherKey),
            ..Verdict::default()
        }
    }

    // shortcut: codes >= OTHER_CODES are not tracked, so their repeats count as fresh downs;
    // no current OS sends one (VK < 256, evdev KEY_MAX 0x2ff).
    fn other_held(&self, code: u16) -> bool {
        code < OTHER_CODES && self.other_down[usize::from(code / 64)] & (1 << (code % 64)) != 0
    }

    fn set_other(&mut self, code: u16, held: bool) {
        if code < OTHER_CODES {
            let (word, bit) = (
                &mut self.other_down[usize::from(code / 64)],
                1 << (code % 64),
            );
            if held {
                *word |= bit;
            } else {
                *word &= !bit;
            }
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
        if key == Key::Esc && ctx.recording && self.esc_cancels {
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
