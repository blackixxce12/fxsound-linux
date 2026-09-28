//! Did the shipped presets move when the engine under them changed?
//!
//! Every fix to the engine changes what some preset sounds like — the 0.4.0 audit of the Windows
//! defects the port had copied changed the leveller, Dynamic Boost, its limiter, and the way every
//! control glides — and whether a preset got *worse* is a question about the preset, not the fix.
//! It is answered by rendering each preset through both engines on the same material and
//! comparing what comes out. This file is the measuring half: it renders every shipped preset (the
//! `.fac` files in `Factsoft/` and `BonusPresets/`, and the voice presets in `Input/`) through the
//! engine it is compiled against, writes everything it measured to a JSON file, and, handed the
//! JSON another build wrote, prints what moved and flags what moved too far.
//!
//! It is `#[ignore]`d because one run proves nothing; it takes two builds. To compare the engine
//! at `<commit>` with the working tree, check the commit out beside it with this file and its
//! material (`scripts/reference-checkout.sh`), somewhere other than `/tmp` — two release builds
//! do not fit a tmpfs:
//!
//! ```text
//! scripts/reference-checkout.sh <commit> ../before
//! (cd ../before && PRESET_DRIFT_OUT=$PWD/before.json \
//!     cargo test --release -p fxsound-dsp --test preset_drift -- --ignored --nocapture)
//! PRESET_DRIFT_OUT=target/after.json PRESET_DRIFT_BEFORE=../before/before.json \
//!     cargo test --release -p fxsound-dsp --test preset_drift -- --ignored --nocapture
//! git worktree remove --force ../before   # --force: the copied files are untracked there
//! ```
//!
//! **A preset is rendered as the application plays it** — its effects through the sliders, its
//! curve on ten bands, the settings' default levels ([`fxsound_dsp::preset::preset_params`]) —
//! not from the raw values of the file, so a drift found here is one the application plays. A
//! commit older than 0.5.0 has no `fxsound_dsp::preset`; the script writes one there from that
//! commit's own application. Beyond it the older build needs nothing of this file but `Engine`,
//! `InputChain` and the `analysis` module, which is why it can be copied into a checkout that
//! predates it. The presets are read from that checkout's own `assets/`, so a commit that also
//! changed a preset measures its own version of it. `PRESET_DRIFT_AFTER=<json>` in place of a
//! render compares two files already on disk; without `PRESET_DRIFT_OUT` the measurement goes to
//! `target/preset-drift.json`.
//!
//! # The material
//!
//! Output presets, each rendered at 48 kHz in 1024-frame blocks as `genre_voicing.rs` renders
//! them, with Volume Leveling off (the default) unless the material says otherwise:
//!
//! * `genre/<name>` — the seeded genre material `genre_voicing.rs` builds, one per genre preset
//!   name: the genre-neutral baseline spectrum under that genre's burst envelope.
//! * `loud` — pink noise under a burst envelope, mastered to about −9 dBFS RMS by a hard ceiling
//!   at −0.2 dBFS: a modern master, loud enough that Dynamic Boost's back-off and its limiter are
//!   both working.
//! * `loud/mono` — the same master with one channel copied to the other. The genre and loud
//!   material's two channels are independent noise, the worst case for a limiter that turns both
//!   sides down together; a real mix sits between the two.
//! * `tone/40Hz`, `tone/80Hz` — sines at −1 dBFS, where the bass a preset adds meets the limiter.
//! * `unbalanced` — the loud material with the left side 10 dB below the right.
//! * `silence` — digital silence.
//! * `quiet/Pop` — the Pop material 40 dB down, where nothing in the chain works hard: the
//!   preset's own voicing, which every other render's change of shape is measured against.
//! * `leveling/Classical`, `leveling/tone50Hz` — the Classical material, and a 50 Hz sine at 0.3,
//!   with Volume Leveling at its maximum, which is where a preset's bass meets the leveller.
//!
//! Every piece of music is played three times back to back and only the last pass is measured,
//! the tones likewise and the levelled tone four times: Dynamic Boost's level estimator has a
//! 1.6 s time constant and the leveller takes longer, and a measurement of their start-up is a
//! measurement of the first seconds of a song rather than of the song. The `genre_voicing.rs`
//! protocol — one pass, measured from 0.1 s — is kept for the genre confusion matrix, so that
//! matrix is the one that test prints.
//!
//! Voice presets, through the microphone chain the preset names, on the four signals
//! `voice_presets.rs` probes with — a syllabic speech stand-in at −6 and −26 dBFS, a room floor of
//! hum and hiss at −55 dBFS, and sibilance at −12 dBFS — three seconds each, measured after the
//! first.
//!
//! # What is measured, and what is flagged
//!
//! Per preset and material: BS.1770 integrated loudness, true peak, PLR, the third-octave LTAS,
//! samples past full scale, the sample peak; THD and THD+N on the tones; on stereo material the
//! left-right balance and how much it wanders from one 100 ms window to the next. Per genre
//! column, the full ranking of the twelve genre presets. The comparison flags a preset and
//! material when
//!
//! * integrated loudness moves by more than [`MAX_LUFS_DRIFT`];
//! * any third-octave band from 40 Hz to 16 kHz changes *shape* — both curves level-normalised —
//!   by more than [`MAX_BAND_DRIFT_DB`] (music only; a tone's spectrum is its distortion). A band
//!   the input leaves empty, [`EMPTY_BAND_DB`] under its loudest, carries only what the chain made
//!   there, and is flagged as distortion rather than tone;
//! * PLR moves by more than [`MAX_PLR_DRIFT`];
//! * the true peak rises by more than [`TRUE_PEAK_TOLERANCE_DB`], or samples past full scale
//!   appear or multiply;
//! * THD on a tone rises by more than [`THD_TOLERANCE_PCT`] points and a tenth of itself;
//! * the balance or its wander moves by more than [`MAX_IMAGE_DRIFT_DB`];
//! * silence stops being silent;
//! * a genre preset falls in its own genre's column.
//!
//! A flag says a preset moved, not that it moved the wrong way. One number helps decide which:
//! beside every change of shape the comparison prints how far the render's action on its input
//! sits from the preset's own voicing (its action on `quiet/Pop`), before and after. A change that
//! brings a loud render closer to the voicing is the chain doing less to the preset; one that takes
//! it further away is the preset sounding less like itself. The rest is a judgement this file
//! leaves to whoever reads it.
//!
//! The true peak is read by a meter primed with the 5 ms before the measured section, faded in:
//! [`ProgramMeter`]'s own starts from silence, and a section that begins mid-waveform reads up to
//! 11 % high on its first samples.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use fxsound_core::{Preset, messages::DspParams};
use fxsound_dsp::Engine;
use fxsound_dsp::analysis::{
    NUM_THIRD_OCTAVES, ProgramMeter, SILENT_BAND_DB, THIRD_OCTAVE_CENTRES, TruePeakMeter,
    band_range, level_normalise, shape_distance_db,
};
use fxsound_preset::input::InputPreset;
use realfft::RealFftPlanner;

#[allow(dead_code)]
mod genre_material;
#[allow(dead_code)]
mod output_material;
#[allow(dead_code)]
mod voice_material;

use genre_material::{
    BASELINE_FILE, BLOCK, CHANNELS, COLUMNS, COMPARE_HI_HZ, COMPARE_LO_HZ, GENRE_PRESETS, RATE,
    dynamics_for, load_reference, material, measure, preset_params, render, seed_for,
};
use output_material::{
    Kind, LOUD_RMS_DBFS, Material, TONE_DBFS, UNBALANCED_DB, VOICING_REFERENCE, db_to_gain,
    output_materials, rms, tone,
};
use voice_material::{CAPTURE_RATE, chain_for, room_floor, sibilance, speech_like};

/// A band whose input sits this far under the input's loudest band carries no programme, so what
/// comes out there is made by the chain: distortion, not tone.
const EMPTY_BAND_DB: f64 = 40.0;
/// The window the stereo balance is followed in.
const IMAGE_WINDOW_FRAMES: usize = 4_800;
/// Voice material: three seconds, the first of them left to settle.
const VOICE_FRAMES: usize = 3 * 48_000;
const VOICE_SETTLE_FRAMES: usize = 48_000;
/// The microphone chain's quantum, the same as the output's.
const VOICE_BLOCK: usize = 1024;
/// How much of what comes before a measured section the true-peak meter hears first: 5 ms.
const TRUE_PEAK_PREROLL_FRAMES: usize = 240;
/// The highest harmonic counted into THD; the second is the lowest.
const HARMONICS: usize = 10;
/// Bins either side of a harmonic that belong to it. A 4-term Blackman–Harris window's main lobe
/// is four bins wide on each side; eight leave room for the limiter's slow gain wobble.
const THD_HALF_WIDTH_BINS: usize = 8;

/// Integrated loudness that moves further than this, in LU, is flagged.
const MAX_LUFS_DRIFT: f64 = 0.5;
/// A third-octave band whose level-normalised level moves further than this, in dB, is flagged.
const MAX_BAND_DRIFT_DB: f64 = 1.0;
/// A peak-to-loudness ratio that moves further than this, in dB, is flagged.
const MAX_PLR_DRIFT: f64 = 1.0;
/// A true peak that rises by more than this, in dB, is flagged.
const TRUE_PEAK_TOLERANCE_DB: f64 = 0.1;
/// THD that rises by more than this many percentage points (and a tenth of itself) is flagged.
const THD_TOLERANCE_PCT: f64 = 0.1;
/// A stereo balance, or its wander, that moves further than this, in dB, is flagged.
const MAX_IMAGE_DRIFT_DB: f64 = 1.0;
/// Silence that comes back louder than this is not silence.
const SILENCE_FLOOR_DBFS: f64 = -120.0;

/// Where the measurement goes, and where a comparison's two sides come from.
const OUT_VAR: &str = "PRESET_DRIFT_OUT";
const BEFORE_VAR: &str = "PRESET_DRIFT_BEFORE";
const AFTER_VAR: &str = "PRESET_DRIFT_AFTER";
const FORMAT: &str = "fxsound preset drift 1";

/// The pseudo-preset every material's own measurement is filed under. It must come back identical
/// from both builds, which is what shows the two measured the same thing.
const INPUT: &str = "(input)";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root")
        .to_path_buf()
}

// ---------------------------------------------------------------------------------------------
// Material
// ---------------------------------------------------------------------------------------------

fn voice_materials() -> Vec<Material> {
    let signals = [
        ("speech/-6dBFS", speech_like(db_to_gain(-6.0), VOICE_FRAMES)),
        (
            "speech/-26dBFS",
            speech_like(db_to_gain(-26.0), VOICE_FRAMES),
        ),
        (
            "room-floor/-55dBFS",
            room_floor(db_to_gain(-55.0), VOICE_FRAMES),
        ),
        (
            "sibilance/-12dBFS",
            sibilance(db_to_gain(-12.0), VOICE_FRAMES),
        ),
    ];
    signals
        .into_iter()
        .map(|(name, samples)| Material {
            name: name.to_owned(),
            kind: Kind::Music,
            tone_hz: 0.0,
            leveling: 0.0,
            channels: 1,
            samples,
            measure_from: VOICE_SETTLE_FRAMES,
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Presets and rendering
// ---------------------------------------------------------------------------------------------

/// A `.fac` as the application plays it on ten bands with the settings' default levels:
/// [`fxsound_dsp::preset::preset_params`], the one reading `process_wav --preset`,
/// `genre_voicing.rs` and the application share, so a drift measured here is a drift the
/// application plays. Volume Leveling is the material's ([`through_engine`]).
fn params_of(preset: &Preset) -> DspParams {
    fxsound_dsp::preset::preset_params(
        preset,
        &fxsound_dsp::preset::ladder(fxsound_core::eq::DEFAULT_BANDS),
        fxsound_dsp::preset::MusicLevels::default(),
    )
}

/// Every shipped `.fac`, keyed by directory and file stem.
fn output_presets() -> Vec<(String, DspParams)> {
    let mut out = Vec::new();
    for dir in ["Factsoft", "BonusPresets"] {
        let path = repo_root().join("assets/presets").join(dir);
        let mut files: Vec<PathBuf> = std::fs::read_dir(&path)
            .unwrap_or_else(|err| panic!("{}: {err}", path.display()))
            .flatten()
            .map(|entry| entry.path())
            .filter(|file| file.extension().is_some_and(|e| e == "fac"))
            .collect();
        files.sort();
        for file in files {
            let preset = fxsound_preset::load(&file)
                .unwrap_or_else(|err| panic!("{}: {err}", file.display()));
            let stem = file
                .file_stem()
                .and_then(|s| s.to_str())
                .expect("a preset file name is UTF-8");
            out.push((format!("{dir}/{stem}"), params_of(&preset)));
        }
    }
    assert!(out.len() >= 30, "only found {} presets", out.len());
    out
}

fn voice_presets() -> Vec<InputPreset> {
    let dir = repo_root().join("assets/presets/Input");
    let presets =
        InputPreset::load_dir(&dir).unwrap_or_else(|err| panic!("{}: {err}", dir.display()));
    assert!(
        !presets.is_empty(),
        "{} holds no voice presets",
        dir.display()
    );
    presets
}

fn through_engine(params: &DspParams, material: &Material) -> Vec<f32> {
    let mut params = *params;
    params.volume_leveling_db = material.leveling;
    let mut engine = Engine::new(RATE, BLOCK, CHANNELS);
    engine.apply(&params);
    let mut buffer = material.samples.clone();
    for block in buffer.chunks_mut(BLOCK * CHANNELS) {
        engine.process(block, CHANNELS);
    }
    buffer
}

fn through_voice_chain(preset: &InputPreset, material: &Material) -> Vec<f32> {
    let mut chain = chain_for(preset);
    let mut buffer = material.samples.clone();
    for block in buffer.chunks_mut(VOICE_BLOCK) {
        chain.process(block, 1);
    }
    buffer
}

/// `f` over every item, on every core, in order.
fn parallel_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(items.len().max(1));
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<R>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(index) else {
                        break;
                    };
                    let result = f(item);
                    results.lock().expect("no worker panicked")[index] = Some(result);
                }
            });
        }
    });
    results
        .into_inner()
        .expect("no worker panicked")
        .into_iter()
        .map(|result| result.expect("every item was mapped"))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Measuring
// ---------------------------------------------------------------------------------------------

/// Everything measured about one preset on one material.
#[derive(Clone, Debug, PartialEq)]
struct Record {
    preset: String,
    material: String,
    kind: Kind,
    lufs: Option<f64>,
    true_peak_dbtp: Option<f64>,
    plr_db: Option<f64>,
    ltas_db: Vec<f64>,
    /// Samples past full scale, anywhere in the render.
    clipped: u64,
    /// The largest sample anywhere in the render, `None` when every one is zero.
    peak_dbfs: Option<f64>,
    thd_pct: Option<f64>,
    thdn_pct: Option<f64>,
    /// Left minus right, in dB of RMS.
    balance_db: Option<f64>,
    /// The standard deviation of that balance across 100 ms windows.
    image_sd_db: Option<f64>,
}

fn measure_render(preset: &str, material: &Material, rendered: &[f32]) -> Record {
    let channels = material.channels;
    let section = &rendered[material.measure_from * channels..];
    let mut meter = ProgramMeter::new(RATE, channels, 60.0);
    meter.push(section);
    let measurement = meter.measurement();
    assert_eq!(
        measurement.dropped_blocks, 0,
        "the loudness meter overran its history"
    );

    let peak = rendered.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
    let (thd_pct, thdn_pct) = if material.kind == Kind::Tone {
        let (thd, thdn) = (0..channels)
            .map(|channel| distortion(section, channels, channel, material.tone_hz))
            .fold((0.0f64, 0.0f64), |acc, (thd, thdn)| {
                (acc.0.max(thd), acc.1.max(thdn))
            });
        (Some(thd), Some(thdn))
    } else {
        (None, None)
    };
    let (balance_db, image_sd_db) = if channels == 2 {
        stereo_image(section)
    } else {
        (None, None)
    };

    let lufs = measurement.integrated_lufs.map(f64::from);
    let true_peak_dbtp = true_peak(rendered, channels, material.measure_from);
    Record {
        preset: preset.to_owned(),
        material: material.name.clone(),
        kind: material.kind,
        lufs,
        true_peak_dbtp,
        plr_db: true_peak_dbtp.zip(lufs).map(|(peak, lufs)| peak - lufs),
        ltas_db: measurement.ltas_db.iter().map(|v| f64::from(*v)).collect(),
        clipped: rendered.iter().filter(|s| s.abs() > 1.0).count() as u64,
        peak_dbfs: (peak > 0.0).then(|| 20.0 * f64::from(peak).log10()),
        thd_pct,
        thdn_pct,
        balance_db,
        image_sd_db,
    }
}

/// The true peak from `from` on, read by a meter that has heard the 5 ms before it.
///
/// Not [`ProgramMeter`]'s: its interpolator starts from a delay line of zeros, and a section that
/// starts mid-waveform is a step from zero that rings up to 11 % high on its first samples — a
/// clean 50 Hz sine at −0.30 dBFS read +0.56 dBTP. Fading the samples before the section in
/// under a raised cosine gives the interpolator its history without a step.
fn true_peak(rendered: &[f32], channels: usize, from: usize) -> Option<f64> {
    let preroll = from.min(TRUE_PEAK_PREROLL_FRAMES);
    let start = from - preroll;
    let mut faded = rendered[start * channels..from * channels].to_vec();
    for (i, frame) in faded.chunks_exact_mut(channels).enumerate() {
        let gain = 0.5 - 0.5 * (std::f32::consts::PI * i as f32 / preroll as f32).cos();
        for sample in frame {
            *sample *= gain;
        }
    }
    let mut meter = TruePeakMeter::new(channels);
    meter.push(&faded);
    meter.push(&rendered[from * channels..]);
    (meter.peak() > 0.0).then(|| 20.0 * f64::from(meter.peak()).log10())
}

/// THD and THD+N of one channel, in percent, from the largest power-of-two stretch at its end.
///
/// THD counts harmonics 2 to [`HARMONICS`]; THD+N counts everything from 10 Hz to 20 kHz that is
/// not the fundamental, which is where a limiter's gain wobble lands when it is not a harmonic.
fn distortion(section: &[f32], channels: usize, channel: usize, hz: f32) -> (f64, f64) {
    let frames = section.len() / channels;
    assert!(frames >= 4_096, "too short to measure distortion on");
    let n = 1usize << (usize::BITS - 1 - frames.leading_zeros());
    let start = frames - n;
    let tau = std::f64::consts::TAU;
    let mut input: Vec<f32> = (0..n)
        .map(|i| {
            let x = i as f64 / n as f64;
            // 4-term Blackman–Harris: sidelobes 92 dB down, far under anything counted here.
            let window = 0.358_75 - 0.488_29 * (tau * x).cos() + 0.141_28 * (2.0 * tau * x).cos()
                - 0.011_68 * (3.0 * tau * x).cos();
            (f64::from(section[(start + i) * channels + channel]) * window) as f32
        })
        .collect();
    let mut planner = RealFftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(n);
    let mut spectrum = fft.make_output_vec();
    fft.process(&mut input, &mut spectrum)
        .expect("the section transforms");
    let power: Vec<f64> = spectrum
        .iter()
        .map(|bin| f64::from(bin.norm_sqr()))
        .collect();

    let bin_hz = f64::from(RATE) / n as f64;
    let last = power.len() - 1;
    let around = |centre_hz: f64| -> f64 {
        let centre = (centre_hz / bin_hz).round() as usize;
        let lo = centre.saturating_sub(THD_HALF_WIDTH_BINS).max(1);
        let hi = (centre + THD_HALF_WIDTH_BINS).min(last);
        power[lo..=hi].iter().sum()
    };
    let fundamental = around(f64::from(hz));
    if fundamental <= 0.0 {
        return (0.0, 0.0);
    }
    let harmonics: f64 = (2..=HARMONICS)
        .map(|h| h as f64 * f64::from(hz))
        .filter(|f| *f < 20_000.0)
        .map(around)
        .sum();
    let lo = (10.0 / bin_hz).ceil() as usize;
    let hi = ((20_000.0 / bin_hz).floor() as usize).min(last);
    let total: f64 = power[lo..=hi].iter().sum();
    let noise = (total - fundamental).max(0.0);
    (
        100.0 * (harmonics / fundamental).sqrt(),
        100.0 * (noise / fundamental).sqrt(),
    )
}

/// Left minus right over the whole section, and how far that wanders from window to window.
fn stereo_image(section: &[f32]) -> (Option<f64>, Option<f64>) {
    let power = |frames: &[f32], channel: usize| -> f64 {
        frames
            .as_chunks::<CHANNELS>()
            .0
            .iter()
            .map(|frame| f64::from(frame[channel]).powi(2))
            .sum()
    };
    let ratio_db = |frames: &[f32]| -> Option<f64> {
        let (left, right) = (power(frames, 0), power(frames, 1));
        (left > 1e-12 && right > 1e-12).then(|| 10.0 * (left / right).log10())
    };
    let balance = ratio_db(section);
    let windows: Vec<f64> = section
        .as_chunks::<{ IMAGE_WINDOW_FRAMES * CHANNELS }>()
        .0
        .iter()
        .filter_map(|window| ratio_db(window))
        .collect();
    let wander = (windows.len() > 1).then(|| {
        let mean = windows.iter().sum::<f64>() / windows.len() as f64;
        (windows.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / windows.len() as f64).sqrt()
    });
    (balance, wander)
}

// ---------------------------------------------------------------------------------------------
// The genre confusion matrix
// ---------------------------------------------------------------------------------------------

/// One genre's column: the twelve genre presets best first, each with `D(out) − D(in)` against
/// that genre's reference. Negative moved the material toward it.
#[derive(Clone, Debug, PartialEq)]
struct Column {
    genre: String,
    /// `genre_voicing` — one pass, measured from 0.1 s, as that test does — or `steady`, the last
    /// of three passes, from the drift renders.
    protocol: String,
    scores: Vec<(String, f64)>,
}

fn sorted(mut scores: Vec<(String, f64)>) -> Vec<(String, f64)> {
    scores.sort_by(|a, b| a.1.partial_cmp(&b.1).expect("finite"));
    scores
}

/// The matrix exactly as `genre_voicing.rs` computes and prints it.
fn genre_voicing_columns() -> Vec<Column> {
    let baseline = load_reference(BASELINE_FILE);
    let range = band_range(COMPARE_LO_HZ, COMPARE_HI_HZ);
    let params: Vec<DspParams> = GENRE_PRESETS.iter().map(|p| preset_params(p)).collect();
    COLUMNS
        .iter()
        .map(|(genre, file)| {
            let reference = load_reference(file);
            let samples = material(&baseline.level_db, seed_for(genre), dynamics_for(genre));
            let before = shape_distance_db(
                &measure(&samples).ltas_db,
                &reference.level_db,
                range.clone(),
            );
            let scores = parallel_map(&params, |params| {
                let out = render(&samples, params).measurement;
                shape_distance_db(&out.ltas_db, &reference.level_db, range.clone()) - before
            });
            Column {
                genre: (*genre).to_owned(),
                protocol: "genre_voicing".to_owned(),
                scores: sorted(
                    GENRE_PRESETS
                        .iter()
                        .map(|p| (*p).to_owned())
                        .zip(scores.into_iter().map(f64::from))
                        .collect(),
                ),
            }
        })
        .collect()
}

fn ltas_of(record: &Record) -> [f32; NUM_THIRD_OCTAVES] {
    let mut out = [SILENT_BAND_DB; NUM_THIRD_OCTAVES];
    for (slot, value) in out.iter_mut().zip(&record.ltas_db) {
        *slot = *value as f32;
    }
    out
}

/// The same scoring over the drift renders' steady state.
fn steady_columns(records: &[Record]) -> Vec<Column> {
    let range = band_range(COMPARE_LO_HZ, COMPARE_HI_HZ);
    let find = |preset: &str, material: &str| -> &Record {
        records
            .iter()
            .find(|r| r.preset == preset && r.material == material)
            .unwrap_or_else(|| panic!("no render of {preset} on {material}"))
    };
    COLUMNS
        .iter()
        .map(|(genre, file)| {
            let reference = load_reference(file);
            let material = format!("genre/{genre}");
            let before = shape_distance_db(
                &ltas_of(find(INPUT, &material)),
                &reference.level_db,
                range.clone(),
            );
            let scores = GENRE_PRESETS
                .iter()
                .map(|preset| {
                    let out = ltas_of(find(&format!("BonusPresets/{preset}"), &material));
                    let after = shape_distance_db(&out, &reference.level_db, range.clone());
                    ((*preset).to_owned(), f64::from(after - before))
                })
                .collect();
            Column {
                genre: (*genre).to_owned(),
                protocol: "steady".to_owned(),
                scores: sorted(scores),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// One whole measurement
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
struct Run {
    output: Vec<Record>,
    voice: Vec<Record>,
    columns: Vec<Column>,
}

fn measure_everything() -> Run {
    let materials = output_materials();
    let presets = output_presets();
    let mut jobs: Vec<(Option<usize>, usize)> = Vec::new();
    for material in 0..materials.len() {
        jobs.push((None, material));
        for preset in 0..presets.len() {
            jobs.push((Some(preset), material));
        }
    }
    let output = parallel_map(&jobs, |&(preset, material)| {
        let material = &materials[material];
        match preset {
            None => measure_render(INPUT, material, &material.samples),
            Some(index) => {
                let (key, params) = &presets[index];
                measure_render(key, material, &through_engine(params, material))
            }
        }
    });

    let materials = voice_materials();
    let presets = voice_presets();
    let mut jobs: Vec<(Option<usize>, usize)> = Vec::new();
    for material in 0..materials.len() {
        jobs.push((None, material));
        for preset in 0..presets.len() {
            jobs.push((Some(preset), material));
        }
    }
    let voice = parallel_map(&jobs, |&(preset, material)| {
        let material = &materials[material];
        match preset {
            None => measure_render(INPUT, material, &material.samples),
            Some(index) => {
                let preset = &presets[index];
                measure_render(
                    &format!("Input/{}", preset.name),
                    material,
                    &through_voice_chain(preset, material),
                )
            }
        }
    });

    let mut columns = genre_voicing_columns();
    columns.extend(steady_columns(&output));
    Run {
        output,
        voice,
        columns,
    }
}

// ---------------------------------------------------------------------------------------------
// JSON, both ways, without a dependency the older build may not have
// ---------------------------------------------------------------------------------------------

fn json_string(out: &mut String, value: &str) {
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn json_number(out: &mut String, value: Option<f64>) {
    match value {
        Some(v) if v.is_finite() => {
            let _ = write!(out, "{v}");
        }
        _ => out.push_str("null"),
    }
}

fn record_json(out: &mut String, record: &Record) {
    out.push_str("    {\"preset\": ");
    json_string(out, &record.preset);
    out.push_str(", \"material\": ");
    json_string(out, &record.material);
    out.push_str(", \"kind\": ");
    json_string(out, record.kind.key());
    for (key, value) in [
        ("lufs", record.lufs),
        ("true_peak_dbtp", record.true_peak_dbtp),
        ("plr_db", record.plr_db),
        ("peak_dbfs", record.peak_dbfs),
        ("thd_pct", record.thd_pct),
        ("thdn_pct", record.thdn_pct),
        ("balance_db", record.balance_db),
        ("image_sd_db", record.image_sd_db),
    ] {
        let _ = write!(out, ", \"{key}\": ");
        json_number(out, value);
    }
    let _ = write!(out, ", \"clipped\": {}, \"ltas_db\": [", record.clipped);
    for (i, value) in record.ltas_db.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        json_number(out, Some(*value));
    }
    out.push_str("]}");
}

fn to_json(run: &Run) -> String {
    let mut out = String::new();
    out.push_str("{\n  \"format\": ");
    json_string(&mut out, FORMAT);
    let _ = write!(
        out,
        ",\n  \"rate\": {RATE},\n  \"block\": {BLOCK},\n  \"third_octave_centres_hz\": ["
    );
    for (i, centre) in THIRD_OCTAVE_CENTRES.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "{centre}");
    }
    out.push(']');
    for (key, records) in [("output", &run.output), ("voice", &run.voice)] {
        let _ = write!(out, ",\n  \"{key}\": [\n");
        for (i, record) in records.iter().enumerate() {
            if i > 0 {
                out.push_str(",\n");
            }
            record_json(&mut out, record);
        }
        out.push_str("\n  ]");
    }
    out.push_str(",\n  \"columns\": [\n");
    for (i, column) in run.columns.iter().enumerate() {
        if i > 0 {
            out.push_str(",\n");
        }
        out.push_str("    {\"genre\": ");
        json_string(&mut out, &column.genre);
        out.push_str(", \"protocol\": ");
        json_string(&mut out, &column.protocol);
        out.push_str(", \"scores\": [");
        for (j, (preset, score)) in column.scores.iter().enumerate() {
            if j > 0 {
                out.push_str(", ");
            }
            out.push('[');
            json_string(&mut out, preset);
            out.push_str(", ");
            json_number(&mut out, Some(*score));
            out.push(']');
        }
        out.push_str("]}");
    }
    out.push_str("\n  ]\n}\n");
    out
}

#[derive(Clone, Debug, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Number(f64),
    Text(String),
    List(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    fn get(&self, key: &str) -> &Self {
        match self {
            Self::Object(fields) => fields
                .iter()
                .find(|(name, _)| name == key)
                .map_or(&Self::Null, |(_, value)| value),
            _ => &Self::Null,
        }
    }

    fn number(&self) -> Option<f64> {
        match self {
            Self::Number(v) => Some(*v),
            _ => None,
        }
    }

    fn text(&self) -> &str {
        match self {
            Self::Text(s) => s,
            _ => "",
        }
    }

    fn list(&self) -> &[Self] {
        match self {
            Self::List(items) => items,
            _ => &[],
        }
    }
}

struct JsonReader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl JsonReader<'_> {
    fn skip_space(&mut self) {
        while self
            .bytes
            .get(self.at)
            .is_some_and(|b| b.is_ascii_whitespace())
        {
            self.at += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        self.skip_space();
        if self.bytes.get(self.at) == Some(&byte) {
            self.at += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at byte {}", byte as char, self.at))
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        self.skip_space();
        match self.bytes.get(self.at) {
            Some(b'{') => {
                self.at += 1;
                let mut fields = Vec::new();
                self.skip_space();
                if self.bytes.get(self.at) == Some(&b'}') {
                    self.at += 1;
                    return Ok(Json::Object(fields));
                }
                loop {
                    self.skip_space();
                    let key = self.string()?;
                    self.expect(b':')?;
                    fields.push((key, self.value()?));
                    self.skip_space();
                    match self.bytes.get(self.at) {
                        Some(b',') => self.at += 1,
                        Some(b'}') => {
                            self.at += 1;
                            return Ok(Json::Object(fields));
                        }
                        _ => return Err(format!("bad object at byte {}", self.at)),
                    }
                }
            }
            Some(b'[') => {
                self.at += 1;
                let mut items = Vec::new();
                self.skip_space();
                if self.bytes.get(self.at) == Some(&b']') {
                    self.at += 1;
                    return Ok(Json::List(items));
                }
                loop {
                    items.push(self.value()?);
                    self.skip_space();
                    match self.bytes.get(self.at) {
                        Some(b',') => self.at += 1,
                        Some(b']') => {
                            self.at += 1;
                            return Ok(Json::List(items));
                        }
                        _ => return Err(format!("bad list at byte {}", self.at)),
                    }
                }
            }
            Some(b'"') => Ok(Json::Text(self.string()?)),
            Some(_) => self.word(),
            None => Err("unexpected end".to_owned()),
        }
    }

    fn string(&mut self) -> Result<String, String> {
        if self.bytes.get(self.at) != Some(&b'"') {
            return Err(format!("expected a string at byte {}", self.at));
        }
        self.at += 1;
        let mut out = Vec::new();
        loop {
            let byte = *self.bytes.get(self.at).ok_or("unterminated string")?;
            self.at += 1;
            match byte {
                b'"' => break,
                b'\\' => {
                    let escaped = *self.bytes.get(self.at).ok_or("unterminated escape")?;
                    self.at += 1;
                    match escaped {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'u' => {
                            let hex = self
                                .bytes
                                .get(self.at..self.at + 4)
                                .ok_or("short \\u escape")?;
                            self.at += 4;
                            let code = u32::from_str_radix(
                                std::str::from_utf8(hex).map_err(|e| e.to_string())?,
                                16,
                            )
                            .map_err(|e| e.to_string())?;
                            let c = char::from_u32(code).ok_or("a \\u escape out of range")?;
                            let mut buffer = [0u8; 4];
                            out.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
                        }
                        other => out.push(other),
                    }
                }
                other => out.push(other),
            }
        }
        String::from_utf8(out).map_err(|e| e.to_string())
    }

    fn word(&mut self) -> Result<Json, String> {
        let start = self.at;
        while self
            .bytes
            .get(self.at)
            .is_some_and(|b| !matches!(b, b',' | b']' | b'}') && !b.is_ascii_whitespace())
        {
            self.at += 1;
        }
        let word = std::str::from_utf8(&self.bytes[start..self.at]).map_err(|e| e.to_string())?;
        match word {
            "null" => Ok(Json::Null),
            "true" => Ok(Json::Bool(true)),
            "false" => Ok(Json::Bool(false)),
            number => number
                .parse()
                .map(Json::Number)
                .map_err(|_| format!("not a value: {number:?} at byte {start}")),
        }
    }
}

fn parse_json(text: &str) -> Result<Json, String> {
    let mut reader = JsonReader {
        bytes: text.as_bytes(),
        at: 0,
    };
    let value = reader.value()?;
    reader.skip_space();
    if reader.at == reader.bytes.len() {
        Ok(value)
    } else {
        Err(format!("trailing bytes at {}", reader.at))
    }
}

fn record_from(json: &Json) -> Record {
    Record {
        preset: json.get("preset").text().to_owned(),
        material: json.get("material").text().to_owned(),
        kind: Kind::from_key(json.get("kind").text()),
        lufs: json.get("lufs").number(),
        true_peak_dbtp: json.get("true_peak_dbtp").number(),
        plr_db: json.get("plr_db").number(),
        ltas_db: json
            .get("ltas_db")
            .list()
            .iter()
            .map(|v| v.number().unwrap_or(f64::from(SILENT_BAND_DB)))
            .collect(),
        clipped: json.get("clipped").number().unwrap_or(0.0) as u64,
        peak_dbfs: json.get("peak_dbfs").number(),
        thd_pct: json.get("thd_pct").number(),
        thdn_pct: json.get("thdn_pct").number(),
        balance_db: json.get("balance_db").number(),
        image_sd_db: json.get("image_sd_db").number(),
    }
}

fn run_from_json(text: &str) -> Run {
    let json = parse_json(text).unwrap_or_else(|err| panic!("not a drift file: {err}"));
    assert_eq!(
        json.get("format").text(),
        FORMAT,
        "not a file this harness wrote"
    );
    let records = |key: &str| json.get(key).list().iter().map(record_from).collect();
    Run {
        output: records("output"),
        voice: records("voice"),
        columns: json
            .get("columns")
            .list()
            .iter()
            .map(|column| Column {
                genre: column.get("genre").text().to_owned(),
                protocol: column.get("protocol").text().to_owned(),
                scores: column
                    .get("scores")
                    .list()
                    .iter()
                    .map(|pair| {
                        let pair = pair.list();
                        (
                            pair.first().map_or("", Json::text).to_owned(),
                            pair.get(1).and_then(Json::number).unwrap_or(f64::NAN),
                        )
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn read_run(path: &Path) -> Run {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    run_from_json(&text)
}

// ---------------------------------------------------------------------------------------------
// The comparison
// ---------------------------------------------------------------------------------------------

/// A change of shape in one third-octave band, in dB, and the band's centre in Hz.
type BandShift = (f64, f32);

/// One preset on one material, before against after.
struct Drift<'a> {
    before: &'a Record,
    after: &'a Record,
    lufs: Option<f64>,
    true_peak: Option<f64>,
    plr: Option<f64>,
    /// The largest change of shape in a band from 40 Hz to 16 kHz that carries programme.
    band: Option<BandShift>,
    /// The same over the bands the input leaves empty, where what comes out is the chain's own
    /// distortion.
    empty_band: Option<BandShift>,
    /// How far the render's change of shape sits from the preset's own voicing, before and after:
    /// the RMS over 40 Hz–16 kHz of (output shape − input shape) − (the same on the quiet
    /// reference). Smaller means closer to what the preset's curve and effects ask for.
    voicing: Option<(f64, f64)>,
    flags: Vec<String>,
}

fn difference(before: Option<f64>, after: Option<f64>) -> Option<f64> {
    before.zip(after).map(|(b, a)| a - b)
}

fn audible(record: &Record) -> bool {
    record.ltas_db.iter().any(|v| *v > -150.0)
}

fn shape_of(record: &Record) -> [f32; NUM_THIRD_OCTAVES] {
    let mut out = [0.0f32; NUM_THIRD_OCTAVES];
    level_normalise(
        &ltas_of(record),
        band_range(COMPARE_LO_HZ, COMPARE_HI_HZ),
        &mut out,
    );
    out
}

/// What a render did to the shape of its input, band by band.
fn action(output: &Record, input: &Record) -> [f32; NUM_THIRD_OCTAVES] {
    let (out, inp) = (shape_of(output), shape_of(input));
    std::array::from_fn(|i| out[i] - inp[i])
}

/// The largest change of shape, over the bands that carry programme and over those that do not.
fn shape_drift(
    before: &Record,
    after: &Record,
    input: Option<&Record>,
) -> (Option<BandShift>, Option<BandShift>) {
    if !audible(before) || !audible(after) {
        return (None, None);
    }
    let range = band_range(COMPARE_LO_HZ, COMPARE_HI_HZ);
    let (b, a) = (shape_of(before), shape_of(after));
    let input = input.filter(|i| audible(i)).map(shape_of);
    let loudest = input.map(|i| range.clone().map(|k| i[k]).fold(f32::MIN, f32::max));
    let empty = |k: usize| -> bool {
        input
            .zip(loudest)
            .is_some_and(|(i, top)| f64::from(top - i[k]) > EMPTY_BAND_DB)
    };
    let worst = |bands: &mut dyn Iterator<Item = usize>| {
        bands
            .map(|k| (f64::from(a[k] - b[k]), THIRD_OCTAVE_CENTRES[k]))
            .max_by(|x, y| x.0.abs().partial_cmp(&y.0.abs()).expect("finite"))
    };
    (
        worst(&mut range.clone().filter(|k| !empty(*k))),
        worst(&mut range.clone().filter(|k| empty(*k))),
    )
}

fn voicing_distance(output: &Record, input: &Record, voicing: &[f32; NUM_THIRD_OCTAVES]) -> f64 {
    let range = band_range(COMPARE_LO_HZ, COMPARE_HI_HZ);
    let did = action(output, input);
    let squares: f64 = range
        .clone()
        .map(|k| f64::from(did[k] - voicing[k]).powi(2))
        .sum();
    (squares / range.len() as f64).sqrt()
}

fn drift<'a>(
    before: &'a Record,
    after: &'a Record,
    input: Option<&Record>,
    voicing: Option<&[f32; NUM_THIRD_OCTAVES]>,
) -> Drift<'a> {
    let lufs = difference(before.lufs, after.lufs);
    let true_peak = difference(before.true_peak_dbtp, after.true_peak_dbtp);
    let plr = difference(before.plr_db, after.plr_db);
    // A tone's spectrum is its distortion, and silence has none: shape is a question for music.
    let music = matches!(after.kind, Kind::Music | Kind::Unbalanced);
    let (band, empty_band) = if music {
        shape_drift(before, after, input)
    } else {
        (None, None)
    };
    let voicing = input
        .zip(voicing)
        .filter(|(input, _)| music && audible(input) && audible(before) && audible(after))
        .map(|(input, voicing)| {
            (
                voicing_distance(before, input, voicing),
                voicing_distance(after, input, voicing),
            )
        });
    let mut flags = Vec::new();
    if lufs.is_some_and(|d| d.abs() > MAX_LUFS_DRIFT) {
        flags.push("loudness".to_owned());
    }
    if let Some((d, hz)) = band
        && d.abs() > MAX_BAND_DRIFT_DB
    {
        flags.push(format!("tone@{hz}Hz"));
    }
    if let Some((d, hz)) = empty_band
        && d.abs() > MAX_BAND_DRIFT_DB
    {
        flags.push(format!("distortion@{hz}Hz"));
    }
    if plr.is_some_and(|d| d.abs() > MAX_PLR_DRIFT) {
        flags.push("PLR".to_owned());
    }
    let peak_rose = match (before.true_peak_dbtp, after.true_peak_dbtp) {
        (Some(b), Some(a)) => a > b + TRUE_PEAK_TOLERANCE_DB,
        (None, Some(_)) => true,
        _ => false,
    };
    if peak_rose && after.kind != Kind::Silence {
        flags.push("true-peak".to_owned());
    }
    if after.clipped > before.clipped {
        flags.push("clipping".to_owned());
    }
    if let (Some(b), Some(a)) = (before.thd_pct, after.thd_pct)
        && a > b + THD_TOLERANCE_PCT.max(0.1 * b)
    {
        flags.push("THD".to_owned());
    }
    let image_moved = difference(before.balance_db, after.balance_db)
        .is_some_and(|d| d.abs() > MAX_IMAGE_DRIFT_DB)
        || difference(before.image_sd_db, after.image_sd_db)
            .is_some_and(|d| d.abs() > MAX_IMAGE_DRIFT_DB);
    if image_moved && after.kind == Kind::Unbalanced {
        flags.push("image".to_owned());
    }
    if after.kind == Kind::Silence
        && after
            .peak_dbfs
            .is_some_and(|a| a > SILENCE_FLOOR_DBFS && before.peak_dbfs.is_none_or(|b| a > b + 1.0))
    {
        flags.push("noise-in-silence".to_owned());
    }
    Drift {
        before,
        after,
        lufs,
        true_peak,
        plr,
        band,
        empty_band,
        voicing,
        flags,
    }
}

fn find<'a>(records: &'a [Record], preset: &str, material: &str) -> Option<&'a Record> {
    records
        .iter()
        .find(|r| r.preset == preset && r.material == material)
}

/// Every record of `after` against its twin in `before`, with the material as it went in and,
/// where the run has one, the preset's own voicing from `after`'s quiet reference.
fn pairs<'a>(before: &'a [Record], after: &'a [Record]) -> Vec<Drift<'a>> {
    let reference_input = find(after, INPUT, VOICING_REFERENCE);
    let mut out = Vec::new();
    for a in after {
        let Some(b) = find(before, &a.preset, &a.material) else {
            println!("  only after: {} on {}", a.preset, a.material);
            continue;
        };
        let voicing = reference_input
            .zip(find(after, &a.preset, VOICING_REFERENCE))
            .filter(|(input, output)| audible(input) && audible(output))
            .map(|(input, output)| action(output, input));
        out.push(drift(
            b,
            a,
            find(after, INPUT, &a.material),
            voicing.as_ref(),
        ));
    }
    for b in before {
        if find(after, &b.preset, &b.material).is_none() {
            println!("  only before: {} on {}", b.preset, b.material);
        }
    }
    out
}

fn show(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_owned(), |v| format!("{v:+.2}"))
}

fn show_plain(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_owned(), |v| format!("{v:.2}"))
}

fn print_rows(title: &str, drifts: &[Drift<'_>]) {
    println!();
    println!("== {title}: every preset on every material");
    println!(
        "ROW\tpreset\tmaterial\tLUFS b\tLUFS a\tdLUFS\tTP b\tTP a\tdTP\tPLR b\tPLR a\tdPLR\t\
         shape dB@Hz\tempty-band dB@Hz\tvoicing b\tvoicing a\tTHD b\tTHD a\tTHD+N b\tTHD+N a\tclip b\tclip a\tbal b\tbal a\twander b\t\
         wander a\tpeak b\tpeak a\tflags"
    );
    for d in drifts {
        let (b, a) = (d.before, d.after);
        println!(
            "ROW\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t\
             {}\t{}\t{}\t{}\t{}\t{}\t{}",
            a.preset,
            a.material,
            show_plain(b.lufs),
            show_plain(a.lufs),
            show(d.lufs),
            show_plain(b.true_peak_dbtp),
            show_plain(a.true_peak_dbtp),
            show(d.true_peak),
            show_plain(b.plr_db),
            show_plain(a.plr_db),
            show(d.plr),
            d.band
                .map_or_else(|| "-".to_owned(), |(v, hz)| format!("{v:+.2}@{hz}")),
            d.empty_band
                .map_or_else(|| "-".to_owned(), |(v, hz)| format!("{v:+.2}@{hz}")),
            d.voicing
                .map_or_else(|| "-".to_owned(), |(v, _)| format!("{v:.2}")),
            d.voicing
                .map_or_else(|| "-".to_owned(), |(_, v)| format!("{v:.2}")),
            b.thd_pct
                .map_or_else(|| "-".to_owned(), |v| format!("{v:.3}")),
            a.thd_pct
                .map_or_else(|| "-".to_owned(), |v| format!("{v:.3}")),
            b.thdn_pct
                .map_or_else(|| "-".to_owned(), |v| format!("{v:.3}")),
            a.thdn_pct
                .map_or_else(|| "-".to_owned(), |v| format!("{v:.3}")),
            b.clipped,
            a.clipped,
            show(b.balance_db),
            show(a.balance_db),
            show_plain(b.image_sd_db),
            show_plain(a.image_sd_db),
            show_plain(b.peak_dbfs),
            show_plain(a.peak_dbfs),
            d.flags.join(",")
        );
    }
}

/// One line per preset: the ranges its numbers moved over, and every flag it raised.
fn print_summary(title: &str, drifts: &[Drift<'_>]) {
    println!();
    println!("== {title}: per preset");
    // The records run material by material, so a preset's are not next to each other.
    let mut presets: Vec<&str> = Vec::new();
    for drift in drifts {
        if !presets.contains(&drift.after.preset.as_str()) {
            presets.push(&drift.after.preset);
        }
    }
    for preset in presets {
        let mine: Vec<&Drift<'_>> = drifts.iter().filter(|d| d.after.preset == preset).collect();
        let range = |pick: &dyn Fn(&Drift<'_>) -> Option<f64>| -> String {
            let values: Vec<f64> = mine.iter().filter_map(|d| pick(d)).collect();
            if values.is_empty() {
                return "-".to_owned();
            }
            let lo = values.iter().copied().fold(f64::INFINITY, f64::min);
            let hi = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            format!("{lo:+.2}..{hi:+.2}")
        };
        let worst_band = mine
            .iter()
            .filter_map(|d| d.band.map(|(v, hz)| (v, hz, d.after.material.as_str())))
            .max_by(|x, y| x.0.abs().partial_cmp(&y.0.abs()).expect("finite"))
            .map_or_else(
                || "-".to_owned(),
                |(v, hz, m)| format!("{v:+.2} dB @ {hz} Hz ({m})"),
            );
        let mut flags: Vec<String> = Vec::new();
        for d in &mine {
            for flag in &d.flags {
                flags.push(format!("{flag}[{}]", d.after.material));
            }
        }
        println!(
            "SUMMARY\t{preset}\tdLUFS {}\tdTP {}\tdPLR {}\tworst band {worst_band}\t\
             voicing distance before {} after {}\tflags: {}",
            range(&|d| d.lufs),
            range(&|d| d.true_peak),
            range(&|d| d.plr),
            range(&|d| d.voicing.map(|(b, _)| b)),
            range(&|d| d.voicing.map(|(_, a)| a)),
            if flags.is_empty() {
                "none".to_owned()
            } else {
                flags.join(" ")
            }
        );
    }
}

fn print_columns(before: &[Column], after: &[Column]) {
    println!();
    println!("== Genre columns: rank of the preset of that name, and its margin");
    println!(
        "  margin = its dD minus the best other preset's; negative means it wins by that much"
    );
    for a in after {
        let Some(b) = before
            .iter()
            .find(|b| b.genre == a.genre && b.protocol == a.protocol)
        else {
            continue;
        };
        let place = |column: &Column| -> (usize, f64, String) {
            let rank = column
                .scores
                .iter()
                .position(|(p, _)| *p == column.genre)
                .expect("every genre preset is in its column");
            let own = column.scores[rank].1;
            let best_other = column
                .scores
                .iter()
                .find(|(p, _)| *p != column.genre)
                .expect("a column has more than one preset");
            (rank + 1, own - best_other.1, column.scores[0].0.clone())
        };
        let (rank_b, margin_b, winner_b) = place(b);
        let (rank_a, margin_a, winner_a) = place(a);
        let flag = if rank_a > rank_b { "\tFLAG genre" } else { "" };
        println!(
            "COLUMN\t{}\t{}\tbefore #{rank_b} margin {margin_b:+.2} (winner {winner_b})\t\
             after #{rank_a} margin {margin_a:+.2} (winner {winner_a}){flag}",
            a.protocol, a.genre
        );
        let ranking = |column: &Column| -> String {
            column
                .scores
                .iter()
                .map(|(p, s)| format!("{p} {s:+.2}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        println!("  before: {}", ranking(b));
        println!("  after:  {}", ranking(a));
    }
}

fn compare(before: &Run, after: &Run) {
    let output = pairs(&before.output, &after.output);
    let voice = pairs(&before.voice, &after.voice);

    let moved_inputs: Vec<String> = output
        .iter()
        .chain(&voice)
        .filter(|d| d.after.preset == INPUT && d.before != d.after)
        .map(|d| d.after.material.clone())
        .collect();
    println!();
    if moved_inputs.is_empty() {
        println!("== The material is identical in both runs.");
    } else {
        println!(
            "== WARNING: the material differs between the runs, so nothing below compares like \
             with like: {}",
            moved_inputs.join(", ")
        );
    }

    print_rows("Output presets", &output);
    print_rows("Voice presets", &voice);
    print_summary("Output presets", &output);
    print_summary("Voice presets", &voice);
    print_columns(&before.columns, &after.columns);

    let flagged: Vec<&str> = output
        .iter()
        .chain(&voice)
        .filter(|d| !d.flags.is_empty())
        .map(|d| d.after.preset.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    println!();
    println!(
        "== {} preset(s) flagged: {}",
        flagged.len(),
        flagged.join(", ")
    );
}

#[test]
#[ignore = "a measurement for comparing two builds; see the module documentation"]
fn every_shipped_preset_is_measured_and_compared_with_the_build_before() {
    let after = match std::env::var_os(AFTER_VAR) {
        Some(path) => read_run(Path::new(&path)),
        None => {
            let started = std::time::Instant::now();
            let run = measure_everything();
            let path = std::env::var_os(OUT_VAR).map_or_else(
                || repo_root().join("target/preset-drift.json"),
                PathBuf::from,
            );
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .unwrap_or_else(|err| panic!("{}: {err}", dir.display()));
            }
            std::fs::write(&path, to_json(&run))
                .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
            println!(
                "measured {} output and {} voice renders in {:.1} s: {}",
                run.output.len(),
                run.voice.len(),
                started.elapsed().as_secs_f32(),
                path.display()
            );
            run
        }
    };
    if let Some(path) = std::env::var_os(BEFORE_VAR) {
        compare(&read_run(Path::new(&path)), &after);
    }
}

// ---------------------------------------------------------------------------------------------
// The harness's own checks, cheap enough for every run
// ---------------------------------------------------------------------------------------------

#[test]
fn the_distortion_meter_reads_a_clean_tone_as_clean_and_a_distorted_one_as_distorted() {
    let clean = tone("clean", 40.0, db_to_gain(TONE_DBFS), 1, 0.0);
    let section = &clean.samples[clean.measure_from * CHANNELS..];
    let (thd, thdn) = distortion(section, CHANNELS, 0, 40.0);
    assert!(
        thd < 0.001 && thdn < 0.01,
        "a clean sine read {thd}% / {thdn}%"
    );

    // One percent of third harmonic reads as one percent.
    let with_third: Vec<f32> = section
        .as_chunks::<CHANNELS>()
        .0
        .iter()
        .enumerate()
        .flat_map(|(n, frame)| {
            let third = 0.01
                * f64::from(db_to_gain(TONE_DBFS))
                * (std::f64::consts::TAU * 120.0 * n as f64 / f64::from(RATE)).sin();
            let value = frame[0] + third as f32;
            [value, value]
        })
        .collect();
    let (thd, _) = distortion(&with_third, CHANNELS, 1, 40.0);
    assert!(
        (thd - 1.0).abs() < 0.02,
        "1 % of third harmonic read {thd}%"
    );

    // A sine hard-clipped at half its peak is badly distorted.
    let clipped: Vec<f32> = section.iter().map(|s| s.clamp(-0.45, 0.45)).collect();
    let (thd, thdn) = distortion(&clipped, CHANNELS, 0, 40.0);
    assert!(
        thd > 10.0 && thdn >= thd,
        "a clipped sine read {thd}% / {thdn}%"
    );
}

#[test]
fn the_image_meter_reads_a_ten_decibel_imbalance() {
    let quiet = db_to_gain(-UNBALANCED_DB);
    let section: Vec<f32> = tone("probe", 440.0, 0.5, 1, 0.0)
        .samples
        .as_chunks::<CHANNELS>()
        .0
        .iter()
        .flat_map(|frame| [frame[0] * quiet, frame[1]])
        .collect();
    let (balance, wander) = stereo_image(&section);
    let balance = balance.expect("both sides carry signal");
    assert!((balance + 10.0).abs() < 0.01, "read {balance} dB");
    assert!(wander.expect("many windows") < 0.01);
}

#[test]
fn a_measurement_comes_back_from_json_as_it_went_in() {
    let record = Record {
        preset: "BonusPresets/R&B \"quoted\" \\ back".to_owned(),
        material: "genre/70's".to_owned(),
        kind: Kind::Unbalanced,
        lufs: Some(-14.25),
        true_peak_dbtp: Some(-0.3),
        plr_db: None,
        ltas_db: (0..NUM_THIRD_OCTAVES)
            .map(|i| -40.0 - i as f64 * 0.5)
            .collect(),
        clipped: 12,
        peak_dbfs: None,
        thd_pct: Some(1.0e-7),
        thdn_pct: Some(15.625),
        balance_db: Some(-10.0),
        image_sd_db: Some(0.125),
    };
    let run = Run {
        output: vec![record.clone()],
        voice: vec![Record {
            kind: Kind::Music,
            ..record
        }],
        columns: vec![Column {
            genre: "Jazz".to_owned(),
            protocol: "steady".to_owned(),
            scores: vec![("Classical".to_owned(), -0.5), ("Jazz".to_owned(), 0.25)],
        }],
    };
    assert_eq!(run_from_json(&to_json(&run)), run);
}

#[test]
fn every_voice_signal_is_long_enough_to_gate_and_the_music_is_what_genre_voicing_renders() {
    for material in voice_materials() {
        assert_eq!(material.samples.len(), VOICE_FRAMES, "{}", material.name);
    }
    assert!((CAPTURE_RATE - RATE).abs() < f32::EPSILON);
    // The genre material's last pass is the pass `genre_voicing.rs` renders, sample for sample.
    let baseline = load_reference(BASELINE_FILE);
    let pass = material(&baseline.level_db, seed_for("Jazz"), dynamics_for("Jazz"));
    let jazz = output_materials()
        .into_iter()
        .find(|m| m.name == "genre/Jazz")
        .expect("a Jazz material");
    assert_eq!(&jazz.samples[jazz.measure_from * CHANNELS..], &pass[..]);
    // And the loud material is as loud as it claims.
    let loud = output_materials()
        .into_iter()
        .find(|m| m.name == "loud")
        .expect("a loud material");
    let level = 20.0 * rms(&loud.samples).log10();
    assert!((level - LOUD_RMS_DBFS).abs() < 0.05, "loud is {level} dBFS");
}
