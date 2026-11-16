//! What the machine has, for the model recommendation (oxi-core `models::recommend`): system
//! memory, the graphics adapters with their own memory, the free space of a folder's drive. Each
//! call asks the OS once; run them off the UI thread.

use std::path::Path;

use oxi_core::models::Adapter;

/// Total system memory in bytes; `None` when the OS does not say.
#[cfg(windows)]
pub fn ram() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: u32::try_from(size_of::<MEMORYSTATUSEX>()).ok()?,
        ..Default::default()
    };
    // SAFETY: `status` is a valid MEMORYSTATUSEX with `dwLength` set, as the call requires.
    let ok = unsafe { GlobalMemoryStatusEx(&raw mut status) } != 0;
    ok.then_some(status.ullTotalPhys)
}

/// The hardware graphics adapters (DXGI) with their dedicated memory; empty when DXGI fails.
#[cfg(windows)]
pub fn adapters() -> Vec<Adapter> {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIFactory1,
    };
    // SAFETY: a plain factory creation; no COM apartment is needed for DXGI.
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // EnumAdapters1 fails (DXGI_ERROR_NOT_FOUND) past the last adapter.
    // SAFETY: `factory` is a live IDXGIFactory1; each adapter it returns is a live IDXGIAdapter1.
    while let Ok(desc) = unsafe {
        factory
            .EnumAdapters1(u32::try_from(out.len()).unwrap_or(u32::MAX))
            .and_then(|a| a.GetDesc1())
    } {
        let software = desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0.cast_unsigned() != 0;
        let end = desc.Description.iter().position(|&c| c == 0).unwrap_or(128);
        out.push(Adapter {
            name: String::from_utf16_lossy(&desc.Description[..end]),
            // The software rasterizer has no memory of its own to offer a model.
            vram: if software {
                0
            } else {
                u64::try_from(desc.DedicatedVideoMemory).unwrap_or(u64::MAX)
            },
        });
    }
    out
}

/// Free bytes for this user on the drive of `dir`, or of its nearest folder that exists (the
/// models folder is made by the first download).
#[cfg(windows)]
pub fn free_space(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let existing = dir.ancestors().find(|d| d.is_dir())?;
    let wide: Vec<u16> = existing.as_os_str().encode_wide().chain([0]).collect();
    let mut free = 0u64;
    // SAFETY: `wide` is NUL-terminated; the two totals the call may skip are null.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &raw mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } != 0;
    ok.then_some(free)
}

/// shortcut: no OS query wired up here yet, so no recommendation badge; read /proc/meminfo,
/// statvfs and the GPU list when this OS ships.
#[cfg(not(windows))]
pub const fn ram() -> Option<u64> {
    None
}

/// See [`ram`].
#[cfg(not(windows))]
pub const fn adapters() -> Vec<Adapter> {
    Vec::new()
}

/// See [`ram`].
#[cfg(not(windows))]
pub const fn free_space(_dir: &Path) -> Option<u64> {
    None
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn this_machine_reports_memory_and_free_space() {
        assert!(ram().is_some_and(|r| r > 100_000_000));
        let here = std::env::temp_dir();
        assert!(free_space(&here.join("no-such-folder-oxi")).is_some());
        // A machine may have no hardware adapter (a VM); a listed one has a name.
        assert!(adapters().iter().all(|a| !a.name.is_empty()));
    }
}
