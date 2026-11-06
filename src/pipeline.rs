//! Record, transcribe and paste glue shared by the daemon and bench (ARCHITECTURE sections 9 and 10).
//!
//! Model path and check, the dictionary prompt and replace rules, and the text steps after
//! whisper. The daemon's whisper worker and `bench` both call these, so they cannot drift.

#![cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "only the Windows daemon's whisper worker calls these"
    )
)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use oxi_core::config::Model;
use oxi_core::dictionary::build_prompt;
use oxi_core::machine::ModelFailure;
use oxi_core::models::{
    ModelEntry, ModelStatus, find, find_by_file_name, sha256_file, status, write_stamp,
};
use oxi_core::paths::Layout;
use oxi_core::text::{HallucinationFilter, Outcome, ReplaceRules, decode_text_file, process};
use oxi_whisper::Transcript;

/// Where the configured model lives and, for a registry model, what it must hash to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    pub path: PathBuf,
    /// `None` for a custom model: loaded as is.
    pub entry: Option<&'static ModelEntry>,
}

/// `[model] id` (a registry id or `"custom"` with `custom_path`) to a file. Missing when the
/// id is unknown, the custom path is empty or the file is not there.
pub fn resolve_model(model: &Model, models_dir: &Path) -> Result<ResolvedModel, ModelFailure> {
    let resolved = if model.id == "custom" {
        let path = model.custom_path.trim();
        if path.is_empty() {
            return Err(ModelFailure::Missing);
        }
        ResolvedModel {
            path: PathBuf::from(path),
            entry: None,
        }
    } else {
        let entry = find(&model.id).ok_or(ModelFailure::Missing)?;
        ResolvedModel {
            path: models_dir.join(entry.file_name),
            entry: Some(entry),
        }
    };
    if resolved.path.is_file() {
        Ok(resolved)
    } else {
        Err(ModelFailure::Missing)
    }
}

/// A registry-named file whose bytes are not the registry's.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("hash {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "{name}: hash mismatch (expected {expected}, got {actual}); delete the file and download it again"
    )]
    Mismatch {
        name: &'static str,
        expected: &'static str,
        actual: String,
    },
}

fn hash_matches(path: &Path, entry: &ModelEntry) -> Result<(), VerifyError> {
    let actual = sha256_file(path).map_err(|source| VerifyError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if actual == entry.sha256 {
        Ok(())
    } else {
        Err(VerifyError::Mismatch {
            name: entry.file_name,
            expected: entry.sha256,
            actual,
        })
    }
}

/// Full hash of a file that carries a registry file name; any other name is a custom model
/// and loads as is (ARCHITECTURE 8). `bench` calls this on every run.
pub fn verify_hash(path: &Path) -> Result<(), VerifyError> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    find_by_file_name(name).map_or(Ok(()), |entry| hash_matches(path, entry))
}

/// The daemon's check before a load: a registry model is trusted on a matching `.ok` stamp,
/// else hashed once and stamped. Custom files are not hashed. Failures carry no text: the
/// reason goes to the log by kind only.
pub fn verify_stamped(model: &ResolvedModel, models_dir: &Path) -> Result<(), ModelFailure> {
    let Some(entry) = model.entry else {
        return Ok(());
    };
    match status(entry, models_dir) {
        ModelStatus::Ready => Ok(()),
        ModelStatus::Missing => Err(ModelFailure::Missing),
        ModelStatus::Bad => Err(ModelFailure::Bad),
        ModelStatus::NeedsHash => {
            hash_matches(&model.path, entry).map_err(|e| {
                tracing::warn!("model {} failed its hash check: {e}", entry.id);
                ModelFailure::Bad
            })?;
            if let Err(e) = write_stamp(&model.path) {
                tracing::warn!("cannot write the stamp of model {}: {e}", entry.id);
            }
            Ok(())
        }
    }
}

/// The user's text assets: the dictionary prompt for whisper, the replace rules and the
/// hallucination filter.
#[derive(Debug, Clone, Default)]
pub struct TextAssets {
    pub prompt: String,
    filter: HallucinationFilter,
    rules: ReplaceRules,
}

/// File contents; a missing file is empty, an unreadable one is logged (kind only) and empty.
fn read_asset(path: &Path) -> String {
    match fs::read(path) {
        Ok(bytes) => decode_text_file(&bytes).unwrap_or_else(|e| {
            tracing::warn!("{}: {e}", path.display());
            String::new()
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            tracing::warn!("{}: {e}", path.display());
            String::new()
        }
    }
}

impl TextAssets {
    /// Read `dictionary.txt` and `replace.txt` from the config directory. Lines that do not
    /// fit the prompt or are not rules are logged by number, never by content.
    pub fn load(layout: &Layout) -> Self {
        let dictionary = read_asset(&layout.dictionary_file());
        let prompt = build_prompt(dictionary.lines());
        if !prompt.dropped.is_empty() {
            tracing::warn!(
                "dictionary: {} lines past the prompt limit are not sent",
                prompt.dropped.len()
            );
        }
        let parsed = ReplaceRules::parse(&read_asset(&layout.replace_file()));
        if !parsed.bad_lines.is_empty() {
            tracing::warn!("replace.txt: ignored lines {:?}", parsed.bad_lines);
        }
        Self {
            prompt: prompt.text,
            filter: HallucinationFilter::default(),
            rules: parsed.rules,
        }
    }

    /// Only a prompt, no replace rules: what `bench --prompt-file` runs with.
    pub fn with_prompt(prompt: String) -> Self {
        Self {
            prompt,
            ..Self::default()
        }
    }
}

/// Whisper segments to the text to paste: newlines, hallucinations, replace rules, trailing space.
pub fn finish(transcript: &Transcript, assets: &TextAssets, trailing_space: bool) -> Outcome {
    let segments: Vec<&str> = transcript
        .segments
        .iter()
        .map(|s| s.text.as_str())
        .collect();
    process(&segments, &assets.filter, &assets.rules, trailing_space)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxi_core::device::DeviceChoice;
    use oxi_core::models::stamp_file_name;
    use oxi_core::paths::{BaseDirs, Os, layout};
    use oxi_core::types::Millis;
    use oxi_whisper::Segment;

    type Case = (Model, Result<(PathBuf, bool), ModelFailure>);

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oxi-pipeline-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn model(id: &str, custom: &str) -> Model {
        Model {
            id: id.into(),
            custom_path: custom.into(),
            gpu: DeviceChoice::Auto,
            ..Model::default()
        }
    }

    fn transcript(texts: &[&str]) -> Transcript {
        Transcript {
            segments: texts
                .iter()
                .map(|t| Segment {
                    text: (*t).into(),
                    t0: Millis(0),
                    t1: Millis(1),
                    no_speech_prob: 0.0,
                })
                .collect(),
        }
    }

    #[test]
    fn model_resolution_table() {
        let dir = scratch("resolve");
        let tiny = dir.join("ggml-tiny.bin");
        fs::write(&tiny, b"x").unwrap();
        let custom = dir.join("mine.bin");
        fs::write(&custom, b"x").unwrap();
        let custom_s = custom.to_str().unwrap().to_owned();

        let cases: Vec<Case> = vec![
            (model("tiny", ""), Ok((tiny, true))),
            (model("turbo", ""), Err(ModelFailure::Missing)),
            (model("nonsense", ""), Err(ModelFailure::Missing)),
            (model("custom", &custom_s), Ok((custom, false))),
            (
                model("custom", &format!("  {custom_s} ")),
                Ok((PathBuf::from(&custom_s), false)),
            ),
            (model("custom", ""), Err(ModelFailure::Missing)),
            (
                model("custom", dir.join("gone.bin").to_str().unwrap()),
                Err(ModelFailure::Missing),
            ),
        ];
        for (m, want) in cases {
            let got = resolve_model(&m, &dir).map(|r| (r.path, r.entry.is_some()));
            assert_eq!(got, want, "{} {:?}", m.id, m.custom_path);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn registry_named_file_with_wrong_bytes_is_refused_custom_loads() {
        let dir = scratch("model");
        let tiny = dir.join("ggml-tiny.bin");
        fs::write(&tiny, b"not the real model").unwrap();
        let err = verify_hash(&tiny).unwrap_err().to_string();
        assert!(err.contains("hash mismatch"), "{err}");
        let custom = dir.join("my-model.bin");
        fs::write(&custom, b"anything").unwrap();
        verify_hash(&custom).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stamped_verify_trusts_a_stamp_and_refuses_a_wrong_hash() {
        let entry = find("tiny").unwrap();
        let dir = scratch("stamped");
        let file = dir.join(entry.file_name);
        let resolved = ResolvedModel {
            path: file.clone(),
            entry: Some(entry),
        };
        assert_eq!(verify_stamped(&resolved, &dir), Err(ModelFailure::Missing));

        // Right size, wrong bytes: no stamp, so it is hashed and refused; no stamp is written.
        fs::File::create(&file)
            .unwrap()
            .set_len(entry.size)
            .unwrap();
        assert_eq!(verify_stamped(&resolved, &dir), Err(ModelFailure::Bad));
        assert!(!dir.join(stamp_file_name(entry.file_name)).exists());

        // A matching stamp is trusted without reading the bytes.
        write_stamp(&file).unwrap();
        assert_eq!(verify_stamped(&resolved, &dir), Ok(()));

        // Wrong size is Bad before any hashing.
        fs::write(&file, b"cut").unwrap();
        assert_eq!(verify_stamped(&resolved, &dir), Err(ModelFailure::Bad));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn custom_model_is_not_hashed() {
        let resolved = ResolvedModel {
            path: PathBuf::from("any.bin"),
            entry: None,
        };
        assert_eq!(verify_stamped(&resolved, Path::new(".")), Ok(()));
    }

    #[test]
    fn finish_filters_replaces_and_adds_the_space() {
        let dir = scratch("assets");
        let base = BaseDirs {
            config: dir.join("c"),
            data: dir.join("d"),
            logs: dir.join("l"),
        };
        let lay = layout(Os::Windows, &base, "x");
        // No files yet: empty assets.
        let empty = TextAssets::load(&lay);
        assert_eq!(empty.prompt, "");

        fs::create_dir_all(&lay.config_dir).unwrap();
        fs::write(lay.dictionary_file(), "Foo\nBar\n").unwrap();
        fs::write(lay.replace_file(), "teh => the\nnot a rule\n").unwrap();
        let assets = TextAssets::load(&lay);
        assert_eq!(assets.prompt, "Foo, Bar");

        let t = transcript(&["  teh cat\n", "Thanks for watching.", ""]);
        assert_eq!(finish(&t, &assets, true), Outcome::Text("the cat ".into()));
        assert_eq!(finish(&t, &assets, false), Outcome::Text("the cat".into()));
        assert_eq!(
            finish(&transcript(&[" "]), &assets, true),
            Outcome::NothingHeard
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
