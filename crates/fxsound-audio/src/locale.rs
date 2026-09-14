//! The localised half of the two virtual nodes' descriptions.
//!
//! The Windows build has one virtual endpoint and calls it "FxSound"
//! (`SND_DEVICES_DFX_DEVICE_STRING`, `audiopassthru/include/sndDevices.h:51`). The Linux port can
//! be a sink *or* a source, and on the user's machine the two would otherwise read identically in
//! `pavucontrol` and the desktop's volume widget — exactly the confusion the USB microphone in
//! `tests/fixtures/pw-dump-sinks.json` already causes with its twin "fifine Microphone Аналоговый
//! стерео" nodes. So `node.description` becomes `"FxSound (<Output|Input>)"`, with the word in
//! the system language, the way ALSA/UCM already localise "Analog Stereo" into "Аналоговый
//! стерео" on that same machine (`docs/spec/12-audio-io.md` §28.4).
//!
//! Only the *description* is localised. `node.name` — `fxsound_sink`, `fxsound_source` — is
//! matched by name and written into the `default` metadata, so it stays ASCII and fixed
//! (`docs/spec/12-audio-io.md` §21).

use fxsound_core::DeviceDirection;

use crate::SINK_DESCRIPTION;

/// The environment variables that carry the message locale, in POSIX precedence order
/// (`setlocale(3)`: `LC_ALL` overrides `LC_MESSAGES`, which overrides `LANG`).
pub const LOCALE_VARIABLES: [&str; 3] = ["LC_ALL", "LC_MESSAGES", "LANG"];

/// The translations of "Output" / "Input", keyed by ISO 639-1 language code.
///
/// Short on purpose: these are the two words a user sees next to "FxSound" in a device list, and
/// a wrong translation there is worse than an English one, so only languages with an unambiguous
/// everyday word for a sound *output* and *input* are listed. Anything else falls back to English,
/// which is also what the UI itself speaks (`docs/spec/07-startup-tray.md` — only `en` ships).
const WORDS: [(&str, &str, &str); 30] = [
    ("ru", "Вывод", "Ввод"),
    ("uk", "Вивід", "Ввід"),
    ("de", "Ausgabe", "Eingabe"),
    ("fr", "Sortie", "Entrée"),
    ("es", "Salida", "Entrada"),
    ("pt", "Saída", "Entrada"),
    ("it", "Uscita", "Ingresso"),
    ("nl", "Uitvoer", "Invoer"),
    ("pl", "Wyjście", "Wejście"),
    ("tr", "Çıkış", "Giriş"),
    ("ja", "出力", "入力"),
    ("ko", "출력", "입력"),
    ("zh", "输出", "输入"),
    ("ar", "الإخراج", "الإدخال"),
    ("fa", "خروجی", "ورودی"),
    ("th", "เอาต์พุต", "อินพุต"),
    ("vi", "Đầu ra", "Đầu vào"),
    ("id", "Keluaran", "Masukan"),
    ("cs", "Výstup", "Vstup"),
    ("hr", "Izlaz", "Ulaz"),
    // The Windows build's `ba` and ISO's `bs` for Bosnian.
    ("ba", "Izlaz", "Ulaz"),
    ("bs", "Izlaz", "Ulaz"),
    ("sl", "Izhod", "Vhod"),
    ("ro", "Ieșire", "Intrare"),
    ("fi", "Lähtö", "Tulo"),
    ("sv", "Utgång", "Ingång"),
    ("no", "Utgang", "Inngang"),
    ("nb", "Utgang", "Inngang"),
    ("nn", "Utgang", "Inngang"),
    ("hu", "Kimenet", "Bemenet"),
];

/// The language part of a locale string: `ru_RU.UTF-8` → `ru`, `de_DE@euro` → `de`,
/// `zh_CN.UTF-8` → `zh`, lower-cased. `None` for an empty value and for the `C`/`POSIX` locales,
/// which have no language and must fall back to English.
#[must_use]
pub fn language_of(locale: &str) -> Option<String> {
    let language = locale
        .trim()
        .split(['_', '.', '@'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if language.is_empty() || language == "c" || language == "posix" {
        return None;
    }
    Some(language)
}

/// The system language, read the way `setlocale(LC_MESSAGES, "")` would resolve it: the first of
/// [`LOCALE_VARIABLES`] that is set and non-empty, reduced with [`language_of`].
///
/// `get` is the environment; in production it is `std::env::var`, in tests it is a table, which
/// is what keeps the choice deterministic on a CI box whose own `LANG` could be anything.
#[must_use]
pub fn language_from_env(get: &impl Fn(&str) -> Option<String>) -> Option<String> {
    LOCALE_VARIABLES
        .iter()
        .filter_map(|name| get(name))
        .find(|value| !value.trim().is_empty())
        .and_then(|value| language_of(&value))
}

/// [`language_from_env`] over this process's real environment.
#[must_use]
pub fn system_language() -> Option<String> {
    language_from_env(&|name| std::env::var(name).ok())
}

/// The word for a direction in `language` (a lower-case ISO 639-1 code), English when the
/// language is unknown or `None`.
#[must_use]
pub fn direction_word(direction: DeviceDirection, language: Option<&str>) -> &'static str {
    let Some(language) = language else {
        return direction.label();
    };
    // Accept the UI's codes too (`zh-CN`, `pt-br`, and the Windows build's `ua` for Ukrainian):
    // the table is keyed by bare language.
    let language = language.to_ascii_lowercase();
    let language = language.split(['-', '_']).next().unwrap_or_default();
    let language = if language == "ua" { "uk" } else { language };
    WORDS
        .iter()
        .find(|(code, _, _)| *code == language)
        .map_or(direction.label(), |&(_, output, input)| match direction {
            DeviceDirection::Output => output,
            DeviceDirection::Input => input,
        })
}

/// The `node.description` (and `node.nick`) of FxSound's virtual node for a direction:
/// `"FxSound (Вывод)"`, `"FxSound (Input)"`, …
#[must_use]
pub fn node_description(direction: DeviceDirection, language: Option<&str>) -> String {
    format!("{SINK_DESCRIPTION} ({})", direction_word(direction, language))
}

/// [`node_description`] for the virtual sink, in the system language.
#[must_use]
pub fn sink_description() -> String {
    node_description(DeviceDirection::Output, system_language().as_deref())
}

/// [`node_description`] for the virtual source, in the system language.
#[must_use]
pub fn source_description() -> String {
    node_description(DeviceDirection::Input, system_language().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name: &str| {
            owned
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn the_language_is_the_part_before_the_territory_and_the_encoding() {
        assert_eq!(language_of("ru_RU.UTF-8").as_deref(), Some("ru"));
        assert_eq!(language_of("de_DE@euro").as_deref(), Some("de"));
        assert_eq!(language_of("zh_CN.UTF-8").as_deref(), Some("zh"));
        assert_eq!(language_of("ja_JP.eucJP").as_deref(), Some("ja"));
        assert_eq!(language_of("en").as_deref(), Some("en"));
        assert_eq!(language_of("PT_BR").as_deref(), Some("pt"), "case-insensitive");
        assert_eq!(language_of(""), None);
        assert_eq!(language_of("   "), None);
        assert_eq!(language_of("C"), None, "the C locale has no language");
        assert_eq!(language_of("C.UTF-8"), None);
        assert_eq!(language_of("POSIX"), None);
    }

    #[test]
    fn the_locale_variables_are_read_in_posix_precedence_order() {
        // The user's own machine: `LC_ALL` is set but *empty*, `LC_MESSAGES` and `LANG` are
        // `ru_RU.UTF-8`. An empty variable must be skipped, not treated as "no language".
        assert_eq!(
            language_from_env(&env(&[
                ("LC_ALL", ""),
                ("LC_MESSAGES", "ru_RU.UTF-8"),
                ("LANG", "ru_RU.UTF-8"),
            ]))
            .as_deref(),
            Some("ru")
        );
        // `LC_ALL` overrides everything when it is set.
        assert_eq!(
            language_from_env(&env(&[("LC_ALL", "de_DE.UTF-8"), ("LANG", "ru_RU.UTF-8")]))
                .as_deref(),
            Some("de")
        );
        // `LC_MESSAGES` overrides `LANG`.
        assert_eq!(
            language_from_env(&env(&[("LC_MESSAGES", "fr_FR.UTF-8"), ("LANG", "ru_RU.UTF-8")]))
                .as_deref(),
            Some("fr")
        );
        // `LANG` alone.
        assert_eq!(
            language_from_env(&env(&[("LANG", "pl_PL.UTF-8")])).as_deref(),
            Some("pl")
        );
        // Nothing set, or only the C locale: English.
        assert_eq!(language_from_env(&env(&[])), None);
        assert_eq!(language_from_env(&env(&[("LANG", "C.UTF-8")])), None);
        // `LC_ALL=C` wins over a Russian `LANG`, which is what a script setting it expects.
        assert_eq!(
            language_from_env(&env(&[("LC_ALL", "C"), ("LANG", "ru_RU.UTF-8")])),
            None
        );
    }

    #[test]
    fn the_descriptions_carry_the_localised_direction_word() {
        assert_eq!(
            node_description(DeviceDirection::Output, Some("ru")),
            "FxSound (Вывод)"
        );
        assert_eq!(
            node_description(DeviceDirection::Input, Some("ru")),
            "FxSound (Ввод)"
        );
        assert_eq!(
            node_description(DeviceDirection::Output, None),
            "FxSound (Output)"
        );
        assert_eq!(
            node_description(DeviceDirection::Input, None),
            "FxSound (Input)"
        );
        assert_eq!(
            node_description(DeviceDirection::Input, Some("en")),
            "FxSound (Input)",
            "English is English"
        );
        assert_eq!(
            node_description(DeviceDirection::Output, Some("xx")),
            "FxSound (Output)",
            "an unknown language falls back to English rather than to nothing"
        );
        assert_eq!(direction_word(DeviceDirection::Output, Some("RU")), "Вывод", "case-insensitive");
    }

    #[test]
    fn every_listed_language_has_two_distinct_non_empty_words() {
        for (code, _, _) in WORDS {
            let output = direction_word(DeviceDirection::Output, Some(code));
            let input = direction_word(DeviceDirection::Input, Some(code));
            assert!(!output.is_empty() && !input.is_empty(), "{code}");
            assert_ne!(output, input, "{code}: the two nodes must be distinguishable");
            assert_ne!(output, "Output", "{code} must actually be translated");
        }
        // The table the task prescribes, spot-checked.
        assert_eq!(direction_word(DeviceDirection::Output, Some("uk")), "Вивід");
        assert_eq!(direction_word(DeviceDirection::Input, Some("uk")), "Ввід");
        assert_eq!(direction_word(DeviceDirection::Output, Some("de")), "Ausgabe");
        assert_eq!(direction_word(DeviceDirection::Input, Some("de")), "Eingabe");
        assert_eq!(direction_word(DeviceDirection::Output, Some("fr")), "Sortie");
        assert_eq!(direction_word(DeviceDirection::Input, Some("fr")), "Entrée");
        assert_eq!(direction_word(DeviceDirection::Output, Some("es")), "Salida");
        assert_eq!(direction_word(DeviceDirection::Input, Some("pt")), "Entrada");
        assert_eq!(direction_word(DeviceDirection::Output, Some("it")), "Uscita");
        assert_eq!(direction_word(DeviceDirection::Input, Some("nl")), "Invoer");
        assert_eq!(direction_word(DeviceDirection::Output, Some("pl")), "Wyjście");
        assert_eq!(direction_word(DeviceDirection::Input, Some("tr")), "Giriş");
        assert_eq!(direction_word(DeviceDirection::Output, Some("ja")), "出力");
        assert_eq!(direction_word(DeviceDirection::Input, Some("ko")), "입력");
        assert_eq!(direction_word(DeviceDirection::Output, Some("zh")), "输出");
    }

    #[test]
    fn the_system_descriptions_are_well_formed_whatever_the_environment_says() {
        // The value depends on the machine's locale; the shape does not.
        let sink = sink_description();
        let source = source_description();
        assert!(sink.starts_with("FxSound (") && sink.ends_with(')'), "{sink}");
        assert!(source.starts_with("FxSound (") && source.ends_with(')'), "{source}");
        assert_ne!(sink, source);
    }
}

#[cfg(test)]
mod language_coverage {
    use super::*;

    /// Every language the UI can be shown in names its virtual nodes in that language, so the
    /// sound settings and the FxSound window agree.
    #[test]
    fn every_ui_language_has_its_own_direction_words() {
        for language in fxsound_core::i18n::LANGUAGES.iter().skip(1) {
            let output = direction_word(DeviceDirection::Output, Some(language.code));
            let input = direction_word(DeviceDirection::Input, Some(language.code));
            assert_ne!(output, "Output", "{} has no word for Output", language.code);
            assert_ne!(input, "Input", "{} has no word for Input", language.code);
            assert_ne!(output, input, "{}", language.code);
        }
        // Region-qualified and Windows-specific codes resolve too.
        assert_eq!(direction_word(DeviceDirection::Output, Some("zh-TW")), "输出");
        assert_eq!(direction_word(DeviceDirection::Input, Some("pt-br")), "Entrada");
        assert_eq!(direction_word(DeviceDirection::Output, Some("ua")), "Вивід");
    }
}
