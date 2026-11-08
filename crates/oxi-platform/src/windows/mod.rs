//! Windows implementations of the platform traits (ARCHITECTURE section 4).

pub mod clipboard;
pub mod display;
pub mod foreground;
pub mod hook;
pub mod input;
pub mod keys;
pub mod overlay;
pub mod pipe;
pub mod tray;

/// `s` as a NUL-terminated UTF-16 string for a `W` API.
pub(crate) fn wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.as_ref().encode_wide().chain([0]).collect()
}
