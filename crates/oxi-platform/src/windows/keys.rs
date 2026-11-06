//! Virtual-key codes for [`Key`], physical key state and key injection
//! (ARCHITECTURE section 2, hook thread).

use std::io;

use oxi_core::hotkey::{Chord, Key, KeyId};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, SendInput, VK_0, VK_1, VK_2, VK_3,
    VK_4, VK_5, VK_6, VK_7, VK_8, VK_9, VK_A, VK_ADD, VK_APPS, VK_B, VK_BACK, VK_C, VK_CAPITAL,
    VK_D, VK_DECIMAL, VK_DELETE, VK_DIVIDE, VK_DOWN, VK_E, VK_END, VK_ESCAPE, VK_F, VK_F1, VK_F2,
    VK_F3, VK_F4, VK_F5, VK_F6, VK_F7, VK_F8, VK_F9, VK_F10, VK_F11, VK_F12, VK_F13, VK_F14,
    VK_F15, VK_F16, VK_F17, VK_F18, VK_F19, VK_F20, VK_F21, VK_F22, VK_F23, VK_F24, VK_G, VK_H,
    VK_HOME, VK_I, VK_INSERT, VK_J, VK_K, VK_L, VK_LCONTROL, VK_LEFT, VK_LMENU, VK_LSHIFT, VK_LWIN,
    VK_M, VK_MULTIPLY, VK_N, VK_NEXT, VK_NUMLOCK, VK_NUMPAD0, VK_NUMPAD1, VK_NUMPAD2, VK_NUMPAD3,
    VK_NUMPAD4, VK_NUMPAD5, VK_NUMPAD6, VK_NUMPAD7, VK_NUMPAD8, VK_NUMPAD9, VK_O, VK_OEM_1,
    VK_OEM_2, VK_OEM_3, VK_OEM_4, VK_OEM_5, VK_OEM_6, VK_OEM_7, VK_OEM_COMMA, VK_OEM_MINUS,
    VK_OEM_PERIOD, VK_OEM_PLUS, VK_P, VK_PAUSE, VK_PRIOR, VK_Q, VK_R, VK_RCONTROL, VK_RETURN,
    VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_S, VK_SCROLL, VK_SNAPSHOT, VK_SPACE, VK_SUBTRACT,
    VK_T, VK_TAB, VK_U, VK_UP, VK_V, VK_W, VK_X, VK_Y, VK_Z,
};

use super::hook::INJECT_TAG;

/// An unassigned VK: a key event Windows sees during a Win press, so Win-up opens no Start menu.
const START_BLOCKER_VK: u16 = 0xE8;

/// The one table: a key's VK code and whether Windows flags it extended (`LLKHF_EXTENDED`).
#[allow(clippy::too_many_lines)] // one arm per key: the exhaustive match is the table
pub const fn vk(key: Key) -> (u16, bool) {
    match key {
        Key::Esc => (VK_ESCAPE, false),
        Key::F1 => (VK_F1, false),
        Key::F2 => (VK_F2, false),
        Key::F3 => (VK_F3, false),
        Key::F4 => (VK_F4, false),
        Key::F5 => (VK_F5, false),
        Key::F6 => (VK_F6, false),
        Key::F7 => (VK_F7, false),
        Key::F8 => (VK_F8, false),
        Key::F9 => (VK_F9, false),
        Key::F10 => (VK_F10, false),
        Key::F11 => (VK_F11, false),
        Key::F12 => (VK_F12, false),
        Key::F13 => (VK_F13, false),
        Key::F14 => (VK_F14, false),
        Key::F15 => (VK_F15, false),
        Key::F16 => (VK_F16, false),
        Key::F17 => (VK_F17, false),
        Key::F18 => (VK_F18, false),
        Key::F19 => (VK_F19, false),
        Key::F20 => (VK_F20, false),
        Key::F21 => (VK_F21, false),
        Key::F22 => (VK_F22, false),
        Key::F23 => (VK_F23, false),
        Key::F24 => (VK_F24, false),
        Key::Backquote => (VK_OEM_3, false),
        Key::Digit1 => (VK_1, false),
        Key::Digit2 => (VK_2, false),
        Key::Digit3 => (VK_3, false),
        Key::Digit4 => (VK_4, false),
        Key::Digit5 => (VK_5, false),
        Key::Digit6 => (VK_6, false),
        Key::Digit7 => (VK_7, false),
        Key::Digit8 => (VK_8, false),
        Key::Digit9 => (VK_9, false),
        Key::Digit0 => (VK_0, false),
        Key::Minus => (VK_OEM_MINUS, false),
        Key::Equal => (VK_OEM_PLUS, false),
        Key::Backspace => (VK_BACK, false),
        Key::Tab => (VK_TAB, false),
        Key::Q => (VK_Q, false),
        Key::W => (VK_W, false),
        Key::E => (VK_E, false),
        Key::R => (VK_R, false),
        Key::T => (VK_T, false),
        Key::Y => (VK_Y, false),
        Key::U => (VK_U, false),
        Key::I => (VK_I, false),
        Key::O => (VK_O, false),
        Key::P => (VK_P, false),
        Key::BracketLeft => (VK_OEM_4, false),
        Key::BracketRight => (VK_OEM_6, false),
        Key::Backslash => (VK_OEM_5, false),
        Key::CapsLock => (VK_CAPITAL, false),
        Key::A => (VK_A, false),
        Key::S => (VK_S, false),
        Key::D => (VK_D, false),
        Key::F => (VK_F, false),
        Key::G => (VK_G, false),
        Key::H => (VK_H, false),
        Key::J => (VK_J, false),
        Key::K => (VK_K, false),
        Key::L => (VK_L, false),
        Key::Semicolon => (VK_OEM_1, false),
        Key::Quote => (VK_OEM_7, false),
        Key::Enter => (VK_RETURN, false),
        Key::LShift => (VK_LSHIFT, false),
        Key::Z => (VK_Z, false),
        Key::X => (VK_X, false),
        Key::C => (VK_C, false),
        Key::V => (VK_V, false),
        Key::B => (VK_B, false),
        Key::N => (VK_N, false),
        Key::M => (VK_M, false),
        Key::Comma => (VK_OEM_COMMA, false),
        Key::Period => (VK_OEM_PERIOD, false),
        Key::Slash => (VK_OEM_2, false),
        Key::RShift => (VK_RSHIFT, false),
        Key::LCtrl => (VK_LCONTROL, false),
        Key::LWin => (VK_LWIN, true),
        Key::LAlt => (VK_LMENU, false),
        Key::Space => (VK_SPACE, false),
        Key::RAlt => (VK_RMENU, true),
        Key::RWin => (VK_RWIN, true),
        Key::Menu => (VK_APPS, true),
        Key::RCtrl => (VK_RCONTROL, true),
        Key::PrintScreen => (VK_SNAPSHOT, true),
        Key::ScrollLock => (VK_SCROLL, false),
        Key::Pause => (VK_PAUSE, false),
        Key::Insert => (VK_INSERT, true),
        Key::Home => (VK_HOME, true),
        Key::PageUp => (VK_PRIOR, true),
        Key::Delete => (VK_DELETE, true),
        Key::End => (VK_END, true),
        Key::PageDown => (VK_NEXT, true),
        Key::Up => (VK_UP, true),
        Key::Left => (VK_LEFT, true),
        Key::Down => (VK_DOWN, true),
        Key::Right => (VK_RIGHT, true),
        Key::NumLock => (VK_NUMLOCK, true),
        Key::NumDivide => (VK_DIVIDE, true),
        Key::NumMultiply => (VK_MULTIPLY, false),
        Key::NumSubtract => (VK_SUBTRACT, false),
        Key::Num7 => (VK_NUMPAD7, false),
        Key::Num8 => (VK_NUMPAD8, false),
        Key::Num9 => (VK_NUMPAD9, false),
        Key::NumAdd => (VK_ADD, false),
        Key::Num4 => (VK_NUMPAD4, false),
        Key::Num5 => (VK_NUMPAD5, false),
        Key::Num6 => (VK_NUMPAD6, false),
        Key::Num1 => (VK_NUMPAD1, false),
        Key::Num2 => (VK_NUMPAD2, false),
        Key::Num3 => (VK_NUMPAD3, false),
        Key::NumEnter => (VK_RETURN, true),
        Key::Num0 => (VK_NUMPAD0, false),
        Key::NumDecimal => (VK_DECIMAL, false),
    }
}

/// The key a hook event is about. The exact `(vk, extended)` pair wins; otherwise the VK alone,
/// because other tools inject Win or arrows without the extended flag.
// shortcut: with NumLock off the numpad sends Home, arrows and friends, so it reads as those keys;
// map by scan code if a numpad chord must work with NumLock off.
pub fn key_id(code: u16, extended: bool) -> KeyId {
    Key::ALL
        .into_iter()
        .find(|&k| vk(k) == (code, extended))
        .or_else(|| Key::ALL.into_iter().find(|&k| vk(k).0 == code))
        .map_or(KeyId::Other(code), KeyId::Known)
}

/// Whether the key is physically down now.
pub fn key_held(key: Key) -> bool {
    vk_held(vk(key).0)
}

/// [`key_held`] for any key the hook reports, for [`oxi_core::hotkey::Tracker::resync`].
pub fn id_held(id: KeyId) -> bool {
    match id {
        KeyId::Known(key) => key_held(key),
        KeyId::Other(code) => vk_held(code),
    }
}

/// Whether the whole chord is physically held now: the answer to `Effect::PollChord`.
pub fn chord_held_now(chord: &Chord) -> bool {
    chord.is_held(key_held)
}

fn vk_held(code: u16) -> bool {
    // SAFETY: GetAsyncKeyState only reads the key state; any VK value is accepted.
    let state = unsafe { GetAsyncKeyState(i32::from(code)) };
    state < 0
}

/// Injects `(key, down)` events in order, tagged so our own hook ignores them.
/// Fails when Windows took fewer events (UIPI: the foreground window is elevated).
pub fn send_keys(keys: &[(Key, bool)]) -> io::Result<()> {
    send_tagged(keys, INJECT_TAG)
}

/// Injects keys as another program would, so the hook tracks them.
#[cfg(test)]
pub(crate) fn send_keys_untagged(keys: &[(Key, bool)]) -> io::Result<()> {
    send_tagged(keys, 0)
}

fn send_tagged(keys: &[(Key, bool)], tag: usize) -> io::Result<()> {
    let inputs: Vec<INPUT> = keys
        .iter()
        .map(|&(key, down)| {
            let (code, extended) = vk(key);
            input(code, extended, down, tag)
        })
        .collect();
    send(&inputs)
}

/// Injects the Start-menu blocker (VK 0xE8 down and up), tagged as ours.
pub fn send_start_blocker() -> io::Result<()> {
    send(&[
        input(START_BLOCKER_VK, false, true, INJECT_TAG),
        input(START_BLOCKER_VK, false, false, INJECT_TAG),
    ])
}

/// Injects one UTF-16 unit as a tagged `KEYEVENTF_UNICODE` down and up (hooks see `VK_PACKET`).
pub fn send_unit(unit: u16) -> io::Result<()> {
    send(&unit_events(unit))
}

fn unit_events(unit: u16) -> [INPUT; 2] {
    [
        event(0, unit, KEYEVENTF_UNICODE, INJECT_TAG),
        event(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP, INJECT_TAG),
    ]
}

fn input(code: u16, extended: bool, down: bool, tag: usize) -> INPUT {
    let mut flags = 0;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    event(code, 0, flags, tag)
}

fn event(code: u16, scan: u16, flags: KEYBD_EVENT_FLAGS, tag: usize) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: code,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: tag,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> io::Result<()> {
    let count = u32::try_from(inputs.len()).map_err(|_| io::ErrorKind::InvalidInput)?;
    let size = i32::try_from(size_of::<INPUT>()).map_err(|_| io::ErrorKind::InvalidInput)?;
    // SAFETY: `inputs` is a live slice of `count` initialised INPUT values of `size` bytes each.
    let sent = unsafe { SendInput(count, inputs.as_ptr(), size) };
    if sent == count {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_round_trips() {
        for key in Key::ALL {
            let (code, extended) = vk(key);
            assert_eq!(key_id(code, extended), KeyId::Known(key), "{key:?}");
        }
    }

    #[test]
    fn extended_flag_separates_and_falls_back() {
        let cases = [
            ((VK_RETURN, false), KeyId::Known(Key::Enter)),
            ((VK_RETURN, true), KeyId::Known(Key::NumEnter)),
            ((VK_RCONTROL, true), KeyId::Known(Key::RCtrl)),
            ((VK_LCONTROL, false), KeyId::Known(Key::LCtrl)),
            ((VK_LEFT, true), KeyId::Known(Key::Left)),
            ((VK_UP, true), KeyId::Known(Key::Up)),
            // Injected without the extended flag by other tools.
            ((VK_LWIN, false), KeyId::Known(Key::LWin)),
            ((VK_RIGHT, false), KeyId::Known(Key::Right)),
            ((START_BLOCKER_VK, false), KeyId::Other(START_BLOCKER_VK)),
            ((0xAD, true), KeyId::Other(0xAD)), // VK_VOLUME_MUTE
        ];
        for ((code, extended), want) in cases {
            assert_eq!(key_id(code, extended), want, "{code:#x} ext={extended}");
        }
    }

    #[test]
    fn a_unit_is_a_tagged_unicode_down_then_up() {
        let [down, up] = unit_events(0xD83D);
        for (event, flags) in [
            (down, KEYEVENTF_UNICODE),
            (up, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
        ] {
            assert_eq!(event.r#type, INPUT_KEYBOARD);
            // SAFETY: built as a keyboard INPUT above.
            let ki = unsafe { event.Anonymous.ki };
            assert_eq!(
                (ki.wVk, ki.wScan, ki.dwFlags, ki.dwExtraInfo),
                (0, 0xD83D, flags, INJECT_TAG)
            );
        }
    }

    #[test]
    fn extended_keys_are_flagged() {
        for key in [
            Key::RCtrl,
            Key::RAlt,
            Key::Up,
            Key::Left,
            Key::Insert,
            Key::NumEnter,
        ] {
            assert!(vk(key).1, "{key:?}");
        }
        for key in [Key::LCtrl, Key::LAlt, Key::Enter, Key::Num4, Key::RShift] {
            assert!(!vk(key).1, "{key:?}");
        }
    }
}
