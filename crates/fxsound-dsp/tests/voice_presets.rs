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

/// Everything a listener can hear about a preset, as one vector of comparable numbers.
///
/// Each entry is scaled so that "one unit" means about the same amount of audible difference
/// wherever it comes from: a decibel of equalizer, a decibel of threshold, a ratio step. That is a
/// judgement, and it is written down here rather than buried in a comparison.
fn fingerprint(preset: &InputPreset) -> Vec<f32> {
    let params = preset.to_params();
    let eq = equalizer(&params);
    let mut out: Vec<f32> = THIRD_OCTAVES
        .iter()
        .map(|&hz| {
            if hz * 2.0 >= CAPTURE_RATE {
                0.0
            } else {
                eq.response_db(hz)
            }
        })
        .collect();

    // The high-pass: its corner in octaves from 80 Hz, and its order. A 4th-order 120 Hz filter
    // and a 2nd-order 75 Hz one are not the same preset.
    out.push((preset.highpass_hz / 80.0).log2() * 3.0);
    out.push(f32::from(preset.highpass_order));
    out.push(if preset.rnnoise { 6.0 } else { 0.0 });

    // A stage that is off is not a stage at its defaults, so absence has to read as distance.
    match &preset.gate {
        Some(gate) => out.extend([
            gate.threshold_db + 45.0,
            gate.ratio * 3.0,
            gate.range_db + 14.0,
        ]),
        None => out.extend([-20.0, -20.0, -20.0]),
    }
    match &preset.compressor {
        Some(compressor) => out.extend([
            compressor.threshold_db + 18.0,
            compressor.ratio * 3.0,
            (compressor.attack_ms / 20.0).log2() * 3.0,
            (compressor.release_ms / 150.0).log2() * 3.0,
        ]),
        None => out.extend([-20.0, -20.0, -20.0, -20.0]),
    }
    match &preset.deesser {
        Some(deesser) => out.extend([
            (deesser.frequency_hz / 5_500.0).log2() * 6.0,
            deesser.threshold_db + 22.0,
        ]),
        None => out.extend([-20.0, -20.0]),
    }

    out.push(preset.makeup_db);
    out.push(preset.ceiling_db);
    out
}

/// How far apart two presets are: the largest single difference anywhere in the chain.
///
/// A maximum rather than a mean, because a listener notices the one thing that changed, not the
/// average of everything that did not.
fn distance(left: &InputPreset, right: &InputPreset) -> f32 {
    fingerprint(left)
        .iter()
        .zip(fingerprint(right))
        .fold(0.0_f32, |worst, (a, b)| worst.max((a - b).abs()))
}

#[test]
fn no_two_presets_are_the_same_chain_under_two_names() {
    let presets = shipped();
    let mut pairs = Vec::new();
    for (index, left) in presets.iter().enumerate() {
        for right in &presets[index + 1..] {
            let apart = distance(left, right);
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
    let mut pairs = Vec::new();
    for (index, left) in presets.iter().enumerate() {
        for right in &presets[index + 1..] {
            pairs.push((distance(left, right), left.name.clone(), right.name.clone()));
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
             rnnoise {}",
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
            preset.rnnoise,
        );
        println!("               realised curve: {}", curve.join(" "));
    }
}
