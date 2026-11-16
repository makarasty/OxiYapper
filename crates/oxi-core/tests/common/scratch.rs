//! Temporary folders and fake model files for the tests. A folder goes when its test ends, pass or
//! fail; a fake model has a real model's length but takes no disk. Same file in oxi-core,
//! oxi-platform and the binary (src/log/scratch.rs): keep the three in step.
#![allow(dead_code, reason = "each includer uses part of it")]

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

/// A file of `bytes` that takes no disk: sparse, so a fake model costs nothing.
pub fn fake_model(path: &Path, bytes: u64) {
    let file = fs::File::create(path).unwrap();
    // Unix file systems leave the extended length as a hole; NTFS allocates it unless sparse.
    // `fsutil` needs no admin rights for this and keeps the crate free of a `-sys` dev-dependency.
    #[cfg(windows)]
    {
        let status = std::process::Command::new("fsutil")
            .args(["sparse", "setflag"])
            .arg(path)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "cannot make {} sparse",
            path.display()
        );
    }
    file.set_len(bytes).unwrap();
}
