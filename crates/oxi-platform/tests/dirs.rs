use std::fs;
use std::path::PathBuf;

use oxi_core::paths::{PORTABLE_MARKER, portable_layout};
use oxi_platform::dirs::{ensure, layout_for};

#[path = "../src/scratch.rs"]
mod scratch_guard;
use scratch_guard::scratch_dir;

fn scratch(name: &str) -> PathBuf {
    scratch_dir(&format!("oxi-dirs-{name}-{}", std::process::id()))
}

#[test]
fn portable_marker_selects_the_portable_layout() {
    let exe_dir = scratch("portable");
    fs::write(exe_dir.join(PORTABLE_MARKER), b"").unwrap();
    assert_eq!(layout_for(&exe_dir).unwrap(), portable_layout(&exe_dir));
}

#[test]
fn no_marker_uses_the_os_directories() {
    let exe_dir = scratch("installed");
    let layout = layout_for(&exe_dir).unwrap();
    assert_ne!(layout, portable_layout(&exe_dir));
    assert!(!layout.data_dir.starts_with(&exe_dir));
}

#[test]
fn ensure_creates_every_directory_and_is_repeatable() {
    let layout = portable_layout(&scratch("ensure"));
    ensure(&layout).unwrap();
    ensure(&layout).unwrap();
    for dir in [&layout.config_dir, &layout.data_dir, &layout.logs_dir] {
        assert!(dir.is_dir(), "{dir:?}");
    }
}
