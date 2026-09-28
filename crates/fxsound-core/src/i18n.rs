//! UI translations.
//!
//! The Windows build ships its strings as JUCE `LocalisedStrings` files, one per language,
//! embedded through `BinaryData` (`fxsound/JuceLibraryCode/BinaryData.cpp`, `FxSound_<code>_txt`)
//! and swapped in by `FxController::setLanguage()` (`fxsound/Source/GUI/FxController.cpp:2330-2457`).
//! Those exact files — 29 of the 31 codes `FxLanguage.cpp:25` lists as of 1.2.16.0; `en` is the
//! source language and `hu` was never built into the Windows binary — live in
//! `assets/translations/` and are embedded here unchanged. Bulgarian (`bg`, upstream aa220cf) is
//! the newest: `FxSound_bg_txt` of v1.2.16.0's `BinaryData.cpp`, byte for byte.
//! `assets/translations/port/` carries the strings this port added, in
//! the same file format, layered on top of the original's table for the same language — and,
//! for the few strings a Windows table misspells, omits or gets wrong where the port shows them
//! (Croatian's `on`/`off` left in English, Italian's `on` as "su", eleven `"Output: "`s without
//! the space the device name needs), a repaired copy: a layer over a file that is embedded
//! unchanged is the one place such a repair can live.
//!
//! `tests/translations.rs` audits every string the interface passes to [`tr`] against every
//! language's table, so a string added without its translations fails a test rather than
//! shipping in English to every language but English, which is what happened in 0.3.0.
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
    /// The name in the language itself, shown untranslated in the language switch. The Windows
    /// build's `FxController::getLanguageName()` (`FxController.cpp:2471-2594`), with its three
    /// wrong names corrected and every name capitalised as a menu entry is (see [`LANGUAGES`]).
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

/// Every language with a translation table: English first, then the 29 translations in the order
/// of their own names.
///
/// English leads because it is the source language, the one every key is written in and every
/// untranslated string falls back to (`FxController.cpp:2593` falls back to `"English"` the same
/// way). The rest are sorted by their native names — Latin letters first, compared without their
/// accents, then Cyrillic, Arabic script, Thai and CJK, in code-point order — rather than in the
/// Windows build's order (`FxLanguage.cpp:25`), which sorts neither by code nor by name and put
/// Russian twenty-one presses of the switch away (0.4.0 audit #28).
///
/// The native names are the ones the languages call themselves, capitalised as a menu entry is.
/// Three of the Windows build's were not names of the language at all (`FxController.cpp:2471-2594`,
/// same audit): Turkish "Türk" is "a Turk", Thai "แบบไทย" is "Thai-style" and Czech "Česky" is the
/// adverb "in Czech"; they are "Türkçe", "ไทย" and "Čeština" here.
pub static LANGUAGES: [Language; 30] = [
    Language {
        code: ENGLISH,
        native_name: "English",
        original: "",
        port: "",
    },
    language!("id", "Bahasa Indonesia", "id"),
    language!("ba", "Bosanski", "ba"),
    language!("cs", "Čeština", "cs"),
    language!("de", "Deutsch", "de"),
    language!("es", "Español", "es"),
    language!("fr", "Français", "fr"),
    language!("hr", "Hrvatski", "hr"),
    language!("it", "Italiano", "it"),
    language!("nl", "Nederlands", "nl"),
    language!("no", "Norsk", "no"),
    language!("pl", "Polski", "pl"),
    language!("pt", "Português", "pt"),
    language!("pt-br", "Português (Brasil)", "pt-br"),
    language!("ro", "Română", "ro"),
    language!("sl", "Slovenščina", "sl"),
    language!("fi", "Suomi", "fi"),
    language!("sv", "Svenska", "sv"),
    language!("vi", "Tiếng Việt", "vi"),
    language!("tr", "Türkçe", "tr"),
    language!("bg", "Български", "bg"),
    language!("ru", "Русский", "ru"),
    language!("ua", "Українська", "ua"),
    language!("ar", "العربية", "ar"),
    language!("fa", "فارسی", "fa"),
    language!("th", "ไทย", "th"),
    language!("ja", "日本語", "ja"),
    language!("zh-CN", "简体中文", "zh-CN"),
    language!("zh-TW", "繁體中文", "zh-TW"),
    language!("ko", "한국어", "ko"),
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
/// are ours, not the user's; [`canonical_code`] reads what a user or a settings file writes.
#[must_use]
pub fn language(code: &str) -> Option<&'static Language> {
    LANGUAGES.iter().find(|language| language.code == code)
}

/// The code of the table `text` names, whatever way a person or a script writes it: one of
/// [`LANGUAGES`]' codes in any case (`zh-cn`, `PT-BR`), the ISO code where the Windows build uses
/// another (`uk` for `ua`, `bs` for `ba`, `nb` and `nn` for `no`), or a POSIX locale
/// (`ru_RU.UTF-8`, `pt_BR`), as [`language_for_locale`] reads one. `None` for a language with no
/// table — which `--language` refuses rather than saving a code nothing can show (0.4.0 audit
/// #28: `--language=uk` used to be kept and quietly shown in the system's language).
#[must_use]
pub fn canonical_code(text: &str) -> Option<&'static str> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    LANGUAGES
        .iter()
        .find(|language| language.code.eq_ignore_ascii_case(text))
        .map(|language| language.code)
        .or_else(|| language_for_locale(text))
}

/// The native display name for `code`, English for anything unknown (`FxController.cpp:2593`).
#[must_use]
pub fn native_name(code: &str) -> &'static str {
    language(code)
        .or_else(|| language(ENGLISH))
        .map_or("English", |language| language.native_name)
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
    /// multi-line keys. A translation left empty is no translation, as in JUCE, so the key shows in
    /// English rather than as nothing.
    ///
    /// Every line JUCE takes, this takes (0.4.0 audit #50). A line in the exact form is read
    /// whole: from its first quote to the `"` `=` `"` that closes the key, and from there to the
    /// quote that ends the line. Any other line is read the way JUCE reads it ([`juce_entry`]):
    /// the key up to its first unescaped quote, whatever stands between it and the next quote
    /// skipped — no space around the `=`, or none at all — and the translation up to the
    /// next unescaped quote or the end of the line, closed or not. Three lines of the Windows
    /// tables are of that kind and were dropped here while Windows read them: Slovenian's
    /// "Save Preset" and German's hotkey hint never close their translation, and Brazilian
    /// Portuguese's tooltip for the 16 kHz band has no space after its `=`. The exact form is
    /// tried first because JUCE's reading cuts a string at a quote inside it, which lost the
    /// Norwegian and Ukrainian tooltips that quote "thump" and "clickiness" in their keys, and cut
    /// the Persian one that quotes a word in its translation; read whole, they translate.
    #[must_use]
    pub fn parse(code: &str, text: &str) -> Self {
        let mut entries = HashMap::new();
        for line in juce_lines(text) {
            let line = line.trim();
            let Some((key, value)) = exact_entry(line).or_else(|| juce_entry(line)) else {
                continue;
            };
            let key = unescape(key);
            let value = unescape(value);
            if key.is_empty() || value.is_empty() {
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

/// The lines of a table as JUCE's `StringArray::addLines` splits them: at `\n`, `\r\n` and a
/// lone `\r`.
fn juce_lines(text: &str) -> impl Iterator<Item = &str> {
    text.split('\n')
        .flat_map(|line| line.strip_suffix('\r').unwrap_or(line).split('\r'))
}

/// A line in the exact form `"key" = "value"`, spaces around the `=` optional: the key up to the
/// first unescaped `"` that is followed, past any spaces, by `=` and an opening quote, and the
/// value from there to the quote the line ends with. `None` for any other line.
fn exact_entry(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix('"')?;
    // The quote that ends the line closes the value; everything else is between the two.
    let body = rest.strip_suffix('"')?;
    let mut from = 0;
    while let Some(at) = body[from..].find('"').map(|at| from + at) {
        let key = &body[..at];
        if let Some(after) = body[at + 1..].trim_start().strip_prefix('=')
            && let Some(value) = after.trim_start().strip_prefix('"')
            && !key.is_empty()
            && !key.ends_with('\\')
        {
            return Some((key, value));
        }
        from = at + 1;
    }
    None
}

/// The way `LocalisedStrings::loadFromText` reads a line that starts with a quote: the key from
/// there to its closing quote, the value from the next quote after that to the one after it —
/// each closing quote the first `"` not right after a backslash, or the end of the line when
/// there is none (`findCloseQuote`). `None` for a line that does not start with a quote.
fn juce_entry(line: &str) -> Option<(&str, &str)> {
    if !line.starts_with('"') {
        return None;
    }
    let key_end = close_quote(line, 1);
    let opening = close_quote(line, key_end + 1);
    let value_start = (opening + 1).min(line.len());
    let value_end = close_quote(line, value_start);
    Some((&line[1..key_end], &line[value_start..value_end]))
}

/// JUCE's `findCloseQuote`: the byte index of the first `"` at or after `from` that does not
/// follow a backslash, or the line's length when there is none. As in JUCE, the character before
/// `from` does not count as one the quote follows.
fn close_quote(line: &str, from: usize) -> usize {
    let Some(tail) = line.get(from..) else {
        return line.len();
    };
    let mut last = None;
    for (at, c) in tail.char_indices() {
        if c == '"' && last != Some('\\') {
            return from + at;
        }
        last = Some(c);
    }
    line.len()
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

/// Where a translation says a long word may be broken over two lines: `Эксперимен{-}тальное`.
///
/// Only the port's own tables use it, and only in a caption drawn where two lines fit and one does
/// not — the Settings tabs' ([`tr_breakable`]). Everywhere else [`tr`] takes it out. A soft hyphen
/// (U+00AD) would have been the standard spelling, but egui neither breaks at one nor hides it.
pub const BREAK: &str = "{-}";

/// Translate one string. The key is the English text, exactly as the C++ passes it to
/// `TRANS`; an untranslated key comes back as itself. A [`BREAK`] in the translation is taken out.
#[must_use]
pub fn tr(key: &str) -> String {
    let text = tr_breakable(key);
    if text.contains(BREAK) {
        text.replace(BREAK, "")
    } else {
        text
    }
}

/// [`tr`], with each [`BREAK`] left where the translation put it, for a caption that may take
/// two lines.
#[must_use]
pub fn tr_breakable(key: &str) -> String {
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
/// that pick still has a table. A pick written the ISO way (`uk`, `bs`) or as a locale is read
/// through [`canonical_code`].
#[must_use]
pub fn resolve(follows_system: bool, chosen: &str) -> &'static str {
    if !follows_system && let Some(code) = canonical_code(chosen) {
        return code;
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
        // Nine strings with a placeholder, one of them with two, in each of 29 tables.
        assert!(checked >= 9 * 29, "only {checked} placeholders checked");
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
    fn every_line_juce_reads_is_read_here_too() {
        // 0.4.0 audit #50: `LocalisedStrings::loadFromText` takes a translation that is never
        // closed, and an `=` with no space after it; this parser dropped both lines.
        let table = |code: &str| language(code).expect(code).original_catalogue();
        // sl.txt:120, `"Save Preset" = "Save prednastavitev` and nothing after it.
        assert_eq!(table("sl").get("Save Preset"), Some("Save prednastavitev"));
        // de.txt:108 closes with a typographic quote, which JUCE keeps as part of the text.
        assert_eq!(
            table("de").get("Press Ctrl + Alt/Shift + 0-9/A-Z to change the hotkey"),
            Some("Drücken Sie Strg + Alt/Umschalt + 0-9/A-Z, um den Hotkey zu ändern\u{201c}")
        );
        // pt-br.txt:46, `" ="` with no space before the translation's quote.
        let sixteen_k = "The core high-end range. Increase this to make your audio sound more like \
                         it's in an airy, large space, reduce it to help with room noises and \
                         unwanted echoing.";
        assert!(
            table("pt-br")
                .get(sixteen_k)
                .is_some_and(|v| v.starts_with("Gama alta.")),
            "{:?}",
            table("pt-br").get(sixteen_k)
        );
        // hr.txt:44 has no opening quote before its translation, so JUCE reads nothing there
        // either; the port's layer carries the line repaired.
        let high_mid = "The high-mid-range. Increase this to get more instrumental harmonics, \
                        reduce it to improve drums that have too much \"clickiness\" or orchestral \
                        instruments that are piercing.";
        assert_eq!(table("hr").get(high_mid), None);
        assert!(
            Catalogue::for_language(language("hr").expect("hr"))
                .get(high_mid)
                .is_some_and(|v| v.starts_with("Visoki-srednji-raspon."))
        );
    }

    #[test]
    fn a_line_in_the_exact_form_is_read_whole_where_juce_would_cut_it_at_an_inner_quote() {
        // no.txt and ua.txt quote "thump" in the key without escaping it, and fa.txt quotes a word
        // in its translation: JUCE ends the string at that quote and loses the translation.
        let high_mid = "The high-mid-range. Increase this to get more instrumental harmonics, \
                        reduce it to improve drums that have too much \"clickiness\" or orchestral \
                        instruments that are piercing.";
        for code in ["no", "ua"] {
            let table = language(code).expect(code).original_catalogue();
            assert!(table.get(high_mid).is_some(), "{code}");
            assert!(
                table
                    .get("Super-low Bass. Increase this for more rumble and ")
                    .is_none(),
                "{code}: no half key"
            );
        }
        let text = "\"a \"quoted\" word\" = \"ein \"zitiertes\" Wort\"\n";
        assert_eq!(
            Catalogue::parse("xx", text).get("a \"quoted\" word"),
            Some("ein \"zitiertes\" Wort")
        );
    }

    #[test]
    fn the_juce_reading_takes_spaces_line_breaks_and_empty_translations_as_juce_does() {
        let text = "  \"Indented\" = \"Eingerückt\"  \r\
                    \"Tight\"=\"Eng\"\r\n\
                    \"Open\" = \"Offen\n\
                    \"Empty\" = \"\"\n\
                    \"Unquoted\" = Nackt\n\
                    \"\" = \"Kein Schlüssel\"\n\
                    language: Test\n";
        let table = Catalogue::parse("xx", text);
        assert_eq!(
            table.get("Indented"),
            Some("Eingerückt"),
            "a lone CR ends a line"
        );
        assert_eq!(table.get("Tight"), Some("Eng"));
        assert_eq!(table.get("Open"), Some("Offen"));
        assert_eq!(table.get("Empty"), None, "an empty translation is none");
        assert_eq!(
            table.get("Unquoted"),
            None,
            "JUCE reads nothing here either"
        );
        assert_eq!(table.len(), 3);
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
    fn tr_takes_the_break_mark_out_and_tr_breakable_keeps_it() {
        let _table = hold_the_table();
        assert!(set_language("ru"));
        assert_eq!(tr("Experimental"), "Экспериментальное");
        assert_eq!(tr_breakable("Experimental"), "Эксперимен{-}тальное");
        // A string without the mark is the same either way.
        assert_eq!(tr_breakable("Settings"), tr("Settings"));
        set_language(ENGLISH);
        assert_eq!(tr("Experimental"), "Experimental");
    }

    #[test]
    fn only_a_settings_tab_caption_carries_the_break_mark_in_any_table() {
        // The captions drawn with `tr_breakable` (`SettingsTab::nav_label`); anywhere else the
        // mark would be taken out by `tr` and never break anything.
        let captions = [
            "Audio",
            "General",
            "Help",
            "Microphone",
            "Applications",
            "Experimental",
        ];
        for language in &LANGUAGES[1..] {
            for catalogue in [language.original_catalogue(), language.port_catalogue()] {
                for key in catalogue.keys() {
                    let text = catalogue.get(key).unwrap_or_default();
                    assert!(
                        !text.contains(BREAK) || captions.contains(&key),
                        "{}: {key:?} = {text:?}",
                        language.code
                    );
                    // One mark at most, inside a word, never at either end.
                    assert!(
                        text.matches(BREAK).count() <= 1,
                        "{}: {text:?}",
                        language.code
                    );
                    assert!(
                        !text.starts_with(BREAK) && !text.ends_with(BREAK),
                        "{}: {text:?}",
                        language.code
                    );
                }
            }
        }
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
        assert_eq!(language_for_locale("bg_BG.UTF-8"), Some("bg"));
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
        assert_eq!(LANGUAGES.len(), 30);
        assert_eq!(LANGUAGES[0].code, ENGLISH);
        assert!(language("hu").is_none());
        assert_eq!(native_name("ru"), "Русский");
        assert_eq!(native_name("hu"), "English");
        let mut codes: Vec<&str> = LANGUAGES.iter().map(|language| language.code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), 30, "every code once");
    }

    /// How the switch orders two names: Latin letters without their accents and in lower case,
    /// every other script as it is, so the scripts come in code-point order — Latin, Cyrillic,
    /// Arabic, Thai, then the ideographs and Hangul.
    fn sort_key(name: &str) -> String {
        name.chars()
            .map(|c| match c {
                'á' | 'à' | 'â' | 'ã' | 'ă' => 'a',
                'Č' | 'č' | 'ç' => 'c',
                'é' | 'è' | 'ê' | 'ế' | 'ệ' => 'e',
                'í' | 'î' => 'i',
                'ñ' => 'n',
                'ó' | 'ô' | 'õ' | 'ö' => 'o',
                'Š' | 'š' | 'ș' | 'ş' => 's',
                'ú' | 'ü' => 'u',
                other => other.to_lowercase().next().unwrap_or(other),
            })
            .collect()
    }

    #[test]
    fn after_english_the_languages_are_in_the_order_of_their_own_names() {
        // 0.4.0 audit #28: the Windows order (`FxLanguage.cpp:25`) followed neither the codes nor
        // the names.
        let rest = &LANGUAGES[1..];
        for pair in rest.windows(2) {
            assert!(
                sort_key(pair[0].native_name) < sort_key(pair[1].native_name),
                "{} before {}",
                pair[0].native_name,
                pair[1].native_name
            );
        }
        let names: Vec<&str> = LANGUAGES.iter().map(|l| l.native_name).collect();
        assert_eq!(
            &names[..4],
            ["English", "Bahasa Indonesia", "Bosanski", "Čeština"]
        );
        assert_eq!(
            &names[19..23],
            ["Türkçe", "Български", "Русский", "Українська"],
            "Cyrillic after the last Latin name"
        );
        assert_eq!(names[29], "한국어");
    }

    #[test]
    fn every_language_is_called_by_its_own_name_for_itself() {
        // 0.4.0 audit #28: "Türk" is "a Turk", "แบบไทย" is "Thai-style", "Česky" is "in Czech".
        assert_eq!(native_name("tr"), "Türkçe");
        assert_eq!(native_name("th"), "ไทย");
        assert_eq!(native_name("cs"), "Čeština");
        assert_eq!(native_name("pt-br"), "Português (Brasil)");
        // Capitalised as a menu entry is, in every script that has capitals.
        for language in &LANGUAGES {
            let first = language.native_name.chars().next().expect("a name");
            assert!(
                !first.is_lowercase(),
                "{}: {}",
                language.code,
                language.native_name
            );
        }
    }

    #[test]
    fn a_language_is_found_by_its_code_in_any_case_by_its_iso_code_or_by_a_locale() {
        assert_eq!(canonical_code("ru"), Some("ru"));
        assert_eq!(canonical_code(" zh-cn "), Some("zh-CN"));
        assert_eq!(canonical_code("PT-BR"), Some("pt-br"));
        // The ISO codes the Windows build spells its own way.
        assert_eq!(canonical_code("uk"), Some("ua"));
        assert_eq!(canonical_code("bs"), Some("ba"));
        assert_eq!(canonical_code("nb"), Some("no"));
        assert_eq!(canonical_code("nn"), Some("no"));
        // Locales, as `LANG` has them.
        assert_eq!(canonical_code("ru_RU.UTF-8"), Some("ru"));
        assert_eq!(canonical_code("pt_BR"), Some("pt-br"));
        assert_eq!(canonical_code("zh_TW"), Some("zh-TW"));
        assert_eq!(canonical_code("en_GB"), Some(ENGLISH));
        // No table, no language.
        assert_eq!(canonical_code("hu"), None);
        assert_eq!(canonical_code("xx"), None);
        assert_eq!(canonical_code(""), None);
        // And a settings file that says `uk` shows Ukrainian rather than the system's language.
        assert_eq!(resolve(false, "uk"), "ua");
    }

    #[test]
    fn bulgarian_speaks_bulgarian() {
        // `FxController::getLanguageName`, `\u0431\u044a\u043b…` spelled out, capitalised.
        assert_eq!(native_name("bg"), "Български");
        assert_eq!(
            language_from_locales(["".to_owned(), "bg_BG.UTF-8".to_owned()]),
            "bg"
        );

        let bulgarian = language("bg").expect("a table for bg");
        let original = bulgarian.original_catalogue();
        assert_eq!(original.get("Language"), Some("Език"));
        assert_eq!(original.get("Output: "), Some("Изход: "));
        let table = Catalogue::for_language(bulgarian);
        assert_eq!(table.get("Microphone"), Some("Микрофон"));
        assert_eq!(table.get("Input: "), Some("Вход: "));
        assert_eq!(
            table
                .get("Could not load %s")
                .map(|v| v.matches("%s").count()),
            Some(1)
        );
    }
}
