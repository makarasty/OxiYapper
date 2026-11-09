//! Windows implementations of the platform traits (ARCHITECTURE section 4).

pub mod clipboard;
pub mod display;
pub mod foreground;
pub mod hook;
pub mod input;
pub mod keys;
pub mod overlay;
pub mod pipe;
pub mod shortcuts;
pub mod tray;

/// `s` as a NUL-terminated UTF-16 string for a `W` API.
pub(crate) fn wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.as_ref().encode_wide().chain([0]).collect()
}

/// Gives this process's resident pages back to the system now; each returns on its next use.
pub fn trim_working_set() {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
    // SAFETY: the pseudo handle is always valid; (MAX, MAX) is the documented trim request.
    unsafe {
        SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

/// Windows "Animation effects" is off.
pub fn reduced_motion() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SPI_GETCLIENTAREAANIMATION, SystemParametersInfoW,
    };
    let mut animate: i32 = 1;
    // SAFETY: this action writes one BOOL through the pointer, which lives across the call.
    let ok = unsafe {
        SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&raw mut animate).cast(), 0)
    };
    ok != 0 && animate == 0
}
