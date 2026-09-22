//! Voice presets, which are TOML and not `.fac`.
//!
//! The `.fac` format is a byte-for-byte contract with the Windows build: its line order, its
//! spelling and its rounding are all fixed, and it has nowhere to put a gate threshold because the
//! chain it describes has no gate. Bending it to carry a voice preset would break the one property
//! that makes a preset file worth having — that the same file means the same thing in both builds.
//! So the microphone chain gets a format of its own, and the `.fac` path is untouched.
//!
//! TOML rather than a second binary format because these are read by people: the numbers in them
//! were argued over, they will be argued over again, and a preset whose reasoning cannot be read
//! beside its values is a preset nobody can revise.
//!
//! **A stage that is off has no table.** `[gate]` missing means no gate, not a gate at its
//! defaults. That distinction is the whole difference between Flat and Clean Voice, and expressing
//! it as an absence rather than as `enabled = false` makes it impossible to write a preset that
//! carries a full set of gate numbers which nothing reads.
//!
//! **Every key 0.4.0 added is optional, and no key was renamed.** A 0.3.0 file loads and means
//! what it meant; a 0.4.0 file loads on a 0.3.0 binary, which drops the tables it does not know
//! and reads `rnnoise` — kept as the master switch, and written in step with the `[denoise]`
//! table — exactly as it always did. Unknown keys are ignored everywhere for the same reason, so
//! the next version's keys will not stop this one reading a file.

use fxsound_core::messages::InputDspParams;
use fxsound_core::{
    DeEsserMode, DenoiseChannelMode, DenoiseControl, DenoiseLevel, DereverbLevel, Detection, eq,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The stage orderings a preset may name with `chain = "…"`, and the one it gets when it names
/// none.
///
/// The names belong to the chain specs in `fxsound-dsp`; this crate only carries the string. A
/// name outside this list is not refused at load — a preset written for a later version that
/// knows more chains must still open here — and it is the chain builder's job to fall back to
/// `voice` and say so.
pub const CHAIN_NAMES: [&str; 4] = ["voice", "podcast", "broadcast", "streaming"];
/// The chain a preset that names none is built on: the 0.3.0 chain.
pub const DEFAULT_CHAIN: &str = "voice";

/// A named set of voice-chain settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputPreset {
    /// What the preset is called. Shown as written; never translated, as on the output side.
    pub name: String,
    /// One line on who this is for. Read by a person deciding which preset to pick, and by
    /// whoever revises it next.
    #[serde(default)]
    pub description: String,

    /// Run RNNoise in front of everything else.
    ///
    /// Still the master switch, for the 0.3.0 binary's sake. When a `[denoise]` table is present
    /// the table decides and this is written to agree with it; when it is absent, `true` means
    /// what it meant in 0.3.0 — the `Medium` row, one network per channel.
    #[serde(default)]
    pub rnnoise: bool,
    /// How hard the denoiser works and how it treats a stereo capture. Absent means whatever
    /// `rnnoise` says; present, it wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denoise: Option<Denoise>,
    /// Late-reverberation suppression. Absent means the stage is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dereverb: Option<Dereverb>,

    /// The high-pass corner in Hz, and its order — `0`, `2` or `4`.
    pub highpass_hz: f32,
    pub highpass_order: u8,

    /// Absent means the stage is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<Gate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compressor: Option<Compressor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deesser: Option<DeEsser>,

    pub eq: Equalizer,

    /// Applied after every stage that measures, before the limiter.
    pub makeup_db: f32,
    /// The limiter's ceiling. The limiter itself has no switch.
    pub ceiling_db: f32,

    /// Which stage ordering to build: one of [`CHAIN_NAMES`], `voice` when unsaid. Only written
    /// when it is not the default, so a file that never mentioned it keeps not mentioning it.
    #[serde(default = "default_chain", skip_serializing_if = "is_default_chain")]
    pub chain: String,
    /// Let the denoiser's voice probability hold the gate open. Off when unsaid, and unwritten
    /// when off, on the same doctrine as the tables: an absence is the off state.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vad_gate: bool,
}

fn default_chain() -> String {
    DEFAULT_CHAIN.to_owned()
}

fn is_default_chain(chain: &str) -> bool {
    chain == DEFAULT_CHAIN
}

/// The `[denoise]` table: the control surface over RNNoise.
///
/// `level` defaults to [`DenoiseLevel`]'s own default — `Medium`, what `rnnoise = true` always
/// meant — so a table that only names `channels` is a table that still denoises. `Off` can be
/// spelled here, and means off; the shipped set never spells it, because a stage that is off has
/// no table, and a test over the set says so.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Denoise {
    #[serde(default)]
    pub level: DenoiseLevel,
    #[serde(default)]
    pub channels: DenoiseChannelMode,
    /// Overrides for the level's table row. Each is taken from the level unless the file says
    /// otherwise, so a preset can move one number without restating the other three.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_suppression_db: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vad_threshold: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_preservation: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wet_dry: Option<f32>,
}

impl Denoise {
    /// The level's row with this table's overrides written over it.
    ///
    /// Not sanitised, like everything else on the way out of a preset: an override outside its
    /// range is a mistake the shipped-set test exists to find.
    #[must_use]
    pub fn control(&self) -> DenoiseControl {
        let mut control = self.level.control();
        if let Some(value) = self.max_suppression_db {
            control.max_suppression_db = value;
        }
        if let Some(value) = self.vad_threshold {
            control.vad_threshold = value;
        }
        if let Some(value) = self.voice_preservation {
            control.voice_preservation = value;
        }
        if let Some(value) = self.wet_dry {
            control.wet_dry = value;
        }
        control
    }
}

/// The `[dereverb]` table. Its level is required: a table that is present but says nothing would
/// read as `Off`, and a table that means off is the one thing the format does not allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dereverb {
    pub level: DereverbLevel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gate {
    pub threshold_db: f32,
    pub ratio: f32,
    /// The cap on attenuation. Negative, and never absent: an expander with no cap pumps the room
    /// floor in and out at the rate of speech, which is more audible than the floor.
    pub range_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub hold_ms: f32,
    /// Which quantity the threshold is on. Not optional, and not defaulted: peak and RMS against
    /// the same number differ by three to seven decibels, so a preset that does not say has said
    /// two things.
    pub detection: Detection,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Compressor {
    pub threshold_db: f32,
    pub ratio: f32,
    pub knee_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub detection: Detection,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeEsser {
    pub frequency_hz: f32,
    /// Measured in the split band, not in the whole signal.
    pub threshold_db: f32,
    /// Whether `frequency_hz` is the corner or a ceiling on one chosen from the source's
    /// bandwidth. Defaulted, unlike `detection`: `classic` is what every 0.3.0 file meant and
    /// the two modes agree wherever the source is wide enough for the corner asked for.
    #[serde(default)]
    pub mode: DeEsserMode,
}

/// The ten-band ladder, spelled out rather than implied.
///
/// The centres are stored beside the gains for the same reason a `.fac` stores them: a preset is
/// allowed its own ladder, and a file that only carried gains would silently re-voice itself the
/// day the default ladder moved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Equalizer {
    pub centers_hz: Vec<f32>,
    pub gains_db: Vec<f32>,
}

/// What went wrong reading or writing a voice preset.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// The file is not a voice preset. It carries the path because the message reaches a log
    /// line about one file in a directory of them, and "expected a float" on its own names
    /// nothing.
    #[error("{path}: {source}")]
    Toml {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("could not write TOML: {0}")]
    Serialise(#[from] toml::ser::Error),
    #[error("{path}: the equalizer has {centers} centres and {gains} gains")]
    Mismatched {
        path: String,
        centers: usize,
        gains: usize,
    },
    /// A name the store's list does not hold. The string reaches the command line and D-Bus.
    #[error("no voice preset named {0:?}")]
    Unknown(String),
    /// A name with nothing left in it once it has been made safe as a filename.
    #[error("the preset's name leaves nothing to file it under")]
    MissingName,
    /// A name whose file is already another listed preset's; the `.fac` side's
    /// [`PresetError::SharedFile`](crate::PresetError::SharedFile), for the same reason.
    #[error("{name:?} and {existing:?} would share the file {file}")]
    SharedFile {
        name: String,
        existing: String,
        file: String,
    },
}

impl Default for InputPreset {
    /// Clean Voice — the reference the rest of the set is voiced against, and the same numbers
    /// [`InputDspParams::default`] carries. Here so that a preset can be built from a few fields
    /// and `..Default::default()`, which is what keeps every new optional key from breaking a
    /// struct literal somewhere.
    fn default() -> Self {
        Self {
            name: "Clean Voice".to_owned(),
            description: "A desk microphone, tidied rather than styled — Discord, Teams, Zoom or \
                          in-game. The one to compare the others against."
                .to_owned(),
            rnnoise: false,
            denoise: None,
            dereverb: None,
            highpass_hz: 80.0,
            highpass_order: 2,
            gate: Some(Gate {
                threshold_db: -45.0,
                ratio: 2.0,
                range_db: -14.0,
                attack_ms: 5.0,
                release_ms: 150.0,
                hold_ms: 200.0,
                detection: Detection::Rms,
            }),
            compressor: Some(Compressor {
                threshold_db: -18.0,
                ratio: 3.0,
                knee_db: 6.0,
                attack_ms: 20.0,
                release_ms: 150.0,
                detection: Detection::Rms,
            }),
            deesser: Some(DeEsser {
                frequency_hz: 5_500.0,
                threshold_db: -22.0,
                mode: DeEsserMode::Classic,
            }),
            eq: Equalizer {
                centers_hz: eq::DEFAULT_CENTERS_HZ.to_vec(),
                gains_db: vec![0.0, 0.0, -1.0, -1.5, -0.5, 0.0, 1.5, 0.0, 0.0, 0.0],
            },
            makeup_db: 6.0,
            ceiling_db: -3.0,
            chain: default_chain(),
            vad_gate: false,
        }
    }
}

impl InputPreset {
    /// Read one preset from a TOML file.
    ///
    /// # Errors
    /// The file cannot be read, is not valid TOML, or its band tables disagree in length.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path)?;
        let preset: Self = toml::from_str(&text).map_err(|source| Error::Toml {
            path: path.display().to_string(),
            source,
        })?;
        if preset.eq.centers_hz.len() != preset.eq.gains_db.len() {
            return Err(Error::Mismatched {
                path: path.display().to_string(),
                centers: preset.eq.centers_hz.len(),
                gains: preset.eq.gains_db.len(),
            });
        }
        Ok(preset)
    }

    /// Write one preset to a TOML file, durably.
    ///
    /// `rnnoise` is written as the `[denoise]` table implies, whatever the field holds: the two
    /// spellings must agree on disk, because a 0.3.0 binary reads only the switch and a 0.4.0
    /// binary lets the table win.
    ///
    /// # Errors
    /// The preset cannot be serialised, or the file cannot be replaced.
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        let text = self.to_toml()?;
        fxsound_core::atomic::write(path, text.as_bytes())?;
        Ok(())
    }

    /// As [`InputPreset::save`], keeping any previous file as `<path>.bak`.
    ///
    /// For the paths where a *user* replaces a preset they authored. The autosave deliberately
    /// does not use this: it rewrites the same files continuously and would bury the directory in
    /// copies.
    ///
    /// # Errors
    /// As [`InputPreset::save`].
    pub fn save_with_backup(&self, path: &Path) -> Result<(), Error> {
        let text = self.to_toml()?;
        fxsound_core::atomic::write_with_backup(path, text.as_bytes())?;
        Ok(())
    }

    /// The text [`InputPreset::save`] writes.
    fn to_toml(&self) -> Result<String, Error> {
        let mut on_disk = self.clone();
        on_disk.rnnoise = on_disk.denoise_level() != DenoiseLevel::Off;
        Ok(toml::to_string_pretty(&on_disk)?)
    }

    /// Read every `*.toml` in a directory, sorted by name so a listing is stable.
    ///
    /// One file that will not parse is logged and skipped, not fatal: the directory is one the
    /// user writes into, and a typo in one preset must not empty the microphone's list. 0.3.0
    /// gave up on the whole directory at the first bad file.
    ///
    /// # Errors
    /// The directory itself cannot be read.
    pub fn load_dir(dir: &Path) -> Result<Vec<Self>, Error> {
        let mut presets = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(err) => {
                    log::warn!("{}: {err}; skipping an entry", dir.display());
                    continue;
                }
            };
            if !path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("toml"))
            {
                continue;
            }
            match Self::load(&path) {
                Ok(preset) => presets.push(preset),
                // `Io` is the one variant that does not already name the file.
                Err(Error::Io(err)) => log::warn!("{}: {err}; skipping", path.display()),
                Err(err) => log::warn!("{err}; skipping"),
            }
        }
        presets.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(presets)
    }

    /// Every directory a shipped voice preset could be in, source tree first.
    ///
    /// The same search order `PresetStore::with_default_dirs` uses, and for the same reason: a
    /// developer running out of `target/release` must get the presets in the working tree, not the
    /// ones an installed package left behind.
    #[must_use]
    pub fn default_dirs() -> Vec<std::path::PathBuf> {
        let mut dirs = Vec::new();
        if let Ok(exe) = std::env::current_exe()
            && let Some(root) = exe
                .parent()
                .and_then(std::path::Path::parent)
                .and_then(std::path::Path::parent)
        {
            dirs.push(root.join("assets/presets/Input"));
        }
        for prefix in ["/usr/share/fxsound", "/usr/local/share/fxsound"] {
            dirs.push(Path::new(prefix).join("presets/Input"));
        }
        dirs
    }

    /// Load the shipped voice presets from the first directory that has any.
    ///
    /// A missing directory is not an error: a build without them is a build whose microphone chain
    /// runs on its defaults, which is a working chain. A directory that exists and cannot be read
    /// *is*, and a preset that will not parse is logged by [`InputPreset::load_dir`] as it skips
    /// it — a preset silently missing from the list is how someone ends up wondering where their
    /// voice went.
    #[must_use]
    pub fn load_shipped() -> Vec<Self> {
        for dir in Self::default_dirs() {
            match Self::load_dir(&dir) {
                Ok(presets) if !presets.is_empty() => return presets,
                Ok(_) => {}
                Err(Error::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => log::warn!("{}: {err}", dir.display()),
            }
        }
        Vec::new()
    }

    /// How hard the denoiser works, from the table when there is one and from the switch when
    /// there is not.
    ///
    /// The whole 0.3.0 compatibility rule in one place: `rnnoise = true` with no table is the
    /// `Medium` row, which is what the network did then; `false` with no table is `Off`.
    #[must_use]
    pub fn denoise_level(&self) -> DenoiseLevel {
        match (&self.denoise, self.rnnoise) {
            (Some(denoise), _) => denoise.level,
            (None, true) => DenoiseLevel::Medium,
            (None, false) => DenoiseLevel::Off,
        }
    }

    /// How the denoiser treats a stereo capture: `Independent` — one network per channel, what
    /// 0.3.0 did — unless the table says otherwise.
    #[must_use]
    pub fn denoise_channels(&self) -> DenoiseChannelMode {
        self.denoise
            .as_ref()
            .map_or(DenoiseChannelMode::Independent, |denoise| denoise.channels)
    }

    /// The parameter snapshot this preset describes.
    ///
    /// Deliberately *not* sanitised here: a preset that carries a number the engine would clamp is
    /// a preset with a mistake in it, and the test over the shipped set is what has to catch it.
    /// Sanitising on the way out would hide exactly the defect worth finding.
    #[must_use]
    pub fn to_params(&self) -> InputDspParams {
        let denoise_level = self.denoise_level();
        let denoise_control = self
            .denoise
            .as_ref()
            .map_or_else(|| denoise_level.control(), Denoise::control);
        let mut params = InputDspParams {
            power: true,
            rnnoise: denoise_level != DenoiseLevel::Off,
            denoise_level,
            denoise_channels: self.denoise_channels(),
            denoise_control,
            dereverb: self
                .dereverb
                .map_or(DereverbLevel::Off, |dereverb| dereverb.level),
            highpass_hz: self.highpass_hz,
            highpass_order: self.highpass_order,
            makeup_db: self.makeup_db,
            ceiling_db: self.ceiling_db,
            gate_on: self.gate.is_some(),
            vad_gate: self.vad_gate,
            compressor_on: self.compressor.is_some(),
            deesser_on: self.deesser.is_some(),
            eq_on: true,
            ..InputDspParams::default()
        };

        if let Some(gate) = &self.gate {
            params.gate_threshold_db = gate.threshold_db;
            params.gate_ratio = gate.ratio;
            params.gate_range_db = gate.range_db;
            params.gate_attack_ms = gate.attack_ms;
            params.gate_release_ms = gate.release_ms;
            params.gate_hold_ms = gate.hold_ms;
            params.gate_detection = gate.detection;
        }
        if let Some(compressor) = &self.compressor {
            params.compressor_threshold_db = compressor.threshold_db;
            params.compressor_ratio = compressor.ratio;
            params.compressor_knee_db = compressor.knee_db;
            params.compressor_attack_ms = compressor.attack_ms;
            params.compressor_release_ms = compressor.release_ms;
            params.compressor_detection = compressor.detection;
        }
        if let Some(deesser) = &self.deesser {
            params.deesser_hz = deesser.frequency_hz;
            params.deesser_threshold_db = deesser.threshold_db;
            params.deesser_mode = deesser.mode;
        }

        let live = self.eq.centers_hz.len().min(eq::MAX_BANDS);
        params.num_bands = live as u8;
        for index in 0..live {
            params.band_center_hz[index] = self.eq.centers_hz[index];
            params.band_boost_db[index] = self.eq.gains_db[index];
        }
        params
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A preset using every table and key the format has.
    fn sample() -> InputPreset {
        InputPreset {
            name: "Everything".to_owned(),
            description: "Every key the format has, so a round trip covers all of them.".to_owned(),
            rnnoise: true,
            denoise: Some(Denoise {
                level: DenoiseLevel::Strong,
                channels: DenoiseChannelMode::Linked,
                max_suppression_db: Some(30.0),
                vad_threshold: None,
                voice_preservation: Some(0.2),
                wet_dry: None,
            }),
            dereverb: Some(Dereverb {
                level: DereverbLevel::Light,
            }),
            deesser: Some(DeEsser {
                frequency_hz: 6_000.0,
                threshold_db: -24.0,
                mode: DeEsserMode::Adaptive,
            }),
            chain: "podcast".to_owned(),
            vad_gate: true,
            ..InputPreset::default()
        }
    }

    fn parse(text: &str) -> InputPreset {
        toml::from_str(text).expect("parse")
    }

    /// The smallest file the format accepts, plus whatever `extra` says.
    fn minimal(extra: &str) -> String {
        format!(
            "name = \"Minimal\"\nhighpass_hz = 80.0\nhighpass_order = 2\nmakeup_db = 0.0\n\
             ceiling_db = -3.0\n{extra}\n[eq]\ncenters_hz = [100.0, 1000.0]\ngains_db = [0.0, 1.0]\n"
        )
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fxsound-input-preset-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn shipped_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/presets/Input")
            .canonicalize()
            .expect("assets/presets/Input exists")
    }

    fn shipped() -> Vec<InputPreset> {
        let presets = InputPreset::load_dir(&shipped_dir()).expect("read the shipped set");
        assert!(presets.len() >= 13, "found {}", presets.len());
        presets
    }

    #[test]
    fn a_preset_round_trips_through_toml() {
        let preset = sample();
        let text = toml::to_string_pretty(&preset).expect("serialise");
        assert_eq!(parse(&text), preset);
        for key in [
            "[denoise]",
            "level = \"strong\"",
            "channels = \"linked\"",
            "max_suppression_db = 30.0",
            "[dereverb]",
            "level = \"light\"",
            "mode = \"adaptive\"",
            "chain = \"podcast\"",
            "vad_gate = true",
        ] {
            assert!(text.contains(key), "{key} missing from:\n{text}");
        }
        assert!(
            !text.contains("vad_threshold"),
            "an override that was not given was written:\n{text}"
        );
    }

    #[test]
    fn a_stage_that_is_off_is_an_absent_table_and_reads_back_as_off() {
        // The distinction between Flat and Clean Voice, and the reason it is an absence rather
        // than `enabled = false`: there is no way to write a preset carrying a full set of gate
        // numbers that nothing reads. The 0.4.0 tables follow the same rule.
        let preset = InputPreset {
            name: "Flat".to_owned(),
            gate: None,
            compressor: None,
            deesser: None,
            ..InputPreset::default()
        };

        let text = toml::to_string_pretty(&preset).expect("serialise");
        for table in [
            "[gate]",
            "[compressor]",
            "[deesser]",
            "[denoise]",
            "[dereverb]",
        ] {
            assert!(!text.contains(table), "an off stage wrote a table:\n{text}");
        }
        assert!(
            !text.contains("threshold_db"),
            "and it wrote its numbers:\n{text}"
        );
        assert!(!text.contains("vad_gate"), "off is unwritten:\n{text}");
        assert!(
            !text.contains("chain"),
            "the default chain is unwritten:\n{text}"
        );

        let params = preset.to_params();
        assert!(!params.gate_on);
        assert!(!params.compressor_on);
        assert!(!params.deesser_on);
        assert!(!params.rnnoise);
        assert_eq!(params.denoise_level, DenoiseLevel::Off);
        assert_eq!(params.dereverb, DereverbLevel::Off);
        assert!(!params.vad_gate);
    }

    #[test]
    fn the_parameters_a_preset_describes_are_the_ones_it_carries() {
        let params = sample().to_params();
        assert_eq!(params.gate_threshold_db, -45.0);
        assert_eq!(params.gate_range_db, -14.0);
        assert_eq!(params.compressor_ratio, 3.0);
        assert_eq!(params.deesser_hz, 6_000.0);
        assert_eq!(params.deesser_mode, DeEsserMode::Adaptive);
        assert_eq!(params.makeup_db, 6.0);
        assert_eq!(params.num_bands, 10);
        assert_eq!(params.denoise_level, DenoiseLevel::Strong);
        assert_eq!(params.denoise_channels, DenoiseChannelMode::Linked);
        assert_eq!(params.dereverb, DereverbLevel::Light);
        assert!(params.vad_gate);
        assert!(params.rnnoise);
        let (centers, gains) = params.bands();
        assert_eq!(centers[0], eq::DEFAULT_CENTERS_HZ[0]);
        assert_eq!(gains[6], 1.5);
    }

    #[test]
    fn to_params_does_not_launder_a_mistake() {
        // A preset carrying a number the engine would clamp has a mistake in it, and the test over
        // the shipped set is what finds it. Sanitising here would hide exactly that.
        let mut preset = sample();
        preset.makeup_db = 500.0;
        let params = preset.to_params();
        assert_eq!(params.makeup_db, 500.0, "to_params quietly corrected it");

        let mut sane = params;
        sane.sanitise();
        assert_ne!(sane.makeup_db, 500.0, "and sanitise is what would have");
    }

    #[test]
    fn a_denoise_override_is_not_laundered_either() {
        let mut preset = sample();
        preset.denoise = Some(Denoise {
            level: DenoiseLevel::Light,
            wet_dry: Some(7.0),
            ..Denoise::default()
        });
        let params = preset.to_params();
        assert_eq!(params.denoise_control.wet_dry, 7.0);
        let mut sane = params;
        sane.sanitise();
        assert_eq!(sane.denoise_control.wet_dry, 1.0);
    }

    #[test]
    fn a_zero_three_file_with_the_switch_on_means_medium_and_independent() {
        // Laptop Mic as 0.3.0 shipped it: `rnnoise = true` and nothing else. It has to sound
        // after the upgrade exactly as it did before, and what it did before was the Medium row
        // with one network per channel.
        let preset = parse(&minimal("rnnoise = true"));
        assert!(preset.denoise.is_none());
        assert_eq!(preset.denoise_level(), DenoiseLevel::Medium);
        assert_eq!(preset.denoise_channels(), DenoiseChannelMode::Independent);

        let params = preset.to_params();
        assert!(params.rnnoise);
        assert_eq!(params.denoise_level, DenoiseLevel::Medium);
        assert_eq!(params.denoise_channels, DenoiseChannelMode::Independent);
        assert_eq!(params.denoise_control, DenoiseLevel::Medium.control());
    }

    #[test]
    fn a_zero_three_file_with_the_switch_off_means_off() {
        let preset = parse(&minimal("rnnoise = false"));
        assert_eq!(preset.denoise_level(), DenoiseLevel::Off);
        let params = preset.to_params();
        assert!(!params.rnnoise);
        assert_eq!(params.denoise_level, DenoiseLevel::Off);
        assert_eq!(params.denoise_control, DenoiseLevel::Off.control());
    }

    #[test]
    fn an_absent_denoise_table_is_off_and_a_present_one_is_on() {
        // The doctrine, applied to the new table: absent is off, and present without a spelled
        // level is the default level, not off. The switch is implied either way.
        let absent = parse(&minimal(""));
        assert_eq!(absent.denoise_level(), DenoiseLevel::Off);
        assert!(!absent.to_params().rnnoise);

        let present = parse(&minimal("[denoise]\nchannels = \"linked\""));
        assert_eq!(present.denoise_level(), DenoiseLevel::Medium);
        assert_eq!(present.denoise_channels(), DenoiseChannelMode::Linked);
        assert!(present.to_params().rnnoise);
    }

    #[test]
    fn the_denoise_table_wins_over_the_switch() {
        // The two can disagree in a hand-edited file. The table is the one that says what the
        // stage does; the switch is there for a 0.3.0 binary.
        let strong = parse(&minimal("rnnoise = false\n[denoise]\nlevel = \"strong\""));
        assert_eq!(strong.denoise_level(), DenoiseLevel::Strong);
        let params = strong.to_params();
        assert!(params.rnnoise, "the table switched the stage on");
        assert_eq!(params.denoise_level, DenoiseLevel::Strong);
        assert_eq!(params.denoise_control, DenoiseLevel::Strong.control());

        let off = parse(&minimal("rnnoise = true\n[denoise]\nlevel = \"off\""));
        assert_eq!(off.denoise_level(), DenoiseLevel::Off);
        let params = off.to_params();
        assert!(!params.rnnoise, "the table switched the stage off");
        assert_eq!(params.denoise_level, DenoiseLevel::Off);
    }

    #[test]
    fn the_control_surface_is_the_levels_row_unless_the_table_overrides_it() {
        let plain = parse(&minimal("[denoise]\nlevel = \"light\""));
        assert_eq!(
            plain.to_params().denoise_control,
            DenoiseLevel::Light.control()
        );

        let edited = parse(&minimal(
            "[denoise]\nlevel = \"light\"\nvad_threshold = 0.4\nwet_dry = 0.5",
        ));
        let control = edited.to_params().denoise_control;
        let row = DenoiseLevel::Light.control();
        assert_eq!(control.vad_threshold, 0.4);
        assert_eq!(control.wet_dry, 0.5);
        assert_eq!(control.max_suppression_db, row.max_suppression_db);
        assert_eq!(control.voice_preservation, row.voice_preservation);
    }

    #[test]
    fn saving_writes_the_switch_the_table_implies() {
        // On disk the two spellings must agree, whatever the struct held: a 0.3.0 binary reads
        // only the switch.
        let dir = tempdir("switch");
        let mut preset = sample();
        preset.rnnoise = false;
        preset.denoise = Some(Denoise {
            level: DenoiseLevel::Strong,
            ..Denoise::default()
        });
        let path = dir.join("strong.toml");
        preset.save(&path).expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("rnnoise = true"), "{text}");
        assert!(InputPreset::load(&path).expect("load").rnnoise);

        preset.rnnoise = true;
        preset.denoise = Some(Denoise {
            level: DenoiseLevel::Off,
            ..Denoise::default()
        });
        let path = dir.join("off.toml");
        preset.save(&path).expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("rnnoise = false"), "{text}");

        preset.denoise = None;
        preset.rnnoise = true;
        let path = dir.join("legacy.toml");
        preset.save(&path).expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            text.contains("rnnoise = true") && !text.contains("[denoise]"),
            "a preset with no table is written with no table:\n{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_de_esser_without_a_mode_is_classic() {
        let classic = parse(&minimal(
            "[deesser]\nfrequency_hz = 5500.0\nthreshold_db = -22.0",
        ));
        assert_eq!(
            classic.deesser.as_ref().map(|d| d.mode),
            Some(DeEsserMode::Classic)
        );
        assert_eq!(classic.to_params().deesser_mode, DeEsserMode::Classic);

        let adaptive = parse(&minimal(
            "[deesser]\nfrequency_hz = 5500.0\nthreshold_db = -22.0\nmode = \"adaptive\"",
        ));
        assert_eq!(adaptive.to_params().deesser_mode, DeEsserMode::Adaptive);

        let none = parse(&minimal(""));
        assert_eq!(
            none.to_params().deesser_mode,
            DeEsserMode::Classic,
            "no de-esser at all is the default mode with the stage off"
        );
    }

    #[test]
    fn a_dereverb_table_names_its_level() {
        let absent = parse(&minimal(""));
        assert_eq!(absent.to_params().dereverb, DereverbLevel::Off);

        let medium = parse(&minimal("[dereverb]\nlevel = \"medium\""));
        assert_eq!(medium.to_params().dereverb, DereverbLevel::Medium);

        // A table with no level would read as `Off`, and a table that means off is the one thing
        // the format does not allow.
        let unsaid: Result<InputPreset, _> = toml::from_str(&minimal("[dereverb]"));
        assert!(unsaid.is_err(), "a [dereverb] table with no level parsed");
    }

    #[test]
    fn the_chain_and_the_vad_gate_default_and_read_through() {
        let unsaid = parse(&minimal(""));
        assert_eq!(unsaid.chain, DEFAULT_CHAIN);
        assert!(!unsaid.vad_gate);
        assert!(!unsaid.to_params().vad_gate);

        let said = parse(&minimal("chain = \"broadcast\"\nvad_gate = true"));
        assert_eq!(said.chain, "broadcast");
        assert!(said.vad_gate);
        assert!(said.to_params().vad_gate);

        let text = toml::to_string_pretty(&said).expect("serialise");
        assert!(text.contains("chain = \"broadcast\""), "{text}");
        assert!(text.contains("vad_gate = true"), "{text}");
        assert!(CHAIN_NAMES.contains(&DEFAULT_CHAIN));
    }

    #[test]
    fn a_chain_this_version_does_not_know_still_loads() {
        // Forward compatibility: the name is data, and refusing it here would stop a later
        // version's preset opening in this one.
        let later = parse(&minimal("chain = \"karaoke\""));
        assert_eq!(later.chain, "karaoke");
    }

    #[test]
    fn unknown_keys_are_ignored_at_every_level() {
        // What lets a 0.4.0 file load on a 0.3.0 binary, and the next version's file on this one.
        // `deny_unknown_fields` must never appear on these types.
        let preset = parse(&minimal(
            "sparkle = 3\n[denoise]\nlevel = \"light\"\nflavour = \"mint\"\n\
             [gate]\nthreshold_db = -40.0\nratio = 2.0\nrange_db = -12.0\nattack_ms = 5.0\n\
             release_ms = 100.0\nhold_ms = 50.0\ndetection = \"rms\"\nfuture = true\n\
             [something_new]\nkey = 1",
        ));
        assert_eq!(preset.denoise_level(), DenoiseLevel::Light);
        assert_eq!(preset.gate.as_ref().map(|g| g.threshold_db), Some(-40.0));
    }

    #[test]
    fn the_default_preset_is_the_shipped_clean_voice() {
        // The one preset the rest are voiced against, and the numbers `..Default::default()`
        // hands a struct literal. If the file and the default drift apart, one of them is wrong.
        let file = InputPreset::load(&shipped_dir().join("Clean Voice.toml")).expect("load");
        assert_eq!(file, InputPreset::default());
        assert_eq!(
            InputPreset::default().to_params().makeup_db,
            InputDspParams::default().makeup_db
        );
    }

    #[test]
    fn a_file_whose_band_tables_disagree_is_refused() {
        let dir = tempdir("mismatch");
        let path = dir.join("broken.toml");
        let mut preset = sample();
        preset.eq.gains_db.pop();
        std::fs::write(&path, toml::to_string_pretty(&preset).expect("serialise")).expect("write");

        let error = InputPreset::load(&path).expect_err("nine gains for ten centres");
        assert!(matches!(error, Error::Mismatched { .. }), "{error}");
        assert!(error.to_string().contains("broken.toml"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_parse_error_names_the_file() {
        let dir = tempdir("named");
        let path = dir.join("typo.toml");
        std::fs::write(&path, "name = \"Typo\"\nhighpass_hz = \"eighty\"\n").expect("write");
        let error = InputPreset::load(&path).expect_err("a string for a float");
        assert!(matches!(error, Error::Toml { .. }), "{error}");
        assert!(error.to_string().contains("typo.toml"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_unreadable_file_does_not_hide_the_rest_of_the_directory() {
        // The 0.3.0 bug: `load_dir` gave up at the first bad file, and `load_shipped` then fell
        // through to the next directory, so one typo in /usr/share removed all ten presets.
        let dir = tempdir("tolerant");
        InputPreset {
            name: "Good".to_owned(),
            ..InputPreset::default()
        }
        .save(&dir.join("good.toml"))
        .expect("write");
        std::fs::write(
            dir.join("bad.toml"),
            "name = \"Bad\"\nhighpass_hz = \"no\"\n",
        )
        .expect("write");
        std::fs::write(dir.join("notes.txt"), "not a preset").expect("write");

        let presets = InputPreset::load_dir(&dir).expect("the directory reads");
        let names: Vec<&str> = presets.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Good"]);

        let missing = InputPreset::load_dir(&dir.join("absent"));
        assert!(
            matches!(&missing, Err(Error::Io(err)) if err.kind() == std::io::ErrorKind::NotFound),
            "{missing:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_dir_lists_by_name_whatever_the_filenames_are() {
        let dir = tempdir("order");
        for (file, name) in [("z.toml", "Alpha"), ("a.toml", "Zulu"), ("m.toml", "Mike")] {
            InputPreset {
                name: name.to_owned(),
                ..InputPreset::default()
            }
            .save(&dir.join(file))
            .expect("write");
        }
        let names: Vec<String> = InputPreset::load_dir(&dir)
            .expect("read")
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["Alpha", "Mike", "Zulu"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_shipped_preset_keeps_the_switch_in_step_with_its_table() {
        // The compatibility invariant of the set: a 0.3.0 binary reads `rnnoise`, a 0.4.0 binary
        // reads the table, and both must hear the same stage. And the doctrine: a table that is
        // present is a stage that is on, so no shipped file spells `off` inside one.
        for preset in shipped() {
            assert_eq!(
                preset.rnnoise,
                preset.denoise_level() != DenoiseLevel::Off,
                "{}: rnnoise = {} disagrees with its [denoise] table",
                preset.name,
                preset.rnnoise
            );
            if let Some(denoise) = &preset.denoise {
                assert_ne!(
                    denoise.level,
                    DenoiseLevel::Off,
                    "{}: a [denoise] table that means off",
                    preset.name
                );
            }
            if let Some(dereverb) = &preset.dereverb {
                assert_ne!(
                    dereverb.level,
                    DereverbLevel::Off,
                    "{}: a [dereverb] table that means off",
                    preset.name
                );
            }
            assert!(
                CHAIN_NAMES.contains(&preset.chain.as_str()),
                "{}: chain {:?} is not one the engine builds",
                preset.name,
                preset.chain
            );
        }
    }

    #[test]
    fn the_shipped_presets_that_denoise_say_how() {
        // The 0.4.0 additions to the set, as the design record lists them. Every preset that
        // switches the denoiser on carries a table saying which row, so nothing in the shipped
        // set relies on the compatibility mapping.
        let presets = shipped();
        let find = |name: &str| {
            presets
                .iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("{name} is not in the shipped set"))
        };
        let expect = |name: &str, level: DenoiseLevel, channels: DenoiseChannelMode, vad: bool| {
            let preset = find(name);
            let denoise = preset
                .denoise
                .as_ref()
                .unwrap_or_else(|| panic!("{name} has no [denoise] table"));
            assert_eq!(denoise.level, level, "{name}");
            assert_eq!(denoise.channels, channels, "{name}");
            assert_eq!(preset.vad_gate, vad, "{name}: vad_gate");
            assert!(preset.rnnoise, "{name}: rnnoise");
        };
        use DenoiseChannelMode::{Independent, Linked, Mono};
        use DenoiseLevel::{Light, Medium, Strong};
        expect("Laptop Mic", Medium, Independent, false);
        expect("Streaming", Light, Independent, false);
        expect("Podcast", Light, Independent, false);
        expect("Gaming Headset", Medium, Linked, true);
        expect("Noisy Room", Strong, Mono, true);
        expect("Mechanical Keyboard", Strong, Independent, true);

        for preset in &presets {
            if preset.rnnoise {
                assert!(
                    preset.denoise.is_some(),
                    "{}: denoises without saying how",
                    preset.name
                );
            }
        }
    }
}
