//! Reading and writing FxSound `.fac` preset files.
//!
//! Despite the extension the format is plain text, one `value: description` line at a time, written
//! by `valsSave()` and read by `valsRead()` in `dsp/ptutil/Vals/Valsfile.cpp`. The grammar is
//! documented in `docs/spec/11-preset-format.md`; this module implements it faithfully enough to
//! round-trip every file FxSound ships.
//!
//! Three quirks of the original are reproduced deliberately:
//!
//! * numbers are parsed the way C's `%d` / `%g` parse them — leading blanks are skipped and the
//!   scan stops at the first character that cannot continue the number, so `"   0: Param 0"`
//!   yields `0`;
//! * floats are written with C's `%g`, i.e. six significant digits with trailing zeros stripped;
//! * the equalizer block exists only in files of version 9 or newer.
//!
//! Line endings are accepted as LF or CRLF, with or without a final terminator, because the files
//! in the FxSound tree are a mix of all three.

#![forbid(unsafe_code)]

pub mod input;
pub mod input_store;
mod store;

pub use input_store::InputPresetStore;
pub use store::{PresetEntry, PresetFile, PresetSource, PresetStore, Store};

use fxsound_core::{EqBand, Preset, eq};
use std::fmt::Write as _;

/// Everything that can go wrong reading a preset.
#[derive(Debug, thiserror::Error)]
pub enum PresetError {
    #[error("not a preset file: first line is {found:?}, expected it to start with CLASS1")]
    NotAPreset { found: String },
    #[error("unexpected end of file at line {line}, expected {expected}")]
    UnexpectedEof { line: usize, expected: &'static str },
    #[error("line {line}: expected {expected}, found {found:?}")]
    Malformed {
        line: usize,
        expected: &'static str,
        found: String,
    },
    #[error("line {line}: element index is {found}, expected {expected}")]
    ElementOutOfOrder {
        line: usize,
        found: i64,
        expected: usize,
    },
    #[error("preset has no name")]
    MissingName,
    /// A name the store's list does not hold. Its own variant rather than a `Malformed` at line
    /// zero, because the string reaches the command line and D-Bus, and "line 0: expected a known
    /// preset name" describes a file that was never opened.
    #[error("no preset named {0:?}")]
    Unknown(String),
    /// A name whose file is already another listed preset's. The two names differ only in
    /// characters a filename cannot hold — `Mu:sic` and `Music` are both `Music.fac` — so writing
    /// the second would replace the first's file, or share its autosave when the first is a
    /// factory preset kept under a numbered name, and the list would show one name where two had
    /// been saved. [`crate::Store::save_as`] refuses it instead.
    #[error("{name:?} and {existing:?} would share the file {file}")]
    SharedFile {
        name: String,
        existing: String,
        file: String,
    },
    #[error("{0} equalizer bands, the engine supports at most {max}", max = eq::MAX_BANDS)]
    TooManyBands(usize),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The literal first line of every preset file.
const MAGIC: &str = "CLASS1";
/// `VALS_NUM_MAIN_PARAMS`.
const NUM_MAIN_PARAMS: usize = 6;
/// `VALS_NUM_ELEMENT_PARAMS`.
const NUM_ELEMENT_PARAMS: usize = 7;
/// `DFXG_VALS_NUM_APP_DEPEND_INTS`.
const NUM_APP_INTS: usize = 7;
/// The first version that carries an equalizer block.
const EQ_MIN_VERSION: f32 = 9.0;
/// `DFXG_MAX_PRESET_NAME_LENGTH`: what the file format can hold.
pub const MAX_NAME_LEN: usize = 128;

/// Characters stripped from a preset name before it is used, from
/// `FxController::sanitizePresetName` (`fxsound/Source/GUI/FxController.cpp:378`).
///
/// This is the Windows reserved-filename set and it stays reserved on Linux, because a preset name
/// becomes a `.fac` filename (`FxController.cpp:805-808`) and preset files are meant to travel
/// between the two platforms. NUL is stripped as well: no filesystem takes it, and the original
/// never had to say so because a Windows text field cannot type it.
pub const PRESET_NAME_RESERVED: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Preset names are truncated to this many characters (`FxController.cpp:380-383`, and the
/// interactive editor's `setInputRestrictions(64)` at `FxPresetNameEditor.cpp:52`). Shorter than
/// [`MAX_NAME_LEN`], which is what the *file* can carry; this is what a person may type.
pub const MAX_PRESET_NAME_LEN: usize = 64;

/// Steps 1 and 2 of `FxController::sanitizePresetName` (`FxController.cpp:377-391`): strip the
/// reserved characters, then truncate to [`MAX_PRESET_NAME_LEN`] characters. Surrounding
/// whitespace goes too, because a name that is only spaces is not a name and a trailing space is
/// a filename nobody can see.
///
/// The one sanitiser for every route a name takes to disk. 0.3.0 had two — the command line
/// stripped nine characters and the store replaced three with underscores — and a name typed
/// with a `:` in the window became a file the Windows build could not open. Step 3, the
/// case-insensitive collision check against the existing names (`FxModel.cpp:142-153`), needs the
/// preset list and so belongs to the controller. The order matters and is why
/// `--save_preset="Mu:sic"` is a no-op when a preset named `Music` exists: stripping the `:`
/// produces the collision (`docs/COMMAND_LINE_OPTIONS.md:52`).
#[must_use]
pub fn sanitise_preset_name(name: &str) -> String {
    let stripped: String = name
        .chars()
        .filter(|c| *c != '\0' && !PRESET_NAME_RESERVED.contains(c))
        .collect();
    let cut: String = stripped.trim().chars().take(MAX_PRESET_NAME_LEN).collect();
    cut.trim_end().to_owned()
}

/// Parse a `.fac` file.
///
/// `bytes` rather than `&str` because the name line is raw UTF-8 written by a narrow `fprintf`
/// while the rest of the file is ASCII; a file with an invalid byte in the name should still load
/// with the name lossily decoded rather than failing outright.
pub fn parse(bytes: &[u8]) -> Result<Preset, PresetError> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = Lines::new(&text);

    // Line 1 — read, validated on its first whitespace-delimited token, then discarded.
    let magic = lines.next_line("the CLASS1 header")?;
    if magic.split_whitespace().next() != Some(MAGIC) {
        return Err(PresetError::NotAPreset {
            found: magic.to_owned(),
        });
    }

    let version = lines.next_f32("the version")?;

    // Line 3 is the preset name: raw bytes, no `value:` prefix.
    let name = lines.next_line("the preset name")?.trim_end().to_owned();
    if name.is_empty() {
        return Err(PresetError::MissingName);
    }

    // The double-params flag only exists from version 2 onwards.
    let double_params = if version > 1.0 {
        lines.next_i64("the double-params flag")? != 0
    } else {
        false
    };

    let total_elements = lines.next_i64("the element count")?.max(0) as usize;

    let mut main_midi = [0_u8; NUM_MAIN_PARAMS];
    for slot in &mut main_midi {
        *slot = lines.next_i64("a Main value")?.clamp(0, 127) as u8;
        if double_params {
            lines.next_i64("a Main_2 value")?;
        }
    }

    // Every shipped preset has exactly one element whose seven params are zero, but the format
    // allows any number and the reader is strict about their ordering.
    let mut element_params = [0_i32; NUM_ELEMENT_PARAMS];
    for element in 0..total_elements {
        let index = lines.next_i64("an element index")?;
        if index != element as i64 {
            return Err(PresetError::ElementOutOfOrder {
                line: lines.line_no,
                found: index,
                expected: element,
            });
        }
        for slot in &mut element_params {
            let value = lines.next_i64("an element parameter")? as i32;
            // Only the first element's params are kept; the format allows more, no preset uses them.
            if element == 0 {
                *slot = value;
            }
            if double_params {
                lines.next_i64("a Param_2 value")?;
            }
        }
    }

    let num_ints = lines.next_i64("the app-dependent integer count")?.max(0) as usize;
    let num_reals = lines.next_i64("the app-dependent real count")?.max(0) as usize;
    let num_strings = lines.next_i64("the app-dependent string count")?.max(0) as usize;

    let mut app_ints = [0_i32; NUM_APP_INTS];
    for i in 0..num_ints {
        let value = lines.next_i64("an app-dependent integer")? as i32;
        if let Some(slot) = app_ints.get_mut(i) {
            *slot = value;
        }
    }
    for _ in 0..num_reals {
        lines.next_f32("an app-dependent real")?;
    }
    for _ in 0..num_strings {
        lines.next_line("a string label")?;
        lines.next_line("a string value")?;
    }

    // The equalizer block. The writer emits it whenever it holds an EQ handle; the reader only
    // looks for it from version 9 on, and that is the behaviour that matters for compatibility.
    let (eq_bands, eq_on) = if version >= EQ_MIN_VERSION && !lines.is_exhausted() {
        let num_bands = lines.next_i64("the band count")?.max(0) as usize;
        if num_bands > eq::MAX_BANDS {
            return Err(PresetError::TooManyBands(num_bands));
        }
        let on = lines.next_i64("the equalizer on/off flag")? != 0;
        let mut bands = Vec::with_capacity(num_bands);
        for _ in 0..num_bands {
            lines.next_line("a Band label")?;
            let center_hz = lines.next_f32("a band centre frequency")?;
            let boost_db = lines.next_f32("a band boost/cut")?;
            bands.push(EqBand::new(center_hz, boost_db));
        }
        (bands, on)
    } else {
        (eq::default_bands(), true)
    };

    Ok(Preset {
        name,
        version,
        main_midi,
        app_ints,
        element_params,
        eq_bands,
        eq_on,
    })
}

/// Serialise a preset back to `.fac` text with LF line endings.
///
/// Byte-identical to what the original writes for any preset it produced, apart from the line
/// ending: the Windows build opens the file in text mode so it emits CRLF. Files in the FxSound
/// tree use both, and the reader accepts either.
#[must_use]
pub fn write(preset: &Preset) -> String {
    let mut out = String::with_capacity(1024);

    let _ = writeln!(out, "CLASS1 : Effect Type");
    let _ = writeln!(out, "{}: Version", format_g(preset.version));
    let _ = writeln!(out, "{}", preset.name);
    let _ = writeln!(out, "0: Double Params Flag");
    let _ = writeln!(out, "1: Total number of elements");

    for (i, value) in preset.main_midi.iter().enumerate() {
        let _ = writeln!(out, "{value}: Main {i}");
    }

    let _ = writeln!(out, "0: Element Number");
    for (i, value) in preset.element_params.iter().enumerate() {
        let _ = writeln!(out, "   {value}: Param {i}");
    }

    let _ = writeln!(
        out,
        "{NUM_APP_INTS}: Number of Application Dependent Integers"
    );
    let _ = writeln!(out, "0: Number of Application Dependent Reals");
    let _ = writeln!(out, "0: Number of Application Dependent Strings");
    for (i, value) in preset.app_ints.iter().enumerate() {
        let _ = writeln!(out, "{value}: Integer[{i}]");
    }

    let _ = writeln!(out, "{}: Number of EQ Bands", preset.eq_bands.len());
    let _ = writeln!(out, "{}: On/Off Flag", i32::from(preset.eq_on));
    for (i, band) in preset.eq_bands.iter().enumerate() {
        let _ = writeln!(out, "Band {}", i + 1);
        let _ = writeln!(out, "   {}: CF", format_g(band.center_hz));
        let _ = writeln!(out, "   {}: Boost/Cut", format_g(band.boost_db));
    }

    out
}

/// Read a preset from disk, taking the display name from the file stem when the file itself
/// carries a different one — which is what the shipping application shows.
pub fn load(path: &std::path::Path) -> Result<Preset, PresetError> {
    let bytes = std::fs::read(path)?;
    parse(&bytes)
}

/// Write a preset to disk, creating parent directories as needed.
///
/// A durable replace rather than a plain write: this is the one function every preset write goes
/// through, including the autosave that fires on each preset switch with unsaved edits and again
/// on shutdown, so an interrupted one would regularly leave a truncated `.fac` where a readable
/// preset used to be.
pub fn save(preset: &Preset, path: &std::path::Path) -> Result<(), PresetError> {
    fxsound_core::atomic::write(path, write(preset).as_bytes())?;
    Ok(())
}

/// As [`save`], but keeps the previous contents as `<path>.fac.bak`.
///
/// For the paths where a *user* replaces a preset they authored. The autosave deliberately does
/// not use this: it rewrites the same files continuously and would bury the directory in copies.
pub fn save_with_backup(preset: &Preset, path: &std::path::Path) -> Result<(), PresetError> {
    fxsound_core::atomic::write_with_backup(path, write(preset).as_bytes())?;
    Ok(())
}

/// C's `%g` with the default precision of six significant digits.
///
/// Used for every float the format carries, so that `62.5` stays `62.5`, `13000` stays `13000`
/// and `4.7244094` becomes `4.72441`.
#[must_use]
pub fn format_g(value: f32) -> String {
    const PRECISION: i32 = 6;

    if value == 0.0 {
        return "0".to_owned();
    }
    if !value.is_finite() {
        return if value.is_nan() {
            "nan".to_owned()
        } else if value > 0.0 {
            "inf".to_owned()
        } else {
            "-inf".to_owned()
        };
    }

    // The exponent %g uses to decide between %e and %f is the one of the *rounded* value.
    let exponent = {
        let raw = f64::from(value.abs()).log10().floor() as i32;
        // Rounding to `PRECISION` significant digits can carry into the next decade (9.999996 -> 10).
        let scale = 10_f64.powi(PRECISION - 1 - raw);
        let rounded = (f64::from(value.abs()) * scale).round() / scale;
        if rounded.log10().floor() as i32 > raw {
            raw + 1
        } else {
            raw
        }
    };

    if !(-4..PRECISION).contains(&exponent) {
        let formatted = format!("{:.*e}", (PRECISION - 1) as usize, value);
        let (mantissa, exp) = formatted
            .split_once('e')
            .unwrap_or((formatted.as_str(), "0"));
        let mantissa = trim_trailing_zeros(mantissa);
        let exp: i32 = exp.parse().unwrap_or(0);
        format!(
            "{mantissa}e{}{:02}",
            if exp < 0 { '-' } else { '+' },
            exp.abs()
        )
    } else {
        let decimals = (PRECISION - 1 - exponent).max(0) as usize;
        trim_trailing_zeros(&format!("{value:.decimals$}"))
    }
}

fn trim_trailing_zeros(s: &str) -> String {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        s.to_owned()
    }
}

/// A line-at-a-time cursor that parses numbers the way C's `scanf` does.
struct Lines<'a> {
    inner: std::iter::Peekable<std::str::Lines<'a>>,
    line_no: usize,
}

impl<'a> Lines<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            // `str::lines` already treats \r\n as a terminator and tolerates a missing final one.
            inner: text.lines().peekable(),
            line_no: 0,
        }
    }

    fn is_exhausted(&mut self) -> bool {
        self.inner.peek().is_none()
    }

    fn next_line(&mut self, expected: &'static str) -> Result<&'a str, PresetError> {
        self.line_no += 1;
        self.inner.next().ok_or(PresetError::UnexpectedEof {
            line: self.line_no,
            expected,
        })
    }

    fn next_i64(&mut self, expected: &'static str) -> Result<i64, PresetError> {
        let line = self.next_line(expected)?;
        scan_i64(line).ok_or_else(|| PresetError::Malformed {
            line: self.line_no,
            expected,
            found: line.to_owned(),
        })
    }

    fn next_f32(&mut self, expected: &'static str) -> Result<f32, PresetError> {
        let line = self.next_line(expected)?;
        scan_f32(line).ok_or_else(|| PresetError::Malformed {
            line: self.line_no,
            expected,
            found: line.to_owned(),
        })
    }
}

/// C `%d`: skip blanks, take an optional sign and the digits that follow.
fn scan_i64(line: &str) -> Option<i64> {
    let s = line.trim_start();
    let mut end = 0;
    let bytes = s.as_bytes();
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        end = 1;
    }
    let digits_start = end;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == digits_start {
        return None;
    }
    s[..end].parse().ok()
}

/// C `%g`: skip blanks, then take a float, stopping at the first character that cannot extend it.
fn scan_f32(line: &str) -> Option<f32> {
    let s = line.trim_start();
    let bytes = s.as_bytes();
    let mut end = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        end = 1;
    }
    let mut seen_digit = false;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
        seen_digit = true;
    }
    if end < bytes.len() && bytes[end] == b'.' {
        end += 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
            seen_digit = true;
        }
    }
    if !seen_digit {
        return None;
    }
    // Optional exponent, but only if it is well-formed; otherwise it is not part of the number.
    if end < bytes.len() && (bytes[end] == b'e' || bytes[end] == b'E') {
        let mut probe = end + 1;
        if matches!(bytes.get(probe), Some(b'+' | b'-')) {
            probe += 1;
        }
        let exp_digits = probe;
        while probe < bytes.len() && bytes[probe].is_ascii_digit() {
            probe += 1;
        }
        if probe > exp_digits {
            end = probe;
        }
    }
    // A literal `nan` or `inf` cannot get this far — the scan above requires an ASCII digit —
    // but an overflowing magnitude can: `1e40` parses cleanly to `+inf`. That value rides into
    // the preset, and the moment the application autosaves it, `format_g` writes it back as C's
    // `%g` spelling of infinity, which this scanner then refuses. One bad import makes the
    // preset permanently unreadable, by this port and by the Windows original alike. Refusing it
    // here turns it into an error the user is told about at import time instead.
    s[..end]
        .parse::<f32>()
        .ok()
        .filter(|value| value.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fxsound_core::Effect;

    /// A complete version 9 preset, written out here rather than read from `assets/presets`.
    ///
    /// The tests below are about the grammar and about which Main slot feeds which effect, and
    /// neither has anything to do with how a particular preset is voiced. Reading a shipped `.fac`
    /// for its expected numbers turned every retune of that file into a test failure, and the
    /// tempting way out — editing the preset back — would have changed the product to suit the
    /// suite. The shipped files are still covered, by `round_trips_every_shipped_preset` below and
    /// by the checks in `fxsound-dsp`, both of which assert properties rather than particular
    /// values.
    ///
    /// The shape is deliberately that of a real file: leading blanks on the indented lines, a
    /// fractional first-band gain to exercise the `%g` scan, and a zero on the last band.
    const FIXTURE: &str = "\
CLASS1 : Effect Type
9: Version
Fixture
0: Double Params Flag
1: Total number of elements
50: Main 0
20: Main 1
0: Main 2
0: Main 3
60: Main 4
60: Main 5
0: Element Number
   0: Param 0
   0: Param 1
   0: Param 2
   0: Param 3
   0: Param 4
   0: Param 5
   0: Param 6
7: Number of Application Dependent Integers
0: Number of Application Dependent Reals
0: Number of Application Dependent Strings
1: Integer[0]
1: Integer[1]
0: Integer[2]
1: Integer[3]
1: Integer[4]
0: Integer[5]
2: Integer[6]
10: Number of EQ Bands
1: On/Off Flag
Band 1
   62.5: CF
   4.72441: Boost/Cut
Band 2
   115: CF
   3: Boost/Cut
Band 3
   215: CF
   1: Boost/Cut
Band 4
   400: CF
   0: Boost/Cut
Band 5
   900: CF
   -2: Boost/Cut
Band 6
   1600: CF
   0: Boost/Cut
Band 7
   2900: CF
   1: Boost/Cut
Band 8
   5200: CF
   2: Boost/Cut
Band 9
   9000: CF
   1.5: Boost/Cut
Band 10
   13000: CF
   0: Boost/Cut
";

    #[test]
    fn parses_every_field_of_a_version_9_file() {
        let preset = parse(FIXTURE.as_bytes()).expect("parse the fixture");
        assert_eq!(preset.name, "Fixture");
        assert_eq!(preset.version, 9.0);
        // "50: Main 0" .. "60: Main 5"
        assert_eq!(preset.main_midi, [50, 20, 0, 0, 60, 60]);
        assert_eq!(preset.app_ints, [1, 1, 0, 1, 1, 0, 2]);
        assert_eq!(preset.element_params, [0; 7]);
        assert_eq!(preset.eq_bands.len(), 10);
        assert!(preset.eq_on);
        assert_eq!(preset.eq_bands[0], EqBand::new(62.5, 4.72441));
        assert_eq!(preset.eq_bands[9], EqBand::new(13000.0, 0.0));
    }

    #[test]
    fn effect_values_use_the_right_slot() {
        let preset = parse(FIXTURE.as_bytes()).expect("parse");
        // Main 0 = 50 is Fidelity, Main 1 = 20 is Surround, Main 3 = 0 is Ambience.
        assert!((preset.effect(Effect::Fidelity) - 50.0 / 127.0).abs() < 1e-6);
        assert!((preset.effect(Effect::Surround) - 20.0 / 127.0).abs() < 1e-6);
        assert_eq!(preset.effect(Effect::Ambience), 0.0);
        assert!((preset.effect(Effect::DynamicBoost) - 60.0 / 127.0).abs() < 1e-6);
        assert!((preset.effect(Effect::Bass) - 60.0 / 127.0).abs() < 1e-6);
    }

    #[test]
    fn scan_follows_c_conversion_rules() {
        assert_eq!(scan_i64("   0: Param 0"), Some(0));
        assert_eq!(scan_i64("-1: Boost/Cut"), Some(-1));
        assert_eq!(scan_i64("50: Main 0"), Some(50));
        assert_eq!(scan_i64("Band 1"), None);
        assert_eq!(scan_f32("   62.5: CF"), Some(62.5));
        assert_eq!(scan_f32("   4.72441: Boost/Cut"), Some(4.72441));
        assert_eq!(scan_f32("9: Version"), Some(9.0));
        assert_eq!(scan_f32("1e3: x"), Some(1000.0));
        // A bare `e` that is not a valid exponent must not be swallowed.
        assert_eq!(scan_f32("12etc"), Some(12.0));
    }

    #[test]
    fn format_g_matches_c() {
        assert_eq!(format_g(0.0), "0");
        assert_eq!(format_g(9.0), "9");
        assert_eq!(format_g(62.5), "62.5");
        assert_eq!(format_g(13000.0), "13000");
        assert_eq!(format_g(-1.0), "-1");
        assert_eq!(format_g(4.724_409_5), "4.72441");
        assert_eq!(format_g(0.5), "0.5");
        // Six significant digits, then %e above 1e6.
        assert_eq!(format_g(1_000_000.0), "1e+06");
        assert_eq!(format_g(0.000_012_345_6), "1.23456e-05");
    }

    #[test]
    fn rejects_a_file_that_is_not_a_preset() {
        let err = parse(b"hello\nworld\n").unwrap_err();
        assert!(matches!(err, PresetError::NotAPreset { .. }));
    }

    #[test]
    fn accepts_crlf_and_a_missing_final_newline() {
        let crlf = FIXTURE.replace('\n', "\r\n");
        let parsed = parse(crlf.as_bytes()).expect("parse CRLF");
        assert_eq!(parsed.name, "Fixture");

        let truncated = FIXTURE.trim_end_matches('\n');
        let parsed = parse(truncated.as_bytes()).expect("parse without final newline");
        assert_eq!(parsed.eq_bands.len(), 10);
    }

    #[test]
    fn round_trips_every_shipped_preset() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/presets")
            .canonicalize()
            .expect("assets/presets exists");

        let mut checked = 0;
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read preset dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("fac") {
                    continue;
                }

                let bytes = std::fs::read(&path).expect("read preset");
                let first = parse(&bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                let text = write(&first);
                let second = parse(text.as_bytes())
                    .unwrap_or_else(|e| panic!("{} re-parse: {e}", path.display()));

                assert_eq!(first, second, "{} did not round trip", path.display());
                checked += 1;
            }
        }
        assert!(checked >= 30, "only found {checked} presets to check");
    }

    #[test]
    fn a_gain_that_overflows_an_f32_is_refused_rather_than_becoming_infinity() {
        // Before this was rejected the value became `+inf`, survived into the running preset, and
        // was written back by `format_g` in a spelling neither this parser nor the Windows
        // original can read — so a single bad import cost the user the preset for good.
        let good = parse(FIXTURE.as_bytes()).expect("the fixture parses");

        let poisoned = FIXTURE.to_owned();
        let first_band = good.eq_bands[0].boost_db;
        let needle = crate::format_g(first_band);
        assert!(
            poisoned.contains(&needle),
            "the fixture should contain the first band's gain as written"
        );
        let poisoned = poisoned.replacen(&needle, "1e40", 1);

        let err = parse(poisoned.as_bytes()).expect_err("1e40 must not parse");
        assert!(
            matches!(err, PresetError::Malformed { .. }),
            "expected a Malformed error, got {err:?}"
        );
    }

    #[test]
    fn every_value_a_preset_can_carry_survives_being_written_and_read_again() {
        // The property the previous test protects: `parse` and `write` are inverses over exactly
        // the set of values `parse` can produce.
        let preset = parse(FIXTURE.as_bytes()).expect("parse");
        let text = write(&preset);
        let again = parse(text.as_bytes()).expect("reparse what we just wrote");
        assert_eq!(preset, again);
        for band in &again.eq_bands {
            assert!(band.boost_db.is_finite() && band.center_hz.is_finite());
        }
    }

    #[test]
    fn surrounding_whitespace_is_not_part_of_a_preset_name() {
        // A trailing space is a filename nobody can see, and a name that is only spaces is not a
        // name. Spaces inside stay: "Bass Boost" is a shipped preset.
        assert_eq!(sanitise_preset_name("  Rock  "), "Rock");
        assert_eq!(sanitise_preset_name("Bass  Boost"), "Bass  Boost");
        assert_eq!(sanitise_preset_name("   "), "");
    }

    #[test]
    fn nul_is_stripped_like_a_reserved_character() {
        // Not in the original's list only because a Windows text field cannot type it; no
        // filesystem takes it, and `--save_preset` can be handed one by a script.
        assert_eq!(sanitise_preset_name("a\0b"), "ab");
        assert_eq!(sanitise_preset_name("\0"), "");
    }

    #[test]
    fn every_reserved_character_is_stripped_wherever_it_sits() {
        for c in PRESET_NAME_RESERVED {
            assert_eq!(
                sanitise_preset_name(&format!("{c}Ro{c}ck{c}")),
                "Rock",
                "{c:?}"
            );
        }
        assert_eq!(sanitise_preset_name(r#"<>:"/\|?*"#), "");
        // And nothing else is: the set is deliberately the Windows one, not the shell's, and a
        // shipped preset is called R&B.
        assert_eq!(sanitise_preset_name("R&B"), "R&B");
        assert_eq!(
            sanitise_preset_name("Naïve 'Jazz' (live) #2"),
            "Naïve 'Jazz' (live) #2"
        );
    }

    #[test]
    fn a_long_name_is_cut_to_sixty_four_characters_and_does_not_end_in_a_space() {
        // 63 letters, a space, 6 more letters: the cut lands on the space, and a stem with a
        // trailing space is the filename problem the trim exists for.
        let name = format!("{} {}", "a".repeat(63), "b".repeat(6));
        assert_eq!(name.chars().count(), 70);
        assert_eq!(sanitise_preset_name(&name), "a".repeat(63));
        // The limit is characters, not bytes: a name of two-byte letters keeps 64 of them.
        let wide = "é".repeat(70);
        assert_eq!(
            sanitise_preset_name(&wide).chars().count(),
            MAX_PRESET_NAME_LEN
        );
        // Exactly 64 is not cut at all.
        let exact = "a".repeat(MAX_PRESET_NAME_LEN);
        assert_eq!(sanitise_preset_name(&exact), exact);
    }

    #[test]
    fn reserved_characters_are_stripped_before_the_cut_is_measured() {
        // The original's order (`FxController.cpp:378` before `:380`): the characters that go
        // do not count towards the 64, so ten colons in front of 64 letters leave all 64.
        let name = format!("{}{}", ":".repeat(10), "a".repeat(MAX_PRESET_NAME_LEN));
        assert_eq!(sanitise_preset_name(&name), "a".repeat(MAX_PRESET_NAME_LEN));
    }
}
