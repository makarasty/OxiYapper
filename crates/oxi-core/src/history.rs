//! History retention (ARCHITECTURE sections 7 and 9): the file names of saved dictations, where
//! the folder is, and which entries go when the age or size limit is hit. Pure: the daemon lists
//! the folder, calls [`expired`] and deletes what it names. The app only works inside a folder
//! it made ([`OWNED_DIR`], [`MARKER`]) and only on names that pass the strict checks here.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::History;
use crate::stats::Date;
use crate::tuning::SettingsSet;

/// Extension of the saved audio.
pub const WAV: &str = "wav";
/// Extension of the saved text.
pub const TXT: &str = "txt";
/// Extension of the meta file (see [`Meta`] and `docs/HISTORY-FORMAT.md`).
pub const JSON: &str = "json";
/// The files of one entry, in the order they are written.
pub const EXTENSIONS: [&str; 3] = [WAV, TXT, JSON];

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

/// Is `name` what a killed write left: the `.wav.part`, `.txt.tmp` or `.json.tmp` of a history stem?
pub fn is_leftover(name: &str) -> bool {
    name.strip_suffix(".wav.part")
        .or_else(|| name.strip_suffix(".txt.tmp"))
        .or_else(|| name.strip_suffix(".json.tmp"))
        .is_some_and(is_file_stem)
}

/// Is `id` exactly the `YYYY-MM-DD/HHMMSS-mmm` stem of an entry? The only form the daemon accepts
/// from a caller, so an id can never name a path outside the folder.
pub fn is_id(id: &str) -> bool {
    id.split_once('/')
        .is_some_and(|(day, file)| day_dir(day).is_some() && is_file_stem(file))
}

/// `2026-11-08T13:00:00.123`: the local time an entry's stem stands for; `None` for a bad id.
pub fn started(id: &str) -> Option<String> {
    let (day, t) = id.split_once('/').filter(|_| is_id(id))?;
    Some(format!(
        "{day}T{}:{}:{}.{}",
        &t[..2],
        &t[2..4],
        &t[4..6],
        &t[7..]
    ))
}

/// How an entry ended (`status` in the meta file).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Transcribed and delivered.
    Done,
    /// Cancelled after speech was heard; the audio was kept, there is no text.
    Cancelled,
    /// The daemon died while recording; the audio was recovered at the next start.
    Interrupted,
    /// Recorded, but no transcript came out of it.
    Failed,
}

/// The meta file `<stem>.json` of an entry. The layout is a stable contract (R3.3): fields are
/// only ever added, and a reader must ignore the ones it does not know.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    /// Layout version of this file, [`Meta::VERSION`].
    pub version: u32,
    /// Local start of the recording, `YYYY-MM-DDTHH:MM:SS.mmm` (no zone).
    pub started: String,
    pub duration_ms: u64,
    pub status: Status,
    /// Registry id of the model that made the text, `custom` for a custom file; empty when unknown.
    pub model: String,
    /// `auto` or a language code; empty when unknown.
    pub language: String,
    /// The audio settings and model of the run that made the text; `None` when unknown.
    pub settings: Option<SettingsSet>,
    /// The other version of the text: what it was before the last transcribe-again or edit, or
    /// what a restore put aside. Absent until a text was replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<Previous>,
}

/// The text an entry had before it was replaced, with the run that made it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Previous {
    pub text: String,
    pub model: String,
    pub language: String,
    pub settings: Option<SettingsSet>,
    /// The kept text is the newer version: the current one is an old text a Restore brought back.
    #[serde(default, skip_serializing_if = "is_false")]
    pub newer: bool,
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "the `skip_serializing_if` signature"
)]
fn is_false(b: &bool) -> bool {
    !*b
}

impl Meta {
    pub const VERSION: u32 = 1;

    /// This entry's current text and run as the "other version" kept beside a new one.
    #[must_use]
    pub fn as_previous(&self, text: &str) -> Previous {
        Previous {
            text: text.to_owned(),
            model: self.model.clone(),
            language: self.language.clone(),
            settings: self.settings.clone(),
            newer: false,
        }
    }

    /// The entry after Restore (or Use new): the kept version becomes the current one and the
    /// current one is kept in its place, so the same call undoes it. `text` is the current text;
    /// the answer is the new meta and the new text. `None` when no other version is kept.
    #[must_use]
    pub fn swapped(&self, text: &str) -> Option<(Self, String)> {
        let kept = self.previous.as_ref()?;
        let meta = Self {
            status: Status::Done,
            model: kept.model.clone(),
            language: kept.language.clone(),
            settings: kept.settings.clone(),
            previous: Some(Previous {
                newer: !kept.newer,
                ..self.as_previous(text)
            }),
            ..self.clone()
        };
        Some((meta, kept.text.clone()))
    }

    /// # Errors
    /// Never in practice: every field serialises.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// # Errors
    /// The text is not a meta file of this layout.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

/// One piece of a word-by-word comparison of two texts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffPart {
    pub kind: DiffKind,
    /// The words with the spaces that follow them, as they stand in their text.
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    /// In both texts.
    Same,
    /// Only in the old text.
    Removed,
    /// Only in the new text.
    Added,
}

/// Most cells of the comparison table; above it the middle of two texts is shown as one removed
/// and one added run.
const DIFF_CELLS: usize = 4_000_000;

/// A word-by-word comparison of `old` and `new` (longest common subsequence of their words, exact
/// match including punctuation). The `Same` and `Added` parts joined give `new`; `Same` and
/// `Removed` give `old` (up to the white space between words). A removed run comes before the
/// added run that replaces it.
///
/// shortcut: two long texts that differ in the middle by more than `DIFF_CELLS` word pairs show
/// the middle as one removed and one added run; use a linear-space diff if that ever shows.
#[must_use]
pub fn word_diff(old: &str, new: &str) -> Vec<DiffPart> {
    let (old_t, new_t) = (tokens(old), tokens(new));
    let same = |left: &str, right: &str| left.trim() == right.trim();
    let head = old_t
        .iter()
        .zip(&new_t)
        .take_while(|(left, right)| same(left, right))
        .count();
    let tail = old_t[head..]
        .iter()
        .rev()
        .zip(new_t[head..].iter().rev())
        .take_while(|(left, right)| same(left, right))
        .count();
    let (old_mid, new_mid) = (
        &old_t[head..old_t.len() - tail],
        &new_t[head..new_t.len() - tail],
    );
    let mut out: Vec<DiffPart> = Vec::new();
    let mut push = |kind: DiffKind, token: &str| match out.last_mut() {
        Some(part) if part.kind == kind => part.text.push_str(token),
        _ => out.push(DiffPart {
            kind,
            text: token.to_owned(),
        }),
    };
    for token in &new_t[..head] {
        push(DiffKind::Same, token);
    }
    if (old_mid.len() + 1).saturating_mul(new_mid.len() + 1) > DIFF_CELLS {
        for token in old_mid {
            push(DiffKind::Removed, token);
        }
        for token in new_mid {
            push(DiffKind::Added, token);
        }
    } else {
        // `lcs[i * stride + j]`: common words of `old_mid[i..]` and `new_mid[j..]`.
        let stride = new_mid.len() + 1;
        let mut lcs = vec![0_u32; (old_mid.len() + 1) * stride];
        for i in (0..old_mid.len()).rev() {
            for j in (0..new_mid.len()).rev() {
                lcs[i * stride + j] = if same(old_mid[i], new_mid[j]) {
                    lcs[(i + 1) * stride + j + 1] + 1
                } else {
                    lcs[(i + 1) * stride + j].max(lcs[i * stride + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < old_mid.len() || j < new_mid.len() {
            if i < old_mid.len() && j < new_mid.len() && same(old_mid[i], new_mid[j]) {
                push(DiffKind::Same, new_mid[j]);
                (i, j) = (i + 1, j + 1);
            } else if i < old_mid.len()
                && (j == new_mid.len() || lcs[(i + 1) * stride + j] >= lcs[i * stride + j + 1])
            {
                push(DiffKind::Removed, old_mid[i]);
                i += 1;
            } else {
                push(DiffKind::Added, new_mid[j]);
                j += 1;
            }
        }
    }
    for token in &new_t[new_t.len() - tail..] {
        push(DiffKind::Same, token);
    }
    out
}

/// The words of `text`, each with the white space after it; leading white space goes with the
/// first word.
fn tokens(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let (mut in_space, mut seen_word) = (false, false);
    for (at, c) in text.char_indices() {
        if c.is_whitespace() {
            in_space = true;
        } else {
            if in_space && seen_word {
                out.push(&text[start..at]);
                start = at;
            }
            (in_space, seen_word) = (false, true);
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// Bytes of the canonical 16-bit mono PCM wav header the spool writes.
pub const WAV_HEADER: usize = 44;

/// A spool file with its header made true to the bytes on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repaired {
    pub header: [u8; WAV_HEADER],
    /// Whole bytes of audio the file keeps; a half sample at the end is cut.
    pub data_bytes: u64,
    pub sample_rate: u32,
}

/// The header of a recording killed mid-write: the sizes in it are those of the last flush, so
/// they are set again from the real `file_len`. `None` when `header` is not the canonical
/// 16-bit mono PCM header (then the file is not ours to repair).
pub fn repaired_header(header: &[u8], file_len: u64) -> Option<Repaired> {
    let header: &[u8; WAV_HEADER] = header.get(..WAV_HEADER)?.try_into().ok()?;
    let word = |at: usize| {
        u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
    };
    let half = |at: usize| u16::from_le_bytes([header[at], header[at + 1]]);
    let canonical = &header[..4] == b"RIFF"
        && &header[8..16] == b"WAVEfmt "
        && word(16) == 16
        && half(20) == 1 // PCM
        && half(22) == 1 // mono
        && half(34) == 16
        && &header[36..40] == b"data"
        && word(24) > 0;
    if !canonical {
        return None;
    }
    let size =
        u32::try_from((file_len.checked_sub(WAV_HEADER as u64)?).min(u64::from(u32::MAX - 36)))
            .ok()?
            & !1; // whole samples only
    let mut fixed = *header;
    fixed[4..8].copy_from_slice(&(36 + size).to_le_bytes());
    fixed[40..44].copy_from_slice(&size.to_le_bytes());
    Some(Repaired {
        header: fixed,
        data_bytes: u64::from(size),
        sample_rate: word(24),
    })
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

    fn meta(status: Status) -> Meta {
        Meta {
            version: Meta::VERSION,
            started: "2026-11-08T13:00:00.123".into(),
            duration_ms: 4_500,
            status,
            model: "small".into(),
            language: "ru".into(),
            settings: Some(SettingsSet {
                normalize: true,
                denoise: false,
                trim: true,
                model: "small".into(),
            }),
            previous: None,
        }
    }

    #[test]
    fn meta_round_trips_and_names_the_status_in_lowercase() {
        for status in [
            Status::Done,
            Status::Cancelled,
            Status::Interrupted,
            Status::Failed,
        ] {
            let m = meta(status);
            assert_eq!(Meta::from_json(&m.to_json().unwrap()).unwrap(), m);
        }
        let json = meta(Status::Interrupted).to_json().unwrap();
        assert!(json.contains("\"status\": \"interrupted\""), "{json}");
        let none = Meta {
            settings: None,
            ..meta(Status::Done)
        };
        assert_eq!(Meta::from_json(&none.to_json().unwrap()).unwrap(), none);
    }

    #[test]
    fn a_reader_ignores_fields_it_does_not_know() {
        let mut json: serde_json::Value =
            serde_json::from_str(&meta(Status::Done).to_json().unwrap()).unwrap();
        json["added_later"] = serde_json::json!([1, 2]);
        assert_eq!(
            Meta::from_json(&json.to_string()).unwrap(),
            meta(Status::Done)
        );
    }

    #[test]
    fn an_id_is_a_day_and_a_time_and_nothing_else() {
        assert!(is_id("2026-11-08/134501-123"));
        for bad in [
            "",
            "../x/134501-123",
            "2026-11-08/../134501-123",
            "2026-11-08/134501-123/x",
            r"2026-11-08\134501-123",
            "2026-11-08/134501-123.wav",
            "134501-123",
        ] {
            assert!(!is_id(bad), "{bad}");
        }
        assert_eq!(
            started("2026-11-08/134501-123").as_deref(),
            Some("2026-11-08T13:45:01.123")
        );
        assert_eq!(started("nope"), None);
    }

    #[test]
    fn a_meta_leftover_is_a_leftover() {
        assert!(is_leftover("134501-123.json.tmp"));
        assert!(!is_leftover("notes.json.tmp"));
    }

    /// A header as the spool writes it (hound, 16 kHz mono 16-bit), with the sizes of an old flush.
    fn header(rate: u32, flushed_data: u32) -> Vec<u8> {
        let mut h = b"RIFF".to_vec();
        h.extend((36 + flushed_data).to_le_bytes());
        h.extend(b"WAVEfmt ");
        h.extend(16_u32.to_le_bytes());
        h.extend([1, 0, 1, 0]); // PCM, mono
        h.extend(rate.to_le_bytes());
        h.extend((rate * 2).to_le_bytes());
        h.extend([2, 0, 16, 0]);
        h.extend(b"data");
        h.extend(flushed_data.to_le_bytes());
        h
    }

    #[test]
    fn a_truncated_spool_gets_the_sizes_of_the_bytes_it_has() {
        // Flushed at 100 bytes of audio, killed after 1001 bytes (the last sample is half).
        let got = repaired_header(&header(16_000, 100), 44 + 1001).unwrap();
        assert_eq!(got.data_bytes, 1000);
        assert_eq!(got.sample_rate, 16_000);
        assert_eq!(got.header[..], header(16_000, 1000)[..]);
        // Only the header was ever written: nothing to keep, but a valid result.
        assert_eq!(
            repaired_header(&header(48_000, 0), 44).unwrap().data_bytes,
            0
        );
    }

    #[test]
    fn a_file_that_is_not_our_spool_is_not_repaired() {
        let good = header(16_000, 0);
        assert!(repaired_header(&good[..40], 100).is_none(), "short header");
        assert!(
            repaired_header(&good, 20).is_none(),
            "shorter than a header"
        );
        for (at, byte) in [(0, b'X'), (22, 2), (34, 8), (20, 3), (24, 0)] {
            let mut bad = good.clone();
            bad[at] = byte;
            if at == 24 {
                bad[25..28].fill(0);
            }
            assert!(repaired_header(&bad, 100).is_none(), "byte {at}");
        }
    }

    fn joined(parts: &[DiffPart], keep: [DiffKind; 2]) -> String {
        parts
            .iter()
            .filter(|p| keep.contains(&p.kind))
            .map(|p| p.text.as_str())
            .collect()
    }

    fn shape(parts: &[DiffPart]) -> Vec<(DiffKind, &str)> {
        parts.iter().map(|p| (p.kind, p.text.as_str())).collect()
    }

    #[test]
    fn the_diff_of_equal_texts_is_one_unchanged_part() {
        assert_eq!(
            shape(&word_diff("a b  c", "a b  c")),
            [(DiffKind::Same, "a b  c")]
        );
        assert_eq!(word_diff("", ""), []);
    }

    #[test]
    fn the_diff_marks_a_replaced_an_added_and_a_removed_word() {
        use DiffKind::{Added, Removed, Same};
        assert_eq!(
            shape(&word_diff("one two three", "one 2 three")),
            [
                (Same, "one "),
                (Removed, "two "),
                (Added, "2 "),
                (Same, "three")
            ]
        );
        assert_eq!(
            shape(&word_diff("one three", "one two three")),
            [(Same, "one "), (Added, "two "), (Same, "three")]
        );
        assert_eq!(
            shape(&word_diff("one two three", "one three")),
            [(Same, "one "), (Removed, "two "), (Same, "three")]
        );
        assert_eq!(shape(&word_diff("", "new text")), [(Added, "new text")]);
        assert_eq!(shape(&word_diff("gone", "")), [(Removed, "gone")]);
    }

    #[test]
    fn the_diff_parts_give_both_texts_back() {
        let pairs = [
            ("привет мир как дела", "Привет, мир. Как дела"),
            ("a b c d e f", "f e d c b a"),
            ("  lead and trail  ", "lead and  trail"),
            ("x", "x x x"),
        ];
        let squeeze = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
        for (old, new) in pairs {
            let parts = word_diff(old, new);
            let old_back = joined(&parts, [DiffKind::Same, DiffKind::Removed]);
            let new_back = joined(&parts, [DiffKind::Same, DiffKind::Added]);
            assert_eq!(squeeze(&old_back), squeeze(old), "{old:?} -> {new:?}");
            assert_eq!(squeeze(&new_back), squeeze(new), "{old:?} -> {new:?}");
        }
    }

    #[test]
    fn a_huge_difference_falls_back_to_one_removed_and_one_added_run() {
        let words = |p: &str| {
            (0..2500)
                .map(|i| format!("{p}{i}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let parts = word_diff(&words("a"), &words("b"));
        assert_eq!(
            parts.iter().map(|p| p.kind).collect::<Vec<_>>(),
            [DiffKind::Removed, DiffKind::Added]
        );
    }

    #[test]
    fn the_kept_version_swaps_with_the_current_one_and_back() {
        assert!(meta(Status::Done).swapped("text").is_none(), "nothing kept");
        let now = Meta {
            previous: Some(Previous {
                text: "old text".into(),
                model: "turbo".into(),
                language: "en".into(),
                settings: None,
                newer: false,
            }),
            ..meta(Status::Done)
        };
        let (back, text) = now.swapped("new text").unwrap();
        assert_eq!(text, "old text");
        assert_eq!(
            (back.model.as_str(), back.language.as_str()),
            ("turbo", "en")
        );
        assert!(back.settings.is_none());
        let kept = back.previous.as_ref().unwrap();
        assert_eq!(
            (kept.text.as_str(), kept.model.as_str()),
            ("new text", "small")
        );
        assert!(kept.newer, "the old text is shown, the kept one is newer");
        let (again, text) = back.swapped(&text).unwrap();
        assert_eq!(text, "new text");
        assert_eq!(again.model, "small");
        let kept = again.previous.unwrap();
        assert_eq!(kept.text, "old text");
        assert!(!kept.newer, "the new text is shown again");
    }

    #[test]
    fn the_kept_version_is_written_only_when_there_is_one_and_old_files_still_read() {
        let json = meta(Status::Done).to_json().unwrap();
        assert!(!json.contains("previous"), "{json}");
        assert_eq!(Meta::from_json(&json).unwrap().previous, None);
        let with = Meta {
            previous: Some(meta(Status::Done).as_previous("before")),
            ..meta(Status::Done)
        };
        assert_eq!(Meta::from_json(&with.to_json().unwrap()).unwrap(), with);
    }
}
