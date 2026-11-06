//! Model registry: ids, sizes, hashes, download sources (ARCHITECTURE section 8).
//!
//! Every URL is pinned to a Hugging Face commit, never `main`. Sizes and SHA256 come from
//! the HF tree API (`/api/models/<repo>/tree/<commit>`, field `lfs.oid`).

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
        display_name: "Whisper large-v3 Russian (podlodka v1.2, f16)",
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

/// What a `<file>.ok` stamp records about a verified file: size and modification time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub size: u64,
    /// Modification time, milliseconds since the Unix epoch.
    pub mtime_ms: u64,
}

impl Stamp {
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
