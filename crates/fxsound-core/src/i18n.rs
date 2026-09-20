//! UI translations.
//!
//! The Windows build ships its strings as JUCE `LocalisedStrings` files, one per language,
//! embedded through `BinaryData` (`fxsound/JuceLibraryCode/BinaryData.cpp`, `FxSound_<code>_txt`)
//! and swapped in by `FxController::setLanguage()` (`fxsound/Source/GUI/FxController.cpp:2330-2457`).
//! Those exact files — 28 of the 30 codes `FxLanguage.cpp:25` lists; `en` is the source language
//! and `hu` was never built into the Windows binary — live in `assets/translations/` and are
//! embedded here unchanged. `assets/translations/port/` carries the handful of strings this port
//! added, in the same file format, layered on top of the original's table for the same language.
//!
//! Lookups go through [`tr`]: the English source string is the key, exactly as `TRANS("…")` is in
//! the C++, and an unknown key comes back unchanged, so a missing translation degrades to English
//! rather than to nothing. The table in effect is swapped atomically with [`set_language`], which
//! is what lets the Settings pane switch languages live: every view asks [`tr`] on every frame,
//! the way `FxAudioControls::paint` re-applies its captions on every repaint.
//!
//! Which language is in effect is decided by [`resolve`]: the system locale unless the user has
//! picked one explicitly in Settings (`Settings::language_follows_system`).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use arc_swap::ArcSwap;

/// The source language; there is no file for it because the keys *are* the English strings.
pub const ENGLISH: &str = "en";

/// One language the UI can be shown in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Language {
    /// The code the Windows build uses (`FxLanguage.cpp:25`) — persisted, so it is stable and not
    /// always ISO (`ua` for Ukrainian, `ba` for Bosnian).
    pub code: &'static str,
    /// The name in the language itself, as `FxController::getLanguageName()` returns it
    /// (`FxController.cpp:2471-2594`); shown untranslated in the language switch.
    pub native_name: &'static str,
    original: &'static str,
    port: &'static str,
}

macro_rules! language {
    ($code:literal, $name:literal, $file:literal) => {
        Language {
            code: $code,
            native_name: $name,
            original: include_str!(concat!("../../../assets/translations/", $file, ".txt")),
            port: include_str!(concat!("../../../assets/translations/port/", $file, ".txt")),
        }
    };
}

/// Every language with a translation table, in the Windows build's order (`FxLanguage.cpp:25`).
///
/// English first, so index 0 is the fallback the way `getLanguageName` falls back to
/// `"English"` (`FxController.cpp:2593`).
pub static LANGUAGES: [Language; 29] = [
    Language {
        code: ENGLISH,
        native_name: "English",
        original: "",
        port: "",
    },
    language!("ar", "العربية", "ar"),
    language!("ba", "bosanski", "ba"),
    language!("hr", "hrvatski", "hr"),
    language!("cs", "Česky", "cs"),
    language!("de", "Deutsch", "de"),
    language!("es", "Español", "es"),
    language!("fi", "Suomi", "fi"),
    language!("fr", "français", "fr"),
    language!("id", "bahasa Indonesia", "id"),
    language!("it", "Italiano", "it"),
    language!("ja", "日本語", "ja"),
    language!("ko", "한국어", "ko"),
    language!("nl", "Nederlands", "nl"),
    language!("no", "Norsk", "no"),
    language!("fa", "فارسی", "fa"),
    language!("pl", "Polski", "pl"),
    language!("pt", "Português", "pt"),
    language!("pt-br", "português brasileiro", "pt-br"),
    language!("ro", "Română", "ro"),
    language!("ru", "русский", "ru"),
    language!("sl", "Slovenščina", "sl"),
    language!("sv", "svenska", "sv"),
    language!("th", "แบบไทย", "th"),
    language!("tr", "Türk", "tr"),
    language!("ua", "українська", "ua"),
    language!("vi", "Tiếng Việt", "vi"),
    language!("zh-CN", "简体中文", "zh-CN"),
    language!("zh-TW", "繁體中文", "zh-TW"),
];

/// The language for `code`, if there is a table for it. Exact match, case-sensitive — the codes
/// are ours, not the user's.
#[must_use]
pub fn language(code: &str) -> Option<&'static Language> {
    LANGUAGES.iter().find(|language| language.code == code)
}

/// The native display name for `code`, English for anything unknown (`FxController.cpp:2593`).
#[must_use]
pub fn native_name(code: &str) -> &'static str {
    language(code).map_or(LANGUAGES[0].native_name, |language| language.native_name)
}

// ---------------------------------------------------------------------------------------------
// The file format
// ---------------------------------------------------------------------------------------------

/// A parsed translation table: English source string → translation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalogue {
    code: String,
    entries: HashMap<String, String>,
}

impl Catalogue {
    /// Parse a JUCE `LocalisedStrings` file (`juce_LocalisedStrings.cpp`, `loadFromText`).
    ///
    /// The format is line based: `language:` and `countries:` headers, then one
    /// `"source" = "translation"` per line. Both strings are unescaped the way JUCE's
    /// `unescapeString` does (`\"`, `\'`, `\\`, `\t`, `\r`, `\n`), and Windows line breaks
    /// inside a string are then folded to `\n`, because that is how this port writes its own
    /// multi-line keys. Anything that does not parse is skipped rather than fatal: a broken line
    /// costs one translation, not the language.
    #[must_use]
    pub fn parse(code: &str, text: &str) -> Self {
        let mut entries = HashMap::new();
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            let Some(rest) = line.strip_prefix('"') else {
                continue;
            };
            let Some((key, value)) = rest.split_once("\" = \"") else {
                continue;
            };
            let Some(value) = value.strip_suffix('"') else {
                continue;
            };
            let key = unescape(key);
            let value = unescape(value);
            if key.is_empty() {
                continue;
            }
            entries.insert(key, value);
        }
        Self {
            code: code.to_owned(),
            entries,
        }
    }

    /// The original's table for `language`, with the port's additions merged over it.
    #[must_use]
    pub fn for_language(language: &Language) -> Self {
        let mut catalogue = Self::parse(language.code, language.original);
        let port = Self::parse(language.code, language.port);
        catalogue.entries.extend(port.entries);
        catalogue
    }

    /// The language code this table is for.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// How many strings the table translates.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The translation of `key`, if there is one.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }
}

/// JUCE's `unescapeString`, plus the CRLF fold described on [`Catalogue::parse`].
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out.replace("\r\n", "\n")
}

// ---------------------------------------------------------------------------------------------
// The table in effect
// ---------------------------------------------------------------------------------------------

static CURRENT: LazyLock<ArcSwap<Catalogue>> =
    LazyLock::new(|| ArcSwap::from_pointee(Catalogue::parse(ENGLISH, "")));

/// Put the table for `code` into effect, English for an unknown code. Returns whether the code
/// was known.
///
/// Building a table is a few hundred small allocations and happens once per switch; every
/// [`tr`] afterwards is one lock-free load and one hash lookup.
pub fn set_language(code: &str) -> bool {
    match language(code) {
        Some(language) => {
            CURRENT.store(Arc::new(Catalogue::for_language(language)));
            true
        }
        None => {
            CURRENT.store(Arc::new(Catalogue::parse(ENGLISH, "")));
            false
        }
    }
}

/// The code of the table in effect.
#[must_use]
pub fn current() -> String {
    CURRENT.load().code.clone()
}

/// Translate one string. The key is the English text, exactly as the C++ passes it to
/// `TRANS`; an untranslated key comes back as itself.
#[must_use]
pub fn tr(key: &str) -> String {
    CURRENT
        .load()
        .get(key)
        .map_or_else(|| key.to_owned(), str::to_owned)
}

/// Translate a string with `%s` placeholders, filling them in order — the original formats its
/// notifications this way (`"Preset %s is deleted."`, `FxController.cpp:1313`).
#[must_use]
pub fn tr_args(key: &str, args: &[&str]) -> String {
    let mut text = tr(key);
    for arg in args {
        text = text.replacen("%s", arg, 1);
    }
    text
}

// ---------------------------------------------------------------------------------------------
// Which language
// ---------------------------------------------------------------------------------------------

/// The language the desktop session is running in, as one of [`LANGUAGES`]' codes.
///
/// Reads `LC_ALL`, then `LC_MESSAGES`, then `LANG` — the precedence `setlocale(3)` gives them —
/// and takes the first that names a language there is a table for. `C`/`POSIX` mean English.
#[must_use]
pub fn system_language() -> &'static str {
    let values = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok());
    language_from_locales(values)
}

/// [`system_language`] over an explicit list, for tests and for callers with their own source.
#[must_use]
pub fn language_from_locales<I>(locales: I) -> &'static str
where
    I: IntoIterator<Item = String>,
{
    locales
        .into_iter()
        .filter(|value| !value.trim().is_empty())
        .find_map(|value| language_for_locale(&value))
        .unwrap_or(ENGLISH)
}

/// Map one POSIX locale string (`ru_RU.UTF-8`, `pt_BR`, `zh_TW.utf8@…`) to a table code.
///
/// The Windows codes are not all ISO 639, so a few are spelled out: Ukrainian is `uk` on Linux
/// and `ua` here, Bosnian `bs`/`ba`, Norwegian `nb`/`nn`/`no`. Portuguese and Chinese pick their
/// variant from the region. Anything with no table yields `None` so the next variable can try.
#[must_use]
pub fn language_for_locale(locale: &str) -> Option<&'static str> {
    let locale = locale.trim().to_ascii_lowercase();
    let locale = locale.split(['.', '@']).next().unwrap_or_default();
    if locale.is_empty() {
        return None;
    }
    if locale == "c" || locale == "posix" {
        return Some(ENGLISH);
    }
    let mut parts = locale.split(['_', '-']);
    let lang = parts.next().unwrap_or_default();
    let region = parts.next().unwrap_or_default();

    let code = match (lang, region) {
        ("uk", _) => "ua",
        ("bs", _) => "ba",
        ("nb" | "nn" | "no", _) => "no",
        ("pt", "br") => "pt-br",
        ("pt", _) => "pt",
        ("zh", "tw" | "hk" | "mo" | "hant") => "zh-TW",
        ("zh", _) => "zh-CN",
        ("fa" | "pes", _) => "fa",
        ("in", _) => "id",
        (other, _) => other,
    };
    language(code).map(|language| language.code)
}

/// The language the app should show: the system's, unless the user picked one in Settings and
/// that pick still has a table.
#[must_use]
pub fn resolve(follows_system: bool, chosen: &str) -> &'static str {
    if !follows_system && let Some(language) = language(chosen) {
        return language.code;
    }
    system_language()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_embedded_table_parses_to_the_originals_hundred_odd_strings() {
        for language in &LANGUAGES[1..] {
            let catalogue = Catalogue::parse(language.code, language.original);
            assert!(
                catalogue.len() >= 130,
                "{}: only {} strings parsed",
                language.code,
                catalogue.len()
            );
        }
    }

    #[test]
    fn every_language_carries_the_ports_own_strings_too() {
        // `assets/translations/port/<code>.txt` layers the strings the Windows build never had.
        for language in &LANGUAGES[1..] {
            let table = Catalogue::for_language(language);
            for key in [
                "System language",
                "Input: ",
                "Output",
                "Input",
                "Keyboard shortcuts",
            ] {
                let value = table.get(key);
                assert!(
                    value.is_some_and(|v| !v.is_empty()),
                    "{}: {key:?}",
                    language.code
                );
            }
            // Placeholders survive translation.
            assert_eq!(
                table
                    .get("Could not load %s")
                    .map_or(0, |v| v.matches("%s").count()),
                1,
                "{}",
                language.code
            );
        }
    }

    #[test]
    fn the_russian_table_translates_the_menu_and_the_sliders() {
        let ru = Catalogue::for_language(language("ru").expect("ru"));
        assert_eq!(ru.get("Settings"), Some("Настройки"));
        assert_eq!(ru.get("Bass Boost"), Some("Усиление басов"));
        assert!(ru.get("Clarity").is_some());
        assert!(ru.get("Surround Sound").is_some());
        assert_eq!(ru.get("Not a key"), None);
    }

    #[test]
    fn juce_escapes_and_windows_line_breaks_are_unescaped() {
        let text = "language: Test\r\n\r\n\"There\\'s a \\\"thump\\\"\\r\\nhere\" = \"Есть \\\"стук\\\"\\r\\nтут\"\r\n";
        let catalogue = Catalogue::parse("xx", text);
        assert_eq!(
            catalogue.get("There's a \"thump\"\nhere"),
            Some("Есть \"стук\"\nтут")
        );
    }

    #[test]
    fn the_effect_tooltips_keys_match_after_the_line_break_fold() {
        // The C++ passes `"Enhances and elevates high end\r\nfidelity and presence"`; this port
        // writes the same key with a bare `\n`, and the fold makes both spellings meet.
        let ru = Catalogue::for_language(language("ru").expect("ru"));
        assert!(
            ru.get("Enhances and elevates high end\nfidelity and presence")
                .is_some()
        );
    }

    #[test]
    fn an_unknown_key_comes_back_as_itself_in_every_language() {
        assert!(set_language("de"));
        assert_eq!(tr("Port-only string"), "Port-only string");
        assert_eq!(tr("Settings"), "Einstellungen");
        assert!(!set_language("xx"));
        assert_eq!(current(), ENGLISH);
        assert_eq!(tr("Settings"), "Settings");
    }

    #[test]
    fn placeholders_are_filled_in_order() {
        set_language(ENGLISH);
        assert_eq!(
            tr_args("Preset %s is deleted.", &["Jazz"]),
            "Preset Jazz is deleted."
        );
        assert_eq!(tr_args("%s and %s", &["a", "b"]), "a and b");
        assert_eq!(tr_args("no placeholder", &["a"]), "no placeholder");
    }

    #[test]
    fn the_users_locale_resolves_to_russian() {
        // `LC_ALL` empty, `LANG=ru_RU.UTF-8` — the shape on the reference machine.
        let picked = language_from_locales(["".to_owned(), "ru_RU.UTF-8".to_owned()]);
        assert_eq!(picked, "ru");
    }

    #[test]
    fn locales_map_to_the_windows_codes() {
        assert_eq!(language_for_locale("uk_UA.UTF-8"), Some("ua"));
        assert_eq!(language_for_locale("bs_BA"), Some("ba"));
        assert_eq!(language_for_locale("nb_NO.UTF-8"), Some("no"));
        assert_eq!(language_for_locale("pt_BR.UTF-8"), Some("pt-br"));
        assert_eq!(language_for_locale("pt_PT"), Some("pt"));
        assert_eq!(language_for_locale("zh_TW.UTF-8"), Some("zh-TW"));
        assert_eq!(language_for_locale("zh_CN.UTF-8"), Some("zh-CN"));
        assert_eq!(language_for_locale("de_AT.UTF-8@euro"), Some("de"));
        assert_eq!(language_for_locale("C.UTF-8"), Some(ENGLISH));
        assert_eq!(language_for_locale("POSIX"), Some(ENGLISH));
        assert_eq!(
            language_for_locale("hu_HU.UTF-8"),
            None,
            "no Hungarian table"
        );
        assert_eq!(language_for_locale("xx"), None);
        assert_eq!(language_from_locales(["hu_HU".to_owned()]), ENGLISH);
    }

    #[test]
    fn an_explicit_choice_wins_only_while_it_has_a_table() {
        assert_eq!(resolve(false, "ja"), "ja");
        assert_eq!(resolve(false, "hu"), system_language());
        assert_eq!(resolve(true, "ja"), system_language());
    }

    #[test]
    fn the_language_list_is_the_originals_minus_hungarian_with_english_first() {
        assert_eq!(LANGUAGES.len(), 29);
        assert_eq!(LANGUAGES[0].code, ENGLISH);
        assert_eq!(LANGUAGES[28].code, "zh-TW");
        assert!(language("hu").is_none());
        assert_eq!(native_name("ru"), "русский");
        assert_eq!(native_name("hu"), "English");
    }
}
