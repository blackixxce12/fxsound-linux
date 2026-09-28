//! Does the output lane render every shipped preset bit for bit as another build renders it?
//!
//! The acceptance test of «Like FxSound for Windows» (`docs/0.5.0-windows-parity.md`, roadmap
//! 0.5.0 §1 A1 and A3): at "Interface and sound" the output lane must play as the engine before
//! the 0.4.0 audit's fixes played, and at Off as 0.4.0 plays. Both are the same question about two
//! builds — the working tree, and a checkout of the commit it is held to — so this file is both
//! halves of it. Run in the working tree, it starts the other build's copy of itself, hands it
//! every case over a socket, renders each case itself, and compares the two renders sample by
//! sample; run by the working tree, it is the other build's renderer.
//!
//! **The parameters are the application's.** Each side reads the `.fac` with its own parser and
//! turns it into `DspParams` with its own application's reading,
//! [`fxsound_dsp::preset::preset_params`] — the effects through the sliders, the curve on the
//! user's band count, the settings' levels — and sanitises them as the engine handle does. A
//! harness that copied the file's raw values into the engine would pass while the application
//! played a preset differently (`preset_drift.rs` did, until 0.5.0). A commit older than 0.5.0
//! has no `fxsound_dsp::preset`: `scripts/reference-checkout.sh` writes it there from that
//! commit's own application.
//!
//! It is `#[ignore]`d because it takes two builds; `scripts/windows-parity-bitexact.sh` makes
//! the reference checkout, builds both and runs it:
//!
//! ```text
//! scripts/windows-parity-bitexact.sh v0.4.0                            # A3: Off plays as 0.4.0
//! scripts/windows-parity-bitexact.sh --compat=windows --report 3596e99 # A1: Interface and sound
//! ```
//!
//! **Whose DSP the working tree plays** is `FXSOUND_BITEXACT_COMPAT`: `linux`, the default, for
//! Off (A3), and `windows` for Interface and sound (A1), where the output lane plays the Windows
//! build's arithmetic ([`fxsound_core::DspCompat`]). It reaches the engine the way the
//! application's level does, through [`MusicLevels`] and [`preset_params`]. Both sides are told;
//! a build with one DSP only, as every reference is so far, plays it either way
//! (`scripts/reference-checkout.sh` gives it a `MusicLevels::with_windows_dsp` that changes
//! nothing).
//!
//! # The cases
//!
//! Two passes, each over every shipped `.fac` (`Factsoft/`, `BonusPresets/`) × the band counts
//! 10, 20 and 31 × every piece of `preset_drift.rs`'s output material × blocks of 480, 512, 1024
//! and 2048 frames × two sets of levels: the settings' defaults, and −6 dB of master gain, +3 dB
//! of balance and a filter width of 2 — the gain stage and the Q multiplier, which the defaults
//! leave at unity. Every render is at 48 kHz in stereo, from a new engine, which is handed the
//! snapshot before every block, as the lane's audio thread is. The material's own Volume
//! Leveling (`leveling/…`) replaces the levels' one.
//!
//! * **Cold** (`cold`): the preset from the first sample to the last.
//! * **Switching** (`switching`, A1's second pass): the same start, then three things the user
//!   does while it plays, each at the first block that starts past a quarter, a half and three
//!   quarters of the material — another preset picked (the next shipped one), the band count
//!   changed (10 → 20 → 31 → 10) and one effect's slider moved to another whole position (the
//!   effect after the preset's place in the list; to 8 from below 5, else to 2). The application
//!   reads the picked preset on the live ladder, and after a band count change reads the same
//!   unedited preset again on the new ladder and sends `ResetFilterState`, which the lane handles
//!   after the snapshot, before the block: each build does this through its own
//!   [`preset_params`], so the snapshots are its application's, and the events are the working
//!   tree's application's on both sides (below). The [`GLIDE_SECONDS`] after each switch —
//!   the glide it starts — are not compared.
//!
//! The actions are one script on both sides because they are what is held still, and
//! `3596e99`'s application answered two of them differently: it cleared the filters on every
//! preset picked, and it changed the band count by remapping the curve on screen by position
//! instead of reading the preset again. The first is a transition (#9–#11), which stays at every
//! level; the second is the application's reading of a curve (#13, #45, W1c), not the engine's.
//! Here both builds read the preset again on the new ladder, so what follows each switch asks
//! the cold pass's question of an engine that has been playing: that it settles into the same
//! sound after a change as from a cold start. How long after a switch a case still differs past
//! the limit is in the report (`past_limit_ms`), for tuning the settling time;
//! `FXSOUND_BITEXACT_SETTLE_MS` lengthens it.
//!
//! # What passes
//!
//! A case passes when the two renders are identical, or differ nowhere by more than
//! [`DEFAULT_LIMIT_DB`] (−120 dBFS) — in the switching pass, nowhere outside the settling time
//! after each switch. Both builds must be made by the same toolchain on the same
//! machine: nothing here is stored, and nothing promises that two versions of rustc or two
//! machines' maths libraries round alike.
//!
//! The environment:
//!
//! | variable | default | what it changes |
//! | --- | --- | --- |
//! | `FXSOUND_BITEXACT_REFERENCE` | — | the reference build's copy of this test (required) |
//! | `FXSOUND_BITEXACT_COMPAT` | `linux` | `windows` plays the Windows build's DSP (Interface and sound) |
//! | `FXSOUND_BITEXACT_EXPECT` | `match` | `report` prints what differs and passes |
//! | `FXSOUND_BITEXACT_LIMIT_DB` | `-120` | the largest difference a case may have, in dBFS |
//! | `FXSOUND_BITEXACT_BANDS` | `10,20,31` | the band counts |
//! | `FXSOUND_BITEXACT_BLOCKS` | `480,512,1024,2048` | the block sizes, in frames |
//! | `FXSOUND_BITEXACT_PRESETS` | every one | only the presets whose `dir/stem` contains this |
//! | `FXSOUND_BITEXACT_PASSES` | `cold,switching` | the passes |
//! | `FXSOUND_BITEXACT_LEVELS` | `settings,gain` | the sets of levels, by name |
//! | `FXSOUND_BITEXACT_SETTLE_MS` | `20` ([`GLIDE_SECONDS`]) | how long after a switch goes uncompared |
//! | `FXSOUND_BITEXACT_OUT` | `target/windows-parity-bitexact.tsv` | every case's result |

use std::collections::{BTreeMap, HashMap};
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use fxsound_core::messages::{DspEvent, DspParams};
use fxsound_core::{Effect, scale};
use fxsound_dsp::Engine;
use fxsound_dsp::preset::{MusicLevels, ladder, preset_params};

#[allow(dead_code)]
mod genre_material;
#[allow(dead_code)]
mod output_material;

use genre_material::{CHANNELS, RATE, repo_root};
use output_material::{Material, output_materials};

/// Where the reference side finds the working tree's socket.
const SERVE_VAR: &str = "FXSOUND_BITEXACT_SERVE";
/// How many connections the reference side opens.
const WORKERS_VAR: &str = "FXSOUND_BITEXACT_WORKERS";
const REFERENCE_VAR: &str = "FXSOUND_BITEXACT_REFERENCE";
const EXPECT_VAR: &str = "FXSOUND_BITEXACT_EXPECT";
const LIMIT_VAR: &str = "FXSOUND_BITEXACT_LIMIT_DB";
const BANDS_VAR: &str = "FXSOUND_BITEXACT_BANDS";
const BLOCKS_VAR: &str = "FXSOUND_BITEXACT_BLOCKS";
const PRESETS_VAR: &str = "FXSOUND_BITEXACT_PRESETS";
const OUT_VAR: &str = "FXSOUND_BITEXACT_OUT";
const PASSES_VAR: &str = "FXSOUND_BITEXACT_PASSES";
const SETTLE_VAR: &str = "FXSOUND_BITEXACT_SETTLE_MS";
const COMPAT_VAR: &str = "FXSOUND_BITEXACT_COMPAT";
const LEVELS_VAR: &str = "FXSOUND_BITEXACT_LEVELS";

/// This test's own name, which the working tree runs the reference build's copy of by.
const TEST_NAME: &str = "every_shipped_preset_renders_as_the_reference_build_renders_it";

/// Bumped whenever the two sides' messages change, so a stale copy says so instead of misreading.
const PROTOCOL: u32 = 3;

const DEFAULT_LIMIT_DB: f64 = -120.0;
const DEFAULT_BANDS: [usize; 3] = [10, 20, 31];
const DEFAULT_BLOCKS: [usize; 4] = [480, 512, 1024, 2048];
/// The engine is built for the largest block any case uses, as the lane's is for the largest
/// quantum.
const MAX_BLOCK: usize = 2048;

/// How long a change glides for: the engine's `smooth::GLIDE_SECONDS`, written out because the
/// reference builds need not have that module (`3596e99` has none), and held to it by
/// [`the_settling_time_is_the_engines_glide`].
const GLIDE_SECONDS: f32 = 0.020;

/// The band count the switching pass changes to from each of [`DEFAULT_BANDS`]; any other count
/// goes to the first of them.
fn next_band_count(count: usize) -> usize {
    DEFAULT_BANDS
        .iter()
        .position(|&c| c == count)
        .map_or(DEFAULT_BANDS[0], |i| {
            DEFAULT_BANDS[(i + 1) % DEFAULT_BANDS.len()]
        })
}

/// Where the switching pass's three switches are due, in frames: a quarter, a half and three
/// quarters of the way through `frames`. Each lands at the first block that starts there or later.
const fn switches_due(frames: usize) -> [usize; 3] {
    [frames / 4, frames / 2, frames / 4 * 3]
}

/// The slider position the switching pass moves an effect to from `slider`: a whole position
/// well away from it.
fn moved_slider(slider: f32) -> f32 {
    if slider < 5.0 { 8.0 } else { 2.0 }
}

/// The levels a case plays with, by name.
fn level_sets() -> [(&'static str, MusicLevels); 2] {
    [
        ("settings", MusicLevels::default()),
        (
            "gain",
            MusicLevels {
                filter_q: 2.0,
                master_gain_db: -6.0,
                balance_db: 3.0,
                volume_leveling: 0.0,
                // Named field by field above, so that the copy compiles in a build whose
                // `MusicLevels` has nothing more; the DSP is the case's (`Request::windows`).
                ..MusicLevels::default()
            },
        ),
    ]
}

/// The sets of levels `FXSOUND_BITEXACT_LEVELS` names, all of them by default.
fn chosen_level_sets() -> Vec<(&'static str, MusicLevels)> {
    let all = level_sets();
    std::env::var(LEVELS_VAR).map_or_else(
        |_| all.to_vec(),
        |list| {
            list.split(',')
                .map(|name| {
                    *all.iter()
                        .find(|(set, _)| *set == name.trim())
                        .unwrap_or_else(|| panic!("{LEVELS_VAR}: no set of levels {name}"))
                })
                .collect()
        },
    )
}

/// Whether the working tree plays the Windows build's DSP: `FXSOUND_BITEXACT_COMPAT`.
fn windows_dsp() -> bool {
    match std::env::var(COMPAT_VAR).as_deref().map(str::trim) {
        Err(_) | Ok("" | "linux") => false,
        Ok("windows") => true,
        Ok(other) => panic!("{COMPAT_VAR}: {other} is neither linux nor windows"),
    }
}

// ---------------------------------------------------------------------------------------------
// Rendering: the same on both sides
// ---------------------------------------------------------------------------------------------

/// What the user does during a render of the switching pass.
#[derive(Clone, Debug, PartialEq)]
struct Switches {
    /// The preset picked at a quarter of the way.
    preset: PathBuf,
    /// The band count at half way, the picked preset read again on its ladder.
    bands: u32,
    /// The effect whose slider moves at three quarters, as its place in `Effect::ALL`.
    effect: u32,
}

/// One case as the wire carries it.
#[derive(Clone, Debug, PartialEq)]
struct Request {
    preset: PathBuf,
    bands: u32,
    levels: [f32; 4],
    block: u32,
    material: u32,
    /// `None` for the cold pass.
    switches: Option<Switches>,
    /// The Windows build's DSP ([`windows_dsp`]).
    windows: bool,
}

impl Request {
    fn levels(&self) -> MusicLevels {
        let [filter_q, master_gain_db, balance_db, volume_leveling] = self.levels;
        MusicLevels {
            filter_q,
            master_gain_db,
            balance_db,
            volume_leveling,
            ..MusicLevels::default()
        }
        .with_windows_dsp(self.windows)
    }
}

/// The snapshot's fields both builds have, as numbers, so that the working tree can say whether a
/// difference came from the parameters or from the engine.
fn snapshot_numbers(params: &DspParams) -> Vec<f32> {
    let flag = |on: bool| if on { 1.0 } else { 0.0 };
    let (centres, gains) = params.bands();
    let mut numbers = vec![flag(params.power), flag(params.mute), flag(params.eq_on)];
    numbers.extend_from_slice(&params.effects);
    numbers.push(f32::from(params.num_bands));
    numbers.extend_from_slice(centres);
    numbers.extend_from_slice(gains);
    numbers.extend([
        params.filter_q,
        params.master_gain_db,
        params.balance,
        params.volume_leveling_db,
    ]);
    numbers
}

/// One render, and what it was made from.
struct Render {
    samples: Vec<f32>,
    /// [`snapshot_numbers`] of every snapshot the engine was handed, one after the other.
    numbers: Vec<f32>,
    /// The frames the switches landed at, in order; empty in the cold pass.
    switched_at: Vec<usize>,
}

/// A preset as this build's application plays it on `bands` bands.
fn read_preset(path: &Path, bands: u32, levels: MusicLevels) -> DspParams {
    let preset =
        fxsound_preset::load(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    let mut params = preset_params(&preset, &ladder(bands as usize), levels);
    params.sanitise();
    params
}

/// `request` through this build: its parser, its application's reading, its engine.
fn render(request: &Request, material: &[f32]) -> Render {
    let levels = request.levels();
    let mut params = read_preset(&request.preset, request.bands, levels);
    let mut numbers = snapshot_numbers(&params);
    let mut switched_at = Vec::new();
    let due = switches_due(material.len() / CHANNELS);
    let frames = request.block as usize;
    let mut engine = Engine::new(RATE, MAX_BLOCK, CHANNELS);
    let mut buffer = material.to_vec();
    for (index, block) in buffer.chunks_mut(frames * CHANNELS).enumerate() {
        let start = index * frames;
        let mut reset = false;
        if let Some(switches) = &request.switches
            && let Some(&when) = due.get(switched_at.len())
            && start >= when
        {
            match switched_at.len() {
                0 => params = read_preset(&switches.preset, request.bands, levels),
                1 => {
                    params = read_preset(&switches.preset, switches.bands, levels);
                    reset = true;
                }
                _ => {
                    let effect = Effect::ALL[switches.effect as usize % Effect::COUNT];
                    let slider = scale::value_to_slider_for(effect, params.effect(effect));
                    params.set_effect(
                        effect,
                        scale::slider_to_value_for(effect, moved_slider(slider)),
                    );
                    params.sanitise();
                }
            }
            switched_at.push(start);
            numbers.extend(snapshot_numbers(&params));
        }
        // The lane's order: the newest snapshot, then the events waiting, then the block.
        engine.apply(&params);
        if reset {
            engine.handle_event(DspEvent::ResetFilterState);
        }
        engine.process(block, CHANNELS);
    }
    Render {
        samples: buffer,
        numbers,
        switched_at,
    }
}

// ---------------------------------------------------------------------------------------------
// The wire
// ---------------------------------------------------------------------------------------------

fn put_u32(out: &mut impl Write, value: u32) {
    out.write_all(&value.to_le_bytes())
        .expect("the socket takes it");
}

fn put_floats(out: &mut impl Write, values: &[f32]) {
    put_u32(
        out,
        u32::try_from(values.len()).expect("fewer than 2^32 samples"),
    );
    let mut bytes = Vec::with_capacity(values.len() * 4);
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    out.write_all(&bytes).expect("the socket takes it");
}

fn get_u32(input: &mut impl Read) -> Option<u32> {
    let mut bytes = [0; 4];
    input.read_exact(&mut bytes).ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn get_floats(input: &mut impl Read) -> Option<Vec<f32>> {
    let len = get_u32(input)? as usize;
    let mut bytes = vec![0; len * 4];
    input.read_exact(&mut bytes).ok()?;
    Some(
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect(),
    )
}

fn put_path(out: &mut impl Write, path: &Path) {
    let path = path.to_str().expect("a preset path is UTF-8");
    put_u32(out, u32::try_from(path.len()).expect("a short path"));
    out.write_all(path.as_bytes()).expect("the socket takes it");
}

fn get_path(input: &mut impl Read) -> Option<PathBuf> {
    let len = get_u32(input)? as usize;
    let mut path = vec![0; len];
    input.read_exact(&mut path).ok()?;
    Some(PathBuf::from(String::from_utf8(path).ok()?))
}

fn put_request(out: &mut impl Write, request: &Request, material: Option<&[f32]>) {
    put_u32(out, PROTOCOL);
    put_path(out, &request.preset);
    put_u32(out, request.bands);
    put_floats(out, &request.levels);
    put_u32(out, request.block);
    put_u32(out, request.material);
    put_u32(out, u32::from(request.windows));
    match &request.switches {
        Some(switches) => {
            put_u32(out, 1);
            put_path(out, &switches.preset);
            put_u32(out, switches.bands);
            put_u32(out, switches.effect);
        }
        None => put_u32(out, 0),
    }
    match material {
        Some(samples) => {
            put_u32(out, 1);
            put_floats(out, samples);
        }
        None => put_u32(out, 0),
    }
    out.flush().expect("the socket takes it");
}

/// A request, and the material it carries the first time a connection names it. `None` at the end
/// of the stream.
fn get_request(input: &mut impl Read) -> Option<(Request, Option<Vec<f32>>)> {
    let protocol = get_u32(input)?;
    assert_eq!(
        protocol, PROTOCOL,
        "the working tree speaks protocol {protocol}, this copy {PROTOCOL}: copy the harness again"
    );
    let preset = get_path(input)?;
    let bands = get_u32(input)?;
    let levels: [f32; 4] = get_floats(input)?.try_into().ok()?;
    let block = get_u32(input)?;
    let material = get_u32(input)?;
    let windows = get_u32(input)? != 0;
    let switches = match get_u32(input)? {
        0 => None,
        _ => Some(Switches {
            preset: get_path(input)?,
            bands: get_u32(input)?,
            effect: get_u32(input)?,
        }),
    };
    let samples = match get_u32(input)? {
        0 => None,
        _ => Some(get_floats(input)?),
    };
    let request = Request {
        preset,
        bands,
        levels,
        block,
        material,
        switches,
        windows,
    };
    Some((request, samples))
}

// ---------------------------------------------------------------------------------------------
// The reference side
// ---------------------------------------------------------------------------------------------

/// Serve the working tree: render each request it sends and send the render back, one thread per
/// connection, until it hangs up.
fn serve(socket: &Path) {
    let workers: usize = std::env::var(WORKERS_VAR)
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(1);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                let stream = UnixStream::connect(socket)
                    .unwrap_or_else(|err| panic!("{}: {err}", socket.display()));
                let mut input = BufReader::new(stream.try_clone().expect("a second handle"));
                let mut output = BufWriter::new(stream);
                let mut materials: HashMap<u32, Vec<f32>> = HashMap::new();
                while let Some((request, samples)) = get_request(&mut input) {
                    if let Some(samples) = samples {
                        materials.insert(request.material, samples);
                    }
                    let material = materials
                        .get(&request.material)
                        .expect("the working tree sends a material before naming it");
                    let rendered = render(&request, material);
                    put_floats(&mut output, &rendered.samples);
                    put_floats(&mut output, &rendered.numbers);
                    output.flush().expect("the socket takes it");
                }
            });
        }
    });
}

// ---------------------------------------------------------------------------------------------
// The working tree's side
// ---------------------------------------------------------------------------------------------

fn list_var(name: &str, default: &[usize]) -> Vec<usize> {
    std::env::var(name).map_or_else(
        |_| default.to_vec(),
        |list| {
            list.split(',')
                .map(|n| {
                    n.trim()
                        .parse()
                        .unwrap_or_else(|_| panic!("{name}: {n} is not a number"))
                })
                .collect()
        },
    )
}

/// Every shipped `.fac`, keyed by directory and file stem.
fn shipped_presets() -> Vec<(String, PathBuf)> {
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
            let stem = file
                .file_stem()
                .and_then(|s| s.to_str())
                .expect("a preset file name is UTF-8");
            out.push((format!("{dir}/{stem}"), file));
        }
    }
    out
}

/// The two passes, by the names `FXSOUND_BITEXACT_PASSES` and the report give them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Pass {
    Cold,
    Switching,
}

impl Pass {
    const ALL: [Self; 2] = [Self::Cold, Self::Switching];

    const fn name(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::Switching => "switching",
        }
    }
}

fn passes() -> Vec<Pass> {
    std::env::var(PASSES_VAR).map_or_else(
        |_| Pass::ALL.to_vec(),
        |list| {
            list.split(',')
                .map(|name| {
                    Pass::ALL
                        .into_iter()
                        .find(|pass| pass.name() == name.trim())
                        .unwrap_or_else(|| panic!("{PASSES_VAR}: no pass {name}"))
                })
                .collect()
        },
    )
}

/// The frames after each switch that are not compared: [`GLIDE_SECONDS`], or
/// `FXSOUND_BITEXACT_SETTLE_MS`.
fn settle_frames() -> usize {
    let seconds = std::env::var(SETTLE_VAR).ok().map_or(GLIDE_SECONDS, |ms| {
        ms.trim()
            .parse::<f32>()
            .unwrap_or_else(|_| panic!("{SETTLE_VAR}: {ms} is not a number"))
            / 1000.0
    });
    (seconds * RATE).round() as usize
}

/// One case, named for the report.
struct Case {
    pass: Pass,
    preset: String,
    levels: &'static str,
    request: Request,
}

/// How far apart two renders of one case came out.
#[derive(Clone, Debug)]
struct Outcome {
    /// The largest difference between two samples, in dBFS; `None` when the renders are
    /// identical.
    worst_db: Option<f64>,
    /// How many samples differ at all.
    differing: usize,
    /// The first frame that differs.
    first_frame: Option<usize>,
    /// Whether the two builds handed their engines the same snapshots.
    same_params: bool,
    /// In the switching pass, the longest a difference past the limit lasted after a switch
    /// landed, settling time included, in frames; `None` when none did.
    past_limit_after_switch: Option<usize>,
}

/// Compare two renders, leaving out the `settle` frames from each frame in `switched_at`; a
/// difference past `limit_db` after a switch, compared or not, is timed from the switch.
fn compare(
    ours: &[f32],
    theirs: &[f32],
    switched_at: &[usize],
    settle: usize,
    limit_db: f64,
) -> Outcome {
    assert_eq!(ours.len(), theirs.len(), "the two renders differ in length");
    let limit = 10f64.powf(limit_db / 20.0);
    let mut worst = 0.0f64;
    let mut differing = 0;
    let mut first = None;
    let mut past_limit_after_switch = None;
    for (index, (a, b)) in ours.iter().zip(theirs).enumerate() {
        if a.to_bits() == b.to_bits() {
            continue;
        }
        let frame = index / CHANNELS;
        let difference = (f64::from(*a) - f64::from(*b)).abs();
        let difference = if difference.is_nan() {
            f64::INFINITY
        } else {
            difference
        };
        let since = switched_at
            .iter()
            .rev()
            .find(|&&at| at <= frame)
            .map(|&at| frame - at);
        if let Some(since) = since
            && difference >= limit
        {
            past_limit_after_switch = Some(past_limit_after_switch.unwrap_or(0).max(since + 1));
        }
        if since.is_some_and(|since| since < settle) {
            continue;
        }
        differing += 1;
        first.get_or_insert(frame);
        worst = worst.max(difference);
    }
    Outcome {
        worst_db: (differing > 0).then(|| 20.0 * worst.max(1e-300).log10()),
        differing,
        first_frame: first,
        same_params: true,
        past_limit_after_switch,
    }
}

fn socket_path() -> PathBuf {
    // A socket's path has to fit 108 bytes, which a checkout's target directory need not.
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    dir.join(format!("fxsound-bitexact-{}.sock", std::process::id()))
}

/// The bound socket's file, removed when this goes out of scope: after every worker connected,
/// or when anything on the way there panics, so that no run leaves a socket behind.
struct SocketFile(PathBuf);

impl Drop for SocketFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// How long the reference build has to connect all its workers.
const CONNECT_DEADLINE: Duration = Duration::from_mins(5);

/// The reference build's `workers` connections. A build that exits before it connects — one
/// without the test, one that panics at startup — or that does not connect within
/// [`CONNECT_DEADLINE`] fails the test instead of hanging it.
fn connections(
    listener: &UnixListener,
    child: &mut Child,
    reference: &Path,
    workers: usize,
) -> Vec<UnixStream> {
    listener
        .set_nonblocking(true)
        .expect("a listener that does not block");
    let deadline = Instant::now() + CONNECT_DEADLINE;
    let mut streams = Vec::with_capacity(workers);
    while streams.len() < workers {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .expect("a connection that blocks");
                streams.push(stream);
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if let Some(status) = child.try_wait().expect("the reference build's status") {
                    panic!(
                        "{} exited before it connected ({} of {workers} workers): {status}",
                        reference.display(),
                        streams.len()
                    );
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "{} did not connect within {CONNECT_DEADLINE:?} ({} of {workers} workers)",
                        reference.display(),
                        streams.len()
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the reference build connects: {err}");
            }
        }
    }
    streams
}

fn run_against(reference: &Path) {
    let expect_match = std::env::var(EXPECT_VAR).map_or(true, |mode| mode != "report");
    let limit_db: f64 = std::env::var(LIMIT_VAR)
        .ok()
        .and_then(|db| db.parse().ok())
        .unwrap_or(DEFAULT_LIMIT_DB);
    let bands = list_var(BANDS_VAR, &DEFAULT_BANDS);
    let blocks = list_var(BLOCKS_VAR, &DEFAULT_BLOCKS);
    let passes = passes();
    let settle = settle_frames();
    let windows = windows_dsp();
    let level_sets = chosen_level_sets();
    assert!(
        blocks.iter().all(|&b| (1..=MAX_BLOCK).contains(&b)),
        "blocks go from 1 to {MAX_BLOCK} frames"
    );

    let materials: Vec<Material> = output_materials();
    let shipped = shipped_presets();
    let filter = std::env::var(PRESETS_VAR).unwrap_or_default();
    let presets: Vec<usize> = (0..shipped.len())
        .filter(|&i| shipped[i].0.contains(&filter))
        .collect();
    assert!(!presets.is_empty(), "no preset matches {PRESETS_VAR}");
    let mut cases = Vec::new();
    for &pass in &passes {
        for &which in &presets {
            let (key, file) = &shipped[which];
            for &count in &bands {
                // The next shipped preset whatever the filter, so that a narrowed run still
                // switches to another one.
                let switches = (pass == Pass::Switching).then(|| Switches {
                    preset: shipped[(which + 1) % shipped.len()].1.clone(),
                    bands: u32::try_from(next_band_count(count)).expect("a band count"),
                    effect: u32::try_from((which + 1) % Effect::COUNT).expect("an effect"),
                });
                for &(name, levels) in &level_sets {
                    for (index, material) in materials.iter().enumerate() {
                        for &block in &blocks {
                            cases.push(Case {
                                pass,
                                preset: key.clone(),
                                levels: name,
                                request: Request {
                                    preset: file.clone(),
                                    bands: u32::try_from(count).expect("a band count"),
                                    levels: [
                                        levels.filter_q,
                                        levels.master_gain_db,
                                        levels.balance_db,
                                        material.leveling,
                                    ],
                                    block: u32::try_from(block).expect("a block size"),
                                    material: u32::try_from(index).expect("a material index"),
                                    switches: switches.clone(),
                                    windows,
                                },
                            });
                        }
                    }
                }
            }
        }
    }

    let workers = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(cases.len());
    let socket = socket_path();
    let _ = std::fs::remove_file(&socket);
    let listener =
        UnixListener::bind(&socket).unwrap_or_else(|err| panic!("{}: {err}", socket.display()));
    let socket_file = SocketFile(socket);
    let mut child = Command::new(reference)
        .args([TEST_NAME, "--exact", "--ignored", "--test-threads", "1"])
        .env(SERVE_VAR, &socket_file.0)
        .env(WORKERS_VAR, workers.to_string())
        .stdout(Stdio::null())
        .spawn()
        .unwrap_or_else(|err| panic!("{}: {err}", reference.display()));
    let streams = connections(&listener, &mut child, reference, workers);
    drop(socket_file);

    eprintln!(
        "windows_parity_bitexact: {} cases ({:?} passes × {} presets × {:?} bands × {:?} levels × {} \
         materials × {:?} frames; {settle} frames after a switch not compared; {} DSP) on \
         {workers} threads against {}",
        cases.len(),
        passes.iter().map(|pass| pass.name()).collect::<Vec<_>>(),
        presets.len(),
        bands,
        level_sets.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        materials.len(),
        blocks,
        if windows { "the Windows" } else { "the Linux" },
        reference.display()
    );
    let next = AtomicUsize::new(0);
    let outcomes: Mutex<Vec<Option<Outcome>>> = Mutex::new(vec![None; cases.len()]);
    std::thread::scope(|scope| {
        for stream in streams {
            let (cases, materials, next, outcomes) = (&cases, &materials, &next, &outcomes);
            scope.spawn(move || {
                let mut input = BufReader::new(stream.try_clone().expect("a second handle"));
                let mut output = BufWriter::new(stream);
                let mut sent = vec![false; materials.len()];
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(case) = cases.get(index) else {
                        break;
                    };
                    let which = case.request.material as usize;
                    let samples = &materials[which].samples;
                    let first_time = !std::mem::replace(&mut sent[which], true);
                    put_request(
                        &mut output,
                        &case.request,
                        first_time.then_some(samples.as_slice()),
                    );
                    let ours = render(&case.request, samples);
                    let theirs = get_floats(&mut input).expect("the reference build renders it");
                    let their_numbers =
                        get_floats(&mut input).expect("the reference build sends its snapshot");
                    let mut outcome =
                        compare(&ours.samples, &theirs, &ours.switched_at, settle, limit_db);
                    outcome.same_params = ours.numbers.len() == their_numbers.len()
                        && ours
                            .numbers
                            .iter()
                            .zip(&their_numbers)
                            .all(|(a, b)| a.to_bits() == b.to_bits());
                    outcomes.lock().expect("no worker panicked")[index] = Some(outcome);
                }
            });
        }
    });
    let status = child.wait().expect("the reference build exits");
    assert!(status.success(), "the reference build failed: {status}");
    let outcomes: Vec<Outcome> = outcomes
        .into_inner()
        .expect("no worker panicked")
        .into_iter()
        .map(|outcome| outcome.expect("every case was compared"))
        .collect();

    report(&cases, &materials, &outcomes, limit_db, expect_match);
}

fn report(
    cases: &[Case],
    materials: &[Material],
    outcomes: &[Outcome],
    limit_db: f64,
    expect_match: bool,
) {
    let out = std::env::var_os(OUT_VAR).map_or_else(
        || repo_root().join("target/windows-parity-bitexact.tsv"),
        PathBuf::from,
    );
    let mut tsv = String::from(
        "pass\tpreset\tbands\tlevels\tmaterial\tblock\tworst_dbfs\tdiffering_samples\tfirst_frame\t\
         same_params\tpast_limit_ms\n",
    );
    // Per pass: identical, within the limit, past it.
    let mut totals: BTreeMap<&str, (usize, usize, usize)> = BTreeMap::new();
    // Per pass and preset, and per pass and material: how many cases differ past the limit, and
    // the worst of them.
    let mut by_preset: BTreeMap<(&str, &str), (usize, f64, bool)> = BTreeMap::new();
    let mut by_material: BTreeMap<(&str, &str), (usize, f64)> = BTreeMap::new();
    // The longest a difference past the limit lasted after a switch.
    let mut longest_after_switch: Option<usize> = None;
    for (case, outcome) in cases.iter().zip(outcomes) {
        let pass = case.pass.name();
        let material = materials[case.request.material as usize].name.as_str();
        let worst = outcome.worst_db;
        let past_limit_ms = outcome
            .past_limit_after_switch
            .map(|frames| frames as f64 * 1000.0 / f64::from(RATE));
        tsv.push_str(&format!(
            "{pass}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            case.preset,
            case.request.bands,
            case.levels,
            material,
            case.request.block,
            worst.map_or_else(|| "identical".to_owned(), |db| format!("{db:.1}")),
            outcome.differing,
            outcome
                .first_frame
                .map_or_else(String::new, |f| f.to_string()),
            outcome.same_params,
            past_limit_ms.map_or_else(String::new, |ms| format!("{ms:.1}")),
        ));
        longest_after_switch = longest_after_switch.max(outcome.past_limit_after_switch);
        let total = totals.entry(pass).or_default();
        match worst {
            None => total.0 += 1,
            Some(db) if db < limit_db => total.1 += 1,
            Some(db) => {
                total.2 += 1;
                let entry = by_preset
                    .entry((pass, &case.preset))
                    .or_insert((0, f64::MIN, true));
                entry.0 += 1;
                entry.1 = entry.1.max(db);
                entry.2 &= outcome.same_params;
                let entry = by_material.entry((pass, material)).or_insert((0, f64::MIN));
                entry.0 += 1;
                entry.1 = entry.1.max(db);
            }
        }
    }
    if let Some(parent) = out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&out, tsv).unwrap_or_else(|err| panic!("{}: {err}", out.display()));

    for (pass, (identical, below, above)) in &totals {
        eprintln!(
            "windows_parity_bitexact: {pass}: {identical} identical, {below} within {limit_db} \
             dBFS, {above} past it"
        );
    }
    if let Some(frames) = longest_after_switch {
        eprintln!(
            "windows_parity_bitexact: the longest a difference past {limit_db} dBFS lasted after \
             a switch: {:.1} ms",
            frames as f64 * 1000.0 / f64::from(RATE)
        );
    }
    eprintln!("windows_parity_bitexact: every case in {}", out.display());
    let above: usize = totals.values().map(|total| total.2).sum();
    if above > 0 {
        eprintln!(
            "\npast the limit, by pass and preset (cases, worst dBFS, snapshots the same in all):"
        );
        for ((pass, preset), (count, worst, same)) in &by_preset {
            eprintln!("  {pass:<10} {preset:<40} {count:>5} {worst:>8.1} {same}");
        }
        eprintln!("\npast the limit, by pass and material (cases, worst dBFS):");
        for ((pass, material), (count, worst)) in &by_material {
            eprintln!("  {pass:<10} {material:<40} {count:>5} {worst:>8.1}");
        }
    }
    if expect_match {
        assert_eq!(
            above, 0,
            "{above} cases differ from the reference build by more than {limit_db} dBFS"
        );
    }
}

#[test]
#[ignore = "takes a reference build: scripts/windows-parity-bitexact.sh"]
fn every_shipped_preset_renders_as_the_reference_build_renders_it() {
    if let Some(socket) = std::env::var_os(SERVE_VAR) {
        serve(Path::new(&socket));
        return;
    }
    let reference = std::env::var_os(REFERENCE_VAR).unwrap_or_else(|| {
        panic!("{REFERENCE_VAR} names no reference build; run scripts/windows-parity-bitexact.sh")
    });
    run_against(Path::new(&reference));
}

#[test]
fn a_case_crosses_the_wire_as_it_was_sent() {
    let cold = Request {
        preset: PathBuf::from("assets/presets/BonusPresets/Jazz.fac"),
        bands: 31,
        levels: [2.0, -6.0, 3.0, 4.0],
        block: 480,
        material: 7,
        switches: None,
        windows: false,
    };
    let switching = Request {
        switches: Some(Switches {
            preset: PathBuf::from("assets/presets/BonusPresets/Metal.fac"),
            bands: 10,
            effect: 3,
        }),
        windows: true,
        ..cold.clone()
    };
    let samples = vec![0.25, -0.5, f32::MIN_POSITIVE, 1.0];
    let mut wire = Vec::new();
    put_request(&mut wire, &cold, Some(&samples));
    put_request(&mut wire, &cold, None);
    put_request(&mut wire, &switching, None);
    let mut input = wire.as_slice();
    assert_eq!(get_request(&mut input), Some((cold.clone(), Some(samples))));
    assert_eq!(get_request(&mut input), Some((cold, None)));
    assert_eq!(get_request(&mut input), Some((switching, None)));
    assert_eq!(get_request(&mut input), None);
}

#[test]
fn two_renders_that_differ_by_one_sample_report_it_in_dbfs() {
    let ours = vec![0.5, 0.5, 0.25, 0.25];
    let mut theirs = ours.clone();
    theirs[3] += 1.0e-3;
    let outcome = compare(&ours, &theirs, &[], 0, DEFAULT_LIMIT_DB);
    assert_eq!(outcome.differing, 1);
    assert_eq!(outcome.first_frame, Some(1));
    let db = outcome.worst_db.expect("a difference");
    assert!((db - -60.0).abs() < 0.1, "{db}");
    assert_eq!(outcome.past_limit_after_switch, None);
    assert_eq!(
        compare(&ours, &ours, &[], 0, DEFAULT_LIMIT_DB).worst_db,
        None
    );
}

#[test]
fn a_difference_inside_the_settling_time_is_timed_but_not_compared() {
    let ours = vec![0.0; 10 * CHANNELS];
    let mut theirs = ours.clone();
    // A switch at frame 4 settling for 3 frames: frames 4, 5 and 6 are not compared.
    theirs[5 * CHANNELS] = 0.5;
    let outcome = compare(&ours, &theirs, &[4], 3, DEFAULT_LIMIT_DB);
    assert_eq!(outcome.worst_db, None);
    assert_eq!(outcome.past_limit_after_switch, Some(2));
    // Frame 7 is.
    theirs[7 * CHANNELS] = 0.5;
    let outcome = compare(&ours, &theirs, &[4], 3, DEFAULT_LIMIT_DB);
    assert_eq!(outcome.differing, 1);
    assert_eq!(outcome.first_frame, Some(7));
    assert_eq!(outcome.past_limit_after_switch, Some(4));
    // Before the first switch everything is compared, and nothing is timed.
    theirs.fill(0.0);
    theirs[CHANNELS] = 0.5;
    let outcome = compare(&ours, &theirs, &[4], 3, DEFAULT_LIMIT_DB);
    assert_eq!(outcome.first_frame, Some(1));
    assert_eq!(outcome.past_limit_after_switch, None);
}

/// The Jazz preset, on 20 bands with the "gain" levels, in blocks of 480 frames.
fn jazz(switches: Option<Switches>) -> (String, Request) {
    let (key, file) = shipped_presets()
        .into_iter()
        .find(|(key, _)| key.ends_with("/Jazz"))
        .expect("the Jazz preset ships");
    let request = Request {
        preset: file,
        bands: 20,
        levels: [2.0, -6.0, 3.0, 0.0],
        block: 480,
        material: 0,
        switches,
        windows: false,
    };
    (key, request)
}

/// A second of a 40 Hz tone.
fn a_second_of_tone() -> Vec<f32> {
    let tone = output_material::tone("tone/40Hz", 40.0, 0.9, 1, 0.0);
    tone.samples[..48_000 * CHANNELS].to_vec()
}

#[test]
fn a_preset_renders_the_same_twice_in_one_build() {
    // The harness's premise: a render is a pure function of the case, so any difference between
    // two builds is the builds'.
    let material = a_second_of_tone();
    let presets = shipped_presets();
    for switches in [
        None,
        Some(Switches {
            preset: presets[0].1.clone(),
            bands: 31,
            effect: 0,
        }),
    ] {
        let (key, request) = jazz(switches);
        let first = render(&request, &material);
        let second = render(&request, &material);
        let outcome = compare(&first.samples, &second.samples, &[], 0, DEFAULT_LIMIT_DB);
        assert_eq!(outcome.worst_db, None, "{key}");
        assert_eq!(first.numbers, second.numbers);
        assert_eq!(first.switched_at, second.switched_at);
    }
}

#[test]
fn a_windows_case_is_played_by_the_windows_dsp() {
    // `FXSOUND_BITEXACT_COMPAT=windows` reaches the engine: the same case renders otherwise,
    // from the same snapshot but for whose DSP plays it.
    let material = a_second_of_tone();
    let (key, linux) = jazz(None);
    let windows = Request {
        windows: true,
        ..linux.clone()
    };
    let (linux, windows) = (render(&linux, &material), render(&windows, &material));
    assert_eq!(linux.numbers, windows.numbers, "{key}");
    assert_ne!(linux.samples, windows.samples, "{key}");
}

#[test]
fn the_switching_pass_switches_three_times_at_block_starts_past_its_quarters() {
    let material = a_second_of_tone();
    let presets = shipped_presets();
    let (_, cold) = jazz(None);
    let (key, switching) = jazz(Some(Switches {
        preset: presets
            .iter()
            .find(|(key, _)| key.ends_with("/Metal"))
            .expect("the Metal preset ships")
            .1
            .clone(),
        bands: 31,
        effect: Effect::Bass as u32,
    }));
    let cold = render(&cold, &material);
    let switched = render(&switching, &material);
    // 48 000 frames in blocks of 480: 12 000, 24 000 and 36 000 are block starts.
    assert_eq!(switched.switched_at, [12_000, 24_000, 36_000], "{key}");
    assert!(cold.switched_at.is_empty());
    // The snapshots: Jazz, Metal as the application reads it on 20 bands and again on 31, then
    // with Bass moved to another whole position.
    let metal = &switching.switches.as_ref().expect("switches").preset;
    let on_twenty = read_preset(metal, 20, switching.levels());
    let on_thirty_one = read_preset(metal, 31, switching.levels());
    let mut moved = on_thirty_one;
    let bass = scale::value_to_slider_for(Effect::Bass, moved.effect(Effect::Bass));
    moved.set_effect(
        Effect::Bass,
        scale::slider_to_value_for(Effect::Bass, moved_slider(bass)),
    );
    moved.sanitise();
    assert_ne!(moved, on_thirty_one, "the slider moves");
    let expected: Vec<f32> = cold
        .numbers
        .iter()
        .copied()
        .chain(snapshot_numbers(&on_twenty))
        .chain(snapshot_numbers(&on_thirty_one))
        .chain(snapshot_numbers(&moved))
        .collect();
    assert_eq!(switched.numbers, expected);
    // Until the first switch the render is the cold one, and after it, it is not.
    let until = 12_000 * CHANNELS;
    assert_eq!(switched.samples[..until], cold.samples[..until]);
    assert_ne!(switched.samples[until..], cold.samples[until..]);
}

#[test]
fn the_band_count_changes_to_each_of_the_others_in_turn() {
    assert_eq!(next_band_count(10), 20);
    assert_eq!(next_band_count(20), 31);
    assert_eq!(next_band_count(31), 10);
    assert_eq!(next_band_count(15), 10);
    assert_eq!(moved_slider(0.0), 8.0);
    assert_eq!(moved_slider(4.9), 8.0);
    assert_eq!(moved_slider(5.0), 2.0);
}

#[test]
fn the_settling_time_is_the_engines_glide() {
    // Only where the engine has `smooth.rs`: a reference build older than it runs its own copy of
    // this test never.
    let smooth = repo_root().join("crates/fxsound-dsp/src/smooth.rs");
    let Ok(source) = std::fs::read_to_string(&smooth) else {
        return;
    };
    let glide = source
        .lines()
        .find_map(|line| line.strip_prefix("pub const GLIDE_SECONDS: Real = "))
        .and_then(|rest| rest.strip_suffix(';'))
        .expect("smooth.rs defines GLIDE_SECONDS");
    assert_eq!(glide.parse::<f32>(), Ok(GLIDE_SECONDS));
}
