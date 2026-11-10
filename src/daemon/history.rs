//! History worker: saves each dictation's audio, text and meta to the app's own history folder
//! and applies the retention limits (`oxi_core::history`). The audio of a recording streams to
//! disk while the microphone runs (`oxi_platform::wav::WavSpool`); a save adopts that file, and a
//! file left by a killed daemon is recovered at the next start. The layout is a stable contract:
//! `docs/HISTORY-FORMAT.md`. Nothing outside a folder holding the marker is listed or deleted.
//! A failed write comes back as a warning and never reaches the paste. The text is written to
//! the files and nowhere else: no log line, no error string carries it.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::thread::{self, JoinHandle};

use oxi_core::audio::{SAMPLE_RATE, resample};
use oxi_core::config::{ConfigFile, History};
use oxi_core::history::{self, EXTENSIONS, Entry, JSON, Meta, Status, TXT, WAV};
use oxi_core::stats::Date;
use oxi_core::tuning::SettingsSet;
use oxi_platform::fs::{local_now, write_atomic};
use oxi_platform::wav::{
    WavError, read_wav_mono_f32, set_spool_dir, spool_busy, spool_give_up, spool_give_up_all,
    spool_take, spool_take_orphans, write_wav,
};

use super::cmd::{HistoryCmd, Inbox};

/// What a meta file says about the run that made the text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Context {
    pub model: String,
    pub language: String,
    pub settings: Option<SettingsSet>,
}

impl Context {
    /// The model, language and audio settings of `config.toml` as it is now; empty when it
    /// cannot be read (the entry is still saved).
    fn from_config_file(path: &Path) -> Self {
        let cfg = fs::read_to_string(path)
            .ok()
            .and_then(|text| ConfigFile::parse(&text).ok())
            .map(|f| f.config);
        let Some(cfg) = cfg else {
            return Self::default();
        };
        Self {
            model: cfg.model.id.clone(),
            language: cfg.model.language.as_str().to_owned(),
            settings: Some(SettingsSet {
                normalize: cfg.audio.normalize,
                denoise: cfg.audio.denoise,
                trim: cfg.audio.trim,
                model: cfg.model.id,
            }),
        }
    }
}

/// `default_dir` is the folder used while `[history] folder` is not set; the model, language and
/// settings of each entry are read from `config_file` when it is saved.
pub fn spawn(
    rx: Receiver<HistoryCmd>,
    inbox: Sender<Inbox>,
    default_dir: PathBuf,
    config_file: PathBuf,
) -> io::Result<JoinHandle<()>> {
    spawn_with(rx, inbox, default_dir, local_now, move || {
        Context::from_config_file(&config_file)
    })
}

fn spawn_with(
    rx: Receiver<HistoryCmd>,
    inbox: Sender<Inbox>,
    default_dir: PathBuf,
    clock: fn() -> (Date, u32),
    context: impl Fn() -> Context + Send + 'static,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("history".into())
        .spawn(move || {
            let mut shown = None;
            let mut last = None;
            for cmd in rx {
                let (HistoryCmd::Config(cfg) | HistoryCmd::Save { cfg, .. }) = &cmd;
                last = Some(cfg.clone());
                let warning = handle(cmd, &default_dir, clock(), &context);
                if warning != shown {
                    shown.clone_from(&warning);
                    if inbox.send(Inbox::HistoryWarning(warning)).is_err() {
                        break;
                    }
                }
            }
            // The daemon is stopping: a recording still waiting for its text never gets one.
            spool_give_up_all();
            if let Some(cfg) = last {
                handle(HistoryCmd::Config(cfg), &default_dir, clock(), &context);
            }
        })
}

/// No text will come for the dictation whose streamed recording is `spool` (the transcription
/// failed, came out empty, or the job was dropped): it is kept as failed at the next history
/// command. Returns whether it was still waiting.
pub fn no_transcript(spool: &Path) -> bool {
    spool_give_up(spool)
}

/// Runs one command; returns the warning it leaves standing. The limits always act on the folder
/// in use, saving on or off; a former folder is never touched (SPEC History, owner 2026-11-10).
/// While saving is on the folder is also where the next recording streams to.
fn handle(
    cmd: HistoryCmd,
    default_dir: &Path,
    now: (Date, u32),
    context: &impl Fn() -> Context,
) -> Option<String> {
    let mut pass = Pass::default();
    let (cfg, dictation) = match cmd {
        HistoryCmd::Config(cfg) => (cfg, None),
        HistoryCmd::Save {
            samples,
            text,
            cfg,
            spool,
        } => (cfg, Some((samples, text, spool))),
    };
    let dir = history::folder(&cfg, default_dir);
    let claimed = if cfg.enabled { claim(&dir) } else { Ok(()) };
    set_spool_dir((cfg.enabled && claimed.is_ok()).then(|| dir.clone()));
    let refused = |e: &io::Error| {
        tracing::warn!(kind = ?e.kind(), "history not written");
        Some(format!("History not saved ({:?})", e.kind()))
    };
    match dictation {
        Some((samples, text, spool)) if cfg.enabled => {
            let saved = claimed
                .and_then(|()| save(&dir, &samples, &text, spool.as_deref(), now, &context()));
            if let Err(e) = saved {
                return refused(&e);
            }
        }
        _ => {
            // Saving was turned off during the dictation: its streamed recording goes too.
            if let Some(part) = dictation
                .and_then(|(_, _, spool)| spool)
                .filter(|p| spool_take(p))
            {
                let _ = fs::remove_file(part);
            }
            if let Err(e) = &claimed {
                return refused(e);
            }
            if !owned(&dir) {
                return None;
            }
            recover(&dir, &mut pass, context);
        }
    }
    prune(&dir, &cfg, now.0, &mut pass);
    pass.failed
        .map(|kind| format!("Old history not removed ({kind:?})"))
}

/// One clean-up pass: a step that fails is logged and the pass goes on with the next entry.
#[derive(Default)]
struct Pass {
    failed: Option<io::ErrorKind>,
    /// Entries this pass recovered: the limits leave them for a later pass, so a recording is
    /// never brought back and deleted unseen in one go.
    recovered: HashSet<String>,
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

/// The `YYYY-MM-DD/HHMMSS-mmm` id of `base` (an entry's path without extension) inside `dir`.
fn id_of(dir: &Path, base: &Path) -> Option<String> {
    let parts: Vec<&str> = base
        .strip_prefix(dir)
        .ok()?
        .components()
        .map(|c| c.as_os_str().to_str())
        .collect::<Option<_>>()?;
    let id = parts.join("/");
    history::is_id(&id).then_some(id)
}

fn meta(id: &str, duration_ms: u64, status: Status, ctx: &Context) -> Meta {
    Meta {
        version: Meta::VERSION,
        started: history::started(id).unwrap_or_default(),
        duration_ms,
        status,
        model: ctx.model.clone(),
        language: ctx.language.clone(),
        settings: ctx.settings.clone(),
    }
}

fn write_meta(base: &Path, meta: &Meta) -> io::Result<()> {
    let json = meta.to_json().map_err(io::Error::other)?;
    write_atomic(&base.with_extension(JSON), json.as_bytes())
}

fn wav_error(e: WavError) -> io::Error {
    match e {
        WavError::Io(e) => e,
        _ => io::Error::other("wav not written"),
    }
}

/// Writes `<dir>/<day>/<time>.wav`, then `.txt`, then `.json`; a failure removes what was written,
/// so an entry is all three files or none. The recording that streamed to disk while `samples`
/// were recorded (`spool`, carried by the job) gives the entry its name (the time it started): it becomes the `.wav` first and
/// is then replaced by the processed audio, so the audio is on disk at every step. If a later
/// step fails that audio stays, since it may be all there is of the recording; with no `.txt` it
/// lists as failed. An empty text means a cancelled recording that was kept.
fn save(
    dir: &Path,
    samples: &[f32],
    text: &str,
    spool: Option<&Path>,
    (date, ms): (Date, u32),
    ctx: &Context,
) -> io::Result<()> {
    let spooled = spool.filter(|p| spool_take(p)).and_then(|part| {
        let base = PathBuf::from(part.to_str()?.strip_suffix(".wav.part")?);
        id_of(dir, &base)?;
        Some((part, base))
    });
    let base = match &spooled {
        Some((part, base)) => {
            fs::rename(part, base.with_extension(WAV))?;
            base.clone()
        }
        None => dir.join(history::stem(date, ms)),
    };
    if let Some(day) = base.parent() {
        fs::create_dir_all(day)?;
    }
    write_wav(&base.with_extension(WAV), samples).map_err(wav_error)?;
    let status = if text.is_empty() {
        Status::Cancelled
    } else {
        Status::Done
    };
    let duration_ms = samples.len() as u64 * 1000 / u64::from(SAMPLE_RATE);
    let id = id_of(dir, &base).unwrap_or_default();
    write_atomic(&base.with_extension(TXT), text.as_bytes())
        .and_then(|()| write_meta(&base, &meta(&id, duration_ms, status, ctx)))
        .inspect_err(|_| {
            let written = if spooled.is_some() {
                &EXTENSIONS[1..]
            } else {
                &EXTENSIONS[..]
            };
            for ext in written {
                let _ = fs::remove_file(base.with_extension(ext));
            }
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

/// What a killed daemon left in the day folders: a `.txt.tmp` or `.json.tmp` is removed; a
/// `.wav.part` (a recording that was still streaming, or one that never got a transcript) becomes
/// an entry marked interrupted, or failed when it was finished. One this process is still writing
/// or has not handed to a save is left alone.
fn recover(dir: &Path, pass: &mut Pass, context: &impl Fn() -> Context) {
    let orphans: HashSet<PathBuf> = spool_take_orphans().into_iter().collect();
    for day in day_dirs(dir, pass) {
        for file in day_files(&day, pass) {
            let path = file.path();
            let leftover = file.file_name().to_str().is_some_and(history::is_leftover);
            if !leftover || !file.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            if path.extension().is_none_or(|e| e != "part") {
                pass.check("remove a leftover", fs::remove_file(path));
            } else if !spool_busy(&path) {
                let status = if orphans.contains(&path) {
                    Status::Failed
                } else {
                    Status::Interrupted
                };
                let done = recover_part(dir, &path, status, &context());
                if let Some(Some(id)) = pass.check("recover a recording", done) {
                    pass.recovered.insert(id);
                }
            }
        }
    }
}

/// Makes the `.wav.part` at `part` a whole entry: the header is set from the bytes on disk, the
/// file renamed to `.wav`, an empty `.txt` and a meta file written; returns its id. A part with
/// no audio, or not one of ours, is removed; one whose `.wav` already exists was adopted by a
/// save and is removed.
fn recover_part(
    dir: &Path,
    part: &Path,
    status: Status,
    ctx: &Context,
) -> io::Result<Option<String>> {
    let Some(base) = part
        .to_str()
        .and_then(|p| p.strip_suffix(".wav.part"))
        .map(PathBuf::from)
    else {
        return Ok(None);
    };
    let wav = base.with_extension(WAV);
    let mut file = OpenOptions::new().read(true).write(true).open(part)?;
    let mut head = [0; history::WAV_HEADER];
    let read = file.read(&mut head)?;
    let fixed = history::repaired_header(&head[..read], file.metadata()?.len())
        .filter(|r| r.data_bytes > 0 && !wav.exists());
    let Some(fixed) = fixed else {
        drop(file);
        return fs::remove_file(part).map(|()| None);
    };
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&fixed.header)?;
    file.set_len(history::WAV_HEADER as u64 + fixed.data_bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(part, &wav)?;
    let duration_ms = fixed.data_bytes / 2 * 1000 / u64::from(fixed.sample_rate);
    let id = id_of(dir, &base).unwrap_or_default();
    write_atomic(&base.with_extension(TXT), b"")?;
    write_meta(&base, &meta(&id, duration_ms, status, ctx))?;
    Ok(Some(id))
}

/// Every entry of `dir`: the `.wav`, `.txt` and `.json` files named `HHMMSS-mmm` in day folders.
fn scan(dir: &Path, pass: &mut Pass) -> Vec<Entry> {
    let mut sizes: BTreeMap<String, u64> = BTreeMap::new();
    for day in day_dirs(dir, pass) {
        let day_name = day.file_name().map(|n| n.to_string_lossy().into_owned());
        for file in day_files(&day, pass) {
            let path = file.path();
            let named = path.file_stem().and_then(|s| s.to_str()).filter(|s| {
                history::is_file_stem(s)
                    && path
                        .extension()
                        .is_some_and(|e| EXTENSIONS.iter().any(|x| e == *x))
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

/// Removes the files of the entry `stem` and its day folder when that ends up empty. Only regular
/// files go: a folder with an entry's name stays.
fn remove_entry(dir: &Path, stem: &str, pass: &mut Pass) {
    let base = dir.join(stem);
    for ext in EXTENSIONS {
        let path = base.with_extension(ext);
        if regular(&path) {
            pass.check("remove an entry", fs::remove_file(path));
        }
    }
    if let Some(day) = base.parent() {
        let _ = fs::remove_dir(day); // fails while anything is left in it
    }
}

/// Deletes the entries `history::expired` names, and the day folders that end up empty.
fn prune(dir: &Path, cfg: &History, today: Date, pass: &mut Pass) {
    for stem in history::expired(&scan(dir, pass), today, cfg) {
        if !pass.recovered.contains(&stem) {
            remove_entry(dir, &stem, pass);
        }
    }
}

// The calls below are for the History page. The IPC messages that reach them are t12's
// (`crates/oxi-core/src/ipc.rs`); until they are wired the daemon never calls them.

/// One saved recording as the History page shows it.
#[allow(dead_code, reason = "wired to IPC by t12")]
pub struct Recording {
    /// `YYYY-MM-DD/HHMMSS-mmm`, the id every call below takes.
    pub id: String,
    pub meta: Meta,
    /// The transcript; empty for a cancelled, interrupted or failed one.
    pub text: String,
    pub wav: PathBuf,
}

/// The recordings in `dir`, newest first. A folder this app did not make lists nothing. An entry
/// without a readable meta file gets one made from its name and audio: done when it has a text
/// (saved by an older build), failed when it has none (a save that stopped after the audio).
#[allow(dead_code, reason = "wired to IPC by t12")]
pub fn list(dir: &Path) -> Vec<Recording> {
    if !owned(dir) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for entry in scan(dir, &mut Pass::default()).into_iter().rev() {
        let base = dir.join(&entry.stem);
        let wav = base.with_extension(WAV);
        if !regular(&wav) {
            continue;
        }
        let text = fs::read_to_string(base.with_extension(TXT)).unwrap_or_default();
        let info = fs::read_to_string(base.with_extension(JSON))
            .ok()
            .and_then(|json| Meta::from_json(&json).ok())
            .unwrap_or_else(|| {
                let ms = wav_ms(&wav);
                let status = if text.is_empty() {
                    Status::Failed
                } else {
                    Status::Done
                };
                meta(&entry.stem, ms, status, &Context::default())
            });
        out.push(Recording {
            id: entry.stem,
            meta: info,
            text,
            wav,
        });
    }
    out
}

/// The audio of recording `id` as 16 kHz mono samples, ready for whisper (a recovered one is at
/// the device rate and is resampled).
#[allow(dead_code, reason = "wired to IPC by t12")]
pub fn audio_16k(dir: &Path, id: &str) -> io::Result<Vec<f32>> {
    let base = entry_base(dir, id)?;
    let (samples, rate) = read_wav_mono_f32(&base.with_extension(WAV)).map_err(wav_error)?;
    if rate == SAMPLE_RATE {
        return Ok(samples);
    }
    resample(&samples, rate, SAMPLE_RATE).map_err(io::Error::other)
}

/// A transcript of recording `id` made again: the text replaces the old one, the meta says it is
/// done and names the model, language and settings of this run.
#[allow(dead_code, reason = "wired to IPC by t12")]
pub fn set_text(dir: &Path, id: &str, text: &str, ctx: &Context) -> io::Result<()> {
    let base = entry_base(dir, id)?;
    let old = fs::read_to_string(base.with_extension(JSON))
        .ok()
        .and_then(|json| Meta::from_json(&json).ok());
    let duration_ms = old.map_or_else(|| wav_ms(&base.with_extension(WAV)), |m| m.duration_ms);
    write_atomic(&base.with_extension(TXT), text.as_bytes())?;
    write_meta(&base, &meta(id, duration_ms, Status::Done, ctx))
}

/// Deletes recording `id` (its wav, text and meta).
#[allow(dead_code, reason = "wired to IPC by t12")]
pub fn delete(dir: &Path, id: &str) -> io::Result<()> {
    entry_base(dir, id)?;
    let mut pass = Pass::default();
    remove_entry(dir, id, &mut pass);
    pass.failed.map_or(Ok(()), |kind| Err(kind.into()))
}

/// The length of the audio in `wav`, at the rate its header names: a save that stopped after the
/// streamed recording became the `.wav` leaves it at the device rate. 0 when it cannot be read.
fn wav_ms(wav: &Path) -> u64 {
    let read = || -> io::Result<Option<history::Repaired>> {
        let mut file = fs::File::open(wav)?;
        let mut head = [0; history::WAV_HEADER];
        let got = file.read(&mut head)?;
        Ok(history::repaired_header(
            &head[..got],
            file.metadata()?.len(),
        ))
    };
    read()
        .ok()
        .flatten()
        .map_or(0, |h| h.data_bytes / 2 * 1000 / u64::from(h.sample_rate))
}

/// `<dir>/<id>` for an id this app made: a well-formed id inside a folder it owns, with a wav.
fn entry_base(dir: &Path, id: &str) -> io::Result<PathBuf> {
    if !history::is_id(id) || !owned(dir) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let base = dir.join(id);
    if regular(&base.with_extension(WAV)) {
        Ok(base)
    } else {
        Err(io::ErrorKind::NotFound.into())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::mpsc::channel;
    use std::sync::{Mutex, MutexGuard, PoisonError};

    use oxi_platform::wav::WavSpool;

    use super::*;
    use crate::log::scratch::scratch_dir;

    /// The spool folder is process-wide state: the tests run one at a time.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    thread_local! {
        static HELD: RefCell<Option<MutexGuard<'static, ()>>> = const { RefCell::new(None) };
    }

    const TEXT: &str = "тестовый текст";
    const NO_WARNINGS: [Option<String>; 0] = [];

    fn clock() -> (Date, u32) {
        (Date::from_ymd(2026, 11, 8).unwrap(), 13 * 3_600_000 + 123)
    }

    fn temp(name: &str) -> PathBuf {
        HELD.with(|held| {
            held.borrow_mut().get_or_insert_with(|| {
                ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner)
            });
        });
        set_spool_dir(None); // a former test's folder must not take this test's recordings
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
            cfg,
            spool,
        }
    }

    /// Runs `cmds` through a real worker thread and returns the warnings it sent.
    fn run(dir: &Path, cmds: Vec<HistoryCmd>) -> Vec<Option<String>> {
        let (tx, rx) = channel();
        let (inbox, replies) = channel();
        let worker = spawn_with(rx, inbox, dir.to_owned(), clock, ctx).unwrap();
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
            cfg: cfg(true),
            spool: None,
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
            thread::sleep(std::time::Duration::from_millis(5));
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

    /// Recording 2 starts and stops while recording 1 is still being transcribed: each save gets
    /// its own recording, and the second, whose text never comes, is kept as failed.
    #[test]
    fn overlapping_recordings_each_keep_their_own_audio() {
        let dir = owned_temp("overlap");
        handle(HistoryCmd::Config(cfg(true)), &dir, clock(), &ctx);
        let mut first = WavSpool::begin(48_000).unwrap();
        first.push(&vec![0.1; 4800]);
        let first_part = first.finish();
        thread::sleep(std::time::Duration::from_millis(3)); // the next one gets its own name
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
        let worker = spawn_with(rx, inbox, dir.clone(), clock, ctx).unwrap();
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
        let (a, b) = ("2026-11-08/090000-000", "2026-11-08/100000-000");
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
        let samples = audio_16k(&dir, "2026-11-08/110000-000").unwrap();
        assert!(samples.len().abs_diff(16_000) < 400, "{}", samples.len());

        for bad in ["../x", "2026-11-08/090000-000/..", "2026-11-08/090000-001"] {
            assert!(delete(&dir, bad).is_err(), "{bad}");
        }
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
}
