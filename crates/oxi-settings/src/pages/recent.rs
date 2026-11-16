//! The newest history entries, for the suggestions from the person's own dictations (hold item
//! 41): a walk over the day folders, newest first, run on a worker thread by the pages; and when
//! such a read is old enough to be done again.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use oxi_core::history;

/// A read of the history older than this is done again when its page opens (ia-map 3.4: "a cache
/// older than a day refreshes itself"; no timer, no polling).
pub(super) const REFRESH_MS: u64 = 24 * 60 * 60 * 1000;

/// A read made at `at` (ms on any clock), seen at `now`: old enough to read again.
pub(super) fn stale(at: u64, now: u64) -> bool {
    now.saturating_sub(at) >= REFRESH_MS
}

/// The most dismissals kept in the file (the newest stay).
const MAX_DISMISSED: usize = 500;
/// Where the suggestions given "Not now" or "Not this one" are kept, in the data folder, for the
/// Audio, Model and Text pages: one hashed key per line (`oxi_core::learn::dismiss_key`), no
/// dictated text.
pub(super) const DISMISSED_FILE: &str = "learn-dismissed.txt";

/// The dismissed keys in the file text `file`.
pub(super) fn parse_dismissed(file: &str) -> HashSet<String> {
    file.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The dismissed keys kept in the data folder `data_dir`; none when the file is missing.
pub(super) fn dismissed(data_dir: &Path) -> HashSet<String> {
    fs::read_to_string(data_dir.join(DISMISSED_FILE))
        .map(|f| parse_dismissed(&f))
        .unwrap_or_default()
}

/// Adds `key` to the file in `data_dir`. Failing to write only means the dismissal lasts until
/// the window closes.
pub(super) fn add_dismissed(data_dir: &Path, key: &str) {
    let path = data_dir.join(DISMISSED_FILE);
    let mut lines: Vec<String> = fs::read_to_string(&path)
        .map(|f| f.lines().map(str::to_owned).collect())
        .unwrap_or_default();
    lines.push(key.to_owned());
    let from = lines.len().saturating_sub(MAX_DISMISSED);
    let _ = fs::create_dir_all(data_dir);
    let _ = fs::write(
        &path,
        lines[from..].join(
            "
",
        ) + "
",
    );
}

/// Calls `each` with the folder and stem of the entries in the history folder `dir`, newest first,
/// until it returns false or `max` entries were seen. Folders and files that are not the app's
/// are skipped; a missing folder has none.
pub(super) fn newest(dir: &Path, max: usize, mut each: impl FnMut(&Path, &str) -> bool) {
    let names = |dir: &Path| -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        // Day folders and `HHMMSS-mmm` stems sort by time as text.
        v.sort_unstable_by(|a, b| b.cmp(a));
        v
    };
    let mut seen = 0;
    for day in names(dir)
        .into_iter()
        .filter(|d| history::day_dir(d).is_some())
    {
        let day = dir.join(day);
        for file in names(&day) {
            let Some(stem) = file.strip_suffix(".json") else {
                continue;
            };
            if !history::is_file_stem(stem) {
                continue;
            }
            if seen >= max {
                return;
            }
            seen += 1;
            if !each(&day, stem) {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_is_stale_after_a_day() {
        assert!(!stale(1_000, 1_000 + REFRESH_MS - 1));
        assert!(stale(1_000, 1_000 + REFRESH_MS));
        assert!(!stale(5_000, 1_000), "a clock that went back is not stale");
    }
}
