//! The whisper.cpp language table: the one list of codes the config, CLI and picker share.

use crate::types::Language;

/// One language whisper.cpp can transcribe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageInfo {
    /// Whisper's language code, also the config value.
    pub code: &'static str,
    pub english: &'static str,
    pub native: &'static str,
}

const fn l(code: &'static str, english: &'static str, native: &'static str) -> LanguageInfo {
    LanguageInfo {
        code,
        english,
        native,
    }
}

/// Every language of whisper.cpp's `g_lang`, in its order (by training data size).
pub const LANGUAGES: [LanguageInfo; 100] = [
    l("en", "English", "English"),
    l("zh", "Chinese", "中文"),
    l("de", "German", "Deutsch"),
    l("es", "Spanish", "Español"),
    l("ru", "Russian", "Русский"),
    l("ko", "Korean", "한국어"),
    l("fr", "French", "Français"),
    l("ja", "Japanese", "日本語"),
    l("pt", "Portuguese", "Português"),
    l("tr", "Turkish", "Türkçe"),
    l("pl", "Polish", "Polski"),
    l("ca", "Catalan", "Català"),
    l("nl", "Dutch", "Nederlands"),
    l("ar", "Arabic", "العربية"),
    l("sv", "Swedish", "Svenska"),
    l("it", "Italian", "Italiano"),
    l("id", "Indonesian", "Bahasa Indonesia"),
    l("hi", "Hindi", "हिन्दी"),
    l("fi", "Finnish", "Suomi"),
    l("vi", "Vietnamese", "Tiếng Việt"),
    l("he", "Hebrew", "עברית"),
    l("uk", "Ukrainian", "Українська"),
    l("el", "Greek", "Ελληνικά"),
    l("ms", "Malay", "Bahasa Melayu"),
    l("cs", "Czech", "Čeština"),
    l("ro", "Romanian", "Română"),
    l("da", "Danish", "Dansk"),
    l("hu", "Hungarian", "Magyar"),
    l("ta", "Tamil", "தமிழ்"),
    l("no", "Norwegian", "Norsk"),
    l("th", "Thai", "ไทย"),
    l("ur", "Urdu", "اردو"),
    l("hr", "Croatian", "Hrvatski"),
    l("bg", "Bulgarian", "Български"),
    l("lt", "Lithuanian", "Lietuvių"),
    l("la", "Latin", "Latina"),
    l("mi", "Maori", "Māori"),
    l("ml", "Malayalam", "മലയാളം"),
    l("cy", "Welsh", "Cymraeg"),
    l("sk", "Slovak", "Slovenčina"),
    l("te", "Telugu", "తెలుగు"),
    l("fa", "Persian", "فارسی"),
    l("lv", "Latvian", "Latviešu"),
    l("bn", "Bengali", "বাংলা"),
    l("sr", "Serbian", "Српски"),
    l("az", "Azerbaijani", "Azərbaycanca"),
    l("sl", "Slovenian", "Slovenščina"),
    l("kn", "Kannada", "ಕನ್ನಡ"),
    l("et", "Estonian", "Eesti"),
    l("mk", "Macedonian", "Македонски"),
    l("br", "Breton", "Brezhoneg"),
    l("eu", "Basque", "Euskara"),
    l("is", "Icelandic", "Íslenska"),
    l("hy", "Armenian", "Հայերեն"),
    l("ne", "Nepali", "नेपाली"),
    l("mn", "Mongolian", "Монгол"),
    l("bs", "Bosnian", "Bosanski"),
    l("kk", "Kazakh", "Қазақша"),
    l("sq", "Albanian", "Shqip"),
    l("sw", "Swahili", "Kiswahili"),
    l("gl", "Galician", "Galego"),
    l("mr", "Marathi", "मराठी"),
    l("pa", "Punjabi", "ਪੰਜਾਬੀ"),
    l("si", "Sinhala", "සිංහල"),
    l("km", "Khmer", "ខ្មែរ"),
    l("sn", "Shona", "chiShona"),
    l("yo", "Yoruba", "Yorùbá"),
    l("so", "Somali", "Soomaali"),
    l("af", "Afrikaans", "Afrikaans"),
    l("oc", "Occitan", "Occitan"),
    l("ka", "Georgian", "ქართული"),
    l("be", "Belarusian", "Беларуская"),
    l("tg", "Tajik", "Тоҷикӣ"),
    l("sd", "Sindhi", "سنڌي"),
    l("gu", "Gujarati", "ગુજરાતી"),
    l("am", "Amharic", "አማርኛ"),
    l("yi", "Yiddish", "ייִדיש"),
    l("lo", "Lao", "ລາວ"),
    l("uz", "Uzbek", "Oʻzbekcha"),
    l("fo", "Faroese", "Føroyskt"),
    l("ht", "Haitian Creole", "Kreyòl ayisyen"),
    l("ps", "Pashto", "پښتو"),
    l("tk", "Turkmen", "Türkmençe"),
    l("nn", "Nynorsk", "Nynorsk"),
    l("mt", "Maltese", "Malti"),
    l("sa", "Sanskrit", "संस्कृतम्"),
    l("lb", "Luxembourgish", "Lëtzebuergesch"),
    l("my", "Myanmar", "မြန်မာ"),
    l("bo", "Tibetan", "བོད་སྐད"),
    l("tl", "Tagalog", "Tagalog"),
    l("mg", "Malagasy", "Malagasy"),
    l("as", "Assamese", "অসমীয়া"),
    l("tt", "Tatar", "Татарча"),
    l("haw", "Hawaiian", "ʻŌlelo Hawaiʻi"),
    l("ln", "Lingala", "Lingála"),
    l("ha", "Hausa", "Hausa"),
    l("ba", "Bashkir", "Башҡортса"),
    l("jw", "Javanese", "Basa Jawa"),
    l("su", "Sundanese", "Basa Sunda"),
    l("yue", "Cantonese", "粵語"),
];

/// The table row for a whisper code, `None` for `auto` and anything unknown.
pub fn find(code: &str) -> Option<&'static LanguageInfo> {
    LANGUAGES.iter().find(|i| i.code == code)
}

/// The two languages on the segments. `recent` turns over on every pick, but a segment must not
/// jump under the pointer, so the two already shown keep their places until the language in use is
/// not one of them. A language set by hand in the config, not one of `recent`, is shown first so it
/// can be on.
#[must_use]
pub fn slots(shown: [Language; 2], current: Language, recent: [Language; 2]) -> [Language; 2] {
    // `Auto` in a slot is the start: nothing was shown yet.
    let started = !shown.contains(&Language::AUTO);
    if started && (current == Language::AUTO || shown.contains(&current)) {
        shown
    } else if current == Language::AUTO || recent.contains(&current) {
        recent
    } else {
        [current, recent[0]]
    }
}

/// The whisper languages `query` finds by code, English or native name: the exact code first, then
/// names starting with it, then names containing it, each in table order. Empty finds them all.
#[must_use]
pub fn search(query: &str) -> Vec<&'static LanguageInfo> {
    let query = query.trim().to_lowercase();
    let rank = |info: &LanguageInfo| {
        let names = [info.english.to_lowercase(), info.native.to_lowercase()];
        if info.code == query {
            Some(0)
        } else if names.iter().any(|n| n.starts_with(&query)) {
            Some(1)
        } else if names.iter().any(|n| n.contains(&query)) {
            Some(2)
        } else {
            None
        }
    };
    let mut found: Vec<_> = LANGUAGES
        .iter()
        .filter_map(|info| rank(info).map(|r| (r, info)))
        .collect();
    found.sort_by_key(|(r, _)| *r);
    found.into_iter().map(|(_, info)| info).collect()
}

#[cfg(test)]
mod tests {
    use super::{LANGUAGES, search, slots};
    use crate::types::Language;

    #[test]
    fn language_search_ranks_code_then_prefix_then_substring() {
        let codes = |q: &str| -> Vec<&str> { search(q).iter().map(|i| i.code).collect() };
        assert_eq!(codes("").len(), LANGUAGES.len());
        assert_eq!(codes("  ").len(), LANGUAGES.len());
        // By code, English and native name, in any case.
        assert_eq!(codes("haw"), ["haw"]);
        assert_eq!(codes("HAWAII")[0], "haw");
        assert_eq!(codes("中文"), ["zh"]);
        assert_eq!(codes("русск"), ["ru"]);
        // "ru" is Russian's code, ahead of the names that only contain it.
        assert_eq!(codes("ru")[0], "ru");
        assert!(codes("ru").len() > 1);
        // Tamil starts with "ta" and has the code, then Catalan and Italian, which only contain it.
        let ta = codes("ta");
        assert_eq!(ta[0], "ta");
        let position = |code: &str| ta.iter().position(|c| *c == code).unwrap();
        assert!(position("tl") < position("ca") && position("tl") < position("it"));
        assert_eq!(codes("zzzz"), Vec::<&str>::new());
    }

    #[test]
    fn language_slots_hold_still_until_a_new_language_comes() {
        let (ru, en, auto) = (Language::RU, Language::EN, Language::AUTO);
        let zh: Language = "zh".parse().unwrap();
        // First shown: the config's order.
        assert_eq!(slots([auto; 2], ru, [ru, en]), [ru, en]);
        // `recent` turned over, the same two stay where they are.
        assert_eq!(slots([ru, en], en, [en, ru]), [ru, en]);
        assert_eq!(slots([ru, en], auto, [en, ru]), [ru, en]);
        // A new one takes the front.
        assert_eq!(slots([ru, en], zh, [zh, en]), [zh, en]);
        // Set by hand, not one of `recent`: shown, and on.
        assert_eq!(slots([auto; 2], zh, [ru, en]), [zh, ru]);
        // One of the two shown is picked while `recent` has turned over: nothing moves.
        assert_eq!(slots([zh, ru], ru, [ru, zh]), [zh, ru]);
        assert_eq!(slots([zh, ru], auto, [zh, ru]), [zh, ru]);
    }
}
