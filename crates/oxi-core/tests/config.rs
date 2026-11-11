use oxi_core::config::{
    AppLanguage, CURRENT_VERSION, Config, ConfigError, ConfigFile, MIGRATIONS, Migration,
    OverlayPosition, PasteMode, PasteRule, Theme,
};
use oxi_core::machine::Mode;
use oxi_core::types::Language;
use toml::{Table, Value};
use toml_edit::{DocumentMut, Item};

const FULL: &str = include_str!("fixtures/config/v1_full.toml");
const UNKNOWN: &str = include_str!("fixtures/config/v1_unknown.toml");

#[test]
fn version_matches_migration_count() {
    assert_eq!(
        CURRENT_VERSION,
        i64::try_from(MIGRATIONS.len() + 1).unwrap()
    );
}

#[test]
fn v1_fixture_round_trips_byte_identical() {
    let file = ConfigFile::parse(FULL).unwrap();
    assert!(!file.read_only);
    // The fixture differs from the defaults everywhere, so the typed values were really read.
    assert_ne!(file.config, Config::default());
    assert_eq!(file.config.hotkey.mode, Mode::Toggle);
    assert_eq!(file.config.model.language, Language::AUTO);
    assert_eq!(file.config.paste.rules.len(), 2);
    assert_eq!(
        file.to_toml_string().unwrap(),
        FULL.replacen("version = 1", "version = 2", 1)
    );
}

#[test]
fn defaults_round_trip() {
    let text = ConfigFile::default().to_toml_string().unwrap();
    let again = ConfigFile::parse(&text).unwrap();
    assert_eq!(again.config, Config::default());
    assert_eq!(again.to_toml_string().unwrap(), text);
}

#[test]
fn default_values_match_the_spec() {
    let c = Config::default();
    assert_eq!(c.audio.max_seconds, 3600);
    assert_eq!(c.model.language, Language::RU);
    assert_eq!(c.model.id, "turbo");
    assert_eq!(c.hotkey.chord, ["Ctrl", "Win"]);
    assert!(c.history.enabled);
}

#[test]
fn empty_file_gives_defaults() {
    let file = ConfigFile::parse("").unwrap();
    assert_eq!(file.config, Config::default());
    assert!(file.to_toml_string().unwrap().starts_with("version = 2\n"));
}

#[test]
fn unknown_keys_survive_and_known_edits_apply() {
    let mut file = ConfigFile::parse(UNKNOWN).unwrap();
    assert!((file.config.audio.gain_db - 1.5).abs() < f64::EPSILON);
    file.config.audio.gain_db = 2.0;
    let out = file.to_toml_string().unwrap();
    let table: Table = out.parse().unwrap();
    assert_eq!(table["zeta"], Value::String("top".into()));
    assert_eq!(table["audio"]["future_field"], Value::Integer(42));
    assert_eq!(table["audio"]["gain_db"], Value::Float(2.0));
    assert_eq!(table["future"]["flag"], Value::Boolean(true));
}

#[test]
fn newer_version_is_read_only() {
    let text = "version = 99\n[audio]\ngain_db = 4.0\nnew_thing = 1\n";
    let file = ConfigFile::parse(text).unwrap();
    assert!(file.read_only);
    assert!((file.config.audio.gain_db - 4.0).abs() < f64::EPSILON);
    assert!(matches!(file.to_toml_string(), Err(ConfigError::ReadOnly)));
}

#[test]
fn syntax_error_reports_line_and_column() {
    let err = ConfigFile::parse("version = 1\n[audio]\ngain_db = = 3\n").unwrap_err();
    match err {
        ConfigError::Syntax { line, column, .. } => assert_eq!((line, column), (3, 11)),
        other => panic!("expected Syntax, got {other:?}"),
    }
}

#[test]
fn wrong_type_reports_line() {
    let err = ConfigFile::parse("version = 1\n[audio]\n\ngain_db = \"loud\"\n").unwrap_err();
    match err {
        ConfigError::Syntax { line, .. } => assert_eq!(line, 4),
        other => panic!("expected Syntax, got {other:?}"),
    }
}

#[test]
fn bad_version_is_rejected() {
    for text in ["version = 0\n", "version = \"1\"\n", "version = -3\n"] {
        assert!(
            matches!(ConfigFile::parse(text), Err(ConfigError::Invalid(_))),
            "{text}"
        );
    }
}

fn rename_gain(doc: &mut DocumentMut) {
    if let Some(audio) = doc.get_mut("audio").and_then(Item::as_table_like_mut)
        && let Some(v) = audio.remove("gain")
    {
        audio.insert("gain_db", v);
    }
}

fn gain_plus_one(doc: &mut DocumentMut) {
    if let Some(audio) = doc.get_mut("audio").and_then(Item::as_table_like_mut)
        && let Some(g) = audio.get("gain_db").and_then(Item::as_float)
    {
        audio.insert("gain_db", toml_edit::value(g + 1.0));
    }
}

#[test]
fn migrations_run_in_order_from_the_file_version() {
    let migrations: [Migration; 2] = [rename_gain, gain_plus_one];
    // v1: both steps run.
    let v1 = ConfigFile::parse_with("version = 1\n[audio]\ngain = 1.0\n", &migrations).unwrap();
    assert!((v1.config.audio.gain_db - 2.0).abs() < f64::EPSILON);
    // v2: only the second step runs.
    let v2 = ConfigFile::parse_with("version = 2\n[audio]\ngain_db = 1.0\n", &migrations).unwrap();
    assert!((v2.config.audio.gain_db - 2.0).abs() < f64::EPSILON);
    // v3 is current: nothing runs.
    let v3 = ConfigFile::parse_with("version = 3\n[audio]\ngain_db = 1.0\n", &migrations).unwrap();
    assert!((v3.config.audio.gain_db - 1.0).abs() < f64::EPSILON);
    // Migrated output carries the new version.
    assert!(v1.to_toml_string().unwrap().starts_with("version = 3\n"));
}

#[test]
fn version_above_a_custom_migration_list_is_read_only() {
    let migrations: [Migration; 1] = [gain_plus_one];
    let file = ConfigFile::parse_with("version = 3\n", &migrations).unwrap();
    assert!(file.read_only);
}

#[test]
fn error_after_migration_reports_the_line_of_the_migrated_document() {
    let migrations: [Migration; 1] = [gain_plus_one];
    let text = "version = 1\n[audio]\ngain_db = 1.0\n\ntail_ms = \"x\"\n";
    match ConfigFile::parse_with(text, &migrations).unwrap_err() {
        ConfigError::Syntax { line, .. } => assert_eq!(line, 5),
        other => panic!("expected Syntax, got {other:?}"),
    }
}

/// The `toml` block of ARCHITECTURE section 6, read from the doc so the two cannot drift.
fn architecture_sample() -> String {
    let doc = include_str!("../../../docs/ARCHITECTURE.md");
    let section = &doc[doc.find("## 6. Config").unwrap()..];
    let start = section.find("```toml\n").unwrap() + "```toml\n".len();
    let end = start + section[start..].find("```").unwrap();
    section[start..end].to_owned()
}

#[test]
fn architecture_sample_is_the_defaults_and_round_trips_byte_identical() {
    let sample = architecture_sample();
    let file = ConfigFile::parse(&sample).unwrap();
    assert_eq!(file.config, Config::default());
    assert_eq!(file.to_toml_string().unwrap(), sample);
}

type Edit = fn(&mut Config);

#[test]
fn one_changed_value_changes_only_its_text() {
    let sample = architecture_sample();
    let cases: [(Edit, &str, &str); 3] = [
        (|c| c.audio.gain_db = 1.5, "gain_db = 0.0", "gain_db = 1.5"),
        (
            |c| c.overlay.position = OverlayPosition::TopLeft,
            "position = \"bottom-right\"",
            "position = \"top-left\"",
        ),
        (
            |c| c.paste.rules[0].mode = Some(PasteMode::Clipboard),
            "mode = \"type\" }",
            "mode = \"clipboard\" }",
        ),
    ];
    for (edit, from, to) in cases {
        let mut file = ConfigFile::parse(&sample).unwrap();
        edit(&mut file.config);
        assert_eq!(
            file.to_toml_string().unwrap(),
            sample.replacen(from, to, 1),
            "{to}"
        );
    }
}

#[test]
fn hand_edited_comments_and_order_survive() {
    let text = "# my mic\nversion = 2\n[audio]\ngain_db = 1.0 # loud\n";
    let mut file = ConfigFile::parse(text).unwrap();
    assert!(file.to_toml_string().unwrap().starts_with(text));
    file.config.audio.gain_db = 2.0;
    let out = file.to_toml_string().unwrap();
    assert!(
        out.starts_with("# my mic\nversion = 2\n[audio]\ngain_db = 2.0 # loud\n"),
        "{out}"
    );
    assert_eq!(ConfigFile::parse(&out).unwrap().config, file.config);
}

const RULES: &str = "[[paste.rules]]\napp = \"a.exe\"\nmode = \"type\"\nfuture_key = 1\n\n[[paste.rules]]\napp = \"b.exe\"\n";

#[test]
fn rule_unknown_keys_survive_an_edit() {
    let mut file = ConfigFile::parse(RULES).unwrap();
    file.config.paste.rules[0].mode = Some(PasteMode::Clipboard);
    let out = file.to_toml_string().unwrap();
    let t: Table = out.parse().unwrap();
    let rule = &t["paste"]["rules"][0];
    assert_eq!(rule["future_key"], Value::Integer(1), "{out}");
    assert_eq!(rule["mode"], Value::String("clipboard".into()));
}

#[test]
fn clearing_an_optional_rule_field_removes_it() {
    let mut file = ConfigFile::parse(RULES).unwrap();
    file.config.paste.rules[0].mode = None;
    let out = file.to_toml_string().unwrap();
    let t: Table = out.parse().unwrap();
    let rule = t["paste"]["rules"][0].as_table().unwrap();
    assert!(!rule.contains_key("mode"), "{out}");
    assert_eq!(rule["future_key"], Value::Integer(1));
}

#[test]
fn rules_merge_by_app_not_by_index() {
    let mut file = ConfigFile::parse(RULES).unwrap();
    // Drop a.exe, keep b.exe, add c.exe: the unknown key of a.exe goes with it.
    file.config.paste.rules.remove(0);
    file.config.paste.rules.push(PasteRule {
        app: "c.exe".into(),
        mode: Some(PasteMode::Type),
        chord: None,
        restore_delay_ms: Some(10),
        ..PasteRule::default()
    });
    let out = file.to_toml_string().unwrap();
    let again = ConfigFile::parse(&out).unwrap();
    assert_eq!(again.config.paste.rules, file.config.paste.rules);
    assert!(!out.contains("future_key"), "{out}");
}

#[test]
fn inline_rules_merge_by_app() {
    let text = "[paste]\nrules = [{ app = \"a.exe\", x = 1 }, { app = \"b.exe\" }]\n";
    let mut file = ConfigFile::parse(text).unwrap();
    file.config.paste.rules.swap(0, 1);
    file.config.paste.rules[1].restore_delay_ms = Some(5);
    let out = file.to_toml_string().unwrap();
    assert!(
        out.contains(
            "rules = [{ app = \"b.exe\" }, { app = \"a.exe\", x = 1, restore_delay_ms = 5 }]"
        ),
        "{out}"
    );
}

#[test]
fn overlay_position_values() {
    for (text, want) in [
        ("bottom-right", OverlayPosition::BottomRight),
        ("bottom-centre", OverlayPosition::BottomCentre),
        ("bottom-left", OverlayPosition::BottomLeft),
        ("top-right", OverlayPosition::TopRight),
        ("top-centre", OverlayPosition::TopCentre),
        ("top-left", OverlayPosition::TopLeft),
    ] {
        let file = ConfigFile::parse(&format!("[overlay]\nposition = \"{text}\"\n")).unwrap();
        assert_eq!(file.config.overlay.position, want);
    }
    assert_eq!(
        Config::default().overlay.position,
        OverlayPosition::BottomRight
    );
}

#[test]
fn unknown_overlay_position_reports_its_line() {
    let text = "version = 2\n[overlay]\nenabled = true\nposition = \"middle\"\n";
    match ConfigFile::parse(text).unwrap_err() {
        ConfigError::Syntax { line, .. } => assert_eq!(line, 4),
        other => panic!("expected Syntax, got {other:?}"),
    }
}

#[test]
fn default_paste_rule_types_into_remote_desktop() {
    assert_eq!(
        Config::default().paste.rules,
        [PasteRule {
            app: "mstsc.exe".into(),
            mode: Some(PasteMode::Type),
            chord: None,
            restore_delay_ms: None,
            ..PasteRule::default()
        }]
    );
}

#[test]
fn error_column_counts_characters() {
    let bad = "device = \"Микрофон\" x";
    let err = ConfigFile::parse(&format!("[audio]\n{bad}\n")).unwrap_err();
    let at_x = bad[..bad.find('x').unwrap()].chars().count() + 1;
    match err {
        ConfigError::Syntax { line, column, .. } => assert_eq!((line, column), (2, at_x)),
        other => panic!("expected Syntax, got {other:?}"),
    }
}

#[test]
fn deleting_every_rule_survives_a_reload() {
    for text in [RULES.to_owned(), architecture_sample()] {
        let mut file = ConfigFile::parse(&text).unwrap();
        file.config.paste.rules.clear();
        let out = file.to_toml_string().unwrap();
        assert!(out.contains("rules = []"), "{out}");
        assert_eq!(ConfigFile::parse(&out).unwrap().config.paste.rules, []);
    }
    let empty = ConfigFile::parse("[paste]\nrules = []\n").unwrap();
    assert_eq!(empty.config.paste.rules, []);
}

#[test]
fn minimal_file_stays_minimal() {
    let text = "version = 2\n[audio]\ngain_db = 1.0\n";
    assert_eq!(
        ConfigFile::parse(text).unwrap().to_toml_string().unwrap(),
        text
    );
    // A changed key the file lacks is added alone, not with its neighbours.
    let mut file = ConfigFile::parse(text).unwrap();
    file.config.model.id = "base".into();
    let out = file.to_toml_string().unwrap();
    assert!(out.starts_with(text), "{out}");
    assert!(out.contains("[model]\nid = \"base\"\n"), "{out}");
    assert!(!out.contains("gpu") && !out.contains("tail_ms"), "{out}");
    assert_eq!(ConfigFile::parse(&out).unwrap().config, file.config);
}

#[test]
fn new_file_has_every_default() {
    let out = ConfigFile::default().to_toml_string().unwrap();
    assert!(out.contains("max_seconds = 3600"), "{out}");
    assert!(out.contains("app = \"mstsc.exe\""), "{out}");
}

fn lf_count_is_crlf_count(s: &str) -> bool {
    s.matches('\n').count() == s.matches("\r\n").count()
}

#[test]
fn crlf_and_bom_survive_a_save() {
    let crlf = architecture_sample().replace('\n', "\r\n");
    for text in [
        crlf.clone(),
        format!("\u{feff}{}", architecture_sample()),
        format!("\u{feff}{crlf}"),
    ] {
        let mut file = ConfigFile::parse(&text).unwrap();
        assert_eq!(file.config, Config::default());
        assert_eq!(file.to_toml_string().unwrap(), text);
        file.config.audio.gain_db = 1.5;
        assert_eq!(
            file.to_toml_string().unwrap(),
            text.replacen("gain_db = 0.0", "gain_db = 1.5", 1)
        );
    }
    // Lines this build adds to a CRLF file use CRLF too.
    let mut file = ConfigFile::parse("version = 2\r\n[audio]\r\ngain_db = 1.0\r\n").unwrap();
    file.config.model.id = "base".into();
    file.config.audio.tail_ms = 5;
    let out = file.to_toml_string().unwrap();
    assert!(lf_count_is_crlf_count(&out), "{out:?}");
    assert_eq!(ConfigFile::parse(&out).unwrap().config, file.config);
}

#[test]
fn mixed_line_endings_normalise_to_the_dominant_one() {
    for (text, want) in [
        // Three CRLF lines, one LF line: CRLF wins.
        (
            "version = 2\r\n# c\r\n[audio]\ngain_db = 1.0\r\n",
            "version = 2\r\n# c\r\n[audio]\r\ngain_db = 1.0\r\n",
        ),
        // One CRLF line, three LF lines: LF wins.
        (
            "version = 2\r\n# c\n[audio]\ngain_db = 1.0\n",
            "version = 2\n# c\n[audio]\ngain_db = 1.0\n",
        ),
        // A tie goes to LF, or to CRLF in a BOM file.
        (
            "version = 2\r\n[audio]\ngain_db = 1.0\r\n# c\n",
            "version = 2\n[audio]\ngain_db = 1.0\n# c\n",
        ),
        (
            "\u{feff}version = 2\r\n[audio]\ngain_db = 1.0\r\n# c\n",
            "\u{feff}version = 2\r\n[audio]\r\ngain_db = 1.0\r\n# c\r\n",
        ),
    ] {
        let file = ConfigFile::parse(text).unwrap();
        assert_eq!(file.to_toml_string().unwrap(), want, "{text:?}");
    }
}

#[test]
fn theme_round_trips_and_is_written_only_when_set() {
    assert_eq!(Config::default().ui.theme, Theme::System);
    for (theme, text) in [
        (Theme::System, "system"),
        (Theme::Dark, "dark"),
        (Theme::Light, "light"),
    ] {
        let file = ConfigFile::parse(&format!("[ui]\ntheme = \"{text}\"\n")).unwrap();
        assert_eq!(file.config.ui.theme, theme);
        let mut minimal = ConfigFile::parse("version = 2\n").unwrap();
        minimal.config.ui.theme = theme;
        let out = minimal.to_toml_string().unwrap();
        assert_eq!(out.contains("[ui]"), theme != Theme::System, "{out}");
        assert_eq!(ConfigFile::parse(&out).unwrap().config.ui.theme, theme);
    }
}

#[test]
fn meter_sensitivity_is_clamped_on_load() {
    // (value in the file, value loaded, warned)
    let table = [
        ("0.0", 0.0, false),
        ("-20.0", -20.0, false),
        ("1.5", 1.5, false),
        ("20.5", 20.0, true),
        ("-100.0", -20.0, true),
        ("nan", 0.0, true),
    ];
    for (text, want, warned) in table {
        let file = ConfigFile::parse(&format!("[audio]\nmeter_sensitivity_db = {text}\n")).unwrap();
        let got = file.config.audio.meter_sensitivity_db;
        assert_eq!(got.to_bits(), f32::to_bits(want), "{text}");
        assert_eq!(file.warnings.len(), usize::from(warned), "{text}");
    }
}

#[test]
fn meter_sensitivity_survives_a_save() {
    // 1.1 has no exact f32: the save must not rewrite it as the widened f64.
    let text = "version = 2\n[audio]\nmeter_sensitivity_db = 1.1\n";
    assert_eq!(
        ConfigFile::parse(text).unwrap().to_toml_string().unwrap(),
        text
    );
    let mut minimal = ConfigFile::parse("version = 2\n").unwrap();
    minimal.config.audio.meter_sensitivity_db = -3.5;
    let out = minimal.to_toml_string().unwrap();
    assert!(
        out.contains("[audio]\nmeter_sensitivity_db = -3.5\n"),
        "{out}"
    );
}

#[test]
fn language_values_load_and_unknown_code_is_a_config_error() {
    let lang = |v: &str| ConfigFile::parse(&format!("[model]\nlanguage = \"{v}\"\n"));
    for (text, want) in [
        ("auto", Language::AUTO),
        ("ru", Language::RU),
        ("en", Language::EN),
        ("zh", "zh".parse().unwrap()),
        ("haw", "haw".parse().unwrap()),
    ] {
        assert_eq!(lang(text).unwrap().config.model.language, want, "{text}");
    }
    for bad in ["xx", "RU", "", "russian"] {
        match lang(bad).unwrap_err() {
            ConfigError::Syntax { line, message, .. } => {
                assert_eq!(line, 2, "{bad}");
                assert!(message.contains("unknown language code"), "{message}");
            }
            other => panic!("expected Syntax, got {other:?}"),
        }
    }
}

#[test]
fn any_whisper_language_round_trips() {
    let mut file = ConfigFile::parse(FULL).unwrap();
    file.config.model.language = "zh".parse().unwrap();
    let text = file.to_toml_string().unwrap();
    assert!(text.contains("language = \"zh\""));
    let again = ConfigFile::parse(&text).unwrap();
    assert_eq!(again.config.model.language.as_str(), "zh");
    assert_eq!(again.to_toml_string().unwrap(), text);
}

#[test]
fn recent_languages_and_app_language_default_and_round_trip() {
    let c = Config::default();
    assert_eq!(c.model.recent_languages, [Language::RU, Language::EN]);
    assert_eq!(c.ui.language, AppLanguage::En);
    // A v1 file without the new keys changes only its version line on a save.
    assert_eq!(
        ConfigFile::parse(FULL).unwrap().to_toml_string().unwrap(),
        FULL.replacen("version = 1", "version = 2", 1)
    );

    let mut file = ConfigFile::default();
    let zh = "zh".parse().unwrap();
    file.config.model.use_language(zh);
    assert_eq!(file.config.model.recent_languages, [zh, Language::RU]);
    let again = ConfigFile::parse(&file.to_toml_string().unwrap()).unwrap();
    assert_eq!(again.config, file.config);
}

#[test]
fn use_language_keeps_the_last_two_distinct() {
    let mut m = Config::default().model;
    let de = "de".parse().unwrap();
    m.use_language(Language::EN);
    assert_eq!(m.recent_languages, [Language::EN, Language::RU]);
    m.use_language(de);
    assert_eq!(m.recent_languages, [de, Language::EN]);
    m.use_language(de);
    assert_eq!(m.recent_languages, [de, Language::EN]);
    m.use_language(Language::AUTO);
    assert_eq!(
        (m.language, m.recent_languages),
        (Language::AUTO, [de, Language::EN])
    );
}

#[test]
fn recent_languages_are_two_different_real_languages_on_load() {
    let de: Language = "de".parse().unwrap();
    let (ru, en, auto) = (Language::RU, Language::EN, Language::AUTO);
    // (list in the file, list loaded, warned)
    let table = [
        ("\"ru\", \"en\"", [ru, en], false),
        ("\"de\", \"ru\"", [de, ru], false),
        ("\"auto\", \"ru\"", [ru, en], true),
        ("\"ru\", \"ru\"", [ru, en], true),
        ("\"auto\", \"auto\"", [ru, en], true),
        ("\"en\", \"auto\"", [en, ru], true),
        ("\"de\", \"de\"", [de, ru], true),
    ];
    for (list, want, warned) in table {
        let file = ConfigFile::parse(&format!(
            "[model]
recent_languages = [{list}]
"
        ))
        .unwrap();
        assert_eq!(file.config.model.recent_languages, want, "{list}");
        assert_eq!(file.warnings.len(), usize::from(warned), "{list}");
        assert!(!want.contains(&auto));
    }
}

#[test]
fn keep_in_clipboard_history_defaults_on_for_files_without_the_key() {
    // Older files have no such key: no migration needed, the default applies and nothing is added.
    let file = ConfigFile::parse(FULL).unwrap();
    assert!(file.config.paste.keep_in_clipboard_history);
    assert!(
        !file
            .to_toml_string()
            .unwrap()
            .contains("keep_in_clipboard_history")
    );
    assert!(Config::default().paste.keep_in_clipboard_history);
}

#[test]
fn keep_in_clipboard_history_off_survives_a_save() {
    let mut file = ConfigFile::parse(
        "version = 2
",
    )
    .unwrap();
    file.config.paste.keep_in_clipboard_history = false;
    let out = file.to_toml_string().unwrap();
    assert!(out.contains("keep_in_clipboard_history = false"), "{out}");
    let again = ConfigFile::parse(&out).unwrap();
    assert!(!again.config.paste.keep_in_clipboard_history);
}

#[test]
fn v1_idle_unload_zero_stays_never_and_immediately_is_explicit() {
    let model = |body: &str| {
        ConfigFile::parse(&format!("version = 1\n[model]\n{body}"))
            .unwrap()
            .config
            .model
    };
    let old = model("idle_unload_minutes = 0\n");
    assert_eq!(
        (old.idle_unload_minutes, old.unload_immediately),
        (0, false)
    );
    let now = model("unload_immediately = true\n");
    assert!(now.unload_immediately);
    assert!(!Config::default().model.unload_immediately);
}

#[test]
fn sound_defaults_and_values_load() {
    let s = Config::default().sounds;
    assert_eq!(
        (s.start.as_str(), s.stop.as_str()),
        ("apa-hit-3", "apa-hit-2")
    );
    assert_eq!(s.volume.to_bits(), 0.5_f64.to_bits());
    let file = ConfigFile::parse(FULL).unwrap();
    assert_eq!(file.config.sounds.start, "pop");
    assert_eq!(file.config.sounds.stop, r"C:\Sounds\stop.wav");
    assert!(file.warnings.is_empty(), "{:?}", file.warnings);
}

#[test]
fn unknown_sound_ids_and_bad_volume_fall_back_with_a_warning() {
    let file =
        ConfigFile::parse("[sounds]\nstart = \"nope\"\nstop = \"pop\"\nvolume = 3.0\n").unwrap();
    let s = &file.config.sounds;
    assert_eq!((s.start.as_str(), s.stop.as_str()), ("apa-hit-3", "pop"));
    assert_eq!(s.volume.to_bits(), 1.0_f64.to_bits());
    assert_eq!(file.warnings.len(), 2, "{:?}", file.warnings);
    assert!(
        file.warnings[1].contains("sounds.start"),
        "{:?}",
        file.warnings
    );
    // A wrong type is a config error, not a silent default.
    assert!(ConfigFile::parse("[sounds]\nstart = 3\n").is_err());
    let nan = ConfigFile::parse("[sounds]\nvolume = nan\n").unwrap();
    assert_eq!(nan.config.sounds.volume.to_bits(), 0.5_f64.to_bits());
}

#[test]
fn a_custom_wav_path_is_kept_and_survives_a_save() {
    let text = "version = 2\n[sounds]\nstart = 'D:\\a\\b.wav'\n";
    let file = ConfigFile::parse(text).unwrap();
    assert_eq!(file.warnings, [] as [String; 0]);
    assert_eq!(file.config.sounds.start, r"D:\a\b.wav");
    assert_eq!(file.to_toml_string().unwrap(), text);
    let mut changed = file.clone();
    changed.config.sounds.start = r"E:\x y\c.wav".into();
    let again = ConfigFile::parse(&changed.to_toml_string().unwrap()).unwrap();
    assert_eq!(again.config.sounds.start, r"E:\x y\c.wav");
}

#[test]
fn history_defaults_and_values_load() {
    let h = Config::default().history;
    assert_eq!(
        (h.enabled, h.max_bytes, h.max_days, h.folder.as_str()),
        (true, 2_147_483_648, 30, "")
    );
    // A file from before the age limit and the folder keeps the defaults for them.
    let old = ConfigFile::parse("[history]\nenabled = true\nmax_bytes = 5\n").unwrap();
    assert_eq!(
        (
            old.config.history.max_days,
            old.config.history.folder.as_str()
        ),
        (30, "")
    );
    let file = ConfigFile::parse(FULL).unwrap();
    let h = &file.config.history;
    assert_eq!(
        (h.enabled, h.max_bytes, h.max_days, h.folder.as_str()),
        (true, 1_048_576, 7, r"D:\Dictations")
    );
}

#[test]
fn v1_history_off_becomes_on_and_is_reported() {
    let file =
        ConfigFile::parse("version = 1\n[history]\nenabled = false # old\nmax_days = 7\n").unwrap();
    assert!(file.config.history.enabled);
    assert!(file.history_turned_on);
    let out = file.to_toml_string().unwrap();
    assert_eq!(
        out,
        "version = 2\n[history]\nenabled = true # old\nmax_days = 7\n"
    );
    // The saved file is v2: loading it again reports nothing.
    let back = ConfigFile::parse(&out).unwrap();
    assert!(back.config.history.enabled && !back.history_turned_on);
}

#[test]
fn v1_file_without_an_explicit_off_reports_nothing() {
    for text in [
        "",
        "version = 1\n",
        "version = 1\n[history]\nenabled = true\n",
    ] {
        let file = ConfigFile::parse(text).unwrap();
        assert!(
            file.config.history.enabled && !file.history_turned_on,
            "{text}"
        );
    }
}

#[test]
fn v2_history_off_stays_off_and_round_trips() {
    let text = "version = 2\n[history]\nenabled = false\n";
    let file = ConfigFile::parse(text).unwrap();
    assert!(!file.config.history.enabled && !file.history_turned_on);
    assert_eq!(file.to_toml_string().unwrap(), text);
}

#[test]
fn a_newer_file_with_history_off_is_not_migrated() {
    let file = ConfigFile::parse("version = 99\n[history]\nenabled = false\n").unwrap();
    assert!(file.read_only && !file.config.history.enabled && !file.history_turned_on);
}
