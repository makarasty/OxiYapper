//! The OS autostart entry (item 54): on Windows a value under
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` whose data is the command line to start.
//! Other OSes answer [`io::ErrorKind::Unsupported`] until they get theirs.

use std::io;

/// The entry name of the real app. Tests use another one.
pub const NAME: &str = oxi_core::APP_NAME;

#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// The longest command line Windows starts, in UTF-16 units, plus its NUL.
#[cfg(windows)]
const MAX_COMMAND: usize = 32_768;

/// The command line of entry `name`; `None` when there is none.
#[cfg(windows)]
pub fn query(name: &str) -> io::Result<Option<String>> {
    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};
    let (key, value) = (crate::os::wide(RUN_KEY), crate::os::wide(name));
    let mut data = vec![0u16; MAX_COMMAND];
    let mut len = u32::try_from(MAX_COMMAND * 2).unwrap_or(u32::MAX);
    // SAFETY: both names are NUL-terminated and outlive the call; `data` holds `len` bytes.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &raw mut len,
        )
    };
    match status {
        0 => {
            let text = &data[..usize::try_from(len).unwrap_or(0) / 2];
            let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
            Ok(Some(String::from_utf16_lossy(&text[..end])))
        }
        ERROR_FILE_NOT_FOUND => Ok(None),
        e => Err(win32(e)),
    }
}

/// Writes entry `name` with `command`, replacing what it held.
#[cfg(windows)]
pub fn register(name: &str, command: &str) -> io::Result<()> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegSetKeyValueW};
    let (key, value, data) = (
        crate::os::wide(RUN_KEY),
        crate::os::wide(name),
        crate::os::wide(command),
    );
    let bytes = u32::try_from(data.len() * 2).map_err(|_| io::ErrorKind::InvalidInput)?;
    // SAFETY: the names and `data` are NUL-terminated and outlive the call; `bytes` is the size
    // of `data`, its NUL included, as REG_SZ wants.
    let status = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            REG_SZ,
            data.as_ptr().cast(),
            bytes,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(win32(status))
    }
}

/// Removes entry `name`; one that is not there is no error.
#[cfg(windows)]
pub fn unregister(name: &str) -> io::Result<()> {
    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteKeyValueW};
    let (key, value) = (crate::os::wide(RUN_KEY), crate::os::wide(name));
    // SAFETY: both names are NUL-terminated and outlive the call.
    let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr()) };
    match status {
        0 | ERROR_FILE_NOT_FOUND => Ok(()),
        e => Err(win32(e)),
    }
}

#[cfg(windows)]
fn win32(code: u32) -> io::Error {
    io::Error::from_raw_os_error(i32::try_from(code).unwrap_or(i32::MAX))
}

/// No autostart on this OS yet.
#[cfg(not(windows))]
pub fn query(_: &str) -> io::Result<Option<String>> {
    Err(io::ErrorKind::Unsupported.into())
}

/// No autostart on this OS yet.
#[cfg(not(windows))]
pub fn register(_: &str, _: &str) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// No autostart on this OS yet.
#[cfg(not(windows))]
pub fn unregister(_: &str) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}
