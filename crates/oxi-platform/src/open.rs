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
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    use crate::os::wide;

    let verb = wide("open");
    let file = wide(path);
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
    // Success is a value above 32; at or below 32 the value itself is the error, and
    // GetLastError is not set (documented contract of ShellExecuteW).
    let code = code as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(shell_error(code))
    }
}

/// A failed `ShellExecuteW` return value (0..=32) as an error.
#[cfg(windows)]
fn shell_error(code: isize) -> io::Error {
    use io::ErrorKind;
    match code {
        // Plain Win32 codes: file or path not found, access denied, out of memory, bad exe.
        2 | 3 | 5 | 8 | 11 => io::Error::from_raw_os_error(i32::try_from(code).unwrap_or(0)),
        0 => ErrorKind::OutOfMemory.into(),
        26 => io::Error::new(ErrorKind::ResourceBusy, "the file is in use by another app"),
        27 | 31 => io::Error::new(
            ErrorKind::Unsupported,
            "no app is set to open this file type",
        ),
        28 => io::Error::new(
            ErrorKind::TimedOut,
            "the app did not answer the open request",
        ),
        29 | 30 => io::Error::other("the app failed to take the open request"),
        32 => io::Error::new(
            ErrorKind::NotFound,
            "a library needed to open the file is missing",
        ),
        _ => io::Error::other(format!("ShellExecuteW failed with code {code}")),
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

    #[cfg(windows)]
    #[test]
    fn shell_error_maps_every_failure_code() {
        use std::io::ErrorKind;
        let table = [
            (0, ErrorKind::OutOfMemory),
            (2, ErrorKind::NotFound),
            (3, ErrorKind::NotFound),
            (5, ErrorKind::PermissionDenied),
            (26, ErrorKind::ResourceBusy),
            (27, ErrorKind::Unsupported),
            (28, ErrorKind::TimedOut),
            (29, ErrorKind::Other),
            (31, ErrorKind::Unsupported),
            (32, ErrorKind::NotFound),
        ];
        for (code, kind) in table {
            let err = super::shell_error(code);
            assert_eq!(err.kind(), kind, "code {code}");
            assert!(
                !err.to_string().contains("successfully"),
                "code {code}: {err}"
            );
        }
        assert_eq!(super::shell_error(8).raw_os_error(), Some(8));
    }
}
