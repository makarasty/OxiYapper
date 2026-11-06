use super::chord::Chord;
use super::key::Key;
use crate::machine::Event;

/// One key event as the OS hook saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyInput {
    pub key: Key,
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
    /// `ChordDown` was sent and its `ChordUp` was not.
    active: bool,
    /// An Esc down was swallowed: its repeats and its up are swallowed too.
    esc_swallowed: bool,
    win: WinPress,
}

impl Tracker {
    pub fn new(chord: Chord) -> Self {
        Self {
            chord,
            capture: false,
            down: [false; Key::ALL.len()],
            active: false,
            esc_swallowed: false,
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

    pub fn on_key(&mut self, input: KeyInput, ctx: Ctx) -> Verdict {
        if input.ours {
            return Verdict::default();
        }
        if input.down {
            self.on_down(input.key, ctx)
        } else {
            self.on_up(input.key, ctx)
        }
    }

    fn on_down(&mut self, key: Key, ctx: Ctx) -> Verdict {
        if self.down[key as usize] {
            // Auto-repeat: never an event, but a swallowed key stays swallowed.
            let swallow = if self.capture {
                ctx.capture_swallow
            } else {
                key == Key::Esc && self.esc_swallowed
            };
            return Verdict {
                swallow,
                ..Verdict::default()
            };
        }
        if is_win(key) && !self.win_held() {
            self.win = WinPress::default();
        }
        self.down[key as usize] = true;
        if self.capture {
            return captured(key, true, ctx);
        }
        if key == Key::Esc && ctx.recording {
            self.esc_swallowed = true;
            return Verdict {
                event: Some(Event::Esc),
                swallow: true,
                ..Verdict::default()
            };
        }
        let mut event = None;
        if self.chord.contains(key) {
            if !self.active && self.chord.is_held(|k| self.down[k as usize]) {
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

    fn on_up(&mut self, key: Key, ctx: Ctx) -> Verdict {
        self.down[key as usize] = false;
        if key == Key::Esc && self.esc_swallowed {
            self.esc_swallowed = false;
            return Verdict {
                swallow: true,
                ..Verdict::default()
            };
        }
        if self.capture {
            return captured(key, false, ctx);
        }
        let mut verdict = Verdict::default();
        if self.active && self.chord.contains(key) && !self.chord.is_held(|k| self.down[k as usize])
        {
            self.active = false;
            verdict.event = Some(Event::ChordUp);
        }
        if is_win(key) && !self.win_held() {
            verdict.suppress_start = self.win.chord && !self.win.other;
            self.win = WinPress::default();
        }
        verdict
    }

    fn win_held(&self) -> bool {
        self.down[Key::LWin as usize] || self.down[Key::RWin as usize]
    }
}

fn captured(key: Key, down: bool, ctx: Ctx) -> Verdict {
    Verdict {
        swallow: ctx.capture_swallow,
        captured: Some((key, down)),
        ..Verdict::default()
    }
}

fn is_win(key: Key) -> bool {
    matches!(key, Key::LWin | Key::RWin)
}
