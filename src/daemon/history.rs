//! History worker: saves each dictation's audio, text and meta to the app's own history folder
//! and applies the retention limits (`oxi_core::history`). The audio of a recording streams to
//! disk while the microphone runs (`oxi_platform::wav::WavSpool`); a save adopts that file, and a
//! file left by a killed daemon is recovered at the next start. The layout is a stable contract:
//! `docs/HISTORY-FORMAT.md`. Nothing outside a folder holding the marker is listed or deleted.
//! A failed write comes back as a warning and never reaches the paste. The text is written to
//! the files and nowhere else: no log line, no error string carries it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::thread::{self, JoinHandle};

use oxi_core::audio::{SAMPLE_RATE, resample};
use oxi_core::config::{ConfigFile, History};
use oxi_core::history::{self, EXTENSIONS, Entry, HistoryId, JSON, Meta, Status, TXT, WAV};
use oxi_core::ipc::{HeldText, HistoryItem, Push, Spoken};
use oxi_core::machine::Notice;
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
/// settings of each entry are read from `config_file` when it is saved. `notify` gets
/// [`Notice::Recovered`] when the start finds recordings a crash left behind.
pub fn spawn(
    rx: Receiver<HistoryCmd>,
    inbox: Sender<Inbox>,
    default_dir: PathBuf,
    config_file: PathBuf,
    notify: impl Fn(Notice) + Send + 'static,
) -> io::Result<JoinHandle<()>> {
    spawn_with(
        rx,
        inbox,
        default_dir,
        local_now,
        move || Context::from_config_file(&config_file),
        notify,
        // Lazy: nothing is read unless a settings window is connected.
        super::ipc::announce,
    )
}

fn spawn_with(
    rx: Receiver<HistoryCmd>,
    inbox: Sender<Inbox>,
    default_dir: PathBuf,
    clock: fn() -> (Date, u32),
    context: impl Fn() -> Context + Send + 'static,
    notify: impl Fn(Notice) + Send + 'static,
    changed: impl Fn(Push) + Send + 'static,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("history".into())
        .spawn(move || {
            let mut shown = None;
            let mut last = None;
            let mut first = true;
            for cmd in rx {
                let (HistoryCmd::Config(cfg) | HistoryCmd::Save { cfg, .. }) = &cmd;
                last = Some(cfg.clone());
                let mut pass = Pass::default();
                let warning = handle_in(cmd, &default_dir, clock(), &context, &mut pass);
                // Only the first pass that searched finds what a crash left; later ones file this
                // run's own failed recordings, which had their notice already (triage #14). A pass
                // that never reached the folder (refused, not owned) does not use up the notice.
                if first && pass.searched {
                    first = false;
                    if !pass.recovered.is_empty() {
                        notify(Notice::Recovered);
                    }
                }
                // After the files are on disk: an open History shows the change at once.
                for item in std::mem::take(&mut pass.added) {
                    changed(Push::HistoryAdded { item });
                }
                if !pass.removed.is_empty() {
                    changed(Push::HistoryRemoved {
                        ids: std::mem::take(&mut pass.removed),
                    });
                }
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
    handle_in(cmd, default_dir, now, context, &mut Pass::default())
}

/// [`handle`] in `pass`, which tells what it recovered.
fn handle_in(
    cmd: HistoryCmd,
    default_dir: &Path,
    now: (Date, u32),
    context: &impl Fn() -> Context,
    pass: &mut Pass,
) -> Option<String> {
    let (cfg, dictation) = match cmd {
        HistoryCmd::Config(cfg) => (cfg, None),
        HistoryCmd::Save {
            samples,
            text,
            words,
            held,
            delivered,
            cfg,
            spool,
            carried,
        } => {
            let heard = Heard {
                words,
                held,
                delivered,
            };
            (cfg, Some((samples, text, heard, spool, carried)))
        }
    };
    let dir = history::folder(&cfg, default_dir);
    let claimed = if cfg.enabled { claim(&dir) } else { Ok(()) };
    set_spool_dir((cfg.enabled && claimed.is_ok()).then(|| dir.clone()));
    let refused = |e: &io::Error| {
        tracing::warn!(kind = ?e.kind(), "history not written");
        Some(format!("History not saved ({:?})", e.kind()))
    };
    match dictation {
        Some((samples, text, heard, spool, carried)) if cfg.enabled => {
            let saved = claimed.and_then(|()| {
                save_with(
                    &dir,
                    &samples,
                    &text,
                    &heard,
                    spool.as_deref(),
                    now,
                    &context(),
                )
            });
            match saved {
                Ok(item) => {
                    pass.added.push(item);
                    // The merged audio is on disk now: the earlier parts' own recordings can go.
                    remove_spools(&carried);
                }
                Err(e) => {
                    // They may be all there is of those parts: kept as failed at the next pass.
                    for part in &carried {
                        spool_give_up(part);
                    }
                    return refused(&e);
                }
            }
        }
        _ => {
            // Saving was turned off during the dictation: its streamed recordings go too.
            if let Some((_, _, _, spool, carried)) = dictation {
                remove_spools(spool.iter().chain(&carried));
            }
            if let Err(e) = &claimed {
                return refused(e);
            }
            if !owned(&dir) {
                return None;
            }
            recover(&dir, pass, context);
        }
    }
    prune(&dir, &cfg, now.0, pass);
    pass.failed
        .map(|kind| format!("Old history not removed ({kind:?})"))
}

/// Deletes the finished recordings at `parts` that are still waiting, once their files are closed.
fn remove_spools<'a>(parts: impl IntoIterator<Item = &'a PathBuf>) {
    for part in parts.into_iter().filter(|p| spool_take(p)) {
        let _ = fs::remove_file(part);
    }
}

/// One clean-up pass: a step that fails is logged and the pass goes on with the next entry.
#[derive(Default)]
struct Pass {
    failed: Option<io::ErrorKind>,
    /// Entries this pass recovered: the limits leave them for a later pass, so a recording is
    /// never brought back and deleted unseen in one go.
    recovered: HashSet<String>,
    /// The pass looked for recordings to recover.
    searched: bool,
    /// The recordings this pass saved or recovered.
    added: Vec<HistoryItem>,
    /// The entries this pass removed, for the window.
    removed: Vec<HistoryId>,
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
fn id_of(dir: &Path, base: &Path) -> Option<HistoryId> {
    let parts: Vec<&str> = base
        .strip_prefix(dir)
        .ok()?
        .components()
        .map(|c| c.as_os_str().to_str())
        .collect::<Option<_>>()?;
    HistoryId::try_from(parts.join("/")).ok()
}

fn meta(id: &str, duration_ms: u64, status: Status, ctx: &Context) -> Meta {
    Meta {
        settings: ctx.settings.clone(),
        ..Meta::basic(id, duration_ms, status, &ctx.model, &ctx.language)
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

/// What the whisper worker and the text pipeline made besides the text, for the entry's json.
struct Heard {
    words: Vec<history::Word>,
    held: Vec<history::HeldPart>,
    delivered: Option<history::Delivered>,
}

/// [`save_with`] for a text with only words.
#[cfg(test)]
fn save(
    dir: &Path,
    samples: &[f32],
    text: &str,
    words: &[history::Word],
    spool: Option<&Path>,
    now: (Date, u32),
    ctx: &Context,
) -> io::Result<HistoryItem> {
    let heard = Heard {
        words: words.to_vec(),
        held: Vec::new(),
        delivered: None,
    };
    save_with(dir, samples, text, &heard, spool, now, ctx)
}

/// Writes `<dir>/<day>/<time>.wav`, then `.txt`, then `.json`; a failure removes what was written,
/// so an entry is all three files or none. The recording that streamed to disk while `samples`
/// were recorded (`spool`, carried by the job) gives the entry its name (the time it started): it becomes the `.wav` first and
/// is then replaced by the processed audio, so the audio is on disk at every step. If a later
/// step fails that audio stays, since it may be all there is of the recording; with no `.txt` it
/// lists as failed. An empty text means a cancelled recording that was kept.
fn save_with(
    dir: &Path,
    samples: &[f32],
    text: &str,
    heard: &Heard,
    spool: Option<&Path>,
    (date, ms): (Date, u32),
    ctx: &Context,
) -> io::Result<HistoryItem> {
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
    let id = id_of(dir, &base).ok_or(io::ErrorKind::InvalidInput)?;
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
    write_atomic(&base.with_extension(TXT), text.as_bytes())
        .and_then(|()| {
            write_meta(
                &base,
                &Meta {
                    words: heard.words.clone(),
                    held: heard.held.clone(),
                    delivered: heard.delivered,
                    ..meta(&id, duration_ms, status, ctx)
                },
            )
        })
        .map(|()| HistoryItem {
            id: id.clone(),
            status,
            duration_ms,
            model: ctx.model.clone(),
            language: ctx.language.clone(),
            text: Spoken(text.to_owned()),
            held: heard.held.iter().map(HeldText::from).collect(),
        })
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
    pass.searched = true;
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
                if let Some(Some(item)) = pass.check("recover a recording", done) {
                    pass.recovered.insert(item.id.to_string());
                    pass.added.push(item);
                }
            }
        }
    }
}

/// Makes the `.wav.part` at `part` a whole entry: the header is set from the bytes on disk, the
/// file renamed to `.wav`, an empty `.txt` and a meta file written; returns the entry. A part with
/// no audio, or not one of ours, is removed; one whose `.wav` already exists was adopted by a
/// save and is removed.
fn recover_part(
    dir: &Path,
    part: &Path,
    status: Status,
    ctx: &Context,
) -> io::Result<Option<HistoryItem>> {
    let Some(base) = part
        .to_str()
        .and_then(|p| p.strip_suffix(".wav.part"))
        .map(PathBuf::from)
    else {
        return Ok(None);
    };
    let Some(id) = id_of(dir, &base) else {
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
    write_atomic(&base.with_extension(TXT), b"")?;
    // The recovered audio is the raw microphone audio, whatever its rate.
    let raw = Meta {
        raw: true,
        ..meta(&id, duration_ms, status, ctx)
    };
    write_meta(&base, &raw)?;
    Ok(Some(HistoryItem {
        id,
        status,
        duration_ms,
        model: ctx.model.clone(),
        language: ctx.language.clone(),
        text: Spoken(String::new()),
        held: Vec::new(),
    }))
}

/// Every entry of `dir`: the `.wav`, `.txt` and `.json` files named `HHMMSS-mmm` in day folders.
fn scan(dir: &Path, pass: &mut Pass) -> Vec<Entry> {
    scan_stamped(dir, pass)
        .into_iter()
        .map(|(stem, (bytes, _))| Entry { stem, bytes })
        .collect()
}

/// [`scan`] with a stamp of each entry's files (their sizes and change times, as the folder
/// listing gives them): a new stamp means a file of the entry was written.
fn scan_stamped(dir: &Path, pass: &mut Pass) -> BTreeMap<String, (u64, u64)> {
    use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
    let mut sizes: BTreeMap<String, (u64, u64)> = BTreeMap::new();
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
                let stamp = BuildHasherDefault::<DefaultHasher>::default().hash_one((
                    path.extension(),
                    meta.len(),
                    meta.modified().ok(),
                ));
                let entry = sizes.entry(format!("{day_name}/{stem}")).or_default();
                entry.0 += meta.len();
                // A sum: the order the folder lists the files in does not matter.
                entry.1 = entry.1.wrapping_add(stamp);
            }
        }
    }
    sizes
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
            let failed = pass.failed;
            remove_entry(dir, &stem, pass);
            if pass.failed == failed {
                pass.removed.extend(HistoryId::try_from(stem).ok());
            }
        }
    }
}

// The calls below are for the History page. The IPC messages that reach them are t12's
// (`crates/oxi-core/src/ipc.rs`); until they are wired the daemon never calls them.

/// One saved recording as the History page shows it.
#[derive(Clone)]
pub struct Recording {
    pub id: HistoryId,
    pub meta: Meta,
    /// The transcript; empty for a cancelled, interrupted or failed one.
    pub text: String,
}

/// The recordings in `dir` older than `older_than`, newest first, read one at a time: the
/// folder is listed (names and sizes, no file opened) and an entry's `.txt` and `.json` are read
/// only when the caller asks for it, so a page of 200 reads 400 files however long the history is.
/// A folder this app did not make lists nothing. An entry without a readable meta file gets one
/// made from its name and audio: done when it has a text (saved by an older build), failed when
/// it has none (a save that stopped after the audio).
pub fn list_from<'a>(
    dir: &'a Path,
    older_than: Option<&'a str>,
) -> impl Iterator<Item = Recording> + 'a {
    list_with(dir, older_than, read_recording)
}

/// [`list_from`] with the reader of one entry given.
fn list_with<'a>(
    dir: &'a Path,
    older_than: Option<&'a str>,
    mut read: impl FnMut(&Path, HistoryId) -> Option<Recording> + 'a,
) -> impl Iterator<Item = Recording> + 'a {
    let entries = if owned(dir) {
        scan(dir, &mut Pass::default())
    } else {
        Vec::new()
    };
    entries
        .into_iter()
        .rev()
        // Ids are `YYYY-MM-DD/HHMMSS-mmm`: text order is time order.
        .filter(move |e| older_than.is_none_or(|id| e.stem.as_str() < id))
        .filter_map(move |e| read(dir, HistoryId::try_from(e.stem).ok()?))
}

fn read_recording(dir: &Path, id: HistoryId) -> Option<Recording> {
    let base = dir.join(id.as_str());
    let wav = base.with_extension(WAV);
    if !regular(&wav) {
        return None;
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
            meta(&id, ms, status, &Context::default())
        });
    Some(Recording {
        id,
        meta: info,
        text,
    })
}

/// Every recording in `dir`, newest first.
#[cfg(test)]
pub fn list(dir: &Path) -> Vec<Recording> {
    list_from(dir, None).collect()
}

/// The entries a search has read (arch-plan 7.5): each with the stamp its files had then and its
/// folded text. Kept by the settings connection, so a query reads only what is new or changed
/// since the last.
#[derive(Default)]
pub struct SearchCache {
    dir: PathBuf,
    read: HashMap<String, (u64, String, Recording)>,
}

/// The recordings in `dir` that pass `filter` and hold every word of `query`, newest first,
/// older than `older_than`. The folder is listed each time (no file opened); an entry is read
/// only when `cache` has no copy of it with the same stamp, so an entry saved, edited or deleted
/// since is seen at once and the rest is not read again. A day filter skips the other days
/// without reading them. shortcut: the stamp is the files' size and change time, so a rewrite
/// to the same size within one tick of the file clock (FAT: 2 s) is missed until the next change.
pub fn search<'a>(
    dir: &'a Path,
    query: &'a str,
    filter: &'a history::Filter,
    older_than: Option<&'a str>,
    cache: &'a mut SearchCache,
) -> impl Iterator<Item = Recording> + 'a {
    search_with(dir, query, filter, older_than, cache, read_recording)
}

/// [`search`] with the reader of one entry given.
fn search_with<'a>(
    dir: &'a Path,
    query: &'a str,
    filter: &'a history::Filter,
    older_than: Option<&'a str>,
    cache: &'a mut SearchCache,
    mut read: impl FnMut(&Path, HistoryId) -> Option<Recording> + 'a,
) -> impl Iterator<Item = Recording> + 'a {
    let entries = if owned(dir) {
        scan_stamped(dir, &mut Pass::default())
    } else {
        BTreeMap::new()
    };
    if cache.dir != dir {
        dir.clone_into(&mut cache.dir);
        cache.read.clear();
    }
    cache.read.retain(|id, _| entries.contains_key(id));
    let day = filter.day.map(|d| d.to_string());
    entries
        .into_iter()
        .rev()
        // Ids are `YYYY-MM-DD/HHMMSS-mmm`: text order is time order.
        .filter(move |(id, _)| older_than.is_none_or(|o| id.as_str() < o))
        .filter(move |(id, _)| day.as_deref().is_none_or(|d| id.starts_with(d)))
        .filter_map(move |(id, (_, stamp))| {
            if cache.read.get(&id).is_none_or(|(at, ..)| *at != stamp) {
                let rec = read(dir, HistoryId::parse(&id)?)?;
                let folded = history::fold(&rec.text);
                cache.read.insert(id.clone(), (stamp, folded, rec));
            }
            let (_, folded, rec) = cache.read.get(&id)?;
            let searched = history::Searched {
                text_lower: folded,
                day: history::day_dir(id.get(..10)?)?,
                duration_ms: rec.meta.duration_ms,
            };
            history::matches(query, filter, &searched).then(|| rec.clone())
        })
}

/// The run context of a transcribed file: the model and audio settings of `config_file` and the
/// language the file was transcribed in.
pub fn file_context(config_file: &Path, language: &str) -> Context {
    Context {
        language: language.to_owned(),
        ..Context::from_config_file(config_file)
    }
}

/// Saves a transcribed file as a new record: `samples` are the 16 kHz mono copy, `text` the
/// result. Delivered nowhere (`unknown`). Not pruned here: the next dictation's pass does it.
pub fn save_transcribed(
    dir: &Path,
    samples: &[f32],
    text: &str,
    ctx: &Context,
    now: (Date, u32),
) -> io::Result<HistoryItem> {
    claim(dir)?;
    let heard = Heard {
        words: Vec::new(),
        held: Vec::new(),
        delivered: Some(history::Delivered::Unknown),
    };
    save_with(dir, samples, text, &heard, None, now, ctx)
}

/// The audio of recording `id` as 16 kHz mono samples, ready for whisper (a recovered one is at
/// the device rate and is resampled).
#[allow(dead_code, reason = "wired to IPC by t12")]
pub fn audio_16k(dir: &Path, id: &HistoryId) -> io::Result<Vec<f32>> {
    let base = entry_base(dir, id)?;
    let (samples, rate) = read_wav_mono_f32(&base.with_extension(WAV)).map_err(wav_error)?;
    if rate == SAMPLE_RATE {
        return Ok(samples);
    }
    resample(&samples, rate, SAMPLE_RATE).map_err(io::Error::other)
}

/// The text of the entry at `base` and its meta, when it has a readable one.
fn read_version(base: &Path) -> (String, Option<Meta>) {
    let text = fs::read_to_string(base.with_extension(TXT)).unwrap_or_default();
    let meta = fs::read_to_string(base.with_extension(JSON))
        .ok()
        .and_then(|json| Meta::from_json(&json).ok());
    (text, meta)
}

/// Writes `text` and `meta` of the entry at `base`, the text first.
fn write_version(base: &Path, text: &str, meta: &Meta) -> io::Result<()> {
    write_atomic(&base.with_extension(TXT), text.as_bytes())?;
    write_meta(base, meta)
}

/// A transcript of recording `id` made again: the text replaces the old one, the meta says it is
/// done and names the model, language and settings of this run. The old text and its run stay
/// as the other version (`Meta::previous`), unless there was no text.
#[cfg(test)]
pub fn set_text(dir: &Path, id: &HistoryId, text: &str, ctx: &Context) -> io::Result<()> {
    set_text_with_words(dir, id, text, ctx, Vec::new())
}

/// [`set_text`] that also stores the words of the new run (their confidences; times only when the
/// run made them). Words a timing pass made (`timings` set) stay: they describe the audio, which a
/// new run does not change. Only a confidence-only list is replaced by the new run's.
pub fn set_text_with_words(
    dir: &Path,
    id: &HistoryId,
    text: &str,
    ctx: &Context,
    words: Vec<history::Word>,
) -> io::Result<()> {
    let base = entry_base(dir, id)?;
    let (old_text, old) = read_version(&base);
    let duration_ms = old
        .as_ref()
        .map_or_else(|| wav_ms(&base.with_extension(WAV)), |m| m.duration_ms);
    let mut new = meta(id, duration_ms, Status::Done, ctx);
    // The audio does not change with the text: raw stays raw. What was held back and how the
    // dictation arrived belong to the dictation, not to one version of its text.
    if let Some(old) = &old {
        new.raw = old.raw;
        new.held.clone_from(&old.held);
        new.delivered = old.delivered;
    }
    match old.as_ref().filter(|m| m.timings.is_some()) {
        Some(timed) => {
            new.words.clone_from(&timed.words);
            new.timings.clone_from(&timed.timings);
        }
        None => new.words = words,
    }
    if !old_text.is_empty() {
        let old = old.unwrap_or_else(|| meta(id, duration_ms, Status::Done, &Context::default()));
        new.previous = Some(old.as_previous(&old_text));
    }
    write_version(&base, text, &new)
}

/// `text` the user edited replaces the transcript of recording `id`; the run that made it stays
/// and the old text is kept as the other version. No change (and no write) when it is the same.
pub fn edit_text(dir: &Path, id: &HistoryId, text: &str) -> io::Result<()> {
    let base = entry_base(dir, id)?;
    let (old_text, old) = read_version(&base);
    if old_text == text {
        return Ok(());
    }
    let duration_ms = old
        .as_ref()
        .map_or_else(|| wav_ms(&base.with_extension(WAV)), |m| m.duration_ms);
    let old = old.unwrap_or_else(|| meta(id, duration_ms, Status::Done, &Context::default()));
    let new = Meta {
        status: Status::Done,
        previous: (!old_text.is_empty()).then(|| old.as_previous(&old_text)),
        ..old
    };
    write_version(&base, text, &new)
}

/// The words of a transcript for the record: the confidence of each, and the times when the
/// engine that ran was a timing one. Empty text never becomes a word.
pub fn words_of(transcript: &oxi_whisper::Transcript) -> Vec<history::Word> {
    let ms = |t: Option<oxi_core::types::Millis>| t.map(|m| u32::try_from(m.0).unwrap_or(u32::MAX));
    transcript
        .segments
        .iter()
        .flat_map(|s| &s.words)
        .map(|w| history::Word {
            w: w.text.clone(),
            // NaN would break the equality `Meta` has, and 0 reads as "doubtful".
            p: if w.p.is_nan() {
                0.0
            } else {
                w.p.clamp(0.0, 1.0)
            },
            t0: ms(w.t0),
            t1: ms(w.t1),
        })
        .collect()
}

/// How the times of a pass were made, for `timings` in the record.
pub fn timings_of(timing: oxi_whisper::Timing, model: &str) -> history::Timings {
    match timing {
        oxi_whisper::Timing::Dtw(preset) => history::Timings {
            by: history::TimingBy::Dtw,
            preset: preset.name().to_owned(),
            model: model.to_owned(),
        },
        oxi_whisper::Timing::Tokens => history::Timings {
            by: history::TimingBy::Tokens,
            preset: String::new(),
            model: model.to_owned(),
        },
    }
}

/// The stored words of recording `id` and how their times were made (`None` while they have
/// none). Empty for a record from before words were kept.
pub fn read_words(
    dir: &Path,
    id: &HistoryId,
) -> io::Result<(Vec<history::Word>, Option<history::Timings>)> {
    let base = entry_base(dir, id)?;
    let (_, meta) = read_version(&base);
    Ok(meta.map_or_else(Default::default, |m| (m.words, m.timings)))
}

/// Stores the words a timing pass made as the words of recording `id`, with how they were made.
/// Only the meta file is written; `NotFound` when the entry has none to put them in.
pub fn store_timed_words(
    dir: &Path,
    id: &HistoryId,
    words: Vec<history::Word>,
    timings: history::Timings,
) -> io::Result<()> {
    let base = entry_base(dir, id)?;
    let (_, meta) = read_version(&base);
    let meta = meta.ok_or(io::ErrorKind::NotFound)?;
    write_meta(
        &base,
        &Meta {
            words,
            timings: Some(timings),
            ..meta
        },
    )
}

/// Swaps the transcript of recording `id` with its other version; `NotFound` when none is kept.
pub fn restore(dir: &Path, id: &HistoryId) -> io::Result<()> {
    let base = entry_base(dir, id)?;
    let (text, old) = read_version(&base);
    let (meta, text) = old
        .and_then(|m| m.swapped(&text))
        .ok_or(io::ErrorKind::NotFound)?;
    write_version(&base, &text, &meta)
}

/// Deletes recording `id` (its wav, text and meta).
#[allow(dead_code, reason = "wired to IPC by t12")]
pub fn delete(dir: &Path, id: &HistoryId) -> io::Result<()> {
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
fn entry_base(dir: &Path, id: &HistoryId) -> io::Result<PathBuf> {
    if !owned(dir) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let base = dir.join(id.as_str());
    if regular(&base.with_extension(WAV)) {
        Ok(base)
    } else {
        Err(io::ErrorKind::NotFound.into())
    }
}

#[cfg(test)]
pub(crate) mod tests;
