//! History retention (ARCHITECTURE sections 7 and 9): the file names of saved dictations, where
//! the folder is, and which entries go when the age or size limit is hit. Pure: the daemon lists
//! the folder, calls [`expired`] and deletes what it names. The app only works inside a folder
//! it made ([`OWNED_DIR`], [`MARKER`]) and only on names that pass the strict checks here.

use std::path::{Path, PathBuf};

use crate::config::History;
use crate::stats::Date;

/// Extension of the saved audio.
pub const WAV: &str = "wav";
/// Extension of the saved text.
pub const TXT: &str = "txt";

/// The subfolder made inside a folder the user chose.
pub const OWNED_DIR: &str = "OxiYapper history";
/// The file that marks a history folder as made by this app; a folder without it is not touched.
pub const MARKER: &str = ".oxiyapper-history";

/// One saved dictation: its `YYYY-MM-DD/HHMMSS-mmm` stem and the bytes of its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub stem: String,
    pub bytes: u64,
}

/// `YYYY-MM-DD/HHMMSS-mmm` for a local day and the milliseconds since its midnight.
pub fn stem(date: Date, ms_of_day: u32) -> String {
    let ms = ms_of_day % 86_400_000;
    let (h, m, s) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60);
    format!("{date}/{h:02}{m:02}{s:02}-{:03}", ms % 1000)
}

/// The day a folder name stands for, when the name is exactly `YYYY-MM-DD`.
pub fn day_dir(name: &str) -> Option<Date> {
    let date = name.parse::<Date>().ok()?;
    (date.to_string() == name).then_some(date)
}

/// Is `name` exactly `HHMMSS-mmm` with a real time of day (the stem of a file in a day folder)?
pub fn is_file_stem(name: &str) -> bool {
    name.len() == 10
        && name.bytes().enumerate().all(|(i, b)| {
            if i == 6 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        })
        && &name[..2] < "24"
        && &name[2..4] < "60"
        && &name[4..6] < "60"
}

/// Is `name` what a killed write left: the `.wav.part` or `.txt.tmp` of a history stem?
pub fn is_leftover(name: &str) -> bool {
    name.strip_suffix(".wav.part")
        .or_else(|| name.strip_suffix(".txt.tmp"))
        .is_some_and(is_file_stem)
}

/// The app's own folder history goes to: [`OWNED_DIR`] inside `[history] folder` when that is an
/// absolute path, else `default` (the data directory's `history/`, which is the app's already).
pub fn folder(cfg: &History, default: &Path) -> PathBuf {
    let set = Path::new(cfg.folder.trim());
    if set.is_absolute() {
        set.join(OWNED_DIR)
    } else {
        default.to_owned()
    }
}

fn entry_day(e: &Entry) -> Option<Date> {
    e.stem.split_once('/').and_then(|(day, _)| day_dir(day))
}

/// The stems to delete: every entry older than `max_days` local days (the last 30 days keep 30
/// day folders, today included), strictly by date, then the oldest ones until the rest fits
/// `max_bytes`. An entry dated after `today` (the clock was set back) is never deleted by age.
/// A limit of 0 is off. The size limit needs no clock.
pub fn expired(entries: &[Entry], today: Date, cfg: &History) -> Vec<String> {
    let mut sorted: Vec<&Entry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.stem.cmp(&b.stem)); // fixed-width names: oldest first
    let cutoff = (cfg.max_days > 0)
        .then(|| today.add_days(-i32::try_from(cfg.max_days).unwrap_or(i32::MAX)));
    // `cutoff` is before `today`, so a future-dated entry is never past it.
    let old = |e: &Entry| cutoff.is_some_and(|c| entry_day(e).is_some_and(|d| d <= c));
    // Age frees size first: the cap counts only what the age limit leaves.
    let mut total: u64 = sorted.iter().filter(|e| !old(e)).map(|e| e.bytes).sum();
    let mut gone = Vec::new();
    for e in sorted {
        if old(e) {
            gone.push(e.stem.clone());
        } else if cfg.max_bytes > 0 && total > cfg.max_bytes {
            total -= e.bytes;
            gone.push(e.stem.clone());
        }
    }
    gone
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> Date {
        Date::from_ymd(y, m, d).unwrap()
    }

    fn entry(date: Date, time: &str, bytes: u64) -> Entry {
        Entry {
            stem: format!("{date}/{time}"),
            bytes,
        }
    }

    fn cfg(max_days: u32, max_bytes: u64) -> History {
        History {
            max_bytes,
            max_days,
            ..History::default()
        }
    }

    #[test]
    fn stem_is_local_date_and_time_of_day() {
        assert_eq!(
            stem(day(2026, 11, 8), 13 * 3_600_000 + 45 * 60_000 + 1_123),
            "2026-11-08/134501-123"
        );
        assert_eq!(stem(day(2026, 1, 2), 0), "2026-01-02/000000-000");
    }

    #[test]
    fn only_exact_history_names_pass() {
        assert!(day_dir("2026-11-08").is_some());
        for bad in ["2026-1-8", "2026-11-08x", "2026-13-01", "notes", ""] {
            assert!(day_dir(bad).is_none(), "{bad}");
        }
        assert!(is_file_stem("134501-123"));
        assert!(is_file_stem("235959-999"));
        for bad in [
            "134501-12",
            "13450a-123",
            "134501_123",
            "readme",
            "",
            "240000-000",
            "136000-000",
            "130060-000",
        ] {
            assert!(!is_file_stem(bad), "{bad}");
        }
    }

    #[test]
    fn only_leftovers_of_a_history_stem_count() {
        for good in ["134501-123.wav.part", "134501-123.txt.tmp"] {
            assert!(is_leftover(good), "{good}");
        }
        for bad in [
            "134501-123.wav",
            "134501-123.txt.part",
            "134501-123.wav.tmp",
            "song.wav.part",
            "240000-000.wav.part",
            "notes.txt.tmp",
            ".wav.part",
        ] {
            assert!(!is_leftover(bad), "{bad}");
        }
    }

    #[test]
    fn folder_is_the_owned_subfolder_or_the_default() {
        let default = Path::new("data/history");
        assert_eq!(folder(&History::default(), default), default);
        let rel = History {
            folder: "rel/dir".into(),
            ..History::default()
        };
        assert_eq!(folder(&rel, default), default);
        let abs = std::env::temp_dir();
        let set = History {
            folder: abs.to_string_lossy().into_owned(),
            ..History::default()
        };
        assert_eq!(folder(&set, default), abs.join(OWNED_DIR));
    }

    #[test]
    fn nothing_to_delete_inside_both_limits() {
        let today = day(2026, 11, 8);
        let entries = [
            entry(day(2026, 10, 20), "100000-000", 10),
            entry(today, "100000-000", 10),
        ];
        assert_eq!(
            expired(&entries, today, &cfg(30, 100)),
            Vec::<String>::new()
        );
        assert_eq!(expired(&[], today, &cfg(30, 100)), Vec::<String>::new());
    }

    #[test]
    fn age_removes_days_older_than_the_last_n() {
        let today = day(2026, 11, 8);
        // The last 3 days are 11-06, 11-07 and 11-08.
        let entries = [
            entry(day(2026, 11, 5), "100000-000", 1),
            entry(day(2026, 11, 6), "100000-000", 1),
            entry(today, "100000-000", 1),
        ];
        assert_eq!(
            expired(&entries, today, &cfg(3, 0)),
            ["2026-11-05/100000-000"]
        );
    }

    #[test]
    fn size_removes_the_oldest_until_it_fits() {
        let today = day(2026, 11, 8);
        let entries = [
            entry(today, "120000-000", 40), // given out of order on purpose
            entry(today, "100000-000", 40),
            entry(today, "110000-000", 40),
        ];
        assert_eq!(
            expired(&entries, today, &cfg(0, 100)),
            ["2026-11-08/100000-000"]
        );
        assert_eq!(expired(&entries, today, &cfg(0, 119)).len(), 1);
        assert_eq!(expired(&entries, today, &cfg(0, 79)).len(), 2);
    }

    #[test]
    fn both_limits_apply_and_the_age_frees_size_first() {
        let today = day(2026, 11, 8);
        let entries = [
            entry(day(2026, 10, 8), "100000-000", 90), // too old
            entry(day(2026, 11, 7), "100000-000", 60),
            entry(today, "100000-000", 60),
        ];
        // Age alone leaves 120 bytes, over the cap of 100: the older recent entry goes too.
        assert_eq!(
            expired(&entries, today, &cfg(30, 100)),
            ["2026-10-08/100000-000", "2026-11-07/100000-000"]
        );
        // Without the age, 210 bytes drop to 120 and then to 60: two oldest.
        assert_eq!(expired(&entries, today, &cfg(0, 100)).len(), 2);
    }

    /// SPEC History (owner 2026-11-10): age retention is strict by date; only entries dated after
    /// today (the clock was set back) are kept from it.
    #[test]
    fn the_age_limit_is_strict_by_date_and_skips_future_entries() {
        let now = day(2026, 11, 8);
        let far = day(2099, 1, 1);
        let table = [
            // today, entry days, how many the age limit of 30 days deletes
            (
                "dictation 60 days apart",
                now,
                vec![day(2026, 8, 1), day(2026, 10, 1)],
                2,
            ),
            (
                "gap between two entries",
                now,
                vec![day(2026, 8, 1), day(2026, 10, 20), now],
                1,
            ),
            ("clock jumped forward", far, vec![day(2026, 11, 7), now], 2),
            (
                "clock set back: future entries stay",
                now,
                vec![day(2026, 10, 1), day(2026, 11, 9), far],
                1,
            ),
            (
                "normal daily use",
                now,
                (1..=10).map(|d| day(2026, 10, d)).chain([now]).collect(),
                9,
            ),
            (
                "the 31st day back goes, the 30th stays",
                now,
                vec![day(2026, 10, 9), day(2026, 10, 10)],
                1,
            ),
        ];
        for (name, today, days, gone) in table {
            let entries: Vec<_> = days.iter().map(|&d| entry(d, "100000-000", 1)).collect();
            assert_eq!(expired(&entries, today, &cfg(30, 0)).len(), gone, "{name}");
        }
    }

    #[test]
    fn the_size_limit_works_with_any_clock() {
        let entries = [
            entry(day(2026, 11, 7), "100000-000", 60),
            entry(day(2026, 11, 8), "100000-000", 60),
        ];
        assert_eq!(
            expired(&entries, day(2000, 1, 1), &cfg(30, 100)),
            ["2026-11-07/100000-000"]
        );
    }

    #[test]
    fn limit_zero_keeps_everything() {
        let today = day(2026, 11, 8);
        let entries = [entry(day(2020, 1, 1), "100000-000", u64::MAX / 4)];
        assert_eq!(expired(&entries, today, &cfg(0, 0)), Vec::<String>::new());
    }
}
