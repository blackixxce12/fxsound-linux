//! Every string the interface hands to `tr` has a translation in every language.
//!
//! `tr` falls back to its key, so a string that was never translated looks like English and
//! reads as deliberate. That is how the 0.3.0 microphone readouts — `Gate`, `Compressor`,
//! `De-esser`, `Denoise` and their two status words — shipped in English to twenty-eight
//! languages without anyone noticing. This audit is what notices: it reads the source of the
//! two crates that draw text, gathers what they pass to `tr`, and checks each string against
//! every language's table built the way the app builds it at run time, the Windows original
//! with the port's additions layered over it.
//!
//! A source scan rather than a hand-kept list, because a hand-kept list is the thing that was
//! missing. It follows a literal, a `const NAME: &str` and a `const NAME: [&str; N]` table; a
//! value that arrives through a variable it cannot follow, and those are named in
//! [`INDIRECT_KEYS`] instead.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use fxsound_core::i18n::{Catalogue, LANGUAGES};
use fxsound_core::{
    DeEsserMode, DenoiseChannelMode, DenoiseChannelsOverride, DenoiseLevel, DereverbLevel,
    DeviceDirection, Effect, NoiseSuppressionOverride,
};

/// The crates that draw text, relative to this one. Everything else passes strings *to* them.
const DRAWING_CRATES: [&str; 2] = ["../fxsound-ui/src", "../fxsound-app/src"];

/// Strings that reach `tr` through a variable rather than a literal, which the source scan
/// cannot follow. When a new one is written, it goes here, or the audit does not cover it.
const INDIRECT_KEYS: &[&str] = &[
    // `Stage::meter(name, …)` in `views/pro.rs`: the readout strip's stage names.
    "Gate",
    "Compressor",
    "De-esser",
    "Denoise",
    // `tr(if on { "on" } else { "off" })` in `tray.rs` and `notify.rs`.
    "on",
    "off",
    // `SettingsTab::nav_label` and `pane_title` in `dialogs/settings.rs`.
    "Audio",
    "General",
    "Help",
    "General Preferences",
    // `HotkeyCommand::label` in `dialogs/settings.rs`.
    "Turn FxSound On/Off",
    "Open/Close FxSound",
    "Use Next Preset",
    "Use Previous Preset",
    "Change Playback Device",
    // The preset name editor's hint in `main.rs`.
    "Enter your preset name",
    "Enter new preset name",
    // The tray tooltip's device line in `tray.rs`.
    "Output: ",
    "Input: ",
];

// ---------------------------------------------------------------------------------------------
// Reading the source
// ---------------------------------------------------------------------------------------------

/// Decode the body of a Rust string literal, `src` starting just after its opening quote.
///
/// Returns the text and how many bytes the body and its closing quote took. Handles the escapes
/// the interface actually uses — `\"`, `\\`, `\n`, `\t` — and a backslash at the end of a line,
/// which continues the literal on the next line without its leading whitespace. `None` for a
/// literal that never closes.
fn string_literal(src: &str) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut chars = src.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match c {
            '"' => return Some((out, at + 1)),
            '\\' => match chars.next()?.1 {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                '0' => out.push('\0'),
                'u' => {
                    if chars.next()?.1 != '{' {
                        return None;
                    }
                    let mut hex = String::new();
                    loop {
                        let (_, h) = chars.next()?;
                        if h == '}' {
                            break;
                        }
                        hex.push(h);
                    }
                    out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                }
                '\n' | '\r' => {
                    while chars.peek().is_some_and(|(_, w)| w.is_whitespace()) {
                        chars.next();
                    }
                }
                other => out.push(other),
            },
            other => out.push(other),
        }
    }
    None
}

/// An identifier spelled the way a constant is: `SCREAMING_SNAKE_CASE`, digits allowed.
fn constant_name(src: &str) -> &str {
    let end = src
        .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
        .unwrap_or(src.len());
    &src[..end]
}

/// The string constants one file declares, by name: `const NAME: &str = "…"` yields one string,
/// `const NAME: [&str; N] = ["…", …]` every string in the table.
///
/// The only string table the drawing crates declare is the equalizer's band tooltips, and it
/// exists to be translated; so a table's strings are taken as keys whether or not a `tr` call
/// naming the table is found, because the call passes an element, not the name.
fn constants(src: &str) -> BTreeMap<String, Vec<String>> {
    let mut found = BTreeMap::new();
    let mut rest = src;
    while let Some(at) = rest.find("const ") {
        rest = &rest[at + "const ".len()..];
        let name = constant_name(rest);
        if name.is_empty() {
            continue;
        }
        let Some((ty, value)) = rest[name.len()..].split_once('=') else {
            break;
        };
        let ty = ty.trim().trim_start_matches(':').trim();
        let value = value.trim_start();
        let mut items = Vec::new();
        if ty == "&str" || ty == "&'static str" {
            if let Some((text, _)) = value.strip_prefix('"').and_then(string_literal) {
                items.push(text);
            }
        } else if ty.starts_with("[&str;") || ty == "&[&str]" {
            let mut body = value.strip_prefix('[').unwrap_or("");
            loop {
                body = body.trim_start();
                if let Some(after) = body.strip_prefix(',') {
                    body = after;
                } else if let Some(after) = body.strip_prefix("//") {
                    body = after.split_once('\n').map_or("", |(_, next)| next);
                } else if let Some(after) = body.strip_prefix('"') {
                    let Some((text, used)) = string_literal(after) else {
                        break;
                    };
                    items.push(text);
                    body = &after[used..];
                } else {
                    // `]`, or something that is not a table of strings.
                    break;
                }
            }
        }
        if !items.is_empty() {
            found.insert(name.to_owned(), items);
        }
    }
    found
}

/// What one file passes to `tr` and `tr_args`: literals as they are, constants by name.
fn keys_passed_to_tr(src: &str, consts: &BTreeMap<String, Vec<String>>) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for call in ["tr(", "tr_args("] {
        let mut from = 0;
        while let Some(at) = src[from..].find(call) {
            let start = from + at;
            from = start + call.len();
            // A word boundary before `tr`, so `str(` and `attr(` are not calls to it.
            if src[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.')
            {
                continue;
            }
            let arg = src[from..].trim_start();
            let arg = arg.strip_prefix('&').unwrap_or(arg).trim_start();
            if let Some(body) = arg.strip_prefix('"') {
                if let Some((text, _)) = string_literal(body) {
                    keys.insert(text);
                }
            } else if let Some(items) = consts.get(constant_name(arg)) {
                keys.extend(items.iter().cloned());
            }
            // Anything else is a variable: covered by `INDIRECT_KEYS`, or not at all.
        }
    }
    keys
}

/// Every `.rs` file under `dir`, recursively.
fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|err| panic!("{}: {err}", dir.display()));
    for entry in entries {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Every string the drawing crates pass to `tr`, plus [`INDIRECT_KEYS`].
fn interface_keys() -> BTreeSet<String> {
    let mut keys: BTreeSet<String> = INDIRECT_KEYS.iter().map(|k| (*k).to_owned()).collect();
    let mut tables_seen = 0;
    for crate_dir in DRAWING_CRATES {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(crate_dir);
        assert!(
            dir.is_dir(),
            "{}: the audit reads the drawing crates' source and runs from the workspace",
            dir.display()
        );
        let mut files = Vec::new();
        rust_sources(&dir, &mut files);
        for file in files {
            let src = std::fs::read_to_string(&file)
                .unwrap_or_else(|err| panic!("{}: {err}", file.display()));
            let consts = constants(&src);
            for (_, items) in consts.iter().filter(|(_, items)| items.len() > 1) {
                tables_seen += 1;
                keys.extend(items.iter().cloned());
            }
            keys.extend(keys_passed_to_tr(&src, &consts));
        }
    }
    assert_eq!(
        tables_seen, 1,
        "the drawing crates declare one string table, the band tooltips; a second one needs a \
         look before its strings are taken as translation keys"
    );
    keys
}

// ---------------------------------------------------------------------------------------------
// Checking the tables
// ---------------------------------------------------------------------------------------------

/// Every language's table as the app builds it at run time.
fn tables() -> Vec<Catalogue> {
    LANGUAGES[1..].iter().map(Catalogue::for_language).collect()
}

/// The `language: key` pairs among `keys` that a table lacks, translates as nothing, or
/// translates without the key's `%s` placeholders — the last being the one a translator can
/// break without noticing, and the one `tr_args` cannot recover from.
fn untranslated<'a>(tables: &[Catalogue], keys: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut problems = Vec::new();
    for key in keys {
        for table in tables {
            let code = table.code();
            match table.get(key) {
                None => problems.push(format!("{code}: {key:?} has no translation")),
                Some(value) if value.trim().is_empty() => {
                    problems.push(format!("{code}: {key:?} is translated as nothing"));
                }
                Some(value) if value.matches("%s").count() != key.matches("%s").count() => {
                    problems.push(format!("{code}: {key:?} loses a placeholder in {value:?}"));
                }
                Some(_) => {}
            }
        }
    }
    problems
}

fn assert_all_translated(what: &str, problems: &[String]) {
    assert!(
        problems.is_empty(),
        "{} {what} untranslated:\n  {}",
        problems.len(),
        problems.join("\n  ")
    );
}

// ---------------------------------------------------------------------------------------------
// The audit
// ---------------------------------------------------------------------------------------------

#[test]
fn every_string_the_interface_passes_to_tr_is_translated_in_every_language() {
    let keys = interface_keys();
    let problems = untranslated(&tables(), keys.iter().map(String::as_str));
    assert_all_translated("interface strings", &problems);
}

#[test]
fn the_scan_sees_literals_constants_tables_and_continued_lines() {
    // An audit that had gone blind would pass, which is the one way it must not fail. Each of
    // these reaches `tr` by a different route through the source.
    let keys = interface_keys();
    assert!(keys.len() >= 80, "only {} keys found", keys.len());
    for (route, key) in [
        ("a literal", "Settings"),
        (
            "a literal continued over a line break",
            "FxSound is still running, but this session has no tray icon.\nRun 'fxsound --show' \
             to bring the window back.",
        ),
        ("a constant", "Not used on a microphone"),
        (
            "a constant declared on the line after its name",
            "Keyboard shortcuts",
        ),
        (
            "a table entry with escaped quotes",
            "Super-low Bass. Increase this for more rumble and \"thump\", decrease if there's \
             too much boominess.",
        ),
        (
            "a constant with line breaks in it",
            "This wheel allows you to adjust which frequencies this EQ band is affecting\nup or \
             down to target different frequencies/pitches. The EQ slider above\ncontrols the \
             volume of this EQ band. Increase or decrease to boost or cut\na portion of your \
             audio's frequencies, without modifying the rest of your sound.",
        ),
        ("a name handed to a meter", "unavailable at this rate"),
    ] {
        assert!(keys.contains(key), "{route}: {key:?} was not gathered");
    }
    // And a string that is only mentioned, never passed to `tr`, is not mistaken for a key.
    assert!(!keys.contains("instance.sock"));
    assert!(!keys.contains("Gilroy-Regular"));
}

#[test]
fn every_label_a_core_enum_hands_to_tr_is_translated_in_every_language() {
    // These never appear as a literal in the drawing crates — the interface writes
    // `tr(level.label())` — so the scan above cannot see them and they are enumerated here.
    let mut keys: BTreeSet<&str> = BTreeSet::new();
    keys.extend(DenoiseLevel::ALL.iter().map(|v| v.label()));
    keys.extend(DenoiseChannelMode::ALL.iter().map(|v| v.label()));
    keys.extend(DeEsserMode::ALL.iter().map(|v| v.label()));
    keys.extend(DereverbLevel::ALL.iter().map(|v| v.label()));
    keys.extend(NoiseSuppressionOverride::ALL.iter().map(|v| v.label()));
    keys.extend(DenoiseChannelsOverride::ALL.iter().map(|v| v.label()));
    keys.extend(DeviceDirection::ALL.iter().map(|v| v.label()));
    keys.extend(Effect::ALL.iter().map(|e| e.label()));
    keys.extend(Effect::ALL.iter().map(|e| e.tooltip()));
    // The four levels, three channel modes, two de-esser modes, `Preset`, two directions, and
    // the five effects twice: the level and override enums share their words, deliberately.
    assert!(keys.len() >= 22, "only {} labels found", keys.len());
    let problems = untranslated(&tables(), keys.iter().copied());
    assert_all_translated("labels", &problems);
}

#[test]
fn the_level_labels_do_not_borrow_the_theme_switchs_word() {
    // `"Light"` names the light theme in every Windows table, and a key has one translation per
    // language: had the level kept the same key, German would have called a gentle noise floor
    // "Hell" and Russian "Светлая". The level's word is its own, so the two translate apart.
    let de = Catalogue::for_language(&LANGUAGES[5]);
    assert_eq!(de.code(), "de");
    let theme = de.get("Light").expect("the theme's word");
    for label in [
        DenoiseLevel::Light.label(),
        DereverbLevel::Light.label(),
        NoiseSuppressionOverride::Light.label(),
    ] {
        let level = de.get(label).expect("the level's word");
        assert_ne!(level, theme, "{label:?} shares the theme's translation");
    }
    // The socket and D-Bus spelling is untouched by the label.
    assert_eq!(DenoiseLevel::Light.key(), "light");
    assert_eq!(DenoiseLevel::from_key("light"), Some(DenoiseLevel::Light));
}

#[test]
fn a_placeholder_dropped_by_a_translation_is_reported() {
    // The check `untranslated` makes, on a table built by hand, so that a real table passing it
    // means something.
    let table = Catalogue::parse(
        "xx",
        "\"Could not load %s\" = \"Konnte nicht laden\"\n\"Fine %s\" = \"Gut %s\"\n\"Empty\" = \"\"\n",
    );
    let problems = untranslated(
        &[table],
        ["Could not load %s", "Fine %s", "Empty", "Absent"],
    );
    assert_eq!(problems.len(), 3, "{problems:?}");
    assert!(problems[0].contains("loses a placeholder"), "{problems:?}");
    assert!(
        problems[1].contains("translated as nothing"),
        "{problems:?}"
    );
    assert!(problems[2].contains("has no translation"), "{problems:?}");
}

#[test]
fn the_literal_decoder_reads_what_rustc_reads() {
    let (text, used) = string_literal("a\\\"b\\\\c\\n\" rest").expect("closes");
    assert_eq!(text, "a\"b\\c\n");
    assert_eq!(&"a\\\"b\\\\c\\n\" rest"[used..], " rest");
    // A backslash at the end of a line swallows the break and the next line's indentation.
    let (text, _) = string_literal("one \\\n         two\"").expect("closes");
    assert_eq!(text, "one two");
    assert_eq!(string_literal("never closes"), None);
    assert_eq!(constant_name("HOTKEY_TITLE)"), "HOTKEY_TITLE");
    assert_eq!(constant_name("Self::X"), "S");
    assert_eq!(constant_name("name)"), "");
}
