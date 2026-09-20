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

use fxsound_core::messages::InputDspParams;
use fxsound_core::{Detection, eq};
use serde::{Deserialize, Serialize};
use std::path::Path;

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
    #[serde(default)]
    pub rnnoise: bool,

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

/// What went wrong reading a voice preset.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Toml(#[from] toml::de::Error),
    #[error("could not write TOML: {0}")]
    Serialise(#[from] toml::ser::Error),
    #[error("{path}: the equalizer has {centers} centres and {gains} gains")]
    Mismatched {
        path: String,
        centers: usize,
        gains: usize,
    },
}

impl InputPreset {
    /// Read one preset from a TOML file.
    ///
    /// # Errors
    /// The file cannot be read, is not valid TOML, or its band tables disagree in length.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path)?;
        let preset: Self = toml::from_str(&text)?;
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
    /// # Errors
    /// The preset cannot be serialised, or the file cannot be replaced.
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        let text = toml::to_string_pretty(self)?;
        fxsound_core::atomic::write(path, text.as_bytes())?;
        Ok(())
    }

    /// Read every `*.toml` in a directory, sorted by name so a listing is stable.
    ///
    /// # Errors
    /// The directory cannot be read, or one of its presets cannot be.
    pub fn load_dir(dir: &Path) -> Result<Vec<Self>, Error> {
        let mut presets = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("toml"))
            {
                presets.push(Self::load(&path)?);
            }
        }
        presets.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(presets)
    }

    /// The parameter snapshot this preset describes.
    ///
    /// Deliberately *not* sanitised here: a preset that carries a number the engine would clamp is
    /// a preset with a mistake in it, and the test over the shipped set is what has to catch it.
    /// Sanitising on the way out would hide exactly the defect worth finding.
    #[must_use]
    pub fn to_params(&self) -> InputDspParams {
        let mut params = InputDspParams {
            power: true,
            rnnoise: self.rnnoise,
            highpass_hz: self.highpass_hz,
            highpass_order: self.highpass_order,
            makeup_db: self.makeup_db,
            ceiling_db: self.ceiling_db,
            gate_on: self.gate.is_some(),
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

    fn sample() -> InputPreset {
        InputPreset {
            name: "Clean Voice".to_owned(),
            description: "The reference the rest of the set was voiced against.".to_owned(),
            rnnoise: false,
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
            }),
            eq: Equalizer {
                centers_hz: eq::DEFAULT_CENTERS_HZ.to_vec(),
                gains_db: vec![0.0, 0.0, -1.0, -1.5, -0.5, 0.0, 1.5, 0.0, 0.0, 0.0],
            },
            makeup_db: 6.0,
            ceiling_db: -3.0,
        }
    }

    #[test]
    fn a_preset_round_trips_through_toml() {
        let preset = sample();
        let text = toml::to_string_pretty(&preset).expect("serialise");
        let back: InputPreset = toml::from_str(&text).expect("parse");
        assert_eq!(preset, back);
    }

    #[test]
    fn a_stage_that_is_off_is_an_absent_table_and_reads_back_as_off() {
        // The distinction between Flat and Clean Voice, and the reason it is an absence rather
        // than `enabled = false`: there is no way to write a preset carrying a full set of gate
        // numbers that nothing reads.
        let mut preset = sample();
        preset.name = "Flat".to_owned();
        preset.gate = None;
        preset.compressor = None;
        preset.deesser = None;

        let text = toml::to_string_pretty(&preset).expect("serialise");
        assert!(
            !text.contains("[gate]"),
            "an off stage wrote a table:\n{text}"
        );
        assert!(
            !text.contains("threshold_db"),
            "and it wrote its numbers:\n{text}"
        );

        let params = preset.to_params();
        assert!(!params.gate_on);
        assert!(!params.compressor_on);
        assert!(!params.deesser_on);
    }

    #[test]
    fn the_parameters_a_preset_describes_are_the_ones_it_carries() {
        let params = sample().to_params();
        assert_eq!(params.gate_threshold_db, -45.0);
        assert_eq!(params.gate_range_db, -14.0);
        assert_eq!(params.compressor_ratio, 3.0);
        assert_eq!(params.deesser_hz, 5_500.0);
        assert_eq!(params.makeup_db, 6.0);
        assert_eq!(params.num_bands, 10);
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
    fn a_file_whose_band_tables_disagree_is_refused() {
        let dir = std::env::temp_dir().join(format!("fxsound-input-preset-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create");
        let path = dir.join("broken.toml");
        let mut preset = sample();
        preset.eq.gains_db.pop();
        std::fs::write(&path, toml::to_string_pretty(&preset).expect("serialise")).expect("write");

        let error = InputPreset::load(&path).expect_err("nine gains for ten centres");
        assert!(matches!(error, Error::Mismatched { .. }), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
