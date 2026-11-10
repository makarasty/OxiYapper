//! Model registry: ids, sizes, hashes, download sources (ARCHITECTURE section 8).
//!
//! Every URL is pinned to a Hugging Face commit, never `main`. Sizes and SHA256 come from
//! the HF tree API (`/api/models/<repo>/tree/<commit>`, field `lfs.oid`).

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::device::{Device, DeviceChoice, pick};
use crate::paths::part_path;

/// One curated model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelEntry {
    /// Registry id, as written in `[model] id` of the config.
    pub id: &'static str,
    pub display_name: &'static str,
    /// File name under the `models/` directory.
    pub file_name: &'static str,
    /// `https://huggingface.co/<repo>/resolve/<40-hex commit>/<file>`.
    pub url: &'static str,
    pub size: u64,
    /// Lower-case hex SHA256 of the whole file.
    pub sha256: &'static str,
}

/// Curated models. `tiny` is for tests and CI, not offered in settings.
pub const MODELS: &[ModelEntry] = &[
    ModelEntry {
        id: "turbo",
        display_name: "Whisper large-v3 turbo",
        file_name: "ggml-large-v3-turbo.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-large-v3-turbo.bin",
        size: 1_624_555_275,
        sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
    },
    ModelEntry {
        id: "large-v3",
        display_name: "Whisper large-v3",
        file_name: "ggml-large-v3.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-large-v3.bin",
        size: 3_095_033_483,
        sha256: "64d182b440b98d5203c4f9bd541544d84c605196c4f7b845dfa11fb23594d1e2",
    },
    ModelEntry {
        id: "podlodka",
        display_name: "Whisper large-v3 Russian",
        file_name: "ggml-large-v3-russian-ties-podlodka-v1.2-f16.bin",
        url: "https://huggingface.co/Pomni/whisper-large-v3-russian-ties-podlodka-v1.2-ggml-allquants/resolve/c3a2a8054130089588bd1982bf728f4507699b49/ggml-large-v3-russian-ties-podlodka-v1.2-f16.bin",
        size: 3_095_033_483,
        sha256: "ccf563aef24b4157c1fdc064b8dd9a45e2b8d9092b58c8bdf27a260f34fec487",
    },
    ModelEntry {
        id: "tiny",
        display_name: "Whisper tiny (tests and CI)",
        file_name: "ggml-tiny.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-tiny.bin",
        size: 77_691_713,
        sha256: "be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21",
    },
];

/// Registry entry for `id`; `None` for unknown ids and for `"custom"` (loaded as is, no hash).
pub fn find(id: &str) -> Option<&'static ModelEntry> {
    MODELS.iter().find(|m| m.id == id)
}

/// Registry entry stored under `file_name`; `None` means a custom model (loaded as is).
pub fn find_by_file_name(file_name: &str) -> Option<&'static ModelEntry> {
    MODELS.iter().find(|m| m.file_name == file_name)
}

/// Lower-case hex, the form the registry stores.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// SHA256 state after the whole file; a resumed download goes on hashing from it.
pub fn hash_file(path: &Path) -> io::Result<Sha256> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(hasher);
        }
        hasher.update(&buf[..n]);
    }
}

/// SHA256 of a whole file as lower-case hex.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    Ok(hex(&hash_file(path)?.finalize()))
}

/// What a `<file>.ok` stamp records about a verified file: size and modification time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub size: u64,
    /// Modification time, milliseconds since the Unix epoch.
    pub mtime_ms: u64,
}

impl Stamp {
    /// What the file at `path` is now: its size and modification time.
    pub fn of(path: &Path) -> io::Result<Self> {
        let meta = fs::metadata(path)?;
        let since_epoch = meta
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?;
        Ok(Self {
            size: meta.len(),
            mtime_ms: u64::try_from(since_epoch.as_millis()).map_err(io::Error::other)?,
        })
    }

    /// Contents of the `.ok` file: `<size> <mtime_ms>\n`.
    pub fn to_line(self) -> String {
        format!("{} {}\n", self.size, self.mtime_ms)
    }

    /// Reads [`Stamp::to_line`] back; `None` for anything else (a re-hash follows).
    pub fn parse(text: &str) -> Option<Self> {
        let mut fields = text.trim_end().split(' ');
        let size = fields.next()?.parse().ok()?;
        let mtime_ms = fields.next()?.parse().ok()?;
        fields.next().is_none().then_some(Self { size, mtime_ms })
    }
}

/// Name of the stamp file next to `file_name`.
pub fn stamp_file_name(file_name: &str) -> String {
    format!("{file_name}.ok")
}

/// Whether the file may be loaded without hashing again. `stamp` is the stored one (if
/// any), `actual` is read from the file now. A missing stamp, a changed size or mtime, or
/// a size other than the registry's means re-hash.
pub fn stamp_trusted(stamp: Option<Stamp>, actual: Stamp, entry: &ModelEntry) -> bool {
    stamp == Some(actual) && actual.size == entry.size
}

/// Records the file as verified: writes `<file>.ok` next to it with its stamp as of now.
pub fn write_stamp(file: &Path) -> io::Result<()> {
    let name = file.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "model path has no file name")
    })?;
    fs::write(
        file.with_file_name(stamp_file_name(name)),
        Stamp::of(file)?.to_line(),
    )
}

/// What the files on disk say about a registry model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelStatus {
    /// No file.
    Missing,
    /// No file, but a cancelled or cut-off download left its `.part`; the next download
    /// resumes from it.
    Partial,
    /// Right size and a stamp that still matches: load without hashing.
    Ready,
    /// Right size but no usable stamp: hash the file before the first load.
    NeedsHash,
    /// Wrong size (cut-off download, other file) or unreadable: cannot be used.
    Bad,
}

/// Status of `entry` in `models_dir`, from the file's size, its `.ok` stamp and its `.part`.
/// Reads the metadata only; the hash is the caller's step on [`ModelStatus::NeedsHash`].
pub fn status(entry: &ModelEntry, models_dir: &Path) -> ModelStatus {
    let file = models_dir.join(entry.file_name);
    let actual = match Stamp::of(&file) {
        Ok(actual) => actual,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let partial = fs::metadata(part_path(&file)).is_ok_and(|m| m.len() > 0);
            return if partial {
                ModelStatus::Partial
            } else {
                ModelStatus::Missing
            };
        }
        Err(_) => return ModelStatus::Bad,
    };
    if actual.size != entry.size {
        return ModelStatus::Bad;
    }
    let stored = fs::read_to_string(models_dir.join(stamp_file_name(entry.file_name)))
        .ok()
        .and_then(|text| Stamp::parse(&text));
    if stamp_trusted(stored, actual, entry) {
        ModelStatus::Ready
    } else {
        ModelStatus::NeedsHash
    }
}

/// A model `.part` is kept this long after its last write (a paused download), then removed.
pub const PART_KEEP: Duration = Duration::from_hours(7 * 24);

/// True when a `.part` last written at `modified` is old enough to remove at `now`. A resumed
/// download writes again, so it is young; a time in the future (clock set back) is kept.
pub fn part_expired(modified: SystemTime, now: SystemTime) -> bool {
    now.duration_since(modified)
        .is_ok_and(|age| age >= PART_KEEP)
}

/// Whole days, rounded up, a `.part` due for removal at `remove_at` is still kept at `now`; 0
/// once it is due (the next start removes it).
pub fn days_kept(remove_at: SystemTime, now: SystemTime) -> u64 {
    remove_at
        .duration_since(now)
        .map_or(0, |left| left.as_secs().div_ceil(86_400))
}

/// A registry model this large runs slowly without a dedicated GPU; turbo (1.6 GB) does not.
const HEAVY_FROM: u64 = 2_000_000_000;

/// True when `entry` will likely run badly on the device `choice` resolves to among `gpus`: a
/// heavy model on the CPU or an integrated GPU. Unknown GPUs (`None`, the daemon has not listed
/// them) never warn. Only a warning: the model can still be downloaded and used.
pub fn too_heavy(entry: &ModelEntry, gpus: Option<&[Device]>, choice: &DeviceChoice) -> bool {
    let Some(gpus) = gpus else { return false };
    let weak = pick(gpus, choice).index.is_none_or(|i| gpus[i].uma);
    weak && entry.size >= HEAVY_FROM
}

/// A partly downloaded model file, for the Model page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialDownload {
    /// The model file it belongs to, without the `.part` suffix.
    pub file_name: String,
    pub size: u64,
    /// When the startup sweep removes it unless the download goes on.
    pub remove_at: SystemTime,
}

#[cfg(test)]
#[path = "../tests/common/scratch.rs"]
mod scratch;

#[cfg(test)]
mod tests {
    use super::scratch::scratch_dir;
    use super::*;

    #[test]
    fn a_non_empty_part_file_is_partial() {
        let entry = find("tiny").unwrap();
        let dir = scratch_dir(&format!("oxi-core-partial-{}", std::process::id()));
        let part = part_path(&dir.join(entry.file_name));
        fs::write(&part, b"").unwrap();
        assert_eq!(status(entry, &dir), ModelStatus::Missing, "empty .part");
        fs::write(&part, b"half").unwrap();
        assert_eq!(status(entry, &dir), ModelStatus::Partial);
        fs::write(dir.join(entry.file_name), b"cut").unwrap();
        assert_eq!(
            status(entry, &dir),
            ModelStatus::Bad,
            "the file wins over its .part"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn days_kept_rounds_up_and_stops_at_zero() {
        let now = UNIX_EPOCH + Duration::from_hours(1000 * 24);
        let hours = |h| Duration::from_hours(h);
        for (remove_at, want) in [
            (now + PART_KEEP, 7),
            (now + hours(6 * 24 + 23), 7),
            (now + hours(1), 1),
            (now, 0),
            (now - hours(1), 0),
        ] {
            assert_eq!(days_kept(remove_at, now), want, "{remove_at:?}");
        }
    }

    #[test]
    fn heavy_models_warn_only_without_a_dedicated_gpu() {
        let gpu = |uma| Device {
            name: if uma { "Vulkan1" } else { "Vulkan0" }.into(),
            description: String::new(),
            uma,
        };
        let (discrete, integrated) = ([gpu(false)], [gpu(true)]);
        let (both, none) = ([gpu(false), gpu(true)], []);
        let (turbo, large) = (find("turbo").unwrap(), find("large-v3").unwrap());
        let named = |n: &str| DeviceChoice::Named(n.into());
        let table: [(Option<&[Device]>, DeviceChoice, bool); 7] = [
            (None, DeviceChoice::Cpu, false),
            (Some(&discrete), DeviceChoice::Auto, false),
            (Some(&both), named("Vulkan0"), false),
            (Some(&discrete), DeviceChoice::Cpu, true),
            (Some(&integrated), DeviceChoice::Auto, true),
            (Some(&both), named("Vulkan1"), true),
            (Some(&none), DeviceChoice::Auto, true),
        ];
        for (gpus, choice, heavy) in table {
            assert_eq!(
                too_heavy(large, gpus, &choice),
                heavy,
                "{gpus:?} {choice:?}"
            );
            assert!(
                !too_heavy(turbo, gpus, &choice),
                "turbo {gpus:?} {choice:?}"
            );
        }
    }
}
