//! The on-screen keyboard of the Hotkey page: a full ANSI board with the F-row, the navigation
//! cluster, the arrows and the numpad. Widths are in key units; `None` is a gap.

use oxi_core::hotkey::Key;

/// Width of the whole board in key units: main block 15, gap, nav 3, gap, numpad 4.
pub const WIDTH: f32 = 22.5;
/// Height in key units: the F-row, a quarter gap, five rows, a half gap, the F13-F24 row.
pub const HEIGHT: f32 = 7.75;

type Row = (f32, &'static [(Option<Key>, f32)]);

/// Each row: its top in key units, then its keys and gaps from the left edge.
#[rustfmt::skip]
const ROWS: [Row; 7] = {
    #[allow(clippy::enum_glob_use, reason = "the table reads as a keyboard only with bare names")]
    use Key::*;
    [
        (0.0, &[
            (Some(Esc), 1.0), (None, 1.0),
            (Some(F1), 1.0), (Some(F2), 1.0), (Some(F3), 1.0), (Some(F4), 1.0), (None, 0.5),
            (Some(F5), 1.0), (Some(F6), 1.0), (Some(F7), 1.0), (Some(F8), 1.0), (None, 0.5),
            (Some(F9), 1.0), (Some(F10), 1.0), (Some(F11), 1.0), (Some(F12), 1.0), (None, 0.25),
            (Some(PrintScreen), 1.0), (Some(ScrollLock), 1.0), (Some(Pause), 1.0),
        ]),
        (1.25, &[
            (Some(Backquote), 1.0), (Some(Digit1), 1.0), (Some(Digit2), 1.0), (Some(Digit3), 1.0),
            (Some(Digit4), 1.0), (Some(Digit5), 1.0), (Some(Digit6), 1.0), (Some(Digit7), 1.0),
            (Some(Digit8), 1.0), (Some(Digit9), 1.0), (Some(Digit0), 1.0), (Some(Minus), 1.0),
            (Some(Equal), 1.0), (Some(Backspace), 2.0), (None, 0.25),
            (Some(Insert), 1.0), (Some(Home), 1.0), (Some(PageUp), 1.0), (None, 0.25),
            (Some(NumLock), 1.0), (Some(NumDivide), 1.0), (Some(NumMultiply), 1.0),
            (Some(NumSubtract), 1.0),
        ]),
        (2.25, &[
            (Some(Tab), 1.5), (Some(Q), 1.0), (Some(W), 1.0), (Some(E), 1.0), (Some(R), 1.0),
            (Some(T), 1.0), (Some(Y), 1.0), (Some(U), 1.0), (Some(I), 1.0), (Some(O), 1.0),
            (Some(P), 1.0), (Some(BracketLeft), 1.0), (Some(BracketRight), 1.0),
            (Some(Backslash), 1.5), (None, 0.25),
            (Some(Delete), 1.0), (Some(End), 1.0), (Some(PageDown), 1.0), (None, 0.25),
            (Some(Num7), 1.0), (Some(Num8), 1.0), (Some(Num9), 1.0), (Some(NumAdd), 1.0),
        ]),
        (3.25, &[
            (Some(CapsLock), 1.75), (Some(A), 1.0), (Some(S), 1.0), (Some(D), 1.0), (Some(F), 1.0),
            (Some(G), 1.0), (Some(H), 1.0), (Some(J), 1.0), (Some(K), 1.0), (Some(L), 1.0),
            (Some(Semicolon), 1.0), (Some(Quote), 1.0), (Some(Enter), 2.25), (None, 3.5),
            (Some(Num4), 1.0), (Some(Num5), 1.0), (Some(Num6), 1.0),
        ]),
        (4.25, &[
            (Some(LShift), 2.25), (Some(Z), 1.0), (Some(X), 1.0), (Some(C), 1.0), (Some(V), 1.0),
            (Some(B), 1.0), (Some(N), 1.0), (Some(M), 1.0), (Some(Comma), 1.0),
            (Some(Period), 1.0), (Some(Slash), 1.0), (Some(RShift), 2.75), (None, 1.25),
            (Some(Up), 1.0), (None, 1.25),
            (Some(Num1), 1.0), (Some(Num2), 1.0), (Some(Num3), 1.0), (Some(NumEnter), 1.0),
        ]),
        (5.25, &[
            (Some(LCtrl), 1.25), (Some(LWin), 1.25), (Some(LAlt), 1.25), (Some(Space), 6.25),
            (Some(RAlt), 1.25), (Some(RWin), 1.25), (Some(Menu), 1.25), (Some(RCtrl), 1.25),
            (None, 0.25), (Some(Left), 1.0), (Some(Down), 1.0), (Some(Right), 1.0), (None, 0.25),
            (Some(Num0), 2.0), (Some(NumDecimal), 1.0),
        ]),
        // Not on most boards, but a chord may use them: under F1-F12.
        (6.75, &[
            (None, 2.0),
            (Some(F13), 1.0), (Some(F14), 1.0), (Some(F15), 1.0), (Some(F16), 1.0), (None, 0.5),
            (Some(F17), 1.0), (Some(F18), 1.0), (Some(F19), 1.0), (Some(F20), 1.0), (None, 0.5),
            (Some(F21), 1.0), (Some(F22), 1.0), (Some(F23), 1.0), (Some(F24), 1.0),
        ]),
    ]
};

/// One key on the board, in key units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cap {
    pub key: Key,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// The key unit, in px, from which `cap_label(key, true)` fits its cap on one line at 12 px. Spec
/// 5.1 says 36 for all; measured, the 35.8 of the default window fits all but the two wide words.
pub fn full_from(key: Key) -> f32 {
    if matches!(key, Key::Home | Key::Pause) {
        40.0
    } else {
        35.0
    }
}

/// The label of `key`: the full one when the cap has room, or else the short one `Key::label`
/// gives (Menu shorter still, so it stays on one line on the smallest board).
pub fn cap_label(key: Key, full: bool) -> &'static str {
    match key {
        Key::Menu if !full => "Mnu",
        Key::Backspace if full => "Backspace",
        Key::PrintScreen if full => "PrtSc",
        Key::ScrollLock if full => "ScrLk",
        Key::Pause if full => "Pause",
        Key::Home if full => "Home",
        Key::PageUp if full => "PgUp",
        Key::PageDown if full => "PgDn",
        Key::NumLock if full => "Num",
        Key::NumEnter if full => "Enter",
        _ => key.label(),
    }
}

/// Every key of the board, row by row, left to right.
pub fn caps() -> Vec<Cap> {
    let mut caps = Vec::new();
    for (y, row) in ROWS {
        let mut x = 0.0;
        for &(key, w) in row {
            if let Some(key) = key {
                // Numpad + and Enter span two rows.
                let h = if matches!(key, Key::NumAdd | Key::NumEnter) {
                    2.0
                } else {
                    1.0
                };
                caps.push(Cap { key, x, y, w, h });
            }
            x += w;
        }
    }
    caps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spoken_names_are_unique_on_the_board() {
        let names: Vec<String> = caps().iter().map(|c| c.key.spoken()).collect();
        for (i, n) in names.iter().enumerate() {
            assert!(!names[i + 1..].contains(n), "{n} twice");
        }
    }

    #[test]
    fn every_key_a_chord_can_hold_is_on_the_board_once() {
        let caps = caps();
        for key in Key::ALL {
            let n = caps.iter().filter(|c| c.key == key).count();
            assert_eq!(n, 1, "{key:?}");
        }
    }

    #[test]
    fn keys_fit_the_board_and_never_overlap() {
        let caps = caps();
        for (i, a) in caps.iter().enumerate() {
            assert!(a.x + a.w <= WIDTH && a.y + a.h <= HEIGHT, "{a:?}");
            for b in &caps[i + 1..] {
                let apart =
                    a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y;
                assert!(apart, "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn full_labels_replace_only_the_abbreviations() {
        for cap in caps() {
            let (full, short) = (cap_label(cap.key, true), cap_label(cap.key, false));
            assert!(
                full.chars().count() >= short.chars().count(),
                "{:?}",
                cap.key
            );
        }
        assert_eq!(cap_label(Key::Home, true), "Home");
        assert_eq!(cap_label(Key::Home, false), "Hm");
    }

    #[test]
    fn main_block_rows_are_fifteen_units() {
        for (_, row) in ROWS {
            let main: f32 = row
                .iter()
                .scan(0.0, |x, &(_, w)| {
                    *x += w;
                    Some(*x)
                })
                .find(|&x| x >= 15.0)
                .unwrap_or_default();
            assert!((main - 15.0).abs() < f32::EPSILON, "{row:?}");
        }
    }
}
