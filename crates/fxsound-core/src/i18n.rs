//! UI translations.
//!
//! The Windows build ships its strings as JUCE `LocalisedStrings` files, one per language,
//! embedded through `BinaryData` (`fxsound/JuceLibraryCode/BinaryData.cpp`, `FxSound_<code>_txt`)
//! and swapped in by `FxController::setLanguage()` (`fxsound/Source/GUI/FxController.cpp:2330-2457`).
//! Those exact files — 28 of the 30 codes `FxLanguage.cpp:25` lists; `en` is the source language
//! and `hu` was never built into the Windows binary — live in `assets/translations/` and are
//! embedded here unchanged. `assets/translations/port/` carries the strings this port added, in
//! the same file format, layered on top of the original's table for the same language — and,
//! for the few strings a Windows table misspells, omits or gets wrong where the port shows them
//! (Croatian's `on`/`off` left in English, Italian's `on` as "su", eleven `"Output: "`s without
//! the space the device name needs), a repaired copy: a layer over a file that is embedded
//! unchanged is the one place such a repair can live.
//!
//! `tests/translations.rs` audits every string the interface passes to [`tr`] against every
//! language's table, so a string added without its translations fails a test rather than
//! shipping in English to twenty-eight languages, which is what happened in 0.3.0.
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

impl Language {
    /// The Windows build's table for this language on its own, as it is embedded.
    #[must_use]
    pub fn original_catalogue(&self) -> Catalogue {
        Catalogue::parse(self.code, self.original)
    }

    /// The strings this port layers over [`Self::original_catalogue`], on their own: the ones the
    /// Windows build never had, and the repairs of the ones it misspells or mistranslates.
    #[must_use]
    pub fn port_catalogue(&self) -> Catalogue {
        Catalogue::parse(self.code, self.port)
    }
}

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
        let mut catalogue = language.original_catalogue();
        catalogue.entries.extend(language.port_catalogue().entries);
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

    /// Every English string the table translates, in no particular order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
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
    use std::sync::Mutex;

    use super::*;

    /// `CURRENT` is one process-wide table, and cargo runs tests on parallel threads. Every test
    /// that *swaps* the table holds this while it does, so that one test's `set_language("de")`
    /// cannot land between another's `set_language("en")` and its assertion. Tests that only
    /// build a `Catalogue` need nothing: they never touch the global.
    static LANGUAGE_IN_EFFECT: Mutex<()> = Mutex::new(());

    /// Hold the table for the duration of a test, whether or not a previous holder panicked — a
    /// poisoned lock would otherwise turn one failure into two.
    fn hold_the_table() -> std::sync::MutexGuard<'static, ()> {
        LANGUAGE_IN_EFFECT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

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
                // The six 0.3.0 microphone strings that shipped in English everywhere…
                "Gate",
                "Compressor",
                "De-esser",
                "Denoise",
                "unavailable at this rate",
                "unavailable",
                // …the two device pickers and the readout strip…
                "Off",
                "Floor",
                "Voice",
                "Echo",
                "Reverb",
                // …the Microphone page and its values…
                "Microphone",
                "Noise suppression",
                "Denoiser channels",
                "De-reverb",
                "Echo cancellation",
                "Mild",
                "Medium",
                "Strong",
                "Linked stereo",
                "Adaptive",
                "Preset",
                // …and the calibration dialog.
                "Calibrate microphone",
                "Calibrate microphone…",
                "Stay quiet for 3 seconds",
                "Speak normally for 5 seconds",
                "Speak loudly for 2 seconds",
                "Speech",
                "Peak",
                "Clipping",
                "Apply",
                "Retry",
                "Calibration failed",
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
    fn no_port_translation_drops_or_adds_a_placeholder() {
        // `tr_args` fills `%s` in order; a translation with one too few shows the rest of the
        // sentence without its name, and one too many prints a literal `%s`.
        let mut checked = 0;
        for language in &LANGUAGES[1..] {
            let port = language.port_catalogue();
            for key in port.keys() {
                let value = port.get(key).expect("a key the table listed");
                assert_eq!(
                    value.matches("%s").count(),
                    key.matches("%s").count(),
                    "{}: {key:?} = {value:?}",
                    language.code
                );
                checked += usize::from(key.contains("%s"));
            }
        }
        // Nine strings with a placeholder, one of them with two, in each of 28 tables.
        assert!(checked >= 9 * 28, "only {checked} placeholders checked");
    }

    #[test]
    fn the_port_repairs_the_windows_words_the_new_readouts_show() {
        // The readout strip and the tray show `on`/`off`, and the tray and the notifications put
        // a device after `"Output: "` — three places where a Windows table's slip used to show.
        let get = |code: &str, key: &str| {
            Catalogue::for_language(language(code).expect(code))
                .get(key)
                .map(str::to_owned)
        };
        // Croatian left both words in English; Italian said "up", Romanian "upon".
        assert_eq!(get("hr", "on").as_deref(), Some("uključen"));
        assert_eq!(get("hr", "off").as_deref(), Some("isključen"));
        assert_eq!(get("it", "on").as_deref(), Some("acceso"));
        assert_eq!(get("ro", "on").as_deref(), Some("pornit"));
        // Italian's output was "Produzione" (production); German's lost its space.
        assert_eq!(get("it", "Output: ").as_deref(), Some("Uscita: "));
        assert_eq!(get("de", "Output: ").as_deref(), Some("Ausgabe: "));
        // …and the originals themselves are untouched.
        let original = |code: &str, key: &str| {
            language(code)
                .expect(code)
                .original_catalogue()
                .get(key)
                .map(str::to_owned)
        };
        assert_eq!(original("it", "on").as_deref(), Some("su"));
        assert_eq!(original("de", "Output: ").as_deref(), Some("Ausgabe:"));
    }

    #[test]
    fn finnish_says_output_and_input_the_way_its_windows_table_does() {
        // The original writes `Ulostulo` for output throughout; the port's own pair follows it
        // rather than setting `Lähtö` beside it in the same tooltip.
        let fi = Catalogue::for_language(language("fi").expect("fi"));
        assert_eq!(fi.get("Output: "), Some("Ulostulo: "));
        assert_eq!(fi.get("Output"), Some("Ulostulo"));
        assert_eq!(fi.get("Input"), Some("Sisääntulo"));
        assert_eq!(fi.get("Input: "), Some("Sisääntulo: "));
    }

    #[test]
    fn a_catalogue_lists_its_keys_and_a_language_its_two_layers() {
        let table = Catalogue::parse("xx", "\"One\" = \"Eins\"\n\"Two\" = \"Zwei\"\n");
        let mut keys: Vec<&str> = table.keys().collect();
        keys.sort_unstable();
        assert_eq!(keys, ["One", "Two"]);
        let ru = language("ru").expect("ru");
        let merged = Catalogue::for_language(ru);
        let (original, port) = (ru.original_catalogue(), ru.port_catalogue());
        assert!(port.len() < original.len());
        // The port wins where both have a string: Windows' Russian says "with Windows".
        assert_ne!(
            original.get("Launch on system startup"),
            port.get("Launch on system startup")
        );
        assert_eq!(
            merged.get("Launch on system startup"),
            port.get("Launch on system startup")
        );
        assert!(merged.len() >= original.len().max(port.len()));
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
        let _table = hold_the_table();
        assert!(set_language("de"));
        assert_eq!(tr("Port-only string"), "Port-only string");
        assert_eq!(tr("Settings"), "Einstellungen");
        assert!(!set_language("xx"));
        assert_eq!(current(), ENGLISH);
        assert_eq!(tr("Settings"), "Settings");
    }

    #[test]
    fn placeholders_are_filled_in_order() {
        let _table = hold_the_table();
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
