//! Tests of the settings added by the UI overhaul (arch-plan 1.2). Synthetic text only.

use super::*;
use crate::types::Language;

/// A v2 file from before the overhaul: none of the new keys.
const OLD: &str = "version = 2
# mine
[hotkey]
chord = [\"Ctrl\", \"Win\"]
mode = \"push\"

[model]
id = \"turbo\"

[paste]
rules = [{ app = \"x.exe\", mode = \"type\" }]

[ui]
theme = \"dark\"
";

fn warnings(text: &str) -> Vec<String> {
    ConfigFile::parse(text).unwrap().warnings
}

fn says(text: &str, key: &str) {
    let w = warnings(text);
    assert!(w.iter().any(|m| m.contains(key)), "{key} not in {w:?}");
}

#[test]
fn config_defaults_match_the_plan() {
    let c = Config::default();
    assert_eq!(c.hotkey.hold_gap_ms, 400);
    assert_eq!(c.audio.channel, AudioChannel::Mix);
    assert_eq!(c.audio.vad_sensitivity, VadSensitivity::Normal);
    assert!(!c.model.preload_at_start && c.model.by_language.is_empty());
    let a = &c.model.advanced;
    assert_eq!(a.threads, Auto::Auto);
    assert_eq!((a.best_of, a.beam_size), (1, 5));
    assert_eq!(a.initial_prompt, "");
    for auto in [a.temperature, a.no_speech_threshold, a.logprob_threshold] {
        assert_eq!(auto, Auto::Auto);
    }
    assert_eq!(c.paste.type_speed, TypeSpeed::Fast);
    assert!(!c.text.pause_punctuation && !c.text.filler_filter);
    assert!(!c.text.voice_commands && !c.text.confidence_filter);
    assert_eq!(
        (c.text.pause_sentence_ms, c.text.pause_paragraph_ms),
        (800, 0)
    );
    assert!((c.text.confidence_min - 0.35).abs() < f32::EPSILON);
    assert_eq!(c.text.fillers.len(), 7);
    assert_eq!(c.text.snippets, []);
    assert_eq!(c.output.file_mode, FileMode::Off);
    assert_eq!(c.output.file_format, FileFormat::Md);
    assert!(!c.history.skip_pauses && c.sounds.folder.is_empty());
    assert_eq!(c.ui.accent, Accent::Default);
}

#[test]
fn config_old_file_loads_with_new_defaults_and_writes_back_identical() {
    let file = ConfigFile::parse(OLD).unwrap();
    assert!(file.warnings.is_empty(), "{:?}", file.warnings);
    let mut want = Config::default();
    want.ui.theme = Theme::Dark;
    want.paste.rules = vec![PasteRule {
        app: "x.exe".into(),
        mode: Some(PasteMode::Type),
        ..PasteRule::default()
    }];
    assert_eq!(file.config, want);
    assert_eq!(file.to_toml_string().unwrap(), OLD);
    // The default file round-trips too.
    let text = toml::to_string(&Config::default()).unwrap();
    assert_eq!(toml::from_str::<Config>(&text).unwrap(), Config::default());
}

#[test]
fn config_every_enum_value_round_trips() {
    let mut c = Config::default();
    for ch in [AudioChannel::Mix, AudioChannel::Left, AudioChannel::Right] {
        for vad in [
            VadSensitivity::Low,
            VadSensitivity::Normal,
            VadSensitivity::High,
        ] {
            for speed in [TypeSpeed::Fast, TypeSpeed::Safe] {
                for mode in [FileMode::Off, FileMode::Also, FileMode::Only] {
                    for fmt in [FileFormat::Md, FileFormat::Txt] {
                        c.audio.channel = ch;
                        c.audio.vad_sensitivity = vad;
                        c.paste.type_speed = speed;
                        c.output.file_mode = mode;
                        c.output.file_folder = "C:/out".into();
                        c.output.file_format = fmt;
                        let back: Config = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
                        assert_eq!(back, c);
                    }
                }
            }
        }
    }
    for decoding in [Decoding::Greedy, Decoding::Beam] {
        for ctx in [AudioCtx::Full, AudioCtx::Fit] {
            c.model.advanced.decoding = Auto::Set(decoding);
            c.model.advanced.audio_ctx = Auto::Set(ctx);
            let text = toml::to_string(&c).unwrap();
            assert_eq!(toml::from_str::<Config>(&text).unwrap(), c);
        }
    }
}

#[test]
fn config_auto_reads_auto_a_value_and_names_the_key_on_a_wrong_type() {
    let a: Advanced = toml::from_str(
        "threads = \"auto\"\nflash_attn = true\ndecoding = \"beam\"\ntemperature = 1\nlogprob_threshold = -1.5\n",
    )
    .unwrap();
    assert_eq!(a.threads, Auto::Auto);
    assert_eq!(a.flash_attn, Auto::Set(true));
    assert_eq!(a.decoding, Auto::Set(Decoding::Beam));
    assert_eq!(a.temperature, Auto::Set(1.0));
    assert_eq!(a.logprob_threshold, Auto::Set(-1.5));
    let t: Advanced = toml::from_str("threads = 6").unwrap();
    assert_eq!(t.threads, Auto::Set(6));
    assert_eq!(toml::to_string(&t).unwrap().trim(), {
        let mut want = toml::to_string(&Advanced::default()).unwrap();
        want = want.replace("threads = \"auto\"", "threads = 6");
        want.trim().to_owned()
    });
    for bad in [
        "threads = \"many\"",
        "threads = true",
        "flash_attn = 3",
        "decoding = \"auto2\"",
        "temperature = \"hot\"",
    ] {
        let err = toml::from_str::<Advanced>(bad).unwrap_err().to_string();
        let key = bad.split(' ').next().unwrap();
        assert!(err.contains(key), "{bad}: {err}");
    }
}

#[test]
fn config_clamps_name_the_key_below_above_and_nan() {
    let adv = |line: &str| format!("[model.advanced]\n{line}\n");
    for (line, key, fixed) in [
        ("threads = 0", "threads", "threads = 1"),
        ("threads = 40", "threads", "threads = 32"),
        ("best_of = 0", "best_of", "best_of = 1"),
        ("best_of = 9", "best_of", "best_of = 5"),
        ("beam_size = 1", "beam_size", "beam_size = 2"),
        ("beam_size = 9", "beam_size", "beam_size = 8"),
        ("temperature = -1.0", "temperature", "temperature = 0.0"),
        ("temperature = 2.0", "temperature", "temperature = 1.0"),
        ("no_speech_threshold = 3.0", "no_speech_threshold", "= 1.0"),
        ("logprob_threshold = -9.0", "logprob_threshold", "= -5.0"),
        ("logprob_threshold = 1.0", "logprob_threshold", "= 0.0"),
        ("entropy_threshold = 11.0", "entropy_threshold", "= 10.0"),
        ("entropy_threshold = -1.0", "entropy_threshold", "= 0.0"),
    ] {
        let text = adv(line);
        says(&text, &format!("model.advanced.{key}"));
        let file = ConfigFile::parse(&text).unwrap();
        let out = toml::to_string(&file.config.model.advanced).unwrap();
        assert!(out.contains(fixed), "{line}: {out}");
    }
    for (text, key) in [
        ("[text]\npause_sentence_ms = 100", "text.pause_sentence_ms"),
        ("[text]\npause_sentence_ms = 9000", "text.pause_sentence_ms"),
        ("[text]\npause_paragraph_ms = 5", "text.pause_paragraph_ms"),
        (
            "[text]\npause_paragraph_ms = 99999",
            "text.pause_paragraph_ms",
        ),
        ("[text]\nconfidence_min = 0.0", "text.confidence_min"),
        ("[text]\nconfidence_min = 1.0", "text.confidence_min"),
        ("[text]\nconfidence_min = nan", "text.confidence_min"),
    ] {
        says(text, key);
    }
    let nan = ConfigFile::parse("[text]\nconfidence_min = nan").unwrap();
    assert!((nan.config.text.confidence_min - 0.35).abs() < f32::EPSILON);
    let low = ConfigFile::parse("[text]\npause_paragraph_ms = 5").unwrap();
    assert_eq!(low.config.text.pause_paragraph_ms, 1000);
    let off = ConfigFile::parse("[text]\npause_paragraph_ms = 0").unwrap();
    assert!(off.warnings.is_empty() && off.config.text.pause_paragraph_ms == 0);
}

#[test]
fn config_hold_gap_clamps_but_zero_is_off() {
    let low = ConfigFile::parse("[hotkey]\nhold_gap_ms = 50\n").unwrap();
    assert_eq!(low.config.hotkey.hold_gap_ms, 100);
    assert!(low.warnings[0].contains("hotkey.hold_gap_ms"));
    let high = ConfigFile::parse("[hotkey]\nhold_gap_ms = 5000\n").unwrap();
    assert_eq!(high.config.hotkey.hold_gap_ms, 1500);
    let off = ConfigFile::parse("[hotkey]\nhold_gap_ms = 0\n").unwrap();
    assert_eq!(off.config.hotkey.hold_gap_ms, 0);
    assert_eq!(off.warnings, Vec::<String>::new());
    // The corrected value is what a save writes.
    assert!(low.to_toml_string().unwrap().contains("hold_gap_ms = 100"));
}

#[test]
fn config_accent_takes_hex_and_falls_back_with_a_warning() {
    let file = ConfigFile::parse("[ui]\naccent = \"#12ab3F\"\n").unwrap();
    assert_eq!(file.config.ui.accent, Accent::Hex(Rgb(0x12, 0xAB, 0x3F)));
    assert_eq!(file.warnings, Vec::<String>::new());
    assert!(file.to_toml_string().unwrap().contains("\"#12AB3F\""));
    assert_eq!(file.config.ui.accent.to_string(), "#12AB3F");
    for bad in ["red", "#12", "#12ab3", "#12ab3g", "12ab3f", ""] {
        let file = ConfigFile::parse(&format!("[ui]\naccent = \"{bad}\"\n")).unwrap();
        assert_eq!(file.config.ui.accent, Accent::Default, "{bad}");
        assert!(
            file.warnings.iter().any(|w| w.contains("ui.accent")),
            "{bad}"
        );
    }
    let default = ConfigFile::parse("[ui]\naccent = \"default\"\n").unwrap();
    assert_eq!(default.warnings, Vec::<String>::new());
}

#[test]
fn config_rule_with_every_new_field_round_trips_and_a_bare_rule_writes_only_app() {
    let full = PasteRule {
        app: "code.exe".into(),
        mode: Some(PasteMode::Type),
        chord: Some(vec!["Shift".into(), "Insert".into()]),
        restore_delay_ms: Some(50),
        type_speed: Some(TypeSpeed::Safe),
        language: Some(Language::EN),
        model: Some("small".into()),
        to_file: Some(FileMode::Only),
        dictionary: Some(false),
        replacements: Some(true),
        voice_commands: Some(true),
        filler_filter: Some(false),
        pause_punctuation: Some(true),
        join_lines: Some(true),
    };
    let mut file = ConfigFile::parse("version = 2\n").unwrap();
    file.config.paste.rules = vec![
        full.clone(),
        PasteRule {
            app: "bare.exe".into(),
            ..PasteRule::default()
        },
    ];
    let out = file.to_toml_string().unwrap();
    let back = ConfigFile::parse(&out).unwrap();
    assert_eq!(back.config.paste.rules, file.config.paste.rules);
    let table: toml::Table = toml::from_str(&out).unwrap();
    let rules = table["paste"]["rules"].as_array().unwrap();
    let bare = rules[1].as_table().unwrap();
    assert_eq!(bare.keys().collect::<Vec<_>>(), ["app"], "{out}");
    // Clearing every optional field of a kept rule removes the keys again.
    let mut again = back;
    again.config.paste.rules[0] = PasteRule {
        app: "code.exe".into(),
        ..PasteRule::default()
    };
    let cleared = again.to_toml_string().unwrap();
    assert!(
        !cleared.contains("type_speed")
            && !cleared.contains("pause_punctuation")
            && !cleared.contains("join_lines")
    );
}

#[test]
fn config_text_snippets_fillers_and_by_language_survive_edits() {
    let text = "version = 2
[model.by_language]
ru = \"turbo\"
en = \"small\"

[text]
fillers = [\"э\", \"Um\", \"um\", \"\"]

[[text.snippets]]
trigger = \"sig\"
text = \"Best,\\nMe\"

[[text.snippets]]
trigger = \"\"
text = \"nobody\"
";
    let mut file = ConfigFile::parse(text).unwrap();
    assert_eq!(file.warnings.len(), 2, "{:?}", file.warnings);
    assert_eq!(file.config.text.fillers, ["э", "um"]);
    assert_eq!(file.config.text.snippets.len(), 1);
    assert_eq!(file.config.text.snippets[0].text, "Best,\nMe");
    let by = &file.config.model.by_language;
    assert_eq!(by.get(&LanguageKey(Language::RU)).unwrap(), "turbo");
    assert_eq!(by.len(), 2);
    // Remove one language and add a snippet: the file follows.
    file.config
        .model
        .by_language
        .remove(&LanguageKey(Language::EN));
    file.config.text.snippets.push(Snippet {
        trigger: "adr".into(),
        text: "line 1\nline 2".into(),
    });
    let out = file.to_toml_string().unwrap();
    assert!(!out.contains("small"), "{out}");
    let back = ConfigFile::parse(&out).unwrap();
    assert_eq!(back.config.text.snippets, file.config.text.snippets);
    assert_eq!(back.config.model.by_language, file.config.model.by_language);
    assert!(back.warnings.is_empty(), "{:?}", back.warnings);
}

#[test]
fn config_output_without_a_folder_acts_as_off() {
    let file = ConfigFile::parse("[output]\nfile_mode = \"also\"\n").unwrap();
    assert!(file.warnings[0].contains("output.file_folder"));
    assert_eq!(file.config.output.file_mode, FileMode::Also);
    assert_eq!(file.config.output.effective_mode(), FileMode::Off);
    let with =
        ConfigFile::parse("[output]\nfile_mode = \"only\"\nfile_folder = \"C:/o\"\n").unwrap();
    assert_eq!(with.warnings, Vec::<String>::new());
    assert_eq!(with.config.output.effective_mode(), FileMode::Only);
}

#[test]
fn config_f32_values_keep_their_text_on_save() {
    let text = "version = 2\n[text]\nconfidence_min = 0.4\n[model.advanced]\ntemperature = 0.2\n";
    let file = ConfigFile::parse(text).unwrap();
    assert_eq!(file.to_toml_string().unwrap(), text);
}

#[test]
fn config_emptying_a_map_removes_it_from_the_file() {
    let mut file = ConfigFile::parse("[model.by_language]\nru = \"turbo\"\n").unwrap();
    file.config.model.by_language.clear();
    let back = ConfigFile::parse(&file.to_toml_string().unwrap()).unwrap();
    assert!(back.config.model.by_language.is_empty());
}

#[test]
fn config_a_rule_keeps_an_unknown_key_named_like_a_map() {
    let text = "version = 2\n[[paste.rules]]\napp = \"x.exe\"\nby_language = 1\napp_names = 2\n";
    let file = ConfigFile::parse(text).unwrap();
    assert_eq!(file.to_toml_string().unwrap(), text);
}

#[test]
fn config_every_float_clamp_covers_below_above_and_nan() {
    for (key, below, above) in [
        ("temperature", "-1.0", "2.0"),
        ("no_speech_threshold", "-1.0", "2.0"),
        ("logprob_threshold", "-9.0", "1.0"),
        ("entropy_threshold", "-1.0", "11.0"),
    ] {
        for v in [below, above] {
            says(&format!("[model.advanced]\n{key} = {v}\n"), key);
        }
        let file = ConfigFile::parse(&format!("[model.advanced]\n{key} = nan\n")).unwrap();
        let toml = toml::to_string(&file.config.model.advanced).unwrap();
        assert!(toml.contains(&format!("{key} = \"auto\"")), "{key}: {toml}");
        assert!(file.warnings.iter().any(|m| m.contains(key)), "{key}");
    }
    let a = ConfigFile::parse("").unwrap().config.model.advanced;
    assert_eq!(a, Advanced::default());
    for auto in [
        a.flash_attn == Auto::Auto,
        a.decoding == Auto::Auto,
        a.temperature_fallback == Auto::Auto,
        a.audio_ctx == Auto::Auto,
        a.entropy_threshold == Auto::Auto,
        a.suppress_non_speech == Auto::Auto,
    ] {
        assert!(auto);
    }
}

#[test]
fn config_out_of_range_integers_clamp_with_a_warning_instead_of_failing() {
    for (line, key, want) in [
        ("threads = 256", "threads", "threads = 32"),
        ("threads = -1", "threads", "threads = 1"),
        ("best_of = 300", "best_of", "best_of = 5"),
        ("best_of = -1", "best_of", "best_of = 1"),
        ("beam_size = 300", "beam_size", "beam_size = 8"),
    ] {
        let text = format!("[model.advanced]\n{line}\n");
        let file = ConfigFile::parse(&text).unwrap_or_else(|e| panic!("{line}: {e}"));
        says(&text, &format!("model.advanced.{key}"));
        let out = toml::to_string(&file.config.model.advanced).unwrap();
        assert!(out.contains(want), "{line}: {out}");
    }
}

#[test]
fn config_load_error_names_the_key() {
    let err = ConfigFile::parse("[model.advanced]\nthreads = \"many\"\n")
        .err()
        .unwrap();
    let ConfigError::Syntax { message, .. } = err else {
        panic!("not a syntax error: {err}");
    };
    assert!(message.starts_with("threads:"), "{message}");
}
