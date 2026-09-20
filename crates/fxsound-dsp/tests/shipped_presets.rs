//! What every preset this project ships has to be true of.
//!
//! A `.fac` file is data, so nothing in the ordinary test suite ever looks at one: the parser is
//! tested against round-tripping and the engine against synthetic parameters, and a preset that
//! says one thing and sounds like another passes both. These checks close that gap, and they are
//! written against the engine rather than against a table of expected numbers, so they stay true
//! if the mapping from a slider to a gain is ever corrected.
//!
//! Three classes of defect, all of which the shipped set actually contains:
//!
//! * a stored effect amount the engine ignores — the author moved a slider, the file recorded it,
//!   and nothing happens;
//! * two presets that are the same voicing under different names;
//! * an equalizer curve that does far more than any of its bands claims, because two bands sit on
//!   top of each other and add.

use fxsound_core::{Effect, Preset, scale};
use fxsound_dsp::eq::GraphicEq;
use std::path::{Path, PathBuf};

/// Below this stored value the Ambience stage never turns on at all.
///
/// `(int)(midi * 0.34) > 12`, so the first value that reaches it is 39 — a slider position of 3.1
/// out of 10. Anything from 1 to 38 is a preset claiming an effect it does not get.
const AMBIENCE_MIN_AUDIBLE_MIDI: u8 = 39;

/// Above this stored value Dynamic Boost stops changing.
///
/// Its mapping saturates at +11.60 dB and every value from here to 127 produces the identical
/// gain, so the differences between them are theatre.
const DYNAMIC_BOOST_SATURATION_MIDI: u8 = 70;

/// Below this ratio, two adjacent band centres are worth looking at.
///
/// Reported rather than asserted, deliberately. Overlap is normal on a graphic equalizer: the Q is
/// derived from the band *count* and never recomputed when a preset installs its own centres, so
/// at the ten-band Q of about 1.6 the −3 dB skirts of neighbouring bands overlap even when the
/// centres are nearly an octave apart. Spacing is therefore only a smell; the defect it hints at
/// is bands that *sum* past what any of them asks for, and that is measured directly below.
const BAND_SPACING_WORTH_A_LOOK: f32 = std::f32::consts::SQRT_2;

/// How far the summed curve may exceed its largest single band before it is lying about itself.
const MAX_SUMMING_EXCESS_DB: f32 = 2.0;

fn preset_dirs() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root")
        .join("assets/presets");
    vec![root.join("Factsoft"), root.join("BonusPresets")]
}

fn shipped() -> Vec<(String, Preset)> {
    let mut out = Vec::new();
    for dir in preset_dirs() {
        let entries =
            std::fs::read_dir(&dir).unwrap_or_else(|err| panic!("{}: {err}", dir.display()));
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("fac") {
                continue;
            }
            let preset = fxsound_preset::load(&path)
                .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
            out.push((preset.name.clone(), preset));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(out.len() >= 30, "only found {} presets", out.len());
    out
}

/// The magnitude response of a preset's equalizer, in dB, sampled across the audible band.
fn response_db(preset: &Preset) -> Vec<(f32, f32)> {
    const RATE: f32 = 48_000.0;
    const N: usize = 16_384;

    let mut eq = GraphicEq::new();
    eq.set_sample_rate(RATE);
    let centers: Vec<f32> = preset.eq_bands.iter().map(|b| b.center_hz).collect();
    let boosts: Vec<f32> = preset.eq_bands.iter().map(|b| b.boost_db).collect();
    eq.set_bands(&centers, &boosts);
    eq.set_enabled(true);

    // An impulse through the real filter cascade, rather than a sum of textbook bell curves: what
    // matters is what the engine does, including whatever its Q derivation decides.
    let mut buffer = vec![0.0_f32; N];
    buffer[0] = 1.0;
    eq.process(&mut buffer, 1);

    let mut planner = realfft::RealFftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(N);
    let mut spectrum = fft.make_output_vec();
    fft.process(&mut buffer, &mut spectrum)
        .expect("the impulse response transforms");

    spectrum
        .iter()
        .enumerate()
        .map(|(bin, value)| {
            let hz = bin as f32 * RATE / N as f32;
            (hz, 20.0 * value.norm().max(1e-12).log10())
        })
        .filter(|(hz, _)| (20.0..=20_000.0).contains(hz))
        .collect()
}

#[test]
fn no_preset_stores_an_effect_the_engine_ignores() {
    let mut complaints = Vec::new();
    for (name, preset) in shipped() {
        let ambience = preset.main_midi[Effect::Ambience.vals_index()];
        if (1..AMBIENCE_MIN_AUDIBLE_MIDI).contains(&ambience) {
            complaints.push(format!(
                "{name}: Ambience is {ambience}, below the {AMBIENCE_MIN_AUDIBLE_MIDI} the stage \
                 needs to turn on \u{2014} the file claims an effect it does not get"
            ));
        }
    }
    assert!(
        complaints.is_empty(),
        "{} preset(s) store an inaudible effect:\n  {}",
        complaints.len(),
        complaints.join("\n  ")
    );
}

/// Two curves this close, at every frequency, are the same curve.
///
/// Comparing the stored numbers instead would miss the real cases: the five presets named 70's,
/// 80's, Classic Rock, Modern Country and Trap are *not* identical files — a band sits at 2150 Hz
/// in one and 2120 Hz in another — and the difference that produces is at most 0.43 dB anywhere in
/// the spectrum, because the Q is derived from the band count and a thirty-hertz move at 2 kHz is
/// far inside one filter's skirt. Stored-value comparison calls those five distinct; a listener
/// does not.
const INDISTINGUISHABLE_DB: f32 = 0.5;

#[test]
fn no_two_presets_are_the_same_voicing_under_different_names() {
    // A duplicate is not a bug in the engine, it is a row in a combo box that does nothing. A user
    // who picks Jazz over Classical is entitled to hear a difference.
    let presets = shipped();
    let curves: Vec<(String, Vec<f32>, [u8; 6])> = presets
        .iter()
        .map(|(name, preset)| {
            let curve = response_db(preset).into_iter().map(|(_, db)| db).collect();
            (name.clone(), curve, preset.main_midi)
        })
        .collect();

    let mut pairs = Vec::new();
    for (i, left) in curves.iter().enumerate() {
        for right in &curves[i + 1..] {
            if left.2 != right.2 {
                continue;
            }
            let difference = left
                .1
                .iter()
                .zip(&right.1)
                .fold(0.0_f32, |worst, (a, b)| worst.max((a - b).abs()));
            if difference < INDISTINGUISHABLE_DB {
                pairs.push(format!(
                    "{} and {} differ by at most {difference:.2} dB and share every effect amount",
                    left.0, right.0
                ));
            }
        }
    }
    assert!(
        pairs.is_empty(),
        "{} pair(s) of presets a listener cannot tell apart:\n  {}",
        pairs.len(),
        pairs.join("\n  ")
    );
}

/// Third-octave centres from 31.5 Hz to 20 kHz — the resolution a listener compares tone at.
const THIRD_OCTAVES: [f32; 29] = [
    31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0, 500.0, 630.0,
    800.0, 1000.0, 1250.0, 1600.0, 2000.0, 2500.0, 3150.0, 4000.0, 5000.0, 6300.0, 8000.0, 10000.0,
    12500.0, 16000.0, 20000.0,
];

/// A preset's curve sampled where it is judged, rather than at every FFT bin.
fn thirds(preset: &Preset) -> Vec<f32> {
    let full = response_db(preset);
    THIRD_OCTAVES
        .iter()
        .map(|&target| {
            full.iter()
                .min_by(|a, b| {
                    (a.0 - target)
                        .abs()
                        .partial_cmp(&(b.0 - target).abs())
                        .expect("finite")
                })
                .map_or(0.0, |(_, db)| *db)
        })
        .collect()
}

#[test]
fn how_close_the_closest_presets_are_is_reported() {
    // The assertion above only fires when two presets also share every effect amount. This is the
    // softer question it cannot answer: of thirty-four presets, which are nearest in tone? Printed
    // rather than asserted, because how close is too close depends on what the two claim to be —
    // a genre preset beside a utility preset is a different matter from two genres.
    let presets = shipped();
    let curves: Vec<(String, Vec<f32>)> = presets
        .iter()
        .map(|(name, preset)| (name.clone(), thirds(preset)))
        .collect();

    let mut pairs = Vec::new();
    for (i, left) in curves.iter().enumerate() {
        for right in &curves[i + 1..] {
            let worst = left
                .1
                .iter()
                .zip(&right.1)
                .fold(0.0_f32, |acc, (a, b)| acc.max((a - b).abs()));
            pairs.push((worst, format!("{} / {}", left.0, right.0)));
        }
    }
    pairs.sort_by(|a, b| a.0.partial_cmp(&b.0).expect("finite"));
    eprintln!("note: the eight closest preset pairs, by worst third-octave difference:");
    for (worst, pair) in pairs.iter().take(8) {
        eprintln!("  {worst:>5.2} dB  {pair}");
    }
}

#[test]
fn bands_that_sit_on_top_of_each_other_are_reported() {
    let mut complaints = Vec::new();
    for (name, preset) in shipped() {
        let mut centers: Vec<f32> = preset.eq_bands.iter().map(|b| b.center_hz).collect();
        centers.sort_by(|a, b| a.partial_cmp(b).expect("finite centres"));
        for pair in centers.windows(2) {
            let (low, high) = (pair[0], pair[1]);
            if low > 0.0 && high / low < BAND_SPACING_WORTH_A_LOOK {
                complaints.push(format!(
                    "{name}: bands at {low:.1} Hz and {high:.1} Hz are {:.2}x apart, closer than \
                     the half-octave two independent filters need",
                    high / low
                ));
            }
        }
    }
    if !complaints.is_empty() {
        eprintln!(
            "note: {} closely spaced band pair(s):\n  {}",
            complaints.len(),
            complaints.join("\n  ")
        );
    }
}

#[test]
fn a_presets_curve_does_no_more_than_its_loudest_band_asks_for() {
    let mut complaints = Vec::new();
    for (name, preset) in shipped() {
        let largest = preset
            .eq_bands
            .iter()
            .map(|b| b.boost_db)
            .fold(0.0_f32, |acc, db| acc.max(db));
        let peak = response_db(&preset)
            .into_iter()
            .fold(f32::NEG_INFINITY, |acc, (_, db)| acc.max(db));
        if peak > largest + MAX_SUMMING_EXCESS_DB {
            complaints.push(format!(
                "{name}: the curve peaks at {peak:+.1} dB while its largest band asks for \
                 {largest:+.1} dB"
            ));
        }
    }
    assert!(
        complaints.is_empty(),
        "{} preset(s) deliver more than they ask for:\n  {}",
        complaints.len(),
        complaints.join("\n  ")
    );
}

#[test]
fn every_effect_amount_a_preset_stores_is_one_a_slider_can_reach() {
    // The GUI slider has eleven positions. A stored value between them cannot be reproduced by a
    // user, so saving the preset again would silently change it.
    let mut complaints = Vec::new();
    for (name, preset) in shipped() {
        for effect in Effect::ALL {
            let midi = preset.main_midi[effect.vals_index()];
            let round_tripped = scale::value_to_midi(scale::slider_to_value(
                scale::value_to_slider(scale::midi_to_value(midi)).round(),
            ));
            if midi != round_tripped {
                complaints.push(format!(
                    "{name}: {:?} is {midi}, which is between slider positions (nearest is \
                     {round_tripped})",
                    effect
                ));
            }
        }
    }
    // Reported rather than asserted: the shipped files were authored against the Windows build,
    // whose slider had the same eleven positions but whose preset editor did not round. Turning
    // this into a failure would mean rewriting files that must round-trip byte for byte.
    if !complaints.is_empty() {
        eprintln!(
            "note: {} stored amount(s) sit between slider positions:\n  {}",
            complaints.len(),
            complaints.join("\n  ")
        );
    }
}

#[test]
fn a_saturated_dynamic_boost_is_reported() {
    // Not a failure: a preset is allowed to ask for the maximum. But three presets differing only
    // above the saturation point are three names for one voicing, so this is worth seeing.
    let saturated: Vec<String> = shipped()
        .into_iter()
        .filter_map(|(name, preset)| {
            let midi = preset.main_midi[Effect::DynamicBoost.vals_index()];
            (midi > DYNAMIC_BOOST_SATURATION_MIDI).then(|| format!("{name} ({midi})"))
        })
        .collect();
    if !saturated.is_empty() {
        eprintln!(
            "note: {} preset(s) store a Dynamic Boost above the {DYNAMIC_BOOST_SATURATION_MIDI} \
             saturation point, where the stored value no longer changes the gain: {}",
            saturated.len(),
            saturated.join(", ")
        );
    }
}
