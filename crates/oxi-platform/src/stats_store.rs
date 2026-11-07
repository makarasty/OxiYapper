//! The usage stats file (`oxi_core::stats::FILE` in the data dir): the local date, reading,
//! resetting a corrupt file, atomic writes. Counts only; what it holds is in `oxi_core::stats`.

use std::fs;
use std::io;
use std::path::Path;

use oxi_core::stats::{BAD_FILE, Date, Stats};

use crate::fs::write_atomic;

/// Today in the user's local time zone.
#[cfg(windows)]
pub fn today() -> Date {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut t = SYSTEMTIME::default();
    // SAFETY: `t` is a valid SYSTEMTIME that GetLocalTime only writes.
    unsafe { GetLocalTime(&raw mut t) };
    Date::from_ymd(i32::from(t.wYear), u32::from(t.wMonth), u32::from(t.wDay)).unwrap_or_default()
}

/// Today in UTC.
/// shortcut: UTC, not local time; switch to the OS local date when the macOS/Linux daemon lands.
#[cfg(not(windows))]
pub fn today() -> Date {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    Date::from_epoch_days(i32::try_from(secs / 86_400).unwrap_or(0))
}

/// The stats at `path`; a missing file is empty stats. A corrupt file is an `InvalidData` error.
pub fn read(path: &Path) -> io::Result<Stats> {
    match fs::read(path) {
        Ok(bytes) => {
            Stats::parse(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Stats::default()),
        Err(e) => Err(e),
    }
}

/// [`read`] for the writer: a corrupt file is renamed to `stats.bad` beside it (replacing an
/// older one) and counting starts fresh. Other errors are returned, so a file that is only
/// locked for a moment is never replaced.
pub fn load(path: &Path) -> io::Result<Stats> {
    match read(path) {
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            fs::rename(path, path.with_file_name(BAD_FILE))?;
            tracing::warn!("stats file unreadable, kept as {BAD_FILE}; counting starts fresh");
            Ok(Stats::default())
        }
        other => other,
    }
}

/// Writes `stats` to `path` atomically.
pub fn save(path: &Path, stats: &Stats) -> io::Result<()> {
    let json = stats.to_json().map_err(io::Error::other)?;
    write_atomic(path, &json)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use oxi_core::stats::FILE;
    use oxi_core::types::Millis;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oxi-stats-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_is_empty_and_saved_stats_read_back() {
        let dir = scratch("roundtrip");
        let path = dir.join(FILE);
        let mut stats = load(&path).unwrap();
        assert_eq!(stats, Stats::default());
        stats.record(today(), 4, Millis(2_000));
        save(&path, &stats).unwrap();
        assert_eq!(read(&path).unwrap(), stats);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_is_kept_as_bad_and_counting_starts_fresh() {
        let dir = scratch("corrupt");
        let path = dir.join(FILE);
        fs::write(&path, b"{half").unwrap();
        assert_eq!(read(&path).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(load(&path).unwrap(), Stats::default());
        assert_eq!(fs::read(dir.join(BAD_FILE)).unwrap(), b"{half");
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn today_is_a_plausible_date() {
        let (y, _, _) = today().ymd();
        assert!((2026..2100).contains(&y), "{y}");
    }
}
