//! Bring a window of this process to the front. Windows lets a process take the foreground only
//! with the user's last input or with permission from the process that has it; the daemon grants
//! it (`AllowSetForegroundWindow`) before it asks the settings window to raise.

use std::io;

use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumThreadWindows, IsIconic, IsWindowVisible, SW_RESTORE, SetForegroundWindow, ShowWindow,
};

/// Restores `hwnd` if it is minimised, then makes it the foreground window. Refusal by the OS
/// is logged by kind and otherwise ignored: the window stays where it is.
fn bring_to_front(hwnd: HWND) {
    // SAFETY: both calls only read or change the state of a window handle; a stale or null
    // handle makes them fail, which is reported below.
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        if SetForegroundWindow(hwnd) == 0 {
            tracing::debug!(kind = ?io::Error::last_os_error().kind(), "window not brought to front");
        }
    }
}

/// [`bring_to_front`] for the visible top-level window of the calling thread, which is the
/// settings window on the UI thread. `false` when the thread has none.
pub fn raise_current_thread_window() -> bool {
    unsafe extern "system" fn first_visible(hwnd: HWND, found: LPARAM) -> i32 {
        // SAFETY: `found` is the address of the `HWND` slot below, live for the whole enumeration.
        let slot = unsafe { &mut *(found as *mut HWND) };
        // SAFETY: `hwnd` is a live window handed out by the enumeration. A minimised window
        // is still visible; winit's hidden helper window is not.
        if unsafe { IsWindowVisible(hwnd) } != 0 {
            *slot = hwnd;
            return 0;
        }
        1
    }
    let mut found: HWND = std::ptr::null_mut();
    // SAFETY: the callback only writes through the pointer passed as `lparam`, which outlives
    // the call; the enumeration is synchronous.
    unsafe {
        EnumThreadWindows(
            GetCurrentThreadId(),
            Some(first_visible),
            std::ptr::from_mut(&mut found) as LPARAM,
        );
    }
    if found.is_null() {
        return false;
    }
    bring_to_front(found);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_without_windows_raises_nothing() {
        assert!(!raise_current_thread_window());
    }

    #[test]
    fn a_null_handle_is_ignored() {
        bring_to_front(std::ptr::null_mut());
    }
}
