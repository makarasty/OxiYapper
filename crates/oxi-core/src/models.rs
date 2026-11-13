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
use crate::types::Language;

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

/// Curated models, in gallery order: the three main ones, their compact (quantized) files, an
/// English one, two light ones. `tiny` is for tests and CI, not offered in settings.
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
        id: "turbo-compact",
        display_name: "Whisper turbo compact",
        file_name: "ggml-large-v3-turbo-q5_0.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-large-v3-turbo-q5_0.bin",
        size: 574_041_195,
        sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
    },
    ModelEntry {
        id: "large-v3-compact",
        display_name: "Whisper large-v3 compact",
        file_name: "ggml-large-v3-q5_0.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-large-v3-q5_0.bin",
        size: 1_081_140_203,
        sha256: "d75795ecff3f83b5faa89d1900604ad8c780abd5739fae406de19f23ecd98ad1",
    },
    ModelEntry {
        id: "podlodka-compact",
        display_name: "Whisper Russian compact",
        file_name: "ggml-large-v3-russian-ties-podlodka-v1.2-q8_0.bin",
        url: "https://huggingface.co/Pomni/whisper-large-v3-russian-ties-podlodka-v1.2-ggml-allquants/resolve/c3a2a8054130089588bd1982bf728f4507699b49/ggml-large-v3-russian-ties-podlodka-v1.2-q8_0.bin",
        size: 1_656_538_283,
        sha256: "170c93338ec0525ab5e2f4dd52ef07cd8ab9af1864c1f41e87e5dae2a77b6b03",
    },
    ModelEntry {
        id: "distil-en",
        display_name: "Distil-Whisper English",
        file_name: "ggml-distil-large-v3.bin",
        url: "https://huggingface.co/distil-whisper/distil-large-v3-ggml/resolve/0d78dd96ed9fc152325f63b53788fec3b43de031/ggml-distil-large-v3.bin",
        size: 1_519_521_155,
        sha256: "2883a11b90fb10ed592d826edeaee7d2929bf1ab985109fe9e1e7b4d2b69a298",
    },
    ModelEntry {
        id: "small",
        display_name: "Whisper small",
        file_name: "ggml-small.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-small.bin",
        size: 487_601_967,
        sha256: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
    },
    ModelEntry {
        id: "base",
        display_name: "Whisper base",
        file_name: "ggml-base.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-base.bin",
        size: 147_951_465,
        sha256: "60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe",
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

/// What a model is for, one glyph each (DS-24: no two classes share one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelClass {
    Accurate,
    Fast,
    Language,
    Small,
}

impl ModelClass {
    /// The place of the class in the page's glyph list; "own file" comes after the last class.
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// What a model card says besides its size: its class, how fast it is, the languages it hears,
/// the memory it needs and what it is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    pub class: ModelClass,
    pub speed: &'static str,
    pub languages: &'static str,
    /// The one language it is tuned for; `None` hears all of Whisper's 99.
    pub only: Option<Language>,
    /// Memory it needs to run, in bytes: graphics memory on a graphics card, else system memory.
    /// The whisper.cpp README's table where it has the size, else file size plus the compute
    /// buffers of the same architecture (b04 research).
    pub memory: u64,
    /// What it is best for, in a few plain words.
    pub note: &'static str,
}

const MB: u64 = 1_000_000;

/// The facts of registry model `id`; an id the registry does not know reads as a small model.
pub fn facts(id: &str) -> Facts {
    let (class, speed, only, memory, note) = match id {
        "turbo" => (
            ModelClass::Fast,
            "Fast",
            None,
            2300 * MB,
            "Fast and nearly as accurate as large-v3: good for most",
        ),
        "turbo-compact" => (
            ModelClass::Fast,
            "Fast",
            None,
            1300 * MB,
            "Turbo in a smaller file, for a graphics card with little memory or a PC without one",
        ),
        "large-v3" => (
            ModelClass::Accurate,
            "Slower",
            None,
            3900 * MB,
            "The most accurate; wants a strong graphics card",
        ),
        "large-v3-compact" => (
            ModelClass::Accurate,
            "Medium",
            None,
            2100 * MB,
            "Large-v3 in a smaller file: close in accuracy, half the memory",
        ),
        "podlodka" => (
            ModelClass::Language,
            "Slower",
            Some(Language::RU),
            3900 * MB,
            "Tuned for Russian, the most accurate there",
        ),
        "podlodka-compact" => (
            ModelClass::Language,
            "Medium",
            Some(Language::RU),
            2700 * MB,
            "The Russian model in a smaller file, nearly twice as fast",
        ),
        "distil-en" => (
            ModelClass::Language,
            "Fast",
            Some(Language::EN),
            2200 * MB,
            "English only, fast and accurate there",
        ),
        "small" => (
            ModelClass::Small,
            "Fastest",
            None,
            900 * MB,
            "Light enough for a PC without a graphics card; less accurate",
        ),
        "base" => (
            ModelClass::Small,
            "Fastest",
            None,
            400 * MB,
            "The lightest, for a weak PC; the least accurate",
        ),
        _ => (ModelClass::Small, "Fastest", None, 300 * MB, ""),
    };
    let languages = match only {
        Some(Language::RU) => "Russian",
        Some(Language::EN) => "English",
        _ => "99 languages",
    };
    Facts {
        class,
        speed,
        languages,
        only,
        memory,
        note,
    }
}

/// A graphics adapter as the OS lists it: its name and its own (dedicated) memory in bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adapter {
    pub name: String,
    pub vram: u64,
}

/// What the settings know about the machine; `None` is a value that could not be read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Machine {
    /// Total system memory.
    pub ram: Option<u64>,
    /// Graphics memory of the device the model runs on ([`graphics_memory`]); `Some(0)` on the
    /// processor or an integrated GPU, which share the system memory.
    pub vram: Option<u64>,
    /// Free space on the drive of the models folder.
    pub free_disk: Option<u64>,
}

/// The graphics memory a model gets where `choice` runs it among the daemon's `gpus`: the listed
/// adapter of the same name (case aside), else the only adapter with memory of its own. With two
/// such cards and no name match it is `None`: no badge rather than one judged on the wrong card.
/// `Some(0)` on the processor or an integrated GPU; `None` while the GPUs are not listed or no
/// adapter says its memory.
pub fn graphics_memory(
    gpus: Option<&[Device]>,
    choice: &DeviceChoice,
    adapters: &[Adapter],
) -> Option<u64> {
    let gpus = gpus?;
    let Some(gpu) = pick(gpus, choice)
        .index
        .map(|i| &gpus[i])
        .filter(|d| !d.uma)
    else {
        return Some(0);
    };
    let name = gpu.description.trim();
    let named = adapters
        .iter()
        .find(|a| !name.is_empty() && a.name.trim().eq_ignore_ascii_case(name));
    let mut own = adapters.iter().filter(|a| a.vram > 0);
    let only = match (own.next(), own.next()) {
        (Some(a), None) => Some(a),
        _ => None,
    };
    named.or(only).map(|a| a.vram).filter(|&v| v > 0)
}

/// The model to recommend for this machine and dictation language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recommendation {
    pub id: &'static str,
    /// Judged for a graphics card's memory, not the processor and system memory.
    pub gpu: bool,
}

/// The registry model that suits `machine` best for `language`, or `None` when the hardware is
/// not known well enough to say (no badge rather than a wrong one) or nothing fits. On a graphics
/// card a model fits in 90 % of its memory; on the processor in half the RAM, and only the light
/// models qualify (a full large decoder is slow there). What a model still has to download (its
/// size less the bytes `on_disk` says it has: the whole file, a paused `.part` or 0) must fit the
/// free space ([`disk_short`]); unknown free space does not rule one out.
pub fn recommend(
    machine: &Machine,
    language: Language,
    on_disk: impl Fn(&str) -> u64,
) -> Option<Recommendation> {
    let vram = machine.vram?;
    let gpu = vram > 0;
    let budget = if gpu { vram / 10 * 9 } else { machine.ram? / 2 };
    let ladder: &[&str] = match (gpu, language == Language::RU) {
        (true, true) => &[
            "podlodka",
            "podlodka-compact",
            "turbo-compact",
            "small",
            "base",
        ],
        (true, false) => &["turbo", "turbo-compact", "small", "base"],
        (false, _) => &["turbo-compact", "small", "base"],
    };
    ladder
        .iter()
        .filter_map(|id| find(id))
        .find(|e| {
            facts(e.id).memory <= budget
                && disk_short(e, on_disk(e.id), machine.free_disk).is_none()
        })
        .map(|e| Recommendation { id: e.id, gpu })
}

/// The bytes a download of `entry` still needs when the drive has less free (`part` bytes of it
/// are on disk already); `None` when it fits or the free space is unknown.
pub fn disk_short(entry: &ModelEntry, part: u64, free: Option<u64>) -> Option<u64> {
    let need = entry.size.saturating_sub(part);
    free.filter(|&f| f < need).map(|_| need)
}

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

/// True when `entry` will likely run badly on the device `choice` resolves to among `gpus`: a
/// model with the full large decoder (speed "Medium" or "Slower", whatever its file size) on the
/// CPU or an integrated GPU; turbo, distil and the light ones do not. Unknown GPUs (`None`, the
/// daemon has not listed them) never warn. Only a warning: the model can still be downloaded and
/// used.
pub fn too_heavy(entry: &ModelEntry, gpus: Option<&[Device]>, choice: &DeviceChoice) -> bool {
    let Some(gpus) = gpus else { return false };
    let weak = pick(gpus, choice).index.is_none_or(|i| gpus[i].uma);
    weak && matches!(facts(entry.id).speed, "Medium" | "Slower")
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
    fn every_offered_model_has_facts_of_its_own_and_classes_keep_their_glyph_order() {
        for m in MODELS.iter().filter(|m| m.id != "tiny") {
            let f = facts(m.id);
            assert!(!f.note.is_empty(), "{}: no note", m.id);
            assert!(f.memory > m.size / 2, "{}: memory below the file", m.id);
        }
        assert_eq!(facts("podlodka").languages, "Russian");
        assert_eq!(facts("distil-en").only, Some(Language::EN));
        assert_eq!(facts("tiny").class, ModelClass::Small);
        assert_eq!(ModelClass::Small.index(), 3);
    }

    fn rec(machine: Machine, language: Language) -> Option<&'static str> {
        recommend(&machine, language, |_| 0).map(|r| r.id)
    }

    const fn gb(n: u64) -> u64 {
        n * 1_000_000_000
    }

    fn card(vram: u64) -> Machine {
        Machine {
            ram: Some(gb(16)),
            vram: Some(vram),
            free_disk: Some(gb(100)),
        }
    }

    #[test]
    fn the_recommendation_follows_graphics_memory_and_language() {
        for (vram, language, want) in [
            (gb(12), Language::AUTO, Some("turbo")),
            (gb(12), Language::EN, Some("turbo")),
            (gb(12), Language::RU, Some("podlodka")),
            // 3.9 GB does not fit 90 % of 4 GB; the compact Russian (2.7 GB) does.
            (gb(4), Language::RU, Some("podlodka-compact")),
            // Low VRAM: turbo (2.3 GB) does not fit 2 GB, its compact file does.
            (gb(2), Language::AUTO, Some("turbo-compact")),
            (gb(2), Language::RU, Some("turbo-compact")),
            (gb(1), Language::AUTO, Some("small")),
            (600_000_000, Language::AUTO, Some("base")),
            (200_000_000, Language::AUTO, None),
        ] {
            assert_eq!(rec(card(vram), language), want, "{vram} {language:?}");
        }
    }

    #[test]
    fn without_a_graphics_card_only_light_models_are_recommended_by_ram() {
        let cpu = |ram| Machine {
            ram: Some(ram),
            vram: Some(0),
            free_disk: None,
        };
        for (ram, want) in [
            (gb(64), Some("turbo-compact")),
            (gb(8), Some("turbo-compact")),
            (gb(2), Some("small")),
            (gb(1), Some("base")),
            (500_000_000, None),
        ] {
            assert_eq!(rec(cpu(ram), Language::RU), want, "{ram}");
            assert_eq!(rec(cpu(ram), Language::AUTO), want, "{ram}");
        }
        let unknown_ram = Machine {
            ram: None,
            ..cpu(0)
        };
        assert_eq!(rec(unknown_ram, Language::AUTO), None, "RAM unknown");
    }

    #[test]
    fn unknown_hardware_gets_no_badge() {
        let unknown = Machine {
            ram: Some(gb(16)),
            vram: None,
            free_disk: Some(gb(100)),
        };
        assert_eq!(rec(unknown, Language::AUTO), None);
        assert_eq!(rec(Machine::default(), Language::RU), None);
    }

    #[test]
    fn low_disk_skips_what_does_not_fit_unless_it_is_downloaded() {
        let low = Machine {
            free_disk: Some(gb(1)),
            ..card(gb(12))
        };
        assert_eq!(rec(low, Language::RU), Some("turbo-compact"));
        assert_eq!(rec(low, Language::AUTO), Some("turbo-compact"));
        let have = |id: &str| {
            if id == "podlodka" {
                find(id).unwrap().size
            } else {
                0
            }
        };
        assert_eq!(
            recommend(&low, Language::RU, have).map(|r| r.id),
            Some("podlodka"),
            "a downloaded model needs no room"
        );
        // Review b04: a paused podlodka-compact with all but 0.46 GB on disk fits 1 GB free on a
        // 4 GB card, so it is recommended (and its Resume stays on), not turbo-compact.
        let compact = find("podlodka-compact").unwrap();
        let paused = |id: &str| {
            if id == compact.id {
                compact.size - 460_000_000
            } else {
                0
            }
        };
        let small_card = Machine {
            free_disk: Some(gb(1)),
            ..card(gb(4))
        };
        assert_eq!(rec(small_card, Language::RU), Some("turbo-compact"));
        assert_eq!(
            recommend(&small_card, Language::RU, paused).map(|r| r.id),
            Some("podlodka-compact"),
            "a paused download needs only the rest"
        );
        assert_eq!(
            disk_short(compact, compact.size - 460_000_000, Some(gb(1))),
            None
        );
        let full = Machine {
            free_disk: Some(100_000_000),
            ..card(gb(12))
        };
        assert_eq!(rec(full, Language::AUTO), None, "nothing fits the disk");
        assert!(recommend(&card(gb(12)), Language::AUTO, |_| 0).is_some_and(|r| r.gpu));
    }

    #[test]
    fn disk_short_counts_what_is_already_downloaded() {
        let turbo = find("turbo").unwrap();
        assert_eq!(disk_short(turbo, 0, None), None, "unknown free space");
        assert_eq!(disk_short(turbo, 0, Some(gb(10))), None);
        assert_eq!(disk_short(turbo, 0, Some(gb(1))), Some(turbo.size));
        assert_eq!(
            disk_short(turbo, turbo.size - 500_000_000, Some(gb(1))),
            None,
            "a paused download needs only the rest"
        );
    }

    #[test]
    fn graphics_memory_is_that_of_the_card_the_model_runs_on() {
        let dev = |name: &str, description: &str, uma| Device {
            name: name.into(),
            description: description.into(),
            uma,
        };
        let adapter = |name: &str, vram| Adapter {
            name: name.into(),
            vram,
        };
        let gpus = [
            dev("Vulkan0", "RTX", false),
            dev("Vulkan1", "Intel UHD", true),
        ];
        let adapters = [adapter("Intel UHD", gb(0)), adapter("RTX", gb(8))];
        let named = |n: &str| DeviceChoice::Named(n.into());
        assert_eq!(graphics_memory(None, &DeviceChoice::Auto, &adapters), None);
        assert_eq!(
            graphics_memory(Some(&gpus), &DeviceChoice::Auto, &adapters),
            Some(gb(8))
        );
        assert_eq!(
            graphics_memory(Some(&gpus), &DeviceChoice::Cpu, &adapters),
            Some(0)
        );
        assert_eq!(
            graphics_memory(Some(&gpus), &named("Vulkan1"), &adapters),
            Some(0),
            "an integrated GPU shares the RAM"
        );
        assert_eq!(
            graphics_memory(Some(&[]), &DeviceChoice::Auto, &[]),
            Some(0)
        );
        // A driver name the OS spells otherwise: the only card with memory of its own.
        let other = [dev("Vulkan0", "NVIDIA RTX", false)];
        assert_eq!(
            graphics_memory(Some(&other), &DeviceChoice::Auto, &adapters),
            Some(gb(8))
        );
        assert_eq!(
            graphics_memory(
                Some(&[dev("Vulkan0", "rtx ", false)]),
                &DeviceChoice::Auto,
                &adapters
            ),
            Some(gb(8)),
            "case and spaces aside"
        );
        // Review b04: two cards of their own, "Run on" names the 8 GB one in a spelling the OS
        // does not use: no guess at the 24 GB one.
        let two = [adapter("RTX 3070", gb(8)), adapter("RTX 4090", gb(24))];
        let named_8 = [dev("Vulkan0", "NVIDIA GeForce RTX 3070 (driver)", false)];
        assert_eq!(
            graphics_memory(Some(&named_8), &DeviceChoice::Auto, &two),
            None
        );
        assert_eq!(
            graphics_memory(
                Some(&[dev("Vulkan0", "RTX 3070", false)]),
                &DeviceChoice::Auto,
                &two
            ),
            Some(gb(8))
        );
        assert_eq!(
            graphics_memory(Some(&other), &DeviceChoice::Auto, &[]),
            None,
            "no adapter listed"
        );
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
