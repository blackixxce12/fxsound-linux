//! The output lane's test material, shared by `preset_drift.rs`, which measures what a preset
//! does to it, and `windows_parity_bitexact.rs`, which checks that two builds do the same to it.
//! Built from nothing in the engine, so it is the same material whichever engine is measured.
//!
//! `preset_drift.rs` describes each piece ("The material").

use super::genre_material::{
    BASELINE_FILE, CHANNELS, Dynamics, FRAMES, GENRE_PRESETS, RATE, dynamics_for, load_reference,
    material, seed_for,
};
use fxsound_dsp::analysis::NUM_THIRD_OCTAVES;

/// How many times each piece of music is played back to back; only the last pass is measured.
pub const PASSES: usize = 3;
/// The loud material's RMS, and the ceiling it is mastered against.
pub const LOUD_RMS_DBFS: f64 = -9.0;
pub const LOUD_CEILING: f32 = 0.977;
/// A modern master's movement: dense, shallow.
pub const LOUD_DYNAMICS: Dynamics = Dynamics {
    bursts_per_second: 4.0,
    floor_db: -6.0,
};
pub const TONE_DBFS: f32 = -1.0;
pub const TONES_HZ: [f32; 2] = [40.0, 80.0];
pub const UNBALANCED_DB: f32 = 10.0;
/// Volume Leveling at its maximum, `VolumeLeveller`'s own `MAX_AMOUNT`.
pub const LEVELING_AMOUNT: f32 = 4.0;
/// The leveller's audit case (#1): a bass tone well under full scale, which it lifts.
pub const LEVELED_TONE_HZ: f32 = 50.0;
pub const LEVELED_TONE_AMPLITUDE: f32 = 0.3;
/// How far under the genre material the voicing reference sits, and its name.
pub const QUIET_DB: f32 = -40.0;
pub const VOICING_REFERENCE: &str = "quiet/Pop";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Music,
    Tone,
    Unbalanced,
    Silence,
}

impl Kind {
    pub const fn key(self) -> &'static str {
        match self {
            Self::Music => "music",
            Self::Tone => "tone",
            Self::Unbalanced => "unbalanced",
            Self::Silence => "silence",
        }
    }

    pub fn from_key(key: &str) -> Self {
        match key {
            "tone" => Self::Tone,
            "unbalanced" => Self::Unbalanced,
            "silence" => Self::Silence,
            _ => Self::Music,
        }
    }
}

pub struct Material {
    pub name: String,
    pub kind: Kind,
    /// The fundamental, for a tone.
    pub tone_hz: f32,
    pub leveling: f32,
    pub channels: usize,
    /// Interleaved, the whole render.
    pub samples: Vec<f32>,
    /// The frame the measurement starts at.
    pub measure_from: usize,
}

pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

pub fn rms(samples: &[f32]) -> f64 {
    let sum: f64 = samples.iter().map(|s| f64::from(*s).powi(2)).sum();
    (sum / samples.len().max(1) as f64).sqrt()
}

/// One pass of music, played [`PASSES`] times, measured on the last.
pub fn music(name: &str, kind: Kind, pass: &[f32], leveling: f32) -> Material {
    Material {
        name: name.to_owned(),
        kind,
        tone_hz: 0.0,
        leveling,
        channels: CHANNELS,
        samples: pass.repeat(PASSES),
        measure_from: (PASSES - 1) * pass.len() / CHANNELS,
    }
}

/// A sine on both channels, faded in over 10 ms, `passes` material lengths long, measured on the
/// last of them.
pub fn tone(name: &str, hz: f32, amplitude: f32, passes: usize, leveling: f32) -> Material {
    let frames = FRAMES * passes;
    let fade = (0.01 * RATE) as usize;
    let mut samples = Vec::with_capacity(frames * CHANNELS);
    for n in 0..frames {
        let phase = std::f64::consts::TAU * f64::from(hz) * n as f64 / f64::from(RATE);
        let ramp = if n < fade {
            0.5 - 0.5 * (std::f64::consts::PI * n as f64 / fade as f64).cos()
        } else {
            1.0
        };
        let value = (phase.sin() * ramp * f64::from(amplitude)) as f32;
        samples.extend(std::iter::repeat_n(value, CHANNELS));
    }
    Material {
        name: name.to_owned(),
        kind: Kind::Tone,
        tone_hz: hz,
        leveling,
        channels: CHANNELS,
        samples,
        measure_from: frames - FRAMES,
    }
}

/// Raise `pass` into a hard ceiling until its RMS is [`LOUD_RMS_DBFS`]: the crudest mastering
/// there is, and deliberately built from nothing in the engine, so that it is the same material
/// whichever engine is being measured.
pub fn mastered(pass: &[f32]) -> Vec<f32> {
    let target = 10f64.powf(LOUD_RMS_DBFS / 20.0);
    let clip = |gain: f32| -> Vec<f32> {
        pass.iter()
            .map(|s| (s * gain).clamp(-LOUD_CEILING, LOUD_CEILING))
            .collect()
    };
    let (mut lo, mut hi) = (0.01f32, 100.0f32);
    for _ in 0..60 {
        let mid = (lo * hi).sqrt();
        if rms(&clip(mid)) < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    clip((lo * hi).sqrt())
}

pub fn output_materials() -> Vec<Material> {
    let baseline = load_reference(BASELINE_FILE);
    let mut out = Vec::new();
    for genre in GENRE_PRESETS {
        let pass = material(&baseline.level_db, seed_for(genre), dynamics_for(genre));
        out.push(music(&format!("genre/{genre}"), Kind::Music, &pass, 0.0));
    }

    let pink = [0.0f32; NUM_THIRD_OCTAVES];
    let loud = mastered(&material(&pink, seed_for("loud"), LOUD_DYNAMICS));
    out.push(music("loud", Kind::Music, &loud, 0.0));
    // The same master with one channel copied to the other. Real mixes sit between the two: the
    // genre and loud material's channels are independent noise, which is the worst case for a
    // limiter that turns both sides down together, and a mono mix is the best.
    let mono: Vec<f32> = loud
        .as_chunks::<CHANNELS>()
        .0
        .iter()
        .flat_map(|frame| [frame[1]; CHANNELS])
        .collect();
    out.push(music("loud/mono", Kind::Music, &mono, 0.0));

    for hz in TONES_HZ {
        out.push(tone(
            &format!("tone/{hz}Hz"),
            hz,
            db_to_gain(TONE_DBFS),
            PASSES,
            0.0,
        ));
    }

    let mut unbalanced = mastered(&material(&pink, seed_for("unbalanced"), LOUD_DYNAMICS));
    let quiet = db_to_gain(-UNBALANCED_DB);
    for frame in unbalanced.as_chunks_mut::<CHANNELS>().0 {
        frame[0] *= quiet;
    }
    out.push(music("unbalanced", Kind::Unbalanced, &unbalanced, 0.0));

    out.push(Material {
        name: "silence".to_owned(),
        kind: Kind::Silence,
        tone_hz: 0.0,
        leveling: 0.0,
        channels: CHANNELS,
        samples: vec![0.0; FRAMES * CHANNELS],
        measure_from: 0,
    });

    let classical = material(
        &baseline.level_db,
        seed_for("Classical"),
        dynamics_for("Classical"),
    );
    out.push(music(
        "leveling/Classical",
        Kind::Music,
        &classical,
        LEVELING_AMOUNT,
    ));
    // The Pop material 40 dB down, where nothing in the chain is working hard: the preset's own
    // voicing, which the comparison measures every other render's change of shape against.
    let quiet: Vec<f32> = material(&baseline.level_db, seed_for("Pop"), dynamics_for("Pop"))
        .iter()
        .map(|s| s * db_to_gain(QUIET_DB))
        .collect();
    out.push(music(VOICING_REFERENCE, Kind::Music, &quiet, 0.0));
    out.push(tone(
        "leveling/tone50Hz",
        LEVELED_TONE_HZ,
        LEVELED_TONE_AMPLITUDE,
        PASSES + 1,
        LEVELING_AMOUNT,
    ));
    out
}
