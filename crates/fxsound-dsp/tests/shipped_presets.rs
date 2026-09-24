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

/// Below this stored value the Ambience stage is all but silent.
///
/// The stage turns on above 12, as the original's does, but the MUSIC2 warp only gives it a real
/// wet level from `(int)(midi * 0.34) > 12`, and the first value that reaches that is 39 — a
/// slider position of 3.1 out of 10 on Windows, and position 1 here since the slider was spread
/// over the audible values (audit report #39, `scale::AMBIENCE_FIRST_AUDIBLE_MIDI`). From 13 to
/// 38 the wet level only ramps up to the one at 39, −40 dB below the dry signal, so a preset
/// storing one of those is claiming an effect it barely gets.
const AMBIENCE_MIN_AUDIBLE_MIDI: u8 = scale::AMBIENCE_FIRST_AUDIBLE_MIDI;

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
                 needs to be heard \u{2014} the file claims an effect it barely gets"
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
    // Audit report #14. The GUI slider has eleven whole positions, and 49 of the shipped
    // presets' 170 effect amounts sit between them: touching the slider used to snap such a value
    // to a position and save that, silently. The slider now shows such a value where it is, with
    // its decimal, and reaches every stored value with Shift, so a preset loaded and saved again
    // is the preset it was. Checked through the mapping the window uses, both ways. The one
    // exception is a Dynamic Boost past its saturation point, where every value is the same gain:
    // it shows at the top of the slider and is saved as the top.
    let mut complaints = Vec::new();
    let mut between = 0;
    for (name, preset) in shipped() {
        for effect in Effect::ALL {
            let midi = preset.main_midi[effect.vals_index()];
            let shown = scale::midi_to_slider_for(effect, midi);
            let saved = scale::slider_to_midi_for(effect, shown);
            let expected = if effect == Effect::DynamicBoost && midi > DYNAMIC_BOOST_SATURATION_MIDI
            {
                DYNAMIC_BOOST_SATURATION_MIDI
            } else {
                midi
            };
            if saved != expected {
                complaints.push(format!(
                    "{name}: {effect:?} is {midi}, shown at {shown}, saved again as {saved}"
                ));
            }
            if scale::whole_position_for(effect, shown).is_none() {
                between += 1;
                let label = scale::slider_label_for(effect, shown);
                if !label.contains('.') {
                    complaints.push(format!(
                        "{name}: {effect:?} is {midi}, between positions, but reads {label:?}"
                    ));
                }
            }
        }
    }
    assert!(
        complaints.is_empty(),
        "{} stored amount(s) do not survive the slider:\n  {}",
        complaints.len(),
        complaints.join("\n  ")
    );
    // The fixture really does hold values between positions, or this checks nothing.
    assert!(
        between > 0,
        "no shipped preset stores a value between positions"
    );
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

#[test]
fn every_shipped_preset_keeps_its_own_bass_on_a_ladder_that_reaches_lower() {
    // Audit report #13 (held ends). The shipped presets are ten-band curves from 62.5 Hz up; on
    // fifteen, twenty or thirty-one bands, which reach down to 25 or 20 Hz, their lowest gain
    // used to be copied onto every band below 62.5 Hz, and those sections added up to a sub-bass
    // shelf none of them has. Measured against each preset's own ten-band response, 241 points
    // from 20 Hz to 20 kHz: below 45 Hz the worst was 25.3 dB off on thirty-one bands ("Life
    // (Quizal)", +10 dB at 62.5 Hz, played +28.6 dB at 31.5 Hz), 21.1 on twenty and 13.5 on
    // fifteen, and the mean RMS departure on thirty-one bands was 2.6 dB. Tapered past the ends,
    // the worst is 4.7, 4.4 and 4.4 dB and the mean 1.0 dB.
    use fxsound_dsp::eq::{fit_preset_gains, standard_centres};
    let points: Vec<f32> = (0..=240)
        .map(|step| 20.0 * 1000.0_f32.powf(step as f32 / 240.0))
        .collect();
    let presets: Vec<(String, Preset)> = shipped()
        .into_iter()
        .filter(|(_, preset)| preset.eq_bands.iter().any(|band| band.boost_db != 0.0))
        .collect();
    for (count, worst_allowed, was) in [(31, 5.0, 25.3), (20, 5.0, 21.1), (15, 5.0, 13.5)] {
        let live = standard_centres(count);
        let mut worst = (0.0_f32, String::new());
        let mut rms_total = 0.0_f32;
        for (name, preset) in &presets {
            let centres: Vec<f32> = preset.eq_bands.iter().map(|b| b.center_hz).collect();
            let gains: Vec<f32> = preset.eq_bands.iter().map(|b| b.boost_db).collect();
            let mut own = GraphicEq::new();
            own.set_sample_rate(48_000.0);
            own.set_bands(&centres, &gains);
            let mut there = GraphicEq::new();
            there.set_sample_rate(48_000.0);
            there.set_bands(&live, &fit_preset_gains(&centres, &gains, &live));
            let mut squares = 0.0;
            for hz in &points {
                let off = there.response_db(*hz) - own.response_db(*hz);
                squares += off * off;
                if *hz < 45.0 && off.abs() > worst.0 {
                    worst = (off.abs(), name.clone());
                }
            }
            rms_total += (squares / points.len() as f32).sqrt();
        }
        assert!(
            worst.0 < worst_allowed,
            "{count} bands: {} is {:.2} dB off its own response below 45 Hz, was {was} dB",
            worst.1,
            worst.0
        );
        let mean_rms = rms_total / presets.len() as f32;
        if count == 31 {
            assert!(
                mean_rms < 1.1,
                "{count} bands: mean RMS departure {mean_rms:.2} dB, was 2.6"
            );
        }
    }
}
