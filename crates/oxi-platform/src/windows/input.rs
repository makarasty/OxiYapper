//! `SendInput` paste keystrokes and the facts about the foreground window (ARCHITECTURE
//! section 10, steps 2, 3, 6 and type mode).

use std::ptr;
use std::thread;
use std::time::Duration;

use oxi_core::hotkey::Key;
use oxi_core::paste::{Foreground, Input, InputBlocked};
use oxi_core::types::Millis;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL,
    TOKEN_QUERY, TokenIntegrityLevel,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, SendInput,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use super::hook::INJECT_TAG;
use super::keys;

/// Ctrl, Shift, Alt and Win, both sides: the keys step 2 waits on.
const MODIFIERS: [Key; 8] = [
    Key::LCtrl,
    Key::RCtrl,
    Key::LShift,
    Key::RShift,
    Key::LAlt,
    Key::RAlt,
    Key::LWin,
    Key::RWin,
];

/// The longest Win32 path in UTF-16 units.
const MAX_PATH_UNITS: usize = 32_768;

/// The keyboard of the paste worker: every event it injects carries [`INJECT_TAG`].
#[derive(Debug, Default)]
pub struct Keyboard;

impl Input for Keyboard {
    fn modifiers_held(&self) -> bool {
        MODIFIERS.into_iter().any(keys::key_held)
    }

    fn foreground(&self) -> Foreground {
        foreground()
    }

    fn send_chord(&mut self, keys: &[Key]) -> Result<(), InputBlocked> {
        keys::send_keys(&chord_events(keys)).map_err(|_| InputBlocked)
    }

    fn type_units(&mut self, units: &[u16], gap: Millis) -> Result<(), InputBlocked> {
        for (i, &unit) in units.iter().enumerate() {
            if i > 0 {
                self.sleep(gap);
            }
            send(&unit_events(unit))?;
        }
        Ok(())
    }

    fn sleep(&mut self, time: Millis) {
        thread::sleep(Duration::from_millis(time.0));
    }
}

/// Presses `keys` in order and releases them in reverse.
fn chord_events(keys: &[Key]) -> Vec<(Key, bool)> {
    let downs = keys.iter().map(|&k| (k, true));
    let ups = keys.iter().rev().map(|&k| (k, false));
    downs.chain(ups).collect()
}

/// One UTF-16 unit as a `KEYEVENTF_UNICODE` down and up (Windows reports it as `VK_PACKET`).
fn unit_events(unit: u16) -> [INPUT; 2] {
    let event = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0,
                wScan: unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    };
    [
        event(KEYEVENTF_UNICODE),
        event(KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
    ]
}

// shortcut: keys::send is private and builds only VK events; move this there if keys.rs grows a
// Unicode sender.
fn send(inputs: &[INPUT; 2]) -> Result<(), InputBlocked> {
    let size = i32::try_from(size_of::<INPUT>()).map_err(|_| InputBlocked)?;
    // SAFETY: `inputs` is a live array of 2 initialised INPUT values of `size` bytes each.
    let sent = unsafe { SendInput(2, inputs.as_ptr(), size) };
    if sent == 2 { Ok(()) } else { Err(InputBlocked) }
}

fn foreground() -> Foreground {
    // SAFETY: no arguments.
    let window = unsafe { GetForegroundWindow() };
    let mut pid = 0;
    // SAFETY: `pid` is a live local; a null or stale window leaves it 0.
    unsafe { GetWindowThreadProcessId(window, &raw mut pid) };
    // SAFETY: opening with query rights only; pid 0 or a dead pid gives a null handle.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    let process = (!process.is_null()).then(|| Owned(process));
    let app = process
        .as_ref()
        .and_then(|p| image_path(p.0))
        .map(|path| exe_name(&path).to_owned())
        .unwrap_or_default();
    let theirs = process.as_ref().and_then(|p| integrity(p.0));
    // SAFETY: a pseudo handle for our own process; it needs no closing.
    let ours = integrity(unsafe { GetCurrentProcess() });
    Foreground {
        app,
        elevated: elevated(theirs, ours),
    }
}

/// UIPI drops our input when the target runs above our integrity level. A target whose level
/// cannot be read (no foreground, a protected or higher-level process) counts as elevated, so
/// the text stays on the clipboard instead of going nowhere.
fn elevated(theirs: Option<u32>, ours: Option<u32>) -> bool {
    match (theirs, ours) {
        (Some(theirs), Some(ours)) => theirs > ours,
        (None, _) => true,
        (Some(_), None) => false,
    }
}

/// The file name of a Win32 path.
fn exe_name(path: &str) -> &str {
    path.rsplit('\\').next().unwrap_or(path)
}

fn image_path(process: HANDLE) -> Option<String> {
    let mut buf = vec![0u16; MAX_PATH_UNITS];
    let mut len = u32::try_from(buf.len()).ok()?;
    // SAFETY: `buf` holds `len` units; on success `len` is the length written without the null.
    let ok = unsafe {
        QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &raw mut len)
    };
    let len = usize::try_from(len).ok()?;
    (ok != 0).then(|| String::from_utf16_lossy(&buf[..len]))
}

/// The mandatory integrity RID of `process` (0x2000 medium, 0x3000 high).
fn integrity(process: HANDLE) -> Option<u32> {
    let mut token = ptr::null_mut();
    // SAFETY: `token` is a live local that receives a handle we own.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) } == 0 {
        return None;
    }
    let token = Owned(token);
    // u64 cells keep the TOKEN_MANDATORY_LABEL aligned; 128 bytes hold it and its SID.
    let mut buf = [0u64; 16];
    let size = u32::try_from(size_of_val(&buf)).ok()?;
    let mut len = 0;
    // SAFETY: `buf` is a live, aligned buffer of `size` bytes.
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenIntegrityLevel,
            buf.as_mut_ptr().cast(),
            size,
            &raw mut len,
        )
    };
    if ok == 0 {
        return None;
    }
    // SAFETY: on success `buf` starts with a TOKEN_MANDATORY_LABEL whose SID points into `buf`;
    // the RID is its last sub-authority.
    unsafe {
        let label = &*buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
        let count = *GetSidSubAuthorityCount(label.Label.Sid);
        Some(*GetSidSubAuthority(
            label.Label.Sid,
            u32::from(count.checked_sub(1)?),
        ))
    }
}

/// A kernel handle closed on drop.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the handle came from an Open* call and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chord_presses_in_order_and_releases_in_reverse() {
        assert_eq!(chord_events(&[]), []);
        assert_eq!(
            chord_events(&[Key::LCtrl, Key::LShift, Key::V]),
            [
                (Key::LCtrl, true),
                (Key::LShift, true),
                (Key::V, true),
                (Key::V, false),
                (Key::LShift, false),
                (Key::LCtrl, false),
            ]
        );
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
    fn elevated_only_above_our_level_or_unreadable() {
        let cases = [
            ((Some(0x3000), Some(0x2000)), true),
            ((Some(0x2000), Some(0x2000)), false),
            ((Some(0x1000), Some(0x2000)), false),
            ((Some(0x4000), Some(0x3000)), true),
            ((None, Some(0x2000)), true),
            ((Some(0x3000), None), false),
        ];
        for ((theirs, ours), want) in cases {
            assert_eq!(elevated(theirs, ours), want, "{theirs:?} vs {ours:?}");
        }
    }

    #[test]
    fn exe_name_is_the_last_path_part() {
        for (path, want) in [
            (r"C:\Windows\System32\notepad.exe", "notepad.exe"),
            (r"\\?\C:\a b\App.EXE", "App.EXE"),
            ("game.exe", "game.exe"),
            ("", ""),
        ] {
            assert_eq!(exe_name(path), want, "{path}");
        }
    }

    #[test]
    fn our_own_integrity_is_readable() {
        // SAFETY: pseudo handle, no closing.
        let level = integrity(unsafe { GetCurrentProcess() });
        assert!(level.is_some_and(|rid| rid >= 0x1000), "{level:?}");
    }
}
