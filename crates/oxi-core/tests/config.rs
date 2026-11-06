use oxi_core::config::{
    CURRENT_VERSION, Config, ConfigError, ConfigFile, HotkeyMode, MIGRATIONS, Migration,
    OverlayPosition, PasteMode, PasteRule,
};
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
    assert_eq!(file.config.hotkey.mode, HotkeyMode::Toggle);
    assert_eq!(file.config.model.language, Language::Auto);
    assert_eq!(file.config.paste.rules.len(), 2);
    assert_eq!(file.to_toml_string().unwrap(), FULL);
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
    assert_eq!(c.audio.max_seconds, 600);
    assert_eq!(c.model.language, Language::Ru);
    assert_eq!(c.model.id, "turbo");
    assert_eq!(c.hotkey.chord, ["Ctrl", "Win"]);
    assert!(!c.history.enabled);
}

#[test]
fn empty_file_gives_defaults() {
    let file = ConfigFile::parse("").unwrap();
    assert_eq!(file.config, Config::default());
    assert!(file.to_toml_string().unwrap().starts_with("version = 1\n"));
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
    let text = "# my mic\nversion = 1\n[audio]\ngain_db = 1.0 # loud\n";
    let mut file = ConfigFile::parse(text).unwrap();
    assert!(file.to_toml_string().unwrap().starts_with(text));
    file.config.audio.gain_db = 2.0;
    let out = file.to_toml_string().unwrap();
    assert!(
        out.starts_with("# my mic\nversion = 1\n[audio]\ngain_db = 2.0 # loud\n"),
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
    let text = "version = 1\n[overlay]\nenabled = true\nposition = \"middle\"\n";
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
