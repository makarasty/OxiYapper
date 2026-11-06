//! Hotkey core: key names, chord parsing, and the hook's verdict per key event.

use oxi_core::hotkey::{Chord, ChordError, Ctx, Key, KeyInput, Tracker, Verdict};
use oxi_core::machine::Event;
use proptest::prelude::*;

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

fn chord(list: &[&str]) -> Chord {
    Chord::parse(&names(list)).expect("valid chord")
}

const IDLE: Ctx = Ctx {
    recording: false,
    capture_swallow: false,
};
const REC: Ctx = Ctx {
    recording: true,
    capture_swallow: false,
};

fn dn(key: Key) -> KeyInput {
    KeyInput {
        key: Some(key),
        down: true,
        ours: false,
    }
}

fn up(key: Key) -> KeyInput {
    KeyInput {
        key: Some(key),
        down: false,
        ours: false,
    }
}

const PASS: Verdict = Verdict {
    event: None,
    swallow: false,
    suppress_start: false,
    captured: None,
};

fn ev(event: Event) -> Verdict {
    Verdict {
        event: Some(event),
        ..PASS
    }
}

/// Feeds `steps` to a fresh Ctrl+Win tracker and checks each verdict.
fn run(steps: &[(KeyInput, Ctx, Verdict)]) {
    let mut t = Tracker::new(chord(&["Ctrl", "Win"]));
    for (i, (input, ctx, want)) in steps.iter().enumerate() {
        assert_eq!(t.on_key(*input, *ctx), *want, "step {i}: {input:?}");
    }
}

// ---- keys -------------------------------------------------------------------------------------

#[test]
fn every_key_round_trips_by_name_and_serde() {
    for (i, k) in Key::ALL.into_iter().enumerate() {
        assert_eq!(k as usize, i);
        assert_eq!(Key::from_name(k.as_str()), Some(k), "{k:?}");
        assert!(!k.label().is_empty(), "{k:?}");
        let json = serde_json::to_string(&k).expect("serialize");
        assert_eq!(json, format!("\"{}\"", k.as_str()));
        assert_eq!(serde_json::from_str::<Key>(&json).expect("deserialize"), k);
    }
    let mut seen: Vec<&str> = Key::ALL.iter().map(|k| k.as_str()).collect();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), Key::ALL.len(), "names are unique");
    assert!(serde_json::from_str::<Key>("\"Hyper\"").is_err());
}

#[test]
fn full_ansi_keyboard_with_numpad_and_sided_modifiers() {
    // 104-key ANSI plus F13-F24.
    assert_eq!(Key::ALL.len(), 104 + 12);
    for name in [
        "LCtrl", "RCtrl", "LShift", "RShift", "LAlt", "RAlt", "LWin", "RWin", "Num0", "NumEnter",
        "F13", "Space",
    ] {
        assert!(Key::from_name(name).is_some(), "{name}");
    }
    assert_eq!(Key::from_name("lctrl"), Some(Key::LCtrl));
}

// ---- chord ------------------------------------------------------------------------------------

#[test]
fn chord_parse_names_and_send_keys() {
    let c = chord(&["win", "Ctrl"]);
    assert_eq!(c.to_names(), ["Ctrl", "Win"]);
    assert_eq!(c.keys_to_send(), [Key::LCtrl, Key::LWin]);
    assert_eq!(c, chord(&["Ctrl", "Win"]));

    let paste = chord(&["V", "Shift", "Ctrl"]);
    assert_eq!(paste.to_names(), ["Ctrl", "Shift", "V"]);
    assert_eq!(paste.keys_to_send(), [Key::LCtrl, Key::LShift, Key::V]);
    assert_eq!(
        chord(&["Shift", "Insert"]).keys_to_send(),
        [Key::LShift, Key::Insert]
    );
}

#[test]
fn chord_errors() {
    let cases: [(&[&str], ChordError); 6] = [
        (&[], ChordError::Empty),
        (&["Ctrl", "Hyper"], ChordError::Unknown("Hyper".into())),
        // One spelling per modifier in config: sided names are not chord names.
        (&["LCtrl", "Win"], ChordError::Unknown("LCtrl".into())),
        (&["Ctrl", "Esc"], ChordError::Esc),
        (&["Ctrl", "ctrl"], ChordError::Duplicate("ctrl".into())),
        (&["F13", "F13"], ChordError::Duplicate("F13".into())),
    ];
    for (input, want) in cases {
        assert_eq!(Chord::parse(&names(input)), Err(want), "{input:?}");
    }
}

#[test]
fn chord_from_picker_keys() {
    let c = Chord::from_keys(&[Key::RWin, Key::LCtrl, Key::RCtrl]).expect("chord");
    assert_eq!(c, chord(&["Ctrl", "Win"]));
    assert_eq!(Chord::from_keys(&[]), Err(ChordError::Empty));
    assert_eq!(
        Chord::from_keys(&[Key::LCtrl, Key::Esc]),
        Err(ChordError::Esc)
    );
    assert_eq!(
        Chord::from_keys(&[Key::F13, Key::LShift])
            .expect("chord")
            .to_names(),
        ["Shift", "F13"]
    );
}

#[test]
fn chord_is_held_on_either_side() {
    let c = chord(&["Ctrl", "Win"]);
    assert!(c.is_held(|k| matches!(k, Key::RCtrl | Key::LWin)));
    assert!(!c.is_held(|k| k == Key::LCtrl));
    assert!(!c.is_held(|k| matches!(k, Key::LCtrl | Key::LShift)));
}

// ---- tracker ----------------------------------------------------------------------------------

#[test]
fn ctrl_then_win_and_win_then_ctrl_give_chord_down() {
    run(&[
        (dn(Key::LCtrl), IDLE, PASS),
        (dn(Key::LWin), IDLE, ev(Event::ChordDown)),
        (
            up(Key::LWin),
            REC,
            Verdict {
                suppress_start: true,
                ..ev(Event::ChordUp)
            },
        ),
        (up(Key::LCtrl), IDLE, PASS),
        (dn(Key::RWin), IDLE, PASS),
        (dn(Key::RCtrl), IDLE, ev(Event::ChordDown)),
        (up(Key::RCtrl), REC, ev(Event::ChordUp)),
        // Chord was complete during this Win press: Start must stay closed.
        (
            up(Key::RWin),
            IDLE,
            Verdict {
                suppress_start: true,
                ..PASS
            },
        ),
    ]);
}

#[test]
fn repeat_gives_nothing() {
    run(&[
        (dn(Key::LCtrl), IDLE, PASS),
        (dn(Key::LWin), IDLE, ev(Event::ChordDown)),
        (dn(Key::LWin), REC, PASS),
        (dn(Key::LCtrl), REC, PASS),
        (dn(Key::LWin), REC, PASS),
    ]);
}

#[test]
fn other_side_of_a_held_modifier_is_no_chord_up() {
    run(&[
        (dn(Key::LCtrl), IDLE, PASS),
        (dn(Key::LWin), IDLE, ev(Event::ChordDown)),
        (dn(Key::RCtrl), REC, PASS),
        (up(Key::LCtrl), REC, PASS),
        (up(Key::RCtrl), REC, ev(Event::ChordUp)),
    ]);
}

#[test]
fn ctrl_win_d_gives_other_key_and_no_suppress() {
    run(&[
        (dn(Key::LCtrl), IDLE, PASS),
        (dn(Key::LWin), IDLE, ev(Event::ChordDown)),
        (dn(Key::D), REC, ev(Event::OtherKey)),
        (up(Key::D), IDLE, PASS),
        (up(Key::LWin), IDLE, ev(Event::ChordUp)),
        (up(Key::LCtrl), IDLE, PASS),
    ]);
}

#[test]
fn win_alone_gives_no_suppress() {
    run(&[
        (dn(Key::LWin), IDLE, PASS),
        (up(Key::LWin), IDLE, PASS),
        // A key before the Win press does not count against the next one.
        (dn(Key::D), IDLE, PASS),
        (up(Key::D), IDLE, PASS),
        (dn(Key::LWin), IDLE, PASS),
        (dn(Key::LCtrl), IDLE, ev(Event::ChordDown)),
        (up(Key::LCtrl), REC, ev(Event::ChordUp)),
        (
            up(Key::LWin),
            IDLE,
            Verdict {
                suppress_start: true,
                ..PASS
            },
        ),
    ]);
}

#[test]
fn win_outside_the_chord_is_never_suppressed() {
    let mut t = Tracker::new(chord(&["Ctrl", "F13"]));
    assert_eq!(t.on_key(dn(Key::LCtrl), IDLE), PASS);
    assert_eq!(t.on_key(dn(Key::F13), IDLE), ev(Event::ChordDown));
    assert_eq!(t.on_key(dn(Key::LWin), REC), ev(Event::OtherKey));
    assert_eq!(t.on_key(up(Key::LWin), IDLE), PASS);
}

#[test]
fn esc_while_recording_swallows_down_repeat_and_up() {
    let swallowed = Verdict {
        swallow: true,
        ..PASS
    };
    run(&[
        (dn(Key::LCtrl), IDLE, PASS),
        (dn(Key::LWin), IDLE, ev(Event::ChordDown)),
        (
            dn(Key::Esc),
            REC,
            Verdict {
                swallow: true,
                ..ev(Event::Esc)
            },
        ),
        (dn(Key::Esc), IDLE, swallowed),
        // The up matches a swallowed down, whatever the phase is now.
        (up(Key::Esc), IDLE, swallowed),
        (up(Key::LCtrl), IDLE, ev(Event::ChordUp)),
        // Esc was swallowed, so Windows saw a bare Ctrl+Win: Start still needs blocking.
        (
            up(Key::LWin),
            IDLE,
            Verdict {
                suppress_start: true,
                ..PASS
            },
        ),
    ]);
}

#[test]
fn esc_while_not_recording_passes() {
    run(&[
        (dn(Key::Esc), IDLE, PASS),
        (up(Key::Esc), REC, PASS),
        (dn(Key::LCtrl), IDLE, PASS),
        (dn(Key::LWin), IDLE, ev(Event::ChordDown)),
        // Chord held but not recording (say, no model): Esc is just another key.
        (dn(Key::Esc), IDLE, ev(Event::OtherKey)),
        (up(Key::Esc), IDLE, PASS),
    ]);
}

#[test]
fn ours_events_are_ignored() {
    let ours = |key, down| KeyInput {
        key: Some(key),
        down,
        ours: true,
    };
    run(&[
        (ours(Key::LCtrl, true), IDLE, PASS),
        (ours(Key::LWin, true), IDLE, PASS),
        (ours(Key::Esc, true), REC, PASS),
        (dn(Key::LCtrl), IDLE, PASS),
        (dn(Key::LWin), IDLE, ev(Event::ChordDown)),
        (ours(Key::V, true), REC, PASS),
        (ours(Key::V, false), REC, PASS),
        (ours(Key::LCtrl, false), REC, PASS),
        (up(Key::LCtrl), REC, ev(Event::ChordUp)),
    ]);
}

#[test]
fn capture_forwards_keys_and_emits_no_event() {
    let mut t = Tracker::new(chord(&["Ctrl", "Win"]));
    t.set_capture(true);
    let fg = Ctx {
        recording: true,
        capture_swallow: true,
    };
    let cap = |key, down, swallow| Verdict {
        swallow,
        captured: Some((key, down)),
        ..PASS
    };
    assert_eq!(t.on_key(dn(Key::LCtrl), fg), cap(Key::LCtrl, true, true));
    assert_eq!(t.on_key(dn(Key::LWin), fg), cap(Key::LWin, true, true));
    assert_eq!(
        t.on_key(dn(Key::LWin), fg),
        Verdict {
            swallow: true,
            ..PASS
        }
    );
    assert_eq!(t.on_key(dn(Key::Esc), fg), cap(Key::Esc, true, true));
    // An up is swallowed exactly when its own down was, whatever the focus is now.
    assert_eq!(t.on_key(up(Key::Esc), IDLE), cap(Key::Esc, false, true));
    assert_eq!(t.on_key(up(Key::LWin), IDLE), cap(Key::LWin, false, true));
    assert_eq!(t.on_key(dn(Key::LAlt), IDLE), cap(Key::LAlt, true, false));
    assert_eq!(t.on_key(up(Key::LAlt), fg), cap(Key::LAlt, false, false));
    assert_eq!(t.on_key(up(Key::LCtrl), fg), cap(Key::LCtrl, false, true));
    // A key outside `Key` is not captured and passes.
    assert_eq!(t.on_key(UNKNOWN_DN, fg), PASS);

    t.set_capture(false);
    assert_eq!(t.on_key(dn(Key::LWin), IDLE), PASS);
    assert_eq!(t.on_key(dn(Key::LCtrl), IDLE), ev(Event::ChordDown));
}

#[test]
fn set_chord_takes_the_new_keys() {
    let mut t = Tracker::new(chord(&["Ctrl", "Win"]));
    t.set_chord(chord(&["Shift", "F13"]));
    assert_eq!(t.chord().to_names(), ["Shift", "F13"]);
    assert_eq!(t.on_key(dn(Key::LCtrl), IDLE), PASS);
    assert_eq!(t.on_key(dn(Key::LWin), IDLE), PASS);
    assert_eq!(t.on_key(dn(Key::RShift), IDLE), PASS);
    assert_eq!(t.on_key(dn(Key::F13), IDLE), ev(Event::ChordDown));
}

const UNKNOWN_DN: KeyInput = KeyInput {
    key: None,
    down: true,
    ours: false,
};
const UNKNOWN_UP: KeyInput = KeyInput {
    key: None,
    down: false,
    ours: false,
};

#[test]
fn unknown_key_while_chord_held_is_other_key_and_passes() {
    run(&[
        (UNKNOWN_DN, IDLE, PASS),
        (UNKNOWN_UP, IDLE, PASS),
        (dn(Key::LCtrl), IDLE, PASS),
        (dn(Key::LWin), IDLE, ev(Event::ChordDown)),
        // Ctrl+Win+VolumeUp: a shortcut; Windows saw a key, so Start stays closed by itself.
        (UNKNOWN_DN, REC, ev(Event::OtherKey)),
        (UNKNOWN_UP, IDLE, PASS),
        (up(Key::LCtrl), IDLE, ev(Event::ChordUp)),
        (up(Key::LWin), IDLE, PASS),
    ]);
}

#[test]
fn resync_forgets_key_ups_lost_on_the_secure_desktop() {
    let mut t = Tracker::new(chord(&["Ctrl", "Win"]));
    assert_eq!(t.on_key(dn(Key::LCtrl), IDLE), PASS);
    assert_eq!(t.on_key(dn(Key::LWin), IDLE), ev(Event::ChordDown));
    // Win+L: both ups go to the secure desktop. Ctrl is still physically held.
    assert_eq!(t.resync(|k| k == Key::LCtrl), Some(Event::ChordUp));
    assert_eq!(t.resync(|k| k == Key::LCtrl), None);
    assert_eq!(t.resync(|_| false), None);
    // A stale Ctrl would make a plain Win tap a chord and keep Start closed.
    assert_eq!(t.on_key(dn(Key::LWin), IDLE), PASS);
    assert_eq!(t.on_key(up(Key::LWin), IDLE), PASS);
    // A stale Win would make a plain Ctrl press a chord.
    assert_eq!(t.on_key(dn(Key::LWin), IDLE), PASS);
    assert_eq!(t.resync(|_| false), None);
    assert_eq!(t.on_key(dn(Key::LCtrl), IDLE), PASS);
}

#[test]
fn resync_keeps_held_keys_and_a_held_chord() {
    let mut t = Tracker::new(chord(&["Ctrl", "Win"]));
    assert_eq!(t.on_key(dn(Key::LCtrl), IDLE), PASS);
    assert_eq!(t.on_key(dn(Key::LWin), IDLE), ev(Event::ChordDown));
    assert_eq!(t.resync(|k| matches!(k, Key::LCtrl | Key::LWin)), None);
    assert_eq!(t.on_key(dn(Key::LWin), REC), PASS, "still a repeat");
    assert_eq!(
        t.on_key(up(Key::LWin), REC),
        Verdict {
            suppress_start: true,
            ..ev(Event::ChordUp)
        }
    );
}

#[test]
fn resync_clears_a_swallowed_esc_whose_up_was_lost() {
    let mut t = Tracker::new(chord(&["Ctrl", "Win"]));
    assert!(t.on_key(dn(Key::Esc), REC).swallow);
    assert_eq!(t.resync(|_| false), None);
    // Not recording now: this Esc passes, so its up must pass too.
    assert_eq!(t.on_key(dn(Key::Esc), IDLE), PASS);
    assert_eq!(t.on_key(up(Key::Esc), IDLE), PASS);
}

// ---- properties -------------------------------------------------------------------------------

const POOL: [Key; 8] = [
    Key::LCtrl,
    Key::RCtrl,
    Key::LWin,
    Key::RWin,
    Key::LShift,
    Key::Esc,
    Key::D,
    Key::F13,
];

#[derive(Debug, Clone, Copy)]
enum Step {
    Key(KeyInput, Ctx),
    /// Watchdog: bit `i` set means `POOL[i]` is physically held.
    Resync(u8),
    Capture(bool),
}

fn held_in(mask: u8) -> impl Fn(Key) -> bool {
    move |k| {
        POOL.iter()
            .enumerate()
            .any(|(i, p)| *p == k && mask & (1 << i) != 0)
    }
}

/// `capture`: also turn capture on and off, with random focus.
fn step(capture: bool) -> impl Strategy<Value = Step> {
    let key = (
        // `POOL.len()` stands for a key outside `Key`.
        0..=POOL.len(),
        any::<bool>(),
        prop::bool::weighted(0.1),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(move |(i, down, ours, recording, swallow)| {
            Step::Key(
                KeyInput {
                    key: POOL.get(i).copied(),
                    down,
                    ours,
                },
                Ctx {
                    recording,
                    capture_swallow: capture && swallow,
                },
            )
        });
    prop_oneof![
        20 => key,
        1 => any::<u8>().prop_map(Step::Resync),
        u32::from(capture) => any::<bool>().prop_map(Step::Capture),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(5_000))]

    /// Chord keys always reach other apps, and the machine sees Down, Up, Down, Up...
    #[test]
    fn chord_keys_pass_and_down_up_alternate(steps in prop::collection::vec(step(false), 0..200)) {
        let mut t = Tracker::new(chord(&["Ctrl", "Win"]));
        let mut held = false;
        for step in steps {
            let v = match step {
                Step::Key(input, ctx) => {
                    let v = t.on_key(input, ctx);
                    let never_swallowed = matches!(
                        input.key,
                        Some(Key::LCtrl | Key::RCtrl | Key::LWin | Key::RWin) | None
                    );
                    prop_assert!(!(never_swallowed && v.swallow), "{input:?} swallowed");
                    v
                }
                Step::Resync(mask) => Verdict { event: t.resync(held_in(mask)), ..PASS },
                Step::Capture(_) => unreachable!("no capture in this property"),
            };
            match v.event {
                Some(Event::ChordDown) => { prop_assert!(!held); held = true; }
                Some(Event::ChordUp) => { prop_assert!(held); held = false; }
                Some(Event::OtherKey) => prop_assert!(held),
                _ => {}
            }
            prop_assert!(v.captured.is_none());
        }
    }

    /// An up is swallowed only when its own down was: no key is left stuck down in the OS.
    #[test]
    fn ups_are_swallowed_only_after_swallowed_downs(
        steps in prop::collection::vec(step(true), 0..200),
    ) {
        let mut t = Tracker::new(chord(&["Ctrl", "Win"]));
        let mut swallowed = [false; POOL.len()];
        let mut down = [false; POOL.len()];
        for step in steps {
            match step {
                Step::Key(input, ctx) => {
                    let v = t.on_key(input, ctx);
                    let Some(i) = input.key.and_then(|k| POOL.iter().position(|p| *p == k)) else {
                        prop_assert!(!v.swallow, "{input:?} swallowed");
                        continue;
                    };
                    if input.ours {
                        prop_assert!(!v.swallow);
                    } else if input.down {
                        if down[i] {
                            prop_assert_eq!(v.swallow, swallowed[i], "repeat of {:?}", input.key);
                        }
                        down[i] = true;
                        swallowed[i] = v.swallow;
                    } else {
                        prop_assert!(!v.swallow || swallowed[i], "{:?} up swallowed", input.key);
                        down[i] = false;
                        swallowed[i] = false;
                    }
                }
                Step::Resync(mask) => {
                    t.resync(held_in(mask));
                    for i in 0..POOL.len() {
                        if mask & (1 << i) == 0 {
                            down[i] = false;
                            swallowed[i] = false;
                        }
                    }
                }
                Step::Capture(on) => t.set_capture(on),
            }
        }
    }
}
