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
//! missing. It reads a literal, an `if` or a `match` whose every branch is a literal, and a
//! `const NAME: &str` or `const NAME: [&str; N]` declared anywhere in the two crates. What it
//! cannot read — a variable, a method call, a branch that is either — it does not pass over in
//! silence, which is how the 0.3.0 strings slipped by (they reached `tr` as a meter's `name`):
//! every such argument is listed in [`INDIRECT_CALLS`] with the strings it can carry, and a new
//! one fails `every_argument_the_scan_cannot_read_is_listed_with_its_strings` until someone
//! lists it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use fxsound_core::i18n::{Catalogue, LANGUAGES};
use fxsound_core::{
    DeEsserMode, DenoiseChannelMode, DenoiseChannelsOverride, DenoiseLevel, DereverbLevel,
    DeviceDirection, Effect, NoiseSuppressionOverride, WindowsParity,
};

/// The crates that draw text, relative to this one. Everything else passes strings *to* them.
const DRAWING_CRATES: [&str; 2] = ["../fxsound-ui/src", "../fxsound-app/src"];

/// Crates that draw nothing but translate text the window shows: the audio engine, whose warnings
/// about one Bluetooth headset on both lanes (`ONE_HEADSET_ON_BOTH_LANES`) and about an
/// application left on its lane's preset (`TOO_MANY_APPLICATION_PRESETS`) are looked up on the
/// audio thread and arrive translated. Their calls to `tr` and their string constants are read as
/// the drawing crates' are; their string tables are node names, not keys, and are left alone.
const TRANSLATING_CRATES: [&str; 1] = ["../fxsound-audio/src"];

/// Every argument the drawing crates pass to `tr` that the scan cannot read, as written (spaces
/// collapsed), with the strings it can carry.
///
/// An empty list means the strings are audited by another route: a core enum's labels by
/// `every_label_a_core_enum_hands_to_tr_is_translated_in_every_language`, and `tip` — one of
/// the equalizer's band tooltips — through the string table the scan gathers whole.
const INDIRECT_CALLS: &[(&str, &[&str])] = &[
    // `SettingsTab::nav_label` and `pane_title` in `dialogs/settings.rs`.
    (
        "tab.nav_label()",
        &[
            "Audio",
            "General",
            "Help",
            "Microphone",
            "Applications",
            "Experimental",
        ],
    ),
    (
        "self.state.tab.pane_title()",
        &[
            "Audio",
            "General Preferences",
            "Help",
            "Microphone",
            "Applications",
            "Experimental",
        ],
    ),
    // `HotkeyCommand::label` in `dialogs/settings.rs`.
    (
        "command.label()",
        &[
            "Turn FxSound On/Off",
            "Open/Close FxSound",
            "Use Next Preset",
            "Use Previous Preset",
            "Change Playback Device",
        ],
    ),
    // The tray tooltip's device line in `tray.rs`.
    ("key", &["Output: ", "Input: "]),
    // `widgets/equalizer.rs`: an element of `BAND_TOOLTIPS`.
    ("tip", &[]),
    // Core enums.
    ("direction.label()", &[]),
    ("noise.label()", &[]),
    ("channels.label()", &[]),
    ("deesser.label()", &[]),
    ("dereverb.label()", &[]),
    // The calibration wizard's recommended denoiser level (`fxsound-app/src/calibration.rs`).
    ("self.denoise.label()", &[]),
    ("effect.label()", &[]),
    ("effect.tooltip()", &[]),
    // The Experimental pane's positions and hint (`WindowsParity::label` and `hint`).
    ("shown.hint()", &[]),
    ("shown.label()", &[]),
    ("level.label()", &[]),
    // The line under "Smooth moves in WirePlumber" (`WirePlumberHook::line` in
    // `dialogs/settings.rs`).
    (
        "hook.line()",
        &[
            "Fades the sound WirePlumber moves, as when the desktop picks another device. Changes \
             WirePlumber's settings.",
            "Needs WirePlumber 0.5 or newer.",
            "WirePlumber takes the change when it restarts.",
            "WirePlumber could not be restarted here. It takes the change at your next login.",
        ],
    ),
    // The import window's notice in `dialogs/presets.rs`, which the app files under its English
    // key (`fxsound-app/src/app.rs`, `handle_import`).
    (
        "notice",
        &["Preset files not found in the selected folder."],
    ),
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

/// How many bytes the character literal `src` starts with takes — `'"'`, `'\''`, `'\u{2026}'` —
/// or `None` when the quote begins a lifetime instead.
fn char_literal(src: &str) -> Option<usize> {
    let rest = src.strip_prefix('\'')?;
    let mut chars = rest.chars();
    let body = match chars.next()? {
        '\\' => match chars.next()? {
            'u' => rest.find('}')? + 1,
            escaped => 1 + escaped.len_utf8(),
        },
        c => c.len_utf8(),
    };
    rest.get(body..)?.starts_with('\'').then_some(1 + body + 1)
}

/// Where in `src` the first place that `stop` accepts is, looking only at places outside a
/// string literal, a character literal and a `//` comment, and not inside a bracket opened in
/// `src`. `None` when there is none, when a bracket closes that `src` did not open, or when a
/// literal never closes. Raw strings are not read; the interface passes none to `tr`.
fn top_level(src: &str, stop: impl Fn(&str) -> bool) -> Option<usize> {
    let mut depth = 0_usize;
    let mut at = 0;
    while let Some(c) = src[at..].chars().next() {
        let rest = &src[at..];
        if depth == 0 && stop(rest) {
            return Some(at);
        }
        match c {
            '"' => {
                let (_, used) = string_literal(&rest[1..])?;
                at += 1 + used;
                continue;
            }
            '\'' => {
                if let Some(used) = char_literal(rest) {
                    at += used;
                    continue;
                }
            }
            '/' if rest.starts_with("//") => {
                at += rest.find('\n').unwrap_or(rest.len());
                continue;
            }
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.checked_sub(1)?,
            _ => {}
        }
        at += c.len_utf8();
    }
    None
}

/// The first argument of a call, `src` starting just after its opening parenthesis: everything up
/// to the comma or the closing parenthesis that is not inside a bracket, a literal or a comment.
/// `None` for a call that never closes.
fn first_argument(src: &str) -> Option<&str> {
    top_level(src, |rest| rest.starts_with([',', ')', ']', '}'])).map(|end| &src[..end])
}

/// `src` without the whitespace and `//` comments it starts with.
fn skip_space(mut src: &str) -> &str {
    loop {
        src = src.trim_start();
        match src.strip_prefix("//") {
            Some(comment) => src = comment.split_once('\n').map_or("", |(_, next)| next),
            None => return src,
        }
    }
}

/// `src` after the keyword `word` it starts with; `None` when it does not start with it, which
/// `iffy` does not with `if`.
fn keyword<'a>(src: &'a str, word: &str) -> Option<&'a str> {
    let rest = src.strip_prefix(word)?;
    (!rest.starts_with(|c: char| c.is_alphanumeric() || c == '_')).then_some(rest)
}

/// The inside of the `{ … }` block `src` starts with, and what follows its closing brace.
fn block(src: &str) -> Option<(&str, &str)> {
    let inner = skip_space(src).strip_prefix('{')?;
    let close = top_level(inner, |rest| rest.starts_with([')', ']', '}']))?;
    Some((&inner[..close], &inner[close + 1..]))
}

/// The keys an expression made of nothing but string literals can be: a literal, a block around
/// one, or an `if` or a `match` whose every branch is one of these, in the order written.
///
/// `None` for anything else, and for an `if` or a `match` with a single branch the scan cannot
/// read — `if c { "On" } else { name }` — because taking its literals as all it can be is exactly
/// the silence the audit exists to break: `name` is a string no test would look at.
fn expression_keys(expr: &str) -> Option<Vec<String>> {
    let expr = skip_space(expr).trim_end();
    if let Some(body) = expr.strip_prefix('"') {
        let (text, used) = string_literal(body)?;
        return skip_space(&body[used..]).is_empty().then(|| vec![text]);
    }
    if expr.starts_with('{') {
        let (inner, after) = block(expr)?;
        return if skip_space(after).is_empty() {
            expression_keys(inner)
        } else {
            None
        };
    }
    if let Some(rest) = keyword(expr, "if") {
        return if_keys(rest);
    }
    if let Some(rest) = keyword(expr, "match") {
        return match_keys(rest);
    }
    None
}

/// The keys of `if … { a } else if … { b } else { c }`, `src` starting after the first `if`.
///
/// A condition that holds a brace outside brackets — `if let Mode { level } = mode {` — is taken
/// as ending at it; the branch read is then the pattern's, which is not a literal, so the argument
/// is named rather than misread.
fn if_keys(src: &str) -> Option<Vec<String>> {
    let mut keys = Vec::new();
    let mut condition = src;
    loop {
        let open = top_level(condition, |rest| rest.starts_with('{'))?;
        let (branch, after) = block(&condition[open..])?;
        keys.extend(expression_keys(branch)?);
        // Without an `else`, an `if` is `()`, not a string.
        let otherwise = keyword(skip_space(after), "else")?;
        match keyword(skip_space(otherwise), "if") {
            Some(next) => condition = next,
            None => {
                let (branch, after) = block(otherwise)?;
                keys.extend(expression_keys(branch)?);
                return skip_space(after).is_empty().then_some(keys);
            }
        }
    }
}

/// The keys of `match … { P => a, Q if g => { b } … }`, `src` starting after `match`.
fn match_keys(src: &str) -> Option<Vec<String>> {
    let open = top_level(src, |rest| rest.starts_with('{'))?;
    let (mut arms, after) = block(&src[open..])?;
    if !skip_space(after).is_empty() {
        return None;
    }
    let mut keys = Vec::new();
    loop {
        arms = skip_space(arms);
        if arms.is_empty() {
            // A `match` with no arms is `!`, not a string.
            return (!keys.is_empty()).then_some(keys);
        }
        let arrow = top_level(arms, |rest| rest.starts_with("=>"))?;
        // A pattern and its guard hold no comma outside brackets: one here is the tail of the
        // previous arm's body, `{ "x" }.to_uppercase(),`, which the scan did not read.
        if top_level(&arms[..arrow], |rest| rest.starts_with([',', ';'])).is_some() {
            return None;
        }
        let body = skip_space(&arms[arrow + "=>".len()..]);
        let end = if body.starts_with('{') {
            let (_, after) = block(body)?;
            body.len() - after.len()
        } else {
            top_level(body, |rest| rest.starts_with(',')).unwrap_or(body.len())
        };
        keys.extend(expression_keys(&body[..end])?);
        arms = skip_space(&body[end..]);
        arms = arms.strip_prefix(',').unwrap_or(arms);
    }
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

/// What the scan made of one argument to `tr`.
#[derive(Debug, PartialEq, Eq)]
enum Argument {
    /// The keys it can be: one for a literal or a string constant, several for a table or for an
    /// `if` or a `match` whose every branch is a literal.
    Keys(Vec<String>),
    /// Something the scan cannot read, as written with its whitespace collapsed.
    Unreadable(String),
}

/// Read one argument: `file` is the constants of the file it is in, `anywhere` those of every
/// file in the drawing crates, for a constant declared in one module and used in another.
fn read_argument(
    arg: &str,
    file: &BTreeMap<String, Vec<String>>,
    anywhere: &BTreeMap<String, Vec<String>>,
) -> Argument {
    let arg = arg.trim();
    let arg = arg.strip_prefix('&').unwrap_or(arg).trim_start();
    if let Some(keys) = expression_keys(arg) {
        return Argument::Keys(keys);
    }
    if !arg.is_empty()
        && constant_name(arg) == arg
        && let Some(items) = file.get(arg).or_else(|| anywhere.get(arg))
    {
        return Argument::Keys(items.clone());
    }
    Argument::Unreadable(arg.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Every call to `tr` or `tr_args` in one file, its argument read. A `tr` that is part of a
/// longer name (`str(`, `attr(`, `.tr(`), a definition (`fn tr(`) and a call in a `//` comment
/// are not calls to it.
fn calls_to_tr(
    src: &str,
    file: &BTreeMap<String, Vec<String>>,
    anywhere: &BTreeMap<String, Vec<String>>,
) -> Vec<Argument> {
    let mut found = Vec::new();
    for call in ["tr(", "tr_args(", "tr_breakable("] {
        let mut from = 0;
        while let Some(at) = src[from..].find(call) {
            let start = from + at;
            from = start + call.len();
            let before = &src[..start];
            if before
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.')
                || before.trim_end().ends_with("fn")
            {
                continue;
            }
            let line = &before[before.rfind('\n').map_or(0, |i| i + 1)..];
            if line.trim_start().starts_with("//") {
                continue;
            }
            let Some(arg) = first_argument(&src[from..]) else {
                continue;
            };
            found.push(read_argument(arg, file, anywhere));
        }
    }
    found
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

/// The drawing crates' source, file by file.
fn drawing_sources() -> Vec<(PathBuf, String)> {
    sources_of(&DRAWING_CRATES)
}

/// The source of `crates`, relative to this one, file by file.
fn sources_of(crates: &[&str]) -> Vec<(PathBuf, String)> {
    let mut sources = Vec::new();
    for crate_dir in crates {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(crate_dir);
        assert!(
            dir.is_dir(),
            "{}: the audit reads the drawing crates' source and runs from the workspace",
            dir.display()
        );
        let mut files = Vec::new();
        rust_sources(&dir, &mut files);
        files.sort();
        for file in files {
            let src = std::fs::read_to_string(&file)
                .unwrap_or_else(|err| panic!("{}: {err}", file.display()));
            sources.push((file, src));
        }
    }
    sources
}

/// What the drawing crates pass to `tr`.
struct Scan {
    /// The keys read off the source, the band tooltip table's included.
    keys: BTreeSet<String>,
    /// The arguments that could not be read, each with the files it is written in.
    unreadable: BTreeMap<String, BTreeSet<String>>,
}

fn scan() -> Scan {
    let drawing = drawing_sources();
    let drawing_files = drawing.len();
    let mut sources = drawing;
    sources.extend(sources_of(&TRANSLATING_CRATES));
    let per_file: Vec<_> = sources
        .iter()
        .enumerate()
        .map(|(index, (_, src))| {
            let mut consts = constants(src);
            if index >= drawing_files {
                // A translating crate's tables are not interface text (see TRANSLATING_CRATES).
                consts.retain(|_, items| items.len() == 1);
            }
            consts
        })
        .collect();
    let mut anywhere = BTreeMap::new();
    for consts in &per_file {
        for (name, items) in consts {
            anywhere
                .entry(name.clone())
                .or_insert_with(Vec::new)
                .extend(items.iter().cloned());
        }
    }

    let mut keys = BTreeSet::new();
    let mut unreadable: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut tables_seen = 0;
    for ((path, src), consts) in sources.iter().zip(&per_file) {
        for items in consts.values().filter(|items| items.len() > 1) {
            tables_seen += 1;
            keys.extend(items.iter().cloned());
        }
        for argument in calls_to_tr(src, consts, &anywhere) {
            match argument {
                Argument::Keys(found) => keys.extend(found),
                Argument::Unreadable(arg) => {
                    let file = path
                        .components()
                        .rev()
                        .take(3)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<PathBuf>();
                    unreadable
                        .entry(arg)
                        .or_default()
                        .insert(file.display().to_string());
                }
            }
        }
    }
    assert_eq!(
        tables_seen, 1,
        "the drawing crates declare one string table, the band tooltips; a second one needs a \
         look before its strings are taken as translation keys"
    );
    Scan { keys, unreadable }
}

/// Every label a core enum hands to `tr`. These never appear as a literal in the drawing crates
/// — the interface writes `tr(level.label())` — so the scan cannot see them.
fn core_labels() -> BTreeSet<&'static str> {
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
    keys.extend(WindowsParity::ALL.iter().map(|level| level.label()));
    keys.extend(WindowsParity::ALL.iter().map(|level| level.hint()));
    keys
}

/// Every string the drawing crates pass to `tr`: what the scan reads, what [`INDIRECT_CALLS`]
/// says the unreadable arguments carry, and the core enums' labels.
fn interface_keys() -> BTreeSet<String> {
    let mut keys = scan().keys;
    keys.extend(
        INDIRECT_CALLS
            .iter()
            .flat_map(|(_, strings)| strings.iter().map(|s| (*s).to_owned())),
    );
    keys.extend(core_labels().into_iter().map(str::to_owned));
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
fn the_audio_engines_warning_about_one_headset_on_both_lanes_is_among_the_keys_audited() {
    // Translated on the audio thread (`fxsound-audio`, `ONE_HEADSET_ON_BOTH_LANES`) and shown by
    // the window as it arrives: a crate that draws nothing, which the scan reads for its calls to
    // `tr` all the same, or the warning would reach twenty-eight languages in English unnoticed.
    let keys = scan().keys;
    assert!(
        keys.iter()
            .any(|key| key
                .starts_with("Using this headset's microphone switches it to call quality")),
        "the scan does not see the audio crate's call to tr"
    );
    assert!(
        !keys.contains("fxsound_sink"),
        "a node name from the audio crate's tables is not a key"
    );
}

#[test]
fn the_audio_engines_warning_about_too_many_application_presets_is_among_the_keys_audited() {
    // Formatted on the audio thread (`fxsound-audio`, `TOO_MANY_APPLICATION_PRESETS`, through
    // `tr_args`) when an application's preset cannot get a route of its own, and shown by the
    // window as it arrives: its key has two placeholders — the application, then the most
    // presets a lane runs for applications — which every translation has to keep.
    let keys = scan().keys;
    let key = "%s stays on FxSound's preset: at most %s application presets can run at once";
    assert!(
        keys.contains(key),
        "the scan does not see the audio crate's call to tr_args"
    );
    assert_all_translated(
        "application preset warnings",
        &untranslated(&tables(), [key]),
    );
}

#[test]
fn every_argument_the_scan_cannot_read_is_listed_with_its_strings() {
    // A `tr(name)` whose `name` the scan cannot follow is a string no test looks at, which is the
    // hole the 0.3.0 microphone strings fell through. So the list is exact both ways: a new one
    // fails here until it is listed with what it carries, and a listed one that is gone goes.
    let scan = scan();
    let listed: BTreeSet<&str> = INDIRECT_CALLS.iter().map(|(arg, _)| *arg).collect();
    let unlisted: Vec<String> = scan
        .unreadable
        .iter()
        .filter(|(arg, _)| !listed.contains(arg.as_str()))
        .map(|(arg, files)| format!("tr({arg}) in {files:?}"))
        .collect();
    assert!(
        unlisted.is_empty(),
        "the scan cannot read these; add each to INDIRECT_CALLS with the strings it can carry, \
         or pass `tr` a literal:\n  {}",
        unlisted.join("\n  ")
    );
    let gone: Vec<&&str> = listed
        .iter()
        .filter(|arg| !scan.unreadable.contains_key(**arg))
        .collect();
    assert!(
        gone.is_empty(),
        "listed in INDIRECT_CALLS, no longer written: {gone:?}"
    );
}

#[test]
fn every_string_an_indirect_call_is_listed_with_is_still_written_in_the_interface() {
    // The other half of keeping the list honest: a label renamed at its source and not here
    // would leave the audit checking the old word.
    let sources: String = drawing_sources().into_iter().map(|(_, src)| src).collect();
    for (arg, strings) in INDIRECT_CALLS {
        for string in *strings {
            assert!(
                sources.contains(&format!("{string:?}")),
                "tr({arg}): {string:?} is not written anywhere in the drawing crates"
            );
        }
    }
}

#[test]
fn the_scan_sees_literals_constants_tables_branches_and_continued_lines() {
    // An audit that had gone blind would pass, which is the one way it must not fail. Each of
    // these reaches `tr` by a different route through the source.
    let keys = scan().keys;
    assert!(keys.len() >= 120, "only {} keys found", keys.len());
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
        ("one branch of an `if`", "on"),
        ("the other branch of an `if`", "off"),
        ("an arm of a `match`", "Enter new preset name"),
        (
            "the key of a `tr_args` on a line of its own",
            "FxSound is %s.",
        ),
        ("a stage of the readout strip", "Compressor"),
        (
            "a line of the calibration dialog",
            "Speak loudly for 2 seconds",
        ),
    ] {
        assert!(keys.contains(key), "{route}: {key:?} was not gathered");
    }
    // And a string that is only mentioned, never passed to `tr`, is not mistaken for a key.
    assert!(!keys.contains("instance.sock"));
    assert!(!keys.contains("Gilroy-Regular"));
}

#[test]
fn every_label_a_core_enum_hands_to_tr_is_translated_in_every_language() {
    let keys = core_labels();
    // The four levels, three channel modes, two de-esser modes, `Preset`, two directions, and
    // the five effects twice: the level and override enums share their words, deliberately.
    assert!(keys.len() >= 22, "only {} labels found", keys.len());
    let problems = untranslated(&tables(), keys.iter().copied());
    assert_all_translated("labels", &problems);
}

#[test]
fn every_string_a_port_table_carries_is_one_the_interface_asks_for() {
    // A translation nothing asks for is one that twenty-nine files keep up for nothing, and
    // usually the sign of a string renamed in the source and not in the tables: 0.3.0's
    // lower-case `voice` outlived the readout that showed it.
    let asked = interface_keys();
    let mut stale = BTreeSet::new();
    for language in &LANGUAGES[1..] {
        for key in language.port_catalogue().keys() {
            if !asked.contains(key) {
                stale.insert(format!("{}: {key:?}", language.code));
            }
        }
    }
    assert!(
        stale.is_empty(),
        "{} port translations of strings the interface no longer passes to tr:\n  {}",
        stale.len(),
        stale.into_iter().collect::<Vec<_>>().join("\n  ")
    );
}

#[test]
fn every_port_table_adds_the_same_strings() {
    // The strings the Windows build never had are added to all twenty-nine tables at once; only
    // a repair of a Windows string is particular to the table it repairs.
    let windows: BTreeSet<String> = LANGUAGES[1..]
        .iter()
        .flat_map(|language| {
            language
                .original_catalogue()
                .keys()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect();
    let ports: Vec<(&str, Catalogue)> = LANGUAGES[1..]
        .iter()
        .map(|language| (language.code, language.port_catalogue()))
        .collect();
    let added: BTreeSet<&str> = ports
        .iter()
        .flat_map(|(_, port)| port.keys())
        .filter(|key| !windows.contains(*key))
        .collect();
    assert!(added.len() >= 60, "only {} added strings", added.len());
    let mut missing = Vec::new();
    for key in &added {
        for (code, port) in &ports {
            if port.get(key).is_none() {
                missing.push(format!("{code}: {key:?}"));
            }
        }
    }
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}

#[test]
fn a_string_that_ends_in_a_space_keeps_the_space_in_every_language() {
    // `"Output: "` and `"Preset: "` are followed by a name — in the tray tooltip and in the
    // desktop notifications — and eleven Windows tables drop the space, which glued German into
    // "Ausgabe:Lautsprecher". A CJK full-width colon carries its own spacing.
    let keys: Vec<String> = interface_keys()
        .into_iter()
        .filter(|key| key.ends_with(' '))
        .collect();
    assert!(
        keys.iter().any(|key| key == "Output: ") && keys.iter().any(|key| key == "Preset: "),
        "{keys:?}"
    );
    let mut glued = Vec::new();
    for table in tables() {
        for key in &keys {
            if let Some(value) = table.get(key)
                && !(value.ends_with(char::is_whitespace) || value.ends_with('：'))
            {
                glued.push(format!("{}: {key:?} = {value:?}", table.code()));
            }
        }
    }
    assert!(glued.is_empty(), "{}", glued.join("\n"));
}

/// Strings some language writes exactly as English does: audio terms its engineers borrow
/// (`Gate`, `De-esser`, `Mono`), words the two languages share (Dutch `Help`, Spanish `No`,
/// Romanian `General`, the German, French and Spanish `Balance`, the Spanish, Portuguese and
/// Romanian `Experimental`, the French, Dutch and Portuguese `Interface`), and the effect names
/// the Polish original keeps as FxSound's own. Any other string a table translates as itself is a
/// string nobody translated.
const SPELLED_AS_IN_ENGLISH: &[&str] = &[
    "Ambience",
    "Applications",
    "Audio",
    "Balance",
    "Bass Boost",
    "Clarity",
    "Clipping",
    "Compressor",
    "De-esser",
    "Dynamic Boost",
    "Echo",
    "Experimental",
    "Export",
    "Filter Q",
    "FxSound is %s.",
    "Gate",
    "Gate %s",
    "General",
    "Help",
    "Import",
    "Independent",
    "Interface",
    "Menu",
    "Microphone",
    "Mono",
    "No",
    "OK",
    "Preset",
    "Preset: ",
    "Reverb",
    "Start",
    "Surround Sound",
    "Version",
];

#[test]
fn no_table_leaves_a_string_in_english_unless_the_language_spells_it_so() {
    // `untranslated` cannot see this one: a value that *is* the key is present and not empty.
    // Croatian's Windows table did it to "on" and "off", which the readout strip and the tray
    // show, and a port entry copied from its key as a placeholder would do it again.
    let keys = interface_keys();
    let tables = tables();
    let mut english = Vec::new();
    for table in &tables {
        for key in &keys {
            if table.get(key) == Some(key.as_str())
                && !SPELLED_AS_IN_ENGLISH.contains(&key.as_str())
            {
                english.push(format!("{}: {key:?}", table.code()));
            }
        }
    }
    assert!(english.is_empty(), "{}", english.join("\n"));
    // The list is not a way round the check: every word on it is spelled so somewhere.
    for word in SPELLED_AS_IN_ENGLISH {
        assert!(
            tables.iter().any(|table| table.get(word) == Some(*word)),
            "{word:?} is translated in every table; it need not be listed"
        );
    }
}

#[test]
fn the_level_labels_do_not_borrow_the_theme_switchs_word() {
    // `"Light"` names the light theme in every Windows table, and a key has one translation per
    // language: had the level kept the same key, German would have called a gentle noise floor
    // "Hell" and Russian "Светлая". The level's word is its own, so the two translate apart.
    let de = Catalogue::for_language(fxsound_core::i18n::language("de").expect("German"));
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

/// Strings the interface shows side by side, or that name two different things, and so must not
/// read alike in any language. Each group is checked pair by pair.
const TOLD_APART: &[(&str, &[&str])] = &[
    // Settings ▸ General's hotkey list, one line per command.
    (
        "the hotkey list",
        &[
            "Turn FxSound On/Off",
            "Open/Close FxSound",
            "Use Next Preset",
            "Use Previous Preset",
            "Change Playback Device",
        ],
    ),
    // The tray menu (`tray.rs`), top level.
    (
        "the tray menu",
        &[
            "Open",
            "Turn Off",
            "Turn On",
            "Output Presets",
            "Input Presets",
            "Playback Device Select",
            "Recording Device Select",
            "Settings",
            "Theme",
            "Exit",
        ],
    ),
];

#[test]
fn no_table_gives_two_strings_shown_side_by_side_the_same_words() {
    // Simplified and Traditional Chinese wrote 开启/关闭 FxSound for both the power and the window
    // in the hotkey list; the port's layer repairs both, and this keeps any table from doing it
    // again.
    let tables = tables();
    let mut alike = Vec::new();
    for (what, keys) in TOLD_APART {
        for table in &tables {
            for (i, a) in keys.iter().enumerate() {
                for b in &keys[i + 1..] {
                    let (ta, tb) = (table.get(a).unwrap_or(a), table.get(b).unwrap_or(b));
                    if ta.trim() == tb.trim() {
                        alike.push(format!(
                            "{}: {what}: {a:?} and {b:?} both read {ta:?}",
                            table.code()
                        ));
                    }
                }
            }
        }
    }
    assert!(alike.is_empty(), "{}", alike.join("\n"));
}

#[test]
fn a_placeholder_dropped_by_a_translation_is_reported() {
    // The check `untranslated` makes, on a table built by hand, so that a real table passing it
    // means something.
    // An empty translation is no translation, as JUCE reads one (0.4.0 audit #50); one of spaces
    // alone is kept, and is nothing to read.
    let table = Catalogue::parse(
        "xx",
        "\"Could not load %s\" = \"Konnte nicht laden\"\n\"Fine %s\" = \"Gut %s\"\n\
         \"Blank\" = \"  \"\n\"Empty\" = \"\"\n",
    );
    let problems = untranslated(
        &[table],
        ["Could not load %s", "Fine %s", "Blank", "Empty", "Absent"],
    );
    assert_eq!(problems.len(), 4, "{problems:?}");
    assert!(problems[0].contains("loses a placeholder"), "{problems:?}");
    assert!(
        problems[1].contains("translated as nothing"),
        "{problems:?}"
    );
    assert!(problems[2].contains("has no translation"), "{problems:?}");
    assert!(problems[3].contains("has no translation"), "{problems:?}");
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

#[test]
fn an_argument_ends_at_its_own_comma_or_parenthesis_and_not_at_one_inside_it() {
    assert_eq!(first_argument("\"a, (b)\", &[x])"), Some("\"a, (b)\""));
    assert_eq!(
        first_argument("match p { A => \"x)\", B => \"y\" }) + 1"),
        Some("match p { A => \"x)\", B => \"y\" }")
    );
    assert_eq!(
        first_argument("if c { ')' } else { '\\'' })"),
        Some("if c { ')' } else { '\\'' }")
    );
    assert_eq!(first_argument("x.label::<'a>())"), Some("x.label::<'a>()"));
    assert_eq!(first_argument("never closes"), None);
    assert_eq!(char_literal("'\"' rest"), Some(3));
    assert_eq!(char_literal("'\\u{2026}'"), Some(10));
    assert_eq!(char_literal("'static str"), None);
}

#[test]
fn a_literal_a_branch_and_a_constant_are_read_and_anything_else_is_named() {
    let file: BTreeMap<String, Vec<String>> =
        BTreeMap::from([("HERE".to_owned(), vec!["Here".to_owned()])]);
    let anywhere: BTreeMap<String, Vec<String>> = BTreeMap::from([
        ("HERE".to_owned(), vec!["Here".to_owned()]),
        ("THERE".to_owned(), vec!["There".to_owned()]),
    ]);
    let read = |arg: &str| read_argument(arg, &file, &anywhere);
    let keys = |k: &[&str]| Argument::Keys(k.iter().map(|s| (*s).to_owned()).collect());
    assert_eq!(read(" &\"Save\""), keys(&["Save"]));
    assert_eq!(
        read("if on { \"on\" } else { \"off\" }"),
        keys(&["on", "off"])
    );
    assert_eq!(
        read("match purpose {\n    A => \"One\",\n    B => \"Two\",\n}"),
        keys(&["One", "Two"])
    );
    assert_eq!(read("HERE"), keys(&["Here"]));
    assert_eq!(read("THERE"), keys(&["There"]));
    assert_eq!(read("NOWHERE"), Argument::Unreadable("NOWHERE".to_owned()));
    assert_eq!(
        read("self.state\n    .tab.pane_title()"),
        Argument::Unreadable("self.state .tab.pane_title()".to_owned())
    );
    assert_eq!(read("name"), Argument::Unreadable("name".to_owned()));
}

#[test]
fn an_if_or_a_match_with_one_branch_that_is_not_a_literal_is_named_not_half_read() {
    // Taking `"On"` as all `tr(if c { "On" } else { name })` can be would leave `name` a string
    // no test looks at, and say nothing: the 0.3.0 hole with a literal beside it.
    let none = BTreeMap::new();
    let read = |arg: &str| read_argument(arg, &none, &none);
    let unread = |arg: &str| Argument::Unreadable(arg.to_owned());
    assert_eq!(
        read("if c { \"On\" } else { name }"),
        unread("if c { \"On\" } else { name }")
    );
    assert_eq!(
        read("if c { name } else { \"Off\" }"),
        unread("if c { name } else { \"Off\" }")
    );
    assert_eq!(
        read("if a { \"A\" } else if b { label() } else { \"C\" }"),
        unread("if a { \"A\" } else if b { label() } else { \"C\" }")
    );
    assert_eq!(
        read("match p {\n    A => \"One\",\n    B => mode.label(),\n}"),
        unread("match p { A => \"One\", B => mode.label(), }")
    );
    assert_eq!(
        read("match p { A => \"One\", B => { let s = \"Two\"; s } }"),
        unread("match p { A => \"One\", B => { let s = \"Two\"; s } }")
    );
    assert_eq!(
        read("match p { A => if c { \"x\" } else { y }, B => \"z\" }"),
        unread("match p { A => if c { \"x\" } else { y }, B => \"z\" }")
    );
    // A literal with something done to it is not that literal.
    assert_eq!(
        read("if c { \"On\".trim() } else { \"Off\" }"),
        unread("if c { \"On\".trim() } else { \"Off\" }")
    );
    assert_eq!(
        read("match p { A => { \"x\" }.to_uppercase(), B => \"y\" }"),
        unread("match p { A => { \"x\" }.to_uppercase(), B => \"y\" }")
    );
    assert_eq!(
        read("if c { \"On\" } else { \"Off\" }.to_uppercase()"),
        unread("if c { \"On\" } else { \"Off\" }.to_uppercase()")
    );
    // An `if` without an `else`, and a `match` without arms, are not strings at all.
    assert_eq!(read("if c { \"On\" }"), unread("if c { \"On\" }"));
    assert_eq!(read("match p {}"), unread("match p {}"));
    // A brace in an `if let` pattern is not mistaken for the branch.
    assert_eq!(
        read("if let Mode { level } = mode { \"x\" } else { \"y\" }"),
        unread("if let Mode { level } = mode { \"x\" } else { \"y\" }")
    );
}

#[test]
fn branches_that_are_all_literals_are_read_however_they_are_written() {
    let none = BTreeMap::new();
    let read = |arg: &str| read_argument(arg, &none, &none);
    let keys = |k: &[&str]| Argument::Keys(k.iter().map(|s| (*s).to_owned()).collect());
    assert_eq!(
        read("if a { \"A\" } else if b { \"B\" } else { \"C\" }"),
        keys(&["A", "B", "C"])
    );
    assert_eq!(
        read("if let Some(x) = y.map(|v| { v }) {\n    \"Yes\"\n} else {\n    \"No\"\n}"),
        keys(&["Yes", "No"])
    );
    assert_eq!(
        read(
            "match p {\n    // the one \"quoted\" => here is a comment\n    A => { \"x\" }\n    \
             Some(B { .. }) | C if y >= 2 => \"y\",\n    _ => if z { \"z\" } else { \"w\" },\n}"
        ),
        keys(&["x", "y", "z", "w"])
    );
    assert_eq!(
        read("match (a, b) { (1, _) => \"a, b\", _ => \"=> {\" }"),
        keys(&["a, b", "=> {"])
    );
    assert_eq!(read("{ \"Block\" }"), keys(&["Block"]));
    assert_eq!(read("\"Save\" // why\n"), keys(&["Save"]));
}

#[test]
fn a_comment_inside_an_argument_does_not_end_it() {
    assert_eq!(
        first_argument("\n    // not \"here\", or here)\n    \"Key\",\n)"),
        Some("\n    // not \"here\", or here)\n    \"Key\"")
    );
    assert_eq!(first_argument("a / b)"), Some("a / b"));
}

#[test]
fn a_definition_a_comment_and_a_longer_name_are_not_calls() {
    let none = BTreeMap::new();
    let src = "pub fn tr(key: &str) {}\n// tr(\"Commented\")\nlet s = str(\"No\");\nx.tr(\"No\");\n\
               let y = tr(\"Yes\");\nlet z = tr_args(\n    \"Also %s\",\n    &[&tr(v)],\n);\n";
    let calls = calls_to_tr(src, &none, &none);
    assert_eq!(
        calls,
        [
            Argument::Keys(vec!["Yes".to_owned()]),
            Argument::Unreadable("v".to_owned()),
            Argument::Keys(vec!["Also %s".to_owned()]),
        ]
    );
}
