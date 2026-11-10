//! The hook's verdicts for the keys around a held chord (R2.1): a key always reaches the system.

use super::{Chord, Ctx, Key, KeyId, KeyInput, Tracker, Verdict};
use crate::machine::Event;

const REC: Ctx = Ctx {
    recording: true,
    capture_swallow: false,
};

fn tracker() -> Tracker {
    Tracker::new(Chord::from_keys(&[Key::LCtrl, Key::LWin]).unwrap())
}

fn dn(key: Key) -> KeyInput {
    KeyInput {
        key: KeyId::Known(key),
        down: true,
        ours: false,
    }
}

fn up(key: Key) -> KeyInput {
    KeyInput {
        down: false,
        ..dn(key)
    }
}

fn event(t: &mut Tracker, input: KeyInput) -> Option<Event> {
    let v = t.on_key(input, REC);
    assert!(!v.swallow, "{input:?} must reach the system");
    v.event
}

fn start(t: &mut Tracker) {
    assert_eq!(event(t, dn(Key::LCtrl)), None);
    assert_eq!(event(t, dn(Key::LWin)), Some(Event::ChordDown));
}

#[test]
fn a_shortcut_key_is_reported_and_always_passes() {
    let mut t = tracker();
    start(&mut t);
    assert_eq!(event(&mut t, dn(Key::D)), Some(Event::OtherKey));
    assert_eq!(event(&mut t, up(Key::D)), None);
}

#[test]
fn esc_is_an_ordinary_key_unless_esc_cancels_is_on() {
    let mut t = tracker();
    start(&mut t);
    // Off (the default): reported as another key and not swallowed, its up passes too.
    assert_eq!(event(&mut t, dn(Key::Esc)), Some(Event::OtherKey));
    assert_eq!(event(&mut t, up(Key::Esc)), None);
    t.set_esc_cancels(true);
    let v = t.on_key(dn(Key::Esc), REC);
    assert_eq!((v.event, v.swallow), (Some(Event::Esc), true));
    assert!(t.on_key(up(Key::Esc), REC).swallow);
}

#[test]
fn alt_tab_while_held_is_two_other_keys_and_the_chord_goes_on() {
    let mut t = tracker();
    start(&mut t);
    assert_eq!(event(&mut t, dn(Key::LAlt)), Some(Event::OtherKey));
    assert_eq!(event(&mut t, dn(Key::Tab)), Some(Event::OtherKey));
    // Tab auto-repeats while Alt is held: no new events.
    assert_eq!(event(&mut t, dn(Key::Tab)), None);
    assert_eq!(event(&mut t, up(Key::Tab)), None);
    assert_eq!(event(&mut t, up(Key::LAlt)), None);
    assert_eq!(event(&mut t, up(Key::LCtrl)), Some(Event::ChordUp));
}

#[test]
fn a_layout_switch_while_held_keeps_the_chord() {
    // Alt+Shift changes the layout: key codes do not depend on it, so the chord stays held and
    // its release is still delivered.
    let mut t = tracker();
    start(&mut t);
    assert_eq!(event(&mut t, dn(Key::LAlt)), Some(Event::OtherKey));
    assert_eq!(event(&mut t, dn(Key::LShift)), Some(Event::OtherKey));
    assert_eq!(event(&mut t, up(Key::LShift)), None);
    assert_eq!(event(&mut t, up(Key::LAlt)), None);
    assert_eq!(event(&mut t, up(Key::LWin)), Some(Event::ChordUp));
}

#[test]
fn the_win_key_alone_is_not_ours_and_opens_start() {
    let mut t = tracker();
    assert_eq!(t.on_key(dn(Key::LWin), Ctx::default()), Verdict::default());
    // Not a chord press: no Start blocker, Windows opens the menu as usual.
    assert_eq!(t.on_key(up(Key::LWin), Ctx::default()), Verdict::default());
}

#[test]
fn chord_released_in_either_order_ends_it_once_and_blocks_start_once() {
    for (first, second) in [(Key::LCtrl, Key::LWin), (Key::LWin, Key::LCtrl)] {
        let mut t = tracker();
        start(&mut t);
        let v = t.on_key(up(first), REC);
        assert_eq!(v.event, Some(Event::ChordUp), "{first:?}");
        let v2 = t.on_key(up(second), REC);
        assert_eq!(v2.event, None, "{first:?}");
        // Whichever key went last, Windows saw a bare Win press once.
        let blocks = [&v, &v2].iter().filter(|v| v.suppress_start).count();
        assert_eq!(blocks, 1, "{first:?}");
    }
}

#[test]
fn a_shortcut_with_win_keeps_start_closed_by_itself() {
    let mut t = tracker();
    start(&mut t);
    assert_eq!(event(&mut t, dn(Key::D)), Some(Event::OtherKey));
    assert_eq!(event(&mut t, up(Key::D)), None);
    let v = t.on_key(up(Key::LWin), REC);
    assert!(!v.suppress_start, "Windows saw Win+D: Start stays closed");
}

#[test]
fn chord_repeat_is_silent() {
    let mut t = tracker();
    start(&mut t);
    for _ in 0..5 {
        assert_eq!(event(&mut t, dn(Key::LWin)), None);
        assert_eq!(event(&mut t, dn(Key::LCtrl)), None);
    }
}

#[test]
fn a_chord_press_after_a_lost_key_up_starts_over_after_resync() {
    // Lock screen (Win+L) or a session switch eats the key-ups; unlock resyncs.
    let mut t = tracker();
    start(&mut t);
    assert_eq!(t.resync(|_| false), Some(Event::ChordUp));
    start(&mut t);
}
