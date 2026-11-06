//! Open a file in the user's default app for it. The path always travels as one argument,
//! never through a shell or a command line that would split it (commas, spaces, `&`).

use std::io;
use std::path::Path;

/// A missing file is `NotFound` here, before the OS opener would show its own error dialog.
pub fn open(path: &Path) -> io::Result<()> {
    std::fs::metadata(path)?;
    open_existing(path)
}

/// Windows: `ShellExecuteW` with the "open" verb.
#[cfg(windows)]
fn open_existing(path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let wide = |s: &std::ffi::OsStr| s.encode_wide().chain([0]).collect::<Vec<u16>>();
    let verb = wide("open".as_ref());
    let file = wide(path.as_os_str());
    // SAFETY: both strings are NUL-terminated and outlive the call; the null pointers are the
    // optional owner window, parameters and directory.
    let code = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // Success is a value above 32 (documented contract of ShellExecuteW).
    if code as isize > 32 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// macOS `open`, elsewhere `xdg-open`; only a failed spawn is an error.
#[cfg(not(windows))]
fn open_existing(path: &Path) -> io::Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut child = std::process::Command::new(opener).arg(path).spawn()?;
    // Reap it off the caller's thread.
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn open_missing_file_is_not_found() {
        let path = std::env::temp_dir().join("oxi-open-missing, with comma.toml");
        let _ = std::fs::remove_file(&path);
        let err = super::open(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
