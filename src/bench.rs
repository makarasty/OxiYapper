//! Hidden `OxiYapper bench <dir> --model <path> [--gpu <name|cpu|auto>] [--lang auto|<code>]
//! [--prompt-file <f>]`: every `*.wav` in `<dir>` through the audio pipeline, whisper and the
//! text pipeline. Prints text and timings; never touches the hotkey or the clipboard.
//!
//! Hidden A/B flags (not in the usage text): `--no-trim`, `--no-normalize`, `--denoise`,
//! `--cut-end-ms <n>` (drop the last n ms of each clip, a shorter release tail) and `--vram`
//! (print `nvidia-smi` memory before load, after load, after the run and after unload).
//!
//! Accuracy guard of `audio_ctx` (arch-plan 7.3): `--history` reads `<dir>` as a History folder
//! (read only; records with status done, `--min-s`/`--max-s`/`--limit` pick them, the stored wav
//! goes in unprocessed) and `--ctx both` runs each record with `Full` and with `Fit` against the
//! stored text. `--no-text` prints no transcript, only counts, timings and error rates.
//!
//! Word times: `--history --timings live|tokens|turbo|large-v3|tiny` scores the times of the records (whisper's token
//! timestamps, or DTW with that preset) against the voice gate. Numbers only (arch-plan 8.2).
//!
//! `OxiYapper bench --record <seconds> <out.wav>` records the default microphone instead. The
//! clip is raw: 16 kHz mono through the capture pipeline with trim and normalise off.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use oxi_core::audio;
use oxi_core::config::{Audio, AudioCtx};
use oxi_core::device::DeviceChoice;
use oxi_core::history::{self, Meta, Status};
use oxi_core::strings;
use oxi_core::text::Outcome;
use oxi_core::types::{Language, Millis};
use oxi_platform::capture::{Capture, CaptureEvent, CpalCapture, LevelMeter};
use oxi_platform::wav::{read_wav_mono_f32, write_wav};
use oxi_whisper::{Engine, Timing, dtw_preset};

use crate::pipeline::{self, TextAssets};

/// Usage text; a `Display` constant so `{USAGE}` still works while the name comes from `APP_NAME`.
struct Usage;
const USAGE: Usage = Usage;

impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = oxi_core::APP_NAME;
        write!(
            f,
            "usage: {name} bench <dir> --model <path> [--gpu <name|cpu|auto>] \
             [--lang auto|<code>] [--prompt-file <file>]\n       \
             {name} bench --record <seconds> <out.wav>  (raw 16 kHz mono: no trim, no normalise)"
        )
    }
}

#[derive(Debug)]
struct Args {
    dir: PathBuf,
    model: PathBuf,
    gpu: DeviceChoice,
    lang: Language,
    prompt_file: Option<PathBuf>,
    audio: Audio,
    cut_end_ms: u32,
    vram: bool,
    no_text: bool,
    history: bool,
    ctx: Ctx,
    /// Seconds of audio a `--history` record may have: at least, at most.
    min_s: u64,
    max_s: u64,
    limit: usize,
    /// `--timings`: score word times of the History records instead of transcribing them.
    timings: Option<BenchTiming>,
}

/// What `--timings` scores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BenchTiming {
    /// Token times on the normal engine (flash attention as the app loads it).
    Live,
    /// A timing engine of its own ([`Engine::load_timing`]).
    Method(Timing),
}

/// Which `audio_ctx` runs: `both` is the accuracy guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Full,
    Fit,
    Both,
}

fn parse_args(args: &[String]) -> Result<Args> {
    let (mut dir, mut model, mut prompt_file) = (None, None, None);
    let (mut gpu, mut lang) = (DeviceChoice::Auto, Language::AUTO);
    let (mut audio, mut cut_end_ms, mut vram) = (Audio::default(), 0, false);
    let (mut no_text, mut history) = (false, false);
    let (mut ctx, mut min_s, mut max_s, mut limit) = (Ctx::Full, 0, u64::MAX, 20);
    let mut timings = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--no-trim" => audio.trim = false,
            "--no-normalize" => audio.normalize = false,
            "--denoise" => audio.denoise = true,
            "--vram" => vram = true,
            "--no-text" => no_text = true,
            "--history" => history = true,
            "--model" | "--prompt-file" | "--gpu" | "--lang" | "--cut-end-ms" | "--ctx"
            | "--min-s" | "--max-s" | "--limit" | "--timings" => {
                let value = it
                    .next()
                    .with_context(|| format!("{arg} needs a value\n{USAGE}"))?;
                match arg.as_str() {
                    "--model" => model = Some(PathBuf::from(value)),
                    "--prompt-file" => prompt_file = Some(PathBuf::from(value)),
                    "--gpu" => gpu = DeviceChoice::from(value.clone()),
                    "--cut-end-ms" => cut_end_ms = value.parse().context("--cut-end-ms")?,
                    "--min-s" => min_s = value.parse().context("--min-s")?,
                    "--max-s" => max_s = value.parse().context("--max-s")?,
                    "--limit" => limit = value.parse().context("--limit")?,
                    "--timings" => timings = Some(parse_timing(value)?),
                    "--ctx" => {
                        ctx = match value.as_str() {
                            "full" => Ctx::Full,
                            "fit" => Ctx::Fit,
                            "both" => Ctx::Both,
                            _ => bail!("--ctx is full, fit or both\n{USAGE}"),
                        }
                    }
                    _ => lang = value.parse().context("--lang")?,
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
        audio,
        cut_end_ms,
        vram,
        no_text,
        history,
        ctx,
        min_s,
        max_s,
        limit,
        timings,
    })
}

/// MiB of GPU memory in use, from `nvidia-smi`; `None` when the tool is missing.
fn vram_mib() -> Option<u32> {
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=memory.used", "--format=csv,noheader,nounits"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

fn print_vram(args: &Args, when: &str) {
    if args.vram {
        println!("vram {when}: {} MiB", vram_mib().map_or(-1, i64::from));
    }
}

/// Nearest-rank percentile of an ascending slice, `pct` in 1..=100; 0 for an empty slice.
fn percentile(sorted: &[u128], pct: usize) -> u128 {
    match sorted.len() {
        0 => 0,
        n => sorted[(n * pct).div_ceil(100).max(1) - 1],
    }
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

/// A `--history` record: the stored wav (already processed when the app saved it) and its text.
struct Record {
    wav: PathBuf,
    reference: String,
}

/// Newest-first records of the History folder `args.dir` that finished (`done`), have text and
/// last `min_s..=max_s` seconds. Read only: json and txt are read in place, nothing is copied.
fn history_records(args: &Args) -> Result<Vec<Record>> {
    let mut days: Vec<PathBuf> = fs::read_dir(&args.dir)
        .with_context(|| format!("read {}", args.dir.display()))?
        .filter_map(|e| Some(e.ok()?.path()))
        .filter(|p| {
            p.is_dir()
                && history::day_dir(&p.file_name().unwrap_or_default().to_string_lossy()).is_some()
        })
        .collect();
    days.sort_unstable_by(|a, b| b.cmp(a));
    let range = args.min_s.saturating_mul(1000)..=args.max_s.saturating_mul(1000);
    let mut out = Vec::new();
    for day in days {
        let mut jsons: Vec<PathBuf> = fs::read_dir(&day)?
            .filter_map(|e| Some(e.ok()?.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        jsons.sort_unstable_by(|a, b| b.cmp(a));
        for json in jsons {
            if out.len() >= args.limit {
                return Ok(out);
            }
            let Some(meta) = fs::read_to_string(&json)
                .ok()
                .and_then(|t| Meta::from_json(&t).ok())
            else {
                continue;
            };
            let Ok(reference) = fs::read_to_string(json.with_extension("txt")) else {
                continue;
            };
            let wav = json.with_extension("wav");
            if meta.status == Status::Done
                && range.contains(&meta.duration_ms)
                && !reference.trim().is_empty()
                && wav.is_file()
            {
                out.push(Record { wav, reference });
            }
        }
    }
    Ok(out)
}

/// Lower case, letters and digits only, single spaces: what the error rates compare.
fn normalized(s: &str) -> String {
    let kept: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn edit_distance(a: &[char], b: &[char]) -> usize {
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut diag = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let next = (row[j + 1] + 1)
                .min(row[j] + 1)
                .min(diag + usize::from(ca != cb));
            diag = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

/// Character error rate of `hyp` against `reference`, after [`normalized`]; 1.0 for a missing
/// hypothesis, 0.0 when both are empty.
#[allow(
    clippy::cast_precision_loss,
    reason = "text lengths are far below 2^52"
)]
fn cer(reference: &str, hyp: &str) -> f64 {
    let (r, h): (Vec<char>, Vec<char>) = (
        normalized(reference).chars().collect(),
        normalized(hyp).chars().collect(),
    );
    if r.is_empty() {
        return f64::from(u8::from(!h.is_empty()));
    }
    edit_distance(&r, &h) as f64 / r.len() as f64
}

fn bag(s: &str) -> HashMap<String, usize> {
    let mut bag = HashMap::new();
    for w in normalized(s).split_whitespace() {
        *bag.entry(w.to_owned()).or_insert(0) += 1;
    }
    bag
}

fn take(bag: &mut HashMap<String, usize>, word: &str) -> bool {
    bag.get_mut(word).is_some_and(|n| {
        *n > 0 && {
            *n -= 1;
            true
        }
    })
}

/// Words of `reference` that `full` got and `fit` did not (the guard allows at most one a record).
fn lost_words(reference: &str, full: &str, fit: &str) -> usize {
    let (mut full, mut fit) = (bag(full), bag(fit));
    normalized(reference)
        .split_whitespace()
        .filter(|w| take(&mut full, w) && !take(&mut fit, w))
        .count()
}

/// An outcome as printed: its text, or only its length under `--no-text`; "nothing heard" is
/// never mistaken for a text of the same length.
fn shown(outcome: &Outcome, no_text: bool) -> String {
    match outcome {
        Outcome::NothingHeard => "(nothing heard)".to_owned(),
        Outcome::Text(t) if no_text => format!("({} chars)", t.chars().count()),
        Outcome::Text(t) => t.clone(),
    }
}

/// One record through `engine` under `ctx`: milliseconds and the text after the text pipeline.
fn run_ctx(
    engine: &mut Engine,
    ctx: AudioCtx,
    clip: &[f32],
    args: &Args,
    assets: &TextAssets,
) -> Result<(u128, String)> {
    engine.set_bench_fit_ctx(ctx == AudioCtx::Fit);
    let started = Instant::now();
    let transcript = engine.transcribe(clip, args.lang, &assets.prompt)?;
    let ms = started.elapsed().as_millis();
    let text = match pipeline::finish(&transcript, assets, false) {
        Outcome::Text(t) => t,
        Outcome::NothingHeard => String::new(),
    };
    Ok((ms, text))
}

/// The accuracy guard: every record under `Full` and/or `Fit`, error rates against the stored
/// text. Prints numbers per record (the text too, unless `--no-text`) and the totals.
#[allow(clippy::cast_precision_loss, reason = "record counts are tiny")]
fn run_history(args: &Args, engine: &mut Engine, assets: &TextAssets) -> Result<()> {
    let records = history_records(args)?;
    if records.is_empty() {
        bail!("no matching done records in {}", args.dir.display());
    }
    let modes: &[AudioCtx] = match args.ctx {
        Ctx::Full => &[AudioCtx::Full],
        Ctx::Fit => &[AudioCtx::Fit],
        Ctx::Both => &[AudioCtx::Full, AudioCtx::Fit],
    };
    // A first run pays one-off costs (kernels, buffers); keep it out of the timings.
    // The first readable record; an unreadable one is reported by the loop below.
    if let Some((mono, _)) = records.iter().find_map(|r| read_wav_mono_f32(&r.wav).ok()) {
        run_ctx(engine, AudioCtx::Full, &mono, args, assets)?;
    }
    let mut times = vec![Vec::new(); modes.len()];
    let mut errors = vec![Vec::new(); modes.len()];
    let mut worst_lost = 0;
    for (n, rec) in records.iter().enumerate() {
        let (mono, rate) = match read_wav_mono_f32(&rec.wav) {
            Ok(wav) => wav,
            Err(e) => {
                println!("#{n}\terror: {:#}", anyhow::Error::from(e));
                continue;
            }
        };
        if rate != audio::SAMPLE_RATE {
            println!("#{n}\tskipped: {rate} Hz");
            continue;
        }
        let mut line = format!("#{n}\taudio {} ms", mono.len() / 16);
        let mut texts = Vec::new();
        for (m, &ctx) in modes.iter().enumerate() {
            let (ms, text) = run_ctx(engine, ctx, &mono, args, assets)?;
            let e = cer(&rec.reference, &text);
            times[m].push(ms);
            errors[m].push(e);
            let _ = write!(line, "\t{ctx:?} {ms} ms cer {e:.3}");
            texts.push(text);
        }
        if let [full, fit] = &texts[..] {
            let lost = lost_words(&rec.reference, full, fit);
            worst_lost = worst_lost.max(lost);
            let _ = write!(line, "\tlost {lost}");
        }
        if !args.no_text {
            let _ = write!(line, "\t{}", texts.last().map_or("", String::as_str));
        }
        println!("{line}");
    }
    for (m, &ctx) in modes.iter().enumerate() {
        times[m].sort_unstable();
        let mean = errors[m].iter().sum::<f64>() / errors[m].len().max(1) as f64;
        println!(
            "{ctx:?}: files {}\tp50 {} ms\tp95 {} ms\tmean cer {mean:.4}",
            times[m].len(),
            percentile(&times[m], 50),
            percentile(&times[m], 95),
        );
    }
    println!("worst lost words in one record: {worst_lost}");
    Ok(())
}

/// `live`, `tokens`, or a registry model id (`turbo`, `large-v3`, `tiny`) for its DTW preset.
fn parse_timing(value: &str) -> Result<BenchTiming> {
    match value {
        "live" => return Ok(BenchTiming::Live),
        "tokens" => return Ok(BenchTiming::Method(Timing::Tokens)),
        _ => {}
    }
    dtw_preset(value)
        .map(|preset| BenchTiming::Method(Timing::Dtw(preset)))
        .with_context(|| {
            format!("--timings is live, tokens, turbo, large-v3 or tiny, not {value:?}")
        })
}

/// A word's start and end in ms.
type Span = (u64, u64);

/// Longest a word may last from its start by the capped variant: a base plus a rate per letter.
const CAP_BASE_MS: u64 = 150;
const CAP_PER_CHAR_MS: u64 = 70;

/// What the timing check adds up over the records.
#[derive(Default)]
struct TimingStats {
    words: usize,
    /// `t0 <= t1 <= next t0`.
    monotone: usize,
    /// `t1` inside the clip.
    inside: usize,
    word_ms: u64,
    voiced_ms: u64,
    /// Distance from each edge of a gate pause (400 ms or more, between two voiced runs) to the
    /// nearest word boundary.
    edges: Vec<u64>,
}

/// Ms of `a` that lie inside `runs` (both sorted, `runs` not overlapping).
fn overlap_ms(a: Span, runs: &[Span]) -> u64 {
    runs.iter()
        .map(|r| a.1.min(r.1).saturating_sub(a.0.max(r.0)))
        .sum()
}

impl TimingStats {
    /// Scores the words of one clip of `clip_ms` against the gate's voiced `runs` (in ms).
    fn add(&mut self, words: &[Span], clip_ms: u64, runs: &[Span]) {
        for (i, w) in words.iter().enumerate() {
            self.words += 1;
            let next = words.get(i + 1).map_or(u64::MAX, |n| n.0);
            self.monotone += usize::from(w.0 <= w.1 && w.1 <= next);
            self.inside += usize::from(w.1 <= clip_ms);
            self.word_ms += w.1.saturating_sub(w.0);
            self.voiced_ms += overlap_ms(*w, runs);
        }
        for pair in runs.windows(2) {
            if pair[1].0 - pair[0].1 < 400 {
                continue;
            }
            for edge in [pair[0].1, pair[1].0] {
                let near = words.iter().flat_map(|w| [w.0, w.1]);
                self.edges.extend(near.map(|b| b.abs_diff(edge)).min());
            }
        }
    }

    /// One line: shares in percent, then the pause-edge median and 90th percentile.
    #[allow(clippy::cast_precision_loss, reason = "counts are tiny")]
    fn line(&mut self) -> String {
        let pct = |a: usize, b: usize| 100.0 * a as f64 / b.max(1) as f64;
        self.edges.sort_unstable();
        let edges: Vec<u128> = self.edges.iter().map(|&e| u128::from(e)).collect();
        format!(
            "words {}\tmonotone {:.1} %\tinside {:.1} %\tvoiced {:.1} %\tpause edges {}: p50 {} ms p90 {} ms",
            self.words,
            pct(self.monotone, self.words),
            pct(self.inside, self.words),
            100.0 * self.voiced_ms as f64 / self.word_ms.max(1) as f64,
            edges.len(),
            percentile(&edges, 50),
            percentile(&edges, 90),
        )
    }
}

/// Word spans in ms of a transcript; words with no time are left out (counted in the second value).
fn word_spans(t: &oxi_whisper::Transcript) -> (Vec<(Span, usize)>, usize) {
    let mut out = Vec::new();
    let mut missing = 0;
    for w in t.segments.iter().flat_map(|s| &s.words) {
        match (w.t0, w.t1) {
            (Some(a), Some(b)) => out.push(((a.0, b.0), w.text.chars().count())),
            _ => missing += 1,
        }
    }
    (out, missing)
}

/// Each word ends at most [`CAP_BASE_MS`] plus [`CAP_PER_CHAR_MS`] a letter after its start.
fn capped(words: &[(Span, usize)]) -> Vec<Span> {
    words
        .iter()
        .map(|&((a, b), n)| (a, b.min(a + CAP_BASE_MS + CAP_PER_CHAR_MS * n as u64)))
        .collect()
}

/// `--timings`: word times of the History records, by one method, scored against the gate. Read
/// only; numbers only, no text is printed.
fn run_timings(args: &Args, timing: BenchTiming) -> Result<()> {
    let records = history_records(args)?;
    if records.is_empty() {
        bail!("no matching done records in {}", args.dir.display());
    }
    let started = Instant::now();
    let mut engine = match timing {
        BenchTiming::Live => {
            let mut engine = Engine::load(&args.model, &args.gpu)?;
            engine.set_token_times(true);
            engine
        }
        BenchTiming::Method(method) => Engine::load_timing(&args.model, &args.gpu, method)?,
    };
    println!(
        "{timing:?}: loaded in {} ms on {}",
        started.elapsed().as_millis(),
        engine.info().device.as_deref().unwrap_or("CPU")
    );
    let (mut raw, mut cap) = (TimingStats::default(), TimingStats::default());
    let (mut times, mut missing, mut audio_ms) = (Vec::new(), 0, 0);
    for (n, rec) in records.iter().enumerate() {
        let Ok((mono, rate)) = read_wav_mono_f32(&rec.wav) else {
            continue;
        };
        if rate != audio::SAMPLE_RATE {
            continue;
        }
        let clip_ms = mono.len() as u64 / 16;
        let runs: Vec<Span> = audio::voiced_runs(&mono, args.audio.vad_sensitivity)
            .iter()
            .map(|r| (r.start as u64 / 16, r.end as u64 / 16))
            .collect();
        let at = Instant::now();
        let t = engine.transcribe(&mono, args.lang, "")?;
        let ms = at.elapsed().as_millis();
        let (words, miss) = word_spans(&t);
        missing += miss;
        raw.add(
            &words.iter().map(|w| w.0).collect::<Vec<_>>(),
            clip_ms,
            &runs,
        );
        cap.add(&capped(&words), clip_ms, &runs);
        times.push(ms);
        audio_ms += clip_ms;
        println!(
            "#{n}\taudio {clip_ms} ms\trun {ms} ms\twords {}\twithout time {miss}",
            words.len()
        );
    }
    times.sort_unstable();
    println!(
        "records {}\taudio {audio_ms} ms\tp50 {} ms\tp95 {} ms\twords without time {missing}",
        times.len(),
        percentile(&times, 50),
        percentile(&times, 95)
    );
    println!("raw\t{}", raw.line());
    println!("capped\t{}", cap.line());
    Ok(())
}

/// `--record <seconds> <out.wav>`: the whole argument list, nothing else. Seconds are capped
/// at the config recording limit; the path is checked before the microphone opens.
fn parse_record(args: &[String]) -> Result<(Duration, PathBuf)> {
    let [_, secs, out] = args else {
        bail!("--record takes <seconds> <out.wav>\n{USAGE}");
    };
    let max = Audio::default().max_seconds;
    let len = secs
        .parse()
        .ok()
        .filter(|s: &f64| *s > 0.0)
        .and_then(|s| Duration::try_from_secs_f64(s).ok())
        .with_context(|| format!("--record: seconds must be a positive number, got {secs:?}"))?;
    if len > Duration::from_secs(max.into()) {
        bail!("--record: at most {max} s, got {secs}");
    }
    let out = PathBuf::from(out);
    // write_wav accepts only a lowercase extension.
    if out.extension().is_none_or(|e| e != "wav") {
        bail!("--record: {} must end in .wav (lowercase)", out.display());
    }
    Ok((len, out))
}

fn record(args: &[String]) -> Result<()> {
    let (len, out) = parse_record(args)?;
    // Raw clip: the gate would cut silence and normalising would change levels.
    let opts = Audio {
        trim: false,
        normalize: false,
        ..Audio::default()
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let capture = CpalCapture::open(&opts, LevelMeter::default(), move |e| {
        let _ = tx.send(e);
    })?;
    println!(
        "recording {:.1} s from the default microphone",
        len.as_secs_f64()
    );
    // Any event ends the wait: a lost device keeps its part, a failure surfaces from `finish`.
    if let Ok(CaptureEvent::DeviceLost) = rx.recv_timeout(len) {
        eprintln!("warning: microphone lost, keeping what was captured");
    }
    let clip = capture.finish()?;
    write_wav(&out, &clip)?;
    println!(
        "{}: {} samples at {} Hz",
        out.display(),
        clip.len(),
        audio::SAMPLE_RATE
    );
    Ok(())
}

pub fn run(args: &[String]) -> Result<()> {
    if args.first().is_some_and(|a| a == "--record") {
        return record(args);
    }
    let args = parse_args(args)?;
    if args.timings.is_some() && !args.history {
        bail!("--timings needs --history");
    }
    let files = if args.history {
        Vec::new()
    } else {
        wav_files(&args.dir)?
    };
    if files.is_empty() && !args.history {
        bail!("no .wav files in {}", args.dir.display());
    }
    let assets = match &args.prompt_file {
        // Strict, unlike the daemon: a prompt file named on the command line must exist.
        Some(p) => TextAssets::with_dictionary(
            &pipeline::read_text(p)?.with_context(|| format!("read {}: not found", p.display()))?,
        ),
        None => TextAssets::default(),
    };

    pipeline::verify_hash(&args.model)?;
    if let Some(timing) = args.timings {
        return run_timings(&args, timing);
    }
    print_vram(&args, "before load");
    let started = Instant::now();
    let mut engine = Engine::load(&args.model, &args.gpu)?;
    let load_ms = started.elapsed().as_millis();
    print_vram(&args, "after load");
    let info = engine.info();
    println!("device: {}", info.device.as_deref().unwrap_or("CPU"));
    if let Some(w) = &info.warning {
        println!("warning: {}", strings::engine_warning(w));
    }
    if args.history {
        let ran = run_history(&args, &mut engine, &assets);
        print_vram(&args, "after run");
        return ran;
    }
    if args.ctx == Ctx::Both {
        bail!("--ctx both needs --history");
    }
    if args.ctx == Ctx::Fit {
        engine.set_bench_fit_ctx(true);
    }

    let (mut times, mut heard) = (Vec::new(), 0_usize);
    for path in &files {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let (mono, rate) = match read_wav_mono_f32(path) {
            Ok(wav) => wav,
            Err(e) => {
                // `{:#}` of an anyhow error prints the cause, which `WavError` keeps in `source`.
                println!("{name}\terror: {:#}", anyhow::Error::from(e));
                continue;
            }
        };
        let keep = mono
            .len()
            .saturating_sub(audio::samples_for(Millis(args.cut_end_ms.into()), rate));
        let clip = match audio::process(&mono[..keep], rate, &args.audio) {
            Ok(clip) => clip,
            Err(e) => {
                println!("{name}\tskipped: {e}");
                continue;
            }
        };
        let started = Instant::now();
        let transcript = engine.transcribe(&clip, args.lang, &assets.prompt)?;
        let ms = started.elapsed().as_millis();
        times.push(ms);
        let outcome = pipeline::finish(&transcript, &assets, false);
        heard += usize::from(matches!(outcome, Outcome::Text(_)));
        let text = shown(&outcome, args.no_text);
        println!("{name}\t{text}\tload {load_ms} ms\ttranscribe {ms} ms");
    }
    print_vram(&args, "after run");
    drop(engine);
    print_vram(&args, "after unload");
    if heard == 0 {
        bail!(
            "no file produced a transcript: {} skipped (unreadable, silent or nothing heard)",
            files.len()
        );
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

    #[test]
    fn no_text_never_hides_a_silent_file_behind_a_char_count() {
        let heard = Outcome::Text("(nothing heard)".to_owned());
        assert_eq!(shown(&heard, true), "(15 chars)");
        assert_eq!(shown(&Outcome::NothingHeard, true), "(nothing heard)");
        assert_eq!(shown(&heard, false), "(nothing heard)");
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
        assert_eq!(a.lang, Language::RU);
        assert_eq!(a.prompt_file, Some(PathBuf::from("p.txt")));
        let a = parse_args(&strs(&["d", "--model", "m", "--gpu", "cpu"])).unwrap();
        assert_eq!((a.gpu, a.lang), (DeviceChoice::Cpu, Language::AUTO));
    }

    #[test]
    fn hidden_ab_flags_change_the_audio_options() {
        let a = parse_args(&strs(&["d", "--model", "m"])).unwrap();
        assert_eq!(
            (a.audio.clone(), a.cut_end_ms, a.vram),
            (Audio::default(), 0, false)
        );
        let a = parse_args(&strs(&[
            "d",
            "--model",
            "m",
            "--no-trim",
            "--no-normalize",
            "--denoise",
            "--vram",
            "--cut-end-ms",
            "300",
        ]))
        .unwrap();
        assert!(!a.audio.trim && !a.audio.normalize && a.audio.denoise && a.vram);
        assert_eq!(a.cut_end_ms, 300);
        assert!(parse_args(&strs(&["d", "--model", "m", "--cut-end-ms", "x"])).is_err());
    }

    #[test]
    fn rejects_bad_args() {
        for bad in [
            &["--model", "m"][..],
            &["d"],
            &["d", "--model"],
            &["d", "--model", "m", "--lang", "xx"],
            &["d", "--model", "m", "--bogus"],
            &["d", "e", "--model", "m"],
        ] {
            assert!(parse_args(&strs(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn record_args_need_positive_seconds_and_a_path() {
        let (len, out) = parse_record(&strs(&["--record", "2.5", "a.wav"])).unwrap();
        assert_eq!(
            (len, out),
            (Duration::from_millis(2500), PathBuf::from("a.wav"))
        );
        for bad in [
            &["--record"][..],
            &["--record", "3"],
            &["--record", "0", "a.wav"],
            &["--record", "-1", "a.wav"],
            &["--record", "x", "a.wav"],
            &["--record", "3", "a.wav", "b.wav"],
            &["--record", "1e30", "a.wav"],
            &["--record", "inf", "a.wav"],
            &["--record", "NaN", "a.wav"],
            &["--record", "3600.5", "a.wav"],
            &["--record", "3", "a.WAV"],
            &["--record", "3", "a.txt"],
            &["--record", "3", "a"],
        ] {
            assert!(parse_record(&strs(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn record_seconds_up_to_the_config_limit() {
        let (len, _) = parse_record(&strs(&["--record", "600", "a.wav"])).unwrap();
        assert_eq!(len, Duration::from_secs(600));
    }

    #[test]
    fn usage_has_no_space_runs_mid_line() {
        for line in USAGE.to_string().lines() {
            assert!(!line.trim_start().contains("   "), "{line:?}");
        }
    }

    #[test]
    fn guard_flags_parse_and_error_rates_ignore_case_and_punctuation() {
        let a = parse_args(&strs(&[
            "d",
            "--model",
            "m",
            "--history",
            "--ctx",
            "both",
            "--no-text",
            "--max-s",
            "6",
            "--limit",
            "12",
        ]))
        .unwrap();
        assert!(a.history && a.no_text && a.ctx == Ctx::Both);
        assert_eq!((a.min_s, a.max_s, a.limit), (0, 6, 12));
        assert!(parse_args(&strs(&["d", "--model", "m", "--ctx", "half"])).is_err());
        assert!(cer("Hello, World!", "hello world").abs() < f64::EPSILON);
        assert!((cer("abcd", "abxd") - 0.25).abs() < 1e-9);
        assert!((cer("abc", "") - 1.0).abs() < 1e-9);
        assert_eq!(lost_words("a b c b", "a b c b", "a c b"), 1);
        assert_eq!(lost_words("a b", "a", "a b"), 0);
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        assert_eq!(percentile(&[], 50), 0);
        assert_eq!(percentile(&[7], 95), 7);
        let v: Vec<u128> = (1..=10).collect();
        assert_eq!((percentile(&v, 50), percentile(&v, 95)), (5, 10));
    }

    #[test]
    fn timing_stats_count_order_clip_voice_and_pause_edges() {
        let mut s = TimingStats::default();
        // Voice 0..900 and 1800..2300 with a 900 ms pause between; the words follow it exactly.
        s.add(
            &[(0, 500), (500, 900), (1800, 2300)],
            2400,
            &[(0, 900), (1800, 2300)],
        );
        assert_eq!((s.words, s.monotone, s.inside), (3, 3, 3));
        assert_eq!(s.voiced_ms, s.word_ms);
        assert_eq!(s.edges, [0, 0]);
        // A word that runs into the next one, and one past the end of the clip.
        let mut s = TimingStats::default();
        s.add(&[(0, 600), (500, 3000)], 2400, &[]);
        assert_eq!((s.monotone, s.inside, s.voiced_ms), (1, 1, 0));
        assert_eq!(s.edges.len(), 0);
    }

    #[test]
    fn the_capped_variant_cuts_only_long_words() {
        let words = [((0, 5000), 3), ((6000, 6200), 3)];
        assert_eq!(capped(&words), [(0, 360), (6000, 6200)]);
    }

    #[test]
    fn timings_flag_takes_live_tokens_or_a_model() {
        let a = parse_args(&strs(&["d", "--model", "m", "--timings", "live"])).unwrap();
        assert_eq!(a.timings, Some(BenchTiming::Live));
        let a = parse_args(&strs(&["d", "--model", "m", "--timings", "turbo"])).unwrap();
        assert!(matches!(
            a.timings,
            Some(BenchTiming::Method(Timing::Dtw(_)))
        ));
        assert!(parse_args(&strs(&["d", "--model", "m", "--timings", "huge"])).is_err());
    }
}
