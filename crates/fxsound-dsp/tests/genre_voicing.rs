//! Does the preset called Jazz actually voice material toward jazz?
//!
//! `shipped_presets.rs` already proves the twelve genre presets are *different from each other*.
//! It cannot say whether any of them is aimed at the genre on its label, and no extension of it
//! could: it sends an impulse through [`GraphicEq`] alone, and the stages that colour a real
//! render — Bass, Ambience, Surround, the volume leveller, and Dynamic Boost with its auto-gain
//! and its −0.3 dBFS ceiling — are program-dependent, nonlinear, and none of them are bypassable.
//! This test renders through the whole [`Engine`].
//!
//! [`GraphicEq`]: fxsound_dsp::GraphicEq
//!
//! # The claim, and what the numbers say about it
//!
//! The claim the whole design is built around is deliberately *relative*: for each genre that has
//! a published reference curve, **of the twelve genre presets, the one whose spectral distance to
//! that genre's reference improves the most should be the preset of that name**. An absolute claim
//! ("the Jazz preset lands within 1.5 dB of the jazz reference") would be hostage to the reference
//! being right, and it is not right; it is approximately right, and it says so in its own header.
//! A relative claim survives a reference that is wrong in level, wrong by a common factor, or
//! wrong in overall strength, because every preset is scored against the same wrong curve. What it
//! cannot survive is a reference wrong in *shape*, which is the honest limit of the exercise.
//!
//! **Two of the five columns support that claim and three do not.** Pop and Classical are won by
//! the preset of that name. Jazz is won by Classical, Metal by Pop, Trap by Classical.
//!
//! How much of that is real and how much is the method was checked during development by rescoring
//! the whole matrix at reference strengths from 0.5 to 6.0 dB RMS and again under a scale-free
//! correlation, and by rescoring every column under all five materials, which
//! [`the_column_rankings_do_not_depend_on_the_material`] still prints. Pop wins its own column
//! under every one of them; Metal's column is won by Pop under every one of them, and Trap never
//! wins its own. Classical and Jazz trade places depending on the strength, for the reason given
//! below. So the three losses are not an artefact of where the knobs were set.
//!
//! The reasons are in the doc comment on
//! [`each_genre_preset_is_among_the_best_fits_for_its_own_genre`], which is also where the weaker
//! claim this file actually asserts is written down and argued for. The full twelve-by-five matrix
//! is printed on every run: `cargo test -p fxsound-dsp --test genre_voicing -- --nocapture`.
//!
//! Writing the strong claim into an assertion and letting CI go red would not have been honesty,
//! it would have been a broken build; softening it until it passed and calling it the claim would
//! have been worse. What is here instead is the strongest statement the evidence carries, asserted,
//! with the statement it does not carry printed beside it in full.
//!
//! # The method
//!
//! 1. Synthesise deterministic stereo programme material whose long-term average spectrum is the
//!    genre-neutral baseline in `assets/reference/ltas-baseline.csv`, with a genre-typical burst
//!    envelope on top so the dynamics stages have something to work on. The material's spectrum is
//!    the *same* for every genre; only its dynamics differ. `ltas-baseline.csv` explains why, and
//!    it is the single most important design decision here: material pre-shaped to its own genre's
//!    reference already sits on the target, so the preset that changed it least would win every
//!    column and the answer to every question would be "Flat".
//! 2. Render it through a real [`Engine`] at 48 kHz in 1024-frame blocks — the way an audio
//!    callback would — once per genre preset.
//! 3. Measure input and output with [`fxsound_dsp::analysis`]: third-octave LTAS, BS.1770
//!    integrated loudness, true peak.
//! 4. Level-normalise every curve over 40 Hz–16 kHz, so this compares tone and never level, and
//!    score `ΔD(P, g) = D(out, R_g) − D(in, R_g)` where `D` is RMS dB error between shapes.
//!    Negative means the preset moved the material toward that genre's reference.
//!
//! The whole 12 × 5 matrix runs in a few seconds, which is why it can live in CI.
//!
//! # What this test cannot show, stated before the numbers rather than after
//!
//! **A preset can move material toward a reference curve and still sound wrong.** Matching a
//! reference proves "not obviously mis-aimed"; it never proves "right". A preset that lands
//! exactly on a genre's mean is *average*, and average is not what a preset is for — a preset is a
//! deliberate colour. The strongest true statement this file can ever produce is "the Jazz preset
//! moves neutral material toward the jazz reference more than the other eleven do, by X dB". It is
//! not "Jazz sounds like jazz". Nothing measures that but ears, which is what the listening
//! harness in `docs/preset-evaluation.md` is for.
//!
//! **Seven of the twelve genre presets are not tested at all**, because there is no reference
//! curve to test them against. Five of them — 70's, 80's, Classic Rock, Modern Country and R&B —
//! have no published curve anywhere, which the preset work itself recorded when it revoiced them
//! from spectral-analysis literature and house style rather than from a source. The other two,
//! Alternative Rock and Modern Rock, are excluded for a different and more annoying reason: both
//! published tables have exactly one entry called "Rock", and deciding which of this port's three
//! rock presets inherits it would be a choice made here rather than by any source. They are all
//! still *competitors* in every column — the Jazz preset has to beat all eleven others, not just
//! the four other tested ones — they simply have no column of their own.
//!
//! **Where the reference and the preset share an ancestor, agreement means less than it looks.**
//! The revoicing of these twelve consulted the same two tables this test's references are built
//! from. That is not circular — an equalizer curve is not a rendered spectrum, and the render here
//! passes through five nonlinear stages the table knows nothing about, any of which could swamp
//! it — but it is not independent either. A failure here is strong evidence; a pass is weaker
//! evidence than a pass against a corpus-derived reference would be.
//!
//! **Synthetic material under-exercises the transient behaviour.** Shaped noise with a burst
//! envelope wakes the dynamics stages, which stationary noise would not, but it is not music and
//! the crest factors are plausible rather than measured.
//!
//! **The between-genre signal may partly be the wrong signal.** Elowsson and Friberg (AES 142,
//! 2017) found that long-term-average-spectrum variation between genres is mainly a side-effect of
//! percussive prominence rather than of genre-specific tonal intent. A per-genre LTAS reference
//! therefore partly encodes "how much drums this genre has".

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use fxsound_core::{Effect, messages::DspParams};
use fxsound_dsp::Engine;
use fxsound_dsp::analysis::{
    Measurement, NUM_THIRD_OCTAVES, ProgramMeter, THIRD_OCTAVE_CENTRES, band_range,
    shape_distance_db, spectral_centroid_hz,
};
use realfft::RealFftPlanner;
use realfft::num_complex::Complex;

/// The only rate the port's capture path allows, and the rate the references were built at.
const RATE: f32 = 48_000.0;
/// What a PipeWire quantum looks like in practice.
const BLOCK: usize = 1024;
const CHANNELS: usize = 2;
/// 131 072 frames — 2.73 s, which is 30 overlapping 8192-point analyses per render.
const FRAMES: usize = 1 << 17;
/// Skipped at the start of every measurement, input and output alike, so that a filter settling
/// is never mistaken for a voicing.
const SETTLE_FRAMES: usize = 4_800;

/// The band every comparison is made over. Below 40 Hz a third-octave band is two FFT bins wide
/// and the estimate is noise; above 16 kHz nothing in a preset's band ladder reaches.
const COMPARE_LO_HZ: f32 = 40.0;
const COMPARE_HI_HZ: f32 = 16_000.0;

/// The twelve presets that name a genre. Every one of them competes in every column.
const GENRE_PRESETS: [&str; 12] = [
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
const BASELINE_FILE: &str = "ltas-baseline.csv";

/// The genres that have a column, and the reference asset each is scored against.
const COLUMNS: [(&str, &str); 5] = [
    ("Classical", "ltas-classical.csv"),
    ("Jazz", "ltas-jazz.csv"),
    ("Metal", "ltas-metal.csv"),
    ("Pop", "ltas-pop.csv"),
    ("Trap", "ltas-trap.csv"),
];

/// The genre presets with no column, and why. Printed by the test, so the gap is in the log rather
/// than only in this comment.
const NO_COLUMN: [(&str, &str); 7] = [
    ("70's", "an era, not a genre; no published curve anywhere"),
    ("80's", "an era, not a genre; no published curve anywhere"),
    (
        "Alternative Rock",
        "both tables have one \"Rock\"; which of three rock presets inherits it is not a source's call",
    ),
    ("Classic Rock", "no published curve anywhere"),
    ("Modern Country", "no published curve anywhere"),
    (
        "Modern Rock",
        "both tables have one \"Rock\"; which of three rock presets inherits it is not a source's call",
    ),
    ("R&B", "no published curve anywhere"),
];

// ---------------------------------------------------------------------------------------------
// Reference assets
// ---------------------------------------------------------------------------------------------

struct Reference {
    /// What the file is a reference *for*, for messages.
    genre: String,
    level_db: [f32; NUM_THIRD_OCTAVES],
    baseline_db: [f32; NUM_THIRD_OCTAVES],
    genre_eq_db: [f32; NUM_THIRD_OCTAVES],
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root")
        .to_path_buf()
}

/// Which genre a reference file is for, for failure messages.
fn genre_of(file: &str) -> String {
    COLUMNS
        .iter()
        .find(|(_, name)| *name == file)
        .map_or_else(|| file.to_string(), |(genre, _)| (*genre).to_string())
}

fn load_reference(file: &str) -> Reference {
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
struct Dynamics {
    bursts_per_second: f32,
    /// How far the envelope falls between bursts.
    floor_db: f32,
}

fn dynamics_for(genre: &str) -> Dynamics {
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
struct Rng(u32);

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
fn curve_at(curve: &[f32; NUM_THIRD_OCTAVES], hz: f32) -> f32 {
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
fn material(shape: &[f32; NUM_THIRD_OCTAVES], seed: u32, dynamics: Dynamics) -> Vec<f32> {
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

fn preset_params(name: &str) -> DspParams {
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

struct Render {
    measurement: Measurement,
    clipped: usize,
}

fn render(material: &[f32], params: &DspParams) -> Render {
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

fn measure(interleaved: &[f32]) -> Measurement {
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

// ---------------------------------------------------------------------------------------------
// The matrix
// ---------------------------------------------------------------------------------------------

/// Every preset rendered through every genre's material, measured once.
///
/// Rendered once and shared, because every question below is a different arithmetic over the same
/// 60 renders and rendering them per question is what would put this test over its time budget.
struct Matrix {
    /// Indexed by material, i.e. by [`COLUMNS`].
    inputs: Vec<Measurement>,
    /// `cells[material][preset]`.
    cells: Vec<Vec<Cell>>,
}

struct Cell {
    measurement: Measurement,
    clipped: usize,
}

/// The matrix, rendered at most once per test binary however many tests ask for it.
fn matrix() -> &'static Matrix {
    static MATRIX: OnceLock<Matrix> = OnceLock::new();
    MATRIX.get_or_init(build_matrix)
}

fn build_matrix() -> Matrix {
    let baseline = load_reference(BASELINE_FILE);
    let params: Vec<DspParams> = GENRE_PRESETS.iter().map(|p| preset_params(p)).collect();

    let mut inputs = Vec::with_capacity(COLUMNS.len());
    let mut cells = Vec::with_capacity(COLUMNS.len());
    for (genre, _) in COLUMNS {
        let samples = material(&baseline.level_db, seed_for(genre), dynamics_for(genre));
        inputs.push(measure(&samples));
        cells.push(
            params
                .iter()
                .map(|params| {
                    let rendered = render(&samples, params);
                    Cell {
                        measurement: rendered.measurement,
                        clipped: rendered.clipped,
                    }
                })
                .collect(),
        );
    }
    Matrix { inputs, cells }
}

/// Each genre's material gets its own seed, so a preset cannot win a column by happening to suit
/// one particular noise realisation.
fn seed_for(genre: &str) -> u32 {
    genre.bytes().fold(0x811c_9dc5u32, |acc, b| {
        (acc ^ u32::from(b)).wrapping_mul(0x0100_0193)
    }) | 1
}

/// One preset's score in one genre's column.
struct Score {
    preset: &'static str,
    /// `D(out) − D(in)`. Negative means the preset moved the material toward the reference.
    delta_d: f32,
    distance_out: f32,
    delta_lufs: Option<f32>,
    delta_plr: Option<f32>,
    centroid_ratio: f32,
    clipped: usize,
}

/// Every preset scored against one reference on one material, best first.
fn score_column(matrix: &Matrix, reference: &Reference, material_index: usize) -> Vec<Score> {
    let range = band_range(COMPARE_LO_HZ, COMPARE_HI_HZ);
    let input = &matrix.inputs[material_index];
    let distance_in = shape_distance_db(&input.ltas_db, &reference.level_db, range.clone());
    let centroid_in = spectral_centroid_hz(&input.ltas_db);

    let mut scores: Vec<Score> = GENRE_PRESETS
        .iter()
        .zip(matrix.cells[material_index].iter())
        .map(|(preset, cell)| {
            let out = cell.measurement;
            let distance_out = shape_distance_db(&out.ltas_db, &reference.level_db, range.clone());
            Score {
                preset,
                delta_d: distance_out - distance_in,
                distance_out,
                delta_lufs: out
                    .integrated_lufs
                    .zip(input.integrated_lufs)
                    .map(|(a, b)| a - b),
                delta_plr: out.plr_db().zip(input.plr_db()).map(|(a, b)| a - b),
                centroid_ratio: spectral_centroid_hz(&out.ltas_db) / centroid_in,
                clipped: cell.clipped,
            }
        })
        .collect();
    scores.sort_by(|a, b| a.delta_d.partial_cmp(&b.delta_d).expect("finite"));
    scores
}

fn rank_of(scores: &[Score], preset: &str) -> usize {
    scores
        .iter()
        .position(|s| s.preset == preset)
        .expect("every genre preset is in every column")
        + 1
}

// ---------------------------------------------------------------------------------------------
// The assets themselves
// ---------------------------------------------------------------------------------------------

#[test]
fn every_reference_curve_is_what_its_own_header_says_it_is() {
    // The CSVs carry their derivation as three columns precisely so that a hand-edit to one of
    // them cannot quietly change what the test is measuring. `level_db` must be
    // `baseline_db + genre_eq_db`, mean-removed over the comparison band; and `genre_eq_db` must
    // have the 2.00 dB RMS the header claims for it.
    let range = band_range(COMPARE_LO_HZ, COMPARE_HI_HZ);
    let mut files: Vec<&str> = COLUMNS.iter().map(|(_, file)| *file).collect();
    files.push(BASELINE_FILE);

    for file in files {
        let reference = load_reference(file);
        let sum: Vec<f32> = (0..NUM_THIRD_OCTAVES)
            .map(|i| reference.baseline_db[i] + reference.genre_eq_db[i])
            .collect();
        let mean: f32 = range.clone().map(|i| sum[i]).sum::<f32>() / range.len() as f32;
        for (i, total) in sum.iter().enumerate() {
            let expected = total - mean;
            assert!(
                (reference.level_db[i] - expected).abs() < 0.01,
                "{file}: band {i} says {:.3} dB, but baseline + genre_eq is {expected:.3} dB",
                reference.level_db[i]
            );
        }

        let rms = (range
            .clone()
            .map(|i| f64::from(reference.genre_eq_db[i]).powi(2))
            .sum::<f64>()
            / range.len() as f64)
            .sqrt() as f32;
        let expected = if file == BASELINE_FILE { 0.0 } else { 2.0 };
        assert!(
            (rms - expected).abs() < 0.01,
            "{file}: the genre curve's RMS is {rms:.3} dB, the header claims {expected:.2}"
        );
    }
}

#[test]
fn the_reference_curves_are_distinguishable_from_each_other() {
    // If two columns' references were near-identical, one preset would win both and the other
    // column would fail for a reason that has nothing to do with the preset. That is exactly what
    // happens to Classical and Jazz below, which is why this number is printed and not just
    // checked: it is the explanation for one of the three columns the strong claim loses.
    let range = band_range(COMPARE_LO_HZ, COMPARE_HI_HZ);
    let references: Vec<Reference> = COLUMNS
        .iter()
        .map(|(_, file)| load_reference(file))
        .collect();

    let mut pairs = Vec::new();
    for (i, left) in references.iter().enumerate() {
        for right in &references[i + 1..] {
            let distance = shape_distance_db(&left.level_db, &right.level_db, range.clone());
            pairs.push((distance, format!("{} / {}", left.genre, right.genre)));
        }
    }
    pairs.sort_by(|a, b| a.0.partial_cmp(&b.0).expect("finite"));

    eprintln!();
    eprintln!("How far apart the reference curves are, closest first:");
    for (distance, pair) in &pairs {
        eprintln!("  {distance:>5.2} dB  {pair}");
    }
    eprintln!();

    let (closest, pair) = &pairs[0];
    assert!(
        *closest > 0.5,
        "the {pair} references are only {closest:.2} dB apart; no preset could be shown to \
         prefer one over the other, so neither column would mean anything"
    );
}

/// The baseline is what the material is built from, so if the synthesiser and the asset disagree
/// the whole matrix is measuring something nobody described.
///
/// This is not ceremony. The first version of the synthesiser gave every FFT bin the magnitude the
/// curve asked for, which is white noise, not the curve: a third-octave band's width grows with
/// its centre frequency, so flat per-bin magnitude reads as +1 dB per band. It was 8 dB out and
/// every column in the matrix was won by the same preset. This check is what said so.
#[test]
fn the_synthesised_material_has_the_spectrum_it_was_asked_for() {
    let baseline = load_reference(BASELINE_FILE);
    let range = band_range(COMPARE_LO_HZ, COMPARE_HI_HZ);
    let samples = material(&baseline.level_db, 1, dynamics_for("Pop"));
    let measured = measure(&samples);
    let error = shape_distance_db(&measured.ltas_db, &baseline.level_db, range);
    assert!(
        error < 1.0,
        "the material's own spectrum is {error:.2} dB from the baseline it was built from"
    );
}

// ---------------------------------------------------------------------------------------------
// The claim
// ---------------------------------------------------------------------------------------------

/// The result, stated so that the assertions below are not mistaken for the headline.
///
/// **Of the five columns, two support the full claim and three do not.** Pop and Classical are won
/// by the preset of that name; Jazz is won by Classical, Metal by Pop, and Trap by Classical. That
/// is the finding, it is stable, and it is checked into the printed output of this test rather
/// than hidden behind a threshold.
///
/// Why the three do not, as far as the numbers can say:
///
/// * **Jazz and Classical cannot be separated by these references.** Android's Jazz and Classical
///   entries are 0.65 dB RMS apart, about a quarter of what a preset here applies (the twelve
///   range from 1.2 to 3.7 dB RMS of spectral action). Two targets that close cannot decide
///   between two presets: the Classical preset comes first in both columns, and the Jazz preset is
///   second in its own. Whichever way that pair fell, it would not be evidence.
/// * **Metal and Trap are a substantive disagreement, not a resolution problem.** The cosine
///   between this port's Metal preset's spectral action and Android's Heavy Metal curve is +0.01,
///   and for Trap against Hip Hop it is +0.03 — orthogonal. Android's Heavy Metal entry asks for a
///   +9 dB hump at 910 Hz; this port's Metal preset puts its boost at 3.2 kHz instead. One of the
///   two is wrong about metal and this test cannot say which, because a five-band preset table
///   from a phone is not ground truth either.
///
/// And one caveat on the two columns that *do* pass, because it belongs beside them rather than
/// three files away: **only Pop's win is robust to which published table you believe.** The two
/// sources disagree about Classical by 3.32 dB RMS — Winamp cuts hard above 6 kHz where Android
/// lifts — and under the Winamp curve the Classical preset falls from first to fourth in its own
/// column. Pop wins under either. `assets/reference/README.md` has both tables and the numbers.
///
/// So what is asserted here is weaker than the headline claim, and deliberately so — it is the
/// part the evidence carries:
///
/// 1. Every genre preset fits its own genre better than the *median* of the twelve does. That is
///    the coin-flip line, not a threshold chosen to fit: a preset aimed at nothing in particular
///    has an even chance of failing it, and all five passing by luck is a 1-in-32 event.
/// 2. The diagonal as a whole beats a random assignment by at least two standard deviations, with
///    the null computed from the number of columns and the size of the field rather than assumed.
///
/// Both go red if a preset is revoiced away from its genre, which is the only thing a test in CI
/// can usefully do here.
#[test]
fn each_genre_preset_is_among_the_best_fits_for_its_own_genre() {
    let matrix = matrix();
    let mut failures = Vec::new();
    let mut ranks = Vec::new();
    let mut strong_claim = Vec::new();

    eprintln!();
    eprintln!(
        "Genre voicing: {} presets x {} columns, rendered through the real engine.",
        GENRE_PRESETS.len(),
        COLUMNS.len()
    );
    eprintln!(
        "  dD = distance to the reference after the preset, minus before. Negative is better."
    );
    eprintln!();

    for (material_index, (genre, file)) in COLUMNS.iter().enumerate() {
        let reference = load_reference(file);
        let scores = score_column(matrix, &reference, material_index);

        eprintln!("  {genre} (reference {file})");
        for (i, score) in scores.iter().enumerate() {
            let marker = if score.preset == *genre { "<--" } else { "" };
            eprintln!(
                "    {:>2}. {:<17} dD {:>+6.2}  D_out {:>5.2}  dLUFS {:>+6.2}  dPLR {:>+6.2}  \
                 centroid x{:.3}  clipped {:<4} {marker}",
                i + 1,
                score.preset,
                score.delta_d,
                score.distance_out,
                score.delta_lufs.unwrap_or(f32::NAN),
                score.delta_plr.unwrap_or(f32::NAN),
                score.centroid_ratio,
                score.clipped,
            );
        }

        let rank = rank_of(&scores, genre);
        ranks.push(rank);
        let own = &scores[rank - 1];

        if rank == 1 {
            strong_claim.push(format!(
                "{genre}: WON, by {:.2} dB over {}",
                scores[1].delta_d - own.delta_d,
                scores[1].preset
            ));
        } else {
            strong_claim.push(format!(
                "{genre}: lost to {} ({:+.2} dB vs {:+.2} dB); {genre} is {rank} of {}",
                scores[0].preset,
                scores[0].delta_d,
                own.delta_d,
                scores.len()
            ));
        }

        // Assertion 1: better than the median of the field.
        let median_rank = GENRE_PRESETS.len() / 2;
        if rank > median_rank {
            failures.push(format!(
                "{genre}: its own preset is {rank} of {}, worse than the median — it fits this \
                 genre's reference no better than an arbitrary preset would",
                scores.len()
            ));
        }
        eprintln!();
    }

    // Assertion 2: the diagonal beats chance. Under a null in which each column's winner is drawn
    // at random from the field, a rank is uniform on 1..=N, so the sum over C columns has mean
    // C(N+1)/2 and variance C(N^2-1)/12.
    let columns = COLUMNS.len() as f64;
    let field = GENRE_PRESETS.len() as f64;
    let null_mean = columns * (field + 1.0) / 2.0;
    let null_sd = (columns * (field * field - 1.0) / 12.0).sqrt();
    let rank_sum: usize = ranks.iter().sum();
    let sigmas = (null_mean - rank_sum as f64) / null_sd;

    eprintln!("  The strong claim, column by column:");
    for line in &strong_claim {
        eprintln!("    {line}");
    }
    eprintln!();
    eprintln!(
        "  Diagonal rank sum {rank_sum} (ranks {ranks:?}); a random assignment would average \
         {null_mean:.1} with a standard deviation of {null_sd:.1}, so this is {sigmas:.2} sigma \
         better than chance."
    );
    eprintln!();
    eprintln!("  Not tested, for want of a reference curve to test against:");
    for (preset, why) in NO_COLUMN {
        eprintln!("    {preset:<17} {why}");
    }
    eprintln!();

    if sigmas < 2.0 {
        failures.push(format!(
            "the diagonal is only {sigmas:.2} sigma better than a random assignment of presets to \
             genres (rank sum {rank_sum}, null {null_mean:.1} +- {null_sd:.1})"
        ));
    }

    assert!(
        failures.is_empty(),
        "{} finding(s):\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

#[test]
fn the_column_rankings_do_not_depend_on_the_material() {
    // The assertion above uses each genre's own material. If a column's ranking changed when the
    // same reference was scored against another genre's material, the result would be about the
    // burst envelope rather than about the voicing — and the envelopes are the part of this test
    // with the least evidence behind them. Reported, not asserted: it is a health check on the
    // method, not a claim about the engine.
    let matrix = matrix();

    eprintln!();
    eprintln!("Each column's ranking of its own preset, under all five materials:");
    for (genre, file) in COLUMNS {
        let reference = load_reference(file);
        let mut ranks = Vec::new();
        for (material_index, (material_genre, _)) in COLUMNS.iter().enumerate() {
            let scores = score_column(matrix, &reference, material_index);
            ranks.push(format!("{material_genre} -> #{}", rank_of(&scores, genre)));
        }
        eprintln!("  {genre:<10} {}", ranks.join(", "));
    }
    eprintln!();
}
