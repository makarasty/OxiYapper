//! Named pipe with a current-user DACL (ARCHITECTURE section 5).

use std::ffi::c_void;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::null_mut;

use interprocess::os::windows::security_descriptor::{
    AsSecurityDescriptorExt, BorrowedSecurityDescriptor, SecurityDescriptor,
};
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Generic-all for `sid` only, protected from inherited ACEs: no Everyone, no other user.
pub fn owner_only_sddl(sid: &str) -> String {
    format!("D:P(A;;GA;;;{sid})")
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
    // SAFETY: GetTokenInformation(TokenUser) wrote a TOKEN_USER at the start of `buf`.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };

    let mut wide = null_mut();
    // SAFETY: `sid` points into `buf`, alive here; `wide` is a valid out pointer.
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
    let sddl: Vec<u16> = owner_only_sddl(&user_sid()?)
        .encode_utf16()
        .chain([0])
        .collect();
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
