//! The genre material and the render protocol `genre_voicing.rs` scores the genre presets with,
//! shared with `preset_drift.rs` so that the drift harness renders exactly the same seeded
//! material through exactly the same engine setup — a copy would drift on its own.
//!
//! Not a test target: Cargo only builds `tests/*.rs` and `tests/*/main.rs` as tests, so this
//! directory is a module each of those files pulls in with `mod genre_material;`.

use std::path::{Path, PathBuf};

use fxsound_core::{Effect, messages::DspParams};
use fxsound_dsp::Engine;
use fxsound_dsp::analysis::{Measurement, NUM_THIRD_OCTAVES, ProgramMeter, THIRD_OCTAVE_CENTRES};
use realfft::RealFftPlanner;
use realfft::num_complex::Complex;

/// The only rate the port's capture path allows, and the rate the references were built at.
pub const RATE: f32 = 48_000.0;
/// What a PipeWire quantum looks like in practice.
pub const BLOCK: usize = 1024;
pub const CHANNELS: usize = 2;
/// 131 072 frames — 2.73 s, which is 30 overlapping 8192-point analyses per render.
pub const FRAMES: usize = 1 << 17;
/// Skipped at the start of every measurement, input and output alike, so that a filter settling
/// is never mistaken for a voicing.
pub const SETTLE_FRAMES: usize = 4_800;

/// The band every comparison is made over. Below 40 Hz a third-octave band is two FFT bins wide
/// and the estimate is noise; above 16 kHz nothing in a preset's band ladder reaches.
pub const COMPARE_LO_HZ: f32 = 40.0;
pub const COMPARE_HI_HZ: f32 = 16_000.0;

/// The twelve presets that name a genre. Every one of them competes in every column.
pub const GENRE_PRESETS: [&str; 12] = [
    "70's",
    "80's",
    "Alternative Rock",
    "Classic Rock",
    "Classical",
    "Jazz",
    "Metal",
    "Modern Country",
    "Modern Rock",
    "Pop",
    "R&B",
    "Trap",
];

/// The genre-neutral baseline every material is synthesised from.
pub const BASELINE_FILE: &str = "ltas-baseline.csv";

/// The genres that have a column, and the reference asset each is scored against.
pub const COLUMNS: [(&str, &str); 5] = [
    ("Classical", "ltas-classical.csv"),
    ("Jazz", "ltas-jazz.csv"),
    ("Metal", "ltas-metal.csv"),
    ("Pop", "ltas-pop.csv"),
    ("Trap", "ltas-trap.csv"),
];

// ---------------------------------------------------------------------------------------------
// Reference assets
// ---------------------------------------------------------------------------------------------

pub struct Reference {
    /// What the file is a reference *for*, for messages.
    pub genre: String,
    pub level_db: [f32; NUM_THIRD_OCTAVES],
    pub baseline_db: [f32; NUM_THIRD_OCTAVES],
    pub genre_eq_db: [f32; NUM_THIRD_OCTAVES],
}

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root")
        .to_path_buf()
}

/// Which genre a reference file is for, for failure messages.
pub fn genre_of(file: &str) -> String {
    COLUMNS
        .iter()
        .find(|(_, name)| *name == file)
        .map_or_else(|| file.to_string(), |(genre, _)| (*genre).to_string())
}

pub fn load_reference(file: &str) -> Reference {
    let path = repo_root().join("assets/reference").join(file);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));

    let mut level_db = [0.0f32; NUM_THIRD_OCTAVES];
    let mut baseline_db = [0.0f32; NUM_THIRD_OCTAVES];
    let mut genre_eq_db = [0.0f32; NUM_THIRD_OCTAVES];
    let mut row = 0usize;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("center_hz") {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        assert_eq!(
            fields.len(),
            4,
            "{}: row {row} has {} fields, expected 4",
            path.display(),
            fields.len()
        );
        assert!(
            row < NUM_THIRD_OCTAVES,
            "{}: more than {NUM_THIRD_OCTAVES} rows",
            path.display()
        );

        let parse = |i: usize| -> f32 {
            fields[i]
                .parse()
                .unwrap_or_else(|err| panic!("{}: row {row} field {i}: {err}", path.display()))
        };
        let centre: f32 = parse(0);
        assert!(
            (centre - THIRD_OCTAVE_CENTRES[row]).abs() < 0.01,
            "{}: row {row} is {centre} Hz, expected {} Hz",
            path.display(),
            THIRD_OCTAVE_CENTRES[row]
        );
        level_db[row] = parse(1);
        baseline_db[row] = parse(2);
        genre_eq_db[row] = parse(3);
        row += 1;
    }
    assert_eq!(
        row,
        NUM_THIRD_OCTAVES,
        "{}: {row} rows, expected {NUM_THIRD_OCTAVES}",
        path.display()
    );

    Reference {
        genre: genre_of(file),
        level_db,
        baseline_db,
        genre_eq_db,
    }
}

// ---------------------------------------------------------------------------------------------
// Programme material
// ---------------------------------------------------------------------------------------------

/// How a genre's material moves, as distinct from how it sounds.
///
/// These numbers are *plausible*, not measured: nobody has published per-genre crest factors on a
/// grid this test could cite. They exist so that the dynamics stages are awake and so that they
/// are awake by different amounts, which is enough for the purpose — every preset sees the same
/// material within a column, so a wrong crest factor cannot favour one preset over another.
#[derive(Clone, Copy)]
pub struct Dynamics {
    pub bursts_per_second: f32,
    /// How far the envelope falls between bursts.
    pub floor_db: f32,
}

pub fn dynamics_for(genre: &str) -> Dynamics {
    match genre {
        // Wide dynamics, slow events.
        "Classical" => Dynamics {
            bursts_per_second: 1.5,
            floor_db: -26.0,
        },
        "Jazz" => Dynamics {
            bursts_per_second: 3.0,
            floor_db: -18.0,
        },
        // Heavily limited modern masters: dense, shallow.
        "Metal" => Dynamics {
            bursts_per_second: 8.0,
            floor_db: -7.0,
        },
        "Pop" => Dynamics {
            bursts_per_second: 4.0,
            floor_db: -9.0,
        },
        "Trap" => Dynamics {
            bursts_per_second: 2.5,
            floor_db: -12.0,
        },
        _ => Dynamics {
            bursts_per_second: 4.0,
            floor_db: -12.0,
        },
    }
}

/// A deterministic 32-bit xorshift. The material has to be bit-identical between runs, or a
/// failure cannot be reproduced.
pub struct Rng(u32);

impl Rng {
    fn next_u32(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }

    fn next_unit(&mut self) -> f32 {
        f64::from(self.next_u32()) as f32 / u32::MAX as f32
    }
}

/// The baseline curve read at an arbitrary frequency: linear in dB against log frequency between
/// the tabulated third-octave centres, held flat outside them.
pub fn curve_at(curve: &[f32; NUM_THIRD_OCTAVES], hz: f32) -> f32 {
    if hz <= THIRD_OCTAVE_CENTRES[0] {
        return curve[0];
    }
    let last = NUM_THIRD_OCTAVES - 1;
    if hz >= THIRD_OCTAVE_CENTRES[last] {
        return curve[last];
    }
    for i in 0..last {
        let (lo, hi) = (THIRD_OCTAVE_CENTRES[i], THIRD_OCTAVE_CENTRES[i + 1]);
        if hz >= lo && hz <= hi {
            let t = (hz.ln() - lo.ln()) / (hi.ln() - lo.ln());
            return curve[i] + t * (curve[i + 1] - curve[i]);
        }
    }
    curve[last]
}

/// Interleaved stereo material whose long-term average spectrum is `shape`.
///
/// Built in the frequency domain — every bin given the magnitude the curve asks for and a
/// pseudo-random phase, then one inverse transform — rather than by filtering noise. That way the
/// material's spectrum is exact and deterministic instead of being an estimate with its own
/// variance, and the test is measuring the engine rather than the noise generator.
pub fn material(shape: &[f32; NUM_THIRD_OCTAVES], seed: u32, dynamics: Dynamics) -> Vec<f32> {
    let mut planner = RealFftPlanner::<f32>::new();
    let ifft = planner.plan_fft_inverse(FRAMES);

    let mut channels: Vec<Vec<f32>> = Vec::with_capacity(CHANNELS);
    for channel in 0..CHANNELS {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9) ^ (channel as u32 + 1));
        let mut spectrum = ifft.make_input_vec();
        let bin_hz = RATE / FRAMES as f32;
        for (bin, value) in spectrum.iter_mut().enumerate() {
            if bin == 0 || bin == FRAMES / 2 {
                // Neither a DC offset nor a Nyquist-alternating component is programme material.
                *value = Complex::new(0.0, 0.0);
                continue;
            }
            let hz = bin as f32 * bin_hz;
            // Roll the extremes off steeply. The baseline is flat below 100 Hz by construction,
            // which taken literally would put full-level rumble at 6 Hz; and nothing above 20 kHz
            // is programme. Both lie outside the comparison band, so this changes no score.
            let skirt = if hz < 31.5 {
                -24.0 * (31.5 / hz).log2()
            } else if hz > 20_000.0 {
                -24.0 * (hz / 20_000.0).log2()
            } else {
                0.0
            };
            // `1 / sqrt(f)`, because the curve is a *band* level and a third-octave band's width
            // is proportional to its centre. Flat per-bin magnitude is white noise, which reads as
            // +1 dB per band; the pink tilt is what makes a flat curve come back flat.
            let pink = (1_000.0 / hz).sqrt();
            let magnitude = 10f32.powf((curve_at(shape, hz) + skirt) / 20.0) * pink;
            let phase = rng.next_unit() * std::f32::consts::TAU;
            *value = Complex::from_polar(magnitude, phase);
        }

        let mut samples = ifft.make_output_vec();
        let mut scratch = ifft.make_scratch_vec();
        ifft.process_with_scratch(&mut spectrum, &mut samples, &mut scratch)
            .expect("the synthesised spectrum transforms");
        channels.push(samples);
    }

    // The burst envelope: a 5 ms raised-cosine attack and an exponential decay, repeating. Applied
    // identically to both channels, because a genre's dynamics are a property of the mix, not of
    // one side of it.
    let period = (RATE / dynamics.bursts_per_second).max(1.0);
    let attack = (0.005 * RATE).min(period * 0.5);
    let floor = 10f32.powf(dynamics.floor_db / 20.0);
    let mut envelope = vec![0.0f32; FRAMES];
    for (frame, slot) in envelope.iter_mut().enumerate() {
        let phase = (frame as f32) % period;
        let shape = if phase < attack {
            0.5 - 0.5 * (std::f32::consts::PI * phase / attack).cos()
        } else {
            (-4.0 * (phase - attack) / (period - attack)).exp()
        };
        *slot = floor + (1.0 - floor) * shape;
    }

    let mut interleaved = vec![0.0f32; FRAMES * CHANNELS];
    for (frame, chunk) in interleaved
        .as_chunks_mut::<CHANNELS>()
        .0
        .iter_mut()
        .enumerate()
    {
        for (channel, slot) in chunk.iter_mut().enumerate() {
            *slot = channels[channel][frame] * envelope[frame];
        }
    }

    // Peak-normalise to −6 dBFS: loud enough that Dynamic Boost's level estimator is well clear of
    // its floor, quiet enough that the input itself never clips.
    let peak = interleaved
        .iter()
        .fold(0.0f32, |acc, sample| acc.max(sample.abs()));
    if peak > 0.0 {
        let gain = 0.5 / peak;
        for sample in &mut interleaved {
            *sample *= gain;
        }
    }
    interleaved
}

// ---------------------------------------------------------------------------------------------
// Rendering and measuring
// ---------------------------------------------------------------------------------------------

pub fn preset_params(name: &str) -> DspParams {
    let path = repo_root()
        .join("assets/presets/BonusPresets")
        .join(format!("{name}.fac"));
    let preset =
        fxsound_preset::load(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));

    let mut params = DspParams::default();
    for effect in Effect::ALL {
        params.set_effect(effect, preset.effect(effect));
    }
    params.set_bands(&preset.eq_bands);
    params.eq_on = preset.eq_on;
    params
}

pub struct Render {
    pub measurement: Measurement,
    pub clipped: usize,
}

pub fn render(material: &[f32], params: &DspParams) -> Render {
    let mut engine = Engine::new(RATE, BLOCK, CHANNELS);
    engine.apply(params);

    let mut buffer = material.to_vec();
    for block in buffer.chunks_mut(BLOCK * CHANNELS) {
        engine.process(block, CHANNELS);
    }
    let clipped = buffer.iter().filter(|s| s.abs() > 1.0).count();
    Render {
        measurement: measure(&buffer),
        clipped,
    }
}

pub fn measure(interleaved: &[f32]) -> Measurement {
    let mut meter = ProgramMeter::new(RATE, CHANNELS, 30.0);
    meter.push(&interleaved[SETTLE_FRAMES * CHANNELS..]);
    let measurement = meter.measurement();
    assert!(
        meter.ltas().analyses() > 0,
        "the render was too short to analyse"
    );
    assert_eq!(
        measurement.dropped_blocks, 0,
        "the loudness meter overran its history"
    );
    measurement
}

/// Each genre's material gets its own seed, so a preset cannot win a column by happening to suit
/// one particular noise realisation.
pub fn seed_for(genre: &str) -> u32 {
    genre.bytes().fold(0x811c_9dc5u32, |acc, b| {
        (acc ^ u32::from(b)).wrapping_mul(0x0100_0193)
    }) | 1
}
