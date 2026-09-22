//! What every voice preset this project ships has to be true of.
//!
//! The mic-side analogue of `shipped_presets.rs`, and written *before* the set it guards, because
//! the defects it looks for are the ones a preset author cannot see: a number the engine quietly
//! clamps, a band that does more than it says because its neighbour adds to it, and two presets
//! that are the same chain under different names.
//!
//! Two checks here that the output side does not need:
//!
//! * **effective gain against stored gain, per band.** On the fixed ladder at a derived Q of about
//!   1.6, adjacent bands overlap; the output test measures whether the summed curve exceeds its
//!   largest band, which catches the gross case. A voice set whose every move is under two
//!   decibels needs the finer question: does band 6 actually do the +1.5 dB it claims?
//! * **distinctness across the whole chain**, not only the curve. Two voice presets can share an
//!   equalizer exactly and still be different instruments, because the gate, the compressor and
//!   the de-esser carry most of what a listener hears. Comparing curves alone would call Flat and
//!   Clean Voice near-identical, which they are not.
//!
//! None of this goes near the `.fac` path: voice presets are TOML, and the byte-exact contract
//! with the Windows build is untouched.

use fxsound_core::messages::InputDspParams;
use fxsound_dsp::ChainSpec;
use fxsound_dsp::eq::GraphicEq;
use fxsound_preset::input::InputPreset;
use std::path::{Path, PathBuf};

/// The rate the capture stream asks for, and therefore the rate every preset is voiced at.
const CAPTURE_RATE: f32 = 48_000.0;

/// How far a band's realised gain may sit from the gain it stores.
///
/// Not zero, and cannot be: the bands overlap by design, so a band's realised gain always carries
/// some of its neighbours. This is the line between "carries some of its neighbours" and "is not
/// the control it claims to be" — half a decibel, against a set whose largest move is two.
const MAX_BAND_ERROR_DB: f32 = 0.5;

/// Below this, two presets are the same chain wearing two names.
///
/// The output side compares curves at a decibel of separation. Here the comparison is over the
/// whole chain, normalised so that a decibel of equalizer and a decibel of compression threshold
/// count alike, and the bar is lower because the voice set is smaller and more deliberate.
const INDISTINGUISHABLE: f32 = 1.0;

fn preset_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root")
        .join("assets/presets/Input")
}

fn shipped() -> Vec<InputPreset> {
    let dir = preset_dir();
    let presets =
        InputPreset::load_dir(&dir).unwrap_or_else(|err| panic!("{}: {err}", dir.display()));
    assert!(
        !presets.is_empty(),
        "{} holds no voice presets; a test over an empty set cannot fail",
        dir.display()
    );
    presets
}

/// The equalizer a preset describes, at the rate it is voiced for.
fn equalizer(params: &InputDspParams) -> GraphicEq {
    let mut eq = GraphicEq::new();
    eq.set_sample_rate(CAPTURE_RATE);
    eq.set_q_multiplier(params.filter_q);
    let (centers, gains) = params.bands();
    eq.set_bands(centers, gains);
    eq
}

#[test]
fn no_preset_carries_a_number_the_engine_would_correct() {
    // The cheapest class of defect and the least visible: a preset author writes a value, the file
    // records it, `sanitise` pulls it back on the way to the audio thread, and the preset sounds
    // like something nobody wrote. Every shipped preset must already be inside every limit.
    let mut wrong = Vec::new();
    for preset in shipped() {
        let raw = preset.to_params();
        let mut sane = raw;
        sane.sanitise();
        if sane != raw {
            wrong.push(preset.name.clone());
        }
    }
    assert!(
        wrong.is_empty(),
        "preset(s) carrying values the engine would change: {}",
        wrong.join(", ")
    );
}

#[test]
fn every_gate_that_exists_caps_its_own_attenuation() {
    // The field the whole design review turned on. An expander with no cap pumps the room floor in
    // and out at the rate of speech, which is more audible than the floor it was hiding. A gate
    // with `range_db = 0` is not a gate with a cap, it is a gate that does nothing — which is a
    // different mistake wearing the same clothes.
    for preset in shipped() {
        let Some(gate) = &preset.gate else {
            continue;
        };
        assert!(
            gate.range_db < 0.0,
            "{}: a gate with range {} dB attenuates nothing",
            preset.name,
            gate.range_db
        );
        assert!(
            gate.range_db >= -40.0,
            "{}: a range of {} dB is a hard gate with extra steps",
            preset.name,
            gate.range_db
        );
    }
}

#[test]
fn every_de_esser_can_be_built_at_the_rate_the_capture_runs_at() {
    // A split the sample rate cannot carry is a preset claiming an effect it does not get. The
    // capture stream asks for 48 kHz precisely so this holds; the check is what notices the day
    // that changes.
    for preset in shipped() {
        let Some(deesser) = &preset.deesser else {
            continue;
        };
        let limit = fxsound_dsp::input::MAX_CORNER_FRACTION * CAPTURE_RATE;
        assert!(
            deesser.frequency_hz < limit,
            "{}: a {} Hz split cannot be built at {CAPTURE_RATE} Hz (the limit is {limit:.0} Hz)",
            preset.name,
            deesser.frequency_hz
        );
    }
}

#[test]
fn each_band_does_what_it_says_it_does() {
    // The Q-summing defect, measured at the resolution a voice set needs. A band that stores
    // +1.5 dB and realises +2.6 dB is not a preset with a bold move in it; it is a preset whose
    // author could not see what they were writing.
    let mut wrong = Vec::new();
    for preset in shipped() {
        let params = preset.to_params();
        let eq = equalizer(&params);
        let (centers, stored) = params.bands();
        for (index, (&centre, &want)) in centers.iter().zip(stored).enumerate() {
            // A band above Nyquist is bypassed by the design, and its stored gain is not a claim
            // about anything.
            if centre * 2.0 >= CAPTURE_RATE {
                continue;
            }
            let realised = eq.response_db(centre);
            if (realised - want).abs() > MAX_BAND_ERROR_DB {
                wrong.push(format!(
                    "{} band {index} at {centre:.0} Hz stores {want:+.1} dB and realises \
                     {realised:+.1} dB",
                    preset.name
                ));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "{} band(s) that do not do what they say:\n  {}",
        wrong.len(),
        wrong.join("\n  ")
    );
}

/// Everything a listener can hear about a preset, measured by running signal through it.
///
/// **The first version of this compared parameter vectors, and it was wrong.** Three of its terms
/// could not be heard: a preset scored 6.0 for switching the denoiser on, 20.0 for having a gate
/// where its neighbour had none, and something for a `range_db` that measurement shows is inert
/// above a −58 dBFS floor. Because the distance is a maximum over terms, those constants *were*
/// the distance for most pairs, and the bar of 1.0 became unreachable. It reported 26.0 for Flat
/// against Studio — two presets whose audible difference is a high-pass corner — and it could not
/// have flagged that pair at any threshold. A guard that cannot fire is not a guard.
///
/// So nothing here is a parameter. Every term is decibels of something that happens to a signal,
/// measured through the real [`fxsound_dsp::InputChain`], and two presets that score alike score
/// alike because they *do* alike.
///
/// Tone and level are measured apart on purpose. Makeup gain is a constant offset, and folded into
/// the tone curve it would swamp a set whose tonal moves are all under two decibels — which is
/// itself worth knowing, because the loudest thing about changing preset should not be loudness.
fn fingerprint(preset: &InputPreset) -> Vec<f32> {
    let mut out = tone_curve(preset);
    out.extend(dynamics(preset));
    out
}

/// The chain a preset describes, with every stage it asks for, in the order it names.
///
/// Through [`fxsound_dsp::InputChain::apply`], which is the one place a snapshot becomes stage
/// settings: 0.3.0 kept a second copy of that mapping here, and a field added to one and not the
/// other would have been a preset whose sound this bench could not measure.
fn chain_for(preset: &InputPreset) -> fxsound_dsp::InputChain {
    let spec = ChainSpec::by_name(&preset.chain)
        .unwrap_or_else(|| panic!("{}: unknown chain {:?}", preset.name, preset.chain));
    let mut chain = fxsound_dsp::InputChain::from_spec(spec, CAPTURE_RATE);
    chain.apply(&preset.to_params());
    chain
}

#[test]
fn every_preset_names_a_chain_that_exists() {
    for preset in shipped() {
        assert!(
            ChainSpec::by_name(&preset.chain).is_some(),
            "{}: chain {:?} is not one of {:?}",
            preset.name,
            preset.chain,
            ChainSpec::NAMES
        );
    }
}

/// Run a block through a chain and report the settled level, in dB against the input.
///
/// RMS, not peak. The peak of a loud probe is whatever the limiter's ceiling is — every preset
/// reads −3 dBFS and the measurement says nothing — which is how a preset differing only in its
/// compressor's detector looked identical to its neighbour here. RMS is what survives the ceiling,
/// and is the better question anyway: a listener judges loudness, not the tallest sample.
fn through(chain: &mut fxsound_dsp::InputChain, block: &mut [f32], reference: f32) -> f32 {
    chain.reset();
    chain.process(block, 1);
    let settled = &block[block.len() / 2..];
    let mean_square = settled.iter().map(|x| x * x).sum::<f32>() / settled.len().max(1) as f32;
    20.0 * (mean_square.sqrt().max(1.0e-9) / reference).log10()
}

/// What the preset does to *tone*: the high-pass and the equalizer, with the dynamics out of the
/// way and the makeup zeroed, so the curve is a shape rather than a shape plus a level.
fn tone_curve(preset: &InputPreset) -> Vec<f32> {
    let mut chain = chain_for(preset);
    chain.set_gate_enabled(false);
    chain.set_compressor_enabled(false);
    chain.set_deesser_enabled(false);
    chain.set_denoise_enabled(false);
    chain.set_makeup_db(0.0);

    THIRD_OCTAVES
        .iter()
        .map(|&hz| {
            if hz * 2.0 >= CAPTURE_RATE {
                return 0.0;
            }
            let amplitude = 0.05;
            let mut block = probe_tone(hz, amplitude, 9_600);
            through(&mut chain, &mut block, amplitude)
        })
        .collect()
}

/// What the preset does to *level*: four probes, each a decibel figure a listener would notice.
///
/// **One thing this cannot see, stated rather than left to be discovered.** Two presets differing
/// *only* in their compressor's `detection` measure within a decibel of each other here, even
/// with a syllabic envelope on the probe — the peak-to-RMS gap the research measures at three to
/// seven decibels needs real speech, not a harmonic sum. Such a pair would be refused as a
/// duplicate. That is the safe direction (this errs toward refusing, never toward shipping two
/// presets that sound alike), no preset in the set is in that position, and if one ever is, the
/// fix is a probe with a real talker's crest factor rather than a looser bar.
///
/// A loud passage and a quiet one together say how much the preset compresses and how much it
/// makes up; the floor says how much the gate and the denoiser take away; the sibilant says how
/// much the de-esser does. All through the whole chain, so a stage that is absent shows up only as
/// the number it fails to change — which is the honest weight for it.
fn dynamics(preset: &InputPreset) -> Vec<f32> {
    let mut chain = chain_for(preset);
    let mut out = Vec::with_capacity(4);

    for level_db in [-6.0_f32, -26.0] {
        let amplitude = 10.0_f32.powf(level_db / 20.0);
        let mut block = speech_like(amplitude, 48_000);
        out.push(through(&mut chain, &mut block, amplitude));
    }

    // A *room* floor, not white noise. The difference matters: RNNoise separates speech from
    // noise, so on undifferentiated hiss it removes about a decibel and on hum-under-hiss about
    // forty-four. Probing with white noise would have made the denoiser almost invisible here —
    // it did, until a preset that differed from its neighbour only by switching the denoiser on
    // slipped past this check.
    let floor = 10.0_f32.powf(-55.0 / 20.0);
    let mut block = room_floor(floor, 48_000);
    out.push(through(&mut chain, &mut block, floor));

    let sibilant = 10.0_f32.powf(-12.0 / 20.0);
    let mut block = sibilance(sibilant, 24_000);
    out.push(through(&mut chain, &mut block, sibilant));

    out
}

fn probe_tone(hz: f32, amplitude: f32, frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|n| (n as f32 * std::f32::consts::TAU * hz / CAPTURE_RATE).sin() * amplitude)
        .collect()
}

/// Deterministic noise, so every run measures the same signal.
fn noise(frames: usize, amplitude: f32) -> Vec<f32> {
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    (0..frames)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ((state >> 40) as f32 / 8_388_608.0 - 1.0) * amplitude
        })
        .collect()
}

/// A stand-in for a voice: a fundamental with harmonics, under a syllabic envelope.
///
/// The envelope is not decoration. Without it the probe is a steady tone whose peak and RMS are a
/// few decibels apart, and a compressor detecting peak looks exactly like one detecting RMS —
/// which made this test refuse two presets that differ only in their detector, the one field the
/// research insisted on because peak and RMS against the same threshold differ by three to seven
/// decibels *on speech*. Speech has a crest factor near twelve decibels because it starts and
/// stops; a probe for a voice chain has to as well.
fn speech_like(amplitude: f32, frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|n| {
            let t = n as f32 / CAPTURE_RATE;
            let f = 130.0;
            let body = (t * std::f32::consts::TAU * f).sin() * 0.6
                + (t * std::f32::consts::TAU * f * 2.0).sin() * 0.3
                + (t * std::f32::consts::TAU * f * 5.0).sin() * 0.2
                + (t * std::f32::consts::TAU * f * 11.0).sin() * 0.1;
            // Four syllables a second, each with a quiet tail — roughly a talker's rhythm.
            let syllable = (t * 4.0).fract();
            let envelope = if syllable < 0.55 {
                (syllable / 0.55 * std::f32::consts::PI).sin().powf(0.6)
            } else {
                0.02
            };
            body * envelope * amplitude / 1.2
        })
        .collect()
}

/// What a desk microphone in a room with a computer in it actually picks up: mains hum with
/// broadband hiss over it.
fn room_floor(amplitude: f32, frames: usize) -> Vec<f32> {
    noise(frames, amplitude * 0.4)
        .iter()
        .enumerate()
        .map(|(n, hiss)| {
            let t = n as f32 / CAPTURE_RATE;
            hiss + (t * std::f32::consts::TAU * 50.0).sin() * amplitude
        })
        .collect()
}

/// Energy where sibilance lives, so the de-esser has something to act on.
fn sibilance(amplitude: f32, frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|n| {
            let t = n as f32 / CAPTURE_RATE;
            ((t * std::f32::consts::TAU * 6_500.0).sin()
                + (t * std::f32::consts::TAU * 7_900.0).sin()
                + (t * std::f32::consts::TAU * 9_100.0).sin())
                * amplitude
                / 3.0
        })
        .collect()
}

/// How far apart two presets are, in decibels: the largest single difference anywhere.
///
/// A maximum rather than a mean, because a listener notices the one thing that changed, not the
/// average of everything that did not. Now that every term is a decibel of something audible, the
/// number this returns is decibels too, and the bar can be argued about in those terms.
fn distance(left: &[f32], right: &[f32]) -> f32 {
    left.iter()
        .zip(right)
        .fold(0.0_f32, |worst, (a, b)| worst.max((a - b).abs()))
}

/// Measure every preset once.
///
/// Measuring is real work — thirty-three probes through a whole chain each — and there are
/// n(n−1)/2 pairs, so measuring inside the comparison did it ten times per preset and made the
/// suite take half a minute.
fn fingerprints(presets: &[InputPreset]) -> Vec<Vec<f32>> {
    presets.iter().map(fingerprint).collect()
}

#[test]
fn no_two_presets_are_the_same_chain_under_two_names() {
    let presets = shipped();
    let measured = fingerprints(&presets);
    let mut pairs = Vec::new();
    for (index, left) in presets.iter().enumerate() {
        for (offset, right) in presets[index + 1..].iter().enumerate() {
            let apart = distance(&measured[index], &measured[index + 1 + offset]);
            if apart < INDISTINGUISHABLE {
                pairs.push(format!(
                    "{} and {} differ by at most {apart:.2} anywhere in the chain",
                    left.name, right.name
                ));
            }
        }
    }
    assert!(
        pairs.is_empty(),
        "{} pair(s) of voice presets that are the same thing twice:\n  {}",
        pairs.len(),
        pairs.join("\n  ")
    );
}

/// Third-octave centres, the resolution a listener compares tone at. The same table the output
/// side uses, so the two sets are judged alike.
const THIRD_OCTAVES: [f32; 29] = [
    31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0, 500.0, 630.0,
    800.0, 1000.0, 1250.0, 1600.0, 2000.0, 2500.0, 3150.0, 4000.0, 5000.0, 6300.0, 8000.0, 10000.0,
    12500.0, 16000.0, 20000.0,
];

#[test]
fn how_close_the_closest_voice_presets_are_is_reported() {
    // Printed rather than asserted, like its opposite number on the output side. The assertion
    // above says when two presets are indefensibly alike; this says which pair is nearest, which
    // is the question worth asking while a set is being voiced. Run with `-- --nocapture`.
    let presets = shipped();
    let measured = fingerprints(&presets);
    let mut pairs = Vec::new();
    for (index, left) in presets.iter().enumerate() {
        for (offset, right) in presets[index + 1..].iter().enumerate() {
            pairs.push((
                distance(&measured[index], &measured[index + 1 + offset]),
                left.name.clone(),
                right.name.clone(),
            ));
        }
    }
    pairs.sort_by(|a, b| a.0.partial_cmp(&b.0).expect("finite"));

    println!("\n{} voice preset(s), nearest pairs first:", presets.len());
    for (apart, left, right) in pairs.iter().take(10) {
        println!("  {apart:6.2}  {left} / {right}");
    }
}

#[test]
fn what_each_preset_actually_does_is_printed() {
    // Not an assertion. The set is small enough to read, and a preset that looks wrong here is
    // worth catching before anyone has to hear it. Run with `-- --nocapture`.
    println!();
    for preset in shipped() {
        let params = preset.to_params();
        let eq = equalizer(&params);
        let curve: Vec<String> = params
            .bands()
            .0
            .iter()
            .map(|&hz| format!("{:+.1}", eq.response_db(hz)))
            .collect();
        println!(
            "{:<14} hpf {:>5.0} Hz/{}  gate {:<10} comp {:<10} de-ess {:<10} makeup {:+.1}  \
             denoise {}",
            preset.name,
            preset.highpass_hz,
            preset.highpass_order,
            preset.gate.as_ref().map_or_else(
                || "off".to_owned(),
                |g| format!("{:.0}/{:.1}", g.threshold_db, g.ratio)
            ),
            preset.compressor.as_ref().map_or_else(
                || "off".to_owned(),
                |c| format!("{:.0}/{:.1}", c.threshold_db, c.ratio)
            ),
            preset.deesser.as_ref().map_or_else(
                || "off".to_owned(),
                |d| format!("{:.0}/{:.0}", d.frequency_hz, d.threshold_db)
            ),
            preset.makeup_db,
            preset.denoise_level().key(),
        );
        println!("               realised curve: {}", curve.join(" "));
    }
}
