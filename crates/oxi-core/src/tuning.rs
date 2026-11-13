//! Recognition test (Test tab): word error rate, ready phrases, scenario profiles, best set.
//!
//! A set is what the test may change: the audio switches and the model; engine parameters stay
//! the configured ones. No AI: the set with the fewest word
//! errors wins, and a tie goes to the set closest to the defaults.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization as _;

use crate::audio::SAMPLE_RATE;
use crate::config::{Advanced, AudioCtx, Auto, Config, Decoding, PromptSource};
use crate::ipc::MAX_LINE;
use crate::types::Language;

/// Longest recording the daemon holds for a test: 5 minutes.
pub const MAX_TEST_SAMPLES: usize = SAMPLE_RATE as usize * 300;
/// Most samples one `TestAudio` line can carry: 8 hex digits each, inside [`MAX_LINE`].
pub const PCM_CHUNK_SAMPLES: usize = (MAX_LINE - 256) / 8;

/// One combination of settings to try.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsSet {
    pub normalize: bool,
    pub denoise: bool,
    pub trim: bool,
    /// `ModelEntry::id`, or `custom` for the configured custom file.
    pub model: String,
}

impl SettingsSet {
    /// The settings of `Config::default()`.
    pub fn defaults() -> Self {
        let cfg = Config::default();
        Self {
            normalize: cfg.audio.normalize,
            denoise: cfg.audio.denoise,
            trim: cfg.audio.trim,
            model: cfg.model.id,
        }
    }

    /// How many settings differ from [`SettingsSet::defaults`].
    pub fn changes(&self) -> usize {
        let d = Self::defaults();
        [
            self.normalize != d.normalize,
            self.denoise != d.denoise,
            self.trim != d.trim,
            self.model != d.model,
        ]
        .iter()
        .filter(|changed| **changed)
        .count()
    }
}

/// A scenario from the Test tab; each one picks the sets worth trying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    General,
    FastSpeech,
    BadMicrophone,
    LongDictation,
    SeveralPasses,
}

/// (normalize, denoise, trim, model).
type Row = (bool, bool, bool, &'static str);

const DEFAULTS: Row = (true, false, true, "turbo");
const DENOISE: Row = (true, true, true, "turbo");
const NO_NORMALIZE: Row = (false, false, true, "turbo");
const NO_TRIM: Row = (true, false, false, "turbo");
const LARGE: Row = (true, false, true, "large-v3");
const LARGE_DENOISE: Row = (true, true, true, "large-v3");
const PODLODKA: Row = (true, false, true, "podlodka");

impl Profile {
    pub const ALL: [Self; 5] = [
        Self::General,
        Self::FastSpeech,
        Self::BadMicrophone,
        Self::LongDictation,
        Self::SeveralPasses,
    ];

    /// The sets to try, the defaults first.
    pub fn sets(self) -> Vec<SettingsSet> {
        let rows: &[Row] = match self {
            Self::General => &[DEFAULTS, DENOISE, NO_NORMALIZE, LARGE],
            // Trimming can eat the edges of words said fast.
            Self::FastSpeech => &[DEFAULTS, NO_TRIM, LARGE, PODLODKA],
            Self::BadMicrophone => &[
                DEFAULTS,
                DENOISE,
                (false, true, true, "turbo"),
                LARGE,
                LARGE_DENOISE,
            ],
            Self::LongDictation => &[DEFAULTS, NO_TRIM, DENOISE, LARGE],
            Self::SeveralPasses => &[DEFAULTS, LARGE, LARGE_DENOISE, PODLODKA],
        };
        rows.iter()
            .map(|&(normalize, denoise, trim, model)| SettingsSet {
                normalize,
                denoise,
                trim,
                model: model.into(),
            })
            .collect()
    }
}

/// What the Test wizard looks for, its first step (owner 2026-11-13, hold 29-32).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Goal {
    /// Each ticked model with the audio settings in use.
    Model,
    /// Each ticked model with the settings in use and with each audio switch flipped.
    Settings,
    /// The recordings made quieter in steps ([`QUIET_STEPS_DB`]): which set still hears them.
    QuietMic,
}

impl Goal {
    pub const ALL: [Self; 3] = [Self::Model, Self::Settings, Self::QuietMic];

    /// The software gain each recording is heard at, in dB: as recorded, then quieter.
    pub fn gains(self) -> &'static [f32] {
        match self {
            Self::QuietMic => &QUIET_STEPS_DB,
            Self::Model | Self::Settings => &[0.0],
        }
    }
}

/// The quiet-microphone check: the same recording as it is and 6, 12 and 18 dB quieter, so no new
/// recording is needed.
pub const QUIET_STEPS_DB: [f32; 4] = [0.0, -6.0, -12.0, -18.0];

/// The quick picks of the wizard's model list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelPreset {
    Downloaded,
    /// The model recommended for this PC and the one in use, to compare against.
    Recommended,
    All,
}

impl ModelPreset {
    pub const ALL: [Self; 3] = [Self::Downloaded, Self::Recommended, Self::All];
}

/// One model of the wizard's list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    pub id: String,
    pub downloaded: bool,
    pub recommended: bool,
    pub in_use: bool,
}

/// The ticks `preset` gives `list`.
pub fn preset_ticks(preset: ModelPreset, list: &[ModelChoice]) -> Vec<bool> {
    list.iter()
        .map(|m| match preset {
            ModelPreset::Downloaded => m.downloaded,
            ModelPreset::Recommended => m.recommended || m.in_use,
            ModelPreset::All => true,
        })
        .collect()
}

/// The preset whose ticks equal `ticks`, the first one when two give the same; `None` for a
/// hand-made pick.
pub fn preset_of(ticks: &[bool], list: &[ModelChoice]) -> Option<ModelPreset> {
    ModelPreset::ALL
        .into_iter()
        .find(|p| preset_ticks(*p, list) == ticks)
}

/// The sets a run of `goal` tries over `models`, from `base` (the audio switches in use; its
/// model is ignored). A model goes once per set, the base set first.
pub fn wizard_sets(goal: Goal, base: &SettingsSet, models: &[&str]) -> Vec<SettingsSet> {
    let with = |model: &str, normalize, denoise, trim| SettingsSet {
        normalize,
        denoise,
        trim,
        model: model.to_owned(),
    };
    let (n, d, t) = (base.normalize, base.denoise, base.trim);
    let mut sets = Vec::new();
    for model in models {
        let variants = match goal {
            Goal::Model => vec![(n, d, t)],
            Goal::Settings => vec![(n, d, t), (n, !d, t), (!n, d, t), (n, d, !t)],
            // A quiet microphone is what normalising and noise suppression are for.
            Goal::QuietMic => vec![(n, d, t), (true, true, t)],
        };
        for (n, d, t) in variants {
            let set = with(model, n, d, t);
            if !sets.contains(&set) {
                sets.push(set);
            }
        }
    }
    sets
}

/// Recordings shorter than this say too little to score.
pub const PICK_MIN_MS: u64 = 1_000;
/// Only this many of the newest recordings are looked at.
pub const PICK_RECENT: usize = 20;
/// Recordings a wizard run takes from History.
pub const PICK_COUNT: usize = 3;

/// The recordings a wizard run takes: among the newest [`PICK_RECENT`] usable ones (`Some`
/// length, at least [`PICK_MIN_MS`] and at most the test limit), up to `n` of varied length (the
/// shortest, the longest and evenly between). `lengths` is newest first; so is the answer.
pub fn pick_recordings(lengths: &[Option<u64>], n: usize) -> Vec<usize> {
    let max_ms = MAX_TEST_SAMPLES as u64 * 1000 / u64::from(SAMPLE_RATE);
    let mut usable: Vec<(u64, usize)> = lengths
        .iter()
        .enumerate()
        .filter_map(|(i, len)| {
            len.filter(|l| (PICK_MIN_MS..=max_ms).contains(l))
                .map(|l| (l, i))
        })
        .take(PICK_RECENT)
        .collect();
    if usable.len() <= n {
        return usable.into_iter().map(|(_, i)| i).collect();
    }
    usable.sort_unstable();
    let last = usable.len() - 1;
    let mut picked: Vec<usize> = match n {
        0 => Vec::new(),
        1 => vec![usable[last / 2].1],
        // Evenly spread, rounded to the nearest.
        _ => (0..n)
            .map(|k| usable[(k * last + (n - 1) / 2) / (n - 1)].1)
            .collect(),
    };
    picked.sort_unstable();
    picked.dedup();
    picked
}

/// `samples` made `db` louder (negative: quieter); `None` for a recording longer than the daemon
/// takes ([`MAX_TEST_SAMPLES`]): trimmed, it would no longer match its reference text.
pub fn with_gain(samples: &[f32], db: f32) -> Option<Vec<f32>> {
    let k = 10f32.powf(db / 20.0);
    (samples.len() <= MAX_TEST_SAMPLES).then(|| samples.iter().map(|s| s * k).collect())
}

/// The models a finished run offers to delete: the ones that heard every recording (`scored`) but
/// lost to `winner`, without the model in use; each once, in table order.
pub fn losers<'a>(scored: &'a [SettingsSet], winner: &str, in_use: &str) -> Vec<&'a str> {
    let mut out: Vec<&str> = Vec::new();
    // The winner's model is kept whatever its other sets scored.
    for set in scored {
        let id = set.model.as_str();
        if id != winner && id != in_use && !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// Ready phrases for `language`; empty when there is no set for it (the person writes their
/// own). Invented sentences, nobody's real text.
pub fn phrases(language: Language) -> &'static [&'static str] {
    match language.as_str() {
        "ru" => &[
            "Сегодня утром на улице шёл мелкий дождь.",
            "Поезд отправляется с третьего пути через десять минут.",
            "Пожалуйста, купи хлеб, молоко и немного яблок.",
            "Завтра мы обсудим новый план работы на следующий месяц.",
            "Маленький щенок быстро бежал через широкое зелёное поле.",
        ],
        "en" => &[
            "It was raining lightly on the street this morning.",
            "The train leaves from platform three in ten minutes.",
            "Please buy some bread, milk and a few apples.",
            "Tomorrow we will discuss the new work plan for next month.",
            "A small puppy ran quickly across the wide green field.",
        ],
        "de" => &[
            "Heute Morgen hat es auf der Straße leicht geregnet.",
            "Der Zug fährt in zehn Minuten von Gleis drei ab.",
            "Bitte kauf Brot, Milch und ein paar Äpfel.",
            "Morgen besprechen wir den neuen Arbeitsplan für den nächsten Monat.",
            "Ein kleiner Hund lief schnell über die weite grüne Wiese.",
        ],
        "es" => &[
            "Esta mañana llovía un poco en la calle.",
            "El tren sale del andén tres dentro de diez minutos.",
            "Por favor, compra pan, leche y unas manzanas.",
            "Mañana hablaremos del nuevo plan de trabajo para el próximo mes.",
            "Un perrito corrió rápido por el ancho campo verde.",
        ],
        "fr" => &[
            "Ce matin, il pleuvait légèrement dans la rue.",
            "Le train part de la voie trois dans dix minutes.",
            "S'il te plaît, achète du pain, du lait et quelques pommes.",
            "Demain, nous parlerons du nouveau plan de travail pour le mois prochain.",
            "Un petit chien a couru vite à travers le grand champ vert.",
        ],
        _ => &[],
    }
}

/// Word errors of one or more phrases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WordErrors {
    /// Substitutions, insertions and deletions.
    pub errors: usize,
    /// Words in the reference.
    pub words: usize,
}

impl WordErrors {
    /// Error percent; insertions can take it past 100. Nothing to say and something heard is 100.
    #[allow(clippy::cast_precision_loss, reason = "word counts are far below 2^52")]
    pub fn percent(self) -> f64 {
        match (self.errors, self.words) {
            (0, _) => 0.0,
            (_, 0) => 100.0,
            (errors, words) => errors as f64 / words as f64 * 100.0,
        }
    }

    /// The sum over the phrases of one set.
    #[must_use]
    pub fn plus(self, other: Self) -> Self {
        Self {
            errors: self.errors + other.errors,
            words: self.words + other.words,
        }
    }
}

/// Punctuation and symbols that separate words. Letters and combining marks of any script are
/// not in it, so words in other scripts stay whole.
fn is_punctuation(c: char) -> bool {
    c.is_ascii_punctuation()
        || matches!(
            c,
            '¡' | '§'
                | '«'
                | '¶'
                | '·'
                | '»'
                | '¿'
                | '،'
                | '؛'
                | '؟'
                | '।'
                | '॥'
                | '\u{2000}'..='\u{206F}'
                | '\u{3000}'..='\u{303F}'
                | '\u{FF01}'..='\u{FF0F}'
                | '\u{FF1A}'..='\u{FF20}'
        )
}

/// Lowercase words without punctuation, yo folded to ye. An apostrophe joins (`don't` is one
/// word); any other punctuation separates (`кто-то` and `кто то` match).
///
/// shortcut: words are split on spaces, so unspaced CJK text is one word per line; split by
/// character when a CJK phrase set is added.
fn words(text: &str) -> Vec<String> {
    let fold = |c| if c == 'ё' { 'е' } else { c };
    let mut clean = String::with_capacity(text.len());
    for c in text.to_lowercase().nfc() {
        if matches!(c, '\'' | '\u{2019}' | '\u{2BC}') {
            continue;
        }
        clean.push(if is_punctuation(c) { ' ' } else { fold(c) });
    }
    clean.split_whitespace().map(str::to_owned).collect()
}

/// Word-level edit distance between `reference` and `hypothesis`.
pub fn word_errors(reference: &str, hypothesis: &str) -> WordErrors {
    let (want, got) = (words(reference), words(hypothesis));
    let mut row: Vec<usize> = (0..=got.len()).collect();
    for (i, w) in want.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, g) in got.iter().enumerate() {
            let substitution = diagonal + usize::from(w != g);
            diagonal = row[j + 1];
            row[j + 1] = substitution.min(row[j] + 1).min(diagonal + 1);
        }
    }
    WordErrors {
        errors: row[got.len()],
        words: want.len(),
    }
}

/// Index of the set to apply: the lowest error percent, then the fewest changes from the
/// defaults, then the earliest. Equal fractions give equal `f64`s (division rounds exactly).
pub fn best(results: &[(SettingsSet, WordErrors)]) -> Option<usize> {
    results
        .iter()
        .enumerate()
        .min_by(|(_, (a_set, a)), (_, (b_set, b))| {
            a.percent()
                .total_cmp(&b.percent())
                .then_with(|| a_set.changes().cmp(&b_set.changes()))
        })
        .map(|(i, _)| i)
}

/// Samples as IPC text: eight hex digits of each `f32::to_bits`.
pub fn encode_pcm(samples: &[f32]) -> String {
    let mut out = String::with_capacity(samples.len() * 8);
    for s in samples {
        let _ = write!(out, "{:08x}", s.to_bits());
    }
    out
}

/// The samples of [`encode_pcm`]; `None` for bad text or a sample that is not finite.
pub fn decode_pcm(hex: &str) -> Option<Vec<f32>> {
    let (samples, rest) = hex.as_bytes().as_chunks::<8>();
    if !rest.is_empty() {
        return None;
    }
    samples
        .iter()
        .map(|digits| {
            let bits = u32::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()?;
            Some(f32::from_bits(bits)).filter(|s| s.is_finite())
        })
        .collect()
}

/// Adds one `TestAudio` piece to the recording, after starting a new one when `reset`. `None` is
/// no recording: bad text or a recording over [`MAX_TEST_SAMPLES`] drops it and returns `false`,
/// and so does every later piece until a `reset`, so a run never scores only the tail.
pub fn append_pcm(held: &mut Option<Vec<f32>>, reset: bool, hex: &str) -> bool {
    if reset {
        *held = Some(Vec::new());
    }
    if let (Some(samples), Some(part)) = (held.as_mut(), decode_pcm(hex))
        && samples.len() + part.len() <= MAX_TEST_SAMPLES
    {
        samples.extend(part);
        return true;
    }
    *held = None;
    false
}

/// How whisper picks the next words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sampling {
    Greedy { best_of: u8 },
    Beam { beam_size: u8 },
}

/// Every per-run engine parameter, resolved from `model.advanced` (`auto` replaced by what the
/// build did before the settings existed).
#[derive(Debug, Clone, PartialEq)]
pub struct RunParams {
    pub threads: u8,
    pub sampling: Sampling,
    pub temperature: f32,
    /// Whisper retries a failed segment at a higher temperature (step 0.2).
    pub temperature_fallback: bool,
    pub no_speech_threshold: f32,
    pub logprob_threshold: f32,
    pub entropy_threshold: f32,
    pub suppress_non_speech: bool,
    pub audio_ctx: AudioCtx,
}

/// Decode threads of `auto`. whisper.cpp scales badly past the physical cores, and `std` only
/// knows logical ones, so halve (SMT assumed). A GPU does the heavy work: 4 is plenty.
pub fn auto_threads(on_gpu: bool, logical: usize) -> u8 {
    if on_gpu {
        4
    } else {
        u8::try_from((logical / 2).clamp(1, 8)).unwrap_or(8)
    }
}

/// Flash attention of the context: `auto` is on with a GPU (M5 measured -14..-41 % p50), off on
/// CPU.
pub fn flash_attn(adv: &Advanced, on_gpu: bool) -> Option<bool> {
    match adv.flash_attn {
        Auto::Auto => on_gpu.then_some(true),
        Auto::Set(v) => Some(v),
    }
}

fn or<T: Copy>(value: Auto<T>, auto: T) -> T {
    match value {
        Auto::Auto => auto,
        Auto::Set(v) => v,
    }
}

/// `adv` with `auto` replaced; pure, so "all auto equals today" is a test.
pub fn resolve(adv: &Advanced, on_gpu: bool, logical: usize) -> RunParams {
    RunParams {
        threads: or(adv.threads, auto_threads(on_gpu, logical)),
        sampling: match or(adv.decoding, Decoding::Greedy) {
            Decoding::Greedy => Sampling::Greedy {
                best_of: adv.best_of,
            },
            Decoding::Beam => Sampling::Beam {
                beam_size: adv.beam_size,
            },
        },
        temperature: or(adv.temperature, 0.0),
        temperature_fallback: or(adv.temperature_fallback, true),
        no_speech_threshold: or(adv.no_speech_threshold, 0.6),
        logprob_threshold: or(adv.logprob_threshold, -1.0),
        entropy_threshold: or(adv.entropy_threshold, 2.4),
        suppress_non_speech: or(adv.suppress_non_speech, false),
        audio_ctx: or(adv.audio_ctx, AudioCtx::Full),
    }
}

/// The prompt of a run: what `prompt_source` says (the dictionary words, nothing, or the
/// person's own text first and then the dictionary words).
pub fn prompt(adv: &Advanced, dictionary: &str) -> String {
    match adv.prompt_source() {
        PromptSource::Dictionary => dictionary.to_owned(),
        PromptSource::None => String::new(),
        PromptSource::Custom => match (adv.initial_prompt.trim(), dictionary) {
            ("", d) => d.to_owned(),
            (p, "") => p.to_owned(),
            (p, d) => format!("{p} {d}"),
        },
    }
}

/// Named values of the Advanced numbers, the choices between Auto and Custom. The values only
/// fill the number: the config keeps the number, so a file never needs the names.
pub const TEMPERATURE_PRESETS: [f32; 2] = [0.3, 0.7];
/// Keep more speech (a segment is dropped only when almost surely silence), drop more silence.
pub const NO_SPEECH_PRESETS: [f32; 2] = [0.9, 0.3];
/// Retry more (stricter), retry less (looser).
pub const LOGPROB_PRESETS: [f32; 2] = [-0.5, -2.0];
/// Retry more (a segment repeating itself is tried again sooner), retry less.
pub const ENTROPY_PRESETS: [f32; 2] = [1.8, 3.5];

/// Max (every logical thread) and Half of them, put in the range `threads` takes.
pub fn thread_presets(logical: usize) -> [f32; 2] {
    let n = |n: usize| {
        let range = Advanced::THREADS;
        f32::from(
            u8::try_from(n)
                .unwrap_or(u8::MAX)
                .clamp(*range.start(), *range.end()),
        )
    };
    [n(logical), n(logical / 2)]
}

/// The choice a number shows: 0 Auto, 1.. the preset it equals, then Custom (`presets.len() + 1`).
/// `custom` keeps Custom picked while the typed number happens to equal a preset.
pub fn preset_mode(value: Auto<f32>, presets: &[f32], custom: bool) -> usize {
    match value {
        Auto::Auto => 0,
        Auto::Set(_) if custom => presets.len() + 1,
        Auto::Set(v) => presets
            .iter()
            .position(|p| (p - v).abs() < 1e-4)
            .map_or(presets.len() + 1, |i| i + 1),
    }
}

/// What choice `mode` writes: Auto, its preset, or the number typed beside it (Custom).
pub fn preset_value(mode: usize, presets: &[f32], typed: f32) -> Auto<f32> {
    match mode {
        0 => Auto::Auto,
        m => Auto::Set(presets.get(m - 1).copied().unwrap_or(typed)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(id: &str, downloaded: bool, recommended: bool, in_use: bool) -> ModelChoice {
        ModelChoice {
            id: id.into(),
            downloaded,
            recommended,
            in_use,
        }
    }

    #[test]
    fn presets_tick_the_downloaded_the_recommended_and_all() {
        let list = [
            choice("turbo", true, false, true),
            choice("small", true, false, false),
            choice("podlodka", false, true, false),
        ];
        let ticks = |p| preset_ticks(p, &list);
        assert_eq!(ticks(ModelPreset::Downloaded), [true, true, false]);
        assert_eq!(
            ticks(ModelPreset::Recommended),
            [true, false, true],
            "the recommended one and the one in use"
        );
        assert_eq!(ticks(ModelPreset::All), [true, true, true]);
        assert_eq!(
            preset_of(&[true, false, true], &list),
            Some(ModelPreset::Recommended)
        );
        assert_eq!(preset_of(&[false, true, false], &list), None, "by hand");
        // Two presets with the same ticks show the first.
        let one = [choice("turbo", true, true, true)];
        assert_eq!(preset_of(&[true], &one), Some(ModelPreset::Downloaded));
    }

    #[test]
    fn each_goal_builds_its_sets_from_the_settings_in_use() {
        let base = SettingsSet::defaults();
        let model = wizard_sets(Goal::Model, &base, &["turbo", "small"]);
        assert_eq!(model.len(), 2);
        assert!(model.iter().all(|s| s.changes() <= 1));
        assert_eq!(model[1].model, "small");

        let settings = wizard_sets(Goal::Settings, &base, &["turbo"]);
        assert_eq!(settings.len(), 4);
        assert_eq!(settings[0], base, "the settings in use first");
        assert!(
            settings[1..].iter().all(|s| s.changes() == 1),
            "each flips one switch"
        );

        let quiet = wizard_sets(Goal::QuietMic, &base, &["turbo"]);
        assert_eq!(quiet.len(), 2);
        assert!(quiet[1].normalize && quiet[1].denoise);
        // Already on: one set, not the same set twice.
        let loud = SettingsSet {
            denoise: true,
            ..base.clone()
        };
        assert_eq!(wizard_sets(Goal::QuietMic, &loud, &["turbo"]).len(), 1);
        assert!(
            wizard_sets(Goal::Settings, &base, &[]).is_empty(),
            "no model"
        );
        assert_eq!(Goal::QuietMic.gains(), QUIET_STEPS_DB);
        assert_eq!(Goal::Model.gains(), [0.0]);
    }

    #[test]
    fn the_wizard_picks_recent_recordings_of_varied_length() {
        let s = |secs: u64| Some(secs * 1000);
        // Newest first: the shortest, the longest and one between, kept newest first.
        let lengths = [s(5), s(30), None, s(2), Some(400), s(12), s(60)];
        assert_eq!(pick_recordings(&lengths, 3), [3, 5, 6]);
        // Too short, too long and unreadable ones never count.
        assert_eq!(pick_recordings(&[Some(500), s(301), None, s(4)], 3), [3]);
        assert_eq!(pick_recordings(&lengths, 1), [5], "the middle length");
        assert!(pick_recordings(&lengths, 0).is_empty(), "none asked");
        // Only the newest are looked at.
        let many: Vec<Option<u64>> = (0..40).map(|i| s(2 + i)).collect();
        assert!(pick_recordings(&many, 3).iter().all(|&i| i < PICK_RECENT));
    }

    #[test]
    fn gain_steps_scale_the_samples() {
        let quieter = with_gain(&[0.5, -1.0], -6.0).unwrap();
        assert!((quieter[0] - 0.5 * 0.501_187).abs() < 1e-4);
        assert!((quieter[1] + 0.501_187).abs() < 1e-4);
        assert_eq!(with_gain(&[0.25], 0.0).unwrap(), [0.25]);
        // A history recording over five minutes gets no quieter copy: the daemon would refuse it.
        assert!(with_gain(&vec![0.0; MAX_TEST_SAMPLES], -6.0).is_some());
        assert!(with_gain(&vec![0.0; MAX_TEST_SAMPLES + 1], -6.0).is_none());
    }

    #[test]
    fn losers_are_the_other_models_but_never_the_one_in_use() {
        let set = |model: &str, denoise| SettingsSet {
            denoise,
            model: model.into(),
            ..SettingsSet::defaults()
        };
        let scored = [
            set("turbo", false),
            set("small", false),
            set("small", true),
            set("large-v3", false),
            set("base", false),
        ];
        assert_eq!(losers(&scored, "small", "turbo"), ["large-v3", "base"]);
        assert!(
            losers(&scored[..1], "turbo", "turbo").is_empty(),
            "one model"
        );
    }

    #[test]
    fn auto_threads_table() {
        // (on_gpu, logical threads, expected)
        for (gpu, logical, want) in [
            (false, 1, 1),
            (false, 2, 1),
            (false, 8, 4),
            (false, 16, 8),
            (false, 64, 8),
            (true, 1, 4),
            (true, 32, 4),
        ] {
            assert_eq!(
                auto_threads(gpu, logical),
                want,
                "gpu={gpu} logical={logical}"
            );
        }
    }

    #[test]
    fn all_auto_equals_today() {
        let adv = Advanced::default();
        for (gpu, logical) in [(false, 16), (true, 16)] {
            let r = resolve(&adv, gpu, logical);
            assert_eq!(r.threads, auto_threads(gpu, logical));
            assert_eq!(r.sampling, Sampling::Greedy { best_of: 1 });
            assert!((r.temperature - 0.0).abs() < f32::EPSILON);
            assert!(r.temperature_fallback);
            assert!((r.no_speech_threshold - 0.6).abs() < f32::EPSILON);
            assert!((r.logprob_threshold + 1.0).abs() < f32::EPSILON);
            assert!((r.entropy_threshold - 2.4).abs() < f32::EPSILON);
            assert!(!r.suppress_non_speech);
            assert_eq!(r.audio_ctx, AudioCtx::Full);
            assert_eq!(flash_attn(&adv, gpu), gpu.then_some(true));
        }
    }

    #[test]
    fn explicit_values_win() {
        let adv = Advanced {
            threads: Auto::Set(3),
            flash_attn: Auto::Set(false),
            decoding: Auto::Set(Decoding::Beam),
            beam_size: 4,
            temperature: Auto::Set(0.3),
            temperature_fallback: Auto::Set(false),
            no_speech_threshold: Auto::Set(0.4),
            logprob_threshold: Auto::Set(-2.0),
            entropy_threshold: Auto::Set(3.0),
            suppress_non_speech: Auto::Set(true),
            ..Advanced::default()
        };
        let r = resolve(&adv, true, 16);
        assert_eq!(r.threads, 3);
        assert_eq!(r.sampling, Sampling::Beam { beam_size: 4 });
        assert!((r.temperature - 0.3).abs() < f32::EPSILON);
        assert!(!r.temperature_fallback && r.suppress_non_speech);
        assert!((r.no_speech_threshold - 0.4).abs() < f32::EPSILON);
        assert_eq!(flash_attn(&adv, true), Some(false));
        let on = Advanced {
            flash_attn: Auto::Set(true),
            ..Advanced::default()
        };
        assert_eq!(
            flash_attn(&on, false),
            Some(true),
            "explicit on reaches the CPU too"
        );
    }

    #[test]
    fn the_prompt_puts_the_persons_text_before_the_dictionary() {
        let adv = |p: &str| Advanced {
            initial_prompt: p.into(),
            ..Advanced::default()
        };
        assert_eq!(prompt(&adv(""), "Oxi, Kubernetes"), "Oxi, Kubernetes");
        assert_eq!(prompt(&adv(" Meeting notes. "), ""), "Meeting notes.");
        assert_eq!(prompt(&adv("Notes."), "Oxi"), "Notes. Oxi");
        assert_eq!(prompt(&adv(""), ""), "");
    }

    #[test]
    fn the_prompt_source_decides_what_whisper_reads_first() {
        let adv = |source, p: &str| Advanced {
            initial_prompt: p.into(),
            prompt_source: source,
            ..Advanced::default()
        };
        // A file from before the choice: the typed text is Custom, none is the dictionary.
        assert_eq!(adv(None, "").prompt_source(), PromptSource::Dictionary);
        assert_eq!(adv(None, "Notes.").prompt_source(), PromptSource::Custom);
        // The dictionary alone keeps the text for later; None drops both; Custom is today's.
        let dictionary = Some(PromptSource::Dictionary);
        let none = Some(PromptSource::None);
        let custom = Some(PromptSource::Custom);
        assert_eq!(prompt(&adv(dictionary, "Notes."), "Oxi"), "Oxi");
        assert_eq!(prompt(&adv(none, "Notes."), "Oxi"), "");
        assert_eq!(prompt(&adv(custom, "Notes."), "Oxi"), "Notes. Oxi");
        assert_eq!(prompt(&adv(custom, ""), "Oxi"), "Oxi");
    }

    #[test]
    fn number_choices_map_to_presets_and_back() {
        let presets = TEMPERATURE_PRESETS;
        assert_eq!(preset_mode(Auto::Auto, &presets, false), 0);
        assert_eq!(preset_mode(Auto::Set(0.3), &presets, false), 1);
        assert_eq!(preset_mode(Auto::Set(0.7), &presets, false), 2);
        assert_eq!(preset_mode(Auto::Set(0.5), &presets, false), 3, "Custom");
        // Custom picked while the number equals a preset stays Custom.
        assert_eq!(preset_mode(Auto::Set(0.3), &presets, true), 3);
        assert_eq!(preset_value(0, &presets, 0.9), Auto::Auto);
        assert_eq!(preset_value(2, &presets, 0.9), Auto::Set(0.7));
        assert_eq!(preset_value(3, &presets, 0.9), Auto::Set(0.9));
        // Every preset is in the range its parameter takes.
        for (presets, range) in [
            (TEMPERATURE_PRESETS, Advanced::TEMPERATURE),
            (NO_SPEECH_PRESETS, Advanced::NO_SPEECH),
            (LOGPROB_PRESETS, Advanced::LOGPROB),
            (ENTROPY_PRESETS, Advanced::ENTROPY),
        ] {
            assert!(presets.iter().all(|p| range.contains(p)), "{presets:?}");
        }
    }

    #[test]
    fn thread_presets_are_max_and_half_of_the_logical_threads() {
        assert_eq!(thread_presets(16), [16.0, 8.0]);
        assert_eq!(thread_presets(1), [1.0, 1.0]);
        assert_eq!(thread_presets(3), [3.0, 1.0]);
        assert_eq!(
            thread_presets(64),
            [32.0, 32.0],
            "put in the range threads takes"
        );
        assert_eq!(thread_presets(500), [32.0, 32.0]);
        // Max wins when both are one number.
        assert_eq!(preset_mode(Auto::Set(1.0), &thread_presets(1), false), 1);
    }
}
