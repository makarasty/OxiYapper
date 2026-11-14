//! Start and dictate on CPU on a PC with no Vulkan loader.
//!
//! `vulkan-1.dll` is delay-loaded (`build.rs`), so the exe starts without it. The first Vulkan
//! call comes from ggml's backend registry, on the CPU path too: `ggml_vk_instance_init` hands
//! `vkGetInstanceProcAddr` to vulkan-hpp's dispatcher, then calls `vk::enumerateInstanceVersion`.
//! A missing loader makes the delay-load helper raise an SEH exception there, which ggml's
//! `catch (...)` (built with `/EHsc`) does not catch. This failure hook answers instead with a
//! `vkGetInstanceProcAddr` that knows only `vkEnumerateInstanceVersion`, and that one returns
//! `VK_ERROR_INITIALIZATION_FAILED`: vulkan-hpp throws `vk::SystemError`, ggml catches it and
//! registers no Vulkan backend, so the device list is empty and whisper runs on CPU.

use std::ffi::{CStr, c_char, c_void};
use std::ptr::null;

const DLI_FAIL_LOAD_LIB: u32 = 3;
const DLI_FAIL_GET_PROC: u32 = 4;
const VK_ERROR_INITIALIZATION_FAILED: i32 = -3;

/// `DelayLoadInfo` of `delayimp.h`, with `DelayLoadProc` flattened (same layout).
#[allow(dead_code)] // filled by the helper; only the names are read
#[repr(C)]
struct DelayLoadInfo {
    cb: u32,
    pidd: *const c_void,
    ppfn: *mut *const c_void,
    sz_dll: *const c_char,
    import_by_name: i32,
    /// A name when `import_by_name` is set, else an ordinal in the low bits.
    proc_name: *const c_char,
    hmod_cur: *mut c_void,
    pfn_cur: *const c_void,
    last_error: u32,
}

type Hook = unsafe extern "system" fn(u32, *const DelayLoadInfo) -> *const c_void;

/// Read by the delay-load helper (`delayimp.lib`) when a delay-loaded DLL or function is missing.
#[allow(non_upper_case_globals)] // name fixed by delayimp.h
#[used]
#[unsafe(no_mangle)]
static __pfnDliFailureHook2: Hook = failure_hook;

unsafe extern "C" {
    /// The exe's own image base, which is its `HMODULE`; provided by the MSVC linker.
    static __ImageBase: u8;
}

/// # Safety
/// `info` points to a valid `DelayLoadInfo` with NUL-terminated names, as the helper passes it.
unsafe extern "system" fn failure_hook(notify: u32, info: *const DelayLoadInfo) -> *const c_void {
    // SAFETY: the caller's contract.
    let info = unsafe { &*info };
    // SAFETY: `sz_dll` is the NUL-terminated DLL name from the import descriptor.
    let dll = unsafe { CStr::from_ptr(info.sz_dll) };
    if !dll.to_bytes().eq_ignore_ascii_case(b"vulkan-1.dll") {
        return null();
    }
    match notify {
        // Any module handle stops the helper from raising; its GetProcAddress on the exe then
        // fails and comes back here as DLI_FAIL_GET_PROC.
        DLI_FAIL_LOAD_LIB => (&raw const __ImageBase).cast(),
        DLI_FAIL_GET_PROC if info.import_by_name != 0 => {
            // SAFETY: with `import_by_name` set, `proc_name` is a NUL-terminated name.
            let name = unsafe { CStr::from_ptr(info.proc_name) };
            if name == c"vkGetInstanceProcAddr" {
                get_instance_proc_addr as *const c_void
            } else {
                null()
            }
        }
        _ => null(),
    }
}

/// # Safety
/// `name` is null or a NUL-terminated string, as Vulkan's `vkGetInstanceProcAddr` takes it.
unsafe extern "system" fn get_instance_proc_addr(
    _instance: *const c_void,
    name: *const c_char,
) -> *const c_void {
    // SAFETY: not null, NUL-terminated by the caller's contract.
    if !name.is_null() && unsafe { CStr::from_ptr(name) } == c"vkEnumerateInstanceVersion" {
        enumerate_instance_version as *const c_void
    } else {
        null()
    }
}

extern "system" fn enumerate_instance_version(_version: *mut u32) -> i32 {
    VK_ERROR_INITIALIZATION_FAILED
}

#[cfg(test)]
mod tests {
    use std::mem::{offset_of, size_of, transmute};
    use std::ptr::null_mut;

    use super::*;

    type Gipa = unsafe extern "system" fn(*const c_void, *const c_char) -> *const c_void;
    type Eiv = unsafe extern "system" fn(*mut u32) -> i32;

    fn hook(notify: u32, dll: &CStr, proc_name: &CStr) -> *const c_void {
        let info = DelayLoadInfo {
            cb: u32::try_from(size_of::<DelayLoadInfo>()).unwrap(),
            pidd: null(),
            ppfn: null_mut(),
            sz_dll: dll.as_ptr(),
            import_by_name: 1,
            proc_name: proc_name.as_ptr(),
            hmod_cur: null_mut(),
            pfn_cur: null(),
            last_error: 0,
        };
        // SAFETY: a valid info with NUL-terminated names.
        unsafe { failure_hook(notify, &raw const info) }
    }

    #[test]
    fn layout_matches_delayimp_h() {
        assert_eq!(size_of::<DelayLoadInfo>(), 72);
        assert_eq!(offset_of!(DelayLoadInfo, sz_dll), 24);
        assert_eq!(offset_of!(DelayLoadInfo, import_by_name), 32);
        assert_eq!(offset_of!(DelayLoadInfo, proc_name), 40);
        assert_eq!(offset_of!(DelayLoadInfo, last_error), 64);
    }

    /// The helper's calls on a PC without `vulkan-1.dll`, then vulkan-hpp's: the dispatcher
    /// init looks up its four global functions, then `enumerateInstanceVersion` must fail.
    #[test]
    fn missing_loader_ends_in_a_vulkan_error_ggml_catches() {
        let gipa = c"vkGetInstanceProcAddr";
        assert!(!hook(DLI_FAIL_LOAD_LIB, c"VULKAN-1.DLL", gipa).is_null());
        let found = hook(DLI_FAIL_GET_PROC, c"vulkan-1.dll", gipa);
        assert!(!found.is_null());
        // SAFETY: the hook returned `get_instance_proc_addr`, which has this signature.
        let gipa: Gipa = unsafe { transmute(found) };
        for name in [
            c"vkCreateInstance",
            c"vkEnumerateInstanceExtensionProperties",
            c"vkEnumerateInstanceLayerProperties",
        ] {
            // SAFETY: a NUL-terminated name.
            assert!(unsafe { gipa(null(), name.as_ptr()) }.is_null());
        }
        // SAFETY: a NUL-terminated name; the result is `enumerate_instance_version`.
        let eiv: Eiv = unsafe { transmute(gipa(null(), c"vkEnumerateInstanceVersion".as_ptr())) };
        let mut version = 0;
        // SAFETY: a valid out pointer.
        let result = unsafe { eiv(&raw mut version) };
        assert_eq!(result, VK_ERROR_INITIALIZATION_FAILED);
    }

    #[test]
    fn other_dlls_and_functions_keep_the_default_failure() {
        let gipa = c"vkGetInstanceProcAddr";
        assert!(hook(DLI_FAIL_LOAD_LIB, c"nvcuda.dll", gipa).is_null());
        assert!(hook(DLI_FAIL_GET_PROC, c"nvcuda.dll", gipa).is_null());
        assert!(hook(DLI_FAIL_GET_PROC, c"vulkan-1.dll", c"vkCmdCopyBuffer").is_null());
        assert!(hook(1, c"vulkan-1.dll", gipa).is_null());
    }
}
