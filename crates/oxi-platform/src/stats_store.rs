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

/// [`read`] for the writer: a corrupt file is renamed to `stats.bad` beside it (`stats.bad-2`,
/// `-3`, ... if one exists: an older copy is never replaced) and counting starts fresh. A file
/// of a newer build that does not parse is left alone and counted in memory only
/// ([`Stats::writable`] is false). Other errors are returned, so a file that is only locked
/// for a moment is never replaced.
pub fn load(path: &Path) -> io::Result<Stats> {
    match read(path) {
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            if let Some(version) = fs::read(path).ok().and_then(|b| Stats::newer_version(&b)) {
                tracing::warn!("stats file is of a newer version; left as is, counted in memory");
                return Ok(Stats {
                    version,
                    ..Stats::default()
                });
            }
            let mut bad = path.with_file_name(BAD_FILE);
            let mut n = 1_u32;
            while bad.exists() {
                n += 1;
                bad = path.with_file_name(format!("{BAD_FILE}-{n}"));
            }
            fs::rename(path, &bad)?;
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
    fn a_second_corrupt_file_keeps_the_first_bad_copy() {
        let dir = scratch("corrupt-twice");
        let path = dir.join(FILE);
        fs::write(&path, b"{one").unwrap();
        load(&path).unwrap();
        fs::write(&path, b"{two").unwrap();
        load(&path).unwrap();
        assert_eq!(fs::read(dir.join(BAD_FILE)).unwrap(), b"{one");
        assert_eq!(
            fs::read(dir.join(format!("{BAD_FILE}-2"))).unwrap(),
            b"{two"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_newer_unparsable_file_is_never_renamed_or_written() {
        let dir = scratch("newer");
        let path = dir.join(FILE);
        let json = format!(
            r#"{{"version":{},"days":"new shape"}}"#,
            oxi_core::stats::VERSION + 1
        );
        fs::write(&path, &json).unwrap();
        let stats = load(&path).unwrap();
        assert!(!stats.writable());
        assert_eq!(fs::read_to_string(&path).unwrap(), json);
        assert!(!dir.join(BAD_FILE).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn today_is_a_plausible_date() {
        let (y, _, _) = today().ymd();
        assert!((2026..2100).contains(&y), "{y}");
    }
}
