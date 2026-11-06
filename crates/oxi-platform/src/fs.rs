//! Atomic file writes (ARCHITECTURE sections 6 and 7): write beside the target, sync, rename.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// `<path>.tmp`, the sibling a config-style file is written to before the rename.
fn tmp_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".tmp");
    PathBuf::from(p)
}

/// Create `tmp`, let `write` fill it, `sync_all`, rename it over `path`. On any error `tmp` is
/// removed and an existing `path` is untouched. Shared by `write_atomic` and the wav writer
/// (which uses the `.part` name).
pub fn write_via<E: From<io::Error>>(
    tmp: &Path,
    path: &Path,
    write: impl FnOnce(&mut File) -> Result<(), E>,
) -> Result<(), E> {
    let mut file = File::create(tmp)?;
    let result = write(&mut file)
        .and_then(|()| Ok(file.sync_all()?))
        .and_then(|()| Ok(fs::rename(tmp, path)?));
    if result.is_err() {
        drop(file);
        let _ = fs::remove_file(tmp);
    }
    result
}

/// Replace `path` with `bytes` through `<path>.tmp`: a crash leaves the old or the new file, never half.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_via(&tmp_path(path), path, |f| f.write_all(bytes))
}
