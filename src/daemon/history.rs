//! History worker: saves each dictation's audio and text to the app's own history folder and
//! applies the retention limits (`oxi_core::history`). Nothing outside a folder holding the
//! marker is listed or deleted. A failed write comes back as a warning and never reaches the
//! paste. The text is written to the files and nowhere else: no log line, no error
//! string carries it.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::thread::{self, JoinHandle};

use oxi_core::config::History;
use oxi_core::history::{self, Entry, TXT, WAV};
use oxi_core::stats::Date;
use oxi_platform::fs::{local_now, write_atomic};
use oxi_platform::wav::{WavError, write_wav};

use super::cmd::{HistoryCmd, Inbox};

/// `default_dir` is the folder used while `[history] folder` is not set.
pub fn spawn(
    rx: Receiver<HistoryCmd>,
    inbox: Sender<Inbox>,
    default_dir: PathBuf,
) -> io::Result<JoinHandle<()>> {
    spawn_with(rx, inbox, default_dir, local_now)
}

fn spawn_with(
    rx: Receiver<HistoryCmd>,
    inbox: Sender<Inbox>,
    default_dir: PathBuf,
    clock: fn() -> (Date, u32),
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("history".into())
        .spawn(move || {
            let mut shown = None;
            for cmd in rx {
                let warning = handle(cmd, &default_dir, clock());
                if warning != shown {
                    shown.clone_from(&warning);
                    if inbox.send(Inbox::HistoryWarning(warning)).is_err() {
                        return;
                    }
                }
            }
        })
}

/// Runs one command; returns the warning it leaves standing. The limits always act on the folder
/// in use, saving on or off; a former folder is never touched (SPEC History, owner 2026-11-10).
fn handle(cmd: HistoryCmd, default_dir: &Path, now: (Date, u32)) -> Option<String> {
    let mut pass = Pass::default();
    let (cfg, dictation) = match cmd {
        HistoryCmd::Config(cfg) => (cfg, None),
        HistoryCmd::Save { samples, text, cfg } => (cfg, Some((samples, text))),
    };
    let dir = history::folder(&cfg, default_dir);
    match dictation {
        Some((samples, text)) if cfg.enabled => {
            if let Err(e) = claim(&dir).and_then(|()| save(&dir, &samples, &text, now)) {
                tracing::warn!(kind = ?e.kind(), "history not written");
                return Some(format!("History not saved ({:?})", e.kind()));
            }
        }
        _ if owned(&dir) => sweep_leftovers(&dir, &mut pass),
        _ => return None,
    }
    prune(&dir, &cfg, now.0, &mut pass);
    pass.failed
        .map(|kind| format!("Old history not removed ({kind:?})"))
}

/// One clean-up pass: a step that fails is logged and the pass goes on with the next entry.
#[derive(Default)]
struct Pass {
    failed: Option<io::ErrorKind>,
}

impl Pass {
    fn check<T>(&mut self, step: &'static str, result: io::Result<T>) -> Option<T> {
        result
            .inspect_err(|e| {
                tracing::warn!(kind = ?e.kind(), step, "history clean-up step failed");
                self.failed.get_or_insert(e.kind());
            })
            .ok()
    }
}

/// Is `path` a regular file, not a link or a folder?
fn regular(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

/// Does `dir` hold the marker this app writes? Nothing in a folder without it is listed or changed.
fn owned(dir: &Path) -> bool {
    regular(&dir.join(history::MARKER))
}

/// Makes `dir` the app's own: a new or empty folder gets the marker. A folder of that name with
/// anything else in it is someone else's and is refused.
fn claim(dir: &Path) -> io::Result<()> {
    if owned(dir) {
        return Ok(());
    }
    fs::create_dir_all(dir)?;
    if fs::read_dir(dir)?.next().is_some() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    fs::write(dir.join(history::MARKER), "OxiYapper dictation history\n")
}

/// Writes `<dir>/<day>/<time>.wav`, then `.txt`; a failed text removes the audio again, so an
/// entry is both files or none.
fn save(dir: &Path, samples: &[f32], text: &str, (date, ms): (Date, u32)) -> io::Result<()> {
    let base = dir.join(history::stem(date, ms));
    if let Some(day) = base.parent() {
        fs::create_dir_all(day)?;
    }
    let wav = base.with_extension(WAV);
    write_wav(&wav, samples).map_err(|e| match e {
        WavError::Io(e) => e,
        _ => io::Error::other("wav not written"),
    })?;
    write_atomic(&base.with_extension(TXT), text.as_bytes()).inspect_err(|_| {
        let _ = fs::remove_file(&wav);
    })
}

/// The day folders of `dir`. Anything not named `YYYY-MM-DD` is not history and stays out of
/// every later step.
fn day_dirs(dir: &Path, pass: &mut Pass) -> Vec<PathBuf> {
    let Some(reader) = pass.check("list the folder", fs::read_dir(dir)) else {
        return Vec::new();
    };
    let mut days = Vec::new();
    for entry in reader {
        let Some(entry) = pass.check("read the folder", entry) else {
            continue;
        };
        let dated = entry
            .file_name()
            .to_str()
            .is_some_and(|n| history::day_dir(n).is_some());
        if dated && entry.file_type().is_ok_and(|t| t.is_dir()) {
            days.push(entry.path());
        }
    }
    days
}

/// The readable files directly in `day`.
fn day_files(day: &Path, pass: &mut Pass) -> Vec<fs::DirEntry> {
    let Some(reader) = pass.check("list a day", fs::read_dir(day)) else {
        return Vec::new();
    };
    reader.filter_map(|f| pass.check("read a day", f)).collect()
}

/// Removes what a killed write left in the day folders: `.wav.part` and `.txt.tmp` of a history
/// stem, which can hold dictated text.
fn sweep_leftovers(dir: &Path, pass: &mut Pass) {
    for day in day_dirs(dir, pass) {
        for file in day_files(&day, pass) {
            let leftover = file.file_name().to_str().is_some_and(history::is_leftover);
            if leftover && file.file_type().is_ok_and(|t| t.is_file()) {
                pass.check("remove a leftover", fs::remove_file(file.path()));
            }
        }
    }
}

/// Every entry of `dir`: the `.wav` and `.txt` files named `HHMMSS-mmm` in day folders.
fn scan(dir: &Path, pass: &mut Pass) -> Vec<Entry> {
    let mut sizes: BTreeMap<String, u64> = BTreeMap::new();
    for day in day_dirs(dir, pass) {
        let day_name = day.file_name().map(|n| n.to_string_lossy().into_owned());
        for file in day_files(&day, pass) {
            let path = file.path();
            let named = path.file_stem().and_then(|s| s.to_str()).filter(|s| {
                history::is_file_stem(s) && path.extension().is_some_and(|e| e == WAV || e == TXT)
            });
            let (Some(day_name), Some(stem)) = (&day_name, named) else {
                continue;
            };
            // `DirEntry::metadata` does not follow links.
            if let Some(meta) = pass.check("size a file", file.metadata())
                && meta.is_file()
            {
                *sizes.entry(format!("{day_name}/{stem}")).or_default() += meta.len();
            }
        }
    }
    sizes
        .into_iter()
        .map(|(stem, bytes)| Entry { stem, bytes })
        .collect()
}

/// Deletes the entries `history::expired` names, and the day folders that end up empty.
fn prune(dir: &Path, cfg: &History, today: Date, pass: &mut Pass) {
    for stem in history::expired(&scan(dir, pass), today, cfg) {
        let base = dir.join(&stem);
        for ext in [WAV, TXT] {
            let path = base.with_extension(ext);
            if regular(&path) {
                pass.check("remove an entry", fs::remove_file(path));
            }
        }
        if let Some(day) = base.parent() {
            let _ = fs::remove_dir(day); // fails while anything is left in it
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::channel;

    use super::*;

    const TEXT: &str = "тестовый текст";
    const NO_WARNINGS: [Option<String>; 0] = [];

    fn clock() -> (Date, u32) {
        (Date::from_ymd(2026, 11, 8).unwrap(), 13 * 3_600_000 + 123)
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oxi-dhist-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cfg(enabled: bool) -> History {
        History {
            enabled,
            ..History::default()
        }
    }

    fn save_cmd(cfg: History) -> HistoryCmd {
        HistoryCmd::Save {
            samples: vec![0.25; 1600],
            text: TEXT.into(),
            cfg,
        }
    }

    /// Runs `cmds` through a real worker thread and returns the warnings it sent.
    fn run(dir: &Path, cmds: Vec<HistoryCmd>) -> Vec<Option<String>> {
        let (tx, rx) = channel();
        let (inbox, replies) = channel();
        let worker = spawn_with(rx, inbox, dir.to_owned(), clock).unwrap();
        for cmd in cmds {
            tx.send(cmd).unwrap();
        }
        drop(tx);
        worker.join().unwrap();
        replies
            .try_iter()
            .map(|m| match m {
                Inbox::HistoryWarning(w) => w,
                _ => panic!("unexpected message"),
            })
            .collect()
    }

    /// A fresh folder this app has already claimed.
    fn owned_temp(name: &str) -> PathBuf {
        let dir = temp(name);
        claim(&dir).unwrap();
        dir
    }

    fn file(path: PathBuf) -> PathBuf {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, [0; 8]).unwrap();
        path
    }

    #[test]
    fn save_writes_the_wav_and_the_text_into_a_day_folder() {
        let dir = temp("save");
        assert_eq!(run(&dir, vec![save_cmd(cfg(true))]), NO_WARNINGS);
        let day = dir.join("2026-11-08");
        assert_eq!(
            fs::read_to_string(day.join("130000-123.txt")).unwrap(),
            TEXT
        );
        assert!(fs::metadata(day.join("130000-123.wav")).unwrap().len() > 3200);
        assert_eq!(
            fs::read_dir(&day).unwrap().count(),
            2,
            "no .part or .tmp left"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn off_writes_nothing() {
        let dir = temp("off");
        let cmds = vec![save_cmd(cfg(false)), HistoryCmd::Config(cfg(false))];
        assert_eq!(run(&dir, cmds), NO_WARNINGS);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    /// SPEC History (owner 2026-11-10): the limits act on the folder in use while saving is off;
    /// a folder no longer in use keeps what it holds.
    #[test]
    fn off_prunes_the_current_folder_and_never_a_former_one() {
        let (dir, former, current) = (
            owned_temp("off-prunes"),
            owned_temp("former"),
            temp("current"),
        );
        let in_use = current.join(history::OWNED_DIR);
        claim(&in_use).unwrap();
        let in_use_aged = file(in_use.join("2026-01-01/090000-000.wav"));
        let aged = file(dir.join("2026-01-01/090000-000.wav"));
        let recent = file(dir.join("2026-11-07/090000-000.wav"));
        let tmp = file(dir.join("2026-11-07/090001-000.txt.tmp"));
        let elsewhere = file(former.join("2026-01-01/090000-000.wav"));
        let off = |max_days, max_bytes| History {
            max_days,
            max_bytes,
            ..cfg(false)
        };
        run(&dir, vec![HistoryCmd::Config(off(30, 0))]);
        assert!(!aged.exists(), "the days limit acts while off");
        assert!(!tmp.exists(), "a leftover is swept while off");
        assert!(recent.is_file());
        run(&dir, vec![save_cmd(off(0, 1))]);
        assert!(!recent.exists(), "the size limit acts while off");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "only the marker");
        let moved = History {
            folder: current.to_string_lossy().into_owned(),
            ..off(1, 1)
        };
        run(
            &former,
            vec![HistoryCmd::Config(moved.clone()), save_cmd(moved)],
        );
        assert!(elsewhere.is_file(), "a former folder is never touched");
        assert!(!in_use_aged.exists(), "the folder in use is pruned");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&former);
        let _ = fs::remove_dir_all(&current);
    }

    #[test]
    fn a_set_folder_replaces_the_default() {
        let (default, other) = (temp("default"), temp("other"));
        let mine = file(other.join("2026-01-01/090000-000.wav"));
        let on = History {
            folder: other.to_string_lossy().into_owned(),
            ..cfg(true)
        };
        run(&default, vec![save_cmd(on)]);
        assert_eq!(fs::read_dir(&default).unwrap().count(), 0);
        let owned_dir = other.join(history::OWNED_DIR);
        assert!(owned_dir.join("2026-11-08/130000-123.txt").is_file());
        assert!(owned_dir.join(history::MARKER).is_file());
        assert!(mine.is_file(), "the chosen folder itself is not touched");
        let _ = fs::remove_dir_all(&default);
        let _ = fs::remove_dir_all(&other);
    }

    #[test]
    fn retention_removes_old_days_and_never_other_files() {
        let dir = owned_temp("prune");
        let old = dir.join("2026-09-01");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("090000-000.wav"), [0; 8]).unwrap();
        fs::write(old.join("090000-000.txt"), "x").unwrap();
        fs::write(old.join("notes.txt"), "mine").unwrap();
        fs::create_dir_all(dir.join("2026-08-01")).unwrap();
        fs::write(dir.join("2026-08-01/090000-000.wav"), [0; 8]).unwrap();
        fs::create_dir_all(dir.join("photos")).unwrap();
        fs::write(dir.join("photos/090000-000.wav"), [0; 8]).unwrap();
        fs::write(dir.join("2020-01-01.txt"), "mine").unwrap();
        let bad_hour = file(old.join("240000-000.wav"));
        assert_eq!(run(&dir, vec![save_cmd(cfg(true))]), NO_WARNINGS);
        assert!(!old.join("090000-000.wav").exists());
        assert!(!old.join("090000-000.txt").exists());
        assert!(old.join("notes.txt").is_file(), "not a history name");
        assert!(bad_hour.is_file(), "hour 24 is not a history name");
        assert!(!dir.join("2026-08-01").exists(), "emptied day folder goes");
        assert!(dir.join("photos/090000-000.wav").is_file());
        assert!(dir.join("2020-01-01.txt").is_file());
        assert!(dir.join("2026-11-08/130000-123.wav").is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn size_cap_removes_the_oldest_after_a_save() {
        let dir = owned_temp("cap");
        let day = dir.join("2026-11-07");
        fs::create_dir_all(&day).unwrap();
        fs::write(day.join("090000-000.wav"), vec![0; 5000]).unwrap();
        let tight = History {
            max_bytes: 4000, // one fresh entry (about 3.3 KB) fits, the old one does not
            ..cfg(true)
        };
        assert_eq!(run(&dir, vec![save_cmd(tight)]), NO_WARNINGS);
        assert!(!day.exists());
        assert!(dir.join("2026-11-08/130000-123.txt").is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_folder_warns_once_and_clears_when_it_works() {
        let dir = owned_temp("fail");
        let blocked = dir.join("2026-11-08");
        fs::write(&blocked, "a file where the day folder should go").unwrap();
        let other = temp("fail-other");
        let elsewhere = History {
            folder: other.to_string_lossy().into_owned(),
            ..cfg(true)
        };
        let cmds = vec![
            save_cmd(cfg(true)),
            save_cmd(cfg(true)),
            save_cmd(elsewhere),
        ];
        let warnings = run(&dir, cmds);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        let line = warnings[0].as_deref().unwrap();
        assert!(line.starts_with("History not saved"), "{line}");
        assert!(!line.contains(TEXT));
        assert_eq!(warnings[1], None, "a save that works clears it");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&other);
    }

    #[test]
    fn turning_history_off_clears_the_warning() {
        let dir = owned_temp("off-clears");
        fs::write(
            dir.join("2026-11-08"),
            "a file where the day folder should go",
        )
        .unwrap();
        let warnings = run(
            &dir,
            vec![save_cmd(cfg(true)), HistoryCmd::Config(cfg(false))],
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert_eq!(warnings[1], None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_without_the_marker_is_refused_and_left_alone() {
        let dir = temp("foreign");
        let old = file(dir.join("2026-09-01/090000-000.wav"));
        let part = file(dir.join("2026-09-01/090000-000.wav.part"));
        let warnings = run(
            &dir,
            vec![HistoryCmd::Config(cfg(true)), save_cmd(cfg(true))],
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let line = warnings[0].as_deref().unwrap();
        assert!(line.starts_with("History not saved"), "{line}");
        assert!(old.is_file());
        assert!(part.is_file());
        assert!(!dir.join(history::MARKER).exists());
        assert!(!dir.join("2026-11-08").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_sweep_takes_only_leftovers_with_a_history_stem_in_day_folders() {
        let dir = owned_temp("sweep");
        let day = dir.join("2026-11-07");
        let part = file(day.join("090000-000.wav.part"));
        let tmp = file(day.join("090000-000.txt.tmp"));
        let kept = [
            file(day.join("song.wav.part")),
            file(day.join("240000-000.wav.part")),
            file(day.join("draft.txt.tmp")),
            file(day.join("sub/090000-000.wav.part")),
            file(dir.join("photos/090000-000.wav.part")),
            file(dir.join("090000-000.wav.part")),
        ];
        assert_eq!(run(&dir, vec![HistoryCmd::Config(cfg(true))]), NO_WARNINGS);
        assert!(!part.exists());
        assert!(!tmp.exists());
        for path in kept {
            assert!(path.is_file(), "{}", path.display());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_removes_only_regular_files() {
        let dir = owned_temp("regular");
        let wav = file(dir.join("2026-09-01/090000-000.wav"));
        let inside = file(dir.join("2026-09-01/090000-000.txt/mine.doc"));
        assert_eq!(run(&dir, vec![HistoryCmd::Config(cfg(true))]), NO_WARNINGS);
        assert!(!wav.exists());
        assert!(inside.is_file(), "a folder with the entry's name stays");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A clock set back: entries dated after today are not deleted by age; older ones still are.
    #[test]
    fn a_clock_set_back_keeps_future_dated_entries() {
        let dir = owned_temp("set-back");
        let future = file(dir.join("2027-01-01/090000-000.wav"));
        let aged = file(dir.join("2026-09-01/090000-000.wav"));
        assert_eq!(run(&dir, vec![HistoryCmd::Config(cfg(false))]), NO_WARNINGS);
        assert!(future.is_file());
        assert!(!aged.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_locked_file_does_not_stop_the_pass_or_read_as_not_saved() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = owned_temp("locked");
        let locked = file(dir.join("2026-11-06/090000-000.wav.part"));
        let same_day = file(dir.join("2026-11-06/090001-000.wav.part"));
        let next_day = file(dir.join("2026-11-07/090000-000.txt.tmp"));
        let hold = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&locked)
            .unwrap();
        let warnings = run(&dir, vec![HistoryCmd::Config(cfg(true))]);
        drop(hold);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let line = warnings[0].as_deref().unwrap();
        assert!(line.starts_with("Old history not removed"), "{line}");
        assert!(locked.is_file());
        assert!(!same_day.exists());
        assert!(!next_day.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
