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
    ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_SEM_TIMEOUT, GENERIC_READ, GENERIC_WRITE,
    INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    ACL, GetAce, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation,
    LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSID, SYSTEM_MANDATORY_LABEL_ACE,
    TOKEN_INFORMATION_CLASS, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::System::Pipes::{GetNamedPipeClientProcessId, WaitNamedPipeW};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, GetForegroundWindow, GetWindowThreadProcessId,
};

use super::wide;

/// winnt.h values; their windows-sys module (`System_SystemServices`) is not enabled here.
const SYSTEM_MANDATORY_LABEL_ACE_TYPE: u8 = 0x11;
const SECURITY_MANDATORY_MEDIUM_RID: u32 = 0x2000;

/// Owned by and generic-all for `sid` only, protected from inherited ACEs: no Everyone, no
/// other user. The explicit owner and the medium integrity label are what clients check;
/// without them an elevated daemon would default the owner to Administrators, and a label at
/// its own (high) level would lock a normal second launch out. Medium is the floor clients
/// accept, so low-integrity squatters stay out at every daemon level. The label also bars
/// lower levels from reading (`NR`), so a low-integrity program cannot open the pipe read-only
/// and hold one of the few instances.
pub fn owner_only_sddl(sid: &str) -> String {
    format!("O:{sid}D:P(A;;GA;;;{sid})S:(ML;;NWNR;;;ME)")
}

/// The current user's SID string, e.g. `S-1-5-21-...`.
pub fn user_sid() -> io::Result<String> {
    let buf = token_info(TokenUser)?;
    // SAFETY: TokenUser wrote a TOKEN_USER at the start of `buf`; its SID points into `buf`.
    unsafe { sid_string((*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid) }
}

/// `GetTokenInformation(class)` of the process token, in u64 cells so the struct at the
/// start is pointer-aligned.
fn token_info(class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<u64>> {
    let mut raw = null_mut();
    // SAFETY: GetCurrentProcess is a pseudo-handle; `raw` is a valid out pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut raw) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenProcessToken succeeded, so we own the token handle.
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut len = 0u32;
    // SAFETY: size query with a null buffer; it fails with the needed length in `len`.
    unsafe { GetTokenInformation(token.as_raw_handle(), class, null_mut(), 0, &raw mut len) };
    let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
    // SAFETY: `buf` holds at least `len` bytes.
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            class,
            buf.as_mut_ptr().cast(),
            len,
            &raw mut len,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(buf)
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

/// The last sub-authority of `sid` (for `S-1-16-<rid>`, the integrity RID); 0 if it has none.
///
/// # Safety
/// `sid` must point to a valid SID.
unsafe fn sid_rid(sid: PSID) -> u32 {
    // SAFETY: valid SID (caller); the index is below its sub-authority count.
    unsafe {
        match *GetSidSubAuthorityCount(sid) {
            0 => 0,
            n => *GetSidSubAuthority(sid, u32::from(n) - 1),
        }
    }
}

/// Security descriptor for the IPC pipe: the current user only, labelled medium.
pub fn owner_only() -> io::Result<SecurityDescriptor> {
    security_descriptor(&owner_only_sddl(&user_sid()?))
}

/// A security descriptor from SDDL.
pub fn security_descriptor(sddl: &str) -> io::Result<SecurityDescriptor> {
    let sddl = wide(sddl);
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
        if err.raw_os_error() != Some(ERROR_PIPE_BUSY.cast_signed()) {
            return Err(err);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        let ms = u32::try_from(left.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: `path` is NUL-terminated. A 0 ms wait would mean the pipe's default timeout.
        let waited = ms != 0 && unsafe { WaitNamedPipeW(path.as_ptr(), ms) } != 0;
        let timed_out = ms == 0
            || (!waited
                && io::Error::last_os_error().raw_os_error()
                    == Some(ERROR_SEM_TIMEOUT.cast_signed()));
        if timed_out {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "IPC pipe busy: every instance is taken",
            ));
        }
        // Freed or gone (the server exited): CreateFileW again gives the real answer.
    }
}

/// What a client learns about the server end of a pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Owned by the current user and labelled medium or above (no label counts as medium).
    Trusted,
    /// Owned by someone else: another account cannot make a pipe with our SID as owner.
    OtherAccount,
    /// Ours but labelled below medium: a sandboxed or low-integrity process of this user,
    /// which cannot label it higher than its own level.
    LowIntegrity,
}

/// Checks the owner and integrity label of the pipe behind `h`.
pub fn check_server(h: BorrowedHandle<'_>) -> io::Result<Verdict> {
    let mut owner: PSID = null_mut();
    let mut sacl: *mut ACL = null_mut();
    let mut sd = null_mut();
    // SAFETY: valid handle and out pointers; `owner` and `sacl` point into `sd`, freed below.
    let err = unsafe {
        GetSecurityInfo(
            h.as_raw_handle(),
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION,
            &raw mut owner,
            null_mut(),
            null_mut(),
            &raw mut sacl,
            &raw mut sd,
        )
    };
    if err != 0 {
        return Err(io::Error::from_raw_os_error(err.cast_signed()));
    }
    // SAFETY: GetSecurityInfo succeeded, so `owner` is a valid SID and `sacl` null or a
    // valid ACL, both inside `sd`.
    let (owner, label) = unsafe { (sid_string(owner), label_rid(sacl)) };
    // SAFETY: allocated by GetSecurityInfo and not used after this.
    unsafe { LocalFree(sd) };
    let label = label.unwrap_or(SECURITY_MANDATORY_MEDIUM_RID);
    Ok(if owner? != user_sid()? {
        Verdict::OtherAccount
    } else if label < SECURITY_MANDATORY_MEDIUM_RID {
        Verdict::LowIntegrity
    } else {
        Verdict::Trusted
    })
}

/// The integrity RID of the mandatory label ACE in `sacl`, if any.
///
/// # Safety
/// `sacl` must be null or point to a valid ACL.
unsafe fn label_rid(sacl: *const ACL) -> Option<u32> {
    if sacl.is_null() {
        return None;
    }
    // SAFETY: `sacl` is a valid ACL (caller), so each index below AceCount is a valid ACE.
    unsafe {
        for i in 0..u32::from((*sacl).AceCount) {
            let mut ace = null_mut();
            if GetAce(sacl, i, &raw mut ace) == 0 {
                continue;
            }
            let ace = ace.cast::<SYSTEM_MANDATORY_LABEL_ACE>();
            if (*ace).Header.AceType == SYSTEM_MANDATORY_LABEL_ACE_TYPE {
                return Some(sid_rid((&raw mut (*ace).SidStart).cast()));
            }
        }
    }
    None
}

/// Process id of the client on the server end `h` of a pipe connection.
pub fn client_pid(h: BorrowedHandle<'_>) -> io::Result<u32> {
    let mut pid = 0;
    // SAFETY: `h` is an open pipe handle and `pid` a live local.
    if unsafe { GetNamedPipeClientProcessId(h.as_raw_handle(), &raw mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pid)
}

/// Process id that owns the foreground window; 0 when there is none.
pub fn foreground_pid() -> u32 {
    let mut pid = 0;
    // SAFETY: no arguments; `pid` is a live local, and a null window leaves it 0.
    unsafe { GetWindowThreadProcessId(GetForegroundWindow(), &raw mut pid) };
    pid
}

/// Lets a window of process `pid` take the foreground (`AllowSetForegroundWindow`), so a
/// `Raise` push can bring it forward.
pub fn allow_foreground(pid: u32) -> io::Result<()> {
    // SAFETY: plain value argument.
    if unsafe { AllowSetForegroundWindow(pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Runs `op`, a blocking I/O call on `h`, and cancels it with `TimedOut` if it is still
/// running after `limit`.
pub fn with_deadline<R>(
    h: BorrowedHandle<'_>,
    limit: Duration,
    op: impl FnOnce() -> io::Result<R>,
) -> io::Result<R> {
    // How often a cancel is repeated: the first may land before the read is pending.
    const RETRY: Duration = Duration::from_millis(10);
    // A raw pointer is not Send; the scope keeps the handle alive while the watchdog runs.
    let raw = h.as_raw_handle().expose_provenance();
    let (done, wait) = mpsc::channel::<()>();
    thread::scope(|s| {
        let watchdog = s.spawn(move || {
            let mut fired = false;
            let mut wait_for = limit;
            while wait.recv_timeout(wait_for) == Err(RecvTimeoutError::Timeout) {
                fired = true;
                wait_for = RETRY;
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
                Err(io::Error::new(io::ErrorKind::TimedOut, "IPC I/O timed out"))
            }
            r => r,
        }
    })
}
