//! Output of a GUI-subsystem process (no console of its own): reuse the parent's console, if any.
//!
//! Started from Explorer there is no parent console and nothing happens; `print!` and `eprint!`
//! then write to nowhere (std ignores a missing standard handle), and the log file is the record.

use std::fs::OpenOptions;
use std::os::windows::io::IntoRawHandle;

type Handle = *mut core::ffi::c_void;

const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5; // (DWORD)-11
const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4; // (DWORD)-12
const INVALID_HANDLE_VALUE: Handle = -1_isize as Handle;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn AttachConsole(process_id: u32) -> i32;
    fn GetStdHandle(which: u32) -> Handle;
    fn SetStdHandle(which: u32, handle: Handle) -> i32;
}

/// Attaches to the parent's console and points stdout and stderr at it, but only those the
/// process does not already have: `OxiYapper --version > out.txt` keeps its redirect.
pub fn attach() {
    // SAFETY: plain Win32 calls with no pointers; failure (no parent console, or this process
    // already has one, as a debug build does) leaves everything as it was.
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } == 0 {
        return;
    }
    for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: as above.
        let current = unsafe { GetStdHandle(which) };
        if !current.is_null() && current != INVALID_HANDLE_VALUE {
            continue;
        }
        if let Ok(conout) = OpenOptions::new().write(true).open("CONOUT$") {
            // SAFETY: the handle is valid and now owned by the process for its whole life.
            unsafe { SetStdHandle(which, conout.into_raw_handle()) };
        }
    }
}
