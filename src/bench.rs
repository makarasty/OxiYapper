//! Hidden `oxi-yapper bench <dir> --model <path> [--gpu <name|cpu|auto>] [--lang ru|en|auto]
//! [--prompt-file <f>]`: every `*.wav` in `<dir>` through the audio pipeline, whisper and the
//! text pipeline. Prints text and timings; never touches the hotkey or the clipboard.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use oxi_core::audio::{self, downmix};
use oxi_core::config::Audio;
use oxi_core::dictionary::build_prompt;
use oxi_core::models::MODELS;
use oxi_core::text::{self, HallucinationFilter, Outcome, ReplaceRules, decode_text_file};
use oxi_core::types::Language;
use oxi_whisper::{DeviceChoice, Engine};
use sha2::{Digest, Sha256};

const USAGE: &str = "usage: oxi-yapper bench <dir> --model <path> [--gpu <name|cpu|auto>] \
                     [--lang ru|en|auto] [--prompt-file <file>]";

#[derive(Debug)]
struct Args {
    dir: PathBuf,
    model: PathBuf,
    gpu: DeviceChoice,
    lang: Language,
    prompt_file: Option<PathBuf>,
}

fn parse_args(args: &[String]) -> Result<Args> {
    let (mut dir, mut model, mut prompt_file) = (None, None, None);
    let (mut gpu, mut lang) = (DeviceChoice::Auto, Language::Auto);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--model" | "--prompt-file" | "--gpu" | "--lang" => {
                let value = it
                    .next()
                    .with_context(|| format!("{arg} needs a value\n{USAGE}"))?;
                match arg.as_str() {
                    "--model" => model = Some(PathBuf::from(value)),
                    "--prompt-file" => prompt_file = Some(PathBuf::from(value)),
                    "--gpu" => {
                        gpu = match value.as_str() {
                            "auto" => DeviceChoice::Auto,
                            "cpu" => DeviceChoice::Cpu,
                            name => DeviceChoice::Named(name.to_owned()),
                        };
                    }
                    _ => {
                        lang = match value.as_str() {
                            "ru" => Language::Ru,
                            "en" => Language::En,
                            "auto" => Language::Auto,
                            other => bail!("--lang: expected ru, en or auto, got {other:?}"),
                        };
                    }
                }
            }
            flag if flag.starts_with("--") => bail!("unknown option {flag}\n{USAGE}"),
            _ if dir.is_none() => dir = Some(PathBuf::from(arg)),
            _ => bail!("unexpected argument {arg:?}\n{USAGE}"),
        }
    }
    Ok(Args {
        dir: dir.with_context(|| format!("no directory given\n{USAGE}"))?,
        // shortcut: no default model, the models dir needs the platform paths (M4); add then.
        model: model.with_context(|| format!("--model is required\n{USAGE}"))?,
        gpu,
        lang,
        prompt_file,
    })
}

/// Nearest-rank percentile of an ascending slice, `pct` in 1..=100; 0 for an empty slice.
fn percentile(sorted: &[u128], pct: usize) -> u128 {
    match sorted.len() {
        0 => 0,
        n => sorted[(n * pct).div_ceil(100).max(1) - 1],
    }
}

/// A wav as mono f32 and its sample rate. Integer formats scale to -1..1.
fn read_wav(path: &Path) -> Result<(Vec<f32>, u32)> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    let interleaved = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<Vec<_>, _>>()?,
        hound::SampleFormat::Int => {
            let scale = (1_i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / scale))
                .collect::<Result<Vec<_>, _>>()?
        }
    };
    Ok((
        downmix(&interleaved, usize::from(spec.channels)),
        spec.sample_rate,
    ))
}

/// Refuse a file that carries a registry file name but not the registry hash. Any other
/// name is a custom model and loads as is (ARCHITECTURE 8).
fn verify_model(path: &Path) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let Some(entry) = MODELS.iter().find(|m| m.file_name == name) else {
        return Ok(());
    };
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual = hasher.finalize().iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    });
    if actual != entry.sha256 {
        bail!(
            "{name}: hash mismatch (expected {}, got {actual}); delete the file and download it again",
            entry.sha256
        );
    }
    Ok(())
}

fn wav_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .filter_map(|e| Some(e.ok()?.path()))
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("wav")))
        .collect();
    files.sort();
    Ok(files)
}

pub fn run(args: &[String]) -> Result<()> {
    let args = parse_args(args)?;
    let files = wav_files(&args.dir)?;
    if files.is_empty() {
        bail!("no .wav files in {}", args.dir.display());
    }
    let prompt = match &args.prompt_file {
        Some(p) => {
            let bytes = fs::read(p).with_context(|| format!("read {}", p.display()))?;
            build_prompt(decode_text_file(&bytes)?.lines()).text
        }
        None => String::new(),
    };

    verify_model(&args.model)?;
    let started = Instant::now();
    let mut engine = Engine::load(&args.model, &args.gpu)?;
    let load_ms = started.elapsed().as_millis();
    let info = engine.info();
    println!("device: {}", info.device.as_deref().unwrap_or("CPU"));
    if let Some(w) = &info.warning {
        println!("warning: {w}");
    }

    let (filter, rules) = (HallucinationFilter::new(""), ReplaceRules::default());
    let mut times = Vec::new();
    for path in &files {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let (mono, rate) = match read_wav(path) {
            Ok(wav) => wav,
            Err(e) => {
                println!("{name}\terror: {e:#}");
                continue;
            }
        };
        let clip = match audio::process(&mono, rate, &Audio::default()) {
            Ok(clip) => clip,
            Err(e) => {
                println!("{name}\tskipped: {e}");
                continue;
            }
        };
        let started = Instant::now();
        let transcript = engine.transcribe(&clip, args.lang, &prompt)?;
        let ms = started.elapsed().as_millis();
        times.push(ms);
        let segments: Vec<&str> = transcript
            .segments
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        let text = match text::process(&segments, &filter, &rules, false) {
            Outcome::Text(t) => t,
            Outcome::NothingHeard => "(nothing heard)".to_owned(),
        };
        println!("{name}\t{text}\tload {load_ms} ms\ttranscribe {ms} ms");
    }
    times.sort_unstable();
    println!(
        "files {}\tp50 {} ms\tp95 {} ms",
        times.len(),
        percentile(&times, 50),
        percentile(&times, 95)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oxi-bench-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_flags_in_any_order() {
        let a = parse_args(&strs(&[
            "--lang",
            "ru",
            "dir",
            "--gpu",
            "Vulkan0",
            "--model",
            "m.bin",
            "--prompt-file",
            "p.txt",
        ]))
        .unwrap();
        assert_eq!(a.dir, PathBuf::from("dir"));
        assert_eq!(a.model, PathBuf::from("m.bin"));
        assert_eq!(a.gpu, DeviceChoice::Named("Vulkan0".into()));
        assert_eq!(a.lang, Language::Ru);
        assert_eq!(a.prompt_file, Some(PathBuf::from("p.txt")));
        let a = parse_args(&strs(&["d", "--model", "m", "--gpu", "cpu"])).unwrap();
        assert_eq!((a.gpu, a.lang), (DeviceChoice::Cpu, Language::Auto));
    }

    #[test]
    fn rejects_bad_args() {
        for bad in [
            &["--model", "m"][..],
            &["d"],
            &["d", "--model"],
            &["d", "--model", "m", "--lang", "de"],
            &["d", "--model", "m", "--bogus"],
            &["d", "e", "--model", "m"],
        ] {
            assert!(parse_args(&strs(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        assert_eq!(percentile(&[], 50), 0);
        assert_eq!(percentile(&[7], 95), 7);
        let v: Vec<u128> = (1..=10).collect();
        assert_eq!((percentile(&v, 50), percentile(&v, 95)), (5, 10));
    }

    #[test]
    fn reads_stereo_int_wav_as_mono() {
        let path = scratch("wav").join("a.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for (l, r) in [(16384_i16, 0_i16), (-16384, -16384)] {
            w.write_sample(l).unwrap();
            w.write_sample(r).unwrap();
        }
        w.finalize().unwrap();
        let (mono, rate) = read_wav(&path).unwrap();
        assert_eq!((rate, mono), (8000, vec![0.25, -0.5]));
    }

    #[test]
    fn registry_named_file_with_wrong_bytes_is_refused_custom_loads() {
        let dir = scratch("model");
        let tiny = dir.join("ggml-tiny.bin");
        fs::write(&tiny, b"not the real model").unwrap();
        let err = verify_model(&tiny).unwrap_err().to_string();
        assert!(err.contains("hash mismatch"), "{err}");
        let custom = dir.join("my-model.bin");
        fs::write(&custom, b"anything").unwrap();
        verify_model(&custom).unwrap();
    }
}
