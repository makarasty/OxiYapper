use std::fs;
use std::path::{Path, PathBuf};

use oxi_platform::fs::write_atomic;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oxi-fs-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn creates_then_replaces_and_leaves_no_tmp() {
    let dir = scratch("replace");
    let path = dir.join("config.toml");
    write_atomic(&path, b"one").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"one");
    write_atomic(&path, b"second, longer").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"second, longer");
    write_atomic(&path, b"x").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"x");
    assert_eq!(names(&dir), ["config.toml"]);
}

#[test]
fn failed_rename_removes_the_tmp_and_keeps_the_target() {
    let dir = scratch("dir-target");
    let path = dir.join("config.toml");
    // A non-empty directory in the way: the rename fails on every OS.
    fs::create_dir_all(path.join("keep")).unwrap();
    assert!(write_atomic(&path, b"new").is_err());
    assert_eq!(names(&dir), ["config.toml"]);
    assert!(path.join("keep").is_dir());
}

#[cfg(windows)]
#[test]
fn read_only_target_keeps_the_old_file() {
    let dir = scratch("readonly-file");
    let path = dir.join("config.toml");
    fs::write(&path, b"old").unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&path, perms.clone()).unwrap();

    let result = write_atomic(&path, b"new");

    // Windows only: clear the flag so the scratch dir can be deleted.
    #[allow(clippy::permissions_set_readonly_false, reason = "Windows only")]
    perms.set_readonly(false);
    fs::set_permissions(&path, perms).unwrap();
    assert!(result.is_err());
    assert_eq!(fs::read(&path).unwrap(), b"old");
    assert_eq!(names(&dir), ["config.toml"]);
}

#[cfg(unix)]
#[test]
fn read_only_dir_keeps_the_old_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("readonly-dir");
    let path = dir.join("config.toml");
    fs::write(&path, b"old").unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();

    // root ignores directory modes: nothing to prove then.
    let blocked = fs::File::create(dir.join("probe")).is_err();
    let result = write_atomic(&path, b"new");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    if blocked {
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert_eq!(names(&dir), ["config.toml"]);
    }
}
