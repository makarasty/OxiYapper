//! The test helpers clean up after themselves: a test's folder goes when the test ends, and a
//! fake model of a real model's size takes no real disk (14,000 leftover folders and 357 GB of
//! fakes filled drive C on 2026-11-10).

mod common;

use std::fs;
use std::path::Path;
use std::thread;

/// The real disk `path` takes: allocated clusters, not its length.
#[cfg(windows)]
fn on_disk(path: &Path) -> u64 {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut high = 0u32;
    // SAFETY: `wide` is a NUL-terminated path that outlives the call; `high` is a valid out slot.
    let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &raw mut high) };
    assert_ne!(low, u32::MAX, "{}", std::io::Error::last_os_error());
    (u64::from(high) << 32) | u64::from(low)
}

#[cfg(unix)]
fn on_disk(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).unwrap().blocks() * 512
}

#[test]
fn a_test_folder_goes_when_its_test_ends() {
    // libtest runs each test on its own thread: the folder lives as long as that thread.
    let dir = thread::spawn(|| {
        let dir = common::layout("hygiene", Some("version = 1\n")).config_dir;
        assert!(dir.join("config.toml").exists());
        dir
    })
    .join()
    .unwrap();
    assert!(!dir.exists(), "{} left behind", dir.display());
}

#[test]
fn a_fake_model_takes_no_disk() {
    let dir = common::layout("hygiene-fake", None).models_dir();
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("ggml-large-v3.bin");
    common::fake_model(&file, 3_095_033_483);
    // The page reads the length, so that stays the model's.
    assert_eq!(fs::metadata(&file).unwrap().len(), 3_095_033_483);
    let used = on_disk(&file);
    assert!(used < 16 << 20, "the fake takes {used} bytes of disk");
}

#[test]
fn scratch_dir_refuses_anything_but_one_plain_name() {
    let temp = std::env::temp_dir();
    let absolute = temp.join("oxi-hygiene-abs");
    for bad in ["", "..", "a/b", ".", absolute.to_str().unwrap()] {
        let name = bad.to_owned();
        let refused = thread::spawn(move || common::scratch_dir(&name)).join().is_err();
        assert!(refused, "scratch_dir accepted {bad:?}");
    }
    assert!(temp.exists(), "the temp folder is gone");
    assert!(!absolute.exists(), "{} was made", absolute.display());
}
