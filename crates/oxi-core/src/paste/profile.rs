//! Per-app profile (arch-plan 9.2, 9.3): what one dictation runs with, from the global settings
//! and the first rule of the foreground app. Pure; the coordinator resolves it at key-down.

use super::{Plan, plan};
use crate::config::{Config, FileMode, LanguageKey, PasteMode};
use crate::paste::{Focus, Foreground};
use crate::text::TextOptions;
use crate::types::Language;

/// Every field is the rule's value, else the global one.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub paste: Plan,
    pub language: Language,
    /// A registry id or "custom". The rule's, else `model.by_language` for an explicit
    /// language, else `model.id`. Never empty.
    pub model: String,
    pub to_file: FileMode,
    /// The `[text]` switches with the rule's own; `newlines_allowed` follows the paste mode of
    /// this app. The facts of the clip (`clip_ms`, `can_hold`) are the whisper worker's to set.
    pub text: TextOptions,
    /// `false`: no dictionary prompt for this dictation.
    pub dictionary: bool,
    /// `false`: the replace rules are not applied to this dictation.
    pub replacements: bool,
    /// `paste.press_enter`: where input went at the record start, for the Enter's check. The
    /// coordinator sets it at key-down and drops it on a lock or a sleep; `None` sends no Enter.
    pub focus_at_start: Option<Focus>,
}

/// The profile of `app` (the first rule whose app matches, case-insensitive; an unknown app gets
/// the globals).
pub fn profile(cfg: &Config, app: &str) -> Profile {
    let paste = plan(
        &cfg.paste,
        &Foreground {
            app: app.to_owned(),
            elevated: false,
        },
    );
    let rule = super::rule_for(&cfg.paste.rules, app);
    let language = rule.and_then(|r| r.language).unwrap_or(cfg.model.language);
    let by_language = (language != Language::AUTO)
        .then(|| cfg.model.by_language.get(&LanguageKey(language)))
        .flatten();
    let model = rule
        .and_then(|r| r.model.clone())
        .or_else(|| by_language.cloned())
        .unwrap_or_else(|| cfg.model.id.clone());
    let mut text = TextOptions::from_config(&cfg.text, cfg.paste.trailing_space);
    if let Some(r) = rule {
        text.voice_commands = r.voice_commands.unwrap_or(text.voice_commands);
        text.filler_filter = r.filler_filter.unwrap_or(text.filler_filter);
        text.pause_punctuation = r.pause_punctuation.unwrap_or(text.pause_punctuation);
    }
    // A typed line break would send a message; the clipboard keeps it. A clipboard paste that
    // ends up typed turns it into a space in the paste worker.
    text.newlines_allowed = paste.mode == PasteMode::Clipboard;
    Profile {
        paste,
        language,
        model,
        to_file: rule.and_then(|r| r.to_file).unwrap_or(cfg.output.file_mode),
        text,
        dictionary: rule.and_then(|r| r.dictionary).unwrap_or(true),
        replacements: rule.and_then(|r| r.replacements).unwrap_or(true),
        focus_at_start: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PasteRule;

    fn cfg(rule: PasteRule) -> Config {
        let mut c = Config::default();
        c.paste.rules = vec![rule];
        c
    }

    fn rule(app: &str) -> PasteRule {
        PasteRule {
            app: app.into(),
            ..PasteRule::default()
        }
    }

    #[test]
    fn an_unknown_app_and_an_empty_rule_get_the_globals() {
        let mut c = cfg(rule("a.exe"));
        c.model.language = Language::EN;
        c.output.file_mode = FileMode::Also;
        c.text.filler_filter = true;
        let globals = profile(&c, "other.exe");
        assert_eq!(globals.language, Language::EN);
        assert_eq!(globals.model, "turbo");
        assert_eq!(globals.to_file, FileMode::Also);
        assert!(globals.text.filler_filter && globals.dictionary && globals.replacements);
        assert_eq!(
            profile(&c, "A.EXE"),
            globals,
            "a rule that sets nothing changes nothing"
        );
    }

    #[test]
    fn a_rule_with_only_a_language_changes_only_the_language() {
        let c = cfg(PasteRule {
            language: Some(Language::RU),
            ..rule("a.exe")
        });
        let (own, base) = (profile(&c, "a.exe"), profile(&c, "b.exe"));
        assert_eq!(own.language, Language::RU);
        assert_eq!(
            Profile {
                language: base.language,
                ..own
            },
            base
        );
    }

    #[test]
    fn the_model_is_the_rules_then_the_languages_then_the_global_one() {
        let mut c = cfg(PasteRule {
            model: Some("tiny".into()),
            language: Some(Language::EN),
            ..rule("a.exe")
        });
        c.model
            .by_language
            .insert(LanguageKey(Language::EN), "small".into());
        c.model
            .by_language
            .insert(LanguageKey(Language::RU), "podlodka".into());
        assert_eq!(profile(&c, "a.exe").model, "tiny");
        c.paste.rules.push(PasteRule {
            language: Some(Language::EN),
            ..rule("b.exe")
        });
        assert_eq!(
            profile(&c, "b.exe").model,
            "small",
            "the rule's language picks"
        );
        assert_eq!(
            profile(&c, "c.exe").model,
            "podlodka",
            "the global language picks"
        );
        c.model.language = Language::AUTO;
        assert_eq!(profile(&c, "c.exe").model, "turbo", "auto never maps");
        c.model.by_language.clear();
        c.paste.rules[1].language = Some(Language::EN);
        assert_eq!(profile(&c, "b.exe").model, "turbo", "an empty map is off");
    }

    #[test]
    fn the_rules_switches_win_either_way() {
        let mut c = cfg(PasteRule {
            voice_commands: Some(true),
            filler_filter: Some(false),
            pause_punctuation: Some(true),
            dictionary: Some(false),
            replacements: Some(false),
            to_file: Some(FileMode::Only),
            ..rule("a.exe")
        });
        c.text.filler_filter = true;
        let p = profile(&c, "a.exe");
        assert!(p.text.voice_commands && !p.text.filler_filter && p.text.pause_punctuation);
        assert!(!p.dictionary && !p.replacements);
        assert_eq!(p.to_file, FileMode::Only);
        assert_eq!(profile(&c, "x.exe").to_file, FileMode::Off);
    }

    #[test]
    fn newlines_follow_the_paste_mode_of_the_app() {
        let mut c = cfg(PasteRule {
            mode: Some(PasteMode::Type),
            ..rule("a.exe")
        });
        assert!(!profile(&c, "a.exe").text.newlines_allowed);
        assert!(profile(&c, "b.exe").text.newlines_allowed);
        c.paste.mode = PasteMode::Type;
        assert!(!profile(&c, "b.exe").text.newlines_allowed);
        c.paste.rules[0].mode = Some(PasteMode::Clipboard);
        assert!(profile(&c, "a.exe").text.newlines_allowed);
    }
}
