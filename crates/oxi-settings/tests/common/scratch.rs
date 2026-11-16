//! Temporary folders and fake model files for the tests, shared by the unit tests
//! (`src/testing.rs`) and the test binaries (`tests/common`). A folder goes when its test ends;
//! a fake model has a real model's length but takes no disk.

use std::cell::RefCell;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Removes its folder when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

thread_local! {
    // libtest runs each test on its own thread, so these drop when the test ends, pass or panic.
    static HELD: RefCell<Vec<Scratch>> = const { RefCell::new(Vec::new()) };
}

/// An empty folder `name` in the temp dir, removed when the calling thread (the test) ends.
pub fn scratch_dir(name: &str) -> PathBuf {
    // Anything but one plain name would make the guard delete the temp dir itself or a path outside it.
    let mut parts = Path::new(name).components();
    assert!(
        matches!(
            (parts.next(), parts.next()),
            (Some(Component::Normal(_)), None)
        ),
        "scratch_dir needs one plain folder name, got {name:?}"
    );
    let dir = std::env::temp_dir().join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    HELD.with_borrow_mut(|held| held.push(Scratch(dir.clone())));
    dir
}

/// A file of `bytes` that takes no disk: sparse, so a fake 3 GB model costs nothing.
pub fn fake_model(path: &Path, bytes: u64) {
    let file = fs::File::create(path).unwrap();
    // Unix file systems leave the extended length as a hole; NTFS allocates it unless sparse.
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::IO::DeviceIoControl;
        // winioctl.h; windows-sys keeps it behind the Ioctl feature, not worth it for one number.
        const FSCTL_SET_SPARSE: u32 = 0x0009_00C4;
        let mut returned = 0u32;
        // SAFETY: the handle is open while `file` lives; no buffers in or out, no overlapped I/O.
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                FSCTL_SET_SPARSE,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                &raw mut returned,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(
            ok,
            0,
            "cannot make {} sparse: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }
    file.set_len(bytes).unwrap();
}
