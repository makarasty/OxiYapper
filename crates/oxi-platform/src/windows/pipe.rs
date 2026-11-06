//! Named pipe with a current-user DACL (ARCHITECTURE section 5).

use std::ffi::c_void;
use std::io;
use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use interprocess::os::windows::security_descriptor::{
    AsSecurityDescriptorExt, BorrowedSecurityDescriptor, SecurityDescriptor,
};
use windows_sys::Win32::Foundation::{
    ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
    LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, OWNER_SECURITY_INFORMATION, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::System::Pipes::WaitNamedPipeW;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Owned by and generic-all for `sid` only, protected from inherited ACEs: no Everyone, no
/// other user. The explicit owner lets clients check it even when an elevated token would
/// default it to Administrators.
pub fn owner_only_sddl(sid: &str) -> String {
    format!("O:{sid}D:P(A;;GA;;;{sid})")
}

/// The current user's SID string, e.g. `S-1-5-21-...`.
pub fn user_sid() -> io::Result<String> {
    let mut raw = null_mut();
    // SAFETY: GetCurrentProcess is a pseudo-handle; `raw` is a valid out pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut raw) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenProcessToken succeeded, so we own the token handle.
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };

    let mut len = 0u32;
    // SAFETY: size query with a null buffer; it fails with the needed length in `len`.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            null_mut(),
            0,
            &raw mut len,
        )
    };
    // u64 cells keep TOKEN_USER (pointer-aligned) aligned.
    let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
    // SAFETY: `buf` holds at least `len` bytes.
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buf.as_mut_ptr().cast(),
            len,
            &raw mut len,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: GetTokenInformation(TokenUser) wrote a TOKEN_USER at the start of `buf`, and
    // its SID points into `buf`, alive for the call.
    unsafe { sid_string((*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid) }
}

/// # Safety
/// `sid` must point to a valid SID.
unsafe fn sid_string(sid: PSID) -> io::Result<String> {
    let mut wide = null_mut();
    // SAFETY: `sid` is valid (caller); `wide` is a valid out pointer.
    if unsafe { ConvertSidToStringSidW(sid, &raw mut wide) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ConvertSidToStringSidW returned a NUL-terminated string.
    let s = unsafe {
        let mut n = 0;
        while *wide.add(n) != 0 {
            n += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(wide, n))
    };
    // SAFETY: the string was allocated by LocalAlloc and is not used after this.
    unsafe { LocalFree(wide.cast()) };
    Ok(s)
}

/// Security descriptor for the IPC pipe: the current user only.
pub fn owner_only() -> io::Result<SecurityDescriptor> {
    let sddl = wide(&owner_only_sddl(&user_sid()?));
    let mut sd: *mut c_void = null_mut();
    // SAFETY: `sddl` is NUL-terminated; `sd` is a valid out pointer.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &raw mut sd,
            null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `sd` is a valid self-relative descriptor until LocalFree; it is copied first.
    let owned = unsafe { BorrowedSecurityDescriptor::from_ptr(sd) }.to_owned_sd();
    // SAFETY: allocated by LocalAlloc in the call above and not used after this.
    unsafe { LocalFree(sd) };
    owned
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Opens the client end of pipe `path` for overlapped I/O at identification level, so the
/// server can read who we are but never act as us. Waits at most `busy_wait` while every
/// instance is taken.
pub fn connect(path: &str, busy_wait: Duration) -> io::Result<OwnedHandle> {
    let path = wide(path);
    let deadline = Instant::now() + busy_wait;
    loop {
        // SAFETY: `path` is NUL-terminated; no security attributes or template.
        let h = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE {
            // SAFETY: CreateFileW succeeded, so we own the handle.
            return Ok(unsafe { OwnedHandle::from_raw_handle(h) });
        }
        let err = io::Error::last_os_error();
        let left = deadline.saturating_duration_since(Instant::now());
        if err.raw_os_error() != Some(ERROR_PIPE_BUSY.cast_signed()) {
            return Err(err);
        }
        let ms = u32::try_from(left.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: `path` is NUL-terminated. A 0 ms wait would mean the pipe's default timeout.
        if ms == 0 || unsafe { WaitNamedPipeW(path.as_ptr(), ms) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "IPC pipe busy: every instance is taken",
            ));
        }
    }
}

/// True when the pipe behind `h` is owned by the current user. Only the user (or an
/// administrator) can create a pipe with that owner, so another account's squatting pipe
/// fails this check.
pub fn owned_by_current_user(h: BorrowedHandle<'_>) -> io::Result<bool> {
    let mut owner: PSID = null_mut();
    let mut sd = null_mut();
    // SAFETY: valid handle and out pointers; `owner` points into `sd`, freed below.
    let err = unsafe {
        GetSecurityInfo(
            h.as_raw_handle(),
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &raw mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &raw mut sd,
        )
    };
    if err != 0 {
        return Err(io::Error::from_raw_os_error(err.cast_signed()));
    }
    // SAFETY: GetSecurityInfo succeeded, so `owner` is a valid SID inside `sd`.
    let owner = unsafe { sid_string(owner) };
    // SAFETY: allocated by GetSecurityInfo and not used after this.
    unsafe { LocalFree(sd) };
    Ok(owner? == user_sid()?)
}

/// Runs `op`, a blocking I/O call on `h`, and cancels it with `TimedOut` if it is still
/// running after `limit`.
pub fn with_deadline<R>(
    h: BorrowedHandle<'_>,
    limit: Duration,
    op: impl FnOnce() -> io::Result<R>,
) -> io::Result<R> {
    // A raw pointer is not Send; the scope keeps the handle alive while the watchdog runs.
    let raw = h.as_raw_handle().expose_provenance();
    let (done, wait) = mpsc::channel::<()>();
    thread::scope(|s| {
        let watchdog = s.spawn(move || {
            let fired = wait.recv_timeout(limit) == Err(RecvTimeoutError::Timeout);
            if fired {
                // SAFETY: the handle is open for the whole scope; this only aborts its I/O.
                unsafe { CancelIoEx(std::ptr::with_exposed_provenance_mut(raw), null()) };
            }
            fired
        });
        let result = op();
        drop(done);
        let fired = watchdog.join().unwrap_or(true);
        match result {
            Err(e) if fired && e.raw_os_error() == Some(ERROR_OPERATION_ABORTED.cast_signed()) => {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "IPC read timed out",
                ))
            }
            r => r,
        }
    })
}
