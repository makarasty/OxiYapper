//! Recognition test (Test tab): word error rate, ready phrases, scenario profiles, best set.
//!
//! A set is what the test may change: the audio switches and the model. `oxi-whisper` has no
//! tuning parameters to vary yet, so a set has none either. No AI: the set with the fewest word
//! errors wins, and a tie goes to the set closest to the defaults.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::audio::SAMPLE_RATE;
use crate::config::Config;
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

/// shortcut: no Unicode tables in the workspace, so only lowercase Cyrillic and Latin-1 letters
/// compose (NFC for those); add `unicode-normalization` when another script shows false errors.
const COMPOSE: [(char, &str); 8] = [
    ('\u{300}', "aàeèiìoòuù"),
    ('\u{301}', "aáeéiíoóuúyý"),
    ('\u{302}', "aâeêiîoôuû"),
    ('\u{303}', "aãnñoõ"),
    ('\u{306}', "ийуў"),
    ('\u{308}', "aäeëiïoöuüyÿеёії"),
    ('\u{30A}', "aå"),
    ('\u{327}', "cç"),
];

fn composed(base: char, mark: char) -> Option<char> {
    let (_, pairs) = COMPOSE.iter().find(|(m, _)| *m == mark)?;
    let mut pairs = pairs.chars();
    while let (Some(plain), Some(whole)) = (pairs.next(), pairs.next()) {
        if plain == base {
            return Some(whole);
        }
    }
    None
}

/// Lowercase words without punctuation, yo folded to ye. An apostrophe joins (`don't` is one
/// word); any other punctuation separates (`кто-то` and `кто то` match).
///
/// shortcut: words are split on spaces, so unspaced CJK text is one word per line; split by
/// character when a CJK phrase set is added.
fn words(text: &str) -> Vec<String> {
    let fold = |c| if c == 'ё' { 'е' } else { c };
    let mut clean = String::with_capacity(text.len());
    for c in text.to_lowercase().chars() {
        if matches!(c, '\'' | '\u{2019}' | '\u{2BC}') {
            continue;
        }
        if is_punctuation(c) {
            clean.push(' ');
        } else if let Some(whole) = clean.chars().next_back().and_then(|b| composed(b, c)) {
            clean.pop();
            clean.push(fold(whole));
        } else {
            clean.push(fold(c));
        }
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
