//! Atomic file writes (ARCHITECTURE sections 6 and 7): write beside the target, sync, rename.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use oxi_core::models::{PART_KEEP, PartialDownload, part_expired};
use oxi_core::paths::PART_SUFFIX;
use oxi_core::stats::Date;

/// `<path>.tmp`, the sibling a config-style file is written to before the rename.
fn tmp_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".tmp");
    PathBuf::from(p)
}

/// Create `tmp`, let `write` fill it, `sync_all`, rename it over `path`, sync the directory (Unix). On any error `tmp` is
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
        .and_then(|()| Ok(fs::rename(tmp, path)?))
        .and_then(|()| Ok(sync_parent(path)?));
    if result.is_err() {
        drop(file);
        let _ = fs::remove_file(tmp);
    }
    result
}

/// The local day and the milliseconds since its midnight, for history file names.
#[cfg(windows)]
pub fn local_now() -> (Date, u32) {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut t = SYSTEMTIME::default();
    // SAFETY: `t` is a valid SYSTEMTIME that GetLocalTime only writes.
    unsafe { GetLocalTime(&raw mut t) };
    let date = Date::from_ymd(i32::from(t.wYear), u32::from(t.wMonth), u32::from(t.wDay))
        .unwrap_or_default();
    let secs = u32::from(t.wHour) * 3600 + u32::from(t.wMinute) * 60 + u32::from(t.wSecond);
    (date, secs * 1000 + u32::from(t.wMilliseconds))
}

/// The UTC day and the milliseconds since its midnight.
/// shortcut: UTC, not local time; switch to the OS local time with `stats_store::today`.
#[cfg(not(windows))]
pub fn local_now() -> (Date, u32) {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let days = i32::try_from(ms / 86_400_000).unwrap_or(0);
    (
        Date::from_epoch_days(days),
        u32::try_from(ms % 86_400_000).unwrap_or(0),
    )
}

/// Make the rename itself durable.
#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    File::open(dir.unwrap_or_else(|| Path::new(".")))?.sync_all()
}

/// Windows has no directory handle to sync.
#[cfg(not(unix))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "same signature as the Unix version"
)]
fn sync_parent(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Replace `path` with `bytes` through `<path>.tmp`: a crash leaves the old or the new file, never half.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_via(&tmp_path(path), path, |f| f.write_all(bytes))
}

/// The `*.part` files in `models_dir` with size and removal date; a missing folder is empty.
/// An entry that cannot be read is logged and skipped, the rest are still listed.
pub fn list_partials(models_dir: &Path) -> io::Result<Vec<PartialDownload>> {
    let entries = match fs::read_dir(models_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "cannot read a models folder entry");
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(file_name) = name.strip_suffix(PART_SUFFIX) else {
            continue;
        };
        match entry
            .metadata()
            .and_then(|m| Ok((m.is_file(), m.len(), m.modified()?)))
        {
            Ok((true, size, modified)) => out.push(PartialDownload {
                file_name: file_name.to_owned(),
                size,
                remove_at: modified + PART_KEEP,
            }),
            Ok((false, ..)) => {}
            Err(e) => {
                tracing::warn!(model = file_name, error = %e, "cannot stat a partial download");
            }
        }
    }
    out.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    Ok(out)
}

/// Startup sweep: delete the model `.part` files older than [`PART_KEEP`]. Returns the count.
/// A file that cannot be removed (locked) is logged and kept; the others are still swept.
pub fn sweep_partials(models_dir: &Path, now: SystemTime) -> io::Result<usize> {
    sweep_with(models_dir, now, |p| fs::remove_file(p))
}

fn sweep_with(
    models_dir: &Path,
    now: SystemTime,
    remove: impl Fn(&Path) -> io::Result<()>,
) -> io::Result<usize> {
    let mut removed = 0;
    for part in list_partials(models_dir)? {
        if !part_expired(part.remove_at - PART_KEEP, now) {
            continue;
        }
        match remove(&models_dir.join(format!("{}{PART_SUFFIX}", part.file_name))) {
            Ok(()) => removed += 1,
            Err(e) => {
                tracing::warn!(model = %part.file_name, error = %e, "cannot remove a stale partial download");
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch::scratch_dir;
    use std::time::Duration;

    #[test]
    fn a_file_that_cannot_be_removed_does_not_stop_the_sweep() {
        let dir = scratch_dir(&format!("oxi-sweep-stuck-{}", std::process::id()));
        let now = SystemTime::now();
        for name in ["a.bin.part", "b.bin.part"] {
            let f = File::create(dir.join(name)).unwrap();
            f.set_modified(now - PART_KEEP - Duration::from_secs(3600))
                .unwrap();
        }
        // The injected remover fails for a.bin.part, as a locked file would.
        let removed = sweep_with(&dir, now, |p| {
            if p.ends_with("a.bin.part") {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            } else {
                fs::remove_file(p)
            }
        })
        .unwrap();
        assert_eq!(removed, 1);
        assert!(dir.join("a.bin.part").exists());
        assert!(!dir.join("b.bin.part").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
