use std::cell::RefCell;
use std::sync::mpsc::channel;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use oxi_platform::wav::WavSpool;

use super::*;
use crate::log::scratch::scratch_dir;

/// The spool folder is process-wide state: the tests that use it (also the coordinator's)
/// run one at a time, through [`temp`].
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

thread_local! {
    static HELD: RefCell<Option<MutexGuard<'static, ()>>> = const { RefCell::new(None) };
}

fn hid(id: &str) -> HistoryId {
    HistoryId::parse(id).unwrap()
}

const TEXT: &str = "тестовый текст";
const NO_WARNINGS: [Option<String>; 0] = [];

fn clock() -> (Date, u32) {
    (Date::from_ymd(2026, 11, 8).unwrap(), 13 * 3_600_000 + 123)
}

pub(crate) fn temp(name: &str) -> PathBuf {
    HELD.with(|held| {
        held.borrow_mut()
            .get_or_insert_with(|| ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner));
    });
    set_spool_dir(None); // a former test's folder must not take this test's recordings
    // Nor may its finished spools: they would show up as this test's orphans.
    spool_give_up_all();
    drop(spool_take_orphans());
    scratch_dir(&format!("oxi-dhist-{name}-{}", std::process::id()))
}

fn cfg(enabled: bool) -> History {
    History {
        enabled,
        ..History::default()
    }
}

fn save_cmd(cfg: History) -> HistoryCmd {
    save_spooled(cfg, None)
}

/// The save of a dictation whose recording streamed to `spool`.
fn save_spooled(cfg: History, spool: Option<PathBuf>) -> HistoryCmd {
    HistoryCmd::Save {
        samples: vec![0.25; 1600],
        text: TEXT.into(),
        words: vec![word("hello", 0.3, None)],
        held: Vec::new(),
        delivered: None,
        cfg,
        spool,
        carried: Vec::new(),
    }
}

/// Runs `cmds` through a real worker thread and returns the warnings it sent.
fn run(dir: &Path, cmds: Vec<HistoryCmd>) -> Vec<Option<String>> {
    let (tx, rx) = channel();
    let (inbox, replies) = channel();
    let worker = spawn_with(rx, inbox, dir.to_owned(), clock, ctx, |_| {}, |_| {}).unwrap();
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
        3,
        "wav, txt and json: no .part or .tmp left"
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
    let hold = OpenOptions::new()
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

fn ctx() -> Context {
    Context {
        model: "small".into(),
        language: "ru".into(),
        settings: Some(SettingsSet {
            normalize: true,
            denoise: false,
            trim: true,
            model: "small".into(),
        }),
    }
}

fn meta_of(base: &Path) -> Meta {
    Meta::from_json(&fs::read_to_string(base.with_extension(JSON)).unwrap()).unwrap()
}

/// What a spool looks like after a kill: the header of the last flush (`flushed` bytes of audio)
/// and `data` bytes after it, the last one maybe half a sample.
fn killed_part(path: &Path, rate: u32, flushed: u32, data: usize) {
    let mut bytes = b"RIFF".to_vec();
    bytes.extend((36 + flushed).to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16_u32.to_le_bytes());
    bytes.extend([1, 0, 1, 0]);
    bytes.extend(rate.to_le_bytes());
    bytes.extend((rate * 2).to_le_bytes());
    bytes.extend([2, 0, 16, 0]);
    bytes.extend(b"data");
    bytes.extend(flushed.to_le_bytes());
    bytes.extend((0..data).map(|i| if i % 2 == 0 { 0x00 } else { 0x20 }));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

#[test]
fn a_save_writes_the_wav_the_text_and_the_meta() {
    let dir = temp("meta");
    assert_eq!(run(&dir, vec![save_cmd(cfg(true))]), NO_WARNINGS);
    let base = dir.join("2026-11-08/130000-123");
    let meta = meta_of(&base);
    assert_eq!(meta.status, Status::Done);
    assert_eq!(meta.started, "2026-11-08T13:00:00.123");
    assert_eq!(meta.duration_ms, 100); // 1600 samples at 16 kHz
    assert_eq!(
        (meta.model.as_str(), meta.language.as_str()),
        ("small", "ru")
    );
    assert_eq!(meta.settings, ctx().settings);
    assert_eq!(fs::read_dir(base.parent().unwrap()).unwrap().count(), 3);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_kept_cancelled_recording_is_saved_with_no_text_and_says_so() {
    let dir = owned_temp("cancelled");
    let cmd = HistoryCmd::Save {
        samples: vec![0.25; 1600],
        text: String::new(),
        words: Vec::new(),
        held: Vec::new(),
        delivered: None,
        cfg: cfg(true),
        spool: None,
        carried: Vec::new(),
    };
    assert_eq!(handle(cmd, &dir, clock(), &ctx), None);
    let base = dir.join("2026-11-08/130000-123");
    assert_eq!(meta_of(&base).status, Status::Cancelled);
    assert_eq!(fs::read_to_string(base.with_extension(TXT)).unwrap(), "");
    let _ = fs::remove_dir_all(&dir);
}

/// Step 1 end to end: the recording streams while the microphone runs, the save takes the
/// streamed file's name and replaces it with the processed audio.
#[test]
fn a_save_adopts_the_streamed_recording_and_its_time() {
    let dir = owned_temp("adopt");
    assert_eq!(
        handle(HistoryCmd::Config(cfg(true)), &dir, clock(), &ctx),
        None
    );
    let mut spool = WavSpool::begin(48_000).unwrap();
    spool.push(&vec![0.1; 4800]);
    let part = spool.finish();
    // The spool is named by the real clock; the save must keep that name, not `clock()`.
    let save = save_spooled(cfg(true), Some(part));
    assert_eq!(handle(save, &dir, clock(), &ctx), None);
    let days: Vec<_> = day_dirs(&dir, &mut Pass::default());
    let files: Vec<_> = day_files(&days[0], &mut Pass::default())
        .into_iter()
        .map(|f| f.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(files.len(), 3, "{files:?}");
    assert!(
        files
            .iter()
            .all(|f| Path::new(f).extension().is_none_or(|e| e != "part")),
        "{files:?}"
    );
    assert!(
        !files.iter().any(|f| f.starts_with("130000-123")),
        "named by the clock, not by the stream: {files:?}"
    );
    let wav = files
        .iter()
        .find(|f| Path::new(f).extension().is_some_and(|e| e == WAV))
        .unwrap();
    let (samples, rate) = read_wav_mono_f32(&days[0].join(wav)).unwrap();
    assert_eq!((samples.len(), rate), (1600, 16_000), "the processed audio");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn saving_turned_off_during_a_dictation_drops_its_streamed_recording() {
    let dir = owned_temp("off-mid");
    handle(HistoryCmd::Config(cfg(true)), &dir, clock(), &ctx);
    let part = WavSpool::begin(16_000).unwrap().finish();
    let save = save_spooled(cfg(false), Some(part.clone()));
    assert_eq!(handle(save, &dir, clock(), &ctx), None);
    assert!(!part.exists());
    assert!(!spool_busy(&part));
    assert_eq!(day_wavs(&dir), Vec::<PathBuf>::new());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_killed_recording_comes_back_as_a_playable_interrupted_entry() {
    let dir = owned_temp("killed");
    let part = dir.join("2026-11-08/090000-000.wav.part");
    // Flushed at 100 bytes of audio, killed after 48 001 bytes: 24 000 samples at 48 kHz.
    killed_part(&part, 48_000, 100, 48_001);
    assert_eq!(run(&dir, vec![HistoryCmd::Config(cfg(true))]), NO_WARNINGS);
    let base = dir.join("2026-11-08/090000-000");
    assert!(!part.exists(), "the part is renamed, not kept");
    assert_eq!(fs::read_to_string(base.with_extension(TXT)).unwrap(), "");
    let (samples, rate) = read_wav_mono_f32(&base.with_extension(WAV)).unwrap();
    assert_eq!((samples.len(), rate), (24_000, 48_000));
    let meta = meta_of(&base);
    assert_eq!(meta.status, Status::Interrupted);
    assert_eq!(meta.duration_ms, 500);
    assert_eq!(meta.started, "2026-11-08T09:00:00.000");
    // A second start finds nothing more to do.
    assert_eq!(run(&dir, vec![HistoryCmd::Config(cfg(true))]), NO_WARNINGS);
    assert!(base.with_extension(WAV).is_file());
    let _ = fs::remove_dir_all(&dir);
}

/// Triage #14: the start says once that it recovered something; a start with nothing to
/// recover, and the passes after the first, say nothing.
#[test]
fn the_start_tells_once_when_it_recovered_a_recording() {
    let dir = owned_temp("told");
    killed_part(&dir.join("2026-11-08/090000-000.wav.part"), 16_000, 0, 3200);
    let (tx, rx) = channel();
    let (inbox, _replies) = channel();
    let (seen_tx, seen) = channel();
    let notify = move |n| seen_tx.send(n).unwrap();
    let worker = spawn_with(rx, inbox, dir.clone(), clock, ctx, notify, |_| {}).unwrap();
    tx.send(HistoryCmd::Config(cfg(true))).unwrap();
    let wait = Duration::from_secs(10);
    assert_eq!(seen.recv_timeout(wait), Ok(Notice::Recovered));
    // A second pass that also recovers something stays quiet: only the start tells.
    killed_part(&dir.join("2026-11-08/100000-000.wav.part"), 16_000, 0, 3200);
    tx.send(HistoryCmd::Config(cfg(true))).unwrap();
    drop(tx);
    worker.join().unwrap();
    assert!(
        dir.join("2026-11-08/100000-000.wav").is_file(),
        "pass 2 recovered"
    );
    assert_eq!(seen.try_iter().collect::<Vec<_>>(), []);
    let _ = fs::remove_dir_all(&dir);
}

/// A start that could not reach the folder has not looked: the first pass that does recovers
/// and tells.
#[test]
fn a_start_without_the_folder_tells_at_the_first_pass_that_recovers() {
    // Someone else's folder (no marker, not empty) is refused.
    let dir = temp("told-late");
    killed_part(&dir.join("2026-11-08/090000-000.wav.part"), 16_000, 0, 3200);
    let (tx, rx) = channel();
    let (inbox, replies) = channel();
    let (seen_tx, seen) = channel();
    let notify = move |n| seen_tx.send(n).unwrap();
    let worker = spawn_with(rx, inbox, dir.clone(), clock, ctx, notify, |_| {}).unwrap();
    tx.send(HistoryCmd::Config(cfg(true))).unwrap();
    let wait = Duration::from_secs(10);
    assert!(matches!(
        replies.recv_timeout(wait),
        Ok(Inbox::HistoryWarning(Some(_)))
    ));
    fs::write(dir.join(history::MARKER), "").unwrap();
    tx.send(HistoryCmd::Config(cfg(true))).unwrap();
    drop(tx);
    worker.join().unwrap();
    assert!(dir.join("2026-11-08/090000-000.wav").is_file(), "recovered");
    assert_eq!(seen.try_iter().collect::<Vec<_>>(), [Notice::Recovered]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_recording_is_recovered_with_saving_off_too_and_is_never_deleted_by_the_sweep() {
    let dir = owned_temp("killed-off");
    let part = dir.join("2026-11-08/090000-000.wav.part");
    killed_part(&part, 16_000, 0, 3200);
    run(&dir, vec![HistoryCmd::Config(cfg(false))]);
    assert!(dir.join("2026-11-08/090000-000.wav").is_file());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_part_with_no_audio_is_dropped_and_one_still_streaming_is_left_alone() {
    let dir = owned_temp("busy");
    let empty = dir.join("2026-11-08/090000-000.wav.part");
    killed_part(&empty, 16_000, 0, 0);
    assert_eq!(
        handle(HistoryCmd::Config(cfg(true)), &dir, clock(), &ctx),
        None
    );
    assert!(!empty.exists());
    assert!(!dir.join("2026-11-08/090000-000.wav").exists());

    let mut live = WavSpool::begin(16_000).unwrap();
    live.push(&[0.1; 800]);
    let path = live.path().to_owned();
    wait_until(|| path.is_file());
    assert_eq!(
        handle(HistoryCmd::Config(cfg(true)), &dir, clock(), &ctx),
        None
    );
    assert!(path.is_file(), "still streaming: left alone");
    assert!(!path.with_extension("").with_extension(WAV).exists());
    live.discard();
    wait_until(|| !spool_busy(&path));
    let _ = fs::remove_dir_all(&dir);
}

/// Waits up to 5 s for `done`, which a spool's writer thread makes true.
fn wait_until(done: impl Fn() -> bool) {
    let start = std::time::Instant::now();
    while !done() {
        assert!(start.elapsed().as_secs() < 5, "spool writer stuck");
        thread::sleep(Duration::from_millis(5));
    }
}

/// The `.wav` files of the day folders in `dir`, sorted.
fn day_wavs(dir: &Path) -> Vec<PathBuf> {
    let days = day_dirs(dir, &mut Pass::default());
    let mut wavs: Vec<_> = days
        .iter()
        .flat_map(|d| day_files(d, &mut Pass::default()))
        .map(|f| f.path())
        .filter(|p| p.extension().is_some_and(|e| e == WAV))
        .collect();
    wavs.sort();
    wavs
}

#[test]
fn a_finished_recording_that_never_got_a_text_is_kept_as_failed() {
    let dir = owned_temp("failed");
    assert_eq!(
        handle(HistoryCmd::Config(cfg(true)), &dir, clock(), &ctx),
        None
    );
    let mut first = WavSpool::begin(16_000).unwrap();
    first.push(&[0.1; 3200]);
    let part = first.finish();
    assert!(no_transcript(&part), "the coordinator gives it up");
    assert!(!no_transcript(&part), "once");
    assert_eq!(
        handle(HistoryCmd::Config(cfg(true)), &dir, clock(), &ctx),
        None
    );
    let [wav] = day_wavs(&dir).try_into().expect("recovered");
    assert_eq!(meta_of(&wav).status, Status::Failed);
    assert_eq!(fs::read_to_string(wav.with_extension(TXT)).unwrap(), "");
    let _ = fs::remove_dir_all(&dir);
}

/// Begins a spool, then waits until the clock that names spools has moved on: the next one gets
/// its own name (a fixed sleep is shorter than a clock tick on a busy box).
fn begin_then_tick(rate: u32) -> WavSpool {
    let spool = WavSpool::begin(rate).unwrap();
    let stamp = local_now();
    while local_now() == stamp {
        thread::yield_now();
    }
    spool
}

/// Two finished recordings: the first is carried on into the second by a press inside the
/// hold gap.
fn carried_pair(dir: &Path) -> (PathBuf, PathBuf) {
    handle(HistoryCmd::Config(cfg(true)), dir, clock(), &ctx);
    let mut first = begin_then_tick(48_000);
    first.push(&vec![0.1; 4800]);
    let first = first.finish();
    let mut second = WavSpool::begin(48_000).unwrap();
    second.push(&vec![0.2; 4800]);
    (first, second.finish())
}

/// The save of the merged dictation of `carried` and `spool`.
fn merged_save(spool: Option<PathBuf>, carried: &Path) -> HistoryCmd {
    HistoryCmd::Save {
        samples: vec![0.25; 1600],
        text: TEXT.into(),
        words: Vec::new(),
        held: Vec::new(),
        delivered: None,
        cfg: cfg(true),
        spool,
        carried: vec![carried.to_owned()],
    }
}

/// The first part's own recording goes once the merged audio is written.
#[test]
fn a_merged_save_deletes_the_carried_part_after_its_audio_is_written() {
    let dir = owned_temp("carried");
    let (first, second) = carried_pair(&dir);
    let save = merged_save(Some(second), &first);
    assert_eq!(handle(save, &dir, clock(), &ctx), None);
    assert!(!first.exists(), "the carried part goes after the write");
    let [wav] = day_wavs(&dir).try_into().expect("one merged entry");
    assert_eq!(meta_of(&wav).status, Status::Done);
    let _ = fs::remove_dir_all(&dir);
}

/// The merged save fails: the first part's recording may be all there is of it and is kept
/// as failed.
#[test]
fn a_failed_merged_save_keeps_the_carried_part_as_failed() {
    let dir = owned_temp("carried-fail");
    let (first, second) = carried_pair(&dir);
    // A file where the merged entry's day folder goes: its write fails.
    let blocked = dir.join("2026-11-08");
    fs::write(&blocked, "not a folder").unwrap();
    assert!(handle(merged_save(None, &first), &dir, clock(), &ctx).is_some());
    assert!(first.is_file(), "the carried part stays");
    fs::remove_file(&blocked).unwrap();
    assert!(spool_take(&second)); // out of the way: only the carried part is looked at
    handle(HistoryCmd::Config(cfg(true)), &dir, clock(), &ctx);
    let base = first.to_str().unwrap().strip_suffix(".wav.part").unwrap();
    assert_eq!(meta_of(&PathBuf::from(base)).status, Status::Failed);
    let _ = fs::remove_dir_all(&dir);
}

/// Recording 2 starts and stops while recording 1 is still being transcribed: each save gets
/// its own recording, and the second, whose text never comes, is kept as failed.
#[test]
fn overlapping_recordings_each_keep_their_own_audio() {
    let dir = owned_temp("overlap");
    handle(HistoryCmd::Config(cfg(true)), &dir, clock(), &ctx);
    let mut first = begin_then_tick(48_000);
    first.push(&vec![0.1; 4800]);
    let first_part = first.finish();
    let mut second = WavSpool::begin(48_000).unwrap();
    second.push(&vec![0.2; 9600]);
    let second_part = second.finish();
    let save = save_spooled(cfg(true), Some(first_part.clone()));
    assert_eq!(handle(save, &dir, clock(), &ctx), None);
    let first_base = PathBuf::from(
        first_part
            .to_str()
            .unwrap()
            .strip_suffix(".wav.part")
            .unwrap(),
    );
    let (samples, rate) = read_wav_mono_f32(&first_base.with_extension(WAV)).unwrap();
    assert_eq!(
        (samples.len(), rate),
        (1600, 16_000),
        "recording 1, processed"
    );
    assert_eq!(
        fs::read_to_string(first_base.with_extension(TXT)).unwrap(),
        TEXT
    );
    assert!(
        spool_busy(&second_part),
        "recording 2 still waits for its text"
    );

    assert!(no_transcript(&second_part));
    handle(HistoryCmd::Config(cfg(true)), &dir, clock(), &ctx);
    let wavs = day_wavs(&dir);
    assert_eq!(wavs.len(), 2, "no duplicate entry: {wavs:?}");
    assert_eq!(meta_of(&wavs[0]).status, Status::Done);
    assert_eq!(meta_of(&wavs[1]).status, Status::Failed);
    let (raw, rate) = read_wav_mono_f32(&wavs[1]).unwrap();
    assert_eq!(
        (raw.len(), rate),
        (9600, 48_000),
        "recording 2 kept its own audio"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_recording_still_waiting_for_its_text_at_quit_is_kept_as_failed() {
    let dir = owned_temp("quit");
    let (tx, rx) = channel();
    let (inbox, _replies) = channel();
    let worker = spawn_with(rx, inbox, dir.clone(), clock, ctx, |_| {}, |_| {}).unwrap();
    tx.send(HistoryCmd::Config(cfg(true))).unwrap();
    // The worker sets the spool folder; a recording can start once it did.
    wait_until(|| {
        WavSpool::begin(16_000).is_some_and(|mut s| {
            s.push(&[0.1; 1600]);
            let _waits_for_its_text = s.finish();
            true
        })
    });
    drop(tx);
    worker.join().unwrap();
    let [wav] = day_wavs(&dir).try_into().expect("kept");
    assert_eq!(meta_of(&wav).status, Status::Failed);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_recovered_recording_is_not_deleted_by_the_limits_in_the_same_pass() {
    let dir = owned_temp("recovered-aged");
    let part = dir.join("2026-01-01/090000-000.wav.part");
    killed_part(&part, 16_000, 0, 3200);
    let aged = file(dir.join("2026-01-02/090000-000.wav"));
    // One pass each (a worker thread runs one more when it stops).
    handle(HistoryCmd::Config(cfg(true)), &dir, clock(), &ctx);
    assert!(dir.join("2026-01-01/090000-000.wav").is_file(), "seen once");
    assert!(!aged.exists(), "the limits still act on the rest");
    handle(HistoryCmd::Config(cfg(true)), &dir, clock(), &ctx);
    assert!(!dir.join("2026-01-01/090000-000.wav").exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn list_set_text_audio_and_delete_work_on_ids_of_an_owned_folder_only() {
    let dir = owned_temp("api");
    let (a, b) = (&hid("2026-11-08/090000-000"), &hid("2026-11-08/100000-000"));
    let entry = |id: &str, text: &str| {
        let base = dir.join(id);
        fs::create_dir_all(base.parent().unwrap()).unwrap();
        write_wav(&base.with_extension(WAV), &vec![0.5; 16_000]).unwrap();
        fs::write(base.with_extension(TXT), text).unwrap();
        write_meta(&base, &meta(id, 1000, Status::Done, &ctx())).unwrap();
    };
    entry(a, "one");
    entry(b, "two");
    // An entry from before the meta file: wav and text only.
    let old = dir.join("2026-11-07/080000-000");
    fs::create_dir_all(old.parent().unwrap()).unwrap();
    write_wav(&old.with_extension(WAV), &vec![0.5; 32_000]).unwrap();
    fs::write(old.with_extension(TXT), "old").unwrap();
    // A save that stopped after the audio (disk full, a kill): wav only.
    let cut = dir.join("2026-11-07/070000-000");
    write_wav(&cut.with_extension(WAV), &vec![0.5; 16_000]).unwrap();

    let got = list(&dir);
    let ids: Vec<_> = got.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        [b, a, "2026-11-07/080000-000", "2026-11-07/070000-000"],
        "newest first"
    );
    assert_eq!(got[0].text, "two");
    assert_eq!(got[2].meta.duration_ms, 2000, "made from the audio");
    assert_eq!(got[2].meta.status, Status::Done);
    assert_eq!(
        got[3].meta.status,
        Status::Failed,
        "never done without a text"
    );
    assert_eq!(audio_16k(&dir, a).unwrap().len(), 16_000);

    set_text(&dir, a, "again", &Context::default()).unwrap();
    assert_eq!(list(&dir)[1].text, "again");
    assert_eq!(
        list(&dir)[1].meta.duration_ms,
        1000,
        "the audio is the same"
    );

    // A recovered recording at the device rate comes out at 16 kHz.
    let rec = dir.join("2026-11-08/110000-000.wav.part");
    killed_part(&rec, 48_000, 0, 96_000);
    run(&dir, vec![HistoryCmd::Config(cfg(true))]);
    let samples = audio_16k(&dir, &hid("2026-11-08/110000-000")).unwrap();
    assert!(samples.len().abs_diff(16_000) < 400, "{}", samples.len());

    // A malformed id cannot be made (`HistoryId::parse`); a well-formed one with no entry fails.
    assert!(delete(&dir, &hid("2026-11-08/090000-001")).is_err());
    delete(&dir, a).unwrap();
    assert!(!dir.join(a).with_extension(WAV).exists());
    assert!(!dir.join(a).with_extension(TXT).exists());
    assert!(!dir.join(a).with_extension(JSON).exists());
    assert!(dir.join(b).with_extension(WAV).is_file());

    let foreign = temp("api-foreign");
    let theirs = file(foreign.join("2026-11-08/090000-000.wav"));
    assert!(list(&foreign).is_empty());
    assert!(delete(&foreign, a).is_err());
    assert!(theirs.is_file());
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&foreign);
}

#[test]
fn a_new_text_keeps_the_old_one_as_the_other_version_and_restore_swaps_them() {
    let dir = owned_temp("versions");
    let id = &hid("2026-11-08/090000-000");
    let base = dir.join(id);
    fs::create_dir_all(base.parent().unwrap()).unwrap();
    write_wav(&base.with_extension(WAV), &vec![0.5; 16_000]).unwrap();
    fs::write(base.with_extension(TXT), "first").unwrap();
    write_meta(&base, &meta(id, 1000, Status::Done, &ctx())).unwrap();
    let text = || fs::read_to_string(base.with_extension(TXT)).unwrap();
    assert!(restore(&dir, id).is_err(), "nothing kept yet");

    let other = Context {
        model: "turbo".into(),
        ..Context::default()
    };
    set_text(&dir, id, "second", &other).unwrap();
    assert_eq!(text(), "second");
    let kept = meta_of(&base).previous.unwrap();
    assert_eq!(
        (kept.text.as_str(), kept.model.as_str()),
        ("first", ctx().model.as_str())
    );

    edit_text(&dir, id, "second fixed").unwrap();
    let now = meta_of(&base);
    assert_eq!(now.model, "turbo", "an edit keeps the run");
    assert_eq!(now.previous.unwrap().text, "second");
    edit_text(&dir, id, "second fixed").unwrap();
    assert_eq!(
        meta_of(&base).previous.unwrap().text,
        "second",
        "same text, no write"
    );

    restore(&dir, id).unwrap();
    assert_eq!(text(), "second");
    restore(&dir, id).unwrap();
    assert_eq!(text(), "second fixed", "the swap undoes itself");

    let bad = &hid("2026-11-08/090000-001");
    assert!(edit_text(&dir, bad, "x").is_err() && restore(&dir, bad).is_err());
    let _ = fs::remove_dir_all(&dir);
}

/// A save whose processed audio could not be written after the streamed recording became the
/// `.wav`: the raw audio at the device rate is all there is; its length comes from its header.
#[test]
fn an_entry_left_with_raw_device_rate_audio_only_lists_its_true_length() {
    let dir = owned_temp("raw-48k");
    let base = dir.join("2026-11-08/090000-000");
    killed_part(&base.with_extension(WAV), 48_000, 96_000, 96_000); // 1 s at 48 kHz
    let [rec] = <[Recording; 1]>::try_from(list(&dir))
        .ok()
        .expect("one entry");
    assert_eq!(rec.meta.duration_ms, 1000, "not 3000 (read as 16 kHz)");
    assert_eq!(rec.meta.status, Status::Failed);
    set_text(&dir, &rec.id, "again", &Context::default()).unwrap();
    assert_eq!(meta_of(&base).duration_ms, 1000);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn retention_removes_every_file_of_an_entry() {
    let dir = owned_temp("retention-all");
    let old = dir.join("2026-09-01/090000-000");
    for ext in EXTENSIONS {
        file(old.with_extension(ext));
    }
    let recent = file(dir.join("2026-11-07/090000-000.json"));
    run(&dir, vec![HistoryCmd::Config(cfg(true))]);
    for ext in EXTENSIONS {
        assert!(!old.with_extension(ext).exists(), "{ext}");
    }
    assert!(!dir.join("2026-09-01").exists());
    assert!(recent.is_file());
    let _ = fs::remove_dir_all(&dir);
}

/// Runs `cmds` through a real worker and returns what it announced to the window.
fn announced(dir: &Path, cmds: Vec<HistoryCmd>) -> Vec<Push> {
    let (tx, rx) = channel();
    let (inbox, _replies) = channel();
    let (seen_tx, seen) = channel();
    let changed = move |p| seen_tx.send(p).unwrap();
    let worker = spawn_with(rx, inbox, dir.to_owned(), clock, ctx, |_| {}, changed).unwrap();
    for cmd in cmds {
        tx.send(cmd).unwrap();
    }
    drop(tx);
    worker.join().unwrap();
    seen.try_iter().collect()
}

#[test]
fn a_saved_recording_is_announced_once_and_never_with_a_text_in_the_log_form() {
    let dir = owned_temp("announce-add");
    let sent = announced(&dir, vec![save_cmd(cfg(true))]);
    let [Push::HistoryAdded { item }] = &sent[..] else {
        panic!("{sent:?}");
    };
    assert_eq!(item.id, "2026-11-08/130000-123");
    assert_eq!(item.status, Status::Done);
    assert_eq!(item.text, Spoken(TEXT.into()));
    assert!(
        dir.join(format!("{}.json", item.id)).is_file(),
        "after the files"
    );
    assert!(!format!("{item:?}").contains(TEXT), "Debug hides the text");
}

#[test]
fn a_retention_pass_that_removes_entries_announces_them_in_one_message() {
    let dir = owned_temp("announce-remove");
    let old = [
        "2026-10-01/090000-000",
        "2026-10-01/090100-000",
        "2026-10-02/090000-000",
    ];
    for id in old {
        file(dir.join(id).with_extension(WAV));
        fs::write(dir.join(id).with_extension(TXT), "x").unwrap();
    }
    let keep = History {
        max_days: 7,
        ..cfg(true)
    };
    let sent = announced(&dir, vec![HistoryCmd::Config(keep)]);
    let [Push::HistoryRemoved { ids }] = &sent[..] else {
        panic!("{sent:?}");
    };
    assert_eq!(ids.len(), 3, "{ids:?}");
    assert!(old.iter().all(|id| ids.iter().any(|i| i == id)));
}

/// Review c04 #2: a recording a Config pass files (a failed capture's streamed part) is
/// announced like a saved one.
#[test]
fn a_recovered_recording_is_announced_to_the_window() {
    let dir = owned_temp("announce-recovered");
    killed_part(&dir.join("2026-11-08/090000-000.wav.part"), 16_000, 0, 3200);
    let sent = announced(&dir, vec![HistoryCmd::Config(cfg(true))]);
    let [Push::HistoryAdded { item }] = &sent[..] else {
        panic!("{sent:?}");
    };
    assert_eq!(item.id, "2026-11-08/090000-000");
    assert_eq!(item.status, Status::Interrupted);
    assert_eq!(item.text, Spoken(String::new()));
}

/// `count` tiny entries, a few bytes each, over several days.
fn many(dir: &Path, count: usize) -> Vec<HistoryId> {
    let mut ids: Vec<HistoryId> = (0..count)
        .map(|i| {
            hid(&format!(
                "2026-{:02}-{:02}/{:02}0000-000",
                1 + i / 500,
                1 + (i % 500) / 20,
                i % 20
            ))
        })
        .collect();
    ids.sort();
    for id in &ids {
        let base = dir.join(id);
        fs::create_dir_all(base.parent().unwrap()).unwrap();
        fs::write(base.with_extension(WAV), [0; 8]).unwrap();
        fs::write(base.with_extension(TXT), "t").unwrap();
    }
    ids
}

#[test]
fn a_page_reads_its_own_entries_only_and_paging_walks_all_once() {
    let dir = owned_temp("index");
    let ids = many(&dir, 2000);
    // The first page of 200 reads 200 entries (400 files), not 2000.
    let mut reads = 0;
    let first: Vec<_> = list_with(&dir, None, |d, id| {
        reads += 1;
        read_recording(d, id)
    })
    .take(200)
    .map(|r| r.id)
    .collect();
    assert_eq!(reads, 200);
    // Walk every page; one entry is deleted between two pages and is skipped.
    let mut seen = first.clone();
    let mut gone = None;
    while seen.len() < 2000 - usize::from(gone.is_some()) {
        let last = seen.last().unwrap().clone();
        if gone.is_none() {
            let victim = ids[ids.len() - seen.len() - 5].clone();
            fs::remove_file(dir.join(&victim).with_extension(WAV)).unwrap();
            gone = Some(victim);
        }
        let page: Vec<_> = list_from(&dir, Some(&last))
            .take(200)
            .map(|r| r.id)
            .collect();
        assert_ne!(page.len(), 0);
        seen.extend(page);
    }
    let mut sorted = seen.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    sorted.dedup();
    assert_eq!(sorted, seen, "newest first, no duplicate");
    assert!(!seen.contains(&gone.unwrap()), "a deleted entry is skipped");
    assert_eq!(seen.len(), 1999, "no gap");
}

fn word(w: &str, p: f32, t: Option<(u32, u32)>) -> history::Word {
    history::Word {
        w: w.into(),
        p,
        t0: t.map(|t| t.0),
        t1: t.map(|t| t.1),
    }
}

#[test]
fn a_save_keeps_the_words_and_a_retranscribe_keeps_timed_ones() {
    let dir = owned_temp("words");
    let id = &hid("2026-11-08/090000-000");
    let base = dir.join(id);
    fs::create_dir_all(base.parent().unwrap()).unwrap();
    write_wav(&base.with_extension(WAV), &vec![0.5; 16_000]).unwrap();
    save(
        &dir,
        &vec![0.5; 16_000],
        "hello world",
        &[word("hello", 0.9, None), word("world", 0.3, None)],
        None,
        (Date::from_ymd(2026, 11, 8).unwrap(), 9 * 3_600_000),
        &ctx(),
    )
    .unwrap();
    let (words, timings) = read_words(&dir, id).unwrap();
    assert_eq!(words.len(), 2);
    assert!(words[1].is_doubtful() && timings.is_none());

    // The timing pass stores its own list.
    let timed = vec![
        word("hello", 0.9, Some((0, 400))),
        word("world", 0.3, Some((500, 900))),
    ];
    let how = timings_of(
        oxi_whisper::Timing::Dtw(oxi_whisper::DtwPreset::LargeV3),
        "podlodka",
    );
    store_timed_words(&dir, id, timed.clone(), how.clone()).unwrap();
    assert_eq!(
        read_words(&dir, id).unwrap(),
        (timed.clone(), Some(how.clone()))
    );
    // A text edit keeps them: they describe the audio.
    edit_text(&dir, id, "hello there").unwrap();
    assert_eq!(read_words(&dir, id).unwrap().0, timed);
    // Transcribe again keeps the timed list and its timings: the audio is the same.
    set_text_with_words(&dir, id, "again", &ctx(), vec![word("again", 0.8, None)]).unwrap();
    assert_eq!(
        read_words(&dir, id).unwrap(),
        (timed.clone(), Some(how.clone()))
    );
    set_text(&dir, id, "third", &ctx()).unwrap();
    assert_eq!(read_words(&dir, id).unwrap(), (timed, Some(how)));
    // An id that is not an owned entry has none to give or take.
    assert!(read_words(&dir, &hid("2026-11-08/110000-000")).is_err());
    assert!(
        store_timed_words(
            &dir,
            &hid("2026-11-08/120000-000"),
            Vec::new(),
            timings_of(oxi_whisper::Timing::Tokens, "")
        )
        .is_err()
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_save_keeps_the_held_parts_and_how_it_was_delivered() {
    let dir = owned_temp("held");
    let held = vec![history::HeldPart {
        text: "doubtful".into(),
        t0: 100,
        t1: 900,
        why: oxi_core::text::HeldWhy::NoSpeech,
    }];
    let mut cmd = save_cmd(cfg(true));
    if let HistoryCmd::Save {
        held: h,
        delivered: d,
        ..
    } = &mut cmd
    {
        h.clone_from(&held);
        *d = Some(history::Delivered::Filtered);
    }
    assert_eq!(handle(cmd, &dir, clock(), &ctx), None);
    let base = dir.join("2026-11-08/130000-123");
    let meta = meta_of(&base);
    assert_eq!(meta.held, held);
    assert_eq!(meta.delivered, Some(history::Delivered::Filtered));
    let json = fs::read_to_string(base.with_extension(JSON)).unwrap();
    assert!(json.contains("\"why\": \"no_speech\""), "{json}");
    // Nothing held, nothing written: the plain entry has neither key.
    assert_eq!(
        handle(
            save_cmd(cfg(true)),
            &dir,
            (clock().0, clock().1 + 1000),
            &ctx
        ),
        None
    );
    let plain = fs::read_to_string(dir.join("2026-11-08/130001-123.json")).unwrap();
    assert!(!plain.contains("held") && !plain.contains("delivered") && !plain.contains("raw"));
    // Review d03 #11: Transcribe again, an edit and a restore keep the held parts.
    let id = &hid("2026-11-08/130000-123");
    set_text(&dir, id, "again", &ctx()).unwrap();
    assert_eq!(meta_of(&base).held, held);
    assert_eq!(meta_of(&base).delivered, Some(history::Delivered::Filtered));
    edit_text(&dir, id, "edited").unwrap();
    restore(&dir, id).unwrap();
    assert_eq!(meta_of(&base).held, held);
    assert_eq!(meta_of(&base).delivered, Some(history::Delivered::Filtered));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_recovered_recording_is_marked_raw_and_stays_so() {
    let dir = owned_temp("raw-marker");
    let id = &hid("2026-11-08/090000-000");
    killed_part(&dir.join(format!("{id}.wav.part")), 16_000, 0, 3200);
    let mut pass = Pass::default();
    recover(&dir, &mut pass, &ctx);
    let base = dir.join(id);
    assert!(meta_of(&base).raw, "recovered audio is raw");
    // A retranscribe and an edit rewrite the status, not the audio.
    set_text(&dir, id, "again", &ctx()).unwrap();
    assert!(meta_of(&base).raw && meta_of(&base).status == Status::Done);
    edit_text(&dir, id, "edited").unwrap();
    assert!(meta_of(&base).raw);
    // A dictation the app saved is processed audio.
    assert_eq!(handle(save_cmd(cfg(true)), &dir, clock(), &ctx), None);
    assert!(!meta_of(&dir.join("2026-11-08/130000-123")).raw);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_saved_dictation_keeps_the_words_it_came_with() {
    let dir = owned_temp("save-words");
    assert_eq!(handle(save_cmd(cfg(true)), &dir, clock(), &ctx), None);
    let base = dir.join("2026-11-08/130000-123");
    assert_eq!(meta_of(&base).words, [word("hello", 0.3, None)]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_retranscribe_replaces_a_confidence_only_list() {
    let dir = owned_temp("words-plain");
    let id = &hid("2026-11-08/100000-000");
    save(
        &dir,
        &vec![0.5; 16_000],
        "hello world",
        &[word("hello", 0.9, None), word("world", 0.3, None)],
        None,
        (Date::from_ymd(2026, 11, 8).unwrap(), 10 * 3_600_000),
        &ctx(),
    )
    .unwrap();
    set_text_with_words(&dir, id, "again", &ctx(), vec![word("again", 0.8, None)]).unwrap();
    assert_eq!(
        read_words(&dir, id).unwrap(),
        (vec![word("again", 0.8, None)], None)
    );
    // Without a list (the plain call) there are no words, never the old ones.
    set_text(&dir, id, "third", &ctx()).unwrap();
    assert_eq!(read_words(&dir, id).unwrap().0.len(), 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn words_of_a_transcript_keep_the_confidence_and_the_times_when_there_are_any() {
    use oxi_core::types::Millis;
    let w = |text: &str, p: f32, t: Option<(u64, u64)>| oxi_whisper::Word {
        text: text.into(),
        p,
        t0: t.map(|t| Millis(t.0)),
        t1: t.map(|t| Millis(t.1)),
    };
    let seg = |words| oxi_whisper::Segment {
        text: String::new(),
        t0: Millis(0),
        t1: Millis(1),
        no_speech_prob: 0.0,
        words,
    };
    let transcript = oxi_whisper::Transcript {
        segments: vec![
            seg(vec![w("a", 0.5, Some((10, 20))), w("b", f32::NAN, None)]),
            seg(vec![w("c", 1.5, None)]),
        ],
    };
    assert_eq!(
        words_of(&transcript),
        [
            word("a", 0.5, Some((10, 20))),
            word("b", 0.0, None),
            word("c", 1.0, None)
        ]
    );
}

/// `entries` as (id, text, duration ms) written the way a save writes them.
fn seeded(dir: &Path, entries: &[(&str, &str, u64)]) {
    for (id, text, ms) in entries {
        let base = dir.join(id);
        fs::create_dir_all(base.parent().unwrap()).unwrap();
        fs::write(base.with_extension(WAV), [0; 8]).unwrap();
        fs::write(base.with_extension(TXT), text).unwrap();
        let ctx = Context::default();
        write_meta(&base, &meta(id, *ms, Status::Done, &ctx)).unwrap();
    }
}

fn found(dir: &Path, query: &str, filter: &history::Filter, older: Option<&str>) -> Vec<String> {
    search(dir, query, filter, older, &mut SearchCache::default())
        .map(|r| r.id.to_string())
        .collect()
}

#[test]
fn a_search_reads_each_text_once_and_then_only_what_changed() {
    let dir = owned_temp("search-cache");
    let ids = many(&dir, 2000);
    let mut cache = SearchCache::default();
    let none = history::Filter::default();
    let walk = |query: &str, cache: &mut SearchCache| {
        let mut reads = 0;
        let ids: Vec<HistoryId> = search_with(&dir, query, &none, None, cache, |d, id| {
            reads += 1;
            read_recording(d, id)
        })
        .map(|r| r.id)
        .collect();
        (reads, ids)
    };
    assert_eq!(
        walk("zz", &mut cache),
        (2000, vec![]),
        "the first reads each once"
    );
    assert_eq!(walk("zz", &mut cache), (0, vec![]), "a second reads none");
    // A new entry, an edited one, one rewritten to the same size and a deleted one: only
    // the first three are read again.
    let new = "2026-12-01/100000-000";
    fs::create_dir_all(dir.join("2026-12-01")).unwrap();
    fs::write(dir.join(new).with_extension(WAV), [0; 8]).unwrap();
    fs::write(dir.join(new).with_extension(TXT), "zz new").unwrap();
    let edited = &ids[10];
    edit_text(&dir, edited, "zz").unwrap();
    let same = &ids[20];
    let txt = dir.join(same).with_extension(TXT);
    fs::write(&txt, "z").unwrap();
    fs::File::options()
        .write(true)
        .open(&txt)
        .unwrap()
        .set_modified(std::time::SystemTime::now() + Duration::from_secs(5))
        .unwrap();
    delete(&dir, &ids[30]).unwrap();
    let (reads, found) = walk("z", &mut cache);
    assert_eq!(reads, 3);
    let mut want = vec![hid(new), edited.clone(), same.clone()];
    want.sort_by(|a, b| b.cmp(a));
    assert_eq!(found, want);
    // The deleted one is gone from the answers, with nothing read.
    let (reads, found) = walk("t", &mut cache);
    assert_eq!(reads, 0);
    assert!(!found.contains(&ids[30]));
    assert_eq!(found.len(), 1997);
}

#[test]
fn a_search_lists_the_matches_newest_first_and_the_filters_narrow_it() {
    let dir = owned_temp("search");
    seeded(
        &dir,
        &[
            ("2026-11-08/100000-000", "Привет мир", 1000),
            ("2026-11-09/100000-000", "привет всем", 5000),
            ("2026-11-09/110000-000", "Ёж пришёл", 9000),
            ("2026-11-10/100000-000", "ничего", 2000),
        ],
    );
    let none = history::Filter::default();
    assert_eq!(
        found(&dir, "ПРИВЕТ", &none, None),
        ["2026-11-09/100000-000", "2026-11-08/100000-000"]
    );
    assert_eq!(
        found(&dir, "еж пришел", &none, None),
        ["2026-11-09/110000-000"]
    );
    assert_eq!(found(&dir, "", &none, None).len(), 4, "no words: all");
    assert_eq!(found(&dir, "нет такого", &none, None), Vec::<String>::new());
    let nov9 = history::Filter {
        day: history::day_dir("2026-11-09"),
        ..none.clone()
    };
    assert_eq!(found(&dir, "", &nov9, None).len(), 2);
    let long = history::Filter {
        min_ms: Some(5000),
        max_ms: Some(9000),
        ..none
    };
    assert_eq!(found(&dir, "", &long, None).len(), 2);
    assert_eq!(
        found(&dir, "привет", &long, None),
        ["2026-11-09/100000-000"]
    );
    // Paging: the same walk as the plain list.
    assert_eq!(
        found(
            &dir,
            "привет",
            &history::Filter::default(),
            Some("2026-11-09/100000-000")
        ),
        ["2026-11-08/100000-000"]
    );
}

#[test]
fn a_new_entry_is_found_and_an_edited_one_by_its_new_text_with_nothing_to_rebuild() {
    let dir = owned_temp("search-fresh");
    let none = history::Filter::default();
    seeded(&dir, &[("2026-11-08/100000-000", "alpha", 1000)]);
    assert_eq!(found(&dir, "beta", &none, None), Vec::<String>::new());
    seeded(&dir, &[("2026-11-08/110000-000", "beta", 1000)]);
    assert_eq!(found(&dir, "beta", &none, None), ["2026-11-08/110000-000"]);
    edit_text(&dir, &hid("2026-11-08/110000-000"), "gamma").unwrap();
    assert_eq!(found(&dir, "beta", &none, None), Vec::<String>::new());
    assert_eq!(found(&dir, "gamma", &none, None).len(), 1);
}

#[test]
fn a_transcribed_file_is_saved_as_a_done_record_delivered_nowhere() {
    let dir = temp("file-save");
    let ctx = file_context(&dir.join("none.toml"), "ru");
    let item = save_transcribed(&dir, &[0.25; 1600], "from a file", &ctx, clock()).unwrap();
    assert_eq!((item.status, item.duration_ms), (Status::Done, 100));
    assert_eq!(item.language, "ru");
    let rec = list_from(&dir, None).next().unwrap();
    assert_eq!(rec.text, "from a file");
    assert_eq!(rec.meta.delivered, Some(history::Delivered::Unknown));
}
